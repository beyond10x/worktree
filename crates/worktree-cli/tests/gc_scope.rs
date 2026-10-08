//! `gc` without `--id` assesses only the records of the repository `--repo` resolves to;
//! `--scope profile` keeps the profile-wide selection, and `--id` names records whatever the scope.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

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
            "-c",
            "maintenance.auto=false",
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

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Two repositories under one activated profile, each with one finished tree.
struct Fixture {
    root: tempfile::TempDir,
    alpha: PathBuf,
    beta: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let alpha = workspace.join("alpha");
        let beta = workspace.join("beta");
        for repository in [&alpha, &beta] {
            std::fs::create_dir_all(repository).unwrap();
            git(repository, &["init", "-b", "main"]);
            write(&repository.join("source"), "original\n");
            git(repository, &["add", "."]);
            git(repository, &["commit", "-m", "fixture"]);
            let remote = root.path().join(format!(
                "{}.git",
                repository.file_name().unwrap().to_str().unwrap()
            ));
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
                repository,
                &["remote", "add", "origin", remote.to_str().unwrap()],
            );
            git(repository, &["fetch", "--quiet", "origin"]);
        }
        let profile = root.path().join("profile.toml");
        write(
            &profile,
            "version = 1\nname = 'test'\nexpire_after_seconds = 604800\nprotect_workspace_root = false\n",
        );
        let fixture = Self { root, alpha, beta };
        fixture.ok(&[
            "activate",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ]);
        for (repository, id) in [(&fixture.alpha, "alpha-tree"), (&fixture.beta, "beta-tree")] {
            fixture.ok(&[
                "create",
                "--repo",
                repository.to_str().unwrap(),
                "--id",
                id,
                "--purpose",
                "gc scope",
            ]);
            fixture.ok(&["finish", id]);
        }
        fixture
    }

    fn run(&self, args: &[&str]) -> Output {
        let outside = self.root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        Command::new(env!("CARGO_BIN_EXE_worktree"))
            .arg("--json")
            .args(args)
            .current_dir(outside)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn assessed(&self, repository: &Path, extra: &[&str]) -> Vec<String> {
        let mut args = vec!["gc", "--repo", repository.to_str().unwrap(), "--dry-run"];
        args.extend_from_slice(extra);
        let report = self.ok(&args);
        let mut ids: Vec<String> = report["assessments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["record"]["id"].as_str().unwrap().to_owned())
            .collect();
        ids.sort();
        ids
    }
}

#[test]
fn gc_defaults_to_the_repository_and_profile_scope_keeps_every_record() {
    let fixture = Fixture::new();

    assert_eq!(fixture.assessed(&fixture.alpha, &[]), vec!["alpha-tree"]);
    assert_eq!(fixture.assessed(&fixture.beta, &[]), vec!["beta-tree"]);
    assert_eq!(
        fixture.assessed(&fixture.alpha, &["--scope", "repo"]),
        vec!["alpha-tree"]
    );
    assert_eq!(
        fixture.assessed(&fixture.alpha, &["--scope", "profile"]),
        vec!["alpha-tree", "beta-tree"]
    );
}

#[test]
fn gc_resolves_repo_from_a_linked_tree_to_its_primary_repository() {
    let fixture = Fixture::new();
    let created = fixture.ok(&[
        "create",
        "--repo",
        fixture.alpha.to_str().unwrap(),
        "--id",
        "alpha-live",
        "--purpose",
        "gc scope from inside a tree",
    ]);
    let live = PathBuf::from(created["evidence"]["path"].as_str().unwrap());

    assert_eq!(fixture.assessed(&live, &[]), vec!["alpha-tree"]);
}

#[test]
fn gc_with_an_id_assesses_the_named_record_whatever_the_scope() {
    let fixture = Fixture::new();

    assert_eq!(
        fixture.assessed(&fixture.alpha, &["--id", "beta-tree"]),
        vec!["beta-tree"]
    );
    assert_eq!(
        fixture.assessed(&fixture.alpha, &["--scope", "repo", "--id", "beta-tree"]),
        vec!["beta-tree"]
    );
    let review = fixture.ok(&[
        "gc",
        "--repo",
        fixture.alpha.to_str().unwrap(),
        "--dry-run",
        "--id",
        "beta-tree",
    ]);
    assert_eq!(review["assessments"][0]["eligible"], true, "{review}");
}

#[test]
fn gc_refuses_an_unknown_scope() {
    let fixture = Fixture::new();
    let output = fixture.run(&[
        "gc",
        "--repo",
        fixture.alpha.to_str().unwrap(),
        "--dry-run",
        "--scope",
        "workspace",
    ]);
    assert!(!output.status.success());
}
