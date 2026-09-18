use std::collections::BTreeSet;
use std::path::Path;

use git2::{Oid, Repository, Tree};
use memory_hub_core::StoredRecord;

use super::{memory_commit, require_retained_revision, serialization_error};
use crate::error::GitStoreError;
use crate::types::{GitRecordId, GitRevision};
use memory_hub_engine::{ChangeKind, JournalChange};

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
                let bytes = serde_json::to_vec(&stored(record))
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

/// The record as it is kept, with what the history says about it taken back
/// off.
///
/// `created_at_epoch_seconds` and `updated_at_epoch_seconds` are read out of
/// the commit chain and handed to callers on the envelope; they are not part of
/// what a record *is*. Writing them would make the chain describe itself — and
/// worse, a record re-stated verbatim would carry a new time, so its bytes
/// would differ, so the transaction below would count it as a change and put an
/// entry in the history for a write that changed nothing.
///
/// A client cannot reach this: both names are reserved, and a write carrying
/// either is refused before it arrives. This is for the writes that begin with
/// a record this store itself read out — a scan settling a locator, a folder
/// being renamed.
fn stored(record: &StoredRecord) -> StoredRecord {
    let StoredRecord::Plaintext { envelope } = record;
    let mut envelope = envelope.clone();
    envelope.created_at_epoch_seconds = None;
    envelope.updated_at_epoch_seconds = None;
    StoredRecord::Plaintext { envelope }
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

/// What one tree did to the records of another, as far as the store can say.
///
/// One walk serving both the diff and the history, because they ask the same
/// question of two different pairs of trees: the diff compares two revisions a
/// caller named, and the history compares each transaction with its own parent.
/// Two implementations would be two chances to disagree about which paths hold
/// records and which deltas are worth reporting.
///
/// The record is decoded to name its kind and title. That is a blob read per
/// change, and it is the price of a history that can say what was removed: for
/// a deletion there is no version left to read afterwards.
pub(super) fn tree_changes(
    repository: &Repository,
    from: &Tree<'_>,
    to: &Tree<'_>,
) -> Result<Vec<JournalChange>, StoreError> {
    let diff = repository
        .diff_tree_to_tree(Some(from), Some(to), None)
        .map_err(|error| StoreError::repository("diff memory trees", error))?;
    let mut changes = Vec::new();
    for delta in diff.deltas() {
        let (oid, change) = match delta.status() {
            git2::Delta::Added => (delta.new_file().id(), ChangeKind::Added),
            git2::Delta::Deleted => (delta.old_file().id(), ChangeKind::Deleted),
            git2::Delta::Modified => (delta.new_file().id(), ChangeKind::Modified),
            _ => continue,
        };
        let path = match change {
            ChangeKind::Deleted => delta.old_file().path(),
            ChangeKind::Added | ChangeKind::Modified => delta.new_file().path(),
        };
        if !path
            .and_then(Path::to_str)
            .is_some_and(|path| path.starts_with("r-"))
        {
            continue;
        }
        let record = decode_record(repository, oid)?;
        let StoredRecord::Plaintext { envelope } = &record;
        changes.push(JournalChange {
            id: RecordId::from_record(&record),
            change,
            kind: envelope.kind.clone(),
            title: envelope.title.clone(),
        });
    }
    changes.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(changes)
}
