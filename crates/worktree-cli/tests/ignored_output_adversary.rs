//! Adversary cases for story:ignored-entries-are-disposable-unless-kept. Each drives the CLI
//! against a line of the story's contract or of the documentation the unit wrote.

use serde_json::Value;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TAG: &str =
    "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag.\n";

const IGNORE: &str = "/target\n/scratch\n/out\n/link\nnode_modules/\n*.log\n";

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

struct Fixture {
    root: tempfile::TempDir,
    tree: PathBuf,
}

impl Fixture {
    fn tracking(files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("one");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        write(&repository.join("source"), "original\n");
        write(&repository.join(".gitignore"), IGNORE);
        for (path, contents) in files {
            write(&repository.join(path), contents);
        }
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
            repository.to_str().unwrap(),
            "--id",
            "adversary",
            "--purpose",
            "ignored output adversary",
        ]);
        fixture.tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        fixture
    }

    fn env(&self, command: &mut Command) {
        command
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
    }

    fn command(&self, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_worktree"));
        command.arg("--json").args(args);
        self.env(&mut command);
        command.output().unwrap()
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

    fn tree_str(&self) -> &str {
        self.tree.to_str().unwrap()
    }

    fn discard(&self, extra: &[&str]) -> Value {
        let mut args = vec!["discard-cache", self.tree_str()];
        args.extend_from_slice(extra);
        self.ok(&args)["cache"].clone()
    }

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
}

fn discarded_paths(cache: &Value) -> Vec<String> {
    cache["discarded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().to_owned())
        .collect()
}

fn retained(cache: &Value) -> Vec<String> {
    cache["retained_ignored"]
        .as_array()
        .unwrap()
        .iter()
        .map(|path| path.as_str().unwrap().to_owned())
        .collect()
}

/// Contract: "kept, a repository, on another device than the tree root, or not readable:
/// retained" (story Contract; the generated skill: "one that cannot be read stays too"). An
/// ignored file this user cannot read is still unlinked, alone or inside an ignored directory.
#[test]
fn an_ignored_file_that_cannot_be_read_is_retained() {
    let fixture = Fixture::tracking(&[]);
    let alone = fixture.tree.join("private.log");
    let inside = fixture.tree.join("scratch/owned-by-a-container.db");
    write(&alone, "unreadable\n");
    write(&inside, "unreadable\n");
    for path in [&alone, &inside] {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        assert!(std::fs::File::open(path).is_err(), "{}", path.display());
    }

    let applied = fixture.discard(&[]);
    let survived = (
        std::fs::symlink_metadata(&alone).is_ok(),
        std::fs::symlink_metadata(&inside).is_ok(),
    );
    for path in [&alone, &inside] {
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    assert_eq!(
        survived,
        (true, true),
        "unreadable ignored files were deleted: {applied}"
    );
}

/// Contract: "A path is kept when a pattern matches it or one of its ancestors." A keep pattern
/// that names an empty ignored directory (a mount point or layout placeholder) matches it, and
/// the directory is still deleted, because the kept set is built from the files Git lists.
#[test]
fn an_empty_directory_a_keep_pattern_names_is_kept() {
    let fixture = Fixture::tracking(&[(".worktreekeep", "scratch/mounts/\n")]);
    std::fs::create_dir_all(fixture.tree.join("scratch/mounts/data")).unwrap();
    write(&fixture.tree.join("scratch/other.txt"), "disposable\n");

    let applied = fixture.discard(&[]);
    assert!(
        fixture.tree.join("scratch/mounts/data").is_dir(),
        "a directory a keep pattern names was deleted: {applied}"
    );
    assert!(
        !discarded_paths(&applied).contains(&"scratch".to_owned()),
        "{applied}"
    );
}

/// `finish --discard-cache --archive` "archives whatever the tree still holds that no advertised
/// ref recovers, and finishes, so nothing kept is lost" (generated skill, AGENTS.md). With
/// `target/` kept, the target keeps its tag and profile, so `archive` still leaves its layout out
/// as build output: a kept record in Cargo's `tmp/` is neither deleted nor archived, and the tree
/// finishes, eligible for GC.
#[test]
fn finish_with_archive_archives_every_kept_path() {
    let fixture = Fixture::tracking(&[(".worktreekeep", "target/\n")]);
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    write(
        &fixture.tree.join("target/tmp/run-evidence.json"),
        "{\"kept\": \"a record a commit cites\"}\n",
    );
    write(
        &fixture.tree.join("target/debug/coverage-report.txt"),
        "kept coverage\n",
    );

    let planned = fixture.discard(&["--dry-run"]);
    assert!(discarded_paths(&planned).is_empty(), "{planned}");
    assert!(
        retained(&planned).contains(&"target/tmp/run-evidence.json".to_owned()),
        "{planned}"
    );

    let finished = fixture.ok(&["finish", "--discard-cache", "--archive", fixture.tree_str()]);
    let archive = PathBuf::from(finished["archive"]["path"].as_str().unwrap());
    let patch = std::fs::read_to_string(archive.join("dirty.patch")).unwrap_or_default();
    assert!(
        fixture.tree.join("target/tmp/run-evidence.json").is_file(),
        "the kept record was deleted"
    );
    assert!(
        patch.contains("a record a commit cites") && patch.contains("kept coverage"),
        "kept paths are missing from the archive the finish relies on: {finished}\n{patch}"
    );
}

/// "Every deletion is inside the tree" (story Contract) and "on another device than the tree
/// root: retained". A bind mount from the same filesystem has the tree's device number, so an
/// ignored directory holding one is deleted whole and the deletion runs through the mount into
/// the directory it shows. Built in an unprivileged user and mount namespace; skipped where
/// the host does not allow one.
#[test]
fn a_bind_mount_below_an_ignored_directory_is_not_deleted_through() {
    let probe = Command::new("unshare").args(["-rm", "true"]).output();
    if !probe.is_ok_and(|output| output.status.success()) {
        eprintln!("skipped: no unprivileged user and mount namespace here");
        return;
    }
    let fixture = Fixture::tracking(&[]);
    let outside = fixture.root.path().join("outside/data");
    write(&outside.join("precious.db"), "outside the tree\n");
    let mount_point = fixture.tree.join("scratch/mnt");
    std::fs::create_dir_all(&mount_point).unwrap();
    write(&fixture.tree.join("scratch/build.txt"), "disposable\n");

    let mut command = Command::new("unshare");
    command
        .args(["-rm", "sh", "-c"])
        .arg("mount --bind \"$1\" \"$2\" && exec \"$3\" --json discard-cache \"$4\"")
        .arg("sh")
        .arg(&outside)
        .arg(&mount_point)
        .arg(env!("CARGO_BIN_EXE_worktree"))
        .arg(&fixture.tree);
    fixture.env(&mut command);
    let output = command.output().unwrap();

    assert_eq!(
        std::fs::read_to_string(outside.join("precious.db")).ok(),
        Some("outside the tree\n".to_owned()),
        "a file outside the tree was deleted through a bind mount; stdout {} stderr {}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
