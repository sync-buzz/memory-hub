//! Atomic Git object store for canonical Memory records.
//!
//! [`GitStore`] is the module's interface. Git tree construction, record path
//! hashing, ref compare-and-swap, and concurrent rebase stay in its
//! implementation. No transaction operation invokes the `git` executable.

mod encrypted;
mod error;
mod store;
mod transport;
mod types;

pub use encrypted::{EncryptedStore, InitResult, RecipientEntry, is_encrypted_project};
pub use error::{StoreError, StoreErrorKind};
pub use store::{CommitSigner, GitStore};
pub use transport::{
    ConflictEntry, FetchResult, MemoryRemote, PushPolicyResult, can_fast_forward,
    check_push_policy, cleanup_temp_ref_pub, fast_forward_to, fetch_and_merge,
    fetch_remote_revision, push_to_remote, read_remote_config, remove_remote_config,
    write_remote_config,
};
pub use types::{
    ApplyResult, ChangeKind, Checkpoint, ExportBundle, Operation, RecordChange, RecordId, Revision,
    Snapshot, Transaction,
};

/// Mutable snapshot ref. It points to the tip of the transaction commit chain.
pub const STAGED_REF: &str = "refs/memory/staged";

/// Checkpoint history ref. It points to a chain of Git commits.
pub const MAIN_REF: &str = "refs/memory/main";
