#![allow(clippy::expect_used)]
//! `GitStore` driven only through the storage-neutral contract.
//!
//! Two things this catches that nothing else does.
//!
//! Every method in `engine_impl` forwards to an inherent method of the same
//! name. If one of those inherent methods is ever renamed or removed, the
//! forwarding call resolves to the trait method instead and recurses forever —
//! a stack overflow at run time, not a compile error. Exercising the store
//! through `&dyn RecordStore` is what turns that into a failing test.
//!
//! And the capability set is a claim that nothing enforces: a backend could
//! advertise `History` while returning `None` from `history()`. Here the two
//! are asserted to agree.

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_engine::{
    Capability, Operation, Ownership, RecordId, RecordStore, StoreView, Transaction,
};
use memory_hub_store::GitStore;

fn record(key: &str, content: &str) -> StoredRecord {
    StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, "note", content).expect("envelope")),
    }
}

#[test]
fn the_git_store_answers_the_whole_contract_through_a_dyn_reference()
-> Result<(), Box<dyn std::error::Error>> {
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let concrete = GitStore::open(project.path())?;
    let store: &dyn RecordStore = &concrete;

    let description = store.describe();
    assert_eq!(description.backend, "refs");
    assert!(
        description.git_dir.is_some(),
        "a Git-backed store reports its Git directory"
    );
    assert!(
        store.index_root().starts_with(
            description
                .git_dir
                .as_ref()
                .expect("git dir present for refs")
        )
    );

    let empty = store.current_revision()?;
    assert!(store.read_records(&empty)?.is_empty());
    store.validate_revision(&empty)?;

    let applied = store.apply(&Transaction {
        id: "through-the-contract".into(),
        expected_revision: empty.clone(),
        operations: vec![Operation::put(record("alpha", "one"))],
    })?;
    assert_eq!(applied.changed_keys, vec!["alpha".to_owned()]);

    let id = RecordId::plaintext("alpha");
    let stored = store
        .read_record(&applied.revision, &id)?
        .expect("the record just written is readable");
    match stored {
        StoredRecord::Plaintext { envelope } => assert_eq!(envelope.content, "one"),
    }

    let view = StoreView::open(store, &applied.revision)?;
    assert_eq!(view.records()?.len(), 1);
    assert!(view.get(&id)?.is_some());

    // The past is still readable, which is what Capability::Snapshots claims.
    assert!(store.read_records(&empty)?.is_empty());
    Ok(())
}

#[test]
fn declared_capabilities_agree_with_the_optional_traits() -> Result<(), Box<dyn std::error::Error>>
{
    let project = tempfile::tempdir()?;
    Repository::init(project.path())?;
    let concrete = GitStore::open(project.path())?;
    let store: &dyn RecordStore = &concrete;

    let capabilities = store.capabilities();
    assert_eq!(capabilities.ownership, Ownership::Owned);

    // A claim and the reachable implementation must not disagree: a caller may
    // branch on either, and they have to lead to the same place.
    assert_eq!(
        capabilities.has(Capability::History),
        store.history().is_some(),
        "the History claim and the history() accessor disagree"
    );
    assert_eq!(
        capabilities.has(Capability::Transport),
        store.portable().is_some(),
        "the Transport claim and the portable() accessor disagree"
    );

    // And the optional sides work through the contract, not only inherently.
    let history = store.history().expect("refs keeps its past");
    let revision = store.current_revision()?;
    assert!(
        history.diff(&revision, &revision)?.is_empty(),
        "a revision compared with itself changed nothing"
    );

    let portable = store.portable().expect("refs exports");
    let bundle = portable.export(&store.current_revision()?)?;
    assert!(!bundle.is_empty());
    Ok(())
}
