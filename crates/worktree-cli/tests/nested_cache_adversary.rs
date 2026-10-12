//! Adversarial cases for `discard-cache` keeping nested repositories: repositories the `.git`
//! walk does not see, retained state the archive then cannot hold, and the finish-then-collect
//! path for every place a repository may sit in a Cargo target.
#![cfg(unix)]

use serde_json::Value;
use std::os::unix::fs::PermissionsExt as _;
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

fn write(path: &Path, contents: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    tree: PathBuf,
    /// Directories a case made unreadable; restored on drop so cleanup can run.
    sealed: Vec<PathBuf>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for directory in &self.sealed {
            let _ = std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o755));
        }
    }
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("one");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        write(&repository.join("source"), "original\n");
        write(&repository.join(".gitignore"), "/target\n");
        write(
            &repository.join("Cargo.toml"),
            "[package]\nname = \"one\"\n",
        );
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
            "cache",
            "--purpose",
            "nested cache adversary",
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
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    /// The refusal `args` must end in, read from stderr.
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

    /// A tagged target `target/` with the profile `target/debug`.
    fn tagged_target_with_profile(&self) {
        write(&self.tree.join("target/CACHEDIR.TAG"), TAG);
        self.profile("target/debug");
    }

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

    /// A repository at `relative` with one commit no remote holds; returns its id.
    fn nested_repository(&self, relative: &str) -> String {
        let nested = self.tree.join(relative);
        std::fs::create_dir_all(&nested).unwrap();
        git(&nested, &["init", "-b", "main"]);
        write(&nested.join("file"), "nested\n");
        git(&nested, &["add", "."]);
        git(&nested, &["commit", "-m", "nested fixture"]);
        git(&nested, &["rev-parse", "HEAD"])
    }

    /// `finish --discard-cache --archive`, then `gc --apply --id cache`; returns the finish
    /// output. Panics, naming the refusal, when either does not succeed.
    fn finish_and_collect(&self) -> Value {
        let finished = self.ok(&["finish", "--discard-cache", "--archive", self.tree_str()]);
        let repo = self.repository.to_str().unwrap();
        let review = self.ok(&["gc", "--repo", repo, "--dry-run", "--id", "cache"]);
        assert_eq!(review["assessments"][0]["eligible"], true, "{review}");
        self.ok(&["gc", "--repo", repo, "--apply", "--id", "cache"]);
        assert!(!self.tree.exists());
        finished
    }
}

fn holds_commit(git_dir: &Path, commit: &str) -> bool {
    Command::new("git")
        .arg("--git-dir")
        .arg(git_dir)
        .args(["cat-file", "-e", &format!("{commit}^{{commit}}")])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap()
        .status
        .success()
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

fn images(finished: &Value) -> Vec<String> {
    finished["archive"]["manifest"]["nested_repositories"]
        .as_array()
        .map(|images| {
            images
                .iter()
                .map(|image| image["path"].as_str().unwrap().to_owned())
                .collect()
        })
        .unwrap_or_default()
}

/// A bare repository is a Git repository with no `.git` entry. Integration tests create bare
/// remotes in scratch; one holding the only copy of a commit is deleted with `tmp/`.
#[test]
fn a_bare_fixture_repository_in_target_tmp_is_never_discarded() {
    let fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    let bare = fixture.tree.join("target/tmp/remote.git");
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "--bare", "-b", "main"]);
    // The commit is written straight into the bare repository; nothing else holds it.
    let blob_tree = git(&bare, &["mktree"]);
    let commit = git(&bare, &["commit-tree", &blob_tree, "-m", "only copy"]);
    git(&bare, &["update-ref", "refs/heads/main", &commit]);
    assert!(holds_commit(&bare, &commit));

    let applied = fixture.ok(&["discard-cache", fixture.tree_str()])["cache"].clone();
    assert!(
        holds_commit(&bare, &commit),
        "the bare repository's only commit {commit} was deleted: {applied}"
    );
}

/// A fixture repository with a linked worktree beside it in `tmp/`, as a worktree tool's own
/// tests leave one. Both are kept: deleting either would lose state no remote holds. The archive
/// cannot image a `.git` with linked worktrees, so `finish --discard-cache --archive` refuses
/// and names it; the tree stays.
#[test]
fn a_target_tmp_fixture_with_a_linked_worktree_is_kept_and_finish_names_it() {
    let fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    let commit = fixture.nested_repository("target/tmp/fixture");
    let repository = fixture.tree.join("target/tmp/fixture");
    git(
        &repository,
        &["worktree", "add", "-q", "-b", "side", "../linked"],
    );
    let linked = fixture.tree.join("target/tmp/linked");

    let refusal = fixture.refused(&["finish", "--discard-cache", "--archive", fixture.tree_str()]);
    assert_eq!(refusal["code"], "archive-unsupported-entry", "{refusal}");
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains("target/tmp/fixture/.git/worktrees/linked"),
        "{refusal}"
    );
    assert!(holds_commit(&repository.join(".git"), &commit), "{refusal}");
    assert_eq!(
        std::fs::read_to_string(repository.join("file")).unwrap(),
        "nested\n"
    );
    assert!(linked.join(".git").is_file(), "{refusal}");
    assert_eq!(
        std::fs::read_to_string(linked.join("file")).unwrap(),
        "nested\n"
    );
    assert!(fixture.tree.exists());
}

/// A repository whose `.git` is a gitfile (`git init --separate-git-dir`), work tree and Git
/// directory both in `tmp/`. `tmp/` splits around `work/`, whose gitfile is kept; `meta/`, the
/// repository itself, holds no entry named `.git`.
#[test]
fn a_separate_git_dir_beside_its_work_tree_in_target_tmp_is_never_discarded() {
    let fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    let work = fixture.tree.join("target/tmp/work");
    std::fs::create_dir_all(&work).unwrap();
    let meta = fixture.tree.join("target/tmp/meta");
    git(
        &work,
        &["init", "-q", "--separate-git-dir", meta.to_str().unwrap()],
    );
    write(&work.join("file"), "nested\n");
    git(&work, &["add", "."]);
    git(&work, &["commit", "-q", "-m", "only copy"]);
    let commit = git(&work, &["rev-parse", "HEAD"]);

    let applied = fixture.ok(&["discard-cache", fixture.tree_str()])["cache"].clone();
    assert!(work.join(".git").is_file(), "{applied}");
    assert!(
        holds_commit(&meta, &commit),
        "the repository behind the kept gitfile, holding the only copy of {commit}, was \
         deleted: {applied}"
    );
}

/// Test scratch a test left unreadable (a permission-error case that panicked before restoring
/// the mode). The walk cannot rule out a repository in it, so it is kept; nothing can prove it
/// recoverable either, so `gc` refuses the tree and names the directory.
#[test]
fn an_unreadable_directory_in_target_tmp_is_kept_and_gc_names_it() {
    let mut fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    let locked = fixture.tree.join("target/tmp/locked");
    write(&locked.join("state.json"), "{}\n");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
    fixture.sealed.push(locked.clone());

    let planned = fixture.ok(&["discard-cache", fixture.tree_str(), "--dry-run"])["cache"].clone();
    assert_eq!(
        paths(&planned, "retained_ignored"),
        ["target/tmp/locked"],
        "{planned}"
    );
    fixture.ok(&["finish", "--discard-cache", "--archive", fixture.tree_str()]);
    let repo = fixture.repository.to_str().unwrap();
    let review = fixture.ok(&["gc", "--repo", repo, "--dry-run", "--id", "cache"]);
    let assessment = &review["assessments"][0];
    assert_eq!(assessment["eligible"], false, "{review}");
    assert_eq!(
        assessment["refusal"]["code"], "worktree-state-unreadable",
        "{review}"
    );
    assert!(
        assessment["refusal"]["message"]
            .as_str()
            .unwrap()
            .contains("target/tmp/locked"),
        "{review}"
    );

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert_eq!(
        std::fs::read_to_string(locked.join("state.json")).unwrap(),
        "{}\n"
    );
    assert!(fixture.tree.exists());
}

/// A Git directory Git itself does not recognise (no `HEAD`: a crashed or deliberately broken
/// fixture). `discard-cache` keeps it as a `.git`; the archive must not then leave it out as
/// build layout, or removal through the archive deletes what discard kept.
#[test]
fn a_git_directory_retained_in_target_tmp_is_not_deleted_through_the_archive() {
    let fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    let commit = fixture.nested_repository("target/tmp/broken");
    let git_dir = fixture.tree.join("target/tmp/broken/.git");
    std::fs::remove_file(git_dir.join("HEAD")).unwrap();

    let planned = fixture.ok(&["discard-cache", fixture.tree_str(), "--dry-run"])["cache"].clone();
    assert_eq!(
        paths(&planned, "retained_ignored"),
        ["target/tmp/broken"],
        "{planned}"
    );
    let finished = fixture.ok(&["finish", "--discard-cache", "--archive", fixture.tree_str()]);
    assert!(
        git_dir.join("objects").is_dir(),
        "finish deleted the retained .git"
    );
    let repo = fixture.repository.to_str().unwrap();
    let applied = fixture.command(&["gc", "--repo", repo, "--apply", "--id", "cache"]);
    assert!(
        git_dir.join("objects").is_dir()
            || images(&finished).contains(&"target/tmp/broken".to_owned()),
        "gc removed the retained .git (objects of {commit}) and no image holds it: {finished} {}",
        String::from_utf8_lossy(&applied.stdout)
    );
}

/// A profile holding a repository is retained whole; finish must image the repository and the
/// tree must still be collected.
#[test]
fn finish_collects_a_tree_whose_profile_holds_a_repository() {
    let fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    fixture.profile("target/release");
    fixture.nested_repository("target/release/build/one/out/fixture");

    let finished = fixture.finish_and_collect();
    assert_eq!(
        images(&finished),
        ["target/release/build/one/out/fixture"],
        "{finished}"
    );
}

/// A profile below a target-triple directory holding a repository.
#[test]
fn finish_collects_a_tree_whose_triple_profile_holds_a_repository() {
    let fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    fixture.profile("target/x86_64-unknown-linux-gnu/debug");
    let commit = fixture.nested_repository("target/x86_64-unknown-linux-gnu/debug/build/out/fx");

    let applied = fixture.ok(&["discard-cache", fixture.tree_str()])["cache"].clone();
    assert_eq!(paths(&applied, "discarded"), ["target/debug"], "{applied}");
    assert!(holds_commit(
        &fixture
            .tree
            .join("target/x86_64-unknown-linux-gnu/debug/build/out/fx/.git"),
        &commit
    ));
    let finished = fixture.finish_and_collect();
    assert_eq!(
        images(&finished),
        ["target/x86_64-unknown-linux-gnu/debug/build/out/fx"],
        "{finished}"
    );
}

/// A nested tagged target inside `tmp/` holding a fixture repository in its own `tmp/`.
#[test]
fn a_nested_target_inside_target_tmp_keeps_its_fixture_repository() {
    let fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    write(&fixture.tree.join("target/tmp/inner/CACHEDIR.TAG"), TAG);
    fixture.profile("target/tmp/inner/debug");
    let commit = fixture.nested_repository("target/tmp/inner/tmp/fixture");

    fixture.ok(&["discard-cache", fixture.tree_str()]);
    assert!(holds_commit(
        &fixture.tree.join("target/tmp/inner/tmp/fixture/.git"),
        &commit
    ));
    let finished = fixture.finish_and_collect();
    assert_eq!(
        images(&finished),
        ["target/tmp/inner/tmp/fixture"],
        "{finished}"
    );
}

/// `tmp/` entries that are symbolic links (to a repository outside, and one named `.git`) and a
/// file named `.git` directly in `tmp/`: links are unlinked, never followed, and every `.git`
/// stays.
#[test]
fn links_and_git_files_in_target_tmp_never_reach_a_repository() {
    let fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    let outside = fixture.root.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    git(&outside, &["init", "-q"]);
    write(&outside.join("file"), "outside\n");
    git(&outside, &["add", "."]);
    git(&outside, &["commit", "-q", "-m", "outside"]);
    let commit = git(&outside, &["rev-parse", "HEAD"]);
    let scratch = fixture.tree.join("target/tmp");
    std::fs::create_dir_all(scratch.join("linked")).unwrap();
    std::os::unix::fs::symlink(&outside, scratch.join("outside")).unwrap();
    std::os::unix::fs::symlink(outside.join(".git"), scratch.join("linked/.git")).unwrap();
    write(&scratch.join(".git"), "gitdir: nowhere\n");

    let applied = fixture.ok(&["discard-cache", fixture.tree_str()])["cache"].clone();
    assert!(holds_commit(&outside.join(".git"), &commit), "{applied}");
    assert!(outside.join("file").is_file());
    assert!(scratch.join(".git").is_file(), "{applied}");
    assert!(
        std::fs::symlink_metadata(scratch.join("linked/.git")).is_ok(),
        "{applied}"
    );
    assert!(std::fs::symlink_metadata(scratch.join("outside")).is_err());
}

/// A `.git` directly in an untagged target's root (`git init target`): the target falls to its
/// parts and the repository is kept and imaged.
#[test]
fn a_repository_at_an_untagged_target_root_is_kept_and_imaged() {
    let fixture = Fixture::new();
    // Cargo.toml is tracked, so `target/` counts as tagged by the untagged rule.
    fixture.profile("target/debug");
    let commit = fixture.nested_repository("target");

    let applied = fixture.ok(&["discard-cache", fixture.tree_str()])["cache"].clone();
    assert!(
        holds_commit(&fixture.tree.join("target/.git"), &commit),
        "{applied}"
    );
    let finished = fixture.finish_and_collect();
    assert_eq!(images(&finished), ["target"], "{finished}");
}

/// A bare repository at `relative` in the tree whose one commit was pushed from a scratch clone
/// outside the tree, so nothing but that repository holds it; returns the commit.
fn bare_repository_with_pushed_commit(fixture: &Fixture, relative: &str) -> String {
    let bare = fixture.tree.join(relative);
    std::fs::create_dir_all(&bare).unwrap();
    git(&bare, &["init", "-q", "--bare", "-b", "main"]);
    let scratch = fixture
        .root
        .path()
        .join(format!("scratch-{}", relative.replace('/', "-")));
    std::fs::create_dir_all(&scratch).unwrap();
    git(&scratch, &["init", "-q", "-b", "main"]);
    write(&scratch.join("work.txt"), "only copy\n");
    git(&scratch, &["add", "."]);
    git(&scratch, &["commit", "-q", "-m", "only copy"]);
    let commit = git(&scratch, &["rev-parse", "HEAD"]);
    git(
        &scratch,
        &["push", "-q", bare.to_str().unwrap(), "HEAD:refs/heads/main"],
    );
    std::fs::remove_dir_all(&scratch).unwrap();
    assert!(holds_commit(&bare, &commit));
    commit
}

/// Apply `archive`'s `dirty.patch` over a fresh checkout of the tree's HEAD; returns the
/// checkout.
fn restore(fixture: &Fixture, archive: &Path) -> PathBuf {
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(archive.join("manifest.json")).unwrap()).unwrap();
    let restored = fixture.root.path().join("restored");
    git(
        fixture.root.path(),
        &[
            "clone",
            "-q",
            "--no-checkout",
            fixture.repository.to_str().unwrap(),
            restored.to_str().unwrap(),
        ],
    );
    git(
        &restored,
        &[
            "checkout",
            "-q",
            "--detach",
            manifest["head"].as_str().unwrap(),
        ],
    );
    git(
        &restored,
        &[
            "apply",
            "--binary",
            archive.join("dirty.patch").to_str().unwrap(),
        ],
    );
    restored
}

/// A bare repository inside a profile `discard-cache` keeps whole. The archive must hold it too:
/// before, it left the repository's files out as cargo build layout, no image and no blocker,
/// and `gc --apply` then removed the tree and the only copy of its commit. Outcome: the tree is
/// collected, `dirty.patch` holds every file of the repository as an ordinary untracked file,
/// the profile's own layout stays left out, and applying the patch restores the commit.
#[test]
fn a_bare_repository_in_a_kept_profile_survives_finish_and_gc() {
    let fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    fixture.profile("target/release");
    let relative = "target/release/build/one/out/remote.git";
    let commit = bare_repository_with_pushed_commit(&fixture, relative);

    let finished = fixture.finish_and_collect();
    let archive = PathBuf::from(finished["archive"]["path"].as_str().unwrap());
    let manifest = &finished["archive"]["manifest"];
    assert_eq!(manifest["format"], "worktree.archive/3", "{finished}");
    assert_eq!(
        manifest["build_output"]["targets"][0]["path"], "target",
        "{finished}"
    );
    let patch =
        String::from_utf8_lossy(&std::fs::read(archive.join("dirty.patch")).unwrap()).into_owned();
    assert!(
        patch.contains(&format!("b/{relative}/HEAD")),
        "the archive left the repository out: {patch}"
    );
    assert!(patch.contains(&format!("b/{relative}/objects/")), "{patch}");
    assert!(!patch.contains("target/release/deps/"), "{patch}");
    assert!(!patch.contains(".fingerprint"), "{patch}");

    let restored = restore(&fixture, &archive);
    assert!(
        holds_commit(&restored.join(relative), &commit),
        "applying the archive did not restore {commit}"
    );
}

/// A bare repository directly below a recognised target's `tmp/` while the target still holds a
/// profile, archived without any discard: `tmp/` is cargo layout, the repository in it is not.
#[test]
fn a_bare_repository_in_target_tmp_is_not_left_out_of_the_archive() {
    let fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    write(&fixture.tree.join("target/tmp/scratch/state.json"), "{}\n");
    let relative = "target/tmp/remote.git";
    let commit = bare_repository_with_pushed_commit(&fixture, relative);

    let archived = fixture.ok(&["archive", fixture.tree_str()]);
    let archive = PathBuf::from(archived["archive"]["path"].as_str().unwrap());
    let manifest = &archived["archive"]["manifest"];
    assert_eq!(manifest["format"], "worktree.archive/3", "{archived}");
    let patch =
        String::from_utf8_lossy(&std::fs::read(archive.join("dirty.patch")).unwrap()).into_owned();
    assert!(
        patch.contains(&format!("b/{relative}/HEAD")),
        "the archive left the repository out: {patch}"
    );
    assert!(!patch.contains("target/tmp/scratch/"), "{patch}");
    assert!(!patch.contains("target/debug/"), "{patch}");

    // The tree still verifies against the archive it was just written from.
    fixture.ok(&["finish", fixture.tree_str()]);
    let repo = fixture.repository.to_str().unwrap();
    let review = fixture.ok(&["gc", "--repo", repo, "--dry-run", "--id", "cache"]);
    assert_eq!(review["assessments"][0]["eligible"], true, "{review}");

    let restored = restore(&fixture, &archive);
    assert!(holds_commit(&restored.join(relative), &commit));
}

/// A bare repository written into a recorded profile after the archive: removal re-observes it
/// immediately before deleting left-out layout, so it is no build layout, the tree no longer
/// matches the archive, and `gc --apply` refuses and deletes nothing.
#[test]
fn a_bare_repository_made_in_recorded_layout_after_the_archive_is_never_deleted() {
    let fixture = Fixture::new();
    fixture.tagged_target_with_profile();
    let archived = fixture.ok(&["archive", fixture.tree_str()]);
    let manifest = &archived["archive"]["manifest"];
    assert_eq!(manifest["format"], "worktree.archive/3", "{archived}");
    fixture.ok(&["finish", fixture.tree_str()]);
    let relative = "target/debug/build/one/out/remote.git";
    let commit = bare_repository_with_pushed_commit(&fixture, relative);

    let repo = fixture.repository.to_str().unwrap();
    let applied = fixture.ok(&["gc", "--repo", repo, "--apply", "--id", "cache"]);
    assert_eq!(
        applied["assessments"][0]["refusal"]["code"], "archive-stale",
        "{applied}"
    );
    assert!(
        holds_commit(&fixture.tree.join(relative), &commit),
        "gc deleted the repository as build layout: {applied}"
    );
}
