//! Recognise, and on request delete, the build cache among a tree's ignored entries.
//!
//! The walk follows no symbolic link and deletes only real directories inside the tree that a
//! rule in [`b10x_worktree_domain::CacheKind`] recognises. Every other ignored entry is reported
//! as retained and left alone.

use crate::ProcessGit;
use b10x_worktree_domain::{
    CACHEDIR_TAG_FILE, CACHEDIR_TAG_SIGNATURE, CARGO_MANIFEST, CARGO_PROFILE_DEPS,
    CARGO_PROFILE_MARKER, CARGO_TARGET_METADATA, CARGO_TARGET_TMP, CARGO_UNTAGGED_TARGET,
    CacheClassification, CacheKind, DiscardedCache, NODE_LOCKFILES, PYTHON_MANIFESTS,
    PYTHON_VIRTUALENV_MARKER, Refusal, TAGGED_TOOL_CACHES, is_cache_tag,
};
use std::collections::BTreeSet;
use std::io::Read as _;
use std::path::{Component, Path, PathBuf};

/// Classified ignored entries, relative to the tree root.
#[derive(Default)]
struct Plan {
    discarded: Vec<(PathBuf, CacheKind)>,
    retained: Vec<PathBuf>,
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
    let mut plan = Plan::default();
    for (relative, directory) in ignored_entries(&status)? {
        if directory {
            classify(worktree, &relative, &tracked, &mut plan)?;
        } else {
            plan.retained.push(relative);
        }
    }
    let users = processes_using(worktree);
    if let Some(active) = users.as_ref().filter(|users| apply && !users.is_empty()) {
        return Err(in_use(worktree, active));
    }
    let mut discarded = Vec::with_capacity(plan.discarded.len());
    for (relative, kind) in plan.discarded {
        let path = worktree.join(&relative);
        let allocated_bytes = allocated_bytes(&path);
        if apply {
            remove(&path)?;
        }
        discarded.push(DiscardedCache {
            path: relative,
            kind,
            allocated_bytes,
        });
    }
    Ok(CacheClassification {
        discarded,
        retained_bytes: plan
            .retained
            .iter()
            .map(|relative| allocated_bytes(&worktree.join(relative)))
            .sum(),
        retained_ignored: plan.retained,
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

fn tracked_paths(worktree: &Path) -> Result<BTreeSet<PathBuf>, Refusal> {
    ProcessGit::output_bytes(worktree, ["ls-files", "-z"])?
        .split(|byte| *byte == 0)
        .filter(|field| !field.is_empty())
        .map(crate::path_from_git_bytes)
        .collect()
}

fn classify(
    worktree: &Path,
    relative: &Path,
    tracked: &BTreeSet<PathBuf>,
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
        Some(CacheKind::CargoProfile)
    } else {
        None
    };
    if let Some(kind) = kind {
        plan.discarded.push((relative.to_path_buf(), kind));
        return Ok(());
    }
    if cargo_target_root(worktree, relative, tracked) {
        match cargo_target(worktree, relative, true)? {
            TargetShape::Whole => plan
                .discarded
                .push((relative.to_path_buf(), CacheKind::CargoTarget)),
            TargetShape::Partial(inner) => {
                plan.discarded.extend(inner.discarded);
                plan.retained.extend(inner.retained);
            }
            TargetShape::NotCargo(children) if children.is_empty() => {
                plan.retained.push(relative.to_path_buf());
            }
            TargetShape::NotCargo(children) => plan.retained.extend(children),
        }
        return Ok(());
    }
    plan.retained.push(relative.to_path_buf());
    Ok(())
}

/// Whether `relative` is a Cargo target classified as tagged: it carries a valid `CACHEDIR.TAG`,
/// or Cargo did not create it and so wrote none, in which case it must be a real directory named
/// `target`, beside a tracked `Cargo.toml`, with a direct child holding both `.fingerprint/` and
/// `deps/` as real directories. An unreadable directory does not qualify.
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
                .any(|entry| full_cargo_profile(&directory.join(entry.file_name())))
        })
}

/// Classify the children of a tagged target, or of a target-triple directory below one.
fn cargo_target(worktree: &Path, relative: &Path, tagged: bool) -> Result<TargetShape, Refusal> {
    let directory = worktree.join(relative);
    let mut children = read_names(&directory)?;
    children.sort();
    let mut inner = Plan::default();
    let mut found_profile = false;
    // Cargo's `tmp` below a tagged target is cache whatever it holds, but only beside a
    // profile, which is known once every child was read.
    let mut scratch = None;
    for name in children {
        let child = relative.join(&name);
        let path = worktree.join(&child);
        if real_directory(&path) {
            if tagged && name == CARGO_TARGET_TMP {
                scratch = Some(child);
                continue;
            }
            if cargo_profile(&path) {
                found_profile = true;
                inner.discarded.push((child, CacheKind::CargoProfile));
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
        } else if !(tagged
            && regular_file(&path)
            && name
                .to_str()
                .is_some_and(|name| CARGO_TARGET_METADATA.contains(&name)))
        {
            inner.retained.push(child);
        }
    }
    if let Some(scratch) = scratch {
        if found_profile {
            let at = inner.discarded.partition_point(|(path, _)| *path < scratch);
            inner
                .discarded
                .insert(at, (scratch, CacheKind::CargoTargetTmp));
        } else if !read_names(&worktree.join(&scratch))?.is_empty() {
            let at = inner.retained.partition_point(|path| *path < scratch);
            inner.retained.insert(at, scratch);
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

fn cargo_profile(path: &Path) -> bool {
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

fn remove(path: &Path) -> Result<(), Refusal> {
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
