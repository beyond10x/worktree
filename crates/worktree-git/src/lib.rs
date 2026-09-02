//! Git CLI adapter. Commands are executed directly, never through a shell.

use b10x_worktree::GitPort;
use b10x_worktree_domain::{
    CreatePlan, DiscoveredWorktree, Refusal, RepositorySnapshot, WorktreeSnapshot,
};
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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
        for relative in [
            "rebase-merge",
            "rebase-apply",
            "sequencer",
            "MERGE_HEAD",
            "CHERRY_PICK_HEAD",
            "REVERT_HEAD",
            "REBASE_HEAD",
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
        Self::validate_object_id(head)?;
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
        if !Self::commitish_exists(repository, head)? {
            return Err(Refusal::new(
                "invalid-recovery-head",
                format!("{head} is not a commit"),
            ));
        }
        let mut refs = Vec::new();
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
                if Self::commitish_exists(repository, &tip)?
                    && Self::contains_commit(repository, head, &tip)?
                {
                    refs.push(format!("{remote}:{reference}"));
                }
            }
        }
        refs.sort();
        refs.dedup();
        Ok(refs)
    }

    fn remove(&self, repository: &Path, worktree: &Path) -> Result<(), Refusal> {
        Self::status(
            repository,
            [
                OsStr::new("worktree"),
                OsStr::new("remove"),
                worktree.as_os_str(),
            ],
        )
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
}
