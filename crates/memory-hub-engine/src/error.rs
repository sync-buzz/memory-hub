// Errors cross the public interface as owned values; preserving that shape in
// constructor call sites is clearer than borrowing short-lived adapter errors.
#![allow(clippy::needless_pass_by_value)]

use std::fmt;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Stable failure classes. A client branches on `kind`, never on the message —
/// the message is for a person, the kind is the contract.
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
    SigningNotConfigured,
    MergeConflict,
    /// The storage holds its records under a key nobody has supplied yet.
    ///
    /// Separate from `invalid_argument` because it is not about the request:
    /// the same call succeeds once the store is unlocked, and a client needs
    /// to tell "ask for a key" apart from "you asked wrongly".
    Locked,
    /// The backend does not offer the capability that was asked for. Reported
    /// rather than silently ignored, so a caller can tell "this store cannot"
    /// apart from "this store did nothing".
    Unsupported,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct StoreError {
    pub kind: StoreErrorKind,
    pub message: String,
    #[serde(default, skip_serializing_if = "Value::is_null")]
    pub data: Value,
}

impl StoreError {
    #[must_use]
    pub fn new(kind: StoreErrorKind, message: impl Into<String>, data: Value) -> Self {
        Self {
            kind,
            message: message.into(),
            data,
        }
    }

    /// A capability this backend does not have.
    #[must_use]
    pub fn unsupported(capability: &str, backend: &str) -> Self {
        Self::new(
            StoreErrorKind::Unsupported,
            format!("this storage does not support {capability}"),
            serde_json::json!({"capability": capability, "backend": backend}),
        )
    }
}

impl fmt::Display for StoreError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for StoreError {}
