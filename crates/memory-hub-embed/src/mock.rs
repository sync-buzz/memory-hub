//! Deterministic, allocation-only test double for [`EmbeddingProvider`].
//!
//! Vectors are derived from blake3-XOF over the input string and then
//! L2-normalised. Same input always produces the same vector; different
//! inputs almost certainly produce different vectors. No GGUF, no GPU, no FFI
//! — safe to use anywhere in tests including without a downloaded model.

use crate::provider::{EmbeddingProvider, Pooling};
use anyhow::Result;
use async_trait::async_trait;

pub struct MockProvider {
    name: String,
    model_id: String,
    dimensions: usize,
    max_tokens: usize,
    pooling: Pooling,
    query_prefix: Option<String>,
    doc_prefix: Option<String>,
    constant: bool,
}

impl MockProvider {
    /// Build a mock with the given output dimension. Defaults match
    /// nomic-embed-text-v1.5 so tests that don't care about specifics behave
    /// like the production default.
    #[must_use]
    pub fn new(dimensions: usize) -> Self {
        Self {
            name: "mock".to_string(),
            model_id: "mock-deterministic-v1".to_string(),
            dimensions,
            max_tokens: 2048,
            pooling: Pooling::Mean,
            query_prefix: None,
            doc_prefix: None,
            constant: false,
        }
    }

    /// Answer every input with one and the same vector.
    ///
    /// Derived vectors are blake3 noise, and noise never clears the vector
    /// channel's rescue floor — that floor is tuned for a model whose
    /// distances mean something. A test that has to see the whole field
    /// rather than a ranking of it asks for this: every record is then exactly
    /// as near every query, and nothing is dropped on the way out.
    #[must_use]
    pub fn constant(mut self) -> Self {
        self.constant = true;
        self
    }

    #[must_use]
    pub fn with_model_id(mut self, id: impl Into<String>) -> Self {
        self.model_id = id.into();
        self
    }

    #[must_use]
    pub fn with_max_tokens(mut self, n: usize) -> Self {
        self.max_tokens = n;
        self
    }

    #[must_use]
    pub fn with_pooling(mut self, p: Pooling) -> Self {
        self.pooling = p;
        self
    }

    #[must_use]
    pub fn with_query_prefix(mut self, p: impl Into<String>) -> Self {
        self.query_prefix = Some(p.into());
        self
    }

    #[must_use]
    pub fn with_doc_prefix(mut self, p: impl Into<String>) -> Self {
        self.doc_prefix = Some(p.into());
        self
    }
}

#[async_trait]
impl EmbeddingProvider for MockProvider {
    fn name(&self) -> &str {
        &self.name
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    fn dimensions(&self) -> usize {
        self.dimensions
    }

    fn max_tokens(&self) -> usize {
        self.max_tokens
    }

    fn pooling(&self) -> Pooling {
        self.pooling
    }

    fn query_prefix(&self) -> Option<&str> {
        self.query_prefix.as_deref()
    }

    fn doc_prefix(&self) -> Option<&str> {
        self.doc_prefix.as_deref()
    }

    async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        Ok(texts
            .iter()
            .map(|text| {
                let seed = if self.constant { "" } else { text.as_str() };
                derive_vector(seed, self.dimensions)
            })
            .collect())
    }
}

#[allow(clippy::cast_possible_wrap)]
fn derive_vector(text: &str, dim: usize) -> Vec<f32> {
    let mut buf = vec![0u8; dim];
    let mut xof = blake3::Hasher::new().update(text.as_bytes()).finalize_xof();
    xof.fill(&mut buf);

    let mut v: Vec<f32> = buf
        .into_iter()
        .map(|b| f32::from(b as i8) / 127.0)
        .collect();
    let mag: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if mag > 0.0 {
        for x in &mut v {
            *x /= mag;
        }
    }
    v
}
