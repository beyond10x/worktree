//! Cargo build layout an archive leaves out, and the strip of it from an archive written before.
//!
//! An archive holds what no remote recovers. Cargo's own build layout is recreated from tracked
//! sources by the next build, so `archive` leaves it out of `dirty.patch` and the fingerprint and
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

use crate::{
    ARCHIVE_PATCH_FILE, ArchiveContents, ArchiveEntryKind, ArchiveManifest, CACHEDIR_TAG_FILE,
    CARGO_PROFILE_MARKER, CARGO_TARGET_TMP, PruneVerdict, SkippedArchiveEntry, is_cache_tag,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

/// Independent, immutable manifest format identifier of an archive that left cargo build layout
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

/// How a left-out file was found to be cargo build layout (`worktree.archive.BuildOutputOrigin`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BuildOutputOrigin {
    /// `archive` observed it on disk below a target `discard-cache` recognises.
    Archive,
    /// `prune-archives --strip-build-output` removed its section from an existing patch.
    Strip,
}

/// Build layout left out below one cargo target (`worktree.archive.BuildOutputTarget`).
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

/// Decide which sections of a patch are cargo build layout.
///
/// Only a pure addition is stripped: a `new file mode` section whose path decoded, that no other
/// section names, and that HEAD does not track ([`ScannedSection::tracked`]); and only when that
/// path is build layout below a directory the patch's own pure additions recognise as a cargo
/// target, by a Cargo-signed `CACHEDIR.TAG` or a profile's `.fingerprint/`. Every other section is
/// kept whole: a type change of a tracked path is a `deleted file mode` and a `new file mode`
/// section of one path, and both halves stay.
pub fn plan_patch_strip(sections: &[ScannedSection]) -> PatchStripPlan {
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
    let recognised = PatchTargets::recognise(&added);
    let targets = &recognised.targets;
    let mut plan = PatchStripPlan {
        strip: Vec::with_capacity(sections.len()),
        targets: Vec::new(),
        sections_stripped: 0,
        sections_kept: 0,
        strip_bytes: 0,
        kept_bytes: 0,
    };
    let mut per_target: Vec<(u64, u64)> = vec![(0, 0); targets.len()];
    for section in sections {
        let target = section
            .path
            .as_deref()
            .filter(|_| pure_addition(section))
            .and_then(|path| {
                layout_target(
                    path,
                    targets,
                    |target| recognised.facts(target),
                    |target, prefix| recognised.profile(target, prefix),
                )
            });
        plan.strip.push(target.is_some());
        if let Some(index) = target {
            plan.sections_stripped += 1;
            plan.strip_bytes += section.patch_bytes;
            per_target[index].0 += 1;
            per_target[index].1 += section.content_bytes;
        } else {
            plan.sections_kept += 1;
            plan.kept_bytes += section.patch_bytes;
        }
    }
    plan.targets = targets
        .iter()
        .zip(per_target)
        .filter(|(_, (files, _))| *files > 0)
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
    scan: impl FnOnce(&ArchiveManifest) -> Result<Vec<ScannedSection>, String>,
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
    let sections = match scan(manifest) {
        Ok(sections) => sections,
        Err(error) => return decision(StripVerdict::PatchUnusable, error),
    };
    let plan = plan_patch_strip(&sections);
    if plan.sections_stripped == 0 {
        return StripDecision {
            verdict: StripVerdict::NothingToStrip,
            reason: "no section of the patch adds cargo build layout".into(),
            plan: Some(plan),
        };
    }
    StripDecision {
        verdict: StripVerdict::Strippable,
        reason: format!(
            "{} section(s) add cargo build layout below {}",
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
        let plan = plan_patch_strip(&sections);
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
        let plan = plan_patch_strip(&[section("target/debug/deps/libone.rlib", true)]);
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
        let plan = plan_patch_strip(&sections);
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
            let plan = plan_patch_strip(&[
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
            let plan = plan_patch_strip(&unrecognised);
            assert_eq!(plan.sections_stripped, 0, "{plan:?}");
        }
        // A signed tag recognises the target, but its `tmp/` stays without a profile.
        let plan = plan_patch_strip(&[
            signed("target/CACHEDIR.TAG"),
            section("target/.rustc_info.json", true),
            section("target/tmp/scratch", true),
            section("target/debug/deps/libone.rlib", true),
        ]);
        assert_eq!(plan.strip, [true, true, false, true], "{plan:?}");
        // Beside a profile, even a cross-target one, `tmp/` goes; an unsigned tag stays.
        let plan = plan_patch_strip(&[
            section("target/CACHEDIR.TAG", true),
            section(
                "target/x86_64-unknown-linux-gnu/debug/.fingerprint/one/lib",
                true,
            ),
            section("target/tmp/scratch", true),
        ]);
        assert_eq!(plan.strip, [false, true, false], "{plan:?}");
        let plan = plan_patch_strip(&[
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
}
