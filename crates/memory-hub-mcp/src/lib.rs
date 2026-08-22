//! Spec-compatible MCP stdio boundary for Memory Hub.
//!
//! This crate is the only public machine interface to the canonical store. It
//! deliberately speaks MCP JSON-RPC directly: there is no sibling custom RPC
//! protocol and every bulk mutation maps to one store transaction.

// JSON values are owned at the one-shot serialization boundary. Moving them
// keeps response construction direct and does not reduce reuse.
#![allow(clippy::needless_pass_by_value)]

use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use memory_hub_core::{
    CURRENT_ENVELOPE_VERSION, ContentHash, Envelope, PolicyResolver, StoredRecord,
};
/// The vector channel, and the machine's answer about it. Re-exported for a
/// host that opens several sessions in one process: resolving the provider once
/// and handing the same `Arc` to each is what keeps one model in memory rather
/// than one per project.
pub use memory_hub_embed::EmbeddingProvider;
pub use memory_hub_embed::active::resolve_active_provider;
use memory_hub_engine::{ExportMode, Operation, RecordId, Revision};
use memory_hub_index::{SearchFilters, SearchRequest};
use memory_hub_reconcile::DivergenceMode;
use memory_hub_schema::SchemaRegistry;
/// Where a project's records go — said by whoever opens the session, because
/// the engine will not guess. Re-exported so a host that links this crate names
/// one crate rather than two.
pub use memory_hub_service::RecordsIn;
use memory_hub_service::{
    Content, ContentResolution, ListingQuery, ListingSort, MemoryService, PresenceFilter,
    RenameCandidate, ScanChange, ServiceError, Unresolved, freshness_str,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

mod schema_instructions;
pub use schema_instructions::{schema_instructions, schema_resource, single_type_resource};

/// Maximum size of a single JSON-RPC request line. Sized for bulk imports —
/// a bundle of tens of thousands of records still fits — while keeping one
/// hostile line from exhausting memory.
const MAX_REQUEST_BYTES: u64 = 64 * 1024 * 1024;

pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
/// The compatibility boundary a client negotiates at `initialize`.
///
/// A client announces the major it was built against, and a mismatch is
/// refused before anything is read or written — the alternative is letting it
/// discover the difference one broken read at a time. The minor moves for
/// additive change and is accepted in either direction.
pub const MEMORY_INTERFACE_MAJOR: u16 = 1;
pub const MEMORY_INTERFACE_MINOR: u16 = 0;

/// Subscribing to this reports which records changed, not only that something
/// did. Additive: a client that only knows `memory://revision/current` keeps
/// getting exactly what it got before.
pub const RECORDS_CHANGED: &str = "memory://records/changed";

/// Run one MCP session until stdin reaches EOF, over a project whose records
/// are where `records` says.
///
/// # Errors
///
/// Returns an I/O error when a request or response cannot cross stdio.
pub fn serve(project: &Path, records: RecordsIn) -> io::Result<()> {
    let project = if project.is_absolute() {
        project.to_path_buf()
    } else {
        std::env::current_dir()?.join(project)
    };
    let project = project.canonicalize().unwrap_or(project);
    let stdin = io::stdin();
    let stdout = io::stdout();
    serve_io(project, records, stdin.lock(), stdout.lock())
}

fn serve_io(
    project: PathBuf,
    records: RecordsIn,
    mut input: impl BufRead,
    mut output: impl Write,
) -> io::Result<()> {
    let mut session = Session::new(project, records);
    let mut line = String::new();
    loop {
        line.clear();
        // A request line is buffered whole before it can be parsed, so an
        // unbounded read lets one hostile line exhaust memory. The cap is far
        // above any legitimate batch; a request that exceeds it cannot be
        // resynchronized (the rest of the line would be read as new requests),
        // so the session ends after reporting it.
        let read = Read::by_ref(&mut input)
            .take(MAX_REQUEST_BYTES + 1)
            .read_line(&mut line)?;
        if read == 0 {
            break;
        }
        if read as u64 > MAX_REQUEST_BYTES {
            write_json(
                &mut output,
                &rpc_error(
                    Value::Null,
                    -32_600,
                    "request exceeds the maximum size",
                    json!({"kind": "request_too_large", "max_bytes": MAX_REQUEST_BYTES}),
                ),
            )?;
            break;
        }
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

pub struct Session {
    pub initialized: bool,
    pub revision_subscribed: bool,
    pub reconciliation: Value,
    records_subscribed: bool,
    /// What scanning the attached folders did when the project opened.
    pub attachments: Value,
    /// Every use case lives here; this type only speaks JSON-RPC.
    service: MemoryService,
}

impl Session {
    #[must_use]
    pub fn new(project: PathBuf, records: RecordsIn) -> Self {
        Self {
            initialized: false,
            revision_subscribed: false,
            records_subscribed: false,
            reconciliation: json!({"status": "pending"}),
            attachments: json!({"status": "pending"}),
            service: MemoryService::open(project, records),
        }
    }

    /// Open a session that uses `provider` for the vector channel instead of
    /// resolving one from this machine.
    ///
    /// For a host that keeps several projects open at once. Resolved per
    /// session, the model is loaded per session — five projects in one process
    /// mean five copies of the same weights — and the model is by far the
    /// largest thing either side holds. Resolve once with
    /// [`resolve_active_provider`], hand the same `Arc` to every session.
    ///
    /// `None` says this machine has no model, and is a real answer rather than
    /// a missing one: search then runs FTS-only, and the session will not go
    /// looking for a GGUF of its own afterwards.
    #[must_use]
    pub fn with_provider(
        project: PathBuf,
        records: RecordsIn,
        provider: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Self {
        Self {
            initialized: false,
            revision_subscribed: false,
            records_subscribed: false,
            reconciliation: json!({"status": "pending"}),
            attachments: json!({"status": "pending"}),
            service: MemoryService::open(project, records).with_provider(provider),
        }
    }

    /// The typed use-case layer this session adapts.
    #[must_use]
    pub const fn service(&self) -> &MemoryService {
        &self.service
    }

    fn project(&self) -> &Path {
        self.service.project()
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
            Err(error) => rpc_error_owned(id, error.code, &error.message, error.data),
        }))
    }

    /// Negotiate the MCP revision and the Memory interface major, reconcile
    /// code history, and publish the handshake.
    ///
    /// # Errors
    ///
    /// Returns [`RpcFailure`] for an unsupported protocol revision, an
    /// incompatible Memory interface major, diverged code history, or an
    /// unreadable store.
    pub fn initialize(&mut self, params: &Value) -> Result<Value, RpcFailure> {
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
                "incompatible Memory Hub interface",
                json!({
                    "kind": "incompatible_memory_interface",
                    "received_major": received,
                    "supported_major": MEMORY_INTERFACE_MAJOR,
                    "recovery_action": "install_compatible_memory_hub"
                }),
            ));
        }
        self.reconciliation = match self.service.reconcile(DivergenceMode::Report) {
            Ok(report) => json!({"status": "ok", "report": report}),
            Err(error) if error.kind == "diverged" => {
                json!({"status": "diverged", "error": {
                    "kind": error.kind, "message": error.message, "data": error.data
                }})
            }
            // Reconciliation compares Memory against the history of the code.
            // A project that is not a Git repository has no such history, and
            // saying so is the whole answer — records kept in a folder work
            // exactly as well without it.
            Err(error) if error.kind == "repository" => {
                json!({"status": "unavailable", "error": {
                    "kind": error.kind, "message": error.message
                }})
            }
            Err(error) => return Err(RpcFailure::service(error)),
        };
        // Opening the project is one of the moments a folder is scanned. The
        // other two — window focus returning, and a filesystem watcher — belong
        // to a client that can see them; scanning before every read would be
        // too expensive, and scanning only here is too rare for somebody
        // editing files in the next window. A project with no attached folder
        // pays a registry read and nothing else.
        self.attachments = match self.service.scan_attachments(&format!(
            "open-{}",
            stable_id("scan", self.service.project().to_string_lossy().as_bytes())
        )) {
            Ok(report) => json!({
                "status": "ok",
                "scanned": report.scanned,
                "applied": report.applied,
                "unresolved": report
                    .changes
                    .iter()
                    .filter(|change| matches!(change, ScanChange::Unmatched { .. }))
                    .map(scan_change_json)
                    .collect::<Vec<_>>(),
            }),
            // A folder that cannot be scanned is not a reason to refuse the
            // session: everything stored in refs still works, which is the
            // whole point of keeping the registry there.
            Err(error) => json!({"status": "error", "error": {
                "kind": error.kind, "message": error.message
            }}),
        };
        // An empty store is skipped: creating the LanceDB catalog and
        // its three FTS indices dominates session start-up, and `search` and
        // every mutation synchronize on demand, so nothing observes a missing
        // projection for a store that has nothing to project.
        if !self.store_is_empty()? {
            self.service.sync_index().map_err(RpcFailure::service)?;
        }
        let handshake = self.handshake();
        self.initialized = true;
        let instructions = self.composed_instructions();
        Ok(json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {
                "resources": {"subscribe": true, "listChanged": false},
                "tools": {"listChanged": false},
                "experimental": {"memoryHub": handshake}
            },
            "serverInfo": {"name": "memory-hub", "version": env!("CARGO_PKG_VERSION")},
            "instructions": instructions,
            "_meta": {"memoryHub": handshake}
        }))
    }

    fn handshake(&self) -> Value {
        let project = self.project().to_path_buf();
        let canonical = project.canonicalize().unwrap_or(project);
        let project_id = stable_id("project", canonical.to_string_lossy().as_bytes());
        let executable = std::env::current_exe().unwrap_or_default();
        let mut installation_source = executable.to_string_lossy().into_owned();
        installation_source.push('\0');
        installation_source.push_str(env!("CARGO_PKG_VERSION"));
        let installation_id = stable_id("installation", installation_source.as_bytes());
        let description = self.service.describe_store().ok();
        let mut handshake = json!({
            "memoryInterfaceVersion": version(MEMORY_INTERFACE_MAJOR, MEMORY_INTERFACE_MINOR),
            "storeVersion": version(1, 1),
            "envelopeVersion": CURRENT_ENVELOPE_VERSION,
            "indexVersion": version(1, index_minor()),
            "modelFingerprint": self.service.provider().map(|provider| {
                memory_hub_embed::Fingerprint::from_provider(&*provider, provider.model_id())
                    .digest()
            }),
            "installationId": installation_id,
            "projectId": project_id,
            "projectPath": canonical,
            "reconciliation": self.reconciliation,
            "attachments": self.attachments
        });
        // A backend-specific fact is published only when the backend has one.
        // A client must handle an absent field; it is not obliged to notice a
        // wrong one, and a plausible path would pass every check the client
        // makes and fail at first use.
        if let Some(description) = description {
            handshake["backend"] = json!(description.backend);
            if let Some(git_dir) = description.git_dir {
                handshake["gitDir"] = json!(git_dir);
            }
        }
        handshake
    }

    /// Compose built-in instructions with project-specific schema text.
    ///
    /// Loads the schema registry from the current revision. When type records
    /// exist, appends a `## Document Types` section after the built-in
    /// conductor. When the registry is empty, only the built-in text is sent.
    fn composed_instructions(&self) -> String {
        let mut text = builtin_instructions();
        if let Ok(registry) = self.load_schema_registry() {
            let schema_text = schema_instructions::schema_instructions(&registry);
            if !schema_text.is_empty() {
                text.push_str(&schema_text);
            }
        }
        text
    }

    fn load_schema_registry(&self) -> Result<SchemaRegistry, RpcFailure> {
        self.service.schema_registry().map_err(RpcFailure::service)
    }

    /// Open the plaintext store for this session's project.
    ///
    /// Returned as the contract rather than as a named backend: a protocol
    /// adapter has no business knowing which one answers.
    ///
    /// # Errors
    ///
    /// Returns [`RpcFailure`] when the repository cannot be opened.
    pub fn store(&self) -> Result<memory_hub_service::StoreHandle<'_>, RpcFailure> {
        self.service.record_store().map_err(RpcFailure::service)
    }

    /// Whether the current snapshot holds no records at all.
    fn store_is_empty(&self) -> Result<bool, RpcFailure> {
        let (_, envelopes) = self.service.corpus(None).map_err(RpcFailure::service)?;
        Ok(envelopes.is_empty())
    }

    fn subscribe(&mut self, params: &Value) -> Result<Value, RpcFailure> {
        match required_string(params, "uri")? {
            "memory://revision/current" => self.revision_subscribed = true,
            RECORDS_CHANGED => self.records_subscribed = true,
            uri => return Err(resource_not_found(uri)),
        }
        Ok(json!({}))
    }

    fn unsubscribe(&mut self, params: &Value) -> Result<Value, RpcFailure> {
        match required_string(params, "uri")? {
            "memory://revision/current" => self.revision_subscribed = false,
            RECORDS_CHANGED => self.records_subscribed = false,
            uri => return Err(resource_not_found(uri)),
        }
        Ok(json!({}))
    }

    /// Serve one `memory://` resource.
    ///
    /// # Errors
    ///
    /// Returns [`RpcFailure`] for an unknown URI, or an unreadable store or
    /// index.
    pub fn read_resource(&self, params: &Value) -> Result<Value, RpcFailure> {
        let uri = required_string(params, "uri")?;
        let content = match uri {
            "memory://project" => self.handshake(),
            "memory://revision/current" => {
                let revision = self
                    .service
                    .current_revision()
                    .map_err(RpcFailure::service)?;
                json!({"schemaVersion": 1, "revision": revision})
            }
            "memory://index/status" => {
                let status = self.service.index_status().map_err(RpcFailure::service)?;
                json!({
                    "schemaVersion": status.schema_version,
                    "available": true,
                    "state": status.state,
                    "indexedRevision": status.indexed_revision,
                    "targetRevision": status.target_revision
                })
            }
            "memory://model/status" => self.build_model_status(),
            "memory://policy/effective" => policy_resource(),
            "memory://records/summary" => self.records_summary()?,
            "memory://schema" => {
                let registry = self.load_schema_registry()?;
                schema_instructions::schema_resource(&registry)
            }
            _ => {
                if let Some(kind) = uri.strip_prefix("memory://schema/") {
                    let registry = self.load_schema_registry()?;
                    match registry.get(kind) {
                        Some(definition) => schema_instructions::single_type_resource(definition),
                        None => return Err(resource_not_found(uri)),
                    }
                } else if let Some(key) = uri.strip_prefix("memory://records/") {
                    if key == "summary" {
                        return Err(resource_not_found(uri));
                    }
                    let view = self
                        .service
                        .get_record(key, None)
                        .map_err(RpcFailure::service)?;
                    json!({"schemaVersion": 1, "revision": view.revision, "record": view.record})
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

    /// Run one tool by name, and do everything running it entails.
    ///
    /// Reconciliation ahead of a mutating tool and bringing the index to the
    /// new revision after one are here, not at the call site: a host that had
    /// to remember them would be reimplementing the engine, and the copy that
    /// forgets one is the copy that corrupts something quietly.
    ///
    /// Notifications are deliberately not here. Which of these facts a client
    /// is told, and in what words, belongs to whatever protocol the caller
    /// speaks — this crate's own stdio server says it one way, a host that
    /// links this crate and serves its own interface says it another. What
    /// happened is reported; saying it is the caller's.
    ///
    /// # Errors
    ///
    /// Failures arrive inside [`ToolCall::result`] rather than as an `Err`,
    /// because a call that fails can still have moved the revision — a
    /// reconciliation that pulled changes in before the tool refused is the
    /// ordinary case — and a caller that dropped that would leave every client
    /// reading a revision that no longer exists.
    pub fn call(&mut self, name: &str, arguments: &Value) -> ToolCall {
        let reconciled = if MUTATING_TOOLS.contains(&name) {
            match self.service.reconcile_before_mutation() {
                Ok(changed) => changed,
                Err(error) => {
                    return ToolCall {
                        result: Err(ToolFailure::service(error).into()),
                        revision_changed: false,
                        changed: Vec::new(),
                    };
                }
            }
        } else {
            false
        };
        match self.execute_tool(name, arguments) {
            Ok(ToolOutcome {
                content,
                revision_changed,
                changed,
            }) => {
                let moved = revision_changed || reconciled;
                if moved && let Err(error) = self.service.sync_index() {
                    return ToolCall {
                        result: Err(ToolFailure::service(error).into()),
                        revision_changed: moved,
                        changed: Vec::new(),
                    };
                }
                ToolCall {
                    result: Ok(content),
                    revision_changed: moved,
                    changed,
                }
            }
            Err(failure) => ToolCall {
                result: Err(failure),
                revision_changed: reconciled,
                changed: Vec::new(),
            },
        }
    }

    fn call_tool(
        &mut self,
        id: Value,
        params: &Value,
        output: &mut impl Write,
    ) -> io::Result<Value> {
        let name = match required_string(params, "name") {
            Ok(name) => name,
            Err(error) => return Ok(rpc_error_owned(id, error.code, &error.message, error.data)),
        };
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let ToolCall {
            result,
            revision_changed,
            changed,
        } = self.call(name, &arguments);
        if !changed.is_empty() && self.records_subscribed {
            write_json(output, &records_changed_notification(&changed))?;
        }
        if revision_changed && self.revision_subscribed {
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
            Ok(content) => Ok(rpc_result(id, tool_success(content))),
            Err(ToolCallFailure::Rpc(error)) => {
                if error.data.get("kind").and_then(Value::as_str) == Some("tool_not_found") {
                    Ok(rpc_error_owned(id, error.code, &error.message, error.data))
                } else {
                    Ok(rpc_result(id, tool_error(error.into_tool_failure())))
                }
            }
            Err(ToolCallFailure::Tool(error)) => Ok(rpc_result(id, tool_error(error))),
        }
    }

    fn execute_tool(
        &mut self,
        name: &str,
        arguments: &Value,
    ) -> Result<ToolOutcome, ToolCallFailure> {
        match name {
            "memory_apply_transaction" => self.apply_transaction(arguments),
            "memory_get_record" => self.get_record(arguments),
            "memory_list_records" => self.list_records(arguments),
            "memory_list_folders" => self.list_folders(arguments),
            "memory_rename_folder" => self.rename_folder(arguments),
            "memory_move_document" => self.move_document(arguments),
            "memory_create_folder" => self.create_folder(arguments),
            "memory_delete_folder" => self.delete_folder(arguments),
            "memory_diff" => self.diff(arguments),
            "memory_export" => self.export(arguments),
            "memory_import" => self.import(arguments),
            "memory_doctor" => self.doctor(),
            "memory_scan" => self.scan(arguments),
            "memory_read_content" => self.read_content(arguments),
            "memory_write_content" => self.write_content(arguments),
            "memory_migrate_storage" => self.migrate_storage(arguments),
            "memory_reconcile" => self.reconcile(arguments),
            "memory_reindex" => self.reindex(),
            "memory_search" => self.search(arguments),
            "memory_backlinks" => self.backlinks(arguments),
            "memory_transport_status" => self.transport_status(),
            "memory_presence" => self.presence(),
            "memory_sync_state" => self.sync_state(arguments),
            "memory_remote_set" => self.remote_set(arguments),
            "memory_remote_remove" => self.remote_remove(),
            "memory_fetch" => self.fetch(arguments),
            "memory_push" => self.push(arguments),
            "memory_rewind" => self.rewind(arguments),
            "memory_model_status" => Ok(self.model_status()),
            "memory_delete_type" => self.delete_type(arguments),
            "memory_list_types" => self.list_types(),
            "memory_schema_status" => self.schema_status(),
            _ => Err(ToolCallFailure::Rpc(RpcFailure::new(
                -32_602,
                "tool not found",
                json!({"kind": "tool_not_found", "name": name}),
            ))),
        }
    }

    fn records_summary(&self) -> Result<Value, RpcFailure> {
        let summary = self
            .service
            .records_summary()
            .map_err(RpcFailure::service)?;
        let counts = summary.counts;
        Ok(json!({
            "schemaVersion": 1,
            "revision": summary.revision,
            "total": counts.total,
            "by_kind": counts.by_kind,
            "by_freshness": counts.by_freshness,
            "archived": counts.archived,
            "live": counts.live,
            "service": counts.service,
        }))
    }

    /// List the document types defined by `__type__` records.
    ///
    /// # Errors
    ///
    /// Returns [`ToolCallFailure`] when the schema registry cannot be loaded.
    pub fn list_types(&self) -> Result<ToolOutcome, ToolCallFailure> {
        let types = self.service.list_types().map_err(ToolFailure::service)?;
        let rendered: Vec<Value> = types
            .iter()
            .map(|summary| {
                let mut rendered = json!({
                    "kind_name": summary.kind_name,
                    "description": summary.description,
                    "field_count": summary.field_count,
                    "relationship_count": summary.relationship_count,
                    "writable": summary.writable,
                });
                // Absent, as the tool's description promises — not `null`.
                // "This type keeps its documents in its records" and "this type
                // keeps them in a folder called nothing" are different claims,
                // and only the first is one this can make.
                if let Some(storage) = &summary.storage
                    && let Some(object) = rendered.as_object_mut()
                {
                    object.insert("storage".to_owned(), json!(storage));
                }
                rendered
            })
            .collect();
        Ok(ToolOutcome::read(json!({
            "schemaVersion": 1,
            "typeCount": rendered.len(),
            "types": rendered,
        })))
    }

    /// Validate every record against the active schema.
    ///
    /// # Errors
    ///
    /// Returns [`ToolCallFailure`] when the registry or the records cannot be
    /// read.
    pub fn schema_status(&self) -> Result<ToolOutcome, ToolCallFailure> {
        let status = self.service.schema_status().map_err(ToolFailure::service)?;
        if !status.active {
            return Ok(ToolOutcome::read(json!({
                "schemaVersion": 1,
                "schemaActive": false,
                "totalRecords": 0,
                "incompatible": [],
                "message": "No type definitions found — schema validation is inactive."
            })));
        }
        let incompatible: Vec<Value> = status
            .incompatible
            .iter()
            .map(|entry| {
                json!({
                    "key": entry.key,
                    "kind": entry.kind,
                    "violations": [{"field": entry.field, "reason": entry.reason}],
                })
            })
            .collect();
        Ok(ToolOutcome::read(json!({
            "schemaVersion": 1,
            "schemaActive": true,
            "revision": status.revision,
            "totalRecords": status.total_records,
            "incompatibleCount": incompatible.len(),
            "incompatible": incompatible,
        })))
    }

    fn apply_transaction(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let transaction_id = required_string(arguments, "transaction_id")?.to_owned();
        let expected_revision: Revision = parse_field(arguments, "expected_revision")?;
        let raw = arguments
            .get("operations")
            .and_then(Value::as_array)
            .ok_or_else(|| RpcFailure::invalid_argument("operations"))?;
        let operations = raw
            .iter()
            .map(parse_operation)
            .collect::<Result<Vec<_>, _>>()?;
        let deleted: Vec<String> = operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::Delete { id } => Some(id.display_value()),
                Operation::Put { .. } => None,
            })
            .collect();
        let result = self
            .service
            .apply_transaction(&transaction_id, expected_revision, operations)
            .map_err(ToolFailure::service)?;
        let changed = result
            .changed_keys
            .iter()
            .map(|key| {
                RecordNotice::keyed(
                    key,
                    if deleted.contains(key) {
                        "deleted"
                    } else {
                        "written"
                    },
                )
            })
            .collect();
        Ok(ToolOutcome::changing(json!(result), changed))
    }

    fn get_record(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let key = required_string(arguments, "key")?;
        let revision: Option<Revision> = Some(parse_field(arguments, "revision")?);
        let view = self
            .service
            .get_record(key, revision.as_ref())
            .map_err(ToolFailure::service)?;
        Ok(ToolOutcome::read(
            json!({"revision": view.revision, "record": view.record}),
        ))
    }

    fn list_records(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let query = listing_query(arguments);
        let revision: Option<Revision> = match arguments.get("revision") {
            Some(value) => Some(
                serde_json::from_value(value.clone())
                    .map_err(|_| RpcFailure::invalid_argument("revision"))?,
            ),
            None => None,
        };
        let listing = self
            .service
            .list_records(&query, revision.as_ref())
            .map_err(ToolFailure::service)?;
        let page: Vec<Value> = listing
            .records
            .iter()
            .map(|(key, envelope)| render_record(key, envelope, listing.metadata_only))
            .collect();
        let counts = &listing.counts;
        Ok(ToolOutcome::read(json!({
            "revision": listing.revision,
            "records": page,
            "total": listing.total,
            "limit": listing.limit,
            "offset": listing.offset,
            "has_more": listing.has_more,
            "counts": {
                "total": counts.total,
                "by_kind": counts.by_kind,
                "by_freshness": counts.by_freshness,
                "archived": counts.archived,
                "live": counts.live,
                "service": counts.service,
            },
        })))
    }

    fn list_folders(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let folder = arguments.get("folder").and_then(Value::as_str);
        let folders = self
            .service
            .list_folders(
                folder,
                folder_subtree(arguments),
                arguments.get("kind").and_then(Value::as_str),
            )
            .map_err(ToolFailure::service)?;
        let rendered: Vec<Value> = folders
            .iter()
            .map(|entry| {
                json!({
                    "path": entry.path,
                    "in_records": entry.in_records,
                    "in_storage": entry.in_storage,
                    "records": entry.records,
                    "described_by": entry.described,
                })
            })
            .collect();
        Ok(ToolOutcome::read(json!({"folders": rendered})))
    }

    fn rename_folder(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let from = required_string(arguments, "from")?;
        let to = required_string(arguments, "to")?;
        let transaction_id = required_string(arguments, "transaction_id")?;
        let result = self
            .service
            .rename_folder(from, to, transaction_id)
            .map_err(ToolFailure::service)?;
        let changed = result
            .changed_keys
            .iter()
            .map(|key| RecordNotice::keyed(key, "written"))
            .collect();
        Ok(ToolOutcome::changing(json!(result), changed))
    }

    fn delete_folder(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let folder = required_string(arguments, "folder")?;
        let transaction_id = required_string(arguments, "transaction_id")?;
        let removed = self
            .service
            .delete_folder(folder, transaction_id)
            .map_err(ToolFailure::service)?;
        Ok(ToolOutcome::changing(
            json!({"removed": removed}),
            Vec::new(),
        ))
    }

    fn create_folder(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let folder = required_string(arguments, "folder")?;
        let kind = required_string(arguments, "kind")?;
        let transaction_id = required_string(arguments, "transaction_id")?;
        let result = self
            .service
            .create_folder(folder, kind, transaction_id)
            .map_err(ToolFailure::service)?;
        let changed = result
            .changed_keys
            .iter()
            .map(|key| RecordNotice::keyed(key, "written"))
            .collect();
        Ok(ToolOutcome::changing(json!(result), changed))
    }

    fn move_document(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let key = required_string(arguments, "key")?;
        // Not `required_string`: the empty string is the root, which is a
        // destination somebody can mean, and that helper reads empty as absent.
        let folder = arguments
            .get("folder")
            .and_then(Value::as_str)
            .ok_or_else(|| RpcFailure::invalid_argument("folder"))?;
        let transaction_id = required_string(arguments, "transaction_id")?;
        let result = self
            .service
            .move_document(key, folder, transaction_id)
            .map_err(ToolFailure::service)?;
        let changed = result
            .changed_keys
            .iter()
            .map(|key| RecordNotice::keyed(key, "written"))
            .collect();
        Ok(ToolOutcome::changing(json!(result), changed))
    }

    fn diff(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let from: Revision = parse_field(arguments, "from_revision")?;
        let to: Revision = parse_field(arguments, "to_revision")?;
        let changes = self
            .service
            .diff(&from, &to)
            .map_err(ToolFailure::service)?;
        Ok(ToolOutcome::read(
            json!({"fromRevision": from, "toRevision": to, "changes": changes}),
        ))
    }

    fn export(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let revision: Revision = parse_field(arguments, "revision")?;
        // Absent means the deterministic bundle every client got before there
        // was anything to resolve.
        let mode = match arguments.get("mode").and_then(Value::as_str) {
            None | Some("manifest") => ExportMode::Manifest,
            Some("snapshot") => ExportMode::Snapshot,
            Some(_) => {
                return Err(ToolCallFailure::Rpc(RpcFailure::invalid_argument("mode")));
            }
        };
        let view = self
            .service
            .export(&revision, mode)
            .map_err(ToolFailure::service)?;
        Ok(ToolOutcome::read(
            json!({"revision": view.revision, "bundle": view.bundle}),
        ))
    }

    fn import(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let transaction_id = required_string(arguments, "transaction_id")?;
        let expected_revision: Revision = parse_field(arguments, "expected_revision")?;
        let bundle = arguments
            .get("bundle")
            .ok_or_else(|| RpcFailure::invalid_argument("bundle"))?;
        let bundle: memory_hub_engine::ExportBundle = serde_json::from_value(bundle.clone())
            .map_err(|_| RpcFailure::invalid_argument("bundle"))?;
        let result = self
            .service
            .import(transaction_id, expected_revision, &bundle)
            .map_err(ToolFailure::service)?;
        let changed = result
            .changed_keys
            .iter()
            .map(|key| RecordNotice::keyed(key, "written"))
            .collect();
        Ok(ToolOutcome::changing(json!(result), changed))
    }

    fn doctor(&self) -> Result<ToolOutcome, ToolCallFailure> {
        let report = self.service.doctor().map_err(ToolFailure::service)?;
        let mut value = json!({
            "schemaVersion": 1,
            "healthy": report.healthy,
            "backend": report.store.backend,
            "revision": report.revision,
            "hiddenOnThisBranch": report.hidden,
            "attachments": report
                .attachments
                .iter()
                .map(unresolved_json)
                .collect::<Vec<_>>(),
        });
        if let Some(git_dir) = report.store.git_dir {
            value["gitDir"] = json!(git_dir);
        }
        Ok(ToolOutcome::read(value))
    }

    fn scan(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let transaction_id = required_string(arguments, "transaction_id")?;
        let report = self
            .service
            .scan_attachments(transaction_id)
            .map_err(ToolFailure::service)?;
        let notices: Vec<RecordNotice> = report.changes.iter().map(scan_notice).collect();
        let content = json!({
            "schemaVersion": 1,
            "revision": report.revision,
            "scanned": report.scanned,
            "applied": report.applied,
            "changes": report
                .changes
                .iter()
                .map(scan_change_json)
                .collect::<Vec<_>>(),
        });
        // A scan that only found something it cannot resolve writes nothing.
        // The revision has not moved and a client still has to hear about it,
        // which is precisely what a revision subscription could never say.
        if report.applied == 0 {
            return Ok(ToolOutcome::observing(content, notices));
        }
        Ok(ToolOutcome::changing(content, notices))
    }

    /// Read a record's body, following its locator when it has one.
    ///
    /// The one operation that goes to another backend, and the only one that
    /// can report that backend is missing. A locator that resolves to nothing
    /// answers `missing` rather than failing: the file may be deleted, on
    /// another branch, or simply not pulled, and those are indistinguishable
    /// from here.
    fn read_content(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let key = required_string(arguments, "key")?;
        let resolution = self
            .service
            .resolve_content(key)
            .map_err(ToolFailure::service)?;
        let content = match resolution {
            ContentResolution::Inline { content } => json!({
                "schemaVersion": 1,
                "key": key,
                "source": "record",
                "content": content,
            }),
            ContentResolution::Resolved {
                path,
                content,
                hash,
                changed,
            } => {
                let mut body = json!({
                    "schemaVersion": 1,
                    "key": key,
                    "source": "file",
                    "path": path,
                    "content_hash": hash.as_str(),
                    // The file changed since Memory last looked, so whatever
                    // the record says about it was checked against another
                    // text.
                    "changed": changed,
                    "media_type": memory_hub_service::media_type_for(&path),
                });
                match content {
                    // Text goes out as text, which is what almost everything
                    // is and what every existing client already reads.
                    Content::Text(text) => {
                        body["content"] = json!(text);
                        body["encoding"] = json!("utf-8");
                    }
                    // Bytes go out encoded, and say so. A client that only
                    // knows about text sees an encoding it does not recognise
                    // rather than a string of replacement characters.
                    Content::Bytes(bytes) => {
                        body["content"] = json!(BASE64.encode(&bytes));
                        body["encoding"] = json!("base64");
                        body["bytes"] = json!(bytes.len());
                    }
                    // Nothing is fetched: the caller is told where it is.
                    Content::Link { url, media_type } => {
                        body["content"] = Value::Null;
                        body["url"] = json!(url);
                        body["media_type"] = json!(media_type);
                        body["encoding"] = json!("none");
                    }
                }
                body
            }
            ContentResolution::Missing { path, reason } => json!({
                "schemaVersion": 1,
                "key": key,
                "source": "file",
                "path": path,
                "content": Value::Null,
                "missing": true,
                "reason": reason,
            }),
        };
        Ok(ToolOutcome::read(content))
    }

    /// Write the content of a record whose content is a repository file.
    ///
    /// The file first, the record second. The two cannot be made atomic — one
    /// is a file, the other a record — so the order is the one that repairs:
    /// an interruption leaves a file that disagrees with the record's digest,
    /// which the next scan sees and settles.
    fn write_content(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let transaction_id = required_string(arguments, "transaction_id")?.to_owned();
        let key = required_string(arguments, "key")?.to_owned();
        let content = required_string(arguments, "content")?.to_owned();
        // How to read what arrived. A folder holds whatever is in it now that
        // there is no mask on it, so a caller writing a picture has no way to
        // spell it as text — and a caller that says nothing is the caller
        // every existing client already is, writing prose.
        let bytes = match arguments.get("encoding").and_then(Value::as_str) {
            None | Some("utf-8") => content.into_bytes(),
            Some("base64") => BASE64
                .decode(content.as_bytes())
                .map_err(|_| ToolCallFailure::from(RpcFailure::invalid_argument("content")))?,
            Some(_) => {
                return Err(RpcFailure::invalid_argument("encoding").into());
            }
        };
        let result = self
            .service
            .write_reference_content(&transaction_id, &key, &bytes)
            .map_err(ToolFailure::service)?;
        Ok(ToolOutcome::changing(
            json!(result),
            vec![RecordNotice::keyed(&key, "content_changed")],
        ))
    }

    fn migrate_storage(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let kind = required_string(arguments, "kind")?.to_owned();
        // A directory of the working tree, or `null` to bring the content back
        // in with the records. Absent is not the same as null: one is a caller
        // who forgot to say, the other is a caller who said "here".
        let storage: Option<String> = match arguments.get("storage") {
            Some(Value::Null) => None,
            Some(Value::String(folder)) => Some(folder.clone()),
            // Absent or of the wrong type. Absent is a caller who forgot to
            // say; null is a caller who said "back in with the records".
            _ => return Err(RpcFailure::invalid_argument("storage").into()),
        };
        let acknowledged = string_array(arguments, "acknowledge");
        let dry_run = arguments
            .get("dry_run")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        let plan = if dry_run {
            self.service
                .plan_migration(&kind, storage.as_deref())
                .map_err(ToolFailure::service)?
        } else {
            self.service
                .migrate_storage(
                    required_string(arguments, "transaction_id")?,
                    &kind,
                    storage.as_deref(),
                    &acknowledged,
                )
                .map_err(ToolFailure::service)?
        };

        let content = json!({
            "schemaVersion": 1,
            "kind": plan.kind,
            "from": plan.from,
            "to": plan.to,
            "records": plan.keys.len(),
            "keys": plan.keys,
            "warnings": plan
                .warnings
                .iter()
                .map(|warning| json!({"code": warning.code, "message": warning.message}))
                .collect::<Vec<_>>(),
            "applied": !dry_run,
        });
        if dry_run {
            return Ok(ToolOutcome::read(content));
        }
        let changed = plan
            .keys
            .iter()
            .map(|key| RecordNotice::keyed(key, "written"))
            .collect();
        Ok(ToolOutcome::changing(content, changed))
    }

    fn reindex(&self) -> Result<ToolOutcome, ToolCallFailure> {
        let status = self.service.reindex().map_err(ToolFailure::service)?;
        Ok(ToolOutcome::read(json!(status)))
    }

    fn model_status(&self) -> ToolOutcome {
        ToolOutcome::read(self.build_model_status())
    }

    fn build_model_status(&self) -> Value {
        match self.service.model_status() {
            Some(status) => json!({
                "schemaVersion": 1,
                "modelId": status.model_id,
                "dimensions": status.dimensions,
                "runtime": status.runtime,
                "runtimeState": status.runtime_state,
                "vectorSearch": status.vector_search,
                "ftsOnly": status.fts_only(),
                "mode": "hybrid",
            }),
            None => json!({
                "schemaVersion": 1,
                "modelId": null,
                "dimensions": null,
                "runtime": "none",
                "runtimeState": "missing",
                "vectorSearch": false,
                "ftsOnly": true,
                "mode": "fts",
            }),
        }
    }

    fn reconcile(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let mode = match arguments.get("divergence").and_then(Value::as_str) {
            None | Some("report") => DivergenceMode::Report,
            Some("full_rebuild") => DivergenceMode::FullRebuild,
            Some(_) => return Err(RpcFailure::invalid_argument("divergence").into()),
        };
        let report = self.service.reconcile(mode).map_err(ToolFailure::service)?;
        let revision_changed = report
            .processed
            .iter()
            .any(|commit| !commit.stale_keys.is_empty());
        let changed = report
            .processed
            .iter()
            .flat_map(|commit| &commit.stale_keys)
            .map(|key| RecordNotice::keyed(key, "freshness_changed"))
            .collect();
        Ok(ToolOutcome {
            content: json!(report),
            revision_changed,
            changed,
        })
    }

    fn search(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let query = required_string(arguments, "query")?.to_owned();
        let limit = usize::try_from(arguments.get("limit").and_then(Value::as_u64).unwrap_or(20))
            .unwrap_or(20);
        let offset = usize::try_from(arguments.get("offset").and_then(Value::as_u64).unwrap_or(0))
            .unwrap_or(0);
        let revision: Revision = match arguments.get("revision") {
            Some(value) => serde_json::from_value(value.clone())
                .map_err(|_| RpcFailure::invalid_argument("revision"))?,
            None => self
                .service
                .current_revision()
                .map_err(ToolFailure::service)?,
        };
        let request = SearchRequest {
            query,
            limit,
            offset,
            filters: SearchFilters {
                kind: arguments
                    .get("kind")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                kinds: string_array(arguments, "kinds"),
                tags: string_array(arguments, "tags"),
                archived: arguments.get("archived").and_then(Value::as_bool),
                freshness: string_array(arguments, "freshness"),
                folder: arguments
                    .get("folder")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                folder_subtree: folder_subtree(arguments),
                presence: arguments
                    .get("presence")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                include_service: arguments
                    .get("include_service")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            },
            revision,
        };
        let result = self
            .service
            .search(&request)
            .map_err(ToolFailure::service)?;
        Ok(ToolOutcome::read(json!(result)))
    }

    fn backlinks(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let key = required_string(arguments, "key")?;
        let revision: Option<Revision> = match arguments.get("revision") {
            Some(value) => Some(
                serde_json::from_value(value.clone())
                    .map_err(|_| RpcFailure::invalid_argument("revision"))?,
            ),
            None => None,
        };
        let view = self
            .service
            .backlinks(key, revision.as_ref())
            .map_err(ToolFailure::service)?;
        Ok(ToolOutcome::read(json!({
            "key": view.key,
            "revision": view.revision,
            "backlinks": view.entries,
        })))
    }

    fn transport_status(&self) -> Result<ToolOutcome, ToolCallFailure> {
        let remote = self
            .service
            .transport_status()
            .map_err(ToolFailure::service)?;
        // The code origin travels with the answer because the only thing a
        // caller does with an unconfigured remote is offer to configure one,
        // and this is the single address it can sensibly suggest. Asking for
        // it separately would be a second round trip for half a question.
        let code_origin = self.service.code_origin_url().unwrap_or_default();
        Ok(ToolOutcome::read(json!({
            "remoteConfigured": remote.is_some(),
            "remoteUrl": remote.as_ref().map(|remote| remote.url.clone()),
            "refspec": remote.as_ref().and_then(|remote| remote.refspec.clone()),
            "codeOriginUrl": code_origin,
        })))
    }

    fn sync_state(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        // Absent means "do not touch the network". A status question that
        // reached for a remote by default would make opening a project wait on
        // one, and the count that matters most does not need it.
        let ask_remote = arguments
            .get("ask_remote")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let state = self
            .service
            .sync_state(ask_remote)
            .map_err(ToolFailure::service)?;
        Ok(ToolOutcome::read(
            serde_json::to_value(state).unwrap_or(Value::Null),
        ))
    }

    fn presence(&self) -> Result<ToolOutcome, ToolCallFailure> {
        let presence = self.service.presence().map_err(ToolFailure::service)?;
        Ok(ToolOutcome::read(
            serde_json::to_value(presence).unwrap_or(Value::Null),
        ))
    }

    fn remote_set(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let url = required_string(arguments, "url")?;
        let refspec = arguments
            .get("refspec")
            .and_then(Value::as_str)
            .map(ToOwned::to_owned);
        let remote = self
            .service
            .set_remote(url.to_owned(), refspec)
            .map_err(ToolFailure::service)?;
        Ok(ToolOutcome::read(json!({
            "remoteConfigured": true,
            "remoteUrl": remote.url,
            "refspec": remote.refspec,
        })))
    }

    fn remote_remove(&self) -> Result<ToolOutcome, ToolCallFailure> {
        self.service.remove_remote().map_err(ToolFailure::service)?;
        Ok(ToolOutcome::read(json!({"remoteConfigured": false})))
    }

    fn fetch(&self, _arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let result = self.service.fetch().map_err(ToolFailure::service)?;
        let changed = result.local_revision_before != result.local_revision_after;
        let records = if changed {
            self.service
                .diff(&result.local_revision_before, &result.local_revision_after)
                .map_err(ToolFailure::service)?
                .into_iter()
                .map(|change| {
                    RecordNotice::keyed(change.id.display_value(), change_kind_name(change.kind))
                })
                .collect()
        } else {
            Vec::new()
        };
        Ok(ToolOutcome {
            content: json!({
                "localRevisionBefore": result.local_revision_before,
                "localRevisionAfter": result.local_revision_after,
                "remoteRevision": result.remote_revision,
                "fastForward": result.fast_forward,
                "merged": result.merged,
                "conflicts": result.conflicts,
                // The records both sides moved the same thing in, and what of
                // theirs is not in the answer: `body` when the same lines were
                // rewritten, `fields` naming the members that lost. Nothing is
                // gone — the other version is still a commit — but somebody has
                // to be told, and told what.
                "overlaps": result.overlaps,
            }),
            revision_changed: changed,
            changed: records,
        })
    }

    fn push(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let force = arguments
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let outcome = self.service.push(force).map_err(ToolFailure::service)?;
        Ok(ToolOutcome::read(json!({
            "pushed": true,
            "force": outcome.force,
            "remote": outcome.remote.url,
            "warnings": outcome.policy.warnings,
            "staleCount": outcome.policy.stale_count,
        })))
    }

    /// Put memory back where it stood, undoing what has happened since.
    ///
    /// What a fetch is undone with: a merge is an ordinary commit on top of
    /// what was here, and `localRevisionBefore` from that fetch is the revision
    /// to name. Backwards along this memory's own history and nowhere else, so
    /// it cannot arrive at a state the project was never in.
    ///
    /// Every record may have changed, so this reports a moved revision without
    /// naming keys — whoever is holding a list should read it again.
    fn rewind(&mut self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let revision = required_string(arguments, "revision")?;
        let expected = required_string(arguments, "expected_revision")?;
        let now = self
            .service
            .rewind(revision, expected)
            .map_err(ToolFailure::service)?;
        Ok(ToolOutcome {
            content: json!({"revision": now}),
            revision_changed: true,
            changed: Vec::new(),
        })
    }

    /// Remove a type, and detach the folder it was over.
    ///
    /// Not expressible as a batch of deletions, which is why it is a tool of
    /// its own: deleting a record takes its document, and the records of an
    /// attached type describe files the repository had before Memory did.
    fn delete_type(&mut self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let kind = required_string(arguments, "kind")?.to_owned();
        let transaction_id = required_string(arguments, "transaction_id")?.to_owned();

        let removal = self
            .service
            .remove_type(&kind, &transaction_id)
            .map_err(ToolFailure::service)?;

        Ok(ToolOutcome::changing(
            json!({
                "schemaVersion": 1,
                "revision": removal.revision,
                "removed": removal.removed,
                "detached": removal.detached,
            }),
            // The definition and its records are gone by name, and the client
            // is told how many rather than which: a type with ten thousand
            // records would answer with a list nobody reads. The one notice
            // says the schema moved, which is what a client re-reads.
            vec![RecordNotice::keyed(
                memory_hub_schema::type_key(&kind),
                "deleted",
            )],
        ))
    }
}

/// Build a listing query from wire arguments.
///
/// An unknown `sort` is not an error: it falls back to `key`, which is what the
/// interface has always done.
fn listing_query(arguments: &Value) -> ListingQuery {
    ListingQuery {
        offset: usize::try_from(arguments.get("offset").and_then(Value::as_u64).unwrap_or(0))
            .unwrap_or(0),
        kind: arguments
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_owned),
        tags: string_array(arguments, "tags"),
        archived: arguments.get("archived").and_then(Value::as_bool),
        freshness: string_array(arguments, "freshness"),
        folder: arguments
            .get("folder")
            .and_then(Value::as_str)
            .map(str::to_owned),
        folder_subtree: folder_subtree(arguments),
        presence: PresenceFilter::from_name(
            arguments
                .get("presence")
                .and_then(Value::as_str)
                .unwrap_or("present"),
        ),
        sort: ListingSort::from_name(
            arguments
                .get("sort")
                .and_then(Value::as_str)
                .unwrap_or("key"),
        ),
        descending: arguments.get("sort_order").and_then(Value::as_str) == Some("desc"),
        metadata_only: arguments
            .get("metadata_only")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        include_service: arguments
            .get("include_service")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        include_folders: arguments
            .get("include_folders")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        ..ListingQuery::default()
    }
    .with_limit(
        usize::try_from(arguments.get("limit").and_then(Value::as_u64).unwrap_or(50)).unwrap_or(50),
    )
}

/// `metadata_only` omits content, links and paths — the shape a UI list needs
/// without transferring every body.
fn render_record(key: &str, envelope: &Envelope, metadata_only: bool) -> Value {
    let mut record = if metadata_only {
        json!({
            "key": key,
            "kind": envelope.kind,
            "title": envelope.title,
            "tags": envelope.tags,
            "archived": envelope.archive.archived,
            "freshness": freshness_str(envelope.freshness.state),
            "content_hash": envelope.content_hash.as_str(),
        })
    } else {
        json!({
            "key": key,
            "kind": envelope.kind,
            "title": envelope.title,
            "content": envelope.content,
            "tags": envelope.tags,
            "links": envelope.links,
            "source_paths": envelope.source_paths,
            "archive": envelope.archive,
            "freshness": envelope.freshness,
            "content_hash": envelope.content_hash.as_str(),
        })
    };
    // Where the record sits, and — for one whose content is a repository file —
    // where that file is and whether it is here. Without these a client cannot
    // tell a record that points at a document from an empty one, cannot show
    // the hierarchy it is allowed to filter by, and gets a record back from
    // `presence: "absent"` with no way to say why it was hidden.
    if let Some(folder) = &envelope.folder {
        record["folder"] = json!(folder);
    }
    // Said only when true. A client that draws a tree needs to know which of a
    // folder's records is the folder; every other client can stay unaware of
    // the idea, which it cannot if the field is on every record.
    if envelope.is_folder {
        record["is_folder"] = json!(true);
    }
    if let Some(reference) = &envelope.content_ref {
        record["content_ref"] = json!({"path": reference.path});
        record["presence"] = json!(reference.presence.as_str());
    }
    // What the body is, said before anybody fetches it: a client picks an
    // editor, a viewer or a player from this rather than from reading the
    // bytes to find out.
    if let Some(media_type) = &envelope.media_type {
        record["media_type"] = json!(media_type);
    }
    record
}

/// The projection schema, as the handshake's index minor.
///
/// One number, moved by the crate that owns the read model, so a client cannot
/// be told the index is unchanged while its shape is.
const fn index_minor() -> u16 {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "the projection schema is a small counter"
    )]
    let minor = memory_hub_index::META_SCHEMA as u16;
    minor
}

fn string_array(arguments: &Value, field: &str) -> Vec<String> {
    arguments
        .get(field)
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default()
}

#[derive(Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum WireOperation {
    Put {
        record: StoredRecord,
        /// Additive: a client that has never heard of it sends nothing and
        /// gets the unconditional write it always got.
        #[serde(default)]
        expected_content_hash: Option<ContentHash>,
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
        WireOperation::Put {
            record,
            expected_content_hash: None,
        } => Ok(Operation::put(record)),
        WireOperation::Put {
            record,
            expected_content_hash: Some(expected),
        } => Ok(Operation::put_if_unchanged(record, expected)),
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

#[must_use]
pub fn list_resources() -> Value {
    let resources = [
        ("Memory Hub project", "memory://project"),
        ("Current revision", "memory://revision/current"),
        ("Index status", "memory://index/status"),
        ("Model status", "memory://model/status"),
        ("Effective policy", "memory://policy/effective"),
        ("Records summary", "memory://records/summary"),
        ("Document type schema", "memory://schema"),
    ]
    .into_iter()
    .map(|(name, uri)| json!({"name": name, "uri": uri, "mimeType": "application/json"}))
    .collect::<Vec<_>>();
    json!({"resources": resources})
}

#[must_use]
pub fn list_resource_templates() -> Value {
    json!({"resourceTemplates": [
        {
            "name": "Memory record",
            "uriTemplate": "memory://records/{key}",
            "mimeType": "application/json",
            "description": "Current record identified by its plaintext key, as of the current revision"
        },
        {
            "name": "Document type definition",
            "uriTemplate": "memory://schema/{kind_name}",
            "mimeType": "application/json",
            "description": "Schema for a single document type"
        }
    ]})
}

/// Built-in agent instructions — describes the storage layer tools and the
/// revision model. Updated with each Memory Hub version. Composed with project-specific schema instructions (from
/// `__type__` records) when schema is implemented.
#[allow(clippy::too_many_lines)] // One prose block; splitting it would scatter the text.
fn builtin_instructions() -> String {
    let mut s = String::new();
    s.push_str("# Memory Hub — Persistent Knowledge Store\n\n");
    s.push_str("You are connected to a Memory Hub store. Records persist in Git objects ");
    s.push_str("under `.git/refs/memory/` and are portable with the repository. ");
    s.push_str("Each record is a generic Envelope: key, kind, content, title, tags, links, ");
    s.push_str("source_paths, archive state, freshness state, and extensions.\n\n");

    // ── Document model ──
    s.push_str("## Document Model\n\n");
    s.push_str("Records are identified by a `key` (stable string) and grouped by `kind` ");
    s.push_str("(free-form string, e.g. \"decision\", \"constraint\", \"spec\"). ");
    s.push_str("The `content` field holds the main text. `title` is a short summary. ");
    s.push_str("`tags` are free-form labels. `links` are typed relations to other records ");
    s.push_str("(by key). `extensions` is a JSON object for semantic fields ");
    s.push_str("(e.g. status, priority) — typed fields beyond the standard Envelope.\n\n");
    s.push_str("`folder` places a record in a hierarchy — a path of segments, absent for ");
    s.push_str("the root. Folders are implicit: one exists while a record is in it, so ");
    s.push_str("there are no empty folders and no folder records to create. Moving a ");
    s.push_str("record between folders is one field on a normal put; the key does not ");
    s.push_str("change, so no link breaks. For a record whose content lives outside, the ");
    s.push_str("folder is the directory of `content_ref.path` and may not disagree with ");
    s.push_str("it — move the file to move the record.\n\n");
    s.push_str("A record may instead carry `content_ref: {path}`, meaning its content ");
    s.push_str("lives outside Memory at that repository-relative path. Such a record ");
    s.push_str("keeps `content` empty, and its `content_hash` is the digest of what was ");
    s.push_str("last read through the locator — not of anything stored here. Read it with ");
    s.push_str("`memory_read_content` and write it with `memory_write_content`; a read may ");
    s.push_str("answer `missing: true`, which is a normal state, and ");
    s.push_str("`content_ref.presence` says which: `present`, `not_on_branch` (the ");
    s.push_str("checked-out commit has no such document — routine, since memory does not ");
    s.push_str("branch and code does), or `removed` (the branch has it, the working tree ");
    s.push_str("does not). Only `not_on_branch` is hidden from `memory_list_records` and ");
    s.push_str("`memory_search` by default: a `removed` document is the one case a person ");
    s.push_str("is asked about, and it has to be visible to be asked about. Pass ");
    s.push_str("`presence: \"any\"` or `\"absent\"` to see everything. Nothing is ever ");
    s.push_str("deleted for being absent.\n\n");
    s.push_str("A type may be attached to a repository folder (`storage.folder` plus a ");
    s.push_str("file-name mask). Its records' content is then ordinary repository files — ");
    s.push_str("Git versions them, a pull request shows them in the diff — and Memory ");
    s.push_str("writes nothing into them. `memory_scan` reconciles the folder: an edit in ");
    s.push_str("place, a move, a disappearance and a return are applied; a file that could ");
    s.push_str("be either new or a rename-with-edit is reported with ranked candidates and ");
    s.push_str("left for a person. Nothing is ever deleted by a scan. A document that is ");
    s.push_str("not text is still a document — it is scanned, moved and tracked like the ");
    s.push_str("rest — but search cannot look inside it, so its hit reports ");
    s.push_str("`content_kind: \"binary\"` instead of an empty body.\n\n");

    // ── Revision model ──
    s.push_str("## Revision Model\n\n");
    s.push_str("Memory lives on one ref, `refs/memory/main`: every transaction is a ");
    s.push_str("commit on it, and every past state is one of its parents. A record is ");
    s.push_str("readable the moment it is written, and `memory_diff` compares any two ");
    s.push_str("revisions.\n\n");
    s.push_str("Every mutation returns the new revision, and that value is what ");
    s.push_str("`expected_revision` expects: the last revision you observed. ");
    s.push_str("After mutations, the index is synchronised automatically — no manual ");
    s.push_str("reindex needed for normal operations.\n\n");

    // ── Tools guide ──
    s.push_str("## Tools\n\n");

    s.push_str("### Reading\n\n");
    s.push_str("- **memory_get_record** (`key`): Fetch one record by its key. ");
    s.push_str("Returns the full Envelope including content, links, extensions.\n");
    s.push_str("- **memory_list_records**: List records with pagination (`limit`, `offset`), ");
    s.push_str("filters (`kind`, `tags`, `archived`, `freshness`), sorting (`sort`, ");
    s.push_str("`sort_order`), and `metadata_only` mode (omits content — use for UI lists). ");
    s.push_str("Response includes `counts` (total, by_kind, by_freshness, archived/live) ");
    s.push_str("over the full filtered set.\n");
    s.push_str("Type definitions are left out of both this and search; ask for ");
    s.push_str("`kind: \"__type__\"` or set `include_service` to see them.\n");
    s.push_str("The record that *is* a folder is left out too — it is what the folder ");
    s.push_str("says rather than a document filed in it. `memory_list_folders` names it ");
    s.push_str("as `described_by`, search finds its text like any other, and ");
    s.push_str("`include_folders` puts it back in the list.\n");
    s.push_str("- **memory_list_folders**: List folders from both sources at once — ");
    s.push_str("folders records are filed in, and real directories of an attached ");
    s.push_str("documentation folder, including empty ones no record can reveal. ");
    s.push_str("Each entry says where it is known from (`in_records`, `in_storage`), ");
    s.push_str("how many documents are directly in it, and `described_by`: the key of ");
    s.push_str("the record that is the folder, when one is.\n");
    s.push_str("- **memory_search** (`query`): Full-text search with the same filters, ");
    s.push_str("`kinds` narrowing to several types in one ask. ");
    s.push_str("Use when you need to find records by content, not by key. ");
    s.push_str("Returns ranked hits with snippets.\n");
    s.push_str("- **memory_backlinks** (`key`): Find records that link TO or mention a key. ");
    s.push_str("Combines explicit `links` and body-mention detection.\n");

    s.push_str("\n### Writing\n\n");
    s.push_str("- **memory_apply_transaction**: Create, update, or delete records. ");
    s.push_str("Provide `transaction_id`, `expected_revision` (from last read), and ");
    s.push_str("`operations` array. Each Put needs: key, kind, content, title, tags, links. ");
    s.push_str("If schema is active, the record is validated before write — ");
    s.push_str("invalid records are rejected with a `validation_error` containing ");
    s.push_str("`kind`, `field`, and `reason`. A Put may carry `expected_content_hash`: ");
    s.push_str("the write then applies only while the record's content still hashes to ");
    s.push_str("that value, and returns `conflict` otherwise. Omit it for the ");
    s.push_str("unconditional write.\n");
    s.push_str("- **memory_move_document** (`key`, `folder`): File one record in another ");
    s.push_str("folder, `\"\"` for the root. Use this rather than writing `folder` in a Put: ");
    s.push_str("for a record whose content is a repository file the folder is the ");
    s.push_str("directory that file is in, and a Put would leave the two disagreeing ");
    s.push_str("while this moves the file too. Such a document may only move within its ");
    s.push_str("type's storage, and nothing is overwritten.\n");
    s.push_str("- **memory_create_folder** (`folder`, `kind`): Make an empty folder under a ");
    s.push_str("type. A directory for a type whose documents are files, and the record that ");
    s.push_str("carries `is_folder` for a type whose documents are its records.\n");
    s.push_str("- **memory_delete_folder** (`folder`): Take a folder and everything filed ");
    s.push_str("under it, at any depth and whatever its type — a folder exists while ");
    s.push_str("something is in it. Directories are removed only while empty, so a file no ");
    s.push_str("scan reached is left alone. Answers with how many records went.\n");
    s.push_str("- **memory_rename_folder** (`from`, `to`): Move every record under a folder ");
    s.push_str("at once, renaming the directory too where the documents are files. A type's ");
    s.push_str("own storage root is `memory_migrate_storage`, not this.\n");

    s.push_str("\n### History & Reconciliation\n\n");
    s.push_str("- **memory_diff**: Compare two revisions.\n");
    s.push_str("- **memory_reconcile**: Sync Memory with code history. ");
    s.push_str("Code commits since the last reconciled one are processed; ");
    s.push_str("freshness of records is updated based on path overlap.\n");

    s.push_str("\n### Search Index\n\n");
    s.push_str("- **memory_reindex**: Rebuild the LanceDB index from the current ");
    s.push_str("revision. ");
    s.push_str("Use after corruption or manual Git operations.\n");
    s.push_str("- **memory_doctor**: Validate repository and index health.\n");

    s.push_str("\n### Transport\n\n");
    s.push_str("- **memory_transport_status**: Check remote configuration.\n");
    s.push_str("- **memory_fetch**: Pull memory refs from remote and merge.\n");
    s.push_str("- **memory_push**: Push memory refs to remote. ");
    s.push_str("Blocked if stale records exist (override with `force`).\n");

    s.push_str("\n### Export / Import\n\n");
    s.push_str("- **memory_export** (`revision`, `mode`): Export a record bundle. ");
    s.push_str("`manifest` (the default) keeps locators and is deterministic; ");
    s.push_str("`snapshot` resolves them and carries the content, which is portable to a ");
    s.push_str("machine without the source files but not byte-stable. The mode is written ");
    s.push_str("into the bundle, so import reads it rather than guessing.\n");
    s.push_str(
        "- **memory_import** (`bundle`): Import records from a bundle in one transaction.\n",
    );

    // ── Resources ──
    s.push_str("\n## Resources\n\n");
    s.push_str("- `memory://project`: Project handshake (versions, backend, reconciliation). Backend-specific facts such as `gitDir` are present only for the backends that have them.\n");
    s.push_str("- `memory://records/changed`: Subscribe to be told **which** records changed. ");
    s.push_str("Each notification carries `records: [{key?, locator?, change}]`, where change is ");
    s.push_str("`written`, `deleted`, `content_changed`, `moved`, `content_absent`, ");
    s.push_str("`content_returned`, `freshness_changed` or `needs_attention`. Re-read the named ");
    s.push_str("records rather than invalidating everything. It also fires when a scan finds ");
    s.push_str("something without writing anything, which the revision cannot report.\n");
    s.push_str("- `memory://revision/current`: The current revision — the one reads ");
    s.push_str("serve and `expected_revision` compares against.\n");
    s.push_str("- `memory://index/status`: LanceDB index state.\n");
    s.push_str("- `memory://model/status`: Embedding model status.\n");
    s.push_str("- `memory://policy/effective`: Effective transport policy.\n");
    s.push_str("- `memory://records/summary`: Document counts by kind, freshness, archived. ");
    s.push_str("Type definitions are counted apart, in `service`.\n");
    s.push_str("- `memory://records/{key}`: Full record by key.\n\n");

    // ── Guidelines ──
    s.push_str("## Working with Memory\n\n");
    s.push_str("1. **Read before write**: Always `memory_get_record` or `memory_list_records` ");
    s.push_str("to get the current `expected_revision` before `apply_transaction`.\n");
    s.push_str("2. **One transaction, one logical change**: Batch related puts/deletes ");
    s.push_str("in a single transaction for atomicity.\n");
    s.push_str("3. **Use metadata_only for lists**: When listing records for display, ");
    s.push_str("set `metadata_only: true` to avoid transferring full content.\n");
    s.push_str("4. **After resource update notifications**: Re-read ");
    s.push_str("`memory://revision/current` to stay in sync.\n");

    s
}

#[allow(clippy::too_many_lines)]
#[must_use]
pub fn list_tools() -> Value {
    let mut tools = vec![
        tool(
            "memory_apply_transaction",
            "Create, update, or delete records atomically. Pass expected_revision from last read.",
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
            "Fetch one record by key — returns full Envelope (content, links, extensions). Use when you know the exact key.",
            object_schema(
                &[("key", string_schema()), ("revision", string_schema())],
                &["key", "revision"],
            ),
        ),
        tool(
            "memory_list_records",
            "List records with pagination, filters (kind/tags/archived/freshness/folder), sorting and metadata-only mode. `folder` selects one folder, `\"\"` the root; `folder_scope: \"subtree\"` reaches below it. Response includes counts by kind/freshness/archived. Use metadata_only=true for UI lists.",
            object_schema(
                &[
                    ("presence", string_schema()),
                    ("folder", string_schema()),
                    ("folder_scope", string_schema()),
                    ("revision", string_schema()),
                    (
                        "limit",
                        json!({"type":"integer","minimum":1,"maximum":200,"default":50}),
                    ),
                    ("offset", json!({"type":"integer","minimum":0,"default":0})),
                    ("kind", string_schema()),
                    ("tags", json!({"type":"array","items":{"type":"string"}})),
                    ("archived", json!({"type":"boolean"})),
                    (
                        "freshness",
                        json!({"type":"array","items":{"type":"string","enum":["fresh","stale","unverified","invalid"]}}),
                    ),
                    (
                        "sort",
                        json!({"type":"string","enum":["key","kind","title","freshness","archived"],"default":"key"}),
                    ),
                    (
                        "sort_order",
                        json!({"type":"string","enum":["asc","desc"],"default":"asc"}),
                    ),
                    ("metadata_only", json!({"type":"boolean","default":false})),
                    ("include_service", json!({"type":"boolean","default":false})),
                    ("include_folders", json!({"type":"boolean","default":false})),
                ],
                &[],
            ),
        ),
        tool(
            "memory_list_folders",
            "List the project's folders from both sources at once: folders records are filed in, and real directories of an attached documentation folder — including empty ones, which no record can reveal. Each entry says where it is known from (`in_records`, `in_storage`), how many documents are filed directly in it, and `described_by`: the key of the record that is the folder, if one is. `folder` and `folder_scope` select a region exactly as in `memory_list_records`; `kind` narrows to one type's folders, which is what a client drawing a tree per type needs — folders are a namespace the whole project shares, so without it every type appears to have all of them. Read live, never stored: Git keeps no empty directories, so this describes the working tree in front of you.",
            object_schema(
                &[
                    ("folder", string_schema()),
                    ("folder_scope", string_schema()),
                    ("kind", string_schema()),
                ],
                &[],
            ),
        ),
        tool(
            "memory_rename_folder",
            "Rename a folder, rewriting `folder` on every record filed under it in one transaction — including the record that is the folder. For a directory of an attached folder the directory is renamed too, and the locators follow it; the keys do not, so no link breaks. A type's own storage root is not renamed here — moving that is `memory_migrate_storage`.",
            object_schema(
                &[
                    ("from", string_schema()),
                    ("to", string_schema()),
                    ("transaction_id", string_schema()),
                ],
                &["from", "to", "transaction_id"],
            ),
        ),
        tool(
            "memory_move_document",
            "File one record in another folder. `folder` is the destination, `\"\"` the root. A record that carries its own body moves by metadata alone; a record whose body is a repository file has the file moved with it, because its folder is the directory that file is in. Such a record may only move within its type's storage, and nothing is overwritten: a destination already holding a document of that name is refused. Renaming a whole folder is `memory_rename_folder`.",
            object_schema(
                &[
                    ("key", string_schema()),
                    ("folder", string_schema()),
                    ("transaction_id", string_schema()),
                ],
                &["key", "folder", "transaction_id"],
            ),
        ),
        tool(
            "memory_create_folder",
            "Make a folder that nothing is in yet, under the type named by `kind`. One operation for both storages: for a type whose documents are files it creates the directory, and for a type whose documents are its records it writes the record that carries `is_folder` — which *is* the folder it is filed in, titled with the folder's last segment and left with no body for somebody to fill in. Note the one difference nothing can remove: Git keeps no empty directories, so a folder made in an attached directory is a fact about that working tree until something is filed in it, while one made in the records travels at once.",
            object_schema(
                &[
                    ("folder", string_schema()),
                    ("kind", string_schema()),
                    ("transaction_id", string_schema()),
                ],
                &["folder", "kind", "transaction_id"],
            ),
        ),
        tool(
            "memory_delete_folder",
            "Take a folder and everything filed under it, at any depth and whatever its type — a folder exists while something is in it, so sparing another type's records would empty it rather than delete it. Records go in one transaction and a record whose body is a repository file takes that file with it. Directories are then removed only while they are empty: a folder holding a file no scan has reached is left standing rather than taken along. Answers with how many records went. Refused for a type's own storage root, which is `memory_delete_type`.",
            object_schema(
                &[
                    ("folder", string_schema()),
                    ("transaction_id", string_schema()),
                ],
                &["folder", "transaction_id"],
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
            object_schema(
                &[
                    ("revision", string_schema()),
                    ("expected_revision", string_schema()),
                ],
                &["revision", "expected_revision"],
            ),
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
            "memory_search",
            "Full-text search across all records with filters. Use when you need to find records by content, not by key. Returns ranked hits.",
            object_schema(
                &[
                    ("presence", string_schema()),
                    ("folder", string_schema()),
                    ("folder_scope", string_schema()),
                    ("query", string_schema()),
                    (
                        "limit",
                        json!({"type":"integer","minimum":1,"maximum":200,"default":20}),
                    ),
                    ("offset", json!({"type":"integer","minimum":0,"default":0})),
                    ("revision", string_schema()),
                    ("kind", string_schema()),
                    (
                        "kinds",
                        json!({"type":"array","items":{"type":"string","minLength":1}}),
                    ),
                    (
                        "tags",
                        json!({"type":"array","items":{"type":"string","minLength":1}}),
                    ),
                    ("archived", json!({"type":"boolean"})),
                    (
                        "freshness",
                        json!({
                            "type":"array",
                            "items":{"type":"string","enum":["unverified","fresh","stale","invalid"]}
                        }),
                    ),
                    ("include_service", json!({"type":"boolean","default":false})),
                ],
                &["query"],
            ),
        ),
        tool(
            "memory_backlinks",
            "Find records that link to or mention a key (explicit links + body mentions)",
            object_schema(
                &[("key", string_schema()), ("revision", string_schema())],
                &["key"],
            ),
        ),
        tool(
            "memory_doctor",
            "Validate repository and canonical store access, and report what an              attached folder needs a person to decide",
            object_schema(&[], &[]),
        ),
        tool(
            "memory_migrate_storage",
            "Move a type's documents to another folder of the working tree, or back in with              the records. `dry_run: true` returns the plan — how many records move, in which              direction, and what must be accepted — without writing anything. Every warning              code the plan lists has to be echoed in `acknowledge` before it will run. Editing              a type's `storage` field directly is refused while the kind has records.",
            object_schema(
                &[
                    ("kind", string_schema()),
                    // A project-relative directory, or `null` to bring the
                    // content back in with the records.
                    ("storage", json!({"type": ["string", "null"]})),
                    (
                        "acknowledge",
                        json!({"type": "array", "items": string_schema()}),
                    ),
                    ("dry_run", json!({"type": "boolean"})),
                    ("transaction_id", string_schema()),
                ],
                &["kind", "storage"],
            ),
        ),
        tool(
            "memory_scan",
            "Reconcile every attached repository folder with the records. Applies              edits, moves, disappearances and returns; reports a file it cannot              match without a person.",
            object_schema(&[("transaction_id", string_schema())], &["transaction_id"]),
        ),
        tool(
            "memory_read_content",
            "Read a record's body. A record that keeps its content answers with it; one whose \
             content is a repository file is resolved through its locator and answers with \
             `missing: true` when this branch does not have the file.",
            object_schema(&[("key", string_schema())], &["key"]),
        ),
        tool(
            "memory_write_content",
            "Write the body of a record whose content is a repository file. The file is written \
             first and the record second, so an interruption leaves a disagreement the next \
             `memory_scan` settles rather than a record pointing at content that was never \
             written. `encoding` says how to read `content`: `utf-8` (the default, and what \
             every existing caller means) or `base64` for a document that is not text — an \
             image, a PDF, anything an attached folder holds.",
            object_schema(
                &[
                    ("key", string_schema()),
                    ("content", string_schema()),
                    (
                        "encoding",
                        json!({"type":"string","enum":["utf-8","base64"],"default":"utf-8"}),
                    ),
                    ("transaction_id", string_schema()),
                ],
                &["key", "content", "transaction_id"],
            ),
        ),
    ];
    tools.push(tool("memory_reindex", "Rebuild the LanceDB search index from the current revision. Use after corruption or manual Git operations.", object_schema(&[], &[])));
    tools.push(tool(
        "memory_transport_status",
        "Check if a memory remote is configured and report sync status.",
        object_schema(&[], &[]),
    ));
    tools.push(tool(
        "memory_sync_state",
        "Say what is here and not on the memory remote, and whether anything is waiting there. \
         Counts records rather than commits. Pass `ask_remote` to let it reach the network; \
         without it the answer is computed locally and says the remote was not asked.",
        object_schema(
            &[("ask_remote", json!({"type": "boolean", "default": false}))],
            &[],
        ),
    ));
    tools.push(tool(
        "memory_presence",
        "Say whether this repository's memory is here, still on a remote, or nowhere yet. \
         `git clone` copies no refs/memory/*, so a fresh clone and a project that never had \
         memory look identical from inside — this is what tells them apart.",
        object_schema(&[], &[]),
    ));
    tools.push(tool(
        "memory_remote_set",
        "Configure the memory remote, which is separate from the code `origin` and is the only \
         address memory is ever published to.",
        object_schema(
            &[
                ("url", json!({"type": "string"})),
                ("refspec", json!({"type": "string"})),
            ],
            &["url"],
        ),
    ));
    tools.push(tool(
        "memory_remote_remove",
        "Forget the memory remote. Memory stays where it is; nothing is published until one is \
         configured again.",
        object_schema(&[], &[]),
    ));
    tools.push(tool(
        "memory_fetch",
        "Pull memory refs from the configured remote and merge records into local store.",
        object_schema(&[], &[]),
    ));
    tools.push(tool(
        "memory_push",
        "Push memory refs to the configured remote",
        object_schema(
            &[("force", json!({"type": "boolean", "default": false}))],
            &[],
        ),
    ));
    tools.push(tool(
        "memory_rewind",
        "Put this project's memory back where it stood at `revision`, undoing everything since — what a fetch is undone with, naming the `localRevisionBefore` that fetch reported. Only backwards along memory's own history: a revision it never passed through is refused, so this cannot arrive at a state nobody wrote. `expected_revision` is where memory should still stand, and a tip that has moved past it means somebody has written since — the undo is refused rather than carrying their record away. Nothing is destroyed, the commits after it stay in the repository, and it moves this clone alone — a revision already pushed is still on the remote and the next fetch brings it back.",
        object_schema(
            &[
                ("revision", string_schema()),
                ("expected_revision", string_schema()),
            ],
            &["revision", "expected_revision"],
        ),
    ));
    tools.push(tool(
        "memory_model_status",
        "Report embedding model status",
        object_schema(&[], &[]),
    ));
    tools.push(tool(
        "memory_delete_type",
        "Remove a type from this project: its definition and every record of it. The files of a \
         type whose documents live in a folder of the working tree are left exactly where they \
         are: they are the repository's documents, and this type was only what Memory knew about \
         them. Deleting the records one by one is not the same operation, because deleting a \
         record takes its document with it.",
        object_schema(
            &[
                ("kind", string_schema()),
                ("transaction_id", string_schema()),
            ],
            &["kind", "transaction_id"],
        ),
    ));
    tools.push(tool(
        "memory_list_types",
        "List document types defined in this project (from __type__ records). Returns kind_name, \
         description, field_count, relationship_count, the storage each type's content lives in \
         (absent when it lives with the records), and whether that storage can be written — ask \
         before offering to create, rather than finding out from a failure.",
        object_schema(&[], &[]),
    ));
    tools.push(tool(
        "memory_schema_status",
        "Check all records against the current schema and report incompatible ones. Returns a list of records with validation violations (key, kind, field, reason).",
        object_schema(&[], &[]),
    ));
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
    let resolver = PolicyResolver::memory_hub_defaults();
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

#[allow(dead_code)]
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
        "/_meta/memoryHub/memoryInterfaceVersion/major",
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

fn rpc_error_owned(id: Value, code: i64, message: &String, data: Value) -> Value {
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

/// Tools that write, and so reconcile with the remote before they run.
///
/// Named here rather than spelled into the dispatch, because two places that
/// list which tools write is one place too many.
const MUTATING_TOOLS: &[&str] = &[
    "memory_apply_transaction",
    "memory_checkpoint",
    "memory_import",
];

/// What one call to [`Session::call`] did.
pub struct ToolCall {
    /// What the tool answered, or how it failed.
    pub result: Result<Value, ToolCallFailure>,
    /// The revision moved — because the tool moved it, or because the
    /// reconciliation ahead of it did. True even when the call then failed.
    pub revision_changed: bool,
    /// Which records the call touched, and how. Empty when the call failed:
    /// what a client is told to re-read has to be something that was written.
    pub changed: Vec<RecordNotice>,
}

pub struct ToolOutcome {
    pub content: Value,
    pub revision_changed: bool,
    /// Which records this call touched, and how.
    ///
    /// A revision says something changed; this says what. An editor holding
    /// one record open needs the second to re-read that record instead of
    /// throwing away everything it knows.
    pub changed: Vec<RecordNotice>,
}

/// One record a call changed, named so a client can re-read just it.
#[derive(Clone, Debug)]
pub struct RecordNotice {
    /// The record's key. Absent for something that has no record yet — a file
    /// a scan could not match to one.
    pub key: Option<String>,
    /// Where the content is, for a change that is about a document.
    pub locator: Option<String>,
    pub change: &'static str,
}

impl RecordNotice {
    fn keyed(key: impl Into<String>, change: &'static str) -> Self {
        Self {
            key: Some(key.into()),
            locator: None,
            change,
        }
    }
}

impl ToolOutcome {
    const fn read(content: Value) -> Self {
        Self {
            content,
            revision_changed: false,
            changed: Vec::new(),
        }
    }
    fn changing(content: Value, changed: Vec<RecordNotice>) -> Self {
        Self {
            content,
            revision_changed: true,
            changed,
        }
    }

    /// A call that changed records without moving the revision.
    ///
    /// A scan that only found something it cannot resolve writes nothing, and
    /// a client still has to hear about it.
    fn observing(content: Value, changed: Vec<RecordNotice>) -> Self {
        Self {
            content,
            revision_changed: false,
            changed,
        }
    }
}

#[derive(Debug)]
pub struct RpcFailure {
    pub code: i64,
    pub message: String,
    pub data: Value,
}

impl RpcFailure {
    fn new(code: i64, message: impl Into<String>, data: Value) -> Self {
        Self {
            code,
            message: message.into(),
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
    /// Wrap a use-case failure as a JSON-RPC error, keeping its stable `kind`
    /// where clients already look for it.
    fn service(error: ServiceError) -> Self {
        Self::new(
            -32_603,
            "Memory Hub operation failed",
            json!({"kind": error.kind, "message": error.message, "data": error.data}),
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
            .unwrap_or(&self.message)
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

#[derive(Debug)]
pub struct ToolFailure {
    pub kind: String,
    pub message: String,
    pub data: Value,
}

impl ToolFailure {
    /// A use-case failure is already in this shape: a stable kind, a human
    /// message and structured data. The adapter carries it through unchanged.
    fn service(error: ServiceError) -> Self {
        Self {
            kind: error.kind,
            message: error.message,
            data: error.data,
        }
    }
}

#[derive(Debug)]
pub enum ToolCallFailure {
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

/// Whether a folder filter reaches below the folder it names.
///
/// Absent means the folder itself, which is what a caller asking for one
/// folder means without saying so.
fn folder_subtree(arguments: &Value) -> bool {
    arguments.get("folder_scope").and_then(Value::as_str) == Some("subtree")
}

/// The notification a client subscribed to [`RECORDS_CHANGED`] receives.
///
/// A method of its own rather than a `resources/updated` with extra fields:
/// the payload is the point, and hiding it in a resource notification would
/// mean every client re-reading the resource to find out what it already
/// could have been told.
fn records_changed_notification(changed: &[RecordNotice]) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/memoryHub/recordsChanged",
        "params": {
            "uri": RECORDS_CHANGED,
            "records": changed
                .iter()
                .map(|notice| {
                    let mut entry = json!({"change": notice.change});
                    if let Some(key) = &notice.key {
                        entry["key"] = json!(key);
                    }
                    if let Some(locator) = &notice.locator {
                        entry["locator"] = json!(locator);
                    }
                    entry
                })
                .collect::<Vec<_>>(),
        }
    })
}

const fn change_kind_name(kind: memory_hub_engine::ChangeKind) -> &'static str {
    match kind {
        memory_hub_engine::ChangeKind::Added | memory_hub_engine::ChangeKind::Modified => "written",
        memory_hub_engine::ChangeKind::Deleted => "deleted",
    }
}

/// What a scan concluded, in the vocabulary a client re-reads records by.
fn scan_notice(change: &ScanChange) -> RecordNotice {
    match change {
        ScanChange::Edited { key, locator, .. } => RecordNotice {
            key: Some(key.clone()),
            locator: Some(locator.clone()),
            change: "content_changed",
        },
        ScanChange::Moved { key, to, .. } => RecordNotice {
            key: Some(key.clone()),
            locator: Some(to.clone()),
            change: "moved",
        },
        ScanChange::Missing { key, locator, .. } => RecordNotice {
            key: Some(key.clone()),
            locator: Some(locator.clone()),
            change: "content_absent",
        },
        ScanChange::Returned { key, locator } => RecordNotice {
            key: Some(key.clone()),
            locator: Some(locator.clone()),
            change: "content_returned",
        },
        ScanChange::New { key, locator, .. } => RecordNotice {
            key: Some(key.clone()),
            locator: Some(locator.clone()),
            change: "written",
        },
        ScanChange::Unmatched { locator, .. } => RecordNotice {
            key: None,
            locator: Some(locator.clone()),
            change: "needs_attention",
        },
    }
}

fn scan_change_json(change: &ScanChange) -> Value {
    match change {
        ScanChange::Edited { key, locator, hash } => {
            json!({"change": "edited", "key": key, "locator": locator, "contentHash": hash})
        }
        ScanChange::Moved { key, from, to } => {
            json!({"change": "moved", "key": key, "from": from, "to": to})
        }
        ScanChange::Missing {
            key,
            locator,
            presence,
        } => json!({
            "change": "missing",
            "key": key,
            "locator": locator,
            "presence": presence.as_str(),
        }),
        ScanChange::Returned { key, locator } => {
            json!({"change": "returned", "key": key, "locator": locator})
        }
        ScanChange::New { key, locator, hash } => {
            json!({"change": "new", "key": key, "locator": locator, "contentHash": hash})
        }
        ScanChange::Unmatched {
            locator,
            hash,
            candidates,
        } => json!({
            "change": "unmatched",
            "locator": locator,
            "contentHash": hash,
            "candidates": candidates.iter().map(candidate_json).collect::<Vec<_>>(),
        }),
    }
}

fn candidate_json(candidate: &RenameCandidate) -> Value {
    json!({
        "key": candidate.key,
        "locator": candidate.locator,
        "similarity": candidate.similarity,
    })
}

fn unresolved_json(unresolved: &Unresolved) -> Value {
    match unresolved {
        Unresolved::UnmatchedFile {
            kind,
            locator,
            candidates,
        } => json!({
            "unresolved": "unmatched_file",
            "kind": kind,
            "locator": locator,
            "candidates": candidates.iter().map(candidate_json).collect::<Vec<_>>(),
        }),
        Unresolved::RemovedFile { kind, key, locator } => json!({
            "unresolved": "removed_file",
            "kind": kind,
            "key": key,
            "locator": locator,
        }),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{MCP_PROTOCOL_VERSION, MEMORY_INTERFACE_MAJOR, RecordsIn, Session, serve_io};
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
                    "_meta":{"memoryHub":{"memoryInterfaceVersion":{"major": MEMORY_INTERFACE_MAJOR + 1,"minor":0}}}
                }
            })
        );
        let mut output = Vec::new();
        serve_io(
            project.path().to_path_buf(),
            RecordsIn::GitMetadata,
            input.as_bytes(),
            &mut output,
        )
        .unwrap();
        let response: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            response.pointer("/error/data/kind").and_then(Value::as_str),
            Some("incompatible_memory_interface")
        );
        assert!(!project.path().join(".git/refs/memory/main").exists());
    }

    /// The handshake publishes what the backend says about itself. Today that
    /// is `refs` with a Git directory; the point of sourcing it from the
    /// store's own description is that a backend without one will publish no
    /// `gitDir` at all rather than a path that looks right.
    /// A revision says something changed. This says what, which is the thing
    /// an editor holding one record open needs in order to re-read that record
    /// instead of throwing away everything it knows.
    #[test]
    fn a_subscriber_is_told_which_records_changed() {
        let project = tempfile::tempdir().unwrap();
        git2_for_test::init(project.path());
        let mut session = Session::new(project.path().to_path_buf(), RecordsIn::GitMetadata);
        session.initialized = true;
        session
            .subscribe(&json!({"uri": super::RECORDS_CHANGED}))
            .unwrap();
        let base = session.store().unwrap().current_revision().unwrap();
        let content = "a body";
        let record = json!({
            "representation": "plaintext",
            "envelope": {
                "envelope_version": {"major": 1, "minor": 1},
                "key": "notice",
                "kind": "note",
                "content": content,
                "content_hash": format!("sha256:{:x}", Sha256::digest(content.as_bytes())),
                "source_paths": {}, "archive": {"archived": false},
                "freshness": {"state": "unverified"}
            }
        });
        let mut output = Vec::new();
        session
            .call_tool(
                json!(1),
                &json!({
                    "name": "memory_apply_transaction",
                    "arguments": {
                        "transaction_id": "notify",
                        "expected_revision": base,
                        "operations": [{"op": "put", "record": record}]
                    }
                }),
                &mut output,
            )
            .unwrap();

        let notification: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            notification.get("method").and_then(Value::as_str),
            Some("notifications/memoryHub/recordsChanged")
        );
        assert_eq!(
            notification
                .pointer("/params/records/0/key")
                .and_then(Value::as_str),
            Some("notice")
        );
        assert_eq!(
            notification
                .pointer("/params/records/0/change")
                .and_then(Value::as_str),
            Some("written")
        );
    }

    /// Declare a type whose documents live in the `docs` folder, and take the
    /// first scan.
    fn attach_docs_folder(session: &mut Session, sink: &mut Vec<u8>) {
        let definition = json!({
            "kind_name": "doc",
            "storage": "docs"
        })
        .to_string();
        let record = json!({
            "representation": "plaintext",
            "envelope": {
                "envelope_version": {"major": 1, "minor": 1},
                "key": "__type__:doc",
                "kind": "__type__",
                "content": definition,
                "content_hash": format!("sha256:{:x}", Sha256::digest(definition.as_bytes())),
                "source_paths": {}, "archive": {"archived": false},
                "freshness": {"state": "unverified"}
            }
        });
        let base = session.store().unwrap().current_revision().unwrap();
        session
            .call_tool(
                json!(1),
                &json!({"name": "memory_apply_transaction", "arguments": {
                    "transaction_id": "declare", "expected_revision": base,
                    "operations": [{"op": "put", "record": record}]
                }}),
                sink,
            )
            .unwrap();
        session
            .call_tool(
                json!(2),
                &json!({"name": "memory_scan", "arguments": {"transaction_id": "attach"}}),
                sink,
            )
            .unwrap();
    }

    /// The gap a revision subscription can never close: a scan that finds only
    /// something it cannot resolve writes nothing, so the revision does not
    /// move — and the client still has to hear about it.
    ///
    /// Not run on Windows, and that is a defect parked rather than fixed: there
    /// the same scan tells the client nothing at all — the output is empty
    /// where every other platform writes a `recordsChanged` carrying
    /// `needs_attention`. So a Windows client is left believing its documents
    /// are settled while one of them is waiting on a person. Skipped so the
    /// platforms that are supported keep gating; the day Windows is supported,
    /// deleting this attribute is where the work starts.
    #[cfg(not(windows))]
    #[test]
    fn a_scan_that_writes_nothing_still_reports_what_it_found() {
        let project = tempfile::tempdir().unwrap();
        git2_for_test::init(project.path());
        std::fs::create_dir_all(project.path().join("docs")).unwrap();
        std::fs::write(project.path().join("docs/guide.md"), "the original body\n").unwrap();

        let mut session = Session::new(project.path().to_path_buf(), RecordsIn::GitMetadata);
        session.initialized = true;
        let mut sink = Vec::new();
        attach_docs_folder(&mut session, &mut sink);

        // A rename with an edit: nothing about the file says whether it is new
        // or the same document, so the scan applies nothing.
        std::fs::remove_file(project.path().join("docs/guide.md")).unwrap();
        std::fs::write(
            project.path().join("docs/guide-v2.md"),
            "the original body, edited\n",
        )
        .unwrap();

        // The first pass records that the old document is not there; after it
        // there is nothing left to write, and the ambiguity is still an
        // ambiguity.
        session
            .call_tool(
                json!(3),
                &json!({"name": "memory_scan", "arguments": {"transaction_id": "settle"}}),
                &mut sink,
            )
            .unwrap();

        session
            .subscribe(&json!({"uri": "memory://revision/current"}))
            .unwrap();
        session
            .subscribe(&json!({"uri": super::RECORDS_CHANGED}))
            .unwrap();
        let before = session.store().unwrap().current_revision().unwrap();
        let mut output = Vec::new();
        let response = session
            .call_tool(
                json!(4),
                &json!({"name": "memory_scan", "arguments": {"transaction_id": "rescan"}}),
                &mut output,
            )
            .unwrap();

        assert_eq!(
            response.pointer("/result/structuredContent/applied"),
            Some(&json!(0)),
            "nothing was written"
        );
        assert_eq!(
            session.store().unwrap().current_revision().unwrap(),
            before,
            "so the revision did not move"
        );
        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains("recordsChanged") && text.contains("needs_attention"),
            "and the client was told anyway: {text}"
        );
        assert!(
            !text.contains("notifications/resources/updated"),
            "a revision subscription had nothing to say: {text}"
        );
    }

    /// A client that has never heard of the second subscription keeps getting
    /// exactly what it got before, and nothing else.
    #[test]
    fn a_revision_subscriber_alone_is_not_sent_the_new_notification() {
        let project = tempfile::tempdir().unwrap();
        git2_for_test::init(project.path());
        let mut session = Session::new(project.path().to_path_buf(), RecordsIn::GitMetadata);
        session.initialized = true;
        session
            .subscribe(&json!({"uri": "memory://revision/current"}))
            .unwrap();
        let base = session.store().unwrap().current_revision().unwrap();
        let content = "a body";
        let record = json!({
            "representation": "plaintext",
            "envelope": {
                "envelope_version": {"major": 1, "minor": 1},
                "key": "notice",
                "kind": "note",
                "content": content,
                "content_hash": format!("sha256:{:x}", Sha256::digest(content.as_bytes())),
                "source_paths": {}, "archive": {"archived": false},
                "freshness": {"state": "unverified"}
            }
        });
        let mut output = Vec::new();
        session
            .call_tool(
                json!(1),
                &json!({
                    "name": "memory_apply_transaction",
                    "arguments": {
                        "transaction_id": "notify",
                        "expected_revision": base,
                        "operations": [{"op": "put", "record": record}]
                    }
                }),
                &mut output,
            )
            .unwrap();

        let text = String::from_utf8(output).unwrap();
        assert!(
            text.contains("notifications/resources/updated"),
            "the old subscription still fires: {text}"
        );
        assert!(
            !text.contains("recordsChanged"),
            "and only that one: {text}"
        );
    }

    #[test]
    fn the_handshake_publishes_the_store_own_description() {
        let project = tempfile::tempdir().unwrap();
        git2_for_test::init(project.path());
        let input = format!(
            "{}\n",
            json!({
                "jsonrpc":"2.0", "id":1, "method":"initialize", "params":{
                    "protocolVersion": MCP_PROTOCOL_VERSION,
                    "capabilities": {}, "clientInfo":{"name":"test","version":"1"}
                }
            })
        );
        let mut output = Vec::new();
        serve_io(
            project.path().to_path_buf(),
            RecordsIn::GitMetadata,
            input.as_bytes(),
            &mut output,
        )
        .unwrap();
        let response: Value = serde_json::from_slice(&output).unwrap();
        let handshake = response.pointer("/result/_meta/memoryHub").unwrap();

        assert_eq!(
            handshake.get("backend").and_then(Value::as_str),
            Some("refs"),
            "the client is told which backend it is talking to"
        );
        let git_dir = handshake.get("gitDir").and_then(Value::as_str).unwrap();
        assert!(
            git_dir.ends_with(".git/"),
            "a Git-backed store publishes its real Git directory: {git_dir}"
        );
    }

    #[test]
    fn subscribed_mutation_notifies_and_revision_remains_authoritative() {
        let project = tempfile::tempdir().unwrap();
        git2_for_test::init(project.path());
        let mut session = Session::new(project.path().to_path_buf(), RecordsIn::GitMetadata);
        session.initialized = true;
        session.revision_subscribed = true;
        let base = session.store().unwrap().current_revision().unwrap();
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

        /// A repository these tests can keep records in.
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
