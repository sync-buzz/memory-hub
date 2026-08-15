#![allow(clippy::expect_used, clippy::unwrap_used)]

//! Crash/kill/restart safety for the ephemeral LanceDB index on encrypted
//! projects (GITMEMO-12).
//!
//! When an encrypted project is unlocked, the LanceDB index is rebuilt from
//! decrypted records and lives on disk under `.git/git-memory/index/`. If the
//! process is killed before `memory_lock` destroys it, plaintext LanceDB/WAL
//! files would remain. This test verifies that starting a new MCP session
//! wipes the stale index so no plaintext survives a crash/restart cycle.

use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::{json, Value};

use git_memory_core::Envelope;
use git_memory_crypto::{generate_backup_identity, Identity};
use git_memory_index::Projection;
use git_memory_store::{EncryptedStore, GitStore, RecipientEntry};

fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_git-memory"))
}

fn init_repository() -> tempfile::TempDir {
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

/// Set up an encrypted project, unlock it, and write one record whose
/// plaintext content is distinctive enough to detect if it leaks.
fn setup_encrypted_project(dir: &Path) -> age::x25519::Identity {
    let (identity, recipient) = make_identity();
    let mut store = EncryptedStore::open_locked(dir).expect("open encrypted");
    store.unlock(box_identity(&identity)).expect("unlock");
    let _ = store
        .init(vec![recipient_entry(&recipient, "owner")])
        .expect("init");

    let envelope = Envelope::new("secret-key", "note", "CRASH_LEAK_MARKER plaintext payload")
        .expect("envelope");
    let revision = store.current_revision().expect("revision");
    store
        .apply("seed", revision, &[("secret-key", envelope)], &[])
        .expect("apply");

    identity
}

/// Build the ephemeral index from decrypted records — this writes plaintext
/// LanceDB files to `.git/git-memory/index/`, exactly as `memory_unlock` does.
fn index_dir(dir: &Path) -> PathBuf {
    let git_store = GitStore::open(dir).expect("open git store");
    git_store.git_dir().join("git-memory/index")
}

/// Count files under the index directory (recursively).
fn count_files(dir: &Path) -> usize {
    if !dir.exists() {
        return 0;
    }
    let mut count = 0;
    for entry in walkdir(dir) {
        if entry.is_file() {
            count += 1;
        }
    }
    count
}

fn walkdir(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk_inner(dir, &mut out);
    out
}

fn walk_inner(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_inner(&path, out);
            out.push(path);
        } else {
            out.push(path);
        }
    }
}

/// Search the index directory tree for any file whose bytes contain the
/// plaintext marker. LanceDB stores content columns, so a built index over a
/// record with the marker should contain it somewhere in the lance files.
fn grep_plaintext_marker(dir: &Path, marker: &[u8]) -> bool {
    if !dir.exists() {
        return false;
    }
    for entry in walkdir(dir) {
        if entry.is_file() {
            if let Ok(bytes) = fs::read(&entry) {
                if memmem(&bytes, marker) {
                    return true;
                }
            }
        }
    }
    false
}

fn memmem(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

/// Send `initialize` to an MCP server over stdio and return immediately after
/// the handshake. `Session::new` runs before any request is processed, so the
/// crash-cleanup has already happened by the time we get the response.
fn mcp_initialize_and_close(project: &Path) {
    let mut child = Command::new(binary())
        .args(["mcp", "--project"])
        .arg(project)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("git memory mcp starts");
    let mut stdin = child.stdin.take().expect("stdin");
    let stdout = child.stdout.take().expect("stdout");
    let request = json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "crash-test", "version": "0.0.0"},
        }
    });
    serde_json::to_writer(&mut stdin, &request).expect("write");
    stdin.write_all(b"\n").expect("newline");
    stdin.flush().expect("flush");

    let reader = BufReader::new(stdout);
    let mut got_response = false;
    for line in reader.lines() {
        match line {
            Ok(l) if l.trim().is_empty() => continue,
            Ok(l) => {
                if let Ok(v) = serde_json::from_str::<Value>(&l) {
                    if v.get("id") == Some(&json!(1)) {
                        got_response = true;
                        break;
                    }
                }
            }
            Err(_) => break,
        }
    }
    // Drop stdin to signal EOF, then reap the child.
    drop(stdin);
    let _ = child.kill();
    let output = child.wait_with_output().expect("wait");
    assert!(
        got_response,
        "MCP server responded to initialize; stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn stale_ephemeral_index_is_wiped_on_restart() {
    let dir = init_repository();
    let identity = setup_encrypted_project(dir.path());

    // Decrypt the records and rebuild the ephemeral index, exactly as
    // `memory_unlock` does. This puts plaintext LanceDB files on disk.
    let mut store = EncryptedStore::open_locked(dir.path()).expect("open");
    store.unlock(box_identity(&identity)).expect("unlock");
    let records = store.list().expect("list decrypted records");
    let git_store = GitStore::open(dir.path()).expect("git store");
    let rev = store.current_revision().expect("revision");
    Projection::rebuild_from_envelopes_store(&git_store, &records, &rev)
        .expect("rebuild ephemeral index");

    let idx = index_dir(dir.path());
    let marker = b"CRASH_LEAK_MARKER";
    assert!(idx.exists(), "ephemeral index exists on disk after rebuild");
    assert!(
        count_files(&idx) > 0,
        "index directory contains lance files"
    );
    assert!(
        grep_plaintext_marker(&idx, marker),
        "plaintext marker is present in the built index — index holds plaintext"
    );

    // Simulate a crash: drop everything WITHOUT calling lock/destroy.
    drop(store);
    drop(git_store);
    // The index files are still on disk (crash left them).

    assert!(
        idx.exists(),
        "crash left the plaintext index on disk (no lock was called)"
    );

    // Start a new MCP session. Session::new detects the encrypted project and
    // wipes the stale index before any request is served.
    let detected = git_memory_store::is_encrypted_project(dir.path()).expect("detect");
    assert!(detected, "project is detected as encrypted by the store");
    mcp_initialize_and_close(dir.path());

    assert!(
        !idx.exists(),
        "index directory is gone after restart — no plaintext survives crash/restart"
    );
}

#[test]
fn destroy_store_silent_removes_all_plaintext_artifacts() {
    let dir = init_repository();
    let identity = setup_encrypted_project(dir.path());

    // Build the ephemeral index (plaintext on disk).
    let mut store = EncryptedStore::open_locked(dir.path()).expect("open");
    store.unlock(box_identity(&identity)).expect("unlock");
    let records = store.list().expect("list");
    let git_store = GitStore::open(dir.path()).expect("git store");
    let rev = store.current_revision().expect("revision");
    Projection::rebuild_from_envelopes_store(&git_store, &records, &rev).expect("rebuild");

    let idx = index_dir(dir.path());
    assert!(idx.exists(), "index built");
    assert!(count_files(&idx) > 0);

    drop(store);
    drop(git_store);

    // Direct crash-cleanup — what Session::new calls.
    let git_store = GitStore::open(dir.path()).expect("reopen git store");
    Projection::destroy_store_silent(&git_store).expect("silent destroy");

    assert!(!idx.exists(), "index directory fully removed");
}

#[test]
fn destroy_store_silent_is_a_noop_when_no_index_exists() {
    let dir = init_repository();
    let _identity = setup_encrypted_project(dir.path());

    // No index built yet.
    let idx = index_dir(dir.path());
    assert!(!idx.exists(), "no index yet");

    let git_store = GitStore::open(dir.path()).expect("git store");
    Projection::destroy_store_silent(&git_store).expect("no-op on absent index");
    assert!(!idx.exists());
}

#[test]
fn destroy_store_silent_handles_a_corrupt_index_directory() {
    let dir = init_repository();
    let identity = setup_encrypted_project(dir.path());

    // Build the ephemeral index, then corrupt it by truncating a lance file —
    // simulating a crash mid-write. destroy_store (which opens LanceDB) might
    // fail on this; destroy_store_silent must remove it regardless.
    let mut store = EncryptedStore::open_locked(dir.path()).expect("open");
    store.unlock(box_identity(&identity)).expect("unlock");
    let records = store.list().expect("list");
    let git_store = GitStore::open(dir.path()).expect("git store");
    let rev = store.current_revision().expect("revision");
    Projection::rebuild_from_envelopes_store(&git_store, &records, &rev).expect("rebuild");
    drop(store);
    drop(git_store);

    let idx = index_dir(dir.path());
    // Corrupt: overwrite a lance file with garbage.
    for entry in walkdir(&idx) {
        if entry.is_file() {
            let _ = fs::write(&entry, b"corrupted-garbage-not-a-lance-file");
        }
    }

    let git_store = GitStore::open(dir.path()).expect("git store");
    Projection::destroy_store_silent(&git_store).expect("removes corrupt index");
    assert!(!idx.exists(), "corrupt index removed");
}
