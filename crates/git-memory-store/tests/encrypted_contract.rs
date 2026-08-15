//! Encrypted store contract tests using age X25519 identities.
//!
//! These tests exercise the full encrypted store: init → apply → get → list,
//! lock/unlock, add/remove recipient, and update/delete operations.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use git_memory_crypto::{Identity, generate_backup_identity};
use git_memory_store::{EncryptedStore, RecipientEntry};
use tempfile::TempDir;

fn init_repo() -> TempDir {
    let dir = tempfile::tempdir().expect("temp dir");
    git2::Repository::init(dir.path()).expect("git init");
    dir
}

fn make_identity() -> (age::x25519::Identity, age::x25519::Recipient) {
    generate_backup_identity()
}

fn box_identity(id: &age::x25519::Identity) -> Identity {
    Box::new(id.clone())
}

fn recipient_entry(recipient: &age::x25519::Recipient, label: &str) -> RecipientEntry {
    RecipientEntry {
        public_key: recipient.to_string(),
        key_type: "x25519".to_string(),
        label: Some(label.to_string()),
    }
}

/// Set up a store: init repo, open, init with owner recipient, unlock.
fn setup_store(
    dir: &TempDir,
    owner: &(age::x25519::Identity, age::x25519::Recipient),
) -> EncryptedStore {
    let mut store = EncryptedStore::open_locked(dir.path()).expect("open");
    store.unlock(box_identity(&owner.0)).expect("unlock");
    store
        .init(vec![recipient_entry(&owner.1, "owner")])
        .expect("init");
    store
}

fn make_envelope(key: &str, content: &str) -> git_memory_core::Envelope {
    git_memory_core::Envelope::new(key, "note", content).expect("envelope is valid")
}

#[test]
fn full_round_trip() {
    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store(&dir, &owner);

    let rev = store.current_revision().expect("revision");
    store
        .apply(
            "tx-1",
            rev,
            &[
                ("alpha", make_envelope("alpha", "first record")),
                ("beta", make_envelope("beta", "second record")),
            ],
            &[],
        )
        .expect("apply");

    let got1 = store.get("alpha").expect("get alpha");
    let got2 = store.get("beta").expect("get beta");

    assert_eq!(got1.as_ref().unwrap().content, "first record");
    assert_eq!(got1.as_ref().unwrap().kind, "note");
    assert_eq!(got2.as_ref().unwrap().content, "second record");
}

#[test]
fn locked_store_rejects_reads_and_writes() {
    let dir = init_repo();
    let store = EncryptedStore::open_locked(dir.path()).expect("open");
    assert!(!store.is_unlocked());

    assert!(store.get("any").is_err());
}

#[test]
fn unlock_with_wrong_identity_fails_when_manifest_exists() {
    let dir = init_repo();
    let owner = make_identity();
    let _store = setup_store(&dir, &owner);

    // Reopen and try to unlock with wrong identity.
    let mut store2 = EncryptedStore::open_locked(dir.path()).expect("open");
    let (wrong_id, _) = make_identity();
    let result = store2.unlock(box_identity(&wrong_id));
    assert!(result.is_err(), "wrong identity must be rejected");
    assert!(!store2.is_unlocked());
}

#[test]
fn unlock_succeeds_when_no_manifest_exists() {
    let dir = init_repo();
    let owner = make_identity();

    // No init — manifest doesn't exist yet.
    let mut store = EncryptedStore::open_locked(dir.path()).expect("open");
    let result = store.unlock(box_identity(&owner.0));
    assert!(result.is_ok(), "any identity accepted when no manifest");
    assert!(store.is_unlocked());
}

#[test]
fn list_returns_all_records() {
    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store(&dir, &owner);

    let rev = store.current_revision().expect("revision");
    store
        .apply(
            "tx-1",
            rev,
            &[
                ("alpha", make_envelope("alpha", "one")),
                ("beta", make_envelope("beta", "two")),
                ("gamma", make_envelope("gamma", "three")),
            ],
            &[],
        )
        .expect("apply");

    let list = store.list().expect("list");
    assert_eq!(list.len(), 3);
    assert_eq!(list[0].0, "alpha");
    assert_eq!(list[1].0, "beta");
    assert_eq!(list[2].0, "gamma");
}

#[test]
fn delete_removes_record() {
    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store(&dir, &owner);

    let rev = store.current_revision().expect("revision");
    store
        .apply(
            "tx-1",
            rev,
            &[("alpha", make_envelope("alpha", "data"))],
            &[],
        )
        .expect("apply");

    let rev2 = store.current_revision().expect("revision 2");
    store.apply("tx-2", rev2, &[], &["alpha"]).expect("delete");

    assert!(store.get("alpha").expect("get").is_none());
}

#[test]
fn update_record_replaces_old_content() {
    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store(&dir, &owner);

    let rev = store.current_revision().expect("revision");
    store
        .apply(
            "tx-1",
            rev,
            &[("alpha", make_envelope("alpha", "original"))],
            &[],
        )
        .expect("apply first");

    let rev2 = store.current_revision().expect("revision 2");
    store
        .apply(
            "tx-2",
            rev2,
            &[("alpha", make_envelope("alpha", "updated"))],
            &[],
        )
        .expect("apply update");

    let got = store.get("alpha").expect("get after update");
    assert_eq!(got.as_ref().unwrap().content, "updated");

    let list = store.list().expect("list");
    assert_eq!(list.len(), 1, "only one record should exist after update");
}

#[test]
fn lock_blocks_access_then_unlock_restores() {
    let dir = init_repo();
    let owner = make_identity();
    let mut store = setup_store(&dir, &owner);

    let rev = store.current_revision().expect("revision");
    store
        .apply(
            "tx-1",
            rev,
            &[("alpha", make_envelope("alpha", "persisted"))],
            &[],
        )
        .expect("apply");

    store.lock();
    assert!(!store.is_unlocked());
    assert!(store.get("alpha").is_err());

    store.unlock(box_identity(&owner.0)).expect("re-unlock");
    let got = store.get("alpha").expect("get after re-unlock");
    assert_eq!(got.as_ref().unwrap().content, "persisted");
}

#[test]
fn add_recipient_grants_access() {
    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store(&dir, &owner);

    let rev = store.current_revision().expect("revision");
    store
        .apply(
            "tx-1",
            rev,
            &[("secret", make_envelope("secret", "shared data"))],
            &[],
        )
        .expect("apply");

    // Add Bob as recipient.
    let bob = make_identity();
    store
        .add_recipient(recipient_entry(&bob.1, "bob"))
        .expect("add recipient");

    // Bob can now decrypt.
    let mut store_bob = EncryptedStore::open_locked(dir.path()).expect("open for bob");
    store_bob.unlock(box_identity(&bob.0)).expect("bob unlock");
    let got = store_bob.get("secret").expect("bob get");
    assert_eq!(got.as_ref().unwrap().content, "shared data");
}

#[test]
fn remove_recipient_blocks_new_data() {
    let dir = init_repo();
    let owner = make_identity();
    let bob = make_identity();
    let store = setup_store_with_recipients(&dir, &owner, &[&bob]);

    // Both can read.
    let rev = store.current_revision().expect("revision");
    store
        .apply(
            "tx-1",
            rev,
            &[("shared", make_envelope("shared", "both can read"))],
            &[],
        )
        .expect("apply");

    // Bob can read.
    let mut store_bob = EncryptedStore::open_locked(dir.path()).expect("open bob 1");
    store_bob
        .unlock(box_identity(&bob.0))
        .expect("bob unlock 1");
    assert!(store_bob.get("shared").unwrap().is_some());

    // Remove Bob.
    store
        .remove_recipient(&bob.1.to_string())
        .expect("remove bob");

    // Write new data after removal.
    let rev2 = store.current_revision().expect("revision 2");
    store
        .apply(
            "tx-2",
            rev2,
            &[("new-data", make_envelope("new-data", "bob cannot read"))],
            &[],
        )
        .expect("apply after removal");

    // Bob cannot unlock anymore (manifest encrypted without his key).
    let mut store_bob2 = EncryptedStore::open_locked(dir.path()).expect("open bob 2");
    let result = store_bob2.unlock(box_identity(&bob.0));
    assert!(result.is_err(), "bob should be locked out after removal");
}

#[test]
fn init_rejects_duplicate() {
    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store(&dir, &owner);

    // Second init must fail.
    let result = store.init(vec![recipient_entry(&owner.1, "owner")]);
    assert!(result.is_err());
}

#[test]
fn remove_last_recipient_fails() {
    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store(&dir, &owner);

    let result = store.remove_recipient(&owner.1.to_string());
    assert!(result.is_err(), "cannot remove the last recipient");
}

#[test]
fn list_recipients_shows_all() {
    let dir = init_repo();
    let owner = make_identity();
    let bob = make_identity();
    let store = setup_store_with_recipients(&dir, &owner, &[&bob]);

    let recipients = store.list_recipients().expect("list recipients");
    assert_eq!(recipients.len(), 2);
}

#[test]
fn git_tree_contains_no_plaintext() {
    use std::process::Command;

    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store(&dir, &owner);

    let rev = store.current_revision().expect("revision");
    let env = make_envelope("secret-key", "this is sensitive content");
    store
        .apply("tx-1", rev, &[("secret-key", env)], &[])
        .expect("apply");

    let output = Command::new("git")
        .args(["log", "--all", "--format=%B"])
        .current_dir(dir.path())
        .output()
        .expect("git log");

    let log = String::from_utf8_lossy(&output.stdout);
    assert!(
        !log.contains("sensitive content"),
        "content leaked in commit"
    );
    assert!(!log.contains("secret-key"), "key name leaked in commit");
}

#[test]
fn apply_with_empty_puts_and_deletes_is_noop() {
    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store(&dir, &owner);

    let rev = store.current_revision().expect("revision");
    let result = store.apply("tx-empty", rev, &[], &[]);
    assert!(result.is_ok(), "empty apply should succeed");
}

/// Helper: set up a store with multiple recipients.
fn setup_store_with_recipients(
    dir: &TempDir,
    owner: &(age::x25519::Identity, age::x25519::Recipient),
    others: &[&(age::x25519::Identity, age::x25519::Recipient)],
) -> EncryptedStore {
    let mut store = EncryptedStore::open_locked(dir.path()).expect("open");
    store.unlock(box_identity(&owner.0)).expect("unlock");

    let mut recipients = vec![recipient_entry(&owner.1, "owner")];
    for (i, (_, recip)) in others.iter().enumerate() {
        recipients.push(recipient_entry(recip, &format!("member-{i}")));
    }
    store.init(recipients).expect("init");
    store
}
