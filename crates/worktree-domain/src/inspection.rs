//! Observations for inspection; none of these values authorize removal.

use crate::{RecoveryKind, Refusal, WorktreeRecord, WorktreeSnapshot};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// A separately versioned report, leaving existing lifecycle envelopes unchanged.
pub const INSPECTION_FORMAT: &str = "worktree.inspection/2";

/// Counts under one immediate child of a checkout, without following symbolic links.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageEntry {
    /// Relative child path; `.` denotes the root directory itself.
    pub path: PathBuf,
    /// Logical bytes, counting each inode once on platforms with inode identities.
    pub logical_bytes: u64,
    /// Allocated bytes where the platform exposes them. Not a reclaimability promise.
    pub allocated_bytes: Option<u64>,
}

/// Bounded, non-atomic filesystem observation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StorageObservation {
    /// Total logical bytes observed.
    pub logical_bytes: u64,
    /// Allocated bytes, with hard links deduplicated within this checkout on Unix.
    pub allocated_bytes: Option<u64>,
    /// Entries examined, including directories and symbolic links.
    pub entries: u64,
    /// False when the traversal limit, another filesystem, or an I/O failure was encountered.
    pub complete: bool,
    /// Immediate-child breakdown, largest allocation first.
    pub children: Vec<StorageEntry>,
    /// Reasons the count is partial. Bounded independently of directory size.
    pub errors: Vec<Refusal>,
}

/// Git state and storage from one inspection adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectionDetails {
    /// Current HEAD and the same dirty/lock facts used by lifecycle checks.
    pub snapshot: WorktreeSnapshot,
    /// Empty for a detached checkout.
    pub branch: String,
    /// Number of tracked status entries, counting a rename once.
    pub tracked_changes: u64,
    /// Number of untracked status entries.
    pub untracked_entries: u64,
    /// Number of ignored status entries; an ignored directory counts once.
    pub ignored_entries: u64,
    /// Up to 64 ignored entry paths for evidence review, never deletion classification.
    pub ignored_paths: Vec<PathBuf>,
    /// Size observation; an unreadable root is a refusal instead of a zero measurement.
    pub storage: StorageObservation,
}

/// What this inspection actually established about remote recoverability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum InspectedRecovery {
    /// No remote observation was requested.
    NotChecked,
    /// Freshly advertised exact refs contain the observed HEAD or carry its unique patches.
    Proven {
        /// How the refs prove recovery.
        kind: RecoveryKind,
        /// Exact configured-remote and ref names returned by the Git adapter.
        refs: Vec<String>,
        /// Commits held by no advertised ref whose patches the proving refs carry.
        equivalent_commits: Vec<String>,
    },
    /// The advertised refs did not establish recovery of the observed HEAD.
    Unproven,
    /// Remote observation failed or HEAD changed while it was in progress.
    Unavailable {
        /// The original refusal, without interpreting failure as absence.
        refusal: Refusal,
    },
}

/// Complete per-record observations and retention reasons.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInspection {
    /// Durable metadata, deliberately not refreshed by inspection.
    pub record: WorktreeRecord,
    /// Unix seconds when observation began.
    pub observed_at: i64,
    /// Age of the registry activity timestamp; not an abandonment claim.
    pub seconds_since_recorded_activity: i64,
    /// Live leases; absence is not proof of owner inactivity.
    pub live_leases: Option<u64>,
    /// Fresh Git and storage facts, absent when inspection failed.
    pub details: Option<InspectionDetails>,
    /// Whether the recorded and observed HEADs differ, when both were observed.
    pub recorded_head_differs: Option<bool>,
    /// Whether lifecycle/expiry selects this record for ordinary GC.
    pub cleanup_candidate: bool,
    /// Explicitly requested recovery observation, separate from lifecycle selection.
    pub recovery: InspectedRecovery,
    /// All observed blockers, including missing lifecycle or recovery checks.
    pub blockers: Vec<Refusal>,
    /// Current metadata cannot establish a work-item ownership or completion relation.
    pub work_item_status: String,
}

/// Scope and aggregate of a native inspection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InspectionReport {
    /// Independent format identifier.
    pub format: String,
    /// Canonical repository selected by the caller.
    pub repository_root: PathBuf,
    /// Whether the caller explicitly expanded scope to the activated workspace.
    pub workspace: bool,
    /// Reports ordered by observed allocation, then stable worktree id.
    pub inspections: Vec<WorktreeInspection>,
}
