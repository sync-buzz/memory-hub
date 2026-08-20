use std::collections::BTreeSet;

use git2::{Oid, Repository, Tree};
use memory_hub_core::StoredRecord;

use super::{memory_commit, require_retained_revision, serialization_error};
use crate::error::GitStoreError;
use crate::types::{GitRecordId, GitRevision};
use crate::{Operation, RecordId, Revision, StoreError, StoreErrorKind, Transaction};

const FILE_MODE: i32 = 0o100_644;

pub(super) fn build_tree(
    repository: &Repository,
    base: &Tree<'_>,
    transaction: &Transaction,
) -> Result<(Oid, BTreeSet<RecordId>), StoreError> {
    let mut builder = repository
        .treebuilder(Some(base))
        .map_err(|error| StoreError::repository("open tree builder", error))?;
    let mut changed = BTreeSet::new();
    for operation in &transaction.operations {
        let id = operation.id();
        let name = id.tree_name();
        match operation {
            Operation::Put { record, .. } => {
                let bytes = serde_json::to_vec(record)
                    .map_err(|error| serialization_error("serialize record", error))?;
                let blob = repository
                    .blob(&bytes)
                    .map_err(|error| StoreError::repository("write record blob", error))?;
                let existing = builder
                    .get(&name)
                    .map_err(|error| StoreError::repository("find existing record", error))?
                    .map(|entry| entry.id());
                if existing != Some(blob) {
                    builder
                        .insert(&name, blob, FILE_MODE)
                        .map_err(|error| StoreError::repository("insert record", error))?;
                    changed.insert(id);
                }
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
                    changed.insert(id);
                }
            }
        }
    }
    let tree = builder
        .write()
        .map_err(|error| StoreError::repository("write transaction tree", error))?;
    Ok((tree, changed))
}

pub(super) fn decode_record(repository: &Repository, oid: Oid) -> Result<StoredRecord, StoreError> {
    let blob = repository
        .find_blob(oid)
        .map_err(|error| StoreError::repository("read record blob", error))?;
    serde_json::from_slice(blob.content())
        .map_err(|error| serialization_error("decode record", error))
}

pub(super) fn verify_record_location(
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

pub(super) fn snapshot_tree<'repo>(
    repository: &'repo Repository,
    revision: &Revision,
) -> Result<Tree<'repo>, StoreError> {
    let oid = revision.oid()?;
    require_retained_revision(repository, oid)?;
    memory_commit(repository, oid)?
        .tree()
        .map_err(|error| StoreError::repository("read snapshot tree", error))
}
