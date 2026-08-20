use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable categories returned when durable contract data is rejected.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContractErrorKind {
    InvalidField,
    IncompatibleVersion,
    UnknownPolicy,
}

/// Machine-readable contract failure.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ContractError {
    pub kind: ContractErrorKind,
    pub field: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl ContractError {
    pub(crate) fn invalid(field: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            kind: ContractErrorKind::InvalidField,
            field: field.into(),
            message: message.into(),
            data: None,
        }
    }

    pub(crate) fn incompatible_version(field: &str, major: u16, supported: u16) -> Self {
        Self {
            kind: ContractErrorKind::IncompatibleVersion,
            field: field.to_owned(),
            message: format!(
                "unsupported major version {major}; this build supports major version {supported}"
            ),
            data: Some(serde_json::json!({
                "received_major": major,
                "supported_major": supported,
            })),
        }
    }

    pub(crate) fn unknown_policy(event: &str) -> Self {
        Self {
            kind: ContractErrorKind::UnknownPolicy,
            field: format!("policy.{event}"),
            message: format!("policy event `{event}` has no declared default"),
            data: Some(serde_json::json!({"event": event})),
        }
    }
}

impl fmt::Display for ContractError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.field, self.message)
    }
}

impl std::error::Error for ContractError {}
