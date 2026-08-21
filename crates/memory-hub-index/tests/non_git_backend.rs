#![allow(clippy::expect_used)]
//! The seam is only real if something that is not Git can sit behind it.
//!
//! This backend keeps records in a map, has no history, no transport, no
//! encryption, and cannot reopen a past state — the opposite of `GitStore` on
//! every axis the contract makes optional. If the projection works against it,
//! the abstraction is not `GitStore` wearing a trait.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Mutex;

use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_engine::{
    ApplyResult, Capabilities, Capability, Ownership, RecordId, RecordStore, Revision,
    StoreDescription, StoreError, StoreErrorKind, StoreView, Transaction,
};
use memory_hub_index::{Projection, ProjectionState};

#[derive(Debug)]
struct MapStore {
    index_root: PathBuf,
    records: Mutex<BTreeMap<RecordId, StoredRecord>>,
    revision: Mutex<Revision>,
}

impl MapStore {
    fn new(index_root: PathBuf) -> Self {
        Self {
            index_root,
            records: Mutex::new(BTreeMap::new()),
            revision: Mutex::new(Revision::new("v0")),
        }
    }

    fn seed(&self, key: &str, content: &str) {
        let envelope = Envelope::new(key, "note", content).expect("envelope");
        self.records.lock().expect("lock").insert(
            RecordId::plaintext(key),
            StoredRecord::Plaintext {
                envelope: Box::new(envelope),
            },
        );
        let mut revision = self.revision.lock().expect("lock");
        let next = revision
            .as_str()
            .trim_start_matches('v')
            .parse::<u32>()
            .unwrap_or(0)
            + 1;
        *revision = Revision::new(format!("v{next}"));
    }
}

impl RecordStore for MapStore {
    fn capabilities(&self) -> Capabilities {
        // Nothing optional is offered. Every `Capability` is absent on purpose.
        Capabilities::new(Ownership::Shared, [])
    }

    fn describe(&self) -> StoreDescription {
        StoreDescription {
            backend: "map".to_owned(),
            // Absent rather than plausible: this store has no Git directory.
            git_dir: None,
        }
    }

    fn index_root(&self) -> PathBuf {
        self.index_root.clone()
    }

    fn current_revision(&self) -> Result<Revision, StoreError> {
        Ok(self.revision.lock().expect("lock").clone())
    }

    fn read_record(
        &self,
        _revision: &Revision,
        id: &RecordId,
    ) -> Result<Option<StoredRecord>, StoreError> {
        Ok(self.records.lock().expect("lock").get(id).cloned())
    }

    fn read_records(
        &self,
        _revision: &Revision,
    ) -> Result<Vec<(RecordId, StoredRecord)>, StoreError> {
        Ok(self
            .records
            .lock()
            .expect("lock")
            .iter()
            .map(|(id, record)| (id.clone(), record.clone()))
            .collect())
    }

    fn apply(&self, _transaction: &Transaction) -> Result<ApplyResult, StoreError> {
        Err(StoreError::unsupported("writes in this test double", "map"))
    }

    fn validate_revision(&self, revision: &Revision) -> Result<(), StoreError> {
        // Only the present exists here, which is what a store without
        // `Capability::Snapshots` means.
        if *revision == *self.revision.lock().expect("lock") {
            return Ok(());
        }
        Err(StoreError::new(
            StoreErrorKind::RevisionNotFound,
            "this store keeps no past revisions",
            serde_json::json!({"revision": revision.as_str()}),
        ))
    }
}

#[tokio::test]
async fn the_projection_indexes_a_backend_that_is_not_git() -> Result<(), Box<dyn std::error::Error>>
{
    let workspace = tempfile::tempdir()?;
    let store = MapStore::new(workspace.path().join("index"));
    store.seed("alpha", "the first note");
    store.seed("beta", "the second note");

    // Where the index lives comes from the store, not from a Git directory.
    let projection = Projection::open(&store.index_root()).await?;
    let view = StoreView::current(&store)?;
    projection.rebuild(&view).await?;

    let rows = projection.records(view.revision()).await?;
    assert_eq!(rows.len(), 2, "both records reached the projection");
    assert_eq!(projection.status()?.state, ProjectionState::Fresh);
    assert_eq!(
        projection.status()?.indexed_revision.as_ref(),
        Some(view.revision())
    );
    Ok(())
}

#[tokio::test]
async fn a_store_without_history_refuses_incremental_update_instead_of_guessing()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let store = MapStore::new(workspace.path().join("index"));
    store.seed("alpha", "the first note");

    let projection = Projection::open(&store.index_root()).await?;
    let before = StoreView::current(&store)?.revision().clone();
    projection.rebuild(&StoreView::current(&store)?).await?;

    store.seed("beta", "the second note");
    let after = StoreView::current(&store)?.revision().clone();

    // The capability is the machine-readable statement, so assert on it rather
    // than on the wording of the failure.
    assert!(
        !store.capabilities().has(Capability::History),
        "this store declares no history"
    );

    // Incremental repair needs a diff between two past states. Without history
    // the honest answer is a refusal — not a half-updated index.
    assert!(
        projection.update(&store, &before, &after).await.is_err(),
        "a store without history must refuse incremental update"
    );

    // And the refusal leaves the projection usable: a full rebuild still works.
    projection.rebuild(&StoreView::current(&store)?).await?;
    assert_eq!(projection.records(&after).await?.len(), 2);
    Ok(())
}

#[test]
fn a_backend_without_a_git_directory_omits_the_field() {
    let store = MapStore::new(PathBuf::from("/tmp/does-not-matter"));
    let description = store.describe();
    assert_eq!(description.backend, "map");
    assert!(description.git_dir.is_none());

    let wire = serde_json::to_value(&description).expect("serialise");
    assert!(
        wire.get("gitDir").is_none() && wire.get("git_dir").is_none(),
        "an absent Git directory is omitted, never sent as a plausible path: {wire}"
    );

    let capabilities = store.capabilities();
    assert_eq!(capabilities.ownership, Ownership::Shared);
    for capability in [
        Capability::History,
        Capability::Transport,
        Capability::Snapshots,
    ] {
        assert!(
            !capabilities.has(capability),
            "{capability:?} is not offered"
        );
    }
}
