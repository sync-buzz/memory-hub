use std::collections::BTreeMap;
use std::path::PathBuf;

use memory_hub_core::{
    CURRENT_ENVELOPE_VERSION, EncryptedRecord, Envelope, OpaqueStorageId, StoredRecord,
};
use memory_hub_crypto::{
    CIPHER_SUITE, CryptoError, Identity, Recipient, backup_identity_to_string, decrypt_b64,
    encrypt_b64, generate_backup_identity, generate_storage_id, hex_lower,
};
// The one place a backend still knows what a type is, and it is not an
// oversight. Every other store hands its corpus to `TransactionPolicy` and
// lets the layer above judge it; this one's corpus is ciphertext, so doing the
// same would mean decrypting every record on every write. The manifest already
// carries the kinds and folders the rules ask about, and only `__type__`
// records — a handful — are decrypted to read the schema.
use memory_hub_schema::{SchemaRegistry, TYPE_KIND, TypeDefinition};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use memory_hub_engine::{Capabilities, Capability, Ownership, RecordStore, StoreDescription};

use crate::error::GitStoreError;
use crate::types::GitRevision;
use crate::{
    ApplyResult, CommitSigner, GitStore, Operation, RecordId, Revision, StoreError, StoreErrorKind,
    Transaction,
};

/// Identifier this backend reports in [`StoreDescription::backend`].
pub const ENCRYPTED_BACKEND: &str = "refs+age";

/// Check whether a Git repository is an encrypted Memory project.
///
/// A project is encrypted when a manifest blob (at the deterministic
/// `manifest_storage_id`) exists in the current staged snapshot. Plaintext
/// projects never produce this blob.
///
/// This is a read-only check: it does not create `refs/memory/staged` if it
/// does not exist, so it is safe to call before `initialize`.
///
/// # Errors
///
/// Returns [`StoreError`] if the repository cannot be opened.
pub fn is_encrypted_project(project: impl AsRef<std::path::Path>) -> Result<bool, StoreError> {
    let project = project.as_ref();
    if !project.is_absolute() {
        return Err(StoreError::new(
            crate::StoreErrorKind::InvalidArgument,
            "project must be an absolute repository root or Git directory",
            serde_json::json!({"field": "project"}),
        ));
    }
    let git_dir = GitStore::discover_git_dir(project)?;
    let repository = git2::Repository::open(&git_dir)
        .map_err(|error| StoreError::repository("open for encrypted detection", error))?;
    // Read the staged ref without creating it.
    let revision = match repository.refname_to_id("refs/memory/staged") {
        Ok(oid) => Revision::from_oid(oid),
        Err(_) => return Ok(false),
    };
    let id = RecordId::opaque(manifest_storage_id());
    let store = GitStore::from_git_dir(git_dir);
    Ok(store.read_record_pub(&revision, &id)?.is_some())
}

/// Deterministic storage id for the encrypted manifest blob.
#[allow(clippy::expect_used)]
fn manifest_storage_id() -> OpaqueStorageId {
    let digest = Sha256::digest(b"memory-hub-manifest");
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
    /// Kept because the manifest is the only copy: an encrypted record is
    /// reconstructed from this entry and its decrypted bytes, so a field the
    /// entry drops is a field the record loses on the way back.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    is_folder: bool,
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
    strict_schema: bool,
}

/// Written by hand, and deliberately incomplete.
///
/// The lock state holds an age identity. A derived `Debug` would print it, and
/// the places `Debug` output ends up — a log line, a panic message, an error
/// report mailed to somebody — are exactly the places a private key must not
/// be. What is safe to say is whether the store is unlocked.
impl std::fmt::Debug for EncryptedStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("EncryptedStore")
            .field("git_dir", &self.git_dir())
            .field("unlocked", &self.is_unlocked())
            .finish_non_exhaustive()
    }
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
            strict_schema: true,
        })
    }

    /// Attach a commit signer so every subsequent commit on `refs/memory/*`
    /// carries an SSH signature for integrity.
    #[must_use]
    pub fn with_signer(mut self, signer: std::sync::Arc<dyn CommitSigner>) -> Self {
        self.store = self.store.with_signer(signer);
        self
    }

    /// Control whether records with unknown kinds are rejected.
    ///
    /// When `strict` (the default), a record whose `kind` has no matching
    /// `__type__` definition is rejected. When `false`, unknown kinds are
    /// accepted without schema validation.
    #[must_use]
    pub fn with_schema_strict(mut self, strict: bool) -> Self {
        self.strict_schema = strict;
        self
    }

    /// Return the git directory path (for config/transport operations).
    #[must_use]
    pub fn git_dir(&self) -> &std::path::Path {
        self.store.git_dir()
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
                expected_content_hash: None,
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
        self.store.current().map(|view| view.revision().clone())
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

        // Schema validation happens here rather than through the store's
        // `TransactionPolicy`, because a policy is given the corpus and this
        // store's corpus is ciphertext: answering the same questions through it
        // would mean decrypting every record on every write. The manifest
        // already carries the kinds, folders and paths the rules ask about, so
        // they are asked of the manifest.
        let schema_registry = self.load_encrypted_schema_registry(&manifest)?;
        if !schema_registry.is_empty() {
            for (_, envelope) in puts {
                validate_envelope_against_schema(&schema_registry, envelope, self.strict_schema)?;
            }
        }
        require_one_record_per_folder(&manifest, puts, deletes)?;

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
                expected_content_hash: None,
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
                expected_content_hash: None,
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

    /// Fetch from the configured memory remote and perform an encrypted
    /// record-level merge.
    ///
    /// Decrypts both local and remote records, merges at the envelope level
    /// (different keys auto-merge, same-key conflicts returned), re-encrypts
    /// to the union of both recipients lists, and writes the merged
    /// transaction.
    ///
    /// When the remote is a fast-forward, the staged ref is updated directly
    /// without re-encryption.
    ///
    /// SSH signature verification is automatic: the local manifest's
    /// recipients' public keys are used as the allowed signers list. If
    /// `allowed_signers` is non-empty, it is merged with the manifest
    /// recipients (caller-supplied override for first-fetch before manifest
    /// exists).
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for transport failures, signature issues, or
    /// encryption errors.
    #[allow(clippy::too_many_lines)]
    pub fn fetch_and_merge(
        &self,
        remote: &crate::MemoryRemote,
        allowed_signers: &[String],
    ) -> Result<crate::FetchResult, StoreError> {
        let identity = self.require_unlocked()?;

        // Build the effective signers list: caller-supplied keys + manifest
        // recipients' public keys (for SSH-type keys only — x25519 backup
        // keys can't sign Git commits).
        let local_revision_for_signers = self.current_revision()?;
        let mut signers: Vec<String> = allowed_signers.to_vec();
        if let Ok(manifest) = self.read_manifest(identity, &local_revision_for_signers) {
            for entry in &manifest.recipients {
                if entry.key_type == "ssh" && !signers.contains(&entry.public_key) {
                    signers.push(entry.public_key.clone());
                }
            }
        }

        // Step 1: Fetch remote to temp ref and get revisions.
        let (local_revision, remote_revision) =
            crate::fetch_remote_revision(&self.store, remote, &signers)?;

        // Step 2: Check if fast-forward is possible.
        if crate::can_fast_forward(&self.store, &local_revision, &remote_revision)? {
            crate::fast_forward_to(self.store.git_dir(), &remote_revision)?;
            crate::cleanup_temp_ref_pub(self.store.git_dir())?;
            return Ok(crate::FetchResult {
                local_revision_before: local_revision.clone(),
                local_revision_after: remote_revision.clone(),
                remote_revision,
                fast_forward: true,
                merged: false,
                conflicts: Vec::new(),
            });
        }

        // Step 3: Read both manifests (decrypting).
        let mut local_manifest = self.read_manifest(identity, &local_revision)?;
        let remote_manifest = self.read_manifest_unchecked(identity, &remote_revision)?;

        // Step 4: Take union of recipients.
        let union_recipients =
            union_recipient_lists(&local_manifest.recipients, &remote_manifest.recipients);
        local_manifest.recipients = union_recipients;
        let parsed_recipients = parse_recipients(&local_manifest.recipients)?;

        // Step 5: Merge records at the envelope level.
        let mut conflicts = Vec::new();
        let mut puts: Vec<(&str, Envelope)> = Vec::new();

        for (key, remote_entry) in &remote_manifest.records {
            if let Some(local_entry) = local_manifest.records.get(key) {
                // Same key — check if content differs.
                if local_entry.content_hash != remote_entry.content_hash {
                    // Conflict: same key, different content.
                    conflicts.push(crate::ConflictEntry {
                        key: key.clone(),
                        local_content_hash: local_entry.content_hash.clone(),
                        remote_content_hash: remote_entry.content_hash.clone(),
                    });
                }
            } else {
                // Key only in remote — decrypt and add to local.
                let storage_id = parse_storage_id(&remote_entry.storage_id)?;
                let id = RecordId::opaque(storage_id);
                let Some(stored) = self.store.read_record_unchecked(&remote_revision, &id)? else {
                    continue;
                };
                let ciphertext_b64 = match stored {
                    StoredRecord::Encrypted { encrypted } => encrypted.ciphertext.clone(),
                    StoredRecord::Plaintext { .. } => continue,
                };
                let content = decrypt_b64(&ciphertext_b64, std::slice::from_ref(identity))
                    .map_err(crypto_to_store_error)?;
                let envelope = reconstruct_envelope(key, remote_entry, content)?;
                puts.push((key.as_str(), envelope));
            }
        }

        if !conflicts.is_empty() {
            crate::cleanup_temp_ref_pub(self.store.git_dir())?;
            return Ok(crate::FetchResult {
                local_revision_before: local_revision.clone(),
                local_revision_after: local_revision,
                remote_revision,
                fast_forward: false,
                merged: false,
                conflicts,
            });
        }

        // Step 6: Apply merged puts and re-encrypt ALL records to the union
        // recipients. We first persist the union recipients to the manifest
        // via a manifest-only transaction, then apply the new puts, then
        // run reencrypt_all to re-encrypt existing records.
        if !puts.is_empty() {
            // First, write the union manifest (recipients only, no record changes).
            // This ensures subsequent apply() calls use the union recipients.
            self.store.apply(&Transaction {
                id: format!("merge-recipients-{}", random_suffix()),
                expected_revision: local_revision.clone(),
                operations: vec![Operation::Put {
                    record: StoredRecord::Encrypted {
                        encrypted: Self::encrypt_manifest(&local_manifest, &parsed_recipients)?,
                    },
                    expected_content_hash: None,
                }],
            })?;
        }

        // Now get the updated revision after manifest write (or original if no puts).
        let manifest_revision = if puts.is_empty() {
            local_revision.clone()
        } else {
            self.current_revision()?
        };

        // Re-encrypt existing records to the union recipients.
        self.reencrypt_all(
            &mut local_manifest,
            identity,
            &parsed_recipients,
            &manifest_revision,
        )?;

        // If we have new puts from the remote, apply them now (after
        // reencrypt_all, so they're encrypted to the union recipients).
        if !puts.is_empty() {
            let after_reencrypt = self.current_revision()?;
            let puts_owned: Vec<(String, Envelope)> = puts
                .iter()
                .map(|(k, e)| ((*k).to_owned(), e.clone()))
                .collect();
            let puts_refs: Vec<(&str, Envelope)> = puts_owned
                .iter()
                .map(|(k, e)| (k.as_str(), e.clone()))
                .collect();
            self.apply(
                &format!("merge-puts-{}", random_suffix()),
                after_reencrypt,
                &puts_refs,
                &[],
            )?;
        }

        let after_revision = self.current_revision()?;
        crate::cleanup_temp_ref_pub(self.store.git_dir())?;

        Ok(crate::FetchResult {
            local_revision_before: local_revision,
            local_revision_after: after_revision,
            remote_revision,
            fast_forward: false,
            merged: true,
            conflicts: Vec::new(),
        })
    }

    /// Build a [`SchemaRegistry`] from decrypted `__type__` records in the
    /// manifest.
    ///
    /// Type definitions are stored as regular records with `kind = "__type__"`.
    /// Their content is the JSON type definition, encrypted alongside other
    /// records. This method decrypts only the `__type__` records to build
    /// the registry.
    fn load_encrypted_schema_registry(
        &self,
        manifest: &Manifest,
    ) -> Result<SchemaRegistry, StoreError> {
        let identity = self.require_unlocked()?;
        let revision = self.current_revision()?;
        let mut definitions = Vec::new();
        for (key, entry) in &manifest.records {
            if entry.kind != TYPE_KIND {
                continue;
            }
            let storage_id = parse_storage_id(&entry.storage_id)?;
            let id = RecordId::opaque(storage_id);
            let Some(stored) = self.store.read_record_pub(&revision, &id)? else {
                continue;
            };
            let ciphertext_b64 = match stored {
                StoredRecord::Encrypted { encrypted } => encrypted.ciphertext.clone(),
                StoredRecord::Plaintext { .. } => continue,
            };
            let plaintext = decrypt_b64(&ciphertext_b64, std::slice::from_ref(identity))
                .map_err(crypto_to_store_error)?;
            let definition =
                TypeDefinition::from_content(&String::from_utf8(plaintext).map_err(|e| {
                    StoreError::new(
                        crate::StoreErrorKind::InvalidRecord,
                        "type record content is not valid UTF-8",
                        serde_json::json!({"key": key, "detail": e.to_string()}),
                    )
                })?)
                .map_err(|e| {
                    StoreError::new(
                        crate::StoreErrorKind::InvalidRecord,
                        "type definition record has malformed JSON",
                        serde_json::json!({"key": key, "detail": e.to_string()}),
                    )
                })?;
            definitions.push(definition);
        }
        SchemaRegistry::from_type_definitions(definitions).map_err(|error| {
            StoreError::new(
                crate::StoreErrorKind::InvalidRecord,
                "schema registry could not be built from type records",
                serde_json::json!({
                    "field": error.field,
                    "reason": error.message,
                }),
            )
        })
    }

    fn require_unlocked(&self) -> Result<&Identity, StoreError> {
        match &self.state {
            LockState::Unlocked { identity } => Ok(identity),
            LockState::Locked => Err(StoreError::new(
                crate::StoreErrorKind::Locked,
                "encrypted store is locked — unlock before reading or writing",
                serde_json::json!({"recovery_action": "unlock_with_identity"}),
            )),
        }
    }

    fn read_manifest(
        &self,
        identity: &Identity,
        revision: &Revision,
    ) -> Result<Manifest, StoreError> {
        self.read_manifest_inner(identity, revision, false)
    }

    /// Read manifest from a revision that may not be in the local staged
    /// history (e.g. a fetched remote revision).
    fn read_manifest_unchecked(
        &self,
        identity: &Identity,
        revision: &Revision,
    ) -> Result<Manifest, StoreError> {
        self.read_manifest_inner(identity, revision, true)
    }

    fn read_manifest_inner(
        &self,
        identity: &Identity,
        revision: &Revision,
        unchecked: bool,
    ) -> Result<Manifest, StoreError> {
        let id = RecordId::opaque(manifest_storage_id());
        let stored = if unchecked {
            self.store.read_record_unchecked(revision, &id)?
        } else {
            self.store.read_record_pub(revision, &id)?
        };
        let Some(stored) = stored else {
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
                    expected_content_hash: None,
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
            expected_content_hash: None,
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

/// Take the union of two recipient lists, preserving order (local first, then
/// remote-only). Deduplicates by `public_key`.
fn union_recipient_lists(
    local: &[RecipientEntry],
    remote: &[RecipientEntry],
) -> Vec<RecipientEntry> {
    let mut result = local.to_vec();
    let local_keys: std::collections::HashSet<&str> =
        local.iter().map(|r| r.public_key.as_str()).collect();
    for entry in remote {
        if !local_keys.contains(entry.public_key.as_str()) {
            result.push(entry.clone());
        }
    }
    result
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
        .map(|e| memory_hub_crypto::parse_recipient(&e.public_key).map_err(crypto_to_store_error))
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
        folder: envelope.folder.clone(),
        is_folder: envelope.is_folder,
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
    envelope.folder.clone_from(&entry.folder);
    envelope.is_folder = entry.is_folder;
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

/// Refuse a write that would leave two records standing for one folder.
///
/// The same rule the plaintext store keeps, checked here because there it
/// cannot be: a record on its way into an encrypted corpus is opaque by the
/// time the Git layer sees it. The manifest is what makes the check possible —
/// it carries every record's folder in the clear to whoever can already read
/// the manifest, and to nobody else.
fn require_one_record_per_folder(
    manifest: &Manifest,
    puts: &[(&str, Envelope)],
    deletes: &[&str],
) -> Result<(), StoreError> {
    let mut standing: BTreeMap<&str, &str> = BTreeMap::new();
    for (key, entry) in &manifest.records {
        let rewritten = puts.iter().any(|(candidate, _)| candidate == key)
            || deletes.iter().any(|candidate| candidate == key);
        if entry.is_folder && !rewritten {
            standing.insert(entry.folder.as_deref().unwrap_or(""), key.as_str());
        }
    }
    for (key, envelope) in puts {
        if !envelope.is_folder {
            continue;
        }
        let folder = envelope.folder.as_deref().unwrap_or("");
        if let Some(taken) = standing.get(folder)
            && taken != key
        {
            return Err(StoreError::new(
                crate::StoreErrorKind::InvalidRecord,
                "a folder already has the record that stands for it",
                serde_json::json!({
                    "folder": folder,
                    "key": key,
                    "existing_key": taken,
                }),
            ));
        }
        standing.insert(folder, key);
    }
    Ok(())
}

/// Validate a single envelope against the schema registry.
///
/// `__type__` records are validated structurally. Regular records are
/// validated against the registry — in strict mode, unknown kinds are
/// rejected.
fn validate_envelope_against_schema(
    registry: &SchemaRegistry,
    envelope: &Envelope,
    strict: bool,
) -> Result<(), StoreError> {
    if envelope.kind == TYPE_KIND {
        let definition = TypeDefinition::from_content(&envelope.content).map_err(|e| {
            StoreError::new(
                crate::StoreErrorKind::InvalidRecord,
                "type definition record has malformed JSON",
                serde_json::json!({"detail": e.to_string()}),
            )
        })?;
        definition.validate_self().map_err(|error| {
            StoreError::new(
                crate::StoreErrorKind::InvalidRecord,
                "type definition failed self-validation",
                serde_json::json!({
                    "kind": "__type__",
                    "field": error.field,
                    "reason": error.message,
                }),
            )
        })?;
    } else {
        registry
            .validate_record(envelope, strict)
            .map_err(|error| {
                StoreError::new(
                    crate::StoreErrorKind::InvalidRecord,
                    format!(
                        "record of kind `{}` failed schema validation",
                        envelope.kind
                    ),
                    serde_json::json!({
                        "kind": envelope.kind,
                        "field": error.field,
                        "reason": error.message,
                    }),
                )
            })?;
    }
    Ok(())
}

/// The encrypted store, as a storage like any other.
///
/// Everything a caller does to records — read one, read them all, apply a
/// batch — goes through the same contract as a plaintext store, and the fact
/// that the bytes on disk are ciphertext is one line in [`Capabilities`].
///
/// What is *not* here is the key work: unlocking, recipients, rotation. Those
/// stay on the concrete type for the same reason transport stays on
/// [`GitStore`] — they are not something every storage has, and putting an age
/// identity into the storage contract would make every backend link a crypto
/// library to say it does not encrypt.
impl RecordStore for EncryptedStore {
    fn capabilities(&self) -> Capabilities {
        // No `Snapshots`. The past is *kept* — the checkpoints are Git's and
        // `history` still answers — but reopening a past revision as a view of
        // records is not something this store can do: reads go through the
        // manifest, and the manifest describes the state it is in now. Saying
        // otherwise would be a store that accepts an old revision and answers
        // with today's records.
        Capabilities::new(
            Ownership::Owned,
            [
                Capability::History,
                Capability::Transport,
                Capability::Encryption,
            ],
        )
    }

    fn describe(&self) -> StoreDescription {
        StoreDescription {
            backend: ENCRYPTED_BACKEND.to_owned(),
            git_dir: Some(self.git_dir().to_path_buf()),
        }
    }

    fn index_root(&self) -> PathBuf {
        self.store.index_root()
    }

    fn current_revision(&self) -> Result<Revision, StoreError> {
        Self::current_revision(self)
    }

    fn read_record(
        &self,
        _revision: &Revision,
        id: &RecordId,
    ) -> Result<Option<StoredRecord>, StoreError> {
        // Records are addressed by their semantic key. The opaque id is how
        // this store files them, and a caller holding one is asking about a
        // record it read from somewhere that does not decrypt.
        let RecordId::Plaintext(key) = id else {
            return Ok(None);
        };
        Ok(self.get(key)?.map(|envelope| StoredRecord::Plaintext {
            envelope: Box::new(envelope),
        }))
    }

    fn read_records(
        &self,
        _revision: &Revision,
    ) -> Result<Vec<(RecordId, StoredRecord)>, StoreError> {
        Ok(self
            .list()?
            .into_iter()
            .map(|(key, envelope)| {
                (
                    RecordId::plaintext(key),
                    StoredRecord::Plaintext {
                        envelope: Box::new(envelope),
                    },
                )
            })
            .collect())
    }

    fn apply(&self, transaction: &Transaction) -> Result<ApplyResult, StoreError> {
        let mut puts: Vec<(String, Envelope)> = Vec::new();
        let mut deletes: Vec<String> = Vec::new();
        for operation in &transaction.operations {
            match operation {
                Operation::Put {
                    record,
                    expected_content_hash,
                } => match record {
                    StoredRecord::Plaintext { envelope } => {
                        if expected_content_hash.is_some() {
                            // Refused rather than ignored: the stored bytes are
                            // ciphertext and hash to something the caller has
                            // never seen, so the condition it wants cannot be
                            // checked. A caller that asked for a conditional
                            // write and silently got an unconditional one is
                            // worse off than one told no.
                            return Err(StoreError::new(
                                StoreErrorKind::Unsupported,
                                "an encrypted store cannot make a write conditional on content \
                                 — what it stores is ciphertext",
                                serde_json::json!({"transaction_id": transaction.id}),
                            ));
                        }
                        puts.push((envelope.key.clone(), (**envelope).clone()));
                    }
                    // Already ciphertext. Encrypting it again would produce a
                    // record only this process could read.
                    StoredRecord::Encrypted { .. } => {
                        return Err(StoreError::new(
                            StoreErrorKind::InvalidRecord,
                            "an encrypted project takes plaintext records and encrypts them",
                            serde_json::json!({"transaction_id": transaction.id}),
                        ));
                    }
                },
                Operation::Delete { id } => match id {
                    RecordId::Plaintext(key) => deletes.push(key.clone()),
                    RecordId::Opaque(_) => {
                        return Err(StoreError::new(
                            StoreErrorKind::InvalidArgument,
                            "a record of an encrypted project is deleted by its key",
                            serde_json::json!({"id": id.display_value()}),
                        ));
                    }
                },
            }
        }
        let put_refs: Vec<(&str, Envelope)> = puts
            .iter()
            .map(|(key, envelope)| (key.as_str(), envelope.clone()))
            .collect();
        let delete_refs: Vec<&str> = deletes.iter().map(String::as_str).collect();
        Self::apply(
            self,
            &transaction.id,
            transaction.expected_revision.clone(),
            &put_refs,
            &delete_refs,
        )
    }

    fn validate_revision(&self, revision: &Revision) -> Result<(), StoreError> {
        // Not delegated to the Git store. That one would accept any commit it
        // holds, and this one would then read the current manifest anyway — a
        // caller asking for last week's records would be handed today's and
        // told the revision was theirs.
        let current = Self::current_revision(self)?;
        if *revision == current {
            return Ok(());
        }
        Err(StoreError::new(
            StoreErrorKind::RevisionNotFound,
            "an encrypted store serves only its current state — a past revision cannot be reopened",
            serde_json::json!({
                "requested": revision.as_str(),
                "current": current.as_str(),
            }),
        ))
    }

    fn history(&self) -> Option<&dyn memory_hub_engine::HistoryStore> {
        // Checkpoints and diffs are the Git store's, and they work on
        // ciphertext exactly as well as on plaintext: what changed is a
        // question about blobs.
        RecordStore::history(&self.store)
    }

    fn portable(&self) -> Option<&dyn memory_hub_engine::PortableStore> {
        RecordStore::portable(&self.store)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{RecipientEntry, union_recipient_lists};

    fn entry(key: &str, key_type: &str) -> RecipientEntry {
        RecipientEntry {
            public_key: key.to_owned(),
            key_type: key_type.to_owned(),
            label: None,
        }
    }

    #[test]
    fn union_recipients_deduplicates_by_public_key() {
        let local = vec![
            entry("ssh-ed25519 AAAA alice", "ssh"),
            entry("ssh-ed25519 BBBB bob", "ssh"),
        ];
        let remote = vec![
            entry("ssh-ed25519 BBBB bob", "ssh"), // duplicate
            entry("ssh-ed25519 CCCC carol", "ssh"),
        ];
        let union = union_recipient_lists(&local, &remote);
        assert_eq!(union.len(), 3);
        assert_eq!(union[0].public_key, "ssh-ed25519 AAAA alice");
        assert_eq!(union[1].public_key, "ssh-ed25519 BBBB bob");
        assert_eq!(union[2].public_key, "ssh-ed25519 CCCC carol");
    }

    #[test]
    fn union_recipients_empty_local() {
        let local = vec![];
        let remote = vec![entry("ssh-ed25519 AAAA alice", "ssh")];
        let union = union_recipient_lists(&local, &remote);
        assert_eq!(union.len(), 1);
    }

    #[test]
    fn union_recipients_empty_remote() {
        let local = vec![entry("ssh-ed25519 AAAA alice", "ssh")];
        let remote = vec![];
        let union = union_recipient_lists(&local, &remote);
        assert_eq!(union.len(), 1);
    }
}
