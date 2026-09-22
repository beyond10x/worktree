//! Read-only inspection orchestration through the public facade.

use crate::{
    Clock, GitPort, RegistryPort, WorktreeManager, require_canonical_child,
    require_canonical_policy, require_clean_unlocked, require_exact_snapshot_path,
    validate_selected_records,
};
use b10x_worktree_domain::{
    INSPECTION_FORMAT, InspectedRecovery, InspectionDetails, InspectionReport, Lifecycle, Refusal,
    WorkspacePolicy, WorktreeId, WorktreeInspection, WorktreeRecord,
};
use std::path::Path;

/// Optional observation capability, separate from the lifecycle mutation port.
pub trait InspectionPort: Send + Sync {
    /// Observe current Git status and bounded checkout storage without modifying either.
    fn inspection_details(
        &self,
        repository: &Path,
        worktree: &Path,
        max_entries: u64,
    ) -> Result<InspectionDetails, Refusal>;
}

impl<G: GitPort + InspectionPort, R: RegistryPort, C: Clock> WorktreeManager<G, R, C> {
    /// Inspect non-removed records, scoped to one repository unless explicitly expanded.
    ///
    /// `refresh` permits the Git recovery adapter to fetch missing objects. It never changes
    /// lifecycle or leases. A report is not an approved GC plan or proof of owner abandonment.
    pub fn inspect(
        &self,
        policy: &WorkspacePolicy,
        repository: &Path,
        workspace: bool,
        ids: &[WorktreeId],
        refresh: bool,
        max_entries: u64,
    ) -> Result<InspectionReport, Refusal> {
        require_canonical_policy(policy)?;
        if max_entries == 0 {
            return Err(Refusal::new(
                "invalid-inspection-limit",
                "max entries must be greater than zero",
            ));
        }
        let repository = self.git.repository_snapshot(repository)?.root;
        if !repository.starts_with(&policy.workspace_root) {
            return Err(Refusal::new(
                "repository-outside-workspace",
                "inspection repository is outside the selected workspace",
            ));
        }
        let records = self.registry.list()?;
        validate_selected_records(policy, &records, ids)?;
        if !workspace
            && records
                .iter()
                .any(|record| ids.contains(&record.id) && record.repository_root != repository)
        {
            return Err(Refusal::new(
                "inspection-id-outside-repository",
                "selected id belongs to another repository; use --workspace to expand scope explicitly",
            ));
        }
        let now = self.clock.now();
        let mut inspections = records
            .into_iter()
            .filter(|record| {
                record.lifecycle != Lifecycle::Removed
                    && record.repository_root.starts_with(&policy.workspace_root)
                    && (workspace || record.repository_root == repository)
                    && (ids.is_empty() || ids.contains(&record.id))
            })
            .map(|record| self.inspect_record(policy, record, now, refresh, max_entries))
            .collect::<Vec<_>>();
        inspections.sort_by(|a, b| {
            let size = |item: &WorktreeInspection| {
                item.details.as_ref().map_or(0, |d| {
                    d.storage.allocated_bytes.unwrap_or(d.storage.logical_bytes)
                })
            };
            size(b)
                .cmp(&size(a))
                .then_with(|| a.record.id.cmp(&b.record.id))
        });
        Ok(InspectionReport {
            format: INSPECTION_FORMAT.into(),
            repository_root: repository,
            workspace,
            inspections,
        })
    }

    fn inspect_record(
        &self,
        policy: &WorkspacePolicy,
        record: WorktreeRecord,
        now: i64,
        refresh: bool,
        max_entries: u64,
    ) -> WorktreeInspection {
        let elapsed = now.saturating_sub(record.last_seen_at).max(0);
        let candidate = record.lifecycle == Lifecycle::Finished
            || (record.lifecycle == Lifecycle::Active && elapsed >= policy.expire_after_seconds);
        let mut report = WorktreeInspection {
            record,
            observed_at: now,
            seconds_since_recorded_activity: elapsed,
            live_leases: None,
            details: None,
            recorded_head_differs: None,
            cleanup_candidate: candidate,
            recovery: InspectedRecovery::NotChecked,
            blockers: Vec::new(),
            work_item_status:
                "unknown: registry has no explicit work-item ownership or completion evidence"
                    .into(),
        };
        if !candidate {
            report.blockers.push(Refusal::new(
                "lifecycle-retained",
                "not finished or expired; owner review and explicit finish are required",
            ));
        }
        match self.registry.live_lease_count(
            report.record.id.as_str(),
            now,
            self.lease_timeout_seconds,
        ) {
            Ok(count) => {
                report.live_leases = Some(count);
                if count > 0 {
                    report.blockers.push(Refusal::new(
                        "live-session-lease",
                        "a current session lease blocks cleanup",
                    ));
                }
            }
            Err(error) => report.blockers.push(error),
        }
        // Do not walk an alias, replaced path or uncontained legacy tree while estimating storage.
        if let Err(error) = require_canonical_child(&policy.worktree_root, &report.record.path) {
            report.blockers.push(error);
            return report;
        }
        let details = self
            .git
            .inspection_details(
                &report.record.repository_root,
                &report.record.path,
                max_entries,
            )
            .and_then(|details| {
                require_exact_snapshot_path(&details.snapshot, &report.record.path)?;
                Ok(details)
            });
        let details = match details {
            Ok(details) => details,
            Err(error) => {
                report.blockers.push(error);
                return report;
            }
        };
        if let Err(error) = require_clean_unlocked(&details.snapshot) {
            report.blockers.push(error);
        }
        if !details.storage.complete {
            report.blockers.push(Refusal::new(
                "storage-observation-partial",
                "size is a lower bound; inspect the storage errors before reviewing cleanup",
            ));
        }
        report.recorded_head_differs = report
            .record
            .head
            .as_ref()
            .map(|head| head != &details.snapshot.head);
        if refresh {
            report.recovery = self.inspect_recovery(&report.record, &details.snapshot.head);
            match &report.recovery {
                InspectedRecovery::Unproven => report.blockers.push(Refusal::new(
                    "no-remote-recovery-proof",
                    "observed HEAD is neither reachable from nor patch-equivalent to an advertised remote ref",
                )),
                InspectedRecovery::Unavailable { refusal } => report.blockers.push(refusal.clone()),
                _ => {}
            }
        } else {
            report.blockers.push(Refusal::new(
                "recovery-not-checked",
                "use --refresh for current remote-ref evidence; GC will revalidate before removal",
            ));
        }
        report.details = Some(details);
        report
    }

    fn inspect_recovery(&self, record: &WorktreeRecord, head: &str) -> InspectedRecovery {
        let result = self
            .git
            .recovery_evidence(&record.repository_root, head)
            .and_then(|evidence| {
                let after = self.exact_worktree_snapshot(&record.repository_root, &record.path)?;
                if after.head != head {
                    return Err(Refusal::new(
                        "inspection-head-changed",
                        "HEAD changed while remote recovery evidence was being observed",
                    ));
                }
                Ok(evidence)
            });
        match result {
            Ok(evidence) if evidence.refs.is_empty() => InspectedRecovery::Unproven,
            Ok(evidence) => InspectedRecovery::Proven {
                kind: evidence.kind,
                refs: evidence.refs,
                equivalent_commits: evidence.equivalent_commits,
            },
            Err(refusal) => InspectedRecovery::Unavailable { refusal },
        }
    }
}
