use std::collections::BTreeMap;

use git_memory_core::{
    CURRENT_ENVELOPE_VERSION, EncryptedRecord, Envelope, OpaqueStorageId, StoredRecord,
};
use git_memory_crypto::{
    CIPHER_SUITE, CryptoError, Identity, Recipient, backup_identity_to_string, decrypt_b64,
    encrypt_b64, generate_backup_identity, generate_storage_id, hex_lower,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    ApplyResult, CommitSigner, GitStore, Operation, RecordId, Revision, StoreError, Transaction,
};

/// Deterministic storage id for the encrypted manifest blob.
#[allow(clippy::expect_used)]
fn manifest_storage_id() -> OpaqueStorageId {
    let digest = Sha256::digest(b"git-memory-manifest");
    OpaqueStorageId::new(hex_lower(&digest)).expect("sha256 digest is 32 bytes = 64 hex chars")
}

/// Generate a random opaque storage id for a new record.
#[allow(clippy::expect_used)]
fn generate_opaque_id() -> Result<OpaqueStorageId, CryptoError> {
    let hex = generate_storage_id()?;
    Ok(OpaqueStorageId::new(hex).expect("32 bytes = 64 hex chars"))
}

/// A recipient entry in the manifest — public key + metadata.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RecipientEntry {
    /// Public key string: `ssh-ed25519 AAAA...` or `age1...`.
    pub public_key: String,
    /// Type tag: `ssh` or `x25519`.
    pub key_type: String,
    /// Human-readable label (GitHub username, device name, etc.).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Result of initializing an encrypted store.
///
/// Contains the backup X25519 identity string (`AGE-SECRET-KEY-1...`)
/// generated during `init()`. The caller MUST persist this in a safe
/// location — it is the recovery path if the owner loses their SSH key.
#[derive(Debug)]
#[must_use]
pub struct InitResult {
    /// BECH32-encoded backup X25519 private key.
    pub backup_identity: String,
}

/// Plaintext manifest content before age encryption.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Manifest {
    version: u32,
    recipients: Vec<RecipientEntry>,
    records: BTreeMap<String, ManifestEntry>,
}

impl Manifest {
    fn new(recipients: Vec<RecipientEntry>) -> Self {
        Self {
            version: 1,
            recipients,
            records: BTreeMap::new(),
        }
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
struct ManifestEntry {
    storage_id: String,
    kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(default)]
    links: serde_json::Value,
    #[serde(default)]
    source_paths: serde_json::Value,
    #[serde(default)]
    archive: serde_json::Value,
    #[serde(default)]
    freshness: serde_json::Value,
    content_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    profile: Option<serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    envelope_minor: Option<u32>,
}

/// Lock state for an encrypted project.
enum LockState {
    /// Age identity is loaded and ready for encryption/decryption.
    Unlocked { identity: Identity },
    /// No identity in memory; only safe diagnostics available.
    Locked,
}

/// Encrypted store wrapper around [`GitStore`] using age encryption.
///
/// Record content and metadata are age-encrypted before reaching the Git
/// tree. The manifest — mapping semantic keys to opaque storage ids, holding
/// titles, kinds, links, paths, and the recipients list — is itself an
/// age-encrypted blob stored alongside records.
pub struct EncryptedStore {
    store: GitStore,
    state: LockState,
}

impl EncryptedStore {
    /// Open a project in locked encrypted mode.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the Git repository cannot be opened.
    pub fn open_locked(project: impl AsRef<std::path::Path>) -> Result<Self, StoreError> {
        let store = GitStore::open(project)?;
        Ok(Self {
            store,
            state: LockState::Locked,
        })
    }

    /// Attach a commit signer so every subsequent commit on `refs/memory/*`
    /// carries an SSH signature for integrity.
    #[must_use]
    pub fn with_signer(mut self, signer: std::sync::Arc<dyn CommitSigner>) -> Self {
        self.store = self.store.with_signer(signer);
        self
    }

    /// Check whether a manifest already exists in the current snapshot.
    fn has_manifest(&self) -> Result<bool, StoreError> {
        let revision = self.current_revision()?;
        let id = RecordId::opaque(manifest_storage_id());
        Ok(self.store.read_record_pub(&revision, &id)?.is_some())
    }

    /// Unlock the store with an age identity (SSH private key or X25519).
    ///
    /// If no manifest exists yet (first-time setup), any identity is
    /// accepted. If a manifest exists, the identity must be able to
    /// decrypt it — otherwise an error is returned.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the identity cannot decrypt an existing
    /// manifest.
    pub fn unlock(&mut self, identity: Identity) -> Result<(), StoreError> {
        if self.has_manifest()? {
            // Manifest exists — verify the identity can decrypt it.
            let revision = self.current_revision()?;
            self.read_manifest(&identity, &revision)?;
        }
        // No manifest yet (first write) or manifest decrypted successfully.
        self.state = LockState::Unlocked { identity };
        Ok(())
    }

    /// Initialize the encrypted store with the first recipient.
    ///
    /// Generates a backup X25519 identity and adds it to the recipients
    /// list so the owner can recover if they lose their SSH key. The
    /// backup private key is returned in [`InitResult`] — the caller MUST
    /// persist it in a safe location outside the repository.
    ///
    /// Creates an initial empty manifest encrypted to all recipients
    /// (including the backup) and commits it. Must be called before
    /// `apply()`.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the store is locked, encryption fails, or
    /// a manifest already exists.
    pub fn init(&self, mut recipients: Vec<RecipientEntry>) -> Result<InitResult, StoreError> {
        let identity = self.require_unlocked()?;

        if self.has_manifest()? {
            return Err(StoreError::new(
                crate::StoreErrorKind::InvalidArgument,
                "manifest already exists — use add_recipient to add members",
                serde_json::json!({}),
            ));
        }

        if recipients.is_empty() {
            return Err(StoreError::new(
                crate::StoreErrorKind::InvalidArgument,
                "at least one user recipient is required to initialize (backup is added automatically)",
                serde_json::json!({}),
            ));
        }

        // Generate a backup X25519 identity for recovery.
        let (backup_identity, backup_recipient) = generate_backup_identity();
        let backup_key_string = backup_identity_to_string(&backup_identity);
        let backup_recipient_string = backup_recipient.to_string();
        recipients.push(RecipientEntry {
            public_key: backup_recipient_string,
            key_type: "x25519".to_string(),
            label: Some("backup".to_string()),
        });

        let manifest = Manifest::new(recipients);
        let parsed_recipients = parse_recipients(&manifest.recipients)?;

        let manifest_blob = Self::encrypt_manifest(&manifest, &parsed_recipients)?;
        let revision = self.current_revision()?;
        self.store.apply(&Transaction {
            id: format!("init-{}", random_suffix()),
            expected_revision: revision,
            operations: vec![Operation::Put {
                record: StoredRecord::Encrypted {
                    encrypted: manifest_blob,
                },
            }],
        })?;
        let _ = identity;
        Ok(InitResult {
            backup_identity: backup_key_string,
        })
    }

    /// Lock the store by dropping the in-memory identity.
    pub fn lock(&mut self) {
        self.state = LockState::Locked;
    }

    /// Return `true` if the store is unlocked.
    #[must_use]
    pub fn is_unlocked(&self) -> bool {
        matches!(self.state, LockState::Unlocked { .. })
    }

    /// Return the current revision.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the staged ref cannot be read.
    pub fn current_revision(&self) -> Result<Revision, StoreError> {
        self.store.current().map(|s| s.revision)
    }

    /// Read and decrypt a record by its semantic key.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the store is locked, the key is not found,
    /// or decryption fails.
    pub fn get(&self, key: &str) -> Result<Option<Envelope>, StoreError> {
        let identity = self.require_unlocked()?;
        let revision = self.current_revision()?;
        let manifest = self.read_manifest(identity, &revision)?;

        let Some(entry) = manifest.records.get(key) else {
            return Ok(None);
        };
        let storage_id = parse_storage_id(&entry.storage_id)?;
        let id = RecordId::opaque(storage_id);
        let Some(stored) = self.store.read_record_pub(&revision, &id)? else {
            return Ok(None);
        };
        let ciphertext_b64 = extract_ciphertext(&stored, key)?;
        let content = decrypt_b64(&ciphertext_b64, std::slice::from_ref(identity))
            .map_err(crypto_to_store_error)?;
        Ok(Some(reconstruct_envelope(key, entry, content)?))
    }

    /// Apply a batch of envelope put/delete operations atomically.
    ///
    /// Each envelope is age-encrypted to all recipients in the manifest
    /// before reaching the Git tree. The manifest is updated in the same
    /// transaction only if records changed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the store is locked, encryption fails, or
    /// the store transaction conflicts.
    pub fn apply(
        &self,
        transaction_id: &str,
        expected_revision: Revision,
        puts: &[(&str, Envelope)],
        deletes: &[&str],
    ) -> Result<ApplyResult, StoreError> {
        let identity = self.require_unlocked()?;
        let revision = expected_revision.clone();

        let mut manifest = self.read_manifest(identity, &revision)?;

        let recipients = parse_recipients(&manifest.recipients)?;
        if recipients.is_empty() {
            return Err(StoreError::new(
                crate::StoreErrorKind::InvalidArgument,
                "no recipients in manifest — run init first",
                serde_json::json!({}),
            ));
        }

        let mut operations = Vec::with_capacity(puts.len() + deletes.len() + 1);
        let mut changed = false;

        for (key, envelope) in puts {
            envelope.validate().map_err(|e| {
                StoreError::new(
                    crate::StoreErrorKind::InvalidRecord,
                    "envelope validation failed",
                    serde_json::to_value(e).unwrap_or(serde_json::Value::Null),
                )
            })?;
            let storage_id = generate_opaque_id().map_err(crypto_to_store_error)?;
            let content_bytes = envelope.content.as_bytes();
            let ciphertext_b64 =
                encrypt_b64(content_bytes, &recipients).map_err(crypto_to_store_error)?;

            // If updating an existing key, delete the old record blob.
            if let Some(old_entry) = manifest.records.get(*key)
                && let Ok(old_id) = parse_storage_id(&old_entry.storage_id)
            {
                operations.push(Operation::Delete {
                    id: RecordId::opaque(old_id),
                });
            }

            let encrypted = EncryptedRecord {
                envelope_version: CURRENT_ENVELOPE_VERSION,
                storage_id: storage_id.clone(),
                key_epoch: 1,
                cipher_suite: CIPHER_SUITE.to_owned(),
                nonce: String::new(),
                ciphertext: ciphertext_b64,
                extensions: BTreeMap::new(),
            };
            manifest
                .records
                .insert((*key).to_owned(), manifest_entry(&storage_id, envelope));
            operations.push(Operation::Put {
                record: StoredRecord::Encrypted { encrypted },
            });
            changed = true;
        }

        for key in deletes {
            if let Some(entry) = manifest.records.remove(*key)
                && let Ok(storage_id) = parse_storage_id(&entry.storage_id)
            {
                operations.push(Operation::Delete {
                    id: RecordId::opaque(storage_id),
                });
                changed = true;
            }
        }

        // Only re-encrypt and write the manifest if records changed.
        if changed {
            let manifest_blob = Self::encrypt_manifest(&manifest, &recipients)?;
            operations.push(Operation::Put {
                record: StoredRecord::Encrypted {
                    encrypted: manifest_blob,
                },
            });
        }

        self.store.apply(&Transaction {
            id: transaction_id.to_owned(),
            expected_revision,
            operations,
        })
    }

    /// Read all records, decrypting each envelope.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the store is locked or decryption fails.
    pub fn list(&self) -> Result<Vec<(String, Envelope)>, StoreError> {
        let identity = self.require_unlocked()?;
        let revision = self.current_revision()?;
        let manifest = self.read_manifest(identity, &revision)?;
        let mut result = Vec::new();
        for (key, entry) in &manifest.records {
            let storage_id = parse_storage_id(&entry.storage_id)?;
            let id = RecordId::opaque(storage_id);
            let Some(stored) = self.store.read_record_pub(&revision, &id)? else {
                continue;
            };
            let ciphertext_b64 = match stored {
                StoredRecord::Encrypted { encrypted } => encrypted.ciphertext.clone(),
                StoredRecord::Plaintext { .. } => continue,
            };
            let content = decrypt_b64(&ciphertext_b64, std::slice::from_ref(identity))
                .map_err(crypto_to_store_error)?;
            result.push((key.clone(), reconstruct_envelope(key, entry, content)?));
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(result)
    }

    /// Add a recipient to the manifest and re-encrypt all records.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the store is locked or encryption fails.
    pub fn add_recipient(&self, recipient_entry: RecipientEntry) -> Result<(), StoreError> {
        let identity = self.require_unlocked()?;
        let revision = self.current_revision()?;
        let mut manifest = self.read_manifest(identity, &revision)?;

        if manifest
            .recipients
            .iter()
            .any(|r| r.public_key == recipient_entry.public_key)
        {
            return Ok(());
        }
        manifest.recipients.push(recipient_entry);

        let recipients = parse_recipients(&manifest.recipients)?;
        self.reencrypt_all(&mut manifest, identity, &recipients, &revision)?;
        Ok(())
    }

    /// Remove a recipient from the manifest and re-encrypt all records.
    ///
    /// The removed recipient will no longer be able to decrypt new data.
    /// Old commits in Git history remain decryptable by them.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the store is locked, the recipient is not
    /// found, or encryption fails.
    pub fn remove_recipient(&self, public_key: &str) -> Result<(), StoreError> {
        let identity = self.require_unlocked()?;
        let revision = self.current_revision()?;
        let mut manifest = self.read_manifest(identity, &revision)?;

        let before = manifest.recipients.len();
        manifest.recipients.retain(|r| r.public_key != public_key);
        if manifest.recipients.len() == before {
            return Err(StoreError::new(
                crate::StoreErrorKind::InvalidArgument,
                "recipient not found in manifest",
                serde_json::json!({"public_key": public_key}),
            ));
        }
        if manifest.recipients.is_empty() {
            return Err(StoreError::new(
                crate::StoreErrorKind::InvalidArgument,
                "cannot remove the last recipient — no one would be able to decrypt",
                serde_json::json!({}),
            ));
        }

        let recipients = parse_recipients(&manifest.recipients)?;
        self.reencrypt_all(&mut manifest, identity, &recipients, &revision)?;
        Ok(())
    }

    /// List all recipients in the manifest.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the store is locked or the manifest cannot
    /// be read.
    pub fn list_recipients(&self) -> Result<Vec<RecipientEntry>, StoreError> {
        let identity = self.require_unlocked()?;
        let revision = self.current_revision()?;
        let manifest = self.read_manifest(identity, &revision)?;
        Ok(manifest.recipients)
    }

    fn require_unlocked(&self) -> Result<&Identity, StoreError> {
        match &self.state {
            LockState::Unlocked { identity } => Ok(identity),
            LockState::Locked => Err(StoreError::new(
                crate::StoreErrorKind::InvalidArgument,
                "encrypted store is locked — unlock before reading or writing",
                serde_json::json!({}),
            )),
        }
    }

    fn read_manifest(
        &self,
        identity: &Identity,
        revision: &Revision,
    ) -> Result<Manifest, StoreError> {
        let id = RecordId::opaque(manifest_storage_id());
        let Some(stored) = self.store.read_record_pub(revision, &id)? else {
            return Err(StoreError::new(
                crate::StoreErrorKind::InvalidRecord,
                "manifest not found in snapshot — run init first",
                serde_json::json!({"revision": revision}),
            ));
        };
        let ciphertext_b64 = match stored {
            StoredRecord::Encrypted { encrypted } => encrypted.ciphertext.clone(),
            StoredRecord::Plaintext { .. } => {
                return Err(StoreError::new(
                    crate::StoreErrorKind::InvalidRecord,
                    "manifest is stored as plaintext — expected encrypted",
                    serde_json::json!({}),
                ));
            }
        };
        let plaintext = decrypt_b64(&ciphertext_b64, std::slice::from_ref(identity))
            .map_err(crypto_to_store_error)?;
        serde_json::from_slice(&plaintext).map_err(|e| {
            StoreError::new(
                crate::StoreErrorKind::InvalidRecord,
                "manifest JSON is corrupt",
                serde_json::json!({"detail": e.to_string()}),
            )
        })
    }

    fn encrypt_manifest(
        manifest: &Manifest,
        recipients: &[Recipient],
    ) -> Result<EncryptedRecord, StoreError> {
        let storage_id = manifest_storage_id();
        let plaintext = serde_json::to_vec(manifest).map_err(|e| {
            StoreError::new(
                crate::StoreErrorKind::InvalidRecord,
                "serialize manifest",
                serde_json::json!({"detail": e.to_string()}),
            )
        })?;
        let ciphertext_b64 = encrypt_b64(&plaintext, recipients).map_err(crypto_to_store_error)?;
        Ok(EncryptedRecord {
            envelope_version: CURRENT_ENVELOPE_VERSION,
            storage_id,
            // key_epoch reserved for future rotation support; age-v1 does not use it.
            key_epoch: 1,
            cipher_suite: CIPHER_SUITE.to_owned(),
            // age manages nonces internally — left empty for age-v1.
            nonce: String::new(),
            ciphertext: ciphertext_b64,
            extensions: BTreeMap::new(),
        })
    }

    /// Re-encrypt all records with a new recipients list.
    ///
    /// Updates manifest `storage_ids` for re-encrypted records BEFORE writing
    /// the manifest blob, so the manifest always matches the tree.
    ///
    /// Uses `expected_revision` from the caller (the revision the manifest
    /// was read at) so that a concurrent write between read and re-encrypt
    /// is detected as a CAS conflict rather than silently rebasing over it.
    fn reencrypt_all(
        &self,
        manifest: &mut Manifest,
        identity: &Identity,
        recipients: &[Recipient],
        expected_revision: &Revision,
    ) -> Result<(), StoreError> {
        let mut operations = Vec::new();

        // Collect updates first to avoid mutating while iterating.
        let mut storage_id_updates: Vec<(String, String)> = Vec::new();

        for (key, entry) in &manifest.records {
            let storage_id = parse_storage_id(&entry.storage_id)?;
            let id = RecordId::opaque(storage_id.clone());
            if let Some(stored) = self.store.read_record_pub(expected_revision, &id)? {
                let ciphertext_b64 = match stored {
                    StoredRecord::Encrypted { encrypted } => encrypted.ciphertext,
                    StoredRecord::Plaintext { .. } => continue,
                };
                // Decrypt with old identity.
                let plaintext = decrypt_b64(&ciphertext_b64, std::slice::from_ref(identity))
                    .map_err(crypto_to_store_error)?;
                // Re-encrypt with new recipients.
                let new_ct_b64 =
                    encrypt_b64(&plaintext, recipients).map_err(crypto_to_store_error)?;
                let new_storage_id = generate_opaque_id().map_err(crypto_to_store_error)?;

                let encrypted = EncryptedRecord {
                    envelope_version: CURRENT_ENVELOPE_VERSION,
                    storage_id: new_storage_id.clone(),
                    key_epoch: 1,
                    cipher_suite: CIPHER_SUITE.to_owned(),
                    nonce: String::new(),
                    ciphertext: new_ct_b64,
                    extensions: BTreeMap::new(),
                };
                // Delete old blob.
                operations.push(Operation::Delete {
                    id: RecordId::opaque(storage_id),
                });
                // Put new blob.
                operations.push(Operation::Put {
                    record: StoredRecord::Encrypted { encrypted },
                });
                // Track storage_id update for manifest.
                storage_id_updates.push((key.clone(), new_storage_id.as_str().to_owned()));
            }
        }

        // Update manifest entries with new storage_ids BEFORE encrypting.
        for (key, new_id) in &storage_id_updates {
            if let Some(entry) = manifest.records.get_mut(key) {
                entry.storage_id.clone_from(new_id);
            }
        }

        // Now encrypt the updated manifest.
        let manifest_blob = Self::encrypt_manifest(manifest, recipients)?;
        operations.push(Operation::Put {
            record: StoredRecord::Encrypted {
                encrypted: manifest_blob,
            },
        });

        self.store.apply(&Transaction {
            id: format!("reencrypt-{}", random_suffix()),
            expected_revision: expected_revision.clone(),
            operations,
        })?;

        Ok(())
    }
}

/// Parse a storage id string into an `OpaqueStorageId`.
fn parse_storage_id(s: &str) -> Result<OpaqueStorageId, StoreError> {
    OpaqueStorageId::new(s).map_err(|e| {
        StoreError::new(
            crate::StoreErrorKind::InvalidRecord,
            "invalid storage_id in manifest",
            serde_json::json!({"detail": e.to_string()}),
        )
    })
}

/// Extract ciphertext from a `StoredRecord`.
fn extract_ciphertext(stored: &StoredRecord, key: &str) -> Result<String, StoreError> {
    match stored {
        StoredRecord::Encrypted { encrypted } => Ok(encrypted.ciphertext.clone()),
        StoredRecord::Plaintext { .. } => Err(StoreError::new(
            crate::StoreErrorKind::InvalidRecord,
            "expected encrypted record but found plaintext",
            serde_json::json!({"key": key}),
        )),
    }
}

/// Parse recipient entries into age recipients.
fn parse_recipients(entries: &[RecipientEntry]) -> Result<Vec<Recipient>, StoreError> {
    entries
        .iter()
        .map(|e| git_memory_crypto::parse_recipient(&e.public_key).map_err(crypto_to_store_error))
        .collect()
}

fn manifest_entry(storage_id: &OpaqueStorageId, envelope: &Envelope) -> ManifestEntry {
    ManifestEntry {
        storage_id: storage_id.as_str().to_owned(),
        kind: envelope.kind.clone(),
        title: envelope.title.clone(),
        tags: envelope.tags.clone(),
        links: serde_json::to_value(&envelope.links).unwrap_or(serde_json::Value::Null),
        source_paths: serde_json::to_value(&envelope.source_paths)
            .unwrap_or(serde_json::Value::Null),
        archive: serde_json::to_value(&envelope.archive).unwrap_or(serde_json::Value::Null),
        freshness: serde_json::to_value(&envelope.freshness).unwrap_or(serde_json::Value::Null),
        content_hash: envelope.content_hash.as_str().to_owned(),
        profile: serde_json::to_value(&envelope.profile).ok(),
        envelope_minor: Some(u32::from(envelope.envelope_version.minor)),
    }
}

/// Reconstruct an envelope from a manifest entry and decrypted content.
///
/// # Errors
///
/// Returns [`StoreError`] if the decrypted content is not valid UTF-8.
fn reconstruct_envelope(
    key: &str,
    entry: &ManifestEntry,
    content: Vec<u8>,
) -> Result<Envelope, StoreError> {
    let content_str = String::from_utf8(content).map_err(|e| {
        StoreError::new(
            crate::StoreErrorKind::InvalidRecord,
            "decrypted content is not valid UTF-8",
            serde_json::json!({"key": key, "detail": e.to_string()}),
        )
    })?;
    let mut envelope = Envelope::new(key, &entry.kind, content_str).map_err(|e| {
        StoreError::new(
            crate::StoreErrorKind::InvalidRecord,
            "failed to reconstruct envelope",
            serde_json::json!({"key": key, "detail": e.to_string()}),
        )
    })?;
    envelope.title.clone_from(&entry.title);
    envelope.tags.clone_from(&entry.tags);
    envelope.links = serde_json::from_value(entry.links.clone()).unwrap_or_default();
    envelope.source_paths = serde_json::from_value(entry.source_paths.clone()).unwrap_or_default();
    envelope.archive = serde_json::from_value(entry.archive.clone()).unwrap_or_default();
    envelope.freshness = serde_json::from_value(entry.freshness.clone()).unwrap_or_default();
    envelope.profile = entry
        .profile
        .clone()
        .and_then(|v| serde_json::from_value(v).ok());
    Ok(envelope)
}

/// Generate a random suffix for transaction IDs.
fn random_suffix() -> String {
    generate_storage_id().unwrap_or_else(|_| "unknown".into())
}

#[allow(clippy::needless_pass_by_value)]
fn crypto_to_store_error(error: CryptoError) -> StoreError {
    StoreError::new(
        crate::StoreErrorKind::InvalidRecord,
        "cryptographic operation failed",
        serde_json::json!({"detail": error.to_string()}),
    )
}
