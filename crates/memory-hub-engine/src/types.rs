use std::fmt;

use memory_hub_core::{ContentHash, StoredRecord};
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

/// Logical identity used for conflict detection.
///
/// An enum with one variant, and deliberately so: the addressing tag is part
/// of what a store writes down, and a record already written says
/// `"addressing": "plaintext"`.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "addressing", content = "value", rename_all = "snake_case")]
pub enum RecordId {
    Plaintext(String),
}

impl RecordId {
    #[must_use]
    pub fn plaintext(key: impl Into<String>) -> Self {
        Self::Plaintext(key.into())
    }

    #[must_use]
    pub fn from_record(record: &StoredRecord) -> Self {
        match record {
            StoredRecord::Plaintext { envelope } => Self::Plaintext(envelope.key.clone()),
        }
    }

    #[must_use]
    pub fn display_value(&self) -> String {
        match self {
            Self::Plaintext(key) => key.clone(),
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

/// One transaction of the history, with what it did to the records.
///
/// A diff answers *what is different between these two states*; this answers
/// *what happened, one write at a time*. The difference matters to anything
/// that shows a person what has been going on: a record written three times
/// is one line in a diff and three events here, and only the second can say
/// when each of them was or who made it.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct JournalEntry {
    /// The state this transaction left the store in.
    pub revision: Revision,
    /// When it landed, in seconds since the epoch, UTC.
    ///
    /// A number rather than a formatted time, and the name says which unit so
    /// nobody has to guess between seconds and milliseconds. Formatting it is
    /// the reader's: the store has no timezone, no locale and no opinion about
    /// either, and a string here would be all three decided in the wrong place.
    pub at_epoch_seconds: i64,
    /// The id the writer minted for this transaction.
    ///
    /// Carried as it was written rather than interpreted. Callers put their own
    /// occasion in front of it — the engine neither imposes that shape nor
    /// parses it, so a client that gave its writes meaningful prefixes gets
    /// them back and one that did not loses nothing it had.
    pub transaction_id: Option<String>,
    /// What this transaction did, one entry per record it touched.
    pub changes: Vec<JournalChange>,
}

/// What one transaction did to one record.
///
/// It carries the record's own kind and title beside the change because the
/// alternative is reading every key back, and for a deleted record there is
/// nothing left to read: a history that could not name what was removed would
/// report the one event a person most wants named.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct JournalChange {
    pub id: RecordId,
    /// Whether the record was added, changed or removed by this transaction.
    ///
    /// Spelled `change` rather than `kind`, because `kind` is the record's own
    /// type everywhere else in this interface and [`RecordChange`] is the one
    /// place it means something else. That shape is kept for the callers that
    /// already read it; it is not repeated here.
    pub change: ChangeKind,
    /// The record's type, as it stood in this transaction. For a removal, as it
    /// stood in the version that was removed.
    pub kind: String,
    pub title: Option<String>,
}

/// A page of the history, newest first.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct Journal {
    pub entries: Vec<JournalEntry>,
    /// True when the walk stopped because it had filled the page, not because
    /// it had reached the revision it was asked to stop at. A caller that wants
    /// the rest asks again from the oldest entry it received.
    pub has_more: bool,
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
