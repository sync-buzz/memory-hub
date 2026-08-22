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

/// The rules a record store applies before accepting a transaction.
///
/// Built once and shared: it holds no state beyond the strictness flag, and the
/// registry it validates against is read from the corpus on every check —
/// because the corpus is what just changed. Where a type keeps its documents is
/// read from the same place, being part of the definition; nothing here has to
/// be told about the project separately, and so nothing here can be told
/// something the corpus disagrees with.
#[derive(Clone, Copy, Debug)]
pub struct SchemaPolicy {
    strict: bool,
}

impl SchemaPolicy {
    /// Strict: a record whose `kind` has no `__type__` definition is refused.
    #[must_use]
    pub const fn new(strict: bool) -> Self {
        Self { strict }
    }
}

/// Whether a record already lives where its type's storage says it should.
///
/// The folder itself, not merely whether the content is outside: two folders
/// are two places, and a record pointing into the one a type no longer names is
/// exactly a record left behind — which is what the caller of this is trying to
/// find.
fn conforms(envelope: &Envelope, storage: &TypeStorage) -> bool {
    let Some(folder) = storage.folder() else {
        // Content lives with the envelope, so a record pointing elsewhere
        // does not live here.
        return envelope.content_ref.is_none();
    };
    envelope
        .content_ref
        .as_ref()
        .is_some_and(|reference| reference.path.starts_with(&format!("{folder}/")))
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
        //
        // **The state a transaction is checked against is the one it produces**,
        // so the types it introduces are in the registry beside the ones
        // already stored. A transaction is atomic: a record and the definition
        // of its kind arriving together either both land or neither does, and
        // validating the record against the corpus from before would refuse
        // exactly the write that makes it valid.
        //
        // This used to work by accident and only on an empty corpus — with no
        // types at all, validation is skipped entirely — so describing a
        // project whose memory already held a type failed while describing one
        // with no memory succeeded.
        // Collapsed by kind, and the transaction's version wins: a transaction
        // that rewrites a definition is checked against what it writes, not
        // against the one it replaces. The registry refuses a kind declared
        // twice, so the two lists cannot simply be concatenated.
        // Two registries, because two questions are asked and they are asked of
        // different moments. What a record must satisfy is the schema *after*
        // this transaction. What a type is being moved away from is the schema
        // *before* it — a guard reading the merged one would compare a
        // definition against itself and wave every storage move through.
        let corpus = filter_type_definitions(existing)?;
        let stored = SchemaRegistry::from_type_definitions(corpus.clone())
            .map_err(|error| schema_registry_error(&error))?;
        let mut definitions: BTreeMap<String, TypeDefinition> = corpus
            .into_iter()
            .map(|definition| (definition.kind_name.clone(), definition))
            .collect();
        for definition in filter_type_definitions_of(transaction)? {
            definitions.insert(definition.kind_name.clone(), definition);
        }
        let effective =
            SchemaRegistry::from_type_definitions(definitions.into_values().collect::<Vec<_>>())
                .map_err(|error| schema_registry_error(&error))?;
        validate_operations_against_schema(*self, &stored, &effective, transaction, existing)?;
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
            StoredRecord::Plaintext { .. } => None,
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

/// The type definitions a transaction is introducing.
///
/// Later than the stored ones on purpose: a transaction that rewrites a
/// definition is validated against the version it writes, not the one it
/// replaces. `SchemaRegistry` takes the last of a repeated kind, and this list
/// is appended after the corpus for exactly that reason.
fn filter_type_definitions_of(
    transaction: &Transaction,
) -> Result<Vec<TypeDefinition>, StoreError> {
    transaction
        .operations
        .iter()
        .filter_map(|operation| match operation {
            Operation::Put {
                record: StoredRecord::Plaintext { envelope },
                ..
            } if envelope.kind == TYPE_KIND => {
                Some(TypeDefinition::from_content(&envelope.content))
            }
            Operation::Put { .. } | Operation::Delete { .. } => None,
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
    policy: SchemaPolicy,
    stored: &SchemaRegistry,
    effective: &SchemaRegistry,
    transaction: &Transaction,
    existing: &[(RecordId, StoredRecord)],
) -> Result<(), StoreError> {
    for operation in &transaction.operations {
        let Operation::Put { record, .. } = operation else {
            continue;
        };
        let StoredRecord::Plaintext { envelope } = record;
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
            // `stored`: the question is what this type is moving *away* from.
            require_no_silent_storage_move(stored, &definition, existing, transaction)?;
        } else if !effective.is_empty() {
            // The record that is a folder is held to the kind existing and to
            // nothing the kind declares. It is not a document of that type —
            // it is what the folder those documents are in has to say — so a
            // type with a required field would otherwise make its folders
            // undescribable without inventing values for fields that are about
            // documents.
            effective
                .validate_record_shallow(envelope, policy.strict, !envelope.is_folder)
                .map_err(|error| schema_validation_error(&envelope.kind, &error))?;
            require_shape_of_its_storage(effective, envelope, existing)?;
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
        let StoredRecord::Plaintext { envelope } = record;
        if envelope.is_folder {
            standing.insert(folder_path(envelope), envelope.key.as_str());
        }
    }

    for operation in &transaction.operations {
        let Operation::Put { record, .. } = operation else {
            continue;
        };
        let StoredRecord::Plaintext { envelope } = record;
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
pub(crate) fn folder_path(envelope: &Envelope) -> &str {
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
///
/// **The record that is a folder is not one of that type's documents**, so this
/// does not reach it. It is *about* a folder rather than in one: it names no
/// file because there is no file to name, and the scan has nothing to keep
/// honest about it. Requiring one would mean a folder could only be described
/// by putting a document in somebody's repository, which is the one thing
/// attaching a folder promises not to do.
fn require_shape_of_its_storage(
    registry: &SchemaRegistry,
    envelope: &Envelope,
    existing: &[(RecordId, StoredRecord)],
) -> Result<(), StoreError> {
    let Ok(storage) = registry.storage_for(&envelope.kind) else {
        return Ok(());
    };
    if !storage.is_external() {
        return Ok(());
    }
    // The record that is a folder carries no file — that is the point of it —
    // but the folder it stands for still has to be one of this type's. Nothing
    // else checks: the rule below is about a document's locator, and this
    // record has none, so without this a folder record could describe a
    // directory belonging to somebody else's type, or to no type at all.
    if envelope.is_folder {
        return require_folder_of_its_storage(envelope, storage.folder());
    }
    if envelope.content_ref.is_some() {
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
            "storage": storage.folder(),
            "recovery_action": "write_the_document_and_scan",
        }),
    ))
}

/// The folder a folder record stands for is one of its type's, or it is none.
///
/// It stands for the folder it is filed in, so this is that one member: at the
/// storage root or under it. A record describing `elsewhere/notes` as a folder
/// of a type stored in `docs/` is describing something that type does not have,
/// and a tree drawn per type would either hide it or hang it nowhere.
fn require_folder_of_its_storage(
    envelope: &Envelope,
    root: Option<&str>,
) -> Result<(), StoreError> {
    let Some(root) = root else {
        return Ok(());
    };
    let folder = folder_path(envelope);
    if folder == root || folder.starts_with(&format!("{root}/")) {
        return Ok(());
    }
    Err(StoreError::new(
        StoreErrorKind::InvalidRecord,
        format!("a folder of this type lives under `{root}`"),
        serde_json::json!({
            "field": "folder",
            "kind": envelope.kind,
            "key": envelope.key,
            "folder": folder,
            "storage": root,
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
                    && !conforms(envelope, &after)
            }
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
            "from": before.folder(),
            "to": after.folder(),
            "recovery_action": "migrate_storage",
        }),
    ))
}
