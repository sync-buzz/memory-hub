#![allow(clippy::expect_used)]

use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const SUCCESS: i32 = 0;
const USAGE: i32 = 2;
const DOCTOR_FAILED: i32 = 10;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_git-memory"))
}

fn run(args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .output()
        .expect("git-memory should start")
}

fn run_in(directory: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(directory)
        .output()
        .expect("git-memory should start")
}

fn assert_exit(output: &Output, expected: i32) {
    assert_eq!(
        output.status.code(),
        Some(expected),
        "stdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn init_empty_repository() -> TempDir {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let output = Command::new("git")
        .args(["init", "--quiet"])
        .arg(directory.path())
        .output()
        .expect("Git should start");
    assert_exit(&output, SUCCESS);
    directory
}

fn commit_file(repository: &Path, content: &str) {
    fs::write(repository.join("code.txt"), content).expect("fixture should write");
    let add = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args(["add", "code.txt"])
        .output()
        .expect("Git add should start");
    assert_exit(&add, SUCCESS);
    let commit = Command::new("git")
        .arg("-C")
        .arg(repository)
        .args([
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--quiet",
            "-m",
            content,
        ])
        .output()
        .expect("Git commit should start");
    assert_exit(&commit, SUCCESS);
}

#[test]
fn help_and_version_are_stable() {
    let help = run(&["--help"]);
    assert_exit(&help, SUCCESS);
    let help = String::from_utf8_lossy(&help.stdout);
    assert!(help.contains("Usage: git memory <COMMAND>"));
    assert!(help.contains("doctor"));

    let version = run(&["--version"]);
    assert_exit(&version, SUCCESS);
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("git-memory {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn invalid_invocation_has_usage_exit_code() {
    let output = run(&["not-a-command"]);
    assert_exit(&output, USAGE);
    assert!(String::from_utf8_lossy(&output.stderr).contains("unrecognized subcommand"));
}

#[test]
fn doctor_succeeds_in_an_empty_git_repository() {
    let repository = init_empty_repository();
    let output = run(&[
        "doctor",
        "--project",
        repository.path().to_str().expect("temporary path is UTF-8"),
    ]);
    assert_exit(&output, SUCCESS);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("[ok] git.repository"));
    assert!(stdout.contains("Result: healthy"));
}

#[test]
fn doctor_uses_the_current_directory_by_default() {
    let repository = init_empty_repository();
    let output = run_in(repository.path(), &["doctor", "--output", "json"]);
    assert_exit(&output, SUCCESS);
    let report: Value = serde_json::from_slice(&output.stdout).expect("valid JSON report");
    assert_eq!(report["status"], "ok");
}

#[test]
fn doctor_accepts_an_empty_bare_repository() {
    let repository = tempfile::tempdir().expect("temporary directory should be created");
    let init = Command::new("git")
        .args(["init", "--bare", "--quiet"])
        .arg(repository.path())
        .output()
        .expect("Git should start");
    assert_exit(&init, SUCCESS);

    let output = run(&[
        "doctor",
        "--project",
        repository.path().to_str().expect("temporary path is UTF-8"),
    ]);
    assert_exit(&output, SUCCESS);
    assert!(String::from_utf8_lossy(&output.stdout).contains("Result: healthy"));
}

#[test]
fn doctor_json_has_a_versioned_machine_readable_shape() {
    let repository = init_empty_repository();
    let output = run(&[
        "doctor",
        "--project",
        repository.path().to_str().expect("temporary path is UTF-8"),
        "--output",
        "json",
    ]);
    assert_exit(&output, SUCCESS);
    let report: Value = serde_json::from_slice(&output.stdout).expect("valid JSON report");
    assert_eq!(report["schema_version"], 1);
    assert_eq!(report["status"], "ok");
    assert_eq!(report["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(report["checks"][2]["id"], "git.repository");
    assert!(report["checks"][2]["data"]["git_dir"].is_string());
}

#[test]
fn cli_calls_reconcile_code_history_without_hooks() {
    let repository = init_empty_repository();
    commit_file(repository.path(), "one");
    let doctor = run(&[
        "doctor",
        "--project",
        repository.path().to_str().expect("temporary path is UTF-8"),
        "--output",
        "json",
    ]);
    assert_exit(&doctor, SUCCESS);
    let report: Value = serde_json::from_slice(&doctor.stdout).expect("valid JSON report");
    assert_eq!(report["checks"][3]["id"], "memory.reconciliation");
    assert!(
        repository
            .path()
            .join(".git/git-memory/reconcile-cursor.json")
            .is_file()
    );

    commit_file(repository.path(), "two");
    let reconcile = run(&[
        "reconcile",
        "--project",
        repository.path().to_str().expect("temporary path is UTF-8"),
        "--output",
        "json",
    ]);
    assert_exit(&reconcile, SUCCESS);
    let report: Value = serde_json::from_slice(&reconcile.stdout).expect("valid JSON report");
    assert_eq!(report["processed"].as_array().map(Vec::len), Some(1));
}

#[test]
fn doctor_failure_has_stable_code_and_kind() {
    let directory = tempfile::tempdir().expect("temporary directory should be created");
    let output = run(&[
        "doctor",
        "--project",
        directory.path().to_str().expect("temporary path is UTF-8"),
        "--output",
        "json",
    ]);
    assert_exit(&output, DOCTOR_FAILED);
    let report: Value = serde_json::from_slice(&output.stdout).expect("valid JSON report");
    assert_eq!(report["status"], "error");
    assert_eq!(report["checks"][2]["kind"], "not_a_git_repository");
}

#[test]
fn doctor_reports_an_unavailable_git_executable_without_panicking() {
    let empty_path = tempfile::tempdir().expect("temporary directory should be created");
    let output = Command::new(binary())
        .args(["doctor", "--output", "json"])
        .env("PATH", empty_path.path())
        .output()
        .expect("git-memory should start");
    assert_exit(&output, DOCTOR_FAILED);

    let report: Value = serde_json::from_slice(&output.stdout).expect("valid JSON report");
    assert_eq!(report["checks"][1]["kind"], "git_unavailable");
    assert_eq!(report["checks"][2]["kind"], "repository_check_skipped");
}

#[test]
fn git_dispatches_git_memory_to_the_binary() {
    let path_directory = tempfile::tempdir().expect("temporary directory should be created");
    let executable_name = if cfg!(windows) {
        "git-memory.exe"
    } else {
        "git-memory"
    };
    fs::copy(binary(), path_directory.path().join(executable_name))
        .expect("test binary should be copied onto PATH");

    let path = prepend_path(path_directory.path());
    let output = Command::new("git")
        .args(["memory", "--version"])
        .env("PATH", path)
        .output()
        .expect("Git should dispatch the subcommand");
    assert_exit(&output, SUCCESS);
    assert!(String::from_utf8_lossy(&output.stdout).starts_with("git-memory "));
}

fn prepend_path(directory: &Path) -> OsString {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut paths = vec![directory.to_path_buf()];
    paths.extend(std::env::split_paths(&existing));
    std::env::join_paths(paths).expect("PATH components should be valid")
}
