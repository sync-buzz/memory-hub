use std::fmt;

use memory_hub_index::IndexError;
use memory_hub_reconcile::{ReconcileError, ReconcileErrorKind};
use memory_hub_store::{StoreError, StoreErrorKind};
use serde_json::{Value, json};

/// A use-case failure, carrying the same stable `kind` the public interface
/// promises its clients.
///
/// The kind is the contract; `message` is for humans and `data` carries the
/// structured detail a caller may branch on (conflicting keys, the revision it
/// expected, the recovery action). Protocol adapters map this onto their own
/// error shape without re-deciding what went wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ServiceError {
    pub kind: String,
    pub message: String,
    pub data: Value,
}

impl ServiceError {
    #[must_use]
    pub fn new(kind: impl Into<String>, message: impl Into<String>, data: Value) -> Self {
        Self {
            kind: kind.into(),
            message: message.into(),
            data,
        }
    }

    /// A caller supplied a field that is missing, malformed, or contradictory.
    #[must_use]
    pub fn invalid_argument(field: &str, message: impl Into<String>) -> Self {
        Self::new("invalid_argument", message, json!({"field": field}))
    }

    /// The project is encrypted and the store has not been unlocked.
    #[must_use]
    pub fn locked() -> Self {
        Self::new(
            "locked",
            "encrypted store is locked — unlock it with an identity first",
            json!({"recovery_action": "unlock_with_identity"}),
        )
    }

    /// The operation only means something on an encrypted project.
    #[must_use]
    pub fn not_encrypted(message: impl Into<String>) -> Self {
        Self::new("not_encrypted", message, json!({}))
    }

    #[must_use]
    pub fn store(error: StoreError) -> Self {
        Self::new(store_kind(error.kind), error.message, error.data)
    }

    #[must_use]
    pub fn reconcile(error: ReconcileError) -> Self {
        Self::new(reconcile_kind(error.kind), error.message, error.data)
    }

    #[must_use]
    pub fn index(error: &IndexError) -> Self {
        Self::new("index", error.to_string(), json!({}))
    }
}

impl fmt::Display for ServiceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ServiceError {}

const fn store_kind(kind: StoreErrorKind) -> &'static str {
    match kind {
        StoreErrorKind::InvalidArgument => "invalid_argument",
        StoreErrorKind::InvalidRecord => "invalid_record",
        StoreErrorKind::RevisionNotFound => "revision_not_found",
        StoreErrorKind::Conflict => "conflict",
        StoreErrorKind::TransactionReused => "transaction_reused",
        StoreErrorKind::Repository => "repository",
        StoreErrorKind::RetryExhausted => "retry_exhausted",
        StoreErrorKind::FastForwardRequired => "fast_forward_required",
        StoreErrorKind::Diverged => "diverged",
        StoreErrorKind::AuthenticationFailed => "authentication_failed",
        StoreErrorKind::NamespaceRejected => "namespace_rejected",
        StoreErrorKind::TransportFailed => "transport_failed",
        StoreErrorKind::SignatureInvalid => "signature_invalid",
        StoreErrorKind::SigningNotConfigured => "signing_not_configured",
        StoreErrorKind::MergeConflict => "merge_conflict",
        // The vocabulary a client already branches on for a locked project.
        StoreErrorKind::Locked => "locked",
        StoreErrorKind::Unsupported => "unsupported",
    }
}

const fn reconcile_kind(kind: ReconcileErrorKind) -> &'static str {
    match kind {
        ReconcileErrorKind::InvalidProject => "invalid_project",
        ReconcileErrorKind::Repository => "repository",
        ReconcileErrorKind::Cursor => "cursor",
        ReconcileErrorKind::Diverged => "diverged",
        ReconcileErrorKind::Store => "store",
    }
}
