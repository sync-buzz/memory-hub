use std::fmt;
use std::path::PathBuf;

use git_memory_core::{OpaqueStorageId, StoredRecord};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{GitStore, StoreError};

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Revision(String);

impl Revision {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_oid(oid: git2::Oid) -> Self {
        Self(oid.to_string())
    }

    pub(crate) fn oid(&self) -> Result<git2::Oid, StoreError> {
        git2::Oid::from_str(&self.0).map_err(|_| {
            StoreError::new(
                crate::StoreErrorKind::InvalidArgument,
                "revision is not a Git object id",
                serde_json::json!({"field": "revision", "revision": self.0}),
            )
        })
    }
}

impl fmt::Display for Revision {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

/// Logical identity used for conflict detection. Plaintext ids remain useful
/// to callers; opaque ids never reveal the encrypted record key.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "addressing", content = "value", rename_all = "snake_case")]
pub enum RecordId {
    Plaintext(String),
    Opaque(OpaqueStorageId),
}

impl RecordId {
    #[must_use]
    pub fn plaintext(key: impl Into<String>) -> Self {
        Self::Plaintext(key.into())
    }

    #[must_use]
    pub const fn opaque(id: OpaqueStorageId) -> Self {
        Self::Opaque(id)
    }

    #[must_use]
    pub fn from_record(record: &StoredRecord) -> Self {
        match record {
            StoredRecord::Plaintext { envelope } => Self::Plaintext(envelope.key.clone()),
            StoredRecord::Encrypted { encrypted } => Self::Opaque(encrypted.storage_id.clone()),
        }
    }

    #[must_use]
    pub fn display_value(&self) -> String {
        match self {
            Self::Plaintext(key) => key.clone(),
            Self::Opaque(id) => format!("opaque:{}", id.as_str()),
        }
    }

    pub(crate) fn tree_name(&self) -> String {
        match self {
            Self::Plaintext(key) => format!("r-p-{:x}", Sha256::digest(key.as_bytes())),
            Self::Opaque(id) => format!("r-o-{}", id.as_str()),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Put { record: StoredRecord },
    Delete { id: RecordId },
}

impl Operation {
    #[must_use]
    pub fn put(record: StoredRecord) -> Self {
        Self::Put { record }
    }

    #[must_use]
    pub fn delete(id: RecordId) -> Self {
        Self::Delete { id }
    }

    #[must_use]
    pub fn id(&self) -> RecordId {
        match self {
            Self::Put { record } => RecordId::from_record(record),
            Self::Delete { id } => id.clone(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Transaction {
    pub id: String,
    pub expected_revision: Revision,
    pub operations: Vec<Operation>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ApplyResult {
    pub revision: Revision,
    pub changed_keys: Vec<String>,
}

/// An immutable view identified by repository location and Memory commit oid.
#[derive(Clone, Debug)]
pub struct Snapshot {
    pub(crate) git_dir: PathBuf,
    pub(crate) revision: Revision,
}

impl Snapshot {
    #[must_use]
    pub fn revision(&self) -> &Revision {
        &self.revision
    }

    /// Read one record from this immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the repository or stored record is corrupt.
    pub fn get(&self, id: &RecordId) -> Result<Option<StoredRecord>, StoreError> {
        GitStore::from_git_dir(self.git_dir.clone()).read_record(&self.revision, id)
    }

    /// Read every record from this immutable snapshot in logical-id order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the repository or a stored record is corrupt.
    pub fn records(&self) -> Result<Vec<(RecordId, StoredRecord)>, StoreError> {
        GitStore::from_git_dir(self.git_dir.clone()).read_records(&self.revision)
    }
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordChange {
    pub id: RecordId,
    pub kind: ChangeKind,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Checkpoint {
    pub commit: String,
    pub revision: Revision,
    pub message: String,
    pub timestamp: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_revision: Option<String>,
}

/// Deterministic record-only export. Revision and transaction receipts are
/// excluded, so export → import → export is byte-for-byte stable.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExportBundle {
    pub schema_version: u32,
    pub records: Vec<(RecordId, StoredRecord)>,
}
