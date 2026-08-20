//! Search must work after an incremental update, not only after a rebuild.
//!
//! The FTS indices are created by `rebuild`, and only when it has rows to index.
//! An incremental `update` adds rows without touching indices, so a projection
//! that was first built empty — which is what a fresh project produces — served
//! searches that failed inside `LanceDB` rather than returning no hits.

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_index::{Projection, SearchFilters, SearchRequest};
use memory_hub_store::{GitStore, Operation, Transaction};

fn record(key: &str, content: &str) -> Result<StoredRecord, Box<dyn std::error::Error>> {
    Ok(StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, "note", content)?),
    })
}

#[tokio::test]
async fn search_works_after_an_index_built_empty_is_updated()
-> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let store = GitStore::open(project.path())?;
    let projection = Projection::open(project.path().join("index")).await?;

    // Build while the store is still empty — the state a fresh project is in.
    let empty = store.current()?;
    projection.rebuild(&empty).await?;

    let after_write = store.apply(&Transaction {
        id: "seed".to_owned(),
        expected_revision: empty.revision().clone(),
        operations: vec![Operation::put(record("note-1", "sidecar integration")?)],
    })?;

    projection
        .update(&store, empty.revision(), &after_write.revision)
        .await?;

    let result = projection
        .search(&SearchRequest {
            query: "sidecar".to_owned(),
            limit: 10,
            offset: 0,
            filters: SearchFilters::default(),
            revision: after_write.revision.clone(),
        })
        .await?;

    assert_eq!(
        result.hits.len(),
        1,
        "the record added after the empty build is searchable"
    );
    Ok(())
}
