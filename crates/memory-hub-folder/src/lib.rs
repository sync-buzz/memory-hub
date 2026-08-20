//! A record store that is a folder of files, for projects that have no Git.
//!
//! Every record is one JSON file under `records/`, named after its key. That is
//! the whole point of this backend: a person can open the folder, read a
//! record, and see that nothing has been hidden from them. A single corpus file
//! would have been simpler to write atomically and would have made the name a
//! lie.
//!
//! What it does not do is as important as what it does. It keeps no past, so it
//! offers neither history nor snapshots; it talks to no remote; it does not
//! encrypt. Those are absent from its [`Capabilities`], which is the honest way
//! to say "not here" — see the capability set rather than a method that returns
//! an error nobody expected.

mod journal;
mod layout;
mod paths;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use memory_hub_core::StoredRecord;
use memory_hub_engine::{
    ApplyResult, Capabilities, Capability, Operation, Ownership, RecordId, RecordStore, Revision,
    StoreDescription, StoreError, StoreErrorKind, Transaction, TransactionPolicy,
};

use crate::journal::Journal;
use crate::layout::Layout;

/// Identifier this backend reports in [`StoreDescription::backend`].
pub const FOLDER_BACKEND: &str = "folder";

/// Records kept as files in a folder.
#[derive(Debug)]
pub struct FolderStore {
    layout: Layout,
    policy: Option<std::sync::Arc<dyn TransactionPolicy>>,
}

impl FolderStore {
    /// Open — and create, if absent — a record folder at `root`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the folder cannot be created or an interrupted
    /// transaction cannot be finished.
    pub fn open(root: impl AsRef<Path>) -> Result<Self, StoreError> {
        let layout = Layout::create(root.as_ref())?;
        // A journal here means a previous process died mid-write. Finishing it
        // before anything reads is what makes the transaction that wrote it
        // atomic rather than merely quick.
        Journal::recover(&layout)?;
        Ok(Self {
            layout,
            policy: None,
        })
    }

    /// Attach the rules every transaction is checked against.
    #[must_use]
    pub fn with_policy(mut self, policy: std::sync::Arc<dyn TransactionPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// The folder this store keeps its records in.
    #[must_use]
    pub fn root(&self) -> &Path {
        self.layout.root()
    }

    /// Read every record, in logical-id order.
    fn load(&self) -> Result<BTreeMap<RecordId, StoredRecord>, StoreError> {
        self.layout.read_all()
    }
}

impl RecordStore for FolderStore {
    fn capabilities(&self) -> Capabilities {
        // Nothing optional is offered, and that is the design rather than a
        // gap: a folder holding only the present has no past to reopen, no
        // remote to exchange with, and no way to encrypt itself and stay a
        // folder somebody can read.
        Capabilities::new(Ownership::Owned, [] as [Capability; 0])
    }

    fn describe(&self) -> StoreDescription {
        StoreDescription {
            backend: FOLDER_BACKEND.to_owned(),
            git_dir: None,
        }
    }

    fn index_root(&self) -> PathBuf {
        self.layout.index_root()
    }

    fn current_revision(&self) -> Result<Revision, StoreError> {
        Ok(Layout::digest(&self.load()?))
    }

    fn read_record(
        &self,
        revision: &Revision,
        id: &RecordId,
    ) -> Result<Option<StoredRecord>, StoreError> {
        self.validate_revision(revision)?;
        Ok(self.load()?.remove(id))
    }

    fn read_records(
        &self,
        revision: &Revision,
    ) -> Result<Vec<(RecordId, StoredRecord)>, StoreError> {
        self.validate_revision(revision)?;
        Ok(self.load()?.into_iter().collect())
    }

    fn validate_revision(&self, revision: &Revision) -> Result<(), StoreError> {
        // The only revision this store can serve is the one it is in: it keeps
        // no past, so a revision that is not the current one is not "older", it
        // is unknown.
        let current = self.current_revision()?;
        if *revision == current {
            return Ok(());
        }
        Err(StoreError::new(
            StoreErrorKind::RevisionNotFound,
            "a folder store serves only its current state — it keeps no past to reopen",
            serde_json::json!({
                "requested": revision.as_str(),
                "current": current.as_str(),
            }),
        ))
    }

    fn apply(&self, transaction: &Transaction) -> Result<ApplyResult, StoreError> {
        let _guard = self.layout.lock()?;

        if let Some(result) = self.layout.replay(&transaction.id)? {
            return Ok(result);
        }

        let mut records = self.load()?;
        let current = Layout::digest(&records);
        if transaction.expected_revision != current {
            return Err(StoreError::new(
                StoreErrorKind::Conflict,
                "the folder changed since the revision this transaction was built on",
                serde_json::json!({
                    "expected": transaction.expected_revision.as_str(),
                    "current": current.as_str(),
                }),
            ));
        }

        if let Some(policy) = &self.policy {
            let existing: Vec<(RecordId, StoredRecord)> = records
                .iter()
                .map(|(id, r)| (id.clone(), r.clone()))
                .collect();
            policy.check(transaction, &existing)?;
        }

        require_expected_content(transaction, &records)?;

        let mut changed = Vec::new();
        for operation in &transaction.operations {
            let id = operation.id();
            match operation {
                Operation::Put { record, .. } => {
                    records.insert(id.clone(), record.clone());
                }
                Operation::Delete { .. } => {
                    records.remove(&id);
                }
            }
            changed.push(id);
        }

        let revision = Layout::digest(&records);
        let result = ApplyResult {
            revision: revision.clone(),
            changed_keys: {
                let mut keys: Vec<String> = changed.iter().map(RecordId::display_value).collect();
                keys.sort();
                keys.dedup();
                keys
            },
        };

        // Write the plan first, then carry it out. A process that dies between
        // the two leaves a journal, and the next open finishes what it started
        // — which is the difference between a batch and a batch that is atomic.
        Journal::write(&self.layout, transaction, &result)?;
        Journal::commit(&self.layout)?;
        Ok(result)
    }
}

/// Enforce the per-record conditions a batch carries.
///
/// A revision agrees on the whole store, which is the right unit here because
/// nothing else writes this folder. `expected_content_hash` is still honoured:
/// a caller that has it is a caller who read one record and wants to write it
/// back only if it is still the one they read.
fn require_expected_content(
    transaction: &Transaction,
    records: &BTreeMap<RecordId, StoredRecord>,
) -> Result<(), StoreError> {
    for operation in &transaction.operations {
        let Operation::Put {
            record,
            expected_content_hash: Some(expected),
        } = operation
        else {
            continue;
        };
        let id = RecordId::from_record(record);
        let actual = records.get(&id).and_then(|stored| match stored {
            StoredRecord::Plaintext { envelope } => Some(envelope.content_hash.clone()),
            StoredRecord::Encrypted { .. } => None,
        });
        if actual.as_ref() != Some(expected) {
            return Err(StoreError::new(
                StoreErrorKind::Conflict,
                "the stored content is not the content this write was based on",
                serde_json::json!({
                    "id": id.display_value(),
                    "expected": expected,
                    "actual": actual,
                }),
            ));
        }
    }
    Ok(())
}
