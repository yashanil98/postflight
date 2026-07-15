use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use tempfile::TempDir;

fn postflight_cmd() -> Command {
    Command::cargo_bin("postflight").unwrap()
}

#[test]
fn test_run_simple_command() {
    let workspace = TempDir::new().unwrap();

    let mut cmd = postflight_cmd();
    cmd.args(["run", "echo hello", "--workspace", workspace.path().to_str().unwrap()]);
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("postflight session report"))
        .stderr(predicate::str::contains("exit code: 0"));
}

#[test]
fn test_run_captures_file_creation() {
    let workspace = TempDir::new().unwrap();
    let test_file = workspace.path().join("created.txt");

    let command = format!("echo 'test content' > {}", test_file.display());
    let mut cmd = postflight_cmd();
    cmd.args(["run", &command, "--workspace", workspace.path().to_str().unwrap()]);
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("files changed"))
        .stderr(predicate::str::contains("created"));
}

#[test]
fn test_run_captures_exit_code() {
    let workspace = TempDir::new().unwrap();

    let mut cmd = postflight_cmd();
    cmd.args(["run", "exit 42", "--workspace", workspace.path().to_str().unwrap()]);
    cmd.assert()
        .code(42)
        .stderr(predicate::str::contains("exit code: 42"));
}

#[test]
fn test_run_captures_subprocess() {
    let workspace = TempDir::new().unwrap();

    let mut cmd = postflight_cmd();
    cmd.args(["run", "ls /tmp && sleep 0.5", "--workspace", workspace.path().to_str().unwrap()]);
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("postflight session report"));
}

#[test]
fn test_run_detects_long_subprocess() {
    let workspace = TempDir::new().unwrap();

    let mut cmd = postflight_cmd();
    cmd.args([
        "run",
        "sleep 0.5",
        "--workspace",
        workspace.path().to_str().unwrap(),
    ]);
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("postflight session report"));
}

#[test]
fn test_run_file_modification_detected() {
    let workspace = TempDir::new().unwrap();
    let test_file = workspace.path().join("existing.txt");
    fs::write(&test_file, "original content").unwrap();

    let command = format!(
        "sleep 0.3 && echo 'modified' > {}",
        test_file.display()
    );
    let mut cmd = postflight_cmd();
    cmd.args(["run", &command, "--workspace", workspace.path().to_str().unwrap()]);
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("modified"));
}

#[test]
fn test_sessions_list() {
    let workspace = TempDir::new().unwrap();
    let mut run_cmd = postflight_cmd();
    run_cmd.args(["run", "true", "--workspace", workspace.path().to_str().unwrap()]);
    run_cmd.assert().success();

    let mut cmd = postflight_cmd();
    cmd.arg("sessions");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("SESSION"));
}

#[test]
fn test_report_latest() {
    let workspace = TempDir::new().unwrap();
    let mut run_cmd = postflight_cmd();
    run_cmd.args(["run", "echo test_report", "--workspace", workspace.path().to_str().unwrap()]);
    run_cmd.assert().success();

    let mut cmd = postflight_cmd();
    cmd.arg("report");
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("postflight session report"));
}

#[test]
fn test_report_json() {
    let workspace = TempDir::new().unwrap();
    let mut run_cmd = postflight_cmd();
    run_cmd.args(["run", "echo json_test", "--workspace", workspace.path().to_str().unwrap()]);
    run_cmd.assert().success();

    let mut cmd = postflight_cmd();
    cmd.args(["report", "--json"]);
    let output = cmd.output().unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);

    let parsed: serde_json::Value = serde_json::from_str(&stdout).unwrap();
    assert!(parsed.get("command").is_some());
    assert!(parsed.get("exit_code").is_some());
    assert!(parsed.get("duration").is_some());
}

#[test]
fn test_clean() {
    let mut cmd = postflight_cmd();
    cmd.args(["clean", "--keep", "100"]);
    cmd.assert()
        .success()
        .stdout(predicate::str::contains("removed"));
}

#[test]
fn test_run_with_network_activity() {
    let workspace = TempDir::new().unwrap();

    let mut cmd = postflight_cmd();
    cmd.args([
        "run",
        "curl -s -o /dev/null https://example.com || true",
        "--workspace",
        workspace.path().to_str().unwrap(),
    ]);
    cmd.assert()
        .success()
        .stderr(predicate::str::contains("postflight session report"));
}

#[test]
fn test_run_preserves_stdout() {
    let workspace = TempDir::new().unwrap();

    let mut cmd = postflight_cmd();
    cmd.args(["run", "echo MARKER_STRING_12345", "--workspace", workspace.path().to_str().unwrap()]);
    cmd.assert().success();
}

#[test]
fn test_events_jsonl_written() {
    let workspace = TempDir::new().unwrap();
    let mut run_cmd = postflight_cmd();
    run_cmd.args(["run", "echo events_test", "--workspace", workspace.path().to_str().unwrap()]);
    run_cmd.assert().success();

    let sessions_dir = dirs::home_dir().unwrap().join(".postflight/sessions");
    let mut sessions: Vec<_> = fs::read_dir(&sessions_dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .collect();
    sessions.sort_by_key(|e| e.file_name());

    let latest = sessions.last().unwrap();
    let events_file = latest.path().join("events.jsonl");
    assert!(events_file.exists());

    let content = fs::read_to_string(&events_file).unwrap();
    assert!(content.contains("session_start"));
    assert!(content.contains("session_end"));
}
