//! Embedding provider stack for Memory Hub — model registry, download,
//! llama.cpp runtime, canonical renderer, fingerprint, and batch worker.
//!
//! Public surface is the [`EmbeddingProvider`] trait plus the model
//! [`registry`]. [`LlamaCppProvider`] is the production backend; [`MockProvider`]
//! is the test double. Download / cache / background worker live in dedicated
//! submodules.
//!
//! The canonical text for embedding is derived from the generic [`Envelope`] by
//! a single [`renderer`], shared by full rebuild and incremental update. The
//! active [`fingerprint`] ties model digest, dimension, renderer version and
//! runtime together; its change creates an incompatible index generation.

pub mod active;
pub mod cache;
pub mod download;
pub mod fingerprint;
pub mod llama_cpp;
pub mod mock;
pub mod provider;
pub mod registry;
pub mod renderer;
pub mod status;
pub mod worker;

pub use active::{
    ModelConfig, active_model, active_model_is_verified, active_model_path, config_path,
    load_config, resolve_active_provider,
};
pub use cache::content_hash_of;
pub use download::{
    DownloadError, DownloadOpts, DownloadSpec, EnsureOutcome, ModelVerification, ProgressCallback,
    ensure_model, model_path as cached_model_path, verify_model_sync,
};
pub use fingerprint::{Fingerprint, FingerprintError};
pub use llama_cpp::{LlamaCppProvider, backend_name};
pub use mock::MockProvider;
pub use provider::{EmbeddingProvider, Pooling};
pub use registry::{
    ModelEntry, PLACEHOLDER_SHA256, all_models, default_model, find as find_model,
    platform_default_model,
};
pub use renderer::{RENDERER_VERSION, render_envelope};
pub use status::{ModelRuntime, ModelStatus, ModelStatusBuilder};
pub use worker::{
    EmbedJob, EmbedWorker, EmbedWorkerConfig, EmbedWorkerHandle, EmbeddingSink, FreshUpdate,
    WorkerError,
};
