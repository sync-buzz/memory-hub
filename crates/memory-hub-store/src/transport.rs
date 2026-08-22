//! Memory remote transport: fetch, push, and record-level merge.
//!
//! All network operations shell out to the `git` CLI because `git2` is
//! compiled without SSH/HTTPS support. Operations are scoped to
//! `refs/memory/*` via explicit refspecs — code branches are never touched.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use memory_hub_core::{
    ContentRef, Envelope, FreshnessState, PolicyMode, PolicyResolver, Presence, StoredRecord,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::error::GitStoreError;
use crate::types::GitRevision;
use crate::{
    GitStore, MAIN_REF, Operation, RecordId, Revision, StoreError, StoreErrorKind, Transaction,
};

const FETCH_TEMP_REF: &str = "refs/memory/tmp-fetch";

/// Configuration for a memory remote.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct MemoryRemote {
    /// Remote URL (SSH, HTTPS, or local path).
    pub url: String,
    /// Optional custom push refspec (e.g. `+refs/memory/*:refs/memory/*`).
    /// When set, `push_to_remote` passes it verbatim instead of auto-discovering
    /// existing memory refs. Fetch always uses an internal temp-ref refspec
    /// and is not affected by this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refspec: Option<String>,
}

/// Result of a fetch operation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct FetchResult {
    pub local_revision_before: Revision,
    pub local_revision_after: Revision,
    pub remote_revision: Revision,
    pub fast_forward: bool,
    pub merged: bool,
    /// What could not be reconciled at all.
    ///
    /// **Always empty as things stand.** Every same-key difference is now
    /// merged against the common ancestor, and the one that collides is
    /// resolved rather than handed back. The field stays because it is part of
    /// the shape published to clients, and because a storage kind whose records
    /// are not text would have nothing to merge line by line.
    pub conflicts: Vec<ConflictEntry>,
    /// The records where both sides had moved the same thing, and this side's
    /// version was kept.
    ///
    /// Reported rather than absorbed. Nothing is lost — the other version is
    /// still a commit in the history — but somebody has to be told that their
    /// colleague's sentence is not the one in front of them.
    #[serde(default)]
    pub overlaps: Vec<Overlap>,
}

/// One record both sides moved, and what of theirs is not in the answer.
///
/// Named rather than counted, because "a record was merged over" is not
/// something anybody can act on. Knowing it was the title tells a person where
/// to look; knowing it was the body tells them to read it.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Overlap {
    pub key: String,
    /// The same lines of the body were rewritten on both sides.
    #[serde(default)]
    pub body: bool,
    /// Members of the envelope both sides moved: `title`, `folder`, a product
    /// field's own name. Empty when only the body collided.
    #[serde(default)]
    pub fields: Vec<String>,
}

impl Overlap {
    fn of(key: &str) -> Self {
        Self {
            key: key.to_owned(),
            ..Self::default()
        }
    }

    /// Whether this record cost nobody anything, and so is not worth reporting.
    fn is_quiet(&self) -> bool {
        !self.body && self.fields.is_empty()
    }
}

/// A same-key conflict discovered during merge.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConflictEntry {
    pub key: String,
    pub local_content_hash: String,
    pub remote_content_hash: String,
}

/// Reject a remote URL that `git` would read as an option or as a request to
/// run an arbitrary command.
///
/// `git` parses options before positional arguments, so a URL like
/// `--upload-pack=cmd` is an option rather than a location, and the remote
/// helper syntax (`ext::sh -c …`) executes a command by design. The value
/// comes from the repository's Git config, which any process writing to the
/// repository controls, so it is validated before it reaches the argument
/// vector. Callers additionally pass `--` so nothing after it is parsed as an
/// option.
///
/// # Errors
///
/// Returns [`StoreError`] with kind `InvalidArgument` for an empty URL, an
/// option-looking URL, or a remote-helper URL.
pub fn validate_remote_url(url: &str) -> Result<(), StoreError> {
    let trimmed = url.trim();
    if trimmed.is_empty() {
        return Err(StoreError::new(
            StoreErrorKind::InvalidArgument,
            "memory remote URL must not be empty",
            serde_json::json!({"field": "url"}),
        ));
    }
    if trimmed.starts_with('-') {
        return Err(StoreError::new(
            StoreErrorKind::InvalidArgument,
            "memory remote URL must not start with `-` — git would read it as an option",
            serde_json::json!({"field": "url", "url": trimmed}),
        ));
    }
    // Remote helpers are `<helper>::<address>`; `ext::` runs a shell command.
    // A scheme (`ssh://`) or an scp-style host (`git@host:path`) has a single
    // colon before the first `/`, so this only matches helper syntax.
    let head = trimmed.split('/').next().unwrap_or(trimmed);
    if head.contains("::") {
        return Err(StoreError::new(
            StoreErrorKind::InvalidArgument,
            "memory remote URL must not use a git remote helper — `<helper>::` can execute arbitrary commands",
            serde_json::json!({"field": "url", "url": trimmed}),
        ));
    }
    Ok(())
}

/// Reject a push refspec that escapes the Memory namespace or looks like an
/// option.
///
/// Memory Hub promises that code refs are never touched, so a configured
/// refspec may only move `refs/memory/*`. Whitespace splits the value into
/// individual refspecs, matching how [`push_to_remote`] forwards it.
///
/// # Errors
///
/// Returns [`StoreError`] with kind `InvalidArgument` when a refspec is empty,
/// starts with `-`, or references a ref outside `refs/memory/`.
pub fn validate_refspec(refspec: &str) -> Result<(), StoreError> {
    let specs: Vec<&str> = refspec.split_whitespace().collect();
    if specs.is_empty() {
        return Err(StoreError::new(
            StoreErrorKind::InvalidArgument,
            "memory refspec must not be empty",
            serde_json::json!({"field": "refspec"}),
        ));
    }
    for spec in specs {
        if spec.starts_with('-') {
            return Err(StoreError::new(
                StoreErrorKind::InvalidArgument,
                "memory refspec must not start with `-` — git would read it as an option",
                serde_json::json!({"field": "refspec", "refspec": spec}),
            ));
        }
        let body = spec.strip_prefix('+').unwrap_or(spec);
        let sides: Vec<&str> = body.split(':').collect();
        if sides.len() > 2 {
            return Err(StoreError::new(
                StoreErrorKind::InvalidArgument,
                "memory refspec must be `[+]<src>:<dst>`",
                serde_json::json!({"field": "refspec", "refspec": spec}),
            ));
        }
        for side in sides {
            if side.is_empty() {
                continue; // deletion refspec (`:refs/memory/x`)
            }
            if !side.starts_with("refs/memory/") {
                return Err(StoreError::new(
                    StoreErrorKind::NamespaceRejected,
                    "memory refspec may only reference refs/memory/*",
                    serde_json::json!({"field": "refspec", "refspec": spec, "ref": side}),
                ));
            }
        }
    }
    Ok(())
}

/// Read the memory remote from the repository's Git config.
///
/// Returns `Ok(None)` when no remote is configured (the default).
///
/// # Errors
///
/// Returns [`StoreError`] if the config cannot be read.
pub fn read_remote_config(git_dir: &Path) -> Result<Option<MemoryRemote>, StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let config = repo
        .config()
        .map_err(|e| StoreError::repository("read config", e))?;

    let url = match config.get_string("memory-hub.remote.url") {
        Ok(url) => url,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(StoreError::repository("read remote url", e)),
    };
    let refspec = config.get_string("memory-hub.remote.refspec").ok();
    Ok(Some(MemoryRemote { url, refspec }))
}

/// Write the memory remote to the repository's Git config.
///
/// # Errors
///
/// Returns [`StoreError`] if the config cannot be written.
pub fn write_remote_config(git_dir: &Path, remote: &MemoryRemote) -> Result<(), StoreError> {
    validate_remote_url(&remote.url)?;
    if let Some(refspec) = &remote.refspec {
        validate_refspec(refspec)?;
    }
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let mut config = repo
        .config()
        .map_err(|e| StoreError::repository("read config", e))?;
    config
        .set_str("memory-hub.remote.url", &remote.url)
        .map_err(|e| StoreError::repository("set remote url", e))?;
    if let Some(refspec) = &remote.refspec {
        config
            .set_str("memory-hub.remote.refspec", refspec)
            .map_err(|e| StoreError::repository("set remote refspec", e))?;
    } else {
        let _ = config.remove("memory-hub.remote.refspec");
    }
    Ok(())
}

/// Remove the memory remote from the repository's Git config.
///
/// # Errors
///
/// Returns [`StoreError`] if the config cannot be written.
pub fn remove_remote_config(git_dir: &Path) -> Result<(), StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let mut config = repo
        .config()
        .map_err(|e| StoreError::repository("read config", e))?;
    let _ = config.remove("memory-hub.remote.url");
    let _ = config.remove("memory-hub.remote.refspec");
    Ok(())
}

/// What a remote carries, as far as memory is concerned.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RemoteMemory {
    /// The remote answered and carries at least one `refs/memory/*` ref.
    Present,
    /// The remote answered and carries none.
    Absent,
}

/// Ask a remote whether it carries memory, without transferring any of it.
///
/// `git clone` does not copy `refs/memory/*`, so an empty memory in a fresh
/// clone looks exactly like a project that never had one. Only the remote can
/// tell the two apart, and this is the cheapest way to ask: a ref listing
/// moves no objects, writes no ref, and verifies no signature because no
/// history is imported.
///
/// Runs with a closed stdin so a remote that wants a password or a host-key
/// confirmation fails instead of hanging — this is called from diagnostics,
/// where a hang is worse than a "could not reach it".
///
/// # Errors
///
/// Returns [`StoreError`] with kind `InvalidArgument` for a URL Git would read
/// as an option, `AuthenticationFailed` when the remote refuses the
/// connection, or `TransportFailed` when it cannot be reached at all.
pub fn probe_remote_memory(git_dir: &Path, url: &str) -> Result<RemoteMemory, StoreError> {
    validate_remote_url(url)?;
    let child = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args(["ls-remote", "--refs"])
        .arg("--")
        .arg(url)
        .arg("refs/memory/*")
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            StoreError::new(
                StoreErrorKind::TransportFailed,
                "failed to spawn git ls-remote",
                serde_json::json!({"detail": e.to_string()}),
            )
        })?;
    let output = wait_with_deadline(child, PROBE_TIMEOUT)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(classify_read_error(
            "git ls-remote",
            output.status.code().unwrap_or(-1),
            &stderr,
        ));
    }

    if String::from_utf8_lossy(&output.stdout).trim().is_empty() {
        Ok(RemoteMemory::Absent)
    } else {
        Ok(RemoteMemory::Present)
    }
}

/// Where the last-known remote revision is kept.
///
/// Beside the remote it is about, in the repository's own Git config, because
/// it is a fact about this clone's exchanges with that remote and not about the
/// memory itself. **Not a ref**: `refs/memory/main` is the only ref this engine
/// keeps, and a remote-tracking ref would be a second one.
const KNOWN_KEY: &str = "memory-hub.remote.known";

/// What the remote holds, as far as this repository knows.
///
/// Written after a successful exchange in either direction — a push knows
/// because it just put it there, a fetch knows because it just read it. One
/// stored value answers both questions the window asks: what is here and not
/// there, and whether anything is there and not here.
///
/// # Errors
///
/// Returns [`StoreError`] if the configuration cannot be read.
pub fn read_known_remote_revision(git_dir: &Path) -> Result<Option<Revision>, StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let config = repo
        .config()
        .map_err(|e| StoreError::repository("read config", e))?;
    match config.get_string(KNOWN_KEY) {
        Ok(value) => Ok(Some(Revision::new(value))),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(StoreError::repository("read known remote revision", e)),
    }
}

/// Record what the remote holds after an exchange that proved it.
///
/// # Errors
///
/// Returns [`StoreError`] if the configuration cannot be written.
pub fn record_known_remote_revision(git_dir: &Path, revision: &Revision) -> Result<(), StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let mut config = repo
        .config()
        .map_err(|e| StoreError::repository("read config", e))?;
    config
        .set_str(KNOWN_KEY, revision.as_str())
        .map_err(|e| StoreError::repository("record known remote revision", e))
}

/// What the remote had to say, when it was asked at all.
///
/// Four states rather than a `bool`, because "not asked" and "could not be
/// asked" are not "nothing is waiting", and a window that collapsed them would
/// tell somebody their memory is published when nobody could reach the remote.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RemoteCheck {
    /// The network was deliberately not touched.
    NotAsked,
    /// The remote holds something this repository does not.
    Waiting,
    /// The remote holds nothing new.
    UpToDate,
    /// The question could not be put.
    Unreachable,
}

/// Whether synchronisation is needed, and in which direction.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncState {
    pub remote_configured: bool,
    /// **Records**, not commits, that are here and not on the remote.
    ///
    /// Every save is a commit, so a count of commits would say `12` for twelve
    /// edits of one record — a number that is true of the history and not of
    /// anything a person would recognise as theirs.
    pub unpublished: usize,
    pub remote: RemoteCheck,
}

/// Answer whether this repository's memory is in step with its remote.
///
/// `ask_remote` decides whether the network is touched. The unpublished count
/// never needs it — it is computed against the last revision known to be on the
/// remote, whose objects are here because they got here by being exchanged.
///
/// # Errors
///
/// Returns [`StoreError`] when the repository cannot be read. A remote that
/// cannot be reached is [`RemoteCheck::Unreachable`], not an error: the count
/// beside it is still true and still worth showing.
pub fn sync_state(store: &GitStore, ask_remote: bool) -> Result<SyncState, StoreError> {
    let git_dir = store.git_dir();
    let Some(remote) = read_remote_config(git_dir)? else {
        return Ok(SyncState {
            remote_configured: false,
            unpublished: 0,
            remote: RemoteCheck::NotAsked,
        });
    };

    let local = store.current()?.revision().clone();
    let known = read_known_remote_revision(git_dir)?;

    let unpublished = match &known {
        // Nothing has been exchanged yet, so everything here is unpublished.
        // Counted as records for the same reason the diff below is.
        None => store.current()?.records()?.len(),
        Some(known) if *known == local => 0,
        Some(known) => store.diff(known, &local)?.len(),
    };

    let remote = if ask_remote {
        match remote_memory_tip(git_dir, &remote.url) {
            Ok(Some(tip)) if known.as_ref() == Some(&tip) => RemoteCheck::UpToDate,
            // A tip we already have and have already merged past is not news.
            Ok(Some(tip)) => {
                if contains_revision(store, &tip, &local)? {
                    RemoteCheck::UpToDate
                } else {
                    RemoteCheck::Waiting
                }
            }
            // A remote carrying no memory has nothing to send.
            Ok(None) => RemoteCheck::UpToDate,
            Err(_) => RemoteCheck::Unreachable,
        }
    } else {
        RemoteCheck::NotAsked
    };

    Ok(SyncState {
        remote_configured: true,
        unpublished,
        remote,
    })
}

/// Whether `local` already contains `tip` — the tip is an ancestor of what is
/// here, so there is nothing to fetch even though the revisions differ.
///
/// A tip whose object is not in this repository at all has definitionally not
/// been merged, and `find_commit` failing is that answer rather than a fault.
fn contains_revision(
    store: &GitStore,
    tip: &Revision,
    local: &Revision,
) -> Result<bool, StoreError> {
    let repo = Repository::open(store.git_dir()).map_err(|e| StoreError::repository("open", e))?;
    let (Ok(tip_oid), Ok(local_oid)) = (tip.oid(), local.oid()) else {
        return Ok(false);
    };
    if repo.find_commit(tip_oid).is_err() {
        return Ok(false);
    }
    repo.graph_descendant_of(local_oid, tip_oid)
        .map_err(|e| StoreError::repository("check descendant", e))
}

/// The revision `refs/memory/main` points at on the remote, without fetching
/// anything. `None` when the remote carries no memory at all.
///
/// # Errors
///
/// Returns [`StoreError`] when the remote cannot be reached or refuses.
pub fn remote_memory_tip(git_dir: &Path, url: &str) -> Result<Option<Revision>, StoreError> {
    validate_remote_url(url)?;
    let child = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args(["ls-remote", "--refs"])
        .arg("--")
        .arg(url)
        .arg(MAIN_REF)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            StoreError::new(
                StoreErrorKind::TransportFailed,
                "failed to spawn git ls-remote",
                serde_json::json!({"detail": e.to_string()}),
            )
        })?;
    let output = wait_with_deadline(child, PROBE_TIMEOUT)?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(classify_read_error(
            "git ls-remote",
            output.status.code().unwrap_or(-1),
            &stderr,
        ));
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    Ok(stdout
        .split_whitespace()
        .next()
        .filter(|oid| !oid.is_empty())
        .map(Revision::new))
}

/// How long a diagnostic waits for a remote to answer.
///
/// Closing stdin stops `git` asking for a password, but nothing stops a host
/// that accepts the connection and then says nothing. This is called from
/// `doctor`, where a wrong answer is recoverable and a hang is not.
const PROBE_TIMEOUT: Duration = Duration::from_secs(10);

/// Wait for a child, killing it if it outlasts `timeout`.
fn wait_with_deadline(
    mut child: std::process::Child,
    timeout: Duration,
) -> Result<std::process::Output, StoreError> {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => {
                return child.wait_with_output().map_err(|e| {
                    StoreError::new(
                        StoreErrorKind::TransportFailed,
                        "failed to read git output",
                        serde_json::json!({"detail": e.to_string()}),
                    )
                });
            }
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(StoreError::new(
                    StoreErrorKind::TransportFailed,
                    "the remote did not answer in time",
                    serde_json::json!({"timeout_seconds": timeout.as_secs()}),
                ));
            }
            Err(e) => {
                return Err(StoreError::new(
                    StoreErrorKind::TransportFailed,
                    "failed to wait for git",
                    serde_json::json!({"detail": e.to_string()}),
                ));
            }
        }
    }
}

/// Read the URL of the code remote `origin`.
///
/// Memory has its own remote precisely so it does not have to follow the code
/// one, but before anything is configured `origin` is the only address the
/// repository knows. Callers use it to ask a question, never to publish.
///
/// # Errors
///
/// Returns [`StoreError`] if the config cannot be read.
pub fn read_code_origin_url(git_dir: &Path) -> Result<Option<String>, StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let config = repo
        .config()
        .map_err(|e| StoreError::repository("read config", e))?;
    match config.get_string("remote.origin.url") {
        Ok(url) => Ok(Some(url)),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(StoreError::repository("read origin url", e)),
    }
}

/// What memory this repository has, and where it is when it is not here.
///
/// `git clone` copies no `refs/memory/*`, so from inside a fresh clone "this
/// project never had memory" and "its memory is still on the remote" look
/// identical — and only one of them is something a person can fix. This is the
/// one place where memory being invisible in the working tree stops being a
/// property and becomes a defect.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MemoryPresence {
    /// This repository holds memory of its own.
    Present { records: usize },
    /// Nothing here, and `url` carries memory.
    ///
    /// `configured` tells the memory remote apart from the code `origin` that
    /// was asked in its absence, because only the second case has to have a
    /// remote configured before anything can be fetched.
    NotFetched { url: String, configured: bool },
    /// Nothing here and nothing there. An empty project is a normal state
    /// rather than a failure, and `url` is `None` when there was no address to
    /// ask at all.
    Absent { url: Option<String> },
    /// Nothing here, and the remote could not be asked — which is not the same
    /// answer as "there is none".
    Unreachable { url: String, reason: String },
}

/// Ask whether this repository's memory is here, elsewhere, or nowhere.
///
/// The question is asked of the remote without a configured memory remote too:
/// before anyone configures one, the code `origin` is the only address the
/// repository knows, and it is the address the memory almost certainly sits
/// next to.
///
/// # Errors
///
/// Returns [`StoreError`] when the repository or its configuration cannot be
/// read. A remote that cannot be reached is an answer
/// ([`MemoryPresence::Unreachable`]) rather than an error: the caller has to
/// tell "there is no memory" from "nobody could say", and an `Err` collapses
/// the two.
pub fn memory_presence(project: &Path) -> Result<MemoryPresence, StoreError> {
    let git_dir = GitStore::discover_git_dir(project)?;
    let records = local_record_count(&git_dir)?;
    if records > 0 {
        return Ok(MemoryPresence::Present { records });
    }

    let (url, configured) = match read_remote_config(&git_dir)? {
        Some(remote) => (Some(remote.url), true),
        None => (read_code_origin_url(&git_dir)?, false),
    };
    let Some(url) = url else {
        return Ok(MemoryPresence::Absent { url: None });
    };

    match probe_remote_memory(&git_dir, &url) {
        Ok(RemoteMemory::Present) => Ok(MemoryPresence::NotFetched { url, configured }),
        Ok(RemoteMemory::Absent) => Ok(MemoryPresence::Absent { url: Some(url) }),
        Err(error) => Ok(MemoryPresence::Unreachable {
            reason: error.to_string(),
            url,
        }),
    }
}

/// How much memory this repository actually holds.
///
/// Records, not refs: `refs/memory/main` is created, with a genesis commit and
/// nothing in it, by the first store that opens the repository — including a
/// previous call to this function — so the presence of a ref says nothing about
/// whether any memory was ever written. Listing the refs first keeps the common
/// empty case from opening a store at all, which is what stops the question
/// from answering itself.
fn local_record_count(git_dir: &Path) -> Result<usize, StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let mut refs = repo
        .references_glob("refs/memory/*")
        .map_err(|e| StoreError::repository("list memory refs", e))?;
    if refs.next().is_none() {
        return Ok(0);
    }
    drop(refs);
    let store = GitStore::open(git_dir)?;
    Ok(store.current()?.records()?.len())
}

/// Fetch from the configured memory remote into a temporary ref and return
/// the remote revision. Does NOT merge or update `refs/memory/main`.
///
/// The caller is responsible for calling [`cleanup_temp_ref_pub`] after
/// processing the fetched data.
///
/// # Errors
///
/// Returns [`StoreError`] with kind `TransportFailed`, `AuthenticationFailed`,
/// or `NamespaceRejected`.
pub fn fetch_remote_revision(
    store: &GitStore,
    remote: &MemoryRemote,
) -> Result<(Revision, Revision), StoreError> {
    let git_dir = store.git_dir();
    let local_before = store.current()?.revision().clone();

    fetch_to_temp_ref(git_dir, remote)?;

    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let remote_oid = match repo.find_reference(FETCH_TEMP_REF) {
        Ok(reference) => reference.target().ok_or_else(|| {
            StoreError::new(
                StoreErrorKind::TransportFailed,
                "fetched ref is symbolic",
                serde_json::json!({"ref": FETCH_TEMP_REF}),
            )
        })?,
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            return Err(StoreError::new(
                StoreErrorKind::TransportFailed,
                "remote has no refs/memory/main — the remote store is not initialized",
                serde_json::json!({"remote": remote.url}),
            ));
        }
        Err(e) => return Err(StoreError::repository("read fetch temp ref", e)),
    };

    Ok((local_before, Revision::from_oid(remote_oid)))
}

/// Delete the temporary fetch ref if it exists.
///
/// # Errors
///
/// Returns [`StoreError`] if the repository cannot be opened.
pub fn cleanup_temp_ref_pub(git_dir: &Path) -> Result<(), StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    cleanup_temp_ref(&repo)
}

/// Fast-forward `refs/memory/main` to the given revision (which must be
/// the fetched temp ref revision or a descendant of the current tip).
///
/// # Errors
///
/// Returns [`StoreError`] if the ref update fails.
pub fn fast_forward_to(git_dir: &Path, remote_revision: &Revision) -> Result<(), StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let remote_oid = remote_revision.oid()?;
    repo.reference(MAIN_REF, remote_oid, true, "memory-hub: fetch fast-forward")
        .map_err(|e| StoreError::repository("fast-forward memory ref", e))?;
    Ok(())
}

/// Put memory back where it stood, undoing what has happened since.
///
/// **Backwards along its own history, and nowhere else.** The target has to be
/// an ancestor of the current tip, which is what makes this an undo rather than
/// a way to set the ref at anything: a revision off to one side is not a state
/// this memory was ever in, and arriving at one would be a corpus nobody wrote.
///
/// Nothing is destroyed. The commits after the target stay in the repository
/// and stay reachable by their own revisions — this moves a ref, which is all
/// that "where memory stands" ever was. What it undoes is therefore recoverable
/// by the same operation in the other direction.
///
/// # Errors
///
/// Returns [`StoreError`] when the repository cannot be read, when the target
/// is not an ancestor of the current tip, or when the ref update fails.
pub fn rewind_to(
    git_dir: &Path,
    revision: &Revision,
    expected: &Revision,
) -> Result<(), StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let target = revision.oid()?;
    let tip = reference_target(&repo, MAIN_REF)?.ok_or_else(|| {
        StoreError::new(
            StoreErrorKind::InvalidArgument,
            "this project's memory has no history to go back through",
            serde_json::json!({}),
        )
    })?;

    // **Only while memory still stands where the caller thinks it does.** An
    // undo names the state to return to, and what makes that safe is that
    // nothing has happened since — a record written after the fetch is not part
    // of what the fetch did, and going back would take it along without anybody
    // asking. The same compare-and-swap every write here uses, for the same
    // reason.
    if tip != expected.oid()? {
        return Err(StoreError::new(
            StoreErrorKind::Conflict,
            "memory has moved since: something was written after what you are undoing",
            serde_json::json!({
                "expected": expected.to_string(),
                "current": tip.to_string(),
            }),
        ));
    }

    if target != tip
        && !repo
            .graph_descendant_of(tip, target)
            .map_err(|e| StoreError::repository("check ancestor", e))?
    {
        return Err(StoreError::new(
            StoreErrorKind::InvalidArgument,
            "that revision is not one this memory passed through",
            serde_json::json!({
                "revision": revision.to_string(),
                "current": tip.to_string(),
            }),
        ));
    }

    repo.reference(MAIN_REF, target, true, "memory-hub: rewind")
        .map_err(|e| StoreError::repository("rewind memory ref", e))?;
    Ok(())
}

/// The commit a ref points at, or `None` when the ref is not there.
fn reference_target(repo: &Repository, name: &str) -> Result<Option<git2::Oid>, StoreError> {
    match repo.find_reference(name) {
        Ok(reference) => Ok(reference.target()),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(StoreError::repository("find reference", e)),
    }
}

/// Check whether a fast-forward is possible from local to remote.
///
/// Returns `true` if `remote` is a descendant of `local`, or if `local` has
/// no records (empty genesis).
///
/// # Errors
///
/// Returns [`StoreError`] if the repository cannot be read.
pub fn can_fast_forward(
    store: &GitStore,
    local: &Revision,
    remote: &Revision,
) -> Result<bool, StoreError> {
    let git_dir = store.git_dir();
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let local_oid = local.oid()?;
    let remote_oid = remote.oid()?;
    if local_oid == remote_oid {
        return Ok(true);
    }
    let is_ff = repo
        .graph_descendant_of(remote_oid, local_oid)
        .map_err(|e| StoreError::repository("check descendant", e))?;
    if is_ff {
        return Ok(true);
    }
    // Check if local is empty (genesis only).
    let local_records = store.read_records(local)?;
    Ok(local_records.is_empty())
}

/// A guard that cleans up the temporary fetch ref when dropped, even on
/// error paths. Created after `fetch_to_temp_ref` succeeds.
struct TempRefGuard<'a> {
    repo: &'a Repository,
    cleaned: bool,
}

impl Drop for TempRefGuard<'_> {
    fn drop(&mut self) {
        if !self.cleaned {
            let _ = cleanup_temp_ref(self.repo);
        }
    }
}

impl TempRefGuard<'_> {
    /// Mark the ref as already cleaned (success path) so Drop doesn't
    /// double-delete.
    fn disarm(&mut self) {
        self.cleaned = true;
    }
}

/// Fetch from the configured memory remote and merge.
///
/// Downloads `refs/memory/main` from the remote into a temporary ref, then
/// either fast-forwards or performs a record-level merge.
///
/// # Errors
///
/// Returns [`StoreError`] with kind `TransportFailed`, `AuthenticationFailed`,
/// `NamespaceRejected`, `FastForwardRequired`, or `Diverged`.
pub fn fetch_and_merge(store: &GitStore, remote: &MemoryRemote) -> Result<FetchResult, StoreError> {
    let result = merge_from_remote(store, remote)?;
    // Recorded here rather than at each of the five ways the merge can end,
    // every one of which has learned the same thing: this is what the remote
    // holds. Best effort — the memory has already arrived, and a status field
    // that could not be written is not worth failing the fetch over.
    let _ = record_known_remote_revision(store.git_dir(), &result.remote_revision);
    Ok(result)
}

fn merge_from_remote(store: &GitStore, remote: &MemoryRemote) -> Result<FetchResult, StoreError> {
    let git_dir = store.git_dir();
    let local_before = store.current()?.revision().clone();

    // Step 1: Fetch remote refs/memory/main to a temp ref.
    fetch_to_temp_ref(git_dir, remote)?;

    // Step 2: Read the fetched revision.
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let mut guard = TempRefGuard {
        repo: &repo,
        cleaned: false,
    };

    let remote_oid = match repo.find_reference(FETCH_TEMP_REF) {
        Ok(reference) => reference.target().ok_or_else(|| {
            StoreError::new(
                StoreErrorKind::TransportFailed,
                "fetched ref is symbolic",
                serde_json::json!({"ref": FETCH_TEMP_REF}),
            )
        })?,
        Err(e) if e.code() == git2::ErrorCode::NotFound => {
            return Err(StoreError::new(
                StoreErrorKind::TransportFailed,
                "remote has no refs/memory/main — the remote store is not initialized",
                serde_json::json!({"remote": remote.url}),
            ));
        }
        Err(e) => return Err(StoreError::repository("read fetch temp ref", e)),
    };

    // Step 3: Determine fast-forward vs divergence.
    let local_oid = local_before.oid()?;
    if remote_oid == local_oid {
        // Already up to date.
        guard.disarm();
        cleanup_temp_ref(&repo)?;
        return Ok(FetchResult {
            local_revision_before: local_before.clone(),
            local_revision_after: local_before,
            remote_revision: Revision::from_oid(remote_oid),
            fast_forward: true,
            merged: false,
            conflicts: Vec::new(),
            overlaps: Vec::new(),
        });
    }

    let is_ff = repo
        .graph_descendant_of(remote_oid, local_oid)
        .map_err(|e| StoreError::repository("check descendant", e))?;

    if is_ff {
        // Fast-forward: remote is ahead of local.
        repo.reference(MAIN_REF, remote_oid, true, "memory-hub: fetch fast-forward")
            .map_err(|e| StoreError::repository("fast-forward memory ref", e))?;
        guard.disarm();
        cleanup_temp_ref(&repo)?;
        Ok(FetchResult {
            local_revision_before: local_before,
            local_revision_after: Revision::from_oid(remote_oid),
            remote_revision: Revision::from_oid(remote_oid),
            fast_forward: true,
            merged: false,
            conflicts: Vec::new(),
            overlaps: Vec::new(),
        })
    } else {
        // Not a fast-forward. Check if local is empty (genesis only, no records).
        // If so, treat it as a fast-forward — there's nothing to merge.
        let local_records = store.read_records(&local_before)?;
        if local_records.is_empty() {
            repo.reference(
                MAIN_REF,
                remote_oid,
                true,
                "memory-hub: fetch fast-forward from empty",
            )
            .map_err(|e| StoreError::repository("fast-forward memory ref", e))?;
            guard.disarm();
            cleanup_temp_ref(&repo)?;
            return Ok(FetchResult {
                local_revision_before: local_before,
                local_revision_after: Revision::from_oid(remote_oid),
                remote_revision: Revision::from_oid(remote_oid),
                fast_forward: true,
                merged: false,
                conflicts: Vec::new(),
                overlaps: Vec::new(),
            });
        }
        // Diverged with actual local data: perform record-level merge.
        let result = merge_records(store, &local_before, &Revision::from_oid(remote_oid))?;
        guard.disarm();
        cleanup_temp_ref(&repo)?;
        Ok(result)
    }
}

/// Result of checking the push policy before a network mutation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct PushPolicyResult {
    /// Whether the push may proceed.
    pub allowed: bool,
    /// Human-readable warnings (empty when `allowed` is `true` and no stale records).
    pub warnings: Vec<String>,
    /// Number of stale records detected.
    pub stale_count: usize,
}

/// Check the `memory_push_stale` policy before pushing.
///
/// Reads all plaintext records from the current snapshot and checks their
/// freshness state. If any are `Stale` or `Invalid`, the policy is applied:
/// - `Off` — proceed silently
/// - `Warn` — proceed with warnings
/// - `Block` — refuse to push
///
/// Encrypted records cannot be checked without unlocking the store; their
/// freshness is inside the encrypted manifest. If the store has encrypted
/// records, the check is skipped with a warning.
///
/// # Errors
///
/// Returns [`StoreError`] if the store cannot be read.
pub fn check_push_policy(store: &GitStore) -> Result<PushPolicyResult, StoreError> {
    let resolver = PolicyResolver::memory_hub_defaults();
    let policy = resolver.resolve("memory_push_stale", None).map_err(|e| {
        StoreError::new(
            StoreErrorKind::InvalidArgument,
            "failed to resolve push policy",
            serde_json::json!({"detail": e.to_string()}),
        )
    })?;

    let revision = store.current()?.revision().clone();
    let records = store.read_records_pub(&revision)?;

    let stale_keys: Vec<String> = records
        .iter()
        .filter_map(|(id, record)| match record {
            StoredRecord::Plaintext { envelope } => {
                if matches!(
                    envelope.freshness.state,
                    FreshnessState::Stale | FreshnessState::Invalid
                ) {
                    Some(id.display_value())
                } else {
                    None
                }
            }
        })
        .collect();

    let stale_count = stale_keys.len();
    let warnings = if stale_count > 0 && policy.mode != PolicyMode::Off {
        vec![format!(
            "{} record(s) are stale: {}",
            stale_count,
            stale_keys.join(", ")
        )]
    } else {
        Vec::new()
    };

    let allowed = !matches!(policy.mode, PolicyMode::Block) || stale_count == 0;

    Ok(PushPolicyResult {
        allowed,
        warnings,
        stale_count,
    })
}

/// Push memory refs to the configured remote.
///
/// Only `refs/memory/main` and `refs/memory/main` are pushed (when they
/// exist). Code branches are never touched. Push is always explicit — the
/// caller must invoke this function; no automatic push occurs.
///
/// When `remote.refspec` is set, it is used verbatim (split by whitespace
/// into multiple refspec arguments) instead of auto-discovered memory refs.
///
/// # Errors
///
/// Returns [`StoreError`] with kind `TransportFailed`, `AuthenticationFailed`,
/// `NamespaceRejected`, or `FastForwardRequired`.
pub fn push_to_remote(
    git_dir: &Path,
    remote: &MemoryRemote,
    force: bool,
) -> Result<(), StoreError> {
    validate_remote_url(&remote.url)?;
    // Read before the push, recorded after it: what lands on the remote is what
    // `refs/memory/main` pointed at when git was handed the refspec. A custom
    // refspec is not recorded at all — it may move anything, so nothing here
    // can claim to know what the remote ended up with.
    let pushed = current_main_revision(git_dir).ok().flatten();
    let refspecs: Vec<String> = if let Some(custom) = &remote.refspec {
        validate_refspec(custom)?;
        custom.split_whitespace().map(str::to_owned).collect()
    } else {
        let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
        if repo.find_reference(MAIN_REF).is_err() {
            return Err(StoreError::new(
                StoreErrorKind::TransportFailed,
                "no memory ref to push — the store is not initialized",
                serde_json::json!({}),
            ));
        }
        let prefix = if force { "+" } else { "" };
        vec![format!("{prefix}{MAIN_REF}:{MAIN_REF}")]
    };

    let output = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .arg("push")
        // Everything after `--` is positional: a URL or refspec can never be
        // read as an option, however the config was written.
        .arg("--")
        .arg(&remote.url)
        .args(&refspecs)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            StoreError::new(
                StoreErrorKind::TransportFailed,
                "failed to spawn git push",
                serde_json::json!({"detail": e.to_string()}),
            )
        })?;

    if output.status.success() {
        // Best effort: the memory is published either way, and a status field
        // that could not be written is not worth failing a push over. The next
        // exchange records it again.
        if let (Some(pushed), None) = (&pushed, &remote.refspec) {
            let _ = record_known_remote_revision(git_dir, pushed);
        }
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);
    Err(classify_push_error(exit_code, &stderr))
}

/// What `refs/memory/main` points at, or `None` when there is no such ref yet.
fn current_main_revision(git_dir: &Path) -> Result<Option<Revision>, StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    match repo.find_reference(MAIN_REF) {
        Ok(reference) => Ok(reference.target().map(Revision::from_oid)),
        Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
        Err(e) => Err(StoreError::repository("read memory ref", e)),
    }
}

/// Fetch remote refs/memory/main into a temporary ref.
fn fetch_to_temp_ref(git_dir: &Path, remote: &MemoryRemote) -> Result<(), StoreError> {
    // Clean up any stale temp ref first.
    let _ = cleanup_temp_ref_by_path(git_dir);

    validate_remote_url(&remote.url)?;
    let refspec = format!("+refs/memory/main:{FETCH_TEMP_REF}");
    let output = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args(["fetch"])
        .arg("--")
        .arg(&remote.url)
        .arg(&refspec)
        .env("LC_ALL", "C")
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| {
            StoreError::new(
                StoreErrorKind::TransportFailed,
                "failed to spawn git fetch",
                serde_json::json!({"detail": e.to_string()}),
            )
        })?;

    if output.status.success() {
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);
    Err(classify_read_error("git fetch", exit_code, &stderr))
}

/// Classify a failed read from a remote into a machine-readable `StoreError`.
///
/// Uses the exit code as the primary signal and English stderr (guaranteed
/// by `LC_ALL=C` set on the subprocess) as a secondary discriminator.
/// Git exit codes: 128 = generic error, 1 = no matching refs / nothing
/// fetched. The stderr is always included in `data` for caller-side
/// debugging. `operation` names the command that failed, because fetching
/// history and listing refs fail the same way and read differently.
fn classify_read_error(operation: &str, exit_code: i32, stderr: &str) -> StoreError {
    let trimmed = stderr.trim();
    // Exit code 128 is the universal Git error code. We parse stderr for
    // sub-classification, which is safe because LC_ALL=C guarantees English.
    if exit_code == 128 {
        if trimmed.contains("Permission denied")
            || trimmed.contains("authentication")
            || trimmed.contains("Could not read from remote")
        {
            return StoreError::new(
                StoreErrorKind::AuthenticationFailed,
                format!("authentication or connection failed during {operation}"),
                serde_json::json!({"exit_code": exit_code, "stderr": trimmed}),
            );
        }
        if trimmed.contains("does not appear to be a git repository") {
            return StoreError::new(
                StoreErrorKind::TransportFailed,
                "remote is not a Git repository",
                serde_json::json!({"exit_code": exit_code, "stderr": trimmed}),
            );
        }
    }
    StoreError::new(
        StoreErrorKind::TransportFailed,
        format!("{operation} failed"),
        serde_json::json!({"exit_code": exit_code, "stderr": trimmed}),
    )
}

/// Classify a git push error into a machine-readable `StoreError`.
///
/// Uses the exit code as the primary signal and English stderr (guaranteed
/// by `LC_ALL=C`) as a secondary discriminator. Git push uses:
/// 128 = generic error, 1 = nothing to push / refs up to date.
fn classify_push_error(exit_code: i32, stderr: &str) -> StoreError {
    let trimmed = stderr.trim();
    if exit_code == 128 {
        if trimmed.contains("Permission denied") || trimmed.contains("authentication") {
            return StoreError::new(
                StoreErrorKind::AuthenticationFailed,
                "authentication failed during push",
                serde_json::json!({"exit_code": exit_code, "stderr": trimmed}),
            );
        }
        if trimmed.contains("! [remote rejected]")
            && (trimmed.contains("refs/memory/") || trimmed.contains("namespace"))
        {
            return StoreError::new(
                StoreErrorKind::NamespaceRejected,
                "remote rejected the refs/memory/* namespace",
                serde_json::json!({"exit_code": exit_code, "stderr": trimmed}),
            );
        }
        if trimmed.contains("non-fast-forward")
            || trimmed.contains("fetch first")
            || trimmed.contains("Updates were rejected")
        {
            return StoreError::new(
                StoreErrorKind::FastForwardRequired,
                "remote has diverged — fetch and merge first",
                serde_json::json!({"exit_code": exit_code, "stderr": trimmed}),
            );
        }
    }
    StoreError::new(
        StoreErrorKind::TransportFailed,
        "git push failed",
        serde_json::json!({"exit_code": exit_code, "stderr": trimmed}),
    )
}

/// Perform a record-level merge between local and remote snapshots.
///
/// Different keys are merged automatically. Same-key conflicts are collected
/// and returned in the result — the caller decides resolution.
fn merge_records(
    store: &GitStore,
    local_revision: &Revision,
    remote_revision: &Revision,
) -> Result<FetchResult, StoreError> {
    let local_records = store.read_records(local_revision)?;
    let remote_records = store.read_records_unchecked(remote_revision)?;

    let local_map: BTreeMap<&RecordId, &StoredRecord> =
        local_records.iter().map(|(id, r)| (id, r)).collect();
    let remote_map: BTreeMap<&RecordId, &StoredRecord> =
        remote_records.iter().map(|(id, r)| (id, r)).collect();

    let all_keys: BTreeSet<&RecordId> =
        local_map.keys().chain(remote_map.keys()).copied().collect();

    // What both sides had before they diverged. Absent only when the two
    // histories share no commit at all, which a memory that has ever been
    // exchanged does not — and where it is absent, every same-key difference
    // is a record two people wrote independently, which the merge below treats
    // as an empty common ancestor rather than refusing.
    let base_records = merge_base_records(store, local_revision, remote_revision)?;
    let base_map: BTreeMap<&RecordId, &StoredRecord> =
        base_records.iter().map(|(id, r)| (id, r)).collect();

    let mut operations = Vec::new();
    let mut overlaps: Vec<Overlap> = Vec::new();

    for key in &all_keys {
        let local = local_map.get(key);
        let remote = remote_map.get(key);

        match (local, remote) {
            (Some(local), Some(remote)) => {
                if records_equal(local, remote) {
                    continue;
                }
                // Both sides moved. What was common to them is the third point
                // a merge needs, and it is what turns most of these from a
                // conflict into an ordinary join where nobody loses anything.
                let base = base_map.get(key).copied();
                match reconcile(base, local, remote)? {
                    Reconciled::Unchanged => {}
                    Reconciled::Merged { record, overlap } => {
                        if !overlap.is_quiet() {
                            overlaps.push(overlap);
                        }
                        operations.push(Operation::put(record));
                    }
                }
            }
            (Some(_) | None, None) => {
                // Key exists only locally or neither — keep as-is / nothing to do.
            }
            (None, Some(remote)) => {
                // Key exists only remotely — add it, minus the one thing about
                // it that is not a fact about the record.
                operations.push(Operation::put(without_foreign_presence(remote)));
            }
        }
    }

    if operations.is_empty() {
        // No changes — identical content despite different commit history.
        return Ok(FetchResult {
            local_revision_before: local_revision.clone(),
            local_revision_after: local_revision.clone(),
            remote_revision: remote_revision.clone(),
            fast_forward: false,
            merged: true,
            conflicts: Vec::new(),
            overlaps,
        });
    }

    // Apply the merge as a new transaction on top of local.
    let result = store.apply(&Transaction {
        id: format!("merge-{}", random_suffix()),
        expected_revision: local_revision.clone(),
        operations,
    })?;

    Ok(FetchResult {
        local_revision_before: local_revision.clone(),
        local_revision_after: result.revision,
        remote_revision: remote_revision.clone(),
        fast_forward: false,
        merged: true,
        conflicts: Vec::new(),
        overlaps,
    })
}

/// What both sides held before they diverged.
///
/// The third point a merge needs. Without it, "this record has a paragraph the
/// other does not" has two readings — one side added it, or the other side
/// removed it — and no way to choose between them; that ambiguity is the whole
/// reason a two-way comparison can only report a conflict.
///
/// An empty answer is a valid one: two histories with no commit in common mean
/// every shared key was written independently by two people, which merges as
/// though from an empty ancestor.
fn merge_base_records(
    store: &GitStore,
    local: &Revision,
    remote: &Revision,
) -> Result<Vec<(RecordId, StoredRecord)>, StoreError> {
    let repo = Repository::open(store.git_dir()).map_err(|e| StoreError::repository("open", e))?;
    let (Ok(local_oid), Ok(remote_oid)) = (local.oid(), remote.oid()) else {
        return Ok(Vec::new());
    };
    let Ok(base) = repo.merge_base(local_oid, remote_oid) else {
        return Ok(Vec::new());
    };
    store.read_records_unchecked(&Revision::from_oid(base))
}

/// What became of one record that both sides changed.
///
/// There is no "cannot be reconciled" here, and that is the point of the whole
/// change: every same-key difference now becomes one record. What used to be a
/// third answer is now the overlap this one carries — the merge happened, and
/// somebody is owed the news of what a colleague wrote that is not in it.
enum Reconciled {
    /// The two differ in nothing that is kept, so there is nothing to write.
    Unchanged,
    /// A single record, and what the other side lost to it.
    Merged {
        record: StoredRecord,
        overlap: Overlap,
    },
}

/// Join one record's two versions, keeping this side only where they collide.
///
/// **Every member is asked the same three-way question**, because "this side
/// wins" is a rule about disagreement and most differences are not one. A field
/// this side never touched has exactly one change in it — theirs — and taking
/// ours there would not be resolving a conflict but discarding a colleague's
/// work that nothing here was contesting.
///
/// So, member by member: if ours still equals the common ancestor, only they
/// moved and theirs is the answer; if theirs equals it, only we moved. Where
/// both moved and disagree, *then* this side keeps its version — whoever is
/// fetching is at the keyboard, can see the result and can put it right, while
/// the other party is not here to be asked — and the member is named in the
/// overlap so the decision is reported rather than absorbed.
///
/// The body is the same rule at a finer grain: merged line by line by Git, so a
/// paragraph added here and a paragraph added there both survive, and only the
/// same lines rewritten twice make a collision.
///
/// Sets — tags, links, the source paths — merge per member, and cannot collide:
/// with two states to be in, a member the two sides disagree about is one that
/// exactly one of them moved.
fn reconcile(
    base: Option<&StoredRecord>,
    local: &StoredRecord,
    remote: &StoredRecord,
) -> Result<Reconciled, StoreError> {
    let (StoredRecord::Plaintext { envelope: ours }, StoredRecord::Plaintext { envelope: theirs }) =
        (local, remote);
    // Absent when the two histories share no commit: every difference then
    // reads as something both sides wrote independently, which is a collision
    // by this rule and resolved as one.
    let was: Option<&Envelope> = match base {
        Some(StoredRecord::Plaintext { envelope }) => Some(envelope),
        None => None,
    };

    let mut overlap = Overlap::of(&ours.key);
    let mut merged = (**ours).clone();

    // A record whose bytes are a file in the working tree does not merge them
    // here: that file is merged by Git along with the branch it belongs to, and
    // doing it a second time would be two answers to one question. Its digest
    // is what the local scan resolved, so it stays local for the same reason.
    if !ours.is_reference() && !theirs.is_reference() {
        let (content, collided) = merge_bodies(
            was.map_or("", |envelope| envelope.content.as_str()),
            &ours.content,
            &theirs.content,
        )?;
        merged.content = content;
        merged.refresh_content_hash();
        overlap.body = collided;
    }

    merged.kind = pick(
        "kind",
        was.map(|e| &e.kind),
        &ours.kind,
        &theirs.kind,
        &mut overlap,
    );
    merged.title = pick(
        "title",
        was.map(|e| &e.title),
        &ours.title,
        &theirs.title,
        &mut overlap,
    );
    merged.folder = pick(
        "folder",
        was.map(|e| &e.folder),
        &ours.folder,
        &theirs.folder,
        &mut overlap,
    );
    merged.is_folder = pick(
        "is_folder",
        was.map(|e| &e.is_folder),
        &ours.is_folder,
        &theirs.is_folder,
        &mut overlap,
    );
    merged.archive = pick(
        "archive",
        was.map(|e| &e.archive),
        &ours.archive,
        &theirs.archive,
        &mut overlap,
    );
    merged.freshness = pick(
        "freshness",
        was.map(|e| &e.freshness),
        &ours.freshness,
        &theirs.freshness,
        &mut overlap,
    );
    merged.media_type = pick(
        "media_type",
        was.map(|e| &e.media_type),
        &ours.media_type,
        &theirs.media_type,
        &mut overlap,
    );

    merged.tags = merge_members(was.map(|e| e.tags.as_slice()), &ours.tags, &theirs.tags);
    merged.links = merge_members(was.map(|e| e.links.as_slice()), &ours.links, &theirs.links);
    merged.source_paths.scope = merge_members(
        was.map(|e| e.source_paths.scope.as_slice()),
        &ours.source_paths.scope,
        &theirs.source_paths.scope,
    );
    merged.source_paths.observed = merge_members(
        was.map(|e| e.source_paths.observed.as_slice()),
        &ours.source_paths.observed,
        &theirs.source_paths.observed,
    );

    // The product fields the record's type declares live here, flattened onto
    // the envelope. They are the project's own data and merge like any other
    // member, one key at a time.
    merged.extensions = merge_keys(
        was.map(|e| &e.extensions),
        &ours.extensions,
        &theirs.extensions,
        &mut overlap,
    );

    merged.content_ref = merge_content_ref(
        was.and_then(|e| e.content_ref.as_ref()),
        ours.content_ref.as_ref(),
        theirs.content_ref.as_ref(),
        &mut overlap,
    );

    if merged == **ours && overlap.is_quiet() {
        return Ok(Reconciled::Unchanged);
    }
    Ok(Reconciled::Merged {
        record: StoredRecord::Plaintext {
            envelope: Box::new(merged),
        },
        overlap,
    })
}

/// One member, answered by which side actually moved it.
///
/// The whole of the rule in four lines, and it is deliberately not "ours wins":
/// ours wins *the disagreement*, and a member only one side touched is not one.
fn pick<T: Clone + PartialEq>(
    member: &'static str,
    was: Option<&T>,
    ours: &T,
    theirs: &T,
    overlap: &mut Overlap,
) -> T {
    if ours == theirs {
        return ours.clone();
    }
    match was {
        // We are still where the ancestor left us, so the only change here is
        // theirs and there is nothing to resolve.
        Some(was) if was == ours => theirs.clone(),
        // They never moved.
        Some(was) if was == theirs => ours.clone(),
        // Both moved, or there is no ancestor to tell. This is the collision,
        // and it is the one place the fetcher's version wins.
        _ => {
            overlap.fields.push(member.to_owned());
            ours.clone()
        }
    }
}

/// A set, merged by asking each member whether it was added or removed.
///
/// Order is this side's, with what they added appended: a set has no order to
/// disagree about, and keeping ours stable means a fetch that changes nothing a
/// person can see does not reorder their tags.
///
/// No collision is possible. A member is present or it is not, so if the two
/// sides disagree about one, exactly one of them differs from the ancestor —
/// and that one is the change.
fn merge_members<T: Clone + PartialEq>(was: Option<&[T]>, ours: &[T], theirs: &[T]) -> Vec<T> {
    let had = |member: &T| was.is_some_and(|was| was.contains(member));
    let mut merged: Vec<T> = ours
        .iter()
        // Ours, minus what they removed: it was there before and is not there
        // now on their side, so removing it is their change.
        .filter(|member| theirs.contains(member) || !had(member))
        .cloned()
        .collect();
    // Theirs, minus what we removed, and minus what we already have.
    let added: Vec<T> = theirs
        .iter()
        .filter(|member| !merged.contains(member) && !had(member))
        .cloned()
        .collect();
    merged.extend(added);
    merged
}

/// A map, merged one key at a time by the same rule as any other member.
fn merge_keys(
    was: Option<&BTreeMap<String, Value>>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
    overlap: &mut Overlap,
) -> BTreeMap<String, Value> {
    let mut merged = BTreeMap::new();
    let names: BTreeSet<&String> = ours.keys().chain(theirs.keys()).collect();
    for name in names {
        let chosen = pick_optional(
            name,
            was.and_then(|was| was.get(name)),
            ours.get(name),
            theirs.get(name),
            overlap,
        );
        if let Some(value) = chosen {
            merged.insert(name.clone(), value);
        }
    }
    merged
}

/// [`pick`] where absence is one of the states a member can be in.
///
/// A field somebody removed and a field somebody added are the same question
/// asked from opposite ends, so `None` takes part in the comparison rather than
/// short-circuiting it.
fn pick_optional<T: Clone + PartialEq>(
    member: &str,
    was: Option<&T>,
    ours: Option<&T>,
    theirs: Option<&T>,
    overlap: &mut Overlap,
) -> Option<T> {
    if ours == theirs {
        return ours.cloned();
    }
    match was {
        Some(_) | None if was == ours => theirs.cloned(),
        _ if was == theirs => ours.cloned(),
        _ => {
            overlap.fields.push(member.to_owned());
            ours.cloned()
        }
    }
}

/// Where a record's bytes are, merged — and whether they are here, not.
///
/// **The locator travels.** It is a fact about the document that everybody
/// shares: a colleague who moved a file moved it for the project, and a fetch
/// that dropped that would leave this corpus pointing at a path their commit
/// emptied. What the local scan then makes of it is the scan's business — it
/// pairs a record with the file carrying its digest and files the locator back
/// where this working tree keeps it, or asks, and until then `presence` says
/// the document is not on this branch.
///
/// **`presence` does not travel**, and that is the one member held back. It is
/// this working tree's answer to "is the file here", which is a different
/// answer on every branch and on every machine; accepting theirs would import
/// somebody else's checkout as a fact about ours.
fn merge_content_ref(
    was: Option<&ContentRef>,
    ours: Option<&ContentRef>,
    theirs: Option<&ContentRef>,
    overlap: &mut Overlap,
) -> Option<ContentRef> {
    // Compared without it, so a record identical but for whose branch is
    // checked out is not a difference at all.
    let bare = |reference: &ContentRef| ContentRef {
        presence: Presence::Present,
        ..reference.clone()
    };
    let merged = pick_optional(
        "content_ref",
        was.map(bare).as_ref(),
        ours.map(bare).as_ref(),
        theirs.map(bare).as_ref(),
        overlap,
    );
    merged.map(|reference| ContentRef {
        presence: ours.map_or(Presence::Present, |ours| ours.presence),
        ..reference
    })
}

/// Three-way merge of one body, resolved in this side's favour where it must be.
///
/// Two passes in the colliding case, and the second is what makes the first
/// worth doing: asked to resolve, libgit2 reports the result as automergeable
/// whether or not anything collided, so the plain pass is the only way to learn
/// that somebody's lines were dropped. The ordinary case costs one pass.
fn merge_bodies(ancestor: &str, ours: &str, theirs: &str) -> Result<(String, bool), StoreError> {
    let merged = merge_once(ancestor, ours, theirs, None)?;
    if merged.1 {
        return Ok((merged.0, false));
    }
    let resolved = merge_once(ancestor, ours, theirs, Some(FileFavor::Ours))?;
    Ok((resolved.0, true))
}

/// One pass, answering the merged text and whether it needed no help.
fn merge_once(
    ancestor: &str,
    ours: &str,
    theirs: &str,
    favor: Option<FileFavor>,
) -> Result<(String, bool), StoreError> {
    let mut base = MergeFileInput::new();
    base.content(ancestor.as_bytes());
    let mut mine = MergeFileInput::new();
    mine.content(ours.as_bytes());
    let mut yours = MergeFileInput::new();
    yours.content(theirs.as_bytes());

    let mut options = MergeFileOptions::new();
    if let Some(favor) = favor {
        options.favor(favor);
    }

    let result = git2::merge_file(&base, &mine, &yours, Some(&mut options))
        .map_err(|e| StoreError::repository("merge record bodies", e))?;
    let text = String::from_utf8(result.content().to_vec()).map_err(|_| {
        StoreError::new(
            StoreErrorKind::InvalidArgument,
            "a merged record body is not valid UTF-8",
            serde_json::json!({}),
        )
    })?;
    Ok((text, result.is_automergeable()))
}

/// Check whether two stored records have identical content.
/// A record as it arrives, with the sender's opinion of our working tree
/// dropped.
///
/// `presence` says whether the content is here — on this machine, on this
/// branch, right now. It is written by a scan and it travels with the record
/// because the record is where a scan can put it, not because it means anything
/// away from the tree it was measured in. Adopting the sender's value would
/// hide a document that is sitting in front of us because it was missing from
/// somebody else's checkout. The default is what "nobody has looked yet" means,
/// and the scan at project open settles it.
fn without_foreign_presence(record: &StoredRecord) -> StoredRecord {
    let mut record = record.clone();
    let StoredRecord::Plaintext { envelope } = &mut record;
    if let Some(reference) = &mut envelope.content_ref {
        reference.presence = memory_hub_core::Presence::Present;
    }
    record
}

/// Whether two records say the same thing.
///
/// **The whole envelope**, save one member. This used to be the content digest
/// alone, which made a record retitled, refiled, retagged or relinked on the
/// other side *equal* to ours — so the merge below skipped it and their work
/// was dropped without anybody merging anything. A record is what it says about
/// itself as much as what it holds.
///
/// `presence` is the exception, and stays out for the reason it always did: it
/// is this working tree's answer to whether the file is here, so comparing it
/// would turn "these two machines are on different branches" into a difference
/// on every record whose content is a repository file.
///
/// Two clones per shared key, which a fetch can afford: it is already reading
/// and writing Git objects for every one of them, and a comparison spelled out
/// member by member is a comparison somebody forgets to extend.
fn records_equal(a: &StoredRecord, b: &StoredRecord) -> bool {
    let (StoredRecord::Plaintext { envelope: ours }, StoredRecord::Plaintext { envelope: theirs }) =
        (a, b);
    as_shared(ours) == as_shared(theirs)
}

/// An envelope with the sender's opinion of our working tree dropped.
fn as_shared(envelope: &Envelope) -> Envelope {
    let mut envelope = envelope.clone();
    if let Some(reference) = &mut envelope.content_ref {
        reference.presence = Presence::Present;
    }
    envelope
}

fn cleanup_temp_ref(repo: &Repository) -> Result<(), StoreError> {
    match repo.find_reference(FETCH_TEMP_REF) {
        Ok(mut reference) => {
            reference
                .delete()
                .map_err(|e| StoreError::repository("delete temp ref", e))?;
        }
        Err(e) if e.code() == git2::ErrorCode::NotFound => {}
        Err(e) => return Err(StoreError::repository("find temp ref", e)),
    }
    Ok(())
}

fn cleanup_temp_ref_by_path(git_dir: &Path) -> Result<(), StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    cleanup_temp_ref(&repo)
}

/// A suffix that makes a merge's transaction id its own.
///
/// Transaction ids may not be reused, and two merges in the same second are
/// ordinary, so this is random rather than derived from the clock. A machine
/// whose CSPRNG refuses is a machine with larger problems than a merge: the
/// id falls back to a fixed word, and the reuse check refuses the second one.
fn random_suffix() -> String {
    use std::fmt::Write;
    let mut bytes = [0u8; 16];
    if getrandom::fill(&mut bytes).is_err() {
        return "unknown".to_owned();
    }
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, byte| {
            let _ = write!(acc, "{byte:02x}");
            acc
        })
}

use git2::{FileFavor, MergeFileInput, MergeFileOptions, Repository};
use std::collections::BTreeSet;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::check_push_policy;
    use crate::{GitStore, Operation, Transaction};
    use memory_hub_core::StoredRecord;

    #[test]
    fn remote_urls_that_look_like_options_are_rejected() {
        assert!(super::validate_remote_url("--upload-pack=/bin/sh").is_err());
        assert!(super::validate_remote_url("-o").is_err());
        assert!(super::validate_remote_url("").is_err());
    }

    #[test]
    fn remote_helper_urls_are_rejected() {
        assert!(super::validate_remote_url("ext::sh -c 'id > /tmp/pwned'").is_err());
        assert!(super::validate_remote_url("hg::https://example.com/repo").is_err());
    }

    #[test]
    fn ordinary_remote_urls_are_accepted() {
        for url in [
            "https://example.com/team/project.git",
            "ssh://git@example.com/team/project.git",
            "git@example.com:team/project.git",
            "/srv/git/project.git",
            "../sibling-repo",
            "ssh://git@[::1]:22/team/project.git",
        ] {
            assert!(super::validate_remote_url(url).is_ok(), "rejected {url}");
        }
    }

    #[test]
    fn refspecs_outside_the_memory_namespace_are_rejected() {
        assert!(super::validate_refspec("+refs/heads/main:refs/heads/main").is_err());
        assert!(super::validate_refspec("refs/memory/main:refs/heads/main").is_err());
        assert!(super::validate_refspec("--receive-pack=/bin/sh").is_err());
        assert!(super::validate_refspec("").is_err());
    }

    #[test]
    fn memory_namespace_refspecs_are_accepted() {
        assert!(super::validate_refspec("+refs/memory/main:refs/memory/main").is_ok());
        assert!(
            super::validate_refspec(
                "refs/memory/main:refs/memory/main refs/memory/main:refs/memory/main"
            )
            .is_ok()
        );
        assert!(super::validate_refspec(":refs/memory/stale").is_ok());
    }

    #[test]
    fn push_policy_allows_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let store = GitStore::open(dir.path()).unwrap();
        let result = check_push_policy(&store).unwrap();
        assert!(result.allowed);
        assert_eq!(result.stale_count, 0);
        assert!(result.warnings.is_empty());
    }

    #[test]
    fn push_policy_allows_fresh_records() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let store = GitStore::open(dir.path()).unwrap();
        let revision = store.current().unwrap().revision().clone();
        store
            .apply(&Transaction {
                id: "put-fresh".into(),
                expected_revision: revision,
                operations: vec![Operation::put(StoredRecord::Plaintext {
                    envelope: Box::new(
                        memory_hub_core::Envelope::new("note/fresh", "note", "content").unwrap(),
                    ),
                })],
            })
            .unwrap();
        let result = check_push_policy(&store).unwrap();
        assert!(result.allowed);
        assert_eq!(result.stale_count, 0);
    }

    #[test]
    fn push_policy_detects_stale_records() {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        let store = GitStore::open(dir.path()).unwrap();
        let revision = store.current().unwrap().revision().clone();

        // Create a stale record.
        let mut envelope = memory_hub_core::Envelope::new("note/stale", "note", "content").unwrap();
        envelope.freshness.state = memory_hub_core::FreshnessState::Stale;

        store
            .apply(&Transaction {
                id: "put-stale".into(),
                expected_revision: revision,
                operations: vec![Operation::put(StoredRecord::Plaintext {
                    envelope: Box::new(envelope),
                })],
            })
            .unwrap();

        let result = check_push_policy(&store).unwrap();
        assert_eq!(result.stale_count, 1);
        // Default policy is "warn" — allowed but with warnings.
        assert!(result.allowed);
        assert!(!result.warnings.is_empty());
    }
}
