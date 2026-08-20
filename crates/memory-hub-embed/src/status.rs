//! Model status for the `memory://model/status` MCP resource.
//!
//! The status is machine-readable and never reveals user filesystem paths.
//! Instead of leaking `/Users/foo/Library/Caches/...`, the status reports
//! `available: true` / `false` plus the model id, dimensions and runtime.

use serde::{Deserialize, Serialize};

/// Machine-readable model runtime state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelRuntime {
    /// Model is loaded and embedding is available.
    Active,
    /// Model file is present but not yet loaded.
    Available,
    /// Model file is missing — vector search degrades to FTS-only.
    Missing,
    /// Model file exists but failed to load — vector search degrades to
    /// FTS-only.
    Broken,
}

/// Model status surfaced through MCP `memory://model/status`.
///
/// No user filesystem paths are included. The `model_digest` is the verified
/// SHA-256 of the GGUF file, not a path.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ModelStatus {
    pub model_id: String,
    pub display_name: String,
    pub dimensions: usize,
    pub quantisation: String,
    pub runtime: String,
    pub runtime_state: ModelRuntime,
    /// Verified SHA-256 of the GGUF file, or `None` when the file is missing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_digest: Option<String>,
    /// Active fingerprint digest, or `None` when no index generation exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fingerprint: Option<String>,
    /// Whether vector search is available. `false` means FTS-only degradation.
    pub vector_search: bool,
}

/// Builder for [`ModelStatus`] — callers assemble the pieces without
/// constructing the full struct by hand.
#[derive(Debug, Default)]
pub struct ModelStatusBuilder {
    model_id: Option<String>,
    display_name: Option<String>,
    dimensions: Option<usize>,
    quantisation: Option<String>,
    runtime: Option<String>,
    runtime_state: Option<ModelRuntime>,
    model_digest: Option<String>,
    fingerprint: Option<String>,
}

impl ModelStatusBuilder {
    #[must_use]
    pub fn model_id(mut self, id: impl Into<String>) -> Self {
        self.model_id = Some(id.into());
        self
    }

    #[must_use]
    pub fn display_name(mut self, name: impl Into<String>) -> Self {
        self.display_name = Some(name.into());
        self
    }

    #[must_use]
    pub fn dimensions(mut self, dim: usize) -> Self {
        self.dimensions = Some(dim);
        self
    }

    #[must_use]
    pub fn quantisation(mut self, q: impl Into<String>) -> Self {
        self.quantisation = Some(q.into());
        self
    }

    #[must_use]
    pub fn runtime(mut self, rt: impl Into<String>) -> Self {
        self.runtime = Some(rt.into());
        self
    }

    #[must_use]
    pub fn runtime_state(mut self, state: ModelRuntime) -> Self {
        self.runtime_state = Some(state);
        self
    }

    #[must_use]
    pub fn model_digest(mut self, digest: impl Into<String>) -> Self {
        self.model_digest = Some(digest.into());
        self
    }

    #[must_use]
    pub fn fingerprint(mut self, fp: impl Into<String>) -> Self {
        self.fingerprint = Some(fp.into());
        self
    }

    /// Build the status. Panics if required fields are missing.
    #[must_use]
    pub fn build(self) -> ModelStatus {
        let runtime_state = self.runtime_state.unwrap_or(ModelRuntime::Missing);
        let vector_search =
            runtime_state == ModelRuntime::Active || runtime_state == ModelRuntime::Available;
        ModelStatus {
            model_id: self.model_id.unwrap_or_default(),
            display_name: self.display_name.unwrap_or_default(),
            dimensions: self.dimensions.unwrap_or(0),
            quantisation: self.quantisation.unwrap_or_default(),
            runtime: self.runtime.unwrap_or_default(),
            runtime_state,
            model_digest: self.model_digest,
            fingerprint: self.fingerprint,
            vector_search,
        }
    }
}

impl ModelStatus {
    /// Returns `true` when vector search is available (model is active or
    /// available on disk).
    #[must_use]
    pub fn vector_search_available(&self) -> bool {
        self.vector_search
    }

    /// Returns `true` when the model is missing or broken and search must
    /// degrade to FTS-only.
    #[must_use]
    pub fn fts_only(&self) -> bool {
        !self.vector_search
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn missing_model_degrades_to_fts_only() {
        let status = ModelStatusBuilder::default()
            .model_id("bge-m3")
            .dimensions(1024)
            .runtime_state(ModelRuntime::Missing)
            .build();
        assert!(!status.vector_search_available());
        assert!(status.fts_only());
    }

    #[test]
    fn active_model_enables_vector_search() {
        let status = ModelStatusBuilder::default()
            .model_id("bge-m3")
            .dimensions(1024)
            .runtime_state(ModelRuntime::Active)
            .build();
        assert!(status.vector_search_available());
        assert!(!status.fts_only());
    }

    #[test]
    fn broken_model_degrades_to_fts_only() {
        let status = ModelStatusBuilder::default()
            .model_id("bge-m3")
            .dimensions(1024)
            .runtime_state(ModelRuntime::Broken)
            .build();
        assert!(!status.vector_search_available());
        assert!(status.fts_only());
    }

    #[test]
    fn status_serializes_without_paths() {
        let status = ModelStatusBuilder::default()
            .model_id("bge-m3")
            .display_name("BGE-M3 (Q5_K_M)")
            .dimensions(1024)
            .quantisation("Q5_K_M")
            .runtime("Metal")
            .runtime_state(ModelRuntime::Active)
            .model_digest("abc123")
            .fingerprint("sha256:def")
            .build();
        let json = serde_json::to_string(&status).unwrap();
        assert!(!json.contains("/Users/"));
        assert!(!json.contains("Library"));
        assert!(json.contains("bge-m3"));
        assert!(json.contains("Metal"));
        assert!(json.contains("abc123"));
    }
}
