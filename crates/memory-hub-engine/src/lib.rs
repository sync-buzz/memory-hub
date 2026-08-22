//! What a Memory Hub backend has to be, expressed without naming Git.
//!
//! This crate owns the vocabulary every storage shares — revisions, record
//! identities, transactions, changes — and the traits a backend implements. It
//! deliberately has no `git2`, no filesystem, and no index dependency, so a
//! backend can be written against it without linking the Git implementation.
//!
//! The split between the traits is the point. [`RecordStore`] is what a
//! storage must do to be a storage at all. History, transport and the rest are
//! separate traits because they are not universal: a folder of documents in the
//! working tree has no checkpoints to walk and no refs to push, and pretending
//! otherwise would put a method on the interface that some backends can only
//! answer with a lie.

mod contract;
mod error;
mod types;

pub use contract::{
    Capabilities, Capability, HistoryStore, Ownership, PortableStore, RecordStore,
    StoreDescription, StoreView, TransactionPolicy,
};
pub use error::{StoreError, StoreErrorKind};
pub use types::{
    ApplyResult, ChangeKind, ExportBundle, ExportMode, Operation, RecordChange, RecordId, Revision,
    Transaction,
};
