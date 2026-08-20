use std::collections::BTreeMap;

use memory_hub_core::Envelope;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::storage::{self, TypeStorage};
use crate::{ValidationError, ValidationErrorKind};

/// Allowed envelope field names in the `envelope` constraints section.
const ENVELOPE_CONSTRAINT_FIELDS: &[&str] = &["title", "content"];

/// Field type discriminator for semantic fields stored in `envelope.extensions`.
///
/// `text` is semantically longer free-form prose but maps to a JSON string.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum FieldType {
    String,
    Text,
    Integer,
    Number,
    Boolean,
    Enum { values: Vec<String> },
    Array { items: Box<FieldType> },
    Object,
}

/// A semantic field declaration inside a [`TypeDefinition`] `fields` section.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FieldDefinition {
    #[serde(flatten)]
    pub field_type: FieldType,
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<Value>,
}

/// Constraints on a single standard envelope field (`title`, `content`).
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EnvelopeFieldConstraints {
    #[serde(default)]
    pub required: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min_length: Option<usize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<usize>,
}

/// Constraints on standard envelope fields, keyed by field name.
///
/// Only `title` and `content` are meaningful today; unknown keys are rejected
/// during [`TypeDefinition::validate_self`].
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct EnvelopeConstraints(#[serde(default)] BTreeMap<String, EnvelopeFieldConstraints>);

impl EnvelopeConstraints {
    #[must_use]
    pub fn get(&self, field: &str) -> Option<&EnvelopeFieldConstraints> {
        self.0.get(field)
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (&String, &EnvelopeFieldConstraints)> {
        self.0.iter()
    }
}

/// A typed relationship to another kind, declared in a [`TypeDefinition`].
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RelationshipDefinition {
    /// Target kind name, or `any` to allow any kind.
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// A parsed document type definition (`__type__` record content).
///
/// Parsed from the JSON `content` of an envelope whose `kind` is `__type__`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TypeDefinition {
    pub kind_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub guidance: Option<String>,
    /// The storage this type's content lives in, by the name the project gave
    /// it. Absent means it lives with the records, which is what every type was
    /// before storage became a choice.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub storage: Option<String>,
    #[serde(default)]
    pub envelope: EnvelopeConstraints,
    #[serde(default)]
    pub fields: BTreeMap<String, FieldDefinition>,
    #[serde(default)]
    pub relationships: BTreeMap<String, RelationshipDefinition>,
}

impl TypeDefinition {
    /// Parse a type definition from its JSON content string.
    ///
    /// # Errors
    ///
    /// Returns [`serde_json::Error`] when the content is not valid JSON or does
    /// not match the type definition shape.
    pub fn from_content(content: &str) -> Result<Self, serde_json::Error> {
        serde_json::from_str(content)
    }

    /// Where this type's records live, with every axis answered.
    ///
    /// This is also the validation of the storage section: an unknown place,
    /// an ownership the place cannot offer, and `__type__` claiming a place at
    /// all are all rejected here, which is why nothing downstream has to
    /// handle a storage it cannot honour.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] naming the offending `storage.*` field.
    pub fn storage(&self) -> Result<TypeStorage, ValidationError> {
        storage::resolve(&self.kind_name, self.storage.as_deref())
    }

    /// Validate that the type definition is well-formed.
    ///
    /// Checks `kind_name` is non-empty, the storage section names a place
    /// this build has (see [`storage`](Self::storage)), every field type is
    /// internally consistent (enum has values, array has items), envelope
    /// constraints only reference known fields, and relationship targets are
    /// non-empty.
    ///
    /// Cross-type target validation (does `target: "spec"` reference a known
    /// kind?) requires a [`SchemaRegistry`](crate::SchemaRegistry).
    ///
    /// # Errors
    ///
    /// Returns the first [`ValidationError`] found.
    pub fn validate_self(&self) -> Result<(), ValidationError> {
        if self.kind_name.trim().is_empty() {
            return Err(ValidationError::new(
                ValidationErrorKind::InvalidTypeDefinition,
                "kind_name",
                "kind_name must not be empty",
            ));
        }

        self.storage()?;

        for (field_name, field_def) in &self.fields {
            validate_field_type(field_name, &field_def.field_type)?;
        }

        for (field_name, constraint) in self.envelope.iter() {
            if !ENVELOPE_CONSTRAINT_FIELDS.contains(&field_name.as_str()) {
                return Err(ValidationError::new(
                    ValidationErrorKind::InvalidTypeDefinition,
                    format!("envelope.{field_name}"),
                    "only `title` and `content` envelope constraints are supported",
                ));
            }
            if let Some(max) = constraint.max_length
                && let Some(min) = constraint.min_length
                && max < min
            {
                return Err(ValidationError::new(
                    ValidationErrorKind::InvalidTypeDefinition,
                    format!("envelope.{field_name}"),
                    "max_length must not be smaller than min_length",
                ));
            }
        }

        for (relation, rel_def) in &self.relationships {
            if rel_def.target.trim().is_empty() {
                return Err(ValidationError::new(
                    ValidationErrorKind::InvalidTypeDefinition,
                    format!("relationships.{relation}.target"),
                    "target must not be empty",
                ));
            }
        }

        Ok(())
    }

    /// Build a JSON Schema (draft 2020-12) for the semantic `fields` section.
    ///
    /// The schema validates `envelope.extensions` against declared field types,
    /// required flags and enum values. Additional properties are permitted so
    /// future envelope minor-version fields are not rejected.
    #[must_use]
    pub fn build_json_schema(&self) -> Value {
        let mut properties = Map::new();
        let mut required = Vec::new();

        for (name, def) in &self.fields {
            properties.insert(name.clone(), field_type_to_schema(&def.field_type));
            if def.required {
                required.push(Value::String(name.clone()));
            }
        }

        let mut schema = Map::new();
        schema.insert("type".into(), Value::String("object".into()));
        if !properties.is_empty() {
            schema.insert("properties".into(), Value::Object(properties));
        }
        if !required.is_empty() {
            schema.insert("required".into(), Value::Array(required));
        }
        Value::Object(schema)
    }

    /// Validate the standard envelope fields (`title`, `content`) against the
    /// type's envelope constraints.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when a required field is missing or a length
    /// constraint is violated.
    pub fn validate_envelope(&self, envelope: &Envelope) -> Result<(), ValidationError> {
        if let Some(constraint) = self.envelope.get("title") {
            check_envelope_field("title", envelope.title.as_deref(), constraint)?;
        }
        // A reference record's `content` is empty by contract — the bytes are
        // in a file somebody else owns, and the record keeps no copy. Checking
        // a constraint against that emptiness would say nothing about the
        // document and would make the obvious pair of declarations —
        // "content is required" and "content lives in docs/" — impossible to
        // write together.
        if let Some(constraint) = self.envelope.get("content")
            && !envelope.is_reference()
        {
            check_envelope_field("content", Some(&envelope.content), constraint)?;
        }
        Ok(())
    }

    /// Validate `envelope.extensions` against the JSON Schema generated from
    /// the type's `fields` section.
    ///
    /// # Errors
    ///
    /// Returns the first [`ValidationError`] produced by the JSON Schema
    /// validator, translated to the schema error kind.
    pub fn validate_extensions(&self, envelope: &Envelope) -> Result<(), ValidationError> {
        if self.fields.is_empty() {
            return Ok(());
        }
        let schema = self.build_json_schema();
        let validator = match jsonschema::validator_for(&schema) {
            Ok(validator) => validator,
            Err(error) => {
                return Err(ValidationError::with_data(
                    ValidationErrorKind::InvalidExtensions,
                    "extensions",
                    "failed to compile generated JSON Schema",
                    serde_json::json!({"cause": error.to_string()}),
                ));
            }
        };
        let instance = Value::Object(envelope_to_extension_object(envelope));
        if let Some(error) = validator.iter_errors(&instance).next() {
            let field_path = format!("extensions.{}", error.instance_path());
            return Err(ValidationError::with_data(
                ValidationErrorKind::InvalidExtensions,
                field_path,
                error.to_string(),
                serde_json::json!({"schema_path": error.schema_path().to_string()}),
            ));
        }
        Ok(())
    }

    /// Validate that every link relation is declared in the type's
    /// `relationships` section.
    ///
    /// Target-kind matching requires a [`SchemaRegistry`](crate::SchemaRegistry)
    /// with a kind resolver; this standalone check only enforces that the
    /// relation name is declared.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] when a link carries a `relation` that is not
    /// declared in `relationships`.
    pub fn validate_links(&self, envelope: &Envelope) -> Result<(), ValidationError> {
        for (index, link) in envelope.links.iter().enumerate() {
            if let Some(relation) = &link.relation
                && !self.relationships.contains_key(relation)
            {
                return Err(ValidationError::with_data(
                    ValidationErrorKind::InvalidLinks,
                    format!("links[{index}].relation"),
                    format!(
                        "relation `{relation}` is not declared in type `{}`",
                        self.kind_name
                    ),
                    serde_json::json!({"relation": relation, "kind": self.kind_name}),
                ));
            }
        }
        Ok(())
    }

    /// Run envelope, extensions and links validation in sequence.
    ///
    /// # Errors
    ///
    /// Returns the first [`ValidationError`] from any of the three checks.
    pub fn validate(&self, envelope: &Envelope) -> Result<(), ValidationError> {
        self.validate_envelope(envelope)?;
        self.validate_extensions(envelope)?;
        self.validate_links(envelope)?;
        Ok(())
    }
}

fn validate_field_type(name: &str, field_type: &FieldType) -> Result<(), ValidationError> {
    match field_type {
        FieldType::Enum { values } => {
            if values.is_empty() {
                return Err(ValidationError::new(
                    ValidationErrorKind::InvalidTypeDefinition,
                    format!("fields.{name}.values"),
                    "enum field must declare at least one value",
                ));
            }
            let mut seen = std::collections::HashSet::new();
            for (i, value) in values.iter().enumerate() {
                if value.is_empty() {
                    return Err(ValidationError::new(
                        ValidationErrorKind::InvalidTypeDefinition,
                        format!("fields.{name}.values[{i}]"),
                        "enum value must not be empty",
                    ));
                }
                if !seen.insert(value.as_str()) {
                    return Err(ValidationError::with_data(
                        ValidationErrorKind::InvalidTypeDefinition,
                        format!("fields.{name}.values[{i}]"),
                        "duplicate enum value",
                        serde_json::json!({"value": value}),
                    ));
                }
            }
        }
        FieldType::Array { items } => {
            validate_field_type(name, items)?;
        }
        FieldType::String
        | FieldType::Text
        | FieldType::Integer
        | FieldType::Number
        | FieldType::Boolean
        | FieldType::Object => {}
    }
    Ok(())
}

fn field_type_to_schema(field_type: &FieldType) -> Value {
    match field_type {
        FieldType::String | FieldType::Text => serde_json::json!({"type": "string"}),
        FieldType::Integer => serde_json::json!({"type": "integer"}),
        FieldType::Number => serde_json::json!({"type": "number"}),
        FieldType::Boolean => serde_json::json!({"type": "boolean"}),
        FieldType::Enum { values } => serde_json::json!({
            "type": "string",
            "enum": values
        }),
        FieldType::Array { items } => {
            let items_schema = field_type_to_schema(items);
            serde_json::json!({"type": "array", "items": items_schema})
        }
        FieldType::Object => serde_json::json!({"type": "object"}),
    }
}

fn check_envelope_field(
    field: &str,
    value: Option<&str>,
    constraint: &EnvelopeFieldConstraints,
) -> Result<(), ValidationError> {
    let present = value.filter(|value| !value.is_empty());
    if constraint.required && present.is_none() {
        return Err(ValidationError::new(
            ValidationErrorKind::InvalidEnvelope,
            field,
            "field is required but missing or empty",
        ));
    }
    if let Some(text) = present {
        let length = text.chars().count();
        if let Some(min) = constraint.min_length
            && length < min
        {
            return Err(ValidationError::with_data(
                ValidationErrorKind::InvalidEnvelope,
                field,
                format!("value is shorter than min_length {min}"),
                serde_json::json!({"length": length, "min_length": min}),
            ));
        }
        if let Some(max) = constraint.max_length
            && length > max
        {
            return Err(ValidationError::with_data(
                ValidationErrorKind::InvalidEnvelope,
                field,
                format!("value is longer than max_length {max}"),
                serde_json::json!({"length": length, "max_length": max}),
            ));
        }
    }
    Ok(())
}

fn envelope_to_extension_object(envelope: &Envelope) -> Map<String, Value> {
    let mut object = Map::new();
    for (key, value) in &envelope.extensions {
        object.insert(key.clone(), value.clone());
    }
    object
}
