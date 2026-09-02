//! I/O-free values used to decide and record Git worktree lifecycle operations.

use serde::{Deserialize, Serialize};
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};

/// Configuration and workspace-policy schema version.
pub const SURFACE_VERSION: u32 = 1;

/// Reconciliation report version.
///
/// Reconciliation version 2 adds the explicit `retire-external` action without changing the
/// stable version-1 lifecycle, configuration, or hook envelopes.
pub const RECONCILIATION_VERSION: u32 = 2;

/// Version of non-hook CLI JSON envelopes.
pub const CLI_PROTOCOL_VERSION: u32 = 2;

/// Immutable hook protocol version.
pub const HOOK_PROTOCOL_VERSION: u32 = 1;

/// A stable worktree identifier safe to use as one path component.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorktreeId(String);

impl WorktreeId {
    /// Validate and construct an identifier.
    pub fn new(value: impl Into<String>) -> Result<Self, Refusal> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || value.starts_with('.')
            || value
                .bytes()
                .any(|byte| !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
        {
            return Err(Refusal::new(
                "invalid-worktree-id",
                "worktree id must be 1-128 lowercase letters, digits or hyphens and cannot start with a dot",
            ));
        }
        Ok(Self(value))
    }

    /// Borrow the validated identifier.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Display for WorktreeId {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// A validated, displayable Git revision supplied as one argv value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct GitRevision(String);

impl GitRevision {
    /// Refuse values that Git could interpret as an option or that cannot be one argv value.
    pub fn new(value: impl Into<String>) -> Result<Self, Refusal> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 512
            || value.starts_with('-')
            || value
                .bytes()
                .any(|byte| byte == 0 || byte.is_ascii_whitespace())
        {
            return Err(Refusal::new(
                "invalid-git-revision",
                "Git revision must be non-empty, option-safe and contain no whitespace",
            ));
        }
        Ok(Self(value))
    }

    /// Borrow the revision.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// One activated workspace policy.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspacePolicy {
    /// Policy schema version.
    pub version: u32,
    /// Stable profile name.
    pub name: String,
    /// Collection containing primary repository checkouts.
    pub workspace_root: PathBuf,
    /// Root below which this manager may create or remove linked worktrees.
    pub worktree_root: PathBuf,
    /// Seconds without a heartbeat before an unfinished tree is considered abandoned.
    pub expire_after_seconds: i64,
    /// Whether the primary workspace root is expected to be non-writable.
    pub protect_workspace_root: bool,
}

impl WorkspacePolicy {
    /// Validate a resolved policy.
    pub fn validate(&self) -> Result<(), Refusal> {
        if self.version != SURFACE_VERSION {
            return Err(Refusal::new(
                "unsupported-policy-version",
                format!("policy version {} is not supported", self.version),
            ));
        }
        if self.name.is_empty()
            || self.name.len() > 128
            || self.name.starts_with('.')
            || self
                .name
                .bytes()
                .any(|byte| !(byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-'))
        {
            return Err(Refusal::new(
                "invalid-policy-name",
                "policy name must be one lowercase path-safe component",
            ));
        }
        if !self.workspace_root.is_absolute() || !self.worktree_root.is_absolute() {
            return Err(Refusal::new(
                "relative-policy-path",
                "workspace and worktree roots must be absolute",
            ));
        }
        if has_ambiguous_components(&self.workspace_root)
            || has_ambiguous_components(&self.worktree_root)
        {
            return Err(Refusal::new(
                "ambiguous-policy-path",
                "workspace and worktree roots cannot contain dot or parent components",
            ));
        }
        if self.workspace_root.starts_with(&self.worktree_root)
            || self.worktree_root.starts_with(&self.workspace_root)
        {
            return Err(Refusal::new(
                "workspace-roots-overlap",
                "managed worktrees and primary checkouts must use disjoint roots",
            ));
        }
        if self.expire_after_seconds <= 0 {
            return Err(Refusal::new(
                "invalid-expiry",
                "expiry must be greater than zero seconds",
            ));
        }
        Ok(())
    }
}

fn has_ambiguous_components(path: &Path) -> bool {
    path.components().any(|component| {
        matches!(
            component,
            std::path::Component::CurDir | std::path::Component::ParentDir
        )
    })
}

/// Requested creation of one detached linked worktree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateRequest {
    /// Caller-selected stable identifier.
    pub id: WorktreeId,
    /// Any path within the source repository.
    pub repository: PathBuf,
    /// Human-readable, path-safe purpose.
    pub purpose: String,
    /// Starting revision.
    pub base: GitRevision,
    /// Owner class recorded for cleanup delegation.
    pub owner: String,
}

/// A repository observation made by the Git adapter.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepositorySnapshot {
    /// Canonical primary repository root.
    pub root: PathBuf,
    /// Directory name used below the managed worktree root.
    pub name: String,
    /// Current commit id.
    pub head: String,
}

/// Worktree facts needed to decide whether cleanup is admissible.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeSnapshot {
    /// Canonical worktree path.
    pub path: PathBuf,
    /// Current commit id.
    pub head: String,
    /// Whether tracked or untracked state differs from HEAD.
    pub dirty: bool,
    /// Whether Git marks this worktree locked.
    pub locked: bool,
}

/// A linked worktree discovered directly from Git, whether registered or not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiscoveredWorktree {
    /// Worktree path reported by Git.
    pub path: PathBuf,
    /// Current commit id when Git reports one.
    pub head: Option<String>,
    /// Whether Git marks the entry locked.
    pub locked: bool,
    /// Whether this is the repository's primary checkout.
    pub primary: bool,
}

/// A mutation plan created from observations and revalidated before execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreatePlan {
    /// Worktree identity.
    pub id: WorktreeId,
    /// Canonical repository root.
    pub repository_root: PathBuf,
    /// Exact target path.
    pub path: PathBuf,
    /// Requested starting revision.
    pub base: GitRevision,
    /// Purpose retained in local state.
    pub purpose: String,
    /// Owner retained in local state.
    pub owner: String,
    /// Time at which the plan was produced.
    pub planned_at: i64,
}

/// A registered worktree lifecycle state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Lifecycle {
    /// Reserved in the registry but not yet created by Git.
    Provisioning,
    /// Available for work.
    Active,
    /// Atomically claimed for a recoverable relocation; new sessions are blocked.
    Relocating,
    /// Explicitly finished and awaiting safe cleanup.
    Finished,
    /// Removed after all safety proofs passed.
    Removed,
    /// Creation failed and the record remains as evidence.
    Failed,
}

impl Lifecycle {
    /// Stable state spelling used by the SQLite adapter.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Provisioning => "provisioning",
            Self::Active => "active",
            Self::Relocating => "relocating",
            Self::Finished => "finished",
            Self::Removed => "removed",
            Self::Failed => "failed",
        }
    }

    /// Parse stored state without silently accepting new values.
    pub fn parse(value: &str) -> Result<Self, Refusal> {
        match value {
            "provisioning" => Ok(Self::Provisioning),
            "active" => Ok(Self::Active),
            "relocating" => Ok(Self::Relocating),
            "finished" => Ok(Self::Finished),
            "removed" => Ok(Self::Removed),
            "failed" => Ok(Self::Failed),
            _ => Err(Refusal::new(
                "unknown-lifecycle",
                format!("stored lifecycle {value:?} is not supported"),
            )),
        }
    }
}

/// Durable local record for one manager-owned worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeRecord {
    /// Stable id.
    pub id: WorktreeId,
    /// Source repository.
    pub repository_root: PathBuf,
    /// Managed worktree path.
    pub path: PathBuf,
    /// Human purpose.
    pub purpose: String,
    /// Owner class.
    pub owner: String,
    /// Lifecycle.
    pub lifecycle: Lifecycle,
    /// Creation time.
    pub created_at: i64,
    /// Most recent observed activity.
    pub last_seen_at: i64,
    /// Explicit finish time.
    pub finished_at: Option<i64>,
    /// Last observed HEAD.
    pub head: Option<String>,
}

/// Exact remote evidence that makes a clean commit recoverable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryProof {
    /// Commit proven reachable.
    pub head: String,
    /// Fresh, advertised remote refs containing it.
    pub refs: Vec<String>,
    /// Observation time.
    pub observed_at: i64,
}

/// Evidence returned after a mutation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OperationEvidence {
    /// Operation name.
    pub operation: String,
    /// Worktree id.
    pub id: WorktreeId,
    /// Exact path affected.
    pub path: PathBuf,
    /// Commit involved where applicable.
    pub head: Option<String>,
    /// Remote proof where cleanup occurred.
    pub recovery: Option<RecoveryProof>,
    /// Event time.
    pub recorded_at: i64,
}

/// One cleanup assessment, returned for dry-runs and applied garbage collection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CleanupAssessment {
    /// Registered worktree.
    pub record: WorktreeRecord,
    /// Whether all cleanup preconditions currently pass.
    pub eligible: bool,
    /// Refusal explaining why the tree is retained.
    pub refusal: Option<Refusal>,
    /// Removal evidence when an apply run removed it.
    pub evidence: Option<OperationEvidence>,
}

/// One manager-owned registry inconsistency that can be reconciled safely.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "kebab-case")]
pub enum ReconciliationAction {
    /// Complete a provisioning transition after Git created the linked tree.
    RecoverProvisioning {
        /// Exact registered path Git already reports.
        path: PathBuf,
    },
    /// Move an adopted linked worktree into the configured managed root.
    Migrate {
        /// Current canonical path.
        from: PathBuf,
        /// Policy-derived destination.
        to: PathBuf,
    },
    /// Remove a finished, clean adopted worktree outside the managed root.
    ///
    /// This is deliberately available only through exact-id reconciliation, never ordinary GC.
    RetireExternal {
        /// Exact registered path to remove without force.
        path: PathBuf,
    },
    /// Record that a worktree which is already absent is no longer active.
    TombstoneMissing {
        /// Missing registered path.
        path: PathBuf,
    },
}

/// One reconciliation assessment, returned by dry-runs and apply operations.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReconciliationAssessment {
    /// Registered worktree.
    pub record: WorktreeRecord,
    /// Proposed or applied action.
    pub action: ReconciliationAction,
    /// Whether all reconciliation preconditions currently pass.
    pub eligible: bool,
    /// Refusal explaining why the record is retained.
    pub refusal: Option<Refusal>,
    /// Evidence when an apply run changed Git or registry state.
    pub evidence: Option<OperationEvidence>,
}

/// Durable intent used to recover an interrupted legacy relocation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelocationIntent {
    /// Worktree identity.
    pub id: WorktreeId,
    /// Registered source path.
    pub from: PathBuf,
    /// Policy-derived destination path.
    pub to: PathBuf,
    /// HEAD observed before the move.
    pub head: String,
    /// Time at which the intent was recorded.
    pub planned_at: i64,
}

/// Durable proof recorded before removing a worktree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalIntent {
    /// Worktree identity.
    pub id: WorktreeId,
    /// Exact registered path selected for removal.
    pub path: PathBuf,
    /// HEAD revalidated immediately before intent persistence.
    pub head: String,
    /// Fresh remote recovery proof captured before mutation.
    pub recovery: RecoveryProof,
    /// Evidence operation to record after successful removal.
    pub operation: String,
    /// Time at which deletion intent became durable.
    pub planned_at: i64,
}

/// Typed refusal: absence of permission or evidence, not an unstructured error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{code}: {message}")]
#[non_exhaustive]
pub struct Refusal {
    /// Stable machine-readable reason.
    pub code: String,
    /// Human-readable explanation.
    pub message: String,
}

impl Refusal {
    /// Construct a refusal.
    #[must_use]
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

/// Require canonical containment without accepting the root itself.
pub fn require_child(root: &Path, candidate: &Path) -> Result<(), Refusal> {
    if candidate == root || !candidate.starts_with(root) {
        return Err(Refusal::new(
            "path-outside-worktree-root",
            format!(
                "candidate {} is not a child of managed root {}",
                candidate.display(),
                root.display()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_one_safe_path_component() {
        assert!(WorktreeId::new("20260901-doc-sweep-a1b2c3d4").is_ok());
        for invalid in [
            "",
            ".hidden",
            "UPPER",
            "two parts",
            "../escape",
            "slash/name",
        ] {
            assert!(WorktreeId::new(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn worktree_root_must_be_outside_the_primary_collection() {
        let policy = WorkspacePolicy {
            version: 1,
            name: "bad".into(),
            workspace_root: "/workspace".into(),
            worktree_root: "/workspace/.worktrees".into(),
            expire_after_seconds: 1,
            protect_workspace_root: false,
        };
        assert_eq!(
            policy.validate().unwrap_err().code,
            "workspace-roots-overlap"
        );
    }

    #[test]
    fn policy_names_cannot_escape_the_state_root() {
        for invalid in ["/", "../escape", "Two", ".hidden"] {
            let policy = WorkspacePolicy {
                version: 1,
                name: invalid.into(),
                workspace_root: "/workspace".into(),
                worktree_root: "/managed".into(),
                expire_after_seconds: 1,
                protect_workspace_root: true,
            };
            assert_eq!(policy.validate().unwrap_err().code, "invalid-policy-name");
        }
    }

    #[test]
    fn managed_root_cannot_contain_the_workspace() {
        let policy = WorkspacePolicy {
            version: 1,
            name: "bad".into(),
            workspace_root: "/managed/workspace".into(),
            worktree_root: "/managed".into(),
            expire_after_seconds: 1,
            protect_workspace_root: true,
        };
        assert_eq!(
            policy.validate().unwrap_err().code,
            "workspace-roots-overlap"
        );
    }
}
