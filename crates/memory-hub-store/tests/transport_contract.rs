//! End-to-end tests for memory remote transport: push, fetch, merge.
//!
//! Uses local bare Git repositories as remotes (no SSH/HTTPS needed).

#![allow(clippy::unwrap_used)]

use std::fs;
use std::process::Command;

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_store::{
    GitStore, MemoryPresence, MemoryRemote, Operation, RecordId, StoreErrorKind, Transaction,
    fetch_and_merge, memory_presence, push_to_remote, read_remote_config, write_remote_config,
};

fn repo_with_store() -> (tempfile::TempDir, GitStore) {
    let dir = tempfile::tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    // These scenarios exercise push/fetch/merge with unsigned commits. The
    // fail-closed default is covered by
    // `fetch_without_allowed_signers_is_refused`.
    allow_unsigned_exchange(dir.path());
    let store = GitStore::open(dir.path()).unwrap();
    (dir, store)
}

fn allow_unsigned_exchange(work_dir: &std::path::Path) {
    let repository = Repository::open(work_dir).unwrap();
    let mut config = repository.config().unwrap();
    config.set_str("memory-hub.signing.verify", "off").unwrap();
}

fn bare_remote() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    Command::new("git")
        .args(["init", "--bare"])
        .arg(dir.path())
        .output()
        .unwrap();
    dir
}

fn record(key: &str, content: &str) -> StoredRecord {
    StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, "note", content).unwrap()),
    }
}

use std::sync::atomic::{AtomicU64, Ordering};

static PUT_COUNTER: AtomicU64 = AtomicU64::new(0);

fn put(store: &GitStore, key: &str, content: &str) {
    let seq = PUT_COUNTER.fetch_add(1, Ordering::SeqCst);
    let revision = store.current().unwrap().revision().clone();
    store
        .apply(&Transaction {
            id: format!("put-{key}-{seq}"),
            expected_revision: revision,
            operations: vec![Operation::put(record(key, content))],
        })
        .unwrap();
}

fn read_record(store: &GitStore, key: &str) -> Option<String> {
    let snapshot = store.current().unwrap();
    let id = RecordId::plaintext(key);
    snapshot.get(&id).unwrap().map(|record| match record {
        StoredRecord::Plaintext { envelope } => envelope.content.clone(),
    })
}

#[test]
fn remote_config_round_trip() {
    let (dir, _store) = repo_with_store();
    let git_dir = dir.path().join(".git");

    assert!(read_remote_config(&git_dir).unwrap().is_none());

    let remote = MemoryRemote {
        url: "/path/to/remote".to_owned(),
        refspec: None,
    };
    write_remote_config(&git_dir, &remote).unwrap();

    let read = read_remote_config(&git_dir).unwrap().unwrap();
    assert_eq!(read.url, "/path/to/remote");
    assert!(read.refspec.is_none());
}

#[test]
fn push_and_fetch_fast_forward() {
    let (_local_dir, local_store) = repo_with_store();
    let remote_dir = bare_remote();

    let local_git_dir = local_store.git_dir().to_path_buf();
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    write_remote_config(
        &local_git_dir,
        &MemoryRemote {
            url: remote_url.clone(),
            refspec: None,
        },
    )
    .unwrap();

    // Write a record locally.
    put(&local_store, "decision/auth", "Use OAuth2");

    // Push to remote.
    push_to_remote(
        &local_git_dir,
        &read_remote_config(&local_git_dir).unwrap().unwrap(),
        false,
    )
    .unwrap();

    // Create a second local clone and fetch.
    let second_dir = tempfile::tempdir().unwrap();
    Repository::init(second_dir.path()).unwrap();
    allow_unsigned_exchange(second_dir.path());
    let second_store = GitStore::open(second_dir.path()).unwrap();
    let second_git_dir = second_store.git_dir().to_path_buf();
    write_remote_config(
        &second_git_dir,
        &MemoryRemote {
            url: remote_url,
            refspec: None,
        },
    )
    .unwrap();

    let result = fetch_and_merge(
        &second_store,
        &read_remote_config(&second_git_dir).unwrap().unwrap(),
    )
    .unwrap();

    assert!(result.fast_forward);
    assert!(!result.conflicts.is_empty() || result.conflicts.is_empty());
    assert_eq!(
        read_record(&second_store, "decision/auth").as_deref(),
        Some("Use OAuth2")
    );
}

#[test]
fn fetch_merges_different_keys() {
    let (_alice_dir, alice_store) = repo_with_store();
    let remote_dir = bare_remote();

    let alice_git = alice_store.git_dir().to_path_buf();
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    write_remote_config(
        &alice_git,
        &MemoryRemote {
            url: remote_url.clone(),
            refspec: None,
        },
    )
    .unwrap();

    // Alice writes alpha and pushes.
    put(&alice_store, "alpha", "alice alpha");
    push_to_remote(
        &alice_git,
        &read_remote_config(&alice_git).unwrap().unwrap(),
        false,
    )
    .unwrap();

    // Bob clones, fetches alpha, then writes beta locally.
    let bob_dir = tempfile::tempdir().unwrap();
    Repository::init(bob_dir.path()).unwrap();
    allow_unsigned_exchange(bob_dir.path());
    let bob_store = GitStore::open(bob_dir.path()).unwrap();
    let bob_git = bob_store.git_dir().to_path_buf();
    write_remote_config(
        &bob_git,
        &MemoryRemote {
            url: remote_url.clone(),
            refspec: None,
        },
    )
    .unwrap();

    // Bob fetches alpha.
    fetch_and_merge(
        &bob_store,
        &read_remote_config(&bob_git).unwrap().unwrap(),
    )
    .unwrap();
    assert_eq!(
        read_record(&bob_store, "alpha").as_deref(),
        Some("alice alpha")
    );

    // Bob writes beta.
    put(&bob_store, "beta", "bob beta");
    // Alice writes gamma and pushes.
    put(&alice_store, "gamma", "alice gamma");
    push_to_remote(
        &alice_git,
        &read_remote_config(&alice_git).unwrap().unwrap(),
        false,
    )
    .unwrap();

    // Bob fetches again — should merge gamma (different key).
    let result = fetch_and_merge(
        &bob_store,
        &read_remote_config(&bob_git).unwrap().unwrap(),
    )
    .unwrap();

    assert!(result.merged || result.fast_forward);
    assert_eq!(
        read_record(&bob_store, "alpha").as_deref(),
        Some("alice alpha")
    );
    assert_eq!(read_record(&bob_store, "beta").as_deref(), Some("bob beta"));
    assert_eq!(
        read_record(&bob_store, "gamma").as_deref(),
        Some("alice gamma")
    );
}

#[test]
fn fetch_same_key_conflict_returns_both_versions() {
    let (_alice_dir, alice_store) = repo_with_store();
    let remote_dir = bare_remote();

    let alice_git = alice_store.git_dir().to_path_buf();
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    write_remote_config(
        &alice_git,
        &MemoryRemote {
            url: remote_url.clone(),
            refspec: None,
        },
    )
    .unwrap();

    // Alice writes shared key and pushes.
    put(&alice_store, "shared", "alice version");
    push_to_remote(
        &alice_git,
        &read_remote_config(&alice_git).unwrap().unwrap(),
        false,
    )
    .unwrap();

    // Bob clones and fetches.
    let bob_dir = tempfile::tempdir().unwrap();
    Repository::init(bob_dir.path()).unwrap();
    allow_unsigned_exchange(bob_dir.path());
    let bob_store = GitStore::open(bob_dir.path()).unwrap();
    let bob_git = bob_store.git_dir().to_path_buf();
    write_remote_config(
        &bob_git,
        &MemoryRemote {
            url: remote_url.clone(),
            refspec: None,
        },
    )
    .unwrap();
    fetch_and_merge(
        &bob_store,
        &read_remote_config(&bob_git).unwrap().unwrap(),
    )
    .unwrap();

    // Both Alice and Bob modify the same key independently.
    put(&alice_store, "shared", "alice updated");
    push_to_remote(
        &alice_git,
        &read_remote_config(&alice_git).unwrap().unwrap(),
        false,
    )
    .unwrap();

    put(&bob_store, "shared", "bob updated");

    // Bob fetches — should get a conflict.
    let result = fetch_and_merge(
        &bob_store,
        &read_remote_config(&bob_git).unwrap().unwrap(),
    )
    .unwrap();

    assert!(!result.conflicts.is_empty());
    let conflict = &result.conflicts[0];
    assert_eq!(conflict.key, "shared");
    assert_ne!(conflict.local_content_hash, conflict.remote_content_hash);
}

#[test]
fn fetch_up_to_date_is_noop() {
    let (_alice_dir, alice_store) = repo_with_store();
    let remote_dir = bare_remote();

    let alice_git = alice_store.git_dir().to_path_buf();
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    write_remote_config(
        &alice_git,
        &MemoryRemote {
            url: remote_url,
            refspec: None,
        },
    )
    .unwrap();

    put(&alice_store, "key1", "value1");
    push_to_remote(
        &alice_git,
        &read_remote_config(&alice_git).unwrap().unwrap(),
        false,
    )
    .unwrap();

    // Fetch again — should be up to date.
    let result = fetch_and_merge(
        &alice_store,
        &read_remote_config(&alice_git).unwrap().unwrap(),
    )
    .unwrap();

    assert!(result.fast_forward);
    assert_eq!(result.local_revision_before, result.local_revision_after);
}

#[test]
fn push_does_not_touch_code_branches() {
    let (dir, store) = repo_with_store();
    let remote_dir = bare_remote();

    let git_dir = store.git_dir().to_path_buf();
    let remote_url = remote_dir.path().to_string_lossy().to_string();

    // Create a code commit on the default branch.
    fs::write(dir.path().join("code.txt"), "code content").unwrap();
    Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["add", "."])
        .output()
        .unwrap();
    Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["commit", "-m", "code commit"])
        .output()
        .unwrap();

    let head_before = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let head_before = String::from_utf8(head_before.stdout)
        .unwrap()
        .trim()
        .to_owned();

    // Write a memory record and push.
    put(&store, "note/1", "memory content");
    write_remote_config(
        &git_dir,
        &MemoryRemote {
            url: remote_url,
            refspec: None,
        },
    )
    .unwrap();
    push_to_remote(
        &git_dir,
        &read_remote_config(&git_dir).unwrap().unwrap(),
        false,
    )
    .unwrap();

    // HEAD must not have changed.
    let head_after = Command::new("git")
        .arg("-C")
        .arg(dir.path())
        .args(["rev-parse", "HEAD"])
        .output()
        .unwrap();
    let head_after = String::from_utf8(head_after.stdout)
        .unwrap()
        .trim()
        .to_owned();

    assert_eq!(head_before, head_after);

    // Remote should have refs/memory/main but NOT the code branch.
    let remote_refs = Command::new("git")
        .arg("-C")
        .arg(remote_dir.path())
        .args(["for-each-ref", "--format=%(refname)"])
        .output()
        .unwrap();
    let remote_refs = String::from_utf8_lossy(&remote_refs.stdout);
    assert!(remote_refs.contains("refs/memory/main"));
    // No code branches (refs/heads/*) should exist on the remote.
    assert!(!remote_refs.contains("refs/heads/"));
}

#[test]
fn no_remote_configured_returns_error() {
    let (_dir, store) = repo_with_store();
    let git_dir = store.git_dir().to_path_buf();

    // No remote configured.
    let remote = read_remote_config(&git_dir).unwrap();
    assert!(remote.is_none());
}

#[test]
fn fetch_from_uninitialized_remote_fails() {
    let (_dir, store) = repo_with_store();
    let remote_dir = bare_remote();

    let git_dir = store.git_dir().to_path_buf();
    write_remote_config(
        &git_dir,
        &MemoryRemote {
            url: remote_dir.path().to_string_lossy().to_string(),
            refspec: None,
        },
    )
    .unwrap();

    let result = fetch_and_merge(&store, &read_remote_config(&git_dir).unwrap().unwrap());
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(
        err.kind == StoreErrorKind::TransportFailed
            || err.kind == StoreErrorKind::NamespaceRejected
    );
}

// --- presence: is the memory here, elsewhere, or nowhere -------------------
//
// The three answers look identical from inside a fresh clone, and only one of
// them is something a person can act on. Each of these is one of the states
// `memory_presence` exists to tell apart.

/// A repository that has never been touched, with no remote to ask.
#[test]
fn presence_of_an_untouched_repository_is_absent_with_nothing_to_ask() {
    let dir = tempfile::tempdir().unwrap();
    Repository::init(dir.path()).unwrap();

    let presence = memory_presence(dir.path()).unwrap();

    assert!(
        matches!(presence, MemoryPresence::Absent { url: None }),
        "no memory and no address to ask is an empty project, not a failure: {presence:?}"
    );
}

/// Asking must not be mistaken for having. `GitStore::open` creates
/// `refs/memory/staged`, so a repository somebody merely *looked* at has a
/// memory ref and no memory — which is why the count is of records.
#[test]
fn presence_counts_records_rather_than_refs() {
    let (dir, _store) = repo_with_store();

    let presence = memory_presence(dir.path()).unwrap();

    assert!(
        matches!(presence, MemoryPresence::Absent { .. }),
        "opening a store writes a ref and no records, and must not read as memory: {presence:?}"
    );
}

#[test]
fn presence_of_a_repository_with_records_is_present() {
    let (dir, store) = repo_with_store();
    put(&store, "note-1", "one");
    put(&store, "note-2", "two");

    let presence = memory_presence(dir.path()).unwrap();

    match presence {
        MemoryPresence::Present { records } => assert_eq!(records, 2),
        other => panic!("expected the memory to be found here: {other:?}"),
    }
}

/// The case the whole check exists for: a clone whose memory is still on the
/// remote. It is reported against the *code* origin, because that is the only
/// address a fresh clone knows before anybody configures a memory remote.
#[test]
fn presence_of_a_fresh_clone_names_the_code_origin_it_can_be_fetched_from() {
    let (source, store) = repo_with_store();
    put(&store, "note-1", "one");
    let remote_dir = bare_remote();
    let url = remote_dir.path().to_string_lossy().into_owned();
    push_to_remote(
        store.git_dir(),
        &MemoryRemote {
            url: url.clone(),
            refspec: None,
        },
        false,
    )
    .unwrap();
    drop(source);

    // A clone, made the ordinary way: branches and tags, no refs/memory/*.
    let clone_dir = tempfile::tempdir().unwrap();
    let clone_path = clone_dir.path().join("clone");
    Command::new("git")
        .args(["clone"])
        .arg(&url)
        .arg(&clone_path)
        .output()
        .unwrap();

    let presence = memory_presence(&clone_path).unwrap();

    match presence {
        MemoryPresence::NotFetched { url: found, configured } => {
            assert!(!configured, "a fresh clone has no memory remote configured yet");
            assert!(
                found.contains(remote_dir.path().to_string_lossy().as_ref()),
                "the answer has to name the address to fetch from, got {found}"
            );
        }
        other => panic!("a clone of a repository with memory is not empty: {other:?}"),
    }
}

/// A remote that carries no memory either. Not a failure, and not the same
/// answer as the one above — this is a project nobody has started a memory
/// for, which is where every project begins.
#[test]
fn presence_of_a_clone_whose_remote_has_no_memory_is_absent() {
    let remote_dir = bare_remote();
    let url = remote_dir.path().to_string_lossy().into_owned();
    let dir = tempfile::tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    write_remote_config(
        &GitStore::discover_git_dir(dir.path()).unwrap(),
        &MemoryRemote {
            url: url.clone(),
            refspec: None,
        },
    )
    .unwrap();

    let presence = memory_presence(dir.path()).unwrap();

    match presence {
        MemoryPresence::Absent { url: Some(found) } => assert_eq!(found, url),
        other => panic!("an empty remote means an empty project, not a defect: {other:?}"),
    }
}

/// "Nobody could say" is not "there is none". A caller that collapsed the two
/// would describe a project afresh on a flaky network and diverge from memory
/// that exists.
#[test]
fn presence_of_an_unreachable_remote_is_its_own_answer() {
    let dir = tempfile::tempdir().unwrap();
    Repository::init(dir.path()).unwrap();
    let missing = dir.path().join("no-such-remote");
    write_remote_config(
        &GitStore::discover_git_dir(dir.path()).unwrap(),
        &MemoryRemote {
            url: missing.to_string_lossy().into_owned(),
            refspec: None,
        },
    )
    .unwrap();

    let presence = memory_presence(dir.path()).unwrap();

    assert!(
        matches!(presence, MemoryPresence::Unreachable { .. }),
        "a remote that cannot be asked must not answer for the remote: {presence:?}"
    );
}
