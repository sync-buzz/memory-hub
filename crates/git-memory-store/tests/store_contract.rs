use std::fs;
use std::sync::{Arc, Barrier};
use std::thread;

use git_memory_core::{Envelope, StoredRecord};
use git_memory_store::{
    ChangeKind, GitStore, MAIN_REF, Operation, RecordId, STAGED_REF, StoreErrorKind, Transaction,
};
use git2::{Repository, Signature};

fn repository() -> Result<(tempfile::TempDir, GitStore), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    Repository::init(directory.path())?;
    let store = GitStore::open(directory.path())?;
    Ok((directory, store))
}

fn record(key: &str, content: &str) -> Result<StoredRecord, Box<dyn std::error::Error>> {
    Ok(StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, "note", content)?),
    })
}

fn transaction(
    store: &GitStore,
    id: &str,
    operations: Vec<Operation>,
) -> Result<Transaction, Box<dyn std::error::Error>> {
    Ok(Transaction {
        id: id.into(),
        expected_revision: store.current()?.revision().clone(),
        operations,
    })
}

#[test]
fn atomic_batch_preserves_old_snapshot_and_code_state() -> Result<(), Box<dyn std::error::Error>> {
    let (directory, store) = repository()?;
    fs::write(directory.path().join("working-file.txt"), "untouched")?;
    let head_before = fs::read(directory.path().join(".git/HEAD"))?;
    let old = store.current()?;
    let apply = transaction(
        &store,
        "atomic",
        vec![
            Operation::put(record("alpha", "first")?),
            Operation::put(record("beta", "second")?),
        ],
    )?;
    let result = store.apply(&apply)?;

    assert!(old.get(&RecordId::plaintext("alpha"))?.is_none());
    assert!(
        store
            .snapshot(&result.revision)?
            .get(&RecordId::plaintext("alpha"))?
            .is_some()
    );
    assert_eq!(fs::read(directory.path().join(".git/HEAD"))?, head_before);
    assert_eq!(
        fs::read_to_string(directory.path().join("working-file.txt"))?,
        "untouched"
    );
    let git = Repository::open(directory.path())?;
    assert_eq!(
        git.find_reference(STAGED_REF)?.target(),
        result.revision.as_str().parse().ok()
    );
    let current_tree = git.find_tree(result.revision.as_str().parse()?)?;
    assert_eq!(
        current_tree.get_name("previous").map(|entry| entry.id()),
        Some(old.revision().as_str().parse()?)
    );
    assert!(git.find_reference(MAIN_REF).is_err());
    Ok(())
}

#[test]
fn invalid_record_rejects_the_whole_batch_before_ref_update()
-> Result<(), Box<dyn std::error::Error>> {
    let (_directory, store) = repository()?;
    let before = store.current()?.revision().clone();
    let mut invalid = Envelope::new("invalid", "note", "before edit")?;
    invalid.content = "hash is now stale".into();
    let request = Transaction {
        id: "invalid-batch".into(),
        expected_revision: before.clone(),
        operations: vec![
            Operation::put(record("would-be-partial", "must not persist")?),
            Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(invalid),
            }),
        ],
    };

    let Err(error) = store.apply(&request) else {
        panic!("invalid record must reject the batch");
    };
    assert_eq!(error.kind, StoreErrorKind::InvalidRecord);
    assert_eq!(store.current()?.revision(), &before);
    assert!(
        store
            .current()?
            .get(&RecordId::plaintext("would-be-partial"))?
            .is_none()
    );
    Ok(())
}

#[test]
fn different_key_race_rebases_and_same_key_race_conflicts() -> Result<(), Box<dyn std::error::Error>>
{
    let (directory, store) = repository()?;
    let base = store.current()?.revision().clone();
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for (id, key) in [("left-tx", "left"), ("right-tx", "right")] {
        let project = directory.path().to_path_buf();
        let expected_revision = base.clone();
        let barrier = Arc::clone(&barrier);
        let record = record(key, key)?;
        handles.push(thread::spawn(move || {
            let store = GitStore::open(project)?;
            barrier.wait();
            store.apply(&Transaction {
                id: id.into(),
                expected_revision,
                operations: vec![Operation::put(record)],
            })
        }));
    }
    barrier.wait();
    for handle in handles {
        handle.join().map_err(|_| "writer thread panicked")??;
    }
    let merged = store.current()?;
    assert!(merged.get(&RecordId::plaintext("left"))?.is_some());
    assert!(merged.get(&RecordId::plaintext("right"))?.is_some());

    let base = merged.revision().clone();
    let barrier = Arc::new(Barrier::new(3));
    let mut handles = Vec::new();
    for (id, content) in [("same-a", "A"), ("same-b", "B")] {
        let project = directory.path().to_path_buf();
        let expected_revision = base.clone();
        let barrier = Arc::clone(&barrier);
        let record = record("shared", content)?;
        handles.push(thread::spawn(move || {
            let store = GitStore::open(project)?;
            barrier.wait();
            store.apply(&Transaction {
                id: id.into(),
                expected_revision,
                operations: vec![Operation::put(record)],
            })
        }));
    }
    barrier.wait();
    let outcomes = handles
        .into_iter()
        .map(|handle| handle.join().map_err(|_| "writer thread panicked"))
        .collect::<Result<Vec<_>, _>>()?;
    assert_eq!(outcomes.iter().filter(|result| result.is_ok()).count(), 1);
    let conflict = outcomes
        .iter()
        .find_map(|result| result.as_ref().err())
        .ok_or("one writer must conflict")?;
    assert_eq!(conflict.kind, StoreErrorKind::Conflict);
    assert_eq!(
        conflict.data["conflicting_keys"],
        serde_json::json!(["shared"])
    );
    Ok(())
}

#[test]
fn retry_is_idempotent_and_reuse_with_other_input_is_rejected()
-> Result<(), Box<dyn std::error::Error>> {
    let (_directory, store) = repository()?;
    let request = transaction(
        &store,
        "stable-id",
        vec![
            Operation::put(record("z-last", "last")?),
            Operation::put(record("a-first", "first")?),
        ],
    )?;
    let first = store.apply(&request)?;
    let second = store.apply(&request)?;
    assert_eq!(first, second);

    let mut reused = request;
    reused.operations = vec![Operation::put(record("two", "other")?)];
    let Err(error) = store.apply(&reused) else {
        panic!("changed request must fail");
    };
    assert_eq!(error.kind, StoreErrorKind::TransactionReused);
    Ok(())
}

#[test]
fn checkpoint_history_diff_and_export_import_are_stable() -> Result<(), Box<dyn std::error::Error>>
{
    let (_directory, store) = repository()?;
    let empty = store.current()?.revision().clone();
    let first = store.apply(&transaction(
        &store,
        "first",
        vec![Operation::put(record("one", "version one")?)],
    )?)?;
    let checkpoint_one = store.checkpoint("first checkpoint")?;
    let second = store.apply(&Transaction {
        id: "second".into(),
        expected_revision: first.revision.clone(),
        operations: vec![
            Operation::put(record("one", "version two")?),
            Operation::put(record("two", "another")?),
        ],
    })?;
    let checkpoint_two = store.checkpoint("second checkpoint")?;

    let changes = store.diff(&empty, &second.revision)?;
    assert_eq!(changes.len(), 2);
    assert!(
        changes
            .iter()
            .all(|change| change.kind == ChangeKind::Added)
    );
    let modified = store.diff(&first.revision, &second.revision)?;
    assert_eq!(modified.len(), 2);
    assert!(modified.iter().any(|change| {
        change.id == RecordId::plaintext("one") && change.kind == ChangeKind::Modified
    }));
    let history = store.history(10)?;
    assert_eq!(history[0].commit, checkpoint_two.commit);
    assert_eq!(history[1].commit, checkpoint_one.commit);

    let bytes = store.export(&second.revision)?;
    let (other_directory, other) = repository()?;
    let imported = other.import("import", other.current()?.revision().clone(), &bytes)?;
    assert_eq!(other.export(&imported.revision)?, bytes);
    drop(other_directory);
    Ok(())
}

#[test]
fn ordinary_clone_and_heads_only_push_do_not_publish_memory_refs()
-> Result<(), Box<dyn std::error::Error>> {
    let (source_directory, store) = repository()?;
    store.apply(&transaction(
        &store,
        "memory",
        vec![Operation::put(record("private/decision", "secret")?)],
    )?)?;
    store.checkpoint("private memory checkpoint")?;

    let source = Repository::open(source_directory.path())?;
    assert!(source.find_reference(STAGED_REF).is_ok());
    assert!(source.find_reference(MAIN_REF).is_ok());
    let signature = Signature::now("Test", "test@example.invalid")?;
    let tree_oid = source.treebuilder(None)?.write()?;
    let tree = source.find_tree(tree_oid)?;
    let commit = source.commit(
        Some("refs/heads/main"),
        &signature,
        &signature,
        "code",
        &tree,
        &[],
    )?;
    source.set_head("refs/heads/main")?;
    assert!(source.find_commit(commit).is_ok());

    let clone_directory = tempfile::tempdir()?;
    let clone_path = clone_directory.path().join("clone");
    let clone = Repository::clone(
        source_directory.path().to_str().ok_or("utf8 path")?,
        &clone_path,
    )?;
    assert!(clone.find_reference(STAGED_REF).is_err());
    assert!(clone.find_reference(MAIN_REF).is_err());

    let remote_directory = tempfile::tempdir()?;
    Repository::init_bare(remote_directory.path())?;
    let mut remote = source.remote(
        "publish-test",
        remote_directory.path().to_str().ok_or("utf8 path")?,
    )?;
    remote.push(&["refs/heads/main:refs/heads/main"], None)?;
    let remote_repository = Repository::open_bare(remote_directory.path())?;
    assert!(remote_repository.find_reference("refs/heads/main").is_ok());
    assert!(remote_repository.find_reference(STAGED_REF).is_err());
    assert!(remote_repository.find_reference(MAIN_REF).is_err());
    Ok(())
}
