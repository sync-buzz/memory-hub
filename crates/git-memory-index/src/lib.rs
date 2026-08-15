//! Recoverable `LanceDB` read model for generic Git Memory envelopes.
//!
//! Git remains authoritative. The projection metadata is an atomic pointer to
//! the canonical revision represented by the `records` table; callers never
//! receive a table handle and therefore cannot bypass freshness checks.

use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use arrow_array::{Array, BooleanArray, RecordBatch, RecordBatchIterator, StringArray};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use fs2::FileExt;
use futures::TryStreamExt;
use git_memory_core::StoredRecord;
use git_memory_store::{ChangeKind, GitStore, RecordId, Revision, Snapshot};
use lancedb::connection::Connection;
use lancedb::index::Index as LanceIndex;
use lancedb::index::scalar::FtsIndexBuilder;
use lancedb::query::ExecutableQuery;
use serde::{Deserialize, Serialize};

const TABLE: &str = "records";
const META_SCHEMA: u32 = 1;

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
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProjectedRecord {
    pub id: String,
    pub kind: Option<String>,
    pub title: Option<String>,
    pub content: Option<String>,
    pub archived: bool,
    pub freshness: Option<String>,
}

#[derive(Debug)]
pub struct IndexError {
    message: String,
}

impl IndexError {
    fn new(message: impl Into<String>) -> Self {
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
}

impl Projection {
    /// Synchronize the default per-repository projection from synchronous
    /// adapters such as the MCP stdio server.
    ///
    /// # Errors
    ///
    /// Returns an error when the runtime, catalog, or synchronization fails.
    pub fn synchronize_store(store: &GitStore) -> Result<ProjectionStatus, IndexError> {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| IndexError::new(format!("create projection runtime: {error}")))?;
        runtime.block_on(async {
            let projection = Self::open(store.git_dir().join("git-memory/index")).await?;
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
        })
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
        })?;
        let records = snapshot.records().map_err(store_error)?;
        let batch = batch(&records, &target)?;
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
        if batch.num_rows() == 0 {
            connection
                .create_empty_table(TABLE, schema())
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
            let mut merge = table.merge_insert(&["id"]);
            merge
                .when_matched_update_all(None)
                .when_not_matched_insert_all();
            merge
                .execute(reader(batch(&updated_records, to)?))
                .await
                .map_err(lance_error)?;
        }
        let status = ProjectionStatus {
            schema_version: META_SCHEMA,
            state: ProjectionState::Fresh,
            canonical_revision: Some(to.clone()),
            target_revision: None,
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
        Field::new("record_json", DataType::Utf8, false),
    ]))
}

struct ProjectionRow<'a> {
    id: String,
    kind: Option<&'a str>,
    title: Option<&'a str>,
    content: Option<&'a str>,
    archived: bool,
    freshness: Option<String>,
    record_json: String,
}

impl<'a> ProjectionRow<'a> {
    fn from_record(id: &RecordId, record: &'a StoredRecord) -> Result<Self, IndexError> {
        let (kind, title, content, archived, freshness) = match record {
            StoredRecord::Plaintext { envelope } => (
                Some(envelope.kind.as_str()),
                envelope.title.as_deref(),
                Some(envelope.content.as_str()),
                envelope.archive.archived,
                Some(format!("{:?}", envelope.freshness.state).to_ascii_lowercase()),
            ),
            StoredRecord::Encrypted { .. } => (None, None, None, false, None),
        };
        Ok(Self {
            id: id.display_value(),
            kind,
            title,
            content,
            archived,
            freshness,
            record_json: serde_json::to_string(record)
                .map_err(|error| IndexError::new(format!("serialize projected record: {error}")))?,
        })
    }
}

fn batch(records: &[(RecordId, StoredRecord)], _: &Revision) -> Result<RecordBatch, IndexError> {
    let rows = records
        .iter()
        .map(|(id, record)| ProjectionRow::from_record(id, record))
        .collect::<Result<Vec<_>, _>>()?;
    RecordBatch::try_new(
        schema(),
        vec![
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
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|row| &row.record_json),
            )),
        ],
    )
    .map_err(|error| IndexError::new(format!("build projection batch: {error}")))
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
        let archived = batch
            .column_by_name("archived")
            .and_then(|value| value.as_any().downcast_ref::<BooleanArray>())
            .ok_or_else(|| IndexError::new("projection column `archived` is corrupt"))?;
        for row in 0..batch.num_rows() {
            let optional =
                |array: &StringArray| (!array.is_null(row)).then(|| array.value(row).to_owned());
            rows.push(ProjectedRecord {
                id: ids.value(row).to_owned(),
                kind: optional(kinds),
                title: optional(titles),
                content: optional(contents),
                archived: archived.value(row),
                freshness: optional(freshness),
            });
        }
    }
    rows.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(rows)
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
fn lance_error(error: impl fmt::Display) -> IndexError {
    IndexError::new(format!("LanceDB projection operation failed: {error}"))
}
fn store_error(error: impl fmt::Display) -> IndexError {
    IndexError::new(format!("canonical Git Memory read failed: {error}"))
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
