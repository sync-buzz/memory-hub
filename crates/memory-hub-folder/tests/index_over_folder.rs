//! The seam is only real if the read model works over something that is not Git.
//!
//! `memory-hub-index` was written against the storage contract, not against the
//! Git store. This is the test that says so out loud: the same projection, the
//! same search, over a folder of files that has no history, no refs and no
//! commits.

#![allow(clippy::unwrap_used)]

use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_engine::{Operation, RecordStore, StoreView, Transaction};
use memory_hub_folder::FolderStore;
use memory_hub_index::{Projection, SearchRequest};

fn note(key: &str, title: &str, content: &str) -> StoredRecord {
    let mut envelope = Envelope::new(key, "note", content).unwrap();
    envelope.title = Some(title.to_owned());
    StoredRecord::Plaintext {
        envelope: Box::new(envelope),
    }
}

#[tokio::test]
async fn the_projection_indexes_and_searches_a_folder() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let store = FolderStore::open(workspace.path().join("memory"))?;

    store.apply(&Transaction {
        id: "tx-seed".into(),
        expected_revision: store.current_revision()?,
        operations: vec![
            note(
                "decisions/auth",
                "Authentication",
                "We authenticate with SSH keys because every developer already has one.",
            ),
            note(
                "notes/deploy",
                "Deployment",
                "Releases are cut from the main branch and signed.",
            ),
        ]
        .into_iter()
        .map(Operation::put)
        .collect(),
    })?;

    let projection = Projection::open(&store.index_root()).await?;
    projection.rebuild(&StoreView::current(&store)?).await?;

    let revision = store.current_revision()?;
    assert_eq!(projection.records(&revision).await?.len(), 2);

    let hits = projection
        .search(&SearchRequest {
            query: "SSH keys".into(),
            limit: 20,
            offset: 0,
            filters: memory_hub_index::SearchFilters::default(),
            revision: revision.clone(),
        })
        .await?;
    assert!(
        hits.hits.iter().any(|hit| hit.id == "decisions/auth"),
        "full-text search finds the record: {hits:?}"
    );

    Ok(())
}
