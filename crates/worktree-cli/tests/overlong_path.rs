//! A test fixture nesting directories past `PATH_MAX` below a Cargo target's `tmp/` does not
//! refuse `discard-cache`: the walks address what lies that deep relative to an open directory,
//! so it is classified and deleted with the target, without following a symbolic link and
//! without leaving the tree. A repository that deep is still retained, and a keep file that is
//! a symbolic link still refuses.
#![cfg(target_os = "linux")]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const TAG: &str =
    "Signature: 8a477f597d28d172789f06886806bc55\n# This file is a cache directory tag.\n";

/// The longest path the kernel resolves in one call, the terminating NUL included.
const PATH_MAX: usize = 4096;

/// The issue's shape: 1100 levels of `d/` in each half of the over-long chain. One half stays
/// addressable below a temporary directory, two do not.
const NARROW: (&str, usize) = ("d", 1100);

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

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(path, kind)| ((*path).to_owned(), (*kind).to_owned()))
        .collect()
}

/// The defect of GitHub #44: the keep-file lookup inside the fixture met `ENAMETOOLONG` and
/// refused the whole discard as `keep-file-invalid`. The target holds nothing but a profile and
/// scratch, so it is disposable whole, the profile with it; the deletion follows no symbolic
/// link out of the tree.
#[test]
fn a_target_whose_scratch_nests_past_path_max_is_reviewed_and_discarded() {
    let fixture = Fixture::new(&[]);
    let outside = fixture.root.path().join("outside");
    write(&outside.join("record"), "not the tree's\n");
    let overlong = fixture.overlong(NARROW, |deepest| {
        std::os::unix::fs::symlink(&outside, deepest.join("escape")).unwrap();
    });

    let planned = fixture.discard(&["--dry-run"]);
    assert_eq!(planned["applied"], false, "{planned}");
    assert_eq!(
        discarded(&planned),
        pairs(&[("target", "cargo-target")]),
        "the profile target/debug is disposable with its target: {planned}"
    );
    assert_eq!(paths(&planned, "retained_ignored"), [""; 0], "{planned}");
    assert!(fixture.tree.join("target/debug/.fingerprint").is_dir());
    assert!(fixture.bottom_holds(&overlong, "leaf"));

    let applied = fixture.discard(&[]);
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(discarded(&applied), pairs(&[("target", "cargo-target")]));
    assert!(!fixture.tree.join("target/debug").exists());
    assert!(
        std::fs::symlink_metadata(fixture.tree.join("target")).is_err(),
        "the over-long scratch below {} survived",
        overlong.bottom.display()
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("record")).unwrap(),
        "not the tree's\n",
        "the deletion followed a symbolic link out of the tree"
    );
    assert_eq!(
        git(&fixture.tree, &["status", "--porcelain", "--ignored"]),
        ""
    );
}

/// Beside a kept record the target falls to its parts, so the report names the profile itself,
/// and Cargo's `tmp/` with the over-long fixture goes on its own.
#[test]
fn beside_a_kept_record_the_profile_and_the_overlong_scratch_are_named_and_discarded() {
    let fixture = Fixture::new(&[(".worktreekeep", "target/backlog-input/\n")]);
    write(
        &fixture.tree.join("target/backlog-input/notes.md"),
        "a record a commit cites\n",
    );
    let overlong = fixture.overlong(NARROW, |_| {});
    let expected = pairs(&[
        ("target/CACHEDIR.TAG", "ignored-output"),
        ("target/debug", "cargo-profile"),
        ("target/tmp", "cargo-target-tmp"),
    ]);

    let planned = fixture.discard(&["--dry-run"]);
    assert_eq!(discarded(&planned), expected, "{planned}");
    assert_eq!(
        paths(&planned, "retained_ignored"),
        ["target/backlog-input/notes.md"]
    );
    assert!(fixture.bottom_holds(&overlong, "leaf"));

    let applied = fixture.discard(&[]);
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(discarded(&applied), expected, "{applied}");
    assert!(!fixture.tree.join("target/debug").exists());
    assert!(std::fs::symlink_metadata(fixture.tree.join("target/tmp")).is_err());
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("target/backlog-input/notes.md")).unwrap(),
        "a record a commit cites\n"
    );
}

/// A repository is never deleted, however deep: one past `PATH_MAX` keeps every directory above
/// it, the report names what is retained below `target/tmp`, and the profile still goes.
#[test]
fn a_repository_past_path_max_is_retained_and_the_rest_is_discarded() {
    let fixture = Fixture::new(&[]);
    let overlong = fixture.overlong(WIDE, |deepest| {
        std::fs::create_dir(deepest.join(".git")).unwrap();
    });

    let applied = fixture.discard(&[]);
    assert_eq!(applied["applied"], true, "{applied}");
    assert_eq!(
        discarded(&applied),
        pairs(&[
            ("target/CACHEDIR.TAG", "ignored-output"),
            ("target/debug", "cargo-profile"),
        ]),
        "{applied}"
    );
    let retained = paths(&applied, "retained_ignored");
    assert_eq!(retained.len(), 1, "{retained:?}");
    assert!(retained[0].starts_with("target/tmp/x/nnn"), "{retained:?}");
    assert!(!fixture.tree.join("target/debug").exists());
    assert!(fixture.bottom_holds(&overlong, ".git"));
    assert!(fixture.bottom_holds(&overlong, "leaf"));
}

/// `keep-file-invalid` stays the refusal for a keep file that is a symbolic link at a nested
/// directory that can be named, over-long scratch elsewhere or not, and nothing is deleted.
#[test]
fn a_linked_nested_keep_file_still_refuses_beside_an_overlong_scratch() {
    let fixture = Fixture::new(&[]);
    let overlong = fixture.overlong(NARROW, |_| {});
    write(&fixture.tree.join("tool/cache/b"), "cache\n");
    write(&fixture.tree.join("elsewhere"), "cache/\n");
    std::os::unix::fs::symlink("../elsewhere", fixture.tree.join("tool/.worktreekeep")).unwrap();

    for arguments in [
        vec!["discard-cache", "--dry-run", fixture.tree_str()],
        vec!["discard-cache", fixture.tree_str()],
    ] {
        let refusal = fixture.refused(&arguments);
        assert_eq!(refusal["code"], "keep-file-invalid", "{refusal}");
        let message = refusal["message"].as_str().unwrap();
        assert!(
            message.contains("tool/.worktreekeep is a symbolic link"),
            "{refusal}"
        );
    }
    assert!(fixture.tree.join("tool/cache/b").exists());
    assert!(fixture.tree.join("target/debug/.fingerprint").is_dir());
    assert!(fixture.bottom_holds(&overlong, "leaf"));
}

/// `finish --discard-cache --archive` on the fixture: the target goes whole, nothing is left to
/// archive, and the tree is collected.
#[test]
fn finish_discard_cache_archive_collects_a_tree_whose_scratch_nests_past_path_max() {
    let fixture = Fixture::new(&[]);
    fixture.overlong(NARROW, |_| {});

    let finished = fixture.ok(&["finish", "--discard-cache", "--archive", fixture.tree_str()]);
    assert_eq!(
        discarded(&finished["cache"]),
        pairs(&[("target", "cargo-target")]),
        "{finished}"
    );
    assert!(std::fs::symlink_metadata(fixture.tree.join("target")).is_err());

    let repo = fixture.repository.to_str().unwrap();
    let review = fixture.ok(&["gc", "--repo", repo, "--dry-run", "--id", "deep"]);
    assert_eq!(review["assessments"][0]["eligible"], true, "{review}");
    fixture.ok(&["gc", "--repo", repo, "--apply", "--id", "deep"]);
    assert!(!fixture.tree.exists());
}
