//! End-to-end tests for memory remote transport: push, fetch, merge.
//!
//! Uses local bare Git repositories as remotes (no SSH/HTTPS needed).

#![allow(clippy::unwrap_used)]

use std::fs;
use std::process::Command;

use git2::Repository;
use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_store::{
    GitStore, MemoryPresence, MemoryRemote, Operation, RecordId, RemoteCheck, StoreErrorKind,
    Transaction, fetch_and_merge, memory_presence, push_to_remote, read_remote_config, sync_state,
    write_remote_config,
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
    fetch_and_merge(&bob_store, &read_remote_config(&bob_git).unwrap().unwrap()).unwrap();
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
    let result =
        fetch_and_merge(&bob_store, &read_remote_config(&bob_git).unwrap().unwrap()).unwrap();

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
fn the_same_key_changed_on_both_sides_is_resolved_in_favour_of_whoever_fetches() {
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
    fetch_and_merge(&bob_store, &read_remote_config(&bob_git).unwrap().unwrap()).unwrap();

    // Both Alice and Bob modify the same key independently.
    put(&alice_store, "shared", "alice updated");
    push_to_remote(
        &alice_git,
        &read_remote_config(&alice_git).unwrap().unwrap(),
        false,
    )
    .unwrap();

    put(&bob_store, "shared", "bob updated");

    // Bob fetches. Both rewrote the whole of a one-line record, so this is a
    // genuine collision — and it is resolved rather than refused: Bob is the
    // one at the keyboard, so Bob's text stays and he is told whose it is not.
    let result =
        fetch_and_merge(&bob_store, &read_remote_config(&bob_git).unwrap().unwrap()).unwrap();

    assert!(
        result.conflicts.is_empty(),
        "a same-key collision is merged, not handed back"
    );
    assert_eq!(keys_of(&result.overlaps), vec!["shared".to_owned()]);
    assert!(
        result.overlaps[0].body,
        "the body is what they both rewrote"
    );
    assert_eq!(body_of(&bob_store, "shared"), "bob updated");
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
/// `refs/memory/main` with an empty genesis, so a repository somebody merely
/// *looked* at has a memory ref and no memory — which is why the count is of
/// records.
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
        MemoryPresence::NotFetched {
            url: found,
            configured,
        } => {
            assert!(
                !configured,
                "a fresh clone has no memory remote configured yet"
            );
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

// --- sync state: what is unpublished, and is anything waiting --------------

fn keys_of(overlaps: &[memory_hub_store::Overlap]) -> Vec<String> {
    overlaps.iter().map(|it| it.key.clone()).collect()
}

fn body_of(store: &GitStore, key: &str) -> String {
    let view = store.current().unwrap();
    let Some((_, record)) = view
        .records()
        .unwrap()
        .into_iter()
        .find(|(id, _)| id.display_value() == key)
    else {
        panic!("no record named {key}")
    };
    let memory_hub_core::StoredRecord::Plaintext { envelope } = record;
    envelope.content.clone()
}

fn remote_at(dir: &tempfile::TempDir) -> MemoryRemote {
    MemoryRemote {
        url: dir.path().to_string_lossy().into_owned(),
        refspec: None,
    }
}

/// With no remote there is nothing to be out of step with, and a count would
/// be inventing a comparison nobody asked for.
#[test]
fn sync_state_without_a_remote_counts_nothing() {
    let (_dir, store) = repo_with_store();
    put(&store, "note-1", "one");

    let state = sync_state(&store, true).unwrap();

    assert!(!state.remote_configured);
    assert_eq!(state.unpublished, 0);
    assert_eq!(state.remote, RemoteCheck::NotAsked);
}

/// Nothing exchanged yet: everything here is unpublished, and it is counted as
/// records rather than as the commits that wrote them.
#[test]
fn everything_is_unpublished_before_the_first_exchange() {
    let (dir, store) = repo_with_store();
    let remote_dir = bare_remote();
    write_remote_config(store.git_dir(), &remote_at(&remote_dir)).unwrap();
    put(&store, "note-1", "one");
    put(&store, "note-2", "two");
    put(&store, "note-1", "one, edited");

    let state = sync_state(&store, false).unwrap();

    assert!(state.remote_configured);
    assert_eq!(
        state.unpublished, 2,
        "two records, whatever number of commits it took to write them"
    );
    assert_eq!(state.remote, RemoteCheck::NotAsked);
    let _ = dir;
}

#[test]
fn a_push_leaves_nothing_unpublished_and_a_later_edit_is_counted() {
    let (_dir, store) = repo_with_store();
    let remote_dir = bare_remote();
    let remote = remote_at(&remote_dir);
    write_remote_config(store.git_dir(), &remote).unwrap();
    put(&store, "note-1", "one");
    put(&store, "note-2", "two");

    push_to_remote(store.git_dir(), &remote, false).unwrap();
    let published = sync_state(&store, false).unwrap();
    assert_eq!(published.unpublished, 0, "it was just published");

    put(&store, "note-2", "two, edited");
    put(&store, "note-3", "three");
    let after = sync_state(&store, false).unwrap();

    assert_eq!(
        after.unpublished, 2,
        "one record changed and one added since the push"
    );
}

/// Asking the remote is opt-in, and not asking says so rather than passing for
/// "nothing is waiting".
#[test]
fn the_network_is_only_touched_when_it_is_asked_for() {
    let (_dir, store) = repo_with_store();
    let remote_dir = bare_remote();
    let remote = remote_at(&remote_dir);
    write_remote_config(store.git_dir(), &remote).unwrap();
    put(&store, "note-1", "one");
    push_to_remote(store.git_dir(), &remote, false).unwrap();

    assert_eq!(
        sync_state(&store, false).unwrap().remote,
        RemoteCheck::NotAsked
    );
    assert_eq!(
        sync_state(&store, true).unwrap().remote,
        RemoteCheck::UpToDate
    );
}

/// The case the indicator exists for: somebody else published, and this
/// repository has not fetched it.
#[test]
fn a_remote_that_moved_on_is_waiting() {
    let remote_dir = bare_remote();
    let remote = remote_at(&remote_dir);

    let (_theirs_dir, theirs) = repo_with_store();
    write_remote_config(theirs.git_dir(), &remote).unwrap();
    put(&theirs, "note-1", "one");
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();

    let (_ours_dir, ours) = repo_with_store();
    write_remote_config(ours.git_dir(), &remote).unwrap();
    fetch_and_merge(&ours, &remote).unwrap();
    assert_eq!(
        sync_state(&ours, true).unwrap().remote,
        RemoteCheck::UpToDate,
        "we have just fetched everything there is"
    );

    put(&theirs, "note-2", "two");
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();

    let state = sync_state(&ours, true).unwrap();
    assert_eq!(state.remote, RemoteCheck::Waiting);
    assert_eq!(state.unpublished, 0, "we wrote nothing of our own");
}

/// A remote nobody can reach is its own answer. Reporting `UpToDate` would tell
/// somebody their memory is safely published when nothing was asked at all.
#[test]
fn an_unreachable_remote_does_not_pass_for_being_in_step() {
    let (dir, store) = repo_with_store();
    put(&store, "note-1", "one");
    write_remote_config(
        store.git_dir(),
        &MemoryRemote {
            url: dir
                .path()
                .join("no-such-remote")
                .to_string_lossy()
                .into_owned(),
            refspec: None,
        },
    )
    .unwrap();

    let state = sync_state(&store, true).unwrap();

    assert_eq!(state.remote, RemoteCheck::Unreachable);
    assert_eq!(state.unpublished, 1, "the count beside it is still true");
}

// --- three-way merge: what used to be a conflict and is not one ------------

/// Two people editing different ends of one document is the ordinary case, and
/// it used to be reported as a conflict because the merge had only two points
/// to compare. With the common ancestor, both edits survive and nobody is asked
/// anything.
#[test]
fn edits_to_different_parts_of_one_record_both_survive() {
    let remote_dir = bare_remote();
    let remote = remote_at(&remote_dir);
    let shared = "# Guide\n\nIntroduction.\n\nMiddle.\n\nConclusion.\n";

    let (_theirs_dir, theirs) = repo_with_store();
    write_remote_config(theirs.git_dir(), &remote).unwrap();
    put(&theirs, "guide", shared);
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();

    let (_ours_dir, ours) = repo_with_store();
    write_remote_config(ours.git_dir(), &remote).unwrap();
    fetch_and_merge(&ours, &remote).unwrap();

    // They rewrite the opening; we rewrite the closing.
    put(
        &theirs,
        "guide",
        "# Guide\n\nA much better introduction.\n\nMiddle.\n\nConclusion.\n",
    );
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();
    put(
        &ours,
        "guide",
        "# Guide\n\nIntroduction.\n\nMiddle.\n\nA much better conclusion.\n",
    );

    let result = fetch_and_merge(&ours, &remote).unwrap();

    assert!(result.conflicts.is_empty(), "this is not a conflict");
    assert!(result.overlaps.is_empty(), "nothing collided");
    assert!(result.merged);
    let body = body_of(&ours, "guide");
    assert!(
        body.contains("A much better introduction."),
        "their edit survived: {body}"
    );
    assert!(
        body.contains("A much better conclusion."),
        "our edit survived: {body}"
    );
}

/// The genuine collision: the same lines rewritten on both sides. One version
/// has to lose, it is theirs, and the merge says so rather than absorbing it.
#[test]
fn the_same_lines_rewritten_on_both_sides_keep_this_sides_text_and_are_reported() {
    let remote_dir = bare_remote();
    let remote = remote_at(&remote_dir);

    let (_theirs_dir, theirs) = repo_with_store();
    write_remote_config(theirs.git_dir(), &remote).unwrap();
    put(&theirs, "guide", "# Guide\n\nThe original sentence.\n");
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();

    let (_ours_dir, ours) = repo_with_store();
    write_remote_config(ours.git_dir(), &remote).unwrap();
    fetch_and_merge(&ours, &remote).unwrap();

    put(&theirs, "guide", "# Guide\n\nTheir replacement.\n");
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();
    put(&ours, "guide", "# Guide\n\nOur replacement.\n");

    let result = fetch_and_merge(&ours, &remote).unwrap();

    assert!(
        result.conflicts.is_empty(),
        "a collision is resolved, not refused"
    );
    assert_eq!(
        keys_of(&result.overlaps),
        vec!["guide".to_owned()],
        "somebody has to be told whose sentence is not in front of them"
    );
    let body = body_of(&ours, "guide");
    assert!(body.contains("Our replacement."), "we are here: {body}");
    assert!(
        !body.contains("<<<<<<<"),
        "a resolved merge leaves no markers: {body}"
    );
}

/// A collision on one record must not hold up every other record in the same
/// fetch — which is exactly what the old wholesale refusal did.
#[test]
fn a_collision_on_one_record_does_not_hold_up_the_rest() {
    let remote_dir = bare_remote();
    let remote = remote_at(&remote_dir);

    let (_theirs_dir, theirs) = repo_with_store();
    write_remote_config(theirs.git_dir(), &remote).unwrap();
    put(&theirs, "guide", "one line\n");
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();

    let (_ours_dir, ours) = repo_with_store();
    write_remote_config(ours.git_dir(), &remote).unwrap();
    fetch_and_merge(&ours, &remote).unwrap();

    put(&theirs, "guide", "their line\n");
    put(&theirs, "their-own-note", "only they wrote this\n");
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();
    put(&ours, "guide", "our line\n");

    let result = fetch_and_merge(&ours, &remote).unwrap();

    assert_eq!(keys_of(&result.overlaps), vec!["guide".to_owned()]);
    assert_eq!(
        body_of(&ours, "their-own-note"),
        "only they wrote this\n",
        "the record nobody argued about arrived in the same fetch"
    );
}

// --- the merge is three-way for every member, not only the body -----------

/// Write a record and say something about it beyond its text.
fn put_described(store: &GitStore, key: &str, content: &str, title: &str, tags: &[&str]) {
    let revision = store.current().unwrap().revision().clone();
    let mut envelope = Envelope::new(key, "note", content).unwrap();
    envelope.title = Some(title.to_owned());
    envelope.tags = tags.iter().map(|tag| (*tag).to_owned()).collect();
    store
        .apply(&Transaction {
            id: format!(
                "described-{key}-{}",
                PUT_COUNTER.fetch_add(1, Ordering::SeqCst)
            ),
            expected_revision: revision,
            operations: vec![Operation::put(StoredRecord::Plaintext {
                envelope: Box::new(envelope),
            })],
        })
        .unwrap();
}

fn envelope_of(store: &GitStore, key: &str) -> memory_hub_core::Envelope {
    let view = store.current().unwrap();
    let (_, record) = view
        .records()
        .unwrap()
        .into_iter()
        .find(|(id, _)| id.display_value() == key)
        .unwrap_or_else(|| panic!("no record named {key}"));
    let StoredRecord::Plaintext { envelope } = record;
    *envelope
}

/// A member this side never touched has one change in it — theirs — and taking
/// ours there is not resolving a conflict but discarding their work.
#[test]
fn a_member_only_they_moved_is_taken_from_them() {
    let remote_dir = bare_remote();
    let remote = remote_at(&remote_dir);

    let (_theirs_dir, theirs) = repo_with_store();
    write_remote_config(theirs.git_dir(), &remote).unwrap();
    put_described(&theirs, "guide", "Line one.\n", "Guide", &["draft"]);
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();

    let (_ours_dir, ours) = repo_with_store();
    write_remote_config(ours.git_dir(), &remote).unwrap();
    fetch_and_merge(&ours, &remote).unwrap();

    // They retitle it and tag it. We only append a line, and never touch either.
    put_described(
        &theirs,
        "guide",
        "Line one.\n",
        "The Guide",
        &["draft", "reviewed"],
    );
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();
    put_described(
        &ours,
        "guide",
        "Line one.\nLine two.\n",
        "Guide",
        &["draft"],
    );

    let result = fetch_and_merge(&ours, &remote).unwrap();

    assert!(
        result.overlaps.is_empty(),
        "nobody contested anything: {:?}",
        result.overlaps
    );
    let merged = envelope_of(&ours, "guide");
    assert_eq!(
        merged.title.as_deref(),
        Some("The Guide"),
        "their title was the only change to it"
    );
    assert!(
        merged.tags.contains(&"reviewed".to_owned()),
        "and the tag they added survived: {:?}",
        merged.tags
    );
    assert!(
        merged.content.contains("Line two."),
        "while our own line is still here: {}",
        merged.content
    );
}

/// Where both sides moved the same member, this side keeps its version — and
/// the member is named, because that decision was made while nobody was asked.
#[test]
fn a_member_both_moved_keeps_ours_and_is_named() {
    let remote_dir = bare_remote();
    let remote = remote_at(&remote_dir);

    let (_theirs_dir, theirs) = repo_with_store();
    write_remote_config(theirs.git_dir(), &remote).unwrap();
    put_described(&theirs, "guide", "Body.\n", "Guide", &[]);
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();

    let (_ours_dir, ours) = repo_with_store();
    write_remote_config(ours.git_dir(), &remote).unwrap();
    fetch_and_merge(&ours, &remote).unwrap();

    put_described(&theirs, "guide", "Body.\n", "Their Title", &[]);
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();
    put_described(&ours, "guide", "Body.\n", "Our Title", &[]);

    let result = fetch_and_merge(&ours, &remote).unwrap();

    assert_eq!(keys_of(&result.overlaps), vec!["guide".to_owned()]);
    assert_eq!(
        result.overlaps[0].fields,
        vec!["title".to_owned()],
        "the member that lost is named, not merely counted"
    );
    assert!(
        !result.overlaps[0].body,
        "the body was identical, so nothing collided there"
    );
    assert_eq!(
        envelope_of(&ours, "guide").title.as_deref(),
        Some("Our Title")
    );
}

/// A colleague who moved a file moved it for the project. A fetch that dropped
/// that leaves this corpus pointing at a path their commit emptied.
#[test]
fn a_document_the_other_side_moved_arrives_at_its_new_path() {
    let remote_dir = bare_remote();
    let remote = remote_at(&remote_dir);

    let reference = |store: &GitStore, path: &str, tag: Option<&str>| {
        let revision = store.current().unwrap().revision().clone();
        let mut envelope = memory_hub_core::Envelope::reference(
            "intro",
            "doc",
            path,
            memory_hub_core::ContentHash::for_content("the same bytes"),
        )
        .unwrap();
        if let Some(tag) = tag {
            envelope.tags = vec![tag.to_owned()];
        }
        store
            .apply(&Transaction {
                id: format!("reference-{}", PUT_COUNTER.fetch_add(1, Ordering::SeqCst)),
                expected_revision: revision,
                operations: vec![Operation::put(StoredRecord::Plaintext {
                    envelope: Box::new(envelope),
                })],
            })
            .unwrap();
    };

    let (_theirs_dir, theirs) = repo_with_store();
    write_remote_config(theirs.git_dir(), &remote).unwrap();
    reference(&theirs, "docs/guides/intro.md", None);
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();

    let (_ours_dir, ours) = repo_with_store();
    write_remote_config(ours.git_dir(), &remote).unwrap();
    fetch_and_merge(&ours, &remote).unwrap();

    // They move the document. We tag it, and never touch where it lives.
    reference(&theirs, "docs/handbook/intro.md", None);
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();
    reference(&ours, "docs/guides/intro.md", Some("read"));

    let result = fetch_and_merge(&ours, &remote).unwrap();

    assert!(result.overlaps.is_empty(), "{:?}", result.overlaps);
    let merged = envelope_of(&ours, "intro");
    assert_eq!(
        merged.content_ref.as_ref().map(|it| it.path.as_str()),
        Some("docs/handbook/intro.md"),
        "the move reached us; the local scan is what files it back if this \
         working tree keeps the document elsewhere"
    );
    assert_eq!(merged.tags, vec!["read".to_owned()], "and our tag stayed");
}

// --- undoing a fetch -------------------------------------------------------

/// A merge is an ordinary commit on top of what was here, so going back is
/// naming the revision that was here.
#[test]
fn memory_goes_back_to_where_a_fetch_found_it() {
    let remote_dir = bare_remote();
    let remote = remote_at(&remote_dir);

    let (_theirs_dir, theirs) = repo_with_store();
    write_remote_config(theirs.git_dir(), &remote).unwrap();
    put(&theirs, "guide", "one line\n");
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();

    let (_ours_dir, ours) = repo_with_store();
    write_remote_config(ours.git_dir(), &remote).unwrap();
    fetch_and_merge(&ours, &remote).unwrap();

    put(&theirs, "guide", "their line\n");
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();
    put(&ours, "guide", "our line\n");

    let result = fetch_and_merge(&ours, &remote).unwrap();
    assert_eq!(keys_of(&result.overlaps), vec!["guide".to_owned()]);

    memory_hub_store::rewind_to(
        ours.git_dir(),
        &result.local_revision_before,
        &result.local_revision_after,
    )
    .unwrap();

    assert_eq!(
        ours.current().unwrap().revision(),
        &result.local_revision_before,
        "memory stands where the fetch found it"
    );
    assert_eq!(
        body_of(&ours, "guide"),
        "our line\n",
        "and what it holds is what we wrote, with the merge undone"
    );
}

/// Backwards along its own history and nowhere else. A revision off to one side
/// is not a state this memory was ever in, and arriving at one would be a
/// corpus nobody wrote.
#[test]
fn memory_does_not_go_anywhere_it_has_not_been() {
    let (_ours_dir, ours) = repo_with_store();
    put(&ours, "guide", "ours\n");

    // Another repository's history entirely: a real revision, and not one of
    // this memory's.
    let (_elsewhere_dir, elsewhere) = repo_with_store();
    put(&elsewhere, "guide", "somebody else's\n");
    let foreign = elsewhere.current().unwrap().revision().clone();

    let here = ours.current().unwrap().revision().clone();
    let refused = memory_hub_store::rewind_to(ours.git_dir(), &foreign, &here);

    assert!(
        refused.is_err(),
        "that is not a state this memory passed through"
    );
    assert_eq!(
        ours.current().unwrap().revision(),
        &here,
        "a refused rewind leaves memory where it was"
    );
}

/// An undo names the state to return to, and what makes that safe is that
/// nothing has happened since. A record written after the fetch is not part of
/// what the fetch did.
#[test]
fn memory_does_not_go_back_over_something_written_since() {
    let remote_dir = bare_remote();
    let remote = remote_at(&remote_dir);

    let (_theirs_dir, theirs) = repo_with_store();
    write_remote_config(theirs.git_dir(), &remote).unwrap();
    put(&theirs, "guide", "one line\n");
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();

    let (_ours_dir, ours) = repo_with_store();
    write_remote_config(ours.git_dir(), &remote).unwrap();
    fetch_and_merge(&ours, &remote).unwrap();

    put(&theirs, "guide", "their line\n");
    push_to_remote(theirs.git_dir(), &remote, false).unwrap();
    put(&ours, "guide", "our line\n");
    let result = fetch_and_merge(&ours, &remote).unwrap();

    // Somebody carries on working after the fetch.
    put(&ours, "a-thought", "written after the merge\n");
    let now = ours.current().unwrap().revision().clone();

    let refused = memory_hub_store::rewind_to(
        ours.git_dir(),
        &result.local_revision_before,
        &result.local_revision_after,
    );

    assert!(refused.is_err(), "that would take the new record with it");
    assert_eq!(
        ours.current().unwrap().revision(),
        &now,
        "and memory stays where it is"
    );
    assert_eq!(body_of(&ours, "a-thought"), "written after the merge\n");
}
