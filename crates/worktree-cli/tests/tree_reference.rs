//! One tree reference — a registered id, a tree path or a unique tree directory name — works in
//! `finish`, `discard-cache`, `archive`, `gc --id` and `reconcile --id`; `finish` prints the id.

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
            "maintenance.auto=false",
        ])
        .args(args)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
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
}

impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("one");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        write(&repository.join("source"), "original\n");
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
        let fixture = Self { root, repository };
        fixture.ok(&[
            "activate",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ]);
        fixture
    }

    fn repo(&self) -> &str {
        self.repository.to_str().unwrap()
    }

    /// A neutral working directory outside every tree and holding none of their names.
    fn outside(&self) -> PathBuf {
        let outside = self.root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        outside
    }

    fn create(&self, id: &str) -> PathBuf {
        let created = self.ok(&[
            "create",
            "--repo",
            self.repo(),
            "--id",
            id,
            "--purpose",
            "tree reference",
        ]);
        PathBuf::from(created["evidence"]["path"].as_str().unwrap())
    }

    /// Register a linked tree at an arbitrary path the way a legacy tree is adopted, so that its
    /// directory name differs from its id.
    fn adopt(&self, path: &Path, id: &str) -> PathBuf {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        git(
            &self.repository,
            &[
                "worktree",
                "add",
                "--detach",
                path.to_str().unwrap(),
                "main",
            ],
        );
        let adopted = self.ok(&[
            "repo",
            "adopt",
            "--repo",
            self.repo(),
            "--path",
            path.to_str().unwrap(),
            "--id",
            id,
            "--purpose",
            "legacy tree",
        ]);
        PathBuf::from(adopted["evidence"]["path"].as_str().unwrap())
    }

    fn run(&self, cwd: &Path, json: bool, args: &[&str]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_worktree"));
        if json {
            command.arg("--json");
        }
        command
            .args(args)
            .current_dir(cwd)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn ok_in(&self, cwd: &Path, args: &[&str]) -> Value {
        let output = self.run(cwd, true, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        self.ok_in(&self.outside(), args)
    }

    fn refused_in(&self, cwd: &Path, args: &[&str]) -> Value {
        let output = self.run(cwd, true, args);
        assert!(
            !output.status.success(),
            "expected a refusal from {args:?}: {}",
            String::from_utf8_lossy(&output.stdout)
        );
        serde_json::from_slice(&output.stderr).unwrap()
    }

    fn refused(&self, args: &[&str]) -> Value {
        self.refused_in(&self.outside(), args)
    }

    fn human(&self, cwd: &Path, args: &[&str]) -> String {
        let output = self.run(cwd, false, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn assessed_ids(&self, command: &str, reference: &str) -> Vec<String> {
        let report = self.ok(&[
            command,
            "--repo",
            self.repo(),
            "--dry-run",
            "--id",
            reference,
        ]);
        report["assessments"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["record"]["id"].as_str().unwrap().to_owned())
            .collect()
    }
}

#[test]
fn gc_dry_run_by_path_and_by_directory_name_assesses_the_record_named_by_id() {
    let fixture = Fixture::new();
    let tree = fixture.create("alpha");
    fixture.create("other");
    fixture.ok(&["finish", tree.to_str().unwrap()]);
    fixture.ok(&["finish", "other"]);

    let by_id = fixture.assessed_ids("gc", "alpha");
    assert_eq!(by_id, vec!["alpha".to_owned()]);
    assert_eq!(fixture.assessed_ids("gc", tree.to_str().unwrap()), by_id);
    assert_eq!(fixture.assessed_ids("gc", "alpha"), by_id);

    // A relative path resolves against the working directory.
    let relative = fixture.ok_in(
        tree.parent().unwrap(),
        &[
            "gc",
            "--repo",
            fixture.repo(),
            "--dry-run",
            "--id",
            "./alpha",
        ],
    );
    assert_eq!(relative["assessments"][0]["record"]["id"], "alpha");
    assert_eq!(relative["assessments"].as_array().unwrap().len(), 1);
}

#[test]
fn gc_apply_with_the_path_finish_printed_removes_the_finished_tree() {
    let fixture = Fixture::new();
    let tree = fixture.create("printed");

    let printed = fixture.human(&tree, &["finish"]);
    let line = printed.lines().last().unwrap();
    assert_eq!(line, format!("finished printed {}", tree.display()));
    let path = line.split_whitespace().nth(2).unwrap();

    let review = fixture.ok(&["gc", "--repo", fixture.repo(), "--dry-run", "--id", path]);
    assert_eq!(review["assessments"][0]["record"]["id"], "printed");
    assert_eq!(review["assessments"][0]["eligible"], true, "{review}");
    let applied = fixture.ok(&["gc", "--repo", fixture.repo(), "--apply", "--id", path]);
    assert_eq!(applied["assessments"][0]["record"]["id"], "printed");
    assert!(
        applied["assessments"][0]["evidence"].is_object(),
        "{applied}"
    );
    assert!(!tree.exists());

    // The id printed by finish is equally a gc reference for the record after removal.
    let after = fixture.ok(&[
        "gc",
        "--repo",
        fixture.repo(),
        "--dry-run",
        "--id",
        "printed",
    ]);
    assert_eq!(after["assessments"], serde_json::json!([]));
}

#[test]
fn finish_by_id_outside_the_tree_finishes_it_and_reports_the_id() {
    let fixture = Fixture::new();
    let tree = fixture.create("remote-finish");

    let finished = fixture.ok(&["finish", "remote-finish"]);
    assert_eq!(finished["evidence"]["operation"], "finish");
    assert_eq!(finished["evidence"]["id"], "remote-finish");
    assert_eq!(finished["evidence"]["path"], tree.to_str().unwrap());

    let status = fixture.ok(&["status"]);
    assert_eq!(status["records"][0]["lifecycle"], "finished", "{status}");
}

#[test]
fn discard_cache_and_archive_accept_an_id_from_outside_the_tree() {
    let fixture = Fixture::new();
    let tree = fixture.create("by-id");

    let cache = fixture.ok(&["discard-cache", "--dry-run", "by-id"]);
    assert_eq!(cache["cache"]["path"], tree.to_str().unwrap());
    write(&tree.join("draft"), "unpublished\n");
    let archived = fixture.ok(&["archive", "by-id"]);
    assert_eq!(archived["archive"]["manifest"]["id"], "by-id");
}

#[test]
fn a_dotted_directory_name_resolves_and_a_dotted_id_still_refuses() {
    let fixture = Fixture::new();
    let tree = fixture.adopt(
        &fixture.root.path().join("legacy/hard-defects-0.7.0"),
        "hard-defects",
    );

    let finished = fixture.ok(&["finish", "hard-defects-0.7.0"]);
    assert_eq!(finished["evidence"]["id"], "hard-defects");
    assert_eq!(finished["evidence"]["path"], tree.to_str().unwrap());

    assert_eq!(
        fixture.assessed_ids("gc", "hard-defects-0.7.0"),
        vec!["hard-defects".to_owned()]
    );
    assert_eq!(
        fixture.assessed_ids("gc", tree.to_str().unwrap()),
        vec!["hard-defects".to_owned()]
    );
    // A finished tree outside the managed root is a retirement candidate, named by either form.
    let reconcile = fixture.assessed_ids("reconcile", "hard-defects-0.7.0");
    assert_eq!(reconcile, vec!["hard-defects".to_owned()]);
    assert_eq!(reconcile, fixture.assessed_ids("reconcile", "hard-defects"));

    let refusal = fixture.refused(&[
        "create",
        "--repo",
        fixture.repo(),
        "--id",
        "a.b",
        "--purpose",
        "dotted",
    ]);
    assert_eq!(refusal["code"], "invalid-worktree-id");
    assert!(
        refusal["message"]
            .as_str()
            .unwrap()
            .contains("hyphens instead of dots"),
        "{refusal}"
    );
}

#[test]
fn two_trees_sharing_a_directory_name_refuse_as_ambiguous() {
    let fixture = Fixture::new();
    fixture.create("beta");
    fixture.adopt(&fixture.root.path().join("legacy/beta"), "legacy-beta");
    fixture.ok(&["finish", "legacy-beta"]);

    let refusal = fixture.refused(&["gc", "--repo", fixture.repo(), "--dry-run", "--id", "beta"]);
    assert_eq!(refusal["code"], "ambiguous-worktree-reference", "{refusal}");
    let message = refusal["message"].as_str().unwrap();
    assert!(
        message.contains("beta") && message.contains("legacy-beta"),
        "{message}"
    );

    let refusal = fixture.refused(&["finish", "beta"]);
    assert_eq!(refusal["code"], "ambiguous-worktree-reference", "{refusal}");
}

#[test]
fn a_path_and_an_id_naming_different_trees_refuse_as_ambiguous() {
    let fixture = Fixture::new();
    fixture.create("gamma");
    let legacy = fixture.adopt(&fixture.root.path().join("legacy/gamma"), "legacy-gamma");

    // From `legacy/`, `gamma` is a path to legacy-gamma and the id of the other tree.
    let refusal = fixture.refused_in(legacy.parent().unwrap(), &["finish", "gamma"]);
    assert_eq!(refusal["code"], "ambiguous-worktree-reference", "{refusal}");
    let message = refusal["message"].as_str().unwrap();
    assert!(
        message.contains("gamma") && message.contains("legacy-gamma"),
        "{message}"
    );
}

#[test]
fn an_unknown_reference_refuses_and_names_the_value() {
    let fixture = Fixture::new();
    fixture.create("delta");

    for command in ["gc", "reconcile"] {
        let refusal = fixture.refused(&[
            command,
            "--repo",
            fixture.repo(),
            "--dry-run",
            "--id",
            "nothing-here.1",
        ]);
        // Not a valid id and no tree: the released `invalid-worktree-id` code.
        assert_eq!(refusal["code"], "invalid-worktree-id", "{refusal}");
        let message = refusal["message"].as_str().unwrap();
        assert!(message.contains("nothing-here.1"), "{refusal}");
        assert!(
            message.contains(
                "neither a valid worktree id nor a registered tree path or directory name"
            ),
            "{refusal}"
        );
    }
    // The tree argument keeps its old wire code for a value that names nothing.
    for args in [
        &["finish", "nothing-here"][..],
        &["discard-cache", "--dry-run", "nothing-here"][..],
        &["archive", "nothing-here"][..],
        &["finish", "/no/such/tree"][..],
    ] {
        let refusal = fixture.refused(args);
        assert_eq!(refusal["code"], "worktree-not-found", "{refusal}");
        let message = refusal["message"].as_str().unwrap();
        assert!(message.contains(args[args.len() - 1]), "{refusal}");
        assert!(
            message.contains("neither an existing path nor a registered id or tree directory name"),
            "{refusal}"
        );
    }
}

#[test]
fn reconcile_apply_accepts_a_path_and_still_requires_a_selection() {
    let fixture = Fixture::new();
    let tree = fixture.create("epsilon");
    fixture.ok(&["finish", "epsilon"]);

    let refusal = fixture.refused(&["reconcile", "--repo", fixture.repo(), "--apply"]);
    assert_eq!(refusal["code"], "operation-failed", "{refusal}");
    let refusal = fixture.refused(&["gc", "--repo", fixture.repo(), "--apply"]);
    assert_eq!(refusal["code"], "operation-failed", "{refusal}");

    // A record that is no reconciliation candidate is refused by id and by path alike.
    for reference in ["epsilon", tree.to_str().unwrap()] {
        let refusal = fixture.refused(&[
            "reconcile",
            "--repo",
            fixture.repo(),
            "--apply",
            "--id",
            reference,
        ]);
        assert_eq!(
            refusal["code"], "selected-worktree-not-reconciliation-candidate",
            "{refusal}"
        );
    }
}

#[test]
fn the_skill_says_one_reference_works_everywhere() {
    let fixture = Fixture::new();
    let skill = fixture.human(&fixture.outside(), &["skill"]);
    assert!(
        skill.contains(
            "One tree reference works in `finish`, `discard-cache`, `archive`, `gc --id` and `reconcile --id`"
        ),
        "{skill}"
    );
}

// Adversary cases (6f9f5ae).

#[test]
fn finish_by_id_is_not_shadowed_by_an_unrelated_directory_of_that_name() {
    let fixture = Fixture::new();
    let tree = fixture.create("docs");
    // An ordinary working directory that happens to hold a plain `docs/` directory, such as a
    // repository root. `docs` is a registered id and names no registered path from here.
    let cwd = fixture.outside();
    std::fs::create_dir_all(cwd.join("docs")).unwrap();

    // gc resolves the same reference from the same directory as the id.
    let review = fixture.ok_in(
        &cwd,
        &["gc", "--repo", fixture.repo(), "--dry-run", "--id", "docs"],
    );
    assert_eq!(review["assessments"], serde_json::json!([]), "{review}");

    let output = fixture.run(&cwd, true, &["finish", "docs"]);
    assert!(
        output.status.success(),
        "finish <registered id> must finish that tree: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let finished: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(finished["evidence"]["id"], "docs");
    assert_eq!(finished["evidence"]["path"], tree.to_str().unwrap());
}

#[test]
fn gc_id_naming_no_record_keeps_the_released_unknown_worktree_id_code() {
    let fixture = Fixture::new();
    fixture.create("present");
    // 0.12.1 refused a well-formed but unregistered `--id` as `unknown-worktree-id`
    // (validate_selected_records); protocol version 4 refusal codes are a wire contract.
    for command in ["gc", "reconcile"] {
        let refusal = fixture.refused(&[
            command,
            "--repo",
            fixture.repo(),
            "--dry-run",
            "--id",
            "absent",
        ]);
        assert_eq!(
            refusal["code"], "unknown-worktree-id",
            "{command}: {refusal}"
        );
    }
}

#[test]
fn finish_and_discard_cache_by_id_from_outside_keep_the_lease_check() {
    let fixture = Fixture::new();
    let tree = fixture.create("leased");
    fixture.ok(&[
        "hook",
        "session-start",
        "--path",
        tree.to_str().unwrap(),
        "--session",
        "other-session",
    ]);
    let refusal = fixture.refused(&["finish", "leased"]);
    assert_eq!(refusal["code"], "live-session", "{refusal}");
    let refusal = fixture.refused(&["finish", "--discard-cache", "--archive", "leased"]);
    assert_eq!(refusal["code"], "live-session", "{refusal}");
    let refusal = fixture.refused(&["discard-cache", "leased"]);
    assert_eq!(refusal["code"], "live-session", "{refusal}");
    let status = fixture.ok(&["status"]);
    assert_eq!(status["records"][0]["lifecycle"], "active", "{status}");
}

#[test]
fn gc_apply_by_reference_never_reaches_a_tree_outside_the_workspace() {
    let fixture = Fixture::new();
    fixture.create("inside");
    // A second workspace with its own repository and a finished tree named `stranger`.
    let other_workspace = fixture.root.path().join("second");
    let other = other_workspace.join("two");
    std::fs::create_dir_all(&other).unwrap();
    git(&other, &["init", "-b", "main"]);
    write(&other.join("file"), "x\n");
    git(&other, &["add", "."]);
    git(&other, &["commit", "-m", "two"]);
    let profile = fixture.root.path().join("profile-two.toml");
    write(
        &profile,
        "version = 1\nname = 'second'\nexpire_after_seconds = 604800\nprotect_workspace_root = false\n",
    );
    fixture.ok(&[
        "activate",
        "--profile",
        profile.to_str().unwrap(),
        "--workspace",
        other_workspace.to_str().unwrap(),
    ]);
    let created = fixture.ok(&[
        "create",
        "--repo",
        other.to_str().unwrap(),
        "--id",
        "stranger",
        "--purpose",
        "x",
    ]);
    let stranger = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
    fixture.ok(&["finish", "stranger"]);

    for reference in ["stranger", stranger.to_str().unwrap()] {
        let refusal =
            fixture.refused(&["gc", "--repo", fixture.repo(), "--apply", "--id", reference]);
        assert_eq!(
            refusal["code"], "selected-worktree-outside-policy",
            "{reference}: {refusal}"
        );
    }
    // From the tree's parent, the bare directory name is a path to it.
    let refusal = fixture.refused_in(
        stranger.parent().unwrap(),
        &[
            "gc",
            "--repo",
            fixture.repo(),
            "--apply",
            "--id",
            "stranger/",
        ],
    );
    assert_eq!(
        refusal["code"], "selected-worktree-outside-policy",
        "{refusal}"
    );
    assert!(stranger.exists());
}
