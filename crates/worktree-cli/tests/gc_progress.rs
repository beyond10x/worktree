//! `gc` reports each assessment on standard error as it finishes; standard output keeps the
//! protocol 4 envelope, written once at the end.

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

/// One repository under an activated profile with three finished trees; the third holds a commit
/// no remote has, so its assessment is a refusal.
struct Fixture {
    root: tempfile::TempDir,
    repository: PathBuf,
}

const TREES: [&str; 3] = ["tree-a", "tree-b", "tree-c"];

impl Fixture {
    fn new() -> Self {
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
        let fixture = Self { root, repository };
        fixture.ok(&[
            "activate",
            "--profile",
            profile.to_str().unwrap(),
            "--workspace",
            workspace.to_str().unwrap(),
        ]);
        for id in TREES {
            let created = fixture.ok(&[
                "create",
                "--repo",
                fixture.repository.to_str().unwrap(),
                "--id",
                id,
                "--purpose",
                "gc progress",
            ]);
            if id == "tree-c" {
                let tree = PathBuf::from(created["evidence"]["path"].as_str().unwrap());
                write(&tree.join("unpublished"), "local only\n");
                git(&tree, &["add", "."]);
                git(&tree, &["commit", "-m", "unpublished"]);
            }
            fixture.ok(&["finish", id]);
        }
        fixture
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

    fn dry_run(&self, json: bool, extra: &[&str]) -> Output {
        let mut args = vec![
            "gc",
            "--repo",
            self.repository.to_str().unwrap(),
            "--dry-run",
        ];
        args.extend_from_slice(extra);
        let output = self.command(json, &args);
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output
    }
}

#[test]
fn a_json_dry_run_writes_one_progress_object_per_assessment_to_standard_error() {
    let fixture = Fixture::new();

    let output = fixture.dry_run(true, &[]);

    // Standard output is the protocol 4 envelope, exactly one JSON document.
    let report: Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["version"], 4, "{report}");
    assert_eq!(report["ok"], true, "{report}");
    let mut keys: Vec<&str> = report
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["assessments", "ok", "version"], "{report}");
    let assessments = report["assessments"].as_array().unwrap();
    assert_eq!(assessments.len(), TREES.len(), "{report}");

    // Standard error holds one JSON object per assessment, in the order assessed, which is the
    // order of the report.
    let stderr = String::from_utf8(output.stderr).unwrap();
    let lines: Vec<Value> = stderr
        .lines()
        .map(|line| serde_json::from_str(line).unwrap_or_else(|error| panic!("{line}: {error}")))
        .collect();
    assert_eq!(lines.len(), TREES.len(), "{stderr}");
    for (index, (line, assessment)) in lines.iter().zip(assessments).enumerate() {
        assert_eq!(line["progress"], "gc-assessment", "{line}");
        assert_eq!(line["id"], assessment["record"]["id"], "{line}");
        assert_eq!(line["eligible"], assessment["eligible"], "{line}");
        assert_eq!(line["done"], index + 1, "{line}");
        assert_eq!(line["of"], TREES.len(), "{line}");
        // A progress line is never mistaken for the error envelope.
        assert!(line.get("ok").is_none(), "{line}");
        match assessment["refusal"]["code"].as_str() {
            Some(code) => assert_eq!(line["code"], code, "{line}"),
            None => assert!(line.get("code").is_none(), "{line}"),
        }
    }
    let refused: Vec<&Value> = lines
        .iter()
        .filter(|line| line["eligible"] == false)
        .collect();
    assert_eq!(refused.len(), 1, "{stderr}");
    assert_eq!(refused[0]["id"], "tree-c");
}

#[test]
fn a_plain_dry_run_writes_one_progress_line_per_assessment_to_standard_error() {
    let fixture = Fixture::new();

    let output = fixture.dry_run(false, &[]);

    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.lines().count(), TREES.len(), "{stdout}");
    let stderr = String::from_utf8(output.stderr).unwrap();
    let lines: Vec<&str> = stderr.lines().collect();
    assert_eq!(lines.len(), TREES.len(), "{stderr}");
    for (index, line) in lines.iter().enumerate() {
        assert!(
            line.starts_with(&format!("assessed {}/{} ", index + 1, TREES.len())),
            "{line}"
        );
    }
    assert!(
        lines
            .iter()
            .any(|line| line.contains("tree-c") && line.contains("retained: ")),
        "{stderr}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.ends_with("\teligible"))
            .count(),
        2,
        "{stderr}"
    );
}

#[test]
fn a_selected_dry_run_reports_only_the_selected_records() {
    let fixture = Fixture::new();

    let output = fixture.dry_run(true, &["--id", "tree-b"]);

    let stderr = String::from_utf8(output.stderr).unwrap();
    let lines: Vec<Value> = stderr
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 1, "{stderr}");
    assert_eq!(lines[0]["id"], "tree-b");
    assert_eq!(lines[0]["done"], 1);
    assert_eq!(lines[0]["of"], 1);
}
