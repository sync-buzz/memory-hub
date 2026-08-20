//! Commit-signing and fetch-verification policy, resolved from Git config.
//!
//! GitHub does not protect `refs/memory/*` with rulesets, so a collaborator
//! with push access can rewrite Memory refs directly. Signatures are the only
//! protection, which makes two decisions explicit here:
//!
//! - **Signing is opt-in.** Every signed commit forks `ssh-keygen`, which is
//!   real cost on batched writes, and a machine without an SSH key must still
//!   be able to use Memory locally. Configure `memory-hub.signing.key`, or let
//!   Memory Hub reuse Git's own SSH signing key (`gpg.format = ssh` plus
//!   `user.signingkey`).
//! - **Verification is fail-closed.** A fetch that cannot check signatures
//!   fails instead of silently importing whatever the remote sent. Operators
//!   who deliberately want unsigned exchange set
//!   `memory-hub.signing.verify = off`.
//!
//! ```text
//! [memory-hub "signing"]
//!     key = /home/alice/.ssh/id_ed25519            ; sign Memory commits
//!     allowedSigner = ssh-ed25519 AAAA... alice    ; repeatable
//!     allowedSignersFile = .memory-hub/allowed_signers
//!     verify = required                            ; required (default) | off
//! ```

use std::path::{Path, PathBuf};

use git2::{Config, Repository};

use crate::error::GitStoreError;
use crate::{StoreError, StoreErrorKind};

const KEY_PATH: &str = "memory-hub.signing.key";
const ALLOWED_SIGNER: &str = "memory-hub.signing.allowedsigner";
const ALLOWED_SIGNERS_FILE: &str = "memory-hub.signing.allowedsignersfile";
const VERIFY: &str = "memory-hub.signing.verify";

/// How `fetch` treats commits whose signature cannot be checked.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VerifyMode {
    /// Refuse to fetch when no allowed signer is known (the default).
    Required,
    /// Accept unsigned and unverified commits.
    Off,
}

/// Effective signing configuration for one repository.
#[derive(Clone, Debug)]
pub struct SigningConfig {
    /// Private key used to sign Memory commits. `None` disables signing.
    pub key_path: Option<PathBuf>,
    /// Public keys accepted as signers of fetched commits.
    pub allowed_signers: Vec<String>,
    /// Whether an unverifiable fetch fails.
    pub verify: VerifyMode,
}

impl SigningConfig {
    /// Whether Memory commits are signed in this repository.
    #[must_use]
    pub const fn signs(&self) -> bool {
        self.key_path.is_some()
    }
}

/// Read the effective signing configuration for a repository.
///
/// # Errors
///
/// Returns [`StoreError`] when the repository or its config cannot be opened,
/// or when `memory-hub.signing.verify` holds an unknown value.
pub fn read_signing_config(git_dir: &Path) -> Result<SigningConfig, StoreError> {
    let repository = Repository::open(git_dir)
        .map_err(|error| StoreError::repository("open repository for signing config", error))?;
    let config = repository
        .config()
        .map_err(|error| StoreError::repository("read signing config", error))?;

    let verify = match config.get_string(VERIFY).ok().as_deref() {
        None | Some("required") => VerifyMode::Required,
        Some("off") => VerifyMode::Off,
        Some(other) => {
            return Err(StoreError::new(
                StoreErrorKind::InvalidArgument,
                "memory-hub.signing.verify must be `required` or `off`",
                serde_json::json!({"field": VERIFY, "received": other}),
            ));
        }
    };

    Ok(SigningConfig {
        key_path: resolve_key_path(&config),
        allowed_signers: resolve_allowed_signers(&config),
        verify,
    })
}

/// Resolve the signing key: the Memory Hub setting first, then Git's own SSH
/// signing key when `gpg.format = ssh`. A `user.signingkey` holding a literal
/// key rather than a path is ignored — `ssh-keygen -Y sign` needs a file.
fn resolve_key_path(config: &Config) -> Option<PathBuf> {
    if let Ok(path) = config.get_string(KEY_PATH) {
        let path = expand_home(&path);
        return path.is_file().then_some(path);
    }
    if config.get_string("gpg.format").ok().as_deref() != Some("ssh") {
        return None;
    }
    let signing_key = config.get_string("user.signingkey").ok()?;
    let path = expand_home(&signing_key);
    path.is_file().then_some(path)
}

/// Collect allowed signer public keys from the repeatable config entry and
/// from an `ssh-keygen`-style allowed-signers file.
fn resolve_allowed_signers(config: &Config) -> Vec<String> {
    let mut signers = Vec::new();
    if let Ok(entries) = config.multivar(ALLOWED_SIGNER, None) {
        let _ = entries.for_each(|entry| {
            if let Ok(value) = entry.value() {
                push_unique(&mut signers, value.trim().to_owned());
            }
        });
    }
    if let Ok(path) = config.get_string(ALLOWED_SIGNERS_FILE)
        && let Ok(contents) = std::fs::read_to_string(expand_home(&path))
    {
        for key in parse_allowed_signers_file(&contents) {
            push_unique(&mut signers, key);
        }
    }
    signers
}

/// Extract public keys from an `ssh-keygen -Y verify` allowed-signers file.
///
/// Each line is `<principals> <key-type> <base64> [comment]`; Memory Hub
/// verifies against the key itself, so the principal column is dropped.
fn parse_allowed_signers_file(contents: &str) -> Vec<String> {
    contents
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let _principals = fields.next()?;
            let key_type = fields.next()?;
            let key_data = fields.next()?;
            key_type
                .starts_with("ssh-")
                .then(|| format!("{key_type} {key_data}"))
        })
        .collect()
}

fn push_unique(values: &mut Vec<String>, value: String) {
    if !value.is_empty() && !values.contains(&value) {
        values.push(value);
    }
}

/// Expand a leading `~/` so config values written the way Git users write them
/// resolve to a real path.
fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => dirs::home_dir().map_or_else(|| PathBuf::from(path), |home| home.join(rest)),
        None => PathBuf::from(path),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{VerifyMode, parse_allowed_signers_file, read_signing_config};

    fn repository() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git2::Repository::init(dir.path()).unwrap();
        dir
    }

    fn set(dir: &std::path::Path, key: &str, value: &str) {
        let repository = git2::Repository::open(dir).unwrap();
        let mut config = repository.config().unwrap();
        config.set_str(key, value).unwrap();
    }

    #[test]
    fn defaults_are_unsigned_and_verification_required() {
        let dir = repository();
        let config = read_signing_config(&dir.path().join(".git")).unwrap();
        assert!(!config.signs());
        assert!(config.allowed_signers.is_empty());
        assert_eq!(config.verify, VerifyMode::Required);
    }

    #[test]
    fn verify_can_be_turned_off_explicitly() {
        let dir = repository();
        set(dir.path(), "memory-hub.signing.verify", "off");
        let config = read_signing_config(&dir.path().join(".git")).unwrap();
        assert_eq!(config.verify, VerifyMode::Off);
    }

    #[test]
    fn unknown_verify_value_is_rejected() {
        let dir = repository();
        set(dir.path(), "memory-hub.signing.verify", "maybe");
        assert!(read_signing_config(&dir.path().join(".git")).is_err());
    }

    #[test]
    fn a_missing_key_file_does_not_enable_signing() {
        let dir = repository();
        set(
            dir.path(),
            "memory-hub.signing.key",
            "/nonexistent/id_ed25519",
        );
        let config = read_signing_config(&dir.path().join(".git")).unwrap();
        assert!(!config.signs());
    }

    #[test]
    fn an_existing_key_file_enables_signing() {
        let dir = repository();
        let key = dir.path().join("id_ed25519");
        std::fs::write(&key, "key material").unwrap();
        set(dir.path(), "memory-hub.signing.key", key.to_str().unwrap());
        let config = read_signing_config(&dir.path().join(".git")).unwrap();
        assert_eq!(config.key_path.as_deref(), Some(key.as_path()));
    }

    #[test]
    fn git_ssh_signing_key_is_reused() {
        let dir = repository();
        let key = dir.path().join("id_ed25519");
        std::fs::write(&key, "key material").unwrap();
        set(dir.path(), "gpg.format", "ssh");
        set(dir.path(), "user.signingkey", key.to_str().unwrap());
        let config = read_signing_config(&dir.path().join(".git")).unwrap();
        assert_eq!(config.key_path.as_deref(), Some(key.as_path()));
    }

    #[test]
    fn signing_key_is_ignored_when_git_signs_with_gpg() {
        let dir = repository();
        let key = dir.path().join("id_ed25519");
        std::fs::write(&key, "key material").unwrap();
        set(dir.path(), "user.signingkey", key.to_str().unwrap());
        let config = read_signing_config(&dir.path().join(".git")).unwrap();
        assert!(!config.signs());
    }

    #[test]
    fn allowed_signers_come_from_config_and_file() {
        let dir = repository();
        let signers = dir.path().join("allowed_signers");
        std::fs::write(
            &signers,
            "# comment\nalice@example.com ssh-ed25519 AAAAfile alice\n\n",
        )
        .unwrap();
        set(
            dir.path(),
            "memory-hub.signing.allowedSigner",
            "ssh-ed25519 AAAAconfig bob",
        );
        set(
            dir.path(),
            "memory-hub.signing.allowedSignersFile",
            signers.to_str().unwrap(),
        );
        let config = read_signing_config(&dir.path().join(".git")).unwrap();
        assert_eq!(
            config.allowed_signers,
            vec![
                "ssh-ed25519 AAAAconfig bob".to_owned(),
                "ssh-ed25519 AAAAfile".to_owned()
            ]
        );
    }

    #[test]
    fn allowed_signers_file_drops_principals_and_comments() {
        let keys = parse_allowed_signers_file(
            "# header\nalice ssh-ed25519 AAAA1 alice@host\nbob,carol ssh-rsa AAAA2\nbroken-line\n",
        );
        assert_eq!(keys, vec!["ssh-ed25519 AAAA1", "ssh-rsa AAAA2"]);
    }
}
