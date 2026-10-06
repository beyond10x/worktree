//! Build caches a tree may discard: ignored content its own tool recreates from tracked sources.
//!
//! A directory's name never makes it cache. Each rule below names the structure that does, and an
//! ignored entry no rule recognises is retained, because agents keep records in ignored
//! directories too: on 2026-10-06 a prune of every ignored `target/` by name deleted review
//! records that committed evidence cites under `target/`.

use crate::WorktreeId;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The file that marks a cache directory, per the Cache Directory Tagging Specification.
pub const CACHEDIR_TAG_FILE: &str = "CACHEDIR.TAG";

/// The bytes a valid `CACHEDIR.TAG` starts with.
pub const CACHEDIR_TAG_SIGNATURE: &[u8] = b"Signature: 8a477f597d28d172789f06886806bc55";

/// The directory Cargo writes into every profile directory and nowhere else.
pub const CARGO_PROFILE_MARKER: &str = ".fingerprint";

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

/// Whether `contents` is a valid `CACHEDIR.TAG`.
#[must_use]
pub fn is_cache_tag(contents: &[u8]) -> bool {
    contents.starts_with(CACHEDIR_TAG_SIGNATURE)
}

/// Why an ignored directory was recognised as cache.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CacheKind {
    /// A directory holding `.fingerprint/` inside a tagged Cargo target.
    CargoProfile,
    /// A tagged Cargo target holding nothing but profiles and Cargo's own metadata.
    CargoTarget,
    /// `node_modules` at or below a directory holding a tracked lockfile.
    NodeModules,
    /// A directory holding `pyvenv.cfg` beside, or at the root of, a tracked Python manifest.
    PythonVirtualenv,
    /// A tagged `.pytest_cache`, `.mypy_cache` or `.ruff_cache`.
    ToolCache,
}

/// One ignored directory recognised as cache.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscardedCache {
    /// Path relative to the tree root.
    pub path: PathBuf,
    /// The rule that recognised it.
    pub kind: CacheKind,
    /// Allocated bytes observed before deletion, without following symbolic links.
    pub allocated_bytes: u64,
}

/// What a Git adapter recognised among one tree's ignored entries.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CacheClassification {
    /// Recognised cache; deleted when the discard was applied.
    pub discarded: Vec<DiscardedCache>,
    /// Every other ignored entry, relative to the tree root. Never deleted.
    pub retained_ignored: Vec<PathBuf>,
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
    /// Recognised cache.
    pub discarded: Vec<DiscardedCache>,
    /// Every other ignored entry; `worktree archive` keeps these.
    pub retained_ignored: Vec<PathBuf>,
    /// Whether running processes could be observed.
    pub processes_observed: bool,
}

impl CacheDiscard {
    /// Total allocated bytes of the recognised cache.
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
}
