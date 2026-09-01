//! Embeddable lifecycle service. The policy engine depends only on injected ports.

use b10x_worktree_domain::{
    CleanupAssessment, CreatePlan, CreateRequest, DiscoveredWorktree, Lifecycle, OperationEvidence,
    RecoveryProof, Refusal, RepositorySnapshot, WorkspacePolicy, WorktreeRecord, WorktreeSnapshot,
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
