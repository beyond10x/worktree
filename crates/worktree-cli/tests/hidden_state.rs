//! State Git status does not report must retain a tree, with or without an archive.
#![cfg(unix)]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ID: &str = "hidden";

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
    /// A managed tree whose HEAD is published, so ordinary remote proof covers its commits.
    fn published() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("demo");
        std::fs::create_dir_all(&repository).unwrap();
        git_ok(&repository, &["init", "-b", "main"]);
        std::fs::write(repository.join("README.md"), "demo\n").unwrap();
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
            "hidden",
        ]);
        fixture.tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        fixture
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = Command::new(env!("CARGO_BIN_EXE_worktree"))
            .arg("--json")
            .args(args)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "worktree {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// Finish, then apply GC, returning the assessment.
    fn finish_and_apply(&self) -> Value {
        self.ok(&["finish", self.tree.to_str().unwrap()]);
        let report = self.ok(&[
            "gc",
            "--repo",
            self.repository.to_str().unwrap(),
            "--apply",
            "--id",
            ID,
        ]);
        report["assessments"][0].clone()
    }
}

fn refusal(assessment: &Value) -> &str {
    assessment["refusal"]["code"].as_str().unwrap_or("<none>")
}

#[test]
fn an_assume_unchanged_edit_retains_a_published_tree() {
    let fixture = Fixture::published();
    git_ok(
        &fixture.tree,
        &["update-index", "--assume-unchanged", "README.md"],
    );
    std::fs::write(fixture.tree.join("README.md"), "demo\nhidden edit\n").unwrap();

    let assessment = fixture.finish_and_apply();
    assert_eq!(
        refusal(&assessment),
        "worktree-hidden-state",
        "{assessment}"
    );
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("README.md")).unwrap(),
        "demo\nhidden edit\n"
    );
}

#[test]
fn a_skip_worktree_edit_retains_a_published_tree() {
    let fixture = Fixture::published();
    git_ok(
        &fixture.tree,
        &["update-index", "--skip-worktree", "README.md"],
    );
    std::fs::write(fixture.tree.join("README.md"), "demo\nskipped edit\n").unwrap();

    let assessment = fixture.finish_and_apply();
    assert_eq!(
        refusal(&assessment),
        "worktree-hidden-state",
        "{assessment}"
    );
    assert!(fixture.tree.join("README.md").exists());
}

#[test]
fn files_below_a_nested_dot_git_retain_a_published_tree() {
    let fixture = Fixture::published();
    std::fs::create_dir_all(fixture.tree.join("junk/.git")).unwrap();
    std::fs::write(fixture.tree.join("junk/.git/data.txt"), "only copy\n").unwrap();

    let assessment = fixture.finish_and_apply();
    assert_eq!(
        refusal(&assessment),
        "worktree-hidden-state",
        "{assessment}"
    );
    assert!(fixture.tree.join("junk/.git/data.txt").exists());
}

#[test]
fn a_plain_dot_git_file_in_a_subdirectory_retains_a_published_tree() {
    let fixture = Fixture::published();
    std::fs::create_dir_all(fixture.tree.join("sub")).unwrap();
    std::fs::write(fixture.tree.join("sub/.git"), "not a gitfile\n").unwrap();

    let assessment = fixture.finish_and_apply();
    assert_eq!(
        refusal(&assessment),
        "worktree-hidden-state",
        "{assessment}"
    );
    assert!(fixture.tree.join("sub/.git").exists());
}

#[test]
fn a_commit_held_by_a_per_worktree_ref_retains_a_published_tree() {
    let fixture = Fixture::published();
    std::fs::write(fixture.tree.join("parked.txt"), "parked\n").unwrap();
    git_ok(&fixture.tree, &["add", "parked.txt"]);
    git_ok(&fixture.tree, &["commit", "-m", "parked"]);
    let parked = git_ok(&fixture.tree, &["rev-parse", "HEAD"]);
    git_ok(
        &fixture.tree,
        &["update-ref", "refs/worktree/parked", &parked],
    );
    git_ok(&fixture.tree, &["reset", "--quiet", "--hard", "HEAD~1"]);

    let assessment = fixture.finish_and_apply();
    assert_eq!(refusal(&assessment), "worktree-local-refs", "{assessment}");
    assert!(fixture.tree.exists());
}

#[test]
fn staged_content_only_the_index_holds_retains_an_archived_tree() {
    let fixture = Fixture::published();
    std::fs::write(fixture.tree.join("README.md"), "demo\nstaged\n").unwrap();
    git_ok(&fixture.tree, &["add", "README.md"]);
    std::fs::write(fixture.tree.join("README.md"), "demo\nworking\n").unwrap();
    let archived = fixture.ok(&["archive", fixture.tree.to_str().unwrap()]);
    assert_eq!(
        archived["archive"]["blocker"]["code"], "worktree-hidden-state",
        "{archived}"
    );

    let assessment = fixture.finish_and_apply();
    assert_eq!(
        refusal(&assessment),
        "worktree-hidden-state",
        "{assessment}"
    );
    assert_eq!(
        git_ok(&fixture.tree, &["show", ":README.md"]),
        "demo\nstaged"
    );
}

/// The README's restore, with attributes and end-of-line conversion disabled.
///
/// `--attr-source` is deliberately absent: Git 2.55 `apply` segfaults with it on any patch that
/// modifies an existing file. `.git/info/attributes`, written before this runs, outranks every
/// `.gitattributes` instead.
fn raw_git(path: &Path, args: &[&str]) -> String {
    let mut full = vec![
        "-c",
        "core.autocrlf=false",
        "-c",
        "core.fileMode=true",
        "-c",
        "core.symlinks=true",
    ];
    full.extend(args);
    git_ok(path, &full)
}

#[test]
fn the_raw_restore_reproduces_bytes_that_attributes_would_rewrite() {
    let fixture = Fixture::published();
    std::fs::write(fixture.tree.join("notes.txt"), "lf\n").unwrap();
    std::fs::write(fixture.tree.join("plain.txt"), "untouched lf\n").unwrap();
    std::fs::write(fixture.tree.join(".gitattributes"), "*.txt text eol=crlf\n").unwrap();
    git_ok(&fixture.tree, &["add", "."]);
    git_ok(
        &fixture.tree,
        &["commit", "-m", "attributes and a text file"],
    );
    std::fs::write(fixture.tree.join("mixed.txt"), "lf-line\ncrlf-line\r\n").unwrap();
    // A tracked file whose content and mode both change: the patch shape on which
    // `apply --attr-source` segfaults.
    std::fs::write(fixture.tree.join("notes.txt"), "edited\r\nlone lf\n").unwrap();
    std::fs::set_permissions(
        fixture.tree.join("notes.txt"),
        std::os::unix::fs::PermissionsExt::from_mode(0o755),
    )
    .unwrap();
    let tracked = std::fs::read(fixture.tree.join("notes.txt")).unwrap();
    let plain = std::fs::read(fixture.tree.join("plain.txt")).unwrap();
    let mixed = std::fs::read(fixture.tree.join("mixed.txt")).unwrap();

    fixture.ok(&["archive", fixture.tree.to_str().unwrap()]);
    let assessment = fixture.finish_and_apply();
    assert!(assessment["evidence"].is_object(), "{assessment}");
    assert!(!fixture.tree.exists());

    let archive = std::fs::canonicalize(fixture.root.path())
        .unwrap()
        .join("state/worktree/archives/demo")
        .join(ID);
    let clone = fixture.root.path().join("restore");
    git_ok(
        fixture.root.path(),
        &[
            "clone",
            "--quiet",
            "--no-checkout",
            fixture.remote.to_str().unwrap(),
            clone.to_str().unwrap(),
        ],
    );
    std::fs::write(
        clone.join(".git/info/attributes"),
        "* -text -eol -filter -ident -working-tree-encoding\n",
    )
    .unwrap();
    let bundle = archive.join("commits.bundle");
    git_ok(
        &clone,
        &[
            "fetch",
            "--quiet",
            bundle.to_str().unwrap(),
            "refs/worktree-archive/head:refs/heads/restored",
        ],
    );
    raw_git(&clone, &["switch", "--quiet", "restored"]);
    raw_git(
        &clone,
        &[
            "apply",
            "--binary",
            "--whitespace=nowarn",
            archive.join("dirty.patch").to_str().unwrap(),
        ],
    );
    assert_eq!(std::fs::read(clone.join("notes.txt")).unwrap(), tracked);
    assert_eq!(std::fs::read(clone.join("plain.txt")).unwrap(), plain);
    assert_eq!(std::fs::read(clone.join("mixed.txt")).unwrap(), mixed);
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(clone.join("notes.txt"))
            .unwrap()
            .permissions(),
    );
    assert_ne!(mode & 0o100, 0, "the restore lost the executable bit");
}
