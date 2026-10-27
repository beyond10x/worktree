//! Adversarial cases for nested `.worktreekeep` files: patterns that try to leave their
//! directory, layouts a tool really writes (a self-ignoring `.takt/`), precedence against the
//! root keep file, directories the keep-file walk cannot enter, and whether the archive restores
//! what a nested keep file kept byte for byte.
#![cfg(unix)]

use serde_json::Value;
use std::os::unix::fs::PermissionsExt as _;
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim_end().into()
}

fn write(path: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
    let path = path.as_ref();
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

const IGNORE: &str = "/tool/\n/x\n/other/\n/target/\n";

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    remote: PathBuf,
}

impl Fixture {
    fn new(files: &[(&str, &str)]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("one");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        write(repository.join("source"), "original\n");
        write(repository.join(".gitignore"), IGNORE);
        for (path, contents) in files {
            write(repository.join(path), contents);
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
        let fixture = Self {
            root,
            repository,
            remote,
        };
        fixture.ok(&[
            "activate",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ]);
        fixture
    }

    fn run(&self, args: &[&str]) -> Output {
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
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn tree(&self, id: &str) -> PathBuf {
        let created = self.ok(&[
            "create",
            "--repo",
            self.repository.to_str().unwrap(),
            "--id",
            id,
            "--purpose",
            "nested keep adversary",
        ]);
        PathBuf::from(created["evidence"]["path"].as_str().unwrap())
    }
}

fn discarded(cache: &Value) -> Vec<String> {
    cache["discarded"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap().to_owned())
        .collect()
}

fn patch_paths(archive: &Path) -> Vec<String> {
    let patch = std::fs::read(archive.join("dirty.patch")).unwrap();
    String::from_utf8_lossy(&patch)
        .lines()
        .filter_map(|line| line.strip_prefix("diff --git a/"))
        .map(|rest| rest.split_once(" b/").unwrap().1.to_owned())
        .collect()
}

/// Archive `id`, retire its tree, strip the archive when it is strippable, and return its
/// directory and the dry-run item.
fn archived_and_stripped(fixture: &Fixture, id: &str) -> (PathBuf, Value) {
    let archive = PathBuf::from(
        fixture.ok(&["archive", id])["archive"]["path"]
            .as_str()
            .unwrap(),
    );
    fixture.ok(&["finish", id]);
    let repository = fixture.repository.to_str().unwrap();
    fixture.ok(&["gc", "--repo", repository, "--apply", "--id", id]);
    let directory = archive.file_name().unwrap().to_str().unwrap();
    let dry = fixture.ok(&[
        "prune-archives",
        "--strip-build-output",
        "--repo",
        repository,
    ]);
    let item = dry["archives"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["archive_directory"] == directory)
        .unwrap_or_else(|| panic!("{dry}"))
        .clone();
    if item["verdict"] == "strippable" {
        fixture.ok(&[
            "prune-archives",
            "--strip-build-output",
            "--repo",
            repository,
            "--apply",
            "--id",
            directory,
        ]);
    }
    (archive, item)
}

/// Restores a directory's mode on drop, so the temporary directory can be removed.
struct Sealed(PathBuf);

impl Drop for Sealed {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// Before this change an ignored directory this user cannot enter was retained and the rest
/// discarded ("one that cannot be read (a directory it cannot list ...) stay too"). The keep-file
/// walk now stats `<dir>/.worktreekeep` in every directory, and the permission error on an
/// untraversable directory refuses the whole discard as `keep-file-invalid`, though no keep
/// file exists anywhere in the tree.
#[test]
fn an_untraversable_directory_does_not_refuse_the_discard() {
    let fixture = Fixture::new(&[]);
    let tree = fixture.tree("sealed");
    write(tree.join("tool/cache/b"), "cache\n");
    let locked = tree.join("tool/locked");
    write(locked.join("state.json"), "{}\n");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    let _sealed = Sealed(locked.clone());
    if std::fs::read_dir(&locked).is_ok() {
        // Running as a user that enters everything: nothing to observe.
        return;
    }

    let output = fixture.run(&["discard-cache", tree.to_str().unwrap()]);
    assert!(
        output.status.success(),
        "discard-cache refused over an untraversable directory and no keep file: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(!tree.join("tool/cache").exists());
    assert!(std::fs::symlink_metadata(&locked).is_ok());
}

/// The takt layout: `.takt/.gitignore` = `*` hides the directory, `.takt/.worktreekeep` =
/// `/cases/` keeps the records. The `.gitignore` ignores itself, so `discard-cache` deletes it,
/// and the kept cases become untracked, unignored files: the tree turns dirty and the records
/// are one `git add -A` away from a commit.
#[test]
fn takt_layout_discard_keeps_the_gitignore_that_hides_the_cases() {
    let fixture = Fixture::new(&[]);
    let tree = fixture.tree("takt");
    write(tree.join(".takt/.gitignore"), "*\n");
    write(tree.join(".takt/.worktreekeep"), "/cases/\n");
    write(tree.join(".takt/cases/c1/record.json"), "{}\n");
    write(tree.join(".takt/scratch/tmp"), "scratch\n");
    assert_eq!(git(&tree, &["status", "--porcelain"]), "");

    let report = fixture.ok(&["discard-cache", tree.to_str().unwrap()]);
    assert!(tree.join(".takt/cases/c1/record.json").exists());
    assert!(
        tree.join(".takt/.gitignore").exists(),
        "discarded {:?}",
        discarded(&report["cache"])
    );
    assert_eq!(
        git(&tree, &["status", "--porcelain", "--untracked-files=all"]),
        "",
        "the kept cases are no longer ignored"
    );
}

/// A nested keep file adds what it keeps; it must not take away what the root keep file keeps.
/// Git gives the deeper file precedence, so `!cases/` in `tool/.worktreekeep` un-keeps
/// `tool/cases/`, which the root `.worktreekeep` names, and the records are deleted. Before this
/// change the nested file was not read and the records were kept.
#[test]
fn a_nested_negation_does_not_unkeep_what_the_root_keeps() {
    let fixture = Fixture::new(&[(".worktreekeep", "tool/cases/\n")]);
    let tree = fixture.tree("negated");
    write(tree.join("tool/.worktreekeep"), "!cases/\n");
    write(tree.join("tool/cases/a"), "root-kept record\n");
    write(tree.join("tool/cache/b"), "cache\n");

    let report = fixture.ok(&["discard-cache", tree.to_str().unwrap()]);
    assert!(
        tree.join("tool/cases/a").exists(),
        "the root keep file names tool/cases/, yet discarded {:?}",
        discarded(&report["cache"])
    );
}

/// Glob, escape and normalisation forms of patterns in a nested keep file reach nothing above
/// its directory, in `discard-cache` and in the strip.
#[test]
fn escape_forms_in_a_nested_keep_file_reach_nothing_outside() {
    let fixture = Fixture::new(&[]);
    let tree = fixture.tree("escape");
    write(
        tree.join("tool/.worktreekeep"),
        "**/../x\n..\\x\n\\../x\n../**\n/**/x\n../other/**\n**/other\n.././x\n./../x\n/.././other/\n",
    );
    write(tree.join("tool/y"), "disposable\n");
    write(tree.join("x"), "outside\n");
    write(tree.join("other/x"), "outside\n");

    let report = fixture.ok(&["discard-cache", "--dry-run", tree.to_str().unwrap()]);
    let gone = discarded(&report["cache"]);
    for path in ["x", "other", "tool/y"] {
        assert!(gone.contains(&path.to_owned()), "{path}: {gone:?}");
    }

    let (archive, item) = archived_and_stripped(&fixture, "escape");
    assert_eq!(item["verdict"], "strippable", "{item}");
    assert_eq!(patch_paths(&archive), ["tool/.worktreekeep"]);
}

/// CRLF line ends and trailing spaces in a nested keep file keep the same paths in
/// `discard-cache` and in the strip.
#[test]
fn crlf_and_trailing_spaces_keep_the_same_in_discard_and_strip() {
    let fixture = Fixture::new(&[]);
    let tree = fixture.tree("crlf");
    write(tree.join("tool/.worktreekeep"), "cases/\r\nlogs/   \n");
    write(tree.join("tool/cases/a"), "case\n");
    write(tree.join("tool/logs/l"), "log\n");
    write(tree.join("tool/cache/b"), "cache\n");

    let report = fixture.ok(&["discard-cache", "--dry-run", tree.to_str().unwrap()]);
    assert_eq!(discarded(&report["cache"]), ["tool/cache"], "{report}");

    let (archive, item) = archived_and_stripped(&fixture, "crlf");
    assert_eq!(item["verdict"], "strippable", "{item}");
    assert_eq!(
        patch_paths(&archive),
        ["tool/.worktreekeep", "tool/cases/a", "tool/logs/l"]
    );
}

/// A keep file two ignored directories down keeps its records in `discard-cache` and the strip.
#[test]
fn a_keep_file_in_an_ignored_directory_inside_an_ignored_directory() {
    let fixture = Fixture::new(&[]);
    let tree = fixture.tree("deep");
    write(tree.join("tool/.gitignore"), "inner/\n");
    write(tree.join("tool/inner/deeper/.worktreekeep"), "cases/\n");
    write(tree.join("tool/inner/deeper/cases/a"), "deep record\n");
    write(tree.join("tool/inner/deeper/cache/b"), "cache\n");

    let report = fixture.ok(&["discard-cache", "--dry-run", tree.to_str().unwrap()]);
    let gone = discarded(&report["cache"]);
    assert!(!gone.iter().any(|path| path.contains("cases")), "{gone:?}");
    assert!(
        gone.contains(&"tool/inner/deeper/cache".to_owned()),
        "{gone:?}"
    );

    let (archive, _) = archived_and_stripped(&fixture, "deep");
    assert!(
        patch_paths(&archive).contains(&"tool/inner/deeper/cases/a".to_owned()),
        "{:?}",
        patch_paths(&archive)
    );
}

/// A symbolic link to a directory outside the tree that holds a keep file of `*` is deleted
/// itself; the walk follows it nowhere and what it points at is untouched.
#[test]
fn a_linked_directory_holding_a_keep_file_is_not_followed() {
    let fixture = Fixture::new(&[]);
    let tree = fixture.tree("linked");
    let outside = fixture.root.path().join("outside");
    write(outside.join(".worktreekeep"), "*\n");
    write(outside.join("precious"), "outside the tree\n");
    write(tree.join("tool/y"), "disposable\n");
    std::os::unix::fs::symlink(&outside, tree.join("tool/link")).unwrap();

    let report = fixture.ok(&["discard-cache", tree.to_str().unwrap()]);
    assert!(
        tree.join("tool/link").symlink_metadata().is_err(),
        "{report}"
    );
    assert!(!tree.join("tool/y").exists());
    assert_eq!(
        std::fs::read_to_string(outside.join("precious")).unwrap(),
        "outside the tree\n"
    );
    assert!(outside.join(".worktreekeep").exists());
}

/// What a nested keep file kept, binary, non-UTF-8, executable and without a final newline,
/// comes back byte for byte and mode for mode when the archive's patch is applied over HEAD.
#[test]
fn the_archive_restores_kept_binary_bytes() {
    let fixture = Fixture::new(&[]);
    let tree = fixture.tree("restored");
    let bytes: Vec<u8> = (0u8..=255).chain([0, 0xff, 0xfe, b'\r']).collect();
    write(tree.join("tool/.worktreekeep"), "cases/\n");
    write(tree.join("tool/cases/blob"), &bytes);
    std::fs::set_permissions(
        tree.join("tool/cases/blob"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    write(tree.join("tool/cases/text"), "no final newline");
    std::os::unix::fs::symlink("blob", tree.join("tool/cases/link")).unwrap();
    write(tree.join("tool/cache/b"), "cache\n");

    let finished = fixture.ok(&[
        "finish",
        "--discard-cache",
        "--archive",
        tree.to_str().unwrap(),
    ]);
    let archive = PathBuf::from(finished["archive"]["path"].as_str().unwrap());

    let restore = fixture.root.path().join("restore");
    git(
        fixture.root.path(),
        &[
            "clone",
            "--quiet",
            fixture.remote.to_str().unwrap(),
            restore.to_str().unwrap(),
        ],
    );
    git(
        &restore,
        &[
            "apply",
            "--binary",
            archive.join("dirty.patch").to_str().unwrap(),
        ],
    );
    assert_eq!(
        std::fs::read(restore.join("tool/cases/blob")).unwrap(),
        bytes
    );
    let mode = std::fs::metadata(restore.join("tool/cases/blob"))
        .unwrap()
        .permissions()
        .mode();
    assert_ne!(mode & 0o100, 0, "executable bit lost");
    assert_eq!(
        std::fs::read(restore.join("tool/cases/text")).unwrap(),
        b"no final newline"
    );
    assert_eq!(
        std::fs::read_link(restore.join("tool/cases/link")).unwrap(),
        PathBuf::from("blob")
    );
    assert!(!restore.join("tool/cache/b").exists());
}

/// A nested keep file inside a Cargo profile keeps its records out of the build layout the
/// archive leaves out and the strip removes; the rest of the profile goes.
#[test]
fn a_nested_keep_file_in_a_cargo_profile_survives_archive_and_strip() {
    let fixture = Fixture::new(&[(
        "Cargo.toml",
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )]);
    let tree = fixture.tree("profile");
    write(
        tree.join("target/CACHEDIR.TAG"),
        "Signature: 8a477f597d28d172789f06886806bc55\n",
    );
    write(tree.join("target/debug/.fingerprint/f"), "fingerprint\n");
    write(tree.join("target/debug/deps/d"), "dep\n");
    write(tree.join("target/debug/records/.worktreekeep"), "*.json\n");
    write(
        tree.join("target/debug/records/r.json"),
        "{\"kept\":true}\n",
    );
    write(tree.join("target/debug/records/scratch.bin"), "scratch\n");

    let (archive, _) = archived_and_stripped(&fixture, "profile");
    let paths = patch_paths(&archive);
    assert!(
        paths.contains(&"target/debug/records/r.json".to_owned()),
        "{paths:?}"
    );
    assert!(
        !paths.contains(&"target/debug/deps/d".to_owned()),
        "{paths:?}"
    );
}

/// Negation lines of a nested keep file count for nothing, also against the other lines of the
/// same file: `!cases/` neither un-keeps `cases/` nor anything else its directory keeps.
#[test]
fn a_nested_negation_does_not_unkeep_its_own_directorys_keeps() {
    let fixture = Fixture::new(&[]);
    let tree = fixture.tree("own");
    write(
        tree.join("tool/.worktreekeep"),
        "cases/\nlogs/\n!cases/\n!logs/\n!*\n",
    );
    write(tree.join("tool/cases/a"), "case\n");
    write(tree.join("tool/logs/l"), "log\n");
    write(tree.join("tool/cache/b"), "cache\n");

    let report = fixture.ok(&["discard-cache", tree.to_str().unwrap()]);
    assert_eq!(discarded(&report["cache"]), ["tool/cache"], "{report}");
    assert!(tree.join("tool/cases/a").exists());
    assert!(tree.join("tool/logs/l").exists());
}

/// A deeper keep file still keeps a path a shallower one negates: the nested file adds keeps over
/// the root's `!`.
#[test]
fn a_nested_keep_wins_over_a_negation_in_the_root_keep_file() {
    let fixture = Fixture::new(&[(".worktreekeep", "tool/\n!tool/cases/\n")]);
    let tree = fixture.tree("deeper");
    write(tree.join("tool/.worktreekeep"), "cases/\n");
    write(tree.join("tool/cases/a"), "case\n");
    write(tree.join("tool/cache/b"), "cache\n");

    let report = fixture.ok(&["discard-cache", "--dry-run", tree.to_str().unwrap()]);
    let gone = discarded(&report["cache"]);
    assert!(!gone.iter().any(|path| path.contains("cases")), "{gone:?}");
}

/// The strip honours the same rules as `discard-cache`: the `.gitignore` beside a nested keep file
/// stays in the archive with what the keep file keeps, and a nested negation strips nothing the
/// root keeps.
#[test]
fn the_strip_keeps_the_gitignore_beside_a_nested_keep_file_and_ignores_negations() {
    let fixture = Fixture::new(&[
        (".gitignore", "/tool/\n/.takt/\n/target/\n"),
        (".worktreekeep", "tool/cases/\n"),
    ]);
    let tree = fixture.tree("stripped");
    write(tree.join(".takt/.gitignore"), "*\n");
    write(tree.join(".takt/.worktreekeep"), "/cases/\n");
    write(tree.join(".takt/cases/c1/record.json"), "{}\n");
    write(tree.join(".takt/scratch/tmp"), "scratch\n");
    write(tree.join("tool/.worktreekeep"), "!cases/\n");
    write(tree.join("tool/cases/a"), "root-kept record\n");
    write(tree.join("tool/cache/b"), "cache\n");

    let (archive, item) = archived_and_stripped(&fixture, "stripped");
    assert_eq!(item["verdict"], "strippable", "{item}");
    assert_eq!(
        patch_paths(&archive),
        [
            ".takt/.gitignore",
            ".takt/.worktreekeep",
            ".takt/cases/c1/record.json",
            "tool/.worktreekeep",
            "tool/cases/a",
        ]
    );
}
