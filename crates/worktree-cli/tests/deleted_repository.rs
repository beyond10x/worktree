//! A record whose repository was deleted is retired only through a reviewed acknowledgement.
#![cfg(unix)]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ID: &str = "orphan";

struct Fixture {
    root: tempfile::TempDir,
    /// The repository the test deletes.
    repository: PathBuf,
    /// A live repository in the same workspace, used only to select the policy.
    other: PathBuf,
    tree: PathBuf,
    head: String,
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

fn git_ok(path: &Path, args: &[&str]) -> String {
    let output = git(path, args);
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn init_repository(path: &Path, contents: &str) {
    std::fs::create_dir_all(path).unwrap();
    git_ok(path, &["init", "-b", "main"]);
    std::fs::write(path.join("README.md"), contents).unwrap();
    git_ok(path, &["add", "."]);
    git_ok(path, &["commit", "-m", "init"]);
}

impl Fixture {
    /// An active managed tree with one local-only commit, in a workspace with a second repository.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(root.path())
            .unwrap()
            .join("workspace");
        let repository = workspace.join("doomed");
        let other = workspace.join("other");
        init_repository(&repository, "demo\n");
        init_repository(&other, "other\n");
        let profile = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles/default.toml");
        let mut fixture = Self {
            root,
            repository,
            other,
            tree: PathBuf::new(),
            head: String::new(),
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
            ID,
            "--purpose",
            "deleted",
        ]);
        fixture.tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        created["evidence"]["head"]
            .as_str()
            .unwrap()
            .clone_into(&mut fixture.head);
        // Work in the tree that exists nowhere else.
        std::fs::write(fixture.tree.join("feature.txt"), "local work\n").unwrap();
        fixture
    }

    fn record(&self) -> Option<Value> {
        let listed = self.ok(&["status"]);
        listed["records"]
            .as_array()
            .unwrap_or_else(|| panic!("{listed}"))
            .iter()
            .find(|record| record["id"] == ID)
            .cloned()
    }

    fn recorded_head(&self) -> String {
        self.record().unwrap()["head"].as_str().unwrap().to_owned()
    }

    fn delete_tree(&self) {
        std::fs::remove_dir_all(&self.tree).unwrap();
    }

    fn delete_repository(&self) {
        std::fs::remove_dir_all(&self.repository).unwrap();
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_worktree"))
            .arg("--json")
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            output.status.success(),
            "worktree {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn refused(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            !output.status.success(),
            "worktree {args:?} unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stderr).unwrap()
    }

    /// Reconcile the one record, selecting the policy through the live repository.
    fn reconcile(&self, extra: &[&str]) -> Value {
        let mut args = vec!["reconcile", "--repo", self.other.to_str().unwrap()];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--id", ID]);
        let report = self.ok(&args);
        assert_eq!(report["version"], 4, "{report}");
        let assessments = report["assessments"].as_array().unwrap();
        assert_eq!(assessments.len(), 1, "{report}");
        assessments[0].clone()
    }

    fn lifecycle(&self) -> String {
        self.record().map_or_else(
            || "absent".to_owned(),
            |record| record["lifecycle"].as_str().unwrap().to_owned(),
        )
    }
}

#[test]
fn dry_run_reports_a_deleted_repository_naming_the_recorded_commit_and_path() {
    let fixture = Fixture::new();
    assert_eq!(fixture.recorded_head(), fixture.head);
    fixture.delete_tree();
    fixture.delete_repository();

    let assessment = fixture.reconcile(&["--dry-run"]);

    assert_eq!(assessment["eligible"], false, "{assessment}");
    let refusal = &assessment["refusal"];
    assert_eq!(refusal["code"], "repository-missing", "{assessment}");
    let message = refusal["message"].as_str().unwrap();
    assert!(message.contains(&fixture.head), "{message}");
    assert!(
        message.contains(fixture.repository.to_str().unwrap()),
        "{message}"
    );
    assert!(
        message.contains(&format!(
            "--apply --id {ID} --acknowledge-unrecoverable {}",
            fixture.head
        )),
        "{message}"
    );
}

#[test]
fn acknowledging_the_recorded_commit_tombstones_a_record_whose_repository_was_deleted() {
    let fixture = Fixture::new();
    fixture.delete_tree();
    fixture.delete_repository();

    let assessment = fixture.reconcile(&["--apply", "--acknowledge-unrecoverable", &fixture.head]);

    assert_eq!(assessment["eligible"], true, "{assessment}");
    let evidence = &assessment["evidence"];
    assert_eq!(evidence["operation"], "reconcile-abandoned", "{assessment}");
    assert_eq!(evidence["head"], fixture.head.as_str(), "{assessment}");
    assert!(evidence["recovery"].is_null(), "{assessment}");
    assert_eq!(fixture.lifecycle(), "removed");
    // Nothing on disk was created or touched.
    assert!(!fixture.repository.exists());
    assert!(!fixture.tree.exists());
    assert!(fixture.other.join(".git").is_dir());
}

#[test]
fn a_repository_root_that_is_no_longer_a_repository_counts_as_deleted() {
    let fixture = Fixture::new();
    fixture.delete_tree();
    std::fs::remove_dir_all(fixture.repository.join(".git")).unwrap();

    let dry_run = fixture.reconcile(&["--dry-run"]);
    assert_eq!(
        dry_run["refusal"]["code"], "repository-missing",
        "{dry_run}"
    );

    let applied = fixture.reconcile(&["--apply", "--acknowledge-unrecoverable", &fixture.head]);
    assert_eq!(applied["eligible"], true, "{applied}");
    assert_eq!(fixture.lifecycle(), "removed");
    // The directory that remains is left exactly as it was.
    assert_eq!(
        std::fs::read_to_string(fixture.repository.join("README.md")).unwrap(),
        "demo\n"
    );
}

#[test]
fn apply_without_the_acknowledgement_keeps_the_record() {
    let fixture = Fixture::new();
    fixture.delete_tree();
    fixture.delete_repository();

    let assessment = fixture.reconcile(&["--apply"]);

    assert_eq!(assessment["eligible"], false, "{assessment}");
    assert_eq!(assessment["refusal"]["code"], "repository-missing");
    assert!(assessment["evidence"].is_null(), "{assessment}");
    assert_eq!(fixture.lifecycle(), "active");
}

#[test]
fn acknowledging_a_different_commit_is_refused_and_keeps_the_record() {
    let fixture = Fixture::new();
    fixture.delete_tree();
    fixture.delete_repository();
    let other_commit = git_ok(&fixture.other, &["rev-parse", "HEAD"]);

    let refusal = fixture.refused(&[
        "reconcile",
        "--repo",
        fixture.other.to_str().unwrap(),
        "--apply",
        "--id",
        ID,
        "--acknowledge-unrecoverable",
        &other_commit,
    ]);

    assert_eq!(
        refusal["code"], "unmatched-unrecoverable-acknowledgement",
        "{refusal}"
    );
    assert_eq!(fixture.lifecycle(), "active");
}

#[test]
fn a_tree_path_that_still_exists_is_refused_and_left_on_disk() {
    let fixture = Fixture::new();
    fixture.delete_repository();

    let dry_run = fixture.reconcile(&["--dry-run"]);
    assert_eq!(dry_run["eligible"], false, "{dry_run}");
    assert_eq!(
        dry_run["refusal"]["code"], "worktree-path-exists",
        "{dry_run}"
    );

    let applied = fixture.reconcile(&["--apply", "--acknowledge-unrecoverable", &fixture.head]);
    assert_eq!(applied["eligible"], false, "{applied}");
    assert!(applied["evidence"].is_null(), "{applied}");
    assert_eq!(
        applied["refusal"]["code"], "worktree-path-exists",
        "{applied}"
    );
    assert_eq!(fixture.lifecycle(), "active");
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("feature.txt")).unwrap(),
        "local work\n"
    );
}
