//! Generate agent instructions from project document type definitions.
//!
//! The built-in conductor (`builtin_instructions`) describes the storage tools,
//! revision model, and encryption lifecycle. `schema_instructions` composes
//! project-specific text from `__type__` records on top of the built-in base.

use memory_hub_schema::{SchemaRegistry, TypeDefinition};

/// Generate the project-specific schema section for MCP instructions.
///
/// For each registered type definition, emits a human-readable block:
/// description, guidance, required envelope fields, semantic fields, and
/// declared relationships. Returns an empty string when the registry is empty
/// (no type records — only the built-in conductor is sent).
#[must_use]
pub fn schema_instructions(registry: &SchemaRegistry) -> String {
    if registry.is_empty() {
        return String::new();
    }
    let mut sections = Vec::new();
    for (_, definition) in registry.iter() {
        sections.push(format_type_section(definition));
    }
    format!(
        "\n## Document Types\n\nThis project stores the following document types:\n\n{}",
        sections.join("\n")
    )
}

/// Format a single type definition as an instructions block.
fn format_type_section(definition: &TypeDefinition) -> String {
    let mut lines = Vec::new();
    lines.push(format!("### {}\n", definition.kind_name));

    if let Some(description) = &definition.description {
        lines.push(format!("{description}\n"));
    }
    if let Some(guidance) = &definition.guidance {
        lines.push(format!("{guidance}\n"));
    }

    // Required envelope fields
    let required_envelope: Vec<&str> = ["title", "content"]
        .into_iter()
        .filter(|field| definition.envelope.get(field).is_some_and(|c| c.required))
        .collect();
    if !required_envelope.is_empty() {
        lines.push(format!(
            "Required envelope fields: {}\n",
            required_envelope.join(", ")
        ));
    }

    // Semantic fields
    if !definition.fields.is_empty() {
        lines.push("Semantic fields:".into());
        for (name, field_def) in &definition.fields {
            let type_str = field_type_label(&field_def.field_type);
            let req = if field_def.required {
                "required"
            } else {
                "optional"
            };
            let desc = field_def.description.as_deref().unwrap_or("");
            let desc_part = if desc.is_empty() {
                String::new()
            } else {
                format!(": {desc}")
            };
            lines.push(format!("  - {name} ({type_str}, {req}){desc_part}"));
        }
        lines.push(String::new());
    }

    // Relationships
    if !definition.relationships.is_empty() {
        let rels: Vec<String> = definition
            .relationships
            .iter()
            .map(|(relation, rel_def)| format!("{relation} → {}", rel_def.target))
            .collect();
        lines.push(format!("Relationships: {}\n", rels.join(", ")));
    }

    lines.join("\n")
}

/// Human-readable label for a field type.
fn field_type_label(field_type: &memory_hub_schema::FieldType) -> String {
    match field_type {
        memory_hub_schema::FieldType::String => "string".into(),
        memory_hub_schema::FieldType::Text => "text".into(),
        memory_hub_schema::FieldType::Integer => "integer".into(),
        memory_hub_schema::FieldType::Number => "number".into(),
        memory_hub_schema::FieldType::Boolean => "boolean".into(),
        memory_hub_schema::FieldType::Enum { values } => {
            format!("enum: {}", values.join(" | "))
        }
        memory_hub_schema::FieldType::Array { items } => {
            format!("array<{}>", field_type_label(items))
        }
        memory_hub_schema::FieldType::Object => "object".into(),
    }
}

/// Serialize all type definitions in the registry as a JSON array for the
/// `memory://schema` resource.
#[must_use]
pub fn schema_resource(registry: &SchemaRegistry) -> serde_json::Value {
    let types: Vec<serde_json::Value> = registry
        .iter()
        .map(|(_, definition)| serde_json::to_value(definition).unwrap_or(serde_json::Value::Null))
        .collect();
    serde_json::json!({
        "schemaVersion": 1,
        "typeCount": types.len(),
        "types": types
    })
}

/// Serialize a single type definition for the `memory://schema/{kind}` resource.
#[must_use]
pub fn single_type_resource(definition: &TypeDefinition) -> serde_json::Value {
    serde_json::json!({
        "schemaVersion": 1,
        "kindName": definition.kind_name,
        "typeDefinition": definition
    })
}
