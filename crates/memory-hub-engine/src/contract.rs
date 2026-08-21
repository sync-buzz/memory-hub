use std::collections::BTreeSet;
use std::fmt::Debug;
use std::path::PathBuf;

use memory_hub_core::StoredRecord;
use serde::{Deserialize, Serialize};

use crate::{ApplyResult, RecordChange, RecordId, Revision, StoreError, Transaction};

/// Who writes to the storage.
///
/// This is not a detail of the implementation — it decides how a write agrees
/// with what is already there. An owned storage can compare and swap. A shared
/// one cannot: between our read and our write a person saves the file in their
/// editor, so agreement has to be per record, by comparing content.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Ownership {
    /// Memory Hub is the only writer.
    Owned,
    /// Someone else writes here too.
    Shared,
}

/// Something a backend may or may not be able to do.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    /// A past state can be compared with another: what a diff reads.
    History,
    /// Exchange with a remote.
    Transport,
    /// A past revision can be reopened as an immutable view. A folder someone
    /// else edits cannot offer this: the past state is not kept anywhere, so
    /// there is nothing to reopen.
    Snapshots,
}

/// What a backend can actually do, so a caller can ask instead of assume.
///
/// A set rather than a row of flags: capabilities are added over time, and a
/// caller that asks for one it has never heard of should get `false` instead of
/// a struct that no longer deserialises.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct Capabilities {
    pub ownership: Ownership,
    pub supported: BTreeSet<Capability>,
}

impl Capabilities {
    #[must_use]
    pub fn new(ownership: Ownership, supported: impl IntoIterator<Item = Capability>) -> Self {
        Self {
            ownership,
            supported: supported.into_iter().collect(),
        }
    }

    #[must_use]
    pub fn has(&self, capability: Capability) -> bool {
        self.supported.contains(&capability)
    }
}

/// Backend-specific facts for the handshake and for `doctor`.
///
/// Fields that belong to one backend are absent for the others rather than
/// filled with something plausible. A wrong path passes every type check and
/// fails at first use; a missing one fails where the problem is.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StoreDescription {
    /// Stable identifier of the backend kind, e.g. `refs`.
    pub backend: String,
    /// Resolved Git directory. Present only for a Git-backed store.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_dir: Option<PathBuf>,
}

/// A rule about what a transaction may contain, checked against the corpus as
/// it actually stands.
///
/// This is the seam that keeps application rules out of backends. A record
/// store owns the *moment*: it alone knows when the state has been read and has
/// not yet changed, and that moment is not the one the caller last saw — a
/// store that rebases applies a transaction onto state the caller never read.
/// What counts as a legal transaction, on the other hand, belongs to the layer
/// that knows about types, and writing it once there is what keeps two backends
/// from disagreeing about the same record.
///
/// So the backend calls the policy it was given and reports what it says. It
/// never learns why.
pub trait TransactionPolicy: Debug + Send + Sync {
    /// Check `transaction` against `existing` — the corpus this attempt will
    /// build on, not the one the caller last read.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] naming the rule that refused the transaction.
    fn check(
        &self,
        transaction: &Transaction,
        existing: &[(RecordId, StoredRecord)],
    ) -> Result<(), StoreError>;
}

/// What every storage must do to be a storage.
///
/// Everything here is answerable by any backend. Anything that is not lives in
/// one of the traits below.
pub trait RecordStore: Debug + Send + Sync {
    fn capabilities(&self) -> Capabilities;

    fn describe(&self) -> StoreDescription;

    /// Where this store's derived read model belongs. The index is disposable
    /// and rebuildable, but it has to live somewhere the store decides.
    fn index_root(&self) -> PathBuf;

    /// The state every read serves by default.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the store cannot be read.
    fn current_revision(&self) -> Result<Revision, StoreError>;

    /// # Errors
    ///
    /// Returns [`StoreError`] if the store or the stored record is unreadable.
    fn read_record(
        &self,
        revision: &Revision,
        id: &RecordId,
    ) -> Result<Option<StoredRecord>, StoreError>;

    /// Every record at `revision`, in logical-id order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the store or a stored record is unreadable.
    fn read_records(
        &self,
        revision: &Revision,
    ) -> Result<Vec<(RecordId, StoredRecord)>, StoreError>;

    /// Apply a put/delete batch.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on conflict, on a reused transaction id, or if
    /// the batch cannot be written.
    fn apply(&self, transaction: &Transaction) -> Result<ApplyResult, StoreError>;

    /// Reject a revision this store cannot serve, before a caller builds a
    /// view on it and discovers the problem one read later.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the revision is malformed or not retained.
    fn validate_revision(&self, revision: &Revision) -> Result<(), StoreError>;

    /// The history side of this store, when it has one.
    ///
    /// Returning `None` is the same statement as omitting
    /// [`Capability::History`], reachable from a `dyn` reference.
    fn history(&self) -> Option<&dyn HistoryStore> {
        None
    }

    /// The export/import side of this store, when it has one.
    fn portable(&self) -> Option<&dyn PortableStore> {
        None
    }
}

/// An immutable read of one store at one revision.
///
/// A store and a revision are enough to read from, and the pair names nothing
/// storage-specific. Whether the revision can be a *past*
/// one is a capability — see [`Capability::Snapshots`] — because a folder
/// someone else edits keeps no past to reopen.
#[derive(Clone, Debug)]
pub struct StoreView<'a> {
    store: &'a dyn RecordStore,
    revision: Revision,
}

impl<'a> StoreView<'a> {
    /// Open a view on a specific revision.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the store cannot serve that revision.
    pub fn open(store: &'a dyn RecordStore, revision: &Revision) -> Result<Self, StoreError> {
        store.validate_revision(revision)?;
        Ok(Self {
            store,
            revision: revision.clone(),
        })
    }

    /// Open a view on whatever the store currently serves.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the current revision cannot be read.
    pub fn current(store: &'a dyn RecordStore) -> Result<Self, StoreError> {
        let revision = store.current_revision()?;
        Ok(Self { store, revision })
    }

    /// Build a view on a revision the caller already obtained from this same
    /// store, skipping revalidation.
    #[must_use]
    pub const fn trusted(store: &'a dyn RecordStore, revision: Revision) -> Self {
        Self { store, revision }
    }

    #[must_use]
    pub const fn revision(&self) -> &Revision {
        &self.revision
    }

    /// # Errors
    ///
    /// Returns [`StoreError`] if the record cannot be read.
    pub fn get(&self, id: &RecordId) -> Result<Option<StoredRecord>, StoreError> {
        self.store.read_record(&self.revision, id)
    }

    /// # Errors
    ///
    /// Returns [`StoreError`] if the records cannot be read.
    pub fn records(&self) -> Result<Vec<(RecordId, StoredRecord)>, StoreError> {
        self.store.read_records(&self.revision)
    }
}

/// A store that keeps its past, and so can say what changed between two of
/// its states.
pub trait HistoryStore {
    /// # Errors
    ///
    /// Returns [`StoreError`] if either revision is invalid.
    fn diff(&self, from: &Revision, to: &Revision) -> Result<Vec<RecordChange>, StoreError>;
}

/// A store whose contents can leave it and come back.
pub trait PortableStore {
    /// # Errors
    ///
    /// Returns [`StoreError`] if the revision is unreadable.
    fn export(&self, revision: &Revision) -> Result<Vec<u8>, StoreError>;

    /// # Errors
    ///
    /// Returns [`StoreError`] for an invalid bundle or a failed write.
    fn import(
        &self,
        transaction_id: &str,
        expected_revision: Revision,
        bytes: &[u8],
    ) -> Result<ApplyResult, StoreError>;
}
