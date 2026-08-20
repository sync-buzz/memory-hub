use std::fmt;

use memory_hub_core::{ContentHash, OpaqueStorageId, StoredRecord};
use serde::{Deserialize, Serialize};

/// An opaque marker for "the state the store was in".
///
/// What the string means belongs to the backend that produced it — a commit id
/// for the Git store, something else elsewhere — and nothing outside that
/// backend may parse it. Callers compare revisions and hand them back; they do
/// not read them.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct Revision(String);

impl Revision {
    /// Wrap a backend's own state token.
    #[must_use]
    pub fn new(token: impl Into<String>) -> Self {
        Self(token.into())
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
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
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Put {
        record: StoredRecord,
        /// The content this write is based on, when the caller wants the write
        /// conditional on it.
        ///
        /// A revision agrees on the whole store, which is the right unit for a
        /// storage nothing else writes. It is the wrong unit — and for an
        /// external folder an impossible one — when the content belongs to
        /// somebody else: there is no past state to pin, only the bytes that
        /// are there now. So agreement is per record, by digest.
        ///
        /// Absent means an unconditional write, which is what every client
        /// that has never heard of this field sends.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expected_content_hash: Option<ContentHash>,
    },
    Delete {
        id: RecordId,
    },
}

impl Operation {
    /// An unconditional put.
    #[must_use]
    pub fn put(record: StoredRecord) -> Self {
        Self::Put {
            record,
            expected_content_hash: None,
        }
    }

    /// A put that applies only if the stored content still hashes to
    /// `expected_content_hash`.
    #[must_use]
    pub fn put_if_unchanged(record: StoredRecord, expected_content_hash: ContentHash) -> Self {
        Self::Put {
            record,
            expected_content_hash: Some(expected_content_hash),
        }
    }

    #[must_use]
    pub fn delete(id: RecordId) -> Self {
        Self::Delete { id }
    }

    #[must_use]
    pub fn id(&self) -> RecordId {
        match self {
            Self::Put { record, .. } => RecordId::from_record(record),
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

/// What an export does with a record whose content lives outside it.
///
/// Two different requests, not two opinions about one, so the caller chooses
/// and the answer is written into the bundle rather than inferred from it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExportMode {
    /// Keep the locator and the digest last resolved through it.
    ///
    /// Deterministic — export → import → export is byte-for-byte stable —
    /// and incomplete away from the content: an import elsewhere gets records
    /// whose bodies it cannot read until the locators resolve there too.
    Manifest,
    /// Resolve every locator and carry the content.
    ///
    /// Complete and portable to a machine that has never seen the source
    /// folder, at the cost of determinism: the outside can change between two
    /// exports, so two snapshots of one revision may differ.
    Snapshot,
}

/// Record-only export. Revision and transaction receipts are excluded.
///
/// In [`ExportMode::Manifest`] the bundle is deterministic: export → import →
/// export is byte-for-byte stable. A corpus with no reference records exports
/// identically in both modes.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ExportBundle {
    pub schema_version: u32,
    /// What this bundle did with external content. Recorded so an importer
    /// reads it instead of guessing from the records.
    ///
    /// Absent in bundles written before the field existed, and `Manifest` is
    /// what those are: nothing could reference anything outside itself yet.
    #[serde(default = "manifest_mode")]
    pub mode: ExportMode,
    pub records: Vec<(RecordId, StoredRecord)>,
}

const fn manifest_mode() -> ExportMode {
    ExportMode::Manifest
}
