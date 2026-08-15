//! Spec-compatible MCP stdio boundary for Git Memory.
//!
//! This crate is the only public machine interface to the canonical store. It
//! deliberately speaks MCP JSON-RPC directly: there is no sibling custom RPC
//! protocol and every bulk mutation maps to one [`GitStore`] transaction.

// JSON values are owned at the one-shot serialization boundary. Moving them
// keeps response construction direct and does not reduce reuse.
#![allow(clippy::needless_pass_by_value)]

use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};

use git_memory_core::{CURRENT_ENVELOPE_VERSION, PolicyResolver, StoredRecord};
use git_memory_index::{IndexError, Projection};
use git_memory_reconcile::{DivergenceMode, ReconcileError, ReconcileErrorKind, Reconciler};
use git_memory_store::{GitStore, Operation, RecordId, Revision, StoreError, Transaction};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
pub const MEMORY_INTERFACE_MAJOR: u16 = 1;
pub const MEMORY_INTERFACE_MINOR: u16 = 1;

/// Run one MCP session until stdin reaches EOF.
///
/// # Errors
///
/// Returns an I/O error when a request or response cannot cross stdio.
pub fn serve(project: &Path) -> io::Result<()> {
    let project = if project.is_absolute() {
        project.to_path_buf()
    } else {
        std::env::current_dir()?.join(project)
    };
    let project = project.canonicalize().unwrap_or(project);
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_io(project, stdin.lock(), stdout.lock())
}

fn serve_io(project: PathBuf, input: impl BufRead, mut output: impl Write) -> io::Result<()> {
    let mut session = Session::new(project);
    for line in input.lines() {
        let line = line?;
        let request = match serde_json::from_str::<Value>(&line) {
            Ok(request) => request,
            Err(error) => {
                write_json(
                    &mut output,
                    &rpc_error(
                        Value::Null,
                        -32_700,
                        "parse error",
                        json!({
                            "kind": "invalid_json", "detail": error.to_string()
                        }),
                    ),
                )?;
                continue;
            }
        };
        if let Some(response) = session.dispatch(&request, &mut output)? {
            write_json(&mut output, &response)?;
        }
    }
    Ok(())
}

struct Session {
    project: PathBuf,
    initialized: bool,
    revision_subscribed: bool,
    reconciliation: Value,
}

impl Session {
    fn new(project: PathBuf) -> Self {
        Self {
            project,
            initialized: false,
            revision_subscribed: false,
            reconciliation: json!({"status": "pending"}),
        }
    }

    fn dispatch(&mut self, request: &Value, output: &mut impl Write) -> io::Result<Option<Value>> {
        let Some(method) = request.get("method").and_then(Value::as_str) else {
            return Ok(Some(rpc_error(
                request.get("id").cloned().unwrap_or(Value::Null),
                -32_600,
                "invalid request",
                json!({"kind": "invalid_request"}),
            )));
        };
        let id = request.get("id").cloned();
        if id.is_none() {
            return Ok(None);
        }
        let id = id.unwrap_or(Value::Null);
        let params = request.get("params").cloned().unwrap_or_else(|| json!({}));
        let result = match method {
            "initialize" => self.initialize(&params),
            "ping" => Ok(json!({})),
            _ if !self.initialized => Err(RpcFailure::new(
                -32_002,
                "server is not initialized",
                json!({"kind": "not_initialized"}),
            )),
            "resources/list" => Ok(list_resources()),
            "resources/templates/list" => Ok(list_resource_templates()),
            "resources/read" => self.read_resource(&params),
            "resources/subscribe" => self.subscribe(&params),
            "resources/unsubscribe" => self.unsubscribe(&params),
            "tools/list" => Ok(list_tools()),
            "tools/call" => return self.call_tool(id, &params, output).map(Some),
            _ => Err(RpcFailure::new(
                -32_601,
                "method not found",
                json!({"kind": "method_not_found", "method": method}),
            )),
        };
        Ok(Some(match result {
            Ok(result) => rpc_result(id, result),
            Err(error) => rpc_error(id, error.code, error.message, error.data),
        }))
    }

    fn initialize(&mut self, params: &Value) -> Result<Value, RpcFailure> {
        let requested_protocol = required_string(params, "protocolVersion")?;
        if requested_protocol != MCP_PROTOCOL_VERSION {
            return Err(RpcFailure::new(
                -32_002,
                "unsupported MCP protocol revision",
                json!({
                    "kind": "incompatible_mcp_revision",
                    "received": requested_protocol,
                    "supported": MCP_PROTOCOL_VERSION
                }),
            ));
        }
        if let Some(received) = requested_memory_major(params)
            && received != MEMORY_INTERFACE_MAJOR
        {
            return Err(RpcFailure::new(
                -32_002,
                "incompatible Git Memory interface",
                json!({
                    "kind": "incompatible_memory_interface",
                    "received_major": received,
                    "supported_major": MEMORY_INTERFACE_MAJOR,
                    "recovery_action": "install_compatible_git_memory"
                }),
            ));
        }
        self.reconciliation = match Reconciler::open(&self.project)
            .and_then(|reconciler| reconciler.reconcile(DivergenceMode::Report))
        {
            Ok(report) => json!({"status": "ok", "report": report}),
            Err(error) if error.kind == ReconcileErrorKind::Diverged => {
                json!({"status": "diverged", "error": error})
            }
            Err(error) => return Err(RpcFailure::reconcile(error)),
        };
        let store = self.store()?;
        Projection::synchronize_store(&store).map_err(RpcFailure::index)?;
        let handshake = self.handshake();
        self.initialized = true;
        Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {
                "resources": {"subscribe": true, "listChanged": false},
                "tools": {"listChanged": false},
                "experimental": {"gitMemory": handshake}
            },
            "serverInfo": {"name": "git-memory", "version": env!("CARGO_PKG_VERSION")},
            "instructions": "Read memory://revision/current after each resource update notification.",
            "_meta": {"gitMemory": handshake}
        }))
    }

    fn handshake(&self) -> Value {
        let canonical = self
            .project
            .canonicalize()
            .unwrap_or_else(|_| self.project.clone());
        let project_id = stable_id("project", canonical.to_string_lossy().as_bytes());
        let executable = std::env::current_exe().unwrap_or_default();
        let mut installation_source = executable.to_string_lossy().into_owned();
        installation_source.push('\0');
        installation_source.push_str(env!("CARGO_PKG_VERSION"));
        let installation_id = stable_id("installation", installation_source.as_bytes());
        let git_dir = GitStore::discover_git_dir(&canonical).ok();
        json!({
            "memoryInterfaceVersion": version(MEMORY_INTERFACE_MAJOR, MEMORY_INTERFACE_MINOR),
            "storeVersion": version(1, 1),
            "envelopeVersion": CURRENT_ENVELOPE_VERSION,
            "indexVersion": version(1, 0),
            "modelFingerprint": null,
            "encryptionMode": "plaintext",
            "installationId": installation_id,
            "projectId": project_id,
            "projectPath": canonical,
            "gitDir": git_dir,
            "reconciliation": self.reconciliation
        })
    }

    fn store(&self) -> Result<GitStore, RpcFailure> {
        GitStore::open(&self.project).map_err(RpcFailure::store)
    }

    fn subscribe(&mut self, params: &Value) -> Result<Value, RpcFailure> {
        let uri = required_string(params, "uri")?;
        if uri != "memory://revision/current" {
            return Err(resource_not_found(uri));
        }
        self.revision_subscribed = true;
        Ok(json!({}))
    }

    fn unsubscribe(&mut self, params: &Value) -> Result<Value, RpcFailure> {
        let uri = required_string(params, "uri")?;
        if uri != "memory://revision/current" {
            return Err(resource_not_found(uri));
        }
        self.revision_subscribed = false;
        Ok(json!({}))
    }

    fn read_resource(&self, params: &Value) -> Result<Value, RpcFailure> {
        let uri = required_string(params, "uri")?;
        let content = match uri {
            "memory://project" => {
                let store = self.store()?;
                let mut value = self.handshake();
                value["gitDir"] = json!(store.git_dir());
                value
            }
            "memory://revision/current" => {
                let snapshot = self.store()?.current().map_err(RpcFailure::store)?;
                json!({"schemaVersion": 1, "revision": snapshot.revision()})
            }
            "memory://index/status" => {
                let status = Projection::status_store(&self.store()?).map_err(RpcFailure::index)?;
                json!({
                    "schemaVersion": status.schema_version,
                    "available": true,
                    "state": status.state,
                    "canonicalRevision": status.canonical_revision,
                    "targetRevision": status.target_revision
                })
            }
            "memory://model/status" => unavailable_status("model", "GITMEMO-8"),
            "memory://policy/effective" => policy_resource(),
            "memory://encryption/status" => json!({
                "schemaVersion": 1,
                "mode": "plaintext",
                "available": true,
                "encryptedStoreAvailable": false,
                "encryptedIndexAvailable": false
            }),
            _ => {
                if let Some(key) = uri.strip_prefix("memory://records/") {
                    let snapshot = self.store()?.current().map_err(RpcFailure::store)?;
                    let record = snapshot
                        .get(&RecordId::plaintext(key))
                        .map_err(RpcFailure::store)?;
                    json!({"schemaVersion": 1, "revision": snapshot.revision(), "record": record})
                } else {
                    return Err(resource_not_found(uri));
                }
            }
        };
        Ok(json!({"contents": [{
            "uri": uri,
            "mimeType": "application/json",
            "text": content.to_string()
        }]}))
    }

    fn call_tool(
        &mut self,
        id: Value,
        params: &Value,
        output: &mut impl Write,
    ) -> io::Result<Value> {
        let name = match required_string(params, "name") {
            Ok(name) => name,
            Err(error) => return Ok(rpc_error(id, error.code, error.message, error.data)),
        };
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let reconciliation_changed = if matches!(
            name,
            "memory_apply_transaction" | "memory_checkpoint" | "memory_import"
        ) {
            match self.reconcile_before_mutation() {
                Ok(changed) => changed,
                Err(error) => return Ok(rpc_result(id, tool_error(error))),
            }
        } else {
            false
        };
        let result = self.execute_tool(name, &arguments);
        if reconciliation_changed && result.is_err() && self.revision_subscribed {
            write_json(
                output,
                &json!({
                    "jsonrpc": "2.0",
                    "method": "notifications/resources/updated",
                    "params": {"uri": "memory://revision/current"}
                }),
            )?;
        }
        match result {
            Ok(ToolOutcome {
                content,
                revision_changed,
            }) => {
                if revision_changed || reconciliation_changed {
                    let index_result = self.store().and_then(|store| {
                        Projection::synchronize_store(&store).map_err(RpcFailure::index)
                    });
                    if let Err(error) = index_result {
                        return Ok(rpc_result(id, tool_error(error.into_tool_failure())));
                    }
                }
                if (revision_changed || reconciliation_changed) && self.revision_subscribed {
                    write_json(
                        output,
                        &json!({
                            "jsonrpc": "2.0",
                            "method": "notifications/resources/updated",
                            "params": {"uri": "memory://revision/current"}
                        }),
                    )?;
                }
                Ok(rpc_result(id, tool_success(content)))
            }
            Err(ToolCallFailure::Rpc(error)) => {
                if error.data.get("kind").and_then(Value::as_str) == Some("tool_not_found") {
                    Ok(rpc_error(id, error.code, error.message, error.data))
                } else {
                    Ok(rpc_result(id, tool_error(error.into_tool_failure())))
                }
            }
            Err(ToolCallFailure::Tool(error)) => Ok(rpc_result(id, tool_error(error))),
        }
    }

    fn execute_tool(&self, name: &str, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        match name {
            "memory_apply_transaction" => self.apply_transaction(arguments),
            "memory_get_record" => self.get_record(arguments),
            "memory_list_records" => self.list_records(arguments),
            "memory_checkpoint" => self.checkpoint(arguments),
            "memory_history" => self.history(arguments),
            "memory_diff" => self.diff(arguments),
            "memory_export" => self.export(arguments),
            "memory_import" => self.import(arguments),
            "memory_doctor" => self.doctor(),
            "memory_reconcile" => self.reconcile(arguments),
            "memory_reindex" => self.reindex(),
            "memory_search" | "memory_backlinks" => {
                Err(unavailable("search_index", "GITMEMO-7/GITMEMO-9"))
            }
            "memory_transport_status" | "memory_fetch" | "memory_push" => {
                Err(unavailable("remote_transport", "GITMEMO-10"))
            }
            "memory_model_status" => Err(unavailable("embedding_model", "GITMEMO-8")),
            "memory_encryption_status" => Ok(ToolOutcome::read(json!({
                "mode": "plaintext", "encryptedStoreAvailable": false,
                "encryptedIndexAvailable": false
            }))),
            _ => Err(ToolCallFailure::Rpc(RpcFailure::new(
                -32_602,
                "tool not found",
                json!({"kind": "tool_not_found", "name": name}),
            ))),
        }
    }

    fn apply_transaction(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let transaction_id = required_string(arguments, "transaction_id")?.to_owned();
        let expected_revision = parse_field(arguments, "expected_revision")?;
        let raw = arguments
            .get("operations")
            .and_then(Value::as_array)
            .ok_or_else(|| RpcFailure::invalid_argument("operations"))?;
        if raw.is_empty() {
            return Err(RpcFailure::invalid_argument("operations").into());
        }
        let operations = raw
            .iter()
            .map(parse_operation)
            .collect::<Result<Vec<_>, _>>()?;
        let result = self
            .store()?
            .apply(&Transaction {
                id: transaction_id,
                expected_revision,
                operations,
            })
            .map_err(ToolFailure::store)?;
        Ok(ToolOutcome::mutation(json!(result)))
    }

    fn get_record(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let key = required_string(arguments, "key")?;
        let revision: Revision = parse_field(arguments, "revision")?;
        let snapshot = self
            .store()?
            .snapshot(&revision)
            .map_err(ToolFailure::store)?;
        let record = snapshot
            .get(&RecordId::plaintext(key))
            .map_err(ToolFailure::store)?;
        Ok(ToolOutcome::read(
            json!({"revision": revision, "record": record}),
        ))
    }

    fn list_records(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let store = self.store()?;
        let snapshot = match arguments.get("revision") {
            Some(value) => store.snapshot(
                &serde_json::from_value(value.clone())
                    .map_err(|_| RpcFailure::invalid_argument("revision"))?,
            ),
            None => store.current(),
        }
        .map_err(ToolFailure::store)?;
        let records = snapshot.records().map_err(ToolFailure::store)?;
        Ok(ToolOutcome::read(
            json!({"revision": snapshot.revision(), "records": records}),
        ))
    }

    fn checkpoint(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let message = required_string(arguments, "message")?;
        let checkpoint = self
            .store()?
            .checkpoint(message)
            .map_err(ToolFailure::store)?;
        Ok(ToolOutcome::read(json!(checkpoint)))
    }

    fn history(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(100);
        let limit = usize::try_from(limit.min(1_000)).unwrap_or(1_000);
        let history = self.store()?.history(limit).map_err(ToolFailure::store)?;
        Ok(ToolOutcome::read(json!({"checkpoints": history})))
    }

    fn diff(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let from = parse_field(arguments, "from_revision")?;
        let to = parse_field(arguments, "to_revision")?;
        let changes = self.store()?.diff(&from, &to).map_err(ToolFailure::store)?;
        Ok(ToolOutcome::read(
            json!({"fromRevision": from, "toRevision": to, "changes": changes}),
        ))
    }

    fn export(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let revision = parse_field(arguments, "revision")?;
        let bytes = self
            .store()?
            .export(&revision)
            .map_err(ToolFailure::store)?;
        let bundle: Value = serde_json::from_slice(&bytes).map_err(|_| {
            RpcFailure::new(
                -32_603,
                "store returned invalid export JSON",
                json!({"kind": "repository"}),
            )
        })?;
        Ok(ToolOutcome::read(
            json!({"revision": revision, "bundle": bundle}),
        ))
    }

    fn import(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let transaction_id = required_string(arguments, "transaction_id")?;
        let expected_revision = parse_field(arguments, "expected_revision")?;
        let bundle = arguments
            .get("bundle")
            .ok_or_else(|| RpcFailure::invalid_argument("bundle"))?;
        let bytes =
            serde_json::to_vec(bundle).map_err(|_| RpcFailure::invalid_argument("bundle"))?;
        let result = self
            .store()?
            .import(transaction_id, expected_revision, &bytes)
            .map_err(ToolFailure::store)?;
        Ok(ToolOutcome::mutation(json!(result)))
    }

    fn doctor(&self) -> Result<ToolOutcome, ToolCallFailure> {
        let store = self.store()?;
        let current = store.current().map_err(ToolFailure::store)?;
        Ok(ToolOutcome::read(json!({
            "schemaVersion": 1,
            "healthy": true,
            "gitDir": store.git_dir(),
            "revision": current.revision()
        })))
    }

    fn reindex(&self) -> Result<ToolOutcome, ToolCallFailure> {
        let store = self.store()?;
        let status = Projection::synchronize_store(&store).map_err(ToolFailure::index)?;
        Ok(ToolOutcome::read(json!(status)))
    }

    fn reconcile(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let mode = match arguments.get("divergence").and_then(Value::as_str) {
            None | Some("report") => DivergenceMode::Report,
            Some("full_rebuild") => DivergenceMode::FullRebuild,
            Some(_) => return Err(RpcFailure::invalid_argument("divergence").into()),
        };
        let report = Reconciler::open(&self.project)
            .and_then(|reconciler| reconciler.reconcile(mode))
            .map_err(ToolFailure::reconcile)?;
        let revision_changed = report
            .processed
            .iter()
            .any(|commit| !commit.stale_keys.is_empty());
        Ok(ToolOutcome {
            content: json!(report),
            revision_changed,
        })
    }

    fn reconcile_before_mutation(&self) -> Result<bool, ToolFailure> {
        let report = Reconciler::open(&self.project)
            .and_then(|reconciler| reconciler.reconcile(DivergenceMode::Report))
            .map_err(ToolFailure::reconcile)?;
        Ok(report
            .processed
            .iter()
            .any(|commit| !commit.stale_keys.is_empty()))
    }
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum WireOperation {
    Put {
        record: StoredRecord,
    },
    Delete {
        key: Option<String>,
        id: Option<RecordId>,
    },
}

fn parse_operation(value: &Value) -> Result<Operation, RpcFailure> {
    let wire: WireOperation = serde_json::from_value(value.clone()).map_err(|_| {
        let field = if value.get("op").and_then(Value::as_str) == Some("delete") {
            "key"
        } else {
            "operation"
        };
        RpcFailure::invalid_argument(field)
    })?;
    match wire {
        WireOperation::Put { record } => Ok(Operation::put(record)),
        WireOperation::Delete {
            key: Some(key),
            id: None,
        } => Ok(Operation::delete(RecordId::plaintext(key))),
        WireOperation::Delete {
            key: None,
            id: Some(id),
        } => Ok(Operation::delete(id)),
        WireOperation::Delete { .. } => Err(RpcFailure::invalid_argument("key")),
    }
}

fn list_resources() -> Value {
    let resources = [
        ("Git Memory project", "memory://project"),
        ("Current revision", "memory://revision/current"),
        ("Index status", "memory://index/status"),
        ("Model status", "memory://model/status"),
        ("Effective policy", "memory://policy/effective"),
        ("Encryption status", "memory://encryption/status"),
    ]
    .into_iter()
    .map(|(name, uri)| json!({"name": name, "uri": uri, "mimeType": "application/json"}))
    .collect::<Vec<_>>();
    json!({"resources": resources})
}

fn list_resource_templates() -> Value {
    json!({"resourceTemplates": [{
        "name": "Memory record",
        "uriTemplate": "memory://records/{key}",
        "mimeType": "application/json",
        "description": "Current canonical record identified by its plaintext key"
    }]})
}

#[allow(clippy::too_many_lines)]
fn list_tools() -> Value {
    let mut tools = vec![
        tool(
            "memory_apply_transaction",
            "Atomically apply one bulk put/delete transaction",
            object_schema(
                &[
                    ("transaction_id", string_schema()),
                    ("expected_revision", string_schema()),
                    (
                        "operations",
                        json!({"type":"array","minItems":1,"items":{"type":"object"}}),
                    ),
                ],
                &["transaction_id", "expected_revision", "operations"],
            ),
        ),
        tool(
            "memory_get_record",
            "Read one record from an immutable revision",
            object_schema(
                &[("key", string_schema()), ("revision", string_schema())],
                &["key", "revision"],
            ),
        ),
        tool(
            "memory_list_records",
            "List records from an immutable revision",
            object_schema(&[("revision", string_schema())], &[]),
        ),
        tool(
            "memory_checkpoint",
            "Checkpoint the current staged revision",
            object_schema(&[("message", string_schema())], &["message"]),
        ),
        tool(
            "memory_history",
            "List checkpoint history newest first",
            object_schema(
                &[(
                    "limit",
                    json!({"type":"integer","minimum":0,"maximum":1000}),
                )],
                &[],
            ),
        ),
        tool(
            "memory_diff",
            "Diff record identities between two revisions",
            object_schema(
                &[
                    ("from_revision", string_schema()),
                    ("to_revision", string_schema()),
                ],
                &["from_revision", "to_revision"],
            ),
        ),
        tool(
            "memory_export",
            "Export a deterministic record bundle",
            object_schema(&[("revision", string_schema())], &["revision"]),
        ),
        tool(
            "memory_import",
            "Replace records from one bundle in one transaction",
            object_schema(
                &[
                    ("transaction_id", string_schema()),
                    ("expected_revision", string_schema()),
                    ("bundle", json!({"type":"object"})),
                ],
                &["transaction_id", "expected_revision", "bundle"],
            ),
        ),
        tool(
            "memory_reconcile",
            "Reconcile code history with Memory checkpoints",
            object_schema(
                &[(
                    "divergence",
                    json!({"type":"string","enum":["report","full_rebuild"]}),
                )],
                &[],
            ),
        ),
        tool(
            "memory_doctor",
            "Validate repository and canonical store access",
            object_schema(&[], &[]),
        ),
        tool(
            "memory_encryption_status",
            "Report the active encryption mode",
            object_schema(&[], &[]),
        ),
    ];
    for (name, description) in [
        ("memory_search", "Search the derived Memory index"),
        ("memory_backlinks", "Find records linking to a record"),
        ("memory_reindex", "Rebuild the derived Memory index"),
        (
            "memory_transport_status",
            "Report Memory remote exchange status",
        ),
        ("memory_fetch", "Fetch from the configured Memory remote"),
        ("memory_push", "Push to the configured Memory remote"),
        ("memory_model_status", "Report embedding model status"),
    ] {
        tools.push(tool(name, description, object_schema(&[], &[])));
    }
    json!({"tools": tools})
}

fn tool(name: &str, description: &str, input_schema: Value) -> Value {
    json!({"name": name, "description": description, "inputSchema": input_schema})
}

fn object_schema(properties: &[(&str, Value)], required: &[&str]) -> Value {
    let properties = properties
        .iter()
        .map(|(key, value)| ((*key).to_owned(), value.clone()))
        .collect::<Map<_, _>>();
    json!({"type": "object", "properties": properties, "required": required, "additionalProperties": false})
}

fn string_schema() -> Value {
    json!({"type": "string", "minLength": 1})
}

fn policy_resource() -> Value {
    let resolver = PolicyResolver::git_memory_defaults();
    let events = [
        "reconcile_divergence",
        "memory_push_stale",
        "code_push_stale",
        "dangling_links",
        "index_lag",
    ];
    let policies = events
        .into_iter()
        .filter_map(|event| resolver.resolve(event, None).ok())
        .collect::<Vec<_>>();
    json!({"schemaVersion": 1, "policies": policies})
}

fn unavailable_status(capability: &str, planned_spec: &str) -> Value {
    json!({"schemaVersion": 1, "available": false, "capability": capability, "plannedSpec": planned_spec})
}

fn unavailable(capability: &'static str, planned_spec: &'static str) -> ToolCallFailure {
    ToolFailure {
        kind: "capability_unavailable".to_owned(),
        message: format!("{capability} is not implemented by this release"),
        data: json!({"capability": capability, "planned_spec": planned_spec, "recovery_action": "upgrade_when_available"}),
    }
    .into()
}

fn requested_memory_major(params: &Value) -> Option<u16> {
    [
        "/_meta/gitMemory/memoryInterfaceVersion/major",
        "/_meta/memoryInterfaceVersion/major",
        "/_meta/memory_interface_version/major",
    ]
    .into_iter()
    .find_map(|pointer| {
        params
            .pointer(pointer)
            .and_then(Value::as_u64)
            .and_then(|major| u16::try_from(major).ok())
    })
}

fn stable_id(namespace: &str, bytes: &[u8]) -> String {
    let mut digest = Sha256::new();
    digest.update(namespace.as_bytes());
    digest.update([0]);
    digest.update(bytes);
    format!("{namespace}-{:x}", digest.finalize())
}

fn version(major: u16, minor: u16) -> Value {
    json!({"major": major, "minor": minor})
}

fn required_string<'a>(value: &'a Value, field: &str) -> Result<&'a str, RpcFailure> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| RpcFailure::invalid_argument(field))
}

fn parse_field<T: serde::de::DeserializeOwned>(
    value: &Value,
    field: &str,
) -> Result<T, RpcFailure> {
    let raw = value
        .get(field)
        .cloned()
        .ok_or_else(|| RpcFailure::invalid_argument(field))?;
    serde_json::from_value(raw).map_err(|_| RpcFailure::invalid_argument(field))
}

fn resource_not_found(uri: &str) -> RpcFailure {
    RpcFailure::new(
        -32_602,
        "resource not found",
        json!({"kind": "resource_not_found", "uri": uri}),
    )
}

fn rpc_result(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn rpc_error(id: Value, code: i64, message: &str, data: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "data": data}})
}

fn write_json(output: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *output, value)?;
    output.write_all(b"\n")?;
    output.flush()
}

fn tool_success(content: Value) -> Value {
    json!({"content": [{"type": "text", "text": content.to_string()}], "structuredContent": content, "isError": false})
}

fn tool_error(error: ToolFailure) -> Value {
    let content =
        json!({"error": {"kind": error.kind, "message": error.message, "data": error.data}});
    json!({"content": [{"type": "text", "text": content.to_string()}], "structuredContent": content, "isError": true})
}

struct ToolOutcome {
    content: Value,
    revision_changed: bool,
}

impl ToolOutcome {
    const fn read(content: Value) -> Self {
        Self {
            content,
            revision_changed: false,
        }
    }
    const fn mutation(content: Value) -> Self {
        Self {
            content,
            revision_changed: true,
        }
    }
}

#[derive(Debug)]
struct RpcFailure {
    code: i64,
    message: &'static str,
    data: Value,
}

impl RpcFailure {
    const fn new(code: i64, message: &'static str, data: Value) -> Self {
        Self {
            code,
            message,
            data,
        }
    }
    fn invalid_argument(field: &str) -> Self {
        Self::new(
            -32_602,
            "invalid tool arguments",
            json!({"kind": "invalid_argument", "field": field}),
        )
    }
    fn store(error: StoreError) -> Self {
        Self::new(
            -32_603,
            "Git Memory store unavailable",
            json!({"kind": snake_store_kind(error.kind), "message": error.message, "data": error.data}),
        )
    }
    fn reconcile(error: ReconcileError) -> Self {
        Self::new(
            -32_603,
            "Git Memory reconciliation unavailable",
            json!({
                "kind": snake_reconcile_kind(error.kind),
                "message": error.message,
                "data": error.data
            }),
        )
    }
    fn index(error: IndexError) -> Self {
        Self::new(
            -32_603,
            "Git Memory index unavailable",
            json!({"kind": "index", "message": error.to_string()}),
        )
    }

    fn into_tool_failure(self) -> ToolFailure {
        let kind = self
            .data
            .get("kind")
            .and_then(Value::as_str)
            .unwrap_or("internal")
            .to_owned();
        let message = self
            .data
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or(self.message)
            .to_owned();
        let data = self.data.get("data").cloned().unwrap_or_else(|| {
            let mut data = self.data.as_object().cloned().unwrap_or_default();
            data.remove("kind");
            Value::Object(data)
        });
        ToolFailure {
            kind,
            message,
            data,
        }
    }
}

struct ToolFailure {
    kind: String,
    message: String,
    data: Value,
}

impl ToolFailure {
    fn store(error: StoreError) -> Self {
        Self {
            kind: snake_store_kind(error.kind).to_owned(),
            message: error.message,
            data: error.data,
        }
    }
    fn reconcile(error: ReconcileError) -> Self {
        Self {
            kind: snake_reconcile_kind(error.kind).to_owned(),
            message: error.message,
            data: error.data,
        }
    }
    fn index(error: IndexError) -> Self {
        Self {
            kind: "index".into(),
            message: error.to_string(),
            data: Value::Null,
        }
    }
}

enum ToolCallFailure {
    Rpc(RpcFailure),
    Tool(ToolFailure),
}
impl From<RpcFailure> for ToolCallFailure {
    fn from(value: RpcFailure) -> Self {
        Self::Rpc(value)
    }
}
impl From<ToolFailure> for ToolCallFailure {
    fn from(value: ToolFailure) -> Self {
        Self::Tool(value)
    }
}

fn snake_store_kind(kind: git_memory_store::StoreErrorKind) -> &'static str {
    use git_memory_store::StoreErrorKind;
    match kind {
        StoreErrorKind::InvalidArgument => "invalid_argument",
        StoreErrorKind::InvalidRecord => "invalid_record",
        StoreErrorKind::RevisionNotFound => "revision_not_found",
        StoreErrorKind::Conflict => "conflict",
        StoreErrorKind::TransactionReused => "transaction_reused",
        StoreErrorKind::Repository => "repository",
        StoreErrorKind::RetryExhausted => "retry_exhausted",
    }
}

fn snake_reconcile_kind(kind: ReconcileErrorKind) -> &'static str {
    match kind {
        ReconcileErrorKind::InvalidProject => "invalid_project",
        ReconcileErrorKind::Repository => "repository",
        ReconcileErrorKind::Cursor => "cursor",
        ReconcileErrorKind::Diverged => "diverged",
        ReconcileErrorKind::Store => "store",
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{MCP_PROTOCOL_VERSION, MEMORY_INTERFACE_MAJOR, Session, serve_io};
    use serde_json::{Value, json};
    use sha2::{Digest, Sha256};

    #[test]
    fn incompatible_interface_fails_before_creating_memory_refs() {
        let project = tempfile::tempdir().unwrap();
        git2_for_test::init(project.path());
        let input = format!(
            "{}\n",
            json!({
                "jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {}, "clientInfo":{"name":"test","version":"1"},
                    "_meta":{"gitMemory":{"memoryInterfaceVersion":{"major": MEMORY_INTERFACE_MAJOR + 1,"minor":0}}}
                }
            })
        );
        let mut output = Vec::new();
        serve_io(project.path().to_path_buf(), input.as_bytes(), &mut output).unwrap();
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            response.pointer("/error/data/kind").and_then(Value::as_str),
            Some("incompatible_memory_interface")
        );
        assert!(!project.path().join(".git/refs/memory/staged").exists());
    }

    #[test]
    fn subscribed_mutation_notifies_and_revision_remains_authoritative() {
        let project = tempfile::tempdir().unwrap();
        git2_for_test::init(project.path());
        let mut session = Session::new(project.path().to_path_buf());
        session.initialized = true;
        session.revision_subscribed = true;
        let base = session
            .store()
            .unwrap()
            .current()
            .unwrap()
            .revision()
            .clone();
        let content = "notification fixture";
        let record = json!({
            "representation": "plaintext",
            "envelope": {
                "envelope_version": {"major": 1, "minor": 0},
                "key": "notice",
                "kind": "note",
                "content": content,
                "content_hash": format!("sha256:{:x}", Sha256::digest(content.as_bytes())),
                "source_paths": {}, "archive": {"archived": false},
                "freshness": {"state": "unverified"}
            }
        });
        let params = json!({
            "name": "memory_apply_transaction",
            "arguments": {
                "transaction_id": "notification-test",
                "expected_revision": base,
                "operations": [{"op": "put", "record": record}]
            }
        });
        let mut output = Vec::new();
        let response = session.call_tool(json!(7), &params, &mut output).unwrap();
        let notification: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            notification.get("method").and_then(Value::as_str),
            Some("notifications/resources/updated")
        );
        let applied = response
            .pointer("/result/structuredContent/revision")
            .and_then(Value::as_str)
            .unwrap();
        let resource = session
            .read_resource(&json!({"uri": "memory://revision/current"}))
            .unwrap();
        let text = resource
            .pointer("/contents/0/text")
            .and_then(Value::as_str)
            .unwrap();
        let reread: Value = serde_json::from_str(text).unwrap();
        assert_eq!(
            reread.get("revision").and_then(Value::as_str),
            Some(applied)
        );
    }

    mod git2_for_test {
        use std::path::Path;
        pub fn init(path: &Path) {
            let status = std::process::Command::new("git")
                .args(["init", "--quiet"])
                .arg(path)
                .status()
                .unwrap();
            assert!(status.success());
        }
    }
}
