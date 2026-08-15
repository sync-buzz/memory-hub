//! `EmbeddingProvider` — the trait every embedding backend implements.
//!
//! Local llama.cpp ([`crate::LlamaCppProvider`]) is the only production
//! implementor. Future HTTP providers slot behind the same trait without
//! touching the index/search pipeline.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Pooling strategy applied to per-token embeddings before L2-normalisation.
///
/// Pinned by the model card and stored in [`crate::ModelEntry`]; never read
/// from GGUF metadata (community-converted files lie about it).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Pooling {
    Mean,
    Cls,
    LastToken,
}

#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Stable provider identifier — written verbatim to projection metadata.
    fn name(&self) -> &str;

    /// Stable model identifier — matches a [`crate::ModelEntry::id`].
    fn model_id(&self) -> &str;

    /// Output dimension. Vector channel rejects mismatched rebuilds via
    /// fingerprint diff.
    fn dimensions(&self) -> usize;

    /// Hard token cap; inputs are truncated before tokenisation.
    fn max_tokens(&self) -> usize;

    fn pooling(&self) -> Pooling;

    /// Prefix prepended to query strings (model-specific, e.g. `"query: "`).
    /// `None` means no prefix — pass query through untouched.
    fn query_prefix(&self) -> Option<&str>;

    /// Prefix prepended to document strings during indexing.
    fn doc_prefix(&self) -> Option<&str>;

    /// Embed a batch. Returned vectors are L2-normalised and have length
    /// [`dimensions`](Self::dimensions). Order matches `texts`.
    ///
    /// # Errors
    ///
    /// Returns an error when the model fails to load, tokenise, or encode.
    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}
