//! Embeddable lifecycle service. The policy engine depends only on injected ports.

use b10x_worktree_domain::{
    CleanupAssessment, CreatePlan, CreateRequest, DiscoveredWorktree, Lifecycle, OperationEvidence,
    ReconciliationAction, ReconciliationAssessment, RecoveryProof, Refusal, RelocationIntent,
    RemovalIntent, RepositorySnapshot, WorkspacePolicy, WorktreeId, WorktreeRecord,
    WorktreeSnapshot, require_child,
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
    /// Resolve a caller-supplied revision to one immutable commit id.
    fn resolve_revision(&self, repository: &Path, revision: &str) -> Result<String, Refusal>;
    /// Inspect one linked worktree.
    fn worktree_snapshot(
        &self,
        repository: &Path,
        worktree: &Path,
    ) -> Result<WorktreeSnapshot, Refusal>;
    /// Create a detached linked worktree from a reviewed plan.
    fn create_detached(&self, plan: &CreatePlan) -> Result<(), Refusal>;
    /// Refresh advertisements and return exact remote refs containing the commit.
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
    /// Atomically mark explicit completion, persist final HEAD, and refuse live leases.
    fn mark_finished(
        &self,
        id: &str,
        head: &str,
        now: i64,
        lease_timeout: i64,
    ) -> Result<(), Refusal>;
    /// Atomically claim an expired active tree for cleanup and block new sessions.
    fn claim_expired(
        &self,
        id: &str,
        head: &str,
        now: i64,
        expire_before: i64,
        lease_timeout: i64,
    ) -> Result<(), Refusal>;
    /// Atomically claim an active legacy tree for relocation and block new sessions.
    fn claim_relocation(&self, id: &str, now: i64, lease_timeout: i64) -> Result<(), Refusal>;
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
    /// Return a pending, proof-bearing removal intent.
    fn removal(&self, id: &str) -> Result<Option<RemovalIntent>, Refusal>;
    /// Persist proof before deleting filesystem state.
    ///
    /// Repeating this call with the same id/path/head/operation refreshes the proof atomically.
    /// A matching stale relocation remains durable until removal completion commits.
    fn begin_removal(&self, intent: &RemovalIntent) -> Result<(), Refusal>;
    /// Atomically mark removal complete and clear its durable removal and relocation intents.
    fn complete_removal(
        &self,
        intent: &RemovalIntent,
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
        require_canonical_policy(policy)?;
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
        require_canonical_child(&policy.worktree_root, &path)?;
        let base = self
            .git
            .resolve_revision(&repository.root, request.base.as_str())?;
        Ok(CreatePlan {
            id: request.id,
            repository_root: repository.root,
            path,
            base: b10x_worktree_domain::GitRevision::new(base)?,
            purpose: request.purpose,
            owner: request.owner,
            planned_at: self.clock.now(),
        })
    }

    /// Reserve and create one detached worktree.
    pub fn create(
        &self,
        policy: &WorkspacePolicy,
        plan: &CreatePlan,
    ) -> Result<OperationEvidence, Refusal> {
        require_canonical_policy(policy)?;
        let repository = self.git.repository_snapshot(&plan.repository_root)?;
        if repository.root != plan.repository_root
            || !repository.root.starts_with(&policy.workspace_root)
        {
            return Err(Refusal::new(
                "create-plan-repository-changed",
                "create plan repository no longer matches the selected workspace",
            ));
        }
        let expected_path = policy
            .worktree_root
            .join(&repository.name)
            .join(plan.id.as_str());
        require_canonical_child(&policy.worktree_root, &expected_path)?;
        if plan.path != expected_path {
            return Err(Refusal::new(
                "create-plan-path-changed",
                "create plan path does not match current policy",
            ));
        }
        let resolved_base = self
            .git
            .resolve_revision(&repository.root, plan.base.as_str())?;
        if resolved_base != plan.base.as_str() {
            return Err(Refusal::new(
                "create-plan-base-not-immutable",
                "create plan base must be an exact commit id",
            ));
        }
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
        let snapshot = self.exact_worktree_snapshot(&plan.repository_root, &plan.path)?;
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
        let snapshot = self.exact_worktree_snapshot(&record.repository_root, &record.path)?;
        require_clean_unlocked(&snapshot)?;
        self.registry.mark_finished(
            record.id.as_str(),
            &snapshot.head,
            now,
            self.lease_timeout_seconds,
        )?;
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
        selected_ids: &[WorktreeId],
        apply: bool,
    ) -> Result<Vec<CleanupAssessment>, Refusal> {
        require_canonical_policy(policy)?;
        if apply && selected_ids.is_empty() {
            return Err(Refusal::new(
                "explicit-cleanup-selection-required",
                "cleanup apply requires at least one reviewed worktree id",
            ));
        }
        let now = self.clock.now();
        let records = self.registry.list()?;
        validate_selected_records(policy, &records, selected_ids)?;
        let mut planned = Vec::new();
        for record in records {
            if !record.repository_root.starts_with(&policy.workspace_root)
                || (!selected_ids.is_empty() && !selected_ids.contains(&record.id))
            {
                continue;
            }
            if record.lifecycle != Lifecycle::Finished
                && !(record.lifecycle == Lifecycle::Active
                    && now.saturating_sub(record.last_seen_at) >= policy.expire_after_seconds)
            {
                continue;
            }
            let assessment = self.assess_cleanup(policy, &record, now);
            planned.push((record, assessment));
        }
        if apply {
            for id in selected_ids {
                if !planned.iter().any(|(record, _)| record.id == *id) {
                    return Err(Refusal::new(
                        "selected-worktree-not-cleanup-candidate",
                        format!("{} is no longer a cleanup candidate", id.as_str()),
                    ));
                }
            }
        }

        let mut assessments = Vec::with_capacity(planned.len());
        for (record, assessment) in planned {
            match assessment {
                Ok(_) if !apply => assessments.push(CleanupAssessment {
                    record,
                    eligible: true,
                    refusal: None,
                    evidence: None,
                }),
                Ok(_) => {
                    match self.claim_cleanup(policy, record.clone()) {
                        Err(refusal) => assessments.push(CleanupAssessment {
                            record,
                            eligible: false,
                            refusal: Some(refusal),
                            evidence: None,
                        }),
                        Ok(claimed) => {
                            // Re-observe immediately before the only destructive call.
                            let applied = self
                                .assess_cleanup(policy, &claimed, self.clock.now())
                                .and_then(|proof| {
                                    self.apply_removal(
                                        &claimed,
                                        &claimed.path,
                                        "remove",
                                        proof,
                                        None,
                                    )
                                });
                            match applied {
                                Ok(evidence) => assessments.push(CleanupAssessment {
                                    record: claimed,
                                    eligible: true,
                                    refusal: None,
                                    evidence: Some(evidence),
                                }),
                                Err(refusal) => assessments.push(CleanupAssessment {
                                    record: claimed,
                                    eligible: false,
                                    refusal: Some(refusal),
                                    evidence: None,
                                }),
                            }
                        }
                    }
                }
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
        allow_external_retirement: bool,
    ) -> Result<Vec<ReconciliationAssessment>, Refusal> {
        require_canonical_policy(policy)?;
        if apply && selected_ids.is_empty() {
            return Err(Refusal::new(
                "explicit-reconciliation-selection-required",
                "reconciliation apply requires at least one reviewed worktree id",
            ));
        }
        let records = self.registry.list()?;
        validate_selected_records(policy, &records, selected_ids)?;
        let mut planned = Vec::new();
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
            let assessment = self.assess_reconciliation(policy, &record, &action, self.clock.now());
            planned.push((record, action, assessment));
        }
        if apply {
            for id in selected_ids {
                if !planned.iter().any(|(record, _, _)| record.id == *id) {
                    return Err(Refusal::new(
                        "selected-worktree-not-reconciliation-candidate",
                        format!("{} is no longer a reconciliation candidate", id.as_str()),
                    ));
                }
            }
        }
        if apply
            && !allow_external_retirement
            && planned
                .iter()
                .any(|(_, action, _)| matches!(action, ReconciliationAction::RetireExternal { .. }))
        {
            return Err(Refusal::new(
                "external-retirement-confirmation-required",
                "retiring a finished tree outside the managed root requires explicit confirmation",
            ));
        }

        let mut assessments = Vec::with_capacity(planned.len());
        for (record, action, assessment) in planned {
            match assessment {
                Ok(_) if !apply => assessments.push(ReconciliationAssessment {
                    record,
                    action,
                    eligible: true,
                    refusal: None,
                    evidence: None,
                }),
                Ok(recovery) => {
                    match self.apply_reconciliation(
                        policy,
                        &record,
                        &action,
                        recovery,
                        self.clock.now(),
                    ) {
                        Ok(evidence) => assessments.push(ReconciliationAssessment {
                            record,
                            action,
                            eligible: true,
                            refusal: None,
                            evidence: Some(evidence),
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
        if record.lifecycle != Lifecycle::Active {
            return Err(Refusal::new(
                "worktree-not-active",
                "only an active worktree may acquire or refresh a session lease",
            ));
        }
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
        policy: &WorkspacePolicy,
        repository: &Path,
        path: &Path,
        id: b10x_worktree_domain::WorktreeId,
        purpose: String,
        owner: String,
    ) -> Result<OperationEvidence, Refusal> {
        require_canonical_policy(policy)?;
        validate_label("purpose", &purpose)?;
        validate_label("owner", &owner)?;
        let repo = self.git.repository_snapshot(repository)?;
        if !repo.root.starts_with(&policy.workspace_root) {
            return Err(Refusal::new(
                "repository-outside-workspace",
                format!(
                    "repository {} is not below {}",
                    repo.root.display(),
                    policy.workspace_root.display()
                ),
            ));
        }
        let snapshot = self.git.worktree_snapshot(&repo.root, path)?;
        let linked = self
            .git
            .list_worktrees(&repo.root)?
            .into_iter()
            .find(|item| item.path == snapshot.path)
            .ok_or_else(|| {
                Refusal::new(
                    "worktree-not-discovered",
                    "Git does not report the requested linked worktree",
                )
            })?;
        if linked.primary {
            return Err(Refusal::new(
                "primary-worktree",
                "the primary checkout cannot be adopted",
            ));
        }
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

    fn exact_worktree_snapshot(
        &self,
        repository: &Path,
        path: &Path,
    ) -> Result<WorktreeSnapshot, Refusal> {
        let snapshot = self.git.worktree_snapshot(repository, path)?;
        require_exact_snapshot_path(&snapshot, path)?;
        Ok(snapshot)
    }

    fn assess_cleanup(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
        now: i64,
    ) -> Result<RecoveryProof, Refusal> {
        require_canonical_child(&policy.worktree_root, &record.path)?;
        self.require_idle(record, now)?;
        let snapshot = self.exact_worktree_snapshot(&record.repository_root, &record.path)?;
        require_clean_unlocked(&snapshot)?;
        self.recovery_for_record(record, &snapshot.head, now)
    }

    fn claim_cleanup(
        &self,
        policy: &WorkspacePolicy,
        record: WorktreeRecord,
    ) -> Result<WorktreeRecord, Refusal> {
        if record.lifecycle != Lifecycle::Active {
            return Ok(record);
        }
        let snapshot = self.exact_worktree_snapshot(&record.repository_root, &record.path)?;
        require_clean_unlocked(&snapshot)?;
        let now = self.clock.now();
        self.registry.claim_expired(
            record.id.as_str(),
            &snapshot.head,
            now,
            now.saturating_sub(policy.expire_after_seconds),
            self.lease_timeout_seconds,
        )?;
        self.registry.find_by_path(&record.path)?.ok_or_else(|| {
            Refusal::new(
                "cleanup-claim-missing",
                "claimed worktree disappeared from the lifecycle registry",
            )
        })
    }

    fn reconciliation_action(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
    ) -> Result<Option<ReconciliationAction>, Refusal> {
        if let Some(intent) = self.registry.relocation(record.id.as_str())? {
            if intent.from != record.path {
                return Err(Refusal::new(
                    "relocation-source-mismatch",
                    "pending relocation source differs from the registered worktree path",
                ));
            }
            require_canonical_child(&policy.worktree_root, &intent.to)?;
            if record.lifecycle == Lifecycle::Finished
                && !record.path.starts_with(&policy.worktree_root)
            {
                let discovered = self.git.list_worktrees(&record.repository_root)?;
                let source_exists = discovered.iter().any(|item| item.path == intent.from);
                let destination_exists = discovered.iter().any(|item| item.path == intent.to);
                if source_exists && !destination_exists {
                    return Ok(Some(ReconciliationAction::RetireExternal {
                        path: record.path.clone(),
                    }));
                }
                if !source_exists && self.registry.removal(record.id.as_str())?.is_some() {
                    return Ok(Some(ReconciliationAction::TombstoneMissing {
                        path: record.path.clone(),
                    }));
                }
            }
            return Ok(Some(ReconciliationAction::Migrate {
                from: intent.from,
                to: intent.to,
            }));
        }
        if path_absent(&record.path)? {
            return Ok(Some(ReconciliationAction::TombstoneMissing {
                path: record.path.clone(),
            }));
        }
        if record.path.starts_with(&policy.worktree_root) {
            if matches!(
                record.lifecycle,
                Lifecycle::Provisioning | Lifecycle::Failed
            ) {
                return Ok(Some(ReconciliationAction::RecoverProvisioning {
                    path: record.path.clone(),
                }));
            }
            return Ok(None);
        }
        if record.lifecycle == Lifecycle::Finished {
            return Ok(Some(ReconciliationAction::RetireExternal {
                path: record.path.clone(),
            }));
        }
        let repository = self.git.repository_snapshot(&record.repository_root)?;
        let to = policy
            .worktree_root
            .join(repository.name)
            .join(record.id.as_str());
        require_canonical_child(&policy.worktree_root, &to)?;
        Ok(Some(ReconciliationAction::Migrate {
            from: record.path.clone(),
            to,
        }))
    }

    fn assess_reconciliation(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
        action: &ReconciliationAction,
        now: i64,
    ) -> Result<Option<RecoveryProof>, Refusal> {
        self.require_idle(record, now)?;
        match action {
            ReconciliationAction::RecoverProvisioning { path } => {
                self.assess_provisioning_recovery(record, path)
            }
            ReconciliationAction::Migrate { from, to } => {
                self.assess_migration(policy, record, from, to)
            }
            ReconciliationAction::RetireExternal { path } => {
                self.assess_external_retirement(policy, record, path, now)
            }
            ReconciliationAction::TombstoneMissing { path } => {
                self.assess_missing(policy, record, path, now)
            }
        }
    }

    fn assess_provisioning_recovery(
        &self,
        record: &WorktreeRecord,
        path: &Path,
    ) -> Result<Option<RecoveryProof>, Refusal> {
        if !matches!(
            record.lifecycle,
            Lifecycle::Provisioning | Lifecycle::Failed
        ) || path != record.path
        {
            return Err(Refusal::new(
                "invalid-provisioning-recovery",
                "provisioning recovery requires the exact failed or provisioning record",
            ));
        }
        let linked = self
            .git
            .list_worktrees(&record.repository_root)?
            .into_iter()
            .find(|item| item.path == *path)
            .ok_or_else(|| {
                Refusal::new(
                    "worktree-not-discovered",
                    "Git does not report the provisioned worktree",
                )
            })?;
        if linked.primary || linked.locked {
            return Err(Refusal::new(
                "worktree-locked",
                "provisioning recovery refuses primary or locked worktrees",
            ));
        }
        let snapshot = self.exact_worktree_snapshot(&record.repository_root, path)?;
        if record
            .head
            .as_deref()
            .is_some_and(|head| head != snapshot.head)
        {
            return Err(Refusal::new(
                "provisioning-head-changed",
                "provisioned worktree HEAD differs from the recorded commit",
            ));
        }
        Ok(None)
    }

    fn assess_migration(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
        from: &Path,
        to: &Path,
    ) -> Result<Option<RecoveryProof>, Refusal> {
        if from != record.path {
            return Err(Refusal::new(
                "relocation-source-mismatch",
                "migration source differs from the registered worktree path",
            ));
        }
        require_canonical_child(&policy.worktree_root, to)?;
        let discovered = self.git.list_worktrees(&record.repository_root)?;
        let source = discovered.iter().find(|item| item.path == from);
        let destination = discovered.iter().find(|item| item.path == to);
        if source.is_some() && destination.is_some() {
            return Err(Refusal::new(
                "ambiguous-relocation",
                "Git reports both relocation source and destination",
            ));
        }
        if !matches!(record.lifecycle, Lifecycle::Active | Lifecycle::Relocating) {
            return Err(Refusal::new(
                "invalid-relocation-lifecycle",
                "only an active or already-relocating worktree may be migrated",
            ));
        }
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
                if !path_absent(to)? {
                    return Err(Refusal::new(
                        "migration-target-exists",
                        format!("{} already exists", to.display()),
                    ));
                }
                self.git.validate_move_worktree(from, to)?;
                let snapshot = self.exact_worktree_snapshot(&record.repository_root, from)?;
                if let Some(intent) = self.registry.relocation(record.id.as_str())? {
                    if snapshot.head != intent.head {
                        return Err(Refusal::new(
                            "relocation-head-changed",
                            "worktree HEAD changed after relocation was planned",
                        ));
                    }
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
                let snapshot = self.exact_worktree_snapshot(&record.repository_root, to)?;
                if snapshot.head != intent.head {
                    return Err(Refusal::new(
                        "relocation-head-changed",
                        "relocated worktree HEAD differs from durable intent",
                    ));
                }
            }
            (Some(_), Some(_)) => unreachable!("ambiguous topology was refused above"),
            (None, None) => {
                return Err(Refusal::new(
                    "worktree-not-discovered",
                    "Git reports neither relocation source nor destination",
                ));
            }
        }
        Ok(None)
    }

    fn assess_external_retirement(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
        path: &Path,
        now: i64,
    ) -> Result<Option<RecoveryProof>, Refusal> {
        if record.lifecycle != Lifecycle::Finished {
            return Err(Refusal::new(
                "external-worktree-not-finished",
                "only a finished legacy worktree may be retired outside the managed root",
            ));
        }
        if path != record.path || path.starts_with(&policy.worktree_root) {
            return Err(Refusal::new(
                "invalid-external-retirement-path",
                "external retirement requires the exact registered path outside the managed root",
            ));
        }
        let discovered = self.git.list_worktrees(&record.repository_root)?;
        let linked = discovered
            .iter()
            .find(|item| item.path == path)
            .ok_or_else(|| {
                Refusal::new(
                    "worktree-not-discovered",
                    "Git does not report the registered external worktree",
                )
            })?;
        let relocation = self.registry.relocation(record.id.as_str())?;
        if let Some(intent) = relocation.as_ref() {
            if intent.from != *path {
                return Err(Refusal::new(
                    "relocation-source-mismatch",
                    "pending relocation source differs from the registered worktree path",
                ));
            }
            require_canonical_child(&policy.worktree_root, &intent.to)?;
            if discovered.iter().any(|item| item.path == intent.to) {
                return Err(Refusal::new(
                    "ambiguous-relocation",
                    "Git reports both relocation source and destination",
                ));
            }
            if !path_absent(&intent.to)? {
                return Err(Refusal::new(
                    "relocation-destination-exists",
                    "pending relocation destination exists outside Git's worktree inventory",
                ));
            }
            if record.head.as_deref() != Some(intent.head.as_str()) {
                return Err(Refusal::new(
                    "relocation-head-changed",
                    "pending relocation HEAD differs from the finished worktree record",
                ));
            }
        }
        if linked.primary {
            return Err(Refusal::new(
                "primary-worktree",
                "a primary checkout cannot be retired",
            ));
        }
        let snapshot = self.exact_worktree_snapshot(&record.repository_root, path)?;
        if relocation
            .as_ref()
            .is_some_and(|intent| intent.head != snapshot.head)
        {
            return Err(Refusal::new(
                "relocation-head-changed",
                "external worktree HEAD differs from the pending relocation intent",
            ));
        }
        require_clean_unlocked(&snapshot)?;
        self.recovery_for_record(record, &snapshot.head, now)
            .map(Some)
    }

    fn assess_missing(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
        path: &Path,
        now: i64,
    ) -> Result<Option<RecoveryProof>, Refusal> {
        if !path_absent(path)? {
            return Err(Refusal::new(
                "worktree-path-exists",
                format!("{} still exists", path.display()),
            ));
        }
        let discovered = self.git.list_worktrees(&record.repository_root)?;
        if discovered.iter().any(|item| item.path == path) {
            return Err(Refusal::new(
                "worktree-still-registered-by-git",
                "Git still reports the missing worktree path",
            ));
        }
        let removal = self.registry.removal(record.id.as_str())?;
        if let Some(relocation) = self.registry.relocation(record.id.as_str())? {
            if record.lifecycle != Lifecycle::Finished
                || path != record.path
                || path.starts_with(&policy.worktree_root)
                || relocation.from != *path
            {
                return Err(Refusal::new(
                    "invalid-external-retirement-recovery",
                    "stale relocation recovery requires its exact finished external source",
                ));
            }
            require_canonical_child(&policy.worktree_root, &relocation.to)?;
            if discovered.iter().any(|item| item.path == relocation.to) {
                return Err(Refusal::new(
                    "relocation-destination-exists",
                    "pending relocation destination is still registered by Git",
                ));
            }
            if !path_absent(&relocation.to)? {
                return Err(Refusal::new(
                    "relocation-destination-exists",
                    "pending relocation destination still exists on disk",
                ));
            }
            let intent = removal.ok_or_else(|| {
                Refusal::new(
                    "missing-retirement-intent",
                    "an absent external relocation source requires durable removal proof",
                )
            })?;
            if intent.operation != "retire-external"
                || intent.path != *path
                || intent.head != intent.recovery.head
                || intent.head != relocation.head
                || record.head.as_deref() != Some(relocation.head.as_str())
            {
                return Err(Refusal::new(
                    "removal-relocation-mismatch",
                    "external removal proof must match the relocation source and every recorded HEAD",
                ));
            }
            return Ok(Some(intent.recovery));
        }
        if matches!(record.lifecycle, Lifecycle::Active | Lifecycle::Relocating)
            && removal.is_none()
        {
            return Err(Refusal::new(
                "missing-active-worktree",
                "an active or relocating worktree disappeared without durable removal intent",
            ));
        }
        if let Some(intent) = removal {
            if intent.path != *path || intent.head != intent.recovery.head {
                return Err(Refusal::new(
                    "removal-intent-mismatch",
                    "pending removal proof does not match the missing worktree path and HEAD",
                ));
            }
            return Ok(Some(intent.recovery));
        }
        let Some(head) = record.head.as_deref() else {
            return if matches!(
                record.lifecycle,
                Lifecycle::Failed | Lifecycle::Provisioning
            ) {
                Ok(None)
            } else {
                Err(Refusal::new(
                    "missing-recovery-head",
                    "only a failed provisioning record may be reconciled without HEAD",
                ))
            };
        };
        self.recovery_proof(&record.repository_root, head, now)
            .map(Some)
    }

    fn apply_reconciliation(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
        action: &ReconciliationAction,
        _recovery: Option<RecoveryProof>,
        now: i64,
    ) -> Result<OperationEvidence, Refusal> {
        let recovery = self.assess_reconciliation(policy, record, action, now)?;
        match action {
            ReconciliationAction::RecoverProvisioning { path } => {
                let snapshot = self.exact_worktree_snapshot(&record.repository_root, path)?;
                let recorded_at = self.clock.now();
                self.registry
                    .activate(record.id.as_str(), &snapshot.head, recorded_at)?;
                Ok(OperationEvidence {
                    operation: "recover-provisioning".into(),
                    id: record.id.clone(),
                    path: path.clone(),
                    head: Some(snapshot.head),
                    recovery: None,
                    recorded_at,
                })
            }
            ReconciliationAction::Migrate { from, to } => {
                self.apply_migration(record, from, to, now)
            }
            ReconciliationAction::RetireExternal { path } => {
                let proof = recovery.ok_or_else(|| {
                    Refusal::new(
                        "missing-removal-proof",
                        "external retirement requires durable recovery proof",
                    )
                })?;
                let relocation = self.registry.relocation(record.id.as_str())?;
                self.apply_removal(
                    record,
                    path,
                    "retire-external",
                    proof,
                    Some((policy, relocation.as_ref())),
                )
            }
            ReconciliationAction::TombstoneMissing { path } => {
                let pending = self.registry.removal(record.id.as_str())?;
                let head = pending
                    .as_ref()
                    .map(|intent| intent.head.clone())
                    .or_else(|| recovery.as_ref().map(|proof| proof.head.clone()))
                    .or_else(|| record.head.clone());
                let evidence = OperationEvidence {
                    operation: "reconcile-missing".into(),
                    id: record.id.clone(),
                    path: path.clone(),
                    head,
                    recovery,
                    recorded_at: self.clock.now(),
                };
                if let Some(intent) = pending {
                    self.registry.complete_removal(&intent, &evidence)?;
                } else {
                    self.registry.mark_removed(&evidence)?;
                }
                Ok(evidence)
            }
        }
    }

    fn apply_migration(
        &self,
        record: &WorktreeRecord,
        from: &Path,
        to: &Path,
        now: i64,
    ) -> Result<OperationEvidence, Refusal> {
        if record.lifecycle == Lifecycle::Active {
            self.registry.claim_relocation(
                record.id.as_str(),
                self.clock.now(),
                self.lease_timeout_seconds,
            )?;
        } else if record.lifecycle != Lifecycle::Relocating {
            return Err(Refusal::new(
                "invalid-relocation-lifecycle",
                "only an active or already-relocating worktree may be migrated",
            ));
        }
        let pending = self.registry.relocation(record.id.as_str())?;
        let intent = if let Some(intent) = pending {
            intent
        } else {
            let snapshot = self.exact_worktree_snapshot(&record.repository_root, from)?;
            let intent = RelocationIntent {
                id: record.id.clone(),
                from: from.to_path_buf(),
                to: to.to_path_buf(),
                head: snapshot.head,
                planned_at: now,
            };
            self.registry.begin_relocation(&intent)?;
            intent
        };
        let discovered = self.git.list_worktrees(&record.repository_root)?;
        let source_exists = discovered.iter().any(|item| item.path == from);
        let destination_exists = discovered.iter().any(|item| item.path == to);
        if source_exists && !destination_exists {
            self.git.move_worktree(&record.repository_root, from, to)?;
        }
        let snapshot = self.exact_worktree_snapshot(&record.repository_root, to)?;
        if snapshot.head != intent.head {
            return Err(Refusal::new(
                "relocation-head-changed",
                "relocated worktree HEAD differs from durable intent",
            ));
        }
        let evidence = OperationEvidence {
            operation: "migrate".into(),
            id: record.id.clone(),
            path: to.to_path_buf(),
            head: Some(snapshot.head),
            recovery: None,
            recorded_at: self.clock.now(),
        };
        self.registry.complete_relocation(&intent, &evidence)?;
        Ok(evidence)
    }

    fn recovery_proof(
        &self,
        repository: &Path,
        head: &str,
        now: i64,
    ) -> Result<RecoveryProof, Refusal> {
        let refs = self.git.recovery_refs(repository, head)?;
        if refs.is_empty() {
            return Err(Refusal::new(
                "no-remote-recovery-proof",
                format!("commit {head} is not reachable from an advertised remote ref"),
            ));
        }
        Ok(RecoveryProof {
            head: head.to_owned(),
            refs,
            observed_at: now,
        })
    }

    fn recovery_for_record(
        &self,
        record: &WorktreeRecord,
        head: &str,
        now: i64,
    ) -> Result<RecoveryProof, Refusal> {
        if let Some(intent) = self.registry.removal(record.id.as_str())? {
            if intent.path != record.path {
                return Err(Refusal::new(
                    "removal-intent-mismatch",
                    "pending removal proof does not match the current worktree path",
                ));
            }
        }
        self.recovery_proof(&record.repository_root, head, now)
    }

    fn require_retirement_topology(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
        path: &Path,
        expected: &RelocationIntent,
        source_should_exist: bool,
    ) -> Result<(), Refusal> {
        let current = self
            .registry
            .relocation(record.id.as_str())?
            .ok_or_else(|| {
                Refusal::new(
                    "relocation-intent-missing",
                    "stale relocation evidence disappeared during external retirement",
                )
            })?;
        if current != *expected {
            return Err(Refusal::new(
                "relocation-intent-changed",
                "stale relocation evidence changed during external retirement",
            ));
        }
        if record.lifecycle != Lifecycle::Finished
            || path != record.path
            || path.starts_with(&policy.worktree_root)
            || expected.from != *path
            || record.head.as_deref() != Some(expected.head.as_str())
        {
            return Err(Refusal::new(
                "invalid-external-retirement-recovery",
                "external retirement requires the exact finished source and recorded HEAD",
            ));
        }
        require_canonical_child(&policy.worktree_root, &expected.to)?;
        let discovered = self.git.list_worktrees(&record.repository_root)?;
        let source_exists = discovered.iter().any(|item| item.path == expected.from);
        if discovered.iter().any(|item| item.path == expected.to) {
            return Err(Refusal::new(
                "relocation-destination-exists",
                "pending relocation destination is registered by Git",
            ));
        }
        if !path_absent(&expected.to)? {
            return Err(Refusal::new(
                "relocation-destination-exists",
                "pending relocation destination exists on disk",
            ));
        }
        if source_should_exist {
            if !source_exists {
                return Err(Refusal::new(
                    "relocation-source-missing",
                    "external retirement source disappeared before removal",
                ));
            }
        } else if source_exists || !path_absent(path)? {
            return Err(Refusal::new(
                "worktree-removal-incomplete",
                "external retirement source still exists after removal",
            ));
        }
        Ok(())
    }

    fn apply_removal(
        &self,
        record: &WorktreeRecord,
        path: &Path,
        operation: &str,
        proof: RecoveryProof,
        retirement: Option<(&WorkspacePolicy, Option<&RelocationIntent>)>,
    ) -> Result<OperationEvidence, Refusal> {
        if let Some((policy, Some(relocation))) = retirement {
            self.require_retirement_topology(policy, record, path, relocation, true)?;
        }
        let before_intent = self.exact_worktree_snapshot(&record.repository_root, path)?;
        require_clean_unlocked(&before_intent)?;
        if before_intent.head != proof.head {
            return Err(Refusal::new(
                "worktree-head-changed-during-proof",
                "worktree HEAD changed while remote recovery proof was collected",
            ));
        }
        if let Some(intent) = self.registry.removal(record.id.as_str())? {
            if intent.path != path || intent.operation != operation {
                return Err(Refusal::new(
                    "removal-intent-mismatch",
                    "pending removal intent differs from the revalidated operation",
                ));
            }
        }
        let intent = RemovalIntent {
            id: record.id.clone(),
            path: path.to_path_buf(),
            head: proof.head.clone(),
            recovery: proof,
            operation: operation.to_owned(),
            planned_at: self.clock.now(),
        };
        self.registry.begin_removal(&intent)?;
        let before_remove = self.exact_worktree_snapshot(&record.repository_root, path)?;
        require_clean_unlocked(&before_remove)?;
        if before_remove.head != intent.head {
            return Err(Refusal::new(
                "worktree-head-changed-after-intent",
                "worktree HEAD changed after removal intent became durable",
            ));
        }
        if let Some((policy, Some(relocation))) = retirement {
            self.require_retirement_topology(policy, record, path, relocation, true)?;
        }
        self.git.remove(&record.repository_root, path)?;
        if let Some((policy, Some(relocation))) = retirement {
            self.require_retirement_topology(policy, record, path, relocation, false)?;
        }
        if !path_absent(path)?
            || self
                .git
                .list_worktrees(&record.repository_root)?
                .iter()
                .any(|item| item.path == path)
        {
            return Err(Refusal::new(
                "worktree-removal-incomplete",
                "Git returned success but the worktree still exists",
            ));
        }
        let evidence = OperationEvidence {
            operation: intent.operation.clone(),
            id: record.id.clone(),
            path: path.to_path_buf(),
            head: Some(intent.head.clone()),
            recovery: Some(intent.recovery.clone()),
            recorded_at: self.clock.now(),
        };
        self.registry.complete_removal(&intent, &evidence)?;
        Ok(evidence)
    }
}

fn path_absent(path: &Path) -> Result<bool, Refusal> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(Refusal::new(
            "worktree-path-inspection-failed",
            format!("{}: {error}", path.display()),
        )),
    }
}

fn validate_selected_records(
    policy: &WorkspacePolicy,
    records: &[WorktreeRecord],
    selected_ids: &[WorktreeId],
) -> Result<(), Refusal> {
    for id in selected_ids {
        let record = records
            .iter()
            .find(|record| record.id == *id)
            .ok_or_else(|| {
                Refusal::new(
                    "unknown-worktree-id",
                    format!("{} is not registered", id.as_str()),
                )
            })?;
        if !record.repository_root.starts_with(&policy.workspace_root) {
            return Err(Refusal::new(
                "selected-worktree-outside-policy",
                format!(
                    "{} belongs to repository {} outside workspace {}",
                    id.as_str(),
                    record.repository_root.display(),
                    policy.workspace_root.display()
                ),
            ));
        }
    }
    Ok(())
}

fn require_canonical_policy(policy: &WorkspacePolicy) -> Result<(), Refusal> {
    policy.validate()?;
    let workspace = std::fs::canonicalize(&policy.workspace_root).map_err(|error| {
        Refusal::new(
            "workspace-root-invalid",
            format!("{}: {error}", policy.workspace_root.display()),
        )
    })?;
    let worktrees = canonicalize_future_path(&policy.worktree_root)?;
    if workspace != policy.workspace_root || worktrees != policy.worktree_root {
        return Err(Refusal::new(
            "non-canonical-policy-path",
            "workspace and managed worktree roots must be canonical",
        ));
    }
    Ok(())
}

fn require_canonical_child(root: &Path, candidate: &Path) -> Result<(), Refusal> {
    require_child(root, candidate)?;
    let canonical = canonicalize_future_path(candidate)?;
    if canonical != candidate || !canonical.starts_with(root) {
        return Err(Refusal::new(
            "non-canonical-worktree-path",
            format!(
                "managed path {} resolves to {} instead of remaining below {}",
                candidate.display(),
                canonical.display(),
                root.display()
            ),
        ));
    }
    Ok(())
}

fn canonicalize_future_path(path: &Path) -> Result<PathBuf, Refusal> {
    let mut suffix = Vec::new();
    let mut existing = path;
    loop {
        match std::fs::metadata(existing) {
            Ok(metadata) => {
                if !metadata.is_dir() {
                    return Err(Refusal::new(
                        "worktree-root-ancestor-not-directory",
                        format!("{} is not a directory", existing.display()),
                    ));
                }
                break;
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if std::fs::symlink_metadata(existing).is_ok() {
                    return Err(Refusal::new(
                        "policy-path-dangling-symlink",
                        format!("{} is a dangling symlink", existing.display()),
                    ));
                }
                let component = existing.file_name().ok_or_else(|| {
                    Refusal::new(
                        "policy-path-invalid",
                        format!("{} has no existing ancestor", path.display()),
                    )
                })?;
                suffix.push(component.to_os_string());
                existing = existing.parent().ok_or_else(|| {
                    Refusal::new(
                        "policy-path-invalid",
                        format!("{} has no existing ancestor", path.display()),
                    )
                })?;
            }
            Err(error) => {
                return Err(Refusal::new(
                    "policy-path-invalid",
                    format!("{}: {error}", existing.display()),
                ));
            }
        }
    }
    let mut canonical = std::fs::canonicalize(existing).map_err(|error| {
        Refusal::new(
            "policy-path-invalid",
            format!("{}: {error}", existing.display()),
        )
    })?;
    for component in suffix.into_iter().rev() {
        canonical.push(component);
    }
    Ok(canonical)
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
            "tracked, untracked, or ignored files make cleanup unsafe",
        ));
    }
    Ok(())
}

fn require_exact_snapshot_path(
    snapshot: &WorktreeSnapshot,
    expected: &Path,
) -> Result<(), Refusal> {
    if snapshot.path != expected {
        return Err(Refusal::new(
            "worktree-path-changed",
            format!(
                "registered path {} resolves to linked worktree {}",
                expected.display(),
                snapshot.path.display()
            ),
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
    use std::collections::{BTreeMap, VecDeque};
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
        discovered_sequence: Mutex<VecDeque<Vec<DiscoveredWorktree>>>,
        recoverable: bool,
        resolved_revision: Mutex<Option<String>>,
        fail_remove: bool,
        snapshot_sequence: Mutex<VecDeque<String>>,
    }

    impl GitPort for FakeGit {
        fn repository_snapshot(&self, _repository: &Path) -> Result<RepositorySnapshot, Refusal> {
            Ok(RepositorySnapshot {
                root: self.repository.clone(),
                name: "repo".into(),
                head: "main".into(),
            })
        }

        fn resolve_revision(&self, _repository: &Path, revision: &str) -> Result<String, Refusal> {
            Ok(self
                .resolved_revision
                .lock()
                .unwrap()
                .clone()
                .unwrap_or_else(|| revision.to_owned()))
        }

        fn worktree_snapshot(
            &self,
            _repository: &Path,
            worktree: &Path,
        ) -> Result<WorktreeSnapshot, Refusal> {
            let mut snapshot = self
                .snapshots
                .lock()
                .unwrap()
                .get(worktree)
                .cloned()
                .ok_or_else(|| {
                    Refusal::new("worktree-not-found", worktree.display().to_string())
                })?;
            if let Some(head) = self.snapshot_sequence.lock().unwrap().pop_front() {
                snapshot.head = head;
            }
            Ok(snapshot)
        }

        fn create_detached(&self, _plan: &CreatePlan) -> Result<(), Refusal> {
            Ok(())
        }

        fn recovery_refs(&self, _repository: &Path, _head: &str) -> Result<Vec<String>, Refusal> {
            Ok(if self.recoverable {
                vec!["refs/remotes/origin/main".into()]
            } else {
                Vec::new()
            })
        }

        fn remove(&self, _repository: &Path, worktree: &Path) -> Result<(), Refusal> {
            if self.fail_remove {
                return Err(Refusal::new("remove-failed", "injected removal failure"));
            }
            if worktree.exists() {
                std::fs::remove_dir(worktree).unwrap();
            }
            self.snapshots.lock().unwrap().remove(worktree);
            self.discovered
                .lock()
                .unwrap()
                .retain(|item| item.path != worktree);
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
            if let Some(discovered) = self.discovered_sequence.lock().unwrap().pop_front() {
                return Ok(discovered);
            }
            Ok(self.discovered.lock().unwrap().clone())
        }
    }

    struct FakeRegistry {
        records: Mutex<Vec<WorktreeRecord>>,
        relocations: Mutex<BTreeMap<String, RelocationIntent>>,
        removals: Mutex<BTreeMap<String, RemovalIntent>>,
        live_leases: u64,
    }

    impl RegistryPort for FakeRegistry {
        fn reserve(&self, record: &WorktreeRecord) -> Result<(), Refusal> {
            self.records.lock().unwrap().push(record.clone());
            Ok(())
        }

        fn activate(&self, id: &str, head: &str, now: i64) -> Result<(), Refusal> {
            let mut records = self.records.lock().unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id.as_str() == id)
                .unwrap();
            record.lifecycle = Lifecycle::Active;
            record.head = Some(head.to_owned());
            record.last_seen_at = now;
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

        fn mark_finished(
            &self,
            id: &str,
            head: &str,
            now: i64,
            _lease_timeout: i64,
        ) -> Result<(), Refusal> {
            if self.live_leases > 0 {
                return Err(Refusal::new("live-session", "test lease is live"));
            }
            let mut records = self.records.lock().unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id.as_str() == id)
                .unwrap();
            record.lifecycle = Lifecycle::Finished;
            record.head = Some(head.to_owned());
            record.finished_at = Some(now);
            Ok(())
        }

        fn claim_expired(
            &self,
            id: &str,
            head: &str,
            now: i64,
            expire_before: i64,
            _lease_timeout: i64,
        ) -> Result<(), Refusal> {
            if self.live_leases > 0 {
                return Err(Refusal::new("live-session", "test lease is live"));
            }
            let mut records = self.records.lock().unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id.as_str() == id)
                .unwrap();
            if record.lifecycle != Lifecycle::Active || record.last_seen_at > expire_before {
                return Err(Refusal::new(
                    "invalid-lifecycle-transition",
                    "record is not expired",
                ));
            }
            record.lifecycle = Lifecycle::Finished;
            record.head = Some(head.to_owned());
            record.finished_at = Some(now);
            Ok(())
        }

        fn claim_relocation(
            &self,
            id: &str,
            _now: i64,
            _lease_timeout: i64,
        ) -> Result<(), Refusal> {
            if self.live_leases > 0 {
                return Err(Refusal::new("live-session", "test lease is live"));
            }
            let mut records = self.records.lock().unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id.as_str() == id)
                .unwrap();
            if record.lifecycle != Lifecycle::Active {
                return Err(Refusal::new(
                    "invalid-lifecycle-transition",
                    "record is not active",
                ));
            }
            record.lifecycle = Lifecycle::Relocating;
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
            let mut records = self.records.lock().unwrap();
            let record = records
                .iter_mut()
                .find(|record| record.id == intent.id)
                .unwrap();
            record.path = intent.to.clone();
            record.lifecycle = Lifecycle::Active;
            self.relocations.lock().unwrap().remove(intent.id.as_str());
            Ok(())
        }

        fn removal(&self, id: &str) -> Result<Option<RemovalIntent>, Refusal> {
            Ok(self.removals.lock().unwrap().get(id).cloned())
        }

        fn begin_removal(&self, intent: &RemovalIntent) -> Result<(), Refusal> {
            let relocation = self
                .relocations
                .lock()
                .unwrap()
                .get(intent.id.as_str())
                .cloned();
            if let Some(relocation) = relocation {
                if intent.operation != "retire-external"
                    || relocation.from != intent.path
                    || relocation.head != intent.head
                {
                    return Err(Refusal::new(
                        "removal-relocation-mismatch",
                        "pending relocation can only be superseded by retirement of its exact source and HEAD",
                    ));
                }
            }
            self.removals
                .lock()
                .unwrap()
                .insert(intent.id.to_string(), intent.clone());
            Ok(())
        }

        fn complete_removal(
            &self,
            intent: &RemovalIntent,
            evidence: &OperationEvidence,
        ) -> Result<(), Refusal> {
            if let Some(relocation) = self
                .relocations
                .lock()
                .unwrap()
                .get(intent.id.as_str())
                .cloned()
            {
                if intent.operation != "retire-external"
                    || relocation.from != intent.path
                    || relocation.head != intent.head
                {
                    return Err(Refusal::new(
                        "removal-relocation-mismatch",
                        "pending relocation can only be completed by retirement of its exact source and HEAD",
                    ));
                }
            }
            self.mark_removed(evidence)?;
            self.removals.lock().unwrap().remove(intent.id.as_str());
            self.relocations.lock().unwrap().remove(intent.id.as_str());
            Ok(())
        }
    }

    fn fake_git(repository: PathBuf) -> FakeGit {
        FakeGit {
            repository,
            snapshots: Mutex::new(BTreeMap::new()),
            discovered: Mutex::new(Vec::new()),
            discovered_sequence: Mutex::new(VecDeque::new()),
            recoverable: true,
            resolved_revision: Mutex::new(None),
            fail_remove: false,
            snapshot_sequence: Mutex::new(VecDeque::new()),
        }
    }

    fn fake_registry(records: Vec<WorktreeRecord>) -> FakeRegistry {
        FakeRegistry {
            records: Mutex::new(records),
            relocations: Mutex::new(BTreeMap::new()),
            removals: Mutex::new(BTreeMap::new()),
            live_leases: 0,
        }
    }

    fn policy(workspace_root: PathBuf, worktree_root: PathBuf) -> WorkspacePolicy {
        WorkspacePolicy {
            version: 1,
            name: "test".into(),
            workspace_root,
            worktree_root,
            expire_after_seconds: 60,
            protect_workspace_root: true,
        }
    }

    fn named_record(
        id: &str,
        repository_root: PathBuf,
        path: PathBuf,
        lifecycle: Lifecycle,
    ) -> WorktreeRecord {
        WorktreeRecord {
            id: WorktreeId::new(id).unwrap(),
            repository_root,
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

    fn record(path: PathBuf, lifecycle: Lifecycle) -> WorktreeRecord {
        let repository = path.parent().unwrap().join("repo");
        named_record("legacy-one", repository, path, lifecycle)
    }

    fn discovered_worktree(path: &Path, head: &str) -> DiscoveredWorktree {
        DiscoveredWorktree {
            path: path.to_path_buf(),
            head: Some(head.into()),
            locked: false,
            primary: false,
        }
    }

    fn clean_snapshot(path: &Path, head: &str) -> WorktreeSnapshot {
        WorktreeSnapshot {
            path: path.to_path_buf(),
            head: head.into(),
            dirty: false,
            locked: false,
        }
    }

    fn stale_relocation(record: &WorktreeRecord, destination: &Path) -> RelocationIntent {
        RelocationIntent {
            id: record.id.clone(),
            from: record.path.clone(),
            to: destination.to_path_buf(),
            head: record.head.clone().unwrap(),
            planned_at: 2,
        }
    }

    fn retirement_removal(record: &WorktreeRecord) -> RemovalIntent {
        let head = record.head.clone().unwrap();
        RemovalIntent {
            id: record.id.clone(),
            path: record.path.clone(),
            head: head.clone(),
            recovery: RecoveryProof {
                head,
                refs: vec!["origin:refs/heads/main".into()],
                observed_at: 3,
            },
            operation: "retire-external".into(),
            planned_at: 3,
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
                resolved_revision: Mutex::new(None),
                fail_remove: false,
                snapshot_sequence: Mutex::new(VecDeque::new()),
                discovered_sequence: Mutex::new(VecDeque::new()),
            },
            FakeRegistry {
                records: Mutex::new(vec![registered.clone()]),
                relocations: Mutex::new(BTreeMap::new()),
                removals: Mutex::new(BTreeMap::new()),
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
            .reconcile(&policy, std::slice::from_ref(&registered.id), true, false)
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
                resolved_revision: Mutex::new(None),
                fail_remove: false,
                snapshot_sequence: Mutex::new(VecDeque::new()),
                discovered_sequence: Mutex::new(VecDeque::new()),
            },
            FakeRegistry {
                records: Mutex::new(vec![registered.clone()]),
                relocations: Mutex::new(BTreeMap::new()),
                removals: Mutex::new(BTreeMap::new()),
                live_leases: 0,
            },
            FixedClock,
        );

        let assessments = manager
            .reconcile(&policy, std::slice::from_ref(&registered.id), true, false)
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

    #[test]
    #[allow(clippy::too_many_lines)]
    fn gc_apply_requires_exact_ids_and_only_touches_selected_workspace_records() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let outside_repository = temporary.path().join("outside-repo");
        let managed_root = temporary.path().join("managed");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&outside_repository).unwrap();
        std::fs::create_dir_all(&managed_root).unwrap();

        let selected_path = managed_root.join("repo/selected");
        let unselected_path = managed_root.join("repo/unselected");
        let outside_path = managed_root.join("outside/outside");
        for path in [&selected_path, &unselected_path, &outside_path] {
            std::fs::create_dir_all(path).unwrap();
        }
        let mut selected = named_record(
            "selected",
            repository.clone(),
            selected_path.clone(),
            Lifecycle::Finished,
        );
        // Version 0.2 persisted the creation/adoption HEAD rather than the HEAD observed at finish.
        // An extant clean tree is assessed from its current HEAD so those records remain recoverable.
        selected.head = Some("legacy-stale-head".into());
        let unselected = named_record(
            "unselected",
            repository.clone(),
            unselected_path.clone(),
            Lifecycle::Finished,
        );
        let outside = named_record(
            "outside",
            outside_repository,
            outside_path.clone(),
            Lifecycle::Finished,
        );
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([
                (
                    selected_path.clone(),
                    WorktreeSnapshot {
                        path: selected_path.clone(),
                        head: "abc".into(),
                        dirty: false,
                        locked: false,
                    },
                ),
                (
                    unselected_path.clone(),
                    WorktreeSnapshot {
                        path: unselected_path.clone(),
                        head: "abc".into(),
                        dirty: false,
                        locked: false,
                    },
                ),
                (
                    outside_path.clone(),
                    WorktreeSnapshot {
                        path: outside_path.clone(),
                        head: "abc".into(),
                        dirty: false,
                        locked: false,
                    },
                ),
            ])),
            discovered: Mutex::new(vec![
                DiscoveredWorktree {
                    path: selected_path.clone(),
                    head: Some("abc".into()),
                    locked: false,
                    primary: false,
                },
                DiscoveredWorktree {
                    path: unselected_path.clone(),
                    head: Some("abc".into()),
                    locked: false,
                    primary: false,
                },
                DiscoveredWorktree {
                    path: outside_path.clone(),
                    head: Some("abc".into()),
                    locked: false,
                    primary: false,
                },
            ]),
            ..fake_git(repository)
        };
        let manager = WorktreeManager::new(
            git,
            fake_registry(vec![selected.clone(), unselected.clone(), outside]),
            FixedClock,
        );
        let policy = policy(workspace, managed_root);

        assert_eq!(
            manager.gc(&policy, &[], true).unwrap_err().code,
            "explicit-cleanup-selection-required"
        );
        assert_eq!(
            manager
                .gc(&policy, &[WorktreeId::new("unknown").unwrap()], true)
                .unwrap_err()
                .code,
            "unknown-worktree-id"
        );

        let dry_run = manager.gc(&policy, &[], false).unwrap();
        assert_eq!(dry_run.len(), 2);
        assert!(dry_run.iter().any(|item| item.record.id == selected.id));
        assert!(dry_run.iter().any(|item| item.record.id == unselected.id));

        let applied = manager
            .gc(&policy, std::slice::from_ref(&selected.id), true)
            .unwrap();
        assert_eq!(applied.len(), 1);
        assert_eq!(applied[0].record.id, selected.id);
        assert!(applied[0].evidence.is_some());
        assert!(!selected_path.exists());
        assert!(unselected_path.exists());
        assert!(outside_path.exists());
        let records = manager.registry().list().unwrap();
        assert_eq!(
            records
                .iter()
                .find(|record| record.id == selected.id)
                .unwrap()
                .lifecycle,
            Lifecycle::Removed
        );
        assert_eq!(
            records
                .iter()
                .find(|record| record.id == unselected.id)
                .unwrap()
                .lifecycle,
            Lifecycle::Finished
        );
    }

    #[test]
    fn missing_active_record_without_durable_removal_intent_is_refused() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        std::fs::create_dir_all(&repository).unwrap();
        let missing = managed_root.join("repo/missing-active");
        let registered = named_record(
            "missing-active",
            repository.clone(),
            missing,
            Lifecycle::Active,
        );
        let manager = WorktreeManager::new(
            fake_git(repository),
            fake_registry(vec![registered.clone()]),
            FixedClock,
        );

        let assessments = manager
            .reconcile(
                &policy(workspace, managed_root),
                std::slice::from_ref(&registered.id),
                false,
                false,
            )
            .unwrap();
        assert_eq!(assessments.len(), 1);
        assert!(!assessments[0].eligible);
        assert_eq!(
            assessments[0].refusal.as_ref().unwrap().code,
            "missing-active-worktree"
        );
    }

    #[test]
    fn durable_removal_intent_allows_missing_active_record_to_complete() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        std::fs::create_dir_all(&repository).unwrap();
        let missing = managed_root.join("repo/missing-after-remove");
        let mut registered = named_record(
            "missing-after-remove",
            repository.clone(),
            missing.clone(),
            Lifecycle::Active,
        );
        registered.head = Some("legacy-stale-head".into());
        let removal = RemovalIntent {
            id: registered.id.clone(),
            path: missing,
            head: "abc".into(),
            recovery: RecoveryProof {
                head: "abc".into(),
                refs: vec!["refs/remotes/origin/main".into()],
                observed_at: 900,
            },
            operation: "remove".into(),
            planned_at: 900,
        };
        let registry = fake_registry(vec![registered.clone()]);
        registry
            .removals
            .lock()
            .unwrap()
            .insert(registered.id.to_string(), removal);
        let manager = WorktreeManager::new(fake_git(repository), registry, FixedClock);

        let assessments = manager
            .reconcile(
                &policy(workspace, managed_root),
                std::slice::from_ref(&registered.id),
                true,
                false,
            )
            .unwrap();
        assert!(assessments[0].eligible);
        assert_eq!(
            assessments[0].evidence.as_ref().unwrap().operation,
            "reconcile-missing"
        );
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Removed
        );
        assert!(
            manager
                .registry()
                .removal(registered.id.as_str())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn reconcile_recovers_interrupted_provisioning_and_failed_linked_trees() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        std::fs::create_dir_all(&repository).unwrap();
        let provisioning_path = managed_root.join("repo/provisioning");
        let failed_path = managed_root.join("repo/failed");
        std::fs::create_dir_all(&provisioning_path).unwrap();
        std::fs::create_dir_all(&failed_path).unwrap();
        let mut provisioning = named_record(
            "provisioning",
            repository.clone(),
            provisioning_path.clone(),
            Lifecycle::Provisioning,
        );
        let mut failed = named_record(
            "failed",
            repository.clone(),
            failed_path.clone(),
            Lifecycle::Failed,
        );
        provisioning.head = None;
        failed.head = None;
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([
                (
                    provisioning_path.clone(),
                    WorktreeSnapshot {
                        path: provisioning_path.clone(),
                        head: "provisioned-head".into(),
                        dirty: false,
                        locked: false,
                    },
                ),
                (
                    failed_path.clone(),
                    WorktreeSnapshot {
                        path: failed_path.clone(),
                        head: "failed-head".into(),
                        dirty: false,
                        locked: false,
                    },
                ),
            ])),
            discovered: Mutex::new(vec![
                DiscoveredWorktree {
                    path: provisioning_path,
                    head: Some("provisioned-head".into()),
                    locked: false,
                    primary: false,
                },
                DiscoveredWorktree {
                    path: failed_path,
                    head: Some("failed-head".into()),
                    locked: false,
                    primary: false,
                },
            ]),
            ..fake_git(repository)
        };
        let ids = [provisioning.id.clone(), failed.id.clone()];
        let manager =
            WorktreeManager::new(git, fake_registry(vec![provisioning, failed]), FixedClock);

        let assessments = manager
            .reconcile(&policy(workspace, managed_root), &ids, true, false)
            .unwrap();
        assert_eq!(assessments.len(), 2);
        assert!(assessments.iter().all(|assessment| {
            assessment.eligible
                && assessment
                    .evidence
                    .as_ref()
                    .is_some_and(|evidence| evidence.operation == "recover-provisioning")
        }));
        let records = manager.registry().list().unwrap();
        assert!(
            records
                .iter()
                .all(|record| record.lifecycle == Lifecycle::Active)
        );
        assert!(records.iter().any(|record| {
            record.id.as_str() == "provisioning"
                && record.head.as_deref() == Some("provisioned-head")
        }));
        assert!(records.iter().any(|record| {
            record.id.as_str() == "failed" && record.head.as_deref() == Some("failed-head")
        }));
    }

    #[test]
    fn finish_persists_the_observed_head() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let path = temporary.path().join("managed/repo/finish-head");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        let mut registered = named_record(
            "finish-head",
            repository.clone(),
            path.clone(),
            Lifecycle::Active,
        );
        registered.head = Some("old-head".into());
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([(
                path.clone(),
                WorktreeSnapshot {
                    path: path.clone(),
                    head: "observed-head".into(),
                    dirty: false,
                    locked: false,
                },
            )])),
            ..fake_git(repository)
        };
        let manager = WorktreeManager::new(git, fake_registry(vec![registered]), FixedClock);

        let evidence = manager.finish(&path).unwrap();
        assert_eq!(evidence.head.as_deref(), Some("observed-head"));
        let finished = &manager.registry().list().unwrap()[0];
        assert_eq!(finished.lifecycle, Lifecycle::Finished);
        assert_eq!(finished.head.as_deref(), Some("observed-head"));
        assert_eq!(finished.finished_at, Some(1_000));
    }

    #[test]
    fn finish_refuses_a_live_lease_without_changing_the_record() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let path = temporary.path().join("managed/repo/live-lease");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        let registered = named_record(
            "live-lease",
            repository.clone(),
            path.clone(),
            Lifecycle::Active,
        );
        let mut registry = fake_registry(vec![registered.clone()]);
        registry.live_leases = 1;
        let manager = WorktreeManager::new(fake_git(repository), registry, FixedClock);

        assert_eq!(manager.finish(&path).unwrap_err().code, "live-session");
        assert_eq!(manager.registry().list().unwrap()[0], registered);
    }

    #[test]
    fn adopt_refuses_the_primary_checkout() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        std::fs::create_dir_all(&repository).unwrap();
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([(
                repository.clone(),
                WorktreeSnapshot {
                    path: repository.clone(),
                    head: "abc".into(),
                    dirty: false,
                    locked: false,
                },
            )])),
            discovered: Mutex::new(vec![DiscoveredWorktree {
                path: repository.clone(),
                head: Some("abc".into()),
                locked: false,
                primary: true,
            }]),
            ..fake_git(repository.clone())
        };
        let manager = WorktreeManager::new(git, fake_registry(Vec::new()), FixedClock);

        let refusal = manager
            .adopt(
                &policy(workspace, managed_root),
                &repository,
                &repository,
                WorktreeId::new("primary").unwrap(),
                "test".into(),
                "test".into(),
            )
            .unwrap_err();
        assert_eq!(refusal.code, "primary-worktree");
        assert!(manager.registry().list().unwrap().is_empty());
    }

    #[test]
    fn create_revalidates_the_reviewed_path_and_base() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        std::fs::create_dir_all(&repository).unwrap();
        let manager = WorktreeManager::new(
            fake_git(repository.clone()),
            fake_registry(Vec::new()),
            FixedClock,
        );
        let policy = policy(workspace, managed_root);
        let plan = manager
            .plan_create(
                &policy,
                CreateRequest {
                    id: WorktreeId::new("create-drift").unwrap(),
                    repository,
                    purpose: "test".into(),
                    base: b10x_worktree_domain::GitRevision::new("reviewed-base").unwrap(),
                    owner: "test".into(),
                },
            )
            .unwrap();

        let mut path_drift = plan.clone();
        path_drift.path.push("changed");
        assert_eq!(
            manager.create(&policy, &path_drift).unwrap_err().code,
            "create-plan-path-changed"
        );

        *manager.git.resolved_revision.lock().unwrap() = Some("changed-base".into());
        assert_eq!(
            manager.create(&policy, &plan).unwrap_err().code,
            "create-plan-base-not-immutable"
        );
        assert!(manager.registry().list().unwrap().is_empty());
    }

    #[cfg(unix)]
    #[test]
    fn create_refuses_a_symlinked_parent_introduced_after_planning() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let external = temporary.path().join("external");
        std::fs::create_dir_all(&repository).unwrap();
        let manager = WorktreeManager::new(
            fake_git(repository.clone()),
            fake_registry(Vec::new()),
            FixedClock,
        );
        let policy = policy(workspace, managed_root.clone());
        let plan = manager
            .plan_create(
                &policy,
                CreateRequest {
                    id: WorktreeId::new("symlink-escape").unwrap(),
                    repository,
                    purpose: "test".into(),
                    base: b10x_worktree_domain::GitRevision::new("immutable-base").unwrap(),
                    owner: "test".into(),
                },
            )
            .unwrap();

        std::fs::create_dir(&managed_root).unwrap();
        std::fs::create_dir(&external).unwrap();
        symlink(&external, managed_root.join("repo")).unwrap();

        assert_eq!(
            manager.create(&policy, &plan).unwrap_err().code,
            "non-canonical-worktree-path"
        );
        assert!(!external.join("symlink-escape").exists());
        assert!(manager.registry().list().unwrap().is_empty());
    }

    #[test]
    fn gc_refuses_when_the_observed_path_differs_from_the_registered_path() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let path = managed_root.join("repo/path-drift");
        let observed = temporary.path().join("elsewhere/path-drift");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        let registered = named_record(
            "path-drift",
            repository.clone(),
            path.clone(),
            Lifecycle::Finished,
        );
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([(
                path.clone(),
                WorktreeSnapshot {
                    path: observed,
                    head: "abc".into(),
                    dirty: false,
                    locked: false,
                },
            )])),
            ..fake_git(repository)
        };
        let manager = WorktreeManager::new(git, fake_registry(vec![registered]), FixedClock);

        let assessment = manager
            .gc(&policy(workspace, managed_root), &[], false)
            .unwrap()
            .pop()
            .unwrap();
        assert_eq!(assessment.refusal.unwrap().code, "worktree-path-changed");
        assert!(path.exists());
    }

    #[test]
    fn gc_persists_removal_intent_before_a_failed_delete() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let path = managed_root.join("repo/failing-remove");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        let registered = named_record(
            "failing-remove",
            repository.clone(),
            path.clone(),
            Lifecycle::Finished,
        );
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([(
                path.clone(),
                WorktreeSnapshot {
                    path: path.clone(),
                    head: "abc".into(),
                    dirty: false,
                    locked: false,
                },
            )])),
            discovered: Mutex::new(vec![DiscoveredWorktree {
                path: path.clone(),
                head: Some("abc".into()),
                locked: false,
                primary: false,
            }]),
            fail_remove: true,
            ..fake_git(repository)
        };
        let manager =
            WorktreeManager::new(git, fake_registry(vec![registered.clone()]), FixedClock);

        let assessments = manager
            .gc(
                &policy(workspace, managed_root),
                std::slice::from_ref(&registered.id),
                true,
            )
            .unwrap();
        assert_eq!(assessments.len(), 1);
        assert!(!assessments[0].eligible);
        assert_eq!(
            assessments[0].refusal.as_ref().unwrap().code,
            "remove-failed"
        );
        let intent = manager
            .registry()
            .removal(registered.id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(intent.path, path);
        assert_eq!(intent.head, "abc");
        assert!(
            intent
                .recovery
                .refs
                .contains(&"refs/remotes/origin/main".into())
        );
        assert!(path.exists());
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Finished
        );
    }

    #[test]
    fn gc_refuses_when_head_changes_while_remote_proof_is_collected() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let path = managed_root.join("repo/head-race");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        let registered = named_record(
            "head-race",
            repository.clone(),
            path.clone(),
            Lifecycle::Finished,
        );
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([(
                path.clone(),
                WorktreeSnapshot {
                    path: path.clone(),
                    head: "proof-head".into(),
                    dirty: false,
                    locked: false,
                },
            )])),
            discovered: Mutex::new(vec![DiscoveredWorktree {
                path: path.clone(),
                head: Some("proof-head".into()),
                locked: false,
                primary: false,
            }]),
            snapshot_sequence: Mutex::new(VecDeque::from([
                "proof-head".into(),
                "proof-head".into(),
                "unpublished-head".into(),
            ])),
            ..fake_git(repository)
        };
        let manager =
            WorktreeManager::new(git, fake_registry(vec![registered.clone()]), FixedClock);

        let assessments = manager
            .gc(
                &policy(workspace, managed_root),
                std::slice::from_ref(&registered.id),
                true,
            )
            .unwrap();
        assert_eq!(
            assessments[0].refusal.as_ref().unwrap().code,
            "worktree-head-changed-during-proof"
        );
        assert!(path.exists());
        assert!(
            manager
                .registry()
                .removal(registered.id.as_str())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn gc_refuses_when_head_changes_after_removal_intent_is_durable() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let path = managed_root.join("repo/head-race-after-intent");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        let registered = named_record(
            "head-race-after-intent",
            repository.clone(),
            path.clone(),
            Lifecycle::Finished,
        );
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([(
                path.clone(),
                WorktreeSnapshot {
                    path: path.clone(),
                    head: "proof-head".into(),
                    dirty: false,
                    locked: false,
                },
            )])),
            discovered: Mutex::new(vec![DiscoveredWorktree {
                path: path.clone(),
                head: Some("proof-head".into()),
                locked: false,
                primary: false,
            }]),
            snapshot_sequence: Mutex::new(VecDeque::from([
                "proof-head".into(),
                "proof-head".into(),
                "proof-head".into(),
                "unpublished-head".into(),
            ])),
            ..fake_git(repository)
        };
        let manager =
            WorktreeManager::new(git, fake_registry(vec![registered.clone()]), FixedClock);

        let assessments = manager
            .gc(
                &policy(workspace, managed_root),
                std::slice::from_ref(&registered.id),
                true,
            )
            .unwrap();
        assert_eq!(
            assessments[0].refusal.as_ref().unwrap().code,
            "worktree-head-changed-after-intent"
        );
        assert!(path.exists());
        let intent = manager
            .registry()
            .removal(registered.id.as_str())
            .unwrap()
            .unwrap();
        assert_eq!(intent.head, "proof-head");
    }

    #[test]
    fn external_retirement_requires_separate_explicit_confirmation() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let external = temporary.path().join("legacy-external");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir(&external).unwrap();
        let registered = named_record(
            "legacy-external",
            repository.clone(),
            external.clone(),
            Lifecycle::Finished,
        );
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([(
                external.clone(),
                WorktreeSnapshot {
                    path: external.clone(),
                    head: "abc".into(),
                    dirty: false,
                    locked: false,
                },
            )])),
            discovered: Mutex::new(vec![DiscoveredWorktree {
                path: external.clone(),
                head: Some("abc".into()),
                locked: false,
                primary: false,
            }]),
            ..fake_git(repository)
        };
        let manager =
            WorktreeManager::new(git, fake_registry(vec![registered.clone()]), FixedClock);
        let policy = policy(workspace, managed_root);

        assert_eq!(
            manager
                .reconcile(&policy, std::slice::from_ref(&registered.id), true, false,)
                .unwrap_err()
                .code,
            "external-retirement-confirmation-required"
        );
        assert!(external.exists());

        let assessments = manager
            .reconcile(&policy, std::slice::from_ref(&registered.id), true, true)
            .unwrap();
        assert_eq!(
            assessments[0].evidence.as_ref().unwrap().operation,
            "retire-external"
        );
        assert!(!external.exists());
    }

    #[test]
    fn finished_external_source_supersedes_stale_relocation_on_retirement() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let external = temporary.path().join("legacy-external");
        let destination = managed_root.join("repo/legacy-external");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir(&external).unwrap();
        let registered = named_record(
            "legacy-external",
            repository.clone(),
            external.clone(),
            Lifecycle::Finished,
        );
        let relocation = RelocationIntent {
            id: registered.id.clone(),
            from: external.clone(),
            to: destination,
            head: "abc".into(),
            planned_at: 2,
        };
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([(
                external.clone(),
                WorktreeSnapshot {
                    path: external.clone(),
                    head: "abc".into(),
                    dirty: false,
                    locked: false,
                },
            )])),
            discovered: Mutex::new(vec![DiscoveredWorktree {
                path: external.clone(),
                head: Some("abc".into()),
                locked: false,
                primary: false,
            }]),
            ..fake_git(repository)
        };
        let registry = FakeRegistry {
            records: Mutex::new(vec![registered.clone()]),
            relocations: Mutex::new(BTreeMap::from([(registered.id.to_string(), relocation)])),
            removals: Mutex::new(BTreeMap::new()),
            live_leases: 0,
        };
        let manager = WorktreeManager::new(git, registry, FixedClock);
        let policy = policy(workspace, managed_root);

        let dry_run = manager
            .reconcile(&policy, std::slice::from_ref(&registered.id), false, false)
            .unwrap();
        assert!(matches!(
            dry_run[0].action,
            ReconciliationAction::RetireExternal { .. }
        ));
        assert!(dry_run[0].eligible);

        assert_eq!(
            manager
                .reconcile(&policy, std::slice::from_ref(&registered.id), true, false,)
                .unwrap_err()
                .code,
            "external-retirement-confirmation-required"
        );
        assert!(external.exists());
        assert!(
            manager
                .registry()
                .relocation(registered.id.as_str())
                .unwrap()
                .is_some()
        );

        let applied = manager
            .reconcile(&policy, std::slice::from_ref(&registered.id), true, true)
            .unwrap();
        assert_eq!(
            applied[0].evidence.as_ref().unwrap().operation,
            "retire-external"
        );
        assert!(!external.exists());
        assert!(
            manager
                .registry()
                .relocation(registered.id.as_str())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn finished_stale_relocation_refuses_ambiguous_or_destination_only_topology() {
        for (source_exists, expected_refusal) in [
            (true, "ambiguous-relocation"),
            (false, "invalid-relocation-lifecycle"),
        ] {
            let temporary = tempdir().unwrap();
            let workspace = temporary.path().join("workspace");
            let repository = workspace.join("repo");
            let managed_root = temporary.path().join("managed");
            let external = temporary.path().join("legacy-external");
            let destination = managed_root.join("repo/legacy-external");
            std::fs::create_dir_all(&repository).unwrap();
            let registered = named_record(
                "legacy-external",
                repository.clone(),
                external.clone(),
                Lifecycle::Finished,
            );
            let relocation = RelocationIntent {
                id: registered.id.clone(),
                from: external.clone(),
                to: destination.clone(),
                head: "abc".into(),
                planned_at: 2,
            };
            let mut discovered = vec![DiscoveredWorktree {
                path: destination,
                head: Some("abc".into()),
                locked: false,
                primary: false,
            }];
            if source_exists {
                std::fs::create_dir(&external).unwrap();
                discovered.push(DiscoveredWorktree {
                    path: external,
                    head: Some("abc".into()),
                    locked: false,
                    primary: false,
                });
            }
            let git = FakeGit {
                discovered: Mutex::new(discovered),
                ..fake_git(repository)
            };
            let registry = FakeRegistry {
                records: Mutex::new(vec![registered.clone()]),
                relocations: Mutex::new(BTreeMap::from([(registered.id.to_string(), relocation)])),
                removals: Mutex::new(BTreeMap::new()),
                live_leases: 0,
            };
            let manager = WorktreeManager::new(git, registry, FixedClock);

            let assessments = manager
                .reconcile(
                    &policy(workspace, managed_root),
                    std::slice::from_ref(&registered.id),
                    false,
                    false,
                )
                .unwrap();
            assert!(!assessments[0].eligible);
            assert_eq!(
                assessments[0].refusal.as_ref().unwrap().code,
                expected_refusal
            );
        }
    }

    #[test]
    fn finished_stale_relocation_refuses_an_unlisted_destination_path() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let external = temporary.path().join("legacy-external");
        let destination = managed_root.join("repo/legacy-external");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir(&external).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        let registered = named_record(
            "legacy-external",
            repository.clone(),
            external.clone(),
            Lifecycle::Finished,
        );
        let relocation = RelocationIntent {
            id: registered.id.clone(),
            from: external.clone(),
            to: destination,
            head: "abc".into(),
            planned_at: 2,
        };
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([(
                external.clone(),
                WorktreeSnapshot {
                    path: external.clone(),
                    head: "abc".into(),
                    dirty: false,
                    locked: false,
                },
            )])),
            discovered: Mutex::new(vec![DiscoveredWorktree {
                path: external,
                head: Some("abc".into()),
                locked: false,
                primary: false,
            }]),
            ..fake_git(repository)
        };
        let registry = FakeRegistry {
            records: Mutex::new(vec![registered.clone()]),
            relocations: Mutex::new(BTreeMap::from([(registered.id.to_string(), relocation)])),
            removals: Mutex::new(BTreeMap::new()),
            live_leases: 0,
        };
        let manager = WorktreeManager::new(git, registry, FixedClock);

        let assessments = manager
            .reconcile(
                &policy(workspace, managed_root),
                std::slice::from_ref(&registered.id),
                false,
                false,
            )
            .unwrap();
        assert!(matches!(
            assessments[0].action,
            ReconciliationAction::RetireExternal { .. }
        ));
        assert!(!assessments[0].eligible);
        assert_eq!(
            assessments[0].refusal.as_ref().unwrap().code,
            "relocation-destination-exists"
        );
    }

    #[test]
    fn finished_stale_relocation_requires_matching_record_intent_and_source_heads() {
        for (record_head, intent_head, source_head) in [
            ("different", "abc", "abc"),
            ("abc", "different", "abc"),
            ("abc", "abc", "different"),
        ] {
            let temporary = tempdir().unwrap();
            let workspace = temporary.path().join("workspace");
            let repository = workspace.join("repo");
            let managed_root = temporary.path().join("managed");
            let external = temporary.path().join("legacy-external");
            let destination = managed_root.join("repo/legacy-external");
            std::fs::create_dir_all(&repository).unwrap();
            std::fs::create_dir(&external).unwrap();
            let mut registered = named_record(
                "legacy-external",
                repository.clone(),
                external.clone(),
                Lifecycle::Finished,
            );
            registered.head = Some(record_head.into());
            let relocation = RelocationIntent {
                id: registered.id.clone(),
                from: external.clone(),
                to: destination,
                head: intent_head.into(),
                planned_at: 2,
            };
            let git = FakeGit {
                snapshots: Mutex::new(BTreeMap::from([(
                    external.clone(),
                    WorktreeSnapshot {
                        path: external.clone(),
                        head: source_head.into(),
                        dirty: false,
                        locked: false,
                    },
                )])),
                discovered: Mutex::new(vec![DiscoveredWorktree {
                    path: external,
                    head: Some(source_head.into()),
                    locked: false,
                    primary: false,
                }]),
                ..fake_git(repository)
            };
            let registry = FakeRegistry {
                records: Mutex::new(vec![registered.clone()]),
                relocations: Mutex::new(BTreeMap::from([(registered.id.to_string(), relocation)])),
                removals: Mutex::new(BTreeMap::new()),
                live_leases: 0,
            };
            let manager = WorktreeManager::new(git, registry, FixedClock);

            let assessments = manager
                .reconcile(
                    &policy(workspace, managed_root),
                    std::slice::from_ref(&registered.id),
                    false,
                    false,
                )
                .unwrap();
            assert!(!assessments[0].eligible);
            assert_eq!(
                assessments[0].refusal.as_ref().unwrap().code,
                "relocation-head-changed"
            );
        }
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn late_relocation_destination_refuses_removal_and_preserves_both_intents() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let external = temporary.path().join("legacy-external");
        let destination = managed_root.join("repo/legacy-external");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir(&external).unwrap();
        let registered = named_record(
            "legacy-external",
            repository.clone(),
            external.clone(),
            Lifecycle::Finished,
        );
        let relocation = stale_relocation(&registered, &destination);
        let source = discovered_worktree(&external, "abc");
        let target = discovered_worktree(&destination, "abc");
        let source_only = vec![source.clone()];
        let git = FakeGit {
            snapshots: Mutex::new(BTreeMap::from([(
                external.clone(),
                clean_snapshot(&external, "abc"),
            )])),
            discovered: Mutex::new(source_only.clone()),
            discovered_sequence: Mutex::new(VecDeque::from([
                source_only.clone(),
                source_only.clone(),
                source_only.clone(),
                source_only.clone(),
                vec![source, target.clone()],
            ])),
            ..fake_git(repository)
        };
        let registry = FakeRegistry {
            records: Mutex::new(vec![registered.clone()]),
            relocations: Mutex::new(BTreeMap::from([(
                registered.id.to_string(),
                relocation.clone(),
            )])),
            removals: Mutex::new(BTreeMap::new()),
            live_leases: 0,
        };
        let manager = WorktreeManager::new(git, registry, FixedClock);
        let policy = policy(workspace, managed_root);

        let applied = manager
            .reconcile(&policy, std::slice::from_ref(&registered.id), true, true)
            .unwrap();
        assert!(!applied[0].eligible);
        assert_eq!(
            applied[0].refusal.as_ref().unwrap().code,
            "relocation-destination-exists"
        );
        assert!(external.exists());
        assert_eq!(
            manager
                .registry()
                .relocation(registered.id.as_str())
                .unwrap(),
            Some(relocation.clone())
        );
        assert!(
            manager
                .registry()
                .removal(registered.id.as_str())
                .unwrap()
                .is_some()
        );

        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        std::fs::rename(&external, &destination).unwrap();
        let mut snapshots = manager.git.snapshots.lock().unwrap();
        let mut moved = snapshots.remove(&external).unwrap();
        moved.path.clone_from(&destination);
        snapshots.insert(destination.clone(), moved);
        drop(snapshots);
        *manager.git.discovered.lock().unwrap() = vec![target];

        let retry = manager
            .reconcile(&policy, std::slice::from_ref(&registered.id), false, false)
            .unwrap();
        assert!(!retry[0].eligible);
        assert_eq!(
            retry[0].refusal.as_ref().unwrap().code,
            "relocation-destination-exists"
        );
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Finished
        );
        assert!(
            manager
                .registry()
                .relocation(registered.id.as_str())
                .unwrap()
                .is_some()
        );
        assert!(
            manager
                .registry()
                .removal(registered.id.as_str())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn interrupted_external_removal_completes_only_when_both_paths_are_absent() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let external = temporary.path().join("legacy-external");
        let destination = managed_root.join("repo/legacy-external");
        std::fs::create_dir_all(&repository).unwrap();
        let registered = named_record(
            "legacy-external",
            repository.clone(),
            external,
            Lifecycle::Finished,
        );
        let relocation = stale_relocation(&registered, &destination);
        let removal = retirement_removal(&registered);
        let registry = FakeRegistry {
            records: Mutex::new(vec![registered.clone()]),
            relocations: Mutex::new(BTreeMap::from([(registered.id.to_string(), relocation)])),
            removals: Mutex::new(BTreeMap::from([(registered.id.to_string(), removal)])),
            live_leases: 0,
        };
        let manager = WorktreeManager::new(fake_git(repository), registry, FixedClock);

        let assessments = manager
            .reconcile(
                &policy(workspace, managed_root),
                std::slice::from_ref(&registered.id),
                true,
                false,
            )
            .unwrap();
        assert!(assessments[0].eligible);
        assert_eq!(
            assessments[0].evidence.as_ref().unwrap().operation,
            "reconcile-missing"
        );
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Removed
        );
        assert!(
            manager
                .registry()
                .relocation(registered.id.as_str())
                .unwrap()
                .is_none()
        );
        assert!(
            manager
                .registry()
                .removal(registered.id.as_str())
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn interrupted_external_removal_refuses_an_unlisted_destination_on_disk() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let external = temporary.path().join("legacy-external");
        let destination = managed_root.join("repo/legacy-external");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        let registered = named_record(
            "legacy-external",
            repository.clone(),
            external,
            Lifecycle::Finished,
        );
        let relocation = stale_relocation(&registered, &destination);
        let removal = retirement_removal(&registered);
        let registry = FakeRegistry {
            records: Mutex::new(vec![registered.clone()]),
            relocations: Mutex::new(BTreeMap::from([(registered.id.to_string(), relocation)])),
            removals: Mutex::new(BTreeMap::from([(registered.id.to_string(), removal)])),
            live_leases: 0,
        };
        let manager = WorktreeManager::new(fake_git(repository), registry, FixedClock);

        let assessments = manager
            .reconcile(
                &policy(workspace, managed_root),
                std::slice::from_ref(&registered.id),
                false,
                false,
            )
            .unwrap();
        assert!(!assessments[0].eligible);
        assert_eq!(
            assessments[0].refusal.as_ref().unwrap().code,
            "relocation-destination-exists"
        );
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Finished
        );
        assert!(
            manager
                .registry()
                .relocation(registered.id.as_str())
                .unwrap()
                .is_some()
        );
        assert!(
            manager
                .registry()
                .removal(registered.id.as_str())
                .unwrap()
                .is_some()
        );
    }
}
