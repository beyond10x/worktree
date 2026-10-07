//! A tree whose ignored files hold a nested Git repository is archived with a byte image of it
//! and retired without losing any of the repository's state.
#![cfg(unix)]

use serde_json::Value;
use std::collections::BTreeMap;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ID: &str = "nested";
const FIXTURE: &str = "evidence/run-1/fixture";

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    tree: PathBuf,
}

fn git(path: &Path, args: &[&str]) -> Output {
    Command::new("git")
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
            "init.defaultBranch=main",
            "-c",
            "protocol.file.allow=always",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
}

fn git_ok(path: &Path, args: &[&str]) -> String {
    let output = git(path, args);
    assert!(
        output.status.success(),
        "git {args:?} in {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap()
}

/// One entry of a directory listing: kind, permission bits and content (file bytes or link
/// target; empty for a directory).
type Listing = BTreeMap<Vec<u8>, (char, u32, Vec<u8>)>;

/// Every entry at and below `dir`, symlinks not followed, keyed by path relative to `dir`.
fn listing(dir: &Path) -> Listing {
    fn walk(base: &Path, dir: &Path, out: &mut Listing) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let relative = path
                .strip_prefix(base)
                .unwrap()
                .as_os_str()
                .as_bytes()
                .to_vec();
            let mode = metadata.permissions().mode() & 0o7777;
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.insert(
                    relative,
                    ('l', mode, target.as_os_str().as_bytes().to_vec()),
                );
            } else if metadata.is_dir() {
                out.insert(relative, ('d', mode, Vec::new()));
                walk(base, &path, out);
            } else {
                assert!(metadata.is_file(), "{}", path.display());
                out.insert(relative, ('f', mode, std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Listing::new();
    let mode = std::fs::metadata(dir).unwrap().permissions().mode() & 0o7777;
    out.insert(Vec::new(), ('d', mode, Vec::new()));
    walk(dir, dir, &mut out);
    out
}

/// Repository state that must survive: HEAD, status, stash.
fn repository_state(repository: &Path) -> (String, String, String) {
    (
        git_ok(repository, &["rev-parse", "HEAD"]),
        git_ok(
            repository,
            &[
                "--no-optional-locks",
                "status",
                "--porcelain=v2",
                "--untracked-files=all",
            ],
        ),
        git_ok(repository, &["stash", "list"]),
    )
}

/// A nested repository with one commit, a stash entry, a staged change, an unstaged edit, an
/// untracked file and an untracked symlink.
fn nested_repository(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git_ok(path, &["init", "--quiet"]);
    std::fs::write(path.join("a.txt"), "one\n").unwrap();
    std::fs::write(path.join("b.txt"), "two\n").unwrap();
    std::fs::write(path.join("run.sh"), "#!/bin/sh\necho fixture\n").unwrap();
    std::fs::set_permissions(path.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    git_ok(path, &["add", "."]);
    git_ok(path, &["commit", "--quiet", "-m", "fixture"]);
    std::fs::write(path.join("a.txt"), "stashed\n").unwrap();
    git_ok(path, &["stash", "push", "--quiet", "-m", "parked"]);
    std::fs::write(path.join("b.txt"), "staged\n").unwrap();
    git_ok(path, &["add", "b.txt"]);
    std::fs::write(path.join("a.txt"), "unstaged\n").unwrap();
    std::fs::write(path.join("notes.txt"), "untracked\n").unwrap();
    std::os::unix::fs::symlink("notes.txt", path.join("pointer")).unwrap();
}

impl Fixture {
    /// An active managed tree whose committed `.gitignore` ignores `evidence/`.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("demo");
        std::fs::create_dir_all(&repository).unwrap();
        git_ok(&repository, &["init", "--quiet"]);
        std::fs::write(repository.join("README.md"), "demo\n").unwrap();
        git_ok(&repository, &["add", "."]);
        git_ok(&repository, &["commit", "--quiet", "-m", "init"]);
        let remote = root.path().join("remote.git");
        git_ok(
            root.path(),
            &[
                "clone",
                "--quiet",
                "--bare",
                repository.to_str().unwrap(),
                remote.to_str().unwrap(),
            ],
        );
        git_ok(
            &repository,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        let profile = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles/default.toml");
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
            ID,
            "--purpose",
            "nested",
        ]);
        fixture.tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        std::fs::write(fixture.tree.join(".gitignore"), "evidence/\n").unwrap();
        git_ok(&fixture.tree, &["add", ".gitignore"]);
        git_ok(
            &fixture.tree,
            &["commit", "--quiet", "-m", "ignore evidence"],
        );
        fixture
    }

    /// [`Self::new`] plus a nested repository in the ignored evidence directory and an ignored
    /// file beside it.
    fn with_nested_repository() -> Self {
        let fixture = Self::new();
        nested_repository(&fixture.nested());
        std::fs::write(fixture.tree.join("evidence/run-1/log.txt"), "outer log\n").unwrap();
        fixture
    }

    fn nested(&self) -> PathBuf {
        self.tree.join(FIXTURE)
    }

    fn tree_str(&self) -> &str {
        self.tree.to_str().unwrap()
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
            "worktree {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn refused(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            !output.status.success(),
            "worktree {args:?} unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stderr).unwrap()
    }

    fn gc(&self, mode: &str) -> Value {
        let report = self.ok(&[
            "gc",
            "--repo",
            self.repository.to_str().unwrap(),
            mode,
            "--id",
            ID,
        ]);
        let assessments = report["assessments"].as_array().unwrap();
        assert_eq!(assessments.len(), 1, "{report}");
        assessments[0].clone()
    }

    fn archive_dir(&self) -> PathBuf {
        std::fs::canonicalize(self.root.path())
            .unwrap()
            .join("state/worktree/archives/demo")
            .join(ID)
    }

    fn manifest(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.archive_dir().join("manifest.json")).unwrap())
            .unwrap()
    }

    /// Every archive file's bytes, by name.
    fn archive_bytes(&self) -> BTreeMap<String, Vec<u8>> {
        std::fs::read_dir(self.archive_dir())
            .unwrap()
            .map(|entry| {
                let entry = entry.unwrap();
                (
                    entry.file_name().to_string_lossy().into_owned(),
                    std::fs::read(entry.path()).unwrap(),
                )
            })
            .collect()
    }
}

fn refusal(assessment: &Value) -> &str {
    assessment["refusal"]["code"].as_str().unwrap_or("<none>")
}

#[test]
fn finish_archive_writes_an_image_that_tar_restores_with_every_repository_state() {
    let fixture = Fixture::with_nested_repository();
    let original = listing(&fixture.nested());
    let state = repository_state(&fixture.nested());
    assert!(state.2.contains("parked"), "{state:?}");

    let finished = fixture.ok(&["finish", "--archive", fixture.tree_str()]);
    assert!(finished["archive"].is_object(), "{finished}");
    assert!(finished["archive"]["blocker"].is_null(), "{finished}");

    let manifest = fixture.manifest();
    assert_eq!(manifest["format"], "worktree.archive/2", "{manifest}");
    let images = manifest["nested_repositories"].as_array().unwrap();
    assert_eq!(images.len(), 1, "{manifest}");
    assert_eq!(images[0]["path"], FIXTURE);
    assert_eq!(images[0]["image"]["file"], "nested-1.tar");
    assert_eq!(images[0]["entries"], original.len() as u64);
    assert_eq!(images[0]["fingerprint"].as_str().unwrap().len(), 64);
    // The archive never modifies the tree.
    assert_eq!(listing(&fixture.nested()), original);

    let restored = fixture.root.path().join("restored");
    std::fs::create_dir(&restored).unwrap();
    let extracted = Command::new("tar")
        .arg("-xpf")
        .arg(fixture.archive_dir().join("nested-1.tar"))
        .arg("-C")
        .arg(&restored)
        .output()
        .unwrap();
    assert!(
        extracted.status.success(),
        "{}",
        String::from_utf8_lossy(&extracted.stderr)
    );
    let copy = restored.join(FIXTURE);
    assert_eq!(listing(&copy), original);
    assert_eq!(repository_state(&copy), state);
    let fsck = git(&copy, &["fsck", "--strict"]);
    assert!(
        fsck.status.success(),
        "{}",
        String::from_utf8_lossy(&fsck.stderr)
    );
}

#[test]
fn gc_retires_a_tree_whose_nested_repository_is_imaged_and_keeps_the_archive() {
    let fixture = Fixture::with_nested_repository();
    fixture.ok(&["finish", "--archive", fixture.tree_str()]);
    let archived = fixture.archive_bytes();
    assert!(
        archived.contains_key("nested-1.tar"),
        "{:?}",
        archived.keys()
    );

    let review = fixture.gc("--dry-run");
    assert_eq!(review["eligible"], true, "{review}");
    assert!(fixture.nested().join(".git").is_dir());

    let applied = fixture.gc("--apply");
    assert!(applied["evidence"].is_object(), "{applied}");
    assert!(!fixture.tree.exists());
    assert_eq!(fixture.archive_bytes(), archived);
}

#[test]
fn a_byte_changed_below_the_nested_git_after_archiving_refuses_removal() {
    let fixture = Fixture::with_nested_repository();
    fixture.ok(&["finish", "--archive", fixture.tree_str()]);
    let archived = fixture.archive_bytes();
    let config = fixture.nested().join(".git/config");
    let mut changed = std::fs::read(&config).unwrap();
    changed.push(b'\n');
    std::fs::write(&config, &changed).unwrap();
    let before = listing(&fixture.tree);

    let applied = fixture.gc("--apply");
    assert_eq!(refusal(&applied), "archive-stale", "{applied}");
    assert_eq!(listing(&fixture.tree), before);
    assert_eq!(std::fs::read(&config).unwrap(), changed);
    assert_eq!(fixture.archive_bytes(), archived);
}

#[test]
fn a_second_nested_repository_after_archiving_refuses_removal() {
    let fixture = Fixture::with_nested_repository();
    fixture.ok(&["finish", "--archive", fixture.tree_str()]);
    let archived = fixture.archive_bytes();
    let second = fixture.tree.join("evidence/run-2/fixture");
    std::fs::create_dir_all(&second).unwrap();
    git_ok(&second, &["init", "--quiet"]);
    std::fs::write(second.join("late.txt"), "late\n").unwrap();
    git_ok(&second, &["add", "."]);
    git_ok(&second, &["commit", "--quiet", "-m", "late"]);
    let before = listing(&fixture.tree);

    let applied = fixture.gc("--apply");
    assert_eq!(refusal(&applied), "archive-stale", "{applied}");
    assert_eq!(listing(&fixture.tree), before);
    assert_eq!(fixture.archive_bytes(), archived);
}

/// `archive` refuses, naming the nested repository and the reason.
fn assert_unsupported(fixture: &Fixture, path: &str, reason: &str) {
    let refused = fixture.refused(&["archive", fixture.tree_str()]);
    assert_eq!(refused["code"], "archive-unsupported-entry", "{refused}");
    let message = refused["message"].as_str().unwrap();
    assert!(message.contains(path), "{message}");
    assert!(message.contains(reason), "{message}");
    assert!(!fixture.archive_dir().exists());
}

#[test]
fn a_nested_repository_whose_git_is_a_gitfile_is_refused() {
    let fixture = Fixture::new();
    let separate = fixture.root.path().join("separate.git");
    let nested = fixture.nested();
    std::fs::create_dir_all(&nested).unwrap();
    git_ok(
        &nested,
        &[
            "init",
            "--quiet",
            "--separate-git-dir",
            separate.to_str().unwrap(),
        ],
    );
    std::fs::write(nested.join("a.txt"), "one\n").unwrap();
    git_ok(&nested, &["add", "."]);
    git_ok(&nested, &["commit", "--quiet", "-m", "fixture"]);
    assert!(nested.join(".git").is_file());

    assert_unsupported(&fixture, FIXTURE, ".git is not a directory");
}

#[test]
fn a_nested_repository_with_alternates_is_refused() {
    let fixture = Fixture::new();
    let nested = fixture.nested();
    std::fs::create_dir_all(nested.parent().unwrap()).unwrap();
    git_ok(
        fixture.root.path(),
        &[
            "clone",
            "--quiet",
            "--shared",
            fixture.repository.to_str().unwrap(),
            nested.to_str().unwrap(),
        ],
    );
    assert!(nested.join(".git/objects/info/alternates").is_file());

    assert_unsupported(
        &fixture,
        &format!("{FIXTURE}/.git/objects/info/alternates"),
        "alternates",
    );
}

#[test]
fn a_nested_repository_head_records_as_a_submodule_is_refused() {
    let fixture = Fixture::new();
    let nested = fixture.tree.join("vendor/sub");
    std::fs::create_dir_all(&nested).unwrap();
    git_ok(&nested, &["init", "--quiet"]);
    std::fs::write(nested.join("a.txt"), "one\n").unwrap();
    git_ok(&nested, &["add", "."]);
    git_ok(&nested, &["commit", "--quiet", "-m", "sub"]);
    git_ok(&fixture.tree, &["add", "vendor/sub"]);
    git_ok(&fixture.tree, &["commit", "--quiet", "-m", "gitlink"]);

    assert_unsupported(&fixture, "vendor/sub", "160000");
}

#[test]
fn a_nested_repository_over_tracked_files_is_refused() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.tree.join("lib")).unwrap();
    std::fs::write(fixture.tree.join("lib/tracked.txt"), "tracked\n").unwrap();
    git_ok(&fixture.tree, &["add", "lib/tracked.txt"]);
    git_ok(&fixture.tree, &["commit", "--quiet", "-m", "lib"]);
    git_ok(&fixture.tree.join("lib"), &["init", "--quiet"]);

    assert_unsupported(&fixture, "lib", "tracks lib/tracked.txt");
}

#[test]
fn a_tree_without_nested_repositories_keeps_the_version_1_manifest() {
    let fixture = Fixture::new();
    std::fs::create_dir_all(fixture.tree.join("evidence")).unwrap();
    std::fs::write(fixture.tree.join("evidence/log.txt"), "log\n").unwrap();

    let archived = fixture.ok(&["archive", fixture.tree_str()]);
    let manifest = fixture.manifest();
    assert_eq!(archived["archive"]["manifest"], manifest);
    assert_eq!(manifest["format"], "worktree.archive/1");
    let keys = manifest
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect::<Vec<_>>();
    assert_eq!(
        keys,
        [
            "branch",
            "bundle",
            "created_at",
            "format",
            "head",
            "id",
            "patch",
            "path",
            "repository_root",
            "unique_commits",
            "worktree_tree",
        ]
    );
    let mut files = fixture.archive_bytes().into_keys().collect::<Vec<_>>();
    files.sort();
    assert_eq!(files, ["commits.bundle", "dirty.patch", "manifest.json"]);
}

/// Gives a directory its owner write bit back when a case ends, so a failed case can be cleaned.
struct Unseal(PathBuf);

impl Drop for Unseal {
    fn drop(&mut self) {
        let _ = std::fs::set_permissions(&self.0, std::fs::Permissions::from_mode(0o755));
    }
}

/// Directories without the owner write bit inside a nested repository, beside it in an ignored
/// directory, and as the nested root's own parent: the tree retires, never stopping halfway.
#[test]
fn read_only_directories_in_and_around_a_nested_repository_retire_with_the_tree() {
    let fixture = Fixture::with_nested_repository();
    let inner = fixture.nested().join("sealed");
    let beside = fixture.tree.join("evidence/sealed");
    let parent = fixture.tree.join("evidence/run-1");
    for directory in [&inner, &beside] {
        std::fs::create_dir(directory).unwrap();
        std::fs::write(directory.join("kept.txt"), "kept\n").unwrap();
    }
    let mut guards = Vec::new();
    for directory in [inner, beside, parent] {
        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o555)).unwrap();
        guards.push(Unseal(directory));
    }
    fixture.ok(&["finish", "--archive", fixture.tree_str()]);
    assert_eq!(fixture.manifest()["format"], "worktree.archive/2");

    let applied = fixture.gc("--apply");
    assert!(applied["evidence"].is_object(), "{applied}");
    assert!(!fixture.tree.exists());
}

/// The discard restores tracked edits as well as deleting untracked files and nested images; a
/// directory without the owner write bit must not stop that restore either.
#[test]
fn a_tracked_edit_in_a_read_only_directory_is_restored_and_the_tree_retires() {
    let fixture = Fixture::new();
    let sealed = fixture.tree.join("sealed");
    std::fs::create_dir(&sealed).unwrap();
    std::fs::write(sealed.join("tracked.txt"), "committed\n").unwrap();
    git_ok(&fixture.tree, &["add", "sealed/tracked.txt"]);
    git_ok(&fixture.tree, &["commit", "--quiet", "-m", "sealed"]);
    std::fs::write(sealed.join("tracked.txt"), "edited\n").unwrap();
    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o555)).unwrap();
    let _unseal = Unseal(sealed);
    fixture.ok(&["finish", "--archive", fixture.tree_str()]);

    let applied = fixture.gc("--apply");
    assert!(applied["evidence"].is_object(), "{applied}");
    assert!(!fixture.tree.exists());
}
