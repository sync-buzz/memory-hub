mod error;

pub use error::CryptoError;

use std::io::{Read, Write};
use std::path::Path;

use age::{Decryptor, Encryptor};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use sha2::{Digest, Sha256};

/// Cipher suite identifier stored in `EncryptedRecord`.
pub const CIPHER_SUITE: &str = "age-v1";

/// A recipient that can decrypt data — SSH public key or age-native X25519.
///
/// `Send + Sync` is part of the alias because everything holding one — a store,
/// a session — inherits those bounds, and a store that is not `Send` cannot be
/// owned by an async task or shared across threads by an embedding host.
pub type Recipient = Box<dyn age::Recipient + Send + Sync>;

/// An identity that can decrypt data — SSH private key or age-native X25519.
///
/// Carries the same `Send + Sync` bounds as [`Recipient`], and for the same
/// reason: it is what makes `EncryptedStore` shareable.
pub type Identity = Box<dyn age::Identity + Send + Sync>;

/// Encrypt plaintext to multiple recipients using age.
///
/// Every recipient will be able to independently decrypt the result.
///
/// # Errors
///
/// Returns [`CryptoError`] if encryption fails.
pub fn encrypt(plaintext: &[u8], recipients: &[Recipient]) -> Result<Vec<u8>, CryptoError> {
    // The `Send + Sync` on our aliases is for our own callers; age wants the
    // bare trait object, so the bounds are dropped at the call.
    let encryptor =
        Encryptor::with_recipients(recipients.iter().map(|r| r.as_ref() as &dyn age::Recipient))
            .map_err(|_| CryptoError::Encrypt("no recipients provided".into()))?;

    let mut ciphertext = Vec::new();
    let mut writer = encryptor.wrap_output(&mut ciphertext)?;
    writer.write_all(plaintext)?;
    writer.finish()?;
    Ok(ciphertext)
}

/// Decrypt age ciphertext with any of the provided identities.
///
/// # Errors
///
/// Returns [`CryptoError`] if no identity can decrypt, or the ciphertext is
/// corrupt/tampered.
pub fn decrypt(ciphertext: &[u8], identities: &[Identity]) -> Result<Vec<u8>, CryptoError> {
    let decryptor = Decryptor::new(ciphertext)?;
    let mut reader = decryptor.decrypt(
        identities
            .iter()
            .map(|identity| identity.as_ref() as &dyn age::Identity),
    )?;
    let mut plaintext = Vec::new();
    reader.read_to_end(&mut plaintext)?;
    Ok(plaintext)
}

/// Encrypt and base64-encode for storage in `EncryptedRecord.ciphertext`.
///
/// # Errors
///
/// Returns [`CryptoError`] if encryption fails.
pub fn encrypt_b64(plaintext: &[u8], recipients: &[Recipient]) -> Result<String, CryptoError> {
    let ct = encrypt(plaintext, recipients)?;
    Ok(BASE64.encode(&ct))
}

/// Decrypt a base64-encoded age ciphertext.
///
/// # Errors
///
/// Returns [`CryptoError`] if decryption fails.
pub fn decrypt_b64(ciphertext_b64: &str, identities: &[Identity]) -> Result<Vec<u8>, CryptoError> {
    let ciphertext = BASE64
        .decode(ciphertext_b64)
        .map_err(|e| CryptoError::Decrypt(format!("base64 decode: {e}")))?;
    decrypt(&ciphertext, identities)
}

/// Load an SSH private key from a file as an age identity.
///
/// Supports OpenSSH format (`id_ed25519`, `id_rsa`). Encrypted keys require
/// a passphrase via `ssh-agent` (not supported by age) or an unencrypted key
/// file.
///
/// # Errors
///
/// Returns [`CryptoError`] if the file cannot be read or the key format is
/// unsupported.
pub fn load_ssh_identity(path: &Path) -> Result<Identity, CryptoError> {
    let key_data = read_key_file(path)?;
    parse_ssh_identity(&key_data, path)
}

/// Load any supported private key file as an age identity.
///
/// Accepts both identity formats Memory Hub issues:
///
/// - an OpenSSH private key (`id_ed25519`, `id_rsa`) — the everyday path;
/// - an age-native X25519 secret key (`AGE-SECRET-KEY-1…`) — the backup
///   identity returned by `memory_init_encrypted`, which is the recovery path
///   when the SSH key is lost.
///
/// The format is detected from the file content, not from its name: an age key
/// file is recognised by its `AGE-SECRET-KEY-1` line (comment lines starting
/// with `#` are skipped), anything else is parsed as OpenSSH.
///
/// # Errors
///
/// Returns [`CryptoError`] if the file cannot be read or matches neither
/// format.
pub fn load_identity(path: &Path) -> Result<Identity, CryptoError> {
    let key_data = read_key_file(path)?;
    match find_age_secret_key(&key_data) {
        Some(secret) => {
            let identity: age::x25519::Identity = secret
                .parse()
                .map_err(|e| CryptoError::Key(format!("parse age identity: {e}")))?;
            Ok(Box::new(identity))
        }
        None => parse_ssh_identity(&key_data, path),
    }
}

/// Upper bound for a private key file. An OpenSSH key is a few kilobytes and an
/// age identity a single line; anything larger is not a key, and reading it
/// would only give a caller a way to pull an arbitrary file into memory.
const MAX_KEY_FILE_BYTES: u64 = 64 * 1024;

fn read_key_file(path: &Path) -> Result<String, CryptoError> {
    // A named pipe would block here forever and a device file would stream
    // without end, so the identity must be an ordinary file before it is read.
    let metadata = std::fs::metadata(path)
        .map_err(|e| CryptoError::Key(format!("read key {}: {e}", path.display())))?;
    if !metadata.is_file() {
        return Err(CryptoError::Key(format!(
            "read key {}: not a regular file",
            path.display()
        )));
    }
    if metadata.len() > MAX_KEY_FILE_BYTES {
        return Err(CryptoError::Key(format!(
            "read key {}: {} bytes exceeds the {MAX_KEY_FILE_BYTES} byte limit for a key file",
            path.display(),
            metadata.len()
        )));
    }
    std::fs::read_to_string(path)
        .map_err(|e| CryptoError::Key(format!("read key {}: {e}", path.display())))
}

fn parse_ssh_identity(key_data: &str, path: &Path) -> Result<Identity, CryptoError> {
    let identity = age::ssh::Identity::from_buffer(
        std::io::BufReader::new(key_data.as_bytes()),
        Some(path.to_string_lossy().into_owned()),
    )
    .map_err(|e| CryptoError::Key(format!("parse SSH key: {e}")))?;
    Ok(Box::new(identity))
}

/// Find the age secret key line in a key file, skipping `#` comment lines that
/// `age-keygen` writes above it.
fn find_age_secret_key(key_data: &str) -> Option<&str> {
    key_data.lines().map(str::trim).find(|line| {
        line.len() >= AGE_SECRET_KEY_PREFIX.len()
            && line[..AGE_SECRET_KEY_PREFIX.len()].eq_ignore_ascii_case(AGE_SECRET_KEY_PREFIX)
    })
}

const AGE_SECRET_KEY_PREFIX: &str = "AGE-SECRET-KEY-1";

/// Parse an SSH public key string (e.g. `ssh-ed25519 AAAA... user@host`)
/// as an age recipient.
///
/// # Errors
///
/// Returns [`CryptoError`] if the string is not a valid SSH public key.
pub fn parse_ssh_recipient(key_str: &str) -> Result<Recipient, CryptoError> {
    let recipient: age::ssh::Recipient = key_str
        .parse()
        .map_err(|e| CryptoError::Key(format!("parse SSH recipient: {e:?}")))?;
    Ok(Box::new(recipient))
}

/// Parse an age-native X25519 public key string (`age1...`) as a recipient.
///
/// # Errors
///
/// Returns [`CryptoError`] if the string is not a valid age recipient.
pub fn parse_x25519_recipient(key_str: &str) -> Result<Recipient, CryptoError> {
    let recipient: age::x25519::Recipient = key_str
        .parse()
        .map_err(|e| CryptoError::Key(format!("parse X25519 recipient: {e:?}")))?;
    Ok(Box::new(recipient))
}

/// Parse a recipient from a public key string, auto-detecting SSH vs X25519.
///
/// Tries SSH format first (for `ssh-ed25519`/`ssh-rsa` prefixes), then
/// age-native X25519 (`age1...`). Returns a combined error if neither works.
///
/// # Errors
///
/// Returns [`CryptoError`] if the string is neither a valid SSH nor X25519 key.
pub fn parse_recipient(key_str: &str) -> Result<Recipient, CryptoError> {
    if key_str.starts_with("ssh-") {
        return parse_ssh_recipient(key_str);
    }
    if key_str.starts_with("age1") {
        return parse_x25519_recipient(key_str);
    }
    // Unknown prefix — try both and return the most helpful error.
    let ssh_err = match parse_ssh_recipient(key_str) {
        Ok(r) => return Ok(r),
        Err(e) => e,
    };
    let x25519_err = match parse_x25519_recipient(key_str) {
        Ok(r) => return Ok(r),
        Err(e) => e,
    };
    Err(CryptoError::Key(format!(
        "not a valid SSH or X25519 key — SSH: {ssh_err}, X25519: {x25519_err}"
    )))
}

/// Generate an age-native X25519 identity (backup/recovery keypair).
///
/// This identity does not depend on SSH and can be used as a backup
/// recipient so the owner can always decrypt even if they lose their
/// SSH key.
#[must_use]
pub fn generate_backup_identity() -> (age::x25519::Identity, age::x25519::Recipient) {
    let identity = age::x25519::Identity::generate();
    let recipient = identity.to_public();
    (identity, recipient)
}

/// Serialize a backup X25519 identity to a BECH32 secret-key string
/// (`AGE-SECRET-KEY-1...`).
///
/// The caller MUST persist the returned string in a safe location outside
/// the repository — it is the recovery path if the owner loses their SSH
/// key.
#[must_use]
pub fn backup_identity_to_string(identity: &age::x25519::Identity) -> String {
    use secrecy::ExposeSecret;
    identity.to_string().expose_secret().to_string()
}

/// Generate a random 64-hex-digit opaque storage id.
///
/// # Errors
///
/// Returns [`CryptoError`] if the OS CSPRNG is unavailable.
pub fn generate_storage_id() -> Result<String, CryptoError> {
    let mut buf = [0u8; 32];
    getrandom::fill(&mut buf).map_err(|e| CryptoError::Key(format!("CSPRNG: {e}")))?;
    Ok(hex_lower(&buf))
}

/// Derive a non-secret fingerprint from a public key string for diagnostics.
#[must_use]
pub fn key_fingerprint(key_str: &str) -> String {
    let digest = Sha256::digest(key_str.as_bytes());
    format!("sha256:{}", hex_lower(&digest[..8]))
}

/// SSH commit signer that shells out to `ssh-keygen -Y sign`.
///
/// Produces an armored SSH signature (`-----BEGIN SSH SIGNATURE-----`)
/// suitable for Git's `gpgsig` header. The private key file must be
/// unencrypted or the key must be in `ssh-agent`.
#[derive(Debug)]
pub struct SshSigner {
    key_path: std::path::PathBuf,
}

impl SshSigner {
    /// Create a signer from an SSH private key file path.
    #[must_use]
    pub fn new(key_path: impl Into<std::path::PathBuf>) -> Self {
        Self {
            key_path: key_path.into(),
        }
    }

    /// Create a signer from the default SSH key (`~/.ssh/id_ed25519`).
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError`] if the home directory cannot be resolved
    /// or the default key file does not exist.
    pub fn default_key() -> Result<Self, CryptoError> {
        let home = dirs::home_dir().ok_or_else(|| {
            CryptoError::Key("cannot resolve home directory for default SSH key".into())
        })?;
        let key_path = home.join(".ssh").join("id_ed25519");
        if !key_path.is_file() {
            return Err(CryptoError::Key(format!(
                "default SSH key not found at {}",
                key_path.display()
            )));
        }
        Ok(Self { key_path })
    }

    /// Sign arbitrary data using `ssh-keygen -Y sign -n git`.
    ///
    /// Returns the armored signature string.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError`] if `ssh-keygen` is unavailable, fails, or
    /// produces an unreadable signature.
    pub fn sign(&self, data: &[u8]) -> Result<String, CryptoError> {
        use std::io::Write;
        use std::process::{Command, Stdio};

        // ssh-keygen -Y sign writes the signature to <file>.sig. We create
        // a private temp dir and use a random filename for the data so the
        // .sig path is not predictable by an attacker.
        let temp = private_tempdir()?;
        let data_filename = format!("commit-{}", random_hex_suffix()?);
        let data_path = temp.path().join(&data_filename);
        {
            let mut file = std::fs::File::create(&data_path)
                .map_err(|e| CryptoError::Io(format!("create temp file: {e}")))?;
            file.write_all(data)
                .map_err(|e| CryptoError::Io(format!("write temp file: {e}")))?;
        }

        let output = Command::new("ssh-keygen")
            .args(["-Y", "sign", "-f"])
            .arg(&self.key_path)
            .args(["-n", "git"])
            .arg(&data_path)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .map_err(|e| CryptoError::Key(format!("spawn ssh-keygen: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(CryptoError::Key(format!(
                "ssh-keygen -Y sign failed: {stderr}"
            )));
        }

        let sig_path = temp.path().join(format!("{data_filename}.sig"));
        let signature = std::fs::read_to_string(&sig_path)
            .map_err(|e| CryptoError::Key(format!("read signature file: {e}")))?
            .trim()
            .to_string();

        if signature.is_empty() {
            return Err(CryptoError::Key(
                "ssh-keygen produced an empty signature".into(),
            ));
        }
        Ok(signature)
    }
}

/// Verify an SSH signature against a public key string using `ssh-keygen -Y
/// verify -n git`.
///
/// `signature` must be the armored `-----BEGIN SSH SIGNATURE-----` block as
/// stored in Git's `gpgsig` header. `public_key` must be a single-line
/// OpenSSH public key (`ssh-ed25519 AAAA... user@host`).
///
/// The allowed signers file is written with identity `memory-recipient`
/// prepended, as required by `ssh-keygen -Y verify -I`.
///
/// # Errors
///
/// Returns [`CryptoError`] if `ssh-keygen` is unavailable, fails, or the
/// signature does not match.
pub fn verify_ssh_signature(
    data: &[u8],
    signature: &str,
    public_key: &str,
) -> Result<(), CryptoError> {
    use std::io::Write;
    use std::process::{Command, Stdio};

    let temp = private_tempdir()?;
    let data_path = temp.path().join("commit-data");
    let sig_path = temp.path().join("commit-data.sig");
    let allowed = temp.path().join("allowed");

    {
        let mut file = std::fs::File::create(&data_path)
            .map_err(|e| CryptoError::Io(format!("create temp file: {e}")))?;
        file.write_all(data)
            .map_err(|e| CryptoError::Io(format!("write temp file: {e}")))?;
    }
    std::fs::write(&sig_path, signature.trim())
        .map_err(|e| CryptoError::Io(format!("write signature file: {e}")))?;
    // Allowed signers format: <identity> <key-type> <base64-key> [<comment>]
    // ssh-keygen -Y verify -I looks up the identity in this file.
    std::fs::write(&allowed, format!("memory-recipient {public_key}\n"))
        .map_err(|e| CryptoError::Io(format!("write allowed signers: {e}")))?;

    let output = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-n", "git"])
        .args(["-f", &data_path.to_string_lossy()])
        .args(["-I", "memory-recipient"])
        .args(["-s", &sig_path.to_string_lossy()])
        .arg("-O")
        .arg(format!(
            "allowed_signers_file={}",
            allowed.to_string_lossy()
        ))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| CryptoError::Key(format!("spawn ssh-keygen verify: {e}")))?;

    if output.status.success() {
        Ok(())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        Err(CryptoError::Key(format!(
            "signature verification failed: {stderr}"
        )))
    }
}

/// Create a private temporary directory for signature material.
///
/// On Unix the directory is created with mode `0700`, so no other local user
/// can read the signed payload or win a race for the predictable `.sig` path.
/// Windows has no mode bits; the per-user temporary directory is already
/// restricted to its owner by the default ACL.
#[cfg(unix)]
fn private_tempdir() -> Result<tempfile::TempDir, CryptoError> {
    let mut builder = tempfile::Builder::new();
    builder.permissions(std::os::unix::fs::PermissionsExt::from_mode(0o700));
    builder
        .tempdir()
        .map_err(|e| CryptoError::Io(e.to_string()))
}

#[cfg(not(unix))]
fn private_tempdir() -> Result<tempfile::TempDir, CryptoError> {
    tempfile::Builder::new()
        .tempdir()
        .map_err(|e| CryptoError::Io(e.to_string()))
}

/// Generate a short random hex suffix for unpredictable temp filenames.
fn random_hex_suffix() -> Result<String, CryptoError> {
    let mut buf = [0u8; 8];
    getrandom::fill(&mut buf).map_err(|e| CryptoError::Key(format!("CSPRNG: {e}")))?;
    Ok(hex_lower(&buf))
}

/// Encode bytes as lowercase hex string.
#[must_use]
pub fn hex_lower(bytes: &[u8]) -> String {
    use std::fmt::Write;
    bytes
        .iter()
        .fold(String::with_capacity(bytes.len() * 2), |mut acc, b| {
            let _ = write!(acc, "{b:02x}");
            acc
        })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn backup_pair() -> (age::x25519::Identity, age::x25519::Recipient) {
        generate_backup_identity()
    }

    fn recipients_from_pairs(
        pairs: &[&(age::x25519::Identity, age::x25519::Recipient)],
    ) -> Vec<Recipient> {
        pairs
            .iter()
            .map(|(_, r)| Box::new(r.clone()) as Recipient)
            .collect()
    }

    fn identities_from_pairs(
        pairs: &[&(age::x25519::Identity, age::x25519::Recipient)],
    ) -> Vec<Identity> {
        pairs
            .iter()
            .map(|(i, _)| Box::new(i.clone()) as Identity)
            .collect()
    }

    #[test]
    fn encrypt_decrypt_round_trip() {
        let pair = backup_pair();
        let recipients = recipients_from_pairs(&[&pair]);
        let identities = identities_from_pairs(&[&pair]);

        let plaintext = b"Remember the seam.";
        let ct = encrypt(plaintext, &recipients).unwrap();
        let pt = decrypt(&ct, &identities).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn multiple_recipients_can_decrypt() {
        let alice = backup_pair();
        let bob = backup_pair();
        let recipients = recipients_from_pairs(&[&alice, &bob]);

        let plaintext = b"shared secret";
        let ct = encrypt(plaintext, &recipients).unwrap();

        let alice_ids = identities_from_pairs(&[&alice]);
        let bob_ids = identities_from_pairs(&[&bob]);

        assert_eq!(decrypt(&ct, &alice_ids).unwrap(), plaintext);
        assert_eq!(decrypt(&ct, &bob_ids).unwrap(), plaintext);
    }

    #[test]
    fn removed_recipient_cannot_decrypt() {
        let alice = backup_pair();
        let bob = backup_pair();

        // Encrypt to both
        let recipients = recipients_from_pairs(&[&alice, &bob]);
        let ct_both = encrypt(b"shared", &recipients).unwrap();

        // Encrypt to only alice
        let recipients_alice = recipients_from_pairs(&[&alice]);
        let ct_alice = encrypt(b"shared", &recipients_alice).unwrap();

        let bob_ids = identities_from_pairs(&[&bob]);

        // Bob can decrypt the old ciphertext
        assert!(decrypt(&ct_both, &bob_ids).is_ok());

        // Bob cannot decrypt the new ciphertext (only alice)
        assert!(decrypt(&ct_alice, &bob_ids).is_err());
    }

    #[test]
    fn b64_round_trip() {
        let pair = backup_pair();
        let recipients = recipients_from_pairs(&[&pair]);
        let identities = identities_from_pairs(&[&pair]);

        let plaintext = b"base64 encoded ciphertext";
        let ct_b64 = encrypt_b64(plaintext, &recipients).unwrap();
        let pt = decrypt_b64(&ct_b64, &identities).unwrap();
        assert_eq!(pt, plaintext);
    }

    #[test]
    fn storage_id_is_64_hex_chars() {
        let id = generate_storage_id().unwrap();
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn storage_ids_are_unique() {
        let id1 = generate_storage_id().unwrap();
        let id2 = generate_storage_id().unwrap();
        assert_ne!(id1, id2);
    }

    #[test]
    fn tampered_ciphertext_is_rejected() {
        let pair = backup_pair();
        let recipients = recipients_from_pairs(&[&pair]);
        let identities = identities_from_pairs(&[&pair]);

        let ct = encrypt(b"secret", &recipients).unwrap();
        let mut tampered = ct.clone();
        tampered[0] ^= 0xff;

        assert!(decrypt(&tampered, &identities).is_err());
    }

    #[test]
    fn backup_identity_file_loads_and_decrypts() {
        let (identity, recipient) = generate_backup_identity();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("backup.key");
        // age-keygen writes two comment lines above the secret key.
        std::fs::write(
            &path,
            format!(
                "# created: 2026-08-16T00:00:00Z\n# public key: {recipient}\n{}\n",
                backup_identity_to_string(&identity)
            ),
        )
        .unwrap();

        let loaded = load_identity(&path).unwrap();
        let recipients = vec![Box::new(recipient) as Recipient];
        let ciphertext = encrypt(b"recovered", &recipients).unwrap();
        assert_eq!(
            decrypt(&ciphertext, std::slice::from_ref(&loaded)).unwrap(),
            b"recovered"
        );
    }

    #[test]
    fn load_identity_rejects_a_file_in_neither_format() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("garbage.key");
        std::fs::write(&path, "not a key at all\n").unwrap();
        assert!(load_identity(&path).is_err());
    }

    #[test]
    fn key_fingerprint_is_stable() {
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 test@host";
        let fp1 = key_fingerprint(key);
        let fp2 = key_fingerprint(key);
        assert_eq!(fp1, fp2);
        assert!(fp1.starts_with("sha256:"));
    }
}
