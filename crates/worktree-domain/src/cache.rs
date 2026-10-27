//! What a tree may discard: every ignored entry Git reports, unless the repository keeps it.
//!
//! Git's own exclude rules say which paths a repository calls disposable. An ignored entry is
//! retained only when a keep file of the tree ([`KEEP_FILE`], at the root or in a directory)
//! keeps it, when it is or holds a repository, or when it sits on another device or cannot be
//! read; a directory holding a kept path or a repository is never deleted whole and falls to its
//! direct entries. Records an agent must keep therefore belong outside ignored directories or in
//! the keep file: on 2026-10-06 a prune deleted review records that committed evidence cites
//! under an ignored `target/`.
//!
//! A directory's name never makes it cache. The structural rules below name the build cache an
//! entry was also recognised as ([`CacheKind`]); every other disposable entry is
//! [`CacheKind::IgnoredOutput`].

use crate::{ArchiveEvidence, Refusal, WorktreeId, WorktreeRecord};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The keep file (`worktree.cache.KeepFile`), at the tree root and in any directory below it: Git
/// exclude patterns naming ignored paths `discard-cache` keeps. A nested keep file keeps paths at
/// and below its own directory, its patterns matched relative to that directory as Git reads a
/// nested `.gitignore`; it reaches nothing outside it. A path is kept when a pattern matches it
/// or one of its ancestors, as Git's own exclude engine decides; a directory a pattern names is
/// kept even when it holds no file. Absent, nothing is kept; a symbolic link, a directory or an
/// unreadable file at any of those places refuses the discard as `keep-file-invalid`. A keep file
/// itself is never discarded, and neither is the `.gitignore` of a directory that holds a nested
/// keep file: a tool that hides its own directory with it would otherwise show what the keep file
/// keeps as untracked once the `.gitignore` is gone. A nested keep file only adds keeps: its
/// negation lines (`!pattern`) are ignored, because Git would give the deeper file precedence and
/// take away what a file above keeps ([`drop_keep_negations`]). A kept path is never build layout:
/// `archive` keeps it in `dirty.patch`, and an invalid keep file keeps every path, so nothing is
/// left out.
pub const KEEP_FILE: &str = ".worktreekeep";

/// A nested keep file's `text` without its negation lines, the lines that start with `!`. A
/// nested keep file only adds keeps; Git would let such a line take away a path a keep file above
/// keeps, so the lines count for nothing.
pub fn drop_keep_negations(text: &[u8]) -> Vec<u8> {
    text.split_inclusive(|byte| *byte == b'\n')
        .filter(|line| !line.starts_with(b"!"))
        .flatten()
        .copied()
        .collect()
}

/// The file that marks a cache directory, per the Cache Directory Tagging Specification.
pub const CACHEDIR_TAG_FILE: &str = "CACHEDIR.TAG";

/// The bytes a valid `CACHEDIR.TAG` starts with.
pub const CACHEDIR_TAG_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// The directory Cargo writes into every profile directory and nowhere else.
pub const CARGO_PROFILE_MARKER: &str = ".fingerprint";

/// The directory Cargo writes compiled crates into, in every profile directory. A target
/// without a valid tag counts only through a profile holding both this and
/// [`CARGO_PROFILE_MARKER`] as real directories.
pub const CARGO_PROFILE_DEPS: &str = "deps";

/// The one name a Cargo target without a valid tag may carry to count as tagged.
///
/// Cargo writes `CACHEDIR.TAG` only when it creates the target directory itself, so a `target/`
/// made before Cargo's first build there has none.
pub const CARGO_UNTAGGED_TARGET: &str = "target";

/// The manifest that must be tracked beside an untagged [`CARGO_UNTAGGED_TARGET`].
pub const CARGO_MANIFEST: &str = "Cargo.toml";

/// Cargo's `CARGO_TARGET_TMPDIR`, directly below a target. Cargo reserves the name, so no
/// profile is ever called this.
pub const CARGO_TARGET_TMP: &str = "tmp";

/// Files Cargo writes at the root of a target directory.
pub const CARGO_TARGET_METADATA: [&str; 3] = [
    CACHEDIR_TAG_FILE,
    ".rustc_info.json",
    ".future-incompat-report.json",
];

/// Lockfiles from which npm, Yarn, pnpm and Bun recreate `node_modules`.
pub const NODE_LOCKFILES: [&str; 6] = [
    "package-lock.json",
    "npm-shrinkwrap.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "bun.lock",
    "bun.lockb",
];

/// The file every Python virtual environment holds at its root.
pub const PYTHON_VIRTUALENV_MARKER: &str = "pyvenv.cfg";

/// Manifests from which a Python virtual environment is recreated.
pub const PYTHON_MANIFESTS: [&str; 7] = [
    "pyproject.toml",
    "requirements.txt",
    "uv.lock",
    "poetry.lock",
    "Pipfile.lock",
    "setup.py",
    "setup.cfg",
];

/// Tool caches recognised when they also carry a valid `CACHEDIR.TAG`.
pub const TAGGED_TOOL_CACHES: [&str; 3] = [".pytest_cache", ".mypy_cache", ".ruff_cache"];

/// The name of a repository's administrative entry: a file, directory or symbolic link of this
/// name makes the directory holding it a repository, and a path through it is never cache or
/// build layout.
pub const GIT_ENTRY: &str = ".git";

/// The file every Git directory holds at its root ([`GitDirectoryFacts`]).
pub const GIT_DIRECTORY_HEAD: &str = "HEAD";

/// The object store every Git directory holds at its root ([`GitDirectoryFacts`]).
pub const GIT_DIRECTORY_OBJECTS: &str = "objects";

/// The reference store every Git directory holds at its root ([`GitDirectoryFacts`]).
pub const GIT_DIRECTORY_REFS: &str = "refs";

/// What a caller observed of one directory, the input of the one rule that makes a directory a
/// Git directory whatever its name: a bare repository, or one placed with `--separate-git-dir`,
/// has no `.git` entry, so only this structure shows it. `discard-cache` and `archive` observe
/// the disk, without following a symbolic link.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct GitDirectoryFacts {
    /// [`GIT_DIRECTORY_HEAD`] is a regular file.
    pub head_file: bool,
    /// [`GIT_DIRECTORY_OBJECTS`] is a real directory.
    pub objects_directory: bool,
    /// [`GIT_DIRECTORY_REFS`] is a real directory.
    pub refs_directory: bool,
}

impl GitDirectoryFacts {
    /// Whether these observations make the directory a Git directory.
    #[must_use]
    pub fn recognises(self) -> bool {
        self.head_file && self.objects_directory && self.refs_directory
    }
}

/// Whether `contents` is a valid `CACHEDIR.TAG`.
#[must_use]
pub fn is_cache_tag(contents: &[u8]) -> bool {
    contents.starts_with(CACHEDIR_TAG_SIGNATURE)
}

/// Why an ignored entry was discarded.
///
/// Every ignored entry Git reports is disposable unless the keep file ([`KEEP_FILE`]) keeps it,
/// it is a repository (an entry named `.git`, a directory holding one directly, or a Git
/// directory), it is a mount point or sits on another device than the tree root, or it cannot be
/// read (a directory whose names cannot be listed, a regular file that cannot be opened). A
/// directory holding any of those at any depth is never deleted whole: it falls to its direct
/// entries, each classified by the same rule. The structural variants name the build cache an entry was
/// recognised as; [`CacheKind::IgnoredOutput`] is every other disposable entry.
///
/// A Cargo target is tagged by a valid `CACHEDIR.TAG`. One without a valid tag counts as tagged
/// when it is named [`CARGO_UNTAGGED_TARGET`], a tracked [`CARGO_MANIFEST`] sits beside it, and a
/// direct child is a profile holding both [`CARGO_PROFILE_MARKER`] and [`CARGO_PROFILE_DEPS`] as
/// real directories. The Cargo kinds below then apply to it unchanged. A target holding anything
/// but Cargo's layout falls to its parts, each named by its own rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheKind {
    /// A directory holding `.fingerprint/` inside a tagged Cargo target.
    CargoProfile,
    /// A tagged Cargo target holding nothing but profiles, Cargo's own metadata and its `tmp/`,
    /// and no `.git` at any depth.
    CargoTarget,
    /// `tmp/` directly below a tagged Cargo target that holds a profile: Cargo's
    /// `CARGO_TARGET_TMPDIR`, which test binaries fill with scratch. When it holds a `.git` at
    /// any depth, each of its direct entries holding none instead, one by one.
    CargoTargetTmp,
    /// A real `P/build/` or `P/.dart_tool/` that the one build-output classifier
    /// ([`crate::flutter_output_kind`]) recognises as a Flutter app's output: `P/pubspec.yaml` is
    /// a regular file beside it, and neither HEAD's tree nor the index holds anything at or below
    /// it.
    FlutterOutput,
    /// `node_modules` at or below a directory holding a tracked lockfile.
    NodeModules,
    /// A directory holding `pyvenv.cfg` beside, or at the root of, a tracked Python manifest.
    PythonVirtualenv,
    /// A tagged `.pytest_cache`, `.mypy_cache` or `.ruff_cache`.
    ToolCache,
    /// An ignored file, symbolic link (deleted itself, never followed) or real directory that no
    /// structural variant recognises, that the keep file does not keep, and that neither is nor
    /// holds a repository or a kept path.
    IgnoredOutput,
}

/// One discarded ignored entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscardedCache {
    /// Path relative to the tree root.
    pub path: PathBuf,
    /// The rule that recognised it.
    pub kind: CacheKind,
    /// Allocated bytes observed before deletion, without following symbolic links.
    pub allocated_bytes: u64,
}

/// How a Git adapter classified one tree's ignored entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheClassification {
    /// Disposable entries; deleted when the discard was applied.
    pub discarded: Vec<DiscardedCache>,
    /// Ignored entries the keep file keeps, repositories, mount points, and entries on another
    /// device or not readable, relative to the tree root. Never deleted.
    pub retained_ignored: Vec<PathBuf>,
    /// Allocated bytes of the retained ignored entries, without following symbolic links.
    #[serde(default)]
    pub retained_bytes: u64,
    /// Whether running processes could be observed. When false, only the lease guarded the tree.
    pub processes_observed: bool,
}

/// The report of one cache discard (`worktree.cache.CacheDiscard`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheDiscard {
    /// Worktree id.
    pub id: WorktreeId,
    /// Exact tree path.
    pub path: PathBuf,
    /// Observation time.
    pub observed_at: i64,
    /// False for a dry-run: entries were classified and nothing was deleted.
    pub applied: bool,
    /// Disposable entries.
    pub discarded: Vec<DiscardedCache>,
    /// Retained ignored entries; `worktree archive` keeps these.
    pub retained_ignored: Vec<PathBuf>,
    /// Allocated bytes of the retained ignored entries.
    #[serde(default)]
    pub retained_bytes: u64,
    /// Whether running processes could be observed.
    pub processes_observed: bool,
}

/// What one sweep did, or would do, for one idle record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SweepItem {
    /// The record as the sweep found it. A sweep never changes lifecycle.
    pub record: WorktreeRecord,
    /// Seconds since the later of the recorded activity and the tree's own Git activity.
    pub idle_seconds: i64,
    /// The cache discard, or its classification in a dry-run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache: Option<CacheDiscard>,
    /// The archive written for an expired or finished tree, when one was needed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archive: Option<ArchiveEvidence>,
    /// Why the sweep left this record, or part of it, as it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refusal: Option<Refusal>,
}

impl CacheDiscard {
    /// Total allocated bytes of the discarded entries.
    #[must_use]
    pub fn discarded_bytes(&self) -> u64 {
        self.discarded
            .iter()
            .map(|entry| entry.allocated_bytes)
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_specified_signature_is_a_cache_tag() {
        assert!(is_cache_tag(
            b"Signature: 8a477f597d28d172789f06886806bc55\n# cargo\n"
        ));
        assert!(!is_cache_tag(
            b"signature: 8a477f597d28d172789f06886806bc55"
        ));
        assert!(!is_cache_tag(b""));
        assert!(!is_cache_tag(b"Signature: 0000"));
    }

    /// `ignored-output` is a new kind value on the unchanged JSON protocol.
    #[test]
    fn ignored_output_is_a_kebab_case_kind() {
        assert_eq!(
            serde_json::to_value(CacheKind::IgnoredOutput).unwrap(),
            serde_json::json!("ignored-output")
        );
        assert_eq!(
            serde_json::from_value::<CacheKind>(serde_json::json!("ignored-output")).unwrap(),
            CacheKind::IgnoredOutput
        );
    }
}
