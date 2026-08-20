// Scenario values are intentionally moved into one-shot protocol messages and
// reports, keeping the black-box runner free of borrowed JSON lifetimes.
#![allow(clippy::needless_pass_by_value)]

use std::path::Path;
use std::process::Command;
use std::sync::Barrier;
use std::thread;

use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::ServerTarget;
use crate::client::{CallError, call_tool, interrupt_transaction, read_resource};
use crate::fixtures::{delete, put, record};

type Scenario = fn(&dyn ServerTarget, &Path) -> Result<(), String>;

const SCENARIOS: &[(&str, Scenario)] = &[
    ("atomic_batch", atomic_batch),
    ("snapshot_consistency", snapshot_consistency),
    ("different_key_race", different_key_race),
    ("same_key_conflict", same_key_conflict),
    ("interrupted_write_recovery", interrupted_write_recovery),
    ("history_diff_import_export", history_diff_import_export),
    ("search_fts_and_filters", search_fts_and_filters),
    ("search_pagination", search_pagination),
    (
        "backlinks_explicit_and_mentions",
        backlinks_explicit_and_mentions,
    ),
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
        let outcome = prepare_project()
            .and_then(|project| {
                declare_storage(target, project.path())?;
                Ok(project)
            })
            .and_then(|project| scenario(target, project.path()));
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

/// Declare where the project keeps its records, through the public interface.
///
/// Part of the contract rather than a fixture detail: an implementation that
/// cannot be told where to put its records is not usable by anything that
/// embeds it, and the scenarios below all assume a project that has been told.
fn declare_storage(target: &dyn ServerTarget, project: &Path) -> Result<(), String> {
    call_tool(target, project, "memory_init", json!({"records": "refs"}))
        .map(|_| ())
        .map_err(display)
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
    let seeded = apply(
        target,
        project,
        "atomic-seed",
        &base,
        vec![put(record("obsolete", "remove me"))],
    )
    .map_err(display)?;
    let seeded_revision = string_field(&seeded, "revision")?;
    let result = apply(
        target,
        project,
        "atomic-success",
        &seeded_revision,
        vec![
            put(record("alpha", "first")),
            put(record("beta", "second")),
            delete("obsolete"),
        ],
    )
    .map_err(display)?;
    let revision = string_field(&result, "revision")?;
    assert_string_set(&result, "changed_keys", &["alpha", "beta", "obsolete"])?;

    assert_record_content(target, project, "obsolete", &seeded_revision, "remove me")?;
    assert_record_absent(target, project, "alpha", &seeded_revision)?;
    assert_record_absent(target, project, "beta", &seeded_revision)?;
    assert_record_content(target, project, "alpha", &revision, "first")?;
    assert_record_content(target, project, "beta", &revision, "second")?;
    assert_record_absent(target, project, "obsolete", &revision)?;

    let error = require_error(
        apply(
            target,
            project,
            "atomic-invalid",
            &revision,
            vec![
                put(record("partial", "must not persist")),
                json!({"op": "delete"}),
            ],
        ),
        "a structurally invalid operation must reject the entire batch",
    )?;
    assert_machine_error(&error, "invalid_argument", "field", json!("key"))?;
    equal(
        current_revision(target, project)?,
        revision.clone(),
        "rejected batch changed current revision",
    )?;
    assert_record_absent(target, project, "partial", &revision)
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
    let (second, old_read) = concurrent_pair(
        || {
            apply(
                target,
                project,
                "snapshot-second",
                &first_revision,
                vec![put(record("stable", "version two"))],
            )
        },
        || get_record(target, project, "stable", &first_revision),
    )?;
    let second = second.map_err(display)?;
    let old_read = old_read?
        .ok_or_else(|| "old snapshot lost its record during concurrent write".to_owned())?;
    equal(
        old_read
            .pointer("/envelope/content")
            .and_then(Value::as_str),
        Some("version one"),
        "concurrent old-snapshot read changed",
    )?;
    let second_revision = string_field(&second, "revision")?;
    assert_record_content(target, project, "stable", &first_revision, "version one")?;
    assert_record_content(target, project, "stable", &second_revision, "version two")
}

fn different_key_race(target: &dyn ServerTarget, project: &Path) -> Result<(), String> {
    let base = current_revision(target, project)?;
    let (left, right) = concurrent_pair(
        || {
            apply(
                target,
                project,
                "race-left",
                &base,
                vec![put(record("left", "left writer"))],
            )
        },
        || {
            apply(
                target,
                project,
                "race-right",
                &base,
                vec![put(record("right", "right writer"))],
            )
        },
    )?;
    let left = left.map_err(display)?;
    let right = right.map_err(display)?;
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
    let (first, second) = concurrent_pair(
        || {
            apply(
                target,
                project,
                "conflict-first",
                &base,
                vec![put(record("shared", "first writer"))],
            )
        },
        || {
            apply(
                target,
                project,
                "conflict-second",
                &base,
                vec![put(record("shared", "second writer"))],
            )
        },
    )?;
    let (winner, error) = match (first, second) {
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
    let content = stored.pointer("/envelope/content").and_then(Value::as_str);
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
    let seeded = apply(
        target,
        project,
        "recovery-seed",
        &base,
        vec![put(record("discard", "old value"))],
    )
    .map_err(display)?;
    let base = string_field(&seeded, "revision")?;
    let arguments = json!({
        "transaction_id": "recovery-retry",
        "expected_revision": base,
        "operations": [
            put(record("recoverable-a", "first complete value")),
            put(record("recoverable-b", "second complete value")),
            delete("discard")
        ]
    });
    interrupt_transaction(target, project, arguments.clone()).map_err(display)?;

    let after_interruption = current_revision(target, project)?;
    let discard = get_record(target, project, "discard", &after_interruption)?;
    let first = get_record(target, project, "recoverable-a", &after_interruption)?;
    let second = get_record(target, project, "recoverable-b", &after_interruption)?;
    let all_old =
        record_has_content(discard.as_ref(), "old value") && first.is_none() && second.is_none();
    let all_new = discard.is_none()
        && record_has_content(first.as_ref(), "first complete value")
        && record_has_content(second.as_ref(), "second complete value");
    if !all_old && !all_new {
        return Err("interrupted transaction exposed a partial batch".to_owned());
    }
    if target.has_synchronized_interruption() && (!all_old || after_interruption != base) {
        return Err("pre-commit interruption changed the deterministic fake state".to_owned());
    }

    // A release process can be killed on either side of its atomic commit. The
    // outcome may therefore be old or new, but retrying the transaction must
    // converge and must never expose a partial batch.
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
    assert_record_content(
        target,
        project,
        "recoverable-a",
        &revision,
        "first complete value",
    )?;
    assert_record_content(
        target,
        project,
        "recoverable-b",
        &revision,
        "second complete value",
    )?;
    assert_record_absent(target, project, "discard", &revision)?;
    assert_record_content(target, project, "discard", &base, "old value")
}

fn history_diff_import_export(target: &dyn ServerTarget, project: &Path) -> Result<(), String> {
    let base = current_revision(target, project)?;
    let first = apply(
        target,
        project,
        "history-first",
        &base,
        vec![
            put(record("alpha", "version one")),
            put(record("removed", "temporary")),
        ],
    )
    .map_err(display)?;
    let first_revision = string_field(&first, "revision")?;
    let first_checkpoint = call_tool(
        target,
        project,
        "memory_checkpoint",
        json!({"message": "first checkpoint"}),
    )
    .map_err(display)?;

    let second = apply(
        target,
        project,
        "history-second",
        &first_revision,
        vec![
            put(record("alpha", "version two")),
            put(record("added", "new value")),
            delete("removed"),
        ],
    )
    .map_err(display)?;
    let second_revision = string_field(&second, "revision")?;
    let second_checkpoint = call_tool(
        target,
        project,
        "memory_checkpoint",
        json!({"message": "second checkpoint"}),
    )
    .map_err(display)?;

    assert_history_and_diff(
        target,
        project,
        &first_revision,
        &second_revision,
        &first_checkpoint,
        &second_checkpoint,
    )?;
    assert_export_import_round_trip(target, project, &second_revision)
}

fn assert_history_and_diff(
    target: &dyn ServerTarget,
    project: &Path,
    first_revision: &str,
    second_revision: &str,
    first_checkpoint: &Value,
    second_checkpoint: &Value,
) -> Result<(), String> {
    let history =
        call_tool(target, project, "memory_history", json!({"limit": 10})).map_err(display)?;
    let checkpoints = history
        .get("checkpoints")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("history response has no checkpoints: {history}"))?;
    equal(checkpoints.len(), 2, "checkpoint history length differs")?;
    equal(
        checkpoints[0].get("commit"),
        second_checkpoint.get("commit"),
        "newest checkpoint differs",
    )?;
    equal(
        checkpoints[1].get("commit"),
        first_checkpoint.get("commit"),
        "oldest checkpoint differs",
    )?;

    let diff = call_tool(
        target,
        project,
        "memory_diff",
        json!({"from_revision": first_revision, "to_revision": second_revision}),
    )
    .map_err(display)?;
    assert_changes(
        &diff,
        &[
            ("added", "added"),
            ("alpha", "modified"),
            ("removed", "deleted"),
        ],
    )
}

fn assert_export_import_round_trip(
    target: &dyn ServerTarget,
    project: &Path,
    revision: &str,
) -> Result<(), String> {
    let exported = call_tool(
        target,
        project,
        "memory_export",
        json!({"revision": revision}),
    )
    .map_err(display)?;
    let repeated = call_tool(
        target,
        project,
        "memory_export",
        json!({"revision": revision}),
    )
    .map_err(display)?;
    let bundle = exported
        .get("bundle")
        .cloned()
        .ok_or_else(|| format!("export response has no bundle: {exported}"))?;
    let repeated_bundle = repeated
        .get("bundle")
        .ok_or_else(|| format!("repeated export response has no bundle: {repeated}"))?;
    let bundle_bytes = serde_json::to_vec(&bundle).map_err(|error| error.to_string())?;
    equal(
        bundle_bytes.clone(),
        serde_json::to_vec(repeated_bundle).map_err(|error| error.to_string())?,
        "repeated export bytes differ",
    )?;

    let changed = apply(
        target,
        project,
        "before-import",
        revision,
        vec![put(record("drift", "must disappear"))],
    )
    .map_err(display)?;
    let changed_revision = string_field(&changed, "revision")?;
    let imported = call_tool(
        target,
        project,
        "memory_import",
        json!({
            "transaction_id": "history-import",
            "expected_revision": changed_revision,
            "bundle": bundle
        }),
    )
    .map_err(display)?;
    let imported_revision = string_field(&imported, "revision")?;
    assert_record_absent(target, project, "drift", &imported_revision)?;
    let round_trip = call_tool(
        target,
        project,
        "memory_export",
        json!({"revision": imported_revision}),
    )
    .map_err(display)?;
    let round_trip_bundle = round_trip
        .get("bundle")
        .ok_or_else(|| format!("round-trip export response has no bundle: {round_trip}"))?;
    equal(
        bundle_bytes,
        serde_json::to_vec(round_trip_bundle).map_err(|error| error.to_string())?,
        "export-import-export bytes differ",
    )
}

fn assert_changes(value: &Value, expected: &[(&str, &str)]) -> Result<(), String> {
    let mut actual = value
        .get("changes")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("diff response has no changes: {value}"))?
        .iter()
        .map(|change| {
            let key = change
                .pointer("/id/value")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("diff change has no plaintext id: {change}"))?;
            let kind = change
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("diff change has no kind: {change}"))?;
            Ok((key.to_owned(), kind.to_owned()))
        })
        .collect::<Result<Vec<_>, String>>()?;
    actual.sort_unstable();
    let mut expected = expected
        .iter()
        .map(|(key, kind)| ((*key).to_owned(), (*kind).to_owned()))
        .collect::<Vec<_>>();
    expected.sort_unstable();
    equal(actual, expected, "diff changes differ")
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
        record.pointer("/envelope/content").and_then(Value::as_str),
        Some(expected),
        &format!("record {key:?} has unexpected content"),
    )
}

fn assert_record_absent(
    target: &dyn ServerTarget,
    project: &Path,
    key: &str,
    revision: &str,
) -> Result<(), String> {
    if get_record(target, project, key, revision)?.is_none() {
        Ok(())
    } else {
        Err(format!("record {key:?} unexpectedly exists at {revision}"))
    }
}

fn record_has_content(record: Option<&Value>, expected: &str) -> bool {
    record
        .and_then(|value| value.pointer("/envelope/content"))
        .and_then(Value::as_str)
        == Some(expected)
}

fn assert_string_set(value: &Value, field: &str, expected: &[&str]) -> Result<(), String> {
    let mut actual = value
        .get(field)
        .and_then(Value::as_array)
        .ok_or_else(|| format!("response has no array field {field:?}: {value}"))?
        .iter()
        .map(|item| {
            item.as_str()
                .map(str::to_owned)
                .ok_or_else(|| format!("response field {field:?} contains a non-string"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    actual.sort_unstable();
    let mut expected = expected.iter().map(ToString::to_string).collect::<Vec<_>>();
    expected.sort_unstable();
    equal(
        actual,
        expected,
        &format!("response field {field:?} differs"),
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

fn assert_machine_error(
    error: &CallError,
    expected_kind: &str,
    data_key: &str,
    expected_value: Value,
) -> Result<(), String> {
    let (kind, data) = match error {
        CallError::Tool { kind, data } => (kind.as_str(), data),
        CallError::Rpc { data, .. } => {
            let kind = data
                .get("kind")
                .and_then(Value::as_str)
                .ok_or_else(|| format!("JSON-RPC error has no machine-readable kind: {error}"))?;
            let data = data.get("data").unwrap_or(data);
            (kind, data)
        }
        _ => return Err(format!("expected machine-readable error, got {error}")),
    };
    equal(kind, expected_kind, "machine-readable error kind differs")?;
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

fn concurrent_pair<L, R>(
    left: impl FnOnce() -> L + Send,
    right: impl FnOnce() -> R + Send,
) -> Result<(L, R), String>
where
    L: Send,
    R: Send,
{
    let barrier = Barrier::new(3);
    thread::scope(|scope| {
        let left = scope.spawn(|| {
            barrier.wait();
            left()
        });
        let right = scope.spawn(|| {
            barrier.wait();
            right()
        });
        barrier.wait();
        let left = left
            .join()
            .map_err(|_| "left contract worker thread panicked".to_owned())?;
        let right = right
            .join()
            .map_err(|_| "right contract worker thread panicked".to_owned())?;
        Ok((left, right))
    })
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

fn check(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned())
    }
}

// ---------------------------------------------------------------------------
// Search and backlinks scenarios
// ---------------------------------------------------------------------------

fn search_record(
    key: &str,
    kind: &str,
    content: &str,
    title: Option<&str>,
    tags: Vec<String>,
    links: Vec<Value>,
) -> Value {
    let content_hash = format!("sha256:{:x}", Sha256::digest(content.as_bytes()));
    let mut envelope = json!({
        "envelope_version": {"major": 1, "minor": 0},
        "key": key,
        "kind": kind,
        "content": content,
        "tags": tags,
        "links": links,
        "source_paths": {},
        "archive": {"archived": false},
        "freshness": {"state": "unverified"},
        "content_hash": content_hash,
    });
    if let Some(t) = title {
        envelope["title"] = json!(t);
    }
    json!({"representation": "plaintext", "envelope": envelope})
}

fn search_fts_and_filters(target: &dyn ServerTarget, project: &Path) -> Result<(), String> {
    let base = current_revision(target, project)?;
    let result = apply(
        target,
        project,
        "search-seed",
        &base,
        vec![
            put(search_record(
                "alpha",
                "decision",
                "Use Rust for the core because memory safety matters",
                Some("Language choice"),
                vec!["architecture".into()],
                vec![],
            )),
            put(search_record(
                "beta",
                "note",
                "Rust async runtime requires careful thought",
                Some("Async notes"),
                vec!["async".into()],
                vec![],
            )),
            put(search_record(
                "gamma",
                "decision",
                "Use LanceDB for the projection layer",
                Some("Index store"),
                vec!["architecture".into(), "index".into()],
                vec![],
            )),
        ],
    )
    .map_err(display)?;
    let revision = string_field(&result, "revision")?;
    search_fts_basic(target, project, &revision)?;
    search_fts_kind_filter(target, project, &revision)?;
    search_fts_tag_filter(target, project, &revision)?;
    search_fts_degraded_flag(target, project, &revision)?;
    Ok(())
}

fn search_fts_basic(
    target: &dyn ServerTarget,
    project: &Path,
    revision: &str,
) -> Result<(), String> {
    let search_result = call_tool(
        target,
        project,
        "memory_search",
        json!({"query": "rust", "revision": revision}),
    )
    .map_err(display)?;
    let hits = search_result
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("search returned no hits array: {search_result}"))?;
    check(!hits.is_empty(), "FTS search for 'rust' should return hits")?;
    let ids: Vec<&str> = hits
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str))
        .collect();
    check(ids.contains(&"alpha"), "search should find 'alpha'")?;
    check(ids.contains(&"beta"), "search should find 'beta'")?;
    Ok(())
}

fn search_fts_kind_filter(
    target: &dyn ServerTarget,
    project: &Path,
    revision: &str,
) -> Result<(), String> {
    let filtered = call_tool(
        target,
        project,
        "memory_search",
        json!({"query": "rust", "revision": revision, "kind": "decision"}),
    )
    .map_err(display)?;
    let filtered_hits = filtered
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("filtered search returned no hits: {filtered}"))?;
    let filtered_ids: Vec<&str> = filtered_hits
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str))
        .collect();
    check(
        filtered_ids.contains(&"alpha"),
        "filtered search should find 'alpha'",
    )?;
    check(
        !filtered_ids.contains(&"beta"),
        "filtered search should exclude 'beta' (kind=note)",
    )?;
    Ok(())
}

fn search_fts_tag_filter(
    target: &dyn ServerTarget,
    project: &Path,
    revision: &str,
) -> Result<(), String> {
    let tag_filtered = call_tool(
        target,
        project,
        "memory_search",
        json!({"query": "lancedb", "revision": revision, "tags": ["architecture"]}),
    )
    .map_err(display)?;
    let tag_hits = tag_filtered
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("tag-filtered search returned no hits: {tag_filtered}"))?;
    let tag_ids: Vec<&str> = tag_hits
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str))
        .collect();
    check(
        tag_ids.contains(&"gamma"),
        "tag-filtered search should find 'gamma'",
    )?;
    Ok(())
}

fn search_fts_degraded_flag(
    target: &dyn ServerTarget,
    project: &Path,
    revision: &str,
) -> Result<(), String> {
    let search_result = call_tool(
        target,
        project,
        "memory_search",
        json!({"query": "rust", "revision": revision}),
    )
    .map_err(display)?;
    let degraded = search_result
        .get("degraded")
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("search result has no 'degraded' field: {search_result}"))?;
    let mode = search_result
        .get("mode")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("search result has no 'mode' field: {search_result}"))?;
    // Whether an embedding model is installed is a property of the machine,
    // not of the interface — a contract that demanded one answer would pass or
    // fail depending on the developer's model cache. What the interface does
    // promise is that the two fields agree: degraded means no vector channel,
    // which means FTS-only results.
    check(
        !degraded || mode == "fts",
        "degraded search must report mode=fts",
    )?;
    check(
        matches!(mode, "fts" | "hybrid"),
        "search mode must be `fts` or `hybrid`",
    )?;
    Ok(())
}

fn search_pagination(target: &dyn ServerTarget, project: &Path) -> Result<(), String> {
    let base = current_revision(target, project)?;
    let mut ops = Vec::new();
    for i in 0..10 {
        ops.push(put(search_record(
            &format!("item-{i:02}"),
            "note",
            &format!("pagination test record number {i}"),
            Some(&format!("Item {i}")),
            vec![],
            vec![],
        )));
    }
    let result = apply(target, project, "pagination-seed", &base, ops).map_err(display)?;
    let revision = string_field(&result, "revision")?;

    let page1 = call_tool(
        target,
        project,
        "memory_search",
        json!({"query": "pagination", "revision": revision, "limit": 3, "offset": 0}),
    )
    .map_err(display)?;
    let hits1 = page1
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("page1 has no hits: {page1}"))?;
    equal(hits1.len(), 3, "page 1 should return 3 hits")?;
    let has_more1 = page1
        .get("has_more")
        .and_then(Value::as_bool)
        .ok_or_else(|| format!("page1 has no has_more: {page1}"))?;
    check(has_more1, "page 1 should indicate more results")?;

    let page2 = call_tool(
        target,
        project,
        "memory_search",
        json!({"query": "pagination", "revision": revision, "limit": 3, "offset": 3}),
    )
    .map_err(display)?;
    let hits2 = page2
        .get("hits")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("page2 has no hits: {page2}"))?;
    equal(hits2.len(), 3, "page 2 should return 3 hits")?;

    let ids1: Vec<String> = hits1
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let ids2: Vec<String> = hits2
        .iter()
        .filter_map(|h| h.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    for id in &ids2 {
        check(!ids1.contains(id), "page 2 should not overlap page 1")?;
    }

    Ok(())
}

fn backlinks_explicit_and_mentions(
    target: &dyn ServerTarget,
    project: &Path,
) -> Result<(), String> {
    let base = current_revision(target, project)?;
    let result = apply(
        target,
        project,
        "backlinks-seed",
        &base,
        vec![
            put(search_record(
                "target",
                "decision",
                "The canonical decision record",
                Some("Target"),
                vec![],
                vec![],
            )),
            put(search_record(
                "explicit-linker",
                "note",
                "This note references the target decision",
                Some("Linker"),
                vec![],
                vec![json!({"key": "target", "relation": "references"})],
            )),
            put(search_record(
                "body-mentioner",
                "observation",
                "See target for the full rationale behind this choice",
                Some("Mentioner"),
                vec![],
                vec![],
            )),
            put(search_record(
                "unrelated",
                "note",
                "This record has nothing to do with anything",
                Some("Unrelated"),
                vec![],
                vec![],
            )),
        ],
    )
    .map_err(display)?;
    let revision = string_field(&result, "revision")?;

    let bl_result = call_tool(
        target,
        project,
        "memory_backlinks",
        json!({"key": "target", "revision": revision}),
    )
    .map_err(display)?;
    let backlinks = bl_result
        .get("backlinks")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("backlinks returned no array: {bl_result}"))?;
    check(
        backlinks.len() >= 2,
        &format!(
            "backlinks should find at least 2 entries (explicit + mention), got {}",
            backlinks.len()
        ),
    )?;

    let source_ids: Vec<&str> = backlinks
        .iter()
        .filter_map(|b| b.get("source_id").and_then(Value::as_str))
        .collect();
    check(
        source_ids.contains(&"explicit-linker"),
        "backlinks should include explicit-linker",
    )?;
    check(
        source_ids.contains(&"body-mentioner"),
        "backlinks should include body-mentioner",
    )?;
    check(
        !source_ids.contains(&"unrelated"),
        "backlinks should not include unrelated",
    )?;

    let has_explicit = backlinks.iter().any(|b| {
        b.get("source_id").and_then(Value::as_str) == Some("explicit-linker")
            && b.get("mention_type").and_then(Value::as_str) == Some("explicit_link")
    });
    let has_mention = backlinks.iter().any(|b| {
        b.get("source_id").and_then(Value::as_str) == Some("body-mentioner")
            && b.get("mention_type").and_then(Value::as_str) == Some("body_mention")
    });
    check(has_explicit, "backlinks should have an explicit_link entry")?;
    check(has_mention, "backlinks should have a body_mention entry")?;

    Ok(())
}
