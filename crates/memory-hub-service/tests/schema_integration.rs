//! Schema validation integration tests, driven through a store that carries
//! [`SchemaPolicy`].
//!
//! These rules used to be checked inside the Git store and are checked here
//! now, so the tests moved with them: a store opened without a policy enforces
//! nothing, which is the point of the seam.
//!
//! Covers the acceptance checklist of `s-schema-store-integration`:
//! create/update/delete type records, reject unknown kinds in strict mode,
//! reject missing required fields, strict=false fallback, and that an empty
//! registry (no type records) skips validation.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;

use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_service::{SchemaPolicy, load_registry};
use memory_hub_store::{GitStore, Operation, RecordId, StoreError, StoreErrorKind, Transaction};
use serde_json::{Value, json};

const DECISION_TYPE_CONTENT: &str = r#"{
  "kind_name": "decision",
  "description": "A chosen path among alternatives",
  "envelope": {
    "title": { "required": true },
    "content": { "required": true, "min_length": 10 }
  },
  "fields": {
    "rationale_summary": {
      "type": "string",
      "required": true
    }
  },
  "relationships": {
    "supersedes": { "target": "decision" },
    "references": { "target": "any" }
  }
}"#;

const SPEC_TYPE_CONTENT: &str = r#"{
  "kind_name": "spec",
  "description": "A unit of work",
  "envelope": { "title": { "required": true } },
  "fields": {
    "status": {
      "type": "enum",
      "values": ["todo", "in_progress", "done"],
      "required": true
    }
  },
  "relationships": {
    "depends_on": { "target": "spec" }
  }
}"#;

fn setup() -> (tempfile::TempDir, GitStore) {
    let directory = tempfile::tempdir().unwrap();
    git2::Repository::init(directory.path()).unwrap();
    let store = GitStore::open(directory.path())
        .unwrap()
        .with_policy(Arc::new(SchemaPolicy::default()));
    (directory, store)
}

fn revision(store: &GitStore) -> memory_hub_store::Revision {
    store.current().unwrap().revision().clone()
}

fn tx(store: &GitStore, id: &str, ops: Vec<Operation>) -> Transaction {
    Transaction {
        id: id.into(),
        expected_revision: revision(store),
        operations: ops,
    }
}

fn type_record(kind_name: &str, content: &str) -> StoredRecord {
    let key = memory_hub_schema::type_key(kind_name);
    StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new(key, "__type__", content).unwrap()),
    }
}

fn envelope(
    key: &str,
    kind: &str,
    content: &str,
    title: Option<&str>,
    extensions: Value,
) -> StoredRecord {
    let mut env = Envelope::new(key, kind, content).unwrap();
    env.title = title.map(str::to_owned);
    if let Value::Object(map) = extensions {
        for (k, v) in map {
            env.extensions.insert(k, v);
        }
    }
    StoredRecord::Plaintext {
        envelope: Box::new(env),
    }
}

fn apply(
    store: &GitStore,
    id: &str,
    ops: Vec<Operation>,
) -> Result<memory_hub_store::ApplyResult, StoreError> {
    store.apply(&tx(store, id, ops))
}

// ---------------------------------------------------------------------------
// No type records → validation skipped
// ---------------------------------------------------------------------------

#[test]
fn no_type_records_allows_any_kind() {
    let (_dir, store) = setup();
    let record = envelope("notes/1", "note", "some content", Some("Title"), json!({}));
    let result = apply(&store, "tx1", vec![Operation::put(record)]).unwrap();
    assert!(!result.changed_keys.is_empty());
}

#[test]
fn no_type_records_allows_unknown_kind() {
    let (_dir, store) = setup();
    let record = envelope(
        "custom/1",
        "custom_kind",
        "content",
        Some("Title"),
        json!({}),
    );
    apply(&store, "tx1", vec![Operation::put(record)]).unwrap();
}

// ---------------------------------------------------------------------------
// Create type record
// ---------------------------------------------------------------------------

#[test]
fn create_type_record_succeeds() {
    let (_dir, store) = setup();
    let record = type_record("decision", DECISION_TYPE_CONTENT);
    apply(&store, "tx-create-type", vec![Operation::put(record)]).unwrap();

    let registry = load_registry(&store, &revision(&store)).unwrap();
    assert_eq!(registry.len(), 1);
    assert!(registry.get("decision").is_some());
}

#[test]
fn create_invalid_type_record_is_rejected() {
    let (_dir, store) = setup();
    let bad_content = r#"{"kind_name": "", "fields": {}}"#;
    let record = type_record("", bad_content);
    let error = apply(&store, "tx-bad-type", vec![Operation::put(record)]).unwrap_err();
    assert_eq!(error.kind, StoreErrorKind::InvalidRecord);
    assert!(error.data["reason"].as_str().unwrap().contains("kind_name"));
}

// ---------------------------------------------------------------------------
// After type is created, records of that kind are validated
// ---------------------------------------------------------------------------

#[test]
fn valid_record_after_type_creation_succeeds() {
    let (_dir, store) = setup();

    // Create the type first.
    apply(
        &store,
        "tx-create-type",
        vec![Operation::put(type_record(
            "decision",
            DECISION_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    // Now put a valid decision record.
    let record = envelope(
        "decisions/1",
        "decision",
        "We chose MCP because it is the public contract.",
        Some("Use MCP"),
        json!({ "rationale_summary": "Need a seam" }),
    );
    apply(&store, "tx-valid-record", vec![Operation::put(record)]).unwrap();
}

#[test]
fn reject_unknown_kind_in_strict_mode() {
    let (_dir, store) = setup();
    apply(
        &store,
        "tx-create-type",
        vec![Operation::put(type_record(
            "decision",
            DECISION_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    let record = envelope(
        "unknown/1",
        "unknown_kind",
        "content",
        Some("Title"),
        json!({}),
    );
    let error = apply(&store, "tx-unknown", vec![Operation::put(record)]).unwrap_err();
    assert_eq!(error.kind, StoreErrorKind::InvalidRecord);
    assert_eq!(error.data["kind"].as_str().unwrap(), "unknown_kind");
    assert!(
        error.data["reason"]
            .as_str()
            .unwrap()
            .contains("no type definition")
    );
}

#[test]
fn reject_missing_required_extension_field() {
    let (_dir, store) = setup();
    apply(
        &store,
        "tx-create-type",
        vec![Operation::put(type_record(
            "decision",
            DECISION_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    let record = envelope(
        "decisions/missing",
        "decision",
        "A sufficiently long content body here.",
        Some("Title"),
        json!({}), // missing rationale_summary
    );
    let error = apply(&store, "tx-missing", vec![Operation::put(record)]).unwrap_err();
    assert_eq!(error.kind, StoreErrorKind::InvalidRecord);
    assert_eq!(error.data["kind"].as_str().unwrap(), "decision");
    // jsonschema reports missing required properties at the object level, so
    // the field path is "extensions." — the property name is in the reason.
    let combined = format!(
        "{} {}",
        error.data["field"].as_str().unwrap(),
        error.data["reason"].as_str().unwrap()
    );
    assert!(
        combined.contains("rationale_summary"),
        "expected 'rationale_summary' in field or reason, got: {combined}"
    );
}

#[test]
fn reject_missing_required_envelope_title() {
    let (_dir, store) = setup();
    apply(
        &store,
        "tx-create-type",
        vec![Operation::put(type_record(
            "decision",
            DECISION_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    let record = envelope(
        "decisions/no-title",
        "decision",
        "A sufficiently long content body here.",
        None,
        json!({ "rationale_summary": "x" }),
    );
    let error = apply(&store, "tx-no-title", vec![Operation::put(record)]).unwrap_err();
    assert_eq!(error.kind, StoreErrorKind::InvalidRecord);
    assert_eq!(error.data["field"].as_str().unwrap(), "title");
}

#[test]
fn reject_wrong_enum_value() {
    let (_dir, store) = setup();
    apply(
        &store,
        "tx-create-spec-type",
        vec![Operation::put(type_record("spec", SPEC_TYPE_CONTENT))],
    )
    .unwrap();

    let record = envelope(
        "specs/broken",
        "spec",
        "Spec body content.",
        Some("Broken"),
        json!({ "status": "invalid_status" }),
    );
    let error = apply(&store, "tx-bad-enum", vec![Operation::put(record)]).unwrap_err();
    assert_eq!(error.kind, StoreErrorKind::InvalidRecord);
    assert!(error.data["field"].as_str().unwrap().contains("status"));
}

// ---------------------------------------------------------------------------
// strict=false fallback
// ---------------------------------------------------------------------------

#[test]
fn strict_false_accepts_unknown_kind() {
    let directory = tempfile::tempdir().unwrap();
    git2::Repository::init(directory.path()).unwrap();
    let store = GitStore::open(directory.path())
        .unwrap()
        .with_policy(Arc::new(SchemaPolicy::new(false)));

    apply(
        &store,
        "tx-create-type",
        vec![Operation::put(type_record(
            "decision",
            DECISION_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    let record = envelope(
        "unknown/1",
        "unknown_kind",
        "content",
        Some("Title"),
        json!({}),
    );
    apply(&store, "tx-unknown", vec![Operation::put(record)]).unwrap();
}

#[test]
fn strict_false_still_validates_known_kinds() {
    let directory = tempfile::tempdir().unwrap();
    git2::Repository::init(directory.path()).unwrap();
    let store = GitStore::open(directory.path())
        .unwrap()
        .with_policy(Arc::new(SchemaPolicy::new(false)));

    apply(
        &store,
        "tx-create-type",
        vec![Operation::put(type_record(
            "decision",
            DECISION_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    // Known kind but missing required field — still rejected even in non-strict.
    let record = envelope(
        "decisions/missing",
        "decision",
        "A sufficiently long content body here.",
        Some("Title"),
        json!({}),
    );
    let error = apply(&store, "tx-missing", vec![Operation::put(record)]).unwrap_err();
    assert_eq!(error.kind, StoreErrorKind::InvalidRecord);
}

// ---------------------------------------------------------------------------
// Update type record
// ---------------------------------------------------------------------------

#[test]
fn update_type_record_succeeds() {
    let (_dir, store) = setup();
    apply(
        &store,
        "tx-create-type",
        vec![Operation::put(type_record(
            "decision",
            DECISION_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    // Update with a modified definition (add a field).
    let updated_content = r#"{
      "kind_name": "decision",
      "description": "Updated description",
      "envelope": { "title": { "required": true } },
      "fields": {
        "rationale_summary": { "type": "string", "required": true },
        "impact": { "type": "string", "required": false }
      },
      "relationships": {
        "supersedes": { "target": "decision" },
        "references": { "target": "any" }
      }
    }"#;
    apply(
        &store,
        "tx-update-type",
        vec![Operation::put(type_record("decision", updated_content))],
    )
    .unwrap();

    let registry = load_registry(&store, &revision(&store)).unwrap();
    let def = registry.get("decision").unwrap();
    assert!(def.fields.contains_key("impact"));
}

// ---------------------------------------------------------------------------
// Delete type record
// ---------------------------------------------------------------------------

#[test]
fn delete_type_record_then_kind_rejected_in_strict_mode() {
    let (_dir, store) = setup();
    // Create two types so the registry is non-empty after deleting one.
    apply(
        &store,
        "tx-create-types",
        vec![
            Operation::put(type_record("decision", DECISION_TYPE_CONTENT)),
            Operation::put(type_record("spec", SPEC_TYPE_CONTENT)),
        ],
    )
    .unwrap();

    // Put a valid decision record.
    let record = envelope(
        "decisions/1",
        "decision",
        "A sufficiently long content body here.",
        Some("Title"),
        json!({ "rationale_summary": "x" }),
    );
    apply(&store, "tx-valid", vec![Operation::put(record)]).unwrap();

    // Delete the decision type.
    let type_key = memory_hub_schema::type_key("decision");
    apply(
        &store,
        "tx-delete-type",
        vec![Operation::delete(RecordId::plaintext(type_key))],
    )
    .unwrap();

    let registry = load_registry(&store, &revision(&store)).unwrap();
    assert_eq!(registry.len(), 1);
    assert!(registry.get("decision").is_none());
    assert!(registry.get("spec").is_some());

    // Now a new decision record should be rejected in strict mode (unknown kind).
    let record = envelope(
        "decisions/2",
        "decision",
        "Another sufficiently long body.",
        Some("Title"),
        json!({ "rationale_summary": "y" }),
    );
    let error = apply(&store, "tx-after-delete", vec![Operation::put(record)]).unwrap_err();
    assert_eq!(error.kind, StoreErrorKind::InvalidRecord);
    assert_eq!(error.data["kind"].as_str().unwrap(), "decision");
}

// ---------------------------------------------------------------------------
// Multiple type records in one transaction
// ---------------------------------------------------------------------------

#[test]
fn multiple_types_in_one_transaction() {
    let (_dir, store) = setup();
    apply(
        &store,
        "tx-multi-types",
        vec![
            Operation::put(type_record("decision", DECISION_TYPE_CONTENT)),
            Operation::put(type_record("spec", SPEC_TYPE_CONTENT)),
        ],
    )
    .unwrap();

    let registry = load_registry(&store, &revision(&store)).unwrap();
    assert_eq!(registry.len(), 2);
    assert!(registry.get("decision").is_some());
    assert!(registry.get("spec").is_some());
}

// ---------------------------------------------------------------------------
// Existing records not re-validated on type update
// ---------------------------------------------------------------------------

#[test]
fn existing_records_not_revalidated_on_type_update() {
    let (_dir, store) = setup();
    apply(
        &store,
        "tx-create-type",
        vec![Operation::put(type_record(
            "decision",
            DECISION_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    // Put a valid record.
    let record = envelope(
        "decisions/1",
        "decision",
        "A sufficiently long content body here.",
        Some("Title"),
        json!({ "rationale_summary": "x" }),
    );
    apply(&store, "tx-valid", vec![Operation::put(record)]).unwrap();

    // Update the type to make rationale_summary required AND add a new required field.
    // The existing record doesn't have the new field, but it should NOT be rejected
    // because existing records are validated only on write, not on read.
    let updated_content = r#"{
      "kind_name": "decision",
      "envelope": { "title": { "required": true } },
      "fields": {
        "rationale_summary": { "type": "string", "required": true },
        "impact": { "type": "string", "required": true }
      },
      "relationships": {}
    }"#;
    apply(
        &store,
        "tx-update-type",
        vec![Operation::put(type_record("decision", updated_content))],
    )
    .unwrap();

    // The existing record is still readable.
    let snapshot = store.current().unwrap();
    let existing = snapshot.get(&RecordId::plaintext("decisions/1")).unwrap();
    assert!(existing.is_some());
}

// ---------------------------------------------------------------------------
// Cross-type relationship validation in registry
// ---------------------------------------------------------------------------

/// A type pointing at a kind nothing defines is refused **as it is written**.
///
/// It used to be written successfully and to poison the corpus instead: the
/// registry was built from the previous revision, so nothing checked the
/// definition arriving, and every *later* transaction failed while building a
/// registry that now included it. The failure named a type the caller had not
/// touched, in a write that had nothing to do with it.
///
/// Refusing it here is the same rule stated at the moment it can still be
/// acted on, and it falls out of validating a transaction against the state it
/// produces rather than the one before it.
#[test]
fn type_with_dangling_relationship_target_rejected() {
    let (_dir, store) = setup();
    let bad_content = r#"{
      "kind_name": "orphan",
      "fields": {},
      "relationships": {
        "parent": { "target": "nonexistent_kind" }
      }
    }"#;
    let record = type_record("orphan", bad_content);

    let error = apply(&store, "tx-orphan", vec![Operation::put(record)]).unwrap_err();
    assert_eq!(error.kind, StoreErrorKind::InvalidRecord);
    assert!(
        error.data["reason"]
            .as_str()
            .unwrap()
            .contains("nonexistent_kind")
    );

    // And the corpus is untouched, so the next write is about its own subject.
    let after = envelope("any/1", "any_kind", "content", Some("T"), json!({}));
    apply(&store, "tx-after-orphan", vec![Operation::put(after)]).unwrap();
}

// ---------------------------------------------------------------------------
// ValidationError data contains kind, field, reason
// ---------------------------------------------------------------------------

#[test]
fn validation_error_contains_kind_field_reason() {
    let (_dir, store) = setup();
    apply(
        &store,
        "tx-create-type",
        vec![Operation::put(type_record(
            "decision",
            DECISION_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    let record = envelope(
        "decisions/bad",
        "decision",
        "short", // below min_length 10
        Some("Title"),
        json!({}),
    );
    let error = apply(&store, "tx-bad", vec![Operation::put(record)]).unwrap_err();
    assert_eq!(error.kind, StoreErrorKind::InvalidRecord);
    assert!(error.data["kind"].is_string());
    assert!(error.data["field"].is_string());
    assert!(error.data["reason"].is_string());
}

// ---------------------------------------------------------------------------
// Empty registry when no type records
// ---------------------------------------------------------------------------

#[test]
fn load_schema_registry_empty_when_no_types() {
    let (_dir, store) = setup();
    let record = envelope("notes/1", "note", "content", Some("Title"), json!({}));
    apply(&store, "tx1", vec![Operation::put(record)]).unwrap();

    let registry = load_registry(&store, &revision(&store)).unwrap();
    assert!(registry.is_empty());
}

// ---------------------------------------------------------------------------
// Type record with content as type definition JSON
// ---------------------------------------------------------------------------

#[test]
fn type_record_uses_type_kind_and_key_prefix() {
    let (_dir, store) = setup();
    apply(
        &store,
        "tx-create-type",
        vec![Operation::put(type_record(
            "decision",
            DECISION_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    // The type record should be readable by its key.
    let snapshot = store.current().unwrap();
    let type_key = memory_hub_schema::type_key("decision");
    let stored = snapshot
        .get(&RecordId::plaintext(&type_key))
        .unwrap()
        .unwrap();
    let StoredRecord::Plaintext { envelope } = stored;
    assert_eq!(envelope.kind, "__type__");
    assert!(envelope.content.contains("decision"));
}

// ---------------------------------------------------------------------------
// Schema validation with links
// ---------------------------------------------------------------------------

#[test]
fn reject_undeclared_link_relation() {
    let (_dir, store) = setup();
    apply(
        &store,
        "tx-create-type",
        vec![Operation::put(type_record(
            "decision",
            DECISION_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    let mut env = Envelope::new(
        "decisions/bad-link",
        "decision",
        "A sufficiently long content body here.",
    )
    .unwrap();
    env.title = Some("Bad Link".into());
    env.extensions
        .insert("rationale_summary".into(), json!("x"));
    env.links.push(memory_hub_core::RecordLink {
        key: "specs/foo".into(),
        relation: Some("depends_on".into()),
        extensions: BTreeMap::new(),
    });

    let record = StoredRecord::Plaintext {
        envelope: Box::new(env),
    };
    let error = apply(&store, "tx-bad-link", vec![Operation::put(record)]).unwrap_err();
    assert_eq!(error.kind, StoreErrorKind::InvalidRecord);
    assert!(
        error.data["reason"]
            .as_str()
            .unwrap()
            .contains("depends_on")
    );
}

// ---------------------------------------------------------------------------
// Dangling relationship target: refuse to create, recover from one left behind
// ---------------------------------------------------------------------------

/// A type whose `relationships.parent.target` names a kind that is separate.
const REFERENCING_TYPE_CONTENT: &str = r#"{
  "kind_name": "referencing",
  "relationships": {
    "parent": { "target": "referenced" }
  }
}"#;

const REFERENCED_TYPE_CONTENT: &str = r#"{ "kind_name": "referenced" }"#;

#[test]
fn deleting_a_type_referenced_by_another_is_allowed() {
    let (_dir, store) = setup();
    apply(
        &store,
        "tx-create",
        vec![
            Operation::put(type_record("referencing", REFERENCING_TYPE_CONTENT)),
            Operation::put(type_record("referenced", REFERENCED_TYPE_CONTENT)),
        ],
    )
    .unwrap();

    // The corpus is clean; deleting `referenced` would leave
    // `referencing.relationships.parent.target` pointing at a kind that no
    // longer exists. The person who owns the project owns its types: removing
    // one that another still names is a decision, not a mistake, and a
    // delete-only transaction is allowed to leave a dangling target. The
    // lenient constructor records it rather than refusing the delete.
    let type_key = memory_hub_schema::type_key("referenced");
    apply(
        &store,
        "tx-delete-referenced",
        vec![Operation::delete(RecordId::plaintext(type_key))],
    )
    .unwrap();

    // The registry is now broken — `referencing` still names `referenced` as
    // a target — and that is the honest state to leave it in. Healing it is
    // removing the referencing type, which the same rule allows.
    let registry = load_registry(&store, &revision(&store)).unwrap();
    assert!(registry.is_broken());
    assert_eq!(
        registry.dangling_targets(),
        &[memory_hub_schema::DanglingTarget {
            kind: "referencing".to_owned(),
            relation: "parent".to_owned(),
            target: "referenced".to_owned(),
        }]
    );

    // Healing: removing the type that carries the dangling target clears it.
    let referencing_key = memory_hub_schema::type_key("referencing");
    apply(
        &store,
        "tx-heal",
        vec![Operation::delete(RecordId::plaintext(referencing_key))],
    )
    .unwrap();
    let registry = load_registry(&store, &revision(&store)).unwrap();
    assert!(!registry.is_broken());
    assert!(registry.is_empty());
}

#[test]
fn broken_corpus_can_still_be_read_and_healed() {
    let directory = tempfile::tempdir().unwrap();
    git2::Repository::init(directory.path()).unwrap();
    // A store with no policy enforces nothing, so it accepts the broken
    // corpus directly: a type whose relationship target names a kind that was
    // never written. This is the state a deletion that predates the guard
    // above would leave behind, and it is what locked the system: every reader
    // of the registry failed, and so did the policy on the next transaction.
    let raw = GitStore::open(directory.path()).unwrap();
    apply(
        &raw,
        "tx-broken",
        vec![Operation::put(type_record(
            "referencing",
            REFERENCING_TYPE_CONTENT,
        ))],
    )
    .unwrap();

    // A store carrying the policy can still read this corpus — the strict
    // constructor fails on the dangling target, the lenient fallback builds and
    // records it, so `load_registry` does not lock every reader out.
    let store = GitStore::open(directory.path())
        .unwrap()
        .with_policy(Arc::new(SchemaPolicy::default()));
    let registry = load_registry(&store, &revision(&store)).unwrap();
    assert!(registry.is_broken());
    assert!(registry.get("referencing").is_some());

    // The offending type can be removed, healing the corpus. On a broken
    // corpus the policy relaxes to lenient, so the delete that fixes the state
    // is not refused for the state it is fixing.
    let type_key = memory_hub_schema::type_key("referencing");
    apply(
        &store,
        "tx-heal",
        vec![Operation::delete(RecordId::plaintext(type_key))],
    )
    .unwrap();
    let registry = load_registry(&store, &revision(&store)).unwrap();
    assert!(!registry.is_broken());
    assert!(registry.is_empty());
}
