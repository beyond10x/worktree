//! Embeddable lifecycle service. The policy engine depends only on injected ports.

use b10x_worktree_domain::{
    CleanupAssessment, CreatePlan, CreateRequest, DiscoveredWorktree, Lifecycle, OperationEvidence,
    ReconciliationAction, ReconciliationAssessment, RecoveryProof, Refusal, RelocationIntent,
    RepositorySnapshot, WorkspacePolicy, WorktreeId, WorktreeRecord, WorktreeSnapshot,
    require_child,
};
use std::path::{Path, PathBuf};

/// Time source used by lifecycle decisions.
pub trait Clock: Send + Sync {
    /// Seconds since the Unix epoch.
    fn now(&self) -> i64;
}

/// System clock implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |value| {
                i64::try_from(value.as_secs()).unwrap_or(i64::MAX)
            })
    }
}

/// All Git observations and mutations required by the service.
pub trait GitPort: Send + Sync {
    /// Resolve a repository path to stable facts.
    fn repository_snapshot(&self, repository: &Path) -> Result<RepositorySnapshot, Refusal>;
    /// Inspect one linked worktree.
    fn worktree_snapshot(
        &self,
        repository: &Path,
        worktree: &Path,
    ) -> Result<WorktreeSnapshot, Refusal>;
    /// Create a detached linked worktree from a reviewed plan.
    fn create_detached(&self, plan: &CreatePlan) -> Result<(), Refusal>;
    /// Refresh remote references before proving recoverability.
    fn fetch(&self, repository: &Path) -> Result<(), Refusal>;
    /// Return remote branches and tags containing the commit.
    fn recovery_refs(&self, repository: &Path, head: &str) -> Result<Vec<String>, Refusal>;
    /// Remove a linked worktree without forcing Git.
    fn remove(&self, repository: &Path, worktree: &Path) -> Result<(), Refusal>;
    /// Move a linked worktree without forcing Git.
    fn move_worktree(&self, repository: &Path, from: &Path, to: &Path) -> Result<(), Refusal>;
    /// Refuse a move that the platform cannot perform atomically.
    fn validate_move_worktree(&self, from: &Path, to: &Path) -> Result<(), Refusal>;
    /// Discover all linked worktrees known to Git.
    fn list_worktrees(&self, repository: &Path) -> Result<Vec<DiscoveredWorktree>, Refusal>;
}

/// Durable ownership, lifecycle and lease registry.
pub trait RegistryPort: Send + Sync {
    /// Reserve a record before creating filesystem state.
    fn reserve(&self, record: &WorktreeRecord) -> Result<(), Refusal>;
    /// Complete provisioning.
    fn activate(&self, id: &str, head: &str, now: i64) -> Result<(), Refusal>;
    /// Retain a failed provisioning attempt as evidence.
    fn fail(&self, id: &str, now: i64) -> Result<(), Refusal>;
    /// Find a record by managed path.
    fn find_by_path(&self, path: &Path) -> Result<Option<WorktreeRecord>, Refusal>;
    /// Return all records.
    fn list(&self) -> Result<Vec<WorktreeRecord>, Refusal>;
    /// Mark activity after an observation or heartbeat.
    fn mark_seen(&self, id: &str, head: Option<&str>, now: i64) -> Result<(), Refusal>;
    /// Mark explicit completion.
    fn mark_finished(&self, id: &str, now: i64) -> Result<(), Refusal>;
    /// Record successful removal.
    fn mark_removed(&self, evidence: &OperationEvidence) -> Result<(), Refusal>;
    /// Count leases whose heartbeat remains live.
    fn live_lease_count(&self, id: &str, now: i64, timeout: i64) -> Result<u64, Refusal>;
    /// Acquire or refresh one session lease.
    fn acquire_lease(&self, id: &str, session: &str, now: i64) -> Result<(), Refusal>;
    /// Release one session lease.
    fn release_lease(&self, id: &str, session: &str) -> Result<(), Refusal>;
    /// Register an existing linked tree as manager-owned.
    fn adopt(&self, record: &WorktreeRecord) -> Result<(), Refusal>;
    /// Return a pending relocation for one worktree.
    fn relocation(&self, id: &str) -> Result<Option<RelocationIntent>, Refusal>;
    /// Record relocation intent before changing Git state.
    fn begin_relocation(&self, intent: &RelocationIntent) -> Result<(), Refusal>;
    /// Atomically update the registered path after Git moved the worktree.
    fn complete_relocation(
        &self,
        intent: &RelocationIntent,
        evidence: &OperationEvidence,
    ) -> Result<(), Refusal>;
}

/// Policy-driven worktree lifecycle service suitable for embedding in Harness.
pub struct WorktreeManager<G, R, C> {
    git: G,
    registry: R,
    clock: C,
    lease_timeout_seconds: i64,
}

impl<G, R, C> WorktreeManager<G, R, C>
where
    G: GitPort,
    R: RegistryPort,
    C: Clock,
{
    /// Construct a manager with a one-hour abandoned-session lease timeout.
    pub fn new(git: G, registry: R, clock: C) -> Self {
        Self {
            git,
            registry,
            clock,
            lease_timeout_seconds: 3_600,
        }
    }

    /// Access the registry port for status-oriented integrations.
    pub fn registry(&self) -> &R {
        &self.registry
    }

    /// Produce a deterministic create plan without changing Git or the registry.
    pub fn plan_create(
        &self,
        policy: &WorkspacePolicy,
        request: CreateRequest,
    ) -> Result<CreatePlan, Refusal> {
        policy.validate()?;
        validate_label("purpose", &request.purpose)?;
        validate_label("owner", &request.owner)?;
        let repository = self.git.repository_snapshot(&request.repository)?;
        if !repository.root.starts_with(&policy.workspace_root) {
            return Err(Refusal::new(
                "repository-outside-workspace",
                format!(
                    "repository {} is not below {}",
                    repository.root.display(),
                    policy.workspace_root.display()
                ),
            ));
        }
        let path = policy
            .worktree_root
            .join(&repository.name)
            .join(request.id.as_str());
        require_child(&policy.worktree_root, &path)?;
        Ok(CreatePlan {
            id: request.id,
            repository_root: repository.root,
            path,
            base: request.base,
            purpose: request.purpose,
            owner: request.owner,
            planned_at: self.clock.now(),
        })
    }

    /// Reserve and create one detached worktree.
    pub fn create(&self, plan: &CreatePlan) -> Result<OperationEvidence, Refusal> {
        let record = WorktreeRecord {
            id: plan.id.clone(),
            repository_root: plan.repository_root.clone(),
            path: plan.path.clone(),
            purpose: plan.purpose.clone(),
            owner: plan.owner.clone(),
            lifecycle: Lifecycle::Provisioning,
            created_at: plan.planned_at,
            last_seen_at: plan.planned_at,
            finished_at: None,
            head: None,
        };
        self.registry.reserve(&record)?;
        if let Err(refusal) = self.git.create_detached(plan) {
            let _ = self.registry.fail(plan.id.as_str(), self.clock.now());
            return Err(refusal);
        }
        let snapshot = self
            .git
            .worktree_snapshot(&plan.repository_root, &plan.path)?;
        let now = self.clock.now();
        self.registry
            .activate(plan.id.as_str(), &snapshot.head, now)?;
        Ok(OperationEvidence {
            operation: "create".into(),
            id: plan.id.clone(),
            path: plan.path.clone(),
            head: Some(snapshot.head),
            recovery: None,
            recorded_at: now,
        })
    }

    /// Mark a manager-owned worktree finished after checking it is idle and clean.
    pub fn finish(&self, path: &Path) -> Result<OperationEvidence, Refusal> {
        let record = self.owned_record(path)?;
        if record.lifecycle != Lifecycle::Active {
            return Err(Refusal::new(
                "not-active",
                "only an active worktree can be finished",
            ));
        }
        let now = self.clock.now();
        self.require_idle(&record, now)?;
        let snapshot = self
            .git
            .worktree_snapshot(&record.repository_root, &record.path)?;
        require_clean_unlocked(&snapshot)?;
        self.registry.mark_finished(record.id.as_str(), now)?;
        Ok(OperationEvidence {
            operation: "finish".into(),
            id: record.id,
            path: record.path,
            head: Some(snapshot.head),
            recovery: None,
            recorded_at: now,
        })
    }

    /// Assess cleanup candidates and optionally remove those with fresh recovery proof.
    pub fn gc(
        &self,
        policy: &WorkspacePolicy,
        apply: bool,
    ) -> Result<Vec<CleanupAssessment>, Refusal> {
        policy.validate()?;
        let now = self.clock.now();
        let mut assessments = Vec::new();
        for record in self.registry.list()? {
            if record.lifecycle != Lifecycle::Finished
                && !(record.lifecycle == Lifecycle::Active
                    && now.saturating_sub(record.last_seen_at) >= policy.expire_after_seconds)
            {
                continue;
            }
            let result = self.assess_cleanup(policy, &record, now);
            match result {
                Ok(_) if apply => {
                    // Re-observe immediately before the only destructive call.
                    let proof = self.assess_cleanup(policy, &record, self.clock.now())?;
                    self.git.remove(&record.repository_root, &record.path)?;
                    let evidence = OperationEvidence {
                        operation: "remove".into(),
                        id: record.id.clone(),
                        path: record.path.clone(),
                        head: Some(proof.head.clone()),
                        recovery: Some(proof),
                        recorded_at: self.clock.now(),
                    };
                    self.registry.mark_removed(&evidence)?;
                    assessments.push(CleanupAssessment {
                        record,
                        eligible: true,
                        refusal: None,
                        evidence: Some(evidence),
                    });
                }
                Ok(_) => assessments.push(CleanupAssessment {
                    record,
                    eligible: true,
                    refusal: None,
                    evidence: None,
                }),
                Err(refusal) => assessments.push(CleanupAssessment {
                    record,
                    eligible: false,
                    refusal: Some(refusal),
                    evidence: None,
                }),
            }
        }
        Ok(assessments)
    }

    /// Reconcile adopted legacy paths and registry records whose worktrees are already absent.
    ///
    /// An empty selection assesses every candidate in the activated workspace. Apply callers
    /// should pass the exact ids reviewed in a preceding dry-run.
    pub fn reconcile(
        &self,
        policy: &WorkspacePolicy,
        selected_ids: &[WorktreeId],
        apply: bool,
    ) -> Result<Vec<ReconciliationAssessment>, Refusal> {
        policy.validate()?;
        let records = self.registry.list()?;
        for id in selected_ids {
            if !records.iter().any(|record| record.id == *id) {
                return Err(Refusal::new(
                    "unknown-worktree-id",
                    format!("{} is not registered", id.as_str()),
                ));
            }
        }

        let mut assessments = Vec::new();
        for record in records {
            if record.lifecycle == Lifecycle::Removed
                || !record.repository_root.starts_with(&policy.workspace_root)
                || (!selected_ids.is_empty() && !selected_ids.contains(&record.id))
            {
                continue;
            }
            let Some(action) = self.reconciliation_action(policy, &record)? else {
                continue;
            };
            let result = self.assess_reconciliation(&record, &action, self.clock.now());
            match result {
                Ok(recovery) if apply => {
                    let evidence =
                        self.apply_reconciliation(&record, &action, recovery, self.clock.now())?;
                    assessments.push(ReconciliationAssessment {
                        record,
                        action,
                        eligible: true,
                        refusal: None,
                        evidence: Some(evidence),
                    });
                }
                Ok(_) => assessments.push(ReconciliationAssessment {
                    record,
                    action,
                    eligible: true,
                    refusal: None,
                    evidence: None,
                }),
                Err(refusal) => assessments.push(ReconciliationAssessment {
                    record,
                    action,
                    eligible: false,
                    refusal: Some(refusal),
                    evidence: None,
                }),
            }
        }
        Ok(assessments)
    }

    /// Acquire or heartbeat a lease for a managed worktree.
    pub fn session_start(&self, path: &Path, session: &str) -> Result<(), Refusal> {
        validate_label("session", session)?;
        let record = self.owned_record(path)?;
        let now = self.clock.now();
        self.registry
            .acquire_lease(record.id.as_str(), session, now)?;
        self.registry.mark_seen(record.id.as_str(), None, now)
    }

    /// Release a session lease.
    pub fn session_end(&self, path: &Path, session: &str) -> Result<(), Refusal> {
        let record = self.owned_record(path)?;
        self.registry.release_lease(record.id.as_str(), session)
    }

    /// Adopt an existing linked worktree. This is explicit and never automatic.
    pub fn adopt(
        &self,
        repository: &Path,
        path: &Path,
        id: b10x_worktree_domain::WorktreeId,
        purpose: String,
        owner: String,
    ) -> Result<OperationEvidence, Refusal> {
        validate_label("purpose", &purpose)?;
        validate_label("owner", &owner)?;
        let repo = self.git.repository_snapshot(repository)?;
        let snapshot = self.git.worktree_snapshot(&repo.root, path)?;
        let now = self.clock.now();
        let record = WorktreeRecord {
            id: id.clone(),
            repository_root: repo.root,
            path: snapshot.path.clone(),
            purpose,
            owner,
            lifecycle: Lifecycle::Active,
            created_at: now,
            last_seen_at: now,
            finished_at: None,
            head: Some(snapshot.head.clone()),
        };
        self.registry.adopt(&record)?;
        Ok(OperationEvidence {
            operation: "adopt".into(),
            id,
            path: snapshot.path,
            head: Some(snapshot.head),
            recovery: None,
            recorded_at: now,
        })
    }

    fn owned_record(&self, path: &Path) -> Result<WorktreeRecord, Refusal> {
        let canonical = std::fs::canonicalize(path).map_err(|error| {
            Refusal::new("worktree-not-found", format!("{}: {error}", path.display()))
        })?;
        self.registry.find_by_path(&canonical)?.ok_or_else(|| {
            Refusal::new(
                "unmanaged-worktree",
                format!("{} is not manager-owned", canonical.display()),
            )
        })
    }

    fn require_idle(&self, record: &WorktreeRecord, now: i64) -> Result<(), Refusal> {
        if self
            .registry
            .live_lease_count(record.id.as_str(), now, self.lease_timeout_seconds)?
            > 0
        {
            return Err(Refusal::new(
                "live-session",
                "worktree has a live session lease",
            ));
        }
        Ok(())
    }

    fn assess_cleanup(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
        now: i64,
    ) -> Result<RecoveryProof, Refusal> {
        require_child(&policy.worktree_root, &record.path)?;
        self.require_idle(record, now)?;
        let snapshot = self
            .git
            .worktree_snapshot(&record.repository_root, &record.path)?;
        require_clean_unlocked(&snapshot)?;
        self.git.fetch(&record.repository_root)?;
        let refs = self
            .git
            .recovery_refs(&record.repository_root, &snapshot.head)?;
        if refs.is_empty() {
            return Err(Refusal::new(
                "no-remote-recovery-proof",
                format!(
                    "commit {} is not reachable from a remote branch or tag",
                    snapshot.head
                ),
            ));
        }
        Ok(RecoveryProof {
            head: snapshot.head,
            refs,
            observed_at: now,
        })
    }

    fn reconciliation_action(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
    ) -> Result<Option<ReconciliationAction>, Refusal> {
        if let Some(intent) = self.registry.relocation(record.id.as_str())? {
            return Ok(Some(ReconciliationAction::Migrate {
                from: intent.from,
                to: intent.to,
            }));
        }
        if path_absent(&record.path) {
            return Ok(Some(ReconciliationAction::TombstoneMissing {
                path: record.path.clone(),
            }));
        }
        if record.path.starts_with(&policy.worktree_root) {
            return Ok(None);
        }
        let repository = self.git.repository_snapshot(&record.repository_root)?;
        let to = policy
            .worktree_root
            .join(repository.name)
            .join(record.id.as_str());
        require_child(&policy.worktree_root, &to)?;
        Ok(Some(ReconciliationAction::Migrate {
            from: record.path.clone(),
            to,
        }))
    }

    fn assess_reconciliation(
        &self,
        record: &WorktreeRecord,
        action: &ReconciliationAction,
        now: i64,
    ) -> Result<Option<RecoveryProof>, Refusal> {
        self.require_idle(record, now)?;
        match action {
            ReconciliationAction::Migrate { from, to } => self.assess_migration(record, from, to),
            ReconciliationAction::TombstoneMissing { path } => {
                self.assess_missing(record, path, now)
            }
        }
    }

    fn assess_migration(
        &self,
        record: &WorktreeRecord,
        from: &Path,
        to: &Path,
    ) -> Result<Option<RecoveryProof>, Refusal> {
        let discovered = self.git.list_worktrees(&record.repository_root)?;
        let source = discovered.iter().find(|item| item.path == from);
        let destination = discovered.iter().find(|item| item.path == to);
        match (source, destination) {
            (Some(source), None) => {
                if source.primary {
                    return Err(Refusal::new(
                        "primary-worktree",
                        "a primary checkout cannot be migrated",
                    ));
                }
                if source.locked {
                    return Err(Refusal::new(
                        "worktree-locked",
                        "Git marks the worktree locked",
                    ));
                }
                if !path_absent(to) {
                    return Err(Refusal::new(
                        "migration-target-exists",
                        format!("{} already exists", to.display()),
                    ));
                }
                self.git.validate_move_worktree(from, to)?;
                let snapshot = self.git.worktree_snapshot(&record.repository_root, from)?;
                if let Some(intent) = self.registry.relocation(record.id.as_str())?
                    && snapshot.head != intent.head
                {
                    return Err(Refusal::new(
                        "relocation-head-changed",
                        "worktree HEAD changed after relocation was planned",
                    ));
                }
            }
            (None, Some(destination)) => {
                let intent = self
                    .registry
                    .relocation(record.id.as_str())?
                    .ok_or_else(|| {
                        Refusal::new(
                            "unplanned-relocation",
                            "destination exists without durable relocation intent",
                        )
                    })?;
                if destination.primary || destination.locked {
                    return Err(Refusal::new(
                        "worktree-locked",
                        "relocated worktree is primary or locked",
                    ));
                }
                let snapshot = self.git.worktree_snapshot(&record.repository_root, to)?;
                if snapshot.head != intent.head {
                    return Err(Refusal::new(
                        "relocation-head-changed",
                        "relocated worktree HEAD differs from durable intent",
                    ));
                }
            }
            (Some(_), Some(_)) => {
                return Err(Refusal::new(
                    "ambiguous-relocation",
                    "Git reports both relocation source and destination",
                ));
            }
            (None, None) => {
                return Err(Refusal::new(
                    "worktree-not-discovered",
                    "Git reports neither relocation source nor destination",
                ));
            }
        }
        Ok(None)
    }

    fn assess_missing(
        &self,
        record: &WorktreeRecord,
        path: &Path,
        now: i64,
    ) -> Result<Option<RecoveryProof>, Refusal> {
        if !path_absent(path) {
            return Err(Refusal::new(
                "worktree-path-exists",
                format!("{} still exists", path.display()),
            ));
        }
        if self
            .git
            .list_worktrees(&record.repository_root)?
            .iter()
            .any(|item| item.path == path)
        {
            return Err(Refusal::new(
                "worktree-still-registered-by-git",
                "Git still reports the missing worktree path",
            ));
        }
        let Some(head) = record.head.as_deref() else {
            return if record.lifecycle == Lifecycle::Failed {
                Ok(None)
            } else {
                Err(Refusal::new(
                    "missing-recovery-head",
                    "only a failed provisioning record may be reconciled without HEAD",
                ))
            };
        };
        self.git.fetch(&record.repository_root)?;
        let refs = self.git.recovery_refs(&record.repository_root, head)?;
        if refs.is_empty() {
            return Err(Refusal::new(
                "no-remote-recovery-proof",
                format!("commit {head} is not reachable from a remote branch or tag"),
            ));
        }
        Ok(Some(RecoveryProof {
            head: head.to_owned(),
            refs,
            observed_at: now,
        }))
    }

    fn apply_reconciliation(
        &self,
        record: &WorktreeRecord,
        action: &ReconciliationAction,
        _recovery: Option<RecoveryProof>,
        now: i64,
    ) -> Result<OperationEvidence, Refusal> {
        let recovery = self.assess_reconciliation(record, action, now)?;
        match action {
            ReconciliationAction::Migrate { from, to } => {
                let pending = self.registry.relocation(record.id.as_str())?;
                let intent = if let Some(intent) = pending {
                    intent
                } else {
                    let snapshot = self.git.worktree_snapshot(&record.repository_root, from)?;
                    let intent = RelocationIntent {
                        id: record.id.clone(),
                        from: from.clone(),
                        to: to.clone(),
                        head: snapshot.head,
                        planned_at: now,
                    };
                    self.registry.begin_relocation(&intent)?;
                    intent
                };
                let discovered = self.git.list_worktrees(&record.repository_root)?;
                let source_exists = discovered.iter().any(|item| item.path == *from);
                let destination_exists = discovered.iter().any(|item| item.path == *to);
                if source_exists && !destination_exists {
                    self.git.move_worktree(&record.repository_root, from, to)?;
                }
                let snapshot = self.git.worktree_snapshot(&record.repository_root, to)?;
                if snapshot.head != intent.head {
                    return Err(Refusal::new(
                        "relocation-head-changed",
                        "relocated worktree HEAD differs from durable intent",
                    ));
                }
                let evidence = OperationEvidence {
                    operation: "migrate".into(),
                    id: record.id.clone(),
                    path: to.clone(),
                    head: Some(snapshot.head),
                    recovery: None,
                    recorded_at: self.clock.now(),
                };
                self.registry.complete_relocation(&intent, &evidence)?;
                Ok(evidence)
            }
            ReconciliationAction::TombstoneMissing { path } => {
                let evidence = OperationEvidence {
                    operation: "reconcile-missing".into(),
                    id: record.id.clone(),
                    path: path.clone(),
                    head: record.head.clone(),
                    recovery,
                    recorded_at: self.clock.now(),
                };
                self.registry.mark_removed(&evidence)?;
                Ok(evidence)
            }
        }
    }
}

fn path_absent(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_err_and(|error| error.kind() == std::io::ErrorKind::NotFound)
}

fn validate_label(name: &str, value: &str) -> Result<(), Refusal> {
    if value.trim().is_empty() || value.len() > 256 || value.contains(['\n', '\r', '\0']) {
        return Err(Refusal::new(
            format!("invalid-{name}"),
            format!("{name} must be 1-256 printable characters"),
        ));
    }
    Ok(())
}

fn require_clean_unlocked(snapshot: &WorktreeSnapshot) -> Result<(), Refusal> {
    if snapshot.locked {
        return Err(Refusal::new(
            "worktree-locked",
            "Git marks the worktree locked",
        ));
    }
    if snapshot.dirty {
        return Err(Refusal::new(
            "worktree-dirty",
            "tracked or untracked changes make cleanup unsafe",
        ));
    }
    Ok(())
}

/// Construct the default state root without reading configuration.
#[must_use]
pub fn default_worktree_root(state_home: &Path) -> PathBuf {
    state_home.join("worktree").join("trees")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;
    use tempfile::tempdir;

    struct FixedClock;

    impl Clock for FixedClock {
        fn now(&self) -> i64 {
            1_000
        }
    }

    struct FakeGit {
        repository: PathBuf,
        snapshots: Mutex<BTreeMap<PathBuf, WorktreeSnapshot>>,
        discovered: Mutex<Vec<DiscoveredWorktree>>,
        recoverable: bool,
    }

    impl GitPort for FakeGit {
        fn repository_snapshot(&self, _repository: &Path) -> Result<RepositorySnapshot, Refusal> {
            Ok(RepositorySnapshot {
                root: self.repository.clone(),
                name: "repo".into(),
                head: "main".into(),
            })
        }

        fn worktree_snapshot(
            &self,
            _repository: &Path,
            worktree: &Path,
        ) -> Result<WorktreeSnapshot, Refusal> {
            self.snapshots
                .lock()
                .unwrap()
                .get(worktree)
                .cloned()
                .ok_or_else(|| Refusal::new("worktree-not-found", worktree.display().to_string()))
        }

        fn create_detached(&self, _plan: &CreatePlan) -> Result<(), Refusal> {
            Ok(())
        }

        fn fetch(&self, _repository: &Path) -> Result<(), Refusal> {
            Ok(())
        }

        fn recovery_refs(&self, _repository: &Path, _head: &str) -> Result<Vec<String>, Refusal> {
            Ok(if self.recoverable {
                vec!["refs/remotes/origin/main".into()]
            } else {
                Vec::new()
            })
        }

        fn remove(&self, _repository: &Path, _worktree: &Path) -> Result<(), Refusal> {
            Ok(())
        }

        fn move_worktree(&self, _repository: &Path, from: &Path, to: &Path) -> Result<(), Refusal> {
            std::fs::create_dir_all(to.parent().unwrap()).unwrap();
            std::fs::rename(from, to).unwrap();
            let mut snapshots = self.snapshots.lock().unwrap();
            let mut snapshot = snapshots.remove(from).unwrap();
            snapshot.path = to.to_path_buf();
            snapshots.insert(to.to_path_buf(), snapshot);
            let mut discovered = self.discovered.lock().unwrap();
            let item = discovered
                .iter_mut()
                .find(|item| item.path == from)
                .unwrap();
            item.path = to.to_path_buf();
            Ok(())
        }

        fn validate_move_worktree(&self, _from: &Path, _to: &Path) -> Result<(), Refusal> {
            Ok(())
        }

        fn list_worktrees(&self, _repository: &Path) -> Result<Vec<DiscoveredWorktree>, Refusal> {
            Ok(self.discovered.lock().unwrap().clone())
        }
    }

    struct FakeRegistry {
        records: Mutex<Vec<WorktreeRecord>>,
        relocations: Mutex<BTreeMap<String, RelocationIntent>>,
        live_leases: u64,
    }

    impl RegistryPort for FakeRegistry {
        fn reserve(&self, record: &WorktreeRecord) -> Result<(), Refusal> {
            self.records.lock().unwrap().push(record.clone());
            Ok(())
        }

        fn activate(&self, _id: &str, _head: &str, _now: i64) -> Result<(), Refusal> {
            Ok(())
        }

        fn fail(&self, _id: &str, _now: i64) -> Result<(), Refusal> {
            Ok(())
        }

        fn find_by_path(&self, path: &Path) -> Result<Option<WorktreeRecord>, Refusal> {
            Ok(self
                .records
                .lock()
                .unwrap()
                .iter()
                .find(|record| record.path == path)
                .cloned())
        }

        fn list(&self) -> Result<Vec<WorktreeRecord>, Refusal> {
            Ok(self.records.lock().unwrap().clone())
        }

        fn mark_seen(&self, _id: &str, _head: Option<&str>, _now: i64) -> Result<(), Refusal> {
            Ok(())
        }

        fn mark_finished(&self, _id: &str, _now: i64) -> Result<(), Refusal> {
            Ok(())
        }

        fn mark_removed(&self, evidence: &OperationEvidence) -> Result<(), Refusal> {
            let mut records = self.records.lock().unwrap();
            records
                .iter_mut()
                .find(|record| record.id == evidence.id)
                .unwrap()
                .lifecycle = Lifecycle::Removed;
            Ok(())
        }

        fn live_lease_count(&self, _id: &str, _now: i64, _timeout: i64) -> Result<u64, Refusal> {
            Ok(self.live_leases)
        }

        fn acquire_lease(&self, _id: &str, _session: &str, _now: i64) -> Result<(), Refusal> {
            Ok(())
        }

        fn release_lease(&self, _id: &str, _session: &str) -> Result<(), Refusal> {
            Ok(())
        }

        fn adopt(&self, record: &WorktreeRecord) -> Result<(), Refusal> {
            self.records.lock().unwrap().push(record.clone());
            Ok(())
        }

        fn relocation(&self, id: &str) -> Result<Option<RelocationIntent>, Refusal> {
            Ok(self.relocations.lock().unwrap().get(id).cloned())
        }

        fn begin_relocation(&self, intent: &RelocationIntent) -> Result<(), Refusal> {
            self.relocations
                .lock()
                .unwrap()
                .insert(intent.id.to_string(), intent.clone());
            Ok(())
        }

        fn complete_relocation(
            &self,
            intent: &RelocationIntent,
            _evidence: &OperationEvidence,
        ) -> Result<(), Refusal> {
            self.records
                .lock()
                .unwrap()
                .iter_mut()
                .find(|record| record.id == intent.id)
                .unwrap()
                .path = intent.to.clone();
            self.relocations.lock().unwrap().remove(intent.id.as_str());
            Ok(())
        }
    }

    fn record(path: PathBuf, lifecycle: Lifecycle) -> WorktreeRecord {
        WorktreeRecord {
            id: WorktreeId::new("legacy-one").unwrap(),
            repository_root: path.parent().unwrap().join("repo"),
            path,
            purpose: "test".into(),
            owner: "test".into(),
            lifecycle,
            created_at: 1,
            last_seen_at: 1,
            finished_at: None,
            head: Some("abc".into()),
        }
    }

    #[test]
    fn migrates_dirty_legacy_tree_into_managed_root() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let legacy = temporary.path().join("legacy");
        let managed_root = temporary.path().join("managed");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir(&legacy).unwrap();
        let mut registered = record(legacy.clone(), Lifecycle::Active);
        registered.repository_root.clone_from(&repository);
        let snapshot = WorktreeSnapshot {
            path: legacy.clone(),
            head: "abc".into(),
            dirty: true,
            locked: false,
        };
        let manager = WorktreeManager::new(
            FakeGit {
                repository: repository.clone(),
                snapshots: Mutex::new(BTreeMap::from([(legacy.clone(), snapshot)])),
                discovered: Mutex::new(vec![DiscoveredWorktree {
                    path: legacy.clone(),
                    head: Some("abc".into()),
                    locked: false,
                    primary: false,
                }]),
                recoverable: true,
            },
            FakeRegistry {
                records: Mutex::new(vec![registered.clone()]),
                relocations: Mutex::new(BTreeMap::new()),
                live_leases: 0,
            },
            FixedClock,
        );
        let policy = WorkspacePolicy {
            version: 1,
            name: "test".into(),
            workspace_root: workspace,
            worktree_root: managed_root.clone(),
            expire_after_seconds: 60,
            protect_workspace_root: true,
        };

        let assessments = manager
            .reconcile(&policy, std::slice::from_ref(&registered.id), true)
            .unwrap();
        assert!(assessments[0].eligible);
        assert!(assessments[0].evidence.is_some());
        assert_eq!(
            manager.registry().list().unwrap()[0].path,
            managed_root.join("repo/legacy-one")
        );
    }

    #[test]
    fn tombstones_only_remotely_recoverable_missing_records() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        std::fs::create_dir_all(&repository).unwrap();
        let missing = temporary.path().join("missing");
        let mut registered = record(missing, Lifecycle::Finished);
        registered.repository_root.clone_from(&repository);
        let policy = WorkspacePolicy {
            version: 1,
            name: "test".into(),
            workspace_root: workspace,
            worktree_root: temporary.path().join("managed"),
            expire_after_seconds: 60,
            protect_workspace_root: true,
        };
        let manager = WorktreeManager::new(
            FakeGit {
                repository,
                snapshots: Mutex::new(BTreeMap::new()),
                discovered: Mutex::new(Vec::new()),
                recoverable: true,
            },
            FakeRegistry {
                records: Mutex::new(vec![registered.clone()]),
                relocations: Mutex::new(BTreeMap::new()),
                live_leases: 0,
            },
            FixedClock,
        );

        let assessments = manager
            .reconcile(&policy, std::slice::from_ref(&registered.id), true)
            .unwrap();
        assert_eq!(
            assessments[0].evidence.as_ref().unwrap().operation,
            "reconcile-missing"
        );
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Removed
        );
    }
}
