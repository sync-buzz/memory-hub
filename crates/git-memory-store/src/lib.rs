//! Atomic Git object store for canonical Memory records.
//!
//! [`GitStore`] is the module's interface. Git tree construction, record path
//! hashing, ref compare-and-swap, and concurrent rebase stay in its
//! implementation. No transaction operation invokes the `git` executable.

mod encrypted;
mod error;
mod store;
mod types;

pub use encrypted::{EncryptedStore, RecipientEntry};
pub use error::{StoreError, StoreErrorKind};
pub use store::GitStore;
pub use types::{
    ApplyResult, ChangeKind, Checkpoint, ExportBundle, Operation, RecordChange, RecordId, Revision,
    Snapshot, Transaction,
};

/// Mutable snapshot ref. It points to the tip of the transaction commit chain.
pub const STAGED_REF: &str = "refs/memory/staged";

/// Checkpoint history ref. It points to a chain of Git commits.
pub const MAIN_REF: &str = "refs/memory/main";
