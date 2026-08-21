use std::collections::BTreeSet;

use git2::{Commit, Oid, Repository, Signature};
use serde::{Deserialize, Serialize};

use super::{StoreError, StoreErrorKind, serialization_error};
use crate::error::GitStoreError;
use crate::{RecordId, Transaction};

const SCHEMA_VERSION: u32 = 1;

/// Create a Git commit on `refs/memory/*`.
pub(super) fn create_commit(
    repository: &Repository,
    author: &Signature<'_>,
    committer: &Signature<'_>,
    message: &str,
    tree: &git2::Tree<'_>,
    parents: &[&Commit<'_>],
) -> Result<Oid, StoreError> {
    repository
        .commit(None, author, committer, message, tree, parents)
        .map_err(|error| StoreError::repository("write commit", error))
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct TransactionMetadata {
    schema_version: u32,
    kind: TransactionKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) transaction_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(super) request_hash: Option<String>,
    #[serde(default)]
    pub(super) changed_keys: Vec<RecordId>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
enum TransactionKind {
    Genesis,
    Transaction,
}

pub(super) fn genesis_commit(
    repository: &Repository,
    tree: &git2::Tree<'_>,
) -> Result<Oid, StoreError> {
    let metadata = TransactionMetadata {
        schema_version: SCHEMA_VERSION,
        kind: TransactionKind::Genesis,
        transaction_id: None,
        request_hash: None,
        changed_keys: Vec::new(),
    };
    let message = serde_json::to_string(&metadata)
        .map_err(|error| serialization_error("serialize genesis", error))?;
    let signature = Signature::now("Memory Hub", "memory-hub@localhost")
        .map_err(|error| StoreError::repository("create genesis signature", error))?;
    create_commit(
        repository,
        &signature,
        &signature,
        &message,
        tree,
        &[],
    )
}

pub(super) fn transaction_commit(
    repository: &Repository,
    tree_oid: Oid,
    parent: &Commit<'_>,
    transaction: &Transaction,
    request_hash: &str,
    changed_keys: &[RecordId],
) -> Result<Oid, StoreError> {
    let metadata = TransactionMetadata {
        schema_version: SCHEMA_VERSION,
        kind: TransactionKind::Transaction,
        transaction_id: Some(transaction.id.clone()),
        request_hash: Some(request_hash.to_owned()),
        changed_keys: changed_keys.to_vec(),
    };
    let message = serde_json::to_string(&metadata)
        .map_err(|error| serialization_error("serialize transaction commit", error))?;
    let signature = Signature::now("Memory Hub", "memory-hub@localhost")
        .map_err(|error| StoreError::repository("create transaction signature", error))?;
    let tree = repository
        .find_tree(tree_oid)
        .map_err(|error| StoreError::repository("find transaction tree", error))?;
    create_commit(
        repository,
        &signature,
        &signature,
        &message,
        &tree,
        &[parent],
    )
}

pub(super) fn memory_commit(repository: &Repository, oid: Oid) -> Result<Commit<'_>, StoreError> {
    let commit = repository.find_commit(oid).map_err(|error| {
        StoreError::new(
            StoreErrorKind::RevisionNotFound,
            "Memory revision is not a commit",
            serde_json::json!({"revision": oid.to_string()}),
        )
        .with_repository_context(error)
    })?;
    let metadata = transaction_metadata(&commit)?;
    let valid_shape = match metadata.kind {
        TransactionKind::Genesis => {
            commit.parent_count() == 0
                && metadata.transaction_id.is_none()
                && metadata.request_hash.is_none()
                && metadata.changed_keys.is_empty()
        }
        TransactionKind::Transaction => {
            commit.parent_count() == 1
                && metadata
                    .transaction_id
                    .as_deref()
                    .is_some_and(|id| !id.trim().is_empty())
                && metadata.request_hash.as_deref().is_some_and(|hash| {
                    hash.len() == 71
                        && hash.starts_with("sha256:")
                        && hash[7..]
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
        }
    };
    if !valid_shape {
        return Err(StoreError::new(
            StoreErrorKind::RevisionNotFound,
            "Memory revision has an invalid commit shape",
            serde_json::json!({"revision": oid.to_string()}),
        ));
    }
    commit
        .tree()
        .map_err(|error| StoreError::repository("find Memory revision tree", error))?;
    Ok(commit)
}

fn transaction_metadata(commit: &Commit<'_>) -> Result<TransactionMetadata, StoreError> {
    let metadata: TransactionMetadata =
        serde_json::from_slice(commit.message_bytes()).map_err(|error| {
            StoreError::new(
                StoreErrorKind::RevisionNotFound,
                "commit is not a Memory revision",
                serde_json::json!({
                    "revision": commit.id().to_string(),
                    "detail": error.to_string(),
                }),
            )
        })?;
    if metadata.schema_version != SCHEMA_VERSION {
        return Err(StoreError::new(
            StoreErrorKind::RevisionNotFound,
            "Memory revision has an unsupported schema version",
            serde_json::json!({"revision": commit.id().to_string()}),
        ));
    }
    Ok(metadata)
}

pub(super) fn require_retained_revision(
    repository: &Repository,
    expected: Oid,
) -> Result<(), StoreError> {
    let mut cursor = super::current_oid(repository)?;
    loop {
        let commit = memory_commit(repository, cursor)?;
        if cursor == expected {
            return Ok(());
        }
        cursor = commit.parent_id(0).map_err(|_| {
            StoreError::new(
                StoreErrorKind::RevisionNotFound,
                "revision is not retained by the current Memory history",
                serde_json::json!({"revision": expected.to_string()}),
            )
        })?;
    }
}

pub(super) fn changes_since(
    repository: &Repository,
    expected: Oid,
    current: Oid,
) -> Result<BTreeSet<RecordId>, StoreError> {
    let mut cursor = current;
    let mut changed = BTreeSet::new();
    while cursor != expected {
        let commit = memory_commit(repository, cursor)?;
        let metadata = transaction_metadata(&commit)?;
        changed.extend(metadata.changed_keys);
        cursor = commit.parent_id(0).map_err(|_| {
            StoreError::new(
                StoreErrorKind::RevisionNotFound,
                "expected revision is not an ancestor of current Memory",
                serde_json::json!({"expected_revision": expected.to_string()}),
            )
        })?;
    }
    Ok(changed)
}

pub(super) fn find_transaction(
    repository: &Repository,
    mut cursor: Oid,
    transaction_id: &str,
) -> Result<Option<(Oid, TransactionMetadata)>, StoreError> {
    loop {
        let commit = memory_commit(repository, cursor)?;
        let metadata = transaction_metadata(&commit)?;
        if metadata.transaction_id.as_deref() == Some(transaction_id) {
            return Ok(Some((cursor, metadata)));
        }
        let Ok(parent) = commit.parent_id(0) else {
            return Ok(None);
        };
        cursor = parent;
    }
}
