//! Ignored entries are discarded, named by the structural rule that recognises them; what the
//! keep file keeps and every repository survive, and finish loses nothing that is kept.

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
            // A commit otherwise starts a detached `git maintenance run --auto`, which creates
            // `objects/maintenance.lock` inside a nested repository after the command returns.
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
    fn new() -> Self {
        Self::tracking(&[])
    }

    /// A fixture whose commit also tracks `files`, each force-added past `.gitignore`.
    fn tracking(files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("one");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        write(&repository.join("source"), "original\n");
        write(
            &repository.join(".gitignore"),
            "/target\n/build\nnode_modules/\n.venv/\n.pytest_cache/\n",
        );
        write(&repository.join("web/package-lock.json"), "{}\n");
        write(&repository.join("tools/pyproject.toml"), "[project]\n");
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

    /// Scratch as integration tests leave it in `CARGO_TARGET_TMPDIR`, holding no repository: a
    /// file and a subdirectory.
    fn plain_scratch(&self, relative: &str) {
        let scratch = self.tree.join(relative);
        write(&scratch.join("output.log"), "scratch\n");
        write(&scratch.join("case/one/state.json"), "{}\n");
    }

    /// Test scratch as integration tests leave it in `CARGO_TARGET_TMPDIR`: a file, a
    /// subdirectory and a nested Git repository `fixture` with one commit no remote holds, whose
    /// id it returns.
    fn test_scratch(&self, relative: &str) -> String {
        self.plain_scratch(relative);
        self.nested_repository(&format!("{relative}/fixture"))
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

    /// Whether the nested repository at `relative` still holds `commit`.
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

    /// A Cargo target as Cargo leaves one it did not create: a profile, test scratch in `tmp/`
    /// and `.rustc_info.json`, and no `CACHEDIR.TAG`.
    fn untagged_target(&self, relative: &str) -> String {
        self.profile(&format!("{relative}/debug"));
        let head = self.test_scratch(&format!("{relative}/tmp"));
        write(&self.tree.join(relative).join(".rustc_info.json"), "{}");
        head
    }

    /// What [`Self::untagged_target`] left at `relative` once it was discarded as an unrecognised
    /// directory: only the fixture repository, with its commit `head`.
    fn assert_only_the_fixture_survives(&self, relative: &str, head: &str) {
        let fixture = format!("{relative}/tmp/fixture");
        assert!(self.holds_commit(&fixture, head), "{fixture} lost {head}");
        assert_eq!(
            std::fs::read_to_string(self.tree.join(&fixture).join("file")).unwrap(),
            "nested\n"
        );
        for gone in ["debug", "tmp/output.log", "tmp/case", ".rustc_info.json"] {
            assert!(
                !self.tree.join(relative).join(gone).exists(),
                "{relative}/{gone} survived"
            );
        }
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

fn kinds(cache: &Value) -> Vec<String> {
    cache["discarded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["kind"].as_str().unwrap().to_owned())
        .collect()
}

/// `(path, kind)` of every discarded entry.
fn discarded(cache: &Value) -> Vec<(String, String)> {
    paths(cache, "discarded")
        .into_iter()
        .zip(kinds(cache))
        .collect()
}

fn pairs(expected: &[(&str, &str)]) -> Vec<(String, String)> {
    expected
        .iter()
        .map(|(path, kind)| ((*path).to_owned(), (*kind).to_owned()))
        .collect()
}

/// The keep file that keeps the record [`Fixture`]s write into `target/backlog-input/`.
const KEEP_BACKLOG: (&str, &str) = (".worktreekeep", "target/backlog-input/\n");

#[test]
fn a_kept_record_inside_a_cargo_target_survives_and_the_rest_of_the_target_goes() {
    let fixture = Fixture::tracking(&[KEEP_BACKLOG]);
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    write(&fixture.tree.join("target/.rustc_info.json"), "{}");
    fixture.profile("target/debug");
    fixture.profile("target/x86_64-unknown-linux-gnu/release");
    write(
        &fixture.tree.join("target/backlog-input/notes.md"),
        "a record a commit cites\n",
    );
    let expected = pairs(&[
        ("target/.rustc_info.json", "ignored-output"),
        ("target/CACHEDIR.TAG", "ignored-output"),
        ("target/debug", "cargo-profile"),
        ("target/x86_64-unknown-linux-gnu", "cargo-target"),
    ]);

    let planned = fixture.discard(&["--dry-run"]);
    assert_eq!(planned["applied"], false);
    assert_eq!(discarded(&planned), expected, "{planned}");
    assert_eq!(
        paths(&planned, "retained_ignored"),
        ["target/backlog-input/notes.md"]
    );
    assert!(fixture.tree.join("target/debug/deps/libone.rlib").exists());

    let applied = fixture.discard(&[]);
    assert_eq!(applied["applied"], true);
    assert_eq!(discarded(&applied), expected, "{applied}");
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
    assert!(!fixture.tree.join("target/CACHEDIR.TAG").exists());

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
fn a_target_whose_tmp_holds_only_scratch_goes_whole_and_the_tree_is_collected() {
    let fixture = Fixture::new();
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    fixture.plain_scratch("target/tmp");

    let finished = fixture.ok(&["finish", "--discard-cache", fixture.tree_str()]);
    assert_eq!(paths(&finished["cache"], "discarded"), ["target"]);
    assert_eq!(kinds(&finished["cache"]), ["cargo-target"]);
    assert!(finished.get("archive").is_none());
    assert!(!fixture.tree.join("target").exists());
    assert_eq!(fixture.lifecycle(), "finished");

    let repo = fixture.repository.to_str().unwrap();
    let review = fixture.ok(&["gc", "--repo", repo, "--dry-run", "--id", "cache"]);
    assert_eq!(review["assessments"][0]["eligible"], true, "{review}");
    fixture.ok(&["gc", "--repo", repo, "--apply", "--id", "cache"]);
    assert!(!fixture.tree.exists());
}

#[test]
fn beside_a_kept_record_the_target_tmp_is_discarded_on_its_own() {
    let fixture = Fixture::tracking(&[KEEP_BACKLOG]);
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    fixture.plain_scratch("target/tmp");
    write(
        &fixture.tree.join("target/backlog-input/notes.md"),
        "a record a commit cites\n",
    );
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
    assert!(fixture.tree.join("target/tmp/output.log").exists());

    let applied = fixture.discard(&[]);
    assert_eq!(applied["applied"], true);
    assert_eq!(discarded(&applied), expected, "{applied}");
    assert!(!fixture.tree.join("target/tmp").exists());
    assert!(!fixture.tree.join("target/debug").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("target/backlog-input/notes.md")).unwrap(),
        "a record a commit cites\n"
    );
}

/// Discarded scratch of [`Fixture::test_scratch`] in `target/tmp`, beside the profile
/// `target/debug` and the target's `CACHEDIR.TAG`: every direct entry of `tmp/` but the fixture
/// repository. The target holds a repository, so its own metadata goes on its own.
const SCRATCH_PARTS: [&str; 4] = [
    "target/CACHEDIR.TAG",
    "target/debug",
    "target/tmp/case",
    "target/tmp/output.log",
];
const SCRATCH_KINDS: [&str; 4] = [
    "ignored-output",
    "cargo-profile",
    "cargo-target-tmp",
    "cargo-target-tmp",
];

/// [`SCRATCH_PARTS`] of an untagged target ([`Fixture::untagged_target`]), which holds
/// `.rustc_info.json` instead of a tag.
const UNTAGGED_SCRATCH_PARTS: [&str; 4] = [
    "target/.rustc_info.json",
    "target/debug",
    "target/tmp/case",
    "target/tmp/output.log",
];

/// Integration tests write fixture repositories into `CARGO_TARGET_TMPDIR`; one may hold commits
/// no remote does, so `tmp/` falls to its direct entries and the repository is kept.
#[test]
fn a_nested_repository_in_target_tmp_is_never_discarded() {
    let fixture = Fixture::new();
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    let commit = fixture.test_scratch("target/tmp");

    let planned = fixture.discard(&["--dry-run"]);
    assert_eq!(paths(&planned, "discarded"), SCRATCH_PARTS, "{planned}");
    assert_eq!(kinds(&planned), SCRATCH_KINDS);
    assert_eq!(paths(&planned, "retained_ignored"), ["target/tmp/fixture"]);

    let applied = fixture.discard(&[]);
    assert_eq!(paths(&applied, "discarded"), SCRATCH_PARTS, "{applied}");
    assert_eq!(kinds(&applied), SCRATCH_KINDS);
    assert_eq!(paths(&applied, "retained_ignored"), ["target/tmp/fixture"]);
    assert!(!fixture.tree.join("target/debug").exists());
    assert!(!fixture.tree.join("target/tmp/output.log").exists());
    assert!(!fixture.tree.join("target/tmp/case").exists());
    assert!(
        fixture.holds_commit("target/tmp/fixture", &commit),
        "the fixture's commit {commit} was lost"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("target/tmp/fixture/file")).unwrap(),
        "nested\n"
    );
}

#[test]
fn finish_discard_cache_archive_images_a_fixture_repository_and_the_tree_is_collected() {
    let fixture = Fixture::new();
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    let commit = fixture.test_scratch("target/tmp");

    let finished = fixture.ok(&["finish", "--discard-cache", "--archive", fixture.tree_str()]);
    assert_eq!(
        paths(&finished["cache"], "discarded"),
        SCRATCH_PARTS,
        "{finished}"
    );
    assert_eq!(kinds(&finished["cache"]), SCRATCH_KINDS);
    assert!(fixture.holds_commit("target/tmp/fixture", &commit));
    let archive = &finished["archive"];
    let images: Vec<&str> = archive["manifest"]["nested_repositories"]
        .as_array()
        .unwrap_or_else(|| panic!("{archive}"))
        .iter()
        .map(|image| image["path"].as_str().unwrap())
        .collect();
    assert_eq!(images, ["target/tmp/fixture"], "{archive}");
    assert_eq!(fixture.lifecycle(), "finished");

    let repo = fixture.repository.to_str().unwrap();
    let review = fixture.ok(&["gc", "--repo", repo, "--dry-run", "--id", "cache"]);
    assert_eq!(review["assessments"][0]["eligible"], true, "{review}");
    fixture.ok(&["gc", "--repo", repo, "--apply", "--id", "cache"]);
    assert!(!fixture.tree.exists());
    let directory = PathBuf::from(archive["path"].as_str().unwrap());
    let image = archive["manifest"]["nested_repositories"][0]["image"]["file"]
        .as_str()
        .unwrap();
    assert!(directory.join(image).is_file(), "{archive}");
}

/// A profile holding a repository is never deleted whole: it falls to its direct entries, and
/// each directory above the repository falls in turn.
#[test]
fn a_profile_holding_a_nested_repository_falls_to_its_parts() {
    let fixture = Fixture::new();
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    fixture.profile("target/release");
    let commit = fixture.nested_repository("target/release/build/one/out/fixture");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[
            ("target/CACHEDIR.TAG", "ignored-output"),
            ("target/debug", "cargo-profile"),
            ("target/release/.fingerprint", "ignored-output"),
            ("target/release/deps", "ignored-output"),
        ]),
        "{applied}"
    );
    assert_eq!(
        paths(&applied, "retained_ignored"),
        ["target/release/build/one/out/fixture"]
    );
    assert!(!fixture.tree.join("target/debug").exists());
    assert!(!fixture.tree.join("target/release/deps").exists());
    assert!(fixture.holds_commit("target/release/build/one/out/fixture", &commit));
    assert_eq!(
        std::fs::read_to_string(
            fixture
                .tree
                .join("target/release/build/one/out/fixture/file")
        )
        .unwrap(),
        "nested\n"
    );
}

#[test]
fn node_modules_holding_a_nested_repository_falls_to_its_parts() {
    let fixture = Fixture::new();
    write(&fixture.tree.join("web/node_modules/a/index.js"), "a");
    let commit = fixture.nested_repository("web/node_modules/from-git");
    write(
        &fixture.tree.join("web/packages/ui/node_modules/b/index.js"),
        "b",
    );

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[
            ("web/node_modules/a", "ignored-output"),
            ("web/packages/ui/node_modules", "node-modules"),
        ]),
        "{applied}"
    );
    assert_eq!(
        paths(&applied, "retained_ignored"),
        ["web/node_modules/from-git"]
    );
    assert!(!fixture.tree.join("web/node_modules/a").exists());
    assert!(fixture.holds_commit("web/node_modules/from-git", &commit));
    assert!(!fixture.tree.join("web/packages/ui/node_modules").exists());
}

/// A `tmp/` is Cargo's scratch only directly below a tagged target that holds a profile; any
/// other is discarded as `ignored-output` with the directory it is in.
#[test]
fn a_tmp_outside_a_tagged_target_holding_a_profile_is_not_cargo_scratch() {
    let fixture = Fixture::new();
    // Untagged: a profile and a tmp, but no CACHEDIR.TAG.
    fixture.profile("build/debug");
    write(&fixture.tree.join("build/tmp/output.log"), "scratch\n");
    // Tagged, but no profile.
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    write(&fixture.tree.join("target/tmp/output.log"), "scratch\n");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[("build", "ignored-output"), ("target", "ignored-output")]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert!(!fixture.tree.join("build").exists());
    assert!(!fixture.tree.join("target").exists());
}

/// A symlinked `tmp/` is unlinked and never followed; a `tmp/` below a target-triple directory
/// is not Cargo's scratch. Neither is `cargo-target-tmp`.
#[test]
fn a_symlinked_tmp_or_one_below_a_target_triple_is_not_cargo_scratch() {
    let fixture = Fixture::new();
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    let elsewhere = fixture.tree.join("records");
    write(&elsewhere.join("notes.md"), "kept\n");
    std::os::unix::fs::symlink(&elsewhere, fixture.tree.join("target/tmp")).unwrap();
    fixture.profile("target/x86_64-unknown-linux-gnu/release");
    write(
        &fixture
            .tree
            .join("target/x86_64-unknown-linux-gnu/tmp/output.log"),
        "scratch\n",
    );

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[
            ("target/CACHEDIR.TAG", "ignored-output"),
            ("target/debug", "cargo-profile"),
            ("target/tmp", "ignored-output"),
            ("target/x86_64-unknown-linux-gnu/release", "cargo-profile"),
            ("target/x86_64-unknown-linux-gnu/tmp", "ignored-output"),
        ]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert_eq!(
        std::fs::read_to_string(elsewhere.join("notes.md")).unwrap(),
        "kept\n"
    );
    assert!(std::fs::symlink_metadata(fixture.tree.join("target/tmp")).is_err());
    assert!(
        !fixture
            .tree
            .join("target/x86_64-unknown-linux-gnu/tmp")
            .exists()
    );
}

const MANIFEST: (&str, &str) = ("Cargo.toml", "[package]\nname = \"one\"\n");

#[test]
fn an_untagged_target_holding_a_fixture_repository_falls_to_its_parts_and_the_tree_is_collected() {
    let fixture = Fixture::tracking(&[MANIFEST]);
    let commit = fixture.untagged_target("target");
    assert!(!fixture.tree.join("target/CACHEDIR.TAG").exists());

    let finished = fixture.ok(&["finish", "--discard-cache", "--archive", fixture.tree_str()]);
    assert_eq!(
        paths(&finished["cache"], "discarded"),
        UNTAGGED_SCRATCH_PARTS,
        "{finished}"
    );
    assert_eq!(kinds(&finished["cache"]), SCRATCH_KINDS);
    assert!(!fixture.tree.join("target/debug").exists());
    assert!(fixture.holds_commit("target/tmp/fixture", &commit));
    let images: Vec<&str> = finished["archive"]["manifest"]["nested_repositories"]
        .as_array()
        .unwrap_or_else(|| panic!("{finished}"))
        .iter()
        .map(|image| image["path"].as_str().unwrap())
        .collect();
    assert_eq!(images, ["target/tmp/fixture"], "{finished}");
    assert_eq!(fixture.lifecycle(), "finished");

    let repo = fixture.repository.to_str().unwrap();
    let review = fixture.ok(&["gc", "--repo", repo, "--dry-run", "--id", "cache"]);
    assert_eq!(review["assessments"][0]["eligible"], true, "{review}");
    fixture.ok(&["gc", "--repo", repo, "--apply", "--id", "cache"]);
    assert!(!fixture.tree.exists());
}

#[test]
fn beside_a_kept_record_an_untagged_target_discards_its_profile_and_scratch_but_not_a_fixture_repository()
 {
    let fixture = Fixture::tracking(&[MANIFEST, (".worktreekeep", "target/records/\n")]);
    let commit = fixture.untagged_target("target");
    write(
        &fixture.tree.join("target/records/notes.md"),
        "a record a commit cites\n",
    );
    let retained = ["target/records/notes.md", "target/tmp/fixture"];

    let planned = fixture.discard(&["--dry-run"]);
    assert_eq!(
        paths(&planned, "discarded"),
        UNTAGGED_SCRATCH_PARTS,
        "{planned}"
    );
    assert_eq!(kinds(&planned), SCRATCH_KINDS);
    assert_eq!(paths(&planned, "retained_ignored"), retained);

    let applied = fixture.discard(&[]);
    assert_eq!(
        paths(&applied, "discarded"),
        UNTAGGED_SCRATCH_PARTS,
        "{applied}"
    );
    assert_eq!(kinds(&applied), SCRATCH_KINDS);
    assert_eq!(paths(&applied, "retained_ignored"), retained);
    assert!(!fixture.tree.join("target/debug").exists());
    assert!(!fixture.tree.join("target/tmp/output.log").exists());
    assert!(!fixture.tree.join("target/tmp/case").exists());
    assert!(fixture.holds_commit("target/tmp/fixture", &commit));
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("target/records/notes.md")).unwrap(),
        "a record a commit cites\n"
    );
}

/// What an unrecognised directory holding [`Fixture::untagged_target`]'s layout at `relative`
/// discards: every part but the fixture repository, none of it named as Cargo's.
fn unrecognised_parts(relative: &str) -> Vec<(String, String)> {
    [".rustc_info.json", "debug", "tmp/case", "tmp/output.log"]
        .iter()
        .map(|part| (format!("{relative}/{part}"), "ignored-output".to_owned()))
        .collect()
}

#[test]
fn an_untagged_cargo_layout_under_another_name_is_not_cargo_cache() {
    let fixture = Fixture::tracking(&[MANIFEST]);
    let head = fixture.untagged_target("build");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        unrecognised_parts("build"),
        "{applied}"
    );
    assert_eq!(paths(&applied, "retained_ignored"), ["build/tmp/fixture"]);
    fixture.assert_only_the_fixture_survives("build", &head);
}

#[test]
fn an_untagged_target_beside_no_tracked_manifest_is_not_cargo_cache() {
    // A manifest tracked elsewhere, and one beside the target that Git does not track.
    let fixture = Fixture::tracking(&[("tools/Cargo.toml", MANIFEST.1)]);
    write(&fixture.tree.join("Cargo.toml"), MANIFEST.1);
    let head = fixture.untagged_target("target");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        unrecognised_parts("target"),
        "{applied}"
    );
    assert_eq!(paths(&applied, "retained_ignored"), ["target/tmp/fixture"]);
    fixture.assert_only_the_fixture_survives("target", &head);
    assert!(fixture.tree.join("Cargo.toml").is_file());
}

#[test]
fn an_untagged_target_whose_profile_lacks_deps_is_not_cargo_cache() {
    let fixture = Fixture::tracking(&[MANIFEST]);
    write(
        &fixture.tree.join("target/debug/.fingerprint/one/dep-lib"),
        "fingerprint",
    );
    let head = fixture.test_scratch("target/tmp");
    write(&fixture.tree.join("target/.rustc_info.json"), "{}");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        unrecognised_parts("target"),
        "{applied}"
    );
    assert_eq!(paths(&applied, "retained_ignored"), ["target/tmp/fixture"]);
    fixture.assert_only_the_fixture_survives("target", &head);
}

#[test]
fn a_symlinked_untagged_target_is_unlinked_and_never_followed() {
    let fixture = Fixture::tracking(&[MANIFEST]);
    let head = fixture.untagged_target("build");
    std::os::unix::fs::symlink("build", fixture.tree.join("target")).unwrap();

    let applied = fixture.discard(&[]);
    let mut expected = unrecognised_parts("build");
    expected.push(("target".to_owned(), "ignored-output".to_owned()));
    assert_eq!(discarded(&applied), expected, "{applied}");
    assert_eq!(paths(&applied, "retained_ignored"), ["build/tmp/fixture"]);
    assert!(std::fs::symlink_metadata(fixture.tree.join("target")).is_err());
    fixture.assert_only_the_fixture_survives("build", &head);
}

#[test]
fn an_untagged_target_whose_profile_or_markers_are_symlinks_is_not_followed() {
    let fixture = Fixture::tracking(&[MANIFEST]);
    let outside = fixture.root.path().join("outside");
    write(
        &outside.join("profile/.fingerprint/one/dep-lib"),
        "fingerprint",
    );
    write(&outside.join("profile/deps/libone.rlib"), "rlib");
    write(&outside.join("fingerprint/one/dep-lib"), "fingerprint");
    write(&outside.join("deps/libone.rlib"), "rlib");
    let target = fixture.tree.join("target");
    // A symlinked profile, a profile whose `.fingerprint` is a symlink, and one whose `deps` is.
    std::fs::create_dir_all(&target).unwrap();
    std::os::unix::fs::symlink(outside.join("profile"), target.join("debug")).unwrap();
    write(&target.join("release/deps/libone.rlib"), "rlib");
    std::os::unix::fs::symlink(
        outside.join("fingerprint"),
        target.join("release/.fingerprint"),
    )
    .unwrap();
    write(
        &target.join("bench/.fingerprint/one/dep-lib"),
        "fingerprint",
    );
    std::os::unix::fs::symlink(outside.join("deps"), target.join("bench/deps")).unwrap();
    let head = fixture.test_scratch("target/tmp");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[
            ("target/bench", "ignored-output"),
            ("target/debug", "ignored-output"),
            ("target/release", "ignored-output"),
            ("target/tmp/case", "ignored-output"),
            ("target/tmp/output.log", "ignored-output"),
        ]),
        "{applied}"
    );
    assert_eq!(paths(&applied, "retained_ignored"), ["target/tmp/fixture"]);
    for file in [
        "profile/.fingerprint/one/dep-lib",
        "profile/deps/libone.rlib",
        "fingerprint/one/dep-lib",
        "deps/libone.rlib",
    ] {
        assert!(outside.join(file).exists(), "outside/{file} was deleted");
    }
    assert!(fixture.holds_commit("target/tmp/fixture", &head));
}

#[test]
fn a_profile_git_reports_itself_is_discarded_below_an_untagged_target() {
    // A tracked file inside `target/` makes Git report `target/debug/` as the ignored entry.
    let fixture = Fixture::tracking(&[MANIFEST, ("target/README.md", "kept\n")]);
    fixture.profile("target/debug");

    let applied = fixture.discard(&[]);
    assert_eq!(paths(&applied, "discarded"), ["target/debug"], "{applied}");
    assert_eq!(kinds(&applied), ["cargo-profile"]);
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert!(!fixture.tree.join("target/debug").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("target/README.md")).unwrap(),
        "kept\n"
    );
}

#[test]
fn a_profile_git_reports_itself_is_ignored_output_beside_no_tracked_manifest() {
    let fixture = Fixture::tracking(&[("target/README.md", "kept\n")]);
    fixture.profile("target/debug");

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[("target/debug", "ignored-output")]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert!(!fixture.tree.join("target/debug").exists());
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("target/README.md")).unwrap(),
        "kept\n"
    );
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
        discarded(&applied),
        pairs(&[
            (".pytest_cache", "tool-cache"),
            (".venv", "ignored-output"),
            ("loose/node_modules", "ignored-output"),
            ("tools/.venv", "python-virtualenv"),
            ("web/node_modules", "node-modules"),
            ("web/packages/ui/node_modules", "node-modules"),
        ]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert!(!fixture.tree.join("loose/node_modules").exists());
    assert!(!fixture.tree.join(".venv").exists());
    assert!(!fixture.tree.join("web/node_modules").exists());
}

#[test]
fn a_profile_reached_through_a_symlink_is_unlinked_and_never_followed() {
    let fixture = Fixture::new();
    let outside = fixture.root.path().join("outside");
    write(&outside.join(".fingerprint/one/dep-lib"), "fingerprint");
    write(&fixture.tree.join("target/CACHEDIR.TAG"), TAG);
    fixture.profile("target/debug");
    std::os::unix::fs::symlink(&outside, fixture.tree.join("target/release")).unwrap();

    let applied = fixture.discard(&[]);
    assert_eq!(
        discarded(&applied),
        pairs(&[
            ("target/CACHEDIR.TAG", "ignored-output"),
            ("target/debug", "cargo-profile"),
            ("target/release", "ignored-output"),
        ]),
        "{applied}"
    );
    assert!(paths(&applied, "retained_ignored").is_empty(), "{applied}");
    assert!(outside.join(".fingerprint/one/dep-lib").exists());
    assert!(std::fs::symlink_metadata(fixture.tree.join("target/release")).is_err());
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
