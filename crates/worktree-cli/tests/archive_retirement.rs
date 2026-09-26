//! A tree whose work is on no remote ref is retired against a verified local archive.
#![cfg(unix)]

use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const ID: &str = "archived";
const ARCHIVE_PATCH: &str = "dirty.patch";

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
        self.head()
    }

    fn head(&self) -> String {
        git_ok(&self.tree, &["rev-parse", "HEAD"])
    }

    /// Tracked edit, untracked file and ignored build output.
    fn make_dirty(&self) {
        std::fs::write(self.tree.join("README.md"), "demo\nedited\n").unwrap();
        std::fs::write(self.tree.join("notes.txt"), "untracked notes\n").unwrap();
        std::fs::create_dir_all(self.tree.join("build")).unwrap();
        std::fs::write(self.tree.join("build/output.bin"), [0u8, 1, 2, 255]).unwrap();
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

    fn refused(&self, args: &[&str]) -> Value {
        let output = self.command(args);
        assert!(
            !output.status.success(),
            "worktree {args:?} unexpectedly succeeded: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stderr).unwrap()
    }

    fn archive(&self) -> Value {
        self.ok(&["archive", self.tree.to_str().unwrap()])
    }

    fn finish(&self) -> Value {
        self.ok(&["finish", self.tree.to_str().unwrap()])
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
        assert_eq!(report["version"], 4, "{report}");
        let assessments = report["assessments"].as_array().unwrap();
        assert_eq!(assessments.len(), 1, "{report}");
        assessments[0].clone()
    }

    fn manifest(&self) -> Value {
        serde_json::from_slice(&std::fs::read(self.archive_dir().join("manifest.json")).unwrap())
            .unwrap()
    }

    /// The README's restore into a fresh clone of the remote, which has never seen the tree's
    /// local commits: attributes off through `.git/info/attributes`, fetch, switch, apply.
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
        std::fs::write(
            clone.join(".git/info/attributes"),
            "* -text -eol -filter -ident -working-tree-encoding\n",
        )
        .unwrap();
        let archive = self.archive_dir();
        git_ok(
            &clone,
            &[
                "fetch",
                "--quiet",
                archive.join("commits.bundle").to_str().unwrap(),
                "refs/worktree-archive/head:refs/heads/restored",
            ],
        );
        let raw = [
            "-c",
            "core.autocrlf=false",
            "-c",
            "core.fileMode=true",
            "-c",
            "core.symlinks=true",
        ];
        git_ok(
            &clone,
            &[&raw[..], &["switch", "--quiet", "restored"]].concat(),
        );
        let patch = archive.join(ARCHIVE_PATCH);
        if patch.exists() {
            git_ok(
                &clone,
                &[
                    &raw[..],
                    &[
                        "apply",
                        "--binary",
                        "--whitespace=nowarn",
                        patch.to_str().unwrap(),
                    ],
                ]
                .concat(),
            );
        }
        clone
    }
}

fn refusal_code(assessment: &Value) -> &str {
    assessment["refusal"]["code"].as_str().unwrap_or("<none>")
}

#[test]
fn a_local_only_tree_without_an_archive_is_still_refused() {
    let fixture = Fixture::new();
    fixture.finish();

    let dry = fixture.gc("--dry-run");
    assert_eq!(refusal_code(&dry), "no-remote-recovery-proof", "{dry}");
    assert!(dry.get("archive").is_none_or(Value::is_null), "{dry}");
    let applied = fixture.gc("--apply");
    assert_eq!(refusal_code(&applied), "no-remote-recovery-proof");
    assert!(fixture.tree.exists());
}

#[test]
fn archive_writes_a_verified_manifest_and_leaves_the_tree_untouched() {
    let fixture = Fixture::new();
    let base = git_ok(&fixture.repository, &["rev-parse", "main"]);
    let second = fixture.commit("second.txt", "second\n", "second local");
    fixture.make_dirty();
    let status_before = git_ok(
        &fixture.tree,
        &[
            "status",
            "--porcelain",
            "--untracked-files=all",
            "--ignored",
        ],
    );
    let admin = PathBuf::from(git_ok(
        &fixture.tree,
        &["rev-parse", "--path-format=absolute", "--git-dir"],
    ));
    let index_before = std::fs::read(admin.join("index")).unwrap();

    let report = fixture.archive();

    assert_eq!(report["version"], 4, "{report}");
    assert_eq!(
        report["archive"]["path"],
        fixture.archive_dir().display().to_string()
    );
    let manifest = fixture.manifest();
    assert_eq!(manifest, report["archive"]["manifest"]);
    assert_eq!(manifest["format"], "worktree.archive/1");
    assert_eq!(manifest["id"], ID);
    assert_eq!(manifest["head"], second);
    let unique = manifest["unique_commits"].as_array().unwrap();
    assert_eq!(unique.len(), 2, "{manifest}");
    assert!(
        unique
            .iter()
            .all(|commit| commit != &Value::from(base.clone()))
    );
    assert_eq!(manifest["bundle"]["file"], "commits.bundle");
    assert_eq!(manifest["patch"]["file"], "dirty.patch");
    for file in ["commits.bundle", "dirty.patch"] {
        assert!(fixture.archive_dir().join(file).is_file(), "{file}");
    }
    let verified = git(
        &fixture.repository,
        &[
            "bundle",
            "verify",
            fixture
                .archive_dir()
                .join("commits.bundle")
                .to_str()
                .unwrap(),
        ],
    );
    assert!(verified.status.success());

    assert_eq!(fixture.head(), second);
    assert_eq!(
        git_ok(
            &fixture.tree,
            &[
                "status",
                "--porcelain",
                "--untracked-files=all",
                "--ignored"
            ],
        ),
        status_before
    );
    assert_eq!(std::fs::read(admin.join("index")).unwrap(), index_before);
}

#[test]
fn an_archived_local_commit_is_retired_and_restorable_from_the_bundle() {
    let fixture = Fixture::new();
    let head = fixture.head();
    fixture.finish();
    fixture.archive();

    let dry = fixture.gc("--dry-run");
    assert_eq!(dry["eligible"], true, "{dry}");
    assert_eq!(dry["archive"], fixture.archive_dir().display().to_string());

    let applied = fixture.gc("--apply");
    assert_eq!(
        applied["evidence"]["recovery"]["kind"], "archive",
        "{applied}"
    );
    assert_eq!(
        applied["evidence"]["recovery"]["archive"]["path"],
        fixture.archive_dir().display().to_string()
    );
    assert!(!fixture.tree.exists());
    assert!(fixture.archive_dir().join("manifest.json").is_file());

    let clone = fixture.restore();
    assert_eq!(git_ok(&clone, &["rev-parse", "restored"]), head);
}

#[test]
fn an_archived_dirty_tree_is_finished_retired_and_restorable() {
    let fixture = Fixture::new();
    let head = fixture.head();
    fixture.make_dirty();
    assert_eq!(
        fixture.refused(&["finish", fixture.tree.to_str().unwrap()])["code"],
        "worktree-dirty"
    );

    fixture.archive();
    fixture.finish();
    assert_eq!(fixture.gc("--dry-run")["eligible"], true);
    let applied = fixture.gc("--apply");
    assert!(applied["evidence"].is_object(), "{applied}");
    assert!(!fixture.tree.exists());

    let clone = fixture.restore();
    assert_eq!(git_ok(&clone, &["rev-parse", "HEAD"]), head);
    assert_eq!(
        std::fs::read_to_string(clone.join("README.md")).unwrap(),
        "demo\nedited\n"
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("notes.txt")).unwrap(),
        "untracked notes\n"
    );
    assert_eq!(
        std::fs::read(clone.join("build/output.bin")).unwrap(),
        [0u8, 1, 2, 255]
    );
}

#[test]
fn an_edit_after_the_archive_is_refused_as_stale() {
    let fixture = Fixture::new();
    fixture.make_dirty();
    fixture.archive();
    fixture.finish();
    std::fs::write(fixture.tree.join("notes.txt"), "changed after archive\n").unwrap();

    assert_eq!(refusal_code(&fixture.gc("--dry-run")), "archive-stale");
    let applied = fixture.gc("--apply");
    assert_eq!(refusal_code(&applied), "archive-stale", "{applied}");
    assert_eq!(
        std::fs::read_to_string(fixture.tree.join("notes.txt")).unwrap(),
        "changed after archive\n"
    );
}

#[test]
fn a_new_file_after_the_archive_is_refused_as_stale() {
    let fixture = Fixture::new();
    fixture.make_dirty();
    fixture.archive();
    fixture.finish();
    std::fs::write(fixture.tree.join("build/late.log"), "late\n").unwrap();

    assert_eq!(refusal_code(&fixture.gc("--apply")), "archive-stale");
    assert!(fixture.tree.join("build/late.log").exists());
}

#[test]
fn an_archive_of_a_clean_tree_does_not_cover_later_dirt() {
    let fixture = Fixture::new();
    fixture.archive();
    fixture.finish();
    std::fs::write(fixture.tree.join("notes.txt"), "after\n").unwrap();

    assert_eq!(refusal_code(&fixture.gc("--dry-run")), "archive-stale");
}

#[test]
fn a_commit_after_the_archive_is_refused_as_stale() {
    let fixture = Fixture::new();
    fixture.archive();
    fixture.commit("later.txt", "later\n", "after the archive");
    fixture.finish();

    let dry = fixture.gc("--dry-run");
    assert_eq!(refusal_code(&dry), "archive-stale", "{dry}");
    assert!(fixture.tree.exists());
}

#[test]
fn a_changed_bundle_digest_is_refused() {
    let fixture = Fixture::new();
    fixture.archive();
    fixture.finish();
    let bundle = fixture.archive_dir().join("commits.bundle");
    let mut bytes = std::fs::read(&bundle).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 0xff;
    std::fs::write(&bundle, bytes).unwrap();

    assert_eq!(
        refusal_code(&fixture.gc("--apply")),
        "archive-digest-mismatch"
    );
    assert!(fixture.tree.exists());
}

#[test]
fn a_missing_bundle_is_refused_as_incomplete() {
    let fixture = Fixture::new();
    fixture.archive();
    fixture.finish();
    std::fs::remove_file(fixture.archive_dir().join("commits.bundle")).unwrap();

    assert_eq!(refusal_code(&fixture.gc("--dry-run")), "archive-incomplete");
}

#[test]
fn a_bundle_missing_a_unique_commit_is_refused_even_with_a_matching_digest() {
    let fixture = Fixture::new();
    let head = fixture.commit("second.txt", "second\n", "second local");
    fixture.archive();
    fixture.finish();

    // Replace the bundle with one that names HEAD but omits its parent, and make the manifest
    // agree with the replacement so that only the content check can notice.
    let archive = fixture.archive_dir();
    let bundle = archive.join("commits.bundle");
    git_ok(
        &fixture.repository,
        &["update-ref", "refs/worktree-archive/head", &head],
    );
    std::fs::remove_file(&bundle).unwrap();
    git_ok(
        &fixture.repository,
        &[
            "bundle",
            "create",
            bundle.to_str().unwrap(),
            "refs/worktree-archive/head",
            "--not",
            &format!("{head}~1"),
        ],
    );
    git_ok(
        &fixture.repository,
        &["update-ref", "-d", "refs/worktree-archive/head"],
    );
    let bytes = std::fs::read(&bundle).unwrap();
    let mut manifest = fixture.manifest();
    manifest["bundle"]["sha256"] = Value::from(sha256_hex(&bytes));
    manifest["bundle"]["bytes"] = Value::from(bytes.len());
    std::fs::write(
        archive.join("manifest.json"),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();

    let dry = fixture.gc("--dry-run");
    assert_eq!(refusal_code(&dry), "archive-incomplete", "{dry}");
    assert_eq!(refusal_code(&fixture.gc("--apply")), "archive-incomplete");
    assert!(fixture.tree.exists());
}

#[test]
fn a_live_lease_still_refuses_an_archived_tree() {
    let fixture = Fixture::new();
    fixture.archive();
    fixture.ok(&[
        "hook",
        "session-start",
        "--path",
        fixture.tree.to_str().unwrap(),
        "--session",
        "owner",
    ]);

    assert_eq!(
        fixture.refused(&["finish", fixture.tree.to_str().unwrap()])["code"],
        "live-session"
    );
}

#[test]
fn an_existing_archive_is_replaced_only_on_request_and_never_deleted() {
    let fixture = Fixture::new();
    fixture.archive();
    let later = fixture.commit("later.txt", "later\n", "after the archive");

    assert_eq!(
        fixture.refused(&["archive", fixture.tree.to_str().unwrap()])["code"],
        "archive-exists"
    );
    fixture.ok(&["archive", "--replace", fixture.tree.to_str().unwrap()]);
    assert_eq!(fixture.manifest()["head"], later);
    let superseded = std::fs::read_dir(fixture.archive_dir().parent().unwrap())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(&format!("{ID}.superseded-"))
        })
        .count();
    assert_eq!(superseded, 1);
    fixture.finish();
    assert_eq!(fixture.gc("--dry-run")["eligible"], true);
}

/// SHA-256 through the system tool, so the test does not share the implementation under test.
fn sha256_hex(bytes: &[u8]) -> String {
    use std::io::Write as _;
    let mut child = Command::new("sha256sum")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(bytes).unwrap();
    let output = child.wait_with_output().unwrap();
    String::from_utf8(output.stdout)
        .unwrap()
        .split_whitespace()
        .next()
        .unwrap()
        .to_owned()
}
