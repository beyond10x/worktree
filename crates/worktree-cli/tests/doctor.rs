//! Process-level checks for `worktree doctor` readiness reporting.

use std::path::Path;
use std::process::{Command, Output};

fn doctor(config: &Path, state: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_worktree"))
        .args(args)
        .env("XDG_CONFIG_HOME", config)
        .env("XDG_STATE_HOME", state)
        .output()
        .expect("run worktree CLI")
}

#[test]
fn doctor_check_fails_and_names_missing_active_profile() {
    let config = tempfile::tempdir().expect("temporary config directory");
    let state = tempfile::tempdir().expect("temporary state directory");

    let output = doctor(config.path(), state.path(), &["doctor", "--check"]);

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    assert!(stderr.contains("no active profile"), "stderr: {stderr}");
}

#[test]
fn doctor_check_json_failure_names_missing_active_profile() {
    let config = tempfile::tempdir().expect("temporary config directory");
    let state = tempfile::tempdir().expect("temporary state directory");

    let output = doctor(
        config.path(),
        state.path(),
        &["--json", "doctor", "--check"],
    );

    assert!(!output.status.success());
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("UTF-8 stderr");
    let value: serde_json::Value = serde_json::from_str(stderr.trim()).expect("one JSON document");
    assert_eq!(value["version"], 3);
    assert_eq!(value["ok"], false);
    assert!(
        value["message"]
            .as_str()
            .unwrap()
            .contains("no active profile")
    );
}

#[test]
fn doctor_without_check_still_reports_zero_profiles_successfully() {
    let config = tempfile::tempdir().expect("temporary config directory");
    let state = tempfile::tempdir().expect("temporary state directory");

    let text = doctor(config.path(), state.path(), &["doctor"]);
    assert!(text.status.success());
    let stdout = String::from_utf8(text.stdout).expect("UTF-8 stdout");
    assert!(stdout.contains("profiles=0"), "stdout: {stdout}");

    let json = doctor(config.path(), state.path(), &["--json", "doctor"]);
    assert!(json.status.success());
    let value: serde_json::Value = serde_json::from_slice(&json.stdout).expect("one JSON document");
    assert_eq!(value["version"], 3);
    assert_eq!(value["profiles"], 0);
    assert_eq!(value["errors"], serde_json::json!([]));
}

#[test]
fn doctor_check_passes_with_an_active_profile() {
    let config = tempfile::tempdir().expect("temporary config directory");
    let state = tempfile::tempdir().expect("temporary state directory");
    let workspace = tempfile::tempdir().expect("temporary workspace directory");
    let profile = workspace.path().join("profile.toml");
    std::fs::write(
        &profile,
        "version = 1\nname = \"default\"\nexpire_after_seconds = 604800\nprotect_workspace_root = false\n",
    )
    .expect("write profile");

    let activated = Command::new(env!("CARGO_BIN_EXE_worktree"))
        .arg("activate")
        .arg("--profile")
        .arg(&profile)
        .arg("--workspace")
        .arg(workspace.path())
        .env("XDG_CONFIG_HOME", config.path())
        .env("XDG_STATE_HOME", state.path())
        .output()
        .expect("run worktree CLI");
    assert!(
        activated.status.success(),
        "activate: {}",
        String::from_utf8_lossy(&activated.stderr)
    );

    let output = doctor(config.path(), state.path(), &["doctor", "--check"]);

    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert_eq!(
        stdout.trim(),
        "git=true config=true registry=true profiles=1"
    );
}
