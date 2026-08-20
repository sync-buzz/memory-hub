//! `GitStore` seen through the storage-neutral contract.
//!
//! Nothing new happens here: every method forwards to the inherent one. The
//! value is that callers can hold a `&dyn RecordStore` and stop naming Git.

use std::path::PathBuf;

use memory_hub_core::StoredRecord;
use memory_hub_engine::{
    ApplyResult, Capabilities, Capability, Checkpoint, HistoryStore, Ownership, PortableStore,
    RecordChange, RecordId, RecordStore, Revision, StoreDescription, StoreError, Transaction,
};

use crate::GitStore;

/// Identifier this backend reports in the handshake and in `doctor`.
pub const REFS_BACKEND: &str = "refs";

impl RecordStore for GitStore {
    /// Encryption is deliberately absent.
    ///
    /// This store writes what it is given. When a project is encrypted the
    /// records reaching it are already ciphertext, and the store that did the
    /// encrypting — [`EncryptedStore`](crate::EncryptedStore) — is the one
    /// that declares the capability. Claiming it here would tell a caller that
    /// asking this store for plaintext is safe when it is exactly what a
    /// plaintext project does.
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
    fn checkpoint(&self, message: &str) -> Result<Checkpoint, StoreError> {
        GitStore::checkpoint(self, message)
    }

    fn checkpoint_code(
        &self,
        code_revision: &str,
        message: &str,
    ) -> Result<Checkpoint, StoreError> {
        GitStore::checkpoint_code(self, code_revision, message)
    }

    fn history(&self, limit: usize) -> Result<Vec<Checkpoint>, StoreError> {
        GitStore::history(self, limit)
    }

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
