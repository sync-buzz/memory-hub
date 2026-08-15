use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable categories returned when a record fails schema validation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidationErrorKind {
    /// The `__type__` record itself is malformed.
    InvalidTypeDefinition,
    /// The record's `kind` has no matching type definition (strict mode).
    UnknownKind,
    /// A standard envelope field violates a type constraint.
    InvalidEnvelope,
    /// A semantic field in `extensions` violates the generated JSON Schema.
    InvalidExtensions,
    /// A link relation is undeclared or its target kind mismatches.
    InvalidLinks,
}

/// Machine-readable schema validation failure.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ValidationError {
    pub kind: ValidationErrorKind,
    pub field: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ValidationError {
    pub(crate) fn new(
        kind: ValidationErrorKind,
        field: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            kind,
            field: field.into(),
            message: message.into(),
            data: None,
        }
    }

    pub(crate) fn with_data(
        kind: ValidationErrorKind,
        field: impl Into<String>,
        message: impl Into<String>,
        data: Value,
    ) -> Self {
        Self {
            kind,
            field: field.into(),
            message: message.into(),
            data: Some(data),
        }
    }
}

impl fmt::Display for ValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ValidationError {}
