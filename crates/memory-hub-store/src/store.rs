// serde/git2 errors are mapped at one-shot ownership boundaries.
#![allow(clippy::needless_pass_by_value)]

use std::collections::BTreeSet;
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use git2::{ErrorCode, Oid, Repository};
use memory_hub_core::StoredRecord;
use sha2::{Digest, Sha256};

use crate::error::GitStoreError;
use crate::types::{GitRecordId, GitRevision};
use crate::{
    ApplyResult, ChangeKind, ExportBundle, ExportMode, MAIN_REF, Operation, RecordChange, RecordId,
    Revision, StoreError, StoreErrorKind, StoreView, Transaction, TransactionPolicy,
};

const MAX_CAS_ATTEMPTS: usize = 32;
/// Schema version of the deterministic export bundle. Adapters that build or
/// accept a bundle without going through [`GitStore::export`] validate against
/// this value.
pub const EXPORT_SCHEMA_VERSION: u32 = 2;
const CONTRACT_PAUSE_BEFORE_REF_UPDATE: &str = "MEMORY_HUB_CONTRACT_PAUSE_BEFORE_REF_UPDATE";

mod chain;
mod records;
use chain::{
    changes_since, find_transaction, genesis_commit, memory_commit, require_retained_revision,
    transaction_commit,
};
use records::{build_tree, decode_record, snapshot_tree, verify_record_location};

#[derive(Clone, Debug)]
pub struct GitStore {
    git_dir: PathBuf,
    /// Application rules this store checks a transaction against, if it was
    /// given any. A store with none accepts any well-formed batch: what counts
    /// as a legal record is not a thing Git knows.
    policy: Option<Arc<dyn TransactionPolicy>>,
}

#[derive(Clone, Copy)]
enum RebaseMode {
    Allowed,
    Exact,
}

impl GitStore {
    /// Resolve the actual Git directory without initializing Memory refs.
    ///
    /// This read-only discovery path is used during protocol negotiation so an
    /// incompatible client can be rejected before the first store mutation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if `project` is not an absolute repository path.
    pub fn discover_git_dir(project: impl AsRef<Path>) -> Result<PathBuf, StoreError> {
        let project = project.as_ref();
        if !project.is_absolute() {
            return Err(StoreError::new(
                StoreErrorKind::InvalidArgument,
                "project must be an absolute repository root or Git directory",
                serde_json::json!({"field": "project"}),
            ));
        }
        Repository::open(project)
            .map(|repository| repository.path().to_path_buf())
            .map_err(|error| StoreError::repository("discover explicit project", error))
    }

    /// Return the resolved Git directory owned by this store.
    #[must_use]
    pub fn git_dir(&self) -> &Path {
        &self.git_dir
    }

    /// Open an explicit absolute repository root or Git directory and initialize
    /// the project's private ref when needed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if no repository can be opened or its empty tree
    /// cannot be created/referenced.
    pub fn open(project: impl AsRef<Path>) -> Result<Self, StoreError> {
        let project = project.as_ref();
        if !project.is_absolute() {
            return Err(StoreError::new(
                StoreErrorKind::InvalidArgument,
                "project must be an absolute repository root or Git directory",
                serde_json::json!({"field": "project"}),
            ));
        }
        let git_dir = Self::discover_git_dir(project)?;
        let store = Self {
            git_dir,
            policy: None,
        };
        store.ensure_ref()?;
        Ok(store)
    }

    /// Attach the rules this store checks every transaction against.
    ///
    /// The store decides *when* — with the corpus this attempt builds on, which
    /// after a rebase is not the one the caller read — and the policy decides
    /// *what*. Without one, no application rule is enforced here.
    #[must_use]
    pub fn with_policy(mut self, policy: Arc<dyn TransactionPolicy>) -> Self {
        self.policy = Some(policy);
        self
    }

    /// Return the current immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the memory ref cannot be read.
    pub fn current(&self) -> Result<StoreView<'_>, StoreError> {
        let repository = self.repository()?;
        let revision = current_oid(&repository)?;
        memory_commit(&repository, revision)?;
        Ok(StoreView::trusted(self, Revision::from_oid(revision)))
    }

    /// Open any retained Memory commit revision as an immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the revision is malformed, missing, or is not
    /// part of the transaction history.
    pub fn snapshot(&self, revision: &Revision) -> Result<StoreView<'_>, StoreError> {
        let repository = self.repository()?;
        require_retained_revision(&repository, revision.oid()?)?;
        Ok(StoreView::trusted(self, revision.clone()))
    }

    /// Atomically apply a put/delete batch with same-key conflict detection.
    /// Different-key changes since `expected_revision` are automatically
    /// rebased. A transaction id is idempotent across process restarts.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for invalid input, a same-key conflict, corrupt
    /// objects, transaction-id reuse, or exhausted compare-and-swap retries.
    pub fn apply(&self, transaction: &Transaction) -> Result<ApplyResult, StoreError> {
        self.apply_with_mode(transaction, RebaseMode::Allowed)
    }

    fn apply_with_mode(
        &self,
        transaction: &Transaction,
        mode: RebaseMode,
    ) -> Result<ApplyResult, StoreError> {
        let (ids, request_hash) = validate_transaction(transaction)?;
        let repository = self.repository()?;
        let expected_oid = transaction.expected_revision.oid()?;
        require_retained_revision(&repository, expected_oid)?;

        for _ in 0..MAX_CAS_ATTEMPTS {
            let current_oid = current_oid(&repository)?;
            let current_commit = memory_commit(&repository, current_oid)?;
            let current_tree = current_commit
                .tree()
                .map_err(|error| StoreError::repository("find current tree", error))?;

            if let Some((revision, metadata)) =
                find_transaction(&repository, current_oid, &transaction.id)?
            {
                if metadata.request_hash.as_deref() == Some(request_hash.as_str()) {
                    return Ok(ApplyResult {
                        revision: Revision::from_oid(revision),
                        changed_keys: metadata
                            .changed_keys
                            .iter()
                            .map(RecordId::display_value)
                            .collect(),
                    });
                }
                return Err(StoreError::new(
                    StoreErrorKind::TransactionReused,
                    "transaction id was already used for a different request",
                    serde_json::json!({"transaction_id": transaction.id}),
                ));
            }

            // The policy is checked against the state this attempt will build
            // on, not the one the caller last saw. The two differ exactly when
            // somebody else wrote in between — which is when a rule about
            // "records this edit would leave behind" has something new to
            // count.
            if let Some(policy) = &self.policy {
                let existing = self.read_records(&Revision::from_oid(current_oid))?;
                policy.check(transaction, &existing)?;
            }

            if expected_oid != current_oid {
                if matches!(mode, RebaseMode::Exact) {
                    return Err(conflict_error(transaction, current_oid, Vec::new()));
                }
                let changed = changes_since(&repository, expected_oid, current_oid)?;
                let conflicts = ids.intersection(&changed).cloned().collect::<Vec<_>>();
                if !conflicts.is_empty() {
                    return Err(conflict_error(transaction, current_oid, conflicts));
                }
            }

            self.require_expected_content(&Revision::from_oid(current_oid), transaction)?;

            let (tree_oid, changed_ids) = build_tree(&repository, &current_tree, transaction)?;
            let mut changed_keys = changed_ids
                .iter()
                .map(RecordId::display_value)
                .collect::<Vec<_>>();
            changed_keys.sort();
            let new_oid = transaction_commit(
                &repository,
                tree_oid,
                &current_commit,
                transaction,
                &request_hash,
                &changed_ids.iter().cloned().collect::<Vec<_>>(),
            )?;
            pause_before_ref_update()?;
            match repository.reference_matching(
                MAIN_REF,
                new_oid,
                true,
                current_oid,
                "memory-hub: apply transaction",
            ) {
                Ok(_) => {
                    return Ok(ApplyResult {
                        revision: Revision::from_oid(new_oid),
                        changed_keys: changed_keys.clone(),
                    });
                }
                Err(error) if is_cas_race(&error) => {}
                Err(error) => return Err(StoreError::repository("update the memory ref", error)),
            }
        }
        Err(StoreError::new(
            StoreErrorKind::RetryExhausted,
            "the memory ref kept changing during the transaction",
            serde_json::json!({"attempts": MAX_CAS_ATTEMPTS}),
        ))
    }

    /// Compare record identities between two immutable snapshots.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if either revision or record blob is invalid.
    pub fn diff(&self, from: &Revision, to: &Revision) -> Result<Vec<RecordChange>, StoreError> {
        let repository = self.repository()?;
        let from_tree = snapshot_tree(&repository, from)?;
        let to_tree = snapshot_tree(&repository, to)?;
        let diff = repository
            .diff_tree_to_tree(Some(&from_tree), Some(&to_tree), None)
            .map_err(|error| StoreError::repository("diff memory trees", error))?;
        let mut changes = Vec::new();
        for delta in diff.deltas() {
            let (oid, kind) = match delta.status() {
                git2::Delta::Added => (delta.new_file().id(), ChangeKind::Added),
                git2::Delta::Deleted => (delta.old_file().id(), ChangeKind::Deleted),
                git2::Delta::Modified => (delta.new_file().id(), ChangeKind::Modified),
                _ => continue,
            };
            let path = match kind {
                ChangeKind::Deleted => delta.old_file().path(),
                ChangeKind::Added | ChangeKind::Modified => delta.new_file().path(),
            };
            if !path
                .and_then(Path::to_str)
                .is_some_and(|path| path.starts_with("r-"))
            {
                continue;
            }
            let record = decode_record(&repository, oid)?;
            changes.push(RecordChange {
                id: RecordId::from_record(&record),
                kind,
            });
        }
        changes.sort_by(|left, right| left.id.cmp(&right.id));
        Ok(changes)
    }

    /// Export a record-only JSON bundle in [`ExportMode::Manifest`].
    ///
    /// The store reads refs and cannot resolve a locator, so it produces the
    /// mode that needs no resolution. A snapshot is assembled a layer up, where
    /// the working tree is in reach, and serialised through [`bundle`].
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the snapshot is corrupt or JSON serialization
    /// fails.
    pub fn export(&self, revision: &Revision) -> Result<Vec<u8>, StoreError> {
        bundle(self.read_records(revision)?, ExportMode::Manifest)
    }

    /// Replace the current record set from a deterministic export in one store
    /// transaction.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for an invalid bundle or the same failures as
    /// [`Self::apply`].
    pub fn import(
        &self,
        transaction_id: impl Into<String>,
        expected_revision: Revision,
        bytes: &[u8],
    ) -> Result<ApplyResult, StoreError> {
        let bundle: ExportBundle = serde_json::from_slice(bytes)
            .map_err(|error| serialization_error("parse import", error))?;
        // Version 1 is accepted as what it is: a bundle from before content
        // could live outside a record, which is a manifest by construction and
        // deserialises as one. The mode is then read from the bundle, never
        // inferred from what the records happen to look like.
        if bundle.schema_version == 0 || bundle.schema_version > EXPORT_SCHEMA_VERSION {
            return Err(StoreError::new(
                StoreErrorKind::InvalidArgument,
                "unsupported export schema version",
                serde_json::json!({
                    "received": bundle.schema_version,
                    "supported": EXPORT_SCHEMA_VERSION,
                }),
            ));
        }
        let current = self.read_records(&expected_revision)?;
        let imported_ids = bundle
            .records
            .iter()
            .map(|(id, _)| id.clone())
            .collect::<BTreeSet<_>>();
        let mut operations = current
            .into_iter()
            .filter(|(id, _)| !imported_ids.contains(id))
            .map(|(id, _)| Operation::delete(id))
            .collect::<Vec<_>>();
        for (id, record) in bundle.records {
            if id != RecordId::from_record(&record) {
                return Err(StoreError::new(
                    StoreErrorKind::InvalidRecord,
                    "import record id does not match its payload",
                    serde_json::json!({"id": id}),
                ));
            }
            operations.push(Operation::put(record));
        }
        self.apply_with_mode(
            &Transaction {
                id: transaction_id.into(),
                expected_revision,
                operations,
            },
            RebaseMode::Exact,
        )
    }

    pub(crate) fn read_record(
        &self,
        revision: &Revision,
        id: &RecordId,
    ) -> Result<Option<StoredRecord>, StoreError> {
        let repository = self.repository()?;
        let tree = snapshot_tree(&repository, revision)?;
        let Some(entry) = tree.get_name(&id.tree_name()) else {
            return Ok(None);
        };
        let record = decode_record(&repository, entry.id())?;
        verify_record_location(id, &record, entry.name().ok())?;
        Ok(Some(record))
    }

    /// Public read path for a single record by id, for a caller outside this
    /// module that holds the store rather than a snapshot of it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the repository or record blob is corrupt.
    pub fn read_record_pub(
        &self,
        revision: &Revision,
        id: &RecordId,
    ) -> Result<Option<StoredRecord>, StoreError> {
        self.read_record(revision, id)
    }

    pub(crate) fn read_records(
        &self,
        revision: &Revision,
    ) -> Result<Vec<(RecordId, StoredRecord)>, StoreError> {
        let repository = self.repository()?;
        let tree = snapshot_tree(&repository, revision)?;
        let mut records = Vec::new();
        for entry in &tree {
            if entry.name().ok().is_some_and(|name| name.starts_with("r-")) {
                let record = decode_record(&repository, entry.id())?;
                let id = RecordId::from_record(&record);
                verify_record_location(&id, &record, entry.name().ok())?;
                records.push((id, record));
            }
        }
        records.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(records)
    }

    /// Public read path for all records in a snapshot. Used by the transport
    /// merge module.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the repository or record blobs are corrupt.
    pub fn read_records_pub(
        &self,
        revision: &Revision,
    ) -> Result<Vec<(RecordId, StoredRecord)>, StoreError> {
        self.read_records(revision)
    }

    /// Enforce every per-record condition a batch carries.
    ///
    /// A revision agrees on the whole store, which is right for a storage
    /// nothing else writes and wrong the moment content belongs to somebody
    /// else. `expected_content_hash` is the narrower agreement: this write
    /// applies only if the content it was based on is still the content of
    /// record.
    ///
    /// A record that is not there yet satisfies the condition. There is
    /// nothing to disagree with, and a create is not an overwrite — concurrent
    /// creates are still caught by the revision check, which is the mechanism
    /// that owns that question.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] with kind `Conflict`, naming the key, the
    /// expected digest and the one that is stored.
    fn require_expected_content(
        &self,
        current: &Revision,
        transaction: &Transaction,
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
            let Some(stored) = self.read_record_unchecked(current, &id)? else {
                continue;
            };
            let StoredRecord::Plaintext { envelope } = stored;
            if &envelope.content_hash != expected {
                return Err(StoreError::new(
                    StoreErrorKind::Conflict,
                    "content changed since the write was based on it",
                    serde_json::json!({
                        "key": id.display_value(),
                        "expected_content_hash": expected.as_str(),
                        "actual_content_hash": envelope.content_hash.as_str(),
                    }),
                ));
            }
        }
        Ok(())
    }

    /// Read a single record from a revision without requiring it to be in
    /// the local history. Used by the transport merge module.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the repository or record blob is corrupt.
    pub fn read_record_unchecked(
        &self,
        revision: &Revision,
        id: &RecordId,
    ) -> Result<Option<StoredRecord>, StoreError> {
        let repository = self.repository()?;
        let oid = revision.oid()?;
        let commit = repository
            .find_commit(oid)
            .map_err(|error| StoreError::repository("find commit for unchecked read", error))?;
        let tree = commit
            .tree()
            .map_err(|error| StoreError::repository("find tree for unchecked read", error))?;
        let Some(entry) = tree.get_name(&id.tree_name()) else {
            return Ok(None);
        };
        let record = decode_record(&repository, entry.id())?;
        verify_record_location(id, &record, entry.name().ok())?;
        Ok(Some(record))
    }

    /// Read all records from a revision without requiring it to be in the
    /// local history. Used by the transport merge module to read
    /// records from a fetched remote revision.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the repository or record blobs are corrupt.
    pub fn read_records_unchecked(
        &self,
        revision: &Revision,
    ) -> Result<Vec<(RecordId, StoredRecord)>, StoreError> {
        let repository = self.repository()?;
        let oid = revision.oid()?;
        let commit = repository
            .find_commit(oid)
            .map_err(|error| StoreError::repository("find commit for unchecked read", error))?;
        let tree = commit
            .tree()
            .map_err(|error| StoreError::repository("find tree for unchecked read", error))?;
        let mut records = Vec::new();
        for entry in &tree {
            if entry.name().ok().is_some_and(|name| name.starts_with("r-")) {
                let record = decode_record(&repository, entry.id())?;
                let id = RecordId::from_record(&record);
                verify_record_location(&id, &record, entry.name().ok())?;
                records.push((id, record));
            }
        }
        records.sort_by(|left, right| left.0.cmp(&right.0));
        Ok(records)
    }

    fn repository(&self) -> Result<Repository, StoreError> {
        Repository::open(&self.git_dir).map_err(|error| StoreError::repository("open", error))
    }

    /// Make sure the project's ref exists.
    fn ensure_ref(&self) -> Result<(), StoreError> {
        let repository = self.repository()?;
        if reference_target(&repository, MAIN_REF)?.is_some() {
            return Ok(());
        }
        let empty = repository
            .treebuilder(None)
            .and_then(|builder| builder.write())
            .map_err(|error| StoreError::repository("write empty tree", error))?;
        let tree = repository
            .find_tree(empty)
            .map_err(|error| StoreError::repository("find empty tree", error))?;
        let genesis = genesis_commit(&repository, &tree)?;
        match repository.reference(MAIN_REF, genesis, false, "memory-hub: initialize") {
            Ok(_) => Ok(()),
            Err(error) if error.code() == ErrorCode::Exists => Ok(()),
            Err(error) => Err(StoreError::repository("initialize the memory ref", error)),
        }
    }
}

/// Test-only process failpoint used by the public behavioral contract. The
/// marker proves that all new objects exist while the memory ref still points
/// at the previous revision; the contract runner then terminates the process.
fn pause_before_ref_update() -> Result<(), StoreError> {
    let Some(marker) = std::env::var_os(CONTRACT_PAUSE_BEFORE_REF_UPDATE) else {
        return Ok(());
    };
    fs::write(&marker, b"ready").map_err(|error| {
        StoreError::new(
            StoreErrorKind::Repository,
            "write contract failpoint marker",
            serde_json::json!({"detail": error.to_string()}),
        )
    })?;
    loop {
        thread::park_timeout(Duration::from_secs(60));
    }
}

fn validate_transaction(
    transaction: &Transaction,
) -> Result<(BTreeSet<RecordId>, String), StoreError> {
    if transaction.id.trim().is_empty() {
        return Err(StoreError::new(
            StoreErrorKind::InvalidArgument,
            "transaction id must not be empty",
            serde_json::json!({"field": "id"}),
        ));
    }
    let mut ids = BTreeSet::new();
    for operation in &transaction.operations {
        let id = operation.id();
        if matches!(&id, RecordId::Plaintext(key) if key.trim().is_empty()) {
            return Err(StoreError::new(
                StoreErrorKind::InvalidArgument,
                "plaintext record key must not be empty",
                serde_json::json!({"field": "record_id"}),
            ));
        }
        if !ids.insert(id.clone()) {
            return Err(StoreError::new(
                StoreErrorKind::InvalidArgument,
                "a transaction may touch each record only once",
                serde_json::json!({"record": id.display_value()}),
            ));
        }
        if let Operation::Put { record, .. } = operation {
            record.validate().map_err(|error| {
                StoreError::new(
                    StoreErrorKind::InvalidRecord,
                    "record failed canonical validation",
                    serde_json::to_value(error).unwrap_or(serde_json::Value::Null),
                )
            })?;
        }
    }
    let request = serde_json::to_vec(transaction)
        .map_err(|error| serialization_error("serialize transaction", error))?;
    Ok((ids, format!("sha256:{:x}", Sha256::digest(request))))
}

fn conflict_error(transaction: &Transaction, current: Oid, conflicts: Vec<RecordId>) -> StoreError {
    StoreError::new(
        StoreErrorKind::Conflict,
        "records changed since the expected revision",
        serde_json::json!({
            "expected_revision": transaction.expected_revision,
            "current_revision": current.to_string(),
            "conflicting_keys": conflicts
                .iter()
                .map(RecordId::display_value)
                .collect::<Vec<_>>(),
            "recovery_action": "refresh_and_retry",
        }),
    )
}

fn current_oid(repository: &Repository) -> Result<Oid, StoreError> {
    reference_target(repository, MAIN_REF)?.ok_or_else(|| {
        StoreError::new(
            StoreErrorKind::Repository,
            "the memory ref is missing",
            serde_json::json!({"reference": MAIN_REF}),
        )
    })
}

fn reference_target(repository: &Repository, name: &str) -> Result<Option<Oid>, StoreError> {
    match repository.find_reference(name) {
        Ok(reference) => reference.target().map(Some).ok_or_else(|| {
            StoreError::new(
                StoreErrorKind::Repository,
                "Memory ref is symbolic instead of direct",
                serde_json::json!({"reference": name}),
            )
        }),
        Err(error) if error.code() == ErrorCode::NotFound => Ok(None),
        Err(error) => Err(StoreError::repository("read Memory ref", error)),
    }
}

fn is_cas_race(error: &git2::Error) -> bool {
    matches!(
        error.code(),
        ErrorCode::Modified | ErrorCode::Exists | ErrorCode::Locked
    )
}

/// Serialise records into an export bundle of the given mode.
///
/// Separate from [`GitStore::export`] because a snapshot is assembled where
/// locators can be resolved, and only the byte format belongs to the store.
///
/// # Errors
///
/// Returns [`StoreError`] if JSON serialization fails.
pub fn bundle(
    records: Vec<(RecordId, StoredRecord)>,
    mode: ExportMode,
) -> Result<Vec<u8>, StoreError> {
    serde_json::to_vec(&ExportBundle {
        schema_version: EXPORT_SCHEMA_VERSION,
        mode,
        records,
    })
    .map_err(|error| serialization_error("serialize export", error))
}

fn serialization_error(operation: &str, error: serde_json::Error) -> StoreError {
    StoreError::new(
        StoreErrorKind::InvalidRecord,
        format!("{operation} failed"),
        serde_json::json!({"operation": operation, "detail": error.to_string()}),
    )
}
