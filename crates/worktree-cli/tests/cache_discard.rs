//! Recognised build cache is discarded; every other ignored entry survives, and finish loses
//! nothing that is not cache.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TAG: &str =
    "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag.\n";

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

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    tree: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("one");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        write(&repository.join("source"), "original\n");
        write(
            &repository.join(".gitignore"),
            "/target\nnode_modules/\n.venv/\n.pytest_cache/\n",
        );
        write(&repository.join("web/package-lock.json"), "{}\n");
        write(&repository.join("tools/pyproject.toml"), "[project]\n");
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-m", "fixture"]);
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
        git(&repository, &["fetch", "--quiet", "origin"]);
        let profile = root.path().join("profile.toml");
        write(
            &profile,
            "version = 1\nname = 'test'\nexpire_after_seconds = 604800\nprotect_workspace_root = false\n",
        );
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
            "cache",
            "--purpose",
            "cache discard",
        ]);
        fixture.tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
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
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn refused(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            !output.status.success(),
            "expected a refusal from {args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stderr).unwrap()
    }

    fn tree_str(&self) -> &str {
        self.tree.to_str().unwrap()
    }

    fn discard(&self, extra: &[&str]) -> Value {
        let mut args = vec!["discard-cache", self.tree_str()];
        args.extend_from_slice(extra);
        self.ok(&args)["cache"].clone()
    }

    /// A Cargo profile directory: Cargo writes `.fingerprint/` into every one.
    fn profile(&self, relative: &str) {
        write(
            &self.tree.join(relative).join(".fingerprint/one/dep-lib"),
            "fingerprint",
        );
        write(
            &self.tree.join(relative).join("deps/libone.rlib"),
            &"x".repeat(8192),
        );
    }

    fn lifecycle(&self) -> String {
        let status = self.ok(&["status"]);
        status["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["id"] == "cache")
            .unwrap()["lifecycle"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

fn paths(cache: &Value, field: &str) -> Vec<String> {
    cache[field]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            entry
                .get("path")
                .unwrap_or(entry)
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect()
}

#[test]
fn records_inside_a_cargo_target_survive_and_only_profiles_are_discarded() {
    let fixture = Fixture::new();
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    write(&fixture.tree.join("target/.rustc_info.json"), "{}");
    fixture.profile("target/debug");
    fixture.profile("target/x86_64-unknown-linux-gnu/release");
    write(
        &fixture.tree.join("target/backlog-input/notes.md"),
        "a record a commit cites\n",
    );

    let planned = fixture.discard(&["--dry-run"]);
    assert_eq!(planned["applied"], false);
    assert_eq!(
        paths(&planned, "discarded"),
        ["target/debug", "target/x86_64-unknown-linux-gnu"]
    );
    assert_eq!(
        paths(&planned, "retained_ignored"),
        ["target/backlog-input"]
    );
    assert!(fixture.tree.join("target/debug/deps/libone.rlib").exists());

    let applied = fixture.discard(&[]);
    assert_eq!(applied["applied"], true);
    assert!(!fixture.tree.join("target/debug").exists());
    assert!(
        !fixture
            .tree
            .join("target/x86_64-unknown-linux-gnu")
            .exists()
    );
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("target/backlog-input/notes.md")).unwrap(),
        "a record a commit cites\n"
    );
    assert!(fixture.tree.join("target/CACHEDIR.TAG").exists());

    let refusal = fixture.refused(&["finish", "--discard-cache", fixture.tree_str()]);
    assert_eq!(refusal["code"], "worktree-dirty");
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains("target/backlog-input"),
        "{refusal}"
    );
    assert_eq!(fixture.lifecycle(), "active");

    let finished = fixture.ok(&["finish", "--discard-cache", "--archive", fixture.tree_str()]);
    assert_eq!(finished["evidence"]["operation"], "finish");
    let archive = PathBuf::from(finished["archive"]["path"].as_str().unwrap());
    let patch = std::fs::read_to_string(archive.join("dirty.patch")).unwrap();
    assert!(patch.contains("a record a commit cites"), "{patch}");
    assert!(!patch.contains("libone.rlib"));
    assert_eq!(fixture.lifecycle(), "finished");
}

#[test]
fn a_target_of_nothing_but_cache_goes_whole_and_the_tree_is_collected() {
    let fixture = Fixture::new();
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    std::fs::create_dir_all(fixture.tree.join("target/tmp")).unwrap();

    let finished = fixture.ok(&["finish", "--discard-cache", fixture.tree_str()]);
    assert_eq!(paths(&finished["cache"], "discarded"), ["target"]);
    assert_eq!(finished["cache"]["discarded"][0]["kind"], "cargo-target");
    assert!(finished.get("archive").is_none());
    assert!(!fixture.tree.join("target").exists());

    let repo = fixture.repository.to_str().unwrap();
    let review = fixture.ok(&["gc", "--repo", repo, "--dry-run", "--id", "cache"]);
    assert_eq!(review["assessments"][0]["eligible"], true, "{review}");
    fixture.ok(&["gc", "--repo", repo, "--apply", "--id", "cache"]);
    assert!(!fixture.tree.exists());
}

#[test]
fn dependencies_and_environments_are_cache_only_beside_tracked_sources() {
    let fixture = Fixture::new();
    write(&fixture.tree.join("web/node_modules/a/index.js"), "a");
    write(
        &fixture.tree.join("web/packages/ui/node_modules/b/index.js"),
        "b",
    );
    write(&fixture.tree.join("loose/node_modules/c/index.js"), "c");
    write(
        &fixture.tree.join("tools/.venv/pyvenv.cfg"),
        "home = /usr\n",
    );
    write(&fixture.tree.join(".venv/pyvenv.cfg"), "home = /usr\n");
    write(&fixture.tree.join(".pytest_cache/CACHEDIR.TAG"), TAG);
    write(&fixture.tree.join(".pytest_cache/v/cache/lastfailed"), "{}");

    let applied = fixture.discard(&[]);
    assert_eq!(
        paths(&applied, "discarded"),
        [
            ".pytest_cache",
            "tools/.venv",
            "web/node_modules",
            "web/packages/ui/node_modules"
        ]
    );
    assert_eq!(
        paths(&applied, "retained_ignored"),
        [".venv", "loose/node_modules"]
    );
    assert!(fixture.tree.join("loose/node_modules/c/index.js").exists());
    assert!(fixture.tree.join(".venv/pyvenv.cfg").exists());
    assert!(!fixture.tree.join("web/node_modules").exists());
}

#[test]
fn a_profile_reached_through_a_symlink_is_neither_followed_nor_deleted() {
    let fixture = Fixture::new();
    let outside = fixture.root.path().join("outside");
    write(&outside.join(".fingerprint/one/dep-lib"), "fingerprint");
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    std::os::unix::fs::symlink(&outside, fixture.tree.join("target/release")).unwrap();

    let applied = fixture.discard(&[]);
    assert_eq!(paths(&applied, "discarded"), ["target/debug"]);
    assert_eq!(paths(&applied, "retained_ignored"), ["target/release"]);
    assert!(outside.join(".fingerprint/one/dep-lib").exists());
    assert!(
        std::fs::symlink_metadata(fixture.tree.join("target/release"))
            .unwrap()
            .is_symlink()
    );
}

#[test]
fn a_live_lease_refuses_the_discard_but_not_the_review() {
    let fixture = Fixture::new();
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    fixture.ok(&[
        "hook",
        "session-start",
        "--path",
        fixture.tree_str(),
        "--session",
        "busy",
    ]);

    assert_eq!(
        paths(&fixture.discard(&["--dry-run"]), "discarded"),
        ["target"]
    );
    let refusal = fixture.refused(&["discard-cache", fixture.tree_str()]);
    assert_eq!(refusal["code"], "live-session");
    assert!(fixture.tree.join("target/debug/deps/libone.rlib").exists());
}

#[cfg(target_os = "linux")]
#[test]
fn a_process_working_in_the_tree_refuses_the_discard() {
    let fixture = Fixture::new();
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    let mut sleeper = Command::new("sleep")
        .arg("30")
        .current_dir(fixture.tree.join("target/debug"))
        .spawn()
        .unwrap();

    let refusal = fixture.refused(&["discard-cache", fixture.tree_str()]);
    sleeper.kill().unwrap();
    sleeper.wait().unwrap();
    assert_eq!(refusal["code"], "worktree-in-use");
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains(&sleeper.id().to_string()),
        "{refusal}"
    );
    assert!(fixture.tree.join("target/debug/deps/libone.rlib").exists());
}
