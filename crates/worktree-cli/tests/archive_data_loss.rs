//! Adversarial cases for archive retirement: no byte in a tree may be lost by removing it.
//!
//! Each case builds a tree with local-only work, archives it, and runs `gc --apply`. A removal is
//! acceptable only when the archive holds what the tree held; a refusal is always acceptable.
#![cfg(unix)]

use serde_json::Value;
use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ID: &str = "archived";

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    remote: PathBuf,
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
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

impl Fixture {
    /// An active managed tree off a published `main`, with one local-only commit.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("demo");
        std::fs::create_dir_all(&repository).unwrap();
        git_ok(&repository, &["init", "-b", "main"]);
        std::fs::write(repository.join("README.md"), "demo\n").unwrap();
        std::fs::write(repository.join(".gitignore"), "build/\n").unwrap();
        git_ok(&repository, &["add", "."]);
        git_ok(&repository, &["commit", "-m", "init"]);
        let remote = root.path().join("remote.git");
        git_ok(
            root.path(),
            &[
                "clone",
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
            remote,
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
            "archive",
        ]);
        fixture.tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        fixture.commit("feature.txt", "local work\n", "local only");
        fixture
    }

    fn commit(&self, path: &str, contents: &str, message: &str) -> String {
        std::fs::write(self.tree.join(path), contents).unwrap();
        git_ok(&self.tree, &["add", path]);
        git_ok(&self.tree, &["commit", "-m", message]);
        git_ok(&self.tree, &["rev-parse", "HEAD"])
    }

    fn archive_dir(&self) -> PathBuf {
        std::fs::canonicalize(self.root.path())
            .unwrap()
            .join("state/worktree/archives/demo")
            .join(ID)
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

    /// Archive, finish and apply GC; return whether the tree is gone, with the GC assessment.
    fn archive_and_retire(&self) -> (bool, Value) {
        self.ok(&["archive", self.tree.to_str().unwrap()]);
        self.ok(&["finish", self.tree.to_str().unwrap()]);
        let report = self.ok(&[
            "gc",
            "--repo",
            self.repository.to_str().unwrap(),
            "--apply",
            "--id",
            ID,
        ]);
        let assessment = report["assessments"][0].clone();
        (!self.tree.exists(), assessment)
    }

    fn patch_text(&self) -> String {
        std::fs::read(self.archive_dir().join("dirty.patch"))
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default()
    }

    /// Restore exactly as README.md documents: fetch the bundle, switch, apply the patch.
    fn restore(&self) -> PathBuf {
        let clone = self.root.path().join("restore");
        git_ok(
            self.root.path(),
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                self.remote.to_str().unwrap(),
                clone.to_str().unwrap(),
            ],
        );
        let archive = self.archive_dir();
        // The README's raw restore: `.git/info/attributes` outranks every `.gitattributes`, so
        // no conversion applies and the archived bytes come back unconverted.
        std::fs::write(
            clone.join(".git/info/attributes"),
            "* -text -eol -filter -ident -working-tree-encoding\n",
        )
        .unwrap();
        git_ok(
            &clone,
            &[
                "fetch",
                "--quiet",
                archive.join("commits.bundle").to_str().unwrap(),
                "refs/worktree-archive/head:refs/heads/restored",
            ],
        );
        let raw = |args: &[&str]| {
            let mut all = vec![
                "-c",
                "core.autocrlf=false",
                "-c",
                "core.fileMode=true",
                "-c",
                "core.symlinks=true",
            ];
            all.extend_from_slice(args);
            git_ok(&clone, &all);
        };
        raw(&["switch", "--quiet", "restored"]);
        let patch = archive.join("dirty.patch");
        if patch.exists() {
            raw(&[
                "apply",
                "--binary",
                "--whitespace=nowarn",
                patch.to_str().unwrap(),
            ]);
        }
        clone
    }
}

/// Every regular file and symlink below `root` except Git's own `.git`, with its bytes (or link
/// target) and whether it is executable.
fn snapshot(root: &Path) -> BTreeMap<PathBuf, (String, bool, Vec<u8>)> {
    fn walk(root: &Path, dir: &Path, out: &mut BTreeMap<PathBuf, (String, bool, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if dir == root && entry.file_name() == ".git" {
                continue;
            }
            let metadata = std::fs::symlink_metadata(&path).unwrap();
            let relative = path.strip_prefix(root).unwrap().to_path_buf();
            if metadata.file_type().is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                out.insert(
                    relative,
                    (
                        "symlink".into(),
                        false,
                        target.into_os_string().into_encoded_bytes(),
                    ),
                );
            } else if metadata.is_dir() {
                walk(root, &path, out);
            } else {
                let executable = metadata.permissions().mode() & 0o100 != 0;
                out.insert(
                    relative,
                    ("file".into(), executable, std::fs::read(&path).unwrap()),
                );
            }
        }
    }
    let mut out = BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn staged_content_that_differs_from_the_worktree_is_archived_or_the_removal_is_refused() {
    let fixture = Fixture::new();
    std::fs::write(fixture.tree.join("README.md"), "demo\nstaged-only-line\n").unwrap();
    git_ok(&fixture.tree, &["add", "README.md"]);
    std::fs::write(fixture.tree.join("README.md"), "demo\nworking-copy-line\n").unwrap();
    let staged = git_ok(&fixture.tree, &["rev-parse", ":README.md"]);

    let (removed, assessment) = fixture.archive_and_retire();

    assert!(
        !removed
            || fixture.patch_text().contains("staged-only-line")
            || std::fs::read_to_string(fixture.archive_dir().join("manifest.json"))
                .unwrap()
                .contains(&staged),
        "the tree was removed although its index held blob {staged} ('staged-only-line'), which \
         neither HEAD, the working copy nor any archive file holds: {assessment}"
    );
}

#[test]
fn an_assume_unchanged_edit_is_archived_or_the_removal_is_refused() {
    let fixture = Fixture::new();
    git_ok(
        &fixture.tree,
        &["update-index", "--assume-unchanged", "feature.txt"],
    );
    std::fs::write(
        fixture.tree.join("feature.txt"),
        "local work\nhidden-edit-line\n",
    )
    .unwrap();

    let (removed, assessment) = fixture.archive_and_retire();

    assert!(
        !removed || fixture.patch_text().contains("hidden-edit-line"),
        "the tree was removed although feature.txt held an assume-unchanged edit that no \
         archive file holds: {assessment}"
    );
}

#[test]
fn a_skip_worktree_edit_is_archived_or_the_removal_is_refused() {
    let fixture = Fixture::new();
    git_ok(
        &fixture.tree,
        &["update-index", "--skip-worktree", "feature.txt"],
    );
    std::fs::write(
        fixture.tree.join("feature.txt"),
        "local work\nskipped-edit-line\n",
    )
    .unwrap();

    let (removed, assessment) = fixture.archive_and_retire();

    assert!(
        !removed || fixture.patch_text().contains("skipped-edit-line"),
        "the tree was removed although feature.txt held a skip-worktree edit that no archive \
         file holds: {assessment}"
    );
}

#[test]
fn a_file_below_a_non_repository_dot_git_directory_is_archived_or_the_removal_is_refused() {
    let fixture = Fixture::new();
    std::fs::write(fixture.tree.join("notes.txt"), "untracked notes\n").unwrap();
    std::fs::create_dir_all(fixture.tree.join("junk/.git")).unwrap();
    std::fs::write(
        fixture.tree.join("junk/.git/data.txt"),
        "under-dot-git-line\n",
    )
    .unwrap();

    let (removed, assessment) = fixture.archive_and_retire();

    assert!(
        !removed || fixture.patch_text().contains("under-dot-git-line"),
        "the tree was removed although junk/.git/data.txt held bytes that no archive file \
         holds: {assessment}"
    );
}

#[test]
fn a_commit_held_only_by_a_per_worktree_ref_is_archived_or_the_removal_is_refused() {
    let fixture = Fixture::new();
    let parked = fixture.commit("parked.txt", "parked\n", "parked on a per-worktree ref");
    git_ok(
        &fixture.tree,
        &["update-ref", "refs/worktree/parked", &parked],
    );
    git_ok(&fixture.tree, &["reset", "--quiet", "--hard", "HEAD~1"]);

    let (removed, assessment) = fixture.archive_and_retire();

    let still_held = git(&fixture.repository, &["cat-file", "-e", &parked])
        .status
        .success()
        && !git_ok(
            &fixture.repository,
            &["for-each-ref", "--contains", &parked, "--format=%(refname)"],
        )
        .is_empty();
    let bundled = fixture.archive_dir().join("commits.bundle").exists() && {
        let clone = fixture.restore();
        git(&clone, &["cat-file", "-e", &parked]).status.success()
    };
    assert!(
        !removed || still_held || bundled,
        "the tree was removed although commit {parked} was reachable only from its \
         refs/worktree/parked, and neither a ref nor the bundle holds it now: {assessment}"
    );
}

#[test]
fn the_documented_restore_reproduces_every_file_byte_and_executable_bit() {
    let fixture = Fixture::new();
    let tree = &fixture.tree;
    std::fs::remove_file(tree.join("feature.txt")).unwrap();
    std::fs::set_permissions(
        tree.join("README.md"),
        std::fs::Permissions::from_mode(0o755),
    )
    .unwrap();
    std::fs::write(tree.join("run.sh"), "#!/bin/sh\necho hi\n").unwrap();
    std::fs::set_permissions(tree.join("run.sh"), std::fs::Permissions::from_mode(0o755)).unwrap();
    std::fs::write(tree.join("empty"), "").unwrap();
    std::fs::write(tree.join("no-newline.txt"), "tail").unwrap();
    std::fs::write(tree.join("crlf.txt"), "one\r\ntwo\r\n").unwrap();
    std::fs::write(tree.join("name with space.txt"), "space\n").unwrap();
    std::fs::write(tree.join("ünïcödé.txt"), "unicode\n").unwrap();
    std::fs::write(tree.join("-leading-dash"), "dash\n").unwrap();
    std::fs::write(tree.join("binary.bin"), [0u8, 0, 255, 13, 10, 0, 7]).unwrap();
    std::os::unix::fs::symlink("README.md", tree.join("link")).unwrap();
    std::os::unix::fs::symlink("does/not/exist", tree.join("dangling")).unwrap();
    std::fs::create_dir_all(tree.join("build/deep")).unwrap();
    std::fs::write(tree.join("build/deep/out.o"), [1u8, 2, 3]).unwrap();
    let before = snapshot(tree);

    let (removed, assessment) = fixture.archive_and_retire();
    assert!(removed, "{assessment}");

    let restored = snapshot(&fixture.restore());
    let differing = before
        .keys()
        .chain(restored.keys())
        .filter(|path| before.get(*path) != restored.get(*path))
        .collect::<std::collections::BTreeSet<_>>();
    assert!(
        differing.is_empty(),
        "the documented restore differs from the removed tree at {differing:?}"
    );
}

#[test]
fn the_documented_restore_reproduces_raw_bytes_under_an_eol_attribute() {
    let fixture = Fixture::new();
    fixture.commit(".gitattributes", "*.txt text eol=crlf\n", "attributes");
    // Mixed line endings, as a tool that ignores attributes would write them.
    std::fs::write(fixture.tree.join("mixed.txt"), "lf-line\ncrlf-line\r\n").unwrap();
    let before = std::fs::read(fixture.tree.join("mixed.txt")).unwrap();

    let (removed, assessment) = fixture.archive_and_retire();
    assert!(removed, "{assessment}");

    let restored = std::fs::read(fixture.restore().join("mixed.txt")).unwrap();
    assert_eq!(
        String::from_utf8_lossy(&restored),
        String::from_utf8_lossy(&before),
        "the documented restore rewrote the archived bytes of mixed.txt"
    );
}

#[test]
fn a_bundle_whose_base_left_every_remote_is_refused_before_removal() {
    let fixture = Fixture::new();
    fixture.ok(&["archive", fixture.tree.to_str().unwrap()]);
    fixture.ok(&["finish", fixture.tree.to_str().unwrap()]);
    // The remote's only branch is rewritten to an unrelated root, so the bundle's
    // prerequisite is advertised by nobody.
    let orphan = fixture.root.path().join("orphan");
    git_ok(
        fixture.root.path(),
        &["init", "-q", "-b", "main", orphan.to_str().unwrap()],
    );
    std::fs::write(orphan.join("other.txt"), "other\n").unwrap();
    git_ok(&orphan, &["add", "."]);
    git_ok(&orphan, &["commit", "-q", "-m", "unrelated"]);
    git_ok(
        &orphan,
        &[
            "push",
            "--quiet",
            "--force",
            fixture.remote.to_str().unwrap(),
            "main:main",
        ],
    );

    let report = fixture.ok(&[
        "gc",
        "--repo",
        fixture.repository.to_str().unwrap(),
        "--apply",
        "--id",
        ID,
    ]);
    assert_eq!(
        report["assessments"][0]["refusal"]["code"], "archive-incomplete",
        "{report}"
    );
    assert!(fixture.tree.exists());
}
