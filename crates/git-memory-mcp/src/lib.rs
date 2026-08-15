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
use std::sync::Arc;

use git_memory_core::{CURRENT_ENVELOPE_VERSION, Envelope, PolicyResolver, StoredRecord};
use git_memory_crypto::load_ssh_identity;
use git_memory_embed::{EmbeddingProvider, ModelRuntime, ModelStatusBuilder};
use git_memory_index::{IndexError, Projection, SearchFilters, SearchRequest};
use git_memory_reconcile::{DivergenceMode, ReconcileError, ReconcileErrorKind, Reconciler};
use git_memory_store::{
    EncryptedStore, GitStore, Operation, RecordId, Revision, StoreError, Transaction,
    is_encrypted_project,
};
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
    encrypted_store: Option<EncryptedStore>,
    embed_provider: Option<Arc<dyn EmbeddingProvider>>,
}

impl Session {
    fn new(project: PathBuf) -> Self {
        let encrypted_store = is_encrypted_project(&project)
            .unwrap_or(false)
            .then(|| EncryptedStore::open_locked(&project).ok())
            .flatten();
        // Encrypted projects use an ephemeral index: it exists only while
        // unlocked in a live session. If a previous process was killed before
        // `memory_lock` could destroy the index, plaintext LanceDB/WAL/temp
        // files may remain on disk. Wipe them on every start so no plaintext
        // survives a crash/restart cycle — the index is rebuilt on `unlock`.
        if encrypted_store.is_some() {
            if let Ok(git_store) = GitStore::open(&project) {
                let _ = Projection::destroy_store_silent(&git_store);
            }
        }
        Self {
            project,
            initialized: false,
            revision_subscribed: false,
            reconciliation: json!({"status": "pending"}),
            encrypted_store,
            embed_provider: None,
        }
    }

    /// Resolve the embedding provider from configuration. Returns `None` when
    /// embedding is not enabled or no model is available — the projection
    /// then operates in FTS-only degraded mode.
    fn resolve_provider(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        None
    }

    /// Return the active embedding provider, resolving it lazily on first use.
    fn provider(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.embed_provider
            .clone()
            .or_else(|| self.resolve_provider())
    }

    fn is_encrypted(&self) -> bool {
        self.encrypted_store.is_some()
    }

    fn is_unlocked(&self) -> bool {
        self.encrypted_store
            .as_ref()
            .is_some_and(EncryptedStore::is_unlocked)
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
        // Encrypted projects use an ephemeral index rebuilt only on `unlock`.
        // When locked, there is no plaintext to index — skip synchronization so
        // we don't recreate an empty LanceDB directory that `Session::new`
        // just wiped as part of crash recovery.
        if !self.is_encrypted() || self.is_unlocked() {
            Projection::synchronize_store_with(&store, self.provider())
                .map_err(RpcFailure::index)?;
        }
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
            "instructions": builtin_instructions(if self.is_encrypted() { "encrypted" } else { "plaintext" }),
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
            "modelFingerprint": self.embed_provider.as_ref().map(|p| {
                git_memory_embed::Fingerprint::from_provider(&**p, p.model_id()).digest()
            }),
            "encryptionMode": if self.is_encrypted() { "encrypted" } else { "plaintext" },
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

    /// Return the encrypted store, or a `locked` error if the project is not
    /// encrypted or has not been unlocked.
    fn require_unlocked_encrypted(&self) -> Result<&EncryptedStore, ToolFailure> {
        let store = self.encrypted_store.as_ref().ok_or_else(|| ToolFailure {
            kind: "not_encrypted".to_owned(),
            message: "project is not encrypted".to_owned(),
            data: json!({}),
        })?;
        if store.is_unlocked() {
            Ok(store)
        } else {
            Err(ToolFailure {
                kind: "locked".to_owned(),
                message: "encrypted store is locked — call memory_unlock first".to_owned(),
                data: json!({"recovery_action": "unlock_with_identity"}),
            })
        }
    }

    /// Synchronize the ephemeral index from decrypted records for encrypted
    /// projects, or from the Git snapshot for plaintext projects.
    fn sync_index(&self) -> Result<(), IndexError> {
        let provider = self.provider();
        if let Some(store) = self.encrypted_store.as_ref()
            && store.is_unlocked()
        {
            let records = store.list().map_err(|e| {
                IndexError::new(format!("decrypt records for index rebuild: {e}"))
            })?;
            let revision = store.current_revision().map_err(|e| {
                IndexError::new(format!("read current revision for index: {e}"))
            })?;
            let git_store = GitStore::open(&self.project).map_err(|e| {
                IndexError::new(format!("open store for index rebuild: {e}"))
            })?;
            Projection::rebuild_from_envelopes_store_with(
                &git_store,
                &records,
                &revision,
                provider,
            )?;
        } else {
            let store = GitStore::open(&self.project).map_err(|e| {
                IndexError::new(format!("open store for index sync: {e}"))
            })?;
            Projection::synchronize_store_with(&store, provider)?;
        }
        Ok(())
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
            "memory://model/status" => self.build_model_status(),
            "memory://policy/effective" => policy_resource(),
            "memory://encryption/status" => self.encryption_status(),
            "memory://records/summary" => self.records_summary()?,
            _ => {
                if let Some(key) = uri.strip_prefix("memory://records/") {
                    if key == "summary" {
                        return Err(resource_not_found(uri));
                    }
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
            Err(error) => return Ok(rpc_error_owned(id, error.code, &error.message, error.data)),
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
                    if let Err(error) = self.sync_index() {
                        return Ok(rpc_result(id, tool_error(ToolFailure::index(error))));
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
                    Ok(rpc_error_owned(id, error.code, &error.message, error.data))
                } else {
                    Ok(rpc_result(id, tool_error(error.into_tool_failure())))
                }
            }
            Err(ToolCallFailure::Tool(error)) => Ok(rpc_result(id, tool_error(error))),
        }
    }

    fn execute_tool(&mut self, name: &str, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
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
            "memory_search" => self.search(arguments),
            "memory_backlinks" => self.backlinks(arguments),
            "memory_transport_status" => self.transport_status(),
            "memory_fetch" => self.fetch(arguments),
            "memory_push" => self.push(arguments),
            "memory_model_status" => self.model_status(),
            "memory_encryption_status" => Ok(ToolOutcome::read(self.encryption_status())),
            "memory_unlock" => self.unlock_store(arguments),
            "memory_lock" => self.lock_store(),
            "memory_init_encrypted" => self.init_encrypted(arguments),
            "memory_list_recipients" => self.list_recipients(),
            "memory_add_recipient" => self.add_recipient(arguments),
            "memory_remove_recipient" => self.remove_recipient(arguments),
            _ => Err(ToolCallFailure::Rpc(RpcFailure::new(
                -32_602,
                "tool not found",
                json!({"kind": "tool_not_found", "name": name}),
            ))),
        }
    }

    fn encryption_status(&self) -> Value {
        let encrypted = self.is_encrypted();
        let unlocked = self.is_unlocked();
        let mode = if encrypted { "encrypted" } else { "plaintext" };
        let state = if !encrypted {
            "plaintext"
        } else if unlocked {
            "unlocked"
        } else {
            "locked"
        };
        json!({
            "schemaVersion": 1,
            "mode": mode,
            "state": state,
            "available": true,
            "encryptedStoreAvailable": encrypted,
            "encryptedIndexAvailable": encrypted && unlocked,
            "ephemeralIndex": encrypted
        })
    }

    fn records_summary(&self) -> Result<Value, RpcFailure> {
        let (revision, envelopes): (Revision, Vec<(String, Envelope)>) = if self.is_encrypted() {
            let store = self.require_unlocked_encrypted().map_err(|e| RpcFailure::new(
                -32_602,
                e.message,
                e.data,
            ))?;
            let records = store.list().map_err(RpcFailure::store)?;
            let rev = store.current_revision().map_err(RpcFailure::store)?;
            (rev, records)
        } else {
            let store = self.store()?;
            let snapshot = store.current().map_err(RpcFailure::store)?;
            let rev = snapshot.revision().clone();
            let records = snapshot.records().map_err(RpcFailure::store)?;
            let envelopes = records
                .into_iter()
                .filter_map(|(id, record)| match record {
                    StoredRecord::Plaintext { envelope } => {
                        Some((id.display_value(), *envelope))
                    }
                    StoredRecord::Encrypted { .. } => None,
                })
                .collect();
            (rev, envelopes)
        };
        let total = envelopes.len();
        let mut by_kind: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        let mut by_freshness: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        let mut archived = 0usize;
        for (_, env) in &envelopes {
            *by_kind.entry(env.kind.clone()).or_default() += 1;
            let state = freshness_str(env.freshness.state).to_owned();
            *by_freshness.entry(state).or_default() += 1;
            if env.archive.archived {
                archived += 1;
            }
        }
        Ok(json!({
            "schemaVersion": 1,
            "revision": revision,
            "total": total,
            "by_kind": by_kind,
            "by_freshness": by_freshness,
            "archived": archived,
            "live": total - archived,
        }))
    }

    fn apply_transaction(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let transaction_id = required_string(arguments, "transaction_id")?.to_owned();
        let expected_revision: Revision = parse_field(arguments, "expected_revision")?;
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
        if self.is_encrypted() {
            let store = self.require_unlocked_encrypted()?;
            let mut puts: Vec<(String, Envelope)> = Vec::new();
            let mut deletes: Vec<String> = Vec::new();
            for op in &operations {
                match op {
                    Operation::Put { record } => match record {
                        StoredRecord::Plaintext { envelope } => {
                            puts.push((envelope.key.clone(), (**envelope).clone()));
                        }
                        StoredRecord::Encrypted { .. } => {
                            return Err(ToolFailure {
                                kind: "invalid_argument".to_owned(),
                                message: "encrypted projects accept plaintext envelopes only — the store encrypts them".to_owned(),
                                data: json!({}),
                            }.into());
                        }
                    },
                    Operation::Delete { id } => match id {
                        RecordId::Plaintext(key) => deletes.push(key.clone()),
                        RecordId::Opaque(_) => {
                            return Err(ToolFailure {
                                kind: "invalid_argument".to_owned(),
                                message: "encrypted projects delete by semantic key, not opaque id".to_owned(),
                                data: json!({}),
                            }.into());
                        }
                    },
                }
            }
            let puts_refs: Vec<(&str, Envelope)> = puts
                .iter()
                .map(|(k, e)| (k.as_str(), e.clone()))
                .collect();
            let deletes_refs: Vec<&str> = deletes.iter().map(String::as_str).collect();
            let result = store
                .apply(&transaction_id, expected_revision, &puts_refs, &deletes_refs)
                .map_err(ToolFailure::store)?;
            Ok(ToolOutcome::mutation(json!(result)))
        } else {
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
    }

    fn get_record(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let key = required_string(arguments, "key")?;
        if self.is_encrypted() {
            let store = self.require_unlocked_encrypted()?;
            let envelope = store.get(key).map_err(ToolFailure::store)?;
            let revision = store.current_revision().map_err(ToolFailure::store)?;
            Ok(ToolOutcome::read(
                json!({"revision": revision, "record": envelope}),
            ))
        } else {
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
    }

    fn list_records(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let limit = arguments
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(50)
            .min(200) as usize;
        let offset = arguments
            .get("offset")
            .and_then(Value::as_u64)
            .unwrap_or(0) as usize;
        let kind_filter = arguments
            .get("kind")
            .and_then(Value::as_str)
            .map(str::to_owned);
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
        let archived_filter = arguments.get("archived").and_then(Value::as_bool);
        let freshness_filters: Vec<String> = arguments
            .get("freshness")
            .and_then(Value::as_array)
            .map(|arr| {
                arr.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default();
        let sort_field = arguments
            .get("sort")
            .and_then(Value::as_str)
            .unwrap_or("key");
        let sort_order = arguments
            .get("sort_order")
            .and_then(Value::as_str)
            .unwrap_or("asc");
        let metadata_only = arguments
            .get("metadata_only")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        // Collect records from the appropriate source.
        let (revision, envelopes): (Revision, Vec<(String, Envelope)>) = if self.is_encrypted() {
            let store = self.require_unlocked_encrypted()?;
            let records = store.list().map_err(ToolFailure::store)?;
            let rev = store.current_revision().map_err(ToolFailure::store)?;
            (rev, records)
        } else {
            let store = self.store()?;
            let snapshot = match arguments.get("revision") {
                Some(value) => store.snapshot(
                    &serde_json::from_value(value.clone())
                        .map_err(|_| RpcFailure::invalid_argument("revision"))?,
                ),
                None => store.current(),
            }
            .map_err(ToolFailure::store)?;
            let rev = snapshot.revision().clone();
            let records = snapshot.records().map_err(ToolFailure::store)?;
            let envelopes = records
                .into_iter()
                .filter_map(|(id, record)| match record {
                    StoredRecord::Plaintext { envelope } => {
                        Some((id.display_value(), *envelope))
                    }
                    StoredRecord::Encrypted { .. } => None,
                })
                .collect::<Vec<_>>();
            (rev, envelopes)
        };

        // Apply filters in-memory.
        let filtered: Vec<&(String, Envelope)> = envelopes
            .iter()
            .filter(|(_, env)| {
                if let Some(ref kind) = kind_filter
                    && &env.kind != kind
                {
                    return false;
                }
                if !tag_filters.is_empty()
                    && !tag_filters.iter().all(|tag| env.tags.iter().any(|t| t == tag))
                {
                    return false;
                }
                if let Some(archived_only) = archived_filter
                    && env.archive.archived != archived_only
                {
                    return false;
                }
                if !freshness_filters.is_empty() {
                    let state = freshness_str(env.freshness.state);
                    if !freshness_filters.iter().any(|f| f == &state) {
                        return false;
                    }
                }
                true
            })
            .collect();

        // Compute counts over the full filtered set.
        let total = filtered.len();
        let mut by_kind: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        let mut by_freshness: std::collections::BTreeMap<String, usize> = std::collections::BTreeMap::new();
        let mut archived_count = 0usize;
        for (_, env) in &filtered {
            *by_kind.entry(env.kind.clone()).or_default() += 1;
            let state = freshness_str(env.freshness.state).to_owned();
            *by_freshness.entry(state).or_default() += 1;
            if env.archive.archived {
                archived_count += 1;
            }
        }
        let counts = json!({
            "total": total,
            "by_kind": by_kind,
            "by_freshness": by_freshness,
            "archived": archived_count,
            "live": total - archived_count,
        });

        // Sort.
        let mut sorted: Vec<&(String, Envelope)> = filtered;
        let descending = sort_order == "desc";
        sorted.sort_by(|a, b| {
            let cmp = match sort_field {
                "kind" => a.1.kind.cmp(&b.1.kind),
                "title" => a.1.title.as_deref().unwrap_or("").cmp(b.1.title.as_deref().unwrap_or("")),
                "freshness" => {
                    let fa = freshness_str(a.1.freshness.state);
                    let fb = freshness_str(b.1.freshness.state);
                    fa.cmp(fb)
                }
                "archived" => a.1.archive.archived.cmp(&b.1.archive.archived),
                _ => a.0.cmp(&b.0), // "key" default
            };
            if descending { cmp.reverse() } else { cmp }
        });

        // Paginate.
        let has_more = sorted.len().saturating_sub(offset) > limit;
        let page: Vec<Value> = sorted
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(key, env)| {
                if metadata_only {
                    json!({
                        "key": key,
                        "kind": env.kind,
                        "title": env.title,
                        "tags": env.tags,
                        "archived": env.archive.archived,
                        "freshness": freshness_str(env.freshness.state),
                        "content_hash": env.content_hash.as_str(),
                    })
                } else {
                    json!({
                        "key": key,
                        "kind": env.kind,
                        "title": env.title,
                        "content": env.content,
                        "tags": env.tags,
                        "links": env.links,
                        "source_paths": env.source_paths,
                        "archive": env.archive,
                        "freshness": env.freshness,
                        "content_hash": env.content_hash.as_str(),
                    })
                }
            })
            .collect();

        Ok(ToolOutcome::read(json!({
            "revision": revision,
            "records": page,
            "total": total,
            "limit": limit,
            "offset": offset,
            "has_more": has_more,
            "counts": counts,
        })))
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
        let provider = self.provider();
        if self.is_encrypted() {
            let store = self.require_unlocked_encrypted()?;
            let records = store.list().map_err(ToolFailure::store)?;
            let revision = store.current_revision().map_err(ToolFailure::store)?;
            let git_store = GitStore::open(&self.project).map_err(|e| ToolFailure {
                kind: "repository".to_owned(),
                message: e.message,
                data: e.data,
            })?;
            let status = Projection::rebuild_from_envelopes_store_with(
                &git_store,
                &records,
                &revision,
                provider,
            )
            .map_err(ToolFailure::index)?;
            Ok(ToolOutcome::read(json!(status)))
        } else {
            let store = self.store()?;
            let status =
                Projection::synchronize_store_with(&store, provider).map_err(ToolFailure::index)?;
            Ok(ToolOutcome::read(json!(status)))
        }
    }

    fn model_status(&self) -> Result<ToolOutcome, ToolCallFailure> {
        Ok(ToolOutcome::read(json!(self.build_model_status())))
    }

    fn build_model_status(&self) -> Value {
        match &self.embed_provider {
            Some(provider) => {
                let status = ModelStatusBuilder::default()
                    .model_id(provider.model_id())
                    .display_name(provider.name())
                    .dimensions(provider.dimensions())
                    .runtime_state(ModelRuntime::Active)
                    .build();
                json!({
                    "schemaVersion": 1,
                    "modelId": status.model_id,
                    "dimensions": status.dimensions,
                    "runtime": status.runtime,
                    "runtimeState": status.runtime_state,
                    "vectorSearch": status.vector_search,
                    "ftsOnly": status.fts_only(),
                    "mode": "hybrid",
                })
            }
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

    fn search(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let query = required_string(arguments, "query")?.to_owned();
        let limit = usize::try_from(arguments.get("limit").and_then(Value::as_u64).unwrap_or(20))
            .unwrap_or(20);
        let offset = usize::try_from(arguments.get("offset").and_then(Value::as_u64).unwrap_or(0))
            .unwrap_or(0);
        let revision: Revision = if let Some(rev) = arguments.get("revision") {
            serde_json::from_value(rev.clone())
                .map_err(|_| RpcFailure::invalid_argument("revision"))?
        } else {
            self.store()?
                .current()
                .map_err(ToolFailure::store)?
                .revision()
                .clone()
        };
        let filters = SearchFilters {
            kind: arguments
                .get("kind")
                .and_then(Value::as_str)
                .map(str::to_owned),
            tags: arguments
                .get("tags")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
            archived: arguments.get("archived").and_then(Value::as_bool),
            freshness: arguments
                .get("freshness")
                .and_then(Value::as_array)
                .map(|arr| {
                    arr.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_owned)
                        .collect()
                })
                .unwrap_or_default(),
        };
        let request = SearchRequest {
            query,
            limit,
            offset,
            filters,
            revision: revision.clone(),
        };
        let store = self.store()?;
        // The index is already synchronized after each mutation (see call_tool),
        // so we search directly. If the index is stale for the requested
        // revision, search_store returns a structured error.
        let result =
            Projection::search_store_with(&store, &request, self.provider()).map_err(ToolFailure::index)?;
        Ok(ToolOutcome::read(json!(result)))
    }

    fn backlinks(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let key = required_string(arguments, "key")?.to_owned();
        let revision: Revision = if let Some(rev) = arguments.get("revision") {
            serde_json::from_value(rev.clone())
                .map_err(|_| RpcFailure::invalid_argument("revision"))?
        } else {
            self.store()?
                .current()
                .map_err(ToolFailure::store)?
                .revision()
                .clone()
        };
        let store = self.store()?;
        let entries =
            Projection::backlinks_store(&store, &revision, &key).map_err(ToolFailure::index)?;
        Ok(ToolOutcome::read(json!({
            "key": key,
            "revision": revision,
            "backlinks": entries,
        })))
    }

    fn transport_status(&self) -> Result<ToolOutcome, ToolCallFailure> {
        let store = self.store()?;
        let git_dir = store.git_dir();
        let remote = git_memory_store::read_remote_config(git_dir)
            .map_err(ToolFailure::store)?;
        let has_remote = remote.is_some();
        Ok(ToolOutcome::read(json!({
            "remoteConfigured": has_remote,
            "remoteUrl": remote.as_ref().map(|r| r.url.clone()),
            "refspec": remote.as_ref().and_then(|r| r.refspec.clone()),
        })))
    }

    fn fetch(&self, _arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let store = self.store()?;
        let git_dir = store.git_dir().to_path_buf();
        let remote = git_memory_store::read_remote_config(&git_dir)
            .map_err(ToolFailure::store)?
            .ok_or_else(|| ToolFailure {
                kind: "no_remote_configured".to_owned(),
                message: "no memory remote configured".to_owned(),
                data: json!({"recovery_action": "configure_remote_first"}),
            })?;
        let result = git_memory_store::fetch_and_merge(&store, &remote, &[])
            .map_err(ToolFailure::store)?;
        let changed = result.local_revision_before != result.local_revision_after;
        Ok(ToolOutcome {
            content: json!({
                "localRevisionBefore": result.local_revision_before,
                "localRevisionAfter": result.local_revision_after,
                "remoteRevision": result.remote_revision,
                "fastForward": result.fast_forward,
                "merged": result.merged,
                "conflicts": result.conflicts,
            }),
            revision_changed: changed,
        })
    }

    fn push(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let force = arguments
            .get("force")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let store = self.store()?;

        // Apply push policy before network mutation.
        let policy_result =
            git_memory_store::check_push_policy(&store).map_err(ToolFailure::store)?;
        if !policy_result.allowed {
            return Err(ToolCallFailure::Tool(ToolFailure {
                kind: "push_blocked".to_owned(),
                message: "push blocked by memory_push_stale policy".to_owned(),
                data: json!({
                    "stale_count": policy_result.stale_count,
                    "warnings": policy_result.warnings,
                    "recovery_action": "refresh_stale_records_or_override_policy",
                }),
            }));
        }

        let git_dir = store.git_dir().to_path_buf();
        let remote = git_memory_store::read_remote_config(&git_dir)
            .map_err(ToolFailure::store)?
            .ok_or_else(|| ToolFailure {
                kind: "no_remote_configured".to_owned(),
                message: "no memory remote configured".to_owned(),
                data: json!({"recovery_action": "configure_remote_first"}),
            })?;
        git_memory_store::push_to_remote(&git_dir, &remote, force)
            .map_err(ToolFailure::store)?;
        Ok(ToolOutcome::read(json!({
            "pushed": true,
            "force": force,
            "remote": remote.url,
            "warnings": policy_result.warnings,
            "staleCount": policy_result.stale_count,
        })))
    }

    fn unlock_store(&mut self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let identity_path = required_string(arguments, "identity_path")?;
        let path = std::path::PathBuf::from(identity_path);
        let identity = load_ssh_identity(&path).map_err(|e| ToolFailure {
            kind: "identity_load_failed".to_owned(),
            message: format!("failed to load SSH identity: {e}"),
            data: json!({"path": identity_path}),
        })?;
        let store = self.encrypted_store.as_mut().ok_or_else(|| ToolFailure {
            kind: "not_encrypted".to_owned(),
            message: "project is not encrypted — nothing to unlock".to_owned(),
            data: json!({}),
        })?;
        store.unlock(identity).map_err(ToolFailure::store)?;
        let revision = store.current_revision().map_err(ToolFailure::store)?;
        // Rebuild the ephemeral index from decrypted records.
        self.sync_index().map_err(ToolFailure::index)?;
        Ok(ToolOutcome::read(json!({
            "unlocked": true,
            "revision": revision,
            "indexRebuilt": true,
        })))
    }

    fn lock_store(&mut self) -> Result<ToolOutcome, ToolCallFailure> {
        let store = self.encrypted_store.as_mut().ok_or_else(|| ToolFailure {
            kind: "not_encrypted".to_owned(),
            message: "project is not encrypted — nothing to lock".to_owned(),
            data: json!({}),
        })?;
        store.lock();
        // Destroy the ephemeral index so no plaintext persists on disk.
        let git_store = GitStore::open(&self.project).map_err(|e| ToolFailure {
            kind: "repository".to_owned(),
            message: e.message,
            data: e.data,
        })?;
        Projection::destroy_store(&git_store).map_err(ToolFailure::index)?;
        Ok(ToolOutcome::read(json!({
            "locked": true,
            "indexDestroyed": true,
        })))
    }

    fn init_encrypted(&mut self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let identity_path = required_string(arguments, "identity_path")?;
        let public_key = required_string(arguments, "recipient_public_key")?.to_owned();
        let key_type = arguments
            .get("key_type")
            .and_then(Value::as_str)
            .unwrap_or("ssh")
            .to_owned();
        let label = arguments
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let recipient = git_memory_store::RecipientEntry {
            public_key,
            key_type,
            label,
        };
        // Unlock first (handles both fresh project without manifest
        // and existing project — unlock verifies identity if manifest exists).
        let path = std::path::PathBuf::from(identity_path);
        let identity = load_ssh_identity(&path).map_err(|e| ToolFailure {
            kind: "identity_load_failed".to_owned(),
            message: format!("failed to load SSH identity: {e}"),
            data: json!({"path": identity_path}),
        })?;
        let store = self.encrypted_store.as_mut().ok_or_else(|| ToolFailure {
            kind: "not_encrypted".to_owned(),
            message: "project is not encrypted — nothing to init".to_owned(),
            data: json!({}),
        })?;
        store.unlock(identity).map_err(ToolFailure::store)?;
        let result = store
            .init(vec![recipient])
            .map_err(ToolFailure::store)?;
        Ok(ToolOutcome::read(json!({
            "initialized": true,
            "backupIdentity": result.backup_identity,
            "warning": "persist the backup identity in a safe location outside the repository — it is the recovery path if you lose your SSH key",
        })))
    }

    fn list_recipients(&self) -> Result<ToolOutcome, ToolCallFailure> {
        let store = self.require_unlocked_encrypted()?;
        let recipients = store.list_recipients().map_err(ToolFailure::store)?;
        Ok(ToolOutcome::read(json!({
            "recipients": recipients,
        })))
    }

    fn add_recipient(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let public_key = required_string(arguments, "public_key")?.to_owned();
        let key_type = arguments
            .get("key_type")
            .and_then(Value::as_str)
            .unwrap_or("ssh")
            .to_owned();
        let label = arguments
            .get("label")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let recipient = git_memory_store::RecipientEntry {
            public_key,
            key_type,
            label,
        };
        let store = self.require_unlocked_encrypted()?;
        store.add_recipient(recipient).map_err(ToolFailure::store)?;
        // Re-encryption changed the canonical revision — rebuild the index.
        self.sync_index().map_err(ToolFailure::index)?;
        Ok(ToolOutcome::read(json!({
            "added": true,
            "indexRebuilt": true,
        })))
    }

    fn remove_recipient(&self, arguments: &Value) -> Result<ToolOutcome, ToolCallFailure> {
        let public_key = required_string(arguments, "public_key")?;
        let store = self.require_unlocked_encrypted()?;
        store
            .remove_recipient(public_key)
            .map_err(ToolFailure::store)?;
        // Recipients changed — old vectors are invalid. Full rebuild.
        self.sync_index().map_err(ToolFailure::index)?;
        Ok(ToolOutcome::read(json!({
            "removed": true,
            "indexRebuilt": true,
        })))
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
        ("Records summary", "memory://records/summary"),
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

/// Built-in agent instructions — describes the storage layer tools, the
/// revision model, and the encryption lifecycle. Updated with each Git
/// Memory version. Composed with project-specific schema instructions (from
/// `__type__` records) when schema is implemented.
fn builtin_instructions(encryption_mode: &str) -> String {
    let mut s = String::new();
    s.push_str("# Git Memory — Persistent Knowledge Store\n\n");
    s.push_str("You are connected to a Git Memory store. Records persist in Git objects ");
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

    // ── Revision model ──
    s.push_str("## Revision Model\n\n");
    s.push_str("Memory uses a two-revision model:\n");
    s.push_str("- **Staged** (`refs/memory/staged`): pending changes, not yet permanent.\n");
    s.push_str("- **Canonical** (`refs/memory/main`): committed snapshots.\n\n");
    s.push_str("`apply_transaction` writes to staged. `checkpoint` promotes staged to ");
    s.push_str("canonical. Read operations (`get_record`, `list_records`, `search`) ");
    s.push_str("always read from canonical (current revision).\n\n");
    s.push_str("Every mutation returns the new staged revision. After mutations, ");
    s.push_str("the index is synchronised automatically — no manual reindex needed ");
    s.push_str("for normal operations.\n\n");

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
    s.push_str("- **memory_search** (`query`): Full-text search with the same filters. ");
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
    s.push_str("`kind`, `field`, and `reason`.\n");
    s.push_str("- **memory_checkpoint** (`message`): Promote staged to canonical. ");
    s.push_str("Creates a permanent Git commit with a message.\n");

    s.push_str("\n### History & Reconciliation\n\n");
    s.push_str("- **memory_history**: List checkpoints (canonical commits).\n");
    s.push_str("- **memory_diff**: Compare two revisions.\n");
    s.push_str("- **memory_reconcile**: Sync Memory with code history. ");
    s.push_str("Code commits since the last Memory checkpoint are processed; ");
    s.push_str("freshness of records is updated based on path overlap.\n");

    s.push_str("\n### Search Index\n\n");
    s.push_str("- **memory_reindex**: Rebuild the LanceDB index from canonical records. ");
    s.push_str("Use after corruption or manual Git operations.\n");
    s.push_str("- **memory_doctor**: Validate repository and index health.\n");

    s.push_str("\n### Transport\n\n");
    s.push_str("- **memory_transport_status**: Check remote configuration.\n");
    s.push_str("- **memory_fetch**: Pull memory refs from remote and merge.\n");
    s.push_str("- **memory_push**: Push memory refs to remote. ");
    s.push_str("Blocked if stale records exist (override with `force`).\n");

    s.push_str("\n### Export / Import\n\n");
    s.push_str("- **memory_export** (`revision`): Export a deterministic record bundle.\n");
    s.push_str("- **memory_import** (`bundle`): Import records from a bundle in one transaction.\n");

    // ── Encryption ──
    if encryption_mode == "encrypted" {
        s.push_str("\n## Encryption\n\n");
        s.push_str("This project uses encrypted storage. Records are encrypted with age ");
        s.push_str("(SSH keys) before writing to Git. The search index is ephemeral: ");
        s.push_str("it exists only in memory while unlocked.\n\n");
        s.push_str("- **memory_unlock** (`identity_path`): Decrypt the store with an SSH key. ");
        s.push_str("Rebuilds the search index from decrypted records. ");
        s.push_str("Required before any read/write/search operation.\n");
        s.push_str("- **memory_lock**: Lock the store and destroy the index. ");
        s.push_str("No plaintext persists on disk while locked.\n");
        s.push_str("- **memory_encryption_status**: Check current lock state.\n");
        s.push_str("- **memory_list_recipients** / **memory_add_recipient** / ");
        s.push_str("**memory_remove_recipient**: Manage who can decrypt. ");
        s.push_str("Adding/removing a recipient re-encrypts all records.\n");
        s.push_str("\nWhile locked, read/write/search operations return a `locked` error. ");
        s.push_str("Call `memory_unlock` first.\n");
    }

    // ── Resources ──
    s.push_str("\n## Resources\n\n");
    s.push_str("- `memory://project`: Project handshake (versions, gitDir, reconciliation).\n");
    s.push_str("- `memory://revision/current`: Current canonical revision.\n");
    s.push_str("- `memory://index/status`: LanceDB index state.\n");
    s.push_str("- `memory://model/status`: Embedding model status.\n");
    s.push_str("- `memory://policy/effective`: Effective transport policy.\n");
    s.push_str("- `memory://encryption/status`: Encryption mode and lock state.\n");
    s.push_str("- `memory://records/summary`: Record counts by kind, freshness, archived.\n");
    s.push_str("- `memory://records/{key}`: Full record by key.\n\n");

    // ── Guidelines ──
    s.push_str("## Working with Memory\n\n");
    s.push_str("1. **Read before write**: Always `memory_get_record` or `memory_list_records` ");
    s.push_str("to get the current `expected_revision` before `apply_transaction`.\n");
    s.push_str("2. **One transaction, one logical change**: Batch related puts/deletes ");
    s.push_str("in a single transaction for atomicity.\n");
    s.push_str("3. **Checkpoint after significant changes**: Use `memory_checkpoint` ");
    s.push_str("with a descriptive message after a logical group of transactions.\n");
    s.push_str("4. **Use metadata_only for lists**: When listing records for display, ");
    s.push_str("set `metadata_only: true` to avoid transferring full content.\n");
    s.push_str("5. **After resource update notifications**: Re-read ");
    s.push_str("`memory://revision/current` to stay in sync.\n");

    s
}

#[allow(clippy::too_many_lines)]
fn list_tools() -> Value {
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
            "List records with pagination, filters (kind/tags/archived/freshness), sorting and metadata-only mode. Response includes counts by kind/freshness/archived. Use metadata_only=true for UI lists.",
            object_schema(
                &[
                    ("revision", string_schema()),
                    ("limit", json!({"type":"integer","minimum":1,"maximum":200,"default":50})),
                    ("offset", json!({"type":"integer","minimum":0,"default":0})),
                    ("kind", string_schema()),
                    ("tags", json!({"type":"array","items":{"type":"string"}})),
                    ("archived", json!({"type":"boolean"})),
                    ("freshness", json!({"type":"array","items":{"type":"string","enum":["fresh","stale","unverified","invalid"]}})),
                    ("sort", json!({"type":"string","enum":["key","kind","title","freshness","archived"],"default":"key"})),
                    ("sort_order", json!({"type":"string","enum":["asc","desc"],"default":"asc"})),
                    ("metadata_only", json!({"type":"boolean","default":false})),
                ],
                &[],
            ),
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
            "memory_search",
            "Full-text search across all records with filters. Use when you need to find records by content, not by key. Returns ranked hits.",
            object_schema(
                &[
                    ("query", string_schema()),
                    (
                        "limit",
                        json!({"type":"integer","minimum":1,"maximum":200,"default":20}),
                    ),
                    ("offset", json!({"type":"integer","minimum":0,"default":0})),
                    ("revision", string_schema()),
                    ("kind", string_schema()),
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
            "Validate repository and canonical store access",
            object_schema(&[], &[]),
        ),
        tool(
            "memory_encryption_status",
            "Report the active encryption mode",
            object_schema(&[], &[]),
        ),
    ];
    tools.push(tool("memory_reindex", "Rebuild the LanceDB search index from canonical records. Use after corruption or manual Git operations.", object_schema(&[], &[])));
    tools.push(tool("memory_transport_status", "Check if a memory remote is configured and report sync status.", object_schema(&[], &[])));
    tools.push(tool("memory_fetch", "Pull memory refs from the configured remote and merge records into local store.", object_schema(&[], &[])));
    tools.push(tool(
        "memory_push",
        "Push memory refs to the configured remote",
        object_schema(&[("force", json!({"type": "boolean", "default": false}))], &[]),
    ));
    tools.push(tool("memory_model_status", "Report embedding model status", object_schema(&[], &[])));
    tools.push(tool(
        "memory_unlock",
        "Unlock the encrypted store with an SSH identity and rebuild the ephemeral index",
        object_schema(&[("identity_path", string_schema())], &["identity_path"]),
    ));
    tools.push(tool(
        "memory_lock",
        "Lock the encrypted store and destroy the ephemeral index (no plaintext persists on disk)",
        object_schema(&[], &[]),
    ));
    tools.push(tool(
        "memory_init_encrypted",
        "Initialize the encrypted store with the first recipient. Pass both your SSH private key path (to unlock) and your public key (as recipient). Creates a backup identity — persist it outside the repo.",
        object_schema(
            &[
                ("identity_path", string_schema()),
                ("recipient_public_key", string_schema()),
                ("key_type", json!({"type":"string","enum":["ssh","x25519"],"default":"ssh"})),
                ("label", string_schema()),
            ],
            &["identity_path", "recipient_public_key"],
        ),
    ));
    tools.push(tool(
        "memory_list_recipients",
        "List all recipients in the encrypted manifest",
        object_schema(&[], &[]),
    ));
    tools.push(tool(
        "memory_add_recipient",
        "Add a recipient and re-encrypt all records (requires unlock)",
        object_schema(
            &[
                ("public_key", string_schema()),
                ("key_type", json!({"type":"string","enum":["ssh","x25519"],"default":"ssh"})),
                ("label", string_schema()),
            ],
            &["public_key"],
        ),
    ));
    tools.push(tool(
        "memory_remove_recipient",
        "Remove a recipient, re-encrypt all records, and rebuild the index",
        object_schema(&[("public_key", string_schema())], &["public_key"]),
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

fn freshness_str(state: git_memory_core::FreshnessState) -> &'static str {
    use git_memory_core::FreshnessState;
    match state {
        FreshnessState::Fresh => "fresh",
        FreshnessState::Stale => "stale",
        FreshnessState::Unverified => "unverified",
        FreshnessState::Invalid => "invalid",
    }
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
    message: String,
    data: Value,
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
        StoreErrorKind::FastForwardRequired => "fast_forward_required",
        StoreErrorKind::Diverged => "diverged",
        StoreErrorKind::AuthenticationFailed => "authentication_failed",
        StoreErrorKind::NamespaceRejected => "namespace_rejected",
        StoreErrorKind::TransportFailed => "transport_failed",
        StoreErrorKind::SignatureInvalid => "signature_invalid",
        StoreErrorKind::MergeConflict => "merge_conflict",
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
