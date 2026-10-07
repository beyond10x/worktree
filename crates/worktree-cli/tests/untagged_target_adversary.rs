//! Adversary cases for story:untagged-cargo-target-is-cache: an ignored `target/` without a valid
//! `CACHEDIR.TAG` counts as a tagged Cargo target only when it is named `target`, a tracked
//! `Cargo.toml` sits beside it, and a direct child is a profile holding both `.fingerprint/` and
//! `deps/` as real directories. Each case below tries to make that rule delete something that is
//! not Cargo's rebuildable output, follow a symbolic link, or discard a case the story retains.

use serde_json::Value;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const MANIFEST: (&str, &str) = ("Cargo.toml", "[package]\nname = \"one\"\n");

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
    /// A managed tree whose commit holds `.gitignore` = `gitignore` and also tracks `files`, each
    /// force-added past `.gitignore`.
    fn new(gitignore: &str, files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("one");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        write(&repository.join("source"), "original\n");
        write(&repository.join(".gitignore"), gitignore);
        git(&repository, &["add", "."]);
        for (path, contents) in files {
            write(&repository.join(path), contents);
            git(&repository, &["add", "--force", "--", path]);
        }
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
            "untagged target adversary",
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

    fn discard(&self, extra: &[&str]) -> Value {
        let mut args = vec!["discard-cache", self.tree.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.ok(&args)["cache"].clone()
    }

    /// A full Cargo profile: `.fingerprint/` and `deps/`, both real directories.
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

    /// A directory holding `.fingerprint/` and no `deps/`.
    fn fingerprint_only(&self, relative: &str) {
        write(
            &self.tree.join(relative).join(".fingerprint/one/dep-lib"),
            "fingerprint",
        );
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.tree.join(relative))
            .unwrap_or_else(|error| panic!("{relative} was deleted: {error}"))
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

fn kinds(cache: &Value) -> Vec<String> {
    cache["discarded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["kind"].as_str().unwrap().to_owned())
        .collect()
}

/// `tmp/` is Cargo's `CARGO_TARGET_TMPDIR`; `CARGO_TARGET_TMP` documents that "no profile is ever
/// called this", and the classifier never treats it as one. So a `tmp/` holding `.fingerprint/`
/// and `deps/` is not "a profile holding both", and the only profile here holds `.fingerprint/`
/// alone: the story's contract retains this target ("with no profile holding both markers").
#[test]
fn a_tmp_holding_both_markers_does_not_qualify_an_untagged_target() {
    let fixture = Fixture::new("/target\n", &[MANIFEST]);
    fixture.profile("target/tmp");
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");
    fixture.fingerprint_only("target/debug");
    write(&fixture.tree.join("target/debug/notes.md"), "kept\n");
    write(&fixture.tree.join("target/.rustc_info.json"), "{}");

    let applied = fixture.discard(&[]);
    assert!(paths(&applied, "discarded").is_empty(), "{applied}");
    assert_eq!(paths(&applied, "retained_ignored"), ["target"], "{applied}");
    assert_eq!(fixture.read("target/debug/notes.md"), "kept\n");
    assert_eq!(fixture.read("target/tmp/output.log"), "scratch\n");
}

/// The same loophole through the other call site: Git reports `target/release/` and
/// `target/tmp/` on their own (a tracked file inside `target/`), and the parent counts only
/// because `tmp/` holds both markers.
#[test]
fn a_tmp_holding_both_markers_does_not_qualify_the_parent_of_a_profile_git_reports() {
    let fixture = Fixture::new("/target\n", &[MANIFEST, ("target/README.md", "kept\n")]);
    fixture.profile("target/tmp");
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");
    fixture.fingerprint_only("target/release");
    write(&fixture.tree.join("target/release/notes.md"), "kept\n");

    let applied = fixture.discard(&[]);
    assert!(paths(&applied, "discarded").is_empty(), "{applied}");
    assert_eq!(
        paths(&applied, "retained_ignored"),
        ["target/release", "target/tmp"],
        "{applied}"
    );
    assert_eq!(fixture.read("target/release/notes.md"), "kept\n");
    assert_eq!(fixture.read("target/tmp/output.log"), "scratch\n");
}

/// Before this unit the classifier passed `tagged = true` only for a target whose tag was valid,
/// so the `CACHEDIR.TAG` it treated as Cargo's root metadata was always Cargo's. An untagged
/// target now gets `tagged = true` too, and a `CACHEDIR.TAG` with a bad signature is not a file
/// Cargo writes.
#[test]
fn a_bad_signature_tag_in_an_untagged_target_is_not_cargo_metadata() {
    let fixture = Fixture::new("/target\n", &[MANIFEST]);
    fixture.profile("target/debug");
    write(
        &fixture.tree.join("target/CACHEDIR.TAG"),
        "Signature: not the cache directory tag\nwritten by hand\n",
    );

    let applied = fixture.discard(&[]);
    assert_eq!(paths(&applied, "discarded"), ["target/debug"], "{applied}");
    assert_eq!(kinds(&applied), ["cargo-profile"]);
    assert_eq!(
        paths(&applied, "retained_ignored"),
        ["target/CACHEDIR.TAG"],
        "{applied}"
    );
    assert_eq!(
        fixture.read("target/CACHEDIR.TAG"),
        "Signature: not the cache directory tag\nwritten by hand\n"
    );
    assert!(!fixture.tree.join("target/debug").exists());
}

/// Every target the unit's tests build sits at the tree root beside the root manifest, so a rule
/// that consulted the root `Cargo.toml` for every target would pass them all. Below the root
/// only the manifest in the target's own parent counts.
#[test]
fn an_untagged_target_below_the_root_counts_only_beside_its_own_manifest() {
    let fixture = Fixture::new(
        "target/\n",
        &[
            MANIFEST,
            ("crates/a/Cargo.toml", "[package]\nname = \"a\"\n"),
        ],
    );
    // Beside an untracked manifest, with the root manifest tracked.
    write(
        &fixture.tree.join("crates/b/Cargo.toml"),
        "[package]\nname = \"b\"\n",
    );
    fixture.profile("crates/a/target/debug");
    fixture.profile("crates/b/target/debug");

    let applied = fixture.discard(&[]);
    assert_eq!(
        paths(&applied, "discarded"),
        ["crates/a/target"],
        "{applied}"
    );
    assert_eq!(kinds(&applied), ["cargo-target"]);
    assert_eq!(
        paths(&applied, "retained_ignored"),
        ["crates/b/target"],
        "{applied}"
    );
    assert!(
        fixture
            .tree
            .join("crates/b/target/debug/deps/libone.rlib")
            .exists()
    );
}

#[test]
fn case_variants_of_the_target_name_are_retained() {
    let fixture = Fixture::new("/Target\n/TARGET\n", &[MANIFEST]);
    for name in ["Target", "TARGET"] {
        fixture.profile(&format!("{name}/debug"));
        write(&fixture.tree.join(name).join("tmp/output.log"), "scratch\n");
    }

    let applied = fixture.discard(&[]);
    assert!(paths(&applied, "discarded").is_empty(), "{applied}");
    let mut retained = paths(&applied, "retained_ignored");
    retained.sort();
    assert_eq!(retained, ["TARGET", "Target"]);
    assert_eq!(fixture.read("Target/tmp/output.log"), "scratch\n");
    assert_eq!(fixture.read("TARGET/tmp/output.log"), "scratch\n");
}

#[test]
fn markers_that_are_regular_files_do_not_qualify_an_untagged_target() {
    let fixture = Fixture::new("/target\n", &[MANIFEST]);
    write(&fixture.tree.join("target/debug/.fingerprint"), "a file\n");
    write(&fixture.tree.join("target/debug/deps/libone.rlib"), "rlib");
    write(
        &fixture.tree.join("target/release/.fingerprint/one/dep-lib"),
        "fingerprint",
    );
    write(&fixture.tree.join("target/release/deps"), "a file\n");
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");

    let applied = fixture.discard(&[]);
    assert!(paths(&applied, "discarded").is_empty(), "{applied}");
    assert_eq!(paths(&applied, "retained_ignored"), ["target"]);
    assert_eq!(fixture.read("target/debug/.fingerprint"), "a file\n");
    assert_eq!(fixture.read("target/release/deps"), "a file\n");
    assert_eq!(fixture.read("target/tmp/output.log"), "scratch\n");
}

/// Once a real full profile qualifies the target, a sibling profile whose `deps` is a symlink
/// out of the tree, a symlink inside `tmp/` and one inside the profile go with the target; what
/// they point at must not.
#[test]
fn symlinks_inside_a_discarded_untagged_target_are_not_followed() {
    let fixture = Fixture::new("/target\n", &[MANIFEST]);
    let outside = fixture.root.path().join("outside");
    write(&outside.join("deps/keep.rlib"), "outside deps\n");
    write(&outside.join("scratch/keep.log"), "outside scratch\n");
    fixture.profile("target/debug");
    symlink(outside.join("deps"), fixture.tree.join("target/debug/link")).unwrap();
    write(
        &fixture.tree.join("target/bench/.fingerprint/one/dep-lib"),
        "fingerprint",
    );
    symlink(outside.join("deps"), fixture.tree.join("target/bench/deps")).unwrap();
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");
    symlink(
        outside.join("scratch"),
        fixture.tree.join("target/tmp/link"),
    )
    .unwrap();

    let applied = fixture.discard(&[]);
    assert_eq!(paths(&applied, "discarded"), ["target"], "{applied}");
    assert_eq!(kinds(&applied), ["cargo-target"]);
    assert!(!fixture.tree.join("target").exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("deps/keep.rlib")).unwrap(),
        "outside deps\n"
    );
    assert_eq!(
        std::fs::read_to_string(outside.join("scratch/keep.log")).unwrap(),
        "outside scratch\n"
    );
}

/// Git reports a nested repository inside an ignored `target/` as `!! target/`; its `.git` and
/// its committed files must survive while the profile goes.
#[test]
fn an_untagged_target_that_is_a_nested_repository_keeps_its_repository() {
    let fixture = Fixture::new("/target\n", &[MANIFEST]);
    let nested = fixture.tree.join("target");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "-b", "main"]);
    write(&nested.join("notes.md"), "nested record\n");
    git(&nested, &["add", "notes.md"]);
    git(&nested, &["commit", "-m", "record"]);
    let head = git(&nested, &["rev-parse", "HEAD"]);
    fixture.profile("target/debug");

    let applied = fixture.discard(&[]);
    assert_eq!(paths(&applied, "discarded"), ["target/debug"], "{applied}");
    assert_eq!(
        paths(&applied, "retained_ignored"),
        ["target/.git", "target/notes.md"],
        "{applied}"
    );
    assert_eq!(git(&nested, &["rev-parse", "HEAD"]), head);
    assert_eq!(fixture.read("target/notes.md"), "nested record\n");
}

/// A file force-added inside an untagged target's profile makes Git descend into `target/`; the
/// tracked file must survive whatever Git reports instead.
#[test]
fn a_tracked_file_inside_an_untagged_target_profile_survives() {
    let fixture = Fixture::new("/target\n", &[MANIFEST, ("target/debug/keep.md", "kept\n")]);
    fixture.profile("target/debug");
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");
    write(&fixture.tree.join("target/.rustc_info.json"), "{}");

    let applied = fixture.discard(&[]);
    for discarded in paths(&applied, "discarded") {
        assert!(
            !Path::new("target/debug/keep.md").starts_with(&discarded),
            "{discarded} holds a tracked file: {applied}"
        );
    }
    assert_eq!(fixture.read("target/debug/keep.md"), "kept\n");
}

/// The story's premise, run against real Cargo: a `target/` made before Cargo's first build has
/// no `CACHEDIR.TAG`, and its profile is recognised while the record beside it stays.
#[test]
fn a_target_real_cargo_built_after_mkdir_is_recognised() {
    let manifest =
        "[package]\nname = \"one\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[workspace]\n";
    let fixture = Fixture::new(
        "/target\n",
        &[
            ("Cargo.toml", manifest),
            ("src/lib.rs", "pub fn one() {}\n"),
        ],
    );
    write(&fixture.tree.join("target/records/notes.md"), "a record\n");
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let built = Command::new(cargo)
        .args(["build", "--offline", "--quiet"])
        .current_dir(&fixture.tree)
        .env_remove("CARGO_TARGET_DIR")
        .env_remove("CARGO_BUILD_TARGET_DIR")
        .env_remove("CARGO_BUILD_BUILD_DIR")
        .env("RUSTC_WRAPPER", "")
        .env("CARGO_BUILD_RUSTC_WRAPPER", "")
        .output()
        .unwrap();
    assert!(
        built.status.success(),
        "{}",
        String::from_utf8_lossy(&built.stderr)
    );
    assert!(!fixture.tree.join("target/CACHEDIR.TAG").exists());
    assert!(fixture.tree.join("target/debug/.fingerprint").is_dir());

    let applied = fixture.discard(&[]);
    let discarded = paths(&applied, "discarded");
    assert!(
        discarded.iter().any(|path| path == "target/debug"),
        "{applied}"
    );
    assert!(
        discarded
            .iter()
            .all(|path| path == "target/debug" || path == "target/tmp"),
        "{applied}"
    );
    assert_eq!(
        paths(&applied, "retained_ignored"),
        ["target/records"],
        "{applied}"
    );
    assert_eq!(fixture.read("target/records/notes.md"), "a record\n");
    assert!(!fixture.tree.join("target/debug").exists());
}
