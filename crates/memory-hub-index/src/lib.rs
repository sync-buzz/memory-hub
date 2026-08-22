//! Recoverable `LanceDB` read model for generic Memory Hub envelopes.
//!
//! Git remains authoritative. The projection metadata is an atomic pointer to
//! the canonical revision represented by the `records` table; callers never
//! receive a table handle and therefore cannot bypass freshness checks.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use arrow_array::{
    Array, BooleanArray, FixedSizeListArray, Float32Array, RecordBatch, RecordBatchIterator,
    StringArray,
    builder::{FixedSizeListBuilder, Float32Builder},
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use fs2::FileExt;
use futures::TryStreamExt;
use lancedb::DistanceType;
use lancedb::connection::Connection;
use lancedb::index::Index as LanceIndex;
use lancedb::index::scalar::{FtsIndexBuilder, FullTextSearchQuery};
use lancedb::query::{ExecutableQuery, QueryBase};
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_embed::{
    EmbeddingProvider, Fingerprint, content_hash_of, renderer::render_envelope_inner,
};
use memory_hub_engine::{ChangeKind, RecordId, RecordStore, Revision, StoreView};
use serde::{Deserialize, Serialize};

const TABLE: &str = "records";
/// Schema of the derived table and of the status file beside it.
///
/// Published in the MCP handshake as the index version, so a client can tell
/// one shape of the read model from another. Bumped whenever a column is added
/// or its meaning changes; a projection written by another value is rebuilt
/// rather than read.
pub const META_SCHEMA: u32 = 6;
const TAGS_DELIMITER: char = '\n';
const MAX_SEARCH_LIMIT: usize = 200;
/// BM25 hits below this count trigger the vector rescue channel.
const RESCUE_THRESHOLD: usize = 5;
/// Minimum cosine similarity for a vector hit to survive the rescue floor.
///
/// Measured rather than chosen. Over four classes of query against a real
/// project — nonsense (`qqqqqqqqq`), off-topic but real English (`banana bread
/// recipe`), meaning the corpus holds said in another language (`шифрование
/// записей`), and plain hits — every candidate below this was noise in all four
/// classes, and no class had a genuine answer under it.
///
/// It does not separate a relevant query from an irrelevant one, and no floor
/// can: the best candidate for `photosynthesis` scored 0.517 against a corpus
/// that has never heard of it, higher than `keyboard shortcut` at 0.512 which it
/// answers correctly. Nearest is nearest — in a corpus of a dozen records
/// something is always closest, and how close depends on how broadly that record
/// is written rather than on the question. Which is why a semantic hit is
/// labelled [`MatchedBy::Meaning`] and left for the caller to present as such,
/// instead of being tuned until it looks like a word match.
const VECTOR_RESCUE_FLOOR: f64 = 0.45;
/// RRF fusion constant: `combined = 1/(K+rank_a) + 1/(K+rank_b)`.
const RRF_K: usize = 60;
/// Maximum vector candidates fetched before the rescue floor filter.
const VECTOR_FETCH: usize = 20;
/// Batch size for embedding during a full rebuild.
const EMBED_BATCH: usize = 128;

/// What a locator points at, as far as an index is concerned.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ResolvedContent {
    /// Text the full-text channel can search.
    Text(String),
    /// Bytes that are not text — a diagram, a PDF, an image. It is still a
    /// document: it moves, it is edited, it goes missing. What it cannot be is
    /// searched by its words.
    Binary,
    /// Nothing readable at that locator right now. Another branch has it, or
    /// somebody removed it, and `presence` is where that is recorded.
    Missing,
}

/// Reads what a record points at, for the index to project.
///
/// A reference record keeps no copy of its content, so without this the read
/// model would hold an empty body for every document in an attached folder and
/// full-text search would find them by nothing but their type. The index does
/// not know what a locator is — a path, a URL, a row somewhere — which is why
/// this is a trait the owning layer implements.
///
/// Reading happens while the projection is built, never while a query is
/// served: a corpus operation that reached outside could fail, or quietly
/// return less, because somebody's folder was unavailable.
pub trait ContentResolver: Send + Sync {
    fn resolve(&self, locator: &str) -> ResolvedContent;
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionState {
    Fresh,
    Lagging,
    Rebuilding,
    Corrupt,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectionStatus {
    pub schema_version: u32,
    pub state: ProjectionState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub indexed_revision: Option<Revision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target_revision: Option<Revision>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedRecord {
    pub id: String,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub content: Option<String>,
    pub archived: bool,
    pub freshness: Option<String>,
    pub tags: Vec<String>,
}

// ---------------------------------------------------------------------------
// Search
// ---------------------------------------------------------------------------

/// Filter predicates applied alongside the FTS query.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct SearchFilters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// Several kinds at once. Empty means no restriction from this field.
    ///
    /// A union with `kind` rather than a replacement for it: a caller that
    /// names both is asking for both, and the two spellings of the same idea
    /// disagreeing about which wins is a question nobody should have to look
    /// up. One kind is the common case and stays a string; a person narrowing
    /// a search to the three types they work in is the case this exists for,
    /// and answering it with one query is the difference between a filter and
    /// a fan-out the caller has to fuse itself.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub kinds: Vec<String>,
    /// Restrict to a folder. `Some("")` is the root — records filed nowhere.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Whether the folder filter reaches below the folder it names.
    #[serde(default)]
    pub folder_subtree: bool,
    /// `"present"` (the default), `"any"`, or `"absent"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presence: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// `Some(true)` → only archived, `Some(false)` → only live, `None` → both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// Empty vec → all freshness states.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub freshness: Vec<String>,
    /// Whether records that are Memory's own machinery are searched.
    ///
    /// A type definition is schema, and answering a question about the subject
    /// matter with a JSON schema is answering the wrong question. Asking for
    /// its kind, or raising this, reaches it.
    #[serde(default)]
    pub include_service: bool,
}

/// One search request. `revision` pins the snapshot the index must represent.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct SearchRequest {
    pub query: String,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
    #[serde(default)]
    pub offset: usize,
    #[serde(default)]
    pub filters: SearchFilters,
    pub revision: Revision,
}

fn default_search_limit() -> usize {
    20
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    Fts,
    Hybrid,
}

/// One hit in a [`SearchResult`].
#[derive(Clone, Debug, Serialize)]
pub struct SearchHit {
    pub id: String,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub content: Option<String>,
    pub archived: bool,
    pub freshness: Option<String>,
    pub tags: Vec<String>,
    /// Where the record is filed, so a caller can show the hierarchy it was
    /// allowed to filter by.
    pub folder: Option<String>,
    /// Why the content is not here, when it is not. Absent for a record that
    /// carries its own content.
    pub presence: Option<String>,
    /// `text` or `binary` for a record that points at a file. A binary
    /// document is findable by its metadata and never by its words, and a
    /// caller that shows an empty body should be able to say which of the two
    /// it is looking at.
    pub content_kind: Option<String>,
    /// BM25 score from FTS (higher is better). `None` when FTS did not match.
    pub fts_score: Option<f64>,
    /// Semantic similarity score from vector search (higher is better).
    /// `None` when vector search is unavailable or not run.
    pub vector_score: Option<f64>,
    /// Deterministic fusion of available channel scores (higher is better).
    pub combined_rank: f64,
    /// Which channel found this, so a caller can say so.
    ///
    /// Derivable from the two scores and stated anyway: a caller reading
    /// `fts_score: null` has to know that the absence of a BM25 score means
    /// "the words did not match" rather than "the score was not computed", and
    /// that is a fact about this engine's fusion rather than about the record.
    pub matched: MatchedBy,
}

/// How a hit was found.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MatchedBy {
    /// The query's words are in the record. BM25 ranked it.
    Words,
    /// Nothing matched by words; the record is near the query in meaning.
    /// Always true of *something* when the channel runs, which is why it is
    /// worth telling apart from a word match.
    Meaning,
    /// Both channels returned it.
    Both,
}

/// Result of [`Projection::search`].
#[derive(Clone, Debug, Serialize)]
pub struct SearchResult {
    pub hits: Vec<SearchHit>,
    pub total: usize,
    pub limit: usize,
    pub offset: usize,
    pub has_more: bool,
    pub mode: SearchMode,
    /// `true` when vector search was requested but unavailable (FTS-only).
    pub degraded: bool,
    /// Revision the index represented when serving this search.
    pub revision: Revision,
}

// ---------------------------------------------------------------------------
// Backlinks
// ---------------------------------------------------------------------------

/// How a backlink was discovered.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MentionType {
    /// The target key appears in the source record's `envelope.links` array.
    ExplicitLink,
    /// The target key appears as a word-boundary substring of `envelope.content`.
    BodyMention,
}

/// One record that links to or mentions the target key.
#[derive(Clone, Debug, Serialize)]
pub struct BacklinkEntry {
    pub source_id: String,
    pub source_kind: Option<String>,
    pub source_title: Option<String>,
    pub relation: Option<String>,
    pub mention_type: MentionType,
    /// Whether the source's own content is here.
    ///
    /// A link is a statement about the project, not about a branch, so a
    /// source whose document lives on another branch keeps its backlink and
    /// is marked. Dropping it would show a document as unreferenced on `main`
    /// when it is referenced everywhere else.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_presence: Option<String>,
}

#[derive(Debug)]
pub struct IndexError {
    message: String,
}

impl IndexError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for IndexError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for IndexError {}

impl From<std::io::Error> for IndexError {
    fn from(error: std::io::Error) -> Self {
        Self::new(format!("projection filesystem operation failed: {error}"))
    }
}

#[derive(Clone)]
pub struct Projection {
    root: PathBuf,
    connection: Arc<RwLock<Connection>>,
    embed_provider: Option<Arc<dyn EmbeddingProvider>>,
    content_resolver: Option<Arc<dyn ContentResolver>>,
}

impl Projection {
    /// Open the default per-repository projection for a store, with an optional
    /// embedding provider attached.
    ///
    /// This is the async entry point every `*_store` facade below is a
    /// synchronous wrapper around. Async callers should use it directly: it
    /// composes with an existing runtime instead of standing one up.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog cannot be opened.
    pub async fn open_store(
        store: &dyn RecordStore,
        provider: Option<Arc<dyn EmbeddingProvider>>,
        content: Option<Arc<dyn ContentResolver>>,
    ) -> Result<Self, IndexError> {
        let mut projection = Self::open(store.index_root()).await?;
        if let Some(provider) = provider {
            projection = projection.with_embed_provider(provider);
        }
        if let Some(content) = content {
            projection = projection.with_content_resolver(content);
        }
        Ok(projection)
    }

    /// Synchronize the default per-repository projection from synchronous
    /// adapters such as the MCP stdio server.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime, catalog, or synchronization fails.
    pub fn synchronize_store(store: &dyn RecordStore) -> Result<ProjectionStatus, IndexError> {
        Self::synchronize_store_with(store, None, None)
    }

    /// Synchronous entry point with an optional embedding provider attached.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime, catalog, or synchronization fails.
    pub fn synchronize_store_with(
        store: &dyn RecordStore,
        provider: Option<Arc<dyn EmbeddingProvider>>,
        content: Option<Arc<dyn ContentResolver>>,
    ) -> Result<ProjectionStatus, IndexError> {
        run_off_thread(|| async {
            let projection = Self::open_store(store, provider, content).await?;
            projection.synchronize(store).await
        })
    }

    /// Read the default per-repository projection status without opening the
    /// `LanceDB` catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock or durable status cannot be read.
    pub fn status_store(store: &dyn RecordStore) -> Result<ProjectionStatus, IndexError> {
        let root = store.index_root();
        // A projection that was never built is not an error to ask about: the
        // honest answer is "lagging, indexed nothing". Reporting the missing
        // directory instead would make the first search on a fresh project fail
        // rather than build the index it is missing.
        if !root.exists() {
            return Ok(ProjectionStatus {
                schema_version: META_SCHEMA,
                state: ProjectionState::Lagging,
                indexed_revision: None,
                target_revision: None,
                fingerprint: None,
            });
        }
        let _lock = lock_at(&root, false)?;
        read_status_at(&root)
    }

    /// Synchronous entry point for MCP: open the default per-repository
    /// projection, run a search, and return the result.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime, catalog, or search fails.
    pub fn search_store(
        store: &dyn RecordStore,
        request: &SearchRequest,
    ) -> Result<SearchResult, IndexError> {
        Self::search_store_with(store, request, None, None)
    }

    /// Synchronous entry point for MCP: open the default per-repository
    /// projection, run a search with an optional embedding provider, and return
    /// the result.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime, catalog, or search fails.
    pub fn search_store_with(
        store: &dyn RecordStore,
        request: &SearchRequest,
        provider: Option<Arc<dyn EmbeddingProvider>>,
        content: Option<Arc<dyn ContentResolver>>,
    ) -> Result<SearchResult, IndexError> {
        run_off_thread(|| async {
            let projection = Self::open_store(store, provider, content).await?;
            projection.search(request).await
        })
    }

    /// Compute backlinks from a canonical snapshot — no `LanceDB` required.
    ///
    /// Returns every record that links to or mentions `key` via explicit
    /// `envelope.links` or body-mention scanning.
    ///
    /// # Errors
    ///
    /// Returns an error when the snapshot cannot be read.
    pub fn backlinks_store(
        store: &dyn RecordStore,
        revision: &Revision,
        key: &str,
    ) -> Result<Vec<BacklinkEntry>, IndexError> {
        let view = StoreView::open(store, revision).map_err(store_error)?;
        compute_backlinks(&view, key)
    }

    /// Open the disposable projection at an explicit local-state directory.
    ///
    /// # Errors
    ///
    /// Returns an error for a relative path or when the catalog cannot open.
    pub async fn open(root: impl AsRef<Path>) -> Result<Self, IndexError> {
        let root = root.as_ref();
        if !root.is_absolute() {
            return Err(IndexError::new("projection path must be absolute"));
        }
        fs::create_dir_all(root)?;
        let lance = root.join("lance");
        fs::create_dir_all(&lance)?;
        let connection = lancedb::connect(lance.to_string_lossy().as_ref())
            .read_consistency_interval(std::time::Duration::ZERO)
            .execute()
            .await
            .map_err(lance_error)?;
        Ok(Self {
            root: root.to_path_buf(),
            connection: Arc::new(RwLock::new(connection)),
            embed_provider: None,
            content_resolver: None,
        })
    }

    /// Attach an embedding provider to enable vector-rescue hybrid search.
    /// The provider's fingerprint is derived lazily during rebuild/search.
    #[must_use]
    pub fn with_embed_provider(mut self, provider: Arc<dyn EmbeddingProvider>) -> Self {
        self.embed_provider = Some(provider);
        self
    }

    /// Attach a reader for content that lives outside the records.
    ///
    /// Without one, a record that points at a file is projected with an empty
    /// body — which is what it holds, and not what it means.
    #[must_use]
    pub fn with_content_resolver(mut self, resolver: Arc<dyn ContentResolver>) -> Self {
        self.content_resolver = Some(resolver);
        self
    }

    /// Report the durable projection state. Missing metadata means lagging,
    /// never an implicitly fresh empty index.
    ///
    /// # Errors
    ///
    /// Returns an error when metadata exists but cannot be read or decoded.
    pub fn status(&self) -> Result<ProjectionStatus, IndexError> {
        let _lock = self.read_lock()?;
        self.read_status_unlocked()
    }

    /// Recreate the projection solely from an immutable Memory Hub snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the canonical snapshot or derived catalog fails.
    pub async fn rebuild(&self, snapshot: &StoreView<'_>) -> Result<ProjectionStatus, IndexError> {
        let _lock = self.write_lock_async().await?;
        self.rebuild_unlocked(snapshot).await
    }

    async fn rebuild_unlocked(
        &self,
        snapshot: &StoreView<'_>,
    ) -> Result<ProjectionStatus, IndexError> {
        let target = snapshot.revision().clone();
        self.write_status(&ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Rebuilding,
            indexed_revision: self
                .read_status_unlocked()
                .ok()
                .and_then(|s| s.indexed_revision),
            target_revision: Some(target.clone()),
            fingerprint: self.embed_provider.as_ref().map(provider_fingerprint),
        })?;
        let records = snapshot.records().map_err(store_error)?;
        let (batch, vector_dim) = build_batch_from_records(
            &records,
            self.embed_provider.as_ref(),
            self.content_resolver.as_ref(),
        )
        .await?;
        let connection = self.connection()?;
        let names = connection
            .table_names()
            .execute()
            .await
            .map_err(lance_error)?;
        if names.iter().any(|name| name == TABLE) {
            connection
                .drop_table(TABLE, &[])
                .await
                .map_err(lance_error)?;
        }
        let schema = match vector_dim {
            Some(dim) => schema_with_vector(dim),
            None => schema(),
        };
        if batch.num_rows() == 0 {
            connection
                .create_empty_table(TABLE, schema)
                .execute()
                .await
                .map_err(lance_error)?;
        } else {
            connection
                .create_table(TABLE, reader(batch))
                .execute()
                .await
                .map_err(lance_error)?;
        }
        if !records.is_empty() {
            let table = connection
                .open_table(TABLE)
                .execute()
                .await
                .map_err(lance_error)?;
            ensure_fts_indices(&table).await?;
        }
        let status = ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Fresh,
            indexed_revision: Some(target),
            target_revision: None,
            fingerprint: self.embed_provider.as_ref().map(provider_fingerprint),
        };
        self.write_status(&status)?;
        Ok(status)
    }

    /// Apply the exact canonical key delta between two retained snapshots.
    ///
    /// # Errors
    ///
    /// Returns an error for a stale base or a Git/`LanceDB` operation failure.
    pub async fn update(
        &self,
        store: &dyn RecordStore,
        from: &Revision,
        to: &Revision,
    ) -> Result<ProjectionStatus, IndexError> {
        let _lock = self.write_lock_async().await?;
        let current = self.read_status_unlocked()?;
        if current.state != ProjectionState::Fresh
            || current.indexed_revision.as_ref() != Some(from)
        {
            return Err(IndexError::new(
                "projection revision does not match incremental base",
            ));
        }
        let target = StoreView::open(store, to).map_err(store_error)?;
        // Incremental repair needs a diff between two past states. A store
        // without history cannot produce one, and the caller falls back to a
        // full rebuild rather than serving a half-updated index.
        let delta = store
            .history()
            .ok_or_else(|| IndexError::new("incremental update needs a store that keeps history"))?
            .diff(from, to)
            .map_err(store_error)?;
        self.write_status(&ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Lagging,
            indexed_revision: Some(from.clone()),
            target_revision: Some(to.clone()),
            fingerprint: self.embed_provider.as_ref().map(provider_fingerprint),
        })?;
        let table = self
            .connection()?
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lance_error)?;
        let deleted = delta
            .iter()
            .filter(|change| change.kind == ChangeKind::Deleted)
            .map(|change| change.id.display_value())
            .collect::<Vec<_>>();
        if !deleted.is_empty() {
            // Record ids are store-generated opaque identifiers, so a value the
            // literal rule rejects means the store changed shape underneath the
            // index — a hard error rather than something to escape around.
            let predicate = deleted
                .iter()
                .map(|id| {
                    sql_string_literal(id)
                        .map(|literal| format!("id = {literal}"))
                        .ok_or_else(|| {
                            IndexError::new(format!(
                                "record id is not expressible as a predicate literal: {id}"
                            ))
                        })
                })
                .collect::<Result<Vec<_>, IndexError>>()?
                .join(" OR ");
            table.delete(&predicate).await.map_err(lance_error)?;
        }
        let updated_records = delta
            .iter()
            .filter(|change| change.kind != ChangeKind::Deleted)
            .map(|change| {
                target
                    .get(&change.id)
                    .map_err(store_error)?
                    .map(|record| (change.id.clone(), record))
                    .ok_or_else(|| IndexError::new("changed record is absent from target snapshot"))
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !updated_records.is_empty() {
            let (batch, vector_dim) = build_batch_from_records(
                &updated_records,
                self.embed_provider.as_ref(),
                self.content_resolver.as_ref(),
            )
            .await?;
            let mut merge = table.merge_insert(&["id"]);
            merge
                .when_matched_update_all(None)
                .when_not_matched_insert_all();
            merge.execute(reader(batch)).await.map_err(lance_error)?;
            let _ = vector_dim; // schema dimension; merge_insert infers from the batch.
            // A projection first built on an empty store has no FTS indices —
            // there was nothing to index. Now there is, and search fails inside
            // LanceDB rather than returning nothing if they are still missing.
            ensure_fts_indices(&table).await?;
        }
        let status = ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Fresh,
            indexed_revision: Some(to.clone()),
            target_revision: None,
            fingerprint: self.embed_provider.as_ref().map(provider_fingerprint),
        };
        self.write_status(&status)?;
        Ok(status)
    }

    /// Return rows only when the projection exactly represents `revision`.
    ///
    /// # Errors
    ///
    /// Returns an error when the index is stale, unavailable, or malformed.
    pub async fn records(&self, revision: &Revision) -> Result<Vec<ProjectedRecord>, IndexError> {
        let _lock = self.read_lock_async().await?;
        let status = self.read_status_unlocked()?;
        if status.state != ProjectionState::Fresh
            || status.indexed_revision.as_ref() != Some(revision)
        {
            return Err(IndexError::new(
                "projection is not fresh for the requested revision",
            ));
        }
        let table = self
            .connection()?
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lance_error)?;
        let batches = table
            .query()
            .execute()
            .await
            .map_err(lance_error)?
            .try_collect::<Vec<_>>()
            .await
            .map_err(lance_error)?;
        decode_batches(&batches)
    }

    /// FTS (and future hybrid) search with filters and pagination.
    ///
    /// The projection must be `Fresh` for `request.revision`; a lagging index
    /// is never silently served as current.
    ///
    /// # Errors
    ///
    /// Returns an error when the index is stale, the FTS query fails, or the
    /// table is missing.
    pub async fn search(&self, request: &SearchRequest) -> Result<SearchResult, IndexError> {
        let _lock = self.read_lock_async().await?;
        let status = self.read_status_unlocked()?;
        if status.state != ProjectionState::Fresh
            || status.indexed_revision.as_ref() != Some(&request.revision)
        {
            return Err(IndexError::new(
                "projection is not fresh for the requested revision",
            ));
        }
        let limit = request.limit.clamp(1, MAX_SEARCH_LIMIT);
        let offset = request.offset;
        let max_offset = MAX_SEARCH_LIMIT * 100;
        if offset > max_offset {
            return Err(IndexError::new("search offset exceeds maximum"));
        }
        let table = self
            .connection()?
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lance_error)?;

        let predicate = build_predicate(&request.filters);
        // At least `RESCUE_THRESHOLD` rows are fetched regardless of the page
        // requested. The vector channel fires when BM25 returns fewer than
        // that, so a fetch that grew with `offset` made the decision differ
        // between page 1 and page 2 of the same query — and two pages built
        // from differently-ranked lists overlap.
        let fetch = (limit + offset + 1).max(RESCUE_THRESHOLD);

        let mut query = table
            .query()
            .full_text_search(FullTextSearchQuery::new(request.query.clone()))
            .limit(fetch);
        if let Some(ref sql) = predicate.sql {
            query = query.only_if(sql);
        }
        let batches = query
            .execute()
            .await
            .map_err(lance_error)?
            .try_collect::<Vec<_>>()
            .await
            .map_err(lance_error)?;

        let mut hits = decode_search_hits(&batches)?;
        retain_in_memory(&mut hits, &request.filters, &predicate);

        // Part of a word is still a word somebody typed. BM25 matches whole
        // terms — `arch` does not find `architecture`, which reads as the
        // search being broken rather than as a property of term matching — so a
        // thin result is widened by a substring pass before the meaning channel
        // is reached for. It runs on the same terms, against the same filters,
        // and appends: an exact term match stays above a fragment of one.
        if hits.len() < RESCUE_THRESHOLD {
            let found: std::collections::BTreeSet<String> =
                hits.iter().map(|hit| hit.id.clone()).collect();
            let mut widened = self.substring_rescue(&table, request, &predicate).await?;
            widened.retain(|hit| !found.contains(&hit.id));
            hits.append(&mut widened);
        }

        let fts_count = hits.len();
        let mut mode = SearchMode::Fts;
        let degraded = self.embed_provider.is_none();

        // The vector channel only fires when BM25 came back thin, and only
        // against an index built by this exact model.
        if let Some(ref provider) = self.embed_provider
            && fts_count < RESCUE_THRESHOLD
        {
            let active_fingerprint = provider_fingerprint(provider);
            if status.fingerprint.as_deref() == Some(active_fingerprint.as_str()) {
                let vec_hits = self
                    .vector_rescue(&table, request, &predicate, provider)
                    .await?;
                if !vec_hits.is_empty() {
                    mode = SearchMode::Hybrid;
                    hits = rrf_fuse(hits, vec_hits);
                }
            } else {
                tracing::warn!(
                    "projection fingerprint does not match the active model — vector rescue skipped"
                );
            }
        }

        let filtered_total = hits.len();
        let has_more = filtered_total > limit + offset;
        let page: Vec<SearchHit> = hits.into_iter().skip(offset).take(limit).collect();
        Ok(SearchResult {
            hits: page,
            total: filtered_total,
            limit,
            offset,
            has_more,
            mode,
            degraded,
            revision: request.revision.clone(),
        })
    }

    /// Find records holding the query's terms inside a word.
    ///
    /// A scan rather than an index read, and deliberately the second thing
    /// tried: the inverted index answers whole terms in microseconds, and this
    /// only runs when that came back with almost nothing. Every term has to
    /// appear — in the title or in the body — so two words narrow rather than
    /// widen, which is what typing a second word is for.
    ///
    /// Terms that cannot be written as a SQL literal are dropped rather than
    /// escaped: this is a convenience over the index, and a query full of
    /// punctuation is one the index is better at anyway.
    async fn substring_rescue(
        &self,
        table: &lancedb::Table,
        request: &SearchRequest,
        predicate: &Predicate,
    ) -> Result<Vec<SearchHit>, IndexError> {
        let mut clauses: Vec<String> = Vec::new();
        for term in request.query.split_whitespace() {
            let lowered = term.to_lowercase();
            let Some(literal) = sql_like_literal(&lowered) else {
                return Ok(Vec::new());
            };
            clauses.push(format!(
                "(contains(lower(title), {literal}) OR contains(lower(content), {literal}))"
            ));
        }
        if clauses.is_empty() {
            return Ok(Vec::new());
        }
        if let Some(ref sql) = predicate.sql {
            clauses.push(format!("({sql})"));
        }

        let batches = table
            .query()
            .only_if(clauses.join(" AND "))
            .limit(request.limit + request.offset + 1)
            .execute()
            .await
            .map_err(lance_error)?
            .try_collect::<Vec<_>>()
            .await
            .map_err(lance_error)?;

        let mut hits = decode_search_hits(&batches)?;
        retain_in_memory(&mut hits, &request.filters, predicate);
        Ok(hits)
    }

    /// Run the vector kNN channel for a query whose BM25 result was thin.
    ///
    /// Hits below [`VECTOR_RESCUE_FLOOR`] are dropped by
    /// [`decode_vector_hits`]; the in-memory filters repeat here because this
    /// channel produces its own rows, not a subset of the BM25 ones.
    async fn vector_rescue(
        &self,
        table: &lancedb::Table,
        request: &SearchRequest,
        predicate: &Predicate,
        provider: &Arc<dyn EmbeddingProvider>,
    ) -> Result<Vec<SearchHit>, IndexError> {
        let query_text = apply_prefix(provider.query_prefix(), &request.query);
        let query_vector = provider
            .embed(&[query_text])
            .await
            .map_err(|error| IndexError::new(format!("embed query: {error}")))?
            .into_iter()
            .next()
            .ok_or_else(|| IndexError::new("embed query returned no vector"))?;
        let mut query = table
            .query()
            .nearest_to(query_vector)
            .map_err(lance_error)?
            .distance_type(DistanceType::Cosine)
            .limit(VECTOR_FETCH);
        if let Some(ref sql) = predicate.sql {
            query = query.only_if(sql);
        }
        let batches = query
            .execute()
            .await
            .map_err(lance_error)?
            .try_collect::<Vec<_>>()
            .await
            .map_err(lance_error)?;
        let mut hits = decode_vector_hits(&batches)?;
        // The whole field, before anything is dropped: what separates a query
        // the model understood from one it did not is the shape of this list,
        // and it is worth being able to read it from a log rather than from a
        // rebuild with a print in it.
        if tracing::enabled!(tracing::Level::DEBUG) {
            let scores: Vec<String> = hits
                .iter()
                .filter_map(|hit| hit.vector_score)
                .map(|score| format!("{score:.3}"))
                .collect();
            tracing::debug!(
                query = %request.query,
                candidates = hits.len(),
                scores = %scores.join(" "),
                "vector rescue candidates"
            );
        }
        hits.retain(|hit| {
            hit.vector_score
                .is_some_and(|score| score >= VECTOR_RESCUE_FLOOR)
        });
        retain_in_memory(&mut hits, &request.filters, predicate);
        Ok(hits)
    }

    /// Bring the projection to the store's current revision,
    /// which is what every read serves. Interrupted or corrupt derived state is
    /// rebuilt automatically from Git.
    ///
    /// # Errors
    ///
    /// Returns an error only when both incremental repair and rebuild fail.
    pub async fn synchronize(
        &self,
        store: &dyn RecordStore,
    ) -> Result<ProjectionStatus, IndexError> {
        let snapshot = StoreView::current(store).map_err(store_error)?;
        let expected_fingerprint = self.embed_provider.as_ref().map(provider_fingerprint);
        match self.status() {
            // A model change (or an index built before any model was
            // available) invalidates every stored vector. Mixing generations
            // is what the fingerprint exists to prevent, so rebuild from Git
            // rather than serve a half-vector index.
            Ok(status)
                if expected_fingerprint.is_some() && status.fingerprint != expected_fingerprint =>
            {
                self.recover(&snapshot).await
            }
            Ok(status)
                if status.state == ProjectionState::Fresh
                    && status.indexed_revision.as_ref() == Some(snapshot.revision()) =>
            {
                match self.records(snapshot.revision()).await {
                    Ok(rows) => match self.verify_fts(rows.is_empty()).await {
                        Ok(()) => Ok(status),
                        Err(_) => self.recover(&snapshot).await,
                    },
                    Err(_) => self.recover(&snapshot).await,
                }
            }
            Ok(status)
                if status.state == ProjectionState::Fresh && status.indexed_revision.is_some() =>
            {
                let Some(from) = status.indexed_revision.as_ref() else {
                    return self.recover(&snapshot).await;
                };
                match self.update(store, from, snapshot.revision()).await {
                    Ok(status) => Ok(status),
                    Err(_) => self.recover(&snapshot).await,
                }
            }
            Ok(_) | Err(_) => self.recover(&snapshot).await,
        }
    }

    /// Mark unreadable derived state corrupt, delete it, and rebuild from Git.
    ///
    /// # Errors
    ///
    /// Returns an error when cleanup, catalog reopening, or rebuilding fails.
    pub async fn recover(&self, snapshot: &StoreView<'_>) -> Result<ProjectionStatus, IndexError> {
        let _lock = self.write_lock_async().await?;
        self.write_status(&ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Corrupt,
            indexed_revision: None,
            target_revision: Some(snapshot.revision().clone()),
            fingerprint: None,
        })?;
        let lance = self.root.join("lance");
        if lance.exists() {
            fs::remove_dir_all(&lance)?;
        }
        fs::create_dir_all(&lance)?;
        let replacement = Self::open(&self.root).await?;
        let connection = replacement.connection()?;
        *self
            .connection
            .write()
            .map_err(|_| IndexError::new("projection connection lock is poisoned"))? = connection;
        self.rebuild_unlocked(snapshot).await
    }

    /// Rebuild the projection from envelopes the caller already holds.
    ///
    /// For a store whose records are read as envelopes rather than walked from
    /// a snapshot — a folder of files is the one that does — the caller passes
    /// the `(key, envelope)` pairs it read instead of having them read twice.
    ///
    /// # Errors
    ///
    /// Returns an error when the catalog or derived table fails.
    pub async fn rebuild_from_envelopes(
        &self,
        records: &[(String, Envelope)],
        revision: &Revision,
    ) -> Result<ProjectionStatus, IndexError> {
        let _lock = self.write_lock_async().await?;
        self.write_status(&ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Rebuilding,
            indexed_revision: self
                .read_status_unlocked()
                .ok()
                .and_then(|s| s.indexed_revision),
            target_revision: Some(revision.clone()),
            fingerprint: self.embed_provider.as_ref().map(provider_fingerprint),
        })?;
        let (batch, vector_dim) = build_batch_from_envelopes(
            records,
            self.embed_provider.as_ref(),
            self.content_resolver.as_ref(),
        )
        .await?;
        let connection = self.connection()?;
        let names = connection
            .table_names()
            .execute()
            .await
            .map_err(lance_error)?;
        if names.iter().any(|name| name == TABLE) {
            connection
                .drop_table(TABLE, &[])
                .await
                .map_err(lance_error)?;
        }
        let schema = match vector_dim {
            Some(dim) => schema_with_vector(dim),
            None => schema(),
        };
        if batch.num_rows() == 0 {
            connection
                .create_empty_table(TABLE, schema)
                .execute()
                .await
                .map_err(lance_error)?;
        } else {
            connection
                .create_table(TABLE, reader(batch))
                .execute()
                .await
                .map_err(lance_error)?;
        }
        if !records.is_empty() {
            let table = connection
                .open_table(TABLE)
                .execute()
                .await
                .map_err(lance_error)?;
            for column in ["title", "content", "kind"] {
                table
                    .create_index(
                        &[column],
                        LanceIndex::FTS(FtsIndexBuilder::default().with_position(true)),
                    )
                    .execute()
                    .await
                    .map_err(lance_error)?;
            }
        }
        let status = ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Fresh,
            indexed_revision: Some(revision.clone()),
            target_revision: None,
            fingerprint: self.embed_provider.as_ref().map(provider_fingerprint),
        };
        self.write_status(&status)?;
        Ok(status)
    }

    /// Synchronous wrapper: rebuild the default per-repository projection
    /// from decrypted envelopes.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime, catalog, or rebuild fails.
    pub fn rebuild_from_envelopes_store(
        store: &dyn RecordStore,
        records: &[(String, Envelope)],
        revision: &Revision,
    ) -> Result<ProjectionStatus, IndexError> {
        Self::rebuild_from_envelopes_store_with(store, records, revision, None, None)
    }

    /// Synchronous wrapper: rebuild the default per-repository projection
    /// from decrypted envelopes with an optional embedding provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime, catalog, or rebuild fails.
    pub fn rebuild_from_envelopes_store_with(
        store: &dyn RecordStore,
        records: &[(String, Envelope)],
        revision: &Revision,
        provider: Option<Arc<dyn EmbeddingProvider>>,
        content: Option<Arc<dyn ContentResolver>>,
    ) -> Result<ProjectionStatus, IndexError> {
        run_off_thread(|| async {
            let projection = Self::open_store(store, provider, content).await?;
            projection.rebuild_from_envelopes(records, revision).await
        })
    }

    fn status_path(&self) -> PathBuf {
        self.root.join("status.json")
    }

    fn connection(&self) -> Result<Connection, IndexError> {
        self.connection
            .read()
            .map(|connection| connection.clone())
            .map_err(|_| IndexError::new("projection connection lock is poisoned"))
    }

    fn read_lock(&self) -> Result<File, IndexError> {
        lock_at(&self.root, false)
    }

    async fn read_lock_async(&self) -> Result<File, IndexError> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || lock_at(&root, false))
            .await
            .map_err(|error| IndexError::new(format!("projection lock worker failed: {error}")))?
    }

    async fn write_lock_async(&self) -> Result<File, IndexError> {
        let root = self.root.clone();
        tokio::task::spawn_blocking(move || lock_at(&root, true))
            .await
            .map_err(|error| IndexError::new(format!("projection lock worker failed: {error}")))?
    }

    async fn verify_fts(&self, empty: bool) -> Result<(), IndexError> {
        if empty {
            return Ok(());
        }
        let table = self
            .connection()?
            .open_table(TABLE)
            .execute()
            .await
            .map_err(lance_error)?;
        let indices = table.list_indices().await.map_err(lance_error)?;
        for expected in FTS_COLUMNS {
            let index = indices
                .iter()
                .find(|index| index.columns == [(*expected).to_owned()])
                .ok_or_else(|| {
                    IndexError::new(format!("projection FTS index for `{expected}` is missing"))
                })?;
            if table
                .index_stats(&index.name)
                .await
                .map_err(lance_error)?
                .is_none()
            {
                return Err(IndexError::new(format!(
                    "projection FTS index for `{expected}` is unreadable"
                )));
            }
        }
        Ok(())
    }

    fn read_status_unlocked(&self) -> Result<ProjectionStatus, IndexError> {
        read_status_at(&self.root)
    }

    fn write_status(&self, status: &ProjectionStatus) -> Result<(), IndexError> {
        let temporary = self.root.join("status.json.tmp");
        let bytes = serde_json::to_vec(status)
            .map_err(|error| IndexError::new(format!("serialize projection status: {error}")))?;
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, self.status_path())?;
        Ok(())
    }
}

fn read_status_at(root: &Path) -> Result<ProjectionStatus, IndexError> {
    match fs::read(root.join("status.json")) {
        Ok(bytes) => {
            let status: ProjectionStatus = serde_json::from_slice(&bytes).map_err(|error| {
                IndexError::new(format!("projection status is corrupt: {error}"))
            })?;
            if status.schema_version != META_SCHEMA {
                return Err(IndexError::new("projection status schema is unsupported"));
            }
            Ok(status)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Lagging,
            indexed_revision: None,
            target_revision: None,
            fingerprint: None,
        }),
        Err(error) => Err(error.into()),
    }
}

fn schema() -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, true),
        Field::new("title", DataType::Utf8, true),
        Field::new("content", DataType::Utf8, true),
        Field::new("archived", DataType::Boolean, false),
        Field::new("freshness", DataType::Utf8, true),
        Field::new("tags", DataType::Utf8, true),
        Field::new("folder", DataType::Utf8, true),
        Field::new("presence", DataType::Utf8, true),
        Field::new("content_kind", DataType::Utf8, true),
        Field::new("record_json", DataType::Utf8, false),
    ]))
}

fn schema_with_vector(dim: usize) -> SchemaRef {
    Arc::new(Schema::new(vec![
        Field::new("id", DataType::Utf8, false),
        Field::new("kind", DataType::Utf8, true),
        Field::new("title", DataType::Utf8, true),
        Field::new("content", DataType::Utf8, true),
        Field::new("archived", DataType::Boolean, false),
        Field::new("freshness", DataType::Utf8, true),
        Field::new("tags", DataType::Utf8, true),
        Field::new("folder", DataType::Utf8, true),
        Field::new("presence", DataType::Utf8, true),
        Field::new("content_kind", DataType::Utf8, true),
        Field::new("record_json", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                // Dimensions come from the model registry (hundreds), so the
                // clamp is unreachable; it keeps the cast total.
                i32::try_from(dim).unwrap_or(i32::MAX),
            ),
            true,
        ),
        Field::new("content_hash", DataType::Utf8, true),
    ]))
}

struct ProjectionRow {
    id: String,
    kind: Option<String>,
    title: Option<String>,
    /// What the full-text channel searches. For a record that points at a file
    /// this is the file's text, read through the resolver — otherwise an
    /// attached folder would be searchable by nothing but its type.
    content: Option<String>,
    /// `text` or `binary` for a record that points at a file, absent for one
    /// that carries its own content. A document that is not text cannot be
    /// searched by its words, and saying so is better than leaving it looking
    /// like an empty document.
    content_kind: Option<&'static str>,
    archived: bool,
    freshness: Option<String>,
    tags: Option<String>,
    folder: Option<String>,
    presence: Option<&'static str>,
    record_json: String,
    render_text: String,
    vector: Option<Vec<f32>>,
    content_hash: Option<String>,
}

impl ProjectionRow {
    fn from_record(
        id: &RecordId,
        record: &StoredRecord,
        content: Option<&Arc<dyn ContentResolver>>,
    ) -> Result<Self, IndexError> {
        let record_json = serde_json::to_string(record)
            .map_err(|error| IndexError::new(format!("serialize projected record: {error}")))?;
        let StoredRecord::Plaintext { envelope } = record;
        let mut row = Self::from_envelope(&id.display_value(), envelope, content)?;
        row.record_json = record_json;
        Ok(row)
    }

    fn from_envelope(
        key: &str,
        envelope: &Envelope,
        content: Option<&Arc<dyn ContentResolver>>,
    ) -> Result<Self, IndexError> {
        let (body, content_kind) = resolved_body(envelope, content);
        // What is embedded is what is searched. Rendering the record's own
        // empty body would give every document of an attached folder the same
        // vector, which is worse than no vector at all.
        let render_text = match &body {
            Some(body) => render_envelope_inner(&with_content(envelope, body)),
            None => render_envelope_inner(envelope),
        };
        Ok(Self {
            id: key.to_owned(),
            kind: Some(envelope.kind.clone()),
            title: envelope.title.clone(),
            content: body.or_else(|| Some(envelope.content.clone())),
            content_kind,
            archived: envelope.archive.archived,
            freshness: Some(format!("{:?}", envelope.freshness.state).to_ascii_lowercase()),
            tags: Some(encode_tags(&envelope.tags)),
            folder: envelope.folder.clone(),
            presence: presence_of(envelope),
            record_json: serde_json::to_string(envelope)
                .map_err(|error| IndexError::new(format!("serialize projected record: {error}")))?,
            render_text,
            vector: None,
            content_hash: None,
        })
    }
}

/// The body an index row carries, and what kind of body it is.
///
/// A record that holds its own content answers `(None, None)` — the caller
/// falls back to what the record holds, and the question of text-or-binary does
/// not arise for something the envelope validator already required to be a
/// string.
fn resolved_body(
    envelope: &Envelope,
    content: Option<&Arc<dyn ContentResolver>>,
) -> (Option<String>, Option<&'static str>) {
    let Some(reference) = &envelope.content_ref else {
        return (None, None);
    };
    let Some(resolver) = content else {
        return (Some(String::new()), None);
    };
    match resolver.resolve(&reference.path) {
        ResolvedContent::Text(text) => (Some(text), Some("text")),
        ResolvedContent::Binary => (Some(String::new()), Some("binary")),
        ResolvedContent::Missing => (Some(String::new()), None),
    }
}

/// The same envelope with a body, for rendering only.
fn with_content(envelope: &Envelope, content: &str) -> Envelope {
    let mut rendered = envelope.clone();
    content.clone_into(&mut rendered.content);
    rendered
}

async fn build_batch_from_records(
    records: &[(RecordId, StoredRecord)],
    provider: Option<&Arc<dyn EmbeddingProvider>>,
    content: Option<&Arc<dyn ContentResolver>>,
) -> Result<(RecordBatch, Option<usize>), IndexError> {
    let mut rows = records
        .iter()
        .map(|(id, record)| ProjectionRow::from_record(id, record, content))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(provider) = provider {
        let dim = embed_rows(provider, &mut rows).await?;
        Ok((batch_from_rows(&rows, Some(dim))?, Some(dim)))
    } else {
        Ok((batch_from_rows(&rows, None)?, None))
    }
}

async fn build_batch_from_envelopes(
    records: &[(String, Envelope)],
    provider: Option<&Arc<dyn EmbeddingProvider>>,
    content: Option<&Arc<dyn ContentResolver>>,
) -> Result<(RecordBatch, Option<usize>), IndexError> {
    let mut rows = records
        .iter()
        .map(|(key, envelope)| ProjectionRow::from_envelope(key, envelope, content))
        .collect::<Result<Vec<_>, _>>()?;
    if let Some(provider) = provider {
        let dim = embed_rows(provider, &mut rows).await?;
        Ok((batch_from_rows(&rows, Some(dim))?, Some(dim)))
    } else {
        Ok((batch_from_rows(&rows, None)?, None))
    }
}

/// Embed the render text of each row in batches, attaching the resulting
/// vector and content hash. Returns the provider's output dimension.
async fn embed_rows(
    provider: &Arc<dyn EmbeddingProvider>,
    rows: &mut [ProjectionRow],
) -> Result<usize, IndexError> {
    let dim = provider.dimensions();
    let doc_prefix = provider.doc_prefix();
    let indices: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, row)| !row.render_text.is_empty())
        .map(|(i, _)| i)
        .collect();
    for chunk in indices.chunks(EMBED_BATCH) {
        let texts: Vec<String> = chunk
            .iter()
            .map(|&i| apply_prefix(doc_prefix, &rows[i].render_text))
            .collect();
        let vectors = provider
            .embed(&texts)
            .await
            .map_err(|error| IndexError::new(format!("embed records: {error}")))?;
        for (offset, &row_idx) in chunk.iter().enumerate() {
            if let Some(vector) = vectors.get(offset)
                && vector.len() == dim
            {
                rows[row_idx].vector = Some(vector.clone());
            }
            rows[row_idx].content_hash = Some(content_hash_of(&rows[row_idx].render_text));
        }
    }
    Ok(dim)
}

/// A record that holds its own content is always here; only one that points
/// somewhere else can fail to be. `None` means the question does not apply.
fn presence_of(envelope: &Envelope) -> Option<&'static str> {
    envelope
        .content_ref
        .as_ref()
        .map(|reference| reference.presence.as_str())
}

fn batch_from_rows(
    rows: &[ProjectionRow],
    vector_dim: Option<usize>,
) -> Result<RecordBatch, IndexError> {
    let schema = match vector_dim {
        Some(dim) => schema_with_vector(dim),
        None => schema(),
    };
    let mut columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|row| &row.id),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|row| row.kind.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|row| row.title.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|row| row.content.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(
            rows.iter()
                .map(|row| row.archived)
                .collect::<BooleanArray>(),
        ),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|row| row.freshness.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|row| row.tags.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter()
                .map(|row| row.folder.as_deref())
                .collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter().map(|row| row.presence).collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter().map(|row| row.content_kind).collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|row| &row.record_json),
        )),
    ];
    if let Some(dim) = vector_dim {
        columns.push(Arc::new(build_vector_array(rows, dim)));
        columns.push(Arc::new(StringArray::from(
            rows.iter()
                .map(|row| row.content_hash.as_deref())
                .collect::<Vec<_>>(),
        )));
    }
    RecordBatch::try_new(schema, columns)
        .map_err(|error| IndexError::new(format!("build projection batch: {error}")))
}

fn build_vector_array(rows: &[ProjectionRow], dim: usize) -> FixedSizeListArray {
    let mut builder = FixedSizeListBuilder::with_capacity(
        Float32Builder::new(),
        i32::try_from(dim).unwrap_or(i32::MAX),
        rows.len(),
    );
    for row in rows {
        match &row.vector {
            Some(vector) if vector.len() == dim => {
                for value in vector {
                    builder.values().append_value(*value);
                }
                builder.append(true);
            }
            _ => {
                for _ in 0..dim {
                    builder.values().append_null();
                }
                builder.append(false);
            }
        }
    }
    builder.finish()
}

fn reader(batch: RecordBatch) -> Box<dyn arrow_array::RecordBatchReader + Send> {
    let schema = batch.schema();
    Box::new(RecordBatchIterator::new(vec![Ok(batch)], schema))
}

fn decode_batches(batches: &[RecordBatch]) -> Result<Vec<ProjectedRecord>, IndexError> {
    let mut rows = Vec::new();
    for batch in batches {
        let strings = |name: &str| -> Result<&StringArray, IndexError> {
            batch
                .column_by_name(name)
                .and_then(|value| value.as_any().downcast_ref())
                .ok_or_else(|| IndexError::new(format!("projection column `{name}` is corrupt")))
        };
        let ids = strings("id")?;
        let kinds = strings("kind")?;
        let titles = strings("title")?;
        let contents = strings("content")?;
        let freshness = strings("freshness")?;
        let tags_col = batch
            .column_by_name("tags")
            .and_then(|value| value.as_any().downcast_ref::<StringArray>());
        let archived = batch
            .column_by_name("archived")
            .and_then(|value| value.as_any().downcast_ref::<BooleanArray>())
            .ok_or_else(|| IndexError::new("projection column `archived` is corrupt"))?;
        for row in 0..batch.num_rows() {
            let optional =
                |array: &StringArray| (!array.is_null(row)).then(|| array.value(row).to_owned());
            let tags = tags_col
                .and_then(|array| (!array.is_null(row)).then(|| decode_tags(array.value(row))))
                .unwrap_or_default();
            rows.push(ProjectedRecord {
                id: ids.value(row).to_owned(),
                kind: optional(kinds),
                title: optional(titles),
                content: optional(contents),
                archived: archived.value(row),
                freshness: optional(freshness),
                tags,
            });
        }
    }
    rows.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(rows)
}

fn decode_search_hits(batches: &[RecordBatch]) -> Result<Vec<SearchHit>, IndexError> {
    let mut hits = Vec::new();
    for batch in batches {
        let strings = |name: &str| -> Result<&StringArray, IndexError> {
            batch
                .column_by_name(name)
                .and_then(|value| value.as_any().downcast_ref())
                .ok_or_else(|| IndexError::new(format!("search column `{name}` is corrupt")))
        };
        let ids = strings("id")?;
        let kinds = strings("kind")?;
        let titles = strings("title")?;
        let contents = strings("content")?;
        let freshness = strings("freshness")?;
        let optional_column = |name: &str| -> Option<&StringArray> {
            batch
                .column_by_name(name)
                .and_then(|value| value.as_any().downcast_ref::<StringArray>())
        };
        let folders = optional_column("folder");
        let presences = optional_column("presence");
        let content_kinds = optional_column("content_kind");
        let tags_col = batch
            .column_by_name("tags")
            .and_then(|value| value.as_any().downcast_ref::<StringArray>());
        let archived = batch
            .column_by_name("archived")
            .and_then(|value| value.as_any().downcast_ref::<BooleanArray>())
            .ok_or_else(|| IndexError::new("search column `archived` is corrupt"))?;
        // LanceDB adds `_score` for FTS results (BM25 score, Float32).
        let distance = batch
            .column_by_name("_distance")
            .or_else(|| batch.column_by_name("_score"))
            .and_then(|value| value.as_any().downcast_ref::<Float32Array>());
        for row in 0..batch.num_rows() {
            let optional =
                |array: &StringArray| (!array.is_null(row)).then(|| array.value(row).to_owned());
            let tags = tags_col
                .and_then(|array| (!array.is_null(row)).then(|| decode_tags(array.value(row))))
                .unwrap_or_default();
            let fts_score = distance
                .and_then(|array| (!array.is_null(row)).then(|| f64::from(array.value(row))));
            hits.push(SearchHit {
                id: ids.value(row).to_owned(),
                kind: optional(kinds),
                title: optional(titles),
                content: optional(contents),
                archived: archived.value(row),
                freshness: optional(freshness),
                tags,
                folder: folders.and_then(optional),
                presence: presences.and_then(optional),
                content_kind: content_kinds.and_then(optional),
                fts_score,
                vector_score: None,
                combined_rank: fts_score.unwrap_or(0.0),
                matched: MatchedBy::Words,
            });
        }
    }
    // Sort by combined rank descending (highest score first), then by id for determinism.
    hits.sort_by(|left, right| {
        let left_score = left.fts_score.unwrap_or(0.0);
        let right_score = right.fts_score.unwrap_or(0.0);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(hits)
}

/// Name of the lock file every projection reader and writer contends on. It
/// lives inside the projection directory but is never part of the derived
/// state, so a wipe keeps it: deleting it would hand two processes two
/// different inodes to lock and the mutual exclusion would silently stop
/// working.
const LOCK_FILE: &str = "projection.lock";

fn open_lock_file(root: &Path) -> Result<File, IndexError> {
    Ok(OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(LOCK_FILE))?)
}

/// Drive an async projection operation to completion from synchronous code.
///
/// The runtime is built on a dedicated scoped thread rather than the caller's.
/// Building it on the caller's thread panics with "cannot start a runtime from
/// within a runtime" whenever a synchronous facade is reached from inside an
/// async context — which is exactly what an embedding host does. Off-thread,
/// the same call simply blocks, which is what a synchronous signature already
/// promises. The thread is scoped, so the operation can still borrow the
/// caller's store and request.
fn run_off_thread<'scope, T, F, Fut>(operation: F) -> Result<T, IndexError>
where
    F: FnOnce() -> Fut + Send + 'scope,
    Fut: Future<Output = Result<T, IndexError>>,
    T: Send + 'scope,
{
    std::thread::scope(|scope| {
        scope
            .spawn(|| {
                let runtime = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map_err(|error| {
                        IndexError::new(format!("create projection runtime: {error}"))
                    })?;
                runtime.block_on(operation())
            })
            .join()
            .map_err(|_| IndexError::new("projection worker thread panicked"))?
    })
}

/// Columns the full-text channel searches. Each needs its own inverted index.
const FTS_COLUMNS: &[&str] = &["title", "content", "kind"];

/// Create every missing FTS index on a table that has rows to index.
///
/// Creating one that already exists is wasted work at best, so existing indices
/// are left alone. This is called from both the full rebuild and the
/// incremental update: the rebuild cannot create indices when it has no rows,
/// which makes the update the only place the first row can get them.
async fn ensure_fts_indices(table: &lancedb::Table) -> Result<(), IndexError> {
    let existing = table.list_indices().await.map_err(lance_error)?;
    for column in FTS_COLUMNS {
        if existing
            .iter()
            .any(|index| index.columns == [(*column).to_owned()])
        {
            continue;
        }
        table
            .create_index(
                &[column],
                LanceIndex::FTS(FtsIndexBuilder::default().with_position(true)),
            )
            .execute()
            .await
            .map_err(lance_error)?;
    }
    Ok(())
}

fn lock_at(root: &Path, exclusive: bool) -> Result<File, IndexError> {
    let file = open_lock_file(root)?;
    if exclusive {
        file.lock_exclusive()?;
    } else {
        file.lock_shared()?;
    }
    Ok(file)
}

/// Render a value as a SQL string literal, or refuse it.
///
/// The predicate handed to `LanceDB` is a SQL string, and the only safe way to
/// build one from caller input is to not build it at all when the input is not
/// obviously safe. A value passes when it is short and made of ordinary
/// identifier characters — no quotes, no backslashes, no control characters, so
/// nothing that could end the literal early or be re-interpreted by the parser.
/// Anything else is rejected here and filtered in memory instead, which is
/// slower but cannot change the meaning of the query.
/// A term as a SQL string literal for substring matching.
///
/// Stricter than it has to be on purpose: anything outside this set is left to
/// the inverted index rather than escaped into a scan, so there is no quoting
/// rule here that could be got wrong.
fn sql_like_literal(value: &str) -> Option<String> {
    let safe = !value.is_empty()
        && value.chars().count() <= 64
        && value
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '_' | '-' | '.'));
    safe.then(|| format!("'{value}'"))
}

fn sql_string_literal(value: &str) -> Option<String> {
    const MAX_LITERAL_CHARS: usize = 128;
    let safe = !value.is_empty()
        && value.chars().count() <= MAX_LITERAL_CHARS
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '/' | ':'));
    safe.then(|| format!("'{value}'"))
}

/// Encode tags into a delimited string for column storage.
/// Tags are joined with `TAGS_DELIMITER` (newline). Tags containing the
/// delimiter are sanitized by replacing it with a space, because the envelope
/// validator does not forbid newlines in tag strings.
fn encode_tags(tags: &[String]) -> String {
    let mut encoded = String::new();
    for tag in tags {
        encoded.push(TAGS_DELIMITER);
        encoded.push_str(&tag.replace(TAGS_DELIMITER, " "));
    }
    if !encoded.is_empty() {
        encoded.push(TAGS_DELIMITER);
    }
    encoded
}

fn decode_tags(encoded: &str) -> Vec<String> {
    encoded
        .split(TAGS_DELIMITER)
        .filter(|tag| !tag.is_empty())
        .map(str::to_owned)
        .collect()
}

/// A search filter split into what the store can evaluate and what this process
/// has to.
struct Predicate {
    /// SQL handed to `LanceDB`, or `None` when nothing could be expressed.
    sql: Option<String>,
    /// The kinds asked for, when at least one of them could not be expressed
    /// as a SQL literal. The whole set moves in-memory rather than the
    /// expressible part staying in SQL: a predicate naming two of three kinds
    /// would drop the third before the decoded hits were ever seen.
    residual_kinds: Vec<String>,
    /// The folder filter, and whether it reaches below, when it could not be
    /// expressed as a SQL literal either.
    residual_folder: Option<(String, bool)>,
}

/// Apply the filters the SQL predicate could not carry.
///
/// Tags never reach the predicate — they are stored as one delimited column —
/// and `kind` lands here only when it is not expressible as a SQL literal.
fn retain_in_memory(hits: &mut Vec<SearchHit>, filters: &SearchFilters, predicate: &Predicate) {
    if !predicate.residual_kinds.is_empty() {
        hits.retain(|hit| {
            hit.kind
                .as_deref()
                .is_some_and(|kind| predicate.residual_kinds.iter().any(|asked| asked == kind))
        });
    }
    if let Some((folder, subtree)) = &predicate.residual_folder {
        hits.retain(|hit| match hit.folder.as_deref() {
            Some(actual) => {
                actual == folder || (*subtree && actual.starts_with(&format!("{folder}/")))
            }
            None => false,
        });
    }
    if !filters.tags.is_empty() {
        hits.retain(|hit| {
            filters
                .tags
                .iter()
                .all(|tag| hit.tags.iter().any(|t| t == tag))
        });
    }
}

/// The kind that holds a type definition.
///
/// Spelled here rather than imported: the projection does not depend on the
/// schema crate and is not going to start, for the same reason it does not
/// depend on the store. The literal is duplicated on the same terms as the
/// freshness states below — a closed set, owned elsewhere, cheap to keep in
/// step and expensive to couple for.
const TYPE_KIND: &str = "__type__";

/// Every kind a request narrows to: the single `kind`, the `kinds` set, or
/// both. Deduplicated, because a caller naming the same kind twice means it
/// once and a repeated literal would be in the SQL for nobody's benefit.
fn asked_kinds(filters: &SearchFilters) -> Vec<String> {
    let mut asked: Vec<String> = Vec::new();
    for kind in filters.kind.iter().chain(filters.kinds.iter()) {
        if !asked.iter().any(|held| held == kind) {
            asked.push(kind.clone());
        }
    }
    asked
}

fn build_predicate(filters: &SearchFilters) -> Predicate {
    let mut parts: Vec<String> = Vec::new();
    let mut residual_kinds: Vec<String> = Vec::new();
    let mut residual_folder = None;
    let asked = asked_kinds(filters);
    if !asked.is_empty() {
        let literals: Option<Vec<String>> =
            asked.iter().map(|kind| sql_string_literal(kind)).collect();
        match literals {
            Some(literals) => parts.push(format!("kind IN ({})", literals.join(", "))),
            // One exotic kind takes the whole set in-memory: see `Predicate`.
            None => residual_kinds.clone_from(&asked),
        }
    }
    if !filters.include_service && !asked.iter().any(|kind| kind == TYPE_KIND) {
        // A null kind is a record from an index older than the column, and it
        // is not a type definition: SQL comparison against null is null, so it
        // has to be admitted explicitly or it disappears from every search.
        parts.push(format!("(kind IS NULL OR kind <> '{TYPE_KIND}')"));
    }
    if let Some(archived) = filters.archived {
        parts.push(format!("archived = {archived}"));
    }
    match filters.presence.as_deref().unwrap_or("present") {
        // A record with no locator has no presence recorded, and is here by
        // construction — so absence has to be stated, never inferred from a
        // null. Only the routine absence is hidden: a document deleted on the
        // branch that owns it is the one case somebody is asked about, and it
        // has to be visible to be asked about.
        "any" => {}
        "absent" => parts.push("(presence IS NOT NULL AND presence <> 'present')".to_owned()),
        // Anything else, including a name this build does not know, is the
        // default — the same reading `list_records` gives it, so one filter
        // cannot mean opposite things on the two paths.
        _ => parts.push("(presence IS NULL OR presence <> 'not_on_branch')".to_owned()),
    }
    if let Some(folder) = &filters.folder {
        // A prefix selection is a predicate the store evaluates, not a walk of
        // the corpus followed by a filter — otherwise paging a subtree returns
        // short pages.
        if folder.is_empty() {
            if !filters.folder_subtree {
                parts.push("folder IS NULL".to_owned());
            }
        } else if let Some(literal) = sql_string_literal(folder) {
            if filters.folder_subtree {
                // `%` is not an identifier character, so the literal rule
                // refuses it: the pattern is built from a folder that has
                // already passed that rule instead of being checked with the
                // wildcard attached.
                parts.push(format!("(folder = {literal} OR folder LIKE '{folder}/%')"));
            } else {
                parts.push(format!("folder = {literal}"));
            }
        } else {
            // A folder the literal rule cannot carry — anything outside ASCII,
            // and a documentation tree is full of those — is filtered in memory
            // rather than dropped. A filter that silently does not apply is
            // worse than one that refuses: the caller shows somebody else's
            // records as the contents of a folder.
            residual_folder = Some((folder.clone(), filters.folder_subtree));
        }
    }
    if !filters.freshness.is_empty() {
        // Freshness is a closed set, so an unknown state is a caller mistake
        // rather than something to filter in memory: it matches nothing.
        const VALID_FRESHNESS: &[&str] = &["unverified", "fresh", "stale", "invalid"];
        let values = filters
            .freshness
            .iter()
            .filter(|f| VALID_FRESHNESS.contains(&f.as_str()))
            .filter_map(|f| sql_string_literal(f))
            .collect::<Vec<_>>()
            .join(", ");
        if !values.is_empty() {
            parts.push(format!("freshness IN ({values})"));
        }
    }
    Predicate {
        sql: (!parts.is_empty()).then(|| parts.join(" AND ")),
        residual_kinds,
        residual_folder,
    }
}

/// Compute backlinks from a canonical snapshot.
///
/// Scans every record for:
/// - explicit `envelope.links` entries whose `key` matches `target_key`
/// - body-mention: `target_key` appears as a word-boundary substring of `envelope.content`
///
/// # Errors
///
/// Returns an error when the snapshot cannot be read.
pub fn compute_backlinks(
    snapshot: &StoreView<'_>,
    target_key: &str,
) -> Result<Vec<BacklinkEntry>, IndexError> {
    let records = snapshot.records().map_err(store_error)?;
    let envelopes = records
        .iter()
        .map(|(id, StoredRecord::Plaintext { envelope })| {
            (id.display_value(), envelope.as_ref().clone())
        })
        .collect::<Vec<_>>();
    Ok(compute_backlinks_from_envelopes(&envelopes, target_key))
}

/// Compute backlinks from envelopes the caller already holds.
///
/// For a caller that has already read the corpus — a rebuild, or a store whose
/// records did not come from a snapshot this crate can open. [`compute_backlinks`]
/// is the same computation reached from a snapshot instead.
#[must_use]
pub fn compute_backlinks_from_envelopes(
    records: &[(String, Envelope)],
    target_key: &str,
) -> Vec<BacklinkEntry> {
    let mut entries = Vec::new();
    for (key, envelope) in records {
        // Explicit links.
        for link in &envelope.links {
            if link.key == target_key {
                entries.push(BacklinkEntry {
                    source_id: key.clone(),
                    source_kind: Some(envelope.kind.clone()),
                    source_title: envelope.title.clone(),
                    relation: link.relation.clone(),
                    mention_type: MentionType::ExplicitLink,
                    source_presence: presence_of(envelope).map(str::to_owned),
                });
            }
        }
        // Body mentions.
        if contains_key_mention(&envelope.content, target_key) {
            entries.push(BacklinkEntry {
                source_id: key.clone(),
                source_kind: Some(envelope.kind.clone()),
                source_title: envelope.title.clone(),
                relation: None,
                mention_type: MentionType::BodyMention,
                source_presence: presence_of(envelope).map(str::to_owned),
            });
        }
    }
    // Sort first so duplicates from the same source are adjacent.
    entries.sort_by(|left, right| {
        left.source_id
            .cmp(&right.source_id)
            .then_with(|| left.mention_type.cmp(&right.mention_type))
    });
    // Deduplicate: when the same source has both explicit link and body mention,
    // drop the body mention (explicit link is more informative).
    // ExplicitLink sorts before BodyMention (derived from enum order), so the
    // first element of a consecutive pair is the explicit link — keep it.
    entries.dedup_by(|left, right| left.source_id == right.source_id);
    entries
}

/// Check whether `content` contains `key` as a word-boundary mention.
/// Word boundaries are non-alphanumeric characters (or string start/end).
///
/// Note: word-boundary checks use `u8::is_ascii_alphanumeric` on byte
/// offsets. This means multi-byte UTF-8 continuation bytes are always treated
/// as word boundaries, which is correct for keys consisting of ASCII
/// characters (the common case for record keys). Keys containing non-ASCII
/// characters may produce false-positive matches at byte boundaries that fall
/// inside a multi-byte sequence.
fn contains_key_mention(content: &str, key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    let mut start = 0;
    while let Some(pos) = content[start..].find(key) {
        let abs_pos = start + pos;
        let before_ok = abs_pos == 0
            || !content
                .as_bytes()
                .get(abs_pos - 1)
                .is_some_and(u8::is_ascii_alphanumeric);
        let after_pos = abs_pos + key.len();
        let after_ok = after_pos >= content.len()
            || !content
                .as_bytes()
                .get(after_pos)
                .is_some_and(u8::is_ascii_alphanumeric);
        if before_ok && after_ok {
            return true;
        }
        start = abs_pos + 1;
    }
    false
}
fn lance_error(error: impl fmt::Display) -> IndexError {
    IndexError::new(format!("LanceDB projection operation failed: {error}"))
}
fn store_error(error: impl fmt::Display) -> IndexError {
    IndexError::new(format!("canonical Memory Hub read failed: {error}"))
}

/// Derive the active fingerprint digest for a provider. Uses the provider's
/// `model_id` as the model digest — the projection layer does not have access
/// to the verified GGUF SHA-256, so the model id serves as a stable proxy.
fn provider_fingerprint(provider: &Arc<dyn EmbeddingProvider>) -> String {
    Fingerprint::from_provider(&**provider, provider.model_id()).digest()
}

/// Prepend a prefix (if present) to a text string.
fn apply_prefix(prefix: Option<&str>, text: &str) -> String {
    match prefix {
        Some(p) if !p.is_empty() => format!("{p}{text}"),
        _ => text.to_owned(),
    }
}

/// Decode vector kNN result batches into search hits.
///
/// Every candidate the store returned, floor included: whether the channel
/// found anything is decided against the whole field in [`Projection::
/// vector_rescue`], and a decoder that had already dropped the low end would
/// hide the shape that decision reads.
fn decode_vector_hits(batches: &[RecordBatch]) -> Result<Vec<SearchHit>, IndexError> {
    let mut hits = Vec::new();
    for batch in batches {
        let strings = |name: &str| -> Result<&StringArray, IndexError> {
            batch
                .column_by_name(name)
                .and_then(|value| value.as_any().downcast_ref())
                .ok_or_else(|| IndexError::new(format!("search column `{name}` is corrupt")))
        };
        let ids = strings("id")?;
        let kinds = strings("kind")?;
        let titles = strings("title")?;
        let contents = strings("content")?;
        let freshness = strings("freshness")?;
        let optional_column = |name: &str| -> Option<&StringArray> {
            batch
                .column_by_name(name)
                .and_then(|value| value.as_any().downcast_ref::<StringArray>())
        };
        let folders = optional_column("folder");
        let presences = optional_column("presence");
        let content_kinds = optional_column("content_kind");
        let tags_col = batch
            .column_by_name("tags")
            .and_then(|value| value.as_any().downcast_ref::<StringArray>());
        let archived = batch
            .column_by_name("archived")
            .and_then(|value| value.as_any().downcast_ref::<BooleanArray>())
            .ok_or_else(|| IndexError::new("search column `archived` is corrupt"))?;
        let distance = batch
            .column_by_name("_distance")
            .and_then(|value| value.as_any().downcast_ref::<Float32Array>());
        for row in 0..batch.num_rows() {
            let optional =
                |array: &StringArray| (!array.is_null(row)).then(|| array.value(row).to_owned());
            let tags = tags_col
                .and_then(|array| (!array.is_null(row)).then(|| decode_tags(array.value(row))))
                .unwrap_or_default();
            // Cosine distance: 0 = identical, 2 = opposite.
            // similarity = 1.0 - distance.
            let vector_score = distance.and_then(|array| {
                (!array.is_null(row)).then(|| {
                    let dist = f64::from(array.value(row));
                    1.0 - dist
                })
            });
            let Some(score) = vector_score else { continue };
            hits.push(SearchHit {
                id: ids.value(row).to_owned(),
                kind: optional(kinds),
                title: optional(titles),
                content: optional(contents),
                archived: archived.value(row),
                freshness: optional(freshness),
                tags,
                folder: folders.and_then(optional),
                presence: presences.and_then(optional),
                content_kind: content_kinds.and_then(optional),
                fts_score: None,
                vector_score: Some(score),
                combined_rank: score,
                matched: MatchedBy::Meaning,
            });
        }
    }
    hits.sort_by(|left, right| {
        let left_score = left.vector_score.unwrap_or(0.0);
        let right_score = right.vector_score.unwrap_or(0.0);
        right_score
            .partial_cmp(&left_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.id.cmp(&right.id))
    });
    Ok(hits)
}

/// One Reciprocal Rank Fusion term: `1 / (K + rank)`.
///
/// Ranks are list positions, far below the precision limit of `f64`, so the
/// conversion is exact for any result set a search can return.
#[allow(clippy::cast_precision_loss)]
fn rrf_term(rank: usize) -> f64 {
    1.0 / (RRF_K + rank) as f64
}

/// Reciprocal Rank Fusion of BM25 and vector hits.
///
/// `combined_rank = 1/(K+bm25_rank) + 1/(K+vec_rank)` where a hit absent from
/// one channel contributes nothing for that term. Higher is better.
fn rrf_fuse(fts_hits: Vec<SearchHit>, vec_hits: Vec<SearchHit>) -> Vec<SearchHit> {
    use std::collections::HashMap;
    let mut fts_rank: HashMap<String, (usize, SearchHit)> = HashMap::new();
    for (rank, hit) in fts_hits.into_iter().enumerate() {
        fts_rank.insert(hit.id.clone(), (rank, hit));
    }
    let mut vec_rank: HashMap<String, (usize, SearchHit)> = HashMap::new();
    for (rank, hit) in vec_hits.into_iter().enumerate() {
        vec_rank.insert(hit.id.clone(), (rank, hit));
    }
    let mut ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    ids.extend(fts_rank.keys().cloned());
    ids.extend(vec_rank.keys().cloned());
    let mut fused: Vec<SearchHit> = ids
        .into_iter()
        // The id set is the union of both channels, so at least one lookup
        // always succeeds; `filter_map` states that without a panic path.
        .filter_map(|id| {
            let fts = fts_rank.get(&id);
            let vec = vec_rank.get(&id);
            let mut hit = fts
                .map(|(_, h)| h.clone())
                .or_else(|| vec.map(|(_, h)| h.clone()))?;
            let fts_term = fts.map_or(0.0, |(rank, _)| rrf_term(*rank));
            let vec_term = vec.map_or(0.0, |(rank, _)| rrf_term(*rank));
            hit.fts_score = fts.and_then(|(_, h)| h.fts_score);
            hit.vector_score = vec.and_then(|(_, h)| h.vector_score);
            hit.combined_rank = fts_term + vec_term;
            hit.matched = match (fts.is_some(), vec.is_some()) {
                (true, true) => MatchedBy::Both,
                (true, false) => MatchedBy::Words,
                // The union is built from the two channels' ids, so a hit that
                // is in neither cannot reach this line.
                (false, _) => MatchedBy::Meaning,
            };
            Some(hit)
        })
        .collect();
    fused.sort_by(|left, right| {
        right
            .combined_rank
            .partial_cmp(&left.combined_rank)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.id.cmp(&right.id))
    });
    fused
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use memory_hub_core::{Envelope, StoredRecord};
    use memory_hub_store::{GitStore, Operation, Transaction};

    use super::{Projection, TABLE};

    #[tokio::test]
    async fn synchronize_rebuilds_when_fts_metadata_is_missing() {
        let project = tempfile::tempdir().unwrap();
        git2::Repository::init(project.path()).unwrap();
        let store = GitStore::open(project.path()).unwrap();
        let base = store.current().unwrap().revision().clone();
        store
            .apply(&Transaction {
                id: "fts-health".into(),
                expected_revision: base,
                operations: vec![Operation::put(StoredRecord::Plaintext {
                    envelope: Box::new(Envelope::new("fts", "note", "searchable").unwrap()),
                })],
            })
            .unwrap();
        let projection = Projection::open(project.path().join("index"))
            .await
            .unwrap();
        projection.synchronize(&store).await.unwrap();
        let table = projection
            .connection()
            .unwrap()
            .open_table(TABLE)
            .execute()
            .await
            .unwrap();
        for index in table.list_indices().await.unwrap() {
            table.drop_index(&index.name).await.unwrap();
        }

        projection.synchronize(&store).await.unwrap();
        let table = projection
            .connection()
            .unwrap()
            .open_table(TABLE)
            .execute()
            .await
            .unwrap();
        assert_eq!(table.list_indices().await.unwrap().len(), 3);
    }
}
