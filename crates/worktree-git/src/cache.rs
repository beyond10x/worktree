//! Recognise, and on request delete, the build cache among a tree's ignored entries.
//!
//! The walk follows no symbolic link and deletes only real directories inside the tree that a
//! rule in [`b10x_worktree_domain::CacheKind`] recognises, and the direct entries of Cargo's
//! `tmp/` it splits around a nested repository. A directory holding a `.git` at any depth is
//! never deleted whole. Every other ignored entry is reported as retained and left alone.

use crate::ProcessGit;
use b10x_worktree_domain::{
    CACHEDIR_TAG_FILE, CACHEDIR_TAG_SIGNATURE, CARGO_MANIFEST, CARGO_PROFILE_DEPS,
    CARGO_PROFILE_MARKER, CARGO_TARGET_METADATA, CARGO_TARGET_TMP, CARGO_UNTAGGED_TARGET,
    CacheClassification, CacheKind, DiscardedCache, FlutterFacts, FlutterOutputKind,
    GIT_DIRECTORY_HEAD, GIT_DIRECTORY_OBJECTS, GIT_DIRECTORY_REFS, GIT_ENTRY, GitDirectoryFacts,
    NODE_LOCKFILES, PYTHON_MANIFESTS, PYTHON_VIRTUALENV_MARKER, Refusal, TAGGED_TOOL_CACHES,
    flutter_output_kind, is_cache_tag,
};
use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

/// Classified ignored entries, relative to the tree root.
#[derive(Default)]
struct Plan {
    discarded: Vec<(PathBuf, CacheKind)>,
    retained: Vec<PathBuf>,
    /// Cargo targets recognised at an ignored entry or as its parent.
    targets: Vec<PathBuf>,
}

/// What a tagged Cargo target turned out to hold.
enum TargetShape {
    /// Only profiles, nested targets of the same shape, Cargo metadata, empty directories and
    /// Cargo's `tmp`.
    Whole,
    /// Some cache and some other content.
    Partial(Plan),
    /// No profile at all: not a Cargo target, whatever its tag says. Holds the entries to
    /// retain, empty when only Cargo metadata and empty directories remain.
    NotCargo(Vec<PathBuf>),
}

pub(crate) fn discard(worktree: &Path, apply: bool) -> Result<CacheClassification, Refusal> {
    let plan = classify_tree(worktree)?;
    finish_discard(worktree, plan, apply)
}

/// The cargo targets `discard-cache` recognises among the tree's ignored entries, relative to the
/// tree root, in path order: each holds at least one profile and is tagged or counts as tagged.
/// Archives leave the build layout below them out ([`b10x_worktree_domain::is_build_layout`]).
pub(crate) fn cargo_targets(worktree: &Path) -> Result<Vec<PathBuf>, Refusal> {
    let mut targets = classify_tree(worktree)?.targets;
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
fn classify_tree(worktree: &Path) -> Result<Plan, Refusal> {
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
    let tracked = tracked_paths(worktree)?;
    let mut committed = None;
    let mut plan = Plan::default();
    for (relative, directory) in ignored_entries(&status)? {
        if directory {
            classify(worktree, &relative, &tracked, &mut committed, &mut plan)?;
        } else {
            plan.retained.push(relative);
        }
    }
    Ok(plan)
}

/// Report the classified entries and, with `apply`, delete the recognised cache.
fn finish_discard(
    worktree: &Path,
    plan: Plan,
    apply: bool,
) -> Result<CacheClassification, Refusal> {
    let users = processes_using(worktree);
    if let Some(active) = users.as_ref().filter(|users| apply && !users.is_empty()) {
        return Err(in_use(worktree, active));
    }
    let mut discarded = Vec::with_capacity(plan.discarded.len());
    let mut retained = plan.retained;
    for (relative, kind) in plan.discarded {
        let path = worktree.join(&relative);
        // Re-observed immediately before deletion, whatever the kind: a `.git` that appeared
        // at or below the entry since it was classified keeps it whole.
        if is_or_holds_git_entry(&path) {
            retained.push(relative);
            continue;
        }
        let allocated_bytes = allocated_bytes(&path);
        if apply {
            remove(&path, kind)?;
        }
        discarded.push(DiscardedCache {
            path: relative,
            kind,
            allocated_bytes,
        });
    }
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

/// Classify one ignored directory. `committed` holds, once a candidate needs it, every path HEAD
/// or the index tracks ([`head_or_index_paths`]).
fn classify(
    worktree: &Path,
    relative: &Path,
    tracked: &BTreeSet<PathBuf>,
    committed: &mut Option<BTreeSet<Vec<u8>>>,
    plan: &mut Plan,
) -> Result<(), Refusal> {
    let path = worktree.join(relative);
    if !real_directory(&path) {
        plan.retained.push(relative.to_path_buf());
        return Ok(());
    }
    let name = relative.file_name().and_then(|name| name.to_str());
    let parent = relative.parent().unwrap_or(Path::new(""));
    let kind = if name == Some("node_modules") && lockfile_at_or_above(parent, tracked) {
        Some(CacheKind::NodeModules)
    } else if regular_file(&path.join(PYTHON_VIRTUALENV_MARKER))
        && python_manifest_beside(parent, tracked)
    {
        Some(CacheKind::PythonVirtualenv)
    } else if name.is_some_and(|name| TAGGED_TOOL_CACHES.contains(&name)) && cache_tagged(&path) {
        Some(CacheKind::ToolCache)
    } else if cargo_profile(&path) && cargo_target_root(worktree, parent, tracked) {
        plan.targets.push(parent.to_path_buf());
        Some(CacheKind::CargoProfile)
    } else {
        None
    };
    if let Some(kind) = kind {
        plan.discard_unless_repository(worktree, relative.to_path_buf(), kind);
        return Ok(());
    }
    if cargo_target_root(worktree, relative, tracked) {
        match cargo_target(worktree, relative, true)? {
            // Whole only when nothing in it was retained, so by construction it holds no `.git`.
            TargetShape::Whole => {
                plan.targets.push(relative.to_path_buf());
                plan.discarded
                    .push((relative.to_path_buf(), CacheKind::CargoTarget));
            }
            TargetShape::Partial(inner) => {
                plan.targets.push(relative.to_path_buf());
                plan.discarded.extend(inner.discarded);
                plan.retained.extend(inner.retained);
            }
            TargetShape::NotCargo(_) if flutter_output(worktree, relative, tracked, committed)? => {
                plan.discard_unless_repository(
                    worktree,
                    relative.to_path_buf(),
                    CacheKind::FlutterOutput,
                );
            }
            TargetShape::NotCargo(children) if children.is_empty() => {
                plan.retained.push(relative.to_path_buf());
            }
            TargetShape::NotCargo(children) => plan.retained.extend(children),
        }
        return Ok(());
    }
    if flutter_output(worktree, relative, tracked, committed)? {
        plan.discard_unless_repository(worktree, relative.to_path_buf(), CacheKind::FlutterOutput);
        return Ok(());
    }
    plan.retained.push(relative.to_path_buf());
    Ok(())
}

impl Plan {
    /// Plan the recognised directory `relative` for deletion as `kind`, or retain it whole when
    /// it holds a repository at any depth below it ([`holds_git_entry`]): a nested repository is
    /// never cache, whatever kind of directory holds it.
    fn discard_unless_repository(&mut self, worktree: &Path, relative: PathBuf, kind: CacheKind) {
        if holds_git_entry(&worktree.join(&relative)) {
            self.retained.push(relative);
        } else {
            self.discarded.push((relative, kind));
        }
    }

    /// Plan Cargo's `tmp/` (`scratch`) beside a profile, keeping both lists in path order: whole
    /// as `cargo-target-tmp` when it holds no `.git`; otherwise each direct entry holding none
    /// as `cargo-target-tmp` and each holding one retained. A `tmp/` whose names cannot be read
    /// is retained whole.
    fn scratch(&mut self, worktree: &Path, scratch: PathBuf) {
        let directory = worktree.join(&scratch);
        if !holds_git_entry(&directory) {
            self.discard_sorted(scratch, CacheKind::CargoTargetTmp);
            return;
        }
        let Ok(mut names) = read_names(&directory) else {
            self.retain_sorted(scratch);
            return;
        };
        names.sort();
        for name in names {
            let entry = scratch.join(name);
            if is_or_holds_git_entry(&worktree.join(&entry)) {
                self.retain_sorted(entry);
            } else {
                self.discard_sorted(entry, CacheKind::CargoTargetTmp);
            }
        }
    }

    fn discard_sorted(&mut self, relative: PathBuf, kind: CacheKind) {
        let at = self.discarded.partition_point(|(path, _)| *path < relative);
        self.discarded.insert(at, (relative, kind));
    }

    fn retain_sorted(&mut self, relative: PathBuf) {
        let at = self.retained.partition_point(|path| *path < relative);
        self.retained.insert(at, relative);
    }
}

/// Whether the ignored directory `relative` is a Flutter app's output by the one build-output
/// classifier ([`flutter_output_kind`]), observed on disk. HEAD's and the index's paths are read
/// into `committed` only for a directory whose name could make it one; `tracked` holds the
/// index's paths.
fn flutter_output(
    worktree: &Path,
    relative: &Path,
    tracked: &BTreeSet<PathBuf>,
    committed: &mut Option<BTreeSet<Vec<u8>>>,
) -> Result<bool, Refusal> {
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
    let committed = match committed {
        Some(paths) => paths,
        None => committed.insert(head_or_index_paths(worktree, "HEAD")?),
    };
    Ok(flutter_output_kind(directory, |directory, manifest| {
        flutter_facts(worktree, directory, manifest, committed, tracked)
    })
    .is_some())
}

/// Whether `directory` holds a repository: an entry named `.git` (a file, a directory or a
/// symbolic link) at any depth below it, or a Git directory ([`git_directory`]: a bare
/// repository, or one placed with `--separate-git-dir`) at it or at any depth below it. A
/// repository there may hold commits no remote does, so `discard-cache` never deletes the
/// directory whole, whatever kind of cache it is. The walk follows no symbolic link. An entry
/// whose type or a directory whose names cannot be read counts as one, so it is retained too.
fn holds_git_entry(directory: &Path) -> bool {
    let mut pending = vec![directory.to_path_buf()];
    while let Some(current) = pending.pop() {
        if git_directory(&current) {
            return true;
        }
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
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(entry.path()),
                Ok(_) => {}
                Err(_) => return true,
            }
        }
    }
    false
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

/// Whether the entry at `path` is named `.git` or, as a real directory, is or holds a repository
/// ([`holds_git_entry`]). A file or symbolic link of another name holds none; an entry whose type
/// cannot be read counts as holding one, and a missing one as holding none (its deletion then
/// refuses as changed state).
fn is_or_holds_git_entry(path: &Path) -> bool {
    if path.file_name() == Some(std::ffi::OsStr::new(GIT_ENTRY)) {
        return true;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => holds_git_entry(path),
        Ok(_) => false,
        Err(error) => error.kind() != std::io::ErrorKind::NotFound,
    }
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

/// Classify the children of a tagged target, or of a target-triple directory below one.
fn cargo_target(worktree: &Path, relative: &Path, tagged: bool) -> Result<TargetShape, Refusal> {
    let directory = worktree.join(relative);
    let mut children = read_names(&directory)?;
    children.sort();
    let mut inner = Plan::default();
    let mut found_profile = false;
    // Cargo's `tmp` below a tagged target is cache, but only beside a profile, which is known
    // once every child was read.
    let mut scratch = None;
    for name in children {
        let child = relative.join(&name);
        let path = worktree.join(&child);
        // A repository's `.git` is never a target's part, whatever it holds.
        if name == GIT_ENTRY {
            inner.retained.push(child);
            continue;
        }
        if real_directory(&path) {
            if tagged && name == CARGO_TARGET_TMP {
                scratch = Some(child);
                continue;
            }
            if cargo_profile(&path) {
                found_profile = true;
                inner.discard_unless_repository(worktree, child, CacheKind::CargoProfile);
                continue;
            }
            let nested_tag = cache_tagged(&path);
            if nested_tag || holds_profile(&path)? {
                match cargo_target(worktree, &child, nested_tag)? {
                    TargetShape::Whole => {
                        found_profile = true;
                        inner.discarded.push((child, CacheKind::CargoTarget));
                    }
                    TargetShape::Partial(plan) => {
                        found_profile = true;
                        inner.discarded.extend(plan.discarded);
                        inner.retained.extend(plan.retained);
                    }
                    TargetShape::NotCargo(_) => inner.retained.push(child),
                }
                continue;
            }
            if !read_names(&path)?.is_empty() {
                inner.retained.push(child);
            }
        } else if !(tagged && regular_file(&path) && cargo_metadata(&directory, &name)) {
            inner.retained.push(child);
        }
    }
    if let Some(scratch) = scratch {
        if found_profile {
            inner.scratch(worktree, scratch);
        } else if !read_names(&worktree.join(&scratch))?.is_empty() {
            inner.retain_sorted(scratch);
        }
    }
    Ok(if !found_profile {
        TargetShape::NotCargo(inner.retained)
    } else if inner.retained.is_empty() {
        TargetShape::Whole
    } else {
        TargetShape::Partial(inner)
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

/// Delete one planned entry. Only a direct entry of Cargo's `tmp/` (`cargo-target-tmp`) may be a
/// file or symbolic link, which is unlinked, never followed; every other entry must still be a
/// real directory.
fn remove(path: &Path, kind: CacheKind) -> Result<(), Refusal> {
    let unlinkable = kind == CacheKind::CargoTargetTmp
        && std::fs::symlink_metadata(path)
            .is_ok_and(|metadata| metadata.is_file() || metadata.is_symlink());
    if unlinkable {
        return std::fs::remove_file(path).map_err(|error| {
            Refusal::new(
                "cache-delete-failed",
                format!("{}: {error}", path.display()),
            )
        });
    }
    if !real_directory(path) {
        return Err(Refusal::new(
            "cache-state-changed",
            format!("{} is no longer a directory", path.display()),
        ));
    }
    crate::make_directories_owner_writable(path)?;
    std::fs::remove_dir_all(path).map_err(|error| {
        Refusal::new(
            "cache-delete-failed",
            format!("{}: {error}", path.display()),
        )
    })
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

    const KINDS: [CacheKind; 7] = [
        CacheKind::CargoProfile,
        CacheKind::CargoTarget,
        CacheKind::CargoTargetTmp,
        CacheKind::FlutterOutput,
        CacheKind::NodeModules,
        CacheKind::PythonVirtualenv,
        CacheKind::ToolCache,
    ];

    /// A `.git` that appears below a planned directory after classification, as a file, a
    /// directory or a symbolic link, keeps the directory whole at deletion, whatever its kind.
    #[cfg(unix)]
    #[test]
    fn a_git_entry_found_immediately_before_deletion_retains_every_kind_whole() {
        for (index, kind) in KINDS.into_iter().enumerate() {
            for shape in ["file", "directory", "symlink"] {
                let root = tempfile::tempdir().unwrap();
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

                let result = finish_discard(root.path(), plan, true).unwrap();
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
        assert!(is_or_holds_git_entry(&bare));
        for not_one in [&only_head, &no_refs, &linked_objects, &head_directory] {
            assert!(!git_directory(not_one), "{}", not_one.display());
        }
        assert!(!is_or_holds_git_entry(&only_head));
        assert!(!is_or_holds_git_entry(&linked_objects));

        // One at any depth below a directory counts, as a `.git` does.
        let scratch = root.path().join("scratch");
        std::fs::create_dir_all(scratch.join("deep")).unwrap();
        assert!(!holds_git_entry(&scratch));
        std::fs::rename(&bare, scratch.join("deep/remote.git")).unwrap();
        assert!(holds_git_entry(&scratch));
    }

    /// A direct entry of Cargo's `tmp/` may be a file; it is deleted as `cargo-target-tmp`.
    #[test]
    fn a_file_in_target_tmp_is_discarded_as_target_tmp() {
        let root = tempfile::tempdir().unwrap();
        let relative = PathBuf::from("target/tmp/output.log");
        std::fs::create_dir_all(root.path().join("target/tmp")).unwrap();
        std::fs::write(root.path().join(&relative), "scratch\n").unwrap();
        let plan = Plan {
            discarded: vec![(relative.clone(), CacheKind::CargoTargetTmp)],
            ..Plan::default()
        };

        let result = finish_discard(root.path(), plan, true).unwrap();
        assert_eq!(result.discarded.len(), 1);
        assert_eq!(result.discarded[0].path, relative);
        assert_eq!(result.retained_ignored, [] as [PathBuf; 0]);
        assert!(!root.path().join(&relative).exists());
    }
}
