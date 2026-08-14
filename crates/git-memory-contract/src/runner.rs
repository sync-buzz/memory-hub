#![allow(clippy::needless_pass_by_value)]

use std::path::Path;
use std::process::Command;
use std::sync::Barrier;
use std::thread;

use serde::Serialize;
use serde_json::{Value, json};

use crate::ServerTarget;
use crate::client::{CallError, call_tool, interrupt_transaction, read_resource};
use crate::fixtures::{put, record};

type Scenario = fn(&dyn ServerTarget, &Path) -> Result<(), String>;

const SCENARIOS: &[(&str, Scenario)] = &[
    ("atomic_batch", atomic_batch),
    ("snapshot_consistency", snapshot_consistency),
    ("different_key_race", different_key_race),
    ("same_key_conflict", same_key_conflict),
    ("interrupted_write_recovery", interrupted_write_recovery),
];

#[derive(Debug, Serialize)]
pub struct ContractReport {
    pub schema_version: u32,
    pub target: String,
    pub passed: bool,
    pub scenarios: Vec<ScenarioReport>,
}

#[derive(Debug, Serialize)]
pub struct ScenarioReport {
    pub name: &'static str,
    pub passed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
}

/// Run every behavioral scenario against a fresh project and the supplied
/// black-box process target.
///
/// The scenario list is owned by this crate, so additional consumers use the
/// same assertions without copying test code into their repositories.
#[must_use]
pub fn run_contract(target: &dyn ServerTarget) -> ContractReport {
    let mut reports = Vec::with_capacity(SCENARIOS.len());
    for (name, scenario) in SCENARIOS {
        let outcome = prepare_project().and_then(|project| scenario(target, project.path()));
        reports.push(ScenarioReport {
            name,
            passed: outcome.is_ok(),
            failure: outcome.err(),
        });
    }
    ContractReport {
        schema_version: 1,
        target: target.label().to_owned(),
        passed: reports.iter().all(|report| report.passed),
        scenarios: reports,
    }
}

fn prepare_project() -> Result<tempfile::TempDir, String> {
    let project = tempfile::tempdir().map_err(|error| error.to_string())?;
    let output = Command::new("git")
        .args(["init", "--quiet"])
        .arg(project.path())
        .output()
        .map_err(|error| format!("unable to initialize contract repository: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git init failed with status {:?}",
            output.status.code()
        ));
    }
    Ok(project)
}

fn current_revision(target: &dyn ServerTarget, project: &Path) -> Result<String, String> {
    let resource = read_resource(target, project, "memory://revision/current").map_err(display)?;
    string_field(&resource, "revision")
}

fn get_record(
    target: &dyn ServerTarget,
    project: &Path,
    key: &str,
    revision: &str,
) -> Result<Option<Value>, String> {
    let result = call_tool(
        target,
        project,
        "memory_get_record",
        json!({"key": key, "revision": revision}),
    )
    .map_err(display)?;
    match result.get("record") {
        Some(Value::Null) | None => Ok(None),
        Some(value) => Ok(Some(value.clone())),
    }
}

fn apply(
    target: &dyn ServerTarget,
    project: &Path,
    transaction_id: &str,
    expected_revision: &str,
    operations: Vec<Value>,
) -> Result<Value, CallError> {
    call_tool(
        target,
        project,
        "memory_apply_transaction",
        json!({
            "transaction_id": transaction_id,
            "expected_revision": expected_revision,
            "operations": operations
        }),
    )
}

fn atomic_batch(target: &dyn ServerTarget, project: &Path) -> Result<(), String> {
    let base = current_revision(target, project)?;
    let result = apply(
        target,
        project,
        "atomic-success",
        &base,
        vec![put(record("alpha", "first")), put(record("beta", "second"))],
    )
    .map_err(display)?;
    let revision = string_field(&result, "revision")?;
    assert_record_content(target, project, "alpha", &revision, "first")?;
    assert_record_content(target, project, "beta", &revision, "second")?;

    let before_invalid = current_revision(target, project)?;
    let error = require_error(
        apply(
            target,
            project,
            "atomic-invalid",
            &before_invalid,
            vec![
                put(record("gamma", "must not persist")),
                json!({"op": "delete"}),
            ],
        ),
        "a structurally invalid operation must reject the entire batch",
    )?;
    assert_tool_error(&error, "invalid_argument", "field", json!("key"))?;
    equal(
        current_revision(target, project)?,
        before_invalid.clone(),
        "failed batch changed current revision",
    )?;
    if get_record(target, project, "gamma", &before_invalid)?.is_some() {
        return Err("failed batch persisted one of its records".to_owned());
    }
    Ok(())
}

fn snapshot_consistency(target: &dyn ServerTarget, project: &Path) -> Result<(), String> {
    let base = current_revision(target, project)?;
    let first = apply(
        target,
        project,
        "snapshot-first",
        &base,
        vec![put(record("stable", "version one"))],
    )
    .map_err(display)?;
    let first_revision = string_field(&first, "revision")?;
    let second = apply(
        target,
        project,
        "snapshot-second",
        &first_revision,
        vec![put(record("stable", "version two"))],
    )
    .map_err(display)?;
    let second_revision = string_field(&second, "revision")?;
    assert_record_content(target, project, "stable", &first_revision, "version one")?;
    assert_record_content(target, project, "stable", &second_revision, "version two")
}

fn different_key_race(target: &dyn ServerTarget, project: &Path) -> Result<(), String> {
    let base = current_revision(target, project)?;
    let barrier = Barrier::new(3);
    let (left, right) = thread::scope(|scope| {
        let left = scope.spawn(|| {
            barrier.wait();
            apply(
                target,
                project,
                "race-left",
                &base,
                vec![put(record("left", "left writer"))],
            )
        });
        let right = scope.spawn(|| {
            barrier.wait();
            apply(
                target,
                project,
                "race-right",
                &base,
                vec![put(record("right", "right writer"))],
            )
        });
        barrier.wait();
        (join_call(left), join_call(right))
    });
    let left = left?;
    let right = right?;
    let left_revision = string_field(&left, "revision")?;
    let right_revision = string_field(&right, "revision")?;
    if left_revision == right_revision {
        return Err("different-key rebase did not advance the revision".to_owned());
    }
    let merged_revision = current_revision(target, project)?;
    assert_record_content(target, project, "left", &merged_revision, "left writer")?;
    assert_record_content(target, project, "right", &merged_revision, "right writer")
}

fn same_key_conflict(target: &dyn ServerTarget, project: &Path) -> Result<(), String> {
    let base = current_revision(target, project)?;
    let barrier = Barrier::new(3);
    let (first, second) = thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            apply(
                target,
                project,
                "conflict-first",
                &base,
                vec![put(record("shared", "first writer"))],
            )
        });
        let second = scope.spawn(|| {
            barrier.wait();
            apply(
                target,
                project,
                "conflict-second",
                &base,
                vec![put(record("shared", "second writer"))],
            )
        });
        barrier.wait();
        (join_raw_call(first), join_raw_call(second))
    });
    let (winner, error) = match (first?, second?) {
        (Ok(winner), Err(error)) | (Err(error), Ok(winner)) => (winner, error),
        (Ok(_), Ok(_)) => return Err("both same-key writers succeeded".to_owned()),
        (Err(first), Err(second)) => {
            return Err(format!("both same-key writers failed: {first}; {second}"));
        }
    };
    let current = string_field(&winner, "revision")?;
    assert_tool_error(&error, "conflict", "expected_revision", json!(base))?;
    assert_tool_error(
        &error,
        "conflict",
        "current_revision",
        json!(current.clone()),
    )?;
    assert_tool_error(&error, "conflict", "conflicting_keys", json!(["shared"]))?;
    assert_tool_error(
        &error,
        "conflict",
        "recovery_action",
        json!("refresh_and_retry"),
    )?;
    let stored = get_record(target, project, "shared", &current)?
        .ok_or_else(|| "same-key winner was not persisted".to_owned())?;
    let content = stored.get("content").and_then(Value::as_str);
    if matches!(content, Some("first writer" | "second writer")) {
        Ok(())
    } else {
        Err(format!(
            "same-key winner has unexpected content: {content:?}"
        ))
    }
}

fn interrupted_write_recovery(target: &dyn ServerTarget, project: &Path) -> Result<(), String> {
    let base = current_revision(target, project)?;
    let arguments = json!({
        "transaction_id": "recovery-retry",
        "expected_revision": base,
        "operations": [put(record("recoverable", "complete value"))]
    });
    interrupt_transaction(target, project, arguments.clone()).map_err(display)?;

    // An interrupted request may have committed or may not have been observed by
    // the server. Retrying the same transaction must converge in either case.
    let result = call_tool(
        target,
        project,
        "memory_apply_transaction",
        arguments.clone(),
    )
    .map_err(display)?;
    let revision = string_field(&result, "revision")?;
    let repeated =
        call_tool(target, project, "memory_apply_transaction", arguments).map_err(display)?;
    equal(
        string_field(&repeated, "revision")?,
        revision.clone(),
        "idempotent retry produced another revision",
    )?;
    assert_record_content(target, project, "recoverable", &revision, "complete value")
}

fn assert_record_content(
    target: &dyn ServerTarget,
    project: &Path,
    key: &str,
    revision: &str,
    expected: &str,
) -> Result<(), String> {
    let record = get_record(target, project, key, revision)?
        .ok_or_else(|| format!("record {key:?} is missing at {revision}"))?;
    equal(
        record.get("content").and_then(Value::as_str),
        Some(expected),
        &format!("record {key:?} has unexpected content"),
    )
}

fn assert_tool_error(
    error: &CallError,
    expected_kind: &str,
    data_key: &str,
    expected_value: Value,
) -> Result<(), String> {
    let CallError::Tool { kind, data } = error else {
        return Err(format!("expected structured tool error, got {error}"));
    };
    equal(
        kind.as_str(),
        expected_kind,
        "machine-readable error kind differs",
    )?;
    equal(
        data.get(data_key),
        Some(&expected_value),
        &format!("machine-readable error data.{data_key} differs"),
    )
}

fn require_error(result: Result<Value, CallError>, context: &str) -> Result<CallError, String> {
    match result {
        Ok(value) => Err(format!("{context}; received success: {value}")),
        Err(error) => Ok(error),
    }
}

fn join_call(
    handle: thread::ScopedJoinHandle<'_, Result<Value, CallError>>,
) -> Result<Value, String> {
    join_raw_call(handle)?.map_err(display)
}

fn join_raw_call(
    handle: thread::ScopedJoinHandle<'_, Result<Value, CallError>>,
) -> Result<Result<Value, CallError>, String> {
    handle
        .join()
        .map_err(|_| "contract worker thread panicked".to_owned())
}

fn string_field(value: &Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("response has no string field {field:?}: {value}"))
}

fn equal<T: PartialEq + std::fmt::Debug>(
    actual: T,
    expected: T,
    context: &str,
) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{context}: expected {expected:?}, got {actual:?}"))
    }
}

fn display(error: CallError) -> String {
    error.to_string()
}
