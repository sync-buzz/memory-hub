// Fake responses own short-lived JSON values at the serialization boundary.
#![allow(clippy::needless_pass_by_value)]

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::thread;

use clap::Parser;
use fs2::FileExt;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use memory_hub_contract::MCP_PROTOCOL_VERSION;

#[derive(Debug, Parser)]
#[command(about = "Deterministic black-box fake for the Memory Hub contract")]
struct Cli {
    /// Accepted so this binary can also exercise the release-process adapter.
    #[arg(hide = true)]
    _mode: Option<String>,

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
    #[serde(default)]
    checkpoints: Vec<Checkpoint>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct TransactionResult {
    revision: String,
    changed_keys: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Checkpoint {
    commit: String,
    revision: String,
    message: String,
    timestamp: i64,
}

impl Default for State {
    fn default() -> Self {
        Self {
            current: "r0".to_owned(),
            next_revision: 1,
            snapshots: BTreeMap::from([("r0".to_owned(), Snapshot::default())]),
            completed: BTreeMap::new(),
            checkpoints: Vec::new(),
        }
    }
}

#[derive(Debug)]
struct ToolFailure {
    kind: &'static str,
    data: Value,
}

#[derive(Debug)]
struct RpcFailure {
    code: i64,
    message: &'static str,
    data: Value,
}

fn main() -> io::Result<()> {
    let cli = Cli::parse();
    let state_path = cli.project.join(".memory-hub-contract-fake.json");
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
        let response = match dispatch(&state_path, &request, &mut output) {
            Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
            Err(error) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {"code": error.code, "message": error.message, "data": error.data}
            }),
        };
        write_response(&mut output, response)?;
    }
    Ok(())
}

fn dispatch(
    state_path: &Path,
    request: &Value,
    output: &mut impl Write,
) -> Result<Value, RpcFailure> {
    match request.get("method").and_then(Value::as_str) {
        Some("initialize") => Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {"resources": {}, "tools": {}},
            "serverInfo": {"name": "memory-hub-contract-fake", "version": env!("CARGO_PKG_VERSION")}
        })),
        Some("resources/list") => Ok(list_resources()),
        Some("resources/read") => read_resource(state_path, request),
        Some("tools/list") => Ok(list_tools()),
        Some("tools/call") => call_tool(state_path, request, output),
        _ => Err(RpcFailure {
            code: -32_601,
            message: "method not found",
            data: json!({"kind": "method_not_found"}),
        }),
    }
}

fn read_resource(state_path: &Path, request: &Value) -> Result<Value, RpcFailure> {
    let uri = request
        .pointer("/params/uri")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if uri != "memory://revision/current" {
        return Err(RpcFailure {
            code: -32_602,
            message: "resource not found",
            data: json!({"kind": "resource_not_found", "data": {"uri": uri}}),
        });
    }
    match load_state(state_path) {
        Ok(state) => {
            let text = json!({"revision": state.current}).to_string();
            Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": text}]}))
        }
        Err(error) => Err(RpcFailure {
            code: -32_603,
            message: "fake state unavailable",
            data: json!({"kind": "fake_state_unavailable", "data": {"detail": error.to_string()}}),
        }),
    }
}

fn call_tool(
    state_path: &Path,
    request: &Value,
    output: &mut impl Write,
) -> Result<Value, RpcFailure> {
    let name = request
        .pointer("/params/name")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let arguments = request
        .pointer("/params/arguments")
        .cloned()
        .unwrap_or(Value::Null);
    let result = match name {
        "memory_apply_transaction" => {
            let progress_token = request
                .pointer("/params/_meta/progressToken")
                .and_then(Value::as_str);
            apply_transaction(state_path, &arguments, progress_token, output)
        }
        "memory_get_record" => get_record(state_path, &arguments),
        "memory_diff" => diff(state_path, &arguments),
        "memory_export" => export(state_path, &arguments),
        "memory_import" => import(state_path, &arguments),
        "memory_search" => search(state_path, &arguments),
        "memory_backlinks" => backlinks(state_path, &arguments),
        _ => {
            return Err(RpcFailure {
                code: -32_602,
                message: "tool not found",
                data: json!({"kind": "tool_not_found", "data": {"name": name}}),
            });
        }
    };
    Ok(match result {
        Ok(content) => {
            let text = content.to_string();
            json!({
                "content": [{"type": "text", "text": text}],
                "structuredContent": content
            })
        }
        Err(failure) => tool_failure(failure.kind, failure.data),
    })
}

fn list_resources() -> Value {
    json!({
        "resources": [{
            "name": "Current memory revision",
            "uri": "memory://revision/current",
            "mimeType": "application/json"
        }]
    })
}

fn list_tools() -> Value {
    json!({
        "tools": [
            {
                "name": "memory_apply_transaction",
                "description": "Atomically apply a generic record transaction",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "transaction_id": {"type": "string"},
                        "expected_revision": {"type": "string"},
                        "operations": {"type": "array", "items": {"type": "object"}}
                    },
                    "required": ["transaction_id", "expected_revision", "operations"]
                }
            },
            {
                "name": "memory_get_record",
                "description": "Read a generic record from an immutable snapshot",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "key": {"type": "string"},
                        "revision": {"type": "string"}
                    },
                    "required": ["key", "revision"]
                }
            },
            {"name": "memory_diff", "description": "Diff", "inputSchema": {"type": "object"}},
            {"name": "memory_export", "description": "Export", "inputSchema": {"type": "object"}},
            {"name": "memory_import", "description": "Import", "inputSchema": {"type": "object"}},
            {"name": "memory_search", "description": "Search records", "inputSchema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}},
            {"name": "memory_backlinks", "description": "Backlinks", "inputSchema": {"type": "object", "properties": {"key": {"type": "string"}}, "required": ["key"]}}
        ]
    })
}

fn apply_transaction(
    state_path: &Path,
    arguments: &Value,
    progress_token: Option<&str>,
    output: &mut impl Write,
) -> Result<Value, ToolFailure> {
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
        .ok_or_else(|| ToolFailure {
            kind: "fake_state_invariant",
            data: json!({"missing_revision": state.current}),
        })?;

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
    pause_before_commit(progress_token, output)?;
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

fn diff(state_path: &Path, arguments: &Value) -> Result<Value, ToolFailure> {
    let from_revision = required_string(arguments, "from_revision")?;
    let to_revision = required_string(arguments, "to_revision")?;
    let state = load_state(state_path).map_err(state_failure)?;
    let from = state
        .snapshots
        .get(from_revision)
        .ok_or_else(|| ToolFailure {
            kind: "snapshot_not_found",
            data: json!({"revision": from_revision}),
        })?;
    let to = state
        .snapshots
        .get(to_revision)
        .ok_or_else(|| ToolFailure {
            kind: "snapshot_not_found",
            data: json!({"revision": to_revision}),
        })?;
    let keys = from
        .records
        .keys()
        .chain(to.records.keys())
        .collect::<BTreeSet<_>>();
    let changes = keys
        .into_iter()
        .filter_map(|key| {
            let kind = match (from.records.get(key), to.records.get(key)) {
                (None, Some(_)) => "added",
                (Some(_), None) => "deleted",
                (Some(left), Some(right)) if left != right => "modified",
                _ => return None,
            };
            Some(json!({
                "id": {"addressing": "plaintext", "value": key},
                "kind": kind
            }))
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "fromRevision": from_revision,
        "toRevision": to_revision,
        "changes": changes
    }))
}

fn export(state_path: &Path, arguments: &Value) -> Result<Value, ToolFailure> {
    let revision = required_string(arguments, "revision")?;
    let state = load_state(state_path).map_err(state_failure)?;
    let snapshot = state.snapshots.get(revision).ok_or_else(|| ToolFailure {
        kind: "snapshot_not_found",
        data: json!({"revision": revision}),
    })?;
    let records = snapshot
        .records
        .iter()
        .map(|(key, record)| json!([{"addressing": "plaintext", "value": key}, record]))
        .collect::<Vec<_>>();
    Ok(json!({
        "revision": revision,
        "bundle": {"schema_version": 1, "records": records}
    }))
}

fn import(state_path: &Path, arguments: &Value) -> Result<Value, ToolFailure> {
    let transaction_id = required_string(arguments, "transaction_id")?;
    let expected_revision = required_string(arguments, "expected_revision")?;
    let bundle = arguments
        .get("bundle")
        .ok_or_else(|| invalid_argument("bundle"))?;
    if bundle.get("schema_version").and_then(Value::as_u64) != Some(1) {
        return Err(invalid_argument("bundle"));
    }
    let records = bundle
        .get("records")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid_argument("bundle"))?;
    let mut imported = BTreeMap::new();
    for entry in records {
        let key = entry
            .pointer("/0/value")
            .and_then(Value::as_str)
            .ok_or_else(|| invalid_argument("bundle"))?;
        let record = entry
            .get(1)
            .cloned()
            .ok_or_else(|| invalid_argument("bundle"))?;
        imported.insert(key.to_owned(), record);
    }

    let (_lock, mut state) = load_locked_state(state_path)?;
    if let Some(completed) = state.completed.get(transaction_id) {
        return Ok(json!(completed));
    }
    if expected_revision != state.current {
        return Err(ToolFailure {
            kind: "conflict",
            data: json!({
                "expected_revision": expected_revision,
                "current_revision": state.current,
                "conflicting_keys": [],
                "recovery_action": "refresh_and_retry"
            }),
        });
    }
    let current = state
        .snapshots
        .get(&state.current)
        .ok_or_else(|| ToolFailure {
            kind: "fake_state_invariant",
            data: json!({"missing_revision": state.current}),
        })?;
    let changed_keys = current
        .records
        .keys()
        .chain(imported.keys())
        .filter(|key| current.records.get(*key) != imported.get(*key))
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    let revision = format!("r{}", state.next_revision);
    state.next_revision += 1;
    state.current.clone_from(&revision);
    state
        .snapshots
        .insert(revision.clone(), Snapshot { records: imported });
    let result = TransactionResult {
        revision,
        changed_keys,
    };
    state
        .completed
        .insert(transaction_id.to_owned(), result.clone());
    save_state(state_path, &state).map_err(state_failure)?;
    Ok(json!(result))
}

fn search(state_path: &Path, arguments: &Value) -> Result<Value, ToolFailure> {
    let query = required_string(arguments, "query")?;
    let limit =
        usize::try_from(arguments.get("limit").and_then(Value::as_u64).unwrap_or(20)).unwrap_or(20);
    let offset =
        usize::try_from(arguments.get("offset").and_then(Value::as_u64).unwrap_or(0)).unwrap_or(0);
    let kind_filter = arguments.get("kind").and_then(Value::as_str);
    let tag_filters: Vec<String> = arguments
        .get("tags")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    let state = load_state(state_path).map_err(state_failure)?;
    let revision = if let Some(rev) = arguments.get("revision").and_then(Value::as_str) {
        rev.to_owned()
    } else {
        state.current.clone()
    };
    let snapshot = state.snapshots.get(&revision).ok_or_else(|| ToolFailure {
        kind: "snapshot_not_found",
        data: json!({"revision": revision}),
    })?;
    let query_lower = query.to_lowercase();
    let hits: Vec<Value> = snapshot
        .records
        .iter()
        .filter_map(|(id, record)| search_hit(id, record, &query_lower, kind_filter, &tag_filters))
        .collect();
    let total = hits.len();
    let has_more = total > limit + offset;
    let page: Vec<Value> = hits.into_iter().skip(offset).take(limit).collect();
    Ok(json!({
        "hits": page,
        "total": total,
        "limit": limit,
        "offset": offset,
        "has_more": has_more,
        "mode": "fts",
        "degraded": true,
        "revision": revision,
    }))
}

fn search_hit(
    id: &str,
    record: &Value,
    query_lower: &str,
    kind_filter: Option<&str>,
    tag_filters: &[String],
) -> Option<Value> {
    let envelope = record.pointer("/envelope")?;
    let content = envelope
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let title = envelope
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let kind = envelope.get("kind").and_then(Value::as_str)?;
    if !content.to_lowercase().contains(query_lower)
        && !title.to_lowercase().contains(query_lower)
        && !kind.to_lowercase().contains(query_lower)
    {
        return None;
    }
    if let Some(kf) = kind_filter
        && kind != kf
    {
        return None;
    }
    let tags: Vec<String> = envelope
        .get("tags")
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    if !tag_filters.iter().all(|tf| tags.contains(tf)) {
        return None;
    }
    Some(json!({
        "id": id,
        "kind": kind,
        "title": envelope.get("title"),
        "content": envelope.get("content"),
        "archived": envelope.pointer("/archive/archived").and_then(Value::as_bool).unwrap_or(false),
        "freshness": envelope.pointer("/freshness/state").and_then(Value::as_str),
        "tags": tags,
        "fts_score": 1.0,
        "vector_score": null,
        "combined_rank": 1.0,
    }))
}

fn backlinks(state_path: &Path, arguments: &Value) -> Result<Value, ToolFailure> {
    let key = required_string(arguments, "key")?;
    let state = load_state(state_path).map_err(state_failure)?;
    let revision = if let Some(rev) = arguments.get("revision").and_then(Value::as_str) {
        rev.to_owned()
    } else {
        state.current.clone()
    };
    let snapshot = state.snapshots.get(&revision).ok_or_else(|| ToolFailure {
        kind: "snapshot_not_found",
        data: json!({"revision": revision}),
    })?;
    let mut entries = Vec::new();
    for (id, record) in &snapshot.records {
        let Some(envelope) = record.pointer("/envelope") else {
            continue;
        };
        let kind = envelope.get("kind").and_then(Value::as_str);
        let title = envelope.get("title").and_then(Value::as_str);
        if let Some(links) = envelope.get("links").and_then(Value::as_array) {
            for link in links {
                if link.get("key").and_then(Value::as_str) == Some(key) {
                    entries.push(json!({
                        "source_id": id,
                        "source_kind": kind,
                        "source_title": title,
                        "relation": link.get("relation").and_then(Value::as_str),
                        "mention_type": "explicit_link",
                    }));
                }
            }
        }
        let content = envelope
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if contains_mention(content, key) {
            entries.push(json!({
                "source_id": id,
                "source_kind": kind,
                "source_title": title,
                "relation": null,
                "mention_type": "body_mention",
            }));
        }
    }
    entries.sort_by(|a, b| {
        a["source_id"]
            .as_str()
            .unwrap_or("")
            .cmp(b["source_id"].as_str().unwrap_or(""))
    });
    Ok(json!({"key": key, "revision": revision, "backlinks": entries}))
}

fn contains_mention(content: &str, key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = content[start..].find(key) {
        let abs = start + pos;
        let before_ok = abs == 0
            || !content
                .as_bytes()
                .get(abs - 1)
                .is_some_and(u8::is_ascii_alphanumeric);
        let after = abs + key.len();
        let after_ok = after >= content.len()
            || !content
                .as_bytes()
                .get(after)
                .is_some_and(u8::is_ascii_alphanumeric);
        if before_ok && after_ok {
            return true;
        }
        start = abs + 1;
    }
    false
}

fn operation_key(operation: &Value) -> Result<&str, ToolFailure> {
    match operation.get("op").and_then(Value::as_str) {
        Some("put") => operation
            .pointer("/record/envelope/key")
            .and_then(Value::as_str),
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

fn pause_before_commit(
    progress_token: Option<&str>,
    output: &mut impl Write,
) -> Result<(), ToolFailure> {
    if let Some(marker) = std::env::var_os("MEMORY_HUB_CONTRACT_PAUSE_BEFORE_REF_UPDATE") {
        fs::write(marker, b"ready").map_err(state_failure)?;
        loop {
            thread::park();
        }
    }
    if std::env::var_os("MEMORY_HUB_CONTRACT_PAUSE_BEFORE_COMMIT").is_none() {
        return Ok(());
    }
    let progress_token = progress_token.ok_or_else(|| invalid_argument("_meta.progressToken"))?;
    write_response(
        output,
        json!({
            "jsonrpc": "2.0",
            "method": "notifications/progress",
            "params": {
                "progressToken": progress_token,
                "progress": 0,
                "total": 1,
                "message": "transaction prepared"
            }
        }),
    )
    .map_err(state_failure)?;
    loop {
        thread::park();
    }
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
    let structured = json!({"error": {"kind": kind, "data": data}});
    let text = structured.to_string();
    json!({
        "isError": true,
        "structuredContent": structured,
        "content": [{"type": "text", "text": text}]
    })
}

fn write_response(output: &mut impl Write, response: Value) -> io::Result<()> {
    serde_json::to_writer(&mut *output, &response).map_err(io::Error::other)?;
    output.write_all(b"\n")?;
    output.flush()
}

#[cfg(test)]
#[allow(clippy::expect_used)]
mod tests {
    use super::{dispatch, list_resources, list_tools};
    use serde_json::{Value, json};

    #[test]
    fn advertised_capabilities_have_discovery_endpoints() {
        let tools = list_tools();
        let tool_names = names(&tools, "tools");
        assert_eq!(
            tool_names,
            [
                "memory_apply_transaction",
                "memory_get_record",
                "memory_diff",
                "memory_export",
                "memory_import",
                "memory_search",
                "memory_backlinks"
            ]
        );

        let resources = list_resources();
        assert_eq!(names(&resources, "resources"), ["Current memory revision"]);
    }

    #[test]
    fn unknown_method_is_a_json_rpc_error() {
        let project = tempfile::tempdir().expect("temporary project should be created");
        let mut output = Vec::new();
        let error = dispatch(
            &project.path().join("state.json"),
            &json!({"method": "unknown"}),
            &mut output,
        )
        .expect_err("unknown method should fail");
        assert_eq!(error.code, -32_601);
        assert_eq!(error.data["kind"], "method_not_found");
        assert!(output.is_empty());
    }

    fn names<'a>(value: &'a Value, field: &str) -> Vec<&'a str> {
        value[field]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|item| item.get("name").and_then(Value::as_str))
            .collect()
    }
}
