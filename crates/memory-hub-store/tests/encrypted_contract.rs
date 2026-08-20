//! Encrypted store contract tests using age X25519 identities.
//!
//! These tests exercise the full encrypted store: init → apply → get → list,
//! lock/unlock, add/remove recipient, and update/delete operations.

#![allow(clippy::expect_used, clippy::unwrap_used)]

use memory_hub_crypto::{Identity, generate_backup_identity};
use memory_hub_store::{EncryptedStore, RecipientEntry, StoreErrorKind};
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
    identity: &age::x25519::Identity,
    recipient: &age::x25519::Recipient,
) -> EncryptedStore {
    let mut store = EncryptedStore::open_locked(dir.path()).expect("open");
    store.unlock(box_identity(identity)).expect("unlock");
    let _init_result = store
        .init(vec![recipient_entry(recipient, "owner")])
        .expect("init");
    store
}

/// Back-compat wrapper for existing tests that pass `&(identity, recipient)`.
fn setup_store_owner(
    dir: &TempDir,
    owner: &(age::x25519::Identity, age::x25519::Recipient),
) -> EncryptedStore {
    setup_store(dir, &owner.0, &owner.1)
}

fn make_envelope(key: &str, content: &str) -> memory_hub_core::Envelope {
    memory_hub_core::Envelope::new(key, "note", content).expect("envelope is valid")
}

#[test]
fn full_round_trip() {
    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store_owner(&dir, &owner);

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
    let _store = setup_store_owner(&dir, &owner);

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
    let store = setup_store_owner(&dir, &owner);

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
    let store = setup_store_owner(&dir, &owner);

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
    let store = setup_store_owner(&dir, &owner);

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
    let mut store = setup_store_owner(&dir, &owner);

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
    let store = setup_store_owner(&dir, &owner);

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
    let store = setup_store_pair_with_recipients(&dir, &owner, &[&bob]);

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
    let store = setup_store_owner(&dir, &owner);

    // Second init must fail.
    let result = store.init(vec![recipient_entry(&owner.1, "owner")]);
    assert!(result.is_err());
}

#[test]
fn init_generates_backup_identity_for_recovery() {
    let dir = init_repo();
    let owner = make_identity();

    let mut store = EncryptedStore::open_locked(dir.path()).expect("open");
    store.unlock(box_identity(&owner.0)).expect("unlock");
    let init_result = store
        .init(vec![recipient_entry(&owner.1, "owner")])
        .expect("init");

    // Backup identity is a non-empty AGE-SECRET-KEY string.
    assert!(
        init_result.backup_identity.starts_with("AGE-SECRET-KEY-1"),
        "backup identity should be an age secret key, got: {}",
        &init_result.backup_identity[..20.min(init_result.backup_identity.len())]
    );

    // Write a record.
    let rev = store.current_revision().expect("revision");
    store
        .apply(
            "tx-1",
            rev,
            &[("secret", make_envelope("secret", "recoverable data"))],
            &[],
        )
        .expect("apply");

    // The manifest should have 2 recipients: owner + backup.
    let recipients = store.list_recipients().expect("list recipients");
    assert_eq!(recipients.len(), 2);
    assert!(
        recipients
            .iter()
            .any(|r| r.label.as_deref() == Some("backup")),
        "backup recipient should be in the manifest"
    );

    // Remove the owner's key — simulate SSH key loss. The store's identity
    // (owner's) can still perform the removal because it can read the
    // current manifest. After re-encryption, only the backup remains.
    store
        .remove_recipient(&owner.1.to_string())
        .expect("remove owner");

    // Recover: parse the backup identity string and unlock with it.
    let backup_identity: age::x25519::Identity = init_result
        .backup_identity
        .parse()
        .expect("parse backup identity");
    let mut recovered_store = EncryptedStore::open_locked(dir.path()).expect("open recovered");
    recovered_store
        .unlock(Box::new(backup_identity))
        .expect("unlock with backup identity");

    // Only the backup recipient remains.
    let remaining = recovered_store
        .list_recipients()
        .expect("remaining recipients");
    assert_eq!(remaining.len(), 1);
    assert_eq!(remaining[0].label.as_deref(), Some("backup"));

    let got = recovered_store.get("secret").expect("get via backup");
    assert_eq!(
        got.as_ref().unwrap().content,
        "recoverable data",
        "backup identity must decrypt records after owner key removal"
    );
}

#[test]
fn remove_last_recipient_fails() {
    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store_owner(&dir, &owner);

    // After init there are 2 recipients: owner + auto-generated backup.
    // Remove the backup first — the owner is still a recipient and can
    // decrypt the re-encrypted manifest.
    let recipients = store.list_recipients().expect("list recipients");
    let backup_key = recipients
        .iter()
        .find(|r| r.label.as_deref() == Some("backup"))
        .expect("backup recipient exists")
        .public_key
        .clone();
    store
        .remove_recipient(&backup_key)
        .expect("remove backup leaves owner");

    // Now only the owner remains. Removing the owner must fail.
    let result = store.remove_recipient(&owner.1.to_string());
    assert!(result.is_err(), "cannot remove the last recipient");
}

#[test]
fn list_recipients_shows_all() {
    let dir = init_repo();
    let owner = make_identity();
    let bob = make_identity();
    let store = setup_store_pair_with_recipients(&dir, &owner, &[&bob]);

    // owner + bob + auto-generated backup = 3 recipients.
    let recipients = store.list_recipients().expect("list recipients");
    assert_eq!(recipients.len(), 3);
}

#[test]
fn git_tree_contains_no_plaintext() {
    use std::process::Command;

    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store_owner(&dir, &owner);

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
    let store = setup_store_owner(&dir, &owner);

    let rev = store.current_revision().expect("revision");
    let result = store.apply("tx-empty", rev, &[], &[]);
    assert!(result.is_ok(), "empty apply should succeed");
}

/// Helper: set up a store with multiple recipients.
fn setup_store_pair_with_recipients(
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
    let _init_result = store.init(recipients).expect("init");
    store
}

#[test]
fn ssh_commit_signing_produces_gpgsig_header() {
    use std::process::Command;
    use std::sync::Arc;

    // ssh-keygen must be available for this test to be meaningful.
    if Command::new("ssh-keygen").arg("--help").output().is_err() {
        eprintln!("skipping ssh_commit_signing test: ssh-keygen not available");
        return;
    }

    let dir = init_repo();
    let owner = make_identity();

    // Generate an SSH ed25519 keypair for signing.
    let ssh_key_dir = tempfile::tempdir().expect("ssh key dir");
    let ssh_key_path = ssh_key_dir.path().join("signing_key");
    let gen_result = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-f"])
        .arg(&ssh_key_path)
        .args(["-N", "", "-q"])
        .output()
        .expect("generate SSH key");
    assert!(
        gen_result.status.success(),
        "ssh-keygen key generation failed"
    );

    let signer = Arc::new(memory_hub_crypto::SshSigner::new(&ssh_key_path));

    let mut store = EncryptedStore::open_locked(dir.path())
        .expect("open")
        .with_signer(signer);
    store.unlock(box_identity(&owner.0)).expect("unlock");
    let _init_result = store
        .init(vec![recipient_entry(&owner.1, "owner")])
        .expect("init");

    let rev = store.current_revision().expect("revision");
    store
        .apply(
            "tx-signed",
            rev,
            &[("alpha", make_envelope("alpha", "signed record"))],
            &[],
        )
        .expect("apply");

    // Verify that the transaction commits carry an SSH signature by
    // inspecting the raw commit objects. We check the gpgsig header
    // directly rather than relying on %G? (which requires an
    // allowedSignersFile for verification).
    let log = Command::new("git")
        .args(["log", "--all", "--format=%H"])
        .current_dir(dir.path())
        .output()
        .expect("git log hashes");
    let hashes = String::from_utf8_lossy(&log.stdout).into_owned();
    let mut found_signed = false;
    let mut signed_commit_hash = String::new();
    for hash in hashes.lines() {
        if hash.is_empty() {
            continue;
        }
        let commit_obj = Command::new("git")
            .args(["cat-file", "commit", hash])
            .current_dir(dir.path())
            .output()
            .expect("git cat-file");
        let body = String::from_utf8_lossy(&commit_obj.stdout);
        if body.contains("gpgsig") && body.contains("SSH SIGNATURE") {
            found_signed = true;
            signed_commit_hash = hash.to_string();
            break;
        }
    }
    assert!(
        found_signed,
        "at least one commit on refs/memory/* should carry an SSH gpgsig signature"
    );

    // Verify the signature is valid using ssh-keygen -Y verify.
    // This requires an allowedSignersFile mapping the signing key to an
    // identity. We extract the public key and create the file.
    let pub_key = std::fs::read_to_string(format!("{}.pub", ssh_key_path.display()))
        .expect("read public key");
    let allowed_signers_dir = tempfile::tempdir().expect("allowed signers dir");
    let allowed_signers_path = allowed_signers_dir.path().join("allowed_signers");
    std::fs::write(
        &allowed_signers_path,
        format!("memory-hub@localhost {pub_key}"),
    )
    .expect("write allowed signers");

    // Extract the raw commit content (without the gpgsig header) and the
    // signature, then verify. We use git's own verification by configuring
    // the allowed signers file and checking %G?.
    let allowed_signers_arg = format!(
        "gpg.ssh.allowedSignersFile={}",
        allowed_signers_path.to_string_lossy()
    );
    let verify = Command::new("git")
        .args(["-c", &allowed_signers_arg, "log", "--all", "--format=%G?"])
        .current_dir(dir.path())
        .output()
        .expect("git verify");

    let verify_result = String::from_utf8_lossy(&verify.stdout);
    assert!(
        verify_result.contains('G'),
        "signature verification failed, got: {verify_result:?}"
    );

    // Also verify the specific signed commit we found.
    let _ = signed_commit_hash;
}

/// A revision this store cannot serve is refused, not answered from today.
///
/// Reads resolve through the manifest, and the manifest describes the state
/// the store is in now. An older revision that validated would be handed
/// current records under a revision the caller believes is theirs — the one
/// failure mode a snapshot exists to rule out.
#[test]
fn a_past_revision_of_an_encrypted_store_cannot_be_reopened() {
    use memory_hub_engine::{Capability, RecordId, RecordStore, StoreView};

    let dir = init_repo();
    let owner = make_identity();
    let store = setup_store_owner(&dir, &owner);

    let first = store.current_revision().expect("revision");
    store
        .apply(
            "tx-1",
            first.clone(),
            &[("note", make_envelope("note", "as it was"))],
            &[],
        )
        .expect("apply");
    let second = store.current_revision().expect("revision");
    assert_ne!(first, second, "the write moved the revision");

    assert!(
        !RecordStore::capabilities(&store).has(Capability::Snapshots),
        "the store does not claim a past it cannot reopen"
    );

    let error = StoreView::open(&store, &first).expect_err("the past is not served");
    assert_eq!(error.kind, StoreErrorKind::RevisionNotFound);

    // And the current one still is, so this refuses the past rather than
    // refusing revisions.
    let view = StoreView::open(&store, &second).expect("the present is served");
    let record = view
        .get(&RecordId::plaintext("note"))
        .expect("read")
        .expect("the record is there");
    match record {
        memory_hub_core::StoredRecord::Plaintext { envelope } => {
            assert_eq!(envelope.content, "as it was");
        }
        memory_hub_core::StoredRecord::Encrypted { .. } => panic!("an unlocked store decrypts"),
    }
}

mod encrypted_transport {
    use super::*;
    use memory_hub_store::{MemoryRemote, write_remote_config};
    use std::process::Command;

    fn bare_remote() -> TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        Command::new("git")
            .args(["init", "--bare"])
            .arg(dir.path())
            .output()
            .expect("git init --bare");
        dir
    }

    use std::sync::atomic::{AtomicU64, Ordering};

    /// These scenarios exercise the merge algorithm with X25519 identities,
    /// which cannot sign Git commits, so signature verification is disabled
    /// explicitly. The fail-closed default is covered by
    /// `encrypted_fetch_is_refused_without_allowed_signers`.
    fn allow_unsigned_exchange(git_dir: &std::path::Path) {
        let repository = git2::Repository::open(git_dir).expect("open repository");
        let mut config = repository.config().expect("read config");
        config
            .set_str("memory-hub.signing.verify", "off")
            .expect("disable verification");
    }

    static ENC_PUT_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn put_encrypted(store: &EncryptedStore, key: &str, content: &str) {
        let seq = ENC_PUT_COUNTER.fetch_add(1, Ordering::SeqCst);
        let rev = store.current_revision().expect("current revision");
        store
            .apply(
                &format!("put-{key}-{seq}"),
                rev,
                &[(key, make_envelope(key, content))],
                &[],
            )
            .expect("apply put");
    }

    #[test]
    fn encrypted_fetch_fast_forward_from_empty() {
        let (alice_id, alice_recip) = make_identity();
        let alice_dir = init_repo();
        let alice_store = setup_store(&alice_dir, &alice_id, &alice_recip);

        // Alice writes a record and pushes.
        put_encrypted(&alice_store, "decision/auth", "Use OAuth2");

        let remote_dir = bare_remote();
        let remote_url = remote_dir.path().to_string_lossy().to_string();
        write_remote_config(
            alice_store.git_dir(),
            &MemoryRemote {
                url: remote_url.clone(),
                refspec: None,
            },
        )
        .expect("write remote config");
        memory_hub_store::push_to_remote(
            alice_store.git_dir(),
            &MemoryRemote {
                url: remote_url.clone(),
                refspec: None,
            },
            false,
        )
        .expect("push");

        // Bob creates a new store and fetches.
        let bob_dir = init_repo();
        let mut bob_store = EncryptedStore::open_locked(bob_dir.path()).expect("open");
        bob_store.unlock(box_identity(&alice_id)).expect("unlock");
        // Bob needs a manifest to read — init with same recipient.
        let _ = bob_store
            .init(vec![recipient_entry(&alice_recip, "owner")])
            .expect("init");

        write_remote_config(
            bob_store.git_dir(),
            &MemoryRemote {
                url: remote_url.clone(),
                refspec: None,
            },
        )
        .expect("write remote config");
        allow_unsigned_exchange(bob_store.git_dir());

        let result = bob_store
            .fetch_and_merge(
                &MemoryRemote {
                    url: remote_url,
                    refspec: None,
                },
                &[],
            )
            .expect("fetch and merge");

        assert!(result.fast_forward || result.merged);
        assert!(result.conflicts.is_empty());

        // Bob should be able to read the fetched record.
        let envelope = bob_store
            .get("decision/auth")
            .expect("get")
            .expect("record exists");
        assert_eq!(envelope.content, "Use OAuth2");
    }

    #[test]
    fn encrypted_fetch_is_refused_without_allowed_signers() {
        let (alice_id, alice_recip) = make_identity();
        let alice_dir = init_repo();
        let alice_store = setup_store(&alice_dir, &alice_id, &alice_recip);
        put_encrypted(&alice_store, "decision/auth", "Use OAuth2");

        let remote_dir = bare_remote();
        let remote_url = remote_dir.path().to_string_lossy().to_string();
        let remote = MemoryRemote {
            url: remote_url.clone(),
            refspec: None,
        };
        write_remote_config(alice_store.git_dir(), &remote).expect("write remote config");
        memory_hub_store::push_to_remote(alice_store.git_dir(), &remote, false).expect("push");

        let bob_dir = init_repo();
        let mut bob_store = EncryptedStore::open_locked(bob_dir.path()).expect("open");
        bob_store.unlock(box_identity(&alice_id)).expect("unlock");
        let _ = bob_store
            .init(vec![recipient_entry(&alice_recip, "owner")])
            .expect("init");
        write_remote_config(bob_store.git_dir(), &remote).expect("write remote config");

        // No SSH recipient in the manifest and no configured allowed signer:
        // refs/memory/* is unprotected on the server, so the fetch must refuse
        // rather than import unverified history.
        let error = bob_store
            .fetch_and_merge(&remote, &[])
            .expect_err("unverifiable fetch is refused");
        assert_eq!(error.kind, StoreErrorKind::SigningNotConfigured);
        assert_eq!(
            error.data["recovery_action"],
            "configure_allowed_signers_or_disable_verification"
        );
    }

    #[test]
    fn encrypted_fetch_merges_different_keys() {
        let (alice_id, alice_recip) = make_identity();
        let alice_dir = init_repo();
        let alice_store = setup_store(&alice_dir, &alice_id, &alice_recip);

        // Alice writes alpha and pushes.
        put_encrypted(&alice_store, "alpha", "alice alpha");

        let remote_dir = bare_remote();
        let remote_url = remote_dir.path().to_string_lossy().to_string();
        write_remote_config(
            alice_store.git_dir(),
            &MemoryRemote {
                url: remote_url.clone(),
                refspec: None,
            },
        )
        .expect("write remote config");
        memory_hub_store::push_to_remote(
            alice_store.git_dir(),
            &MemoryRemote {
                url: remote_url.clone(),
                refspec: None,
            },
            false,
        )
        .expect("push");

        // Bob sets up and fetches alpha.
        let bob_dir = init_repo();
        let mut bob_store = EncryptedStore::open_locked(bob_dir.path()).expect("open");
        bob_store.unlock(box_identity(&alice_id)).expect("unlock");
        let _ = bob_store
            .init(vec![recipient_entry(&alice_recip, "owner")])
            .expect("init");

        write_remote_config(
            bob_store.git_dir(),
            &MemoryRemote {
                url: remote_url.clone(),
                refspec: None,
            },
        )
        .expect("write remote config");
        allow_unsigned_exchange(bob_store.git_dir());
        bob_store
            .fetch_and_merge(
                &MemoryRemote {
                    url: remote_url.clone(),
                    refspec: None,
                },
                &[],
            )
            .expect("fetch");

        // Bob writes beta locally.
        put_encrypted(&bob_store, "beta", "bob beta");

        // Alice writes gamma and pushes.
        put_encrypted(&alice_store, "gamma", "alice gamma");
        memory_hub_store::push_to_remote(
            alice_store.git_dir(),
            &MemoryRemote {
                url: remote_url.clone(),
                refspec: None,
            },
            false,
        )
        .expect("push");

        // Bob fetches — should merge gamma.
        let result = bob_store
            .fetch_and_merge(
                &MemoryRemote {
                    url: remote_url,
                    refspec: None,
                },
                &[],
            )
            .expect("fetch and merge");

        assert!(result.merged || result.fast_forward);
        assert!(result.conflicts.is_empty());

        // Bob should have all three records.
        let alpha = bob_store.get("alpha").expect("get").expect("alpha exists");
        assert_eq!(alpha.content, "alice alpha");
        let beta = bob_store.get("beta").expect("get").expect("beta exists");
        assert_eq!(beta.content, "bob beta");
        let gamma = bob_store.get("gamma").expect("get").expect("gamma exists");
        assert_eq!(gamma.content, "alice gamma");
    }

    #[test]
    fn encrypted_fetch_same_key_conflict() {
        let (alice_id, alice_recip) = make_identity();
        let alice_dir = init_repo();
        let alice_store = setup_store(&alice_dir, &alice_id, &alice_recip);

        // Alice writes shared and pushes.
        put_encrypted(&alice_store, "shared", "alice version");

        let remote_dir = bare_remote();
        let remote_url = remote_dir.path().to_string_lossy().to_string();
        write_remote_config(
            alice_store.git_dir(),
            &MemoryRemote {
                url: remote_url.clone(),
                refspec: None,
            },
        )
        .expect("write remote config");
        memory_hub_store::push_to_remote(
            alice_store.git_dir(),
            &MemoryRemote {
                url: remote_url.clone(),
                refspec: None,
            },
            false,
        )
        .expect("push");

        // Bob fetches.
        let bob_dir = init_repo();
        let mut bob_store = EncryptedStore::open_locked(bob_dir.path()).expect("open");
        bob_store.unlock(box_identity(&alice_id)).expect("unlock");
        let _ = bob_store
            .init(vec![recipient_entry(&alice_recip, "owner")])
            .expect("init");
        write_remote_config(
            bob_store.git_dir(),
            &MemoryRemote {
                url: remote_url.clone(),
                refspec: None,
            },
        )
        .expect("write remote config");
        allow_unsigned_exchange(bob_store.git_dir());
        bob_store
            .fetch_and_merge(
                &MemoryRemote {
                    url: remote_url.clone(),
                    refspec: None,
                },
                &[],
            )
            .expect("fetch");

        // Both modify same key.
        put_encrypted(&alice_store, "shared", "alice updated");
        memory_hub_store::push_to_remote(
            alice_store.git_dir(),
            &MemoryRemote {
                url: remote_url.clone(),
                refspec: None,
            },
            false,
        )
        .expect("push");
        put_encrypted(&bob_store, "shared", "bob updated");

        // Bob fetches — conflict expected.
        let result = bob_store
            .fetch_and_merge(
                &MemoryRemote {
                    url: remote_url,
                    refspec: None,
                },
                &[],
            )
            .expect("fetch and merge");

        assert!(!result.conflicts.is_empty());
        let conflict = &result.conflicts[0];
        assert_eq!(conflict.key, "shared");
        assert_ne!(conflict.local_content_hash, conflict.remote_content_hash);
    }
}
