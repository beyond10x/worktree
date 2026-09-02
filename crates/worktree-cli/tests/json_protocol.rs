//! Process-level checks for the versioned JSON command protocol.

use std::process::Command;

#[test]
fn ordinary_json_success_uses_cli_protocol_envelope() {
    let output_root = tempfile::tempdir().expect("temporary output directory");
    let skill = output_root.path().join("worktree");
    let output = Command::new(env!("CARGO_BIN_EXE_worktree"))
        .args(["--json", "skill", "--out"])
        .arg(&skill)
        .output()
        .expect("run worktree CLI");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    let value: serde_json::Value = serde_json::from_str(stdout.trim()).expect("one JSON document");
    assert_eq!(value["version"], 2);
    assert_eq!(value["ok"], true);
    assert_eq!(value["check"], false);
    assert_eq!(value["path"], skill.display().to_string());
}

#[test]
fn operational_json_error_is_exactly_one_document() {
    let output = Command::new(env!("CARGO_BIN_EXE_worktree"))
        .args(["--json", "gc", "--apply"])
        .output()
        .expect("run worktree CLI");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    let value: serde_json::Value = serde_json::from_str(stderr.trim()).expect("one JSON document");
    assert_eq!(value["version"], 2);
    assert_eq!(value["ok"], false);
    assert_eq!(value["code"], "operation-failed");
    assert!(value["message"].as_str().unwrap().contains("reviewed --id"));
    assert!(!stderr.contains("Error:"));
}

#[test]
fn argument_json_error_is_exactly_one_document() {
    let output = Command::new(env!("CARGO_BIN_EXE_worktree"))
        .args(["--json", "--not-a-real-option"])
        .output()
        .expect("run worktree CLI");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    let value: serde_json::Value = serde_json::from_str(stderr.trim()).expect("one JSON document");
    assert_eq!(value["version"], 2);
    assert_eq!(value["ok"], false);
    assert_eq!(value["code"], "invalid-arguments");
}

#[test]
fn json_help_remains_a_successful_clap_display() {
    let output = Command::new(env!("CARGO_BIN_EXE_worktree"))
        .args(["--json", "--help"])
        .output()
        .expect("run worktree CLI");

    assert!(output.status.success());
    assert!(output.stderr.is_empty());
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("Usage:"));
    assert!(stdout.contains("--json"));
}

#[test]
fn hook_json_error_uses_hook_protocol_version() {
    let state = tempfile::tempdir().expect("temporary state directory");
    let output = Command::new(env!("CARGO_BIN_EXE_worktree"))
        .args([
            "--json",
            "hook",
            "session-start",
            "--path",
            "/definitely-not-a-managed-worktree",
            "--session",
            "test",
        ])
        .env("XDG_STATE_HOME", state.path())
        .output()
        .expect("run worktree CLI");

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    let value: serde_json::Value = serde_json::from_str(stderr.trim()).expect("one JSON document");
    assert_eq!(value["version"], 1);
    assert_eq!(value["ok"], false);
}
