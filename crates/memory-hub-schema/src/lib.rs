//! Document type definitions, JSON Schema generation and envelope validation.
//!
//! This crate parses `__type__` records into [`TypeDefinition`] structs,
//! generates JSON Schemas for semantic fields stored in envelope extensions,
//! and validates [`Envelope`]s against the project's declared document types.
//!
//! It depends only on [`memory_hub_core`]; store and MCP integration live in
//! their own crates.

mod definition;
mod error;
mod registry;
mod storage;

pub use definition::{
    EnvelopeConstraints, EnvelopeFieldConstraints, FieldDefinition, FieldType,
    RelationshipDefinition, TypeDefinition,
};
pub use error::{ValidationError, ValidationErrorKind};
pub use registry::{KindResolver, SchemaRegistry};
pub use storage::{STORAGE_NAME_RULE, TypeStorage, is_storage_name};

/// Reserved record kind for document type definitions.
pub const TYPE_KIND: &str = "__type__";

/// Key prefix for type records: `__type__:{kind_name}`.
pub const TYPE_KEY_PREFIX: &str = "__type__:";

/// Build the canonical record key for a document type.
#[must_use]
pub fn type_key(kind_name: &str) -> String {
    format!("{TYPE_KEY_PREFIX}{kind_name}")
}

/// Extract the kind name from a `__type__:` record key, if present.
#[must_use]
pub fn kind_from_type_key(key: &str) -> Option<&str> {
    key.strip_prefix(TYPE_KEY_PREFIX)
}
