// Errors cross the public interface as owned values; preserving that shape in
// constructor call sites is clearer than borrowing short-lived adapter errors.
#![allow(clippy::needless_pass_by_value)]

use memory_hub_engine::{StoreError, StoreErrorKind};

/// Git-specific construction of the shared error type.
///
/// The error itself is storage-neutral and lives in `memory-hub-engine`; only
/// the translation from a `git2` failure belongs here. Written as a trait so
/// existing `StoreError::repository(...)` call sites keep working unchanged.
pub(crate) trait GitStoreError {
    fn repository(operation: &str, error: git2::Error) -> StoreError;

    /// Attach the underlying Git code to an error raised for another reason.
    #[must_use]
    fn with_repository_context(self, error: git2::Error) -> StoreError;
}

impl GitStoreError for StoreError {
    fn repository(operation: &str, error: git2::Error) -> Self {
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

    fn with_repository_context(mut self, error: git2::Error) -> Self {
        self.data["git_code"] =
            serde_json::json!(format!("{:?}", error.code()).to_ascii_lowercase());
        self
    }
}
