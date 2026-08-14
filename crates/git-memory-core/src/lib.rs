//! Canonical, product-neutral data contract for Git Memory.
//!
//! This crate owns the durable envelope and policy semantics. It deliberately
//! contains no store, transport, index, or client-product types.

mod envelope;
mod error;
mod policy;
mod representation;
mod version;

pub use envelope::{
    ArchiveState, ClientProfile, ContentHash, Envelope, Freshness, FreshnessState, RecordLink,
    SourcePaths,
};
pub use error::{ContractError, ContractErrorKind};
pub use policy::{EffectivePolicy, PolicyConfig, PolicyMode, PolicyResolver, PolicySource};
pub use representation::{EncryptedRecord, OpaqueStorageId, StoredRecord};
pub use version::{CURRENT_ENVELOPE_VERSION, FormatVersion};
