//! Smoke test that the `llama-cpp-2` dep actually builds, links and runs its
//! one-shot backend init on this machine. No GGUF model required — this only
//! exercises `llama_backend_init()`.

#![allow(clippy::unwrap_used, clippy::expect_used)]

use memory_hub_embed::llama_cpp::ensure_backend;

#[test]
fn backend_initialises() {
    let backend = ensure_backend().expect("llama backend should initialise");
    let again = ensure_backend().expect("second call must succeed");
    assert!(std::ptr::eq(backend, again));
}
