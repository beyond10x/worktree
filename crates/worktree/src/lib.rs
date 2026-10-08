//! Embeddable lifecycle service. The policy engine depends only on injected ports.

use b10x_worktree_domain::{
    ArchiveContents, ArchiveEvidence, ArchiveManifest, ArchivePruneAssessment, ArchivePruneReport,
    ArchiveReference, ArchiveRequest, ArchiveStateCheck, CacheClassification, CacheDiscard,
    CleanupAssessment, CreatePlan, CreateRequest, DiscoveredWorktree, GitRevision, Lifecycle,
    OperationEvidence, PruneVerdict, ReconciliationAction, ReconciliationAssessment,
    RecoveryEvidence, RecoveryKind, RecoveryProof, Refusal, RelocationIntent, RemoteObservation,
    RemovalIntent, RepositorySnapshot, SkippedArchiveEntry, SweepItem, WorkspacePolicy, WorktreeId,
    WorktreeRecord, WorktreeSnapshot, decide_archive_prune, require_child,
};
use std::path::{Path, PathBuf};

mod inspection;
pub use inspection::InspectionPort;

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
    /// Refresh advertisements and return the evidence that the commit's work is recoverable.
    ///
    /// Ancestry comes first. An adapter may additionally prove that every commit held by no
    /// advertised ref has a patch-identical commit on one advertised ref. The default reports
    /// ancestry only.
    fn recovery_evidence(
        &self,
        repository: &Path,
        head: &str,
    ) -> Result<RecoveryEvidence, Refusal> {
        self.recovery_refs(repository, head)
            .map(RecoveryEvidence::ancestor)
    }
    /// Return every local ref - branch, tag, or remote-tracking - that contains the commit.
    ///
    /// Reports `None` when this repository holds no such commit object at all. It reads only
    /// local state, so it answers "is anything still pointing at this?" rather than proving
    /// recovery; pair it with [`GitPort::recovery_refs`] when fresh remote evidence is required.
    fn containing_refs(
        &self,
        repository: &Path,
        head: &str,
    ) -> Result<Option<Vec<String>>, Refusal>;
    /// Remove a linked worktree without forcing Git.
    fn remove(&self, repository: &Path, worktree: &Path) -> Result<(), Refusal>;
    /// Prove that what an interrupted removal left behind is only the recorded commit's content.
    ///
    /// Git has already unlinked the path, so the tree can no longer be asked for its status.
    /// Every remaining file must match the commit's tracked content. The default refuses.
    fn verify_removal_residue(
        &self,
        _repository: &Path,
        worktree: &Path,
        _head: &str,
    ) -> Result<(), Refusal> {
        Err(Refusal::new(
            "removal-residue-unproven",
            format!(
                "this Git adapter cannot verify the residue at {}",
                worktree.display()
            ),
        ))
    }
    /// Delete the verified residue of an interrupted removal. The default refuses.
    fn delete_residue(&self, worktree: &Path) -> Result<(), Refusal> {
        Err(Refusal::new(
            "removal-residue-unproven",
            format!(
                "this Git adapter cannot delete the residue at {}",
                worktree.display()
            ),
        ))
    }
    /// Move a linked worktree without forcing Git.
    fn move_worktree(&self, repository: &Path, from: &Path, to: &Path) -> Result<(), Refusal>;
    /// Refuse a move that the platform cannot perform atomically.
    fn validate_move_worktree(&self, from: &Path, to: &Path) -> Result<(), Refusal>;
    /// Discover all linked worktrees known to Git.
    fn list_worktrees(&self, repository: &Path) -> Result<Vec<DiscoveredWorktree>, Refusal>;
    /// Write and verify an archive of one tree without modifying the tree. The default refuses.
    ///
    /// The archive holds every commit HEAD adds over the refs currently advertised by the
    /// configured remotes and, for a dirty tree, its complete uncommitted state. It is published
    /// at `request.destination` only after it has been verified.
    fn write_archive(&self, request: &ArchiveRequest<'_>) -> Result<ArchiveEvidence, Refusal> {
        Err(archive_unsupported(request.destination))
    }
    /// Verify an archive as recovery proof for `head`. The default refuses.
    ///
    /// Digests, `git bundle verify`, the bundle's own objects against every commit HEAD adds over
    /// freshly advertised refs, and for a dirty tree its exact current content must all agree.
    fn verify_archive(
        &self,
        _record: &WorktreeRecord,
        archive: &Path,
        _head: &str,
        _state: ArchiveStateCheck<'_>,
    ) -> Result<ArchiveReference, Refusal> {
        Err(archive_unsupported(archive))
    }
    /// Verify, without contacting a remote, that a dirty tree's content equals the archived
    /// state for `head`. The default refuses.
    fn verify_archived_state(
        &self,
        _record: &WorktreeRecord,
        archive: &Path,
        _head: &str,
    ) -> Result<(), Refusal> {
        Err(archive_unsupported(archive))
    }
    /// Return a verified archived tree to its clean HEAD so it can be removed without force.
    ///
    /// Every file removed or restored must match the archive at the moment it is discarded. The
    /// default refuses.
    fn discard_archived_state(
        &self,
        _record: &WorktreeRecord,
        archive: &Path,
        _head: &str,
    ) -> Result<(), Refusal> {
        Err(archive_unsupported(archive))
    }
    /// Refuse state that a removal would lose although Git status reports nothing: entries
    /// marked assume-unchanged or skip-worktree, staged content that differs from both HEAD and
    /// the working copy, files below a nested `.git`, and per-worktree refs.
    ///
    /// Neither remote refs nor an archive cover this state, with one exception: `archive` names
    /// the archive the caller verified in the same flow, and a nested `.git` below a repository
    /// root that archive images, verified against the tree again here, is not refused. The
    /// default refuses, because an adapter that cannot look has not shown that nothing is there.
    fn hidden_state(
        &self,
        _repository: &Path,
        worktree: &Path,
        _archive: Option<&Path>,
    ) -> Result<(), Refusal> {
        Err(Refusal::new(
            "hidden-state-unobserved",
            format!(
                "this Git adapter cannot observe state that Git status hides in {}",
                worktree.display()
            ),
        ))
    }
    /// Classify a tree's ignored entries into recognised build cache and everything else and, with
    /// `apply`, delete the cache. The default refuses.
    ///
    /// Recognition follows [`b10x_worktree_domain::CacheKind`]; an ignored entry no rule
    /// recognises is retained and reported, never deleted. An applying adapter refuses as
    /// `worktree-in-use` while a process other than the caller and its ancestors uses the tree.
    fn discard_cache(
        &self,
        _repository: &Path,
        worktree: &Path,
        _apply: bool,
    ) -> Result<CacheClassification, Refusal> {
        Err(Refusal::new(
            "cache-discard-unsupported",
            format!(
                "this Git adapter cannot classify the build cache in {}",
                worktree.display()
            ),
        ))
    }
    /// The latest Git activity observed in the tree itself, in Unix seconds, or `None` when the
    /// adapter cannot tell. A sweep counts idle time from the later of this and the record's own
    /// activity. The default observes nothing.
    fn last_activity(&self, _worktree: &Path) -> Option<i64> {
        None
    }
    /// Whether the repository recorded at this root is gone: nothing exists at the path, or a
    /// directory is there with no `.git` of its own.
    ///
    /// `true` is a positive observation that no repository remains to ask. Anything else,
    /// including a path this adapter cannot inspect, is `false`, so the record keeps being
    /// assessed through Git. The default never observes an absent repository.
    fn repository_absent(&self, _repository: &Path) -> Result<bool, Refusal> {
        Ok(false)
    }
    /// Refresh advertisements and return which of `commits` no freshly advertised ref holds by
    /// ancestry, using the same advertised-ref observation the removal proof uses. Patch
    /// equivalence never counts. Refuses when no remote is configured or a remote does not
    /// answer. The default refuses.
    fn commits_not_on_remote(
        &self,
        repository: &Path,
        _commits: &[String],
    ) -> Result<Vec<String>, Refusal> {
        Err(Refusal::new(
            "remote-proof-unsupported",
            format!(
                "this Git adapter cannot observe the remotes of {}",
                repository.display()
            ),
        ))
    }
    /// Observe one archive directory: its parsed manifest and every entry directly in it, links
    /// not followed. The default refuses.
    fn read_archive_contents(&self, archive: &Path) -> Result<ArchiveContents, Refusal> {
        Err(archive_unsupported(archive))
    }
    /// Delete an archive assessed removable: each file `manifest` names, then the manifest, then
    /// the empty directory, never recursively. The manifest on disk must still be `manifest`, and
    /// an entry that appeared since refuses and is kept. Returns the bytes deleted. The default
    /// refuses.
    fn delete_archive(&self, archive: &Path, _manifest: &ArchiveManifest) -> Result<u64, Refusal> {
        Err(archive_unsupported(archive))
    }
}

fn archive_unsupported(archive: &Path) -> Refusal {
    Refusal::new(
        "archive-unsupported",
        format!(
            "this Git adapter cannot write or verify the archive at {}",
            archive.display()
        ),
    )
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

/// How one tree reference named its registered record (`worktree.selection.ReferenceForm`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReferenceForm {
    /// The value is a registered worktree id.
    Id,
    /// The value names a directory whose canonical path is a registered record's path.
    Path,
    /// The value contains no `/` and is the final path component of exactly one in-scope,
    /// non-removed record's path.
    DirectoryName,
}

/// Which registered records `gc` assesses when no id is selected
/// (`worktree.selection.CleanupScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CleanupScope {
    /// Records whose repository root is the repository the caller resolved.
    Repository,
    /// Every record below the selected profile's workspace root.
    Profile,
}

/// One resolved tree reference (`worktree.selection.TreeSelection`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TreeSelection {
    /// The value as the caller gave it.
    pub reference: String,
    /// The form that named the record; `Id` before `Path` before `DirectoryName` when several
    /// forms name the same record.
    pub form: ReferenceForm,
    /// The registered record's id.
    pub worktree_id: WorktreeId,
    /// The registered record's path.
    pub path: PathBuf,
}

/// Resolve `reference` against `records`. `canonical` is the canonical path of the directory
/// the value names, if any; `scope` limits the records a directory name may match. `None` when
/// no form names a record; the only refusal is `ambiguous-worktree-reference`.
fn select_reference(
    records: &[WorktreeRecord],
    reference: &str,
    canonical: Option<&Path>,
    scope: Option<&WorkspacePolicy>,
) -> Result<Option<TreeSelection>, Refusal> {
    let live = |record: &&WorktreeRecord| record.lifecycle != Lifecycle::Removed;
    let mut matches: Vec<(ReferenceForm, &WorktreeRecord)> = Vec::new();
    if let Ok(id) = WorktreeId::new(reference) {
        matches.extend(
            records
                .iter()
                .find(|record| record.id == id)
                .map(|record| (ReferenceForm::Id, record)),
        );
    }
    if let Some(canonical) = canonical {
        matches.extend(
            records
                .iter()
                .filter(live)
                .find(|record| record.path == canonical)
                .map(|record| (ReferenceForm::Path, record)),
        );
    }
    if !reference.is_empty() && !reference.contains('/') && reference != "." && reference != ".." {
        matches.extend(
            records
                .iter()
                .filter(live)
                .filter(|record| {
                    scope.is_none_or(|policy| {
                        record.repository_root.starts_with(&policy.workspace_root)
                    })
                })
                .filter(|record| {
                    record
                        .path
                        .file_name()
                        .is_some_and(|name| name == std::ffi::OsStr::new(reference))
                })
                .map(|record| (ReferenceForm::DirectoryName, record)),
        );
    }
    let mut ids: Vec<&str> = Vec::new();
    for (_, record) in &matches {
        if !ids.contains(&record.id.as_str()) {
            ids.push(record.id.as_str());
        }
    }
    match (matches.first(), ids.len()) {
        (Some((form, record)), 1) => Ok(Some(TreeSelection {
            reference: reference.to_owned(),
            form: *form,
            worktree_id: record.id.clone(),
            path: record.path.clone(),
        })),
        (None, _) => Ok(None),
        _ => Err(Refusal::new(
            "ambiguous-worktree-reference",
            format!(
                "`{reference}` names more than one registered worktree ({}); pass one of these ids",
                ids.join(", ")
            ),
        )),
    }
}

/// Policy-driven worktree lifecycle service suitable for embedding in Harness.
pub struct WorktreeManager<G, R, C> {
    git: G,
    registry: R,
    clock: C,
    lease_timeout_seconds: i64,
    archive_root: Option<PathBuf>,
}

impl<G, R, C> WorktreeManager<G, R, C>
where
    G: GitPort,
    R: RegistryPort,
    C: Clock,
{
    /// Construct a manager with a one-hour abandoned-session lease timeout and no archive root.
    pub fn new(git: G, registry: R, clock: C) -> Self {
        Self {
            git,
            registry,
            clock,
            lease_timeout_seconds: 3_600,
            archive_root: None,
        }
    }

    /// Keep tree archives below `root`, as `<root>/<repository name>/<id>/`.
    ///
    /// Without an archive root no archive is written or consulted, so every decision is exactly
    /// the one a manager without archives makes.
    #[must_use]
    pub fn with_archive_root(mut self, root: PathBuf) -> Self {
        self.archive_root = Some(root);
        self
    }

    /// Archive a managed tree's local-only commits and uncommitted state without modifying it.
    ///
    /// An existing archive is refused unless `replace` is set, in which case it is moved aside,
    /// never deleted.
    pub fn archive(&self, path: &Path, replace: bool) -> Result<ArchiveEvidence, Refusal> {
        let record = self.owned_record(path)?;
        if !matches!(record.lifecycle, Lifecycle::Active | Lifecycle::Finished) {
            return Err(Refusal::new(
                "not-archivable",
                "only an active or finished worktree can be archived",
            ));
        }
        let destination = self.archive_dir(&record)?.ok_or_else(|| {
            Refusal::new(
                "archive-root-unconfigured",
                "this manager has no archive root",
            )
        })?;
        let snapshot = self.exact_worktree_snapshot(&record.repository_root, &record.path)?;
        require_unlocked(&snapshot)?;
        if !replace && !path_absent(&destination)? {
            return Err(Refusal::new(
                "archive-exists",
                format!(
                    "{} already holds an archive; pass --replace to move it aside and write a new one",
                    destination.display()
                ),
            ));
        }
        let mut evidence = self.git.write_archive(&ArchiveRequest {
            record: &record,
            head: &snapshot.head,
            destination: &destination,
            replace,
            created_at: self.clock.now(),
        })?;
        // The archive is kept either way; the caller learns now what cleanup will still refuse.
        // It was verified as it was written, so the nested repositories it images are covered.
        evidence.blocker = self
            .git
            .hidden_state(&record.repository_root, &record.path, Some(&destination))
            .err();
        let after = self.exact_worktree_snapshot(&record.repository_root, &record.path)?;
        if after.head != snapshot.head {
            return Err(Refusal::new(
                "archive-stale",
                format!(
                    "HEAD moved while {} was written; rerun `worktree archive --replace`",
                    destination.display()
                ),
            ));
        }
        Ok(evidence)
    }

    /// The archive directory for a record, when an archive root is configured.
    fn archive_dir(&self, record: &WorktreeRecord) -> Result<Option<PathBuf>, Refusal> {
        let Some(root) = &self.archive_root else {
            return Ok(None);
        };
        let repository = record
            .repository_root
            .file_name()
            .filter(|name| !name.is_empty())
            .ok_or_else(|| {
                Refusal::new(
                    "invalid-repository-name",
                    record.repository_root.display().to_string(),
                )
            })?;
        Ok(Some(root.join(repository).join(record.id.as_str())))
    }

    /// The archive directory for a record, only when an archive is actually present there.
    fn existing_archive(&self, record: &WorktreeRecord) -> Result<Option<PathBuf>, Refusal> {
        match self.archive_dir(record)? {
            Some(dir) if !path_absent(&dir)? => Ok(Some(dir)),
            _ => Ok(None),
        }
    }

    /// Recovery proof from a verified archive.
    fn archive_proof(
        &self,
        record: &WorktreeRecord,
        archive: &Path,
        head: &str,
        state: ArchiveStateCheck<'_>,
        now: i64,
    ) -> Result<RecoveryProof, Refusal> {
        let reference = self.git.verify_archive(record, archive, head, state)?;
        Ok(RecoveryProof {
            head: head.to_owned(),
            refs: Vec::new(),
            observed_at: now,
            kind: RecoveryKind::Archive,
            equivalent_commits: Vec::new(),
            archive: Some(reference),
        })
    }

    /// Refuse a locked tree, and a dirty one unless an archive holds exactly its current state.
    ///
    /// `proof` names the archive a removal relies on; its fingerprint is then checked whether
    /// or not Git reports the tree dirty, because Git status can be told not to look. Without
    /// proof (finish, claim), the record's own archive is consulted only for a dirty tree.
    /// Remote-ref proof never covers uncommitted state.
    fn require_clean_or_archived(
        &self,
        record: &WorktreeRecord,
        snapshot: &WorktreeSnapshot,
        proof: Option<&RecoveryProof>,
    ) -> Result<(), Refusal> {
        require_unlocked(snapshot)?;
        if let Some(archive) = proof.and_then(relied_on_archive) {
            return self
                .git
                .verify_archived_state(record, archive, &snapshot.head);
        }
        if !snapshot.dirty {
            return Ok(());
        }
        let archive = match proof {
            Some(_) => None,
            None => self.existing_archive(record)?,
        };
        let Some(archive) = archive else {
            return Err(dirty_refusal());
        };
        self.git
            .verify_archived_state(record, &archive, &snapshot.head)
    }

    /// Assess every archive in scope and, with `apply`, delete the selected ones a remote fully
    /// holds (`worktree.archive.ArchivePruneAssessment`).
    ///
    /// Without `selected`, `scope` chooses the archives: [`CleanupScope::Repository`] those whose
    /// manifest names the canonical `repository`, [`CleanupScope::Profile`] every archive below
    /// the archive root. `selected` names archive directories as `<directory>` or
    /// `<repository name>/<directory>` whatever the scope; `apply` requires it. Each selected
    /// archive is assessed immediately before deletion and deleted only when
    /// [`b10x_worktree_domain::PruneVerdict::Removable`]; nothing makes a refusal removable.
    pub fn prune_archives(
        &self,
        repository: &Path,
        scope: CleanupScope,
        selected: &[String],
        apply: bool,
    ) -> Result<ArchivePruneReport, Refusal> {
        if apply && selected.is_empty() {
            return Err(Refusal::new(
                "explicit-archive-selection-required",
                "prune-archives --apply requires at least one archive directory reviewed in a \
                 dry-run, named with --id",
            ));
        }
        let root = self.archive_root.as_ref().ok_or_else(|| {
            Refusal::new(
                "archive-root-unconfigured",
                "this manager has no archive root",
            )
        })?;
        let repository = std::fs::canonicalize(repository).map_err(|error| {
            Refusal::new(
                "repository-not-found",
                format!("{}: {error}", repository.display()),
            )
        })?;
        let repository_name = repository.file_name().map(std::ffi::OsStr::to_os_string);
        let listing = list_archive_root(root)?;
        let skipped = listing
            .skipped
            .into_iter()
            .filter(|(owner, _)| match scope {
                CleanupScope::Profile => true,
                CleanupScope::Repository => {
                    owner.is_some() && owner.as_deref() == repository_name.as_deref()
                }
            })
            .map(|(_, entry)| entry)
            .collect::<Vec<_>>();

        let mut chosen: Vec<&ArchiveDirectory> = Vec::new();
        if selected.is_empty() {
            for directory in &listing.archives {
                let in_scope = match scope {
                    CleanupScope::Profile => true,
                    CleanupScope::Repository => self
                        .read_archive(&directory.path)
                        .manifest
                        .as_ref()
                        .is_ok_and(|manifest| manifest.repository_root == repository),
                };
                if in_scope {
                    chosen.push(directory);
                }
            }
        } else {
            for reference in selected {
                let directory = select_archive(&listing.archives, reference)?;
                if !chosen.iter().any(|item| item.path == directory.path) {
                    chosen.push(directory);
                }
            }
        }

        let mut archives = Vec::with_capacity(chosen.len());
        for directory in chosen {
            archives.push(self.assess_archive_prune(directory, apply)?);
        }
        Ok(ArchivePruneReport::new(apply, archives, skipped))
    }

    /// Assess one archive directory now and, with `apply`, delete it when removable.
    fn assess_archive_prune(
        &self,
        directory: &ArchiveDirectory,
        apply: bool,
    ) -> Result<ArchivePruneAssessment, Refusal> {
        let contents = self.read_archive(&directory.path);
        let decision = decide_archive_prune(
            &contents,
            |manifest| {
                path_absent(&manifest.path)
                    .map(|absent| !absent)
                    .map_err(|error| error.message)
            },
            |manifest| self.remote_observation(manifest),
        );
        let manifest = contents.manifest.as_ref().ok();
        let mut assessment = ArchivePruneAssessment {
            archive_directory: directory.name.clone(),
            path: directory.path.clone(),
            worktree_id: manifest.map(|manifest| manifest.id.as_str().to_owned()),
            repository_root: manifest.map(|manifest| manifest.repository_root.clone()),
            bytes: contents.bytes(),
            verdict: decision.verdict,
            reason: decision.reason,
            commits_not_on_remote: decision.commits_not_on_remote,
            removed: false,
        };
        if apply && assessment.verdict == PruneVerdict::Removable {
            let manifest = manifest.ok_or_else(|| {
                Refusal::new(
                    "archive-invalid",
                    "a removable archive has a manifest; refusing to delete without one",
                )
            })?;
            match self.git.delete_archive(&directory.path, manifest) {
                Ok(bytes) => {
                    assessment.bytes = bytes;
                    assessment.removed = true;
                }
                Err(refusal) => {
                    assessment.reason = format!("deletion refused: {refusal}");
                }
            }
        }
        Ok(assessment)
    }

    /// Observe one archive directory; a directory that cannot be read is an invalid manifest.
    fn read_archive(&self, archive: &Path) -> ArchiveContents {
        self.git
            .read_archive_contents(archive)
            .unwrap_or_else(|refusal| ArchiveContents {
                manifest: Err(refusal.to_string()),
                entries: Vec::new(),
            })
    }

    /// What the configured remotes of the manifest's repository hold of its recorded commits.
    fn remote_observation(&self, manifest: &ArchiveManifest) -> RemoteObservation {
        let repository = manifest.repository_root.as_path();
        match self.git.repository_absent(repository) {
            Ok(false) => {}
            Ok(true) => {
                return RemoteObservation::Unavailable(format!(
                    "repository {} is gone",
                    repository.display()
                ));
            }
            Err(refusal) => return RemoteObservation::Unavailable(refusal.to_string()),
        }
        match self
            .git
            .commits_not_on_remote(repository, &manifest.recorded_commits())
        {
            Ok(not_on_remote) => RemoteObservation::Observed { not_on_remote },
            Err(refusal) => RemoteObservation::Unavailable(refusal.to_string()),
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
        self.require_clean_or_archived(&record, &snapshot, None)?;
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

    /// Finish a tree after optionally discarding its recognised build cache and archiving what
    /// remains, so that nothing which is not cache is lost.
    ///
    /// With `archive`, an archive is written when the tree still differs from HEAD, or HEAD adds
    /// commits no advertised ref holds, and no existing archive already holds exactly this state.
    /// The final checks are those of [`Self::finish`].
    pub fn finish_with(
        &self,
        path: &Path,
        options: FinishOptions,
    ) -> Result<FinishEvidence, Refusal> {
        let record = self.owned_record(path)?;
        if record.lifecycle != Lifecycle::Active {
            return Err(Refusal::new(
                "not-active",
                "only an active worktree can be finished",
            ));
        }
        self.require_idle(&record, self.clock.now())?;
        let cache = if options.discard_cache {
            Some(self.discard_record_cache(&record, true)?)
        } else {
            None
        };
        let archive = if options.archive {
            self.archive_unless_recoverable(&record, path)?
        } else {
            None
        };
        if let Some(cache) = cache.as_ref().filter(|_| archive.is_none()) {
            let snapshot = self.exact_worktree_snapshot(&record.repository_root, &record.path)?;
            if snapshot.dirty && self.existing_archive(&record)?.is_none() {
                return Err(retained_refusal(cache));
            }
        }
        let evidence = self.finish(path)?;
        Ok(FinishEvidence {
            evidence,
            cache,
            archive,
        })
    }

    /// Classify an idle tree's ignored entries and, with `apply`, delete the recognised build
    /// cache. Every other ignored entry is retained and reported.
    ///
    /// A dry-run reads only. Applying is refused for a live lease, a Git lock, or a tree that is
    /// neither active nor finished.
    pub fn discard_cache(&self, path: &Path, apply: bool) -> Result<CacheDiscard, Refusal> {
        let record = self.owned_record(path)?;
        self.discard_record_cache(&record, apply)
    }

    fn discard_record_cache(
        &self,
        record: &WorktreeRecord,
        apply: bool,
    ) -> Result<CacheDiscard, Refusal> {
        if !matches!(record.lifecycle, Lifecycle::Active | Lifecycle::Finished) {
            return Err(Refusal::new(
                "not-discardable",
                "only an active or finished worktree's build cache can be discarded",
            ));
        }
        let now = self.clock.now();
        let snapshot = self.exact_worktree_snapshot(&record.repository_root, &record.path)?;
        if apply {
            self.require_idle(record, now)?;
            require_unlocked(&snapshot)?;
        }
        let classification =
            self.git
                .discard_cache(&record.repository_root, &record.path, apply)?;
        let after = self.exact_worktree_snapshot(&record.repository_root, &record.path)?;
        if after.head != snapshot.head {
            return Err(Refusal::new(
                "cache-discard-state-changed",
                "HEAD moved while the build cache was classified; inspect the tree and retry",
            ));
        }
        Ok(CacheDiscard {
            id: record.id.clone(),
            path: record.path.clone(),
            observed_at: now,
            applied: apply,
            discarded: classification.discarded,
            retained_ignored: classification.retained_ignored,
            retained_bytes: classification.retained_bytes,
            processes_observed: classification.processes_observed,
        })
    }

    /// Discard the recognised build cache of every idle tree in the workspace and archive what an
    /// expired or finished tree still holds, so that a reviewed GC can remove it without loss.
    ///
    /// A record is idle once neither the registry nor the tree's own Git state shows activity for
    /// `discard_after_seconds`, and expired once that reaches the policy's expiry. A sweep never
    /// changes lifecycle and never removes a tree. Each record's refusal is reported in its item
    /// and the sweep moves on; without `apply` nothing is deleted or written.
    pub fn sweep(
        &self,
        policy: &WorkspacePolicy,
        options: SweepOptions,
    ) -> Result<Vec<SweepItem>, Refusal> {
        require_canonical_policy(policy)?;
        let now = self.clock.now();
        let mut items = Vec::new();
        for record in self.registry.list()? {
            if !record.repository_root.starts_with(&policy.workspace_root)
                || !matches!(record.lifecycle, Lifecycle::Active | Lifecycle::Finished)
                || path_absent(&record.path)?
            {
                continue;
            }
            let activity = self
                .git
                .last_activity(&record.path)
                .map_or(record.last_seen_at, |seen| seen.max(record.last_seen_at));
            let idle_seconds = now.saturating_sub(activity);
            if idle_seconds < options.discard_after_seconds {
                continue;
            }
            let expired = record.lifecycle == Lifecycle::Finished
                || idle_seconds >= policy.expire_after_seconds;
            let mut item = SweepItem {
                record,
                idle_seconds,
                cache: None,
                archive: None,
                refusal: None,
            };
            match self.discard_record_cache(&item.record, options.apply) {
                Ok(cache) => item.cache = Some(cache),
                Err(refusal) => {
                    item.refusal = Some(refusal);
                    items.push(item);
                    continue;
                }
            }
            if expired && options.apply {
                let retained = item.cache.as_ref().map_or(0, |cache| cache.retained_bytes);
                if retained > options.max_archive_bytes {
                    item.refusal = Some(Refusal::new(
                        "archive-too-large",
                        format!(
                            "{retained} bytes of ignored files are not recognised cache; archive \
                             them deliberately with `worktree archive` or remove what is not needed"
                        ),
                    ));
                } else {
                    let path = item.record.path.clone();
                    match self.archive_unless_recoverable(&item.record, &path) {
                        Ok(archive) => item.archive = archive,
                        Err(refusal) => item.refusal = Some(refusal),
                    }
                }
            }
            items.push(item);
        }
        Ok(items)
    }

    /// Write an archive unless the tree is clean with HEAD on an advertised ref, or an existing
    /// archive already holds exactly the current state.
    fn archive_unless_recoverable(
        &self,
        record: &WorktreeRecord,
        path: &Path,
    ) -> Result<Option<ArchiveEvidence>, Refusal> {
        let snapshot = self.exact_worktree_snapshot(&record.repository_root, &record.path)?;
        if !snapshot.dirty {
            match self.recovery_proof(&record.repository_root, &snapshot.head, self.clock.now()) {
                Ok(_) => return Ok(None),
                Err(refusal) if refusal.code == NO_REMOTE_RECOVERY_PROOF => {}
                Err(refusal) => return Err(refusal),
            }
        }
        let existing = self.existing_archive(record)?;
        if let Some(dir) = &existing {
            if self
                .git
                .verify_archived_state(record, dir, &snapshot.head)
                .is_ok()
            {
                return Ok(None);
            }
        }
        self.archive(path, existing.is_some()).map(Some)
    }

    /// Assess cleanup candidates and optionally remove those with fresh recovery proof.
    ///
    /// Without selected ids this assesses every record below the policy's workspace root, as
    /// [`CleanupScope::Profile`] does in [`Self::gc_scoped`].
    pub fn gc(
        &self,
        policy: &WorkspacePolicy,
        selected_ids: &[WorktreeId],
        apply: bool,
    ) -> Result<Vec<CleanupAssessment>, Refusal> {
        require_canonical_policy(policy)?;
        self.gc_selected(policy, None, selected_ids, apply)
    }

    /// Assess cleanup candidates in `scope` and optionally remove those with fresh recovery proof.
    ///
    /// `repository` is the repository root the caller resolved; with
    /// [`CleanupScope::Repository`] only records whose `repository_root` equals its canonical
    /// path are assessed. Selected ids are assessed whatever the scope, still subject to the
    /// policy's workspace check.
    pub fn gc_scoped(
        &self,
        policy: &WorkspacePolicy,
        repository: &Path,
        scope: CleanupScope,
        selected_ids: &[WorktreeId],
        apply: bool,
    ) -> Result<Vec<CleanupAssessment>, Refusal> {
        require_canonical_policy(policy)?;
        let repository = match scope {
            CleanupScope::Profile => None,
            CleanupScope::Repository => {
                Some(std::fs::canonicalize(repository).map_err(|error| {
                    Refusal::new(
                        "repository-not-found",
                        format!("{}: {error}", repository.display()),
                    )
                })?)
            }
        };
        self.gc_selected(policy, repository.as_deref(), selected_ids, apply)
    }

    fn gc_selected(
        &self,
        policy: &WorkspacePolicy,
        repository: Option<&Path>,
        selected_ids: &[WorktreeId],
        apply: bool,
    ) -> Result<Vec<CleanupAssessment>, Refusal> {
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
                || (selected_ids.is_empty()
                    && repository.is_some_and(|root| record.repository_root != root))
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
                Ok(proof) if !apply => assessments.push(CleanupAssessment {
                    record,
                    eligible: true,
                    refusal: None,
                    evidence: None,
                    archive: proof.archive.map(|reference| reference.path),
                }),
                Ok(_) => assessments.push(self.apply_cleanup(policy, record)),
                Err(refusal) => assessments.push(CleanupAssessment {
                    record,
                    eligible: false,
                    refusal: Some(refusal),
                    evidence: None,
                    archive: None,
                }),
            }
        }
        Ok(assessments)
    }

    /// Claim, re-assess and remove one reviewed cleanup candidate.
    fn apply_cleanup(&self, policy: &WorkspacePolicy, record: WorktreeRecord) -> CleanupAssessment {
        let refused = |record, refusal| CleanupAssessment {
            record,
            eligible: false,
            refusal: Some(refusal),
            evidence: None,
            archive: None,
        };
        let claimed = match self.claim_cleanup(policy, record.clone()) {
            Ok(claimed) => claimed,
            Err(refusal) => return refused(record, refusal),
        };
        // Re-observe immediately before the only destructive call.
        let applied = self
            .assess_cleanup(policy, &claimed, self.clock.now())
            .and_then(|proof| match self.interrupted_removal(&claimed)? {
                Some(intent) => self.finish_interrupted_removal(&claimed, intent, proof),
                None => self.apply_removal(&claimed, &claimed.path, "remove", proof, None),
            });
        match applied {
            Ok(evidence) => CleanupAssessment {
                record: claimed,
                eligible: true,
                refusal: None,
                archive: evidence
                    .recovery
                    .as_ref()
                    .and_then(|proof| proof.archive.as_ref())
                    .map(|reference| reference.path.clone()),
                evidence: Some(evidence),
            },
            Err(refusal) => refused(claimed, refusal),
        }
    }

    /// Reconcile adopted legacy paths and registry records whose worktrees are already absent.
    ///
    /// An empty selection assesses every candidate in the activated workspace. Apply callers
    /// should pass the exact ids reviewed in a preceding dry-run.
    ///
    /// `unrecoverable` carries one acknowledgement per commit an operator asserts is gone for
    /// good, which is the only way a missing record with no durable removal intent and no remote
    /// recovery proof can be tombstoned. Each acknowledgement is still checked against Git and is
    /// refused while any ref contains the commit; it grants no deletion, because a record reached
    /// this way has no tree left to delete.
    pub fn reconcile(
        &self,
        policy: &WorkspacePolicy,
        selected_ids: &[WorktreeId],
        apply: bool,
        allow_external_retirement: bool,
        unrecoverable: &[GitRevision],
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
            let assessment = self.assess_reconciliation(
                policy,
                &record,
                &action,
                unrecoverable,
                self.clock.now(),
            );
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
        require_matched_acknowledgements(&planned, unrecoverable)?;

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
                        unrecoverable,
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

    /// Resolve one tree reference — a registered id, a path to a registered tree, or the
    /// directory name of exactly one registered tree — to its record.
    ///
    /// With `scope`, a directory name matches only records whose repository lies below the
    /// policy's workspace root; an id or a path is resolved whatever the scope, so callers keep
    /// their own scope refusal. Removed records answer to their id only. Different records
    /// named by the value refuse as `ambiguous-worktree-reference`. A value naming none keeps
    /// the released codes: `unknown-worktree-id` when it is a well-formed id,
    /// `invalid-worktree-id` otherwise.
    pub fn resolve_reference(
        &self,
        reference: &str,
        scope: Option<&WorkspacePolicy>,
    ) -> Result<TreeSelection, Refusal> {
        if let Some(selection) = self.find_reference(reference, scope)? {
            return Ok(selection);
        }
        Err(if WorktreeId::new(reference).is_ok() {
            Refusal::new(
                "unknown-worktree-id",
                format!(
                    "{reference} is not registered, and names no registered tree path or \
                     directory name; see `worktree status`"
                ),
            )
        } else {
            Refusal::new(
                "invalid-worktree-id",
                format!(
                    "`{reference}` is neither a valid worktree id nor a registered tree path or \
                     directory name; see `worktree status`"
                ),
            )
        })
    }

    fn find_reference(
        &self,
        reference: &str,
        scope: Option<&WorkspacePolicy>,
    ) -> Result<Option<TreeSelection>, Refusal> {
        let records = self.registry.list()?;
        let canonical = std::fs::canonicalize(reference)
            .ok()
            .filter(|path| path.is_dir());
        select_reference(&records, reference, canonical.as_deref(), scope)
    }

    /// Resolve each reference to its record's id, in order and without duplicates. This is the
    /// selection `gc --id` and `reconcile --id` pass on as the reviewed ids.
    pub fn resolve_references(
        &self,
        references: &[String],
        scope: Option<&WorkspacePolicy>,
    ) -> Result<Vec<WorktreeId>, Refusal> {
        let mut ids = Vec::with_capacity(references.len());
        for reference in references {
            let id = self.resolve_reference(reference, scope)?.worktree_id;
            if !ids.contains(&id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    /// Resolve the tree argument of `finish`, `discard-cache` or `archive` to a path.
    ///
    /// The value is resolved as a reference first, so `finish docs` finishes the tree whose id
    /// is `docs` even where the working directory holds an unrelated `docs/`. A value that
    /// names no registered tree but exists is returned unchanged, so the command's own path
    /// refusal applies as before; one that does not exist refuses as `worktree-not-found`. An
    /// ambiguous value refuses as ambiguous.
    pub fn resolve_tree_path(&self, reference: &Path) -> Result<PathBuf, Refusal> {
        let Some(text) = reference.to_str() else {
            return Ok(reference.to_path_buf());
        };
        if let Some(selection) = self.find_reference(text, None)? {
            return Ok(selection.path);
        }
        if reference.exists() {
            return Ok(reference.to_path_buf());
        }
        Err(Refusal::new(
            "worktree-not-found",
            format!(
                "`{text}` is neither an existing path nor a registered id or tree directory name; \
                 see `worktree status`"
            ),
        ))
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
        let archive = self.existing_archive(record)?;
        if let Some(intent) = self.interrupted_removal(record)? {
            // The residue is proven to be HEAD's tracked content, so only commits need proof.
            let proof = self
                .recovery_proof(&record.repository_root, &intent.head, now)
                .or_else(|refusal| match &archive {
                    Some(dir) if refusal.code == NO_REMOTE_RECOVERY_PROOF => self.archive_proof(
                        record,
                        dir,
                        &intent.head,
                        ArchiveStateCheck::Unlinked,
                        now,
                    ),
                    _ => Err(refusal),
                })?;
            self.git
                .verify_removal_residue(&record.repository_root, &record.path, &intent.head)?;
            return Ok(proof);
        }
        let snapshot = self.exact_worktree_snapshot(&record.repository_root, &record.path)?;
        require_unlocked(&snapshot)?;
        if snapshot.dirty && archive.is_none() {
            // Remote refs never hold uncommitted state; only an exact archive can.
            return Err(dirty_refusal());
        }
        // A dirty tree is assessed on its archive alone, verified below in this same flow, so the
        // nested repositories that archive images are covered; a clean one relies on no archive.
        let relied_on = archive.as_deref().filter(|_| snapshot.dirty);
        self.git
            .hidden_state(&record.repository_root, &record.path, relied_on)?;
        if let (true, Some(dir)) = (snapshot.dirty, &archive) {
            self.require_matching_removal_intent(record)?;
            return self.archive_proof(
                record,
                dir,
                &snapshot.head,
                ArchiveStateCheck::Linked {
                    worktree: &record.path,
                },
                now,
            );
        }
        self.recovery_for_record(record, &snapshot.head, now)
            .or_else(|refusal| match &archive {
                Some(dir) if refusal.code == NO_REMOTE_RECOVERY_PROOF => self.archive_proof(
                    record,
                    dir,
                    &snapshot.head,
                    ArchiveStateCheck::Linked {
                        worktree: &record.path,
                    },
                    now,
                ),
                _ => Err(refusal),
            })
    }

    /// Return the durable `remove` intent of a tree Git already unlinked while its path remains.
    ///
    /// `git worktree remove` deletes the administrative directory even when deleting the files
    /// failed, which leaves a present path that is no longer a linked worktree.
    fn interrupted_removal(
        &self,
        record: &WorktreeRecord,
    ) -> Result<Option<RemovalIntent>, Refusal> {
        let Some(intent) = self.registry.removal(record.id.as_str())? else {
            return Ok(None);
        };
        if path_absent(&record.path)?
            || self
                .git
                .list_worktrees(&record.repository_root)?
                .iter()
                .any(|item| item.path == record.path)
        {
            return Ok(None);
        }
        if intent.path != record.path
            || intent.operation != "remove"
            || intent.head != intent.recovery.head
        {
            return Err(Refusal::new(
                "removal-intent-mismatch",
                "pending removal intent does not match the interrupted worktree removal",
            ));
        }
        Ok(Some(intent))
    }

    /// Finish an interrupted `remove` whose residue was proven to hold only the recorded commit.
    fn finish_interrupted_removal(
        &self,
        record: &WorktreeRecord,
        intent: RemovalIntent,
        proof: RecoveryProof,
    ) -> Result<OperationEvidence, Refusal> {
        if proof.head != intent.head {
            return Err(Refusal::new(
                "worktree-head-changed-during-proof",
                "recovery proof differs from the pending removal commit",
            ));
        }
        let intent = RemovalIntent {
            recovery: proof,
            planned_at: self.clock.now(),
            ..intent
        };
        self.registry.begin_removal(&intent)?;
        self.git
            .verify_removal_residue(&record.repository_root, &intent.path, &intent.head)?;
        self.git.delete_residue(&intent.path)?;
        if !path_absent(&intent.path)?
            || self
                .git
                .list_worktrees(&record.repository_root)?
                .iter()
                .any(|item| item.path == intent.path)
        {
            return Err(Refusal::new(
                "worktree-removal-incomplete",
                "the interrupted removal's residue still exists",
            ));
        }
        let evidence = OperationEvidence {
            operation: intent.operation.clone(),
            id: record.id.clone(),
            path: intent.path.clone(),
            head: Some(intent.head.clone()),
            recovery: Some(intent.recovery.clone()),
            recorded_at: self.clock.now(),
        };
        self.registry.complete_removal(&intent, &evidence)?;
        Ok(evidence)
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
        self.require_clean_or_archived(&record, &snapshot, None)?;
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
        // A deleted repository cannot be asked anything, so no other action is assessable; the
        // record is surfaced even while its tree still exists, so the dry-run says why it stays.
        if self.git.repository_absent(&record.repository_root)? {
            return Ok(Some(ReconciliationAction::TombstoneMissing {
                path: record.path.clone(),
            }));
        }
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
        unrecoverable: &[GitRevision],
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
                self.assess_missing(policy, record, path, unrecoverable, now)
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
        self.git.hidden_state(&record.repository_root, path, None)?;
        self.recovery_for_record(record, &snapshot.head, now)
            .map(Some)
    }

    fn assess_missing(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
        path: &Path,
        unrecoverable: &[GitRevision],
        now: i64,
    ) -> Result<Option<RecoveryProof>, Refusal> {
        if self.git.repository_absent(&record.repository_root)? {
            return self.assess_missing_repository(record, path, unrecoverable);
        }
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
            return assess_stale_external_relocation(
                policy,
                record,
                path,
                &discovered,
                &relocation,
                removal,
            )
            .map(Some);
        }
        if matches!(record.lifecycle, Lifecycle::Active | Lifecycle::Relocating)
            && removal.is_none()
        {
            let Some(head) = record.head.as_deref() else {
                return Err(Refusal::new(
                    "missing-active-worktree",
                    "an active or relocating worktree disappeared without durable removal intent, \
                     and its record holds no commit that could be assessed",
                ));
            };
            if !acknowledges(unrecoverable, head) {
                return Err(Refusal::new(
                    "missing-active-worktree",
                    abandonment_guidance(
                        "an active or relocating worktree disappeared without durable removal \
                         intent",
                        record,
                        head,
                    ),
                ));
            }
            self.assess_abandonment(record, head)?;
            return Ok(None);
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
        if acknowledges(unrecoverable, head) {
            self.assess_abandonment(record, head)?;
            return Ok(None);
        }
        self.recovery_proof(&record.repository_root, head, now)
            .map(Some)
            .map_err(|refusal| {
                if refusal.code == "no-remote-recovery-proof" {
                    Refusal::new(
                        refusal.code,
                        abandonment_guidance(&refusal.message, record, head),
                    )
                } else {
                    refusal
                }
            })
    }

    /// Assess a record whose repository no longer exists at its recorded root.
    ///
    /// Git cannot corroborate anything here, so the operator's acknowledgement of the exact
    /// recorded commit is the whole of the evidence, and it is only accepted once every path the
    /// record or its intents name is already absent. Nothing on disk is touched; a tree that is
    /// still there is the operator's to deal with first.
    fn assess_missing_repository(
        &self,
        record: &WorktreeRecord,
        path: &Path,
        unrecoverable: &[GitRevision],
    ) -> Result<Option<RecoveryProof>, Refusal> {
        let repository = record.repository_root.display();
        let removal = self.registry.removal(record.id.as_str())?;
        let relocation = self.registry.relocation(record.id.as_str())?;
        let named = std::iter::once(path)
            .chain(removal.iter().map(|intent| intent.path.as_path()))
            .chain(
                relocation
                    .iter()
                    .flat_map(|intent| [intent.from.as_path(), intent.to.as_path()]),
            );
        for tree in named {
            if !path_absent(tree)? {
                return Err(Refusal::new(
                    "worktree-path-exists",
                    format!(
                        "{} still exists although repository {repository} is gone; nothing \
                         removes it for you, so move or delete it yourself before reconciling \
                         {}",
                        tree.display(),
                        record.id.as_str()
                    ),
                ));
            }
        }
        let Some(head) = record.head.as_deref() else {
            return Err(Refusal::new(
                "repository-missing",
                format!(
                    "repository {repository} is gone and the record holds no commit that could \
                     be acknowledged"
                ),
            ));
        };
        if removal.as_ref().is_some_and(|intent| intent.head != head) {
            return Err(Refusal::new(
                "removal-intent-mismatch",
                "pending removal intent names a different commit than the record",
            ));
        }
        if !acknowledges(unrecoverable, head) {
            return Err(Refusal::new(
                "repository-missing",
                format!(
                    "repository {repository} no longer exists or is not a Git repository, and \
                     tree {} is gone, so nothing can check commit {head}; if it still exists \
                     anywhere, recover it first, and only once you have established that it is \
                     gone for good retire the record with `worktree reconcile --apply --id {} \
                     --acknowledge-unrecoverable {head}`",
                    path.display(),
                    record.id.as_str()
                ),
            ));
        }
        Ok(None)
    }

    /// Assess an operator's assertion that a missing record's recorded commit is gone for good.
    ///
    /// This produces no [`RecoveryProof`], because there is nothing recoverable to prove, and it
    /// deletes nothing: the tree is already absent and every ref is left exactly as it is. It
    /// succeeds only while this repository can still corroborate the assertion - Git holds no
    /// such object at all, or holds it with no local branch, tag or remote-tracking ref pointing
    /// at it and no remote advertising it. Any surviving ref, and any observation that is offline
    /// or otherwise ambiguous, is a refusal.
    fn assess_abandonment(&self, record: &WorktreeRecord, head: &str) -> Result<(), Refusal> {
        let Some(local) = self.git.containing_refs(&record.repository_root, head)? else {
            return Ok(());
        };
        require_no_containing_refs(head, &local)?;
        let advertised = self.git.recovery_refs(&record.repository_root, head)?;
        require_no_containing_refs(head, &advertised)
    }

    fn apply_reconciliation(
        &self,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
        action: &ReconciliationAction,
        _recovery: Option<RecoveryProof>,
        unrecoverable: &[GitRevision],
        now: i64,
    ) -> Result<OperationEvidence, Refusal> {
        let recovery = self.assess_reconciliation(policy, record, action, unrecoverable, now)?;
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
                // An abandonment carries no recovery proof by construction, so it is recorded
                // under its own operation rather than being filed as an ordinary reconciliation.
                let abandoned = pending.is_none()
                    && recovery.is_none()
                    && head
                        .as_deref()
                        .is_some_and(|head| acknowledges(unrecoverable, head));
                let evidence = OperationEvidence {
                    operation: if abandoned {
                        "reconcile-abandoned".into()
                    } else {
                        "reconcile-missing".into()
                    },
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
        let evidence = self.git.recovery_evidence(repository, head)?;
        if evidence.refs.is_empty() {
            return Err(Refusal::new(
                NO_REMOTE_RECOVERY_PROOF,
                format!(
                    "commit {head} is not reachable from an advertised remote ref, and no \
                     advertised ref carries a patch-identical commit for each of its unique commits"
                ),
            ));
        }
        Ok(RecoveryProof {
            head: head.to_owned(),
            refs: evidence.refs,
            observed_at: now,
            kind: evidence.kind,
            equivalent_commits: evidence.equivalent_commits,
            archive: None,
        })
    }

    fn recovery_for_record(
        &self,
        record: &WorktreeRecord,
        head: &str,
        now: i64,
    ) -> Result<RecoveryProof, Refusal> {
        self.require_matching_removal_intent(record)?;
        self.recovery_proof(&record.repository_root, head, now)
    }

    fn require_matching_removal_intent(&self, record: &WorktreeRecord) -> Result<(), Refusal> {
        if let Some(intent) = self.registry.removal(record.id.as_str())? {
            if intent.path != record.path {
                return Err(Refusal::new(
                    "removal-intent-mismatch",
                    "pending removal proof does not match the current worktree path",
                ));
            }
        }
        Ok(())
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

    /// Refuse what [`Self::require_clean_or_archived`] refuses, then hidden state. Only archive
    /// proof, whose state the first check has just verified, lets hidden state rely on its
    /// archive.
    fn require_removable_state(
        &self,
        record: &WorktreeRecord,
        path: &Path,
        snapshot: &WorktreeSnapshot,
        proof: &RecoveryProof,
    ) -> Result<(), Refusal> {
        self.require_clean_or_archived(record, snapshot, Some(proof))?;
        self.git
            .hidden_state(&record.repository_root, path, relied_on_archive(proof))
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
        self.require_removable_state(record, path, &before_intent, &proof)?;
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
        let mut before_remove = self.exact_worktree_snapshot(&record.repository_root, path)?;
        self.require_removable_state(record, path, &before_remove, &intent.recovery)?;
        if before_remove.head != intent.head {
            return Err(Refusal::new(
                "worktree-head-changed-after-intent",
                "worktree HEAD changed after removal intent became durable",
            ));
        }
        if before_remove.dirty {
            // Reached only with archive proof that holds exactly this state: return the tree to
            // HEAD so that Git removes it without force, then prove that it did.
            let archive = intent
                .recovery
                .archive
                .as_ref()
                .map(|reference| reference.path.clone())
                .ok_or_else(dirty_refusal)?;
            self.git
                .discard_archived_state(record, &archive, &intent.head)?;
            before_remove = self.exact_worktree_snapshot(&record.repository_root, path)?;
            require_clean_unlocked(&before_remove)?;
            // The discard deleted every imaged nested repository; a nested `.git` now is not one.
            self.git.hidden_state(&record.repository_root, path, None)?;
            if before_remove.head != intent.head {
                return Err(Refusal::new(
                    "worktree-head-changed-after-intent",
                    "worktree HEAD changed after removal intent became durable",
                ));
            }
        }
        if let Some((policy, Some(relocation))) = retirement {
            self.require_retirement_topology(policy, record, path, relocation, true)?;
        }
        self.git
            .remove(&record.repository_root, path)
            .map_err(|refusal| {
                if operation == "remove" {
                    Refusal::new(
                        refusal.code,
                        format!(
                            "{}; rerun `worktree gc --apply --id {}` to finish the removal",
                            refusal.message, record.id
                        ),
                    )
                } else {
                    refusal
                }
            })?;
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

/// One archive directory, `<archive root>/<repository name>/<name>`.
#[derive(Debug, Clone)]
struct ArchiveDirectory {
    repository: std::ffi::OsString,
    name: String,
    path: PathBuf,
}

/// The archive root's archive directories, and every other entry, each with the repository
/// directory it sits in (`None` directly in the root).
struct ArchiveRootListing {
    archives: Vec<ArchiveDirectory>,
    skipped: Vec<(Option<std::ffi::OsString>, SkippedArchiveEntry)>,
}

/// List `<root>/<repository name>/<directory>` without following links. Directories whose name
/// starts with `.` are archives being written, and files are not archives: both are skipped.
fn list_archive_root(root: &Path) -> Result<ArchiveRootListing, Refusal> {
    fn entries(directory: &Path) -> Result<Vec<(std::ffi::OsString, std::fs::Metadata)>, Refusal> {
        let failed = |error: std::io::Error| {
            Refusal::new(
                "archive-root-invalid",
                format!("{}: {error}", directory.display()),
            )
        };
        let mut entries = Vec::new();
        for entry in std::fs::read_dir(directory).map_err(failed)? {
            let entry = entry.map_err(failed)?;
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(failed)?;
            entries.push((entry.file_name(), metadata));
        }
        entries.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(entries)
    }
    let skip = |path: PathBuf, metadata: &std::fs::Metadata| SkippedArchiveEntry {
        path,
        bytes: if metadata.is_file() {
            metadata.len()
        } else {
            0
        },
    };
    let hidden = |name: &std::ffi::OsStr| name.as_encoded_bytes().starts_with(b".");
    let mut listing = ArchiveRootListing {
        archives: Vec::new(),
        skipped: Vec::new(),
    };
    if path_absent(root)? {
        return Ok(listing);
    }
    for (repository, metadata) in entries(root)? {
        let repository_path = root.join(&repository);
        if !metadata.is_dir() || hidden(&repository) {
            listing
                .skipped
                .push((None, skip(repository_path, &metadata)));
            continue;
        }
        for (name, metadata) in entries(&repository_path)? {
            let path = repository_path.join(&name);
            match name.to_str() {
                Some(text) if metadata.is_dir() && !hidden(&name) => {
                    listing.archives.push(ArchiveDirectory {
                        repository: repository.clone(),
                        name: text.to_owned(),
                        path,
                    });
                }
                _ => listing
                    .skipped
                    .push((Some(repository.clone()), skip(path, &metadata))),
            }
        }
    }
    Ok(listing)
}

/// Resolve one `--id` value: `<directory>` or `<repository name>/<directory>`.
fn select_archive<'a>(
    archives: &'a [ArchiveDirectory],
    reference: &str,
) -> Result<&'a ArchiveDirectory, Refusal> {
    let matches = archives
        .iter()
        .filter(|archive| match reference.split_once('/') {
            Some((repository, name)) => {
                archive.repository == std::ffi::OsStr::new(repository) && archive.name == name
            }
            None => archive.name == reference,
        })
        .collect::<Vec<_>>();
    match matches.as_slice() {
        [archive] => Ok(archive),
        [] => Err(Refusal::new(
            "unknown-archive-directory",
            format!("`{reference}` names no archive directory below the archive root"),
        )),
        _ => Err(Refusal::new(
            "ambiguous-archive-directory",
            format!(
                "`{reference}` names archives of {} repositories; pass <repository>/{reference}",
                matches.len()
            ),
        )),
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

/// Finish a pre-0.3 relocation whose finished external source is already absent.
fn assess_stale_external_relocation(
    policy: &WorkspacePolicy,
    record: &WorktreeRecord,
    path: &Path,
    discovered: &[DiscoveredWorktree],
    relocation: &RelocationIntent,
    removal: Option<RemovalIntent>,
) -> Result<RecoveryProof, Refusal> {
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
    Ok(intent.recovery)
}

/// One planned reconciliation: the record, the proposed action, and its current assessment.
type PlannedReconciliation = (
    WorktreeRecord,
    ReconciliationAction,
    Result<Option<RecoveryProof>, Refusal>,
);

/// Refuse an acknowledgement that no reviewed missing record actually carries.
///
/// A copy-pasted or stale assertion must not silently apply to some other record.
fn require_matched_acknowledgements(
    planned: &[PlannedReconciliation],
    unrecoverable: &[GitRevision],
) -> Result<(), Refusal> {
    for acknowledged in unrecoverable {
        if !planned.iter().any(|(record, action, _)| {
            matches!(action, ReconciliationAction::TombstoneMissing { .. })
                && record.head.as_deref() == Some(acknowledged.as_str())
        }) {
            return Err(Refusal::new(
                "unmatched-unrecoverable-acknowledgement",
                format!(
                    "no reviewed missing record records commit {}",
                    acknowledged.as_str()
                ),
            ));
        }
    }
    Ok(())
}

/// Whether an operator explicitly asserted that this exact commit is unrecoverable.
fn acknowledges(unrecoverable: &[GitRevision], head: &str) -> bool {
    unrecoverable
        .iter()
        .any(|acknowledged| acknowledged.as_str() == head)
}

/// Refuse an abandonment while anything still points at the commit it would forget.
fn require_no_containing_refs(head: &str, refs: &[String]) -> Result<(), Refusal> {
    if refs.is_empty() {
        return Ok(());
    }
    Err(Refusal::new(
        "recorded-commit-still-reachable",
        format!(
            "commit {head} is still reachable from {}, so it is not unrecoverable; reconcile the \
             record without the acknowledgement once that work is published",
            refs.join(", ")
        ),
    ))
}

/// Say how to resolve a missing record that currently has no recoverable evidence.
fn abandonment_guidance(reason: &str, record: &WorktreeRecord, head: &str) -> String {
    format!(
        "{reason}; if commit {head} still exists anywhere, publish it and rerun the dry-run, and \
         only once you have established that it is gone for good abandon the record with \
         `worktree reconcile --apply --id {} --acknowledge-unrecoverable {head}`",
        record.id.as_str()
    )
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

/// Refusal code when no advertised ref proves a commit recoverable.
const NO_REMOTE_RECOVERY_PROOF: &str = "no-remote-recovery-proof";

fn require_clean_unlocked(snapshot: &WorktreeSnapshot) -> Result<(), Refusal> {
    require_unlocked(snapshot)?;
    if snapshot.dirty {
        return Err(dirty_refusal());
    }
    Ok(())
}

fn require_unlocked(snapshot: &WorktreeSnapshot) -> Result<(), Refusal> {
    if snapshot.locked {
        return Err(Refusal::new(
            "worktree-locked",
            "Git marks the worktree locked",
        ));
    }
    Ok(())
}

fn dirty_refusal() -> Refusal {
    Refusal::new(
        "worktree-dirty",
        "tracked, untracked, or ignored files make cleanup unsafe",
    )
}

/// The archive a removal relies on, when its proof is archive proof.
fn relied_on_archive(proof: &RecoveryProof) -> Option<&Path> {
    proof
        .archive
        .as_ref()
        .filter(|_| proof.kind == RecoveryKind::Archive)
        .map(|reference| reference.path.as_path())
}

/// The dirty refusal after a cache discard, naming what was kept and why.
fn retained_refusal(cache: &CacheDiscard) -> Refusal {
    const SHOWN: usize = 8;
    let kept = if cache.retained_ignored.is_empty() {
        "no ignored entry; tracked or untracked changes remain".to_owned()
    } else {
        let named = cache
            .retained_ignored
            .iter()
            .take(SHOWN)
            .map(|path| path.display().to_string())
            .collect::<Vec<_>>()
            .join(", ");
        let more = match cache.retained_ignored.len().saturating_sub(SHOWN) {
            0 => String::new(),
            rest => format!(" and {rest} more"),
        };
        format!("ignored entries that are not recognised cache: {named}{more}")
    };
    Refusal::new(
        "worktree-dirty",
        format!(
            "the recognised build cache was discarded and the tree still differs from HEAD ({kept}); \
             commit what belongs to the work, or add --archive to keep the rest in an archive and \
             finish"
        ),
    )
}

/// Thresholds for [`WorktreeManager::sweep`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SweepOptions {
    /// Delete and write; without it the sweep only classifies.
    pub apply: bool,
    /// Idle seconds before a tree's recognised build cache is discarded.
    pub discard_after_seconds: i64,
    /// Largest retained ignored content an expired tree's archive may take on; larger is refused.
    pub max_archive_bytes: u64,
}

impl Default for SweepOptions {
    fn default() -> Self {
        Self {
            apply: false,
            discard_after_seconds: 86_400,
            max_archive_bytes: 1 << 30,
        }
    }
}

/// Steps [`WorktreeManager::finish_with`] takes before finishing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FinishOptions {
    /// Delete the recognised build cache first.
    pub discard_cache: bool,
    /// Archive whatever the tree still holds that no advertised ref recovers.
    pub archive: bool,
}

/// Evidence of [`WorktreeManager::finish_with`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FinishEvidence {
    /// The finish itself.
    pub evidence: OperationEvidence,
    /// The cache discard, when requested.
    pub cache: Option<CacheDiscard>,
    /// The archive written, when one was needed.
    pub archive: Option<ArchiveEvidence>,
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

/// Observed prerequisites for a readiness check.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReadinessObservation {
    /// Whether a working `git` executable is available.
    pub git: bool,
    /// Whether the configuration loaded and validated.
    pub config: bool,
    /// Whether the registry opened.
    pub registry: bool,
    /// Number of active workspace profiles in the configuration.
    pub profiles: usize,
}

/// A named reason the service is not ready to create managed worktrees.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReadinessFailure {
    /// No working `git` executable.
    GitUnavailable,
    /// The configuration could not be loaded.
    ConfigUnavailable,
    /// The registry could not be opened.
    RegistryUnavailable,
    /// The configuration loaded but holds no workspace profile, so no create has a policy.
    NoActiveProfile,
}

impl std::fmt::Display for ReadinessFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::GitUnavailable => "git is unavailable",
            Self::ConfigUnavailable => "configuration is unavailable",
            Self::RegistryUnavailable => "registry is unavailable",
            Self::NoActiveProfile => "no active profile",
        })
    }
}

/// Decide readiness from observed prerequisites; an empty result means ready.
#[must_use]
pub fn readiness_failures(observation: ReadinessObservation) -> Vec<ReadinessFailure> {
    let mut failures = Vec::new();
    if !observation.git {
        failures.push(ReadinessFailure::GitUnavailable);
    }
    if !observation.config {
        failures.push(ReadinessFailure::ConfigUnavailable);
    } else if observation.profiles == 0 {
        failures.push(ReadinessFailure::NoActiveProfile);
    }
    if !observation.registry {
        failures.push(ReadinessFailure::RegistryUnavailable);
    }
    failures
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
    use tempfile::{TempDir, tempdir};

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
        containment: Mutex<BTreeMap<String, Vec<String>>>,
        resolved_revision: Mutex<Option<String>>,
        remove_fault: Option<RemoveFault>,
        snapshot_sequence: Mutex<VecDeque<String>>,
        residue_matches: bool,
        repository_absent: bool,
        /// The archive each hidden-state observation was told it may rely on, in call order.
        hidden_archives: Mutex<Vec<Option<PathBuf>>>,
    }

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum RemoveFault {
        /// `git worktree remove` refuses and changes nothing.
        Refuse,
        /// `git worktree remove` unlinks the tree and then fails to delete its files.
        Unlink,
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

        fn containing_refs(
            &self,
            _repository: &Path,
            head: &str,
        ) -> Result<Option<Vec<String>>, Refusal> {
            // An absent entry means Git holds no such object; an empty entry means the object
            // survives with nothing pointing at it.
            Ok(self.containment.lock().unwrap().get(head).cloned())
        }

        fn remove(&self, _repository: &Path, worktree: &Path) -> Result<(), Refusal> {
            if self.remove_fault == Some(RemoveFault::Refuse) {
                return Err(Refusal::new("remove-failed", "injected removal failure"));
            }
            if self.remove_fault == Some(RemoveFault::Unlink) {
                self.snapshots.lock().unwrap().remove(worktree);
                self.discovered
                    .lock()
                    .unwrap()
                    .retain(|item| item.path != worktree);
                return Err(Refusal::new("git-command-failed", "Permission denied"));
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

        fn verify_removal_residue(
            &self,
            _repository: &Path,
            worktree: &Path,
            _head: &str,
        ) -> Result<(), Refusal> {
            if self.residue_matches {
                Ok(())
            } else {
                Err(Refusal::new(
                    "removal-residue-unproven",
                    worktree.display().to_string(),
                ))
            }
        }

        fn delete_residue(&self, worktree: &Path) -> Result<(), Refusal> {
            std::fs::remove_dir_all(worktree).unwrap();
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

        fn hidden_state(
            &self,
            _repository: &Path,
            _worktree: &Path,
            archive: Option<&Path>,
        ) -> Result<(), Refusal> {
            self.hidden_archives
                .lock()
                .unwrap()
                .push(archive.map(Path::to_path_buf));
            Ok(())
        }

        fn repository_absent(&self, _repository: &Path) -> Result<bool, Refusal> {
            Ok(self.repository_absent)
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
            containment: Mutex::new(BTreeMap::new()),
            resolved_revision: Mutex::new(None),
            remove_fault: None,
            snapshot_sequence: Mutex::new(VecDeque::new()),
            residue_matches: true,
            repository_absent: false,
            hidden_archives: Mutex::new(Vec::new()),
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
                kind: b10x_worktree_domain::RecoveryKind::Ancestor,
                equivalent_commits: Vec::new(),
                archive: None,
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
                containment: Mutex::new(BTreeMap::new()),
                resolved_revision: Mutex::new(None),
                remove_fault: None,
                snapshot_sequence: Mutex::new(VecDeque::new()),
                discovered_sequence: Mutex::new(VecDeque::new()),
                residue_matches: true,
                repository_absent: false,
                hidden_archives: Mutex::new(Vec::new()),
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
            .reconcile(
                &policy,
                std::slice::from_ref(&registered.id),
                true,
                false,
                &[],
            )
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
                containment: Mutex::new(BTreeMap::new()),
                resolved_revision: Mutex::new(None),
                remove_fault: None,
                snapshot_sequence: Mutex::new(VecDeque::new()),
                discovered_sequence: Mutex::new(VecDeque::new()),
                residue_matches: true,
                repository_absent: false,
                hidden_archives: Mutex::new(Vec::new()),
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
            .reconcile(
                &policy,
                std::slice::from_ref(&registered.id),
                true,
                false,
                &[],
            )
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
    fn gc_scope_selects_the_repository_unless_profile_or_ids_are_given() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let alpha = workspace.join("alpha");
        let beta = workspace.join("beta");
        let managed_root = temporary.path().join("managed");
        for path in [&alpha, &beta, &managed_root] {
            std::fs::create_dir_all(path).unwrap();
        }
        let alpha_tree = named_record(
            "alpha-tree",
            alpha.clone(),
            managed_root.join("alpha/alpha-tree"),
            Lifecycle::Finished,
        );
        let beta_tree = named_record(
            "beta-tree",
            beta.clone(),
            managed_root.join("beta/beta-tree"),
            Lifecycle::Finished,
        );
        let manager = WorktreeManager::new(
            fake_git(alpha.clone()),
            fake_registry(vec![alpha_tree.clone(), beta_tree.clone()]),
            FixedClock,
        );
        let policy = policy(workspace, managed_root);
        let ids = |assessments: Vec<CleanupAssessment>| {
            let mut ids: Vec<String> = assessments
                .into_iter()
                .map(|item| item.record.id.as_str().to_owned())
                .collect();
            ids.sort();
            ids
        };

        let repository = manager
            .gc_scoped(&policy, &alpha, CleanupScope::Repository, &[], false)
            .unwrap();
        assert_eq!(ids(repository), vec!["alpha-tree"]);
        // A non-canonical spelling of the repository root selects the same records.
        let spelled = alpha.join("..").join("alpha");
        let repository = manager
            .gc_scoped(&policy, &spelled, CleanupScope::Repository, &[], false)
            .unwrap();
        assert_eq!(ids(repository), vec!["alpha-tree"]);

        let profile = manager
            .gc_scoped(&policy, &alpha, CleanupScope::Profile, &[], false)
            .unwrap();
        assert_eq!(ids(profile), vec!["alpha-tree", "beta-tree"]);
        // The unscoped method keeps its profile-wide selection for existing callers.
        assert_eq!(
            ids(manager.gc(&policy, &[], false).unwrap()),
            vec!["alpha-tree", "beta-tree"]
        );

        let named = manager
            .gc_scoped(
                &policy,
                &alpha,
                CleanupScope::Repository,
                std::slice::from_ref(&beta_tree.id),
                false,
            )
            .unwrap();
        assert_eq!(ids(named), vec!["beta-tree"]);
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
                &[],
            )
            .unwrap();
        assert_eq!(assessments.len(), 1);
        assert!(!assessments[0].eligible);
        let refusal = assessments[0].refusal.as_ref().unwrap();
        assert_eq!(refusal.code, "missing-active-worktree");
        // The refusal has to say what to do about it, not only that it refuses.
        assert!(refusal.message.contains("publish it and rerun the dry-run"));
        assert!(
            refusal.message.contains("--acknowledge-unrecoverable abc"),
            "{}",
            refusal.message
        );
        assert!(refusal.message.contains("--id missing-active"));
    }

    /// One missing Active record whose recorded commit survives nowhere.
    fn abandoned_fixture(
        temporary: &TempDir,
        containment: Option<Vec<String>>,
        recoverable: bool,
    ) -> (
        WorktreeManager<FakeGit, FakeRegistry, FixedClock>,
        WorkspacePolicy,
        WorktreeRecord,
    ) {
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        std::fs::create_dir_all(&repository).unwrap();
        let registered = named_record(
            "missing-active",
            repository.clone(),
            managed_root.join("repo/missing-active"),
            Lifecycle::Active,
        );
        let git = FakeGit {
            recoverable,
            containment: Mutex::new(
                containment
                    .into_iter()
                    .map(|refs| ("abc".to_owned(), refs))
                    .collect(),
            ),
            ..fake_git(repository)
        };
        let manager =
            WorktreeManager::new(git, fake_registry(vec![registered.clone()]), FixedClock);
        (manager, policy(workspace, managed_root), registered)
    }

    #[test]
    fn deleted_repository_is_not_retired_while_a_relocation_destination_exists() {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let destination = managed_root.join("repo/moved");
        // The repository itself is gone; only its workspace and the moved tree remain.
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&destination).unwrap();
        let registered = named_record(
            "moved",
            repository.clone(),
            workspace.join("legacy/moved"),
            Lifecycle::Relocating,
        );
        let registry = FakeRegistry {
            relocations: Mutex::new(BTreeMap::from([(
                registered.id.to_string(),
                stale_relocation(&registered, &destination),
            )])),
            ..fake_registry(vec![registered.clone()])
        };
        let git = FakeGit {
            repository_absent: true,
            ..fake_git(repository)
        };
        let manager = WorktreeManager::new(git, registry, FixedClock);

        let assessments = manager
            .reconcile(
                &policy(workspace, managed_root),
                std::slice::from_ref(&registered.id),
                true,
                false,
                &[GitRevision::new("abc").unwrap()],
            )
            .unwrap();

        let refusal = assessments[0].refusal.as_ref().unwrap();
        assert_eq!(refusal.code, "worktree-path-exists");
        assert!(
            refusal.message.contains(destination.to_str().unwrap()),
            "{}",
            refusal.message
        );
        assert!(assessments[0].evidence.is_none());
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Relocating
        );
        assert!(destination.is_dir());
    }

    #[test]
    fn acknowledged_unrecoverable_commit_abandons_a_missing_active_record() {
        let temporary = tempdir().unwrap();
        // The object survives with nothing pointing at it, and no remote advertises it.
        let (manager, policy, registered) = abandoned_fixture(&temporary, Some(Vec::new()), false);
        let acknowledgement = [GitRevision::new("abc").unwrap()];

        let assessments = manager
            .reconcile(
                &policy,
                std::slice::from_ref(&registered.id),
                true,
                false,
                &acknowledgement,
            )
            .unwrap();

        assert_eq!(assessments.len(), 1);
        assert!(assessments[0].eligible, "{:?}", assessments[0].refusal);
        let evidence = assessments[0].evidence.as_ref().unwrap();
        assert_eq!(evidence.operation, "reconcile-abandoned");
        assert_eq!(evidence.head.as_deref(), Some("abc"));
        // Nothing is proven, so nothing is claimed: no fabricated recovery evidence.
        assert!(evidence.recovery.is_none());
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Removed
        );
    }

    #[test]
    fn acknowledged_commit_absent_from_the_object_database_is_abandoned() {
        let temporary = tempdir().unwrap();
        // Git cannot resolve the commit at all; there is nothing left to lose.
        let (manager, policy, registered) = abandoned_fixture(&temporary, None, false);

        let assessments = manager
            .reconcile(
                &policy,
                std::slice::from_ref(&registered.id),
                true,
                false,
                &[GitRevision::new("abc").unwrap()],
            )
            .unwrap();

        assert!(assessments[0].eligible, "{:?}", assessments[0].refusal);
        assert_eq!(
            assessments[0].evidence.as_ref().unwrap().operation,
            "reconcile-abandoned"
        );
    }

    #[test]
    fn acknowledgement_is_refused_while_a_local_ref_still_contains_the_commit() {
        let temporary = tempdir().unwrap();
        let (manager, policy, registered) = abandoned_fixture(
            &temporary,
            Some(vec!["refs/heads/wave/hardening".to_owned()]),
            false,
        );

        let assessments = manager
            .reconcile(
                &policy,
                std::slice::from_ref(&registered.id),
                true,
                false,
                &[GitRevision::new("abc").unwrap()],
            )
            .unwrap();

        assert!(!assessments[0].eligible);
        let refusal = assessments[0].refusal.as_ref().unwrap();
        assert_eq!(refusal.code, "recorded-commit-still-reachable");
        assert!(
            refusal.message.contains("refs/heads/wave/hardening"),
            "{}",
            refusal.message
        );
        assert!(assessments[0].evidence.is_none());
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Active
        );
    }

    #[test]
    fn acknowledgement_is_refused_while_a_remote_still_advertises_the_commit() {
        let temporary = tempdir().unwrap();
        // No local ref holds it, but a remote does: the ordinary tombstone applies instead.
        let (manager, policy, registered) = abandoned_fixture(&temporary, Some(Vec::new()), true);

        let assessments = manager
            .reconcile(
                &policy,
                std::slice::from_ref(&registered.id),
                true,
                false,
                &[GitRevision::new("abc").unwrap()],
            )
            .unwrap();

        assert!(!assessments[0].eligible);
        assert_eq!(
            assessments[0].refusal.as_ref().unwrap().code,
            "recorded-commit-still-reachable"
        );
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Active
        );
    }

    #[test]
    fn abandonment_requires_reviewed_ids_and_a_matching_acknowledgement() {
        let temporary = tempdir().unwrap();
        let (manager, policy, registered) = abandoned_fixture(&temporary, Some(Vec::new()), false);
        let acknowledgement = [GitRevision::new("abc").unwrap()];

        // Without the exact ids a preceding dry-run named, there is nothing reviewed to apply.
        assert_eq!(
            manager
                .reconcile(&policy, &[], true, false, &acknowledgement)
                .unwrap_err()
                .code,
            "explicit-reconciliation-selection-required"
        );
        // An acknowledgement that names no reviewed missing record is refused outright.
        assert_eq!(
            manager
                .reconcile(
                    &policy,
                    std::slice::from_ref(&registered.id),
                    true,
                    false,
                    &[GitRevision::new("0123456789").unwrap()],
                )
                .unwrap_err()
                .code,
            "unmatched-unrecoverable-acknowledgement"
        );
        // Without any acknowledgement the record stays exactly as stuck as it was.
        let assessments = manager
            .reconcile(
                &policy,
                std::slice::from_ref(&registered.id),
                true,
                false,
                &[],
            )
            .unwrap();
        assert!(!assessments[0].eligible);
        assert_eq!(
            assessments[0].refusal.as_ref().unwrap().code,
            "missing-active-worktree"
        );
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Active
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
                kind: b10x_worktree_domain::RecoveryKind::Ancestor,
                equivalent_commits: Vec::new(),
                archive: None,
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
                &[],
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
            .reconcile(&policy(workspace, managed_root), &ids, true, false, &[])
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
        assert_eq!(
            manager.registry().list().unwrap(),
            [] as [b10x_worktree_domain::WorktreeRecord; 0]
        );
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
        assert_eq!(
            manager.registry().list().unwrap(),
            [] as [b10x_worktree_domain::WorktreeRecord; 0]
        );
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
        assert_eq!(
            manager.registry().list().unwrap(),
            [] as [b10x_worktree_domain::WorktreeRecord; 0]
        );
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

    /// A finished tree whose `git worktree remove` unlinks it and then fails to delete its files.
    fn interrupted_removal_fixture(
        residue_matches: bool,
    ) -> (
        tempfile::TempDir,
        WorkspacePolicy,
        WorktreeRecord,
        WorktreeManager<FakeGit, FakeRegistry, FixedClock>,
    ) {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let path = managed_root.join("repo/read-only");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(path.join("app/jobs")).unwrap();
        let registered = named_record(
            "read-only",
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
                path,
                head: Some("abc".into()),
                locked: false,
                primary: false,
            }]),
            remove_fault: Some(RemoveFault::Unlink),
            residue_matches,
            ..fake_git(repository)
        };
        let manager =
            WorktreeManager::new(git, fake_registry(vec![registered.clone()]), FixedClock);
        (
            temporary,
            policy(workspace, managed_root),
            registered,
            manager,
        )
    }

    /// A finished tree, an archive root, and optionally an archive directory for the tree.
    ///
    /// `FakeGit` keeps the refusing archive defaults, so any archive consultation is visible as
    /// `archive-unsupported`.
    fn archive_fixture(
        dirty: bool,
        recoverable: bool,
        archived: bool,
    ) -> (
        tempfile::TempDir,
        WorkspacePolicy,
        WorktreeRecord,
        WorktreeManager<FakeGit, FakeRegistry, FixedClock>,
    ) {
        let temporary = tempdir().unwrap();
        let workspace = temporary.path().join("workspace");
        let repository = workspace.join("repo");
        let managed_root = temporary.path().join("managed");
        let path = managed_root.join("repo/archived");
        let archive_root = temporary.path().join("archive_root");
        std::fs::create_dir_all(&repository).unwrap();
        std::fs::create_dir_all(&path).unwrap();
        if archived {
            std::fs::create_dir_all(archive_root.join("repo/archived")).unwrap();
        }
        let registered = named_record(
            "archived",
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
                    dirty,
                    locked: false,
                },
            )])),
            discovered: Mutex::new(vec![discovered_worktree(&path, "abc")]),
            recoverable,
            ..fake_git(repository)
        };
        let manager =
            WorktreeManager::new(git, fake_registry(vec![registered.clone()]), FixedClock)
                .with_archive_root(archive_root);
        (
            temporary,
            policy(workspace, managed_root),
            registered,
            manager,
        )
    }

    fn assess_one(
        manager: &WorktreeManager<FakeGit, FakeRegistry, FixedClock>,
        policy: &WorkspacePolicy,
        record: &WorktreeRecord,
    ) -> CleanupAssessment {
        let mut assessments = manager
            .gc(policy, std::slice::from_ref(&record.id), false)
            .unwrap();
        assert_eq!(assessments.len(), 1);
        assessments.remove(0)
    }

    #[test]
    fn an_archive_is_not_consulted_while_remote_refs_prove_a_clean_tree() {
        let (_temporary, policy, registered, manager) = archive_fixture(false, true, true);

        let assessment = assess_one(&manager, &policy, &registered);
        assert!(assessment.eligible, "{:?}", assessment.refusal);
        assert_eq!(assessment.archive, None);
    }

    #[test]
    fn a_present_archive_stands_in_only_for_missing_remote_proof() {
        let (_temporary, policy, registered, manager) = archive_fixture(false, false, true);

        let assessment = assess_one(&manager, &policy, &registered);
        assert_eq!(assessment.refusal.unwrap().code, "archive-unsupported");
    }

    #[test]
    fn without_an_archive_a_local_only_tree_is_refused_as_before() {
        let (_temporary, policy, registered, manager) = archive_fixture(false, false, false);

        let assessment = assess_one(&manager, &policy, &registered);
        assert_eq!(assessment.refusal.unwrap().code, "no-remote-recovery-proof");
    }

    #[test]
    fn without_an_archive_a_dirty_tree_is_refused_as_before() {
        let (_temporary, policy, registered, manager) = archive_fixture(true, true, false);

        let assessment = assess_one(&manager, &policy, &registered);
        assert_eq!(assessment.refusal.unwrap().code, "worktree-dirty");
    }

    #[test]
    fn remote_refs_never_cover_a_dirty_tree_that_has_an_archive() {
        let (_temporary, policy, registered, manager) = archive_fixture(true, true, true);

        let assessment = assess_one(&manager, &policy, &registered);
        assert_eq!(assessment.refusal.unwrap().code, "archive-unsupported");
    }

    #[test]
    fn hidden_state_relies_on_the_archive_only_of_a_dirty_tree_assessed_on_it() {
        let observed = |dirty, archived| {
            let (temporary, policy, registered, manager) = archive_fixture(dirty, true, archived);
            assess_one(&manager, &policy, &registered);
            let archive = temporary.path().join("archive_root/repo/archived");
            let calls = manager.git.hidden_archives.lock().unwrap().clone();
            (calls, archive)
        };

        let (dirty_archived, archive) = observed(true, true);
        assert_eq!(dirty_archived, vec![Some(archive)]);
        let (clean_archived, _) = observed(false, true);
        assert_eq!(clean_archived, vec![None]);
        let (clean_unarchived, _) = observed(false, false);
        assert_eq!(clean_unarchived, vec![None]);
    }

    #[test]
    fn gc_finishes_a_removal_that_git_interrupted() {
        let (_temporary, policy, registered, manager) = interrupted_removal_fixture(true);
        let ids = std::slice::from_ref(&registered.id);

        let first = manager.gc(&policy, ids, true).unwrap();
        let refusal = first[0].refusal.as_ref().unwrap();
        assert_eq!(refusal.code, "git-command-failed");
        assert!(
            refusal
                .message
                .contains("worktree gc --apply --id read-only")
        );
        assert!(registered.path.exists());

        let dry_run = manager.gc(&policy, ids, false).unwrap();
        assert!(dry_run[0].eligible, "{:?}", dry_run[0].refusal);

        let applied = manager.gc(&policy, ids, true).unwrap();
        let evidence = applied[0].evidence.as_ref().unwrap();
        assert_eq!(evidence.operation, "remove");
        assert_eq!(evidence.head.as_deref(), Some("abc"));
        assert!(!registered.path.exists());
        assert!(
            manager
                .registry()
                .removal(registered.id.as_str())
                .unwrap()
                .is_none()
        );
        assert_eq!(
            manager.registry().list().unwrap()[0].lifecycle,
            Lifecycle::Removed
        );
    }

    #[test]
    fn gc_retains_an_interrupted_removal_whose_residue_is_unproven() {
        let (_temporary, policy, registered, manager) = interrupted_removal_fixture(false);
        let ids = std::slice::from_ref(&registered.id);
        manager.gc(&policy, ids, true).unwrap();

        let applied = manager.gc(&policy, ids, true).unwrap();
        assert_eq!(
            applied[0].refusal.as_ref().unwrap().code,
            "removal-residue-unproven"
        );
        assert!(registered.path.exists());
        assert!(
            manager
                .registry()
                .removal(registered.id.as_str())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn gc_never_deletes_an_unlinked_tree_without_removal_intent() {
        let (_temporary, policy, registered, manager) = interrupted_removal_fixture(true);
        manager.git.snapshots.lock().unwrap().clear();
        manager.git.discovered.lock().unwrap().clear();

        let applied = manager
            .gc(&policy, std::slice::from_ref(&registered.id), true)
            .unwrap();
        assert!(applied[0].evidence.is_none());
        assert_eq!(
            applied[0].refusal.as_ref().unwrap().code,
            "worktree-not-found"
        );
        assert!(registered.path.exists());
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
            remove_fault: Some(RemoveFault::Refuse),
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
                .reconcile(
                    &policy,
                    std::slice::from_ref(&registered.id),
                    true,
                    false,
                    &[]
                )
                .unwrap_err()
                .code,
            "external-retirement-confirmation-required"
        );
        assert!(external.exists());

        let assessments = manager
            .reconcile(
                &policy,
                std::slice::from_ref(&registered.id),
                true,
                true,
                &[],
            )
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
        let selected = std::slice::from_ref(&registered.id);

        let dry_run = manager
            .reconcile(&policy, selected, false, false, &[])
            .unwrap();
        assert!(matches!(
            dry_run[0].action,
            ReconciliationAction::RetireExternal { .. }
        ));
        assert!(dry_run[0].eligible);

        assert_eq!(
            manager
                .reconcile(&policy, selected, true, false, &[])
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
            .reconcile(&policy, selected, true, true, &[])
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
                    &[],
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
                &[],
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
                    &[],
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
            .reconcile(
                &policy,
                std::slice::from_ref(&registered.id),
                true,
                true,
                &[],
            )
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
            .reconcile(
                &policy,
                std::slice::from_ref(&registered.id),
                false,
                false,
                &[],
            )
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
                &[],
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
                &[],
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

    #[test]
    fn readiness_requires_an_active_profile() {
        let ready = ReadinessObservation {
            git: true,
            config: true,
            registry: true,
            profiles: 1,
        };
        assert_eq!(readiness_failures(ready), [] as [ReadinessFailure; 0]);
        let no_profile = ReadinessObservation {
            profiles: 0,
            ..ready
        };
        assert_eq!(
            readiness_failures(no_profile),
            vec![ReadinessFailure::NoActiveProfile]
        );
        assert_eq!(
            ReadinessFailure::NoActiveProfile.to_string(),
            "no active profile"
        );
        let broken_config = ReadinessObservation {
            config: false,
            profiles: 0,
            ..ready
        };
        assert_eq!(
            readiness_failures(broken_config),
            vec![ReadinessFailure::ConfigUnavailable]
        );
    }

    /// A workspace with a repository, a managed root and two real tree directories:
    /// `<managed>/alpha` (id `alpha`) and `<legacy>/release-0.7.0` (id `release`).
    struct ReferenceWorld {
        _root: TempDir,
        workspace: PathBuf,
        managed: PathBuf,
        repository: PathBuf,
        alpha: PathBuf,
        dotted: PathBuf,
    }

    impl ReferenceWorld {
        fn new() -> Self {
            let root = tempdir().unwrap();
            let base = std::fs::canonicalize(root.path()).unwrap();
            let workspace = base.join("workspace");
            let repository = workspace.join("repo");
            let managed = base.join("managed");
            let alpha = managed.join("repo/alpha");
            let dotted = base.join("legacy/release-0.7.0");
            for directory in [&repository, &alpha, &dotted] {
                std::fs::create_dir_all(directory).unwrap();
            }
            Self {
                _root: root,
                workspace,
                managed,
                repository,
                alpha,
                dotted,
            }
        }

        fn record(&self, id: &str, path: &Path, lifecycle: Lifecycle) -> WorktreeRecord {
            named_record(id, self.repository.clone(), path.to_path_buf(), lifecycle)
        }

        fn manager(
            &self,
            records: Vec<WorktreeRecord>,
        ) -> WorktreeManager<FakeGit, FakeRegistry, FixedClock> {
            WorktreeManager::new(
                fake_git(self.repository.clone()),
                fake_registry(records),
                FixedClock,
            )
        }

        fn policy(&self) -> WorkspacePolicy {
            policy(self.workspace.clone(), self.managed.clone())
        }
    }

    #[test]
    fn a_reference_resolves_as_an_id_a_path_or_a_directory_name() {
        let world = ReferenceWorld::new();
        let manager = world.manager(vec![
            world.record("alpha", &world.alpha, Lifecycle::Finished),
            world.record("release", &world.dotted, Lifecycle::Active),
        ]);
        let scope = world.policy();

        let by_id = manager.resolve_reference("release", Some(&scope)).unwrap();
        assert_eq!(by_id.form, ReferenceForm::Id);
        assert_eq!(by_id.worktree_id.as_str(), "release");
        assert_eq!(by_id.path, world.dotted);
        assert_eq!(by_id.reference, "release");

        let by_path = manager
            .resolve_reference(world.dotted.to_str().unwrap(), Some(&scope))
            .unwrap();
        assert_eq!(by_path.form, ReferenceForm::Path);
        assert_eq!(by_path.worktree_id.as_str(), "release");

        let by_name = manager
            .resolve_reference("release-0.7.0", Some(&scope))
            .unwrap();
        assert_eq!(by_name.form, ReferenceForm::DirectoryName);
        assert_eq!(by_name.worktree_id.as_str(), "release");
        assert_eq!(by_name.path, world.dotted);

        // A directory name equal to the id is one record named twice, not an ambiguity.
        let same = manager.resolve_reference("alpha", None).unwrap();
        assert_eq!(same.form, ReferenceForm::Id);
        assert_eq!(same.worktree_id.as_str(), "alpha");
    }

    #[test]
    fn a_relative_path_resolves_against_the_working_directory() {
        let world = ReferenceWorld::new();
        let manager = world.manager(vec![world.record("alpha", &world.alpha, Lifecycle::Active)]);
        let relative = pathdiff(&world.alpha);
        let selection = manager
            .resolve_reference(relative.to_str().unwrap(), None)
            .unwrap();
        assert_eq!(selection.form, ReferenceForm::Path);
        assert_eq!(selection.worktree_id.as_str(), "alpha");
    }

    /// A relative path from the current directory to `target`, through the filesystem root.
    fn pathdiff(target: &Path) -> PathBuf {
        let current = std::fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
        let mut relative = PathBuf::new();
        for _ in current.components().skip(1) {
            relative.push("..");
        }
        relative.join(target.strip_prefix("/").unwrap())
    }

    #[test]
    fn two_records_sharing_a_directory_name_are_ambiguous() {
        let world = ReferenceWorld::new();
        let twin = world.dotted.parent().unwrap().join("alpha");
        std::fs::create_dir_all(&twin).unwrap();
        let manager = world.manager(vec![
            world.record("first", &world.alpha, Lifecycle::Finished),
            world.record("second", &twin, Lifecycle::Finished),
        ]);

        let refusal = manager
            .resolve_reference("alpha", Some(&world.policy()))
            .unwrap_err();
        assert_eq!(refusal.code, "ambiguous-worktree-reference");
        assert!(refusal.message.contains("first"), "{}", refusal.message);
        assert!(refusal.message.contains("second"), "{}", refusal.message);
    }

    #[test]
    fn an_id_and_a_directory_name_naming_different_records_are_ambiguous() {
        let world = ReferenceWorld::new();
        let manager = world.manager(vec![
            world.record("release-0", &world.alpha, Lifecycle::Active),
            world.record("other", &world.dotted, Lifecycle::Active),
        ]);
        // `alpha` is the directory of `release-0` and also, below, the id of another record.
        let manager_with_id = world.manager(vec![
            world.record("alpha", &world.dotted, Lifecycle::Active),
            world.record("named", &world.alpha, Lifecycle::Active),
        ]);
        assert_eq!(
            manager.resolve_reference("alpha", None).unwrap().form,
            ReferenceForm::DirectoryName
        );
        let refusal = manager_with_id
            .resolve_reference("alpha", None)
            .unwrap_err();
        assert_eq!(refusal.code, "ambiguous-worktree-reference");
        assert!(refusal.message.contains("alpha") && refusal.message.contains("named"));
    }

    #[test]
    fn an_unknown_reference_names_the_value() {
        let world = ReferenceWorld::new();
        let manager = world.manager(vec![world.record("alpha", &world.alpha, Lifecycle::Active)]);
        // A well-formed id keeps the released `unknown-worktree-id` code.
        let refusal = manager.resolve_reference("missing", None).unwrap_err();
        assert_eq!(refusal.code, "unknown-worktree-id");
        assert!(refusal.message.contains("missing"), "{}", refusal.message);
        // Anything else keeps `invalid-worktree-id`, and says it named no tree either.
        for value in ["missing-0.1.0", "/no/such/tree", "Upper"] {
            let refusal = manager.resolve_reference(value, None).unwrap_err();
            assert_eq!(refusal.code, "invalid-worktree-id", "{value}");
            assert!(refusal.message.contains(value), "{}", refusal.message);
            assert!(
                refusal.message.contains(
                    "neither a valid worktree id nor a registered tree path or directory name"
                ),
                "{}",
                refusal.message
            );
        }
    }

    #[test]
    fn a_directory_name_counts_only_records_in_scope_and_never_removed_ones() {
        let world = ReferenceWorld::new();
        let elsewhere = world.dotted.parent().unwrap().join("alpha");
        std::fs::create_dir_all(&elsewhere).unwrap();
        let mut outside = world.record("outside", &elsewhere, Lifecycle::Active);
        outside.repository_root = PathBuf::from("/elsewhere/repo");
        let removed_path = world.managed.join("repo/gone");
        let manager = world.manager(vec![
            world.record("first", &world.alpha, Lifecycle::Active),
            outside,
            world.record("gone-tree", &removed_path, Lifecycle::Removed),
        ]);

        // Out of the policy's workspace, the twin directory name does not count.
        let selection = manager
            .resolve_reference("alpha", Some(&world.policy()))
            .unwrap();
        assert_eq!(selection.worktree_id.as_str(), "first");
        assert_eq!(
            manager.resolve_reference("alpha", None).unwrap_err().code,
            "ambiguous-worktree-reference"
        );
        // A removed record answers to its id only.
        assert_eq!(
            manager.resolve_reference("gone", None).unwrap_err().code,
            "unknown-worktree-id"
        );
        assert_eq!(
            manager
                .resolve_reference("gone-tree", None)
                .unwrap()
                .worktree_id
                .as_str(),
            "gone-tree"
        );
    }

    #[test]
    fn a_record_whose_path_is_gone_still_resolves_by_id() {
        let world = ReferenceWorld::new();
        let missing = world.managed.join("repo/missing");
        let manager = world.manager(vec![world.record("missing", &missing, Lifecycle::Active)]);
        let ids = manager
            .resolve_references(
                &["missing".to_owned(), "missing".to_owned()],
                Some(&world.policy()),
            )
            .unwrap();
        assert_eq!(ids, vec![WorktreeId::new("missing").unwrap()]);
    }

    #[test]
    fn a_tree_argument_keeps_existing_directories_and_resolves_registered_names() {
        let world = ReferenceWorld::new();
        let manager = world.manager(vec![
            world.record("alpha", &world.alpha, Lifecycle::Active),
            world.record("release", &world.dotted, Lifecycle::Active),
        ]);
        // An existing directory that names no registered tree in any form passes through
        // unchanged.
        assert_eq!(
            manager.resolve_tree_path(&world.repository).unwrap(),
            world.repository
        );
        assert_eq!(
            manager.resolve_tree_path(&world.alpha).unwrap(),
            world.alpha
        );
        assert_eq!(
            manager.resolve_tree_path(Path::new("release")).unwrap(),
            world.dotted
        );
        assert_eq!(
            manager
                .resolve_tree_path(Path::new("release-0.7.0"))
                .unwrap(),
            world.dotted
        );
        let refusal = manager
            .resolve_tree_path(Path::new("not-registered"))
            .unwrap_err();
        assert_eq!(refusal.code, "worktree-not-found");
        assert!(refusal.message.contains("not-registered"));
        assert!(
            refusal
                .message
                .contains("neither an existing path nor a registered id or tree directory name")
        );
        // An ambiguity is not a missing tree and keeps its own code.
        let twin = world.repository.parent().unwrap().join("alpha");
        std::fs::create_dir_all(&twin).unwrap();
        let ambiguous = world.manager(vec![
            world.record("first", &world.alpha, Lifecycle::Active),
            world.record("second", &twin, Lifecycle::Active),
        ]);
        assert_eq!(
            ambiguous
                .resolve_tree_path(Path::new("alpha"))
                .unwrap_err()
                .code,
            "ambiguous-worktree-reference"
        );
    }

    #[test]
    fn a_dotted_id_is_refused_with_hyphen_guidance() {
        let refusal = WorktreeId::new("hard-defects-0.7.0").unwrap_err();
        assert_eq!(refusal.code, "invalid-worktree-id");
        assert!(refusal.message.contains("hyphens instead of dots"));
    }
}
