//! Model fingerprint — ties model digest, dimension, renderer version and
//! runtime together.
//!
//! A fingerprint change creates an incompatible index generation. The
//! projection stores the fingerprint and refuses to mix vectors from
//! different fingerprints: a mismatch forces a full rebuild instead of
//! silently corrupting search results.

use crate::provider::EmbeddingProvider;
use crate::renderer::RENDERER_VERSION;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

/// Bumped when the fingerprint composition changes.
const FINGERPRINT_SCHEMA: u32 = 1;

/// Error returned when two fingerprints are incompatible.
#[derive(Debug, Error)]
pub enum FingerprintError {
    #[error("fingerprint mismatch: stored {stored}, active {active} — rebuild required")]
    Mismatch { stored: String, active: String },
}

/// A stable hash that uniquely identifies the combination of model, renderer
/// and runtime that produced a set of vectors.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Fingerprint {
    /// SHA-256 of the GGUF file on disk. Ties the vectors to a specific model
    /// file, not just a model id — re-quantisation or corruption is caught.
    pub model_digest: String,
    /// Output dimension. A dimension change is always incompatible.
    pub dimensions: usize,
    /// Renderer version. A rendering logic change invalidates all vectors.
    pub renderer_version: u32,
    /// Compile-time acceleration backend (e.g. "Metal", "CPU"). Different
    /// backends may produce slightly different float values; a backend change
    /// forces a rebuild to avoid mixing.
    pub runtime: String,
    /// Schema version of the fingerprint itself.
    pub schema: u32,
}

impl Fingerprint {
    /// Build the active fingerprint from a provider and a verified model
    /// digest.
    #[must_use]
    pub fn from_provider(provider: &dyn EmbeddingProvider, model_digest: &str) -> Self {
        Self {
            model_digest: model_digest.to_string(),
            dimensions: provider.dimensions(),
            renderer_version: RENDERER_VERSION,
            runtime: crate::llama_cpp::backend_name().to_string(),
            schema: FINGERPRINT_SCHEMA,
        }
    }

    /// Stable string representation for storage and comparison.
    #[must_use]
    pub fn digest(&self) -> String {
        let mut hasher = Sha256::new();
        hasher.update(self.model_digest.as_bytes());
        hasher.update(self.dimensions.to_le_bytes());
        hasher.update(self.renderer_version.to_le_bytes());
        hasher.update(self.runtime.as_bytes());
        hasher.update(self.schema.to_le_bytes());
        format!("sha256:{:x}", hasher.finalize())
    }

    /// Returns `Ok(())` when `self` is compatible with `other`, or an error
    /// describing the mismatch.
    ///
    /// # Errors
    ///
    /// Returns [`FingerprintError::Mismatch`] when the digests differ.
    pub fn require_compatible(&self, other: &Self) -> Result<(), FingerprintError> {
        if self.digest() == other.digest() {
            Ok(())
        } else {
            Err(FingerprintError::Mismatch {
                stored: other.digest(),
                active: self.digest(),
            })
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::mock::MockProvider;

    #[test]
    fn same_inputs_produce_same_digest() {
        let provider = MockProvider::new(768);
        let fp1 = Fingerprint::from_provider(&provider, "abc123");
        let fp2 = Fingerprint::from_provider(&provider, "abc123");
        assert_eq!(fp1.digest(), fp2.digest());
    }

    #[test]
    fn different_model_digest_changes_fingerprint() {
        let provider = MockProvider::new(768);
        let fp1 = Fingerprint::from_provider(&provider, "aaa");
        let fp2 = Fingerprint::from_provider(&provider, "bbb");
        assert_ne!(fp1.digest(), fp2.digest());
        assert!(fp1.require_compatible(&fp2).is_err());
    }

    #[test]
    fn different_dimensions_changes_fingerprint() {
        let p1 = MockProvider::new(768);
        let p2 = MockProvider::new(1024);
        let fp1 = Fingerprint::from_provider(&p1, "same");
        let fp2 = Fingerprint::from_provider(&p2, "same");
        assert_ne!(fp1.digest(), fp2.digest());
        assert!(fp1.require_compatible(&fp2).is_err());
    }

    #[test]
    fn compatible_fingerprints_pass() {
        let provider = MockProvider::new(768);
        let fp1 = Fingerprint::from_provider(&provider, "same");
        let fp2 = Fingerprint::from_provider(&provider, "same");
        assert!(fp1.require_compatible(&fp2).is_ok());
    }

    #[test]
    fn digest_is_stable_and_prefixed() {
        let provider = MockProvider::new(384);
        let fp = Fingerprint::from_provider(&provider, "deadbeef");
        let digest = fp.digest();
        assert!(digest.starts_with("sha256:"));
        assert_eq!(digest.len(), "sha256:".len() + 64);
    }
}
