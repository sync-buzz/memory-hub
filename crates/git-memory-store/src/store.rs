// serde/git2 errors are mapped at one-shot ownership boundaries.
#![allow(clippy::needless_pass_by_value)]

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::Duration;

use git_memory_core::StoredRecord;
use git2::{ErrorCode, Oid, Repository, Signature};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ApplyResult, ChangeKind, Checkpoint, ExportBundle, MAIN_REF, Operation, RecordChange, RecordId,
    Revision, STAGED_REF, Snapshot, StoreError, StoreErrorKind, Transaction,
};

const MAX_CAS_ATTEMPTS: usize = 32;
const EXPORT_SCHEMA_VERSION: u32 = 1;
const CHECKPOINT_SCHEMA_VERSION: u32 = 1;
const CONTRACT_PAUSE_BEFORE_REF_UPDATE: &str = "GIT_MEMORY_CONTRACT_PAUSE_BEFORE_REF_UPDATE";

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
}

#[derive(Debug, Deserialize, Serialize)]
struct CheckpointMetadata {
    schema_version: u32,
    kind: String,
    revision: Revision,
    message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    code_revision: Option<String>,
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
    /// the private staged ref when needed.
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
        memory_commit(&repository, revision)?;
        Ok(Snapshot {
            git_dir: self.git_dir.clone(),
            revision: Revision::from_oid(revision),
        })
    }

    /// Open any retained Memory commit revision as an immutable snapshot.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the revision is malformed, missing, or is not
    /// part of the staged transaction history.
    pub fn snapshot(&self, revision: &Revision) -> Result<Snapshot, StoreError> {
        let repository = self.repository()?;
        require_retained_revision(&repository, revision.oid()?)?;
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
        self.checkpoint_inner(message, None)
    }

    /// Checkpoint the current staged snapshot against one processed code
    /// commit. Repeating the newest code revision is idempotent, which lets a
    /// reconciler recover after a cursor write is interrupted.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the code revision is malformed, missing, or
    /// checkpoint creation/ref CAS fails.
    pub fn checkpoint_code(
        &self,
        code_revision: &str,
        message: &str,
    ) -> Result<Checkpoint, StoreError> {
        let repository = self.repository()?;
        let code_oid = Oid::from_str(code_revision).map_err(|_| {
            StoreError::new(
                StoreErrorKind::InvalidArgument,
                "code revision is not a Git object id",
                serde_json::json!({"field": "code_revision"}),
            )
        })?;
        repository
            .find_commit(code_oid)
            .map_err(|error| StoreError::repository("find code revision", error))?;
        if let Some(existing) = self.history(1)?.into_iter().next()
            && existing.code_revision.as_deref() == Some(code_revision)
        {
            return Ok(existing);
        }
        self.checkpoint_inner(message, Some(code_revision))
    }

    fn checkpoint_inner(
        &self,
        message: &str,
        code_revision: Option<&str>,
    ) -> Result<Checkpoint, StoreError> {
        if message.trim().is_empty() {
            return Err(StoreError::new(
                StoreErrorKind::InvalidArgument,
                "checkpoint message must not be empty",
                serde_json::json!({"field": "message"}),
            ));
        }
        let repository = self.repository()?;
        let revision_oid = current_oid(&repository)?;
        let revision = Revision::from_oid(revision_oid);
        let staged = memory_commit(&repository, revision_oid)?;
        let tree = staged
            .tree()
            .map_err(|error| StoreError::repository("find checkpoint tree", error))?;
        let signature = Signature::now("Git Memory", "git-memory@localhost")
            .map_err(|error| StoreError::repository("create checkpoint signature", error))?;
        let parent_oid = reference_target(&repository, MAIN_REF)?;
        let parent = parent_oid
            .map(|oid| repository.find_commit(oid))
            .transpose()
            .map_err(|error| StoreError::repository("find checkpoint parent", error))?;
        let parents = parent.iter().collect::<Vec<_>>();
        let metadata = CheckpointMetadata {
            schema_version: CHECKPOINT_SCHEMA_VERSION,
            kind: "checkpoint".into(),
            revision: revision.clone(),
            message: message.to_owned(),
            code_revision: code_revision.map(str::to_owned),
        };
        let commit_message = serde_json::to_string(&metadata)
            .map_err(|error| serialization_error("serialize checkpoint", error))?;
        let commit_oid = repository
            .commit(
                None,
                &signature,
                &signature,
                &commit_message,
                &tree,
                &parents,
            )
            .map_err(|error| StoreError::repository("write checkpoint commit", error))?;
        update_optional_ref(&repository, MAIN_REF, commit_oid, parent_oid)?;
        Ok(Checkpoint {
            commit: commit_oid.to_string(),
            revision,
            message: message.to_owned(),
            timestamp: signature.when().seconds(),
            code_revision: code_revision.map(str::to_owned),
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
            let metadata: CheckpointMetadata = serde_json::from_slice(commit.message_bytes())
                .map_err(|error| serialization_error("decode checkpoint", error))?;
            if metadata.schema_version != CHECKPOINT_SCHEMA_VERSION || metadata.kind != "checkpoint"
            {
                return Err(StoreError::new(
                    StoreErrorKind::Repository,
                    "checkpoint commit has an unsupported shape",
                    serde_json::json!({"commit": oid.to_string()}),
                ));
            }
            result.push(Checkpoint {
                commit: oid.to_string(),
                revision: metadata.revision,
                message: metadata.message,
                timestamp: commit.time().seconds(),
                code_revision: metadata.code_revision,
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
        let tree = repository
            .find_tree(empty)
            .map_err(|error| StoreError::repository("find empty tree", error))?;
        let genesis = genesis_commit(&repository, &tree)?;
        match repository.reference(STAGED_REF, genesis, false, "git-memory: initialize") {
            Ok(_) => Ok(()),
            Err(error) if error.code() == ErrorCode::Exists => Ok(()),
            Err(error) => Err(StoreError::repository("initialize staged ref", error)),
        }
    }
}

/// Test-only process failpoint used by the public behavioral contract. The
/// marker proves that all new objects exist while the staged ref still points
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
