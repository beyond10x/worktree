//! Adversary cases for `gc` progress on standard error (#45): tree content in a plain line, the
//! planning pass of `--apply`, zero candidates and a refusal before any assessment.

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

/// One repository under an activated profile; `finished` trees are created and finished, `tree-c`
/// among them holding a commit no remote has.
struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
    trees: Vec<(String, PathBuf)>,
}

impl Fixture {
    fn new(finished: &[&str]) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let repository = workspace.join("alpha");
        std::fs::create_dir_all(&repository).unwrap();
        git(&repository, &["init", "-b", "main"]);
        write(&repository.join("source"), "original\n");
        git(&repository, &["add", "."]);
        git(&repository, &["commit", "-m", "fixture"]);
        let remote = root.path().join("alpha.git");
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
            trees: Vec::new(),
        };
        fixture.ok(&[
            "activate",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ]);
        for id in finished {
            let created = fixture.ok(&[
                "create",
                "--repo",
                fixture.repository.to_str().unwrap(),
                "--id",
                id,
                "--purpose",
                "gc progress adversary",
            ]);
            let tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
            if *id == "tree-c" {
                write(&tree.join("unpublished"), "local only\n");
                git(&tree, &["add", "."]);
                git(&tree, &["commit", "-m", "unpublished"]);
            }
            fixture.ok(&["finish", id]);
            fixture.trees.push(((*id).to_owned(), tree));
        }
        fixture
    }

    fn tree(&self, id: &str) -> &Path {
        &self.trees.iter().find(|(name, _)| name == id).unwrap().1
    }

    fn command(&self, json: bool, args: &[&str]) -> Output {
        let outside = self.root.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let mut command = Command::new(env!("CARGO_BIN_EXE_worktree"));
        if json {
            command.arg("--json");
        }
        command
            .args(args)
            .current_dir(outside)
            .env("XDG_CONFIG_HOME", self.root.path().join("config"))
            .env("XDG_STATE_HOME", self.root.path().join("state"))
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let output = self.command(true, args);
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).unwrap()
    }

    fn gc(&self, json: bool, extra: &[&str]) -> Output {
        let mut args = vec!["gc", "--repo", self.repository.to_str().unwrap()];
        args.extend_from_slice(extra);
        self.command(json, &args)
    }
}

fn progress_lines(output: &Output) -> Vec<Value> {
    String::from_utf8(output.stderr.clone())
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{line}: {error}")))
        .collect()
}

/// A directory name inside a tree is repository content anybody can produce. Quoted into a plain
/// progress line it must not carry terminal control sequences that rewrite earlier lines.
#[test]
fn a_plain_progress_line_carries_no_control_characters_from_tree_content() {
    let fixture = Fixture::new(&["tree-a", "tree-b"]);
    // ESC [1A ESC [2K moves the cursor up and erases the previous verdict; VT and FF break the
    // line on a terminal without a newline byte.
    let hostile = "x\u{1b}[1A\u{1b}[2K\u{0b}\u{0c}y";
    let nested = fixture.tree("tree-b").join(hostile).join(".git");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(nested.join("data.txt"), "only copy\n").unwrap();

    let output = fixture.gc(false, &["--dry-run"]);
    assert!(output.status.success(), "{output:?}");

    let stderr = String::from_utf8(output.stderr).unwrap();
    assert_eq!(stderr.lines().count(), 2, "{stderr:?}");
    let retained = stderr.lines().find(|line| line.contains("tree-b")).unwrap();
    assert!(retained.contains("worktree-hidden-state"), "{retained:?}");
    let controls: Vec<char> = retained
        .chars()
        .filter(|character| character.is_control() && *character != '\t')
        .collect();
    assert!(
        controls.is_empty(),
        "progress line carries control characters {controls:?}: {retained:?}"
    );
}

/// The planning pass of `--apply` reports each reviewed id once; standard output is the applied
/// report and only the eligible reviewed id is removed.
#[test]
fn apply_reports_the_planning_pass_and_removes_only_the_eligible_reviewed_id() {
    let fixture = Fixture::new(&["tree-a", "tree-b", "tree-c"]);

    let output = fixture.gc(true, &["--apply", "--id", "tree-a", "--id", "tree-c"]);
    assert!(output.status.success(), "{output:?}");

    let lines = progress_lines(&output);
    let ids: Vec<&str> = lines
        .iter()
        .map(|line| line["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["tree-a", "tree-c"], "{lines:?}");
    assert_eq!(lines[0]["eligible"], true);
    assert_eq!(lines[1]["eligible"], false);
    assert_eq!(lines[1]["code"], "no-remote-recovery-proof", "{lines:?}");
    for (index, line) in lines.iter().enumerate() {
        assert_eq!(line["done"], index + 1);
        assert_eq!(line["of"], 2);
    }

    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["version"], 4);
    let assessments = report["assessments"].as_array().unwrap();
    assert_eq!(assessments.len(), 2, "{report}");
    assert!(assessments[0]["evidence"].is_object(), "{report}");
    assert!(assessments[1]["evidence"].is_null(), "{report}");
    assert!(!fixture.tree("tree-a").exists());
    assert!(fixture.tree("tree-b").exists());
    assert!(fixture.tree("tree-c").exists());
}

/// Zero candidates: nothing on standard error, the empty report on standard output.
#[test]
fn zero_candidates_write_nothing_to_standard_error() {
    let fixture = Fixture::new(&[]);

    let json = fixture.gc(true, &["--dry-run"]);
    assert!(json.status.success(), "{json:?}");
    assert_eq!(json.stderr, [] as [u8; 0]);
    let report: Value = serde_json::from_slice(&json.stdout).unwrap();
    assert_eq!(report["assessments"], serde_json::json!([]), "{report}");

    let plain = fixture.gc(false, &["--dry-run"]);
    assert!(plain.status.success(), "{plain:?}");
    assert_eq!(plain.stderr, [] as [u8; 0]);
    assert_eq!(plain.stdout, b"no cleanup candidates\n");
}

/// A refusal before any assessment leaves standard error one error envelope, no progress line.
#[test]
fn an_unknown_id_leaves_standard_error_one_error_envelope() {
    let fixture = Fixture::new(&["tree-a"]);

    let output = fixture.gc(true, &["--dry-run", "--id", "tree-z"]);
    assert!(!output.status.success(), "{output:?}");
    assert_eq!(output.stdout, [] as [u8; 0]);
    let error: Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["ok"], false, "{error}");
    assert!(error.get("progress").is_none(), "{error}");
}
