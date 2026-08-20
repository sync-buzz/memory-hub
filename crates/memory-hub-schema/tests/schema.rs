//! Integration tests for `memory-hub-schema`.
//!
//! Covers the acceptance checklist of `s-schema-crate-validation`:
//! valid record, missing required field, wrong enum value, unknown
//! relationship, invalid type record, plus registry strict mode and
//! cross-type link target resolution.

#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;

use memory_hub_core::{Envelope, RecordLink};
use memory_hub_schema::{
    KindResolver, SchemaRegistry, TypeDefinition, ValidationErrorKind, type_key,
};
use serde_json::{Value, json};

const DECISION_TYPE: &str = r#"{
  "kind_name": "decision",
  "description": "A chosen path among alternatives",
  "guidance": "Record decisions the moment they are settled.",
  "envelope": {
    "title": { "required": true },
    "content": { "required": true, "min_length": 10 }
  },
  "fields": {
    "rationale_summary": {
      "type": "string",
      "required": true,
      "description": "One-line summary of why this decision was made"
    },
    "supersedes_key": {
      "type": "string",
      "required": false
    }
  },
  "relationships": {
    "supersedes": { "target": "decision", "description": "Replaces an older one" },
    "references": { "target": "any", "description": "Supporting context" }
  }
}"#;

const SPEC_TYPE: &str = r#"{
  "kind_name": "spec",
  "description": "A unit of work with acceptance criteria",
  "envelope": {
    "title": { "required": true },
    "content": { "required": true }
  },
  "fields": {
    "status": {
      "type": "enum",
      "values": ["backlog", "todo", "in_progress", "in_review", "done", "canceled"],
      "required": true
    },
    "priority": {
      "type": "enum",
      "values": ["critical", "high", "medium", "low"],
      "required": false,
      "default": "medium"
    },
    "milestone_key": { "type": "string", "required": false }
  },
  "relationships": {
    "depends_on": { "target": "spec", "description": "Must complete first" },
    "milestone": { "target": "milestone", "description": "Parent milestone" }
  }
}"#;

const MILESTONE_TYPE: &str = r#"{
  "kind_name": "milestone",
  "description": "A milestone grouping specs",
  "envelope": { "title": { "required": true } },
  "fields": {},
  "relationships": {}
}"#;

fn decision_type() -> TypeDefinition {
    TypeDefinition::from_content(DECISION_TYPE).unwrap()
}

fn spec_type() -> TypeDefinition {
    TypeDefinition::from_content(SPEC_TYPE).unwrap()
}

fn milestone_type() -> TypeDefinition {
    TypeDefinition::from_content(MILESTONE_TYPE).unwrap()
}

fn envelope_with_extensions(
    kind: &str,
    key: &str,
    title: Option<&str>,
    content: &str,
    extensions: Value,
    links: Vec<RecordLink>,
) -> Envelope {
    let mut envelope = Envelope::new(key, kind, content).unwrap();
    envelope.title = title.map(str::to_owned);
    envelope.links = links;
    if let Value::Object(map) = extensions {
        for (k, v) in map {
            envelope.extensions.insert(k, v);
        }
    }
    envelope
}

// ---------------------------------------------------------------------------
// Type definition parsing and self-validation
// ---------------------------------------------------------------------------

#[test]
fn parses_and_validates_a_well_formed_type_record() {
    let definition = decision_type();
    definition.validate_self().unwrap();
    assert_eq!(definition.kind_name, "decision");
    assert_eq!(definition.fields.len(), 2);
    assert_eq!(definition.relationships.len(), 2);
}

#[test]
fn type_key_round_trips() {
    assert_eq!(type_key("decision"), "__type__:decision");
}

#[test]
fn rejects_empty_kind_name() {
    let mut definition = decision_type();
    definition.kind_name = String::new();
    let error = definition.validate_self().unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidTypeDefinition);
    assert_eq!(error.field, "kind_name");
}

#[test]
fn rejects_enum_field_without_values() {
    let content = r#"{
      "kind_name": "broken",
      "fields": {
        "status": { "type": "enum", "values": [], "required": true }
      }
    }"#;
    let definition = TypeDefinition::from_content(content).unwrap();
    let error = definition.validate_self().unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidTypeDefinition);
    assert!(error.field.contains("values"));
}

#[test]
fn rejects_duplicate_enum_values() {
    let content = r#"{
      "kind_name": "broken",
      "fields": {
        "level": { "type": "enum", "values": ["a", "a"], "required": true }
      }
    }"#;
    let definition = TypeDefinition::from_content(content).unwrap();
    let error = definition.validate_self().unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidTypeDefinition);
    assert!(error.message.contains("duplicate"));
}

#[test]
fn rejects_unknown_envelope_constraint_field() {
    let content = r#"{
      "kind_name": "broken",
      "envelope": {
        "key": { "required": true }
      }
    }"#;
    let definition = TypeDefinition::from_content(content).unwrap();
    let error = definition.validate_self().unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidTypeDefinition);
    assert_eq!(error.field, "envelope.key");
}

#[test]
fn rejects_relationship_with_empty_target() {
    let content = r#"{
      "kind_name": "broken",
      "relationships": {
        "references": { "target": "" }
      }
    }"#;
    let definition = TypeDefinition::from_content(content).unwrap();
    let error = definition.validate_self().unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidTypeDefinition);
    assert!(error.field.contains("target"));
}

// ---------------------------------------------------------------------------
// JSON Schema generation
// ---------------------------------------------------------------------------

#[test]
fn generates_json_schema_with_required_and_enum() {
    let definition = spec_type();
    let schema = definition.build_json_schema();
    assert_eq!(schema["type"], "object");
    let required = schema["required"].as_array().unwrap();
    assert!(required.contains(&json!("status")));
    assert!(!required.contains(&json!("priority")));
    assert_eq!(
        schema["properties"]["status"]["enum"],
        json!([
            "backlog",
            "todo",
            "in_progress",
            "in_review",
            "done",
            "canceled"
        ])
    );
}

#[test]
fn empty_fields_produce_empty_object_schema() {
    let definition = milestone_type();
    let schema = definition.build_json_schema();
    assert_eq!(schema["type"], "object");
    assert!(schema.get("properties").is_none());
    assert!(schema.get("required").is_none());
}

// ---------------------------------------------------------------------------
// Envelope validation
// ---------------------------------------------------------------------------

#[test]
fn valid_record_passes_full_validation() {
    let definition = decision_type();
    let extensions = json!({ "rationale_summary": "Need a seam" });
    let envelope = envelope_with_extensions(
        "decision",
        "decisions/seam",
        Some("Use MCP seam"),
        "We chose MCP because it is the public contract.",
        extensions,
        vec![],
    );
    definition.validate(&envelope).unwrap();
}

#[test]
fn rejects_missing_required_envelope_title() {
    let definition = decision_type();
    let extensions = json!({ "rationale_summary": "x" });
    let envelope = envelope_with_extensions(
        "decision",
        "decisions/missing",
        None,
        "A sufficiently long content body here.",
        extensions,
        vec![],
    );
    let error = definition.validate(&envelope).unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidEnvelope);
    assert_eq!(error.field, "title");
}

#[test]
fn rejects_content_below_min_length() {
    let definition = decision_type();
    let extensions = json!({ "rationale_summary": "x" });
    let envelope = envelope_with_extensions(
        "decision",
        "decisions/short",
        Some("Short"),
        "tiny",
        extensions,
        vec![],
    );
    let error = definition.validate(&envelope).unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidEnvelope);
    assert_eq!(error.field, "content");
}

// ---------------------------------------------------------------------------
// Extensions validation
// ---------------------------------------------------------------------------

#[test]
fn rejects_missing_required_extension_field() {
    let definition = spec_type();
    let extensions = json!({ "priority": "high" });
    let envelope = envelope_with_extensions(
        "spec",
        "specs/feature",
        Some("Feature"),
        "Body of the spec.",
        extensions,
        vec![],
    );
    let error = definition.validate(&envelope).unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidExtensions);
    assert!(error.field.starts_with("extensions"));
    assert!(error.message.contains("status") || error.field.contains("status"));
}

#[test]
fn rejects_wrong_enum_value_in_extensions() {
    let definition = spec_type();
    let extensions = json!({ "status": "invalid_status", "priority": "high" });
    let envelope = envelope_with_extensions(
        "spec",
        "specs/broken",
        Some("Broken"),
        "Body of the spec.",
        extensions,
        vec![],
    );
    let error = definition.validate(&envelope).unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidExtensions);
    assert!(error.field.contains("status"));
}

#[test]
fn rejects_wrong_type_in_extension_field() {
    let definition = spec_type();
    let extensions = json!({ "status": "todo", "priority": 42 });
    let envelope = envelope_with_extensions(
        "spec",
        "specs/types",
        Some("Types"),
        "Body of the spec.",
        extensions,
        vec![],
    );
    let error = definition.validate(&envelope).unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidExtensions);
    assert!(error.field.contains("priority"));
}

#[test]
fn allows_optional_extension_to_be_absent() {
    let definition = spec_type();
    let extensions = json!({ "status": "todo" });
    let envelope = envelope_with_extensions(
        "spec",
        "specs/minimal",
        Some("Minimal"),
        "Body of the spec.",
        extensions,
        vec![],
    );
    definition.validate(&envelope).unwrap();
}

// ---------------------------------------------------------------------------
// Links validation
// ---------------------------------------------------------------------------

#[test]
fn rejects_undeclared_link_relation() {
    let definition = decision_type();
    let extensions = json!({ "rationale_summary": "x" });
    let link = RecordLink {
        key: "specs/foo".into(),
        relation: Some("depends_on".into()),
        extensions: BTreeMap::new(),
    };
    let envelope = envelope_with_extensions(
        "decision",
        "decisions/bad-link",
        Some("Bad Link"),
        "A sufficiently long content body here.",
        extensions,
        vec![link],
    );
    let error = definition.validate(&envelope).unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidLinks);
    assert!(error.field.starts_with("links["));
    assert!(error.message.contains("depends_on"));
}

#[test]
fn allows_declared_link_relation_without_target_check() {
    let definition = decision_type();
    let extensions = json!({ "rationale_summary": "x" });
    let link = RecordLink {
        key: "decisions/old".into(),
        relation: Some("supersedes".into()),
        extensions: BTreeMap::new(),
    };
    let envelope = envelope_with_extensions(
        "decision",
        "decisions/new",
        Some("New Decision"),
        "A sufficiently long content body here.",
        extensions,
        vec![link],
    );
    definition.validate(&envelope).unwrap();
}

// ---------------------------------------------------------------------------
// SchemaRegistry
// ---------------------------------------------------------------------------

fn full_registry() -> SchemaRegistry {
    SchemaRegistry::from_type_definitions([decision_type(), spec_type(), milestone_type()]).unwrap()
}

struct MapResolver {
    kinds: BTreeMap<String, String>,
}

impl KindResolver for MapResolver {
    fn resolve_kind(&self, key: &str) -> Option<String> {
        self.kinds.get(key).cloned()
    }
}

#[test]
fn registry_looks_up_type_by_kind() {
    let registry = full_registry();
    assert!(registry.get("decision").is_some());
    assert!(registry.get("spec").is_some());
    assert!(registry.get("milestone").is_some());
    assert!(registry.get("nonexistent").is_none());
    assert_eq!(registry.len(), 3);
}

#[test]
fn registry_rejects_dangling_relationship_target() {
    let definition = TypeDefinition::from_content(
        r#"{
          "kind_name": "orphan",
          "relationships": {
            "parent": { "target": "missing_kind" }
          }
        }"#,
    )
    .unwrap();
    let error = SchemaRegistry::from_type_definitions([definition]).unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidTypeDefinition);
    assert!(error.field.contains("target"));
    assert!(error.message.contains("missing_kind"));
}

#[test]
fn registry_rejects_duplicate_kind() {
    let error =
        SchemaRegistry::from_type_definitions([decision_type(), decision_type()]).unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidTypeDefinition);
    assert!(error.message.contains("duplicate"));
}

#[test]
fn strict_mode_rejects_unknown_kind() {
    let registry = full_registry();
    let envelope = envelope_with_extensions(
        "unknown_kind",
        "unknown/foo",
        Some("Title"),
        "Some content body.",
        json!({}),
        vec![],
    );
    let error = registry.validate_record(&envelope, true).unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::UnknownKind);
}

#[test]
fn non_strict_mode_accepts_unknown_kind() {
    let registry = full_registry();
    let envelope = envelope_with_extensions(
        "unknown_kind",
        "unknown/foo",
        Some("Title"),
        "Some content body.",
        json!({}),
        vec![],
    );
    registry.validate_record(&envelope, false).unwrap();
}

#[test]
fn resolver_rejects_mismatched_link_target_kind() {
    let registry = full_registry();
    let resolver = MapResolver {
        kinds: BTreeMap::from([
            ("specs/foo".into(), "spec".into()),
            ("decisions/old".into(), "decision".into()),
        ]),
    };
    let extensions = json!({ "rationale_summary": "x" });
    let link = RecordLink {
        key: "specs/foo".into(),
        relation: Some("supersedes".into()),
        extensions: BTreeMap::new(),
    };
    let envelope = envelope_with_extensions(
        "decision",
        "decisions/mismatched",
        Some("Mismatched"),
        "A sufficiently long content body here.",
        extensions,
        vec![link],
    );
    let error = registry
        .validate_record_with_resolver(&envelope, true, &resolver)
        .unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidLinks);
    assert!(error.message.contains("spec"));
    assert!(error.message.contains("decision"));
}

#[test]
fn resolver_accepts_matching_link_target_kind() {
    let registry = full_registry();
    let resolver = MapResolver {
        kinds: BTreeMap::from([("decisions/old".into(), "decision".into())]),
    };
    let extensions = json!({ "rationale_summary": "x" });
    let link = RecordLink {
        key: "decisions/old".into(),
        relation: Some("supersedes".into()),
        extensions: BTreeMap::new(),
    };
    let envelope = envelope_with_extensions(
        "decision",
        "decisions/new",
        Some("New"),
        "A sufficiently long content body here.",
        extensions,
        vec![link],
    );
    registry
        .validate_record_with_resolver(&envelope, true, &resolver)
        .unwrap();
}

#[test]
fn resolver_accepts_any_target_kind() {
    let registry = full_registry();
    let resolver = MapResolver {
        kinds: BTreeMap::from([("specs/foo".into(), "spec".into())]),
    };
    let extensions = json!({ "rationale_summary": "x" });
    let link = RecordLink {
        key: "specs/foo".into(),
        relation: Some("references".into()),
        extensions: BTreeMap::new(),
    };
    let envelope = envelope_with_extensions(
        "decision",
        "decisions/ref",
        Some("Ref"),
        "A sufficiently long content body here.",
        extensions,
        vec![link],
    );
    registry
        .validate_record_with_resolver(&envelope, true, &resolver)
        .unwrap();
}

#[test]
fn empty_registry_accepts_everything_in_non_strict_mode() {
    let registry = SchemaRegistry::new();
    let envelope = envelope_with_extensions(
        "anything",
        "any/key",
        Some("Title"),
        "Content body.",
        json!({}),
        vec![],
    );
    registry.validate_record(&envelope, false).unwrap();
}

#[test]
fn array_field_type_validates_items() {
    let content = r#"{
      "kind_name": "tagged",
      "fields": {
        "labels": {
          "type": "array",
          "items": { "type": "string" },
          "required": true
        }
      }
    }"#;
    let definition = TypeDefinition::from_content(content).unwrap();
    definition.validate_self().unwrap();

    let valid = envelope_with_extensions(
        "tagged",
        "t/1",
        Some("T"),
        "body",
        json!({ "labels": ["a", "b"] }),
        vec![],
    );
    definition.validate(&valid).unwrap();

    let invalid = envelope_with_extensions(
        "tagged",
        "t/2",
        Some("T"),
        "body",
        json!({ "labels": [1, 2] }),
        vec![],
    );
    let error = definition.validate(&invalid).unwrap_err();
    assert_eq!(error.kind, ValidationErrorKind::InvalidExtensions);
}

// --- Storage reference ----------------------------------------------------

use memory_hub_schema::TypeStorage;

fn type_with_storage(kind_name: &str, storage: &Value) -> TypeDefinition {
    TypeDefinition::from_content(&json!({"kind_name": kind_name, "storage": storage}).to_string())
        .unwrap()
}

#[test]
fn a_type_that_names_no_storage_keeps_its_content() {
    let definition =
        TypeDefinition::from_content(&json!({"kind_name": "note"}).to_string()).unwrap();
    let storage = definition.storage().unwrap();

    assert_eq!(storage, TypeStorage::WithRecords);
    assert!(
        !storage.is_external(),
        "content sits in the record, which is what every type was before \
         storage became a choice"
    );
    assert_eq!(storage.name(), None);
}

#[test]
fn a_type_names_the_storage_its_content_lives_in() {
    let definition = type_with_storage("guide", &json!("docs"));
    let storage = definition.storage().unwrap();

    assert_eq!(storage, TypeStorage::Named("docs".into()));
    assert_eq!(storage.name(), Some("docs"));
    assert!(
        storage.is_external(),
        "the bytes are somewhere the record is not"
    );
}

#[test]
fn the_name_is_checked_for_shape_and_nothing_else() {
    // Whether a storage called `docs` exists is a question about the project,
    // and the schema has never seen the project. Shape is all it can answer.
    for name in [
        "",
        "Docs",
        "9docs",
        "-docs",
        "docs/nested",
        "docs.md",
        "докс",
    ] {
        let definition = type_with_storage("guide", &json!(name));
        let Err(error) = definition.storage() else {
            panic!("`{name}` is not a storage name");
        };
        assert_eq!(error.field, "storage");
    }

    for name in ["docs", "main", "media-2", "long_name"] {
        assert!(
            type_with_storage("guide", &json!(name)).storage().is_ok(),
            "`{name}` is a storage name"
        );
    }
}

#[test]
fn the_type_registry_cannot_choose_where_it_lives() {
    // Reading a storage name means reading the registry, and reading the
    // registry means already knowing where it is.
    let definition = type_with_storage("__type__", &json!("docs"));
    let error = definition.storage().unwrap_err();

    assert_eq!(error.field, "storage");
    assert_eq!(error.kind, ValidationErrorKind::InvalidTypeDefinition);
}

#[test]
fn a_storage_name_survives_the_round_trip() {
    let definition = type_with_storage("guide", &json!("docs"));
    let text = serde_json::to_string(&definition).unwrap();
    let read_back = TypeDefinition::from_content(&text).unwrap();

    assert_eq!(read_back.storage, Some("docs".to_owned()));
    assert_eq!(read_back.storage().unwrap(), definition.storage().unwrap());
}

#[test]
fn a_type_that_names_no_storage_writes_no_storage_field() {
    let definition =
        TypeDefinition::from_content(&json!({"kind_name": "note"}).to_string()).unwrap();
    let wire = serde_json::to_value(&definition).unwrap();

    assert!(
        wire.get("storage").is_none(),
        "absent rather than spelled out as a default that could drift: {wire}"
    );
}
