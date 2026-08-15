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
    pub canonical_revision: Revision,
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
        let _lock = self.write_lock()?;
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
        let _lock = self.write_lock()?;
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
        let _lock = self.read_lock()?;
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
                    Ok(_) => Ok(status),
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
        {
            let _lock = self.write_lock()?;
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
        }
        // Reopen because Lance connections retain catalog state.
        let replacement = Self::open(&self.root).await?;
        let connection = replacement.connection()?;
        *self
            .connection
            .write()
            .map_err(|_| IndexError::new("projection connection lock is poisoned"))? = connection;
        self.rebuild(snapshot).await
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

    fn lock_file(&self) -> Result<File, IndexError> {
        OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.root.join("projection.lock"))
            .map_err(Into::into)
    }

    fn read_lock(&self) -> Result<File, IndexError> {
        let file = self.lock_file()?;
        file.lock_shared()?;
        Ok(file)
    }

    fn write_lock(&self) -> Result<File, IndexError> {
        let file = self.lock_file()?;
        file.lock_exclusive()?;
        Ok(file)
    }

    fn read_status_unlocked(&self) -> Result<ProjectionStatus, IndexError> {
        match fs::read(self.status_path()) {
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

    fn write_status(&self, status: &ProjectionStatus) -> Result<(), IndexError> {
        let temporary = self.root.join("status.json.tmp");
        let bytes = serde_json::to_vec(status)
            .map_err(|error| IndexError::new(format!("serialize projection status: {error}")))?;
        fs::write(&temporary, bytes)?;
        fs::rename(temporary, self.status_path())?;
        Ok(())
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
        Field::new("canonical_revision", DataType::Utf8, false),
    ]))
}

fn batch(
    records: &[(RecordId, StoredRecord)],
    revision: &Revision,
) -> Result<RecordBatch, IndexError> {
    let ids = records
        .iter()
        .map(|(id, _)| id.display_value())
        .collect::<Vec<_>>();
    let kinds = records
        .iter()
        .map(|(_, record)| match record {
            StoredRecord::Plaintext { envelope } => Some(envelope.kind.as_str()),
            StoredRecord::Encrypted { .. } => None,
        })
        .collect::<Vec<_>>();
    let titles = records
        .iter()
        .map(|(_, record)| match record {
            StoredRecord::Plaintext { envelope } => envelope.title.as_deref(),
            StoredRecord::Encrypted { .. } => None,
        })
        .collect::<Vec<_>>();
    let contents = records
        .iter()
        .map(|(_, record)| match record {
            StoredRecord::Plaintext { envelope } => Some(envelope.content.as_str()),
            StoredRecord::Encrypted { .. } => None,
        })
        .collect::<Vec<_>>();
    let archived = records
        .iter()
        .map(|(_, record)| match record {
            StoredRecord::Plaintext { envelope } => envelope.archive.archived,
            StoredRecord::Encrypted { .. } => false,
        })
        .collect::<Vec<_>>();
    let freshness = records
        .iter()
        .map(|(_, record)| match record {
            StoredRecord::Plaintext { envelope } => {
                Some(format!("{:?}", envelope.freshness.state).to_ascii_lowercase())
            }
            StoredRecord::Encrypted { .. } => None,
        })
        .collect::<Vec<_>>();
    let json = records
        .iter()
        .map(|(_, record)| serde_json::to_string(record))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| IndexError::new(format!("serialize projected record: {error}")))?;
    let revisions = vec![revision.as_str(); records.len()];
    RecordBatch::try_new(
        schema(),
        vec![
            Arc::new(StringArray::from(ids)),
            Arc::new(StringArray::from(kinds)),
            Arc::new(StringArray::from(titles)),
            Arc::new(StringArray::from(contents)),
            Arc::new(BooleanArray::from(archived)),
            Arc::new(StringArray::from(freshness)),
            Arc::new(StringArray::from(json)),
            Arc::new(StringArray::from(revisions)),
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
        let revisions = strings("canonical_revision")?;
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
                canonical_revision: serde_json::from_value(serde_json::json!(revisions.value(row)))
                    .map_err(|error| {
                        IndexError::new(format!("decode projected revision: {error}"))
                    })?,
            });
        }
    }
    rows.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(rows)
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
