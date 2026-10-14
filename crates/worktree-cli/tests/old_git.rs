//! `archive` works on a Git that reads revisions from `--stdin` the way Git before 2.42 does.
//!
//! Git before 2.42 dies with "options not supported in --stdin mode" when a line it reads as a
//! revision under `--stdin` starts with `-`, `--not` included. This test runs without the libtest
//! harness so that the test executable can also stand in for `git`: invoked through a link named
//! `git`, it refuses such input exactly like that Git, and passes every other call to the real one.
#![cfg(unix)]

use serde_json::Value;
use std::ffi::OsString;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Output, Stdio};

/// Names the real Git the double hands every call it does not refuse to.
const REAL_GIT: &str = "WORKTREE_TEST_REAL_GIT";
const ID: &str = "old-git";

fn main() -> ExitCode {
    let mut args = std::env::args_os();
    let invoked = args.next().unwrap_or_default();
    if Path::new(&invoked).file_name() == Some("git".as_ref()) {
        return old_git(&args.collect::<Vec<_>>());
    }
    double_refuses_not_on_stdin();
    archive_bundles_on_old_git();
    println!("old_git: 2 passed");
    ExitCode::SUCCESS
}

/// The subcommand after Git's global options, and whether it reads revisions from stdin.
fn reads_revisions_from_stdin(args: &[OsString]) -> bool {
    let mut index = 0;
    while let Some(arg) = args.get(index).and_then(|arg| arg.to_str()) {
        match arg {
            "-C" | "-c" | "--git-dir" | "--work-tree" => index += 2,
            _ if arg.starts_with('-') => index += 1,
            _ => break,
        }
    }
    let subcommand = args.get(index).and_then(|arg| arg.to_str());
    matches!(subcommand, Some("bundle" | "rev-list"))
        && args[index..].iter().any(|arg| arg == "--stdin")
}

/// Git before 2.42: a stdin revision line starting with `-` other than `--` is fatal.
fn old_git(args: &[OsString]) -> ExitCode {
    let real = std::env::var_os(REAL_GIT).expect("the real git is named");
    if !reads_revisions_from_stdin(args) {
        let status = Command::new(real).args(args).status().unwrap();
        return exit_code(status);
    }
    let mut input = Vec::new();
    std::io::stdin().read_to_end(&mut input).unwrap();
    for line in input.split(|byte| *byte == b'\n') {
        if line.is_empty() || line == b"--" {
            break;
        }
        if line.starts_with(b"-") {
            eprintln!("fatal: options not supported in --stdin mode");
            return ExitCode::from(128);
        }
    }
    let mut child = Command::new(real)
        .args(args)
        .stdin(Stdio::piped())
        .spawn()
        .unwrap();
    child.stdin.take().unwrap().write_all(&input).unwrap();
    let status = child.wait().unwrap();
    exit_code(status)
}

fn real_git() -> PathBuf {
    let path = std::env::var_os("PATH").unwrap();
    std::env::split_paths(&path)
        .map(|directory| directory.join("git"))
        .find(|candidate| candidate.is_file())
        .expect("git is on PATH")
}

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    tree: PathBuf,
    double: PathBuf,
}

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
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

impl Fixture {
    /// A managed tree off a published `main` with one local-only commit, and a directory whose
    /// only entry is `git`, a link to this executable.
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let double = root.path().join("old-git");
        std::fs::create_dir(&double).unwrap();
        std::os::unix::fs::symlink(std::env::current_exe().unwrap(), double.join("git")).unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("demo");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        std::fs::write(repository.join("README.md"), "demo\n").unwrap();
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-m", "init"]);
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
        let profile = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles/default.toml");
        let mut fixture = Self {
            root,
            repository,
            tree: PathBuf::new(),
            double,
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
        std::fs::write(fixture.tree.join("feature.txt"), "local work\n").unwrap();
        git(&fixture.tree, &["add", "feature.txt"]);
        git(&fixture.tree, &["commit", "-m", "local only"]);
        fixture
    }

    /// PATH with the old-Git double in front of every other directory.
    fn path(&self) -> OsString {
        let path = std::env::var_os("PATH").unwrap();
        let directories = std::iter::once(self.double.clone()).chain(std::env::split_paths(&path));
        std::env::join_paths(directories).unwrap()
    }

    fn command(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_worktree"))
            .arg("--json")
            .args(args)
            .env("PATH", self.path())
            .env(REAL_GIT, real_git())
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
}

/// The double is red-capable: it refuses `--not` on stdin as Git before 2.42 does.
fn double_refuses_not_on_stdin() {
    let fixture = Fixture::new();
    let mut child = Command::new(fixture.double.join("git"))
        .arg("-C")
        .arg(&fixture.repository)
        .args(["rev-list", "--stdin"])
        .env(REAL_GIT, real_git())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(b"HEAD\n--not\nHEAD\n")
        .unwrap();
    let output = child.wait_with_output().unwrap();
    assert_eq!(output.status.code(), Some(128));
    assert_eq!(
        String::from_utf8_lossy(&output.stderr),
        "fatal: options not supported in --stdin mode\n"
    );
}

/// `archive` bundles a commit no remote holds through that Git, and the bundle verifies.
fn archive_bundles_on_old_git() {
    let fixture = Fixture::new();
    let report = fixture.ok(&["archive", fixture.tree.to_str().unwrap()]);
    let archive = PathBuf::from(report["archive"]["path"].as_str().expect("archive path"));
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(
        manifest["unique_commits"].as_array().map(Vec::len),
        Some(1),
        "{manifest}"
    );
    assert_eq!(manifest["bundle"]["file"], "commits.bundle", "{manifest}");
    git(
        &fixture.repository,
        &[
            "bundle",
            "verify",
            archive.join("commits.bundle").to_str().unwrap(),
        ],
    );
}

/// The real Git's exit status as this process's.
fn exit_code(status: std::process::ExitStatus) -> ExitCode {
    ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1))
}
