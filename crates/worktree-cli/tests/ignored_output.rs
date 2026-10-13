//! story:ignored-entries-are-disposable-unless-kept: every ignored entry Git reports is
//! disposable unless the tree's `.worktreekeep` keeps it or it is or holds a repository. An
//! entry no structural rule recognises is discarded as `ignored-output`.

use serde_json::Value;
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
            // No detached `git maintenance run --auto` left inside a nested repository.
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
    repository: PathBuf,
    tree: PathBuf,
}

impl Fixture {
    /// A managed tree whose commit also tracks `files` (a `.worktreekeep`, for instance).
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
            "ignored",
            "--purpose",
            "ignored output",
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

    /// A Cargo profile directory: `.fingerprint/` and `deps/`.
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

    /// A tagged Cargo target with a profile, `.rustc_info.json` and a record in
    /// `target/backlog-input/`.
    fn target_with_record(&self) {
        write(&self.tree.join("target/CACHEDIR.TAG"), TAG);
        write(&self.tree.join("target/.rustc_info.json"), "{}");
        self.profile("target/debug");
        write(
            &self.tree.join("target/backlog-input/notes.md"),
            "a record a commit cites\n",
        );
    }

    /// A nested Git repository at `relative` with one commit no remote holds; returns its id.
    fn nested_repository(&self, relative: &str) -> String {
        let nested = self.tree.join(relative);
        std::fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "-b", "main"]);
        write(&nested.join("file"), "nested\n");
        git(&nested, &["add", "."]);
        git(&nested, &["commit", "-m", "nested fixture"]);
        git(&nested, &["rev-parse", "HEAD"])
    }

    fn holds_commit(&self, relative: &str, commit: &str) -> bool {
        Command::new("git")
            .arg("-C")
            .arg(self.tree.join(relative))
            .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
            .status
            .success()
    }

    fn names(&self, relative: &str) -> Vec<String> {
        let mut names: Vec<String> = std::fs::read_dir(self.tree.join(relative))
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        names
    }

    fn lifecycle(&self) -> String {
        let status = self.ok(&["status"]);
        status["records"]
            .as_array()
            .unwrap()
            .iter()
            .find(|record| record["id"] == "ignored")
            .unwrap()["lifecycle"]
            .as_str()
            .unwrap()
            .to_owned()
    }
}

/// `(path, kind)` of every discarded entry, in report order.
fn discarded(cache: &Value) -> Vec<(String, String)> {
    cache["discarded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| {
            (
                entry["path"].as_str().unwrap().to_owned(),
                entry["kind"].as_str().unwrap().to_owned(),
            )
        })
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

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(path, kind)| ((*path).to_owned(), (*kind).to_owned()))
        .collect()
}

/// Acceptance 1 and 7: without a keep file a record in `target/`, `node_modules` beside no
/// tracked lockfile and an ignored log are all discarded; the dry-run reports the same.
#[test]
fn without_a_keep_file_every_ignored_entry_is_discarded() {
    let fixture = Fixture::tracking(&[]);
    fixture.target_with_record();
    write(
        &fixture.tree.join("website/node_modules/left-pad/index.js"),
        "module.exports = 1;\n",
    );
    write(&fixture.tree.join("app.log"), "log line\n");
    let expected = pairs(&[
        ("app.log", "ignored-output"),
        ("target/.rustc_info.json", "ignored-output"),
        ("target/CACHEDIR.TAG", "ignored-output"),
        ("target/backlog-input", "ignored-output"),
        ("target/debug", "cargo-profile"),
        ("website/node_modules", "ignored-output"),
    ]);

    let planned = fixture.discard(&["--dry-run"]);
    assert_eq!(planned["applied"], false);
    assert_eq!(discarded(&planned), expected, "{planned}");
    assert!(retained(&planned).is_empty(), "{planned}");
    assert!(fixture.tree.join("target/backlog-input/notes.md").exists());
    assert!(fixture.tree.join("app.log").exists());

    let applied = fixture.discard(&[]);
    assert_eq!(applied["applied"], true);
    assert_eq!(discarded(&applied), expected, "{applied}");
    assert!(retained(&applied).is_empty(), "{applied}");
    assert!(!fixture.tree.join("target/backlog-input").exists());
    assert!(!fixture.tree.join("website/node_modules").exists());
    assert!(!fixture.tree.join("app.log").exists());
    assert_eq!(fixture.names("target"), Vec::<String>::new());

    // Nothing ignored is left, so plain finish needs no archive.
    fixture.ok(&["finish", "--discard-cache", fixture.tree_str()]);
    assert_eq!(fixture.lifecycle(), "finished");
}

/// Acceptance 2 and 7: the keep file keeps the record; every other entry of `target/` goes, and
/// finish refuses until `--archive` keeps the record in an archive.
#[test]
fn a_kept_record_survives_and_finish_archives_it() {
    let fixture = Fixture::tracking(&[(".worktreekeep", "target/backlog-input/\n")]);
    fixture.target_with_record();
    write(&fixture.tree.join("app.log"), "log line\n");
    let expected = pairs(&[
        ("app.log", "ignored-output"),
        ("target/.rustc_info.json", "ignored-output"),
        ("target/CACHEDIR.TAG", "ignored-output"),
        ("target/debug", "cargo-profile"),
    ]);

    let planned = fixture.discard(&["--dry-run"]);
    assert_eq!(discarded(&planned), expected, "{planned}");
    assert_eq!(
        retained(&planned),
        ["target/backlog-input/notes.md"],
        "{planned}"
    );

    let applied = fixture.discard(&[]);
    assert_eq!(discarded(&applied), expected, "{applied}");
    assert_eq!(
        retained(&applied),
        ["target/backlog-input/notes.md"],
        "{applied}"
    );
    assert_eq!(fixture.names("target"), ["backlog-input"]);
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("target/backlog-input/notes.md")).unwrap(),
        "a record a commit cites\n"
    );

    let refusal = fixture.refused(&["finish", "--discard-cache", fixture.tree_str()]);
    assert_eq!(refusal["code"], "worktree-dirty", "{refusal}");
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains("target/backlog-input"),
        "{refusal}"
    );
    assert_eq!(fixture.lifecycle(), "active");

    let finished = fixture.ok(&["finish", "--discard-cache", "--archive", fixture.tree_str()]);
    let archive = PathBuf::from(finished["archive"]["path"].as_str().unwrap());
    let patch = std::fs::read_to_string(archive.join("dirty.patch")).unwrap();
    assert!(patch.contains("a record a commit cites"), "{patch}");
    assert_eq!(fixture.lifecycle(), "finished");
}

/// Acceptance 3, with Git's own negation: a pattern that excludes the directory's contents
/// (`dir/*`) lets `!dir/keep.md` re-include one file, which is then discarded.
#[test]
fn a_negated_keep_pattern_discards_the_one_path_it_names() {
    let fixture = Fixture::tracking(&[(
        ".worktreekeep",
        "target/backlog-input/*\n!target/backlog-input/keep.md\n",
    )]);
    fixture.target_with_record();
    write(
        &fixture.tree.join("target/backlog-input/keep.md"),
        "scratch\n",
    );
    write(
        &fixture.tree.join("target/backlog-input/sub/deeper.md"),
        "another record\n",
    );

    let applied = fixture.discard(&[]);
    let gone: Vec<String> = discarded(&applied)
        .into_iter()
        .map(|(path, _)| path)
        .collect();
    assert!(
        gone.contains(&"target/backlog-input/keep.md".to_owned()),
        "{applied}"
    );
    assert!(!fixture.tree.join("target/backlog-input/keep.md").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("target/backlog-input/notes.md")).unwrap(),
        "a record a commit cites\n"
    );
    assert!(
        fixture
            .tree
            .join("target/backlog-input/sub/deeper.md")
            .exists()
    );
    assert!(!fixture.tree.join("target/debug").exists());
}

/// Acceptance 3 as the story words it: after `target/backlog-input/`, Git's exclude engine
/// cannot re-include a path whose parent directory is excluded (gitignore(5)), so
/// `!target/backlog-input/keep.md` changes nothing and every path below the directory is kept.
#[test]
fn a_negation_below_an_excluded_directory_keeps_what_git_keeps() {
    let fixture = Fixture::tracking(&[(
        ".worktreekeep",
        "target/backlog-input/\n!target/backlog-input/keep.md\n",
    )]);
    fixture.target_with_record();
    write(
        &fixture.tree.join("target/backlog-input/keep.md"),
        "scratch\n",
    );

    let applied = fixture.discard(&[]);
    assert_eq!(
        retained(&applied),
        [
            "target/backlog-input/keep.md",
            "target/backlog-input/notes.md"
        ],
        "{applied}"
    );
    assert!(fixture.tree.join("target/backlog-input/keep.md").exists());
    assert!(fixture.tree.join("target/backlog-input/notes.md").exists());
    assert!(!fixture.tree.join("target/debug").exists());
}

/// Acceptance 4: a nested repository three levels down keeps every directory above it from being
/// deleted whole; each of their other direct entries goes.
#[test]
fn a_nested_repository_at_depth_three_splits_the_directories_above_it() {
    let fixture = Fixture::tracking(&[]);
    let commit = fixture.nested_repository("scratch/a/b/fixture");
    write(&fixture.tree.join("scratch/other.log"), "log\n");
    write(&fixture.tree.join("scratch/dir/state.json"), "{}\n");
    write(&fixture.tree.join("scratch/a/c.txt"), "c\n");
    write(&fixture.tree.join("scratch/a/b/d/e.txt"), "e\n");
    let expected = pairs(&[
        ("scratch/a/b/d", "ignored-output"),
        ("scratch/a/c.txt", "ignored-output"),
        ("scratch/dir", "ignored-output"),
        ("scratch/other.log", "ignored-output"),
    ]);

    let planned = fixture.discard(&["--dry-run"]);
    assert_eq!(discarded(&planned), expected, "{planned}");
    assert_eq!(retained(&planned), ["scratch/a/b/fixture"], "{planned}");

    let applied = fixture.discard(&[]);
    assert_eq!(discarded(&applied), expected, "{applied}");
    assert_eq!(retained(&applied), ["scratch/a/b/fixture"], "{applied}");
    assert_eq!(fixture.names("scratch"), ["a"]);
    assert_eq!(fixture.names("scratch/a"), ["b"]);
    assert_eq!(fixture.names("scratch/a/b"), ["fixture"]);
    assert!(fixture.holds_commit("scratch/a/b/fixture", &commit));
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("scratch/a/b/fixture/file")).unwrap(),
        "nested\n"
    );
}

/// Acceptance 5: an ignored symbolic link to a directory outside the tree is unlinked, and one
/// inside an ignored directory goes with it; neither target is touched.
#[test]
fn an_ignored_symbolic_link_is_deleted_and_never_followed() {
    let fixture = Fixture::tracking(&[]);
    let outside = fixture.root.path().join("outside");
    write(&outside.join("records/notes.md"), "outside\n");
    std::os::unix::fs::symlink(outside.join("records"), fixture.tree.join("link")).unwrap();
    std::fs::create_dir_all(fixture.tree.join("out")).unwrap();
    std::os::unix::fs::symlink(outside.join("records"), fixture.tree.join("out/records")).unwrap();
    write(&fixture.tree.join("out/build.txt"), "built\n");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[("link", "ignored-output"), ("out", "ignored-output")]),
        "{applied}"
    );
    assert!(std::fs::symlink_metadata(fixture.tree.join("link")).is_err());
    assert!(!fixture.tree.join("out").exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("records/notes.md")).unwrap(),
        "outside\n"
    );
}

/// Acceptance 6: a keep file that is a symbolic link, or a directory, refuses the discard and
/// the dry-run alike, and nothing is deleted.
#[test]
fn a_keep_file_that_is_not_a_regular_file_refuses_and_deletes_nothing() {
    for shape in ["symlink", "directory"] {
        let fixture = Fixture::tracking(&[]);
        fixture.target_with_record();
        write(&fixture.tree.join("app.log"), "log line\n");
        let keep = fixture.tree.join(".worktreekeep");
        if shape == "symlink" {
            let elsewhere = fixture.root.path().join("keep");
            write(&elsewhere, "target/backlog-input/\n");
            std::os::unix::fs::symlink(&elsewhere, &keep).unwrap();
        } else {
            std::fs::create_dir(&keep).unwrap();
            write(&keep.join("patterns"), "target/backlog-input/\n");
        }

        for extra in [&["--dry-run"][..], &[][..]] {
            let mut args = vec!["discard-cache", fixture.tree_str()];
            args.extend_from_slice(extra);
            let refusal = fixture.refused(&args);
            assert_eq!(refusal["code"], "keep-file-invalid", "{shape}: {refusal}");
        }
        let refusal = fixture.refused(&["finish", "--discard-cache", fixture.tree_str()]);
        assert_eq!(refusal["code"], "keep-file-invalid", "{shape}: {refusal}");
        assert!(fixture.tree.join("target/debug/deps/libone.rlib").exists());
        assert!(fixture.tree.join("target/backlog-input/notes.md").exists());
        assert!(fixture.tree.join("app.log").exists());
        assert_eq!(fixture.lifecycle(), "active");
    }
}

/// The keep file is never deleted, even when Git ignores it.
#[test]
fn an_ignored_keep_file_is_itself_kept() {
    let fixture = Fixture::tracking(&[]);
    let exclude = PathBuf::from(git(
        &fixture.tree,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "info/exclude",
        ],
    ));
    write(&exclude, ".worktreekeep\n");
    write(&fixture.tree.join(".worktreekeep"), "/scratch/\n");
    write(&fixture.tree.join("scratch/notes.md"), "kept\n");
    write(&fixture.tree.join("app.log"), "log line\n");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[("app.log", "ignored-output")]),
        "{applied}"
    );
    assert_eq!(
        retained(&applied),
        [".worktreekeep", "scratch/notes.md"],
        "{applied}"
    );
    assert!(fixture.tree.join(".worktreekeep").is_file());
    assert!(fixture.tree.join("scratch/notes.md").is_file());
}
