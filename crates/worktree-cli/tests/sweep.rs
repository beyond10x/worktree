//! A sweep discards idle trees' recognised build cache and archives expired ones, without changing
//! lifecycle, removing a tree, or touching a tree a session still leases.

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TAG: &str = "Signature: 8a477f597d28d172789f06886806bc55\n";

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
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("one");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        write(&repository.join("source"), "original\n");
        write(&repository.join(".gitignore"), "/target\n");
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
            "version = 1\nname = 'test'\nexpire_after_seconds = 1\nprotect_workspace_root = false\n",
        );
        let fixture = Self { root, repository };
        fixture.ok(&[
            "activate",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ]);
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

    /// A tree with a Cargo profile, a record inside `target/` and a commit no remote holds.
    fn tree(&self, id: &str) -> PathBuf {
        let created = self.ok(&[
            "create",
            "--repo",
            self.repository.to_str().unwrap(),
            "--id",
            id,
            "--purpose",
            "sweep",
        ]);
        let tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        write(&tree.join("target/CACHEDIR.TAG"), TAG);
        write(&tree.join("target/debug/.fingerprint/one/dep-lib"), "x");
        write(
            &tree.join("target/debug/deps/libone.rlib"),
            &"x".repeat(8192),
        );
        write(&tree.join("target/review/notes.md"), "kept\n");
        write(&tree.join("source"), "changed\n");
        git(&tree, &["commit", "-qam", "local work"]);
        tree
    }

    fn sweep(&self, extra: &[&str]) -> Vec<Value> {
        let mut args = vec![
            "sweep",
            "--repo",
            self.repository.to_str().unwrap(),
            "--idle-days",
            "0",
        ];
        args.extend_from_slice(extra);
        self.ok(&args)["items"].as_array().unwrap().clone()
    }

    fn lifecycle(&self, id: &str) -> String {
        self.ok(&["status"])["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["id"] == id)
            .unwrap()["lifecycle"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

fn item<'a>(items: &'a [Value], id: &str) -> &'a Value {
    items
        .iter()
        .find(|item| item["record"]["id"] == id)
        .unwrap_or_else(|| panic!("no sweep item for {id}: {items:?}"))
}

#[test]
fn an_idle_expired_tree_loses_only_cache_and_gains_an_archive_gc_accepts() {
    let fixture = Fixture::new();
    let idle = fixture.tree("idle");
    let leased = fixture.tree("leased");
    fixture.ok(&[
        "hook",
        "session-start",
        "--path",
        leased.to_str().unwrap(),
        "--session",
        "busy",
    ]);
    std::thread::sleep(std::time::Duration::from_secs(2));

    let planned = fixture.sweep(&["--dry-run"]);
    assert_eq!(item(&planned, "idle")["cache"]["applied"], false);
    assert!(item(&planned, "idle").get("archive").is_none());
    assert!(idle.join("target/debug").exists());

    let swept = fixture.sweep(&[]);
    let done = item(&swept, "idle");
    assert_eq!(
        done["cache"]["discarded"][0]["path"], "target/debug",
        "{done}"
    );
    assert!(done.get("refusal").is_none(), "{done}");
    let archive = PathBuf::from(done["archive"]["path"].as_str().unwrap());
    assert!(archive.join("commits.bundle").exists());
    assert!(!idle.join("target/debug").exists());
    assert_eq!(
        std::fs::read_to_string(idle.join("target/review/notes.md")).unwrap(),
        "kept\n"
    );
    assert_eq!(fixture.lifecycle("idle"), "active");

    let held = item(&swept, "leased");
    assert_eq!(held["refusal"]["code"], "live-session");
    assert!(leased.join("target/debug/deps/libone.rlib").exists());

    let review = fixture.ok(&[
        "gc",
        "--repo",
        fixture.repository.to_str().unwrap(),
        "--dry-run",
        "--id",
        "idle",
    ]);
    assert_eq!(review["assessments"][0]["eligible"], true, "{review}");
    assert!(idle.exists(), "a sweep never removes a tree");
}

#[test]
fn an_archive_larger_than_the_limit_is_refused_and_nothing_is_written() {
    let fixture = Fixture::new();
    let idle = fixture.tree("big");
    write(&idle.join("target/data/blob"), &"y".repeat(2 * 1024 * 1024));
    std::thread::sleep(std::time::Duration::from_secs(2));

    let swept = fixture.sweep(&["--max-archive-mib", "1"]);
    let big = item(&swept, "big");
    assert_eq!(big["refusal"]["code"], "archive-too-large", "{big}");
    assert!(big.get("archive").is_none());
    assert!(!idle.join("target/debug").exists());
    assert!(idle.join("target/data/blob").exists());
}
