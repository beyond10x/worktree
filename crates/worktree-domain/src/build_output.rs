//! Build layout an archive leaves out, and the strip of it from an archive written before.
//!
//! An archive holds what no remote recovers. Cargo's own build layout and a Flutter app's output
//! are recreated from tracked sources by the next build, so `archive` leaves them out of
//! `dirty.patch` and the fingerprint and
//! records what it left out (`worktree.archive.BuildOutputTarget`), and `prune-archives
//! --strip-build-output` removes it from an existing patch. Every decision here is I/O-free: the
//! Git adapter observes the disk or streams the patch and asks.
//!
//! Inside a recognised cargo target `T` a path is build layout when its first component below `T`
//! is `debug`, `release` or `.rustc_info.json`, `tmp` while `T` holds a profile, `CACHEDIR.TAG`
//! while it carries Cargo's signature, or a profile directory (one holding `.fingerprint/` as a
//! direct child; also `T/<triple>/<profile>/`). Every other path below `T` stays archived. On
//! disk, `T` is recognised by `discard-cache`'s structural rule (a valid tag or the untagged-target
//! rule, and at least one profile); in a patch, `T` is a directory for which the patch purely adds
//! a Cargo-signed `T/CACHEDIR.TAG` or `T/<p>/.fingerprint/…`. A section that is not a pure addition
//! of an untracked path, or whose path another section names too, is never stripped.
//!
//! A Flutter app's output ([`FlutterOutputKind`]) is `P/build/` and `P/.dart_tool/` beside a
//! `pubspec.yaml`, where nothing is tracked at that path. One classifier
//! ([`flutter_output_kind`]) decides it for `archive`, `discard-cache` and the strip, from
//! observations each caller supplies ([`FlutterFacts`]): the disk and the index, or HEAD's tree and
//! the patch. Every untracked file strictly below one is build layout. A directory that is a
//! recognised cargo target too is classified by the cargo rule alone ([`FlutterFacts::cargo_target`]).
//!
//! A repository is never build layout, cargo's or a Flutter app's ([`repository_path`]): a path
//! with a `.git` component, or at or below a Git directory, stays archived, is never deleted as
//! layout and keeps its section in a strip. On disk a Git directory is observed by the same rule
//! `discard-cache` keeps it by ([`crate::GitDirectoryFacts`]); in a patch it is a directory for
//! which the patch purely adds `HEAD` and some path under `objects/`.

use crate::{
    ARCHIVE_PATCH_FILE, ArchiveContents, ArchiveEntryKind, ArchiveManifest, CACHEDIR_TAG_FILE,
    CARGO_PROFILE_MARKER, CARGO_TARGET_TMP, GIT_DIRECTORY_HEAD, GIT_DIRECTORY_OBJECTS, GIT_ENTRY,
    PruneVerdict, SkippedArchiveEntry, is_cache_tag,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Independent, immutable manifest format identifier of an archive that left build layout
/// out, recorded in [`ArchiveManifest::build_output`]. It may also image nested repositories.
pub const ARCHIVE_FORMAT_V3: &str = "worktree.archive/3";

/// The rustc metadata file Cargo writes at the root of a target.
pub const CARGO_RUSTC_INFO: &str = ".rustc_info.json";

/// First components below a cargo target that are build layout whatever they hold, given what is
/// known of the target ([`TargetFacts`]): `tmp` only while the target holds a profile, and
/// `CACHEDIR.TAG` only while it carries Cargo's signature.
pub const CARGO_LAYOUT_NAMES: [&str; 5] = [
    "debug",
    "release",
    "tmp",
    CACHEDIR_TAG_FILE,
    CARGO_RUSTC_INFO,
];

/// The file whose presence beside them makes `build/` and `.dart_tool/` a Flutter app's output.
pub const FLUTTER_MANIFEST: &str = "pubspec.yaml";

/// The output directories of a Flutter app (`worktree.archive.FlutterOutputKind`), recognised by
/// one classifier ([`flutter_output_kind`]) for `archive`, `finish --archive`, `discard-cache` and
/// `prune-archives --strip-build-output`. `P/<name>` is one only when `P/pubspec.yaml` is present
/// beside it as anything but a directory and nothing is tracked at `P/<name>`; on disk it must
/// also be a real directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FlutterOutputKind {
    /// `P/build/`, the output of `flutter build` and `flutter test`.
    Build,
    /// `P/.dart_tool/`, the package configuration and tool caches.
    DartTool,
}

impl FlutterOutputKind {
    /// Every kind.
    pub const ALL: [Self; 2] = [Self::Build, Self::DartTool];

    /// The directory's name beside `pubspec.yaml`.
    pub fn directory_name(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::DartTool => ".dart_tool",
        }
    }

    /// The kind a directory named `name` would be.
    pub fn named(name: &[u8]) -> Option<Self> {
        Self::ALL
            .into_iter()
            .find(|kind| kind.directory_name().as_bytes() == name)
    }
}

/// How a left-out file was found to be build layout (`worktree.archive.BuildOutputOrigin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildOutputOrigin {
    /// `archive` observed it on disk below a target `discard-cache` recognises, or below a
    /// Flutter app's output directory.
    Archive,
    /// `prune-archives --strip-build-output` removed its section from an existing patch.
    Strip,
}

/// Build layout left out below one cargo target or Flutter output directory
/// (`worktree.archive.BuildOutputTarget`). No field says which: removal re-recognises the
/// recorded directory on disk.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildOutputTarget {
    /// Target directory relative to the tree root.
    pub path: String,
    /// How the layout below it was found.
    pub origin: BuildOutputOrigin,
    /// Files left out.
    pub files: u64,
    /// Their content bytes: sizes on disk, or the sizes the stripped sections would recreate.
    pub bytes: u64,
}

/// Everything one archive left out, per target, with totals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildOutput {
    /// One entry per target path and origin, in path-byte then origin order.
    pub targets: Vec<BuildOutputTarget>,
    /// Files over every target.
    pub files: u64,
    /// Bytes over every target.
    pub bytes: u64,
}

impl BuildOutput {
    /// Merge `targets` into `existing`, summing entries of the same path and origin. `None` when
    /// nothing at all was left out, so an archive without build output keeps its old format.
    pub fn merged(existing: Option<&Self>, targets: Vec<BuildOutputTarget>) -> Option<Self> {
        let mut all: Vec<BuildOutputTarget> = Vec::new();
        for target in existing
            .map(|output| output.targets.clone())
            .unwrap_or_default()
            .into_iter()
            .chain(targets)
        {
            match all
                .iter_mut()
                .find(|item| item.path == target.path && item.origin == target.origin)
            {
                Some(item) => {
                    item.files += target.files;
                    item.bytes += target.bytes;
                }
                None => all.push(target),
            }
        }
        all.retain(|target| target.files > 0);
        if all.is_empty() {
            return None;
        }
        all.sort_by(|left, right| {
            (left.path.as_bytes(), left.origin).cmp(&(right.path.as_bytes(), right.origin))
        });
        Some(Self {
            files: all.iter().map(|target| target.files).sum(),
            bytes: all.iter().map(|target| target.bytes).sum(),
            targets: all,
        })
    }

    /// Why this record is malformed, if it is: no target, a target that is not a distinct
    /// relative path in order, an empty target, or totals that are not the sums.
    pub fn defect(&self) -> Option<String> {
        if self.targets.is_empty() {
            return Some("build_output names no target".into());
        }
        let mut previous: Option<(&[u8], BuildOutputOrigin)> = None;
        for target in &self.targets {
            let key = (target.path.as_bytes(), target.origin);
            if !relative_path(&target.path)
                || target.files == 0
                || previous.is_some_and(|previous| previous >= key)
            {
                return Some(format!(
                    "build_output target {:?} is not a distinct relative path in order with at \
                     least one file",
                    target.path
                ));
            }
            previous = Some(key);
        }
        let files = self.targets.iter().map(|target| target.files).sum::<u64>();
        let bytes = self.targets.iter().map(|target| target.bytes).sum::<u64>();
        (files != self.files || bytes != self.bytes)
            .then(|| "build_output totals are not the sums of its targets".into())
    }
}

/// Whether `path` is a non-empty relative path of plain components, none of them `.git`.
pub fn relative_path(path: &str) -> bool {
    !path.is_empty()
        && !path.starts_with('/')
        && !path.ends_with('/')
        && path
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".." | ".git"))
}

/// The part of `path` strictly below `directory`.
pub fn path_below<'a>(path: &'a [u8], directory: &[u8]) -> Option<&'a [u8]> {
    path.strip_prefix(directory)
        .and_then(|rest| rest.strip_prefix(b"/"))
        .filter(|rest| !rest.is_empty())
}

/// What is known of one recognised cargo target beyond its path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TargetFacts {
    /// `T/CACHEDIR.TAG` carries Cargo's signature: only then is it Cargo's own file.
    pub signed_tag: bool,
    /// `T` holds at least one profile (`T/<p>/` or `T/<triple>/<p>/`): only then is `T/tmp/`
    /// Cargo's test scratch, as `discard-cache` keeps a `tmp/` beside no profile.
    pub holds_profile: bool,
}

/// Whether `rest`, a path relative to a recognised cargo target described by `facts`, is build
/// layout.
///
/// `profile(prefix)` reports whether `prefix`, one or two components below the target, is a
/// profile directory: one holding `.fingerprint/` as a direct child.
pub fn is_build_layout(
    rest: &[u8],
    facts: TargetFacts,
    mut profile: impl FnMut(&[u8]) -> bool,
) -> bool {
    let components = rest.split(|byte| *byte == b'/').collect::<Vec<_>>();
    let Some(first) = components.first().filter(|first| !first.is_empty()) else {
        return false;
    };
    if *first == CARGO_TARGET_TMP.as_bytes() {
        return facts.holds_profile;
    }
    if *first == CACHEDIR_TAG_FILE.as_bytes() {
        return facts.signed_tag;
    }
    if CARGO_LAYOUT_NAMES
        .iter()
        .any(|name| name.as_bytes() == *first)
    {
        return true;
    }
    if components.len() >= 2 && profile(first) {
        return true;
    }
    if components.len() >= 3 && !components[1].is_empty() {
        let prefix = &rest[..first.len() + 1 + components[1].len()];
        return profile(prefix);
    }
    false
}

/// The index of the outermost target in `targets` below which `path` is build layout.
///
/// `facts(target)` describes a target, and `profile(target, prefix)` reports whether
/// `target/prefix` is a profile directory.
pub fn layout_target(
    path: &[u8],
    targets: &[Vec<u8>],
    mut facts: impl FnMut(&[u8]) -> TargetFacts,
    mut profile: impl FnMut(&[u8], &[u8]) -> bool,
) -> Option<usize> {
    let mut order = (0..targets.len()).collect::<Vec<_>>();
    order.sort_by(|left, right| {
        (targets[*left].len(), &targets[*left]).cmp(&(targets[*right].len(), &targets[*right]))
    });
    order.into_iter().find(|index| {
        let target = &targets[*index];
        path_below(path, target).is_some_and(|rest| {
            is_build_layout(rest, facts(target), |prefix| profile(target, prefix))
        })
    })
}

/// Decode one C-quoted string as Git writes a path (`quote_c_style`): the opening quote, then
/// escapes `\a \b \t \n \v \f \r \" \\` and three octal digits. Returns the bytes and the rest
/// after the closing quote, or `None` when `quoted` is not one well-formed quoted string.
pub fn c_unquote(quoted: &[u8]) -> Option<(Vec<u8>, &[u8])> {
    let mut rest = quoted.strip_prefix(b"\"")?;
    let mut out = Vec::new();
    loop {
        let (&byte, tail) = rest.split_first()?;
        rest = tail;
        match byte {
            b'"' => return Some((out, rest)),
            b'\\' => {
                let (&escape, tail) = rest.split_first()?;
                rest = tail;
                let decoded = match escape {
                    b'a' => 7,
                    b'b' => 8,
                    b't' => b'\t',
                    b'n' => b'\n',
                    b'v' => 11,
                    b'f' => 12,
                    b'r' => b'\r',
                    b'"' => b'"',
                    b'\\' => b'\\',
                    b'0'..=b'3' => {
                        let digits = [escape, *rest.first()?, *rest.get(1)?];
                        if !digits[1..]
                            .iter()
                            .all(|digit| (b'0'..=b'7').contains(digit))
                        {
                            return None;
                        }
                        rest = &rest[2..];
                        digits
                            .iter()
                            .fold(0u8, |value, digit| value * 8 + (digit - b'0'))
                    }
                    _ => return None,
                };
                out.push(decoded);
            }
            other => out.push(other),
        }
    }
}

/// The path a `diff --git a/<path> b/<path>` line names, when both sides decode to the same path.
/// `line` holds no trailing newline. Anything else, a rename included, is `None`.
pub fn diff_git_path(line: &[u8]) -> Option<Vec<u8>> {
    let rest = line.strip_prefix(b"diff --git ")?;
    if rest.starts_with(b"\"") {
        let (old, tail) = c_unquote(rest)?;
        let tail = tail.strip_prefix(b" ")?;
        let new = if tail.starts_with(b"\"") {
            let (new, end) = c_unquote(tail)?;
            if !end.is_empty() {
                return None;
            }
            new
        } else {
            tail.to_vec()
        };
        let old = old.strip_prefix(b"a/")?;
        let new = new.strip_prefix(b"b/")?;
        return (old == new && !old.is_empty()).then(|| old.to_vec());
    }
    // Unquoted, both sides are the same path: `a/<p> b/<p>` is 2·|p| + 5 bytes.
    let length = rest.len().checked_sub(5)?;
    if length == 0 || length % 2 != 0 {
        return None;
    }
    let path = rest.get(2..2 + length / 2)?;
    let mut expected = b"a/".to_vec();
    expected.extend_from_slice(path);
    expected.extend_from_slice(b" b/");
    expected.extend_from_slice(path);
    (expected == rest).then(|| path.to_vec())
}

/// Whether `line` is one of Git's extended header lines between `diff --git` and the content.
pub fn is_extended_header(line: &[u8]) -> bool {
    [
        &b"old mode "[..],
        b"new mode ",
        b"deleted file mode ",
        b"new file mode ",
        b"copy from ",
        b"copy to ",
        b"rename from ",
        b"rename to ",
        b"similarity index ",
        b"dissimilarity index ",
        b"index ",
    ]
    .iter()
    .any(|prefix| line.starts_with(prefix))
}

/// One `diff --git` section of an archive's patch, as the Git adapter streamed it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedSection {
    /// The decoded path, or `None` when the header could not be decoded.
    pub path: Option<Vec<u8>>,
    /// Whether its extended header holds `new file mode`. Alone this does not make the section a
    /// pure addition: a type change is a `deleted file mode` and a `new file mode` section of one
    /// path.
    pub new_file: bool,
    /// Whether HEAD's tree, read from the repository rather than the patch, holds the decoded
    /// path as a file or directory, or a file at one of its ancestors. Such a section is kept.
    pub tracked: bool,
    /// Whether the file the section adds begins with Cargo's `CACHEDIR.TAG` signature line.
    pub signed_tag: bool,
    /// Bytes of the section in the patch, header included.
    pub patch_bytes: u64,
    /// Bytes of the file a `new file mode` section recreates.
    pub content_bytes: u64,
}

/// An archive's patch as the Git adapter streamed it, with the paths of HEAD's tree read from Git
/// and never from the patch.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScannedPatch {
    /// Every `diff --git` section, in patch order.
    pub sections: Vec<ScannedSection>,
    /// The paths HEAD's tree holds as anything but a directory.
    pub head_files: BTreeSet<Vec<u8>>,
    /// The paths HEAD's tree holds as a directory.
    pub head_directories: BTreeSet<Vec<u8>>,
}

impl ScannedPatch {
    /// The patch of `sections` over a HEAD holding `head_files` and `head_directories`, each
    /// section marked [`ScannedSection::tracked`] when HEAD holds its path ([`Self::head_tracks`]).
    pub fn against_head(
        sections: Vec<ScannedSection>,
        head_files: BTreeSet<Vec<u8>>,
        head_directories: BTreeSet<Vec<u8>>,
    ) -> Self {
        let mut patch = Self {
            sections: Vec::new(),
            head_files,
            head_directories,
        };
        patch.sections = sections
            .into_iter()
            .map(|mut section| {
                section.tracked = section
                    .path
                    .as_deref()
                    .is_some_and(|path| patch.head_tracks(path));
                section
            })
            .collect();
        patch
    }

    /// Whether HEAD's tree holds `path` as a file or a directory.
    pub fn head_holds(&self, path: &[u8]) -> bool {
        self.head_files.contains(path) || self.head_directories.contains(path)
    }

    /// Whether HEAD's tree holds `path` as a file or directory, or a file at one of its
    /// ancestors.
    pub fn head_tracks(&self, path: &[u8]) -> bool {
        self.head_holds(path)
            || path
                .iter()
                .enumerate()
                .filter(|(_, byte)| **byte == b'/')
                .any(|(at, _)| self.head_files.contains(&path[..at]))
    }
}

/// Content bytes of the file one `new file mode` section adds, accumulated line by line: the
/// `literal <n>` size of a binary section, or the added lines of a text one. It also notes
/// whether a text file's first line is Cargo's `CACHEDIR.TAG` signature.
#[derive(Debug, Clone, Default)]
pub struct NewFileSize {
    binary: bool,
    literal: Option<u64>,
    in_hunk: bool,
    after_added: bool,
    text_bytes: u64,
    /// Whether the first line of the first hunk was observed, and whether it was an added
    /// signature line.
    first_line: Option<bool>,
}

impl NewFileSize {
    /// Observe one line of the section after its `diff --git` line, newline included.
    pub fn observe(&mut self, line: &[u8]) {
        if self.binary {
            if self.literal.is_none() {
                self.literal = line
                    .strip_prefix(b"literal ")
                    .and_then(|size| std::str::from_utf8(size).ok())
                    .and_then(|size| size.trim().parse().ok());
            }
            return;
        }
        if line.starts_with(b"GIT binary patch") {
            self.binary = true;
            return;
        }
        if line.starts_with(b"@@") {
            self.in_hunk = true;
            return;
        }
        if !self.in_hunk {
            return;
        }
        if self.first_line.is_none() {
            self.first_line = Some(line.strip_prefix(b"+").is_some_and(is_cache_tag));
        }
        if line.starts_with(b"\\") {
            if self.after_added {
                self.text_bytes = self.text_bytes.saturating_sub(1);
            }
            self.after_added = false;
        } else if line.starts_with(b"+") {
            self.text_bytes += line.len() as u64 - 1;
            self.after_added = true;
        } else {
            self.after_added = false;
        }
    }

    /// The file's size.
    pub fn bytes(&self) -> u64 {
        self.literal.unwrap_or(self.text_bytes)
    }

    /// Whether the text the section adds begins with Cargo's `CACHEDIR.TAG` signature. A binary
    /// section never does.
    pub fn signed_tag(&self) -> bool {
        !self.binary && self.first_line == Some(true)
    }
}

/// Which sections of a patch a strip removes, and what it leaves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchStripPlan {
    /// One flag per section, in patch order: `true` when the section is stripped.
    pub strip: Vec<bool>,
    /// The layout stripped, per target, origin [`BuildOutputOrigin::Strip`].
    pub targets: Vec<BuildOutputTarget>,
    /// Sections removed.
    pub sections_stripped: u64,
    /// Sections kept.
    pub sections_kept: u64,
    /// Patch bytes removed.
    pub strip_bytes: u64,
    /// Patch bytes kept.
    pub kept_bytes: u64,
}

/// The cargo targets a patch's own pure additions recognise.
struct PatchTargets {
    /// Targets that are UTF-8 relative paths, in path-byte order.
    targets: Vec<Vec<u8>>,
    /// Profile directories: each holds an added `.fingerprint/…` file.
    profiles: BTreeSet<Vec<u8>>,
    /// Directories holding an added, Cargo-signed `CACHEDIR.TAG`.
    tagged: BTreeSet<Vec<u8>>,
}

impl PatchTargets {
    /// Recognise targets from added paths, each with whether its content is a signed tag: a
    /// signed `T/CACHEDIR.TAG`, or `T/<p>/.fingerprint/…`.
    fn recognise(added: &[(&[u8], bool)]) -> Self {
        let mut profiles = BTreeSet::new();
        let mut targets = BTreeSet::new();
        let mut tagged = BTreeSet::new();
        for (path, signed) in added {
            let components = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
            let prefix = |count: usize| components[..count].join(&b'/');
            let last = components.len() - 1;
            if last >= 1 && *signed && components[last] == CACHEDIR_TAG_FILE.as_bytes() {
                targets.insert(prefix(last));
                tagged.insert(prefix(last));
            }
            for (index, component) in components.iter().enumerate() {
                if *component == CARGO_PROFILE_MARKER.as_bytes() && index >= 1 && index < last {
                    profiles.insert(prefix(index));
                    if index >= 2 {
                        targets.insert(prefix(index - 1));
                    }
                }
            }
        }
        Self {
            targets: targets
                .into_iter()
                .filter(|target| std::str::from_utf8(target).is_ok_and(relative_path))
                .collect(),
            profiles,
            tagged,
        }
    }

    /// What the patch shows of `target`: a signed tag, and a profile one or two levels below.
    fn facts(&self, target: &[u8]) -> TargetFacts {
        TargetFacts {
            signed_tag: self.tagged.contains(target),
            holds_profile: self.profiles.iter().any(|profile| {
                path_below(profile, target)
                    .is_some_and(|rest| rest.split(|byte| *byte == b'/').count() <= 2)
            }),
        }
    }

    /// Whether `target/prefix` is a profile the patch adds a `.fingerprint/` file to.
    fn profile(&self, target: &[u8], prefix: &[u8]) -> bool {
        let mut directory = target.to_vec();
        directory.push(b'/');
        directory.extend_from_slice(prefix);
        self.profiles.contains(&directory)
    }
}

/// What a caller observed of one candidate Flutter output directory `P/<name>`, the input of the
/// one classifier ([`flutter_output_kind`]). Only the source differs: `archive` and
/// `discard-cache` observe the disk and the index, the strip observes HEAD's tree and the patch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FlutterFacts {
    /// `P/pubspec.yaml` is present beside the directory as anything but a directory: on disk a
    /// regular file, tracked or not; for the strip, a path HEAD's tree holds as anything but a
    /// directory, or a `pubspec.yaml` the patch purely adds.
    pub manifest_beside: bool,
    /// Something is tracked at the directory's path, as a file or a directory: on disk, HEAD's
    /// tree or the index holds the path or anything below it; for the strip, HEAD's tree holds
    /// it.
    pub tracked: bool,
    /// The directory is itself a cargo target recognised from the same source: on disk by
    /// `archive`'s structural rule, in a patch by the patch's own pure additions. Such a
    /// directory is classified by the cargo rule alone, so a file below it that is no cargo
    /// layout stays archived, as `discard-cache` keeps it.
    pub cargo_target: bool,
}

impl FlutterFacts {
    /// Whether these observations make the directory a Flutter app's output.
    pub fn recognises(self) -> bool {
        self.manifest_beside && !self.tracked && !self.cargo_target
    }
}

/// Whether `directory`, a path relative to the tree root, is a Flutter app's output directory
/// (`worktree.archive.FlutterOutputKind`), and which.
///
/// `directory` must be a UTF-8 relative path whose last component is `build` or `.dart_tool`;
/// its parent `P` is the app root, the tree root included. `observe(directory, manifest)` reports
/// what its caller's source shows of `directory` and of `manifest`, `P/pubspec.yaml`, or `None`
/// when that source holds no such directory at all (on disk: a symbolic link or anything but a
/// real directory). It is called only for a candidate that passed the name and path checks.
pub fn flutter_output_kind(
    directory: &[u8],
    observe: impl FnOnce(&[u8], &[u8]) -> Option<FlutterFacts>,
) -> Option<FlutterOutputKind> {
    let name = directory
        .rsplit(|byte| *byte == b'/')
        .next()
        .unwrap_or(directory);
    let kind = FlutterOutputKind::named(name)?;
    if !std::str::from_utf8(directory).is_ok_and(relative_path) {
        return None;
    }
    let mut manifest = directory[..directory.len() - name.len()].to_vec();
    manifest.extend_from_slice(FLUTTER_MANIFEST.as_bytes());
    observe(directory, &manifest)
        .is_some_and(FlutterFacts::recognises)
        .then_some(kind)
}

/// The outermost Flutter output directory ([`flutter_output_kind`]) strictly below which `path`
/// lies, if any. Every ancestor of `path` named `build` or `.dart_tool` is a candidate, outermost
/// first, and `observe` is asked about each in turn.
pub fn flutter_output(
    path: &[u8],
    mut observe: impl FnMut(&[u8], &[u8]) -> Option<FlutterFacts>,
) -> Option<&[u8]> {
    path.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(|(at, _)| &path[..at])
        .filter(|directory| path_below(path, directory).is_some())
        .find(|directory| flutter_output_kind(directory, &mut observe).is_some())
}

/// Of the cargo target and the Flutter output directory a path lies below, the outer one: both
/// are its ancestors, so the shorter is the outer. They are never one directory: the classifier
/// never recognises a cargo target as a Flutter app's output ([`FlutterFacts::cargo_target`]).
pub fn outermost_build_directory<'a>(
    cargo: Option<&'a [u8]>,
    flutter: Option<&'a [u8]>,
) -> Option<&'a [u8]> {
    match (cargo, flutter) {
        (Some(cargo), Some(flutter)) => Some(if flutter.len() < cargo.len() {
            flutter
        } else {
            cargo
        }),
        (cargo, flutter) => cargo.or(flutter),
    }
}

/// Whether `path`, relative to the tree root, is repository content: it has a [`GIT_ENTRY`]
/// component, or it or one of its ancestors below the tree root is a Git directory.
///
/// `git_directory(directory)` reports what its caller's source shows of `directory`: on disk,
/// [`GitDirectoryFacts::recognises`] observed without following a symbolic link; in a patch, a
/// directory for which the patch purely adds `HEAD` and some path under `objects/`. Such a path
/// is never build layout, cargo's or a Flutter app's: a repository there may hold commits no
/// remote does, so `archive` keeps it, removal never deletes it as layout, and the strip keeps
/// its section.
pub fn repository_path(path: &[u8], git_directory: impl FnMut(&[u8]) -> bool) -> bool {
    if path
        .split(|byte| *byte == b'/')
        .any(|component| component == GIT_ENTRY.as_bytes())
    {
        return true;
    }
    path.iter()
        .enumerate()
        .filter(|(_, byte)| **byte == b'/')
        .map(|(at, _)| &path[..at])
        .chain(std::iter::once(path))
        .filter(|directory| !directory.is_empty())
        .any(git_directory)
}

/// The directory `path` is build layout below, from the cargo target and Flutter output
/// directory it lies below ([`outermost_build_directory`]), unless it is repository content
/// ([`repository_path`]), which is never build layout. `git_directory` is asked only when `path`
/// lies below one.
pub fn layout_directory<'a>(
    path: &[u8],
    cargo: Option<&'a [u8]>,
    flutter: Option<&'a [u8]>,
    git_directory: impl FnMut(&[u8]) -> bool,
) -> Option<&'a [u8]> {
    let directory = outermost_build_directory(cargo, flutter)?;
    (!repository_path(path, git_directory)).then_some(directory)
}

/// The Git directories a patch's own pure additions show: each directory `D` for which the patch
/// purely adds `D/HEAD` and at least one path under `D/objects/`. A patch holds no empty
/// directory, so `refs/` cannot be required of it.
fn patch_git_directories(added: &[(&[u8], bool)]) -> BTreeSet<Vec<u8>> {
    let mut heads = BTreeSet::new();
    let mut objects = BTreeSet::new();
    for (path, _) in added {
        let components = path.split(|byte| *byte == b'/').collect::<Vec<_>>();
        let last = components.len() - 1;
        let prefix = |count: usize| components[..count].join(&b'/');
        if last >= 1 && components[last] == GIT_DIRECTORY_HEAD.as_bytes() {
            heads.insert(prefix(last));
        }
        for (index, component) in components.iter().enumerate().take(last).skip(1) {
            if *component == GIT_DIRECTORY_OBJECTS.as_bytes() {
                objects.insert(prefix(index));
            }
        }
    }
    heads.intersection(&objects).cloned().collect()
}

/// Decide which sections of a patch are build layout: cargo's or a Flutter app's.
///
/// Only a pure addition is stripped: a `new file mode` section whose path decoded, that no other
/// section names, and that HEAD does not track ([`ScannedSection::tracked`]); and only when that
/// path is build layout below a directory the patch's own pure additions recognise as a cargo
/// target, by a Cargo-signed `CACHEDIR.TAG` or a profile's `.fingerprint/`, or strictly below a
/// Flutter app's output directory ([`FlutterOutputKind`]) beside a `pubspec.yaml` HEAD holds or
/// the patch purely adds and that is no cargo target the patch recognises. A path with a `.git`
/// component, or at or below a Git directory the patch's own pure additions show
/// ([`repository_path`]), is never stripped. The outermost such directory is recorded. Every other section is kept whole: a type change of a tracked path is a
/// `deleted file mode` and a `new file mode` section of one path, and both halves stay.
pub fn plan_patch_strip(patch: &ScannedPatch) -> PatchStripPlan {
    let sections = patch.sections.as_slice();
    // Per decoded path: how many sections name it, and whether every one is a `new file mode`.
    let mut named: BTreeMap<&[u8], (usize, bool)> = BTreeMap::new();
    for section in sections {
        if let Some(path) = section.path.as_deref() {
            let entry = named.entry(path).or_insert((0, true));
            entry.0 += 1;
            entry.1 &= section.new_file;
        }
    }
    let pure_addition = |section: &ScannedSection| {
        section.new_file
            && !section.tracked
            && section
                .path
                .as_deref()
                .is_some_and(|path| named.get(path) == Some(&(1, true)))
    };
    let added = sections
        .iter()
        .filter(|section| pure_addition(section))
        .filter_map(|section| {
            section
                .path
                .as_deref()
                .map(|path| (path, section.signed_tag))
        })
        .collect::<Vec<_>>();
    let added_manifests = added
        .iter()
        .map(|(path, _)| *path)
        .filter(|path| {
            path.rsplit(|byte| *byte == b'/').next() == Some(FLUTTER_MANIFEST.as_bytes())
        })
        .collect::<BTreeSet<_>>();
    let recognised = PatchTargets::recognise(&added);
    let targets = &recognised.targets;
    let git_directories = patch_git_directories(&added);
    let mut plan = PatchStripPlan {
        strip: Vec::with_capacity(sections.len()),
        targets: Vec::new(),
        sections_stripped: 0,
        sections_kept: 0,
        strip_bytes: 0,
        kept_bytes: 0,
    };
    // Files and bytes per recorded directory, in path-byte order.
    let mut per_target: BTreeMap<&[u8], (u64, u64)> = BTreeMap::new();
    for section in sections {
        let target = section
            .path
            .as_deref()
            .filter(|_| pure_addition(section))
            .and_then(|path| {
                let cargo = layout_target(
                    path,
                    targets,
                    |target| recognised.facts(target),
                    |target, prefix| recognised.profile(target, prefix),
                )
                .map(|index| targets[index].as_slice());
                // The patch's observations: HEAD's tree read from Git, and the manifests the
                // patch purely adds.
                let flutter = flutter_output(path, |directory, manifest| {
                    Some(FlutterFacts {
                        manifest_beside: patch.head_files.contains(manifest)
                            || added_manifests.contains(manifest),
                        tracked: patch.head_holds(directory),
                        cargo_target: targets.iter().any(|target| target == directory),
                    })
                });
                layout_directory(path, cargo, flutter, |directory| {
                    git_directories.contains(directory)
                })
            });
        plan.strip.push(target.is_some());
        if let Some(target) = target {
            plan.sections_stripped += 1;
            plan.strip_bytes += section.patch_bytes;
            let entry = per_target.entry(target).or_default();
            entry.0 += 1;
            entry.1 += section.content_bytes;
        } else {
            plan.sections_kept += 1;
            plan.kept_bytes += section.patch_bytes;
        }
    }
    plan.targets = per_target
        .into_iter()
        .map(|(target, (files, bytes))| BuildOutputTarget {
            path: String::from_utf8_lossy(target).into_owned(),
            origin: BuildOutputOrigin::Strip,
            files,
            bytes,
        })
        .collect();
    plan
}

/// The verdict `prune-archives --strip-build-output` gives one archive directory
/// (`worktree.archive.StripVerdict`). Only [`StripVerdict::Strippable`] is ever rewritten.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum StripVerdict {
    /// At least one `new file mode` section adds a file under build layout.
    Strippable,
    /// No patch, or nothing in it is build layout.
    NothingToStrip,
    /// `manifest.json` is missing, unreadable or of an unknown format.
    InvalidManifest,
    /// The directory holds an entry the manifest does not name.
    UnrecordedContent,
    /// The registered tree path exists; the archive may be its recovery proof.
    TreeStillPresent,
    /// The repository is gone, so a new patch cannot be verified over HEAD.
    RepositoryGone,
    /// `dirty.patch` lost its recorded digest, holds bytes before its first section, or the
    /// stripped patch does not apply over HEAD.
    PatchUnusable,
}

impl StripVerdict {
    /// The variant's specification name, as human-readable output prints it.
    pub fn name(self) -> &'static str {
        match self {
            Self::Strippable => "Strippable",
            Self::NothingToStrip => "NothingToStrip",
            Self::InvalidManifest => "InvalidManifest",
            Self::UnrecordedContent => "UnrecordedContent",
            Self::TreeStillPresent => "TreeStillPresent",
            Self::RepositoryGone => "RepositoryGone",
            Self::PatchUnusable => "PatchUnusable",
        }
    }
}

/// The strip decision for one archive directory, before anything is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StripDecision {
    /// The verdict.
    pub verdict: StripVerdict,
    /// Why, in words.
    pub reason: String,
    /// The sections to strip, for a patch that was scanned.
    pub plan: Option<PatchStripPlan>,
}

/// The local checks pruning and stripping share, in the specification's order.
pub(crate) enum LocalRefusal {
    InvalidManifest,
    UnrecordedContent,
    TreeStillPresent,
}

/// Refuse an archive whose manifest is invalid, whose directory holds anything the manifest does
/// not name as a regular file, or whose registered tree may still exist.
pub(crate) fn require_local(
    contents: &ArchiveContents,
    tree_present: impl FnOnce(&ArchiveManifest) -> Result<bool, String>,
) -> Result<&ArchiveManifest, (LocalRefusal, String)> {
    let manifest = match &contents.manifest {
        Ok(manifest) => manifest,
        Err(reason) => return Err((LocalRefusal::InvalidManifest, reason.clone())),
    };
    let recorded = manifest.recorded_files();
    let mut unrecorded = contents
        .entries
        .iter()
        .filter(|entry| {
            entry.kind != ArchiveEntryKind::File || !recorded.contains(&entry.name.as_str())
        })
        .map(|entry| entry.name.as_str())
        .collect::<Vec<_>>();
    if !unrecorded.is_empty() {
        unrecorded.sort_unstable();
        return Err((
            LocalRefusal::UnrecordedContent,
            format!(
                "holds {} that the manifest does not record as a file",
                unrecorded.join(", ")
            ),
        ));
    }
    match tree_present(manifest) {
        Ok(false) => Ok(manifest),
        Ok(true) => Err((
            LocalRefusal::TreeStillPresent,
            format!(
                "the registered tree {} still exists",
                manifest.path.display()
            ),
        )),
        Err(error) => Err((
            LocalRefusal::TreeStillPresent,
            format!(
                "the registered tree {} could not be observed absent: {error}",
                manifest.path.display()
            ),
        )),
    }
}

/// Decide whether one archive's patch may be stripped, checking in the specification's order:
/// [`StripVerdict::InvalidManifest`], [`StripVerdict::UnrecordedContent`],
/// [`StripVerdict::TreeStillPresent`], [`StripVerdict::RepositoryGone`], then the patch.
///
/// `repository_gone` is asked only once the local checks passed, and `scan` only for a manifest
/// that records a patch; a scan error is [`StripVerdict::PatchUnusable`].
pub fn decide_archive_strip(
    contents: &ArchiveContents,
    tree_present: impl FnOnce(&ArchiveManifest) -> Result<bool, String>,
    repository_gone: impl FnOnce(&ArchiveManifest) -> Result<bool, String>,
    scan: impl FnOnce(&ArchiveManifest) -> Result<ScannedPatch, String>,
) -> StripDecision {
    let decision = |verdict, reason: String| StripDecision {
        verdict,
        reason,
        plan: None,
    };
    let manifest = match require_local(contents, tree_present) {
        Ok(manifest) => manifest,
        Err((refusal, reason)) => {
            let verdict = match refusal {
                LocalRefusal::InvalidManifest => StripVerdict::InvalidManifest,
                LocalRefusal::UnrecordedContent => StripVerdict::UnrecordedContent,
                LocalRefusal::TreeStillPresent => StripVerdict::TreeStillPresent,
            };
            return decision(verdict, reason);
        }
    };
    match repository_gone(manifest) {
        Ok(false) => {}
        Ok(true) => {
            return decision(
                StripVerdict::RepositoryGone,
                format!("repository {} is gone", manifest.repository_root.display()),
            );
        }
        Err(error) => return decision(StripVerdict::RepositoryGone, error),
    }
    if manifest.patch.is_none() {
        return decision(StripVerdict::NothingToStrip, "records no patch".into());
    }
    let patch = match scan(manifest) {
        Ok(patch) => patch,
        Err(error) => return decision(StripVerdict::PatchUnusable, error),
    };
    let plan = plan_patch_strip(&patch);
    if plan.sections_stripped == 0 {
        return StripDecision {
            verdict: StripVerdict::NothingToStrip,
            reason: "no section of the patch adds build layout (cargo or Flutter)".into(),
            plan: Some(plan),
        };
    }
    StripDecision {
        verdict: StripVerdict::Strippable,
        reason: format!(
            "{} section(s) add build layout below {}",
            plan.sections_stripped,
            plan.targets
                .iter()
                .map(|target| target.path.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        plan: Some(plan),
    }
}

/// The directory as it would be after `plan` were applied: no `dirty.patch` when nothing remains,
/// otherwise one of the kept size. Only what the prune decision reads is changed.
pub fn contents_after_strip(contents: &ArchiveContents, plan: &PatchStripPlan) -> ArchiveContents {
    let mut after = contents.clone();
    let remains = plan.sections_kept > 0;
    if let Ok(manifest) = after.manifest.as_mut() {
        if remains {
            if let Some(patch) = manifest.patch.as_mut() {
                patch.bytes = plan.kept_bytes;
            }
        } else {
            manifest.patch = None;
        }
    }
    if remains {
        for entry in &mut after.entries {
            if entry.name == ARCHIVE_PATCH_FILE {
                entry.bytes = plan.kept_bytes;
            }
        }
    } else {
        after
            .entries
            .retain(|entry| entry.name != ARCHIVE_PATCH_FILE);
    }
    after
}

/// The report of one archive directory assessed by `prune-archives --strip-build-output`
/// (`worktree.archive.ArchiveStripAssessment`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveStripAssessment {
    /// Directory name: `<id>` or a superseded `<id>.superseded-<n>`.
    pub archive_directory: String,
    /// Absolute path of the archive directory.
    pub path: PathBuf,
    /// The manifest's worktree id; absent when the manifest is invalid.
    pub worktree_id: Option<String>,
    /// The manifest's repository root; absent when the manifest is invalid.
    pub repository_root: Option<PathBuf>,
    /// Bytes of the regular files directly in the directory, before any rewrite.
    pub bytes: u64,
    /// The verdict.
    pub verdict: StripVerdict,
    /// Why, in words; under `--apply`, a rewrite that was refused says so here.
    pub reason: String,
    /// Patch bytes the strip removes.
    pub strip_bytes: u64,
    /// Sections the strip removes.
    pub sections_stripped: u64,
    /// Sections the strip keeps.
    pub sections_kept: u64,
    /// Bytes of the patch that stays; 0 when nothing remains.
    pub kept_patch_bytes: u64,
    /// Nested repository images, never touched.
    pub nested_images: u64,
    /// The prune verdict the archive has after the strip; absent for a refusal.
    pub prune_verdict_after: Option<PruneVerdict>,
    /// True only under `--apply`, for a strippable archive that was rewritten.
    pub applied: bool,
    /// Bytes the rewrite freed; 0 unless applied.
    pub freed_bytes: u64,
}

/// Totals of one strip run.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct ArchiveStripTotals {
    /// Archive directories assessed.
    pub archives: u64,
    /// Patch bytes the strippable archives would lose.
    pub strip_bytes: u64,
    /// Bytes freed under `--apply`.
    pub freed_bytes: u64,
}

/// The report of one `prune-archives --strip-build-output` run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveStripReport {
    /// Whether this was an `--apply` run.
    pub applied: bool,
    /// One assessment per archive directory in the selection.
    pub archives: Vec<ArchiveStripAssessment>,
    /// Entries below the archive root that are not archive directories.
    pub skipped: Vec<SkippedArchiveEntry>,
    /// Totals over [`Self::archives`].
    pub totals: ArchiveStripTotals,
}

impl ArchiveStripReport {
    /// Build a report and its totals.
    pub fn new(
        applied: bool,
        archives: Vec<ArchiveStripAssessment>,
        skipped: Vec<SkippedArchiveEntry>,
    ) -> Self {
        let mut totals = ArchiveStripTotals {
            archives: archives.len() as u64,
            ..ArchiveStripTotals::default()
        };
        for item in &archives {
            if item.verdict == StripVerdict::Strippable {
                totals.strip_bytes += item.strip_bytes;
            }
            totals.freed_bytes += item.freed_bytes;
        }
        Self {
            applied,
            archives,
            skipped,
            totals,
        }
    }

    /// Archives an `--apply` run selected but did not rewrite.
    pub fn refused(&self) -> Vec<&ArchiveStripAssessment> {
        if !self.applied {
            return Vec::new();
        }
        self.archives.iter().filter(|item| !item.applied).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn section(path: &str, new_file: bool) -> ScannedSection {
        ScannedSection {
            path: Some(path.as_bytes().to_vec()),
            new_file,
            tracked: false,
            signed_tag: false,
            patch_bytes: 10,
            content_bytes: 3,
        }
    }

    /// The plan for `sections` over an empty HEAD, each section's `tracked` as given.
    fn strip_plan(sections: &[ScannedSection]) -> PatchStripPlan {
        plan_patch_strip(&ScannedPatch {
            sections: sections.to_vec(),
            ..ScannedPatch::default()
        })
    }

    /// The plan for `sections` over a HEAD holding `files` and `directories`.
    fn plan_over_head(
        sections: &[ScannedSection],
        files: &[&str],
        directories: &[&str],
    ) -> PatchStripPlan {
        let set = |paths: &[&str]| paths.iter().map(|path| path.as_bytes().to_vec()).collect();
        plan_patch_strip(&ScannedPatch::against_head(
            sections.to_vec(),
            set(files),
            set(directories),
        ))
    }

    fn strip_target(path: &str, files: u64) -> BuildOutputTarget {
        BuildOutputTarget {
            path: path.into(),
            origin: BuildOutputOrigin::Strip,
            files,
            bytes: files * 3,
        }
    }

    /// A Flutter app at the tree root: `build/` and `.dart_tool/` beside a tracked
    /// `pubspec.yaml` are stripped whatever they hold, the app's source is kept.
    #[test]
    fn flutter_output_beside_a_pubspec_is_stripped() {
        let sections = [
            section("lib/main.dart", false),
            section(".dart_tool/package_config.json", true),
            section(".dart_tool/flutter_build/one/app.dill", true),
            section("build/app/outputs/flutter-apk/app-release.apk", true),
            section("build/unit_test_assets/AssetManifest.bin", true),
            section("notes.txt", true),
        ];
        let plan = plan_over_head(&sections, &["pubspec.yaml", "lib/main.dart"], &["lib"]);
        assert_eq!(
            plan.strip,
            [false, true, true, true, true, false],
            "{plan:?}"
        );
        assert_eq!(plan.sections_stripped, 4);
        assert_eq!(plan.sections_kept, 2);
        assert_eq!(
            plan.targets,
            [strip_target(".dart_tool", 2), strip_target("build", 2)]
        );

        // A nested app root: only its own outputs, never a sibling's `build/`.
        let sections = [
            section("app/build/web/main.dart.js", true),
            section("app/.dart_tool/package_config.json", true),
            section("other/build/out.bin", true),
            section("build/out.bin", true),
        ];
        let plan = plan_over_head(&sections, &["app/pubspec.yaml"], &["app"]);
        assert_eq!(plan.strip, [true, true, false, false], "{plan:?}");
        assert_eq!(
            plan.targets,
            [
                strip_target("app/.dart_tool", 1),
                strip_target("app/build", 1)
            ]
        );
    }

    /// Without a `pubspec.yaml` beside them, `build/` and `.dart_tool/` are ordinary
    /// directories; a `pubspec.yaml` HEAD holds as a directory, or one level up, does not count.
    #[test]
    fn flutter_output_without_a_pubspec_beside_is_kept() {
        let sections = [
            section("build/out.bin", true),
            section(".dart_tool/package_config.json", true),
            section("app/build/out.bin", true),
            section("app/lib/build/out.bin", true),
            section("tool/build/out.bin", true),
        ];
        let plan = plan_over_head(
            &sections,
            &["app/pubspec.yaml", "README.md"],
            &["app", "tool", "tool/pubspec.yaml"],
        );
        assert_eq!(plan.strip, [false, false, true, false, false], "{plan:?}");
        let plan = strip_plan(&sections);
        assert_eq!(plan.sections_stripped, 0, "{plan:?}");
    }

    /// A `build` HEAD holds, as a directory or as a file, is the app's own source: nothing
    /// below it is output, and a file `build` added purely is not below an output directory.
    #[test]
    fn a_tracked_build_directory_is_never_flutter_output() {
        let sections = [
            section("build/generated.dart", true),
            section(".dart_tool/package_config.json", true),
        ];
        let plan = plan_over_head(&sections, &["pubspec.yaml", "build/build.yaml"], &["build"]);
        assert_eq!(plan.strip, [false, true], "{plan:?}");
        assert_eq!(plan.targets, [strip_target(".dart_tool", 1)]);

        // HEAD holds `build` as a file: a section below it is tracked, and kept.
        let plan = plan_over_head(
            &[section("build/out.bin", true)],
            &["pubspec.yaml", "build"],
            &[],
        );
        assert_eq!(plan.strip, [false], "{plan:?}");

        // A file `build` added purely is not strictly below an output directory.
        let plan = plan_over_head(
            &[section("build", true), section(".dart_tool", true)],
            &["pubspec.yaml"],
            &[],
        );
        assert_eq!(plan.strip, [false, false], "{plan:?}");
    }

    /// A `pubspec.yaml` the patch purely adds recognises its app root, and its own section is
    /// kept; one that is not a pure addition recognises nothing.
    #[test]
    fn a_pubspec_the_patch_adds_recognises_its_app_root() {
        let sections = [
            section("example_app/pubspec.yaml", true),
            section("example_app/lib/main.dart", true),
            section("example_app/build/out.bin", true),
            section("example_app/.dart_tool/version", true),
        ];
        let plan = plan_over_head(&sections, &["README.md"], &[]);
        assert_eq!(plan.strip, [false, false, true, true], "{plan:?}");
        assert_eq!(
            plan.targets,
            [
                strip_target("example_app/.dart_tool", 1),
                strip_target("example_app/build", 1)
            ]
        );

        // Named twice, or not a `new file mode`: no app root.
        for pubspec in [
            vec![
                section("example_app/pubspec.yaml", true),
                section("example_app/pubspec.yaml", true),
            ],
            vec![section("example_app/pubspec.yaml", false)],
        ] {
            let mut sections = pubspec;
            sections.push(section("example_app/build/out.bin", true));
            let plan = plan_over_head(&sections, &[], &[]);
            assert_eq!(plan.sections_stripped, 0, "{plan:?}");
        }
    }

    /// The outermost directory whose rule strips the path, cargo target or Flutter output, is
    /// recorded: a cargo target inside `build/` counts as `build`.
    #[test]
    fn the_outermost_of_cargo_targets_and_flutter_outputs_is_recorded() {
        let sections = [
            section("build/rust/target/debug/.fingerprint/one/lib", true),
            section("build/rust/target/debug/deps/libone.rlib", true),
            section("build/app.dill", true),
            section("native/target/debug/.fingerprint/one/lib", true),
            section("native/target/debug/deps/libone.rlib", true),
        ];
        let plan = plan_over_head(&sections, &["pubspec.yaml"], &["native"]);
        assert_eq!(plan.strip, [true, true, true, true, true], "{plan:?}");
        assert_eq!(
            plan.targets,
            [strip_target("build", 3), strip_target("native/target", 2)]
        );
        assert_eq!(
            FlutterOutputKind::named(b".dart_tool"),
            Some(FlutterOutputKind::DartTool)
        );
        assert_eq!(FlutterOutputKind::named(b"target"), None);
    }

    /// A directory that is both a recognised cargo target and a Flutter app's output directory is
    /// classified by the cargo rule alone: only cargo's layout below it is stripped, and a file
    /// that is no layout (`build/notes.md`) stays, as `discard-cache` keeps it.
    #[test]
    fn a_cargo_target_that_is_flutter_output_too_is_classified_by_cargo_alone() {
        let mut tag = section("build/CACHEDIR.TAG", true);
        tag.signed_tag = true;
        let sections = [
            tag,
            section("build/debug/.fingerprint/one/lib", true),
            section("build/debug/deps/libone.rlib", true),
            section("build/notes.md", true),
            section(".dart_tool/package_config.json", true),
        ];
        let plan = plan_over_head(&sections, &["pubspec.yaml", "Cargo.toml"], &[]);
        assert_eq!(plan.strip, [true, true, true, false, true], "{plan:?}");
        assert_eq!(
            plan.targets,
            [strip_target(".dart_tool", 1), strip_target("build", 3)]
        );

        // On disk the same: `build` is a recognised cargo target beside a `pubspec.yaml`, so the
        // classifier recognises no Flutter output at it, and the cargo target is the one directory
        // a path below it can be layout of. `.dart_tool` beside it still is Flutter output.
        let mut disk = Disk::new(
            &["pubspec.yaml", "Cargo.toml", "build/notes.md"],
            &["build", "build/debug", ".dart_tool"],
            &["pubspec.yaml", "Cargo.toml"],
        );
        disk.cargo_targets.insert(b"build".to_vec());
        let observe = |directory: &[u8], manifest: &[u8]| disk.observe(directory, manifest);
        assert_eq!(flutter_output_kind(b"build", observe), None);
        assert_eq!(flutter_output(b"build/notes.md", observe), None);
        assert_eq!(
            flutter_output_kind(b".dart_tool", observe),
            Some(FlutterOutputKind::DartTool)
        );
        let targets = [b"build".to_vec()];
        let cargo = |path: &[u8]| {
            layout_target(
                path,
                &targets,
                |_| TargetFacts {
                    signed_tag: true,
                    holds_profile: true,
                },
                |_, prefix| prefix == b"debug",
            )
            .map(|index| targets[index].as_slice())
        };
        for (path, expected) in [
            (&b"build/notes.md"[..], None),
            (b"build/debug/deps/libone.rlib", Some(&b"build"[..])),
        ] {
            assert_eq!(
                outermost_build_directory(cargo(path), flutter_output(path, observe)),
                expected,
                "{}",
                String::from_utf8_lossy(path)
            );
        }
    }

    /// A tree as `archive` and `discard-cache` would observe it on disk: the regular files and
    /// real directories present, and every path HEAD or the index tracks.
    struct Disk {
        files: BTreeSet<Vec<u8>>,
        directories: BTreeSet<Vec<u8>>,
        tracked: BTreeSet<Vec<u8>>,
        /// Directories recognised on disk as cargo targets.
        cargo_targets: BTreeSet<Vec<u8>>,
    }

    impl Disk {
        fn new(files: &[&str], directories: &[&str], tracked: &[&str]) -> Self {
            let set = |paths: &[&str]| paths.iter().map(|path| path.as_bytes().to_vec()).collect();
            Self {
                files: set(files),
                directories: set(directories),
                tracked: set(tracked),
                cargo_targets: BTreeSet::new(),
            }
        }

        /// The observations `archive` supplies: `None` unless `directory` is a real directory;
        /// the manifest as a regular file; anything tracked at or below the directory; whether
        /// the directory is itself a recognised cargo target.
        fn observe(&self, directory: &[u8], manifest: &[u8]) -> Option<FlutterFacts> {
            self.directories.contains(directory).then(|| FlutterFacts {
                manifest_beside: self.files.contains(manifest),
                tracked: self.tracked.iter().any(|path| {
                    path.as_slice() == directory || path_below(path, directory).is_some()
                }),
                cargo_target: self.cargo_targets.contains(directory),
            })
        }
    }

    /// The same classifier decides a Flutter app's output from on-disk observations and from a
    /// patch's; only the source of the observations differs, and both agree on the same tree.
    #[test]
    fn one_classifier_decides_flutter_output_for_disk_and_patch() {
        // HEAD: an app at the root and one in `app/`, with a tracked source directory `tool/build`
        // beside `tool/pubspec.yaml`.
        let head_files = [
            "pubspec.yaml",
            "lib/main.dart",
            "app/pubspec.yaml",
            "tool/pubspec.yaml",
            "tool/build/build.yaml",
        ];
        let head_directories = ["lib", "app", "tool", "tool/build"];
        let untracked = [
            "build/app/outputs/app-release.apk",
            ".dart_tool/package_config.json",
            "app/build/web/main.dart.js",
            "app/.dart_tool/version",
            "tool/build/generated.dart",
            "other/build/out.bin",
            "notes.txt",
        ];
        let expected: [Option<&str>; 7] = [
            Some("build"),
            Some(".dart_tool"),
            Some("app/build"),
            Some("app/.dart_tool"),
            None,
            None,
            None,
        ];

        let mut directories = head_directories.to_vec();
        directories.extend([
            "build",
            "build/app",
            "build/app/outputs",
            ".dart_tool",
            "app/build",
            "app/build/web",
            "app/.dart_tool",
            "other",
            "other/build",
        ]);
        let mut files = head_files.to_vec();
        files.extend(untracked);
        let disk = Disk::new(&files, &directories, &head_files);
        for (path, expected) in untracked.iter().zip(expected) {
            let on_disk = flutter_output(path.as_bytes(), |directory, manifest| {
                disk.observe(directory, manifest)
            });
            assert_eq!(on_disk, expected.map(str::as_bytes), "disk: {path}");
        }

        let sections = untracked
            .iter()
            .map(|path| section(path, true))
            .collect::<Vec<_>>();
        let plan = plan_over_head(&sections, &head_files, &head_directories);
        let stripped = expected.map(|target| target.is_some());
        assert_eq!(plan.strip, stripped, "{plan:?}");
        assert_eq!(
            plan.targets,
            [
                strip_target(".dart_tool", 1),
                strip_target("app/.dart_tool", 1),
                strip_target("app/build", 1),
                strip_target("build", 1),
            ]
        );

        // The directory itself, as `discard-cache` asks of an ignored entry.
        let kind = |directory: &str| {
            flutter_output_kind(directory.as_bytes(), |directory, manifest| {
                disk.observe(directory, manifest)
            })
        };
        assert_eq!(kind("build"), Some(FlutterOutputKind::Build));
        assert_eq!(kind("app/.dart_tool"), Some(FlutterOutputKind::DartTool));
        assert_eq!(kind("tool/build"), None, "tracked source directory");
        assert_eq!(kind("other/build"), None, "no pubspec.yaml beside it");
        assert_eq!(kind("app"), None, "not an output name");

        // On disk a symbolic link named `build` is no real directory, so no observation exists.
        let linked = Disk::new(&["pubspec.yaml"], &[], &["pubspec.yaml"]);
        assert_eq!(
            flutter_output(b"build/out.bin", |directory, manifest| linked
                .observe(directory, manifest)),
            None
        );
    }

    /// Without `pubspec.yaml` beside it, as anything but a directory, a `build/` or `.dart_tool/`
    /// is an ordinary directory, whatever the source of the observations.
    #[test]
    fn a_build_directory_with_no_pubspec_beside_is_not_flutter_output() {
        // On disk: no manifest, one only above or below, or a manifest that is a directory.
        let disk = Disk::new(
            &["pubspec.yaml", "app/lib/pubspec.yaml"],
            &[
                "app",
                "app/build",
                "app/lib",
                "tool",
                "tool/pubspec.yaml",
                "tool/build",
                "build",
            ],
            &["pubspec.yaml"],
        );
        for path in [
            "app/build/out.bin",
            "app/lib/x/build/out.bin",
            "tool/build/out.bin",
        ] {
            assert_eq!(
                flutter_output(path.as_bytes(), |directory, manifest| disk
                    .observe(directory, manifest)),
                None,
                "{path}"
            );
        }
        for directory in ["app/build", "tool/build"] {
            assert_eq!(
                flutter_output_kind(directory.as_bytes(), |directory, manifest| disk
                    .observe(directory, manifest)),
                None,
                "{directory}"
            );
        }
        // The root app's `build` does have one beside it.
        assert_eq!(
            flutter_output(b"build/out.bin", |directory, manifest| disk
                .observe(directory, manifest)),
            Some(&b"build"[..])
        );

        // The classifier never asks about a path that is not below an output name, and a
        // manifest observed absent decides it whatever else holds.
        let mut asked = Vec::new();
        let absent = flutter_output(b"src/build/x/.dart_tool/y", |directory, _| {
            asked.push(directory.to_vec());
            Some(FlutterFacts {
                manifest_beside: false,
                tracked: false,
                cargo_target: false,
            })
        });
        assert_eq!(absent, None);
        assert_eq!(
            asked,
            [b"src/build".to_vec(), b"src/build/x/.dart_tool".to_vec()]
        );

        // In a patch: no `pubspec.yaml` in HEAD or added beside it.
        let plan = plan_over_head(
            &[
                section("build/out.bin", true),
                section("app/build/out.bin", true),
            ],
            &["README.md", "app/lib/pubspec.yaml"],
            &["app", "app/lib"],
        );
        assert_eq!(plan.strip, [false, false], "{plan:?}");
    }

    #[test]
    fn layout_is_the_first_component_or_a_profile_below_the_target() {
        let profiles = ["custom", "x86_64-unknown-linux-gnu/debug"];
        let profile = |prefix: &[u8]| profiles.iter().any(|item| item.as_bytes() == prefix);
        let all = TargetFacts {
            signed_tag: true,
            holds_profile: true,
        };
        for layout in [
            "debug/deps/libone.rlib",
            "release/one",
            "tmp/scratch/file",
            "CACHEDIR.TAG",
            ".rustc_info.json",
            "custom/deps/libone.rlib",
            "x86_64-unknown-linux-gnu/debug/deps/libone.rlib",
        ] {
            assert!(is_build_layout(layout.as_bytes(), all, profile), "{layout}");
        }
        for kept in [
            "ess-conformance/report.json",
            "notes.md",
            "custom",
            "x86_64-unknown-linux-gnu/notes.md",
            ".future-incompat-report.json",
            "",
        ] {
            assert!(!is_build_layout(kept.as_bytes(), all, profile), "{kept}");
        }
        // `tmp/` is Cargo's only beside a profile, `CACHEDIR.TAG` only when it is signed.
        let unsigned = TargetFacts {
            signed_tag: false,
            holds_profile: true,
        };
        assert!(!is_build_layout(b"CACHEDIR.TAG", unsigned, profile));
        assert!(is_build_layout(b"tmp/scratch/file", unsigned, profile));
        let profileless = TargetFacts {
            signed_tag: true,
            holds_profile: false,
        };
        assert!(!is_build_layout(b"tmp/scratch/file", profileless, profile));
        assert!(is_build_layout(b"CACHEDIR.TAG", profileless, profile));
    }

    #[test]
    fn the_outermost_target_holding_layout_is_chosen() {
        let targets = vec![b"target/x86_64".to_vec(), b"target".to_vec()];
        let none = |_: &[u8], _: &[u8]| false;
        let facts = |_: &[u8]| TargetFacts::default();
        assert_eq!(
            layout_target(b"target/debug/x", &targets, facts, none),
            Some(1)
        );
        assert_eq!(
            layout_target(b"target/x86_64/debug/x", &targets, facts, none),
            Some(0)
        );
        assert_eq!(
            layout_target(b"target/records/x", &targets, facts, none),
            None
        );
        assert_eq!(
            layout_target(b"targets/debug/x", &targets, facts, none),
            None
        );
    }

    #[test]
    fn c_quoted_paths_decode_as_git_writes_them() {
        assert_eq!(
            c_unquote(br#""a/lib\303\251.rlib" rest"#),
            Some(("a/lib\u{e9}.rlib".as_bytes().to_vec(), &b" rest"[..]))
        );
        assert_eq!(
            c_unquote(br#""tab\there \"q\" back\\slash""#).unwrap().0,
            b"tab\there \"q\" back\\slash"
        );
        for malformed in [
            &br#""open"#[..],
            br#""\9""#,
            br#""\x""#,
            br#""\38""#,
            b"bare",
        ] {
            assert_eq!(c_unquote(malformed), None, "{malformed:?}");
        }
    }

    #[test]
    fn diff_git_lines_decode_only_when_both_sides_name_one_path() {
        assert_eq!(
            diff_git_path(b"diff --git a/target/debug/x b/target/debug/x"),
            Some(b"target/debug/x".to_vec())
        );
        assert_eq!(
            diff_git_path(b"diff --git a/with space/b x b/with space/b x"),
            Some(b"with space/b x".to_vec())
        );
        assert_eq!(
            diff_git_path(br#"diff --git "a/lib\303\251" "b/lib\303\251""#),
            Some("lib\u{e9}".as_bytes().to_vec())
        );
        for undecodable in [
            &b"diff --git a/one b/two"[..],
            br#"diff --git "a/one" "b/two""#,
            br#"diff --git "a/\q" "b/\q""#,
            b"diff --git a/x  b/x",
            b"not a header",
        ] {
            assert_eq!(diff_git_path(undecodable), None, "{undecodable:?}");
        }
    }

    #[test]
    fn new_file_sizes_come_from_the_literal_or_the_added_lines() {
        let mut text = NewFileSize::default();
        for line in [
            &b"new file mode 100644\n"[..],
            b"--- /dev/null\n",
            b"+++ b/x\n",
            b"@@ -0,0 +1,2 @@\n",
            b"+one\n",
            b"+tw\n",
            b"\\ No newline at end of file\n",
        ] {
            text.observe(line);
        }
        assert_eq!(text.bytes(), 6);
        let mut binary = NewFileSize::default();
        for line in [&b"GIT binary patch\n"[..], b"literal 300\n", b"zcmV\n"] {
            binary.observe(line);
        }
        assert_eq!(binary.bytes(), 300);
    }

    #[test]
    fn only_added_layout_below_a_recognised_target_is_stripped() {
        let sections = vec![
            section("target/.rustc_info.json", true),
            section("target/debug/.fingerprint/one/lib", true),
            section("target/debug/deps/libone.rlib", true),
            section("target/custom/.fingerprint/one/lib", true),
            section("target/custom/deps/x", true),
            section("target/ess-conformance/report.json", true),
            section("target/debug/kept.txt", false),
            section("evidence/debug/notes.txt", true),
            ScannedSection {
                path: None,
                new_file: true,
                tracked: false,
                signed_tag: false,
                patch_bytes: 10,
                content_bytes: 3,
            },
        ];
        let plan = strip_plan(&sections);
        assert_eq!(
            plan.strip,
            [true, true, true, true, true, false, false, false, false]
        );
        assert_eq!(plan.sections_stripped, 5);
        assert_eq!(plan.sections_kept, 4);
        assert_eq!(plan.strip_bytes, 50);
        assert_eq!(
            plan.targets,
            [BuildOutputTarget {
                path: "target".into(),
                origin: BuildOutputOrigin::Strip,
                files: 5,
                bytes: 15,
            }]
        );
        // Without any recognising file nothing is a target.
        let plan = strip_plan(&[section("target/debug/deps/libone.rlib", true)]);
        assert_eq!(plan.sections_stripped, 0);
    }

    /// A section that is not a pure addition of an untracked path is kept, and so is every other
    /// section naming its path: a type change is a `deleted file mode` and a `new file mode`
    /// section of one path, and stripping either half loses the tracked file.
    #[test]
    fn a_path_named_twice_tracked_or_changed_keeps_every_section() {
        let profile = section("target/debug/.fingerprint/one/lib", true);
        let tracked = ScannedSection {
            tracked: true,
            ..section("target/debug/tracked.txt", true)
        };
        let sections = vec![
            profile.clone(),
            section("target/debug/kept.txt", false),
            section("target/debug/kept.txt", true),
            section("target/debug/twice", true),
            section("target/debug/twice", true),
            tracked,
            section("target/debug/deps/libone.rlib", true),
        ];
        let plan = strip_plan(&sections);
        assert_eq!(
            plan.strip,
            [true, false, false, false, false, false, true],
            "{plan:?}"
        );
        // Only a pure addition recognises a target: a tracked or doubly named profile does not.
        for disguised in [
            ScannedSection {
                tracked: true,
                ..profile.clone()
            },
            section("target/debug/.fingerprint/one/lib", false),
        ] {
            let plan = strip_plan(&[
                disguised,
                section("target/debug/.fingerprint/one/lib", true),
                section("target/debug/deps/libone.rlib", true),
            ]);
            assert_eq!(plan.sections_stripped, 0, "{plan:?}");
        }
    }

    /// In a patch, only a Cargo-signed `CACHEDIR.TAG` or a profile's `.fingerprint/` recognises a
    /// target; `.rustc_info.json` or an unsigned tag never does, and `tmp/` is stripped only
    /// beside a profile, as `discard-cache` keeps a profile-less `tmp/` on disk.
    #[test]
    fn a_patch_target_needs_a_signed_tag_or_a_profile() {
        let signed = |path: &str| ScannedSection {
            signed_tag: true,
            ..section(path, true)
        };
        for unrecognised in [
            vec![
                section("target/.rustc_info.json", true),
                section("target/tmp/scratch", true),
                section("target/debug/deps/libone.rlib", true),
            ],
            vec![
                section("target/CACHEDIR.TAG", true),
                section("target/tmp/scratch", true),
                section("target/debug/deps/libone.rlib", true),
            ],
        ] {
            let plan = strip_plan(&unrecognised);
            assert_eq!(plan.sections_stripped, 0, "{plan:?}");
        }
        // A signed tag recognises the target, but its `tmp/` stays without a profile.
        let plan = strip_plan(&[
            signed("target/CACHEDIR.TAG"),
            section("target/.rustc_info.json", true),
            section("target/tmp/scratch", true),
            section("target/debug/deps/libone.rlib", true),
        ]);
        assert_eq!(plan.strip, [true, true, false, true], "{plan:?}");
        // Beside a profile, even a cross-target one, `tmp/` goes; an unsigned tag stays.
        let plan = strip_plan(&[
            section("target/CACHEDIR.TAG", true),
            section(
                "target/x86_64-unknown-linux-gnu/debug/.fingerprint/one/lib",
                true,
            ),
            section("target/tmp/scratch", true),
        ]);
        assert_eq!(plan.strip, [false, true, false], "{plan:?}");
        let plan = strip_plan(&[
            signed("target/CACHEDIR.TAG"),
            section(
                "target/x86_64-unknown-linux-gnu/debug/.fingerprint/one/lib",
                true,
            ),
            section("target/tmp/scratch", true),
        ]);
        assert_eq!(plan.strip, [true, true, true], "{plan:?}");
    }

    #[test]
    fn a_signed_tag_is_the_signature_as_the_first_added_line() {
        let observe = |lines: &[&[u8]]| {
            let mut size = NewFileSize::default();
            for line in lines {
                size.observe(line);
            }
            size.signed_tag()
        };
        assert!(observe(&[
            b"new file mode 100644\n",
            b"@@ -0,0 +1,2 @@\n",
            b"+Signature: 8a477f597d28d172789f06886806bc55\n",
            b"+# cargo\n",
        ]));
        assert!(!observe(&[
            b"new file mode 100644\n",
            b"@@ -0,0 +1,2 @@\n",
            b"+# cargo\n",
            b"+Signature: 8a477f597d28d172789f06886806bc55\n",
        ]));
        assert!(!observe(&[b"@@ -0,0 +1 @@\n", b"+Signature: 0000\n"]));
        assert!(!observe(&[b"GIT binary patch\n", b"literal 3\n"]));
    }

    #[test]
    fn merged_build_output_sums_by_path_and_origin_and_validates() {
        let target = |path: &str, origin, files| BuildOutputTarget {
            path: path.into(),
            origin,
            files,
            bytes: files * 10,
        };
        assert_eq!(BuildOutput::merged(None, Vec::new()), None);
        let first =
            BuildOutput::merged(None, vec![target("target", BuildOutputOrigin::Archive, 2)])
                .unwrap();
        let merged = BuildOutput::merged(
            Some(&first),
            vec![
                target("target", BuildOutputOrigin::Strip, 1),
                target("target", BuildOutputOrigin::Archive, 1),
                target("a/target", BuildOutputOrigin::Strip, 1),
            ],
        )
        .unwrap();
        assert_eq!(merged.files, 5);
        assert_eq!(merged.bytes, 50);
        assert_eq!(merged.targets[0].path, "a/target");
        assert_eq!(merged.targets[1].files, 3);
        assert_eq!(merged.defect(), None);
        let mut bad = merged.clone();
        bad.files = 4;
        assert!(bad.defect().is_some());
        let mut bad = merged;
        bad.targets[0].path = "../escape".into();
        assert!(bad.defect().is_some());
    }

    /// A path is repository content when it has a `.git` component or it, or an ancestor below
    /// the tree root, is a Git directory as its caller observes one; only directories are asked.
    #[test]
    fn repository_content_is_a_git_component_or_below_a_git_directory() {
        let git_directories = [b"target/release/build/one/out/remote.git".as_slice()];
        let mut asked = Vec::new();
        let mut observe = |directory: &[u8]| {
            asked.push(directory.to_vec());
            git_directories.contains(&directory)
        };
        assert!(repository_path(
            b"target/release/build/one/out/remote.git/objects/ab/cdef",
            &mut observe
        ));
        assert!(repository_path(
            b"target/release/build/one/out/remote.git/HEAD",
            &mut observe
        ));
        assert!(!repository_path(
            b"target/release/build/one/out/other",
            &mut observe
        ));
        assert!(!repository_path(
            b"target/release/deps/libone.rlib",
            &mut observe
        ));
        assert!(!asked.iter().any(Vec::is_empty), "the tree root was asked");
        for path in [
            "target/tmp/fixture/.git/HEAD",
            "target/tmp/.git",
            ".git/config",
            "build/app/.git",
        ] {
            assert!(repository_path(path.as_bytes(), |_| false), "{path}");
        }
        // A name only like `.git` is no `.git` component.
        for path in [
            "target/tmp/x.git/HEAD",
            "target/tmp/.gitignore",
            "target/.github/a",
        ] {
            assert!(!repository_path(path.as_bytes(), |_| false), "{path}");
        }

        // Layout below a cargo target or Flutter output directory, unless repository content.
        let path = b"target/release/build/one/out/remote.git/HEAD";
        assert_eq!(
            layout_directory(path, Some(b"target"), None, |directory| {
                git_directories.contains(&directory)
            }),
            None
        );
        assert_eq!(
            layout_directory(b"target/release/deps/x", Some(b"target"), None, |_| false),
            Some(b"target".as_slice())
        );
        assert_eq!(
            layout_directory(b"build/x/.git/HEAD", None, Some(b"build"), |_| false),
            None
        );
        // Nothing to ask when the path lies below no build directory.
        assert_eq!(
            layout_directory(b"notes.txt", None, None, |_| panic!("asked")),
            None
        );
    }

    /// The structure that makes a directory a Git directory: all three parts, or none counts.
    #[test]
    fn a_git_directory_needs_head_objects_and_refs() {
        let all = crate::GitDirectoryFacts {
            head_file: true,
            objects_directory: true,
            refs_directory: true,
        };
        assert!(all.recognises());
        for missing in [
            crate::GitDirectoryFacts {
                head_file: false,
                ..all
            },
            crate::GitDirectoryFacts {
                objects_directory: false,
                ..all
            },
            crate::GitDirectoryFacts {
                refs_directory: false,
                ..all
            },
        ] {
            assert!(!missing.recognises(), "{missing:?}");
        }
    }

    /// A repository is never build layout in a patch: a bare repository the patch purely adds
    /// below a profile (its `HEAD` and a path under its `objects/`) keeps every section at or
    /// below it, and so does any path with a `.git` component; the layout beside it is stripped.
    #[test]
    fn a_repository_the_patch_adds_below_a_profile_is_never_stripped() {
        let sections = [
            section("target/debug/.fingerprint/one/lib", true),
            section("target/debug/deps/libone.rlib", true),
            section("target/debug/x/repo.git/HEAD", true),
            section("target/debug/x/repo.git/config", true),
            section("target/debug/x/repo.git/objects/ab/cdef", true),
            section("target/debug/x/repo.git/refs/heads/main", true),
            section("target/debug/y/.git/config", true),
            section("target/debug/z/.git", true),
            // A `HEAD` without any `objects/` path is no Git directory the patch shows.
            section("target/debug/w/HEAD", true),
            section("target/debug/w/refs/heads/main", true),
        ];
        let plan = strip_plan(&sections);
        assert_eq!(
            plan.strip,
            [
                true, true, false, false, false, false, false, false, true, true
            ],
            "{plan:?}"
        );
        assert_eq!(plan.targets, [strip_target("target", 4)]);
        assert_eq!(plan.sections_kept, 6);

        // The same in a Flutter app's output.
        let sections = [
            section("build/out.bin", true),
            section("build/fixture/HEAD", true),
            section("build/fixture/objects/pack/one.pack", true),
            section(".dart_tool/cache/.git/HEAD", true),
        ];
        let plan = plan_over_head(&sections, &["pubspec.yaml"], &[]);
        assert_eq!(plan.strip, [true, false, false, false], "{plan:?}");
        assert_eq!(plan.targets, [strip_target("build", 1)]);
    }
}
