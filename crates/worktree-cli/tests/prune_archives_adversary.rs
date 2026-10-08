//! Adversarial cases for `prune-archives`: each tries to make the verb delete an archive it must
//! keep, or delete anything outside the archive directory it was given.
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

fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    names
}

/// Rewrite an archive's manifest in place.
fn edit_manifest(archive: &Path, edit: impl FnOnce(&mut Value)) {
    let path = archive.join("manifest.json");
    let mut manifest: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    edit(&mut manifest);
    std::fs::write(&path, serde_json::to_vec_pretty(&manifest).unwrap()).unwrap();
}

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
            write(&repository.join("source"), &format!("original {name}\n"));
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

    fn run(&self, args: &[&str]) -> Output {
        let outside = self.root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let mut json = vec!["--json"];
        json.extend_from_slice(args);
        Command::new(env!("CARGO_BIN_EXE_worktree"))
            .args(&json)
            .current_dir(outside)
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

    fn tree(&self, repository: &str, id: &str) -> PathBuf {
        let created = self.ok(&[
            "create",
            "--repo",
            self.repository(repository).to_str().unwrap(),
            "--id",
            id,
            "--purpose",
            "prune archives adversary",
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

    fn retire(&self, repository: &str, id: &str, tree: &Path) {
        self.ok(&["finish", id]);
        self.ok(&[
            "gc",
            "--repo",
            self.repository(repository).to_str().unwrap(),
            "--apply",
            "--id",
            id,
        ]);
        assert!(!tree.exists(), "{}", tree.display());
    }

    fn removable(&self, repository: &str, id: &str) -> PathBuf {
        let tree = self.tree(repository, id);
        let archive = PathBuf::from(self.archive(id, false)["path"].as_str().unwrap());
        push(&tree, id);
        self.retire(repository, id, &tree);
        archive
    }

    /// An archive whose commit no remote holds, with its tree removed.
    fn unpushed(&self, repository: &str, id: &str) -> (PathBuf, String) {
        let tree = self.tree(repository, id);
        let commit = git(&tree, &["rev-parse", "HEAD"]);
        let archive = PathBuf::from(self.archive(id, false)["path"].as_str().unwrap());
        self.retire(repository, id, &tree);
        (archive, commit)
    }

    fn prune_output(&self, repository: &str, extra: &[&str]) -> Output {
        let repository = self.repository(repository);
        let mut args = vec!["prune-archives", "--repo", repository.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.run(&args)
    }

    fn verdict(&self, repository: &str, directory: &str) -> String {
        let output = self.prune_output(repository, &["--scope", "profile"]);
        assert!(output.status.success(), "{output:?}");
        let report: Value = serde_json::from_slice(&output.stdout).unwrap();
        let matches: Vec<&Value> = report["archives"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|item| item["archive_directory"] == directory)
            .collect();
        assert_eq!(matches.len(), 1, "{report}");
        matches[0]["verdict"].as_str().unwrap().to_owned()
    }

    /// `--apply --id <reference>` must not succeed and must leave every listed path in place.
    fn apply_keeps(&self, repository: &str, reference: &str, kept: &[&Path]) -> Output {
        let output = self.prune_output(repository, &["--apply", "--id", reference]);
        for path in kept {
            assert!(
                std::fs::symlink_metadata(path).is_ok(),
                "{} was deleted by --apply --id {reference}: {}",
                path.display(),
                String::from_utf8_lossy(&output.stdout)
            );
        }
        output
    }
}

// --- Deletion outside the archive directory -------------------------------------------------

#[test]
fn bundle_file_naming_a_sibling_path_is_never_deleted() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "escape");
    // A loose file beside the archive: the contract says such files are never deleted.
    let victim = fixture.archive_root().join("alpha/branch-tips.tsv");
    write(&victim, "main\tabc\n");
    std::fs::remove_file(archive.join("commits.bundle")).unwrap();
    edit_manifest(&archive, |manifest| {
        manifest["bundle"]["file"] = Value::String("../branch-tips.tsv".into());
    });

    fixture.apply_keeps("alpha", "escape", &[&victim]);
}

#[test]
fn bundle_file_naming_an_absolute_path_is_never_deleted() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "absolute");
    let victim = fixture.root.path().join("outside/precious.txt");
    write(&victim, "not an archive file\n");
    std::fs::remove_file(archive.join("commits.bundle")).unwrap();
    edit_manifest(&archive, |manifest| {
        manifest["bundle"]["file"] = Value::String(victim.to_str().unwrap().into());
    });

    fixture.apply_keeps("alpha", "absolute", &[&victim]);
}

#[test]
fn manifest_named_file_that_is_a_directory_is_refused() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "dirnamed");
    std::fs::remove_file(archive.join("commits.bundle")).unwrap();
    write(&archive.join("commits.bundle/inner"), "kept\n");

    assert_eq!(fixture.verdict("alpha", "dirnamed"), "unrecorded-content");
    let inner = archive.join("commits.bundle/inner");
    let output = fixture.apply_keeps("alpha", "dirnamed", &[&inner]);
    assert!(!output.status.success(), "{output:?}");
}

#[test]
fn symlinked_archive_and_repository_directories_are_never_followed() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "target");
    let before = names(&archive);
    std::os::unix::fs::symlink(&archive, fixture.archive_root().join("alpha/linked")).unwrap();
    std::os::unix::fs::symlink(
        fixture.archive_root().join("alpha"),
        fixture.archive_root().join("gamma"),
    )
    .unwrap();

    let output = fixture.prune_output("alpha", &["--scope", "profile"]);
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let listed: Vec<&str> = report["archives"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["archive_directory"].as_str().unwrap())
        .collect();
    assert_eq!(listed, vec!["target"], "{report}");

    for reference in ["linked", "alpha/linked", "gamma/target"] {
        let output = fixture.apply_keeps("alpha", reference, &[&archive]);
        assert!(!output.status.success(), "{reference}: {output:?}");
        assert_eq!(names(&archive), before, "{reference}");
    }
}

#[test]
fn id_with_traversal_or_absolute_path_is_refused() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "named");
    let before = names(&archive);
    let absolute = archive.to_str().unwrap().to_owned();
    for reference in [
        "../alpha/named",
        "alpha/../alpha/named",
        "./named",
        "alpha/named/",
        "alpha//named",
        absolute.as_str(),
    ] {
        let output = fixture.apply_keeps("alpha", reference, &[&archive]);
        assert!(!output.status.success(), "{reference}: {output:?}");
        assert_eq!(names(&archive), before, "{reference}");
    }
}

// --- Remote proof that must not count --------------------------------------------------------

#[test]
fn local_remote_tracking_ref_does_not_count() {
    let fixture = Fixture::new(&["alpha"]);
    let (archive, commit) = fixture.unpushed("alpha", "tracking");
    git(
        &fixture.repository("alpha"),
        &["update-ref", "refs/remotes/origin/tracking", &commit],
    );

    assert_eq!(fixture.verdict("alpha", "tracking"), "commits-not-on-remote");
    let output = fixture.apply_keeps("alpha", "tracking", &[&archive.join("commits.bundle")]);
    assert!(!output.status.success(), "{output:?}");
}

#[test]
fn replacement_ref_does_not_count() {
    let fixture = Fixture::new(&["alpha"]);
    let (archive, commit) = fixture.unpushed("alpha", "replaced");
    let primary = fixture.repository("alpha");
    let main = git(&primary, &["rev-parse", "main"]);
    let tree = git(&primary, &["rev-parse", "main^{tree}"]);
    // A replacement for the advertised main tip whose parent is the unpushed commit.
    let fake = git(
        &primary,
        &["commit-tree", &tree, "-p", &commit, "-m", "replacement"],
    );
    git(&primary, &["replace", &main, &fake]);
    assert!(
        Command::new("git")
            .args(["-C", primary.to_str().unwrap()])
            .args(["merge-base", "--is-ancestor", &commit, &main])
            .status()
            .unwrap()
            .success(),
        "the replacement does not make the commit look held"
    );

    assert_eq!(fixture.verdict("alpha", "replaced"), "commits-not-on-remote");
    let output = fixture.apply_keeps("alpha", "replaced", &[&archive.join("commits.bundle")]);
    assert!(!output.status.success(), "{output:?}");
}

#[test]
fn graft_file_refuses_remote_proof() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "grafted");
    write(&fixture.repository("alpha").join(".git/info/grafts"), "");

    assert_eq!(
        fixture.verdict("alpha", "grafted"),
        "remote-proof-unavailable"
    );
    let output = fixture.apply_keeps("alpha", "grafted", &[&archive.join("manifest.json")]);
    assert!(!output.status.success(), "{output:?}");
}

#[test]
fn unique_commit_off_remote_refuses_although_head_is_held() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "partial");
    let primary = fixture.repository("alpha");
    git(&primary, &["switch", "--quiet", "-c", "side"]);
    write(&primary.join("side.txt"), "local only\n");
    git(&primary, &["add", "side.txt"]);
    git(&primary, &["commit", "--quiet", "-m", "local only"]);
    let local = git(&primary, &["rev-parse", "HEAD"]);
    git(&primary, &["switch", "--quiet", "main"]);
    edit_manifest(&archive, |manifest| {
        manifest["unique_commits"]
            .as_array_mut()
            .unwrap()
            .push(Value::String(local.clone()));
    });

    assert_eq!(fixture.verdict("alpha", "partial"), "commits-not-on-remote");
    let output = fixture.apply_keeps("alpha", "partial", &[&archive.join("commits.bundle")]);
    assert!(!output.status.success(), "{output:?}");
}

#[test]
fn one_unreachable_remote_of_two_refuses() {
    let fixture = Fixture::new(&["alpha"]);
    let archive = fixture.removable("alpha", "split");
    let unreachable = fixture.root.path().join("unreachable.git");
    git(
        &fixture.repository("alpha"),
        &["remote", "add", "backup", unreachable.to_str().unwrap()],
    );

    assert_eq!(fixture.verdict("alpha", "split"), "remote-proof-unavailable");
    let output = fixture.apply_keeps("alpha", "split", &[&archive.join("commits.bundle")]);
    assert!(!output.status.success(), "{output:?}");
}

#[test]
fn manifest_naming_another_repository_is_judged_by_that_repository() {
    let fixture = Fixture::new(&["alpha", "beta"]);
    let (archive, _) = fixture.unpushed("alpha", "foreign");
    let beta = std::fs::canonicalize(fixture.repository("beta")).unwrap();
    edit_manifest(&archive, |manifest| {
        manifest["repository_root"] = Value::String(beta.to_str().unwrap().into());
    });

    assert_eq!(fixture.verdict("beta", "foreign"), "commits-not-on-remote");
    let output = fixture.apply_keeps("beta", "alpha/foreign", &[&archive.join("commits.bundle")]);
    assert!(!output.status.success(), "{output:?}");
}

// --- Trees and selection ---------------------------------------------------------------------

#[test]
fn superseded_archive_of_a_live_tree_is_refused() {
    let fixture = Fixture::new(&["alpha"]);
    let tree = fixture.tree("alpha", "alive");
    fixture.archive("alive", false);
    push(&tree, "alive");
    let second = fixture.archive("alive", true);
    let superseded = PathBuf::from(second["superseded"].as_str().unwrap());
    let name = superseded.file_name().unwrap().to_str().unwrap().to_owned();

    assert_eq!(fixture.verdict("alpha", &name), "tree-still-present");
    assert_eq!(fixture.verdict("alpha", "alive"), "tree-still-present");
    let output = fixture.apply_keeps("alpha", &name, &[&superseded.join("manifest.json")]);
    assert!(!output.status.success(), "{output:?}");
    assert!(tree.is_dir());
}

#[test]
fn apply_with_a_refused_id_processes_the_rest_and_exits_non_zero() {
    let fixture = Fixture::new(&["alpha"]);
    let (kept, _) = fixture.unpushed("alpha", "kept");
    let gone = fixture.removable("alpha", "gone");
    let before = names(&kept);

    let output = fixture.prune_output("alpha", &["--apply", "--id", "kept", "--id", "gone"]);
    assert!(!output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    let by_name = |name: &str| {
        report["archives"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["archive_directory"] == name)
            .cloned()
            .unwrap_or_else(|| panic!("{name} missing: {report}"))
    };
    assert_eq!(by_name("kept")["verdict"], "commits-not-on-remote");
    assert_eq!(by_name("kept")["removed"], false);
    assert_eq!(by_name("gone")["removed"], true);
    assert_eq!(names(&kept), before);
    assert!(!gone.exists());
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["ok"], false, "{error}");
}
