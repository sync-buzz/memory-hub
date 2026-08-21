//! The synchronous facades are callable from inside an async runtime.
//!
//! A synchronous facade must not build a Tokio runtime on the calling thread:
//! that panics with "cannot start a runtime from within a runtime" the moment a
//! host drives it from an async context — the exact shape an embedding consumer
//! has. Blocking is an acceptable cost of a synchronous signature; panicking is
//! not.

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_index::Projection;
use memory_hub_store::{GitStore, Operation, Transaction};

fn record(key: &str, content: &str) -> Result<StoredRecord, Box<dyn std::error::Error>> {
    Ok(StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, "note", content)?),
    })
}

#[tokio::test]
async fn synchronize_and_search_work_inside_a_runtime() -> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let store = GitStore::open(project.path())?;

    let base = store.current()?;
    store.apply(&Transaction {
        id: "seed".to_owned(),
        expected_revision: base.revision().clone(),
        operations: vec![Operation::put(record("note-1", "projection under async")?)],
    })?;

    let status = Projection::synchronize_store(&store)?;
    assert!(status.indexed_revision.is_some(), "projection caught up");

    let request = memory_hub_index::SearchRequest {
        query: "projection".to_owned(),
        revision: store.current()?.revision().clone(),
        limit: 10,
        offset: 0,
        filters: memory_hub_index::SearchFilters::default(),
    };
    let result = Projection::search_store(&store, &request)?;
    assert_eq!(result.hits.len(), 1, "the seeded record is searchable");

    Ok(())
}
