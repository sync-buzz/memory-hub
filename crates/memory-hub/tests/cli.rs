#![allow(clippy::expect_used, clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;
use tempfile::TempDir;

const SUCCESS: i32 = 0;
const USAGE: i32 = 2;
const DOCTOR_FAILED: i32 = 10;

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_memory-hub"))
}

fn run(args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .output()
        .expect("memory-hub should start")
}

fn run_in(directory: &Path, args: &[&str]) -> Output {
    Command::new(binary())
        .args(args)
        .current_dir(directory)
        .output()
        .expect("memory-hub should start")
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
    assert!(help.contains("Usage: memory-hub <COMMAND>"));
    assert!(help.contains("doctor"));

    let version = run(&["--version"]);
    assert_exit(&version, SUCCESS);
    assert_eq!(
        String::from_utf8_lossy(&version.stdout).trim(),
        format!("memory-hub {}", env!("CARGO_PKG_VERSION"))
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

/// Clone `origin` the way a colleague would — no memory refspec anywhere.
fn clone_of(origin: &Path) -> TempDir {
    let parent = tempfile::tempdir().expect("temporary directory should be created");
    let destination = parent.path().join("clone");
    let output = Command::new("git")
        .args(["clone", "--quiet"])
        .arg(origin)
        .arg(&destination)
        .output()
        .expect("Git clone should start");
    assert_exit(&output, SUCCESS);
    parent
}

fn clone_path(parent: &TempDir) -> PathBuf {
    parent.path().join("clone")
}

/// Write one record straight through the store, so a repository has memory
/// worth fetching without standing up an MCP session for it.
fn write_one_record(repository: &Path) {
    use memory_hub_core::{Envelope, StoredRecord};
    use memory_hub_store::{GitStore, Operation, Transaction};

    let store = GitStore::open(repository).expect("store should open");
    let expected_revision = store
        .current()
        .expect("current revision should be readable")
        .revision()
        .clone();
    store
        .apply(&Transaction {
            id: "cli-test-seed".into(),
            expected_revision,
            operations: vec![Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(
                    Envelope::new("seed", "note", "memory worth fetching").expect("envelope"),
                ),
            })],
        })
        .expect("the seed record should apply");
}

fn presence_check(report: &Value) -> &Value {
    let check = &report["checks"][3];
    assert_eq!(check["id"], "memory.presence");
    check
}

fn doctor_json(project: &Path, expected_exit: i32) -> Value {
    let output = run(&[
        "doctor",
        "--project",
        project.to_str().expect("temporary path is UTF-8"),
        "--output",
        "json",
    ]);
    assert_exit(&output, expected_exit);
    serde_json::from_slice(&output.stdout).expect("valid JSON report")
}

/// The whole point of GITMEMO-31: a fresh clone is empty because `git clone`
/// leaves `refs/memory/*` behind, not because the project has no memory —
/// and only the remote can tell those apart.
#[test]
fn doctor_names_the_fetch_when_memory_is_only_on_the_code_remote() {
    let origin = init_empty_repository();
    commit_file(origin.path(), "code");
    write_one_record(origin.path());

    let parent = clone_of(origin.path());
    let report = doctor_json(&clone_path(&parent), DOCTOR_FAILED);

    let check = presence_check(&report);
    assert_eq!(check["status"], "error");
    assert_eq!(check["kind"], "memory_not_fetched");
    let message = check["message"].as_str().expect("message is a string");
    assert!(
        message.contains("memory-hub remote add"),
        "an unconfigured clone is told to configure the remote first: {message}"
    );
    assert!(
        message.contains("memory-hub fetch"),
        "the message names the action that brings the memory here: {message}"
    );
}

/// With the memory remote already configured there is nothing left to set up,
/// so the message must not send the user through `remote add` again.
#[test]
fn doctor_names_only_the_fetch_when_the_memory_remote_is_configured() {
    let origin = init_empty_repository();
    commit_file(origin.path(), "code");
    write_one_record(origin.path());

    let parent = clone_of(origin.path());
    let clone = clone_path(&parent);
    let configure = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args(["config", "memory-hub.remote.url"])
        .arg(origin.path())
        .output()
        .expect("Git config should start");
    assert_exit(&configure, SUCCESS);

    let report = doctor_json(&clone, DOCTOR_FAILED);
    let check = presence_check(&report);
    assert_eq!(check["kind"], "memory_not_fetched");
    let message = check["message"].as_str().expect("message is a string");
    assert!(message.contains("memory-hub fetch"));
    assert!(
        !message.contains("memory-hub remote add"),
        "the remote is already configured: {message}"
    );
}

/// An empty memory on both sides is a normal state, not a failure — and the
/// message still says where memory comes from.
#[test]
fn doctor_stays_healthy_when_neither_side_has_memory() {
    let origin = init_empty_repository();
    commit_file(origin.path(), "code");

    let parent = clone_of(origin.path());
    let report = doctor_json(&clone_path(&parent), SUCCESS);

    let check = presence_check(&report);
    assert_eq!(check["status"], "ok");
    assert_eq!(check["kind"], "no_memory_anywhere");
}

/// `refs/memory/staged` is created by the first store that opens the
/// repository — including the previous `doctor` run. Counting refs instead of
/// records would make the second run claim the clone has memory.
#[test]
fn doctor_does_not_mistake_its_own_staged_ref_for_memory() {
    let origin = init_empty_repository();
    commit_file(origin.path(), "code");

    let parent = clone_of(origin.path());
    let clone = clone_path(&parent);
    let _ = doctor_json(&clone, SUCCESS);
    let report = doctor_json(&clone, SUCCESS);

    assert_eq!(presence_check(&report)["kind"], "no_memory_anywhere");
}

/// The advice has to be advice that works. Running exactly the two commands
/// the message names must leave the clone with the memory in it — otherwise
/// `doctor` is sending people down a path it never walked.
#[test]
fn the_actions_the_message_names_bring_the_memory_here() {
    let origin = init_empty_repository();
    commit_file(origin.path(), "code");
    write_one_record(origin.path());

    let parent = clone_of(origin.path());
    let clone = clone_path(&parent);
    let clone_arg = clone.to_str().expect("temporary path is UTF-8");
    let origin_arg = origin.path().to_str().expect("temporary path is UTF-8");

    let add = run(&["remote", "add", origin_arg, "--project", clone_arg]);
    assert_exit(&add, SUCCESS);

    // Verification is fail-closed on purpose, and an unsigned fixture has no
    // signer to trust. Opting out here keeps the test about discovery; the
    // signing gate has its own message and its own tests.
    let trust = Command::new("git")
        .arg("-C")
        .arg(&clone)
        .args(["config", "memory-hub.signing.verify", "off"])
        .output()
        .expect("Git config should start");
    assert_exit(&trust, SUCCESS);

    let fetch = run(&["fetch", "--project", clone_arg]);
    assert_exit(&fetch, SUCCESS);

    let report = doctor_json(&clone, SUCCESS);
    let check = presence_check(&report);
    assert_eq!(check["status"], "ok");
    assert!(
        check["message"]
            .as_str()
            .expect("message is a string")
            .contains("1 record(s)"),
        "the fetched record is now local: {}",
        check["message"]
    );
}

/// Memory that is here is reported as here, and the remote is never asked.
#[test]
fn doctor_reports_memory_that_is_present_locally() {
    let repository = init_empty_repository();
    commit_file(repository.path(), "code");
    write_one_record(repository.path());

    let report = doctor_json(repository.path(), SUCCESS);
    let check = presence_check(&report);
    assert_eq!(check["status"], "ok");
    assert!(check["kind"].is_null());
    assert!(
        check["message"]
            .as_str()
            .expect("message is a string")
            .contains("1 record(s)")
    );
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
    assert_eq!(report["checks"][4]["id"], "memory.reconciliation");
    // `doctor` only reports: reconciliation state is diagnosed, never advanced,
    // so no cursor exists until reconcile actually runs.
    let cursor = repository
        .path()
        .join(".git/memory-hub/reconcile-cursor.json");
    assert!(!cursor.is_file(), "doctor must not write the cursor");

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
    // First run: the cursor is initialized at HEAD in a single checkpoint.
    assert_eq!(report["processed"].as_array().map(Vec::len), Some(1));
    assert!(cursor.is_file(), "reconcile writes the cursor");

    // A second commit is then picked up incrementally.
    commit_file(repository.path(), "three");
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
        .expect("memory-hub should start");
    assert_exit(&output, DOCTOR_FAILED);

    let report: Value = serde_json::from_slice(&output.stdout).expect("valid JSON report");
    assert_eq!(report["checks"][1]["kind"], "git_unavailable");
    assert_eq!(report["checks"][2]["kind"], "repository_check_skipped");
}

// ---------------------------------------------------------------------------
// model subcommand
// ---------------------------------------------------------------------------

#[test]
fn model_list_succeeds_and_lists_all_registry_models() {
    let output = run(&["model", "list", "--output", "json"]);
    assert_exit(&output, SUCCESS);
    let rows: Value = serde_json::from_slice(&output.stdout).expect("valid JSON array");
    let arr = rows.as_array().expect("list output is an array");
    assert!(arr.len() >= 3, "registry should list at least 3 models");
    let ids: Vec<&str> = arr
        .iter()
        .map(|r| r["id"].as_str().expect("row has id"))
        .collect();
    assert!(ids.contains(&"bge-m3"));
    assert!(ids.contains(&"nomic-embed-text-v1.5"));
    assert!(ids.contains(&"bge-small-en-v1.5"));
}

#[test]
fn model_list_human_output_contains_table_headers() {
    let output = run(&["model", "list"]);
    assert_exit(&output, SUCCESS);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("bge-m3"));
    assert!(stdout.contains("nomic-embed-text-v1.5"));
}

#[test]
fn model_show_known_model_succeeds() {
    let output = run(&["model", "show", "bge-m3", "--output", "json"]);
    assert_exit(&output, SUCCESS);
    let detail: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(detail["id"], "bge-m3");
    assert_eq!(detail["dimensions"], 1024);
    assert_eq!(detail["quantisation"], "Q5_K_M");
    assert!(detail["backend"].is_string());
}

#[test]
fn model_show_unknown_model_returns_usage() {
    let output = run(&["model", "show", "nonexistent", "--output", "json"]);
    assert_exit(&output, USAGE);
    let error: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert!(error["error"].as_str().unwrap().contains("not found"));
}

#[test]
fn model_use_sets_active_model() {
    let config_dir = tempfile::tempdir().expect("config dir should be created");
    let output = Command::new(binary())
        .args(["model", "use", "bge-small-en-v1.5", "--output", "json"])
        .env("MEMORY_HUB_CONFIG_DIR", config_dir.path())
        .output()
        .expect("memory-hub should start");
    assert_exit(&output, SUCCESS);
    let result: Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    assert_eq!(result["active_model"], "bge-small-en-v1.5");
    assert!(result["on_disk"].is_boolean());

    let config_path = config_dir.path().join("config.json");
    let config: Value =
        serde_json::from_str(&fs::read_to_string(&config_path).expect("config file exists"))
            .expect("valid config JSON");
    assert_eq!(config["active_model"], "bge-small-en-v1.5");
}

#[test]
fn model_use_unknown_model_returns_usage() {
    let config_dir = tempfile::tempdir().expect("config dir should be created");
    let output = Command::new(binary())
        .args(["model", "use", "nonexistent"])
        .env("MEMORY_HUB_CONFIG_DIR", config_dir.path())
        .output()
        .expect("memory-hub should start");
    assert_exit(&output, USAGE);
}

#[test]
fn model_help_lists_all_subcommands() {
    let output = run(&["model", "--help"]);
    assert_exit(&output, SUCCESS);
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("download"));
    assert!(help.contains("list"));
    assert!(help.contains("show"));
    assert!(help.contains("use"));
    assert!(help.contains("benchmark"));
}

#[test]
fn doctor_includes_model_check() {
    let repository = init_empty_repository();
    let output = run(&[
        "doctor",
        "--project",
        repository.path().to_str().expect("path is UTF-8"),
        "--output",
        "json",
    ]);
    let report: Value = serde_json::from_slice(&output.stdout).expect("valid JSON report");
    let model_check = report["checks"]
        .as_array()
        .expect("checks is array")
        .iter()
        .find(|c| c["id"] == "memory.model")
        .expect("model check exists");
    assert!(model_check["status"].is_string());
    assert!(
        model_check["message"].as_str().unwrap().contains("model")
            || model_check["kind"].is_string()
    );
}
