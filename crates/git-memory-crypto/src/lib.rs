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
pub type Recipient = Box<dyn age::Recipient>;

/// An identity that can decrypt data — SSH private key or age-native X25519.
pub type Identity = Box<dyn age::Identity>;

/// Encrypt plaintext to multiple recipients using age.
///
/// Every recipient will be able to independently decrypt the result.
///
/// # Errors
///
/// Returns [`CryptoError`] if encryption fails.
pub fn encrypt(plaintext: &[u8], recipients: &[Recipient]) -> Result<Vec<u8>, CryptoError> {
    let encryptor = Encryptor::with_recipients(recipients.iter().map(std::convert::AsRef::as_ref))
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
    let mut reader = decryptor.decrypt(identities.iter().map(std::convert::AsRef::as_ref))?;
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
    let key_data = std::fs::read_to_string(path)
        .map_err(|e| CryptoError::Key(format!("read SSH key {}: {e}", path.display())))?;
    let identity = age::ssh::Identity::from_buffer(
        std::io::BufReader::new(key_data.as_bytes()),
        Some(path.to_string_lossy().into_owned()),
    )
    .map_err(|e| CryptoError::Key(format!("parse SSH key: {e}")))?;
    Ok(Box::new(identity))
}

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
    fn key_fingerprint_is_stable() {
        let key = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5 test@host";
        let fp1 = key_fingerprint(key);
        let fp2 = key_fingerprint(key);
        assert_eq!(fp1, fp2);
        assert!(fp1.starts_with("sha256:"));
    }
}
