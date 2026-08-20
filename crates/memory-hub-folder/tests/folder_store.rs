//! What a folder of records has to do to be a record store.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::collections::BTreeMap;

use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_engine::{
    Capability, Operation, Ownership, RecordId, RecordStore, Revision, StoreErrorKind, Transaction,
};
use memory_hub_folder::FolderStore;

fn record(key: &str, content: &str) -> StoredRecord {
    StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, "note", content).unwrap()),
    }
}

fn put(store: &FolderStore, id: &str, key: &str, content: &str) -> Revision {
    let transaction = Transaction {
        id: id.to_owned(),
        expected_revision: store.current_revision().unwrap(),
        operations: vec![Operation::put(record(key, content))],
    };
    store.apply(&transaction).unwrap().revision
}

fn open(directory: &tempfile::TempDir) -> FolderStore {
    FolderStore::open(directory.path().join("memory")).unwrap()
}

#[test]
fn a_record_is_a_file_a_person_can_open() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);
    put(&store, "tx-1", "decisions/auth", "We chose SSH keys.");

    let file = directory.path().join("memory/records/decisions/auth.json");
    assert!(file.exists(), "the key is the path");
    let text = std::fs::read_to_string(&file).unwrap();
    assert!(
        text.contains("We chose SSH keys."),
        "and the content is right there: {text}"
    );
}

#[test]
fn reading_back_returns_what_was_written() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);
    let revision = put(&store, "tx-1", "notes/one", "first");

    let stored = store
        .read_record(&revision, &RecordId::plaintext("notes/one"))
        .unwrap()
        .expect("the record is there");
    let StoredRecord::Plaintext { envelope } = stored else {
        panic!("expected plaintext");
    };
    assert_eq!(envelope.content, "first");
}

#[test]
fn the_revision_moves_when_the_corpus_does() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);
    let empty = store.current_revision().unwrap();
    let after_write = put(&store, "tx-1", "notes/one", "first");
    assert_ne!(empty, after_write);

    let after_edit = put(&store, "tx-2", "notes/one", "second");
    assert_ne!(after_write, after_edit, "same key, different content");
}

#[test]
fn a_write_built_on_a_stale_revision_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);
    let stale = store.current_revision().unwrap();
    put(&store, "tx-1", "notes/one", "first");

    let error = store
        .apply(&Transaction {
            id: "tx-2".into(),
            expected_revision: stale,
            operations: vec![Operation::put(record("notes/two", "second"))],
        })
        .unwrap_err();
    assert_eq!(error.kind, StoreErrorKind::Conflict);
}

#[test]
fn retrying_a_transaction_returns_the_first_answer() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);
    let base = store.current_revision().unwrap();
    let transaction = Transaction {
        id: "tx-1".into(),
        expected_revision: base,
        operations: vec![Operation::put(record("notes/one", "first"))],
    };

    let first = store.apply(&transaction).unwrap();
    // The same request arriving again — a client that reconnected and could not
    // tell whether the first one landed.
    let again = store.apply(&transaction).unwrap();
    assert_eq!(first, again, "the work is not done twice");
}

#[test]
fn deleting_takes_the_file_and_the_folder_it_emptied() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);
    put(&store, "tx-1", "decisions/auth", "content");

    store
        .apply(&Transaction {
            id: "tx-2".into(),
            expected_revision: store.current_revision().unwrap(),
            operations: vec![Operation::delete(RecordId::plaintext("decisions/auth"))],
        })
        .unwrap();

    assert!(
        !directory
            .path()
            .join("memory/records/decisions/auth.json")
            .exists()
    );
    assert!(
        !directory.path().join("memory/records/decisions").exists(),
        "a folder standing for nothing is not left behind"
    );
}

#[test]
fn an_interrupted_transaction_is_finished_on_the_next_open() {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().join("memory");
    let store = FolderStore::open(&root).unwrap();
    put(&store, "tx-1", "notes/one", "first");
    drop(store);

    // What a process that died between writing the plan and carrying it out
    // leaves behind. The plan is the only evidence the transaction happened.
    let plan = serde_json::json!({
        "transaction_id": "tx-interrupted",
        "steps": [{
            "op": "write",
            "id": {"addressing": "plaintext", "value": "notes/two"},
            "record": serde_json::to_value(record("notes/two", "second")).unwrap(),
        }],
        "result": {"revision": "unused", "changed_keys": ["notes/two"]},
    });
    std::fs::write(
        root.join("pending.json"),
        serde_json::to_vec_pretty(&plan).unwrap(),
    )
    .unwrap();

    let reopened = FolderStore::open(&root).unwrap();
    let revision = reopened.current_revision().unwrap();
    assert!(
        reopened
            .read_record(&revision, &RecordId::plaintext("notes/two"))
            .unwrap()
            .is_some(),
        "the interrupted write landed"
    );
    assert!(
        !root.join("pending.json").exists(),
        "and the plan was taken down"
    );
}

#[test]
fn it_offers_nothing_it_cannot_do() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);
    let capabilities = store.capabilities();

    assert_eq!(capabilities.ownership, Ownership::Owned);
    for capability in [
        Capability::History,
        Capability::Transport,
        Capability::Encryption,
        Capability::Snapshots,
    ] {
        assert!(
            !capabilities.has(capability),
            "{capability:?} is not offered"
        );
    }
    assert!(store.history().is_none());
    assert!(store.portable().is_none());
}

#[test]
fn a_revision_it_does_not_hold_is_not_found() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);
    let stale = store.current_revision().unwrap();
    put(&store, "tx-1", "notes/one", "first");

    let error = store.validate_revision(&stale).unwrap_err();
    assert_eq!(
        error.kind,
        StoreErrorKind::RevisionNotFound,
        "keeping no past means a past revision is unknown, not older"
    );
}

#[test]
fn a_key_that_would_escape_the_folder_is_refused() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);

    for key in ["../outside", "a/../../b", "/absolute", "trailing/"] {
        let mut envelope = Envelope::new("placeholder", "note", "content").unwrap();
        envelope.key = key.to_owned();
        let error = store
            .apply(&Transaction {
                id: format!("tx-{key}"),
                expected_revision: store.current_revision().unwrap(),
                operations: vec![Operation::put(StoredRecord::Plaintext {
                    envelope: Box::new(envelope),
                })],
            })
            .unwrap_err();
        assert_eq!(
            error.kind,
            StoreErrorKind::InvalidArgument,
            "`{key}` must not become a path"
        );
    }
}

#[test]
fn a_file_that_is_not_a_record_is_left_where_it_is() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);
    put(&store, "tx-1", "notes/one", "first");

    // A README in the records folder is not a mistake and not litter: somebody
    // put it there to explain the folder to the next person. It is not a
    // record — records are envelopes, and this is prose — but the difference
    // between "not a record" and "not wanted" matters, so it is read past and
    // never touched.
    let readme = directory.path().join("memory/records/README.md");
    std::fs::write(&readme, "These are the project's memory records.\n").unwrap();

    let revision = store.current_revision().unwrap();
    let records: BTreeMap<_, _> = store.read_records(&revision).unwrap().into_iter().collect();
    assert_eq!(records.len(), 1, "prose is not an envelope");

    put(&store, "tx-2", "notes/two", "second");
    store
        .apply(&Transaction {
            id: "tx-3".into(),
            expected_revision: store.current_revision().unwrap(),
            operations: vec![Operation::delete(RecordId::plaintext("notes/one"))],
        })
        .unwrap();

    assert_eq!(
        std::fs::read_to_string(&readme).unwrap(),
        "These are the project's memory records.\n",
        "writes and deletes went past it without a scratch"
    );
}

#[test]
fn a_folder_holding_only_someone_elses_file_is_not_pruned() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);
    put(&store, "tx-1", "decisions/auth", "content");

    let notes = directory.path().join("memory/records/decisions/NOTES.md");
    std::fs::write(&notes, "why auth is the way it is").unwrap();

    store
        .apply(&Transaction {
            id: "tx-2".into(),
            expected_revision: store.current_revision().unwrap(),
            operations: vec![Operation::delete(RecordId::plaintext("decisions/auth"))],
        })
        .unwrap();

    // Pruning empties folders that stand for nothing. This one stands for
    // something — just not for us.
    assert!(notes.exists(), "the file is still there");
}

#[test]
fn system_litter_is_ignored() {
    let directory = tempfile::tempdir().unwrap();
    let store = open(&directory);
    put(&store, "tx-1", "notes/one", "first");

    // Not somebody's file — the operating system's. Nobody put it there on
    // purpose and nobody will miss it if it goes.
    std::fs::write(directory.path().join("memory/records/.DS_Store"), "junk").unwrap();

    let revision = store.current_revision().unwrap();
    assert_eq!(store.read_records(&revision).unwrap().len(), 1);
}
