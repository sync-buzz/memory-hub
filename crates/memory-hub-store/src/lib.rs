//! Atomic Git object store for canonical Memory records.
//!
//! [`GitStore`] is the module's interface. Git tree construction, record path
//! hashing, ref compare-and-swap, and concurrent rebase stay in its
//! implementation. No transaction operation invokes the `git` executable.

mod engine_impl;
mod error;
mod store;
mod transport;
mod types;

pub use engine_impl::REFS_BACKEND;
pub use memory_hub_engine::{
    ApplyResult, Capabilities, Capability, ChangeKind, ExportBundle, ExportMode, HistoryStore,
    Operation, Ownership, PortableStore, RecordChange, RecordId, RecordStore,
    Revision, StoreDescription, StoreError, StoreErrorKind, StoreView, Transaction,
    TransactionPolicy,
};
pub use store::{EXPORT_SCHEMA_VERSION, GitStore, bundle};
pub use transport::{
    ConflictEntry, FetchResult, MemoryPresence, MemoryRemote, PushPolicyResult, RemoteMemory,
    can_fast_forward, check_push_policy, cleanup_temp_ref_pub, fast_forward_to, fetch_and_merge,
    fetch_remote_revision, memory_presence, probe_remote_memory, push_to_remote,
    read_code_origin_url, read_remote_config, remove_remote_config, validate_refspec,
    validate_remote_url, write_remote_config,
};

/// The one ref a project's memory lives on: the tip of the transaction commit
/// chain, and every past state through its parents.
pub const MAIN_REF: &str = "refs/memory/main";
