//! Recoverable `LanceDB` read model for generic Git Memory envelopes.
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
    StringArray, builder::{FixedSizeListBuilder, Float32Builder},
};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use fs2::FileExt;
use futures::TryStreamExt;
use git_memory_core::{Envelope, StoredRecord};
use git_memory_embed::{
    EmbeddingProvider, Fingerprint, content_hash_of, render_envelope, renderer::render_envelope_inner,
};
use git_memory_store::{ChangeKind, GitStore, RecordId, Revision, Snapshot};
use lancedb::connection::Connection;
use lancedb::index::Index as LanceIndex;
use lancedb::index::scalar::{FtsIndexBuilder, FullTextSearchQuery};
use lancedb::query::{ExecutableQuery, QueryBase};
use lancedb::DistanceType;
use serde::{Deserialize, Serialize};

const TABLE: &str = "records";
const META_SCHEMA: u32 = 2;
const TAGS_DELIMITER: char = '\n';
const MAX_SEARCH_LIMIT: usize = 200;
/// BM25 hits below this count trigger the vector rescue channel.
const RESCUE_THRESHOLD: usize = 5;
/// Minimum cosine similarity for a vector hit to survive the rescue floor.
const VECTOR_RESCUE_FLOOR: f64 = 0.35;
/// RRF fusion constant: `combined = 1/(K+rank_a) + 1/(K+rank_b)`.
const RRF_K: usize = 60;
/// Maximum vector candidates fetched before the rescue floor filter.
const VECTOR_FETCH: usize = 20;
/// Batch size for embedding during a full rebuild.
const EMBED_BATCH: usize = 128;

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
    pub canonical_revision: Option<Revision>,
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
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// `Some(true)` → only archived, `Some(false)` → only live, `None` → both.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived: Option<bool>,
    /// Empty vec → all freshness states.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub freshness: Vec<String>,
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
    /// BM25 score from FTS (higher is better). `None` when FTS did not match.
    pub fts_score: Option<f64>,
    /// Semantic similarity score from vector search (higher is better).
    /// `None` when vector search is unavailable or not run.
    pub vector_score: Option<f64>,
    /// Deterministic fusion of available channel scores (higher is better).
    pub combined_rank: f64,
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
}

impl Projection {
    /// Synchronize the default per-repository projection from synchronous
    /// adapters such as the MCP stdio server.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime, catalog, or synchronization fails.
    pub fn synchronize_store(store: &GitStore) -> Result<ProjectionStatus, IndexError> {
        Self::synchronize_store_with(store, None)
    }

    /// Synchronous entry point with an optional embedding provider attached.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime, catalog, or synchronization fails.
    pub fn synchronize_store_with(
        store: &GitStore,
        provider: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Result<ProjectionStatus, IndexError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| IndexError::new(format!("create projection runtime: {error}")))?;
        runtime.block_on(async {
            let mut projection = Self::open(store.git_dir().join("git-memory/index")).await?;
            if let Some(provider) = provider {
                projection = projection.with_embed_provider(provider);
            }
            projection.synchronize(store).await
        })
    }

    /// Read the default per-repository projection status without opening the
    /// `LanceDB` catalog.
    ///
    /// # Errors
    ///
    /// Returns an error when the lock or durable status cannot be read.
    pub fn status_store(store: &GitStore) -> Result<ProjectionStatus, IndexError> {
        let root = store.git_dir().join("git-memory/index");
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
        store: &GitStore,
        request: &SearchRequest,
    ) -> Result<SearchResult, IndexError> {
        Self::search_store_with(store, request, None)
    }

    /// Synchronous entry point for MCP: open the default per-repository
    /// projection, run a search with an optional embedding provider, and return
    /// the result.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime, catalog, or search fails.
    pub fn search_store_with(
        store: &GitStore,
        request: &SearchRequest,
        provider: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Result<SearchResult, IndexError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| IndexError::new(format!("create search runtime: {error}")))?;
        runtime.block_on(async {
            let mut projection = Self::open(store.git_dir().join("git-memory/index")).await?;
            if let Some(provider) = provider {
                projection = projection.with_embed_provider(provider);
            }
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
        store: &GitStore,
        revision: &Revision,
        key: &str,
    ) -> Result<Vec<BacklinkEntry>, IndexError> {
        let snapshot = store.snapshot(revision).map_err(store_error)?;
        compute_backlinks(&snapshot, key)
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
        })
    }

    /// Attach an embedding provider to enable vector-rescue hybrid search.
    /// The provider's fingerprint is derived lazily during rebuild/search.
    #[must_use]
    pub fn with_embed_provider(mut self, provider: Arc<dyn EmbeddingProvider>) -> Self {
        self.embed_provider = Some(provider);
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

    /// Recreate the projection solely from an immutable Git Memory snapshot.
    ///
    /// # Errors
    ///
    /// Returns an error when the canonical snapshot or derived catalog fails.
    pub async fn rebuild(&self, snapshot: &Snapshot) -> Result<ProjectionStatus, IndexError> {
        let _lock = self.write_lock_async().await?;
        self.rebuild_unlocked(snapshot).await
    }

    async fn rebuild_unlocked(&self, snapshot: &Snapshot) -> Result<ProjectionStatus, IndexError> {
        let target = snapshot.revision().clone();
        self.write_status(&ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Rebuilding,
            canonical_revision: self
                .read_status_unlocked()
                .ok()
                .and_then(|s| s.canonical_revision),
            target_revision: Some(target.clone()),
            fingerprint: self.embed_provider.as_ref().map(|p| provider_fingerprint(p)),
        })?;
        let records = snapshot.records().map_err(store_error)?;
        let (batch, vector_dim) = build_batch_from_records(&records, self.embed_provider.as_ref())
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
            canonical_revision: Some(target),
            target_revision: None,
            fingerprint: self.embed_provider.as_ref().map(|p| provider_fingerprint(p)),
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
        store: &GitStore,
        from: &Revision,
        to: &Revision,
    ) -> Result<ProjectionStatus, IndexError> {
        let _lock = self.write_lock_async().await?;
        let current = self.read_status_unlocked()?;
        if current.state != ProjectionState::Fresh
            || current.canonical_revision.as_ref() != Some(from)
        {
            return Err(IndexError::new(
                "projection revision does not match incremental base",
            ));
        }
        let target = store.snapshot(to).map_err(store_error)?;
        let delta = store.diff(from, to).map_err(store_error)?;
        self.write_status(&ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Lagging,
            canonical_revision: Some(from.clone()),
            target_revision: Some(to.clone()),
            fingerprint: self.embed_provider.as_ref().map(|p| provider_fingerprint(p)),
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
            let predicate = deleted
                .iter()
                .map(|id| format!("id = '{}'", sql_escape(id)))
                .collect::<Vec<_>>()
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
            let (batch, vector_dim) =
                build_batch_from_records(&updated_records, self.embed_provider.as_ref()).await?;
            let mut merge = table.merge_insert(&["id"]);
            merge
                .when_matched_update_all(None)
                .when_not_matched_insert_all();
            merge
                .execute(reader(batch))
                .await
                .map_err(lance_error)?;
            let _ = vector_dim; // schema dimension; merge_insert infers from the batch.
        }
        let status = ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Fresh,
            canonical_revision: Some(to.clone()),
            target_revision: None,
            fingerprint: self.embed_provider.as_ref().map(|p| provider_fingerprint(p)),
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
            || status.canonical_revision.as_ref() != Some(revision)
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
            || status.canonical_revision.as_ref() != Some(&request.revision)
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
        let fetch = limit + offset + 1;

        let mut query = table
            .query()
            .full_text_search(FullTextSearchQuery::new(request.query.clone()))
            .limit(fetch);
        if let Some(ref predicate) = predicate {
            query = query.only_if(predicate);
        }
        let batches = query
            .execute()
            .await
            .map_err(lance_error)?
            .try_collect::<Vec<_>>()
            .await
            .map_err(lance_error)?;

        let mut hits = decode_search_hits(&batches)?;
        if !request.filters.tags.is_empty() {
            hits.retain(|hit| {
                request
                    .filters
                    .tags
                    .iter()
                    .all(|tag| hit.tags.iter().any(|t| t == tag))
            });
        }

        let fts_count = hits.len();
        let mut mode = SearchMode::Fts;
        let degraded = self.embed_provider.is_none();

        if let Some(ref provider) = self.embed_provider {
            let active_fp = provider_fingerprint(provider);
            let fp_matches = status.fingerprint.as_deref() == Some(active_fp.as_str());
            if !fp_matches {
                eprintln!(
                    "git-memory: projection fingerprint mismatch — vector rescue skipped"
                );
            } else if fts_count < RESCUE_THRESHOLD {
                let query_text = apply_prefix(provider.query_prefix(), &request.query);
                let query_vectors = provider
                    .embed(&[query_text])
                    .await
                    .map_err(|error| IndexError::new(format!("embed query: {error}")))?;
                let query_vector = query_vectors
                    .into_iter()
                    .next()
                    .ok_or_else(|| IndexError::new("embed query returned no vector"))?;
                let mut vq = table
                    .query()
                    .nearest_to(query_vector)
                    .map_err(lance_error)?
                    .distance_type(DistanceType::Cosine)
                    .limit(VECTOR_FETCH);
                if let Some(ref predicate) = predicate {
                    vq = vq.only_if(predicate);
                }
                let v_batches = vq
                    .execute()
                    .await
                    .map_err(lance_error)?
                    .try_collect::<Vec<_>>()
                    .await
                    .map_err(lance_error)?;
                let mut vec_hits = decode_vector_hits(&v_batches)?;
                if !request.filters.tags.is_empty() {
                    vec_hits.retain(|hit| {
                        request
                            .filters
                            .tags
                            .iter()
                            .all(|tag| hit.tags.iter().any(|t| t == tag))
                    });
                }
                if !vec_hits.is_empty() {
                    mode = SearchMode::Hybrid;
                    hits = rrf_fuse(hits, vec_hits);
                }
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

    /// Bring the projection to the current canonical revision. Interrupted or
    /// corrupt derived state is rebuilt automatically from Git.
    ///
    /// # Errors
    ///
    /// Returns an error only when both incremental repair and rebuild fail.
    pub async fn synchronize(&self, store: &GitStore) -> Result<ProjectionStatus, IndexError> {
        let snapshot = store.current().map_err(store_error)?;
        match self.status() {
            Ok(status)
                if status.state == ProjectionState::Fresh
                    && status.canonical_revision.as_ref() == Some(snapshot.revision()) =>
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
                if status.state == ProjectionState::Fresh
                    && status.canonical_revision.is_some() =>
            {
                let Some(from) = status.canonical_revision.as_ref() else {
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
    pub async fn recover(&self, snapshot: &Snapshot) -> Result<ProjectionStatus, IndexError> {
        let _lock = self.write_lock_async().await?;
        self.write_status(&ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Corrupt,
            canonical_revision: None,
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

    /// Destroy the ephemeral projection — delete the LanceDB directory and
    /// status file so no plaintext persists on disk.
    ///
    /// Used by encrypted projects on `lock()`: the index is rebuilt from
    /// decrypted records on the next `unlock()`, so it must not survive on
    /// disk while locked.
    ///
    /// # Errors
    ///
    /// Returns an error when the filesystem delete fails.
    pub async fn destroy(&self) -> Result<(), IndexError> {
        let _lock = self.write_lock_async().await?;
        let lance = self.root.join("lance");
        if lance.exists() {
            fs::remove_dir_all(&lance)?;
        }
        let status = self.status_path();
        let _ = fs::remove_file(&status);
        let tmp = self.root.join("status.json.tmp");
        let _ = fs::remove_file(&tmp);
        Ok(())
    }

    /// Synchronous wrapper: destroy the default per-repository projection.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime or destroy fails.
    pub fn destroy_store(store: &GitStore) -> Result<(), IndexError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| IndexError::new(format!("create destroy runtime: {error}")))?;
        runtime.block_on(async {
            let projection = Self::open(store.git_dir().join("git-memory/index")).await?;
            projection.destroy().await
        })
    }

    /// Crash-safe destroy: remove the index directory directly without opening
    /// LanceDB. Used on encrypted-project start to wipe any plaintext left by a
    /// process that was killed before `memory_lock` could run. Unlike
    /// [`destroy_store`](Self::destroy_store), this never opens a catalog, so a
    /// corrupt or half-written index from a crash is removed rather than
    /// failing to open.
    ///
    /// # Errors
    ///
    /// Returns an error only when the directory exists but cannot be removed.
    pub fn destroy_store_silent(store: &GitStore) -> Result<(), IndexError> {
        let root = store.git_dir().join("git-memory/index");
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        Ok(())
    }

    /// Rebuild the projection from pre-decrypted envelopes.
    ///
    /// Used by encrypted projects on `unlock()`: the canonical Git snapshot
    /// contains encrypted blobs, so the standard `rebuild` (which reads
    /// plaintext records from the snapshot) cannot be used. Instead, the
    /// caller decrypts all records via `EncryptedStore::list()` and passes
    /// the resulting `(key, envelope)` pairs here.
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
            canonical_revision: self
                .read_status_unlocked()
                .ok()
                .and_then(|s| s.canonical_revision),
            target_revision: Some(revision.clone()),
            fingerprint: self.embed_provider.as_ref().map(|p| provider_fingerprint(p)),
        })?;
        let (batch, vector_dim) =
            build_batch_from_envelopes(records, self.embed_provider.as_ref()).await?;
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
            canonical_revision: Some(revision.clone()),
            target_revision: None,
            fingerprint: self.embed_provider.as_ref().map(|p| provider_fingerprint(p)),
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
        store: &GitStore,
        records: &[(String, Envelope)],
        revision: &Revision,
    ) -> Result<ProjectionStatus, IndexError> {
        Self::rebuild_from_envelopes_store_with(store, records, revision, None)
    }

    /// Synchronous wrapper: rebuild the default per-repository projection
    /// from decrypted envelopes with an optional embedding provider.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime, catalog, or rebuild fails.
    pub fn rebuild_from_envelopes_store_with(
        store: &GitStore,
        records: &[(String, Envelope)],
        revision: &Revision,
        provider: Option<Arc<dyn EmbeddingProvider>>,
    ) -> Result<ProjectionStatus, IndexError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| IndexError::new(format!("create rebuild runtime: {error}")))?;
        runtime.block_on(async {
            let mut projection = Self::open(store.git_dir().join("git-memory/index")).await?;
            if let Some(provider) = provider {
                projection = projection.with_embed_provider(provider);
            }
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
        for expected in ["title", "content", "kind"] {
            let index = indices
                .iter()
                .find(|index| index.columns == [expected.to_owned()])
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
            canonical_revision: None,
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
        Field::new("record_json", DataType::Utf8, false),
        Field::new(
            "vector",
            DataType::FixedSizeList(
                Arc::new(Field::new("item", DataType::Float32, true)),
                dim as i32,
            ),
            true,
        ),
        Field::new("content_hash", DataType::Utf8, true),
    ]))
}

struct ProjectionRow<'a> {
    id: String,
    kind: Option<&'a str>,
    title: Option<&'a str>,
    content: Option<&'a str>,
    archived: bool,
    freshness: Option<String>,
    tags: Option<String>,
    record_json: String,
    render_text: String,
    vector: Option<Vec<f32>>,
    content_hash: Option<String>,
}

impl<'a> ProjectionRow<'a> {
    fn from_record(id: &RecordId, record: &'a StoredRecord) -> Result<Self, IndexError> {
        let (kind, title, content, archived, freshness, tags, render_text) = match record {
            StoredRecord::Plaintext { envelope } => (
                Some(envelope.kind.as_str()),
                envelope.title.as_deref(),
                Some(envelope.content.as_str()),
                envelope.archive.archived,
                Some(format!("{:?}", envelope.freshness.state).to_ascii_lowercase()),
                Some(encode_tags(&envelope.tags)),
                render_envelope(record),
            ),
            StoredRecord::Encrypted { .. } => (None, None, None, false, None, None, String::new()),
        };
        Ok(Self {
            id: id.display_value(),
            kind,
            title,
            content,
            archived,
            freshness,
            tags,
            record_json: serde_json::to_string(record)
                .map_err(|error| IndexError::new(format!("serialize projected record: {error}")))?,
            render_text,
            vector: None,
            content_hash: None,
        })
    }

    fn from_envelope(key: &str, envelope: &'a Envelope) -> Result<Self, IndexError> {
        let render_text = render_envelope_inner(envelope);
        Ok(Self {
            id: key.to_owned(),
            kind: Some(envelope.kind.as_str()),
            title: envelope.title.as_deref(),
            content: Some(envelope.content.as_str()),
            archived: envelope.archive.archived,
            freshness: Some(format!("{:?}", envelope.freshness.state).to_ascii_lowercase()),
            tags: Some(encode_tags(&envelope.tags)),
            record_json: serde_json::to_string(envelope)
                .map_err(|error| IndexError::new(format!("serialize projected record: {error}")))?,
            render_text,
            vector: None,
            content_hash: None,
        })
    }
}

async fn build_batch_from_records(
    records: &[(RecordId, StoredRecord)],
    provider: Option<&Arc<dyn EmbeddingProvider>>,
) -> Result<(RecordBatch, Option<usize>), IndexError> {
    let mut rows = records
        .iter()
        .map(|(id, record)| ProjectionRow::from_record(id, record))
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
) -> Result<(RecordBatch, Option<usize>), IndexError> {
    let mut rows = records
        .iter()
        .map(|(key, envelope)| ProjectionRow::from_envelope(key, envelope))
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
    rows: &mut [ProjectionRow<'_>],
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
            if let Some(vector) = vectors.get(offset) {
                if vector.len() == dim {
                    rows[row_idx].vector = Some(vector.clone());
                }
            }
            rows[row_idx].content_hash = Some(content_hash_of(&rows[row_idx].render_text));
        }
    }
    Ok(dim)
}

fn batch_from_rows(rows: &[ProjectionRow<'_>], vector_dim: Option<usize>) -> Result<RecordBatch, IndexError> {
    let schema = match vector_dim {
        Some(dim) => schema_with_vector(dim),
        None => schema(),
    };
    let mut columns: Vec<Arc<dyn Array>> = vec![
        Arc::new(StringArray::from_iter_values(
            rows.iter().map(|row| &row.id),
        )),
        Arc::new(StringArray::from(
            rows.iter().map(|row| row.kind).collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter().map(|row| row.title).collect::<Vec<_>>(),
        )),
        Arc::new(StringArray::from(
            rows.iter().map(|row| row.content).collect::<Vec<_>>(),
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

fn build_vector_array(rows: &[ProjectionRow<'_>], dim: usize) -> FixedSizeListArray {
    let mut builder = FixedSizeListBuilder::with_capacity(
        Float32Builder::new(),
        dim as i32,
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
            let fts_score = distance.and_then(|array| {
                (!array.is_null(row)).then(|| f64::from(array.value(row)))
            });
            hits.push(SearchHit {
                id: ids.value(row).to_owned(),
                kind: optional(kinds),
                title: optional(titles),
                content: optional(contents),
                archived: archived.value(row),
                freshness: optional(freshness),
                tags,
                fts_score,
                vector_score: None,
                combined_rank: fts_score.unwrap_or(0.0),
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

fn lock_at(root: &Path, exclusive: bool) -> Result<File, IndexError> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join("projection.lock"))?;
    if exclusive {
        file.lock_exclusive()?;
    } else {
        file.lock_shared()?;
    }
    Ok(file)
}

fn sql_escape(value: &str) -> String {
    value.replace('\'', "''")
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

fn build_predicate(filters: &SearchFilters) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(ref kind) = filters.kind {
        parts.push(format!("kind = '{}'", sql_escape(kind)));
    }
    if let Some(archived) = filters.archived {
        parts.push(format!("archived = {archived}"));
    }
    if !filters.freshness.is_empty() {
        // Only allow known freshness states to prevent SQL injection through
        // arbitrary string values from non-MCP callers.
        const VALID_FRESHNESS: &[&str] = &["unverified", "fresh", "stale", "invalid"];
        let values = filters
            .freshness
            .iter()
            .filter(|f| VALID_FRESHNESS.contains(&f.as_str()))
            .map(|f| format!("'{}'", sql_escape(f)))
            .collect::<Vec<_>>()
            .join(", ");
        if !values.is_empty() {
            parts.push(format!("freshness IN ({values})"));
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" AND "))
    }
}

/// Compute backlinks from a canonical snapshot.
///
/// Scans every plaintext record for:
/// - explicit `envelope.links` entries whose `key` matches `target_key`
/// - body-mention: `target_key` appears as a word-boundary substring of `envelope.content`
///
/// Encrypted records are opaque and never produce backlinks.
///
/// # Errors
///
/// Returns an error when the snapshot cannot be read.
pub fn compute_backlinks(
    snapshot: &Snapshot,
    target_key: &str,
) -> Result<Vec<BacklinkEntry>, IndexError> {
    let records = snapshot.records().map_err(store_error)?;
    let mut entries = Vec::new();
    for (id, record) in &records {
        let StoredRecord::Plaintext { envelope } = record else {
            continue;
        };
        // Explicit links.
        for link in &envelope.links {
            if link.key == target_key {
                entries.push(BacklinkEntry {
                    source_id: id.display_value(),
                    source_kind: Some(envelope.kind.clone()),
                    source_title: envelope.title.clone(),
                    relation: link.relation.clone(),
                    mention_type: MentionType::ExplicitLink,
                });
            }
        }
        // Body mentions.
        if contains_key_mention(&envelope.content, target_key) {
            entries.push(BacklinkEntry {
                source_id: id.display_value(),
                source_kind: Some(envelope.kind.clone()),
                source_title: envelope.title.clone(),
                relation: None,
                mention_type: MentionType::BodyMention,
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
    Ok(entries)
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
    IndexError::new(format!("canonical Git Memory read failed: {error}"))
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

/// Decode vector kNN result batches into search hits. Applies the cosine
/// similarity floor: hits with `similarity < VECTOR_RESCUE_FLOOR` are
/// discarded.
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
            if score < VECTOR_RESCUE_FLOOR {
                continue;
            }
            hits.push(SearchHit {
                id: ids.value(row).to_owned(),
                kind: optional(kinds),
                title: optional(titles),
                content: optional(contents),
                archived: archived.value(row),
                freshness: optional(freshness),
                tags,
                fts_score: None,
                vector_score: Some(score),
                combined_rank: score,
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
        .map(|id| {
            let fts = fts_rank.get(&id);
            let vec = vec_rank.get(&id);
            let mut hit = fts
                .map(|(_, h)| h.clone())
                .or_else(|| vec.map(|(_, h)| h.clone()))
                .unwrap_or_else(|| panic!("rrf_fuse: id present in neither channel"));
            let fts_term = fts.map(|(r, _)| 1.0 / (RRF_K + r) as f64).unwrap_or(0.0);
            let vec_term = vec.map(|(r, _)| 1.0 / (RRF_K + r) as f64).unwrap_or(0.0);
            hit.fts_score = fts.and_then(|(_, h)| h.fts_score);
            hit.vector_score = vec.and_then(|(_, h)| h.vector_score);
            hit.combined_rank = fts_term + vec_term;
            hit
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
    use git_memory_core::{Envelope, StoredRecord};
    use git_memory_store::{GitStore, Operation, Transaction};

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
