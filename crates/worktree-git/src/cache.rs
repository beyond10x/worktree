//! Classify, and on request delete, a tree's ignored entries.
//!
//! Every ignored entry Git reports (`status --ignored=matching`, file or directory) is
//! disposable unless the tree's keep file ([`b10x_worktree_domain::KEEP_FILE`]) keeps it, it is a
//! repository, it sits on another device than the tree root, or it cannot be read. Records an
//! agent must keep are kept through the keep file, whose lines Git's own exclude engine matches
//! (`ls-files --others --ignored --exclude-from`), so negation, directory-only
//! patterns and anchoring mean what they mean in `.gitignore`. A directory holding a kept path or
//! a repository at any depth is never deleted whole: it falls to its direct entries, each
//! classified by the same rule. The structural rules in [`b10x_worktree_domain::CacheKind`] name
//! what they recognise; everything else is `ignored-output`.
//!
//! The walk follows no symbolic link: a symbolic link is deleted itself. Every deletion is
//! inside the tree, and the keep file and the walk for repositories are consulted again
//! immediately before each one.

use crate::ProcessGit;
use b10x_worktree_domain::{
    CACHEDIR_TAG_FILE, CACHEDIR_TAG_SIGNATURE, CARGO_MANIFEST, CARGO_PROFILE_DEPS,
    CARGO_PROFILE_MARKER, CARGO_TARGET_METADATA, CARGO_TARGET_TMP, CARGO_UNTAGGED_TARGET,
    CacheClassification, CacheKind, DiscardedCache, FlutterFacts, FlutterOutputKind,
    GIT_DIRECTORY_HEAD, GIT_DIRECTORY_OBJECTS, GIT_DIRECTORY_REFS, GIT_ENTRY, GitDirectoryFacts,
    KEEP_FILE, NODE_LOCKFILES, PYTHON_MANIFESTS, PYTHON_VIRTUALENV_MARKER, Refusal,
    TAGGED_TOOL_CACHES, flutter_output_kind, is_cache_tag,
};
use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::io::Read as _;
use std::ops::Bound;
use std::path::{Component, Path, PathBuf};

/// Classified ignored entries, relative to the tree root.
#[derive(Default)]
struct Plan {
    discarded: Vec<(PathBuf, CacheKind)>,
    retained: Vec<PathBuf>,
    /// Cargo targets recognised at an ignored entry or as its parent.
    targets: Vec<PathBuf>,
}

impl Plan {
    fn extend(&mut self, other: Self) {
        self.discarded.extend(other.discarded);
        self.retained.extend(other.retained);
        self.targets.extend(other.targets);
    }
}

/// What a tagged Cargo target turned out to hold.
enum TargetShape {
    /// Only profiles, nested targets of the same shape, Cargo metadata, empty directories and
    /// Cargo's `tmp`, none of them kept or holding a repository.
    Whole,
    /// A profile and anything else: each part classified on its own.
    Partial(Plan),
    /// No profile at all: not a Cargo target, whatever its tag says.
    NotCargo,
}

/// What one entry is, observed on disk without following a symbolic link.
enum Observed {
    /// Gone since Git reported it.
    Missing,
    /// A repository, on another device, or unreadable: never deleted.
    Retained,
    /// A file, symbolic link or other non-directory: unlinked itself.
    Unlinkable,
    /// A real, readable directory that is not itself a repository.
    Directory,
}

/// What keeps an ignored entry from deletion beyond its structure, read once per discard.
struct Guard {
    /// What the keep file keeps, as Git lists it ([`kept_paths`]).
    kept: BTreeSet<PathBuf>,
    /// The tree root's device; `None` where devices are not compared.
    device: Option<u64>,
}

impl Guard {
    /// Read the tree's keep file and root device. A keep file that is not a readable regular
    /// file refuses as `keep-file-invalid`.
    fn read(worktree: &Path) -> Result<Self, Refusal> {
        let kept = if keep_file(worktree)? {
            kept_paths(worktree, None)?
        } else {
            BTreeSet::new()
        };
        let root = std::fs::symlink_metadata(worktree).map_err(|error| {
            Refusal::new(
                "cache-classification-failed",
                format!("{}: {error}", worktree.display()),
            )
        })?;
        Ok(Self {
            kept,
            device: device(&root),
        })
    }

    /// No keep file and no device comparison: what the archive's target discovery observes.
    fn structural() -> Self {
        Self {
            kept: BTreeSet::new(),
            device: None,
        }
    }

    /// Whether the keep file keeps `relative`: it or one of its ancestors is listed. The keep file
    /// itself is always kept.
    fn kept(&self, relative: &Path) -> bool {
        relative == Path::new(KEEP_FILE)
            || relative
                .ancestors()
                .filter(|ancestor| !ancestor.as_os_str().is_empty())
                .any(|ancestor| self.kept.contains(ancestor))
    }

    /// Whether the keep file keeps a path strictly below `relative`.
    fn holds_kept(&self, relative: &Path) -> bool {
        self.kept
            .range::<Path, _>((Bound::Excluded(relative), Bound::Unbounded))
            .next()
            .is_some_and(|path| path.starts_with(relative))
    }

    fn foreign_device(&self, metadata: &std::fs::Metadata) -> bool {
        self.device
            .is_some_and(|root| device(metadata).is_some_and(|entry| entry != root))
    }

    /// What the entry at `path` is ([`Observed`]).
    fn observe(&self, path: &Path) -> Observed {
        if path.file_name() == Some(OsStr::new(GIT_ENTRY)) {
            return Observed::Retained;
        }
        let metadata = match std::fs::symlink_metadata(path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Observed::Missing;
            }
            Err(_) => return Observed::Retained,
        };
        if self.foreign_device(&metadata) {
            return Observed::Retained;
        }
        if !metadata.is_dir() {
            return Observed::Unlinkable;
        }
        if repository(path) || std::fs::read_dir(path).is_err() {
            Observed::Retained
        } else {
            Observed::Directory
        }
    }

    /// Whether the directory `relative` holds, at any depth, a kept path, a repository, an entry
    /// on another device or one the walk cannot read. The walk follows no symbolic link.
    fn holds_protected(&self, worktree: &Path, relative: &Path) -> bool {
        if self.holds_kept(relative) {
            return true;
        }
        let directory = worktree.join(relative);
        if git_directory(&directory) {
            return true;
        }
        let mut pending = vec![directory];
        while let Some(current) = pending.pop() {
            let Ok(entries) = std::fs::read_dir(&current) else {
                return true;
            };
            for entry in entries {
                let Ok(entry) = entry else {
                    return true;
                };
                if entry.file_name() == GIT_ENTRY {
                    return true;
                }
                let path = entry.path();
                let Ok(metadata) = std::fs::symlink_metadata(&path) else {
                    return true;
                };
                if self.foreign_device(&metadata) {
                    return true;
                }
                if metadata.is_dir() {
                    if git_directory(&path) {
                        return true;
                    }
                    pending.push(path);
                }
            }
        }
        false
    }

    /// The re-observation immediately before deleting `relative`: whether it is now kept (the
    /// keep file read again, scoped to the path), a repository, on another device or unreadable,
    /// or, as a directory, holds any of those. A keep file that became invalid keeps everything.
    fn protected_now(&self, worktree: &Path, relative: &Path) -> Result<bool, Refusal> {
        if self.kept(relative) {
            return Ok(true);
        }
        match keep_file(worktree) {
            Err(_) => return Ok(true),
            Ok(true) if !kept_paths(worktree, Some(relative))?.is_empty() => return Ok(true),
            Ok(_) => {}
        }
        Ok(match self.observe(&worktree.join(relative)) {
            Observed::Retained => true,
            Observed::Missing | Observed::Unlinkable => false,
            Observed::Directory => self.holds_protected(worktree, relative),
        })
    }
}

#[cfg(unix)]
#[allow(clippy::unnecessary_wraps)]
fn device(metadata: &std::fs::Metadata) -> Option<u64> {
    use std::os::unix::fs::MetadataExt as _;
    Some(metadata.dev())
}

#[cfg(not(unix))]
fn device(_metadata: &std::fs::Metadata) -> Option<u64> {
    None
}

/// Whether the tree has a keep file: absent is `false`; a regular file that can be read is
/// `true`; anything else refuses as `keep-file-invalid`.
fn keep_file(worktree: &Path) -> Result<bool, Refusal> {
    let path = worktree.join(KEEP_FILE);
    let invalid = |reason: &dyn std::fmt::Display| {
        Refusal::new(
            "keep-file-invalid",
            format!(
                "{} {reason}; it must be a regular file of Git exclude patterns, and nothing was \
                 deleted",
                path.display()
            ),
        )
    };
    match std::fs::symlink_metadata(&path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(invalid(&format_args!("cannot be read: {error}"))),
        Ok(metadata) if metadata.is_symlink() => Err(invalid(&"is a symbolic link")),
        Ok(metadata) if metadata.is_dir() => Err(invalid(&"is a directory")),
        Ok(metadata) if !metadata.is_file() => Err(invalid(&"is not a regular file")),
        Ok(_) => std::fs::read(&path)
            .map(|_| true)
            .map_err(|error| invalid(&format_args!("cannot be read: {error}"))),
    }
}

/// What the keep file keeps, as Git's exclude engine lists it: every untracked file a pattern
/// matches, directly or through a matched ancestor directory (and a nested repository Git does
/// not descend into, as `<path>/`). `scope` limits the listing to one path. Without
/// `--directory`, which also lists a parent whose every entry matches, so only what the patterns
/// match is kept; an empty directory holds nothing to keep.
fn kept_paths(worktree: &Path, scope: Option<&Path>) -> Result<BTreeSet<PathBuf>, Refusal> {
    let mut args: Vec<OsString> = [
        "ls-files",
        "-z",
        "--others",
        "--ignored",
        // Relative to `-C <tree>`, so a tree path that is not UTF-8 needs no argument of its own.
        "--exclude-from=.worktreekeep",
    ]
    .into_iter()
    .map(OsString::from)
    .collect();
    if let Some(scope) = scope {
        args.push("--".into());
        let mut literal = OsString::from(":(literal)");
        literal.push(scope);
        args.push(literal);
    }
    ProcessGit::output_bytes(worktree, args)?
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(|field| crate::path_from_git_bytes(field.strip_suffix(b"/").unwrap_or(field)))
        .collect()
}

/// Whether the real directory `path` is itself a repository: it holds a `.git` entry (file,
/// directory or symbolic link) directly, or it is a Git directory ([`git_directory`]). Its
/// files belong to that repository, so it is never deleted or split.
fn repository(path: &Path) -> bool {
    git_directory(path)
        || match std::fs::symlink_metadata(path.join(GIT_ENTRY)) {
            Ok(_) => true,
            Err(error) => error.kind() != std::io::ErrorKind::NotFound,
        }
}

pub(crate) fn discard(worktree: &Path, apply: bool) -> Result<CacheClassification, Refusal> {
    let guard = Guard::read(worktree)?;
    let plan = classify_tree(worktree, &guard)?;
    finish_discard(worktree, plan, &guard, apply)
}

/// The cargo targets `discard-cache` recognises among the tree's ignored entries, relative to the
/// tree root, in path order: each holds at least one profile and is tagged or counts as tagged.
/// Archives leave the build layout below them out ([`b10x_worktree_domain::is_build_layout`]).
/// The keep file plays no part: archiving never deletes anything.
pub(crate) fn cargo_targets(worktree: &Path) -> Result<Vec<PathBuf>, Refusal> {
    let mut targets = classify_tree(worktree, &Guard::structural())?.targets;
    targets.sort();
    targets.dedup();
    Ok(targets)
}

/// Whether `relative` is a cargo target by the structure `discard-cache` requires of one, read
/// on disk whatever Git tracks below it: a real directory that is tagged or counts as tagged
/// ([`cargo_target_root`]) and holds a profile, directly or below a target-triple directory.
///
/// This is the archive's own rule. `discard-cache` reaches a target only through an ignored
/// entry Git reports, so one tracked (or `git add -f`) file below a target hides the target from
/// it, and it keeps that rule for its deletions. An archive leaves out only untracked layout
/// below the target, file by file, so the tracked file never decides whether the target is one.
pub(crate) fn archive_target(
    worktree: &Path,
    relative: &Path,
    tracked: &BTreeSet<PathBuf>,
) -> Result<bool, Refusal> {
    let directory = worktree.join(relative);
    if !real_directory(&directory) {
        return Ok(false);
    }
    if !cargo_target_root(worktree, relative, tracked) {
        return Ok(false);
    }
    for name in read_names(&directory)? {
        if name == CARGO_TARGET_TMP {
            continue;
        }
        let child = directory.join(&name);
        if cargo_profile(&child) || (real_directory(&child) && holds_profile(&child)?) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Whether `directory/CACHEDIR.TAG` is a regular file carrying Cargo's signature.
pub(crate) fn signed_tag(directory: &Path) -> bool {
    cache_tagged(directory)
}

/// Classify every ignored entry of the tree.
fn classify_tree(worktree: &Path, guard: &Guard) -> Result<Plan, Refusal> {
    let status = ProcessGit::output_bytes(
        worktree,
        [
            "--no-optional-locks",
            "status",
            "--porcelain=v1",
            "-z",
            "--untracked-files=all",
            "--ignored=matching",
        ],
    )?;
    let mut walk = Walk {
        worktree,
        guard,
        tracked: tracked_paths(worktree)?,
        committed: None,
    };
    let mut plan = Plan::default();
    for (relative, _directory) in ignored_entries(&status)? {
        walk.entry(relative, true, &mut plan)?;
    }
    plan.discarded.sort_by(|left, right| left.0.cmp(&right.0));
    plan.retained.sort();
    plan.retained.dedup();
    Ok(plan)
}

/// Report the classified entries and, with `apply`, delete the disposable ones.
fn finish_discard(
    worktree: &Path,
    plan: Plan,
    guard: &Guard,
    apply: bool,
) -> Result<CacheClassification, Refusal> {
    // A path that is not UTF-8 cannot be reported, so nothing is deleted that the report could
    // not name.
    if let Some((relative, _)) = plan
        .discarded
        .iter()
        .find(|(relative, _)| relative.to_str().is_none())
    {
        return Err(Refusal::new(
            "cache-path-not-utf8",
            format!(
                "{} is ignored and its path is not UTF-8; rename or delete it, then retry",
                worktree.join(relative).display()
            ),
        ));
    }
    let users = processes_using(worktree);
    if let Some(active) = users.as_ref().filter(|users| apply && !users.is_empty()) {
        return Err(in_use(worktree, active));
    }
    let mut discarded = Vec::with_capacity(plan.discarded.len());
    let mut retained = plan.retained;
    for (relative, kind) in plan.discarded {
        // Re-observed immediately before deletion, whatever the kind: a kept path or a
        // repository that appeared at or below the entry since it was classified keeps it.
        if guard.protected_now(worktree, &relative)? {
            retained.push(relative);
            continue;
        }
        let path = worktree.join(&relative);
        let allocated_bytes = allocated_bytes(&path);
        if apply {
            remove(worktree, &relative, kind)?;
        }
        discarded.push(DiscardedCache {
            path: relative,
            kind,
            allocated_bytes,
        });
    }
    retained.sort();
    retained.dedup();
    Ok(CacheClassification {
        discarded,
        retained_bytes: retained
            .iter()
            .map(|relative| allocated_bytes(&worktree.join(relative)))
            .sum(),
        retained_ignored: retained,
        processes_observed: users.is_some(),
    })
}

/// Ignored porcelain entries, each with whether Git reported it as a directory.
fn ignored_entries(status: &[u8]) -> Result<Vec<(PathBuf, bool)>, Refusal> {
    let invalid = || {
        Refusal::new(
            "invalid-git-status",
            "Git returned an invalid porcelain status record",
        )
    };
    let mut entries = Vec::new();
    let mut fields = status
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty());
    while let Some(field) = fields.next() {
        if field.len() < 4 || field[2] != b' ' {
            return Err(invalid());
        }
        let code = &field[..2];
        if code != b"!!" {
            if code.iter().any(|byte| matches!(byte, b'R' | b'C')) && fields.next().is_none() {
                return Err(invalid());
            }
            continue;
        }
        let raw = &field[3..];
        let directory = raw.ends_with(b"/");
        let relative = crate::path_from_git_bytes(raw.strip_suffix(b"/").unwrap_or(raw))?;
        if !relative
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
        {
            return Err(invalid());
        }
        entries.push((relative, directory));
    }
    Ok(entries)
}

/// Every path the tree's index tracks.
pub(crate) fn tracked_paths(worktree: &Path) -> Result<BTreeSet<PathBuf>, Refusal> {
    ProcessGit::output_bytes(worktree, ["ls-files", "-z"])?
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(crate::path_from_git_bytes)
        .collect()
}

/// One classification of a tree's ignored entries.
struct Walk<'a> {
    worktree: &'a Path,
    guard: &'a Guard,
    /// The index's paths.
    tracked: BTreeSet<PathBuf>,
    /// HEAD's and the index's paths, read once a candidate needs them ([`head_or_index_paths`]).
    committed: Option<BTreeSet<Vec<u8>>>,
}

impl Walk<'_> {
    /// Classify one ignored entry: kept, a repository, on another device or unreadable is
    /// retained; a file or symbolic link is `ignored-output`; a directory goes to [`Self::directory`].
    /// `top` marks an entry Git reported itself, the only place a Cargo target is recorded.
    fn entry(&mut self, relative: PathBuf, top: bool, plan: &mut Plan) -> Result<(), Refusal> {
        if self.guard.kept(&relative) {
            plan.retained.push(relative);
            return Ok(());
        }
        match self.guard.observe(&self.worktree.join(&relative)) {
            Observed::Missing => {}
            Observed::Retained => plan.retained.push(relative),
            Observed::Unlinkable => plan.discarded.push((relative, CacheKind::IgnoredOutput)),
            Observed::Directory => self.directory(relative, top, plan)?,
        }
        Ok(())
    }

    /// Classify a real directory that is neither kept nor a repository, naming it by the first
    /// structural rule that recognises it, else `ignored-output`.
    fn directory(&mut self, relative: PathBuf, top: bool, plan: &mut Plan) -> Result<(), Refusal> {
        let path = self.worktree.join(&relative);
        let name = relative.file_name().and_then(|name| name.to_str());
        let parent = relative.parent().unwrap_or(Path::new(""));
        let kind = if name == Some("node_modules") && lockfile_at_or_above(parent, &self.tracked) {
            Some(CacheKind::NodeModules)
        } else if regular_file(&path.join(PYTHON_VIRTUALENV_MARKER))
            && python_manifest_beside(parent, &self.tracked)
        {
            Some(CacheKind::PythonVirtualenv)
        } else if name.is_some_and(|name| TAGGED_TOOL_CACHES.contains(&name)) && cache_tagged(&path)
        {
            Some(CacheKind::ToolCache)
        } else if cargo_profile(&path) && cargo_target_root(self.worktree, parent, &self.tracked) {
            if top {
                plan.targets.push(parent.to_path_buf());
            }
            Some(CacheKind::CargoProfile)
        } else {
            None
        };
        if let Some(kind) = kind {
            self.discard_or_parts(relative, kind, plan)?;
            return Ok(());
        }
        if cargo_target_root(self.worktree, &relative, &self.tracked) {
            match self.cargo_target(&relative, true)? {
                // Whole only when no part was kept, a repository or holds one.
                TargetShape::Whole => {
                    if top {
                        plan.targets.push(relative.clone());
                    }
                    plan.discarded.push((relative, CacheKind::CargoTarget));
                }
                TargetShape::Partial(inner) => {
                    if top {
                        plan.targets.push(relative);
                    }
                    plan.extend(inner);
                }
                TargetShape::NotCargo => {
                    let kind = if self.flutter_output(&relative)? {
                        CacheKind::FlutterOutput
                    } else {
                        CacheKind::IgnoredOutput
                    };
                    self.discard_or_parts(relative, kind, plan)?;
                }
            }
            return Ok(());
        }
        let kind = if self.flutter_output(&relative)? {
            CacheKind::FlutterOutput
        } else {
            CacheKind::IgnoredOutput
        };
        self.discard_or_parts(relative, kind, plan)
    }

    /// Plan the directory `relative` for deletion as `kind`, or, when it holds a kept path or a
    /// repository at any depth ([`Guard::holds_protected`]), classify each direct entry instead.
    fn discard_or_parts(
        &mut self,
        relative: PathBuf,
        kind: CacheKind,
        plan: &mut Plan,
    ) -> Result<(), Refusal> {
        if self.guard.holds_protected(self.worktree, &relative) {
            self.parts(relative, plan)
        } else {
            plan.discarded.push((relative, kind));
            Ok(())
        }
    }

    /// Classify each direct entry of the directory `relative` by the general rule. One whose
    /// names cannot be read is retained whole.
    fn parts(&mut self, relative: PathBuf, plan: &mut Plan) -> Result<(), Refusal> {
        let Ok(mut names) = read_names(&self.worktree.join(&relative)) else {
            plan.retained.push(relative);
            return Ok(());
        };
        names.sort();
        for name in names {
            self.entry(relative.join(name), false, plan)?;
        }
        Ok(())
    }

    /// Plan one Cargo part (`kind`) of a target, or one direct entry of its `tmp/`: retained when
    /// kept or a repository, whole when it holds nothing protected, otherwise split. A file or
    /// symbolic link is unlinked, as `cargo-target-tmp` in `tmp/` and `ignored-output`
    /// elsewhere. Returns whether it went whole as `kind`.
    fn cargo_part(
        &mut self,
        relative: PathBuf,
        kind: CacheKind,
        plan: &mut Plan,
    ) -> Result<bool, Refusal> {
        if self.guard.kept(&relative) {
            plan.retained.push(relative);
            return Ok(false);
        }
        match self.guard.observe(&self.worktree.join(&relative)) {
            Observed::Missing => Ok(false),
            Observed::Retained => {
                plan.retained.push(relative);
                Ok(false)
            }
            Observed::Unlinkable if kind == CacheKind::CargoTargetTmp => {
                plan.discarded.push((relative, kind));
                Ok(false)
            }
            Observed::Unlinkable => {
                plan.discarded.push((relative, CacheKind::IgnoredOutput));
                Ok(false)
            }
            Observed::Directory if self.guard.holds_protected(self.worktree, &relative) => {
                self.parts(relative, plan)?;
                Ok(false)
            }
            Observed::Directory => {
                plan.discarded.push((relative, kind));
                Ok(true)
            }
        }
    }

    /// Plan Cargo's `tmp/` (`scratch`) beside a profile: whole as `cargo-target-tmp` when it
    /// holds nothing protected; otherwise each direct entry by the general rule, a disposable
    /// one named `cargo-target-tmp`. Returns whether it went whole.
    fn scratch(&mut self, scratch: PathBuf, plan: &mut Plan) -> Result<bool, Refusal> {
        let path = self.worktree.join(&scratch);
        if self.guard.kept(&scratch) || !matches!(self.guard.observe(&path), Observed::Directory) {
            return self.cargo_part(scratch, CacheKind::CargoTargetTmp, plan);
        }
        if !self.guard.holds_protected(self.worktree, &scratch) {
            plan.discarded.push((scratch, CacheKind::CargoTargetTmp));
            return Ok(true);
        }
        let Ok(mut names) = read_names(&path) else {
            plan.retained.push(scratch);
            return Ok(false);
        };
        names.sort();
        for name in names {
            self.cargo_part(scratch.join(name), CacheKind::CargoTargetTmp, plan)?;
        }
        Ok(false)
    }

    /// Classify the children of a tagged target, or of a target-triple directory below one.
    fn cargo_target(&mut self, relative: &Path, tagged: bool) -> Result<TargetShape, Refusal> {
        let directory = self.worktree.join(relative);
        let mut children = read_names(&directory)?;
        children.sort();
        let mut inner = Plan::default();
        let mut found_profile = false;
        let mut whole = true;
        // Cargo's metadata files and empty directories: they go with a whole target, and on
        // their own beside anything else.
        let mut loose = Vec::new();
        // Cargo's `tmp` below a tagged target is cache, but only beside a profile, which is known
        // once every child was read.
        let mut scratch = None;
        for name in children {
            let child = relative.join(&name);
            let path = self.worktree.join(&child);
            // A repository's `.git` is never a target's part, whatever it holds.
            if name == GIT_ENTRY {
                inner.retained.push(child);
                whole = false;
                continue;
            }
            if real_directory(&path) {
                if tagged && name == CARGO_TARGET_TMP {
                    scratch = Some(child);
                    continue;
                }
                if cargo_profile(&path) {
                    found_profile = true;
                    whole &= self.cargo_part(child, CacheKind::CargoProfile, &mut inner)?;
                    continue;
                }
                // A repository's root, an unreadable directory or one on another device.
                if !matches!(self.guard.observe(&path), Observed::Directory) {
                    whole = false;
                    self.entry(child, false, &mut inner)?;
                    continue;
                }
                let nested_tag = cache_tagged(&path);
                if nested_tag || holds_profile(&path)? {
                    match self.cargo_target(&child, nested_tag)? {
                        TargetShape::Whole => {
                            found_profile = true;
                            whole &= self.cargo_part(child, CacheKind::CargoTarget, &mut inner)?;
                        }
                        TargetShape::Partial(plan) => {
                            found_profile = true;
                            whole = false;
                            if self.guard.kept(&child)
                                || matches!(self.guard.observe(&path), Observed::Retained)
                            {
                                inner.retained.push(child);
                            } else {
                                inner.extend(plan);
                            }
                        }
                        TargetShape::NotCargo => {
                            whole = false;
                            self.entry(child, false, &mut inner)?;
                        }
                    }
                    continue;
                }
                if read_names(&path)?.is_empty() && !self.guard.kept(&child) {
                    loose.push(child);
                    continue;
                }
            } else if tagged
                && regular_file(&path)
                && cargo_metadata(&directory, &name)
                && !self.guard.kept(&child)
            {
                loose.push(child);
                continue;
            }
            whole = false;
            self.entry(child, false, &mut inner)?;
        }
        if let Some(scratch) = scratch {
            if found_profile {
                whole &= self.scratch(scratch, &mut inner)?;
            } else {
                whole = false;
            }
        }
        Ok(if !found_profile {
            TargetShape::NotCargo
        } else if whole && inner.retained.is_empty() {
            TargetShape::Whole
        } else {
            for child in loose {
                self.entry(child, false, &mut inner)?;
            }
            TargetShape::Partial(inner)
        })
    }

    /// Whether the directory `relative` is a Flutter app's output by the one build-output
    /// classifier ([`flutter_output_kind`]), observed on disk. HEAD's and the index's paths are
    /// read only for a directory whose name could make it one.
    fn flutter_output(&mut self, relative: &Path) -> Result<bool, Refusal> {
        let Some(directory) = relative.to_str().map(str::as_bytes) else {
            return Ok(false);
        };
        let named = relative
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| FlutterOutputKind::named(name.as_bytes()).is_some());
        if !named {
            return Ok(false);
        }
        let committed = match &mut self.committed {
            Some(paths) => paths,
            None => self
                .committed
                .insert(head_or_index_paths(self.worktree, "HEAD")?),
        };
        let (worktree, tracked) = (self.worktree, &self.tracked);
        Ok(flutter_output_kind(directory, |directory, manifest| {
            flutter_facts(worktree, directory, manifest, committed, tracked)
        })
        .is_some())
    }
}

/// Whether `directory` is itself a Git directory, whatever its name: `HEAD` a regular file, and
/// `objects/` and `refs/` real directories, none reached through a symbolic link. A bare
/// repository and a `--separate-git-dir` have no `.git` entry, so only this structure shows them.
/// The one predicate on disk: `archive` and removal ask it too
/// ([`b10x_worktree_domain::repository_path`]), so what `discard-cache` keeps is never build layout.
pub(crate) fn git_directory(directory: &Path) -> bool {
    GitDirectoryFacts {
        head_file: regular_file(&directory.join(GIT_DIRECTORY_HEAD)),
        objects_directory: real_directory(&directory.join(GIT_DIRECTORY_OBJECTS)),
        refs_directory: real_directory(&directory.join(GIT_DIRECTORY_REFS)),
    }
    .recognises()
}

/// Every path HEAD's tree (`head`) or the tree's index tracks, as raw bytes: such a file is
/// never build layout, and a directory holding one is no Flutter output.
pub(crate) fn head_or_index_paths(
    worktree: &Path,
    head: &str,
) -> Result<BTreeSet<Vec<u8>>, Refusal> {
    let indexed = ProcessGit::output_bytes(worktree, ["ls-files", "-z"])?;
    let committed = ProcessGit::output_bytes(
        worktree,
        ["ls-tree", "-r", "-z", "--name-only", "--full-tree", head],
    )?;
    Ok(indexed
        .split(|byte| *byte == 0)
        .chain(committed.split(|byte| *byte == 0))
        .filter(|path| !path.is_empty())
        .map(<[u8]>::to_vec)
        .collect())
}

/// What the disk shows of one candidate Flutter output directory, the observations the one
/// classifier takes: `None` unless `directory` is a real directory; `manifest` present as a
/// regular file; anything `tracked` (HEAD's and the index's paths) at or below `directory`; and
/// whether `directory` is itself a cargo target by the archive's structural rule
/// ([`archive_target`], reading `indexed`, the index's paths), which then classifies it alone. A
/// target that cannot be read counts as one, so nothing below it is taken for Flutter output.
/// Both paths are relative to `worktree` and UTF-8, as the classifier passes them.
pub(crate) fn flutter_facts(
    worktree: &Path,
    directory: &[u8],
    manifest: &[u8],
    tracked: &BTreeSet<Vec<u8>>,
    indexed: &BTreeSet<PathBuf>,
) -> Option<FlutterFacts> {
    let at = |relative: &[u8]| {
        std::str::from_utf8(relative)
            .ok()
            .map(|relative| worktree.join(relative))
    };
    if !real_directory(&at(directory)?) {
        return None;
    }
    let mut below = directory.to_vec();
    below.push(b'/');
    Some(FlutterFacts {
        manifest_beside: at(manifest).is_some_and(|manifest| regular_file(&manifest)),
        tracked: tracked.contains(directory)
            || tracked
                .range(below.clone()..)
                .next()
                .is_some_and(|path| path.starts_with(&below)),
        cargo_target: std::str::from_utf8(directory).map_or(true, |relative| {
            archive_target(worktree, Path::new(relative), indexed).unwrap_or(true)
        }),
    })
}

/// Whether `relative` is a Cargo target classified as tagged: it carries a valid `CACHEDIR.TAG`,
/// or Cargo did not create it and so wrote none, in which case it must be a real directory named
/// `target`, beside a tracked `Cargo.toml`, with a direct child other than Cargo's `tmp` holding
/// both `.fingerprint/` and `deps/` as real directories. An unreadable directory does not qualify.
fn cargo_target_root(worktree: &Path, relative: &Path, tracked: &BTreeSet<PathBuf>) -> bool {
    let directory = worktree.join(relative);
    if cache_tagged(&directory) {
        return true;
    }
    let parent = relative.parent().unwrap_or(Path::new(""));
    relative.file_name().and_then(|name| name.to_str()) == Some(CARGO_UNTAGGED_TARGET)
        && real_directory(&directory)
        && tracked.contains(&parent.join(CARGO_MANIFEST))
        && std::fs::read_dir(&directory).is_ok_and(|entries| {
            entries
                .flatten()
                .map(|entry| entry.file_name())
                // `tmp` is Cargo's test scratch, never a profile, whatever it holds.
                .filter(|name| *name != CARGO_TARGET_TMP)
                .any(|name| full_cargo_profile(&directory.join(name)))
        })
}

/// Whether `name`, a regular file at a target's root, is one Cargo writes there. A
/// `CACHEDIR.TAG` is Cargo's only when its signature is valid: a target that counts as tagged
/// without one may hold a file of that name Cargo did not write.
fn cargo_metadata(directory: &Path, name: &std::ffi::OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        CARGO_TARGET_METADATA.contains(&name)
            && (name != CACHEDIR_TAG_FILE || cache_tagged(directory))
    })
}

fn read_names(directory: &Path) -> Result<Vec<std::ffi::OsString>, Refusal> {
    std::fs::read_dir(directory)
        .and_then(|entries| {
            entries
                .map(|entry| entry.map(|entry| entry.file_name()))
                .collect()
        })
        .map_err(|error| {
            Refusal::new(
                "cache-classification-failed",
                format!("{}: {error}", directory.display()),
            )
        })
}

fn real_directory(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_dir())
}

fn regular_file(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.is_file())
}

/// A Cargo profile: a real directory holding `.fingerprint/` as a real directory.
pub(crate) fn cargo_profile(path: &Path) -> bool {
    real_directory(path) && real_directory(&path.join(CARGO_PROFILE_MARKER))
}

/// A profile as only Cargo leaves one: `.fingerprint/` and `deps/`, both real directories.
fn full_cargo_profile(path: &Path) -> bool {
    cargo_profile(path) && real_directory(&path.join(CARGO_PROFILE_DEPS))
}

/// Whether any direct child is a Cargo profile, as in `target/<triple>/debug`.
fn holds_profile(path: &Path) -> Result<bool, Refusal> {
    Ok(read_names(path)?
        .into_iter()
        .any(|name| cargo_profile(&path.join(name))))
}

fn cache_tagged(directory: &Path) -> bool {
    let tag = directory.join(CACHEDIR_TAG_FILE);
    if !regular_file(&tag) {
        return false;
    }
    let mut head = vec![0; CACHEDIR_TAG_SIGNATURE.len()];
    std::fs::File::open(&tag)
        .and_then(|mut file| file.read_exact(&mut head))
        .is_ok_and(|()| is_cache_tag(&head))
}

fn lockfile_at_or_above(directory: &Path, tracked: &BTreeSet<PathBuf>) -> bool {
    directory.ancestors().any(|ancestor| {
        NODE_LOCKFILES
            .iter()
            .any(|lockfile| tracked.contains(&ancestor.join(lockfile)))
    })
}

fn python_manifest_beside(directory: &Path, tracked: &BTreeSet<PathBuf>) -> bool {
    [directory, Path::new("")].iter().any(|place| {
        PYTHON_MANIFESTS
            .iter()
            .any(|manifest| tracked.contains(&place.join(manifest)))
    })
}

fn allocated_bytes(path: &Path) -> u64 {
    if !real_directory(path) {
        return std::fs::symlink_metadata(path).map_or(0, |metadata| file_allocation(&metadata));
    }
    crate::inspection::measure(path, u64::MAX).map_or(0, |observation| {
        observation
            .allocated_bytes
            .unwrap_or(observation.logical_bytes)
    })
}

fn file_allocation(metadata: &std::fs::Metadata) -> u64 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt as _;
        metadata.blocks() * 512
    }
    #[cfg(not(unix))]
    {
        metadata.len()
    }
}

/// The latest change to the tree's own Git index, HEAD or HEAD log, in Unix seconds.
///
/// `git status`, staging, commits and checkouts touch these; a build does not.
pub(crate) fn last_activity(worktree: &Path) -> Option<i64> {
    let gitfile = std::fs::read_to_string(worktree.join(".git")).ok()?;
    let admin = PathBuf::from(gitfile.strip_prefix("gitdir:")?.trim());
    let admin = if admin.is_absolute() {
        admin
    } else {
        worktree.join(admin)
    };
    ["index", "HEAD", "logs/HEAD"]
        .iter()
        .filter_map(|name| std::fs::metadata(admin.join(name)).ok()?.modified().ok())
        .filter_map(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|elapsed| i64::try_from(elapsed.as_secs()).unwrap_or(i64::MAX))
        .max()
}

/// Delete one planned entry, `relative` below `worktree`, only while every directory between the
/// tree root and it is still a real directory, so a deletion never leaves the tree. A file or
/// symbolic link of the kinds that may be one (`ignored-output`, a direct entry of Cargo's
/// `tmp/`) is unlinked, never followed; a directory is deleted with everything below it, without
/// following a symbolic link; every other kind must still be a real directory.
fn remove(worktree: &Path, relative: &Path, kind: CacheKind) -> Result<(), Refusal> {
    let path = worktree.join(relative);
    let changed =
        |what: &str| Refusal::new("cache-state-changed", format!("{} {what}", path.display()));
    let inside = relative
        .ancestors()
        .skip(1)
        .filter(|ancestor| !ancestor.as_os_str().is_empty())
        .all(|ancestor| real_directory(&worktree.join(ancestor)));
    if !inside {
        return Err(changed("is no longer below real directories of the tree"));
    }
    let failed = |error: std::io::Error| {
        Refusal::new(
            "cache-delete-failed",
            format!("{}: {error}", path.display()),
        )
    };
    let metadata = std::fs::symlink_metadata(&path).map_err(|_| changed("no longer exists"))?;
    if !metadata.is_dir() {
        if matches!(kind, CacheKind::IgnoredOutput | CacheKind::CargoTargetTmp) {
            return std::fs::remove_file(&path).map_err(failed);
        }
        return Err(changed("is no longer a directory"));
    }
    crate::make_directories_owner_writable(&path)?;
    std::fs::remove_dir_all(&path).map_err(failed)
}

fn in_use(worktree: &Path, users: &[(u32, String)]) -> Refusal {
    let named = users
        .iter()
        .map(|(pid, name)| format!("{pid} ({name})"))
        .collect::<Vec<_>>()
        .join(", ");
    Refusal::new(
        "worktree-in-use",
        format!(
            "process {named} has its working directory, executable or an open file in {}; \
             stop it or wait for it, then retry",
            worktree.display()
        ),
    )
}

/// Processes other than this one and its ancestors using the tree, or `None` where processes
/// cannot be observed.
#[cfg(target_os = "linux")]
fn processes_using(worktree: &Path) -> Option<Vec<(u32, String)>> {
    let entries = std::fs::read_dir("/proc").ok()?;
    let exempt = own_ancestry();
    let inside = |link: std::io::Result<PathBuf>| link.is_ok_and(|path| path.starts_with(worktree));
    let mut users = Vec::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        if exempt.contains(&pid) {
            continue;
        }
        let base = entry.path();
        let holds_tree = inside(std::fs::read_link(base.join("cwd")))
            || inside(std::fs::read_link(base.join("exe")))
            || std::fs::read_dir(base.join("fd")).is_ok_and(|descriptors| {
                descriptors
                    .flatten()
                    .any(|descriptor| inside(std::fs::read_link(descriptor.path())))
            });
        if holds_tree {
            let name = std::fs::read_to_string(base.join("comm")).unwrap_or_default();
            users.push((pid, name.trim().to_owned()));
        }
    }
    users.sort();
    Some(users)
}

#[cfg(not(target_os = "linux"))]
fn processes_using(_worktree: &Path) -> Option<Vec<(u32, String)>> {
    None
}

/// This process and every ancestor, which may legitimately run from inside the tree.
#[cfg(target_os = "linux")]
fn own_ancestry() -> BTreeSet<u32> {
    let mut chain = BTreeSet::new();
    let mut pid = std::process::id();
    while pid > 1 && chain.insert(pid) {
        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            break;
        };
        // The command name is parenthesised and may itself hold spaces or parentheses.
        let Some(parent) = stat
            .rsplit_once(')')
            .and_then(|(_, rest)| rest.split_whitespace().nth(1))
            .and_then(|field| field.parse::<u32>().ok())
        else {
            break;
        };
        pid = parent;
    }
    chain
}

#[cfg(test)]
mod tests {
    use super::*;

    const KINDS: [CacheKind; 8] = [
        CacheKind::CargoProfile,
        CacheKind::CargoTarget,
        CacheKind::CargoTargetTmp,
        CacheKind::FlutterOutput,
        CacheKind::NodeModules,
        CacheKind::PythonVirtualenv,
        CacheKind::ToolCache,
        CacheKind::IgnoredOutput,
    ];

    fn git(path: &Path, args: &[&str]) {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A `.git` that appears below a planned directory after classification, as a file, a
    /// directory or a symbolic link, keeps the directory whole at deletion, whatever its kind.
    #[cfg(unix)]
    #[test]
    fn a_git_entry_found_immediately_before_deletion_retains_every_kind_whole() {
        for (index, kind) in KINDS.into_iter().enumerate() {
            for shape in ["file", "directory", "symlink"] {
                let root = tempfile::tempdir().unwrap();
                let guard = Guard::read(root.path()).unwrap();
                let relative = PathBuf::from(format!("cache-{index}"));
                let nested = root.path().join(&relative).join("deep/fixture");
                std::fs::create_dir_all(&nested).unwrap();
                std::fs::write(nested.join("file"), "only copy\n").unwrap();
                let git = nested.join(".git");
                match shape {
                    "file" => std::fs::write(&git, "gitdir: elsewhere\n").unwrap(),
                    "directory" => std::fs::create_dir(&git).unwrap(),
                    _ => std::os::unix::fs::symlink("elsewhere", &git).unwrap(),
                }
                let plan = Plan {
                    discarded: vec![(relative.clone(), kind)],
                    ..Plan::default()
                };

                let result = finish_discard(root.path(), plan, &guard, true).unwrap();
                assert!(result.discarded.is_empty(), "{kind:?} with a {shape} .git");
                assert_eq!(result.retained_ignored, [relative]);
                assert!(
                    nested.join("file").is_file(),
                    "{kind:?} with a {shape} .git"
                );
            }
        }
    }

    /// A Git directory with no `.git` entry (a bare repository, or one `--separate-git-dir`
    /// placed) is recognised by its own structure: `HEAD` a regular file, `objects/` and `refs/`
    /// real directories. Anything less, or a symlinked part, is not one.
    #[cfg(unix)]
    #[test]
    fn a_git_directory_is_recognised_by_head_objects_and_refs() {
        let root = tempfile::tempdir().unwrap();
        let guard = Guard::structural();
        let make = |name: &str, head: bool, objects: bool, refs: bool| {
            let directory = root.path().join(name);
            std::fs::create_dir_all(&directory).unwrap();
            if head {
                std::fs::write(directory.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            }
            if objects {
                std::fs::create_dir(directory.join("objects")).unwrap();
            }
            if refs {
                std::fs::create_dir(directory.join("refs")).unwrap();
            }
            directory
        };
        let bare = make("remote.git", true, true, true);
        let only_head = make("only-head", true, false, false);
        let no_refs = make("no-refs", true, true, false);
        let linked_objects = make("linked-objects", true, false, true);
        std::os::unix::fs::symlink(bare.join("objects"), linked_objects.join("objects")).unwrap();
        let head_directory = make("head-directory", false, true, true);
        std::fs::create_dir(head_directory.join("HEAD")).unwrap();

        assert!(git_directory(&bare));
        assert!(matches!(guard.observe(&bare), Observed::Retained));
        for not_one in [&only_head, &no_refs, &linked_objects, &head_directory] {
            assert!(!git_directory(not_one), "{}", not_one.display());
        }
        assert!(matches!(guard.observe(&only_head), Observed::Directory));
        assert!(matches!(
            guard.observe(&linked_objects),
            Observed::Directory
        ));

        // One at any depth below a directory counts, as a `.git` does.
        let scratch = root.path().join("scratch");
        std::fs::create_dir_all(scratch.join("deep")).unwrap();
        assert!(!guard.holds_protected(root.path(), Path::new("scratch")));
        std::fs::rename(&bare, scratch.join("deep/remote.git")).unwrap();
        assert!(guard.holds_protected(root.path(), Path::new("scratch")));
    }

    /// A direct entry of Cargo's `tmp/` may be a file; it is deleted as `cargo-target-tmp`.
    #[test]
    fn a_file_in_target_tmp_is_discarded_as_target_tmp() {
        let root = tempfile::tempdir().unwrap();
        let guard = Guard::read(root.path()).unwrap();
        let relative = PathBuf::from("target/tmp/output.log");
        std::fs::create_dir_all(root.path().join("target/tmp")).unwrap();
        std::fs::write(root.path().join(&relative), "scratch\n").unwrap();
        let plan = Plan {
            discarded: vec![(relative.clone(), CacheKind::CargoTargetTmp)],
            ..Plan::default()
        };

        let result = finish_discard(root.path(), plan, &guard, true).unwrap();
        assert_eq!(result.discarded.len(), 1);
        assert_eq!(result.discarded[0].path, relative);
        assert_eq!(result.retained_ignored, [] as [PathBuf; 0]);
        assert!(!root.path().join(&relative).exists());
    }

    /// The keep file is read again immediately before each deletion: a pattern written after
    /// classification keeps the entry it matches, and one that keeps a path below a planned
    /// directory keeps the directory.
    #[test]
    fn a_path_kept_after_classification_is_retained_at_deletion() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        std::fs::create_dir_all(root.path().join("records")).unwrap();
        std::fs::write(root.path().join("records/notes.md"), "a record\n").unwrap();
        std::fs::create_dir_all(root.path().join("output/evidence")).unwrap();
        std::fs::write(root.path().join("output/evidence/run.json"), "{}\n").unwrap();
        std::fs::write(root.path().join("app.log"), "log\n").unwrap();
        let guard = Guard::read(root.path()).unwrap();
        assert!(guard.kept.is_empty());
        let plan = Plan {
            discarded: vec![
                (PathBuf::from("app.log"), CacheKind::IgnoredOutput),
                (PathBuf::from("output"), CacheKind::IgnoredOutput),
                (PathBuf::from("records"), CacheKind::IgnoredOutput),
            ],
            ..Plan::default()
        };
        std::fs::write(root.path().join(KEEP_FILE), "records/\noutput/evidence/\n").unwrap();

        let result = finish_discard(root.path(), plan, &guard, true).unwrap();
        assert_eq!(
            result
                .discarded
                .iter()
                .map(|entry| entry.path.clone())
                .collect::<Vec<_>>(),
            [PathBuf::from("app.log")]
        );
        assert_eq!(
            result.retained_ignored,
            [PathBuf::from("output"), PathBuf::from("records")]
        );
        assert!(root.path().join("records/notes.md").is_file());
        assert!(root.path().join("output/evidence/run.json").is_file());
        assert!(!root.path().join("app.log").exists());
    }

    /// A keep file that turns into a symbolic link after classification keeps everything.
    #[cfg(unix)]
    #[test]
    fn a_keep_file_that_turns_invalid_keeps_everything() {
        let root = tempfile::tempdir().unwrap();
        git(root.path(), &["init", "-q"]);
        std::fs::write(root.path().join(KEEP_FILE), "records/\n").unwrap();
        std::fs::create_dir_all(root.path().join("records")).unwrap();
        std::fs::write(root.path().join("records/notes.md"), "a record\n").unwrap();
        std::fs::write(root.path().join("app.log"), "log\n").unwrap();
        let guard = Guard::read(root.path()).unwrap();
        assert!(guard.kept(Path::new("records/notes.md")));
        let plan = Plan {
            discarded: vec![(PathBuf::from("app.log"), CacheKind::IgnoredOutput)],
            ..Plan::default()
        };
        std::fs::remove_file(root.path().join(KEEP_FILE)).unwrap();
        std::os::unix::fs::symlink("elsewhere", root.path().join(KEEP_FILE)).unwrap();

        let result = finish_discard(root.path(), plan, &guard, true).unwrap();
        assert_eq!(result.discarded, []);
        assert_eq!(result.retained_ignored, [PathBuf::from("app.log")]);
        assert!(root.path().join("app.log").is_file());
    }

    /// An entry on another device than the tree root (a mount point) is retained, and a
    /// directory holding one is never deleted whole.
    #[cfg(unix)]
    #[test]
    fn an_entry_on_another_device_is_retained() {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("mounted/inner")).unwrap();
        std::fs::write(root.path().join("mounted/inner/file"), "elsewhere\n").unwrap();
        let elsewhere = Guard {
            kept: BTreeSet::new(),
            device: Some(u64::MAX),
        };
        assert!(matches!(
            elsewhere.observe(&root.path().join("mounted")),
            Observed::Retained
        ));
        let root_device = Guard {
            device: device(&std::fs::symlink_metadata(root.path()).unwrap()),
            ..Guard::structural()
        };
        assert!(!root_device.holds_protected(root.path(), Path::new("mounted")));
        let below = Guard {
            device: device(&std::fs::symlink_metadata(root.path()).unwrap())
                .map(|device| device ^ 1),
            ..Guard::structural()
        };
        assert!(below.holds_protected(root.path(), Path::new("mounted")));
    }

    /// A deletion never leaves the tree: a planned path below a directory that became a symbolic
    /// link is refused, and what the link points at is untouched.
    #[cfg(unix)]
    #[test]
    fn a_deletion_through_a_symlinked_parent_is_refused() {
        let root = tempfile::tempdir().unwrap();
        let tree = root.path().join("tree");
        let outside = root.path().join("outside");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("file"), "outside\n").unwrap();
        std::os::unix::fs::symlink(&outside, tree.join("link")).unwrap();

        let refusal = remove(&tree, Path::new("link/file"), CacheKind::IgnoredOutput).unwrap_err();
        assert_eq!(refusal.code, "cache-state-changed", "{refusal}");
        assert_eq!(
            std::fs::read_to_string(outside.join("file")).unwrap(),
            "outside\n"
        );
    }
}
