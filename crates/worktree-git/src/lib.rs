//! Git CLI adapter. Commands are executed directly, never through a shell.

use b10x_worktree::GitPort;
use b10x_worktree_domain::{
    CreatePlan, DiscoveredWorktree, RecoveryEvidence, RecoveryKind, Refusal, RepositorySnapshot,
    WorktreeSnapshot,
};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

mod inspection;

/// Confirmed advertised commits, each mapped to the `remote:ref` names advertising it.
type AdvertisedTips = BTreeMap<String, Vec<String>>;

/// Process-backed Git port.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessGit;

impl ProcessGit {
    fn output_bytes<I, S>(repository: &Path, args: I) -> Result<Vec<u8>, Refusal>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .output()
            .map_err(|error| Refusal::new("git-unavailable", error.to_string()))?;
        if !output.status.success() {
            return Err(Refusal::new(
                "git-command-failed",
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        Ok(output.stdout)
    }

    fn output<I, S>(repository: &Path, args: I) -> Result<String, Refusal>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        String::from_utf8(Self::output_bytes(repository, args)?)
            .map_err(|error| Refusal::new("git-output-not-utf8", error.to_string()))
    }

    fn status<I, S>(repository: &Path, args: I) -> Result<(), Refusal>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::output(repository, args).map(|_| ())
    }

    fn existing_ancestor(path: &Path) -> Result<&Path, Refusal> {
        let mut candidate = path;
        loop {
            match std::fs::metadata(candidate) {
                Ok(_) => return Ok(candidate),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(Refusal::new(
                        "move-worktree-parent-inspection-failed",
                        format!("{}: {error}", candidate.display()),
                    ));
                }
            }
            candidate = candidate.parent().ok_or_else(|| {
                Refusal::new(
                    "move-worktree-parent-missing",
                    format!("{} has no existing ancestor", path.display()),
                )
            })?;
        }
    }

    fn canonicalize_future_path(path: &Path) -> Result<PathBuf, Refusal> {
        if !path.is_absolute() {
            return Err(Refusal::new(
                "relative-worktree-path",
                format!("{} is not absolute", path.display()),
            ));
        }
        let mut suffix = Vec::new();
        let mut existing = path;
        loop {
            match std::fs::metadata(existing) {
                Ok(metadata) => {
                    if !metadata.is_dir() {
                        return Err(Refusal::new(
                            "worktree-path-ancestor-not-directory",
                            format!("{} is not a directory", existing.display()),
                        ));
                    }
                    break;
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    if std::fs::symlink_metadata(existing).is_ok() {
                        return Err(Refusal::new(
                            "worktree-path-dangling-symlink",
                            format!("{} is a dangling symlink", existing.display()),
                        ));
                    }
                    let component = existing.file_name().ok_or_else(|| {
                        Refusal::new(
                            "worktree-path-invalid",
                            format!("{} has no existing ancestor", path.display()),
                        )
                    })?;
                    suffix.push(component.to_os_string());
                    existing = existing.parent().ok_or_else(|| {
                        Refusal::new(
                            "worktree-path-invalid",
                            format!("{} has no existing ancestor", path.display()),
                        )
                    })?;
                }
                Err(error) => {
                    return Err(Refusal::new(
                        "worktree-path-inspection-failed",
                        format!("{}: {error}", existing.display()),
                    ));
                }
            }
        }
        let mut canonical = std::fs::canonicalize(existing).map_err(|error| {
            Refusal::new(
                "worktree-path-inspection-failed",
                format!("{}: {error}", existing.display()),
            )
        })?;
        for component in suffix.into_iter().rev() {
            canonical.push(component);
        }
        Ok(canonical)
    }

    fn require_canonical_future_path(path: &Path) -> Result<(), Refusal> {
        let canonical = Self::canonicalize_future_path(path)?;
        if canonical != path {
            return Err(Refusal::new(
                "non-canonical-worktree-path",
                format!(
                    "worktree path {} resolves to {}",
                    path.display(),
                    canonical.display()
                ),
            ));
        }
        Ok(())
    }

    fn remote_names(repository: &Path) -> Result<Vec<String>, Refusal> {
        let mut remotes = Self::output(repository, ["remote"])?
            .lines()
            .map(str::trim)
            .filter(|remote| !remote.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        remotes.sort();
        remotes.dedup();
        Ok(remotes)
    }

    fn advertised_refs(
        repository: &Path,
        remote: &str,
    ) -> Result<BTreeMap<String, String>, Refusal> {
        let output = Self::output(repository, ["ls-remote", "--quiet", "--refs", "--", remote])
            .map_err(|error| {
                Refusal::new(
                    "remote-advertisement-failed",
                    format!("{remote}: {}", error.message),
                )
            })?;
        let mut refs = BTreeMap::new();
        for line in output.lines() {
            let Some((object, reference)) = line.split_once('\t') else {
                return Err(Refusal::new(
                    "invalid-remote-advertisement",
                    format!("{remote} advertised an invalid ref line"),
                ));
            };
            if !reference.starts_with("refs/")
                || !matches!(object.len(), 40 | 64)
                || !object.bytes().all(|byte| byte.is_ascii_hexdigit())
            {
                return Err(Refusal::new(
                    "invalid-remote-advertisement",
                    format!("{remote} advertised an invalid ref"),
                ));
            }
            if refs
                .insert(reference.to_owned(), object.to_owned())
                .is_some_and(|previous| previous != object)
            {
                return Err(Refusal::new(
                    "ambiguous-remote-advertisement",
                    format!("{remote} advertised {reference} more than once"),
                ));
            }
        }
        Ok(refs)
    }

    fn fetch_advertised_refs(
        repository: &Path,
        remote: &str,
        refs: &BTreeMap<String, String>,
    ) -> Result<(), Refusal> {
        if refs.is_empty() {
            return Ok(());
        }

        // Source-only refspecs fetch the advertised objects without creating or updating any
        // local ref. In particular, a remote tag can never overwrite or prune a local tag.
        let mut child = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args([
                "fetch",
                "--quiet",
                "--no-tags",
                "--no-prune",
                "--no-prune-tags",
                "--no-write-fetch-head",
                "--no-recurse-submodules",
                "--filter=blob:none",
                "--stdin",
            ])
            .arg("--")
            .arg(remote)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| Refusal::new("git-unavailable", error.to_string()))?;
        {
            let stdin = child.stdin.as_mut().ok_or_else(|| {
                Refusal::new("remote-refresh-failed", "git fetch stdin was unavailable")
            })?;
            for reference in refs.keys() {
                writeln!(stdin, "{reference}").map_err(|error| {
                    Refusal::new(
                        "remote-refresh-failed",
                        format!("could not request {remote} refs: {error}"),
                    )
                })?;
            }
        }
        let output = child.wait_with_output().map_err(|error| {
            Refusal::new(
                "remote-refresh-failed",
                format!("could not wait for {remote}: {error}"),
            )
        })?;
        if output.status.success() {
            Ok(())
        } else {
            Err(Refusal::new(
                "remote-refresh-failed",
                format!(
                    "{remote}: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            ))
        }
    }

    fn commitish_exists(repository: &Path, object: &str) -> Result<bool, Refusal> {
        let disabled_grafts = tempfile::NamedTempFile::new()
            .map_err(|error| Refusal::new("graft-isolation-failed", error.to_string()))?;
        let revision = format!("{object}^{{commit}}");
        let output = Command::new("git")
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(repository)
            .args(["cat-file", "-e", revision.as_str()])
            .env("GIT_GRAFT_FILE", disabled_grafts.path())
            .output()
            .map_err(|error| Refusal::new("git-unavailable", error.to_string()))?;
        Ok(output.status.success())
    }

    /// Run one ancestry query with replacement objects and graft ancestry disabled.
    fn containment_output<I, S>(repository: &Path, args: I) -> Result<String, Refusal>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let disabled_grafts = tempfile::NamedTempFile::new()
            .map_err(|error| Refusal::new("graft-isolation-failed", error.to_string()))?;
        let output = Command::new("git")
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(repository)
            .args(args)
            .env("GIT_GRAFT_FILE", disabled_grafts.path())
            .output()
            .map_err(|error| Refusal::new("git-unavailable", error.to_string()))?;
        if !output.status.success() {
            return Err(Refusal::new(
                "git-command-failed",
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            ));
        }
        String::from_utf8(output.stdout)
            .map_err(|error| Refusal::new("git-output-not-utf8", error.to_string()))
    }

    /// Refuse ancestry questions while a graft file can rewrite the answer.
    fn require_no_grafts(repository: &Path) -> Result<(), Refusal> {
        let grafts = Self::absolute_git_path(repository, "--git-common-dir")?.join("info/grafts");
        if Self::path_exists(&grafts)? {
            return Err(Refusal::new(
                "git-grafts-present",
                format!(
                    "{} can rewrite ancestry and must be removed before recovery proof",
                    grafts.display()
                ),
            ));
        }
        Ok(())
    }

    fn validate_object_id(object: &str) -> Result<(), Refusal> {
        if !matches!(object.len(), 40 | 64) || !object.bytes().all(|byte| byte.is_ascii_hexdigit())
        {
            return Err(Refusal::new(
                "invalid-object-id",
                "Git object id must be a full SHA-1 or SHA-256 hexadecimal id",
            ));
        }
        Ok(())
    }

    fn contains_commit(repository: &Path, head: &str, tip: &str) -> Result<bool, Refusal> {
        let disabled_grafts = tempfile::NamedTempFile::new()
            .map_err(|error| Refusal::new("graft-isolation-failed", error.to_string()))?;
        let tip = format!("{tip}^{{commit}}");
        let output = Command::new("git")
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(repository)
            .args(["merge-base", "--is-ancestor", head, tip.as_str()])
            .env("GIT_GRAFT_FILE", disabled_grafts.path())
            .output()
            .map_err(|error| Refusal::new("git-unavailable", error.to_string()))?;
        match output.status.code() {
            Some(0) => Ok(true),
            Some(1) => Ok(false),
            _ => Err(Refusal::new(
                "git-command-failed",
                String::from_utf8_lossy(&output.stderr).trim().to_owned(),
            )),
        }
    }

    /// Return the exact advertised refs containing `head`, and every confirmed advertised tip.
    ///
    /// A tip is confirmed when the final re-advertisement lists it and its commit is local. The
    /// tips map each commit to the `remote:ref` names advertising it.
    fn observe_recovery(
        repository: &Path,
        head: &str,
    ) -> Result<(Vec<String>, AdvertisedTips), Refusal> {
        Self::validate_object_id(head)?;
        Self::require_no_grafts(repository)?;
        if !Self::commitish_exists(repository, head)? {
            return Err(Refusal::new(
                "invalid-recovery-head",
                format!("{head} is not a commit"),
            ));
        }
        let mut refs = Vec::new();
        let mut tips = AdvertisedTips::new();
        for remote in Self::remote_names(repository)? {
            let advertised = Self::advertised_refs(repository, &remote)?;
            let mut already_proves_recovery = false;
            let mut missing = BTreeMap::new();
            for (reference, tip) in &advertised {
                if Self::commitish_exists(repository, tip)? {
                    already_proves_recovery |= Self::contains_commit(repository, head, tip)?;
                } else {
                    missing.insert(reference.clone(), tip.clone());
                }
            }
            if !already_proves_recovery {
                Self::fetch_advertised_refs(repository, &remote, &missing)?;
            }

            // Observation and any fetch are separate protocol transactions. Re-advertise and only
            // report these final remote facts. A racing update whose object is not available
            // locally is conservatively ignored.
            for (reference, tip) in Self::advertised_refs(repository, &remote)? {
                if !Self::commitish_exists(repository, &tip)? {
                    continue;
                }
                let label = format!("{remote}:{reference}");
                if Self::contains_commit(repository, head, &tip)? {
                    refs.push(label.clone());
                }
                tips.entry(tip).or_default().push(label);
            }
        }
        refs.sort();
        refs.dedup();
        Ok((refs, tips))
    }

    /// Prove that one advertised tip carries a verbatim patch-identical commit for every commit
    /// `head` adds over all confirmed tips.
    ///
    /// Returns evidence with no refs when nothing proves it. A root, merge or empty commit among
    /// the unique commits has no single patch another commit could carry, so it defeats the proof.
    fn patch_equivalence(
        repository: &Path,
        head: &str,
        tips: &AdvertisedTips,
    ) -> Result<RecoveryEvidence, Refusal> {
        let unproven = RecoveryEvidence::default();
        if tips.is_empty() {
            return Ok(unproven);
        }
        let mut unique_range = vec![head.to_owned(), "--not".to_owned()];
        unique_range.extend(tips.keys().cloned());

        let mut listing = vec!["rev-list".to_owned(), "--parents".to_owned()];
        listing.extend(unique_range.iter().cloned());
        let mut unique = Vec::new();
        for line in Self::containment_output(repository, &listing)?.lines() {
            let mut fields = line.split_whitespace();
            let Some(commit) = fields.next() else {
                continue;
            };
            if fields.count() != 1 {
                return Ok(unproven);
            }
            unique.push(commit.to_owned());
        }
        if unique.is_empty() {
            return Ok(unproven);
        }
        let unique_ids = Self::verbatim_patch_ids(repository, &unique_range)?;
        let Some(wanted) = unique
            .iter()
            .map(|commit| unique_ids.get(commit).cloned())
            .collect::<Option<std::collections::BTreeSet<_>>>()
        else {
            return Ok(unproven);
        };

        let mut candidates = tips.iter().collect::<Vec<_>>();
        candidates.sort_by_key(|(tip, labels)| {
            let rank = labels
                .iter()
                .map(|label| ref_rank(label))
                .min()
                .unwrap_or(u8::MAX);
            (rank, (*tip).clone())
        });
        for (tip, labels) in candidates {
            let symmetric = format!("{tip}...{head}");
            // Git's own whitespace-insensitive equivalence must hold for every commit on the
            // HEAD side, merges included, before the stricter verbatim comparison is attempted.
            let loose = Self::containment_output(
                repository,
                [
                    "rev-list",
                    "--right-only",
                    "--cherry-pick",
                    symmetric.as_str(),
                ],
            )?;
            if !loose.trim().is_empty() {
                continue;
            }
            let carried = Self::verbatim_patch_ids(
                repository,
                &[
                    "--no-merges".to_owned(),
                    "--left-only".to_owned(),
                    symmetric,
                ],
            )?
            .into_values()
            .collect::<std::collections::BTreeSet<_>>();
            if wanted.is_subset(&carried) {
                let mut refs = labels.clone();
                refs.sort();
                return Ok(RecoveryEvidence {
                    kind: RecoveryKind::PatchEquivalent,
                    refs,
                    equivalent_commits: unique,
                });
            }
        }
        Ok(unproven)
    }

    /// Map each commit selected by `revisions` to its whitespace-exact patch id.
    ///
    /// A commit with an empty diff has no id and is absent from the map. Binary changes are
    /// compared by their full binary patch, so two different binary edits never match.
    fn verbatim_patch_ids(
        repository: &Path,
        revisions: &[String],
    ) -> Result<BTreeMap<String, String>, Refusal> {
        let disabled_grafts = tempfile::NamedTempFile::new()
            .map_err(|error| Refusal::new("graft-isolation-failed", error.to_string()))?;
        let log_errors = tempfile::tempfile()
            .map_err(|error| Refusal::new("patch-id-failed", error.to_string()))?;
        let log_stderr = log_errors
            .try_clone()
            .map_err(|error| Refusal::new("patch-id-failed", error.to_string()))?;
        let mut log = Command::new("git")
            .arg("--no-replace-objects")
            .arg("-C")
            .arg(repository)
            .args([
                "log",
                "--patch",
                "--binary",
                "--full-index",
                "--no-color",
                "--no-ext-diff",
                "--no-textconv",
                "--no-renames",
                "--no-show-signature",
                "--diff-merges=off",
                "--src-prefix=a/",
                "--dst-prefix=b/",
                "--format=commit %H",
            ])
            .args(revisions)
            .env("GIT_GRAFT_FILE", disabled_grafts.path())
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::from(log_stderr))
            .spawn()
            .map_err(|error| Refusal::new("git-unavailable", error.to_string()))?;
        let patch = log
            .stdout
            .take()
            .ok_or_else(|| Refusal::new("patch-id-failed", "git log stdout was unavailable"))?;
        let ids = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(["patch-id", "--verbatim"])
            .stdin(Stdio::from(patch))
            .output()
            .map_err(|error| Refusal::new("git-unavailable", error.to_string()));
        let status = log
            .wait()
            .map_err(|error| Refusal::new("patch-id-failed", error.to_string()))?;
        if !status.success() {
            let mut message = String::new();
            let mut errors = log_errors;
            let _ = std::io::Seek::rewind(&mut errors);
            let _ = std::io::Read::read_to_string(&mut errors, &mut message);
            return Err(Refusal::new(
                "git-command-failed",
                message.trim().to_owned(),
            ));
        }
        let ids = ids?;
        if !ids.status.success() {
            return Err(Refusal::new(
                "patch-id-failed",
                String::from_utf8_lossy(&ids.stderr).trim().to_owned(),
            ));
        }
        let output = String::from_utf8(ids.stdout)
            .map_err(|error| Refusal::new("git-output-not-utf8", error.to_string()))?;
        let mut map = BTreeMap::new();
        for line in output.lines() {
            let Some((patch_id, commit)) = line.split_once(' ') else {
                return Err(Refusal::new(
                    "patch-id-failed",
                    "git patch-id emitted an invalid line",
                ));
            };
            map.insert(commit.trim().to_owned(), patch_id.to_owned());
        }
        Ok(map)
    }

    fn absolute_git_path(repository: &Path, argument: &str) -> Result<PathBuf, Refusal> {
        let path = PathBuf::from(
            Self::output(
                repository,
                ["rev-parse", "--path-format=absolute", argument],
            )?
            .trim(),
        );
        std::fs::canonicalize(&path).map_err(|error| {
            Refusal::new(
                "git-directory-not-found",
                format!("{}: {error}", path.display()),
            )
        })
    }

    fn path_exists(path: &Path) -> Result<bool, Refusal> {
        match std::fs::symlink_metadata(path) {
            Ok(_) => Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(Refusal::new(
                "git-lock-inspection-failed",
                format!("{}: {error}", path.display()),
            )),
        }
    }

    fn contains_lock_file(path: &Path) -> Result<bool, Refusal> {
        let entries = match std::fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(Refusal::new(
                    "git-lock-inspection-failed",
                    format!("{}: {error}", path.display()),
                ));
            }
        };
        for entry in entries {
            let entry = entry
                .map_err(|error| Refusal::new("git-lock-inspection-failed", error.to_string()))?;
            let entry_path = entry.path();
            if entry_path.extension() == Some(OsStr::new("lock")) {
                return Ok(true);
            }
            if entry
                .file_type()
                .map_err(|error| {
                    Refusal::new(
                        "git-lock-inspection-failed",
                        format!("{}: {error}", entry_path.display()),
                    )
                })?
                .is_dir()
                && Self::contains_lock_file(&entry_path)?
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn has_operational_lock(repository: &Path) -> Result<bool, Refusal> {
        let git_dir = Self::absolute_git_path(repository, "--git-dir")?;
        let common_dir = Self::absolute_git_path(repository, "--git-common-dir")?;

        // Git can report a perfectly clean index while a sequencer operation is paused. These
        // per-worktree markers are therefore as removal-blocking as an actual lock file: deleting
        // the linked tree would also delete the only state needed to continue or abort it.
        //
        // `REBASE_HEAD` is **not** among them, and that is the whole of this list's subtlety. Git
        // writes it during a rebase and leaves it behind when one finishes; a rebase is in progress
        // if and only if `rebase-merge` or `rebase-apply` exists. Counting the leftover marker made
        // `finish` refuse a clean, idle worktree with `worktree-locked: Git marks the worktree
        // locked`, for a lock Git does not report — measured 2026-09-04 on two managed worktrees
        // whose rebases had completed, where `git worktree list --porcelain` showed no lock and
        // neither rebase directory existed.
        for relative in [
            "rebase-merge",
            "rebase-apply",
            "sequencer",
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "BISECT_LOG",
            "BISECT_START",
        ] {
            if Self::path_exists(&git_dir.join(relative))? {
                return Ok(true);
            }
        }

        for relative in [
            "index.lock",
            "HEAD.lock",
            "ORIG_HEAD.lock",
            "FETCH_HEAD.lock",
            "config.worktree.lock",
            "logs/HEAD.lock",
        ] {
            if Self::path_exists(&git_dir.join(relative))? {
                return Ok(true);
            }
        }
        for relative in ["config.lock", "packed-refs.lock", "shallow.lock"] {
            if Self::path_exists(&common_dir.join(relative))? {
                return Ok(true);
            }
        }
        for root in [
            git_dir.join("refs"),
            git_dir.join("logs/refs"),
            common_dir.join("refs"),
            common_dir.join("logs/refs"),
            common_dir.join("reftable"),
        ] {
            if Self::contains_lock_file(&root)? {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Order in which advertised refs are tried as patch-equivalence proof: default branches first.
fn ref_rank(label: &str) -> u8 {
    let reference = label
        .split_once(':')
        .map_or(label, |(_, reference)| reference);
    match reference {
        "refs/heads/main" => 0,
        "refs/heads/master" => 1,
        _ if reference.starts_with("refs/heads/") => 2,
        _ if reference.starts_with("refs/tags/") => 3,
        _ => 4,
    }
}

impl GitPort for ProcessGit {
    fn repository_snapshot(&self, repository: &Path) -> Result<RepositorySnapshot, Refusal> {
        let root = self
            .list_worktrees(repository)?
            .into_iter()
            .find(|worktree| worktree.primary)
            .map(|worktree| worktree.path)
            .ok_or_else(|| {
                Refusal::new(
                    "repository-not-found",
                    format!("{} has no primary worktree", repository.display()),
                )
            })?;
        let name = root
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| Refusal::new("invalid-repository-name", root.display().to_string()))?
            .to_owned();
        let head = Self::output(&root, ["rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        Ok(RepositorySnapshot { root, name, head })
    }

    fn resolve_revision(&self, repository: &Path, revision: &str) -> Result<String, Refusal> {
        let commit = format!("{revision}^{{commit}}");
        let resolved = Self::output(
            repository,
            ["rev-parse", "--verify", "--end-of-options", commit.as_str()],
        )
        .map_err(|error| Refusal::new("revision-not-found", error.message))?
        .trim()
        .to_owned();
        Self::validate_object_id(&resolved)?;
        Ok(resolved)
    }

    fn worktree_snapshot(
        &self,
        repository: &Path,
        worktree: &Path,
    ) -> Result<WorktreeSnapshot, Refusal> {
        let path = std::fs::canonicalize(worktree).map_err(|error| {
            Refusal::new(
                "worktree-not-found",
                format!("{}: {error}", worktree.display()),
            )
        })?;
        let linked = self
            .list_worktrees(repository)?
            .into_iter()
            .find(|item| item.path == path && !item.primary)
            .ok_or_else(|| {
                Refusal::new(
                    "worktree-not-linked",
                    format!(
                        "{} is not a linked worktree of {}",
                        path.display(),
                        repository.display()
                    ),
                )
            })?;
        let head = Self::output(&path, ["rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let dirty = !Self::output(
            &path,
            [
                "--no-optional-locks",
                "status",
                "--porcelain=v1",
                "--untracked-files=all",
                "--ignored=matching",
            ],
        )?
        .is_empty();
        let locked = linked.locked || Self::has_operational_lock(&path)?;
        Ok(WorktreeSnapshot {
            path,
            head,
            dirty,
            locked,
        })
    }

    fn create_detached(&self, plan: &CreatePlan) -> Result<(), Refusal> {
        match std::fs::symlink_metadata(&plan.path) {
            Ok(_) => {
                return Err(Refusal::new(
                    "worktree-path-exists",
                    plan.path.display().to_string(),
                ));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(Refusal::new(
                    "worktree-path-inspection-failed",
                    format!("{}: {error}", plan.path.display()),
                ));
            }
        }
        Self::require_canonical_future_path(&plan.path)?;
        if let Some(parent) = plan.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Refusal::new("create-worktree-parent-failed", error.to_string())
            })?;
        }
        Self::require_canonical_future_path(&plan.path)?;
        Self::status(
            &plan.repository_root,
            [
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                plan.path.as_os_str(),
                OsStr::new(plan.base.as_str()),
            ],
        )
    }

    fn recovery_refs(&self, repository: &Path, head: &str) -> Result<Vec<String>, Refusal> {
        Self::observe_recovery(repository, head).map(|(refs, _)| refs)
    }

    fn recovery_evidence(
        &self,
        repository: &Path,
        head: &str,
    ) -> Result<RecoveryEvidence, Refusal> {
        let (refs, tips) = Self::observe_recovery(repository, head)?;
        if !refs.is_empty() {
            return Ok(RecoveryEvidence::ancestor(refs));
        }
        Self::patch_equivalence(repository, head, &tips)
    }

    fn containing_refs(
        &self,
        repository: &Path,
        head: &str,
    ) -> Result<Option<Vec<String>>, Refusal> {
        Self::validate_object_id(head)?;
        Self::require_no_grafts(repository)?;
        if !Self::commitish_exists(repository, head)? {
            return Ok(None);
        }
        let mut refs = Self::containment_output(
            repository,
            ["for-each-ref", "--format=%(refname)", "--contains", head],
        )?
        .lines()
        .map(str::trim)
        .filter(|reference| !reference.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
        refs.sort();
        refs.dedup();
        Ok(Some(refs))
    }

    fn remove(&self, repository: &Path, worktree: &Path) -> Result<(), Refusal> {
        make_directories_owner_writable(worktree)?;
        Self::status(
            repository,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                worktree.as_os_str(),
            ],
        )
    }

    #[cfg(unix)]
    fn verify_removal_residue(
        &self,
        repository: &Path,
        worktree: &Path,
        head: &str,
    ) -> Result<(), Refusal> {
        Self::validate_object_id(head)?;
        verify_residue(repository, worktree, head)
    }

    #[cfg(unix)]
    fn delete_residue(&self, worktree: &Path) -> Result<(), Refusal> {
        let is_directory = std::fs::symlink_metadata(worktree)
            .map_err(|error| {
                Refusal::new(
                    "worktree-path-inspection-failed",
                    format!("{}: {error}", worktree.display()),
                )
            })?
            .is_dir();
        if !is_directory {
            return Err(residue_unproven(worktree, "is not a directory"));
        }
        make_directories_owner_writable(worktree)?;
        std::fs::remove_dir_all(worktree).map_err(|error| {
            Refusal::new(
                "residue-delete-failed",
                format!("{}: {error}", worktree.display()),
            )
        })
    }

    fn move_worktree(&self, repository: &Path, from: &Path, to: &Path) -> Result<(), Refusal> {
        Self::require_canonical_future_path(to)?;
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| Refusal::new("move-worktree-parent-failed", error.to_string()))?;
        }
        Self::require_canonical_future_path(to)?;
        Self::status(
            repository,
            [
                OsStr::new("worktree"),
                OsStr::new("move"),
                from.as_os_str(),
                to.as_os_str(),
            ],
        )
        .map_err(|error| Refusal::new("move-worktree-failed", error.message))
    }

    fn validate_move_worktree(&self, from: &Path, to: &Path) -> Result<(), Refusal> {
        let destination_parent = Self::existing_ancestor(to)?;
        same_filesystem(from, destination_parent)
    }

    fn list_worktrees(&self, repository: &Path) -> Result<Vec<DiscoveredWorktree>, Refusal> {
        let output = Self::output_bytes(repository, ["worktree", "list", "--porcelain", "-z"])?;
        let mut result = Vec::new();
        let mut path = None;
        let mut head = None;
        let mut locked = false;
        let mut index = 0usize;
        let flush = |result: &mut Vec<DiscoveredWorktree>,
                     path: &mut Option<PathBuf>,
                     head: &mut Option<String>,
                     locked: &mut bool,
                     index: &mut usize|
         -> Result<(), Refusal> {
            if let Some(value) = path.take() {
                if !value.is_absolute() {
                    return Err(Refusal::new(
                        "invalid-worktree-list",
                        "Git reported a non-absolute worktree path",
                    ));
                }
                let canonical = match std::fs::canonicalize(&value) {
                    Ok(canonical) => canonical,
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => value,
                    Err(error) => {
                        return Err(Refusal::new(
                            "worktree-path-inspection-failed",
                            format!("{}: {error}", value.display()),
                        ));
                    }
                };
                result.push(DiscoveredWorktree {
                    path: canonical,
                    head: head.take(),
                    locked: *locked,
                    primary: *index == 0,
                });
                *locked = false;
                *index += 1;
            }
            Ok(())
        };
        for field in output
            .split(|byte| *byte == 0)
            .chain(std::iter::once(&[][..]))
        {
            if field.is_empty() {
                flush(&mut result, &mut path, &mut head, &mut locked, &mut index)?;
            } else if let Some(value) = field.strip_prefix(b"worktree ") {
                path = Some(path_from_git_bytes(value)?);
            } else if let Some(value) = field.strip_prefix(b"HEAD ") {
                head = Some(
                    std::str::from_utf8(value)
                        .map_err(|error| Refusal::new("git-output-not-utf8", error.to_string()))?
                        .to_owned(),
                );
            } else if field == b"locked" || field.starts_with(b"locked ") {
                locked = true;
            }
        }
        Ok(result)
    }
}

#[cfg(unix)]
fn path_from_git_bytes(value: &[u8]) -> Result<PathBuf, Refusal> {
    use std::os::unix::ffi::OsStringExt as _;

    if value.is_empty() {
        return Err(Refusal::new(
            "invalid-worktree-list",
            "Git reported an empty worktree path",
        ));
    }
    Ok(PathBuf::from(std::ffi::OsString::from_vec(value.to_vec())))
}

#[cfg(not(unix))]
fn path_from_git_bytes(value: &[u8]) -> Result<PathBuf, Refusal> {
    if value.is_empty() {
        return Err(Refusal::new(
            "invalid-worktree-list",
            "Git reported an empty worktree path",
        ));
    }
    Ok(PathBuf::from(std::str::from_utf8(value).map_err(
        |error| Refusal::new("git-output-not-utf8", error.to_string()),
    )?))
}

#[cfg(unix)]
fn same_filesystem(from: &Path, to: &Path) -> Result<(), Refusal> {
    use std::os::unix::fs::MetadataExt as _;

    let from_device = std::fs::metadata(from)
        .map_err(|error| Refusal::new("worktree-metadata-failed", error.to_string()))?
        .dev();
    let to_device = std::fs::metadata(to)
        .map_err(|error| Refusal::new("worktree-metadata-failed", error.to_string()))?
        .dev();
    if from_device == to_device {
        Ok(())
    } else {
        Err(Refusal::new(
            "cross-device-worktree-move",
            format!(
                "{} and {} are on different filesystems",
                from.display(),
                to.display()
            ),
        ))
    }
}

#[cfg(not(unix))]
fn same_filesystem(_from: &Path, _to: &Path) -> Result<(), Refusal> {
    Ok(())
}

/// Give the owner full access to every directory below a tree that is about to be deleted.
///
/// A directory without the owner write bit makes Git's deletion of its files fail after Git has
/// already committed to unlinking the tree. Directory modes are not tracked, so this changes no
/// Git state. The walk follows no symlink, stays on the tree's filesystem, and leaves directories
/// another user owns untouched.
#[cfg(unix)]
fn make_directories_owner_writable(root: &Path) -> Result<(), Refusal> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let failed = |path: &Path, error: std::io::Error| {
        Refusal::new(
            "worktree-permission-repair-failed",
            format!("{}: {error}", path.display()),
        )
    };
    let root_metadata = std::fs::symlink_metadata(root).map_err(|error| failed(root, error))?;
    if !root_metadata.is_dir() {
        return Ok(());
    }
    let (device, owner) = (root_metadata.dev(), root_metadata.uid());
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        let metadata =
            std::fs::symlink_metadata(&directory).map_err(|error| failed(&directory, error))?;
        if !metadata.is_dir() || metadata.dev() != device || metadata.uid() != owner {
            continue;
        }
        let mode = metadata.permissions().mode();
        if mode & 0o700 != 0o700 {
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(mode | 0o700))
                .map_err(|error| failed(&directory, error))?;
        }
        for entry in std::fs::read_dir(&directory).map_err(|error| failed(&directory, error))? {
            let entry = entry.map_err(|error| failed(&directory, error))?;
            if entry
                .file_type()
                .map_err(|error| failed(&entry.path(), error))?
                .is_dir()
            {
                pending.push(entry.path());
            }
        }
    }
    Ok(())
}

#[cfg(not(unix))]
fn make_directories_owner_writable(_root: &Path) -> Result<(), Refusal> {
    Ok(())
}

fn residue_unproven(path: &Path, reason: &str) -> Refusal {
    Refusal::new(
        "removal-residue-unproven",
        format!("{}: {reason}", path.display()),
    )
}

/// Tracked entries of one commit: path bytes mapped to (mode, object id).
type TrackedEntries = BTreeMap<Vec<u8>, (String, String)>;

#[cfg(unix)]
fn tracked_entries(repository: &Path, head: &str) -> Result<TrackedEntries, Refusal> {
    let output = ProcessGit::output_bytes(
        repository,
        [
            "ls-tree",
            "-r",
            "-z",
            "--full-tree",
            "--end-of-options",
            head,
        ],
    )?;
    let mut entries = BTreeMap::new();
    for record in output
        .split(|byte| *byte == 0)
        .filter(|record| !record.is_empty())
    {
        let tab = record
            .iter()
            .position(|byte| *byte == b'\t')
            .ok_or_else(|| Refusal::new("invalid-tree-listing", "ls-tree record has no path"))?;
        let header = std::str::from_utf8(&record[..tab])
            .map_err(|error| Refusal::new("git-output-not-utf8", error.to_string()))?;
        let mut fields = header.split(' ');
        let (Some(mode), Some(_kind), Some(object)) = (fields.next(), fields.next(), fields.next())
        else {
            return Err(Refusal::new(
                "invalid-tree-listing",
                "ls-tree record is missing a field",
            ));
        };
        entries.insert(
            record[tab + 1..].to_vec(),
            (mode.to_owned(), object.to_owned()),
        );
    }
    Ok(entries)
}

/// Prove that every file under an unlinked tree is the recorded commit's tracked content.
#[cfg(unix)]
fn verify_residue(repository: &Path, root: &Path, head: &str) -> Result<(), Refusal> {
    use std::os::unix::ffi::OsStrExt as _;

    let entries = tracked_entries(repository, head)?;
    let worktrees =
        ProcessGit::absolute_git_path(repository, "--git-common-dir")?.join("worktrees");
    let inspect = |path: &Path, error: std::io::Error| {
        Refusal::new(
            "worktree-path-inspection-failed",
            format!("{}: {error}", path.display()),
        )
    };
    let mut pending = vec![root.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for entry in std::fs::read_dir(&directory).map_err(|error| inspect(&directory, error))? {
            let entry = entry.map_err(|error| inspect(&directory, error))?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|error| inspect(&path, error))?;
            if kind.is_dir() {
                pending.push(path);
                continue;
            }
            let relative = path
                .strip_prefix(root)
                .map_err(|_| residue_unproven(&path, "is outside the worktree"))?;
            let key = relative.as_os_str().as_bytes();
            let Some((mode, object)) = entries.get(key) else {
                if key == b".git" && kind.is_file() && gitfile_points_into(&path, &worktrees)? {
                    continue;
                }
                return Err(residue_unproven(
                    &path,
                    "is not tracked by the recorded commit",
                ));
            };
            let matches = if kind.is_symlink() {
                mode == "120000"
                    && std::fs::read_link(&path)
                        .map_err(|error| inspect(&path, error))?
                        .as_os_str()
                        .as_bytes()
                        == ProcessGit::output_bytes(
                            repository,
                            ["cat-file", "blob", object.as_str()],
                        )?
                        .as_slice()
            } else if kind.is_file() && (mode == "100644" || mode == "100755") {
                let attributes = format!("--path={}", relative.display());
                ProcessGit::output(
                    repository,
                    [
                        OsStr::new("hash-object"),
                        OsStr::new(attributes.as_str()),
                        OsStr::new("--"),
                        path.as_os_str(),
                    ],
                )?
                .trim()
                    == object
            } else {
                false
            };
            if !matches {
                return Err(residue_unproven(
                    &path,
                    "differs from the recorded commit's content",
                ));
            }
        }
    }
    Ok(())
}

/// Accept the tree's own `.git` file only while it names an administrative worktree directory.
fn gitfile_points_into(path: &Path, worktrees: &Path) -> Result<bool, Refusal> {
    let contents = std::fs::read_to_string(path).map_err(|error| {
        Refusal::new(
            "worktree-path-inspection-failed",
            format!("{}: {error}", path.display()),
        )
    })?;
    Ok(contents
        .strip_prefix("gitdir: ")
        .map(str::trim_end)
        .is_some_and(|target| {
            let target = Path::new(target);
            target.parent() == Some(worktrees)
        }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::{TempDir, tempdir};

    fn git(repository: &Path, args: &[&OsStr]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git failed in {}: {}",
            repository.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn init_repository(path: &Path) {
        std::fs::create_dir(path).unwrap();
        git(
            path,
            &[OsStr::new("init"), OsStr::new("-b"), OsStr::new("main")],
        );
        git(
            path,
            &[
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("test@example.invalid"),
            ],
        );
        git(
            path,
            &[
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("Worktree Test"),
            ],
        );
        std::fs::write(path.join("tracked"), "one\n").unwrap();
        git(path, &[OsStr::new("add"), OsStr::new("tracked")]);
        git(
            path,
            &[
                OsStr::new("commit"),
                OsStr::new("-m"),
                OsStr::new("initial"),
            ],
        );
    }

    fn add_linked(repository: &Path, linked: &Path) {
        git(
            repository,
            &[
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                linked.as_os_str(),
                OsStr::new("HEAD"),
            ],
        );
    }

    fn commit_change(repository: &Path, contents: &str, message: &str) -> String {
        std::fs::write(repository.join("tracked"), contents).unwrap();
        git(repository, &[OsStr::new("add"), OsStr::new("tracked")]);
        git(
            repository,
            &[OsStr::new("commit"), OsStr::new("-m"), OsStr::new(message)],
        );
        git(repository, &[OsStr::new("rev-parse"), OsStr::new("HEAD")])
            .trim()
            .to_owned()
    }

    fn repository_with_remote() -> (TempDir, PathBuf, PathBuf) {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repo");
        init_repository(&repository);
        let remote = temporary.path().join("remote.git");
        std::fs::create_dir(&remote).unwrap();
        git(&remote, &[OsStr::new("init"), OsStr::new("--bare")]);
        git(
            &remote,
            &[
                OsStr::new("symbolic-ref"),
                OsStr::new("HEAD"),
                OsStr::new("refs/heads/main"),
            ],
        );
        git(
            &repository,
            &[
                OsStr::new("remote"),
                OsStr::new("add"),
                OsStr::new("origin"),
                remote.as_os_str(),
            ],
        );
        git(
            &repository,
            &[
                OsStr::new("push"),
                OsStr::new("--set-upstream"),
                OsStr::new("origin"),
                OsStr::new("main"),
            ],
        );
        (temporary, repository, remote)
    }

    /// A linked tree whose tracked `app/jobs/Job.txt` sits in a directory without write bits.
    #[cfg(unix)]
    fn linked_with_read_only_directory() -> (TempDir, PathBuf, PathBuf, String) {
        use std::os::unix::fs::PermissionsExt as _;

        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repo");
        init_repository(&repository);
        std::fs::create_dir_all(repository.join("app/jobs")).unwrap();
        std::fs::write(repository.join("app/jobs/Job.txt"), "job\n").unwrap();
        git(&repository, &[OsStr::new("add"), OsStr::new("app")]);
        git(
            &repository,
            &[OsStr::new("commit"), OsStr::new("-m"), OsStr::new("jobs")],
        );
        let head = git(&repository, &[OsStr::new("rev-parse"), OsStr::new("HEAD")])
            .trim()
            .to_owned();
        let linked = temporary.path().join("linked");
        add_linked(&repository, &linked);
        let linked = std::fs::canonicalize(linked).unwrap();
        std::fs::set_permissions(
            linked.join("app/jobs"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        (temporary, repository, linked, head)
    }

    /// Reproduce the half-removed state: Git unlinks the tree but cannot delete every file.
    ///
    /// Returns `false` when the process can write anyway (for example as root).
    #[cfg(unix)]
    fn interrupt_removal(repository: &Path, linked: &Path) -> bool {
        let removed = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args([
                OsStr::new("worktree"),
                OsStr::new("remove"),
                linked.as_os_str(),
            ])
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success();
        if removed {
            return false;
        }
        assert!(linked.exists());
        assert!(
            !ProcessGit
                .list_worktrees(repository)
                .unwrap()
                .iter()
                .any(|item| item.path == linked)
        );
        true
    }

    #[cfg(unix)]
    #[test]
    fn removes_a_linked_tree_holding_a_read_only_directory() {
        let (_temporary, repository, linked, _head) = linked_with_read_only_directory();
        ProcessGit.remove(&repository, &linked).unwrap();
        assert!(!linked.exists());
    }

    #[cfg(unix)]
    #[test]
    fn verifies_and_deletes_the_residue_of_an_interrupted_removal() {
        let (_temporary, repository, linked, head) = linked_with_read_only_directory();
        if !interrupt_removal(&repository, &linked) {
            return;
        }
        ProcessGit
            .verify_removal_residue(&repository, &linked, &head)
            .unwrap();
        ProcessGit.delete_residue(&linked).unwrap();
        assert!(!linked.exists());
    }

    #[cfg(unix)]
    #[test]
    fn refuses_residue_that_is_not_the_recorded_commit() {
        let (_temporary, repository, linked, head) = linked_with_read_only_directory();
        if !interrupt_removal(&repository, &linked) {
            return;
        }
        std::fs::write(linked.join("app/jobs/Job.txt"), "changed\n").unwrap();
        let changed = ProcessGit
            .verify_removal_residue(&repository, &linked, &head)
            .unwrap_err();
        assert_eq!(changed.code, "removal-residue-unproven");

        std::fs::write(linked.join("app/jobs/Job.txt"), "job\n").unwrap();
        std::fs::write(linked.join("notes.txt"), "new work\n").unwrap();
        let untracked = ProcessGit
            .verify_removal_residue(&repository, &linked, &head)
            .unwrap_err();
        assert_eq!(untracked.code, "removal-residue-unproven");
        assert!(untracked.message.contains("notes.txt"));
        make_directories_owner_writable(&linked).unwrap();
    }

    #[test]
    fn moves_a_dirty_linked_tree_without_losing_state() {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repo");
        init_repository(&repository);

        let legacy = temporary.path().join("legacy");
        add_linked(&repository, &legacy);
        std::fs::write(legacy.join("untracked"), "preserve me\n").unwrap();
        let managed = temporary.path().join("managed").join("one");

        ProcessGit
            .validate_move_worktree(&legacy, &managed)
            .unwrap();
        ProcessGit
            .move_worktree(&repository, &legacy, &managed)
            .unwrap();

        assert!(!legacy.exists());
        assert_eq!(
            std::fs::read_to_string(managed.join("untracked")).unwrap(),
            "preserve me\n"
        );
        assert!(
            ProcessGit
                .worktree_snapshot(&repository, &managed)
                .unwrap()
                .dirty
        );
        let discovered = ProcessGit.list_worktrees(&repository).unwrap();
        assert!(discovered.iter().any(|item| item.path == managed));
    }

    #[test]
    fn ignored_files_make_a_linked_worktree_dirty() {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repo");
        init_repository(&repository);
        std::fs::write(repository.join(".gitignore"), "ignored/\n").unwrap();
        git(&repository, &[OsStr::new("add"), OsStr::new(".gitignore")]);
        git(
            &repository,
            &[
                OsStr::new("commit"),
                OsStr::new("-m"),
                OsStr::new("ignore generated files"),
            ],
        );
        let linked = temporary.path().join("linked");
        add_linked(&repository, &linked);

        assert!(
            !ProcessGit
                .worktree_snapshot(&repository, &linked)
                .unwrap()
                .dirty
        );
        std::fs::create_dir(linked.join("ignored")).unwrap();
        std::fs::write(linked.join("ignored/cache"), "must survive\n").unwrap();
        assert!(
            ProcessGit
                .worktree_snapshot(&repository, &linked)
                .unwrap()
                .dirty
        );
    }

    #[test]
    fn operational_git_locks_mark_a_linked_worktree_locked() {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repo");
        init_repository(&repository);
        let linked = temporary.path().join("linked");
        add_linked(&repository, &linked);
        let git_dir = ProcessGit::absolute_git_path(&linked, "--git-dir").unwrap();
        let common_dir = ProcessGit::absolute_git_path(&linked, "--git-common-dir").unwrap();
        let locks = [
            git_dir.join("index.lock"),
            git_dir.join("HEAD.lock"),
            git_dir.join("refs/worktree/operation.lock"),
            common_dir.join("config.lock"),
            common_dir.join("packed-refs.lock"),
            common_dir.join("refs/heads/main.lock"),
            common_dir.join("reftable/tables.list.lock"),
        ];

        for lock in locks {
            std::fs::create_dir_all(lock.parent().unwrap()).unwrap();
            std::fs::write(&lock, "").unwrap();
            assert!(
                ProcessGit
                    .worktree_snapshot(&repository, &linked)
                    .unwrap()
                    .locked,
                "{} was not detected",
                lock.display()
            );
            std::fs::remove_file(lock).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn clean_interrupted_rebase_marks_a_linked_worktree_locked() {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repo");
        init_repository(&repository);
        let linked = temporary.path().join("linked");
        add_linked(&repository, &linked);
        commit_change(&linked, "two\n", "linked change");

        let output = Command::new("git")
            .arg("-C")
            .arg(&linked)
            .args(["rebase", "--exec", "false", "HEAD~1"])
            .output()
            .unwrap();
        assert!(!output.status.success());
        assert!(
            ProcessGit::output(
                &linked,
                [
                    "--no-optional-locks",
                    "status",
                    "--porcelain=v1",
                    "--untracked-files=all",
                    "--ignored=matching",
                ],
            )
            .unwrap()
            .is_empty()
        );
        assert!(
            ProcessGit
                .worktree_snapshot(&repository, &linked)
                .unwrap()
                .locked
        );
        git(&linked, &[OsStr::new("rebase"), OsStr::new("--abort")]);
    }

    #[test]
    fn a_stale_rebase_head_leaves_the_worktree_unlocked() {
        // Git writes `REBASE_HEAD` while applying a commit and leaves it behind once the rebase
        // finishes. Counting it made `finish` refuse a clean, idle worktree with
        // `worktree-locked: Git marks the worktree locked`, for a lock Git does not report —
        // observed 2026-09-04 on two managed worktrees whose conflicted rebases had been resolved
        // and continued to the end.
        //
        // The marker is written here rather than produced by a real rebase on purpose: which
        // rebase paths leave it is a Git implementation detail that varies by version, and a test
        // that depends on it tests Git. What this repository decides is what the marker *means*.
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repo");
        init_repository(&repository);
        let linked = temporary.path().join("linked");
        add_linked(&repository, &linked);

        let git_dir = ProcessGit::absolute_git_path(&linked, "--git-dir").unwrap();
        let head = git(&linked, &[OsStr::new("rev-parse"), OsStr::new("HEAD")]);
        std::fs::write(git_dir.join("REBASE_HEAD"), &head).unwrap();
        assert!(
            !git_dir.join("rebase-merge").exists() && !git_dir.join("rebase-apply").exists(),
            "no rebase is in progress: those two directories are what say one is"
        );

        assert!(
            !ProcessGit
                .worktree_snapshot(&repository, &linked)
                .unwrap()
                .locked,
            "a finished rebase is not a paused one"
        );

        // And the two that do mean a paused rebase still do.
        std::fs::create_dir(git_dir.join("rebase-merge")).unwrap();
        assert!(
            ProcessGit
                .worktree_snapshot(&repository, &linked)
                .unwrap()
                .locked,
            "rebase-merge is what a paused rebase looks like"
        );
    }

    #[cfg(unix)]
    #[test]
    fn create_refuses_a_symlinked_target_parent() {
        use std::os::unix::fs::symlink;

        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repo");
        init_repository(&repository);
        let managed_root = temporary.path().join("managed");
        let external = temporary.path().join("external");
        std::fs::create_dir(&managed_root).unwrap();
        std::fs::create_dir(&external).unwrap();
        symlink(&external, managed_root.join("repo")).unwrap();
        let path = managed_root.join("repo/symlink-escape");
        let plan = CreatePlan {
            id: b10x_worktree_domain::WorktreeId::new("symlink-escape").unwrap(),
            repository_root: repository.clone(),
            path: path.clone(),
            base: b10x_worktree_domain::GitRevision::new(
                git(&repository, &[OsStr::new("rev-parse"), OsStr::new("HEAD")])
                    .trim()
                    .to_owned(),
            )
            .unwrap(),
            purpose: "test".into(),
            owner: "test".into(),
            planned_at: 0,
        };

        assert_eq!(
            ProcessGit.create_detached(&plan).unwrap_err().code,
            "non-canonical-worktree-path"
        );
        assert!(!external.join("symlink-escape").exists());
        assert!(
            ProcessGit
                .list_worktrees(&repository)
                .unwrap()
                .iter()
                .all(|item| item.path != path)
        );
    }

    #[test]
    fn snapshots_resolve_the_primary_and_reject_non_members() {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repo");
        init_repository(&repository);
        let linked = temporary.path().join("linked");
        add_linked(&repository, &linked);
        let primary_head = commit_change(&repository, "two\n", "advance primary");

        let snapshot = ProcessGit.repository_snapshot(&linked).unwrap();
        assert_eq!(snapshot.root, std::fs::canonicalize(&repository).unwrap());
        assert_eq!(snapshot.head, primary_head);
        assert_eq!(
            ProcessGit
                .worktree_snapshot(&repository, &repository)
                .unwrap_err()
                .code,
            "worktree-not-linked"
        );

        let other_repository = temporary.path().join("other-repo");
        init_repository(&other_repository);
        let other_linked = temporary.path().join("other-linked");
        add_linked(&other_repository, &other_linked);
        assert_eq!(
            ProcessGit
                .worktree_snapshot(&repository, &other_linked)
                .unwrap_err()
                .code,
            "worktree-not-linked"
        );
    }

    #[cfg(unix)]
    #[test]
    fn lists_non_utf8_worktree_paths_from_nul_delimited_output() {
        use std::os::unix::ffi::OsStringExt as _;

        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repo");
        init_repository(&repository);
        let linked = temporary
            .path()
            .join(std::ffi::OsString::from_vec(b"linked-\xff".to_vec()));
        add_linked(&repository, &linked);

        let worktrees = ProcessGit.list_worktrees(&repository).unwrap();
        assert!(worktrees.iter().any(|item| item.path == linked));
    }

    #[test]
    fn local_tags_and_fabricated_remote_refs_are_not_recovery_proof() {
        let (_temporary, repository, _remote) = repository_with_remote();
        let head = commit_change(&repository, "local only\n", "local only");
        git(
            &repository,
            &[
                OsStr::new("tag"),
                OsStr::new("local-only"),
                OsStr::new(head.as_str()),
            ],
        );
        git(
            &repository,
            &[
                OsStr::new("update-ref"),
                OsStr::new("refs/remotes/not-a-remote/fake"),
                OsStr::new(head.as_str()),
            ],
        );
        git(
            &repository,
            &[
                OsStr::new("config"),
                OsStr::new("fetch.prune"),
                OsStr::new("true"),
            ],
        );
        git(
            &repository,
            &[
                OsStr::new("config"),
                OsStr::new("fetch.pruneTags"),
                OsStr::new("true"),
            ],
        );

        let recovery_refs = ProcessGit.recovery_refs(&repository, &head).unwrap();
        assert_eq!(
            git(
                &repository,
                &[OsStr::new("rev-parse"), OsStr::new("refs/tags/local-only")],
            )
            .trim(),
            head
        );
        assert_eq!(
            git(
                &repository,
                &[
                    OsStr::new("rev-parse"),
                    OsStr::new("refs/remotes/not-a-remote/fake"),
                ],
            )
            .trim(),
            head
        );
        assert!(recovery_refs.is_empty());
    }

    #[test]
    fn containing_refs_separates_a_held_commit_from_an_unreferenced_one() {
        let (_temporary, repository, _remote) = repository_with_remote();
        let head = commit_change(&repository, "local work\n", "local work");
        git(
            &repository,
            &[
                OsStr::new("branch"),
                OsStr::new("wave/hardening"),
                OsStr::new(head.as_str()),
            ],
        );

        // Local branches still hold it, even though no remote advertises it.
        assert!(
            ProcessGit
                .recovery_refs(&repository, &head)
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            ProcessGit.containing_refs(&repository, &head).unwrap(),
            Some(vec![
                "refs/heads/main".to_owned(),
                "refs/heads/wave/hardening".to_owned(),
            ])
        );

        // Detach and drop every ref: the object survives with nothing pointing at it.
        git(
            &repository,
            &[
                OsStr::new("checkout"),
                OsStr::new("--detach"),
                OsStr::new("origin/main"),
            ],
        );
        git(
            &repository,
            &[
                OsStr::new("branch"),
                OsStr::new("-D"),
                OsStr::new("wave/hardening"),
            ],
        );
        git(
            &repository,
            &[OsStr::new("branch"), OsStr::new("-D"), OsStr::new("main")],
        );
        assert_eq!(
            ProcessGit.containing_refs(&repository, &head).unwrap(),
            Some(Vec::new())
        );

        // An id Git holds no object for is reported as absent rather than as an error.
        assert_eq!(
            ProcessGit
                .containing_refs(&repository, &"0".repeat(head.len()))
                .unwrap(),
            None
        );
    }

    #[test]
    fn containing_refs_refuses_while_grafts_can_rewrite_ancestry() {
        let (_temporary, repository, _remote) = repository_with_remote();
        let head = commit_change(&repository, "grafted\n", "grafted");
        let common_dir = ProcessGit::absolute_git_path(&repository, "--git-common-dir").unwrap();
        let grafts = common_dir.join("info/grafts");
        std::fs::create_dir_all(grafts.parent().unwrap()).unwrap();
        std::fs::write(&grafts, format!("{head} {head}\n")).unwrap();

        assert_eq!(
            ProcessGit
                .containing_refs(&repository, &head)
                .unwrap_err()
                .code,
            "git-grafts-present"
        );
    }

    #[test]
    fn grafted_ancestry_cannot_establish_remote_recovery() {
        let (_temporary, repository, _remote) = repository_with_remote();
        let remote_tip = git(
            &repository,
            &[OsStr::new("rev-parse"), OsStr::new("origin/main")],
        )
        .trim()
        .to_owned();
        git(
            &repository,
            &[
                OsStr::new("switch"),
                OsStr::new("--orphan"),
                OsStr::new("local-only"),
            ],
        );
        let local_head = commit_change(&repository, "unrelated\n", "unrelated local commit");
        let common_dir = ProcessGit::absolute_git_path(&repository, "--git-common-dir").unwrap();
        let grafts = common_dir.join("info/grafts");
        std::fs::create_dir_all(grafts.parent().unwrap()).unwrap();
        std::fs::write(&grafts, format!("{remote_tip} {local_head}\n")).unwrap();

        assert!(
            !ProcessGit::contains_commit(&repository, &local_head, &remote_tip).unwrap(),
            "proof subprocess unexpectedly honored repository grafts"
        );
        assert_eq!(
            ProcessGit
                .recovery_refs(&repository, &local_head)
                .unwrap_err()
                .code,
            "git-grafts-present"
        );
    }

    #[test]
    fn exact_advertised_custom_ref_survives_a_deleted_source_branch() {
        let (_temporary, repository, _remote) = repository_with_remote();
        let head = commit_change(&repository, "review\n", "review");
        git(
            &repository,
            &[
                OsStr::new("push"),
                OsStr::new("origin"),
                OsStr::new("HEAD:refs/heads/review-source"),
            ],
        );
        git(
            &repository,
            &[
                OsStr::new("push"),
                OsStr::new("origin"),
                OsStr::new("HEAD:refs/pull/42/head"),
            ],
        );
        git(
            &repository,
            &[
                OsStr::new("push"),
                OsStr::new("origin"),
                OsStr::new(":refs/heads/review-source"),
            ],
        );
        git(
            &repository,
            &[
                OsStr::new("update-ref"),
                OsStr::new("refs/remotes/origin/stale-review"),
                OsStr::new(head.as_str()),
            ],
        );

        assert_eq!(
            ProcessGit.recovery_refs(&repository, &head).unwrap(),
            vec!["origin:refs/pull/42/head"]
        );
    }

    #[test]
    fn advertised_remote_tag_is_recovery_proof_without_a_local_tag() {
        let (_temporary, repository, _remote) = repository_with_remote();
        let head = commit_change(&repository, "tagged remotely\n", "remote tag");
        git(
            &repository,
            &[
                OsStr::new("push"),
                OsStr::new("origin"),
                OsStr::new("HEAD:refs/tags/recovery"),
            ],
        );

        let local_tag = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["show-ref", "--verify", "refs/tags/recovery"])
            .output()
            .unwrap();
        assert!(!local_tag.status.success());
        assert_eq!(
            ProcessGit.recovery_refs(&repository, &head).unwrap(),
            vec!["origin:refs/tags/recovery"]
        );
        let local_tag = Command::new("git")
            .arg("-C")
            .arg(&repository)
            .args(["show-ref", "--verify", "refs/tags/recovery"])
            .output()
            .unwrap();
        assert!(!local_tag.status.success());
    }

    #[test]
    fn fetches_only_when_an_advertised_descendant_object_is_missing() {
        let (temporary, repository, remote) = repository_with_remote();
        let recoverable_head = git(&repository, &[OsStr::new("rev-parse"), OsStr::new("HEAD")])
            .trim()
            .to_owned();
        let publisher = temporary.path().join("publisher");
        git(
            temporary.path(),
            &[
                OsStr::new("clone"),
                remote.as_os_str(),
                publisher.as_os_str(),
            ],
        );
        git(
            &publisher,
            &[
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("test@example.invalid"),
            ],
        );
        git(
            &publisher,
            &[
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("Worktree Test"),
            ],
        );
        let remote_tip = commit_change(&publisher, "remote descendant\n", "remote descendant");
        git(
            &publisher,
            &[OsStr::new("push"), OsStr::new("origin"), OsStr::new("main")],
        );
        assert!(!ProcessGit::commitish_exists(&repository, &remote_tip).unwrap());

        assert_eq!(
            ProcessGit
                .recovery_refs(&repository, &recoverable_head)
                .unwrap(),
            vec!["origin:refs/heads/main"]
        );
        assert!(ProcessGit::commitish_exists(&repository, &remote_tip).unwrap());
    }

    fn run(repository: &Path, args: &[&str]) -> String {
        let args = args.iter().map(OsStr::new).collect::<Vec<_>>();
        git(repository, &args).trim().to_owned()
    }

    fn commit_file(repository: &Path, path: &str, contents: &str, message: &str) -> String {
        std::fs::write(repository.join(path), contents).unwrap();
        run(repository, &["add", path]);
        run(repository, &["commit", "-m", message]);
        run(repository, &["rev-parse", "HEAD"])
    }

    /// Branch `feature` from the pushed main, then move main past the branch point.
    fn diverged_feature() -> (TempDir, PathBuf) {
        let (temporary, repository, _remote) = repository_with_remote();
        run(&repository, &["switch", "-c", "feature"]);
        commit_file(&repository, "first", "first\n", "first unit commit");
        commit_file(&repository, "second", "second\n", "second unit commit");
        run(&repository, &["switch", "main"]);
        commit_file(
            &repository,
            "unrelated",
            "unrelated\n",
            "unrelated main commit",
        );
        (temporary, repository)
    }

    fn feature_head(repository: &Path) -> String {
        run(repository, &["rev-parse", "feature"])
    }

    #[test]
    fn rebased_work_on_main_is_patch_equivalent_recovery() {
        let (_temporary, repository) = diverged_feature();
        run(&repository, &["cherry-pick", "main..feature"]);
        run(&repository, &["push", "origin", "main"]);
        let head = feature_head(&repository);

        assert!(
            ProcessGit
                .recovery_refs(&repository, &head)
                .unwrap()
                .is_empty()
        );
        let evidence = ProcessGit.recovery_evidence(&repository, &head).unwrap();
        assert_eq!(evidence.kind, RecoveryKind::PatchEquivalent);
        assert_eq!(evidence.refs, vec!["origin:refs/heads/main"]);
        let mut expected = run(&repository, &["rev-list", "main..feature"])
            .lines()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let mut observed = evidence.equivalent_commits;
        expected.sort();
        observed.sort();
        assert_eq!(observed, expected);
    }

    #[test]
    fn exact_ancestry_still_reports_ancestor_proof() {
        let (_temporary, repository) = diverged_feature();
        run(&repository, &["push", "origin", "feature"]);
        let head = feature_head(&repository);

        let evidence = ProcessGit.recovery_evidence(&repository, &head).unwrap();
        assert_eq!(evidence.kind, RecoveryKind::Ancestor);
        assert_eq!(evidence.refs, vec!["origin:refs/heads/feature"]);
        assert!(evidence.equivalent_commits.is_empty());
    }

    #[test]
    fn one_commit_missing_from_the_remote_defeats_equivalence() {
        let (_temporary, repository) = diverged_feature();
        run(&repository, &["cherry-pick", "feature~1"]);
        run(&repository, &["push", "origin", "main"]);
        let head = feature_head(&repository);

        let evidence = ProcessGit.recovery_evidence(&repository, &head).unwrap();
        assert!(evidence.refs.is_empty());
        assert!(evidence.equivalent_commits.is_empty());
    }

    #[test]
    fn whitespace_only_difference_defeats_equivalence() {
        let (_temporary, repository, _remote) = repository_with_remote();
        run(&repository, &["switch", "-c", "feature"]);
        commit_file(
            &repository,
            "indented",
            "if x:\n\treturn y\n",
            "tab indentation",
        );
        run(&repository, &["switch", "main"]);
        commit_file(
            &repository,
            "unrelated",
            "unrelated\n",
            "unrelated main commit",
        );
        commit_file(
            &repository,
            "indented",
            "if x:\n    return y\n",
            "space indentation",
        );
        run(&repository, &["push", "origin", "main"]);
        let head = feature_head(&repository);

        // Git's own patch ids ignore whitespace and call these the same change.
        assert!(
            run(
                &repository,
                &[
                    "rev-list",
                    "--right-only",
                    "--cherry-pick",
                    "main...feature"
                ]
            )
            .is_empty()
        );
        let evidence = ProcessGit.recovery_evidence(&repository, &head).unwrap();
        assert!(evidence.refs.is_empty());
    }

    #[test]
    fn a_merge_among_unique_commits_defeats_equivalence() {
        let (_temporary, repository, _remote) = repository_with_remote();
        run(&repository, &["switch", "-c", "side"]);
        commit_file(&repository, "side", "side\n", "side commit");
        run(&repository, &["switch", "main"]);
        run(&repository, &["switch", "-c", "feature"]);
        commit_file(&repository, "first", "first\n", "first unit commit");
        run(&repository, &["merge", "--no-ff", "--no-edit", "side"]);
        run(&repository, &["switch", "main"]);
        commit_file(
            &repository,
            "unrelated",
            "unrelated\n",
            "unrelated main commit",
        );
        run(&repository, &["cherry-pick", "side", "feature^1"]);
        run(&repository, &["push", "origin", "main"]);
        let head = feature_head(&repository);

        let evidence = ProcessGit.recovery_evidence(&repository, &head).unwrap();
        assert!(evidence.refs.is_empty());
    }

    #[test]
    fn an_empty_unique_commit_defeats_equivalence() {
        let (_temporary, repository) = diverged_feature();
        run(&repository, &["cherry-pick", "main..feature"]);
        run(&repository, &["push", "origin", "main"]);
        run(&repository, &["switch", "feature"]);
        run(
            &repository,
            &[
                "commit",
                "--allow-empty",
                "-m",
                "a message and nothing else",
            ],
        );
        let head = feature_head(&repository);

        let evidence = ProcessGit.recovery_evidence(&repository, &head).unwrap();
        assert!(evidence.refs.is_empty());
    }

    #[test]
    fn replacement_objects_cannot_fake_patch_equivalence() {
        let (_temporary, repository) = diverged_feature();
        run(&repository, &["push", "origin", "main"]);
        let remote_tip = run(&repository, &["rev-parse", "main"]);
        // A local-only copy of the unit commits on top of main, substituted for the remote tip.
        run(&repository, &["switch", "-c", "forged", "main"]);
        run(&repository, &["cherry-pick", "main..feature"]);
        let forged = run(&repository, &["rev-parse", "forged"]);
        run(&repository, &["replace", &remote_tip, &forged]);
        let head = feature_head(&repository);

        let evidence = ProcessGit.recovery_evidence(&repository, &head).unwrap();
        assert!(evidence.refs.is_empty());
    }

    #[test]
    fn grafts_still_refuse_equivalence_observation() {
        let (_temporary, repository) = diverged_feature();
        run(&repository, &["cherry-pick", "main..feature"]);
        run(&repository, &["push", "origin", "main"]);
        let common_dir = ProcessGit::absolute_git_path(&repository, "--git-common-dir").unwrap();
        std::fs::write(common_dir.join("info/grafts"), "").unwrap();
        let head = feature_head(&repository);

        assert_eq!(
            ProcessGit
                .recovery_evidence(&repository, &head)
                .unwrap_err()
                .code,
            "git-grafts-present"
        );
    }
}
