// serde/git2 errors are mapped at one-shot ownership boundaries.
#![allow(clippy::needless_pass_by_value)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use git_memory_core::StoredRecord;
use git2::{ErrorCode, Oid, Repository, Signature, Tree};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ApplyResult, ChangeKind, Checkpoint, ExportBundle, MAIN_REF, Operation, RecordChange, RecordId,
    Revision, STAGED_REF, Snapshot, StoreError, StoreErrorKind, Transaction,
};

const FILE_MODE: i32 = 0o100_644;
const TREE_MODE: i32 = 0o040_000;
const PREVIOUS_TREE: &str = "previous";
const MAX_CAS_ATTEMPTS: usize = 32;
const EXPORT_SCHEMA_VERSION: u32 = 1;

#[derive(Clone, Debug)]
pub struct GitStore {
    git_dir: PathBuf,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct Receipt {
    request_hash: String,
    changed_keys: Vec<String>,
}

impl GitStore {
    /// Discover a repository and initialize the private staged ref when needed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if no repository can be opened or its empty tree
    /// cannot be created/referenced.
    pub fn open(project: impl AsRef<Path>) -> Result<Self, StoreError> {
        let repository = Repository::discover(project.as_ref())
            .map_err(|error| StoreError::repository("discover", error))?;
        let git_dir = repository.path().to_path_buf();
        let store = Self { git_dir };
        store.ensure_staged()?;
        Ok(store)
    }

    pub(crate) const fn from_git_dir(git_dir: PathBuf) -> Self {
        Self { git_dir }
    }

    /// Return the current immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the staged ref cannot be read.
    pub fn current(&self) -> Result<Snapshot, StoreError> {
        let repository = self.repository()?;
        let revision = current_oid(&repository)?;
        Ok(Snapshot {
            git_dir: self.git_dir.clone(),
            revision: Revision::from_oid(revision),
        })
    }

    /// Open any retained tree revision as an immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the revision is malformed, missing, or is not
    /// a tree.
    pub fn snapshot(&self, revision: &Revision) -> Result<Snapshot, StoreError> {
        let repository = self.repository()?;
        find_tree(&repository, revision)?;
        Ok(Snapshot {
            git_dir: self.git_dir.clone(),
            revision: revision.clone(),
        })
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
        let (ids, request_hash) = validate_transaction(transaction)?;
        let mut changed_keys = ids.iter().map(RecordId::display_value).collect::<Vec<_>>();
        changed_keys.sort();
        let repository = self.repository()?;
        let expected_oid = transaction.expected_revision.oid()?;
        repository.find_tree(expected_oid).map_err(|error| {
            StoreError::new(
                StoreErrorKind::RevisionNotFound,
                "expected revision does not exist",
                serde_json::json!({"expected_revision": transaction.expected_revision}),
            )
            .with_repository_context(error)
        })?;

        for _ in 0..MAX_CAS_ATTEMPTS {
            let current_oid = current_oid(&repository)?;
            let current_tree = repository
                .find_tree(current_oid)
                .map_err(|error| StoreError::repository("find current tree", error))?;

            if let Some(receipt) = read_receipt(&repository, &current_tree, &transaction.id)? {
                if receipt.request_hash == request_hash {
                    return Ok(ApplyResult {
                        revision: Revision::from_oid(current_oid),
                        changed_keys: receipt.changed_keys,
                    });
                }
                return Err(StoreError::new(
                    StoreErrorKind::TransactionReused,
                    "transaction id was already used for a different request",
                    serde_json::json!({"transaction_id": transaction.id}),
                ));
            }

            if expected_oid != current_oid {
                let expected_tree = repository.find_tree(expected_oid).map_err(|error| {
                    StoreError::repository("find expected tree during rebase", error)
                })?;
                let changed = changed_ids(&repository, &expected_tree, &current_tree)?;
                let conflicts = ids.intersection(&changed).cloned().collect::<Vec<_>>();
                if !conflicts.is_empty() {
                    return Err(StoreError::new(
                        StoreErrorKind::Conflict,
                        "records changed since the expected revision",
                        serde_json::json!({
                            "expected_revision": transaction.expected_revision,
                            "current_revision": current_oid.to_string(),
                            "conflicting_keys": conflicts.iter().map(RecordId::display_value).collect::<Vec<_>>(),
                            "recovery_action": "refresh_and_retry",
                        }),
                    ));
                }
            }

            let new_oid = build_tree(
                &repository,
                &current_tree,
                transaction,
                &request_hash,
                &changed_keys,
            )?;
            match repository.reference_matching(
                STAGED_REF,
                new_oid,
                true,
                current_oid,
                "git-memory: apply transaction",
            ) {
                Ok(_) => {
                    return Ok(ApplyResult {
                        revision: Revision::from_oid(new_oid),
                        changed_keys: changed_keys.clone(),
                    });
                }
                Err(error) if is_cas_race(&error) => {}
                Err(error) => return Err(StoreError::repository("update staged ref", error)),
            }
        }
        Err(StoreError::new(
            StoreErrorKind::RetryExhausted,
            "staged ref kept changing during transaction",
            serde_json::json!({"attempts": MAX_CAS_ATTEMPTS}),
        ))
    }

    /// Create a commit on `refs/memory/main` whose tree is the current staged
    /// snapshot. HEAD and code refs are untouched.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if commit creation or main-ref CAS fails.
    pub fn checkpoint(&self, message: &str) -> Result<Checkpoint, StoreError> {
        if message.trim().is_empty() {
            return Err(StoreError::new(
                StoreErrorKind::InvalidArgument,
                "checkpoint message must not be empty",
                serde_json::json!({"field": "message"}),
            ));
        }
        let repository = self.repository()?;
        let tree_oid = current_oid(&repository)?;
        let tree = repository
            .find_tree(tree_oid)
            .map_err(|error| StoreError::repository("find checkpoint tree", error))?;
        let signature = Signature::now("Git Memory", "git-memory@localhost")
            .map_err(|error| StoreError::repository("create checkpoint signature", error))?;
        let parent_oid = reference_target(&repository, MAIN_REF)?;
        let parent = parent_oid
            .map(|oid| repository.find_commit(oid))
            .transpose()
            .map_err(|error| StoreError::repository("find checkpoint parent", error))?;
        let parents = parent.iter().collect::<Vec<_>>();
        let commit_oid = repository
            .commit(None, &signature, &signature, message, &tree, &parents)
            .map_err(|error| StoreError::repository("write checkpoint commit", error))?;
        update_optional_ref(&repository, MAIN_REF, commit_oid, parent_oid)?;
        Ok(Checkpoint {
            commit: commit_oid.to_string(),
            revision: Revision::from_oid(tree_oid),
            message: message.to_owned(),
            timestamp: signature.when().seconds(),
        })
    }

    /// Walk checkpoint history newest-first.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if checkpoint objects are corrupt.
    pub fn history(&self, limit: usize) -> Result<Vec<Checkpoint>, StoreError> {
        let repository = self.repository()?;
        let Some(mut oid) = reference_target(&repository, MAIN_REF)? else {
            return Ok(Vec::new());
        };
        let mut result = Vec::new();
        while result.len() < limit {
            let commit = repository
                .find_commit(oid)
                .map_err(|error| StoreError::repository("read checkpoint commit", error))?;
            result.push(Checkpoint {
                commit: oid.to_string(),
                revision: Revision::from_oid(commit.tree_id()),
                message: commit.message().unwrap_or_default().to_owned(),
                timestamp: commit.time().seconds(),
            });
            let Ok(parent) = commit.parent_id(0) else {
                break;
            };
            oid = parent;
        }
        Ok(result)
    }

    /// Compare record identities between two immutable snapshots.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if either revision or record blob is invalid.
    pub fn diff(&self, from: &Revision, to: &Revision) -> Result<Vec<RecordChange>, StoreError> {
        let repository = self.repository()?;
        let from_records = record_oids(&repository, &find_tree(&repository, from)?)?;
        let to_records = record_oids(&repository, &find_tree(&repository, to)?)?;
        let ids = from_records
            .keys()
            .chain(to_records.keys())
            .cloned()
            .collect::<BTreeSet<_>>();
        Ok(ids
            .into_iter()
            .filter_map(|id| match (from_records.get(&id), to_records.get(&id)) {
                (None, Some(_)) => Some(RecordChange {
                    id,
                    kind: ChangeKind::Added,
                }),
                (Some(_), None) => Some(RecordChange {
                    id,
                    kind: ChangeKind::Deleted,
                }),
                (Some(left), Some(right)) if left != right => Some(RecordChange {
                    id,
                    kind: ChangeKind::Modified,
                }),
                _ => None,
            })
            .collect())
    }

    /// Export a deterministic record-only JSON bundle.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the snapshot is corrupt or JSON serialization
    /// fails.
    pub fn export(&self, revision: &Revision) -> Result<Vec<u8>, StoreError> {
        let records = self.read_records(revision)?;
        serde_json::to_vec(&ExportBundle {
            schema_version: EXPORT_SCHEMA_VERSION,
            records,
        })
        .map_err(|error| serialization_error("serialize export", error))
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
        if bundle.schema_version != EXPORT_SCHEMA_VERSION {
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
        self.apply(&Transaction {
            id: transaction_id.into(),
            expected_revision,
            operations,
        })
    }

    pub(crate) fn read_record(
        &self,
        revision: &Revision,
        id: &RecordId,
    ) -> Result<Option<StoredRecord>, StoreError> {
        let repository = self.repository()?;
        let tree = find_tree(&repository, revision)?;
        let Some(entry) = tree.get_name(&id.tree_name()) else {
            return Ok(None);
        };
        let record = decode_record(&repository, entry.id())?;
        verify_record_location(id, &record, entry.name().ok())?;
        Ok(Some(record))
    }

    pub(crate) fn read_records(
        &self,
        revision: &Revision,
    ) -> Result<Vec<(RecordId, StoredRecord)>, StoreError> {
        let repository = self.repository()?;
        let tree = find_tree(&repository, revision)?;
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

    fn ensure_staged(&self) -> Result<(), StoreError> {
        let repository = self.repository()?;
        if reference_target(&repository, STAGED_REF)?.is_some() {
            return Ok(());
        }
        let empty = repository
            .treebuilder(None)
            .and_then(|builder| builder.write())
            .map_err(|error| StoreError::repository("write empty tree", error))?;
        match repository.reference(STAGED_REF, empty, false, "git-memory: initialize") {
            Ok(_) => Ok(()),
            Err(error) if error.code() == ErrorCode::Exists => Ok(()),
            Err(error) => Err(StoreError::repository("initialize staged ref", error)),
        }
    }
}

impl StoreError {
    fn with_repository_context(mut self, error: git2::Error) -> Self {
        self.data["git_code"] =
            serde_json::json!(format!("{:?}", error.code()).to_ascii_lowercase());
        self
    }
}

fn validate_transaction(
    transaction: &Transaction,
) -> Result<(BTreeSet<RecordId>, String), StoreError> {
    if transaction.id.trim().is_empty() || transaction.operations.is_empty() {
        return Err(StoreError::new(
            StoreErrorKind::InvalidArgument,
            "transaction id and operations must not be empty",
            serde_json::json!({"field": if transaction.id.trim().is_empty() {"id"} else {"operations"}}),
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
        if let Operation::Put { record } = operation {
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

fn build_tree(
    repository: &Repository,
    base: &Tree<'_>,
    transaction: &Transaction,
    request_hash: &str,
    changed_keys: &[String],
) -> Result<Oid, StoreError> {
    let mut builder = repository
        .treebuilder(Some(base))
        .map_err(|error| StoreError::repository("open tree builder", error))?;
    builder
        .insert(PREVIOUS_TREE, base.id(), TREE_MODE)
        .map_err(|error| StoreError::repository("retain previous snapshot", error))?;
    for operation in &transaction.operations {
        let id = operation.id();
        let name = id.tree_name();
        match operation {
            Operation::Put { record } => {
                let bytes = serde_json::to_vec(record)
                    .map_err(|error| serialization_error("serialize record", error))?;
                let blob = repository
                    .blob(&bytes)
                    .map_err(|error| StoreError::repository("write record blob", error))?;
                builder
                    .insert(&name, blob, FILE_MODE)
                    .map_err(|error| StoreError::repository("insert record", error))?;
            }
            Operation::Delete { .. } => {
                if builder
                    .get(&name)
                    .map_err(|error| StoreError::repository("find record to delete", error))?
                    .is_some()
                {
                    builder
                        .remove(&name)
                        .map_err(|error| StoreError::repository("delete record", error))?;
                }
            }
        }
    }

    let receipt = Receipt {
        request_hash: request_hash.to_owned(),
        changed_keys: changed_keys.to_vec(),
    };
    let receipt_bytes = serde_json::to_vec(&receipt)
        .map_err(|error| serialization_error("serialize transaction receipt", error))?;
    let receipt_blob = repository
        .blob(&receipt_bytes)
        .map_err(|error| StoreError::repository("write transaction receipt", error))?;
    builder
        .insert(transaction_name(&transaction.id), receipt_blob, FILE_MODE)
        .map_err(|error| StoreError::repository("insert transaction receipt", error))?;
    builder
        .write()
        .map_err(|error| StoreError::repository("write transaction tree", error))
}

fn read_receipt(
    repository: &Repository,
    tree: &Tree<'_>,
    transaction_id: &str,
) -> Result<Option<Receipt>, StoreError> {
    let Some(entry) = tree.get_name(&transaction_name(transaction_id)) else {
        return Ok(None);
    };
    let blob = repository
        .find_blob(entry.id())
        .map_err(|error| StoreError::repository("read transaction receipt", error))?;
    serde_json::from_slice(blob.content())
        .map(Some)
        .map_err(|error| serialization_error("decode transaction receipt", error))
}

fn transaction_name(id: &str) -> String {
    format!("t-{:x}", Sha256::digest(id.as_bytes()))
}

fn changed_ids(
    repository: &Repository,
    from: &Tree<'_>,
    to: &Tree<'_>,
) -> Result<BTreeSet<RecordId>, StoreError> {
    let left = record_oids(repository, from)?;
    let right = record_oids(repository, to)?;
    Ok(left
        .keys()
        .chain(right.keys())
        .filter(|id| left.get(*id) != right.get(*id))
        .cloned()
        .collect())
}

fn record_oids(
    repository: &Repository,
    tree: &Tree<'_>,
) -> Result<BTreeMap<RecordId, Oid>, StoreError> {
    let mut result = BTreeMap::new();
    for entry in tree {
        if entry.name().ok().is_some_and(|name| name.starts_with("r-")) {
            let record = decode_record(repository, entry.id())?;
            let id = RecordId::from_record(&record);
            verify_record_location(&id, &record, entry.name().ok())?;
            if result.insert(id.clone(), entry.id()).is_some() {
                return Err(StoreError::new(
                    StoreErrorKind::InvalidRecord,
                    "duplicate logical record in snapshot",
                    serde_json::json!({"record": id.display_value()}),
                ));
            }
        }
    }
    Ok(result)
}

fn decode_record(repository: &Repository, oid: Oid) -> Result<StoredRecord, StoreError> {
    let blob = repository
        .find_blob(oid)
        .map_err(|error| StoreError::repository("read record blob", error))?;
    serde_json::from_slice(blob.content())
        .map_err(|error| serialization_error("decode record", error))
}

fn verify_record_location(
    expected: &RecordId,
    record: &StoredRecord,
    tree_name: Option<&str>,
) -> Result<(), StoreError> {
    let actual = RecordId::from_record(record);
    if actual == *expected && tree_name == Some(expected.tree_name().as_str()) {
        return Ok(());
    }
    Err(StoreError::new(
        StoreErrorKind::InvalidRecord,
        "record payload does not match its tree location",
        serde_json::json!({
            "expected": expected.display_value(),
            "actual": actual.display_value(),
            "tree_name": tree_name,
        }),
    ))
}

fn find_tree<'repo>(
    repository: &'repo Repository,
    revision: &Revision,
) -> Result<Tree<'repo>, StoreError> {
    let oid = revision.oid()?;
    repository.find_tree(oid).map_err(|error| {
        StoreError::new(
            StoreErrorKind::RevisionNotFound,
            "revision tree does not exist",
            serde_json::json!({"revision": revision}),
        )
        .with_repository_context(error)
    })
}

fn current_oid(repository: &Repository) -> Result<Oid, StoreError> {
    reference_target(repository, STAGED_REF)?.ok_or_else(|| {
        StoreError::new(
            StoreErrorKind::Repository,
            "staged ref is missing",
            serde_json::json!({"reference": STAGED_REF}),
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

fn update_optional_ref(
    repository: &Repository,
    name: &str,
    target: Oid,
    previous: Option<Oid>,
) -> Result<(), StoreError> {
    let result = if let Some(previous) = previous {
        repository.reference_matching(name, target, true, previous, "git-memory: checkpoint")
    } else {
        repository.reference(name, target, false, "git-memory: first checkpoint")
    };
    result
        .map(drop)
        .map_err(|error| StoreError::repository("update checkpoint ref", error))
}

fn is_cas_race(error: &git2::Error) -> bool {
    matches!(
        error.code(),
        ErrorCode::Modified | ErrorCode::Exists | ErrorCode::Locked
    )
}

fn serialization_error(operation: &str, error: serde_json::Error) -> StoreError {
    StoreError::new(
        StoreErrorKind::InvalidRecord,
        format!("{operation} failed"),
        serde_json::json!({"operation": operation, "detail": error.to_string()}),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use git_memory_core::{Envelope, StoredRecord};
    use sha2::Digest;

    use super::{GitStore, Operation, Transaction, build_tree, current_oid};
    use crate::RecordId;

    #[test]
    fn objects_written_before_ref_cas_are_safe_to_abandon_and_retry() {
        let directory = tempfile::tempdir().unwrap();
        git2::Repository::init(directory.path()).unwrap();
        let store = GitStore::open(directory.path()).unwrap();
        let before = store.current().unwrap().revision().clone();
        let transaction = Transaction {
            id: "interrupted".into(),
            expected_revision: before.clone(),
            operations: vec![Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(Envelope::new("recoverable", "note", "complete").unwrap()),
            })],
        };
        let repository = store.repository().unwrap();
        let current = current_oid(&repository).unwrap();
        let tree = repository.find_tree(current).unwrap();
        let request = serde_json::to_vec(&transaction).unwrap();
        let request_hash = format!("sha256:{:x}", sha2::Sha256::digest(request));

        let abandoned = build_tree(
            &repository,
            &tree,
            &transaction,
            &request_hash,
            &["recoverable".into()],
        )
        .unwrap();
        assert_ne!(abandoned.to_string(), before.as_str());
        assert_eq!(store.current().unwrap().revision(), &before);
        drop(tree);
        drop(repository);

        let result = store.apply(&transaction).unwrap();
        assert!(
            store
                .snapshot(&result.revision)
                .unwrap()
                .get(&RecordId::plaintext("recoverable"))
                .unwrap()
                .is_some()
        );
    }
}
