//! `prune-archives` lists every archive in scope with its bytes and verdict, and with
//! `--apply --id` deletes only an archive whose every recorded commit a freshly advertised remote
//! ref holds by ancestry and which holds no uncommitted state. Every other archive is refused with
//! its reason and kept.
#![cfg(unix)]

use serde_json::Value;
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
            "init.defaultBranch=main",
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
        "git {args:?} in {}: {}",
        path.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim_end().into()
}

fn push(tree: &Path, id: &str) {
    let target = format!("HEAD:refs/heads/{id}");
    git(tree, &["push", "--quiet", "origin", &target]);
}

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

/// Sum of the regular files directly in `dir`.
fn file_bytes(dir: &Path) -> u64 {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| std::fs::symlink_metadata(entry.unwrap().path()).unwrap())
        .filter(std::fs::Metadata::is_file)
        .map(|metadata| metadata.len())
        .sum()
}

/// File names directly in `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Repositories with bare remotes under one activated profile.
struct Fixture {
    root: tempfile::TempDir,
    workspace: PathBuf,
}

impl Fixture {
    fn new(repositories: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        for name in repositories {
            let repository = workspace.join(name);
            std::fs::create_dir_all(&repository).unwrap();
            git(&repository, &["init", "--quiet"]);
            write(&repository.join("source"), "original\n");
            git(&repository, &["add", "."]);
            git(&repository, &["commit", "--quiet", "-m", "fixture"]);
            let remote = root.path().join(format!("{name}.git"));
            git(
                root.path(),
                &[
                    "clone",
                    "--quiet",
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
        }
        let profile = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles/default.toml");
        let fixture = Self { root, workspace };
        fixture.ok(&[
            "activate",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace",
            fixture.workspace.to_str().unwrap(),
        ]);
        fixture
    }

    fn repository(&self, name: &str) -> PathBuf {
        self.workspace.join(name)
    }

    fn archive_root(&self) -> PathBuf {
        self.root.path().join("state/worktree/archives")
    }

    fn command(&self, args: &[&str]) -> Command {
        let outside = self.root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_worktree"));
        command
            .args(args)
            .current_dir(outside)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1");
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        let mut json = vec!["--json"];
        json.extend_from_slice(args);
        self.command(&json).output().unwrap()
    }

    /// Human-readable output.
    fn human(&self, args: &[&str]) -> Output {
        self.command(args).output().unwrap()
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

    /// Create a managed tree and commit one change in it; returns its path.
    fn tree(&self, repository: &str, id: &str) -> PathBuf {
        let created = self.ok(&[
            "create",
            "--repo",
            self.repository(repository).to_str().unwrap(),
            "--id",
            id,
            "--purpose",
            "prune archives",
        ]);
        let tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
        write(&tree.join("work.txt"), &format!("{id}\n"));
        git(&tree, &["add", "work.txt"]);
        git(&tree, &["commit", "--quiet", "-m", id]);
        tree
    }

    fn archive(&self, id: &str, replace: bool) -> Value {
        let mut args = vec!["archive", id];
        if replace {
            args.push("--replace");
        }
        self.ok(&args)["archive"].clone()
    }

    /// Finish the tree and remove it with `gc --apply`, as an operator would.
    fn retire(&self, repository: &str, id: &str, tree: &Path) {
        self.ok(&["finish", id]);
        let report = self.ok(&[
            "gc",
            "--repo",
            self.repository(repository).to_str().unwrap(),
            "--apply",
            "--id",
            id,
        ]);
        assert!(report["assessments"][0]["evidence"].is_object(), "{report}");
        assert!(!tree.exists(), "{}", tree.display());
    }

    /// A removed tree whose archived commit a remote branch holds: the archive is removable.
    fn removable(&self, repository: &str, id: &str) -> PathBuf {
        let tree = self.tree(repository, id);
        let archive = PathBuf::from(self.archive(id, false)["path"].as_str().unwrap());
        push(&tree, id);
        self.retire(repository, id, &tree);
        archive
    }

    fn prune(&self, repository: &str, extra: &[&str]) -> Value {
        let repository = self.repository(repository);
        let mut args = vec!["prune-archives", "--repo", repository.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.ok(&args)
    }

    fn prune_output(&self, repository: &str, extra: &[&str]) -> Output {
        let repository = self.repository(repository);
        let mut args = vec!["prune-archives", "--repo", repository.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    /// The one assessment of `directory` in a dry-run of `repository`.
    fn assessment(&self, repository: &str, directory: &str) -> Value {
        let report = self.prune(repository, &[]);
        let matches: Vec<&Value> = report["archives"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["archive_directory"] == directory)
            .collect();
        assert_eq!(matches.len(), 1, "{report}");
        matches[0].clone()
    }

    /// `--apply --id <directory>` is refused, names the verdict and keeps every file.
    fn apply_is_refused(&self, repository: &str, archive: &Path, verdict: &str) {
        let before = names(archive);
        let directory = archive.file_name().unwrap().to_str().unwrap();
        let output = self.prune_output(repository, &["--apply", "--id", directory]);
        assert!(!output.status.success(), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(report["archives"][0]["verdict"], verdict, "{report}");
        assert_eq!(report["archives"][0]["removed"], false, "{report}");
        assert_eq!(names(archive), before);
    }
}

#[test]
fn removable_archive_is_listed_then_removed_with_bytes_freed() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "pushed");
    let bytes = file_bytes(&archive);
    assert!(archive.join("commits.bundle").is_file());

    let dry = fixture.human(&[
        "prune-archives",
        "--repo",
        fixture.repository("alpha").to_str().unwrap(),
    ]);
    assert!(dry.status.success(), "{dry:?}");
    let text = String::from_utf8(dry.stdout).unwrap();
    let line = text
        .lines()
        .find(|line| line.starts_with("pushed\t"))
        .unwrap_or_else(|| panic!("{text}"));
    assert!(line.contains("Removable"), "{line}");
    assert!(line.contains(&format!("\t{bytes}\t")), "{line}");
    assert!(archive.is_dir());

    let report = fixture.prune("alpha", &["--dry-run"]);
    let item = &report["archives"][0];
    assert_eq!(item["verdict"], "removable", "{report}");
    assert_eq!(item["bytes"], bytes);
    assert_eq!(item["worktree_id"], "pushed");
    assert_eq!(item["removed"], false);
    assert_eq!(report["totals"]["removable_bytes"], bytes);
    assert!(archive.is_dir());

    let applied = fixture.human(&[
        "prune-archives",
        "--repo",
        fixture.repository("alpha").to_str().unwrap(),
        "--apply",
        "--id",
        "pushed",
    ]);
    assert!(applied.status.success(), "{applied:?}");
    let text = String::from_utf8(applied.stdout).unwrap();
    assert!(text.contains(&format!("removed pushed {bytes}")), "{text}");
    assert!(text.contains(&format!("freed {bytes} bytes")), "{text}");
    assert!(!archive.exists());
}

#[test]
fn unpushed_commit_is_refused() {
    let fixture = Fixture::new(&["alpha"]);
    let tree = fixture.tree("alpha", "local");
    let archive = PathBuf::from(fixture.archive("local", false)["path"].as_str().unwrap());
    fixture.retire("alpha", "local", &tree);

    let item = fixture.assessment("alpha", "local");
    assert_eq!(item["verdict"], "commits-not-on-remote", "{item}");
    assert_eq!(item["commits_not_on_remote"], 1, "{item}");
    fixture.apply_is_refused("alpha", &archive, "commits-not-on-remote");
}

#[test]
fn patch_equivalent_commit_is_still_refused() {
    let fixture = Fixture::new(&["alpha"]);
    let tree = fixture.tree("alpha", "rebased");
    let archive = PathBuf::from(fixture.archive("rebased", false)["path"].as_str().unwrap());
    let commit = git(&tree, &["rev-parse", "HEAD"]);
    let primary = fixture.repository("alpha");
    write(&primary.join("other"), "moves main first\n");
    git(&primary, &["add", "other"]);
    git(&primary, &["commit", "--quiet", "-m", "other"]);
    git(&primary, &["cherry-pick", &commit]);
    assert_ne!(git(&primary, &["rev-parse", "HEAD"]), commit);
    git(&primary, &["push", "--quiet", "origin", "main"]);
    fixture.retire("alpha", "rebased", &tree);

    let item = fixture.assessment("alpha", "rebased");
    assert_eq!(item["verdict"], "commits-not-on-remote", "{item}");
    fixture.apply_is_refused("alpha", &archive, "commits-not-on-remote");
}

#[test]
fn uncommitted_state_is_refused() {
    let fixture = Fixture::new(&["alpha"]);
    let tree = fixture.tree("alpha", "dirty");
    push(&tree, "dirty");
    write(&tree.join("notes.txt"), "never committed\n");
    let archive = PathBuf::from(fixture.archive("dirty", false)["path"].as_str().unwrap());
    assert!(archive.join("dirty.patch").is_file());
    fixture.retire("alpha", "dirty", &tree);

    let item = fixture.assessment("alpha", "dirty");
    assert_eq!(item["verdict"], "uncommitted-state", "{item}");
    fixture.apply_is_refused("alpha", &archive, "uncommitted-state");
}

#[test]
fn nested_repository_image_is_refused() {
    let fixture = Fixture::new(&["alpha"]);
    let tree = fixture.tree("alpha", "nested");
    write(&tree.join(".gitignore"), "evidence/\n");
    git(&tree, &["add", ".gitignore"]);
    git(&tree, &["commit", "--quiet", "-m", "ignore evidence"]);
    push(&tree, "nested");
    let nested = tree.join("evidence/fixture");
    std::fs::create_dir_all(&nested).unwrap();
    git(&nested, &["init", "--quiet"]);
    write(&nested.join("a.txt"), "one\n");
    git(&nested, &["add", "."]);
    git(&nested, &["commit", "--quiet", "-m", "nested"]);
    let archive = PathBuf::from(fixture.archive("nested", false)["path"].as_str().unwrap());
    assert!(archive.join("nested-1.tar").is_file());
    fixture.retire("alpha", "nested", &tree);

    let item = fixture.assessment("alpha", "nested");
    assert_eq!(item["verdict"], "nested-repositories", "{item}");
    fixture.apply_is_refused("alpha", &archive, "nested-repositories");
}

#[test]
fn present_tree_is_refused() {
    let fixture = Fixture::new(&["alpha"]);
    let tree = fixture.tree("alpha", "present");
    let archive = PathBuf::from(fixture.archive("present", false)["path"].as_str().unwrap());
    push(&tree, "present");

    let item = fixture.assessment("alpha", "present");
    assert_eq!(item["verdict"], "tree-still-present", "{item}");
    fixture.apply_is_refused("alpha", &archive, "tree-still-present");
    assert!(tree.is_dir());
}

#[test]
fn offline_remote_is_refused() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "offline");
    let unreachable = fixture.root.path().join("unreachable.git");
    git(
        &fixture.repository("alpha"),
        &["remote", "set-url", "origin", unreachable.to_str().unwrap()],
    );

    let item = fixture.assessment("alpha", "offline");
    assert_eq!(item["verdict"], "remote-proof-unavailable", "{item}");
    fixture.apply_is_refused("alpha", &archive, "remote-proof-unavailable");
}

#[test]
fn unrecorded_file_is_refused() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "extra");
    write(&archive.join("notes.txt"), "an operator's note\n");

    let item = fixture.assessment("alpha", "extra");
    assert_eq!(item["verdict"], "unrecorded-content", "{item}");
    fixture.apply_is_refused("alpha", &archive, "unrecorded-content");
    assert!(archive.join("notes.txt").is_file());
}

#[test]
fn apply_without_id_refuses() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "unnamed");
    let before = names(&archive);

    let output = fixture.prune_output("alpha", &["--apply"]);
    assert!(!output.status.success(), "{output:?}");
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["ok"], false);
    assert!(
        error["message"].as_str().unwrap().contains("--id"),
        "{error}"
    );
    assert_eq!(names(&archive), before);
}

#[test]
fn scope_repo_lists_only_this_repository() {
    let fixture = Fixture::new(&["alpha", "beta"]);
    fixture.removable("alpha", "alpha-tree");
    fixture.removable("beta", "beta-tree");

    let listed = |extra: &[&str]| {
        let mut directories: Vec<String> = fixture.prune("alpha", extra)["archives"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["archive_directory"].as_str().unwrap().to_owned())
            .collect();
        directories.sort();
        directories
    };
    assert_eq!(listed(&[]), vec!["alpha-tree"]);
    assert_eq!(listed(&["--scope", "repo"]), vec!["alpha-tree"]);
    assert_eq!(
        listed(&["--scope", "profile"]),
        vec!["alpha-tree", "beta-tree"]
    );
}

#[test]
fn loose_files_are_skipped_not_deleted() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "loose");
    let tips = fixture.archive_root().join("alpha/branch-tips.tsv");
    let bundle = fixture.archive_root().join("stray.bundle");
    write(&tips, "main\tabc\n");
    write(&bundle, "not an archive\n");

    for scope in ["repo", "profile"] {
        let report = fixture.prune("alpha", &["--scope", scope]);
        let skipped: Vec<&str> = report["skipped"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["path"].as_str().unwrap())
            .collect();
        assert!(skipped.contains(&tips.to_str().unwrap()), "{report}");
        if scope == "profile" {
            assert!(skipped.contains(&bundle.to_str().unwrap()), "{report}");
        }
        assert!(
            report["archives"]
                .as_array()
                .unwrap()
                .iter()
                .all(|item| item["archive_directory"] == "loose"),
            "{report}"
        );
    }

    let output = fixture.prune_output("alpha", &["--apply", "--id", "branch-tips.tsv"]);
    assert!(!output.status.success(), "{output:?}");
    assert!(tips.is_file());

    fixture.prune("alpha", &["--apply", "--id", "loose"]);
    assert!(!archive.exists());
    assert!(tips.is_file());
    assert!(bundle.is_file());
}

#[test]
fn superseded_archive_is_assessed() {
    let fixture = Fixture::new(&["alpha"]);
    let tree = fixture.tree("alpha", "twice");
    let first = fixture.archive("twice", false);
    push(&tree, "twice");
    let second = fixture.archive("twice", true);
    let current = PathBuf::from(second["path"].as_str().unwrap());
    let superseded = PathBuf::from(second["superseded"].as_str().unwrap());
    assert_eq!(first["path"], second["path"]);
    fixture.retire("alpha", "twice", &tree);
    let name = superseded.file_name().unwrap().to_str().unwrap().to_owned();
    assert!(name.starts_with("twice.superseded-"), "{name}");

    let item = fixture.assessment("alpha", &name);
    assert_eq!(item["verdict"], "removable", "{item}");
    assert_eq!(item["worktree_id"], "twice");
    assert_eq!(item["bytes"], file_bytes(&superseded));
    assert_eq!(fixture.assessment("alpha", "twice")["verdict"], "removable");

    let report = fixture.prune("alpha", &["--apply", "--id", &name]);
    assert_eq!(report["archives"][0]["removed"], true, "{report}");
    assert!(!superseded.exists());
    assert!(current.is_dir());
}
