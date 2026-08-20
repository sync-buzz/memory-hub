//! Atomic Git object store for canonical Memory records.
//!
//! [`GitStore`] is the module's interface. Git tree construction, record path
//! hashing, ref compare-and-swap, and concurrent rebase stay in its
//! implementation. No transaction operation invokes the `git` executable.

mod encrypted;
mod engine_impl;
mod error;
mod signing;
mod store;
mod transport;
mod types;

pub use encrypted::{EncryptedStore, InitResult, RecipientEntry, is_encrypted_project};
pub use engine_impl::REFS_BACKEND;
pub use memory_hub_engine::{
    ApplyResult, Capabilities, Capability, ChangeKind, Checkpoint, ExportBundle, ExportMode,
    HistoryStore, Operation, Ownership, PortableStore, RecordChange, RecordId, RecordStore,
    Revision, StoreDescription, StoreError, StoreErrorKind, StoreView, Transaction,
    TransactionPolicy,
};
pub use signing::{SigningConfig, VerifyMode, read_signing_config};
pub use store::{CommitSigner, EXPORT_SCHEMA_VERSION, GitStore, bundle};
pub use transport::{
    ConflictEntry, FetchResult, MemoryRemote, PushPolicyResult, RemoteMemory, can_fast_forward,
    check_push_policy, cleanup_temp_ref_pub, fast_forward_to, fetch_and_merge,
    fetch_remote_revision, probe_remote_memory, push_to_remote, read_code_origin_url,
    read_remote_config, remove_remote_config, validate_refspec, validate_remote_url,
    write_remote_config,
};

/// Mutable snapshot ref. It points to the tip of the transaction commit chain.
pub const STAGED_REF: &str = "refs/memory/staged";

/// Checkpoint history ref. It points to a chain of Git commits.
pub const MAIN_REF: &str = "refs/memory/main";
