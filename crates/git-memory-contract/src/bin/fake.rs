#![allow(clippy::expect_used)]
#![allow(clippy::needless_pass_by_value)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use clap::Parser;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use git_memory_contract::{MCP_PROTOCOL_VERSION, MEMORY_INTERFACE_VERSION};

#[derive(Debug, Parser)]
#[command(about = "Deterministic black-box fake for the Git Memory contract")]
struct Cli {
    /// Accepted so this binary can also exercise the release-process adapter.
    #[arg(hide = true)]
    mcp: Option<String>,

    #[arg(long)]
    project: PathBuf,
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
struct Snapshot {
    records: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct State {
    current: String,
    next_revision: u64,
    snapshots: BTreeMap<String, Snapshot>,
    completed: BTreeMap<String, TransactionResult>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TransactionResult {
    revision: String,
    changed_keys: Vec<String>,
}

impl Default for State {
    fn default() -> Self {
        Self {
            current: "r0".to_owned(),
            next_revision: 1,
            snapshots: BTreeMap::from([("r0".to_owned(), Snapshot::default())]),
            completed: BTreeMap::new(),
        }
    }
}

#[derive(Debug)]
struct ToolFailure {
    kind: &'static str,
    data: Value,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let state_path = cli.project.join(".git-memory-contract-fake.json");
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut output = stdout.lock();
    for line in stdin.lock().lines() {
        let line = line?;
        let request: Value = match serde_json::from_str(&line) {
            Ok(request) => request,
            Err(error) => {
                write_response(
                    &mut output,
                    json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error", "data": {"kind": "invalid_json", "detail": error.to_string()}}}),
                )?;
                continue;
            }
        };
        let Some(id) = request.get("id").cloned() else {
            continue;
        };
        let response = dispatch(&state_path, &request);
        write_response(
            &mut output,
            json!({"jsonrpc": "2.0", "id": id, "result": response}),
        )?;
    }
    Ok(())
}

fn dispatch(state_path: &Path, request: &Value) -> Value {
    match request.get("method").and_then(Value::as_str) {
        Some("initialize") => json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {"resources": {}, "tools": {}},
            "serverInfo": {"name": "git-memory-contract-fake", "version": env!("CARGO_PKG_VERSION")},
            "memoryInterfaceVersion": MEMORY_INTERFACE_VERSION
        }),
        Some("resources/read") => read_resource(state_path, request),
        Some("tools/call") => call_tool(state_path, request),
        _ => json!({}),
    }
}

fn read_resource(state_path: &Path, request: &Value) -> Value {
    let uri = request
        .pointer("/params/uri")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if uri != "memory://revision/current" {
        return tool_failure("resource_not_found", json!({"uri": uri}));
    }
    match load_state(state_path) {
        Ok(state) => {
            let text = json!({"revision": state.current}).to_string();
            json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": text}]})
        }
        Err(error) => tool_failure(
            "fake_state_unavailable",
            json!({"detail": error.to_string()}),
        ),
    }
}

fn call_tool(state_path: &Path, request: &Value) -> Value {
    let name = request
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = request
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or(Value::Null);
    let result = match name {
        "memory_apply_transaction" => apply_transaction(state_path, &arguments),
        "memory_get_record" => get_record(state_path, &arguments),
        _ => Err(ToolFailure {
            kind: "tool_not_found",
            data: json!({"name": name}),
        }),
    };
    match result {
        Ok(content) => json!({
            "content": [{"type": "text", "text": "request completed"}],
            "structuredContent": content
        }),
        Err(failure) => tool_failure(failure.kind, failure.data),
    }
}

fn apply_transaction(state_path: &Path, arguments: &Value) -> Result<Value, ToolFailure> {
    let transaction_id = required_string(arguments, "transaction_id")?;
    let expected_revision = required_string(arguments, "expected_revision")?;
    let operations = arguments
        .get("operations")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_argument("operations"))?;
    if operations.is_empty() {
        return Err(invalid_argument("operations"));
    }

    let (_lock, mut state) = load_locked_state(state_path)?;
    if let Some(completed) = state.completed.get(transaction_id) {
        return Ok(json!(completed));
    }
    let expected = state
        .snapshots
        .get(expected_revision)
        .cloned()
        .ok_or_else(|| ToolFailure {
            kind: "snapshot_not_found",
            data: json!({"revision": expected_revision}),
        })?;
    let current = state
        .snapshots
        .get(&state.current)
        .cloned()
        .expect("current fake snapshot must exist");

    let mut keys = BTreeSet::new();
    for operation in operations {
        let key = operation_key(operation)?;
        if !keys.insert(key.to_owned()) {
            return Err(ToolFailure {
                kind: "duplicate_operation",
                data: json!({"key": key}),
            });
        }
        if operation.get("op").and_then(Value::as_str) == Some("delete")
            && !current.records.contains_key(key)
        {
            return Err(ToolFailure {
                kind: "record_not_found",
                data: json!({"key": key}),
            });
        }
    }

    if expected_revision != state.current {
        let conflicting_keys: Vec<_> = keys
            .iter()
            .filter(|key| expected.records.get(*key) != current.records.get(*key))
            .cloned()
            .collect();
        if !conflicting_keys.is_empty() {
            return Err(ToolFailure {
                kind: "conflict",
                data: json!({
                    "expected_revision": expected_revision,
                    "current_revision": state.current,
                    "conflicting_keys": conflicting_keys,
                    "recovery_action": "refresh_and_retry"
                }),
            });
        }
    }

    let mut next = current;
    for operation in operations {
        let key = operation_key(operation)?.to_owned();
        match operation.get("op").and_then(Value::as_str) {
            Some("put") => {
                let record = operation
                    .get("record")
                    .cloned()
                    .ok_or_else(|| invalid_argument("record"))?;
                next.records.insert(key, record);
            }
            Some("delete") => {
                next.records.remove(&key);
            }
            _ => return Err(invalid_argument("op")),
        }
    }
    let revision = format!("r{}", state.next_revision);
    state.next_revision += 1;
    state.current.clone_from(&revision);
    state.snapshots.insert(revision.clone(), next);
    let result = TransactionResult {
        revision,
        changed_keys: keys.into_iter().collect(),
    };
    state
        .completed
        .insert(transaction_id.to_owned(), result.clone());
    save_state(state_path, &state).map_err(state_failure)?;
    Ok(json!(result))
}

fn get_record(state_path: &Path, arguments: &Value) -> Result<Value, ToolFailure> {
    let key = required_string(arguments, "key")?;
    let revision = required_string(arguments, "revision")?;
    let state = load_state(state_path).map_err(state_failure)?;
    let snapshot = state.snapshots.get(revision).ok_or_else(|| ToolFailure {
        kind: "snapshot_not_found",
        data: json!({"revision": revision}),
    })?;
    Ok(json!({"revision": revision, "record": snapshot.records.get(key)}))
}

fn operation_key(operation: &Value) -> Result<&str, ToolFailure> {
    match operation.get("op").and_then(Value::as_str) {
        Some("put") => operation.pointer("/record/key").and_then(Value::as_str),
        Some("delete") => operation.get("key").and_then(Value::as_str),
        _ => return Err(invalid_argument("op")),
    }
    .ok_or_else(|| invalid_argument("key"))
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, ToolFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_argument(field))
}

fn invalid_argument(field: &str) -> ToolFailure {
    ToolFailure {
        kind: "invalid_argument",
        data: json!({"field": field}),
    }
}

fn state_failure(error: io::Error) -> ToolFailure {
    ToolFailure {
        kind: "fake_state_unavailable",
        data: json!({"detail": error.to_string()}),
    }
}

fn load_state(path: &Path) -> io::Result<State> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(io::Error::other),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(State::default()),
        Err(error) => Err(error),
    }
}

fn load_locked_state(state_path: &Path) -> Result<(File, State), ToolFailure> {
    let lock = OpenOptions::new()
        .create(true)
        .read(true)
        .truncate(false)
        .write(true)
        .open(state_path.with_extension("lock"))
        .map_err(state_failure)?;
    lock.lock_exclusive().map_err(state_failure)?;
    let state = load_state(state_path).map_err(state_failure)?;
    Ok((lock, state))
}

fn save_state(path: &Path, state: &State) -> io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::other("state path has no parent"))?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    serde_json::to_writer(&mut temporary, state).map_err(io::Error::other)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

fn tool_failure(kind: &str, data: Value) -> Value {
    json!({
        "isError": true,
        "structuredContent": {"error": {"kind": kind, "data": data}},
        "content": [{"type": "text", "text": "request failed"}]
    })
}

fn write_response(output: &mut impl Write, response: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *output, &response).map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}
