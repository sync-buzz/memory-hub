use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

use crate::{CURRENT_ENVELOPE_VERSION, ContractError, Envelope, FormatVersion};

const RESERVED_FIELDS: &[&str] = &[
    "envelope_version",
    "storage_id",
    "key_epoch",
    "cipher_suite",
    "nonce",
    "ciphertext",
];

/// Opaque identifier chosen by the encryption adapter. It is the only record
/// identifier allowed in encrypted tree paths.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct OpaqueStorageId(String);

impl OpaqueStorageId {
    /// Validate an opaque storage identifier.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError`] unless the value is exactly 64 lowercase hex
    /// digits.
    pub fn new(value: impl Into<String>) -> Result<Self, ContractError> {
        let value = value.into();
        let valid = value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
        if valid {
            Ok(Self(value))
        } else {
            Err(ContractError::invalid(
                "storage_id",
                "opaque storage id must contain 64 lowercase hex digits",
            ))
        }
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// A deterministic, non-speaking relative path for the Git tree.
    #[must_use]
    pub fn tree_path(&self) -> String {
        format!("records/opaque/{}/{}.record", &self.0[..2], &self.0[2..])
    }
}

impl<'de> Deserialize<'de> for OpaqueStorageId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::new(value).map_err(serde::de::Error::custom)
    }
}

/// Opaque payload reserved for a future encryption adapter. No semantic
/// record key or kind exists outside `ciphertext`.
#[derive(Clone, PartialEq, Serialize)]
pub struct EncryptedRecord {
    pub envelope_version: FormatVersion,
    pub storage_id: OpaqueStorageId,
    pub key_epoch: u32,
    pub cipher_suite: String,
    pub nonce: String,
    pub ciphertext: String,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl fmt::Debug for EncryptedRecord {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EncryptedRecord")
            .field("envelope_version", &self.envelope_version)
            .field("storage_id", &self.storage_id)
            .field("key_epoch", &self.key_epoch)
            .field("cipher_suite", &self.cipher_suite)
            .field("nonce", &"<redacted>")
            .field("ciphertext", &"<redacted>")
            .field("extension_count", &self.extensions.len())
            .finish()
    }
}

#[derive(Deserialize)]
struct RawEncryptedRecord {
    envelope_version: FormatVersion,
    storage_id: OpaqueStorageId,
    key_epoch: u32,
    cipher_suite: String,
    nonce: String,
    ciphertext: String,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

impl EncryptedRecord {
    /// Validate the non-secret encryption header and opaque payload presence.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError`] for an incompatible envelope major, zero key
    /// epoch, an empty cipher suite or ciphertext, or an empty nonce when the
    /// cipher suite does not manage nonces internally (e.g. age).
    pub fn validate(&self) -> Result<(), ContractError> {
        self.envelope_version
            .require_major("envelope_version", CURRENT_ENVELOPE_VERSION.major)?;
        if self.key_epoch == 0 {
            return Err(ContractError::invalid(
                "key_epoch",
                "key epoch must be greater than zero",
            ));
        }
        if self.cipher_suite.is_empty() {
            return Err(ContractError::invalid(
                "cipher_suite",
                "value must not be empty",
            ));
        }
        // age manages nonces internally — empty nonce is valid for age-v1.
        let nonce_managed_internally = self.cipher_suite == "age-v1";
        if !nonce_managed_internally && self.nonce.is_empty() {
            return Err(ContractError::invalid(
                "nonce",
                "value must not be empty for this cipher suite",
            ));
        }
        if self.ciphertext.is_empty() {
            return Err(ContractError::invalid(
                "ciphertext",
                "value must not be empty",
            ));
        }
        if let Some(field) = self
            .extensions
            .keys()
            .find(|field| RESERVED_FIELDS.contains(&field.as_str()))
        {
            return Err(ContractError::invalid(
                format!("extensions.{field}"),
                "extension collides with a reserved encrypted-record field",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn tree_path(&self) -> String {
        self.storage_id.tree_path()
    }
}

impl<'de> Deserialize<'de> for EncryptedRecord {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawEncryptedRecord::deserialize(deserializer)?;
        let record = Self {
            envelope_version: raw.envelope_version,
            storage_id: raw.storage_id,
            key_epoch: raw.key_epoch,
            cipher_suite: raw.cipher_suite,
            nonce: raw.nonce,
            ciphertext: raw.ciphertext,
            extensions: raw.extensions,
        };
        record.validate().map_err(serde::de::Error::custom)?;
        Ok(record)
    }
}

/// Durable representation discriminator. Store code can accept this now
/// without committing encryption details or semantic names to tree layout.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "representation", rename_all = "snake_case")]
pub enum StoredRecord {
    Plaintext { envelope: Box<Envelope> },
    Encrypted { encrypted: EncryptedRecord },
}

impl StoredRecord {
    /// Validate the selected durable representation before writing it.
    ///
    /// # Errors
    ///
    /// Returns the validation error from the plaintext envelope or encrypted
    /// representation.
    pub fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Plaintext { envelope } => envelope.validate(),
            Self::Encrypted { encrypted } => encrypted.validate(),
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use super::{EncryptedRecord, OpaqueStorageId};
    use crate::CURRENT_ENVELOPE_VERSION;

    #[test]
    fn encrypted_tree_path_cannot_reveal_a_semantic_key() {
        let record = EncryptedRecord {
            envelope_version: CURRENT_ENVELOPE_VERSION,
            storage_id: OpaqueStorageId::new(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .unwrap(),
            key_epoch: 1,
            cipher_suite: "reserved-suite".into(),
            nonce: "reserved-nonce".into(),
            ciphertext: "contains architecture/secrets only after encryption".into(),
            extensions: BTreeMap::new(),
        };

        let path = record.tree_path();
        assert_eq!(
            path,
            "records/opaque/01/23456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.record"
        );
        assert!(!path.contains("architecture"));
        record.validate().unwrap();
    }

    #[test]
    fn age_v1_allows_empty_nonce() {
        let record = EncryptedRecord {
            envelope_version: CURRENT_ENVELOPE_VERSION,
            storage_id: OpaqueStorageId::new(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .unwrap(),
            key_epoch: 1,
            cipher_suite: "age-v1".into(),
            nonce: String::new(),
            ciphertext: "base64-encoded-age-ciphertext".into(),
            extensions: BTreeMap::new(),
        };
        record.validate().unwrap();
    }

    #[test]
    fn non_age_suite_rejects_empty_nonce() {
        let record = EncryptedRecord {
            envelope_version: CURRENT_ENVELOPE_VERSION,
            storage_id: OpaqueStorageId::new(
                "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
            )
            .unwrap(),
            key_epoch: 1,
            cipher_suite: "xchacha20-poly1305-v1".into(),
            nonce: String::new(),
            ciphertext: "ciphertext".into(),
            extensions: BTreeMap::new(),
        };
        assert!(record.validate().is_err());
    }
}
