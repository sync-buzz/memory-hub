//! What may be in a transaction, decided where the types are known.
//!
//! These are application rules, so they live here rather than inside a
//! backend. A backend that had to understand `__type__` records to accept a
//! write would oblige every other backend to understand them the same way — or
//! to disagree with the first about the same record.
//!
//! What stays with the backend is the *moment*. A store that rebases applies a
//! transaction onto state the caller never read, and a rule that counts "the
//! records this edit would leave behind" has to count them in that state, not
//! in the one the caller last saw. So the store calls
//! [`TransactionPolicy::check`] with the corpus it is actually building on,
//! and this module answers.

use std::collections::{BTreeMap, BTreeSet};

use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_engine::{
    Operation, RecordId, RecordStore, Revision, StoreError, StoreErrorKind, StoreView, Transaction,
    TransactionPolicy,
};
use memory_hub_schema::{SchemaRegistry, TYPE_KIND, TypeDefinition, TypeStorage, ValidationError};

use crate::config::{ProjectConfig, StorageKind};

/// The rules a record store applies before accepting a transaction.
///
/// Built once and shared: it holds no state beyond the strictness flag, and the
/// registry it validates against is read from the corpus on every check —
/// because the corpus is what just changed.
#[derive(Clone, Debug)]
pub struct SchemaPolicy {
    strict: bool,
    /// What the project declared, when it has declared anything.
    ///
    /// Without it a rule can still say whether a record's content is inside or
    /// outside; what it cannot say is *which* storage a locator belongs to,
    /// because that is written in the declaration and nowhere else.
    config: Option<ProjectConfig>,
}

impl SchemaPolicy {
    /// Strict: a record whose `kind` has no `__type__` definition is refused.
    #[must_use]
    pub const fn new(strict: bool) -> Self {
        Self {
            strict,
            config: None,
        }
    }

    /// Attach the project's declaration, so a locator can be checked against
    /// the storage it is supposed to be in.
    #[must_use]
    pub fn with_config(mut self, config: ProjectConfig) -> Self {
        self.config = Some(config);
        self
    }

    /// Whether a record already lives where its type's storage says it should.
    ///
    /// The storage as a whole, not merely whether it is external: two folders
    /// are two storages, and a record pointing into the one a type no longer
    /// names is exactly a record left behind — which is what the caller of
    /// this is trying to find.
    fn conforms(&self, envelope: &Envelope, storage: &TypeStorage) -> bool {
        let Some(name) = storage.name() else {
            // Content lives with the envelope, so a record pointing elsewhere
            // does not live here.
            return envelope.content_ref.is_none();
        };
        let Some(reference) = &envelope.content_ref else {
            return false;
        };
        let Some(config) = &self.config else {
            // Without the declaration, "it points somewhere" is the most that
            // can honestly be said.
            return true;
        };
        let Ok(declared) = config.storage(name) else {
            return false;
        };
        match (declared.kind, &declared.path) {
            (StorageKind::RepoFolder, Some(folder)) => {
                reference.path.starts_with(&format!("{folder}/"))
            }
            _ => true,
        }
    }
}

impl Default for SchemaPolicy {
    fn default() -> Self {
        Self::new(true)
    }
}

/// Read the type registry a store holds at `revision`.
///
/// Lives here rather than on the store for the same reason the rules do: what a
/// `__type__` record means is not something a backend knows. Encrypted records
/// are skipped — they are validated before encryption, where they are still
/// readable.
///
/// # Errors
///
/// Returns [`StoreError`] if the revision cannot be read or a type record is
/// malformed.
pub fn load_registry(
    store: &dyn RecordStore,
    revision: &Revision,
) -> Result<SchemaRegistry, StoreError> {
    let view = StoreView::open(store, revision)?;
    let records = view.records()?;
    SchemaRegistry::from_type_definitions(filter_type_definitions(&records)?)
        .map_err(|error| schema_registry_error(&error))
}

impl TransactionPolicy for SchemaPolicy {
    fn check(
        &self,
        transaction: &Transaction,
        existing: &[(RecordId, StoredRecord)],
    ) -> Result<(), StoreError> {
        // The registry is read from the same state the rules are checked
        // against. Passing one in would let a caller validate against types
        // that are no longer there.
        let registry = SchemaRegistry::from_type_definitions(filter_type_definitions(existing)?)
            .map_err(|error| schema_registry_error(&error))?;
        validate_operations_against_schema(self, &registry, transaction, existing)?;
        require_one_record_per_folder(transaction, existing)
    }
}

/// Extract and parse `__type__` definitions from a record list.
///
/// Encrypted records are skipped — only plaintext `__type__` records can be
/// validated at the Git layer.
fn filter_type_definitions(
    records: &[(RecordId, StoredRecord)],
) -> Result<Vec<TypeDefinition>, StoreError> {
    records
        .iter()
        .filter_map(|(_, record)| match record {
            StoredRecord::Plaintext { envelope } if envelope.kind == TYPE_KIND => {
                Some(TypeDefinition::from_content(&envelope.content))
            }
            _ => None,
        })
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| {
            StoreError::new(
                StoreErrorKind::InvalidRecord,
                "type definition record has malformed JSON",
                serde_json::json!({"detail": error.to_string()}),
            )
        })
}

/// Map a [`ValidationError`] to a [`StoreError`].
fn schema_validation_error(envelope_kind: &str, error: &ValidationError) -> StoreError {
    StoreError::new(
        StoreErrorKind::InvalidRecord,
        format!("record of kind `{envelope_kind}` failed schema validation"),
        serde_json::json!({
            "kind": envelope_kind,
            "field": error.field,
            "reason": error.message,
            "validation_kind": format!("{:?}", error.kind),
        }),
    )
}

/// Map a [`SchemaRegistry`] construction error to a [`StoreError`].
fn schema_registry_error(error: &ValidationError) -> StoreError {
    StoreError::new(
        StoreErrorKind::InvalidRecord,
        "schema registry could not be built from type records",
        serde_json::json!({
            "field": error.field,
            "reason": error.message,
            "validation_kind": format!("{:?}", error.kind),
        }),
    )
}

/// Validate every plaintext Put operation against the schema registry.
///
/// `__type__` records are always validated structurally (the type definition
/// itself is well-formed), regardless of registry state. Regular records are
/// validated against the registry only when it is non-empty — an empty
/// registry means no types are defined, so validation is disabled. In strict
/// mode, unknown kinds are rejected; in non-strict mode, they pass.
fn validate_operations_against_schema(
    policy: &SchemaPolicy,
    registry: &SchemaRegistry,
    transaction: &Transaction,
    existing: &[(RecordId, StoredRecord)],
) -> Result<(), StoreError> {
    for operation in &transaction.operations {
        let Operation::Put { record, .. } = operation else {
            continue;
        };
        let StoredRecord::Plaintext { envelope } = record else {
            continue;
        };
        if envelope.kind == TYPE_KIND {
            let definition = TypeDefinition::from_content(&envelope.content).map_err(|error| {
                StoreError::new(
                    StoreErrorKind::InvalidRecord,
                    "type definition record has malformed JSON",
                    serde_json::json!({"detail": error.to_string()}),
                )
            })?;
            definition.validate_self().map_err(|error| {
                StoreError::new(
                    StoreErrorKind::InvalidRecord,
                    "type definition failed self-validation",
                    serde_json::json!({
                        "kind": "__type__",
                        "field": error.field,
                        "reason": error.message,
                    }),
                )
            })?;
            require_no_silent_storage_move(policy, registry, &definition, existing, transaction)?;
        } else if !registry.is_empty() {
            registry
                .validate_record(envelope, policy.strict)
                .map_err(|error| schema_validation_error(&envelope.kind, &error))?;
            require_shape_of_its_storage(registry, envelope, existing)?;
        }
    }
    Ok(())
}

/// Refuse a transaction that would leave two records standing for one folder.
///
/// A folder's title and text are held by an ordinary record that carries
/// `is_folder`, and the folder it stands for is the one it is filed in. Two of
/// them in a folder is not a merge to resolve later: it is a question with no
/// answer — which of the two is the folder — asked of every client that draws a
/// tree. It is cheaper to refuse the write, where the caller still knows what
/// it meant.
///
/// The corpus and the transaction are read as one state, so a batch carrying
/// both of them is refused on the same terms, and a batch that retires the old
/// record while introducing the new one is not refused at all.
///
/// Encrypted records are not visible here and are checked before encryption,
/// in [`crate::EncryptedStore`].
fn require_one_record_per_folder(
    transaction: &Transaction,
    existing: &[(RecordId, StoredRecord)],
) -> Result<(), StoreError> {
    let touched: BTreeSet<RecordId> = transaction
        .operations
        .iter()
        .map(|operation| match operation {
            Operation::Put { record, .. } => RecordId::from_record(record),
            Operation::Delete { id } => id.clone(),
        })
        .collect();

    // What the corpus already says, minus everything this transaction is about
    // to rewrite or remove.
    let mut standing: BTreeMap<&str, &str> = BTreeMap::new();
    for (id, record) in existing {
        if touched.contains(id) {
            continue;
        }
        if let StoredRecord::Plaintext { envelope } = record
            && envelope.is_folder
        {
            standing.insert(folder_path(envelope), envelope.key.as_str());
        }
    }

    for operation in &transaction.operations {
        let Operation::Put { record, .. } = operation else {
            continue;
        };
        let StoredRecord::Plaintext { envelope } = record else {
            continue;
        };
        if !envelope.is_folder {
            continue;
        }
        let folder = folder_path(envelope);
        if let Some(taken) = standing.get(folder)
            && *taken != envelope.key.as_str()
        {
            return Err(StoreError::new(
                StoreErrorKind::InvalidRecord,
                "a folder already has the record that stands for it",
                serde_json::json!({
                    "folder": folder,
                    "key": envelope.key,
                    "existing_key": taken,
                }),
            ));
        }
        standing.insert(folder, envelope.key.as_str());
    }
    Ok(())
}

/// The folder a record is filed in, with the root spelled as the empty string
/// so it takes part in the same lookup as every other path.
fn folder_path(envelope: &Envelope) -> &str {
    envelope.folder.as_deref().unwrap_or("")
}

/// Refuse a *new* record whose shape contradicts where its type is stored.
///
/// A type whose content lives in another storage is a reference type by
/// construction: the bytes are somebody else's file, and the record points at
/// them. A record of that kind carrying its content inline would live with the
/// envelopes and have no file anywhere, invisible to the scan that is supposed
/// to keep it honest — the type would say one thing and the corpus hold
/// another.
///
/// Only records that are new are checked. A migration rewrites existing records
/// into the shape of the storage they are moving to *before* the definition
/// that names it, so an existing key is exactly the case this must not refuse.
fn require_shape_of_its_storage(
    registry: &SchemaRegistry,
    envelope: &Envelope,
    existing: &[(RecordId, StoredRecord)],
) -> Result<(), StoreError> {
    let Ok(storage) = registry.storage_for(&envelope.kind) else {
        return Ok(());
    };
    if !storage.is_external() || envelope.content_ref.is_some() {
        return Ok(());
    }
    let id = RecordId::plaintext(&envelope.key);
    if existing.iter().any(|(existing_id, _)| *existing_id == id) {
        return Ok(());
    }
    Err(StoreError::new(
        StoreErrorKind::InvalidRecord,
        "a record of a type stored elsewhere points at its content — \
         it does not carry it",
        serde_json::json!({
            "field": "content_ref",
            "kind": envelope.kind,
            "key": envelope.key,
            "storage": storage.name(),
            "recovery_action": "write_the_document_and_scan",
        }),
    ))
}

/// Refuse a type edit that would move where existing records are stored.
///
/// The storage place is a field of the type definition, and type definitions
/// are edited. Letting the data follow an edited field is data loss wearing
/// the clothes of a setting: nobody editing a definition expects records to be
/// rewritten, and for the direction that publishes them into the working tree
/// nobody expects that either.
///
/// So the edit is refused when it would leave records behind — described by a
/// storage they are not in — and the move is a separate operation that states
/// its plan first.
///
/// A batch that rewrites every record of the kind alongside the definition is
/// the migration itself, and is allowed: nothing is left behind by it. A kind
/// with no records has nothing to leave behind either.
fn require_no_silent_storage_move(
    policy: &SchemaPolicy,
    registry: &SchemaRegistry,
    definition: &TypeDefinition,
    existing: &[(RecordId, StoredRecord)],
    transaction: &Transaction,
) -> Result<(), StoreError> {
    let Some(current) = registry.get(&definition.kind_name) else {
        return Ok(());
    };
    let (Ok(before), Ok(after)) = (current.storage(), definition.storage()) else {
        return Ok(());
    };
    if before == after {
        return Ok(());
    }
    let rewritten: BTreeSet<RecordId> = transaction.operations.iter().map(Operation::id).collect();
    let records = existing
        .iter()
        .filter(|(id, record)| match record {
            StoredRecord::Plaintext { envelope } => {
                envelope.kind == definition.kind_name
                    && !rewritten.contains(id)
                    && !policy.conforms(envelope, &after)
            }
            StoredRecord::Encrypted { .. } => false,
        })
        .count();
    if records == 0 {
        return Ok(());
    }
    Err(StoreError::new(
        StoreErrorKind::InvalidArgument,
        "changing where a type is stored would leave records behind — that is a \
         migration, not an edit",
        serde_json::json!({
            "field": "storage",
            "kind": definition.kind_name,
            "records": records,
            "from": before.name(),
            "to": after.name(),
            "recovery_action": "migrate_storage",
        }),
    ))
}
