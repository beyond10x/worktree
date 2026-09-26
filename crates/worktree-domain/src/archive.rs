//! Local archives that stand in for remote recovery proof.
//!
//! An archive holds every commit a tree adds over the advertised remote refs, the fingerprint of
//! the tree's complete on-disk content, and, when that content differs from HEAD, one binary patch
//! that recreates its tracked, untracked and ignored files over HEAD. The values here are I/O-free;
//! the Git adapter writes and verifies the files.

use crate::{Refusal, WorktreeId, WorktreeRecord};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Independent, immutable manifest format identifier.
pub const ARCHIVE_FORMAT: &str = "worktree.archive/1";
/// Manifest file name inside one archive directory.
pub const ARCHIVE_MANIFEST_FILE: &str = "manifest.json";
/// Bundle file name inside one archive directory.
pub const ARCHIVE_BUNDLE_FILE: &str = "commits.bundle";
/// Patch file name inside one archive directory.
pub const ARCHIVE_PATCH_FILE: &str = "dirty.patch";
/// The only ref an archive bundle carries; it names the archived HEAD.
pub const ARCHIVE_HEAD_REF: &str = "refs/worktree-archive/head";

/// One file an archive holds, with the digest it was verified at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveFile {
    /// File name relative to the archive directory.
    pub file: String,
    /// Lowercase hexadecimal SHA-256 of the file's bytes.
    pub sha256: String,
    /// File size in bytes.
    pub bytes: u64,
}

/// `manifest.json` of one archive, format [`ARCHIVE_FORMAT`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchiveManifest {
    /// Always [`ARCHIVE_FORMAT`].
    pub format: String,
    /// Registered worktree id.
    pub id: WorktreeId,
    /// Canonical repository root the tree belongs to.
    pub repository_root: PathBuf,
    /// Registered worktree path.
    pub path: PathBuf,
    /// HEAD at archive time.
    pub head: String,
    /// Checked-out branch at archive time; absent for a detached HEAD.
    pub branch: Option<String>,
    /// Commits HEAD added over every advertised remote ref when the archive was written.
    pub unique_commits: Vec<String>,
    /// Bundle carrying the unique commits as [`ARCHIVE_HEAD_REF`]; absent when there are none.
    pub bundle: Option<ArchiveFile>,
    /// Git tree id of the tree's complete on-disk content, read without filters or index flags.
    ///
    /// It is recorded for every archive, including one of a tree Git reports clean, because Git
    /// status can be told not to look at a file. Two observations of the same bytes, modes and
    /// paths produce the same id, so it is what a later removal compares the tree against.
    pub worktree_tree: String,
    /// Binary patch from HEAD to [`Self::worktree_tree`]; absent when they are equal.
    pub patch: Option<ArchiveFile>,
    /// Unix seconds when the archive was written.
    pub created_at: i64,
}

impl ArchiveManifest {
    /// Refuse a manifest that was not written for this exact record, or for another HEAD.
    pub fn require_matches(&self, record: &WorktreeRecord, head: &str) -> Result<(), Refusal> {
        if self.format != ARCHIVE_FORMAT {
            return Err(Refusal::new(
                "archive-invalid",
                format!("archive format {:?} is not {ARCHIVE_FORMAT}", self.format),
            ));
        }
        if self.id != record.id
            || self.path != record.path
            || self.repository_root != record.repository_root
        {
            return Err(Refusal::new(
                "archive-invalid",
                format!(
                    "archive was written for {} at {}, not {} at {}",
                    self.id,
                    self.path.display(),
                    record.id,
                    record.path.display()
                ),
            ));
        }
        if self.head != head {
            return Err(Refusal::new(
                "archive-stale",
                format!(
                    "archive holds HEAD {} but the tree is at {head}; rerun `worktree archive \
                     --replace`",
                    self.head
                ),
            ));
        }
        Ok(())
    }
}

/// The archive a removal relied on, carried inside its recovery proof.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveReference {
    /// Archive directory.
    pub path: PathBuf,
    /// SHA-256 of the manifest bytes that were verified.
    pub manifest_sha256: String,
    /// Commits held by no advertised ref at verification time, each carried by the bundle.
    pub commits: Vec<String>,
    /// Content fingerprint the linked tree was verified against; absent for unlinked residue.
    pub worktree_tree: Option<String>,
}

/// Which tree state an archive verification must establish.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArchiveStateCheck<'a> {
    /// Git already unlinked the tree and its residue is proven to be HEAD's tracked content, so
    /// only its commits need the archive.
    Unlinked,
    /// The tree is linked; its complete on-disk content must equal the archived fingerprint.
    Linked {
        /// Linked worktree path to observe.
        worktree: &'a std::path::Path,
    },
}

/// Inputs for writing one archive.
#[derive(Debug, Clone, Copy)]
pub struct ArchiveRequest<'a> {
    /// Registered record the archive is written for.
    pub record: &'a WorktreeRecord,
    /// HEAD observed immediately before archiving.
    pub head: &'a str,
    /// Final archive directory.
    pub destination: &'a std::path::Path,
    /// Move an existing archive aside instead of refusing.
    pub replace: bool,
    /// Caller-supplied time recorded in the manifest.
    pub created_at: i64,
}

/// Result of a successful `archive` operation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveEvidence {
    /// Archive directory.
    pub path: PathBuf,
    /// Verified manifest, as written.
    pub manifest: ArchiveManifest,
    /// Where a replaced archive was moved, if one was.
    pub superseded: Option<PathBuf>,
    /// State no archive can hold that will still make cleanup refuse this tree.
    pub blocker: Option<Refusal>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Lifecycle;

    fn record() -> WorktreeRecord {
        WorktreeRecord {
            id: WorktreeId::new("tree").unwrap(),
            repository_root: "/workspace/repo".into(),
            path: "/managed/repo/tree".into(),
            purpose: "p".into(),
            owner: "o".into(),
            lifecycle: Lifecycle::Finished,
            created_at: 0,
            last_seen_at: 0,
            finished_at: None,
            head: None,
        }
    }

    fn manifest() -> ArchiveManifest {
        ArchiveManifest {
            format: ARCHIVE_FORMAT.into(),
            id: WorktreeId::new("tree").unwrap(),
            repository_root: "/workspace/repo".into(),
            path: "/managed/repo/tree".into(),
            head: "a".repeat(40),
            branch: None,
            unique_commits: Vec::new(),
            bundle: None,
            worktree_tree: "c".repeat(40),
            patch: None,
            created_at: 1,
        }
    }

    #[test]
    fn a_manifest_for_another_head_is_stale() {
        let error = manifest()
            .require_matches(&record(), &"b".repeat(40))
            .unwrap_err();
        assert_eq!(error.code, "archive-stale");
    }

    #[test]
    fn a_manifest_for_another_record_or_format_is_invalid() {
        let head = "a".repeat(40);
        let mut other_path = manifest();
        other_path.path = "/managed/repo/other".into();
        let mut other_format = manifest();
        other_format.format = "worktree.archive/0".into();
        for candidate in [other_path, other_format] {
            assert_eq!(
                candidate
                    .require_matches(&record(), &head)
                    .unwrap_err()
                    .code,
                "archive-invalid"
            );
        }
        assert!(manifest().require_matches(&record(), &head).is_ok());
    }

    #[test]
    fn manifests_refuse_unknown_fields() {
        let mut value = serde_json::to_value(manifest()).unwrap();
        value["extra"] = serde_json::Value::Bool(true);
        assert!(serde_json::from_value::<ArchiveManifest>(value).is_err());
    }
}
