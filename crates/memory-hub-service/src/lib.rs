//! The Memory Hub use cases, expressed in Rust types.
//!
//! Everything a client can ask Memory to do lives here: reading and writing
//! records, checkpoints and history, search, reconciliation, transport, and the
//! encrypted-project lifecycle. What is deliberately absent is any notion of a
//! wire format — no JSON-RPC, no request ids, no tool names. A protocol adapter
//! (`memory-hub-mcp` today) parses its own wire shape into these arguments and
//! renders these results back out.
//!
//! Two things follow from that split. The use cases are testable without
//! spawning a process — the tests in this crate call them directly — and an
//! in-process host can drive Memory without duplicating the branching between
//! plaintext and encrypted projects, which lives here once.

mod attach;
mod error;
mod listing;
mod policy;
mod records;

pub use attach::{
    Attachment, DocumentSource, FolderSource, KnownRecord, RENAME_THRESHOLD, RenameCandidate,
    ScanChange, SourceCapabilities, SourceDocument, directory_rename, media_type_for, unique_key,
};
pub use error::ServiceError;
pub use listing::{
    FolderEntry, Listing, ListingCounts, ListingQuery, ListingSort, PresenceFilter, folder_in_scope,
};
pub use policy::{SchemaPolicy, load_registry};
pub use records::{DEFAULT_RECORDS_PATH, RecordsIn};

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

use memory_hub_core::{ContentHash, ContentRef, Envelope, FreshnessState, Presence, StoredRecord};
use memory_hub_embed::{EmbeddingProvider, ModelRuntime, ModelStatus, ModelStatusBuilder};
use memory_hub_folder::FolderStore;
use memory_hub_index::{
    BacklinkEntry, ContentResolver, Projection, ProjectionStatus, ResolvedContent, SearchRequest,
    SearchResult,
};
use memory_hub_reconcile::{DivergenceMode, ReconcileReport, Reconciler};
use memory_hub_schema::{SchemaRegistry, TYPE_KIND, TypeDefinition, TypeStorage};
use memory_hub_store::{
    ApplyResult, ExportBundle, ExportMode, FetchResult, GitStore,
    MemoryRemote, Operation, PushPolicyResult, RecordChange, RecordId,
    RecordStore, Revision, StoreDescription, StoreView, Transaction, TransactionPolicy,
};

/// Result alias for every use case.
pub type Result<T> = std::result::Result<T, ServiceError>;

/// A store to work through, opened for the call that asked for it.
///
/// A wrapper rather than a bare box so that callers keep saying `&*store` and
/// nothing else has to know how the store was come by.
pub struct StoreHandle<'a>(Box<dyn RecordStore + 'a>);

impl<'a> StoreHandle<'a> {
    fn new(store: Box<dyn RecordStore + 'a>) -> Self {
        Self(store)
    }
}

impl<'a> std::ops::Deref for StoreHandle<'a> {
    type Target = dyn RecordStore + 'a;

    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

/// One project's Memory, driven through typed use cases.
///
/// A service is bound to a project directory for its lifetime.
pub struct MemoryService {
    project: PathBuf,
    /// Where this project's records are, as the host named them when it
    /// opened. There is no default and nothing to read: an engine that guessed
    /// would be deciding for the product that embedded it.
    records: RecordsIn,
    /// Resolved on first use. `None` inside the cell means "no model on disk",
    /// which is a valid, cached answer — search then runs FTS-only.
    embed_provider: OnceLock<Option<Arc<dyn EmbeddingProvider>>>,
}

impl MemoryService {
    /// Bind to a project whose records are where `records` says.
    ///
    /// Nothing is read or created here. The storage is prepared by the first
    /// call that needs it — opening it is what creates it — so a host may bind
    /// to a project that has never held memory and find out from an ordinary
    /// read rather than from a separate ceremony.
    #[must_use]
    pub const fn open(project: PathBuf, records: RecordsIn) -> Self {
        Self {
            project,
            records,
            embed_provider: OnceLock::new(),
        }
    }

    /// Where this project's records are.
    #[must_use]
    pub const fn records(&self) -> &RecordsIn {
        &self.records
    }

    /// Use `provider` for the vector channel instead of whatever model this
    /// machine happens to have on disk.
    ///
    /// Whether a GGUF is present is a property of the machine, so anything
    /// that reaches the meaning channel answers one way on a laptop that has
    /// downloaded a model and another on a runner that never has. A caller
    /// that needs the channel to behave the same everywhere installs its own
    /// provider — a test, above all.
    ///
    /// Takes effect only before the provider is first resolved; after that the
    /// service keeps the one it has already answered with.
    #[must_use]
    pub fn with_provider(self, provider: Option<Arc<dyn EmbeddingProvider>>) -> Self {
        let _ = self.embed_provider.set(provider);
        self
    }

    #[must_use]
    pub fn project(&self) -> &Path {
        &self.project
    }

    /// Reads what a record's locator points at, for the index.
    ///
    /// The projection holds the searchable form of a record, and a record whose
    /// content is a repository file holds none of it. Without this, attaching a
    /// documentation folder would produce records findable by their type and by
    /// nothing else — which is not what anybody attaches a documentation folder
    /// for.
    #[must_use]
    pub fn content_resolver(&self) -> Arc<dyn ContentResolver> {
        Arc::new(WorkingTreeContent {
            project: self.project.clone(),
        })
    }

    /// The embedding model, resolved once per service.
    ///
    /// Resolution checks for a model on disk; the GGUF itself is loaded lazily
    /// by the first search that needs a vector.
    #[must_use]
    pub fn provider(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        self.embed_provider
            .get_or_init(memory_hub_embed::active::resolve_active_provider)
            .clone()
    }

    /// Remove a type from the project, and detach the folder it was over.
    ///
    /// An operation of its own rather than a delete per record, and the
    /// difference is what it does to the working tree. Deleting a record takes
    /// its document, because a record owns its body wherever the project put
    /// it. A type over an attached folder owns nothing: its records are what
    /// Memory knows about files that were in the repository before it was
    /// asked, and were written and reviewed by people who need not have heard
    /// of it. So the definition goes, the records that mirror the folder go,
    /// and every file stays exactly where it is.
    ///
    /// For a type whose documents *are* its records there is nothing to
    /// detach, and its records go with it because they are its content.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the project holds no such type, or when
    /// the records cannot be read or written.
    pub fn remove_type(&self, kind: &str, transaction_id: &str) -> Result<TypeRemoval> {
        let registry = self.schema_registry()?;
        if registry.get(kind).is_none() {
            return Err(ServiceError::invalid_argument(
                "kind",
                format!("this project holds no type `{kind}`"),
            ));
        }
        let storage = registry.storage_for(kind).map_err(|error| {
            ServiceError::invalid_argument("kind", format!("type `{kind}`: {}", error.message))
        })?;
        let detached = attachment_for(&storage).map(|attachment| attachment.folder);

        let (revision, envelopes) = self.corpus(None)?;
        let mut removed = 0;
        let mut operations = Vec::new();
        for (key, envelope) in &envelopes {
            // The definition is found by what it says it is, not by a key
            // derived here: the key a definition was written under belongs to
            // whoever wrote it, and a second opinion about its shape is how a
            // type comes to be deleted except for the record that declares it.
            let is_definition = envelope.kind == TYPE_KIND
                && TypeDefinition::from_content(&envelope.content)
                    .is_ok_and(|definition| definition.kind_name == kind);
            if envelope.kind == kind {
                removed += 1;
            } else if !is_definition {
                continue;
            }
            operations.push(Operation::delete(RecordId::plaintext(key)));
        }

        // Straight to the store, because these deletions are the one kind that
        // must not take documents with them — see the doc comment. Going
        // through `apply_transaction` here is how "delete the type" would come
        // to mean "delete the team's documentation folder".
        let result = self
            .record_store()?
            .apply(&Transaction {
                id: transaction_id.to_owned(),
                expected_revision: revision,
                operations,
            })
            .map_err(ServiceError::store)?;

        Ok(TypeRemoval {
            revision: result.revision,
            removed,
            detached,
        })
    }

    /// Open the storage this project keeps its records in.
    ///
    /// Returned as the contract, because which backend answers is the
    /// project's decision and not this layer's. Callers that need something
    /// only Git can do ask [`Self::git_store`] instead, and get a refusal
    /// naming the storage when the project keeps its records elsewhere.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the project has no declaration or the
    /// storage cannot be opened.
    pub fn record_store(&self) -> Result<StoreHandle<'_>> {
        let policy: Arc<dyn TransactionPolicy> = Arc::new(SchemaPolicy::default());
        match &self.records {
            RecordsIn::GitMetadata => Ok(StoreHandle::new(Box::new(
                GitStore::open(&self.project)
                    .map_err(ServiceError::store)?
                    .with_policy(policy),
            ))),
            RecordsIn::Directory(path) => Ok(StoreHandle::new(Box::new(
                FolderStore::open(self.project.join(path))
                    .map_err(ServiceError::store)?
                    .with_policy(policy),
            ))),
        }
    }

    /// Open this project's Git store, for the things only Git can do.
    ///
    /// Checkpoints, history, transport and signing are not part of the storage
    /// contract because not every storage has them. A project that keeps its
    /// records in a folder gets a refusal naming its storage, which is a
    /// better answer than a Git store quietly opened beside the records it is
    /// not holding.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] with kind `unsupported` when this project's
    /// records do not live in Git, or when the repository cannot be opened.
    pub fn git_store(&self) -> Result<GitStore> {
        let RecordsIn::GitMetadata = &self.records else {
            return Err(ServiceError::new(
                "unsupported",
                "this project keeps its records outside Git, so it has no Git store",
                serde_json::json!({"records": "directory"}),
            ));
        };
        GitStore::open(&self.project)
            .map(|store| store.with_policy(Arc::new(SchemaPolicy::default())))
            .map_err(ServiceError::store)
    }

    /// The source holding a type's documents.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the type keeps its content in its records
    /// — there is no source to reach.
    fn source_for_kind(&self, kind: &str) -> Result<FolderSource<'_>> {
        let storage = self.schema_registry()?.storage_for(kind).map_err(|error| {
            ServiceError::invalid_argument("kind", format!("type `{kind}`: {}", error.message))
        })?;
        let attachment = attachment_for(&storage).ok_or_else(|| {
            ServiceError::invalid_argument(
                "key",
                format!("type `{kind}` keeps its content in its records"),
            )
        })?;
        Ok(FolderSource::new(&self.project, attachment))
    }

    /// What the backend says about itself.
    ///
    /// The only source of backend-specific facts for the handshake and for
    /// `doctor`. Asking the store means a fact that belongs to one backend is
    /// absent for the others instead of being filled in with something
    /// plausible.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the store cannot be opened.
    pub fn describe_store(&self) -> Result<StoreDescription> {
        Ok(self.record_store()?.describe())
    }

    /// Bring the search index in line with the store.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the index cannot be rebuilt.
    pub fn sync_index(&self) -> Result<()> {
        let store = self.record_store()?;
        Projection::synchronize_store_with(&*store, self.provider(), Some(self.content_resolver()))
            .map_err(|error| ServiceError::index(&error))?;
        Ok(())
    }

    /// The corpus a read operates on: the records of a snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the store is locked or unreadable.
    pub fn corpus(
        &self,
        revision: Option<&Revision>,
    ) -> Result<(Revision, Vec<(String, Envelope)>)> {
        let store = self.record_store()?;
        let snapshot = match revision {
            Some(revision) => StoreView::open(&*store, revision),
            None => StoreView::current(&*store),
        }
        .map_err(ServiceError::store)?;
        let revision = snapshot.revision().clone();
        let envelopes = snapshot
            .records()
            .map_err(ServiceError::store)?
            .into_iter()
            .map(|(id, StoredRecord::Plaintext { envelope })| (id.display_value(), *envelope))
            .collect();
        Ok((revision, envelopes))
    }

    /// The revision every read serves by default: the current one.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the store cannot be read.
    pub fn current_revision(&self) -> Result<Revision> {
        self.record_store()?
            .current_revision()
            .map_err(ServiceError::store)
    }

    // ── Records ─────────────────────────────────────────────────────────────
    //
    // What a corpus operation covers when records live in more than one place:
    // the reference data. Listing, search, export and checkpoints answer from
    // what Memory itself holds and has indexed — envelope, links, freshness,
    // `content_hash` — for every record regardless of where its bytes are. No
    // corpus operation reaches outside, so an unreachable backend cannot make
    // one fail or quietly return less.
    //
    // Content is the exception, and it is fetched on demand. Reading the body
    // of a record whose bytes live elsewhere is the one operation routed to
    // another backend, and the one that can report that backend is missing.
    //
    // A checkpoint therefore pins the state of an external record without
    // owning its bytes: the hash says what the content was, and a later
    // divergence is detectable rather than silently absorbed.

    /// Apply a put/delete batch atomically.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] for an empty batch, a same-key conflict, a
    /// reused transaction id, or — on an encrypted project — operations that
    /// address records the way only a plaintext project can.
    pub fn apply_transaction(
        &self,
        transaction_id: &str,
        expected_revision: Revision,
        operations: Vec<Operation>,
    ) -> Result<ApplyResult> {
        if operations.is_empty() {
            return Err(ServiceError::invalid_argument(
                "operations",
                "a transaction needs at least one operation",
            ));
        }
        // A deletion takes the record's document with it, when the record has
        // one. Documents first, records second, for the same reason writing
        // does it in that order — see [`Self::write_reference_content`]: an
        // interruption between the two has to leave something a scan can see
        // and repair. A file removed without its record is a record reporting
        // its document as gone, which `doctor` raises and a person settles. The
        // other order leaves a document belonging to no record, which the next
        // scan hands back as a *new* record — a deletion that undoes itself,
        // with a new key and none of the links the old one had.
        self.remove_deleted_documents(&operations)?;

        // Every envelope lives in the storage that holds records — that is
        // what "holds records" means. Where a type's *content* lives is a
        // separate question, and not one a transaction has to answer.
        //
        // Encryption is likewise the store's business: an encrypted project's
        // store takes plaintext envelopes and encrypts them, and this layer
        // does not need to know which kind it is talking to.
        self.record_store()?
            .apply(&Transaction {
                id: transaction_id.to_owned(),
                expected_revision,
                operations,
            })
            .map_err(ServiceError::store)
    }

    /// Take the documents of the records a transaction deletes.
    ///
    /// Deleting is one operation whatever the storage: what differs is how
    /// much of the record there is to take. A record that keeps its body in
    /// itself is gone when its envelope is; a record whose body is a document
    /// owns that document, and leaving the file behind would be a deletion the
    /// next scan reverses. The caller says `delete` either way — where the
    /// content lives is the project's own declaration and never a parameter of
    /// the operation.
    ///
    /// Records this transaction is *writing* are untouched, including the ones
    /// it writes over: a `put` is not a deletion, and a document only moves
    /// when the scan says its file did.
    fn remove_deleted_documents(&self, operations: &[Operation]) -> Result<()> {
        let deleted: Vec<&RecordId> = operations
            .iter()
            .filter_map(|operation| match operation {
                Operation::Delete { id } => Some(id),
                Operation::Put { .. } => None,
            })
            .collect();
        if deleted.is_empty() {
            return Ok(());
        }

        let store = self.record_store()?;
        let snapshot = StoreView::current(&*store).map_err(ServiceError::store)?;
        // One registry read for the batch: a cascade deletes several records of
        // the same type, and each of them would otherwise re-read the schema.
        let registry = self.schema_registry()?;

        for id in deleted {
            let Some(StoredRecord::Plaintext { envelope }) =
                snapshot.get(id).map_err(ServiceError::store)?
            else {
                // Already not here. The transaction will say so itself.
                continue;
            };
            let Some(reference) = &envelope.content_ref else {
                continue;
            };
            // A type whose declaration this project no longer holds, or one
            // whose storage is the records: there is no source to ask, and a
            // path guessed from the locator would be this layer deciding what a
            // locator means. The record goes; the file is left where it is.
            let Ok(storage) = registry.storage_for(&envelope.kind) else {
                continue;
            };
            let Some(attachment) = attachment_for(&storage) else {
                continue;
            };
            FolderSource::new(&self.project, attachment).remove(&reference.path)?;
        }
        Ok(())
    }

    /// Read a record's body, following its locator when it has one.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the record cannot be read or does not
    /// exist. A locator that resolves to nothing is not one of those cases —
    /// see [`ContentResolution::Missing`].
    pub fn resolve_content(&self, key: &str) -> Result<ContentResolution> {
        let view = self.get_record(key, None)?;
        let envelope = match view.record {
            Some(StoredRecord::Plaintext { envelope }) => *envelope,
            None => return Err(ServiceError::invalid_argument("key", "no such record")),
        };
        let Some(reference) = &envelope.content_ref else {
            return Ok(ContentResolution::Inline {
                content: envelope.content,
            });
        };
        Ok(self.follow(reference, &envelope.content_hash))
    }

    /// Read what a locator points at, here and now.
    ///
    /// A locator that resolves to nothing answers `Missing` rather than
    /// failing. The file may be deleted, on another branch, or simply not
    /// pulled, and those are indistinguishable at this moment — so the record
    /// stays, its links stay live, and the caller is told the body is not here.
    fn follow(&self, reference: &ContentRef, last_known: &ContentHash) -> ContentResolution {
        match std::fs::read(self.project.join(&reference.path)) {
            Ok(bytes) => {
                // Read as bytes and decoded if it decodes. Reading as text
                // first would report a diagram as missing, which is both wrong
                // and the opposite of what the person who put it there sees.
                let hash = ContentHash::for_bytes(&bytes);
                let content = String::from_utf8(bytes)
                    .map_or_else(|error| Content::Bytes(error.into_bytes()), Content::Text);
                ContentResolution::Resolved {
                    path: reference.path.clone(),
                    changed: &hash != last_known,
                    content,
                    hash,
                }
            }
            Err(error) => ContentResolution::Missing {
                path: reference.path.clone(),
                reason: error.to_string(),
            },
        }
    }

    /// Write the content of a reference record: the content first, the record
    /// second.
    ///
    /// The two effects cannot be made atomic — one is a file, the other a
    /// record — so the order is chosen to be repairable. Content first means
    /// an interruption leaves a file that disagrees with the digest on the
    /// record, which the next scan sees and fixes. Record first would leave a
    /// digest for content that was never written, which nothing can recover
    /// from by looking.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the record is not a reference record, the
    /// file cannot be written, or the record cannot be updated.
    pub fn write_reference_content(
        &self,
        transaction_id: &str,
        key: &str,
        content: &[u8],
    ) -> Result<ApplyResult> {
        let view = self.get_record(key, None)?;
        let Some(StoredRecord::Plaintext { envelope }) = view.record else {
            return Err(ServiceError::invalid_argument(
                "key",
                "content can only be written through a plaintext reference record",
            ));
        };
        let mut envelope = *envelope;
        let Some(reference) = envelope.content_ref.clone() else {
            return Err(ServiceError::invalid_argument(
                "key",
                "this record keeps its content inline — write it as a record",
            ));
        };

        // Through the source, not around it. A locator means something to the
        // storage that owns it, and a folder is only the storage this happens
        // to be today.
        self.source_for_kind(&envelope.kind)?
            .write(&reference.path, content)?;

        // Of the bytes, not of a string. A document of an attached folder is
        // whatever is in the folder now that there is no mask on it, and a
        // digest taken over text would describe something a picture is not.
        envelope.content_hash = ContentHash::for_bytes(content);
        self.apply_transaction(
            transaction_id,
            view.revision,
            vec![Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(envelope),
            })],
        )
    }

    /// Reconcile every attached folder with what Memory recorded.
    ///
    /// Runs at project open and whenever a caller asks. A client that can see
    /// window focus or watch the filesystem should ask again on those — before
    /// every read is too expensive, and only at open is too rare for somebody
    /// editing files in the next window.
    ///
    /// Everything unambiguous is applied in one transaction. Everything
    /// ambiguous is returned unapplied: a file matching no record may be new
    /// or may be a rename with an edit, nothing about it says which, and
    /// choosing silently would either lose a record's history or invent a
    /// relationship that does not exist.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the registry or the records cannot be
    /// read, or when the resulting write fails.
    pub fn scan_attachments(&self, transaction_id: &str) -> Result<ScanReport> {
        let registry = self.schema_registry()?;
        // Read through whatever the project actually is. A scan that reads the
        // Git tree directly sees ciphertext on an encrypted project, concludes
        // that no record exists, and reports every document as new — every
        // time.
        let (revision, records) = self.envelopes()?;

        // Derived keys share one namespace with the keys people write, so the
        // set to avoid is every key in the corpus, not the reference records of
        // one type — and it grows as the scan goes, because two documents in
        // the same folder can derive the same key.
        let assigned: std::cell::RefCell<std::collections::BTreeSet<String>> =
            std::cell::RefCell::new(records.iter().map(|record| record.key.clone()).collect());

        let mut changes = Vec::new();
        let mut operations = Vec::new();
        let mut scanned = 0;

        for (kind, definition) in registry.iter() {
            let Ok(storage) = definition.storage() else {
                continue;
            };
            let Some(attachment) = attachment_for(&storage) else {
                continue;
            };
            let source = FolderSource::new(&self.project, attachment.clone());
            let documents = source.list()?;
            scanned += documents.len();
            let known = known_records(&records, kind);
            for change in attach::classify(
                &documents,
                &known,
                &|locator| source.tracked(locator),
                &|locator| {
                    let base = attach::key_for(&attachment, locator);
                    let key = {
                        let taken = assigned.borrow();
                        attach::unique_key(&base, &|candidate| taken.contains(candidate))
                    };
                    assigned.borrow_mut().insert(key.clone());
                    key
                },
            ) {
                if let Some(operation) = Self::operation_for(&change, kind, &records)? {
                    operations.push(operation);
                }
                changes.push(change);
            }
        }

        // A renamed directory moves its files, and the scan has just followed
        // every one of them. What it has not followed is a record filed in that
        // folder with no file of its own — a decision filed next to the
        // documents, the record that is the folder — because nothing about it
        // moved. Its folder is metadata, and metadata does not travel unless
        // somebody carries it.
        let carried = Self::carried_by_a_directory_rename(&changes, &records);

        let code_revision = self.code_revision();
        let previous_code_revision = self.read_scan_cursor();
        self.write_scan_cursor(code_revision.as_deref());

        if operations.is_empty() && carried.is_empty() {
            return Ok(ScanReport {
                revision,
                scanned,
                changes,
                applied: 0,
                code_revision,
                previous_code_revision,
            });
        }
        if operations.is_empty() {
            let applied = carried.len();
            let result = self.apply_transaction(
                &format!("{transaction_id}-carried-{}", digest_of(&carried)),
                revision,
                carried,
            )?;
            return Ok(ScanReport {
                revision: result.revision,
                scanned,
                changes,
                applied,
                code_revision,
                previous_code_revision,
            });
        }
        let applied = operations.len();
        // The caller names the occasion; what the scan concluded names the
        // request. A transaction id is remembered for the life of the store, so
        // an id that depends only on the occasion — "the project was opened" —
        // is refused as reused the second time a scan has something different
        // to write, and the folder is never reconciled again. Repeating the
        // same conclusions still lands on the same id, which is what makes an
        // interrupted scan replayable.
        let applied = applied + carried.len();
        let result = self.apply_transaction(
            &format!("{transaction_id}-{}", digest_of(&operations)),
            revision,
            operations,
        )?;
        let revision = if carried.is_empty() {
            result.revision
        } else {
            self.apply_transaction(
                &format!("{transaction_id}-carried-{}", digest_of(&carried)),
                result.revision,
                carried,
            )?
            .revision
        };
        Ok(ScanReport {
            revision,
            scanned,
            changes,
            applied,
            code_revision,
            previous_code_revision,
        })
    }

    /// The corpus as envelopes, whatever mode the project is in.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the store is locked or unreadable.
    fn envelopes(&self) -> Result<(Revision, Vec<Envelope>)> {
        let (revision, records) = self.corpus(None)?;
        Ok((
            revision,
            records.into_iter().map(|(_, envelope)| envelope).collect(),
        ))
    }

    /// Whether the last scan still describes the working tree's branch.
    ///
    /// One ref read. A branch switch is the only thing that changes what a
    /// scan can see without anybody editing a document, so this is the cheap
    /// signal a caller checks when it wants to know whether to scan again —
    /// on window focus, say — instead of walking the folders to find out.
    ///
    /// Opening a project scans regardless: a document can have been edited
    /// while nothing was watching, and `HEAD` would say nothing about that.
    #[must_use]
    pub fn scan_is_stale(&self) -> bool {
        self.code_revision() != self.read_scan_cursor()
    }

    /// The commit the working tree currently has checked out.
    ///
    /// One ref read. What a scan saw depends on the branch, so this is what
    /// says whether an earlier scan still describes the tree — a branch switch
    /// moves it, and nothing else has to be watched to notice.
    fn code_revision(&self) -> Option<String> {
        let git_dir = GitStore::discover_git_dir(&self.project).ok()?;
        let repository = git2::Repository::open(git_dir).ok()?;
        let head = repository.head().ok()?;
        head.target().map(|oid| oid.to_string())
    }

    fn scan_cursor_path(&self) -> Option<PathBuf> {
        GitStore::discover_git_dir(&self.project)
            .ok()
            .map(|git_dir| git_dir.join("memory-hub/scan-cursor.json"))
    }

    fn read_scan_cursor(&self) -> Option<String> {
        let path = self.scan_cursor_path()?;
        let contents = std::fs::read_to_string(path).ok()?;
        let value: serde_json::Value = serde_json::from_str(&contents).ok()?;
        value
            .get("code_revision")?
            .as_str()
            .map(std::borrow::ToOwned::to_owned)
    }

    fn write_scan_cursor(&self, code_revision: Option<&str>) {
        let Some(path) = self.scan_cursor_path() else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::write(
            path,
            serde_json::json!({"schema_version": 1, "code_revision": code_revision}).to_string(),
        );
    }

    /// Turn one scan conclusion into the write it implies, if any.
    fn operation_for(
        change: &ScanChange,
        kind: &str,
        records: &[Envelope],
    ) -> Result<Option<Operation>> {
        let edit = |key: &str, apply: &dyn Fn(&mut Envelope)| -> Result<Option<Operation>> {
            let mut envelope = find_envelope(records, key).ok_or_else(|| {
                ServiceError::invalid_argument("key", "the scan named a record that is not there")
            })?;
            apply(&mut envelope);
            Ok(Some(Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(envelope),
            })))
        };

        match change {
            ScanChange::Edited { key, locator, hash } => edit(key, &|envelope| {
                envelope.content_hash = hash.clone();
                if let Some(reference) = &mut envelope.content_ref {
                    reference.presence = Presence::Present;
                }
                // Filled here as well as on arrival, so a corpus written
                // before the field existed acquires it as its documents are
                // touched, rather than through one transaction that rewrites
                // every record to add a field derived from a name.
                envelope.media_type = media_type_for(locator).map(str::to_owned);
                // The claim was checked against one text. The text changed, so
                // the check no longer says anything; keeping it would leave a
                // record in the corpus that lies.
                envelope.freshness.state = FreshnessState::Unverified;
            }),
            ScanChange::Moved { key, to, .. } => edit(key, &|envelope| {
                if let Some(reference) = &mut envelope.content_ref {
                    reference.path.clone_from(to);
                    reference.presence = Presence::Present;
                }
                // The folder is the locator's own directory, so a move carries
                // both or neither — one fact, one place.
                envelope.folder = memory_hub_core::folder_of(to);
                // And so is the media type: it is read off the file name, and
                // a move is how a file name changes. Left alone, a document
                // renamed from `.md` to `.png` would keep announcing itself as
                // text.
                envelope.media_type = media_type_for(to).map(str::to_owned);
            }),
            ScanChange::Missing { key, presence, .. } => edit(key, &|envelope| {
                if let Some(reference) = &mut envelope.content_ref {
                    reference.presence = *presence;
                }
            }),
            ScanChange::Returned { key, .. } => edit(key, &|envelope| {
                if let Some(reference) = &mut envelope.content_ref {
                    reference.presence = Presence::Present;
                }
            }),
            ScanChange::New { key, locator, hash } => {
                let mut envelope = Envelope::reference(key, kind, locator, hash.clone())
                    .map_err(|error| ServiceError::invalid_argument("locator", error.message))?;
                // Recorded when the document is first seen, so a client knows
                // what it is looking at before deciding to fetch it.
                envelope.media_type = media_type_for(locator).map(str::to_owned);
                Ok(Some(Operation::put(StoredRecord::Plaintext {
                    envelope: Box::new(envelope),
                })))
            }
            // Ambiguous by construction. Carried out to a person instead.
            ScanChange::Unmatched { .. } => Ok(None),
        }
    }

    /// What moving a type's records to another storage would do.
    ///
    /// Read-only, and the only thing that answers before anything is written:
    /// how many records move, in which direction, and what the caller is being
    /// asked to accept.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the registry or the records cannot be
    /// read, or when the target storage is not one this build can honour.
    pub fn plan_migration(&self, kind: &str, target: Option<&str>) -> Result<MigrationPlan> {
        let registry = self.schema_registry()?;
        let definition = registry.get(kind).ok_or_else(|| {
            ServiceError::invalid_argument("kind", format!("no type definition for `{kind}`"))
        })?;
        let from = definition.storage().map_err(|error| {
            ServiceError::invalid_argument("kind", format!("type `{kind}`: {}", error.message))
        })?;
        let mut moved = definition.clone();
        moved.storage = target.map(str::to_owned);
        let to = moved.storage().map_err(|error| {
            ServiceError::new(
                "invalid_argument",
                format!(
                    "target storage is not one this build can honour: {}",
                    error.message
                ),
                serde_json::json!({"field": error.field, "kind": kind}),
            )
        })?;

        let (_, records) = self.envelopes()?;
        let subjects: Vec<String> = records
            .iter()
            .filter(|envelope| envelope.kind == kind)
            .map(|envelope| envelope.key.clone())
            .collect();

        // Visible to the whole team: a directory of the repository, which is
        // what a type naming a folder keeps its documents in. The other side
        // of the choice is the records, which nobody sees in a diff.
        let visible = TypeStorage::is_external;

        let mut warnings = Vec::new();
        // Two folders are two places, even though both are folders. Saying so
        // is what keeps this from looking like a setting nobody has to think
        // about.
        if visible(&from) && visible(&to) && from != to {
            warnings.push(MigrationWarning {
                code: "files_are_left_in_place",
                message: "the content is written into the new folder and the files in the old \
                          one stay where they are. Memory does not delete what it did not \
                          create — removing them is an ordinary commit, and yours to make."
                    .to_owned(),
            });
        }
        if !visible(&from) && visible(&to) {
            warnings.push(MigrationWarning {
                code: "content_becomes_visible",
                message: "the content of every record of this type will be written into the \
                          working tree, where the whole team sees it in diffs and reviews. \
                          This is a change of visibility, not a technical detail."
                    .to_owned(),
            });
        }
        if visible(&from) && !visible(&to) {
            warnings.push(MigrationWarning {
                code: "does_not_hide_published_history",
                message: "moving content out of the working tree does not hide what was \
                          already published: what was committed stays in Git history for \
                          good. Treat this as changing where new writes go, never as \
                          retroactive privacy."
                    .to_owned(),
            });
            warnings.push(MigrationWarning {
                code: "files_are_left_in_place",
                message: "the files stay in the working tree. Memory stops treating them as \
                          content and does not delete anything it did not create — removing \
                          them is an ordinary commit, and yours to make."
                    .to_owned(),
            });
        }

        Ok(MigrationPlan {
            kind: kind.to_owned(),
            from: from.folder().map(str::to_owned),
            to: to.folder().map(str::to_owned),
            unchanged: from == to,
            keys: subjects,
            warnings,
        })
    }

    /// Move a type's records to another storage, and then the type itself.
    ///
    /// Every warning in the plan must be acknowledged by code. A boolean
    /// nobody reads is not consent, and the two directions are not asking the
    /// same thing.
    ///
    /// Interruption is survivable and the operation is repeatable. Content is
    /// written before the records that point at it, so a run cut short leaves
    /// files that agree with what a repeat would write, and the single
    /// transaction at the end either happened or did not. Running it again
    /// finds the work already done and moves what is left.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when a warning is unacknowledged, when the
    /// content cannot be written, or when the write fails.
    pub fn migrate_storage(
        &self,
        transaction_id: &str,
        kind: &str,
        target: Option<&str>,
        acknowledged: &[String],
    ) -> Result<MigrationPlan> {
        let plan = self.plan_migration(kind, target)?;
        let missing: Vec<&str> = plan
            .warnings
            .iter()
            .map(|warning| warning.code)
            .filter(|code| !acknowledged.iter().any(|given| given == code))
            .collect();
        if !missing.is_empty() {
            return Err(ServiceError::new(
                "confirmation_required",
                "this migration changes something the caller has to accept explicitly",
                serde_json::json!({
                    "kind": kind,
                    "unacknowledged": missing,
                    "warnings": plan
                        .warnings
                        .iter()
                        .map(|warning| serde_json::json!({
                            "code": warning.code,
                            "message": warning.message,
                        }))
                        .collect::<Vec<_>>(),
                }),
            ));
        }
        if plan.unchanged {
            return Ok(plan);
        }

        let registry = self.schema_registry()?;
        let definition = registry.get(kind).ok_or_else(|| {
            ServiceError::invalid_argument("kind", format!("no type definition for `{kind}`"))
        })?;
        let mut moved = definition.clone();
        moved.storage = target.map(str::to_owned);
        let storage = moved
            .storage()
            .map_err(|error| ServiceError::invalid_argument("storage", error.message.clone()))?;

        let (revision, records) = self.envelopes()?;
        let attachment = attachment_for(&storage);
        let mut operations = Vec::new();
        for envelope in &records {
            if envelope.kind != kind {
                continue;
            }
            operations.push(Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(self.move_one(envelope, attachment.as_ref())?),
            }));
        }

        // Two writes, because a migration genuinely spans two places and a
        // single batch may not: the records move first, then the definition
        // that describes them. Interrupted in between, the records have moved
        // and the type still names where they came from — a state to resume,
        // which is what repeating this operation does.
        if !operations.is_empty() {
            self.apply_transaction(&format!("{transaction_id}-records"), revision, operations)?;
        }

        let mut type_record = Self::type_record(&records, kind)?;
        type_record.content = serde_json::to_string(&moved).map_err(|error| {
            ServiceError::invalid_argument("storage", format!("type is not encodable: {error}"))
        })?;
        type_record.refresh_content_hash();
        let revision = self.current_revision()?;
        self.apply_transaction(
            &format!("{transaction_id}-type"),
            revision,
            vec![Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(type_record),
            })],
        )?;
        Ok(plan)
    }

    /// Move one record's content, and return the record that points at where
    /// it now is.
    fn move_one(&self, envelope: &Envelope, attachment: Option<&Attachment>) -> Result<Envelope> {
        let mut moved = envelope.clone();
        // Whatever the direction, the body being moved is the one that is
        // there now — read from the file for a record that points at one, held
        // in the record otherwise. A folder-to-folder move reads the old file
        // for exactly this reason: the record's own `content` is empty, and
        // publishing that would replace every document with nothing.
        let body: Vec<u8> = match &envelope.content_ref {
            Some(reference) => {
                std::fs::read(self.project.join(&reference.path)).unwrap_or_default()
            }
            None => envelope.content.clone().into_bytes(),
        };

        // Back into refs: the record carries the text itself from here on, so
        // content that is not text has nowhere to go and the caller is told
        // which record it was rather than being handed a mangled body.
        let Some(attachment) = attachment else {
            let content = String::from_utf8(body).map_err(|_| {
                ServiceError::invalid_argument(
                    "kind",
                    format!(
                        "record `{}` points at content that is not text, and a record in refs \
                         holds its content itself",
                        envelope.key
                    ),
                )
            })?;
            moved.content_hash = ContentHash::for_content(&content);
            moved.content = content;
            moved.content_ref = None;
            moved.folder = None;
            // The media type described a file, and there is no longer a file.
            moved.media_type = None;
            return Ok(moved);
        };

        // Into the working tree. The locator is checked before anything is
        // written: it is the record's key under the folder, keys are arbitrary
        // strings, and the envelope validator that would catch a bad one runs
        // after the file is already on disk.
        let locator = format!("{}/{}{NEW_DOCUMENT_SUFFIX}", attachment.folder, envelope.key);
        memory_hub_core::validate_locator("content_ref.path", &locator).map_err(|error| {
            ServiceError::invalid_argument(
                "key",
                format!(
                    "record `{}` cannot be published into `{}`: {}",
                    envelope.key, attachment.folder, error.message
                ),
            )
        })?;
        if !attachment.covers(&locator) {
            return Err(ServiceError::invalid_argument(
                "key",
                format!(
                    "record `{}` would be published outside the folder its type names",
                    envelope.key
                ),
            ));
        }

        // Content first, so an interruption leaves content without a record
        // rather than a record pointing at content that was never written.
        FolderSource::new(&self.project, attachment.clone()).write(&locator, &body)?;
        moved.content_hash = ContentHash::for_bytes(&body);
        moved.content = String::new();
        moved.folder = memory_hub_core::folder_of(&locator);
        // Set here on the same terms as a scan sets it: there is a file now,
        // and its name says what it is.
        moved.media_type = media_type_for(&locator).map(str::to_owned);
        moved.content_ref = Some(ContentRef::new(locator));
        Ok(moved)
    }

    fn type_record(records: &[Envelope], kind: &str) -> Result<Envelope> {
        let key = memory_hub_schema::type_key(kind);
        find_envelope(records, &key).ok_or_else(|| {
            ServiceError::invalid_argument("kind", format!("no `__type__` record for `{kind}`"))
        })
    }

    /// Read one record by key.
    ///
    /// `revision` is honoured on plaintext projects; an encrypted project only
    /// serves its current revision, since the key-to-storage manifest is read
    /// from the current snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the store is locked or unreadable.
    pub fn get_record(&self, key: &str, revision: Option<&Revision>) -> Result<RecordView> {
        let store = self.record_store()?;
        let snapshot = match revision {
            Some(revision) => StoreView::open(&*store, revision),
            None => StoreView::current(&*store),
        }
        .map_err(ServiceError::store)?;
        let record = snapshot
            .get(&RecordId::plaintext(key))
            .map_err(ServiceError::store)?;
        Ok(RecordView {
            revision: snapshot.revision().clone(),
            record,
        })
    }

    /// Filter, sort and page the corpus.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the store is locked or unreadable.
    pub fn list_records(
        &self,
        query: &ListingQuery,
        revision: Option<&Revision>,
    ) -> Result<Listing> {
        let (revision, envelopes) = self.corpus(revision)?;
        Ok(query.apply(revision, &envelopes))
    }

    /// Every folder the project has, from both sources at once.
    ///
    /// Two sources, because neither knows the whole answer. Aggregating the
    /// `folder` of known records is the complete answer for `refs`, where a
    /// folder is metadata and cannot exist unnamed. It is an assumption for an
    /// attached directory, which exists on disk whether or not Memory has
    /// anything in it.
    ///
    /// **The directories are read live and never stored.** Git does not keep
    /// empty directories, so an empty `docs/api/` is a fact about one working
    /// tree and is simply not there in a fresh clone. A remembered list would
    /// raise, on one machine, a folder that does not exist on another — the
    /// same copy of somebody else's truth that reference records exist to
    /// avoid.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the store is locked or unreadable.
    pub fn list_folders(&self, folder: Option<&str>, subtree: bool) -> Result<Vec<FolderEntry>> {
        let (_, envelopes) = self.corpus(None)?;
        let mut entries: BTreeMap<String, FolderEntry> = BTreeMap::new();

        for (_, envelope) in &envelopes {
            // A type definition is schema rather than a document, and counting
            // it as one would put a number in front of a person that no folder
            // of theirs contains.
            if envelope.kind == TYPE_KIND {
                continue;
            }
            let path = envelope.folder.as_deref().unwrap_or("");
            let entry = entries
                .entry(path.to_owned())
                .or_insert_with(|| FolderEntry::empty(path));
            entry.in_records = true;
            // Counted the way the default listing counts: a document another
            // branch has is not shown when the folder is opened, and a folder
            // that promises documents nobody can then see is worse than one
            // that says nothing. The folder itself stays — the record is filed
            // there whether or not this branch has its bytes.
            if !envelope
                .content_ref
                .as_ref()
                .is_some_and(|reference| reference.presence == Presence::NotOnBranch)
            {
                entry.records += 1;
            }
            if envelope.is_folder {
                // Two records standing for one folder are refused at the write,
                // so this is the one that is there.
                entry.described = Some(envelope.key.clone());
            }
        }

        for attachment in self.attachments()? {
            let source = FolderSource::new(&self.project, attachment.clone());
            for path in source.folders()? {
                entries
                    .entry(path.clone())
                    .or_insert_with(|| FolderEntry::empty(&path))
                    .in_storage = true;
            }
        }

        let mut folders: Vec<FolderEntry> = entries.into_values().collect();
        if let Some(wanted) = folder {
            folders.retain(|entry| folder_in_scope(wanted, subtree, &entry.path));
        }
        Ok(folders)
    }

    /// Rename a folder of `refs`, moving every record filed under it at once.
    ///
    /// One transaction, because the alternative is N of them: a client that
    /// rewrites each record's folder in turn leaves the folder half-renamed
    /// the moment one of those writes fails, and the record that stands for
    /// the folder is one of the records that can be left behind.
    ///
    /// Refused for a folder of an attached directory. Renaming there means
    /// renaming a directory on disk — which a person does the ordinary way,
    /// with Git watching — and the scan reads it back as what it is. Doing it
    /// from here would either write into somebody else's folder or leave the
    /// records disagreeing with the files.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when either path is not a normalized
    /// repository-relative folder, when the folder belongs to an attachment,
    /// or when the store refuses the write.
    pub fn rename_folder(&self, from: &str, to: &str, transaction_id: &str) -> Result<ApplyResult> {
        for (field, value) in [("from", from), ("to", to)] {
            if value.is_empty() {
                return Err(ServiceError::invalid_argument(
                    field,
                    "the root is not a folder that can be renamed",
                ));
            }
            memory_hub_core::validate_locator(field, value)
                .map_err(|error| ServiceError::invalid_argument(field, error.message.clone()))?;
        }
        if from == to {
            return Err(ServiceError::invalid_argument(
                "to",
                "a rename to the same path is not a rename",
            ));
        }
        for attachment in self.attachments()? {
            // Either way round. Inside an attachment it is a directory on
            // disk. Above one it *contains* a directory on disk, and renaming
            // it in metadata alone would leave the records claiming a path the
            // attachment root contradicts.
            let inside =
                from == attachment.folder || from.starts_with(&format!("{}/", attachment.folder));
            let above = attachment.folder.starts_with(&format!("{from}/"));
            if inside || above {
                return Err(ServiceError::invalid_argument(
                    "from",
                    "a directory of the repository is at or under this folder: rename the \
                     directory itself and the next scan will follow it",
                ));
            }
        }

        let (revision, envelopes) = self.corpus(None)?;
        let operations: Vec<Operation> = envelopes
            .into_iter()
            .filter_map(|(_, mut envelope)| {
                let moved = renamed_folder(envelope.folder.as_deref()?, from, to)?;
                envelope.folder = Some(moved);
                Some(Operation::put(StoredRecord::Plaintext {
                    envelope: Box::new(envelope),
                }))
            })
            .collect();
        if operations.is_empty() {
            return Err(ServiceError::invalid_argument(
                "from",
                "no record is filed in that folder",
            ));
        }
        self.apply_transaction(transaction_id, revision, operations)
    }

    /// Records a directory rename must carry that no file carries for them.
    ///
    /// A renamed directory moves its files, and the scan follows every one of
    /// them. A record filed in that folder with no file of its own — a decision
    /// filed next to the documents, the record that *is* the folder — moves
    /// only if somebody moves it, because its folder is metadata.
    ///
    /// Returned rather than applied, and applied in a transaction of its own:
    /// these records live in `refs` while the documents that moved belong to a
    /// type stored in a folder, and one batch may not cross the two. If that
    /// second write is the one that fails, the records are left filed where the
    /// directory used to be — a state `doctor` reports rather than one that
    /// hides.
    fn carried_by_a_directory_rename(
        changes: &[ScanChange],
        records: &[Envelope],
    ) -> Vec<Operation> {
        let Some((from, to)) = attach::directory_rename(changes) else {
            return Vec::new();
        };
        // A renamed directory is empty afterwards. If a document is still
        // sitting in it — edited, or simply untouched — then whatever those
        // moves were, they were not that directory being renamed, and the
        // prefix derived from them is a coincidence. Without this a single file
        // moved one level deeper reads as its whole parent being renamed, and
        // every record filed anywhere under it would be rewritten.
        let settled: std::collections::BTreeSet<&str> = changes
            .iter()
            .filter_map(|change| match change {
                ScanChange::Moved { key, .. } | ScanChange::Missing { key, .. } => {
                    Some(key.as_str())
                }
                _ => None,
            })
            .collect();
        let prefix = format!("{from}/");
        let left_behind = records.iter().any(|envelope| {
            envelope
                .content_ref
                .as_ref()
                .is_some_and(|reference| reference.path.starts_with(&prefix))
                && !settled.contains(envelope.key.as_str())
        });
        if left_behind {
            return Vec::new();
        }
        records
            .iter()
            .filter(|envelope| envelope.content_ref.is_none())
            .filter_map(|envelope| {
                let moved = renamed_folder(envelope.folder.as_deref()?, &from, &to)?;
                let mut envelope = envelope.clone();
                envelope.folder = Some(moved);
                Some(Operation::put(StoredRecord::Plaintext {
                    envelope: Box::new(envelope),
                }))
            })
            .collect()
    }

    /// Every attachment the schema declares, in kind order.
    fn attachments(&self) -> Result<Vec<Attachment>> {
        let mut attachments = Vec::new();
        for (_, definition) in self.schema_registry()?.iter() {
            let Ok(storage) = definition.storage() else {
                continue;
            };
            if let Some(attachment) = attachment_for(&storage) {
                attachments.push(attachment);
            }
        }
        Ok(attachments)
    }

    /// Compare two revisions.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when either revision cannot be resolved.
    pub fn diff(&self, from: &Revision, to: &Revision) -> Result<Vec<RecordChange>> {
        self.git_store()?
            .diff(from, to)
            .map_err(ServiceError::store)
    }

    /// Export a deterministic record bundle.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the revision is not exportable or the
    /// bundle cannot be read.
    pub fn export(&self, revision: &Revision, mode: ExportMode) -> Result<ExportView> {
        let store = self.record_store()?;
        let mut records = store.read_records(revision).map_err(ServiceError::store)?;
        if mode == ExportMode::Snapshot {
            for (_, record) in &mut records {
                self.inline_resolved_content(record);
            }
        }
        Ok(ExportView {
            revision: revision.clone(),
            bundle: ExportBundle {
                schema_version: memory_hub_store::EXPORT_SCHEMA_VERSION,
                mode,
                records,
            },
        })
    }

    /// Replace a reference record with an inline copy of what its locator
    /// resolves to, for a snapshot.
    ///
    /// A locator that resolves to nothing here is left as it is. A snapshot
    /// carries what it could read; it does not fail the whole export over one
    /// document somebody moved, and it does not invent an empty body for it.
    fn inline_resolved_content(&self, record: &mut StoredRecord) {
        let StoredRecord::Plaintext { envelope } = record;
        let Some(reference) = envelope.content_ref.clone() else {
            return;
        };
        let Ok(content) = std::fs::read_to_string(self.project.join(&reference.path)) else {
            return;
        };
        envelope.content_hash = ContentHash::for_content(&content);
        envelope.content = content;
        envelope.content_ref = None;
    }

    /// Import a bundle in one transaction.
    ///
    /// On an encrypted project every envelope is encrypted before it reaches
    /// the tree, and the deletions are derived from the manifest rather than
    /// from the tree — importing through the plaintext path would delete the
    /// manifest itself and orphan every record.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] for an unsupported bundle version, a record
    /// whose id does not match its payload, or a store failure.
    pub fn import(
        &self,
        transaction_id: &str,
        expected_revision: Revision,
        bundle: &ExportBundle,
    ) -> Result<ApplyResult> {
        // Version 1 is accepted as what it is: a bundle from before content
        // could live outside a record, which is a manifest by construction.
        if bundle.schema_version == 0
            || bundle.schema_version > memory_hub_store::EXPORT_SCHEMA_VERSION
        {
            return Err(ServiceError::new(
                "invalid_argument",
                "unsupported export schema version",
                serde_json::json!({
                    "received": bundle.schema_version,
                    "supported": memory_hub_store::EXPORT_SCHEMA_VERSION,
                }),
            ));
        }
        let bytes = serde_json::to_vec(bundle).map_err(|error| {
            ServiceError::invalid_argument("bundle", format!("bundle is not encodable: {error}"))
        })?;
        let store = self.record_store()?;
        let portable = store.portable().ok_or_else(|| {
            ServiceError::new(
                "unsupported",
                "this project's storage cannot take an imported bundle",
                serde_json::json!({"backend": store.describe().backend}),
            )
        })?;
        portable
            .import(transaction_id, expected_revision, &bytes)
            .map_err(ServiceError::store)
    }

    // ── Index, search and reconciliation ────────────────────────────────────

    /// Rebuild the search index from the current revision.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the store is locked or the rebuild fails.
    pub fn reindex(&self) -> Result<ProjectionStatus> {
        let provider = self.provider();
        let store = self.record_store()?;
        Projection::synchronize_store_with(&*store, provider, Some(self.content_resolver()))
            .map_err(|error| ServiceError::index(&error))
    }

    /// Search the corpus, synchronizing the index first when it cannot answer
    /// the query as it stands.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the index fails.
    pub fn search(&self, request: &SearchRequest) -> Result<SearchResult> {
        let store = self.record_store()?;
        let provider = self.provider();
        // Mutations synchronize the index, but two cases still need work here:
        // the projection may not exist yet (start-up defers it for an empty
        // store), and it may have been built before the model was resolved —
        // an index without this model's vectors cannot serve the vector
        // channel.
        let status =
            Projection::status_store(&*store).map_err(|error| ServiceError::index(&error))?;
        let stale = status.indexed_revision.as_ref() != Some(&request.revision);
        let wrong_generation = provider.as_ref().is_some_and(|provider| {
            let expected =
                memory_hub_embed::Fingerprint::from_provider(&**provider, provider.model_id())
                    .digest();
            status.fingerprint.as_deref() != Some(expected.as_str())
        });
        if stale || wrong_generation {
            self.sync_index()?;
        }
        Projection::search_store_with(&*store, request, provider, Some(self.content_resolver()))
            .map_err(|error| ServiceError::index(&error))
    }

    /// Find records that link to or mention a key.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the project is unreadable.
    pub fn backlinks(&self, key: &str, revision: Option<&Revision>) -> Result<BacklinksView> {
        let revision = match revision {
            Some(revision) => revision.clone(),
            None => self.current_revision()?,
        };
        let store = self.record_store()?;
        let entries = Projection::backlinks_store(&*store, &revision, key)
            .map_err(|error| ServiceError::index(&error))?;
        Ok(BacklinksView {
            key: key.to_owned(),
            revision,
            entries,
        })
    }

    /// Catch Memory up with code history.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the repository diverged and `mode` says to
    /// report rather than rebuild, or when the cursor cannot be advanced.
    pub fn reconcile(&self, mode: DivergenceMode) -> Result<ReconcileReport> {
        // The same rules the record store applies, for the same reason: a
        // policy that cannot tell retargeting a type from editing it would wave
        // through the silent move `record_store`'s policy refuses.
        Reconciler::open(&self.project)
            .map(|reconciler| reconciler.with_policy(Arc::new(SchemaPolicy::default())))
            .and_then(|reconciler| reconciler.reconcile(mode))
            .map_err(ServiceError::reconcile)
    }

    /// Reconcile before a mutation, reporting whether records changed.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when reconciliation fails.
    pub fn reconcile_before_mutation(&self) -> Result<bool> {
        let report = match self.reconcile(DivergenceMode::Report) {
            Ok(report) => report,
            // No code history to reconcile against. A project keeping its
            // records in a folder need not be a repository at all, and a write
            // is not the moment to tell somebody their project is not one.
            Err(error) if error.kind == "repository" => return Ok(false),
            Err(error) => return Err(error),
        };
        Ok(report
            .processed
            .iter()
            .any(|commit| !commit.stale_keys.is_empty()))
    }

    /// Basic repository and store health.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the store cannot be opened or read.
    pub fn doctor(&self) -> Result<DoctorReport> {
        let store = self.git_store()?;
        // One read of the corpus for the whole report: the three questions
        // below used to open the store and walk every record each.
        let (revision, records) = self.envelopes()?;
        Ok(DoctorReport {
            healthy: true,
            store: store.describe(),
            revision,
            attachments: self.unresolved_in(&records)?,
            hidden: Self::hidden_in(&records),
        })
    }

    /// How many records this branch does not have the document for.
    ///
    /// Reported, never counted against health: memory does not branch and code
    /// does, so a record whose document lives elsewhere is a normal state.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the records cannot be read.
    pub fn hidden_count(&self) -> Result<usize> {
        let (_, records) = self.envelopes()?;
        Ok(Self::hidden_in(&records))
    }

    fn hidden_in(records: &[Envelope]) -> usize {
        records
            .iter()
            .filter(|envelope| {
                envelope
                    .content_ref
                    .as_ref()
                    .is_some_and(|reference| reference.presence == Presence::NotOnBranch)
            })
            .count()
    }

    /// What the attached folders are still waiting on a person for.
    ///
    /// Deliberately read-only, and deliberately without an opinion: there is
    /// no automation here that resolves an ambiguity or clears a long-standing
    /// `missing`, because until the rules have been learned from use, any such
    /// automation deletes data on a guess.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the registry or the records cannot be
    /// read.
    pub fn unresolved_attachments(&self) -> Result<Vec<Unresolved>> {
        let (_, records) = self.envelopes()?;
        self.unresolved_in(&records)
    }

    fn unresolved_in(&self, records: &[Envelope]) -> Result<Vec<Unresolved>> {
        let registry = self.schema_registry()?;
        let mut unresolved = Vec::new();
        for (kind, definition) in registry.iter() {
            let Ok(storage) = definition.storage() else {
                continue;
            };
            let Some(attachment) = attachment_for(&storage) else {
                continue;
            };
            let known = known_records(records, kind);
            for record in &known {
                // Absent because this branch has no such document is routine
                // and settles itself on the branch that does. Only a deletion
                // on the branch that owns the document is a decision.
                if record.presence == Presence::Removed {
                    unresolved.push(Unresolved::RemovedFile {
                        kind: kind.clone(),
                        key: record.key.clone(),
                        locator: record.locator.clone(),
                    });
                }
            }
            let source = FolderSource::new(&self.project, attachment.clone());
            let documents = source.list()?;
            for change in attach::classify(
                &documents,
                &known,
                &|locator| source.tracked(locator),
                &|locator| attach::key_for(&attachment, locator),
            ) {
                if let ScanChange::Unmatched {
                    locator,
                    candidates,
                    ..
                } = change
                {
                    unresolved.push(Unresolved::UnmatchedFile {
                        kind: kind.clone(),
                        locator,
                        candidates,
                    });
                }
            }
        }
        Ok(unresolved)
    }

    /// The index's own view of how far it has caught up.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the status cannot be read.
    pub fn index_status(&self) -> Result<ProjectionStatus> {
        Projection::status_store(&*self.record_store()?)
            .map_err(|error| ServiceError::index(&error))
    }

    /// The active embedding model, or `None` when search runs FTS-only.
    #[must_use]
    pub fn model_status(&self) -> Option<ModelStatus> {
        self.provider().map(|provider| {
            ModelStatusBuilder::default()
                .model_id(provider.model_id())
                .display_name(provider.name())
                .dimensions(provider.dimensions())
                .runtime_state(ModelRuntime::Active)
                .build()
        })
    }

    // ── Schema ──────────────────────────────────────────────────────────────

    /// Load the `__type__` corpus.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the registry cannot be read.
    pub fn schema_registry(&self) -> Result<SchemaRegistry> {
        let store = self.record_store()?;
        let snapshot = StoreView::current(&*store).map_err(ServiceError::store)?;
        policy::load_registry(&*store, snapshot.revision()).map_err(ServiceError::store)
    }

    /// Summarise the declared document types.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the registry cannot be read.
    pub fn list_types(&self) -> Result<Vec<TypeSummary>> {
        let mut summaries = Vec::new();
        for (_, definition) in self.schema_registry()?.iter() {
            let storage = definition.storage().ok();
            let folder = storage.as_ref().and_then(|storage| storage.folder());
            // A type keeping its content in its records is written by writing
            // a record, which is always possible. A type pointing at a folder
            // is only as writable as that folder.
            let writable = match storage.as_ref().and_then(attachment_for) {
                None => true,
                Some(attachment) => {
                    FolderSource::new(&self.project, attachment)
                        .capabilities()
                        .writable
                }
            };
            summaries.push(TypeSummary {
                kind_name: definition.kind_name.clone(),
                description: definition.description.clone(),
                field_count: definition.fields.len(),
                relationship_count: definition.relationships.len(),
                storage: folder.map(str::to_owned),
                writable,
            });
        }
        Ok(summaries)
    }

    /// Validate every record against the active schema.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the registry or the corpus cannot be read.
    pub fn schema_status(&self) -> Result<SchemaStatus> {
        let registry = self.schema_registry()?;
        if registry.is_empty() {
            return Ok(SchemaStatus {
                active: false,
                revision: None,
                total_records: 0,
                incompatible: Vec::new(),
            });
        }
        let (revision, envelopes) = self.corpus(None)?;
        let mut incompatible = Vec::new();
        let mut total = 0usize;
        for (key, envelope) in &envelopes {
            if envelope.kind == TYPE_KIND {
                continue;
            }
            total += 1;
            match registry.get(&envelope.kind) {
                Some(definition) => {
                    if let Err(error) = definition.validate(envelope) {
                        incompatible.push(Incompatibility {
                            key: key.clone(),
                            kind: envelope.kind.clone(),
                            field: error.field,
                            reason: error.message,
                        });
                    }
                }
                None => incompatible.push(Incompatibility {
                    key: key.clone(),
                    kind: envelope.kind.clone(),
                    field: "kind".to_owned(),
                    reason: format!("kind `{}` has no type definition", envelope.kind),
                }),
            }
        }
        Ok(SchemaStatus {
            active: true,
            revision: Some(revision),
            total_records: total,
            incompatible,
        })
    }

    /// Counts by kind, freshness and archive state over the whole corpus.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the corpus cannot be read.
    pub fn records_summary(&self) -> Result<RecordsSummary> {
        let (revision, envelopes) = self.corpus(None)?;
        Ok(RecordsSummary {
            revision,
            counts: ListingCounts::over_corpus(&envelopes),
        })
    }

    // ── Transport ───────────────────────────────────────────────────────────

    /// The configured memory remote, if any.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the Git config cannot be read.
    pub fn transport_status(&self) -> Result<Option<MemoryRemote>> {
        let store = self.git_store()?;
        memory_hub_store::read_remote_config(store.git_dir()).map_err(ServiceError::store)
    }

    /// The code `origin`, which is not the memory remote and is only ever
    /// asked, never published to.
    ///
    /// A caller offering to configure a memory remote has one sensible
    /// suggestion to make, and this is it.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the repository or its configuration
    /// cannot be read.
    pub fn code_origin_url(&self) -> Result<Option<String>> {
        let git_dir = self.memory_git_dir()?;
        memory_hub_store::read_code_origin_url(&git_dir).map_err(ServiceError::store)
    }

    /// Whether this repository's memory is here, elsewhere, or nowhere.
    ///
    /// Deliberately independent of the project's storage declaration: the
    /// question is asked precisely by a caller that has just opened a fresh
    /// clone and does not yet know whether declaring anything would be
    /// describing a project that already exists somewhere else.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the repository cannot be read. A remote
    /// that cannot be reached is an answer rather than an error.
    pub fn presence(&self) -> Result<memory_hub_store::MemoryPresence> {
        memory_hub_store::memory_presence(&self.project).map_err(ServiceError::store)
    }

    /// Configure the memory remote.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the URL or the refspec is one Git would
    /// read as something other than a location, or when the configuration
    /// cannot be written.
    pub fn set_remote(&self, url: String, refspec: Option<String>) -> Result<MemoryRemote> {
        let git_dir = self.memory_git_dir()?;
        let remote = MemoryRemote { url, refspec };
        memory_hub_store::write_remote_config(&git_dir, &remote).map_err(ServiceError::store)?;
        Ok(remote)
    }

    /// Forget the memory remote. Publishing nothing is the default state, and
    /// returning to it is not a failure when there was none configured.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the configuration cannot be written.
    pub fn remove_remote(&self) -> Result<()> {
        let git_dir = self.memory_git_dir()?;
        memory_hub_store::remove_remote_config(&git_dir).map_err(ServiceError::store)
    }

    /// The Git directory, found without opening a store.
    ///
    /// `git_store()` would answer this too, and would refuse for a project
    /// that has not declared a storage yet — which is the project every one of
    /// the callers above is about. Opening a store also creates
    /// `refs/memory/staged`, and a repository that has been *asked* about its
    /// memory must not come away looking like one that has some.
    fn memory_git_dir(&self) -> Result<std::path::PathBuf> {
        memory_hub_store::GitStore::discover_git_dir(&self.project).map_err(ServiceError::store)
    }

    /// Fetch memory refs from the remote and merge them.
    ///
    /// Verification is fail-closed: with no allowed signer known, the store
    /// refuses rather than accepting whatever the remote offers.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when no remote is configured, the fetch fails,
    /// or the merge cannot be completed.
    pub fn fetch(&self) -> Result<FetchResult> {
        let store = self.git_store()?;
        let remote = Self::require_remote(&store)?;
        memory_hub_store::fetch_and_merge(&store, &remote).map_err(ServiceError::store)
    }

    /// Push memory refs to the remote, subject to the stale-record policy.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the policy blocks the push, no remote is
    /// configured, or the push itself fails.
    pub fn push(&self, force: bool) -> Result<PushOutcome> {
        let store = self.git_store()?;
        // Apply push policy before the network mutation.
        let policy = memory_hub_store::check_push_policy(&store).map_err(ServiceError::store)?;
        if !policy.allowed {
            return Err(ServiceError::new(
                "push_blocked",
                "push blocked by memory_push_stale policy",
                serde_json::json!({
                    "stale_count": policy.stale_count,
                    "warnings": policy.warnings,
                    "recovery_action": "refresh_stale_records_or_override_policy",
                }),
            ));
        }
        let remote = Self::require_remote(&store)?;
        memory_hub_store::push_to_remote(store.git_dir(), &remote, force)
            .map_err(ServiceError::store)?;
        Ok(PushOutcome {
            remote,
            force,
            policy,
        })
    }

    fn require_remote(store: &GitStore) -> Result<MemoryRemote> {
        memory_hub_store::read_remote_config(store.git_dir())
            .map_err(ServiceError::store)?
            .ok_or_else(|| {
                ServiceError::new(
                    "no_remote_configured",
                    "no memory remote configured",
                    serde_json::json!({"recovery_action": "configure_remote_first"}),
                )
            })
    }

}

/// One record as of a revision.
#[derive(Clone, Debug)]
pub struct RecordView {
    pub revision: Revision,
    /// The record, in the one shape every storage answers in.
    pub record: Option<StoredRecord>,
}

/// A bundle and the revision it was taken from.
#[derive(Clone, Debug)]
pub struct ExportView {
    pub revision: Revision,
    pub bundle: ExportBundle,
}

/// What reading a record's body produced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ContentResolution {
    /// The record carries its own content.
    Inline { content: String },
    /// The locator was followed and the content read.
    Resolved {
        path: String,
        content: Content,
        /// What the content hashes to now.
        hash: ContentHash,
        /// Whether that differs from the digest the record last recorded —
        /// somebody edited the file since Memory last looked.
        changed: bool,
    },
    /// The locator points at nothing readable here. A normal state, not a
    /// failure: deleted, on another branch, or not pulled yet.
    Missing { path: String, reason: String },
}

/// A body, in whatever shape it actually has.
///
/// Three answers rather than one, because "give me the content" has three
/// honest outcomes and pretending otherwise costs something each time: text
/// read as bytes is unreadable, bytes read as text is a failure for a file
/// that is fine, and a video read at all is hundreds of megabytes crossing a
/// protocol for nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Content {
    /// Text, decoded.
    Text(String),
    /// Bytes that are not text — an image, a PDF, a diagram.
    Bytes(Vec<u8>),
    /// The content is somewhere the caller can reach, and reaching it is
    /// theirs to do. No source produces this yet; the shape is here so adding
    /// one later is not a change to what every client already parses.
    Link { url: String, media_type: String },
}

impl Content {
    /// The text, when it is text.
    #[must_use]
    pub fn as_text(&self) -> Option<&str> {
        match self {
            Self::Text(text) => Some(text),
            Self::Bytes(_) | Self::Link { .. } => None,
        }
    }

    /// What this hashes to, whatever shape it is in.
    #[must_use]
    pub fn hash(&self) -> ContentHash {
        match self {
            Self::Text(text) => ContentHash::for_content(text),
            Self::Bytes(bytes) => ContentHash::for_bytes(bytes),
            // A link's bytes are not here to hash; the locator is what there
            // is to compare.
            Self::Link { url, .. } => ContentHash::for_content(url),
        }
    }
}

/// What moving a type's records to another storage would do, or did.
#[derive(Clone, Debug)]
pub struct MigrationPlan {
    pub kind: String,
    /// The storage the records are in now, by name. `None` means they are
    /// in the storage that holds records — the type named none.
    pub from: Option<String>,
    pub to: Option<String>,
    /// Whether the declared storage is the same in every respect, not only in
    /// its place. Two repository folders are the same place and different
    /// storage, and treating them as one would leave every record pointing at
    /// a folder its type no longer names.
    pub unchanged: bool,
    /// The records that move. Named rather than counted, so a caller can show
    /// what it is about to change.
    pub keys: Vec<String>,
    /// What has to be acknowledged before this will run.
    pub warnings: Vec<MigrationWarning>,
}

/// Something a migration does that a caller must accept on purpose.
#[derive(Clone, Debug)]
pub struct MigrationWarning {
    /// Stable, and what an acknowledgement names.
    pub code: &'static str,
    pub message: String,
}

/// What removing a type took with it.
#[derive(Clone, Debug)]
pub struct TypeRemoval {
    /// The revision after the removal.
    pub revision: Revision,
    /// How many records of the type went with the definition.
    pub removed: usize,
    /// The folder that stopped being attached, for a type that had one.
    ///
    /// `None` for a type whose documents were its own records: nothing was
    /// detached, because nothing outside the records was ever involved.
    pub detached: Option<String>,
}

/// What a scan of the attached folders found and did.
#[derive(Clone, Debug)]
pub struct ScanReport {
    /// The revision after whatever the scan applied.
    pub revision: Revision,
    /// Documents looked at, across every attachment.
    pub scanned: usize,
    /// Everything concluded, applied or not.
    pub changes: Vec<ScanChange>,
    /// How many of those were written.
    pub applied: usize,
    /// The commit the working tree had when this scan ran.
    pub code_revision: Option<String>,
    /// What it had when the previous scan ran. A different value means the
    /// branch moved in between, which is the one thing that changes what a
    /// scan can see without anybody editing a document.
    pub previous_code_revision: Option<String>,
}

/// Inbound references to a key.
#[derive(Clone, Debug)]
pub struct BacklinksView {
    pub key: String,
    pub revision: Revision,
    pub entries: Vec<BacklinkEntry>,
}

/// Repository and store health.
#[derive(Clone, Debug)]
pub struct DoctorReport {
    pub healthy: bool,
    /// The backend, and only the backend-specific facts it actually has.
    pub store: StoreDescription,
    pub revision: Revision,
    /// What the attached folders need a person to decide.
    pub attachments: Vec<Unresolved>,
    /// Records whose document this branch does not have. Information, not a
    /// fault.
    pub hidden: usize,
}

/// Something an attached folder cannot settle on its own.
#[derive(Clone, Debug, PartialEq)]
pub enum Unresolved {
    /// A file matching no record. It is either new or a rename with an edit,
    /// and nothing about it says which.
    UnmatchedFile {
        kind: String,
        locator: String,
        candidates: Vec<RenameCandidate>,
    },
    /// A record whose document the branch still has, but the working tree does
    /// not. Somebody deleted it deliberately, on the branch that owns it, and
    /// the only question left is whether the record should go too.
    RemovedFile {
        kind: String,
        key: String,
        locator: String,
    },
}

/// One declared document type, summarised.
#[derive(Clone, Debug)]
pub struct TypeSummary {
    pub kind_name: String,
    pub description: Option<String>,
    pub field_count: usize,
    pub relationship_count: usize,
    /// The storage this type's content lives in, when it is not the one
    /// holding the records.
    pub storage: Option<String>,
    /// Whether documents of this type can be written at all.
    ///
    /// Asked of the storage rather than assumed, and answered before anything
    /// is attempted: a client drawing a list of types should be able to tell
    /// which of them it may add to without finding out from a failure.
    pub writable: bool,
}

/// How the corpus stands against the declared types.
#[derive(Clone, Debug)]
pub struct SchemaStatus {
    /// `false` when no `__type__` record exists — validation is inactive, not
    /// failing.
    pub active: bool,
    pub revision: Option<Revision>,
    pub total_records: usize,
    pub incompatible: Vec<Incompatibility>,
}

/// One record that does not satisfy its type.
#[derive(Clone, Debug)]
pub struct Incompatibility {
    pub key: String,
    pub kind: String,
    pub field: String,
    pub reason: String,
}

/// Corpus-wide counts.
#[derive(Clone, Debug)]
pub struct RecordsSummary {
    pub revision: Revision,
    pub counts: ListingCounts,
}

/// What a push did, and what the policy had to say about it.
#[derive(Clone, Debug)]
pub struct PushOutcome {
    pub remote: MemoryRemote,
    pub force: bool,
    pub policy: PushPolicyResult,
}

/// The wire spelling of a freshness state.
#[must_use]
pub const fn freshness_str(state: FreshnessState) -> &'static str {
    match state {
        FreshnessState::Unverified => "unverified",
        FreshnessState::Fresh => "fresh",
        FreshnessState::Stale => "stale",
        FreshnessState::Invalid => "invalid",
    }
}

/// The reference records of one kind, as the scanner wants them.
fn known_records(records: &[Envelope], kind: &str) -> Vec<KnownRecord> {
    records
        .iter()
        .filter_map(|envelope| {
            if envelope.kind != kind {
                return None;
            }
            let reference = envelope.content_ref.as_ref()?;
            Some(KnownRecord {
                key: envelope.key.clone(),
                locator: reference.path.clone(),
                hash: envelope.content_hash.clone(),
                presence: reference.presence,
            })
        })
        .collect()
}

fn find_envelope(records: &[Envelope], key: &str) -> Option<Envelope> {
    records.iter().find(|envelope| envelope.key == key).cloned()
}

/// A file name for a record being published into a folder.
///
/// What a document Memory writes into a folder is called, after the key.
///
/// A folder of documents is read whatever its files are called — there is no
/// mask, deliberately, because a person who put a diagram in their
/// documentation folder should see the diagram in Memory. This is only what to
/// call a file nobody has named yet, and prose is what Memory writes.
const NEW_DOCUMENT_SUFFIX: &str = ".md";

/// Where a type's documents are, when they are files in the working tree.
///
/// `None` means the bodies sit in the records. Nothing is resolved and nothing
/// can fail: the folder is written in the definition, so a type that names one
/// has already said everything there is to know about where it keeps its
/// documents.
#[must_use]
fn attachment_for(storage: &TypeStorage) -> Option<Attachment> {
    storage
        .folder()
        .map(|folder| Attachment::new(folder.to_owned()))
}

/// Reads a repository-relative locator out of the working tree.
///
/// The one place the engine goes outside for content, and it happens while the
/// index is built — never while a query is served, so an unavailable folder can
/// make a rebuild incomplete but can never make a listing quietly return less.
#[derive(Debug)]
struct WorkingTreeContent {
    project: PathBuf,
}

impl ContentResolver for WorkingTreeContent {
    fn resolve(&self, locator: &str) -> ResolvedContent {
        // The locator is validated on the record, but this reads the
        // filesystem, so it is checked again here rather than trusted: a path
        // that escaped once would read a file outside the project.
        if memory_hub_core::validate_locator("content_ref.path", locator).is_err() {
            return ResolvedContent::Missing;
        }
        let Ok(bytes) = std::fs::read(self.project.join(locator)) else {
            return ResolvedContent::Missing;
        };
        String::from_utf8(bytes).map_or(ResolvedContent::Binary, ResolvedContent::Text)
    }
}

/// A short, stable digest of what a batch of operations says.
///
/// Used to make a transaction id describe the request rather than the occasion.
fn digest_of(operations: &[Operation]) -> String {
    use sha2::{Digest, Sha256};
    let encoded = serde_json::to_vec(operations).unwrap_or_default();
    let digest = Sha256::digest(&encoded);
    format!("{digest:x}")[..16].to_owned()
}

/// A folder's new path when `from` is renamed to `to`, or `None` when the
/// folder is not under `from` at all.
///
/// Prefix work on segments rather than on bytes: `docs/guidelines` starts with
/// `docs/guide` as text and is a different folder entirely.
fn renamed_folder(folder: &str, from: &str, to: &str) -> Option<String> {
    if folder == from {
        return Some(to.to_owned());
    }
    folder
        .strip_prefix(&format!("{from}/"))
        .map(|rest| format!("{to}/{rest}"))
}
