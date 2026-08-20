//! Active-model resolution shared by every entry point.
//!
//! The user's `memory-hub model use <id>` choice lives in a small JSON file
//! under the config directory:
//!
//!   1. `$MEMORY_HUB_CONFIG_DIR/config.json` (explicit override),
//!   2. `dirs::config_dir()/memory-hub/config.json`.
//!
//! Resolution lives here rather than in the CLI so the MCP server reaches the
//! same model the CLI indexed with — a projection built with one model and
//! searched with another produces an incompatible fingerprint and silently
//! loses the vector channel.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::download::{
    DownloadOpts, ModelVerification, model_path as cached_model_path, verify_model_sync,
};
use crate::llama_cpp::LlamaCppProvider;
use crate::provider::EmbeddingProvider;
use crate::registry::{ModelEntry, find, platform_default_model};

const ENV_CONFIG_DIR: &str = "MEMORY_HUB_CONFIG_DIR";
const CONFIG_FILE: &str = "config.json";

/// On-disk model configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_model: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub embedding_enabled: bool,
}

/// Resolve the config directory according to the precedence in the module doc.
#[must_use]
pub fn config_dir() -> Option<PathBuf> {
    if let Ok(env) = std::env::var(ENV_CONFIG_DIR)
        && !env.is_empty()
    {
        return Some(PathBuf::from(env));
    }
    dirs::config_dir().map(|dir| dir.join("memory-hub"))
}

/// Path of the model configuration file.
#[must_use]
pub fn config_path() -> Option<PathBuf> {
    config_dir().map(|dir| dir.join(CONFIG_FILE))
}

/// Load the model configuration, falling back to defaults when the file is
/// absent or unreadable. A corrupt file is reported through `tracing` rather
/// than blocking every operation.
#[must_use]
pub fn load_config() -> ModelConfig {
    let Some(path) = config_path() else {
        return ModelConfig::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => serde_json::from_str(&contents).unwrap_or_else(|error| {
            tracing::warn!(
                path = %path.display(),
                error = %error,
                "model config is corrupted — falling back to defaults"
            );
            ModelConfig::default()
        }),
        Err(_) => ModelConfig::default(),
    }
}

/// The active model entry: the configured choice when it names a known model,
/// otherwise the platform default.
#[must_use]
pub fn active_model() -> &'static ModelEntry {
    load_config()
        .active_model
        .as_deref()
        .and_then(find)
        .unwrap_or_else(platform_default_model)
}

/// Whether the active model's GGUF is on disk and matches its pinned digest.
///
/// This hashes the whole file, so it belongs in diagnostics (`doctor`,
/// `model list`) rather than on a request path.
#[must_use]
pub fn active_model_is_verified() -> bool {
    matches!(
        verify_model_sync(active_model(), &DownloadOpts::default()),
        Ok(ModelVerification::Present { .. })
    )
}

/// Path of the active model's GGUF when it exists on disk.
///
/// Existence only — verifying the digest means hashing hundreds of megabytes,
/// which `model download` and `doctor` already do.
#[must_use]
pub fn active_model_path() -> Option<PathBuf> {
    let path = cached_model_path(active_model(), &DownloadOpts::default()).ok()?;
    path.is_file().then_some(path)
}

/// Build a provider for the active model, or `None` when its GGUF is not on
/// disk — the caller then runs FTS-only.
///
/// Constructing the provider is cheap: [`LlamaCppProvider`] loads the GGUF on
/// its first embed call, so resolving this eagerly does not pay the model's
/// memory cost until vectors are actually needed.
#[must_use]
pub fn resolve_active_provider() -> Option<Arc<dyn EmbeddingProvider>> {
    let path = active_model_path()?;
    Some(Arc::new(LlamaCppProvider::new(active_model(), path)))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{ModelConfig, active_model, load_config};

    #[test]
    fn config_round_trips_through_serde() {
        let config = ModelConfig {
            active_model: Some("bge-m3".to_owned()),
            embedding_enabled: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: ModelConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.active_model.as_deref(), Some("bge-m3"));
        assert!(restored.embedding_enabled);
    }

    #[test]
    fn the_active_model_is_always_a_registry_entry() {
        // Whatever the ambient configuration says — including an unknown or
        // absent model id — resolution lands on a model the registry knows,
        // never on a dangling id.
        let active = active_model();
        assert!(
            crate::registry::all_models()
                .iter()
                .any(|entry| entry.id == active.id),
            "active model {} is not in the registry",
            active.id
        );
    }

    #[test]
    fn a_config_without_a_model_leaves_the_choice_to_the_platform_default() {
        let config: ModelConfig = serde_json::from_str("{}").unwrap();
        assert!(config.active_model.is_none());
        assert!(!config.embedding_enabled);
        let _ = load_config();
    }
}
