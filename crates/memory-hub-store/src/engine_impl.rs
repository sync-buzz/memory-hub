//! `GitStore` seen through the storage-neutral contract.
//!
//! Nothing new happens here: every method forwards to the inherent one. The
//! value is that callers can hold a `&dyn RecordStore` and stop naming Git.

use std::path::PathBuf;

use memory_hub_core::StoredRecord;
use memory_hub_engine::{
    ApplyResult, Capabilities, Capability, HistoryStore, Ownership, PortableStore, RecordChange,
    RecordId, RecordStore, Revision, StoreDescription, StoreError, Transaction,
};

use crate::GitStore;

/// Identifier this backend reports in the handshake and in `doctor`.
pub const REFS_BACKEND: &str = "refs";

impl RecordStore for GitStore {
    /// What this store can be asked for, and nothing beyond it.
    ///
    /// Declared rather than assumed, because a caller reads this to decide
    /// whether to offer an operation at all — a capability claimed here that
    /// the store cannot honour is a failure discovered at the call instead of
    /// at the question.
    fn capabilities(&self) -> Capabilities {
        Capabilities::new(
            Ownership::Owned,
            [
                Capability::History,
                Capability::Transport,
                Capability::Snapshots,
            ],
        )
    }

    fn describe(&self) -> StoreDescription {
        StoreDescription {
            backend: REFS_BACKEND.to_owned(),
            git_dir: Some(self.git_dir().to_path_buf()),
        }
    }

    fn index_root(&self) -> PathBuf {
        self.git_dir().join("memory-hub/index")
    }

    fn current_revision(&self) -> Result<Revision, StoreError> {
        Ok(self.current()?.revision().clone())
    }

    fn read_record(
        &self,
        revision: &Revision,
        id: &RecordId,
    ) -> Result<Option<StoredRecord>, StoreError> {
        // `read_record_pub` cannot collide with a trait method name, so this
        // call can never resolve back into this impl.
        self.read_record_pub(revision, id)
    }

    fn read_records(
        &self,
        revision: &Revision,
    ) -> Result<Vec<(RecordId, StoredRecord)>, StoreError> {
        self.read_records_pub(revision)
    }

    fn apply(&self, transaction: &Transaction) -> Result<ApplyResult, StoreError> {
        GitStore::apply(self, transaction)
    }

    fn validate_revision(&self, revision: &Revision) -> Result<(), StoreError> {
        GitStore::snapshot(self, revision).map(|_| ())
    }

    fn history(&self) -> Option<&dyn HistoryStore> {
        Some(self)
    }

    fn portable(&self) -> Option<&dyn PortableStore> {
        Some(self)
    }
}

impl HistoryStore for GitStore {
    fn diff(&self, from: &Revision, to: &Revision) -> Result<Vec<RecordChange>, StoreError> {
        GitStore::diff(self, from, to)
    }
}

impl PortableStore for GitStore {
    fn export(&self, revision: &Revision) -> Result<Vec<u8>, StoreError> {
        GitStore::export(self, revision)
    }

    fn import(
        &self,
        transaction_id: &str,
        expected_revision: Revision,
        bytes: &[u8],
    ) -> Result<ApplyResult, StoreError> {
        GitStore::import(self, transaction_id, expected_revision, bytes)
    }
}
