//! Finishing what a dead process started.
//!
//! A transaction touches several files, and renaming several files is not one
//! operation. So the plan is written down first, in one file that arrives by
//! rename, and only then carried out. A process that dies in the middle leaves
//! the plan behind; the next `open` finds it and finishes it.
//!
//! Every step is idempotent — write this file, remove that one — so finishing a
//! plan twice is the same as finishing it once, which is what makes recovery
//! safe to retry.

use std::fs;

use memory_hub_core::StoredRecord;
use memory_hub_engine::{
    ApplyResult, Operation, RecordId, StoreError, StoreErrorKind, Transaction,
};
use serde::{Deserialize, Serialize};

use crate::layout::{Layout, io, write_atomic};

/// The plan file, next to the records it is about to change.
const JOURNAL_FILE: &str = "pending.json";

/// One record's fate in a plan.
#[derive(Debug, Deserialize, Serialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum Planned {
    Write { id: RecordId, record: StoredRecord },
    Remove { id: RecordId },
}

/// A transaction that has been decided and not yet carried out.
#[derive(Debug, Deserialize, Serialize)]
pub(crate) struct Journal {
    transaction_id: String,
    steps: Vec<Planned>,
    result: ApplyResult,
}

impl Journal {
    /// Write the plan down. Nothing on disk has changed yet when this returns.
    pub(crate) fn write(
        layout: &Layout,
        transaction: &Transaction,
        result: &ApplyResult,
    ) -> Result<(), StoreError> {
        let steps = transaction
            .operations
            .iter()
            .map(|operation| match operation {
                Operation::Put { record, .. } => Planned::Write {
                    id: RecordId::from_record(record),
                    record: record.clone(),
                },
                Operation::Delete { id } => Planned::Remove { id: id.clone() },
            })
            .collect();
        let journal = Self {
            transaction_id: transaction.id.clone(),
            steps,
            result: result.clone(),
        };
        let bytes = serde_json::to_vec_pretty(&journal).map_err(|error| {
            StoreError::new(
                StoreErrorKind::Repository,
                "the transaction plan could not be written",
                serde_json::json!({"detail": error.to_string()}),
            )
        })?;
        write_atomic(&layout.path_in_root(JOURNAL_FILE), &bytes)
    }

    /// Carry out the plan on disk and take it down.
    pub(crate) fn commit(layout: &Layout) -> Result<(), StoreError> {
        let path = layout.path_in_root(JOURNAL_FILE);
        let Ok(text) = fs::read_to_string(&path) else {
            return Ok(());
        };
        let journal: Self = serde_json::from_str(&text).map_err(|error| {
            StoreError::new(
                StoreErrorKind::Repository,
                "an interrupted transaction left a plan that cannot be read",
                serde_json::json!({
                    "path": path.display().to_string(),
                    "detail": error.to_string(),
                }),
            )
        })?;

        for step in &journal.steps {
            match step {
                Planned::Write { id, record } => {
                    let file = layout.record_file(id)?;
                    let bytes = serde_json::to_vec_pretty(record).map_err(|error| {
                        StoreError::new(
                            StoreErrorKind::InvalidRecord,
                            "a record could not be written as JSON",
                            serde_json::json!({
                                "id": id.display_value(),
                                "detail": error.to_string(),
                            }),
                        )
                    })?;
                    write_atomic(&file, &bytes)?;
                }
                Planned::Remove { id } => {
                    let file = layout.record_file(id)?;
                    match fs::remove_file(&file) {
                        Ok(()) => {}
                        // Already gone is the state the plan asked for.
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(io(&file, "remove", &error)),
                    }
                    prune_empty_parents(layout, &file);
                }
            }
        }

        layout.remember(&journal.transaction_id, &journal.result)?;
        match fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(io(&path, "remove", &error)),
        }
    }

    /// Finish an interrupted transaction, if one was interrupted.
    pub(crate) fn recover(layout: &Layout) -> Result<(), StoreError> {
        if !layout.path_in_root(JOURNAL_FILE).exists() {
            return Ok(());
        }
        let _guard = layout.lock()?;
        Self::commit(layout)
    }
}

/// Remove directories a delete emptied, up to the records folder.
///
/// Without this the tree keeps the shape of records that no longer exist, and a
/// person opening the folder reads a hierarchy that is not there any more.
fn prune_empty_parents(layout: &Layout, file: &std::path::Path) {
    let base = layout.records_dir();
    let mut current = file.parent().map(std::path::Path::to_path_buf);
    while let Some(directory) = current {
        if directory == base || !directory.starts_with(&base) {
            return;
        }
        if fs::remove_dir(&directory).is_err() {
            return;
        }
        current = directory.parent().map(std::path::Path::to_path_buf);
    }
}
