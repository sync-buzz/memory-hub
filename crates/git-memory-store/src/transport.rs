//! Memory remote transport: fetch, push, and record-level merge.
//!
//! All network operations shell out to the `git` CLI because `git2` is
//! compiled without SSH/HTTPS support. Operations are scoped to
//! `refs/memory/*` via explicit refspecs — code branches are never touched.

use std::collections::BTreeMap;
use std::path::Path;
use std::process::{Command, Stdio};

use git_memory_core::{FreshnessState, PolicyMode, PolicyResolver, StoredRecord};
use serde::{Deserialize, Serialize};

use crate::{
    GitStore, Operation, RecordId, Revision, StoreError, StoreErrorKind, Transaction, STAGED_REF,
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
    pub conflicts: Vec<ConflictEntry>,
}

/// A same-key conflict discovered during merge.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ConflictEntry {
    pub key: String,
    pub local_content_hash: String,
    pub remote_content_hash: String,
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

    let url = match config.get_string("git-memory.remote.url") {
        Ok(url) => url,
        Err(e) if e.code() == git2::ErrorCode::NotFound => return Ok(None),
        Err(e) => return Err(StoreError::repository("read remote url", e)),
    };
    let refspec = config.get_string("git-memory.remote.refspec").ok();
    Ok(Some(MemoryRemote { url, refspec }))
}

/// Write the memory remote to the repository's Git config.
///
/// # Errors
///
/// Returns [`StoreError`] if the config cannot be written.
pub fn write_remote_config(git_dir: &Path, remote: &MemoryRemote) -> Result<(), StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let mut config = repo
        .config()
        .map_err(|e| StoreError::repository("read config", e))?;
    config
        .set_str("git-memory.remote.url", &remote.url)
        .map_err(|e| StoreError::repository("set remote url", e))?;
    if let Some(refspec) = &remote.refspec {
        config
            .set_str("git-memory.remote.refspec", refspec)
            .map_err(|e| StoreError::repository("set remote refspec", e))?;
    } else {
        let _ = config.remove("git-memory.remote.refspec");
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
    let _ = config.remove("git-memory.remote.url");
    let _ = config.remove("git-memory.remote.refspec");
    Ok(())
}

/// Fetch from the configured memory remote into a temporary ref and return
/// the remote revision. Does NOT merge or update `refs/memory/staged`.
///
/// The caller is responsible for calling [`cleanup_temp_ref_pub`] after
/// processing the fetched data.
///
/// # Errors
///
/// Returns [`StoreError`] with kind `TransportFailed`, `AuthenticationFailed`,
/// `NamespaceRejected`, or `SignatureInvalid`.
pub fn fetch_remote_revision(
    store: &GitStore,
    remote: &MemoryRemote,
    allowed_signers: &[String],
) -> Result<(Revision, Revision), StoreError> {
    let git_dir = store.git_dir();
    let local_before = store.current()?.revision;

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
                "remote has no refs/memory/staged — the remote store is not initialized",
                serde_json::json!({"remote": remote.url}),
            ));
        }
        Err(e) => return Err(StoreError::repository("read fetch temp ref", e)),
    };

    if !allowed_signers.is_empty() {
        verify_fetched_signatures(&repo, local_before.oid()?, remote_oid, allowed_signers)?;
    }

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

/// Fast-forward `refs/memory/staged` to the given revision (which must be
/// the fetched temp ref revision or a descendant of the current staged tip).
///
/// # Errors
///
/// Returns [`StoreError`] if the ref update fails.
pub fn fast_forward_to(git_dir: &Path, remote_revision: &Revision) -> Result<(), StoreError> {
    let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
    let remote_oid = remote_revision.oid()?;
    repo.reference(
        STAGED_REF,
        remote_oid,
        true,
        "git-memory: fetch fast-forward",
    )
    .map_err(|e| StoreError::repository("fast-forward staged ref", e))?;
    Ok(())
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
/// Downloads `refs/memory/staged` from the remote into a temporary ref,
/// verifies SSH signatures on new commits (when a signer is configured),
/// then either fast-forwards or performs a record-level merge.
///
/// # Errors
///
/// Returns [`StoreError`] with kind `TransportFailed`, `AuthenticationFailed`,
/// `NamespaceRejected`, `FastForwardRequired`, `Diverged`, or `SignatureInvalid`.
pub fn fetch_and_merge(
    store: &GitStore,
    remote: &MemoryRemote,
    allowed_signers: &[String],
) -> Result<FetchResult, StoreError> {
    let git_dir = store.git_dir();
    let local_before = store.current()?.revision;

    // Step 1: Fetch remote refs/memory/staged to a temp ref.
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
                "remote has no refs/memory/staged — the remote store is not initialized",
                serde_json::json!({"remote": remote.url}),
            ));
        }
        Err(e) => return Err(StoreError::repository("read fetch temp ref", e)),
    };

    // Step 3: Verify SSH signatures on new commits (if signers configured).
    if !allowed_signers.is_empty() {
        verify_fetched_signatures(&repo, local_before.oid()?, remote_oid, allowed_signers)?;
    }

    // Step 4: Determine fast-forward vs divergence.
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
        });
    }

    let is_ff = repo
        .graph_descendant_of(remote_oid, local_oid)
        .map_err(|e| StoreError::repository("check descendant", e))?;

    if is_ff {
        // Fast-forward: remote is ahead of local.
        repo.reference(
            STAGED_REF,
            remote_oid,
            true,
            "git-memory: fetch fast-forward",
        )
        .map_err(|e| StoreError::repository("fast-forward staged ref", e))?;
        guard.disarm();
        cleanup_temp_ref(&repo)?;
        Ok(FetchResult {
            local_revision_before: local_before,
            local_revision_after: Revision::from_oid(remote_oid),
            remote_revision: Revision::from_oid(remote_oid),
            fast_forward: true,
            merged: false,
            conflicts: Vec::new(),
        })
    } else {
        // Not a fast-forward. Check if local is empty (genesis only, no records).
        // If so, treat it as a fast-forward — there's nothing to merge.
        let local_records = store.read_records(&local_before)?;
        if local_records.is_empty() {
            repo.reference(
                STAGED_REF,
                remote_oid,
                true,
                "git-memory: fetch fast-forward from empty",
            )
            .map_err(|e| StoreError::repository("fast-forward staged ref", e))?;
            guard.disarm();
            cleanup_temp_ref(&repo)?;
            return Ok(FetchResult {
                local_revision_before: local_before,
                local_revision_after: Revision::from_oid(remote_oid),
                remote_revision: Revision::from_oid(remote_oid),
                fast_forward: true,
                merged: false,
                conflicts: Vec::new(),
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
    let resolver = PolicyResolver::git_memory_defaults();
    let policy = resolver.resolve("memory_push_stale", None).map_err(|e| {
        StoreError::new(
            StoreErrorKind::InvalidArgument,
            "failed to resolve push policy",
            serde_json::json!({"detail": e.to_string()}),
        )
    })?;

    let revision = store.current()?.revision;
    let records = store.read_records_pub(&revision)?;

    // Check if there are encrypted records — if so, we can't check freshness.
    let has_encrypted = records
        .iter()
        .any(|(_, r)| matches!(r, StoredRecord::Encrypted { .. }));

    if has_encrypted {
        return Ok(PushPolicyResult {
            allowed: true,
            warnings: vec![
                "encrypted records detected — freshness policy cannot be checked without unlocking the store".to_owned(),
            ],
            stale_count: 0,
        });
    }

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
            StoredRecord::Encrypted { .. } => None,
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
/// Only `refs/memory/staged` and `refs/memory/main` are pushed (when they
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
    let refspecs: Vec<String> = if let Some(custom) = &remote.refspec {
        custom.split_whitespace().map(str::to_owned).collect()
    } else {
        let repo = Repository::open(git_dir).map_err(|e| StoreError::repository("open", e))?;
        let mut specs = Vec::new();
        for (local, remote_ref) in [
            ("refs/memory/staged", "refs/memory/staged"),
            ("refs/memory/main", "refs/memory/main"),
        ] {
            if repo.find_reference(local).is_ok() {
                let prefix = if force { "+" } else { "" };
                specs.push(format!("{prefix}{local}:{remote_ref}"));
            }
        }
        if specs.is_empty() {
            return Err(StoreError::new(
                StoreErrorKind::TransportFailed,
                "no memory refs to push — the store is not initialized",
                serde_json::json!({}),
            ));
        }
        specs
    };

    let output = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .arg("push")
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
        return Ok(());
    }

    let stderr = String::from_utf8_lossy(&output.stderr);
    let exit_code = output.status.code().unwrap_or(-1);
    Err(classify_push_error(exit_code, &stderr))
}

/// Fetch remote refs/memory/staged into a temporary ref.
fn fetch_to_temp_ref(git_dir: &Path, remote: &MemoryRemote) -> Result<(), StoreError> {
    // Clean up any stale temp ref first.
    let _ = cleanup_temp_ref_by_path(git_dir);

    let refspec = format!("+refs/memory/staged:{FETCH_TEMP_REF}");
    let output = Command::new("git")
        .arg("-C")
        .arg(git_dir)
        .args(["fetch"])
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
    Err(classify_fetch_error(exit_code, &stderr))
}

/// Classify a git fetch error into a machine-readable `StoreError`.
///
/// Uses the exit code as the primary signal and English stderr (guaranteed
/// by `LC_ALL=C` set on the subprocess) as a secondary discriminator.
/// Git exit codes: 128 = generic error, 1 = no matching refs / nothing
/// fetched. The stderr is always included in `data` for caller-side
/// debugging.
fn classify_fetch_error(exit_code: i32, stderr: &str) -> StoreError {
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
                "authentication or connection failed during fetch",
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
        "git fetch failed",
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

/// Walk commits from `remote_oid` back to `local_oid` and verify SSH
/// signatures on each new commit against the allowed signers list.
fn verify_fetched_signatures(
    repo: &Repository,
    local_oid: git2::Oid,
    remote_oid: git2::Oid,
    allowed_signers: &[String],
) -> Result<(), StoreError> {
    let mut revwalk = repo
        .revwalk()
        .map_err(|e| StoreError::repository("create revwalk", e))?;
    revwalk
        .set_sorting(git2::Sort::TOPOLOGICAL)
        .map_err(|e| StoreError::repository("set revwalk sort", e))?;
    revwalk
        .push(remote_oid)
        .map_err(|e| StoreError::repository("push revwalk start", e))?;
    // Hide the local commits we already have.
    revwalk
        .hide(local_oid)
        .map_err(|e| StoreError::repository("hide revwalk base", e))?;

    for oid_result in &mut revwalk {
        let oid = oid_result.map_err(|e| StoreError::repository("revwalk next", e))?;
        let commit = repo
            .find_commit(oid)
            .map_err(|e| StoreError::repository("find commit for verification", e))?;

        // Read raw commit bytes (without gpgsig) for verification.
        let raw = read_raw_commit(repo, oid)?;
        let (commit_without_sig, signature_opt) = extract_gpgsig(&raw);

        let Some(signature) = signature_opt else {
            // Unsigned commit in the memory namespace — reject.
            return Err(StoreError::new(
                StoreErrorKind::SignatureInvalid,
                "fetched commit is unsigned",
                serde_json::json!({
                    "commit": oid.to_string(),
                    "author": commit.author().to_string(),
                }),
            ));
        };

        // Verify against any allowed signer.
        let mut verified = false;
        for signer_key in allowed_signers {
            if git_memory_crypto::verify_ssh_signature(&commit_without_sig, &signature, signer_key)
                .is_ok()
            {
                verified = true;
                break;
            }
        }

        if !verified {
            return Err(StoreError::new(
                StoreErrorKind::SignatureInvalid,
                "commit signature is not from an authorized recipient",
                serde_json::json!({
                    "commit": oid.to_string(),
                    "author": commit.author().to_string(),
                }),
            ));
        }
    }
    Ok(())
}

/// Read the raw commit object bytes.
fn read_raw_commit(repo: &Repository, oid: git2::Oid) -> Result<Vec<u8>, StoreError> {
    let odb = repo
        .odb()
        .map_err(|e| StoreError::repository("open odb", e))?;
    let object = odb
        .read(oid)
        .map_err(|e| StoreError::repository("read raw commit", e))?;
    Ok(object.data().to_vec())
}

/// Extract the `gpgsig` header from a raw commit object.
///
/// Git stores the `gpgsig` header in the commit object as:
///
/// ```text
/// gpgsig -----BEGIN SSH SIGNATURE-----
///  <base64 line 1>
///  <base64 line 2>
///  -----END SSH SIGNATURE-----
/// ```
///
/// The first line of the value follows `gpgsig ` directly (no leading
/// space). Continuation lines are prefixed with a single space. This
/// function collects the first line + all continuation lines, joins them
/// with `\n`, and returns the reconstructed commit without the `gpgsig`
/// header.
///
/// Returns `(commit_without_gpgsig, signature_option)`.
fn extract_gpgsig(raw: &[u8]) -> (Vec<u8>, Option<String>) {
    let text = String::from_utf8_lossy(raw);
    let gpgsig_start = "\ngpgsig ";
    let Some(start) = text.find(gpgsig_start) else {
        return (raw.to_vec(), None);
    };
    // `start` points at the `\n` before `gpgsig `.
    // `header_start` points at `gpgsig `.
    let header_start = start + 1; // skip \n
                                  // `value_start` points at the first char of the value (after "gpgsig ").
    let value_start = header_start + "gpgsig ".len();

    // The rest of the text starting from the first value line.
    let rest = &text[value_start..];

    let mut sig_lines = Vec::new();
    let mut lines_iter = rest.lines().peekable();

    // First line: the value portion on the same line as `gpgsig `.
    if let Some(first_line) = lines_iter.next() {
        sig_lines.push(first_line.to_owned());
    }

    // Continuation lines: each starts with a single space.
    let mut after_sig_bytes = value_start + sig_lines[0].len() + 1; // +1 for \n
    for line in lines_iter {
        if let Some(stripped) = line.strip_prefix(' ') {
            sig_lines.push(stripped.to_owned());
            after_sig_bytes += line.len() + 1; // +1 for \n
        } else {
            break;
        }
    }

    let signature = sig_lines.join("\n");
    if signature.is_empty() || !signature.contains("SSH SIGNATURE") {
        return (raw.to_vec(), None);
    }

    // Reconstruct commit without gpgsig: everything before the `\n` that
    // precedes `gpgsig `, then everything after the signature lines.
    let before = &raw[..start]; // up to and including the char before \n
    let after = &raw[after_sig_bytes..];
    let mut result = before.to_vec();
    result.push(b'\n'); // restore the \n that separated gpgsig from previous header
    result.extend_from_slice(after);
    (result, Some(signature))
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

    let mut operations = Vec::new();
    let mut conflicts = Vec::new();

    for key in &all_keys {
        let local = local_map.get(key);
        let remote = remote_map.get(key);

        match (local, remote) {
            (Some(local), Some(remote)) => {
                if records_equal(local, remote) {
                    continue;
                }
                // Same-key conflict — collect both content hashes.
                conflicts.push(ConflictEntry {
                    key: key.display_value(),
                    local_content_hash: content_hash_of(local),
                    remote_content_hash: content_hash_of(remote),
                });
            }
            (Some(_) | None, None) => {
                // Key exists only locally or neither — keep as-is / nothing to do.
            }
            (None, Some(remote)) => {
                // Key exists only remotely — add it.
                operations.push(Operation::Put {
                    record: (*remote).clone(),
                });
            }
        }
    }

    if !conflicts.is_empty() {
        // Return conflicts without merging — caller resolves.
        return Ok(FetchResult {
            local_revision_before: local_revision.clone(),
            local_revision_after: local_revision.clone(),
            remote_revision: remote_revision.clone(),
            fast_forward: false,
            merged: false,
            conflicts,
        });
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
    })
}

/// Check whether two stored records have identical content.
fn records_equal(a: &StoredRecord, b: &StoredRecord) -> bool {
    match (a, b) {
        (StoredRecord::Plaintext { envelope: ea }, StoredRecord::Plaintext { envelope: eb }) => {
            ea.content_hash == eb.content_hash
        }
        (StoredRecord::Encrypted { encrypted: ea }, StoredRecord::Encrypted { encrypted: eb }) => {
            ea.storage_id == eb.storage_id && ea.ciphertext == eb.ciphertext
        }
        _ => false,
    }
}

/// Extract a content hash for conflict reporting.
fn content_hash_of(record: &StoredRecord) -> String {
    match record {
        StoredRecord::Plaintext { envelope } => envelope.content_hash.as_str().to_owned(),
        StoredRecord::Encrypted { encrypted } => encrypted.storage_id.as_str().to_owned(),
    }
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

fn random_suffix() -> String {
    git_memory_crypto::generate_storage_id().unwrap_or_else(|_| "unknown".into())
}

use git2::Repository;
use std::collections::BTreeSet;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::{check_push_policy, extract_gpgsig};
    use crate::{GitStore, Operation, Transaction};
    use git_memory_core::StoredRecord;

    #[test]
    fn extract_gpgsig_finds_ssh_signature() {
        let raw = b"tree 0123456789abcdef0123456789abcdef01234567\nparent 0123456789abcdef0123456789abcdef01234567\nauthor Test <test@example.com> 1700000000 +0000\ncommitter Test <test@example.com> 1700000000 +0000\ngpgsig -----BEGIN SSH SIGNATURE-----\n U1NIU0lHbmF0dXJlAAEAdXNlci1zdHJpbmc=\n -----END SSH SIGNATURE-----\n\nCommit message\n";

        let (without_sig, sig_opt) = extract_gpgsig(raw);
        let sig = sig_opt.expect("signature should be extracted");
        assert!(sig.contains("BEGIN SSH SIGNATURE"));
        assert!(sig.contains("END SSH SIGNATURE"));
        assert!(sig.contains("U1NIU0lH"));
        // The reconstructed commit must NOT contain gpgsig.
        let without = String::from_utf8_lossy(&without_sig);
        assert!(!without.contains("gpgsig"));
        assert!(without.contains("Commit message"));
    }

    #[test]
    fn extract_gpgsig_returns_none_without_signature() {
        let raw = b"tree 0123456789abcdef0123456789abcdef01234567\nauthor Test <test@example.com> 1700000000 +0000\ncommitter Test <test@example.com> 1700000000 +0000\n\nCommit message\n";

        let (without_sig, sig_opt) = extract_gpgsig(raw);
        assert!(sig_opt.is_none());
        assert_eq!(without_sig.as_slice(), raw);
    }

    #[test]
    fn extract_gpgsig_preserves_other_headers() {
        let raw = b"tree abcdef0000000000000000000000000000000000\nparent 1234560000000000000000000000000000000000\nauthor A <a@b.c> 1700000000 +0000\ncommitter A <a@b.c> 1700000000 +0000\ngpgsig -----BEGIN SSH SIGNATURE-----\n dGVzdA==\n -----END SSH SIGNATURE-----\n\nMsg\n";

        let (without, sig) = extract_gpgsig(raw);
        let s = String::from_utf8_lossy(&without);
        assert!(sig.is_some());
        assert!(s.contains("tree abcdef"));
        assert!(s.contains("parent 123456"));
        assert!(s.contains("author A"));
        assert!(s.contains("Msg"));
        assert!(!s.contains("gpgsig"));
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
                        git_memory_core::Envelope::new("note/fresh", "note", "content").unwrap(),
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
        let mut envelope = git_memory_core::Envelope::new("note/stale", "note", "content").unwrap();
        envelope.freshness.state = git_memory_core::FreshnessState::Stale;

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
