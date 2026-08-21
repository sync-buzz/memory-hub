use std::collections::BTreeMap;

use memory_hub_core::{
    ArchiveState, ClientProfile, ContentHash, Envelope, FormatVersion, Freshness, FreshnessState,
    PolicyConfig, PolicyMode, PolicyResolver, PolicySource, RecordLink, SourcePaths,
};
use serde_json::{Value, json};

#[test]
fn independent_consumer_can_rebuild_from_the_public_envelope()
-> Result<(), Box<dyn std::error::Error>> {
    let mut envelope = Envelope::new("notes/contract", "note", "The public seam is MCP.")?;
    envelope.title = Some("Public seam".into());
    envelope.tags = vec!["architecture".into(), "contract".into()];
    envelope.links = vec![RecordLink {
        key: "decisions/mcp-only".into(),
        relation: Some("supports".into()),
        extensions: BTreeMap::new(),
    }];
    envelope.source_paths = SourcePaths {
        scope: vec!["crates/memory-hub-core/".into()],
        observed: vec!["README.md".into()],
        extensions: BTreeMap::new(),
    };
    envelope.archive = ArchiveState::default();
    envelope.freshness = Freshness {
        state: FreshnessState::Fresh,
        code_revision: Some("abc123".into()),
        validated_at: Some("2026-08-14T22:15:00Z".into()),
        reason: None,
        extensions: BTreeMap::new(),
    };
    envelope.profile = Some(ClientProfile {
        name: "example-client".into(),
        version: FormatVersion::new(3, 1),
        metadata: [("entity".into(), json!({"priority": "high"}))]
            .into_iter()
            .collect(),
        extensions: BTreeMap::new(),
    });
    envelope
        .extensions
        .insert("future_index_hint".into(), json!({"language": "en"}));
    envelope.validate()?;

    let wire = serde_json::to_value(&envelope)?;
    assert_eq!(wire["key"], "notes/contract");
    assert_eq!(
        wire["content_hash"],
        ContentHash::for_content(&envelope.content).as_str()
    );
    assert_eq!(wire["source_paths"]["scope"][0], "crates/memory-hub-core/");
    assert_eq!(wire["freshness"]["code_revision"], "abc123");

    let decoded: Envelope = serde_json::from_value(wire.clone())?;
    assert_eq!(serde_json::to_value(decoded)?, wire);
    Ok(())
}

#[test]
fn policy_wire_accepts_only_declared_modes_and_reports_the_source()
-> Result<(), Box<dyn std::error::Error>> {
    let project: PolicyConfig = serde_json::from_value(json!({
        "memory_push_stale": "block"
    }))?;
    let resolver = PolicyResolver::memory_hub_defaults().with_project(project)?;
    let effective = resolver.resolve("memory_push_stale", None)?;

    assert_eq!(effective.mode, PolicyMode::Block);
    assert_eq!(effective.source, PolicySource::Project);
    assert_eq!(
        serde_json::to_value(effective)?,
        json!({
            "event": "memory_push_stale",
            "mode": "block",
            "source": "project"
        })
    );
    assert!(serde_json::from_value::<PolicyConfig>(json!({"index_lag": "invented"})).is_err());
    Ok(())
}

#[test]
fn unknown_profile_values_are_preserved_as_opaque_json() -> Result<(), Box<dyn std::error::Error>> {
    let content = "body";
    let input = json!({
        "envelope_version": {"major": 1, "minor": 0},
        "key": "one",
        "kind": "third_party_kind",
        "content": content,
        "source_paths": {},
        "archive": {"archived": false},
        "freshness": {"state": "unverified"},
        "content_hash": ContentHash::for_content(content),
        "profile": {
            "name": "third-party",
            "version": {"major": 99, "minor": 4},
            "metadata": {
                "unknown_array": [1, {"nested": true}],
                "unknown_null": Value::Null
            }
        }
    });
    let decoded: Envelope = serde_json::from_value(input.clone())?;
    let output = serde_json::to_value(decoded)?;
    assert_eq!(output["profile"]["metadata"], input["profile"]["metadata"]);
    Ok(())
}

#[test]
fn debug_output_redacts_record_payloads() -> Result<(), Box<dyn std::error::Error>> {
    let envelope = Envelope::new("secret", "note", "do-not-log-plaintext")?;
    let record = memory_hub_core::StoredRecord::Plaintext {
        envelope: Box::new(envelope),
    };
    let debug = format!("{record:?}");
    assert!(!debug.contains("do-not-log-plaintext"));

    Ok(())
}
