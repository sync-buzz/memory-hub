//! Schema MCP integration tests.
//!
//! Covers the acceptance checklist of `s-schema-mcp-instructions`:
//! instructions generation (built-in + schema text), schema resources,
//! `memory_schema_status`, and `memory_list_types`.

#![allow(clippy::unwrap_used)]

use memory_hub_core::{Envelope, StoredRecord};
use memory_hub_engine::{Operation, RecordStore};
use memory_hub_schema::{SchemaRegistry, TypeDefinition};
use memory_hub_store::GitStore;
use serde_json::{Value, json};

/// A repository these tests keep their records in.
fn init_git(path: &std::path::Path) {
    let status = std::process::Command::new("git")
        .args(["init", "--quiet"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

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
      "description": "One-line summary of why"
    }
  },
  "relationships": {
    "supersedes": { "target": "decision" },
    "references": { "target": "any" }
  }
}"#;

const SPEC_TYPE: &str = r#"{
  "kind_name": "spec",
  "description": "A unit of work",
  "envelope": { "title": { "required": true } },
  "fields": {
    "status": {
      "type": "enum",
      "values": ["todo", "done"],
      "required": true
    }
  },
  "relationships": {
    "depends_on": { "target": "spec" }
  }
}"#;

// ---------------------------------------------------------------------------
// schema_instructions module tests
// ---------------------------------------------------------------------------

fn registry_with_types() -> SchemaRegistry {
    let decision = TypeDefinition::from_content(DECISION_TYPE).unwrap();
    let spec = TypeDefinition::from_content(SPEC_TYPE).unwrap();
    SchemaRegistry::from_type_definitions([decision, spec]).unwrap()
}

#[test]
fn schema_instructions_empty_for_empty_registry() {
    let registry = SchemaRegistry::new();
    let text = memory_hub_mcp::schema_instructions(&registry);
    assert!(text.is_empty());
}

#[test]
fn schema_instructions_contains_type_names() {
    let registry = registry_with_types();
    let text = memory_hub_mcp::schema_instructions(&registry);
    assert!(text.contains("### decision"));
    assert!(text.contains("### spec"));
    assert!(text.contains("## Document Types"));
}

#[test]
fn schema_instructions_contains_descriptions_and_guidance() {
    let registry = registry_with_types();
    let text = memory_hub_mcp::schema_instructions(&registry);
    assert!(text.contains("A chosen path among alternatives"));
    assert!(text.contains("Record decisions the moment they are settled."));
}

#[test]
fn schema_instructions_contains_required_envelope_fields() {
    let registry = registry_with_types();
    let text = memory_hub_mcp::schema_instructions(&registry);
    assert!(text.contains("Required envelope fields: title, content"));
}

#[test]
fn schema_instructions_contains_semantic_fields() {
    let registry = registry_with_types();
    let text = memory_hub_mcp::schema_instructions(&registry);
    assert!(text.contains("rationale_summary"));
    assert!(text.contains("string, required"));
    assert!(text.contains("enum: todo | done"));
}

#[test]
fn schema_instructions_contains_relationships() {
    let registry = registry_with_types();
    let text = memory_hub_mcp::schema_instructions(&registry);
    assert!(text.contains("supersedes → decision"));
    assert!(text.contains("references → any"));
    assert!(text.contains("depends_on → spec"));
}

// ---------------------------------------------------------------------------
// Schema resource tests
// ---------------------------------------------------------------------------

#[test]
fn schema_resource_returns_all_types() {
    let registry = registry_with_types();
    let resource = memory_hub_mcp::schema_resource(&registry);
    assert_eq!(resource["schemaVersion"], 1);
    assert_eq!(resource["typeCount"], 2);
    let types = resource["types"].as_array().unwrap();
    assert_eq!(types.len(), 2);
}

#[test]
fn single_type_resource_returns_one_definition() {
    let registry = registry_with_types();
    let definition = registry.get("decision").unwrap();
    let resource = memory_hub_mcp::single_type_resource(definition);
    assert_eq!(resource["schemaVersion"], 1);
    assert_eq!(resource["kindName"], "decision");
    assert!(resource["typeDefinition"]["kind_name"].is_string());
}

// ---------------------------------------------------------------------------
// MCP Session integration tests
// ---------------------------------------------------------------------------

fn setup_project() -> (tempfile::TempDir, memory_hub_mcp::Session) {
    let project = tempfile::tempdir().unwrap();
    init_git(project.path());
    let store = GitStore::open(project.path()).unwrap();
    let snapshot = store.current().unwrap();
    let revision = snapshot.revision().clone();

    // Write type records.
    let decision_type_record = StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new("__type__:decision", "__type__", DECISION_TYPE).unwrap()),
    };
    let spec_type_record = StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new("__type__:spec", "__type__", SPEC_TYPE).unwrap()),
    };
    let tx = memory_hub_store::Transaction {
        id: "setup-types".into(),
        expected_revision: revision,
        operations: vec![
            Operation::put(decision_type_record),
            Operation::put(spec_type_record),
        ],
    };
    store.apply(&tx).unwrap();

    let session = memory_hub_mcp::Session::new(
        project.path().to_path_buf(),
        memory_hub_service::RecordsIn::GitMetadata,
    );
    (project, session)
}

#[test]
fn initialize_includes_schema_instructions_when_types_exist() {
    let (_dir, mut session) = setup_project();
    let init_result = session.initialize(&json!({
        "protocolVersion": memory_hub_mcp::MCP_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "test", "version": "1"},
        "_meta": {"memoryHub": {"memoryInterfaceVersion": {"major": memory_hub_mcp::MEMORY_INTERFACE_MAJOR, "minor": 0}}}
    }));
    let result = init_result.unwrap();
    let instructions = result["instructions"].as_str().unwrap();
    assert!(instructions.contains("## Document Types"));
    assert!(instructions.contains("### decision"));
    assert!(instructions.contains("### spec"));
    assert!(instructions.contains("A chosen path among alternatives"));
}

#[test]
fn initialize_without_types_has_only_builtin_instructions() {
    let project = tempfile::tempdir().unwrap();
    init_git(project.path());
    let mut session = memory_hub_mcp::Session::new(
        project.path().to_path_buf(),
        memory_hub_service::RecordsIn::GitMetadata,
    );
    let result = session.initialize(&json!({
        "protocolVersion": memory_hub_mcp::MCP_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "test", "version": "1"},
        "_meta": {"memoryHub": {"memoryInterfaceVersion": {"major": memory_hub_mcp::MEMORY_INTERFACE_MAJOR, "minor": 0}}}
    }));
    let result = result.unwrap();
    let instructions = result["instructions"].as_str().unwrap();
    assert!(!instructions.contains("## Document Types"));
    assert!(instructions.contains("Memory Hub"));
}

#[test]
fn memory_schema_resource_returns_all_types() {
    let (_dir, mut session) = setup_project();
    session.initialized = true;
    let result = session.read_resource(&json!({"uri": "memory://schema"}));
    let response = result.unwrap();
    let text = &response["contents"][0]["text"];
    let content: Value = serde_json::from_str(text.as_str().unwrap()).unwrap();
    assert_eq!(content["schemaVersion"], 1);
    assert_eq!(content["typeCount"], 2);
    assert_eq!(content["types"].as_array().unwrap().len(), 2);
}

#[test]
fn memory_schema_kind_resource_returns_single_type() {
    let (_dir, mut session) = setup_project();
    session.initialized = true;
    let result = session.read_resource(&json!({"uri": "memory://schema/decision"}));
    let response = result.unwrap();
    let text = &response["contents"][0]["text"];
    let content: Value = serde_json::from_str(text.as_str().unwrap()).unwrap();
    assert_eq!(content["kindName"], "decision");
    assert!(content["typeDefinition"]["kind_name"].is_string());
}

#[test]
fn memory_schema_kind_resource_returns_not_found_for_unknown() {
    let (_dir, mut session) = setup_project();
    session.initialized = true;
    let result = session.read_resource(&json!({"uri": "memory://schema/nonexistent"}));
    assert!(result.is_err());
}

#[test]
fn memory_list_types_returns_type_metadata() {
    let (_dir, mut session) = setup_project();
    session.initialized = true;
    let outcome = session.list_types().unwrap();
    let content = &outcome.content;
    assert_eq!(content["schemaVersion"], 1);
    assert_eq!(content["typeCount"], 2);
    let types = content["types"].as_array().unwrap();
    assert_eq!(types.len(), 2);
    let kind_names: Vec<&str> = types
        .iter()
        .map(|t| t["kind_name"].as_str().unwrap())
        .collect();
    assert!(kind_names.contains(&"decision"));
    assert!(kind_names.contains(&"spec"));
}

#[test]
fn memory_schema_status_reports_no_incompatibles_when_valid() {
    let (_dir, mut session) = setup_project();
    session.initialized = true;

    // Write a valid decision record.
    let store = session.store().unwrap();
    let revision = store.current_revision().unwrap();
    let mut envelope = Envelope::new(
        "decisions/1",
        "decision",
        "We chose MCP because it is the public contract.",
    )
    .unwrap();
    envelope.title = Some("Use MCP".into());
    envelope
        .extensions
        .insert("rationale_summary".into(), json!("Need a seam"));
    let record = StoredRecord::Plaintext {
        envelope: Box::new(envelope),
    };
    store
        .apply(&memory_hub_store::Transaction {
            id: "tx-valid".into(),
            expected_revision: revision,
            operations: vec![Operation::put(record)],
        })
        .unwrap();

    let outcome = session.schema_status().unwrap();
    let content = &outcome.content;
    assert_eq!(content["schemaActive"], true);
    assert_eq!(content["incompatibleCount"], 0);
    assert_eq!(content["totalRecords"], 1);
}

#[test]
fn memory_schema_status_reports_incompatible_records() {
    let (_dir, mut session) = setup_project();
    session.initialized = true;

    // A store opened directly carries no policy, so nothing validates this
    // write — which is exactly how a record the schema would refuse gets into
    // a corpus in the first place.
    let store = GitStore::open(session.service().project()).unwrap();
    let revision = store.current_revision().unwrap();
    let record = StoredRecord::Plaintext {
        envelope: Box::new(Envelope::new("unknown/1", "unknown_kind", "some content").unwrap()),
    };
    store
        .apply(&memory_hub_store::Transaction {
            id: "tx-unknown".into(),
            expected_revision: revision,
            operations: vec![Operation::put(record)],
        })
        .unwrap();

    let outcome = session.schema_status().unwrap();
    let content = &outcome.content;
    assert_eq!(content["schemaActive"], true);
    assert_eq!(content["incompatibleCount"], 1);
    let incompatible = content["incompatible"].as_array().unwrap();
    assert_eq!(incompatible[0]["kind"], "unknown_kind");
}

#[test]
fn memory_schema_status_inactive_when_no_types() {
    let project = tempfile::tempdir().unwrap();
    init_git(project.path());
    let mut session = memory_hub_mcp::Session::new(
        project.path().to_path_buf(),
        memory_hub_service::RecordsIn::GitMetadata,
    );
    session.initialized = true;

    let outcome = session.schema_status().unwrap();
    let content = &outcome.content;
    assert_eq!(content["schemaActive"], false);
    assert!(content["message"].as_str().unwrap().contains("inactive"));
}

#[test]
fn list_tools_includes_schema_tools() {
    let tools = memory_hub_mcp::list_tools();
    let tool_names: Vec<&str> = tools["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap())
        .collect();
    assert!(tool_names.contains(&"memory_list_types"));
    assert!(tool_names.contains(&"memory_schema_status"));
}

#[test]
fn list_resources_includes_schema_resource() {
    let resources = memory_hub_mcp::list_resources();
    let uris: Vec<&str> = resources["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uri"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"memory://schema"));
}

#[test]
fn list_resource_templates_includes_schema_template() {
    let templates = memory_hub_mcp::list_resource_templates();
    let uris: Vec<&str> = templates["resourceTemplates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| r["uriTemplate"].as_str().unwrap())
        .collect();
    assert!(uris.contains(&"memory://schema/{kind_name}"));
}
