//! Git CLI adapter. Commands are executed directly, never through a shell.

use b10x_worktree::GitPort;
use b10x_worktree_domain::{
    CreatePlan, DiscoveredWorktree, Refusal, RepositorySnapshot, WorktreeSnapshot,
};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Process-backed Git port.
#[derive(Debug, Default, Clone, Copy)]
pub struct ProcessGit;

impl ProcessGit {
    fn output<I, S>(repository: &Path, args: I) -> Result<String, Refusal>
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
        String::from_utf8(output.stdout)
            .map_err(|error| Refusal::new("git-output-not-utf8", error.to_string()))
    }

    fn status<I, S>(repository: &Path, args: I) -> Result<(), Refusal>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        Self::output(repository, args).map(|_| ())
    }
}

impl GitPort for ProcessGit {
    fn repository_snapshot(&self, repository: &Path) -> Result<RepositorySnapshot, Refusal> {
        let root =
            PathBuf::from(Self::output(repository, ["rev-parse", "--show-toplevel"])?.trim());
        let root = std::fs::canonicalize(&root).map_err(|error| {
            Refusal::new(
                "repository-not-found",
                format!("{}: {error}", root.display()),
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
        let head = Self::output(&path, ["rev-parse", "HEAD"])?
            .trim()
            .to_owned();
        let dirty =
            !Self::output(&path, ["status", "--porcelain=v1", "--untracked-files=all"])?.is_empty();
        let locked = self
            .list_worktrees(repository)?
            .into_iter()
            .find(|item| item.path == path)
            .is_some_and(|item| item.locked);
        Ok(WorktreeSnapshot {
            path,
            head,
            dirty,
            locked,
        })
    }

    fn create_detached(&self, plan: &CreatePlan) -> Result<(), Refusal> {
        if plan.path.exists() {
            return Err(Refusal::new(
                "worktree-path-exists",
                plan.path.display().to_string(),
            ));
        }
        if let Some(parent) = plan.path.parent() {
            std::fs::create_dir_all(parent).map_err(|error| {
                Refusal::new("create-worktree-parent-failed", error.to_string())
            })?;
        }
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

    fn fetch(&self, repository: &Path) -> Result<(), Refusal> {
        Self::status(repository, ["fetch", "--all", "--prune", "--tags"])
            .map_err(|error| Refusal::new("remote-refresh-failed", error.message))
    }

    fn recovery_refs(&self, repository: &Path, head: &str) -> Result<Vec<String>, Refusal> {
        let mut refs = Vec::new();
        let branches = Self::output(
            repository,
            ["branch", "-r", "--contains", head, "--format=%(refname)"],
        )?;
        refs.extend(
            branches
                .lines()
                .map(str::trim)
                .filter(|line| line.starts_with("refs/remotes/") && !line.ends_with("/HEAD"))
                .map(str::to_owned),
        );
        let tags = Self::output(
            repository,
            [
                "tag",
                "--contains",
                head,
                "--format=refs/tags/%(refname:short)",
            ],
        )?;
        refs.extend(
            tags.lines()
                .map(str::trim)
                .filter(|line| line.starts_with("refs/tags/"))
                .map(str::to_owned),
        );
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
        if let Some(parent) = to.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|error| Refusal::new("move-worktree-parent-failed", error.to_string()))?;
        }
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

    fn list_worktrees(&self, repository: &Path) -> Result<Vec<DiscoveredWorktree>, Refusal> {
        let output = Self::output(repository, ["worktree", "list", "--porcelain"])?;
        let mut result = Vec::new();
        let mut path = None;
        let mut head = None;
        let mut locked = false;
        let mut index = 0usize;
        let flush = |result: &mut Vec<DiscoveredWorktree>,
                     path: &mut Option<PathBuf>,
                     head: &mut Option<String>,
                     locked: &mut bool,
                     index: &mut usize| {
            if let Some(value) = path.take() {
                let canonical = std::fs::canonicalize(&value).unwrap_or(value);
                result.push(DiscoveredWorktree {
                    path: canonical,
                    head: head.take(),
                    locked: *locked,
                    primary: *index == 0,
                });
                *locked = false;
                *index += 1;
            }
        };
        for line in output.lines().chain(std::iter::once("")) {
            if line.is_empty() {
                flush(&mut result, &mut path, &mut head, &mut locked, &mut index);
            } else if let Some(value) = line.strip_prefix("worktree ") {
                path = Some(PathBuf::from(value));
            } else if let Some(value) = line.strip_prefix("HEAD ") {
                head = Some(value.to_owned());
            } else if line == "locked" || line.starts_with("locked ") {
                locked = true;
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn git(repository: &Path, args: &[&OsStr]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(repository)
            .args(args)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn moves_a_dirty_linked_tree_without_losing_state() {
        let temporary = tempdir().unwrap();
        let repository = temporary.path().join("repo");
        std::fs::create_dir(&repository).unwrap();
        git(
            &repository,
            &[OsStr::new("init"), OsStr::new("-b"), OsStr::new("main")],
        );
        git(
            &repository,
            &[
                OsStr::new("config"),
                OsStr::new("user.email"),
                OsStr::new("test@example.invalid"),
            ],
        );
        git(
            &repository,
            &[
                OsStr::new("config"),
                OsStr::new("user.name"),
                OsStr::new("Worktree Test"),
            ],
        );
        std::fs::write(repository.join("tracked"), "one\n").unwrap();
        git(&repository, &[OsStr::new("add"), OsStr::new("tracked")]);
        git(
            &repository,
            &[
                OsStr::new("commit"),
                OsStr::new("-m"),
                OsStr::new("initial"),
            ],
        );

        let legacy = temporary.path().join("legacy");
        git(
            &repository,
            &[
                OsStr::new("worktree"),
                OsStr::new("add"),
                OsStr::new("--detach"),
                legacy.as_os_str(),
                OsStr::new("HEAD"),
            ],
        );
        std::fs::write(legacy.join("untracked"), "preserve me\n").unwrap();
        let managed = temporary.path().join("managed").join("one");

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
}
