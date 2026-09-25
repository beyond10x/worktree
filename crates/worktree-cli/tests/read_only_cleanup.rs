//! GC of a managed tree holding a directory without the owner write bit (issue #16).
#![cfg(unix)]

use serde_json::Value;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    tree: PathBuf,
}

fn git(path: &Path, args: &[&str]) -> Output {
    Command::new("git")
        .arg("-C")
        .arg(path)
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "-c",
            "commit.gpgsign=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
}

fn git_ok(path: &Path, args: &[&str]) {
    let output = git(path, args);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

impl Fixture {
    /// A finished tree whose tracked `app/jobs/Job.txt` sits in a mode-0555 directory.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("demo");
        std::fs::create_dir_all(repository.join("app/jobs")).unwrap();
        git_ok(&repository, &["init", "-b", "main"]);
        std::fs::write(repository.join("README.md"), "demo\n").unwrap();
        std::fs::write(repository.join("app/jobs/Job.txt"), "job\n").unwrap();
        git_ok(&repository, &["add", "."]);
        git_ok(&repository, &["commit", "-m", "init"]);
        let remote = root.path().join("remote.git");
        git_ok(
            root.path(),
            &[
                "clone",
                "--bare",
                repository.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git_ok(
            &repository,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        let profile = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles/default.toml");
        let mut fixture = Self {
            root,
            repository,
            tree: PathBuf::new(),
        };
        fixture.ok(&[
            "activate",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ]);
        let created = fixture.ok(&[
            "create",
            "--repo",
            fixture.repository.to_str().unwrap(),
            "--id",
            "repro",
            "--purpose",
            "repro",
        ]);
        fixture.tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        std::fs::set_permissions(
            fixture.tree.join("app/jobs"),
            std::fs::Permissions::from_mode(0o555),
        )
        .unwrap();
        fixture.ok(&["finish", fixture.tree.to_str().unwrap()]);
        fixture
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_worktree"))
            .arg("--json")
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn gc(&self, mode: &str) -> Value {
        let report = self.ok(&[
            "gc",
            "--repo",
            self.repository.to_str().unwrap(),
            mode,
            "--id",
            "repro",
        ]);
        let assessments = report["assessments"].as_array().unwrap();
        assert_eq!(assessments.len(), 1, "{report}");
        assessments[0].clone()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        if self.tree.join("app/jobs").exists() {
            let _ = std::fs::set_permissions(
                self.tree.join("app/jobs"),
                std::fs::Permissions::from_mode(0o755),
            );
        }
    }
}

#[test]
fn gc_removes_a_tree_holding_a_read_only_directory() {
    let fixture = Fixture::new();
    assert_eq!(fixture.gc("--dry-run")["eligible"], true);

    let applied = fixture.gc("--apply");
    assert!(applied["evidence"].is_object(), "{applied}");
    assert!(!fixture.tree.exists());
}

#[test]
fn gc_leaves_a_tree_git_unlinked_without_removal_intent() {
    let fixture = Fixture::new();
    let removed = git(
        &fixture.repository,
        &["worktree", "remove", fixture.tree.to_str().unwrap()],
    );
    if removed.status.success() {
        // The process can write regardless of mode bits, for example as root.
        return;
    }
    assert!(fixture.tree.join("app/jobs/Job.txt").exists());

    let applied = fixture.gc("--apply");
    assert!(applied["evidence"].is_null(), "{applied}");
    assert_eq!(applied["refusal"]["code"], "worktree-not-linked");
    assert!(fixture.tree.join("app/jobs/Job.txt").exists());
}
