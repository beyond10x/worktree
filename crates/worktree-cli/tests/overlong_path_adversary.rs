//! Adversarial cases for the over-long path re-anchoring of `discard-cache` (GitHub #44).
#![cfg(target_os = "linux")]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TAG: &str =
    "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag.\n";

/// The longest path the kernel resolves in one call, the terminating NUL included.
const PATH_MAX: usize = 4096;

/// The same length in few levels: 11 names of 200 bytes in each half. Splitting a chain down to
/// a retained entry costs more than its depth squared, so the case that splits uses this one.
const WIDE: (&str, usize) = (
    "nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn\
     nnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnnn",
    11,
);

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

/// `levels` directories named `name`, one inside the other.
fn chain((name, levels): (&str, usize)) -> PathBuf {
    std::iter::repeat_n(name, levels).collect()
}

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    tree: PathBuf,
}

/// The chain `<tree>/target/tmp/x/<name>/<name>/…` once it nests past `PATH_MAX`.
struct Overlong {
    /// The deepest directory of the first half, still addressable: it holds the second half
    /// under the chain's name.
    joint: PathBuf,
    /// The name and the levels of each half.
    shape: (&'static str, usize),
    /// The path of the chain's deepest directory, which no call can name.
    bottom: PathBuf,
}

impl Fixture {
    /// A managed tree of a repository that ignores `/target` and `/tool`, with a tagged Cargo
    /// target holding the profile `target/debug`. The commit also tracks `files`.
    fn new(files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("one");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        write(&repository.join("source"), "original\n");
        write(&repository.join(".gitignore"), "/target\n/tool\n");
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
            "deep",
            "--purpose",
            "over-long path",
        ]);
        fixture.tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
        write(
            &fixture.tree.join("target/debug/.fingerprint/one/dep-lib"),
            "fingerprint",
        );
        write(
            &fixture.tree.join("target/debug/deps/libone.rlib"),
            &"x".repeat(8192),
        );
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
            "{args:?}: {}",
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

    /// Nest `target/tmp/x/<name>/<name>/…` past `PATH_MAX`, as a test leaves it in `CARGO_TARGET_TMPDIR`.
    /// Both halves are built at addressable paths, `fill` writes into the deepest directory of
    /// the second, and one `rename` then hangs the second half below the first, so the test
    /// process never changes its working directory. The deepest directory holds a file.
    fn overlong(&self, shape: (&'static str, usize), fill: impl FnOnce(&Path)) -> Overlong {
        let joint = self.tree.join("target/tmp/x").join(chain(shape));
        std::fs::create_dir_all(&joint).unwrap();
        let staged = self.root.path().join("staged");
        let deepest = staged.join(chain(shape));
        std::fs::create_dir_all(&deepest).unwrap();
        write(&deepest.join("leaf"), "scratch\n");
        fill(&deepest);
        std::fs::rename(staged.join(shape.0), joint.join(shape.0)).unwrap();
        std::fs::remove_dir(&staged).unwrap();
        let bottom = joint.join(chain(shape));
        assert!(
            bottom.as_os_str().len() > PATH_MAX + 14,
            "{}",
            bottom.display()
        );
        assert_eq!(
            std::fs::symlink_metadata(&bottom)
                .unwrap_err()
                .raw_os_error(),
            Some(36),
            "the fixture's deepest directory can be named after all"
        );
        Overlong {
            joint,
            shape,
            bottom,
        }
    }

    /// Whether the second half of the chain still ends in `name`: the half is moved out to an
    /// addressable place, looked at, and moved back.
    fn bottom_holds(&self, overlong: &Overlong, name: &str) -> bool {
        let staged = self.root.path().join("looked-at");
        let (link, half) = overlong.shape;
        std::fs::rename(overlong.joint.join(link), &staged).unwrap();
        let held =
            std::fs::symlink_metadata(staged.join(chain((link, half - 1))).join(name)).is_ok();
        std::fs::rename(&staged, overlong.joint.join(link)).unwrap();
        held
    }
}

/// `(path, kind)` of every discarded entry.
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

/// Run `finish --discard-cache --archive` and then `gc --apply`; either may refuse. Returns
/// whether the tree still exists afterwards.
fn finish_and_collect(fixture: &Fixture) -> bool {
    let finished = fixture.command(&["finish", "--discard-cache", "--archive", fixture.tree_str()]);
    eprintln!(
        "finish: {}{}",
        String::from_utf8_lossy(&finished.stdout),
        String::from_utf8_lossy(&finished.stderr)
    );
    let repo = fixture.repository.to_str().unwrap();
    let collected = fixture.command(&["gc", "--repo", repo, "--apply", "--id", "deep"]);
    eprintln!(
        "gc: {}{}",
        String::from_utf8_lossy(&collected.stdout),
        String::from_utf8_lossy(&collected.stderr)
    );
    fixture.tree.exists()
}

/// A nested keep file past `PATH_MAX` keeps what lies beside it: "Keeping is not recovering: a
/// tree whose kept state the archive cannot image ... or no walk can read stays, and `finish
/// --archive` or `gc` refuses naming the path." The fix now reads the deep keep file (before it,
/// the discard refused), so the lifecycle must not end with the kept record gone.
#[test]
fn adversary_a_kept_record_past_path_max_survives_finish_and_gc() {
    let fixture = Fixture::new(&[]);
    let overlong = fixture.overlong(WIDE, |deepest| {
        write(&deepest.join(".worktreekeep"), "*\n");
        write(&deepest.join("record"), "a kept record\n");
    });
    let exists = finish_and_collect(&fixture);
    assert!(
        exists && fixture.bottom_holds(&overlong, "record"),
        "the record kept by {} was destroyed (tree exists: {exists})",
        overlong.bottom.join(".worktreekeep").display()
    );
}

/// A repository past `PATH_MAX` is retained by `discard-cache` (the unit's own case); the same
/// tree must not then be collected with that repository in it.
#[test]
fn adversary_a_repository_past_path_max_survives_finish_and_gc() {
    let fixture = Fixture::new(&[]);
    let overlong = fixture.overlong(WIDE, |deepest| {
        std::fs::create_dir(deepest.join(".git")).unwrap();
        write(&deepest.join(".git/HEAD"), "ref: refs/heads/main\n");
        std::fs::create_dir(deepest.join(".git/objects")).unwrap();
        std::fs::create_dir(deepest.join(".git/refs")).unwrap();
    });
    let exists = finish_and_collect(&fixture);
    assert!(
        exists && fixture.bottom_holds(&overlong, ".git"),
        "the repository at {} was destroyed (tree exists: {exists})",
        overlong.bottom.display()
    );
}

/// A tree whose own path is longer than the walk's re-anchoring threshold (1024 bytes) but far
/// below `PATH_MAX` is an ordinary tree: a nested keep file still keeps, and a symbolic link
/// out of the tree is not followed.
#[test]
fn adversary_an_ordinary_tree_below_a_long_path_behaves_as_before() {
    let fixture = Fixture::new(&[]);
    let long = fixture.tree.join("target/tmp").join(chain(("l", 600)));
    write(&long.join("tool/.worktreekeep"), "keep.md\n");
    write(&long.join("tool/keep.md"), "kept\n");
    write(&long.join("tool/drop.md"), "drop\n");
    let outside = fixture.root.path().join("outside");
    write(&outside.join("record"), "not the tree's\n");
    std::os::unix::fs::symlink(&outside, long.join("tool/escape")).unwrap();
    let applied = fixture.discard(&[]);
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(
        std::fs::read_to_string(long.join("tool/keep.md"))
            .ok()
            .as_deref(),
        Some("kept\n"),
        "{applied}"
    );
    assert!(!long.join("tool/drop.md").exists(), "{applied}");
    assert_eq!(
        std::fs::read_to_string(outside.join("record")).unwrap(),
        "not the tree's\n"
    );
}

/// The root keep file names `record` at any depth. Git's exclude engine (`kept_paths`) cannot
/// reach a file past `PATH_MAX`, so the record is not in the kept set, and the re-anchored walk
/// (`holds_protected`) reads past `PATH_MAX` but never asks whether a pattern keeps what it
/// finds: the target goes whole and the kept record with it. Before the fix the discard refused
/// as `keep-file-invalid` and deleted nothing. AGENTS.md: "a path is kept when a pattern matches
/// it or one of its ancestors".
#[test]
fn adversary_a_root_keep_pattern_keeps_a_record_past_path_max() {
    let fixture = Fixture::new(&[(".worktreekeep", "record\n")]);
    let overlong = fixture.overlong(WIDE, |deepest| {
        write(&deepest.join("record"), "a kept record\n");
    });

    let planned = fixture.discard(&["--dry-run"]);
    assert!(
        !discarded(&planned).contains(&("target".to_owned(), "cargo-target".to_owned())),
        "the root keep file keeps {}, yet the dry run plans its target whole: {planned}",
        overlong.bottom.join("record").display()
    );
    let applied = fixture.discard(&[]);
    assert!(
        overlong.joint.join(overlong.shape.0).exists() && fixture.bottom_holds(&overlong, "record"),
        "the record the root keep file keeps was deleted: {applied}"
    );
}

/// The same with a nested keep file at an addressable directory above the chain: its line
/// `record` keeps at any depth below its own directory.
#[test]
fn adversary_a_nested_keep_pattern_keeps_a_record_past_path_max() {
    let fixture = Fixture::new(&[]);
    write(&fixture.tree.join("target/tmp/x/.worktreekeep"), "record\n");
    let overlong = fixture.overlong(WIDE, |deepest| {
        write(&deepest.join("record"), "a kept record\n");
    });

    let applied = fixture.discard(&[]);
    assert!(
        overlong.joint.join(overlong.shape.0).exists() && fixture.bottom_holds(&overlong, "record"),
        "the record target/tmp/x/.worktreekeep keeps was deleted: {applied}"
    );
}
