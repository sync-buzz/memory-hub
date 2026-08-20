//! The public types are `Send + Sync`, checked at compile time.
//!
//! A host that wants to drive Memory from an async runtime — or from more than
//! one thread — needs to own a `Session` inside a task. `Session` holds an
//! `EncryptedStore`, which holds an age `Identity`, so a missing bound anywhere
//! in that chain makes the whole thing unusable — and nothing in the type
//! system says so until a caller tries. This test says it at compile time.

use memory_hub_mcp::Session;
use memory_hub_store::{EncryptedStore, GitStore};

const fn assert_send_sync<T: Send + Sync>() {}

#[test]
fn public_types_are_send_and_sync() {
    assert_send_sync::<GitStore>();
    assert_send_sync::<EncryptedStore>();
    assert_send_sync::<Session>();
}
