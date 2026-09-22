//! Native inspection integration tests use isolated repositories and registry state.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    other: PathBuf,
    tree: PathBuf,
    other_tree: PathBuf,
    remote: PathBuf,
}

fn git(path: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
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
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim_end().into()
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("one");
        let other = workspace.join("two");
        for path in [&repository, &other] {
            std::fs::create_dir_all(path).unwrap();
            git(path, &["init", "-b", "main"]);
            std::fs::write(path.join("source"), "original\n").unwrap();
            std::fs::write(path.join(".gitignore"), "/target/\n").unwrap();
            git(path, &["add", "."]);
            git(path, &["commit", "-m", "fixture"]);
        }
        let remote = root.path().join("remote.git");
        git(
            root.path(),
            &[
                "clone",
                "--bare",
                repository.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git(
            &repository,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        let profile = root.path().join("profile.toml");
        std::fs::write(&profile, "version = 1\nname = 'test'\nexpire_after_seconds = 604800\nprotect_workspace_root = false\n").unwrap();
        let mut fixture = Self {
            root,
            repository,
            other,
            tree: PathBuf::new(),
            other_tree: PathBuf::new(),
            remote,
        };
        fixture.ok(&[
            "activate",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ]);
        let first = fixture.ok(&[
            "create",
            "--repo",
            fixture.repository.to_str().unwrap(),
            "--id",
            "first",
            "--purpose",
            "review story:sample",
        ]);
        fixture.tree = PathBuf::from(first["evidence"]["path"].as_str().unwrap());
        let second = fixture.ok(&[
            "create",
            "--repo",
            fixture.other.to_str().unwrap(),
            "--id",
            "second",
            "--purpose",
            "other repository",
        ]);
        fixture.other_tree = PathBuf::from(second["evidence"]["path"].as_str().unwrap());
        fixture
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
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(output.stderr.is_empty());
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn inspect(&self, extra: &[&str]) -> Value {
        let mut args = vec!["inspect", "--repo", self.repository.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.ok(&args)
    }
}

#[test]
fn inspection_scopes_by_repository_and_reports_ignored_evidence_without_writes() {
    let fixture = Fixture::new();
    std::fs::create_dir(fixture.tree.join("target")).unwrap();
    std::fs::write(
        fixture.tree.join("target/evidence.log"),
        "irreplaceable observation",
    )
    .unwrap();
    std::fs::write(fixture.tree.join("untracked"), "not published").unwrap();
    std::fs::write(fixture.tree.join("source"), "changed\n").unwrap();
    fixture.ok(&[
        "hook",
        "session-start",
        "--path",
        fixture.tree.to_str().unwrap(),
        "--session",
        "current-owner",
    ]);
    let before = fixture.ok(&["status"]);
    let report = fixture.inspect(&[]);
    assert_eq!(report["version"], 3);
    assert_eq!(report["format"], "worktree.inspection/2");
    let inspections = report["inspections"].as_array().unwrap();
    assert_eq!(inspections.len(), 1);
    let first = &inspections[0];
    assert_eq!(first["live_leases"], 1);
    assert_eq!(first["details"]["tracked_changes"], 1);
    assert_eq!(first["details"]["untracked_entries"], 1);
    assert_eq!(first["details"]["ignored_entries"], 1);
    assert_eq!(first["recovery"]["state"], "not-checked");
    assert!(!first["cleanup_candidate"].as_bool().unwrap());
    assert!(
        first["work_item_status"]
            .as_str()
            .unwrap()
            .starts_with("unknown:")
    );
    assert_eq!(fixture.ok(&["status"]), before);
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("target/evidence.log")).unwrap(),
        "irreplaceable observation"
    );
    assert_eq!(
        fixture.inspect(&["--workspace"])["inspections"]
            .as_array()
            .unwrap()
            .len(),
        2
    );
    let refused = fixture.command(&[
        "inspect",
        "--repo",
        fixture.repository.to_str().unwrap(),
        "--id",
        "second",
    ]);
    assert!(!refused.status.success());
    let error: Value = serde_json::from_slice(&refused.stderr).unwrap();
    assert_eq!(error["code"], "inspection-id-outside-repository");
}

#[test]
fn inspection_distinguishes_unpublished_head_from_fresh_remote_tag_recovery() {
    let fixture = Fixture::new();
    std::fs::write(fixture.tree.join("source"), "local commit\n").unwrap();
    git(&fixture.tree, &["add", "source"]);
    git(&fixture.tree, &["commit", "-m", "local work"]);
    let before = fixture.ok(&["status"]);
    let local = fixture.inspect(&["--refresh"]);
    assert_eq!(local["inspections"][0]["recorded_head_differs"], true);
    assert_eq!(local["inspections"][0]["recovery"]["state"], "unproven");
    git(
        &fixture.tree,
        &[
            "push",
            fixture.remote.to_str().unwrap(),
            "HEAD:refs/tags/recovery",
        ],
    );
    let published = fixture.inspect(&["--refresh"]);
    assert_eq!(published["inspections"][0]["recovery"]["state"], "proven");
    assert_eq!(
        published["inspections"][0]["recovery"]["refs"][0],
        "origin:refs/tags/recovery"
    );
    assert_eq!(fixture.ok(&["status"]), before);
    assert!(fixture.tree.exists());
}

#[test]
fn rebased_work_on_the_remote_main_is_collected_with_patch_equivalent_proof() {
    let fixture = Fixture::new();
    std::fs::write(fixture.tree.join("unit"), "unit work\n").unwrap();
    git(&fixture.tree, &["add", "unit"]);
    git(&fixture.tree, &["commit", "-m", "unit work"]);
    let unit = git(&fixture.tree, &["rev-parse", "HEAD"]);
    std::fs::write(fixture.repository.join("unrelated"), "moved on\n").unwrap();
    git(&fixture.repository, &["add", "unrelated"]);
    git(&fixture.repository, &["commit", "-m", "main moved on"]);
    git(&fixture.repository, &["cherry-pick", unit.as_str()]);
    git(&fixture.repository, &["push", "origin", "main"]);

    let inspected = fixture.inspect(&["--refresh", "--id", "first"]);
    let recovery = &inspected["inspections"][0]["recovery"];
    assert_eq!(recovery["state"], "proven");
    assert_eq!(recovery["kind"], "patch-equivalent");
    assert_eq!(recovery["refs"][0], "origin:refs/heads/main");
    assert_eq!(recovery["equivalent_commits"][0], unit.as_str());

    fixture.ok(&["finish", fixture.tree.to_str().unwrap()]);
    let repository = fixture.repository.to_str().unwrap();
    let reviewed = fixture.ok(&["gc", "--repo", repository, "--dry-run", "--id", "first"]);
    assert_eq!(reviewed["version"], 3);
    assert_eq!(reviewed["assessments"][0]["eligible"], true);
    let applied = fixture.ok(&["gc", "--repo", repository, "--apply", "--id", "first"]);
    let proof = &applied["assessments"][0]["evidence"]["recovery"];
    assert_eq!(proof["kind"], "patch-equivalent");
    assert_eq!(proof["head"], unit.as_str());
    assert_eq!(proof["equivalent_commits"][0], unit.as_str());
    assert!(!fixture.tree.exists());
}

#[test]
fn missing_and_partial_trees_remain_visible() {
    let fixture = Fixture::new();
    std::fs::rename(
        &fixture.other_tree,
        fixture.root.path().join("interrupted-move"),
    )
    .unwrap();
    let report = fixture.inspect(&["--workspace", "--max-entries", "1"]);
    let items = report["inspections"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    let first = items
        .iter()
        .find(|item| item["record"]["id"] == "first")
        .unwrap();
    assert_eq!(first["details"]["storage"]["complete"], false);
    let second = items
        .iter()
        .find(|item| item["record"]["id"] == "second")
        .unwrap();
    assert!(second["details"].is_null());
    assert!(
        second["blockers"]
            .as_array()
            .unwrap()
            .iter()
            .any(|reason| reason["code"] == "worktree-not-found")
    );
}
