//! Black-box behavioral contract for Git Memory MCP servers.
//!
//! The harness deliberately knows only how to launch a process and speak JSON-RPC
//! over its public stdio interface. It does not link to the Git Memory runtime or
//! expose an in-process server adapter.

mod client;
mod fixtures;
mod runner;
mod target;

pub use runner::{ContractReport, ScenarioReport, run_contract};
pub use target::{FakeServerTarget, ReleaseBinaryTarget, ServerTarget};

/// MCP protocol revision exercised by this version of the contract.
pub const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
