// Errors cross the public interface as owned values; preserving that shape in
// constructor call sites is clearer than borrowing short-lived adapter errors.
#![allow(clippy::needless_pass_by_value)]

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StoreErrorKind {
    InvalidArgument,
    InvalidRecord,
    RevisionNotFound,
    Conflict,
    TransactionReused,
    Repository,
    RetryExhausted,
    FastForwardRequired,
    Diverged,
    AuthenticationFailed,
    NamespaceRejected,
    TransportFailed,
    SignatureInvalid,
    MergeConflict,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StoreError {
    pub kind: StoreErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

impl StoreError {
    pub(crate) fn new(kind: StoreErrorKind, message: impl Into<String>, data: Value) -> Self {
        Self {
            kind,
            message: message.into(),
            data,
        }
    }

    pub(crate) fn repository(operation: &str, error: git2::Error) -> Self {
        Self::new(
            StoreErrorKind::Repository,
            format!("Git repository operation `{operation}` failed"),
            serde_json::json!({
                "operation": operation,
                "class": format!("{:?}", error.class()).to_ascii_lowercase(),
                "code": format!("{:?}", error.code()).to_ascii_lowercase(),
            }),
        )
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}", self.message)
    }
}

impl std::error::Error for StoreError {}
