//! Adversarial cases for nested repository images: the restore instruction, permission bits,
//! awkward entries, a tampered image, and a nested root moved behind a symlink.
#![cfg(unix)]

use serde_json::Value;
use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt as _;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ID: &str = "nested-adversary";
const FIXTURE: &str = "evidence/run-1/fixture";

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    tree: PathBuf,
    /// Directories whose owner write bit a case removed; restored on drop so cleanup can run.
    sealed: Vec<PathBuf>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for directory in &self.sealed {
            let _ = std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755));
        }
    }
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

/// Kind, permission bits and content (file bytes or link target; empty for a directory).
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
                out.insert(relative, ('f', mode, std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Listing::new();
    let mode = std::fs::symlink_metadata(dir).unwrap().permissions().mode() & 0o7777;
    out.insert(Vec::new(), ('d', mode, Vec::new()));
    walk(dir, dir, &mut out);
    out
}

/// The entries whose kind, mode or content differ between two listings, by path.
fn differences(left: &Listing, right: &Listing) -> Vec<String> {
    let mut paths = left.keys().chain(right.keys()).collect::<Vec<_>>();
    paths.sort();
    paths.dedup();
    paths
        .into_iter()
        .filter(|path| left.get(*path) != right.get(*path))
        .map(|path| {
            let shape = |side: Option<&(char, u32, Vec<u8>)>| {
                side.map_or("absent".to_owned(), |(kind, mode, _)| {
                    format!("{kind} {mode:o}")
                })
            };
            format!(
                "{}: {} -> {}",
                String::from_utf8_lossy(path),
                shape(left.get(path)),
                shape(right.get(path))
            )
        })
        .collect()
}

/// A nested repository with one commit, a stash entry, a staged change, an unstaged edit and an
/// untracked file.
fn nested_repository(path: &Path) {
    std::fs::create_dir_all(path).unwrap();
    git_ok(path, &["init", "--quiet"]);
    std::fs::write(path.join("a.txt"), "one\n").unwrap();
    std::fs::write(path.join("b.txt"), "two\n").unwrap();
    git_ok(path, &["add", "."]);
    git_ok(path, &["commit", "--quiet", "-m", "fixture"]);
    std::fs::write(path.join("a.txt"), "stashed\n").unwrap();
    git_ok(path, &["stash", "push", "--quiet", "-m", "parked"]);
    std::fs::write(path.join("b.txt"), "staged\n").unwrap();
    git_ok(path, &["add", "b.txt"]);
    std::fs::write(path.join("a.txt"), "unstaged\n").unwrap();
    std::fs::write(path.join("notes.txt"), "untracked\n").unwrap();
}

impl Fixture {
    /// An active managed tree whose committed `.gitignore` ignores `evidence/`, holding a nested
    /// repository at [`FIXTURE`] and an ignored file beside it.
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
            sealed: Vec::new(),
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

    /// Archive the tree with `finish --archive` and require an unblocked `/2` archive.
    fn finish_archive(&self) {
        let finished = self.ok(&["finish", "--archive", self.tree_str()]);
        assert!(finished["archive"]["blocker"].is_null(), "{finished}");
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(self.archive_dir().join("manifest.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(manifest["format"], "worktree.archive/2", "{manifest}");
    }

    /// Extract `nested-1.tar` with the system tar and the given mode flags into a new empty
    /// directory, and return the restored nested repository.
    fn restore(&self, flags: &str) -> PathBuf {
        let restored = self.root.path().join(format!("restored{flags}"));
        std::fs::create_dir(&restored).unwrap();
        let extracted = Command::new("tar")
            .arg(flags)
            .arg(self.archive_dir().join("nested-1.tar"))
            .arg("-C")
            .arg(&restored)
            .output()
            .unwrap();
        assert!(
            extracted.status.success(),
            "{}",
            String::from_utf8_lossy(&extracted.stderr)
        );
        restored.join(FIXTURE)
    }
}

fn refusal(assessment: &Value) -> &str {
    assessment["refusal"]["code"].as_str().unwrap_or("<none>")
}

/// The domain documents the image's restore as `tar -xpf <image> -C <tree>` and says the image
/// keeps permission bits. Without `-p`, an unprivileged tar applies the caller's umask, so a
/// group- or world-writable entry would come back narrower than the image holds it.
#[test]
fn the_documented_tar_xpf_restore_reproduces_every_permission_bit_the_image_holds() {
    let fixture = Fixture::new();
    let nested = fixture.nested();
    std::fs::write(nested.join("shared.txt"), "group writable\n").unwrap();
    std::fs::set_permissions(
        nested.join("shared.txt"),
        std::fs::Permissions::from_mode(0o664),
    )
    .unwrap();
    std::fs::write(nested.join("open.txt"), "world writable\n").unwrap();
    std::fs::set_permissions(
        nested.join("open.txt"),
        std::fs::Permissions::from_mode(0o666),
    )
    .unwrap();
    std::fs::create_dir(nested.join("team")).unwrap();
    std::fs::set_permissions(nested.join("team"), std::fs::Permissions::from_mode(0o775)).unwrap();
    let original = listing(&nested);
    fixture.finish_archive();

    let restored = listing(&fixture.restore("-xpf"));
    assert!(
        restored == original,
        "`tar -xpf <image> -C <dir>` did not restore the permission bits the image holds: {:?}",
        differences(&original, &restored)
    );
}

/// Issue #16 made GC remove a tree holding a directory without the owner write bit. Discarding
/// an imaged nested repository deletes entry by entry before Git's removal repairs any mode, so
/// such a directory below a nested root stops the discard after it has already deleted part of
/// the tree.
#[test]
fn a_read_only_directory_in_a_nested_repository_is_retired_or_refused_before_any_deletion() {
    let mut fixture = Fixture::new();
    let sealed = fixture.nested().join("sealed");
    std::fs::create_dir(&sealed).unwrap();
    std::fs::write(sealed.join("kept.txt"), "kept\n").unwrap();
    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o555)).unwrap();
    fixture.sealed.push(sealed.clone());
    fixture.finish_archive();
    assert_eq!(fixture.gc("--dry-run")["eligible"], true);
    let before = listing(&fixture.tree);

    let applied = fixture.gc("--apply");
    if applied["evidence"].is_object() {
        assert!(!fixture.tree.exists(), "{applied}");
        return;
    }
    let after = listing(&fixture.tree);
    assert!(
        after == before,
        "gc refused with {} after deleting part of the tree: {:?}",
        refusal(&applied),
        differences(&before, &after)
    );
}

/// The same mechanism outside any nested repository: an archived tree whose ignored files sit in
/// a directory without the owner write bit. Not a nested-image path; it measures whether the fix
/// for the case above must also cover the discard of ordinary ignored files.
#[test]
fn a_read_only_ignored_directory_in_an_archived_tree_is_retired_or_refused_before_any_deletion() {
    let mut fixture = Fixture::new();
    std::fs::remove_dir_all(fixture.nested()).unwrap();
    let sealed = fixture.tree.join("evidence/sealed");
    std::fs::create_dir(&sealed).unwrap();
    std::fs::write(sealed.join("kept.txt"), "kept\n").unwrap();
    std::fs::set_permissions(&sealed, std::fs::Permissions::from_mode(0o555)).unwrap();
    fixture.sealed.push(sealed.clone());
    let finished = fixture.ok(&["finish", "--archive", fixture.tree_str()]);
    assert!(finished["archive"]["blocker"].is_null(), "{finished}");
    assert_eq!(fixture.gc("--dry-run")["eligible"], true);
    let before = listing(&fixture.tree);

    let applied = fixture.gc("--apply");
    if applied["evidence"].is_object() {
        assert!(!fixture.tree.exists(), "{applied}");
        return;
    }
    let after = listing(&fixture.tree);
    assert!(
        after == before,
        "gc refused with {} after deleting part of the tree: {:?}",
        refusal(&applied),
        differences(&before, &after)
    );
}

/// Long names and link targets, a name that is not UTF-8, an empty directory, hard links and a
/// repository nested in the nested repository all come back from `tar -xpf`, and GC retires the
/// tree.
#[test]
fn awkward_entries_restore_with_tar_xpf_and_the_tree_retires() {
    let fixture = Fixture::new();
    let nested = fixture.nested();
    let long = "l".repeat(140);
    std::fs::write(nested.join(&long), "long name\n").unwrap();
    std::os::unix::fs::symlink("t/".repeat(80), nested.join("long-target")).unwrap();
    std::fs::write(
        nested.join(OsStr::from_bytes(b"latin-\xe9\tand tab")),
        "odd\n",
    )
    .unwrap();
    std::fs::create_dir(nested.join("empty")).unwrap();
    std::fs::write(nested.join("linked-a"), "same inode\n").unwrap();
    std::fs::hard_link(nested.join("linked-a"), nested.join("linked-b")).unwrap();
    let inner = nested.join("vendor/inner");
    std::fs::create_dir_all(&inner).unwrap();
    git_ok(&inner, &["init", "--quiet"]);
    std::fs::write(inner.join("x.txt"), "inner\n").unwrap();
    git_ok(&inner, &["add", "."]);
    git_ok(&inner, &["commit", "--quiet", "-m", "inner"]);
    let original = listing(&nested);
    fixture.finish_archive();

    let restored = listing(&fixture.restore("-xpf"));
    assert!(
        restored == original,
        "{:?}",
        differences(&original, &restored)
    );
    let applied = fixture.gc("--apply");
    assert!(applied["evidence"].is_object(), "{applied}");
    assert!(!fixture.tree.exists());
}

/// One byte changed in the image after archiving refuses review and removal before the tree
/// loses anything.
#[test]
fn a_tampered_image_refuses_removal_before_anything_is_deleted() {
    let fixture = Fixture::new();
    fixture.finish_archive();
    let image = fixture.archive_dir().join("nested-1.tar");
    let mut bytes = std::fs::read(&image).unwrap();
    let middle = bytes.len() / 2;
    bytes[middle] ^= 0x01;
    std::fs::write(&image, &bytes).unwrap();
    let before = listing(&fixture.tree);

    let review = fixture.gc("--dry-run");
    assert_eq!(review["eligible"], false, "{review}");
    let applied = fixture.gc("--apply");
    assert_eq!(refusal(&applied), "archive-digest-mismatch", "{applied}");
    assert_eq!(
        differences(&before, &listing(&fixture.tree)),
        Vec::<String>::new()
    );
}

/// A `.git` Git does not take for a repository, created beside an imaged root after archiving,
/// changes no fingerprint; hidden state still refuses it. The image covers its own root only,
/// not that root's ancestors such as `evidence/`.
#[test]
fn an_uncovered_dot_git_beside_an_imaged_root_still_refuses_removal() {
    let fixture = Fixture::new();
    fixture.finish_archive();
    let stray = fixture.tree.join("evidence/other/.git");
    std::fs::create_dir_all(&stray).unwrap();
    std::fs::write(stray.join("only-here.txt"), "not a repository\n").unwrap();
    let before = listing(&fixture.tree);

    let applied = fixture.gc("--apply");
    assert_eq!(refusal(&applied), "worktree-hidden-state", "{applied}");
    assert_eq!(
        differences(&before, &listing(&fixture.tree)),
        Vec::<String>::new()
    );
}

/// A nested root, or its parent, moved out of the tree and replaced by a symlink to the moved
/// copy, is no longer the imaged repository: removal refuses and deletes nothing on either side.
#[test]
fn a_nested_root_moved_behind_a_symlink_refuses_removal_and_touches_neither_side() {
    for moved in [FIXTURE, "evidence/run-1"] {
        let fixture = Fixture::new();
        fixture.finish_archive();
        let source = fixture.tree.join(moved);
        let outside = fixture.root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let target = outside.join("moved");
        std::fs::rename(&source, &target).unwrap();
        std::os::unix::fs::symlink(&target, &source).unwrap();
        let tree_before = listing(&fixture.tree);
        let outside_before = listing(&outside);

        let applied = fixture.gc("--apply");
        assert!(applied["evidence"].is_null(), "{moved}: {applied}");
        assert_eq!(
            differences(&tree_before, &listing(&fixture.tree)),
            Vec::<String>::new(),
            "{moved}: {}",
            refusal(&applied)
        );
        assert_eq!(
            differences(&outside_before, &listing(&outside)),
            Vec::<String>::new(),
            "{moved}: {}",
            refusal(&applied)
        );
    }
}
