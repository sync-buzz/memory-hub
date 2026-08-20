//! Minimal configuration store for the active embedding model selection.
//!
//! Persists the user's `memory-hub model use <id>` choice in a JSON file under
//! the config directory:
//!
//!   1. `$MEMORY_HUB_CONFIG_DIR/config.json` (explicit override),
//!   2. `dirs::config_dir()/memory-hub/config.json`.
//!
//! When no config exists, [`resolve_active_model`] falls back to
//! [`platform_default_model`](memory_hub_embed::platform_default_model).

use std::io;

use memory_hub_embed::{ModelConfig as Config, ModelEntry, config_path, load_config as load};

/// Persist config to disk.
///
/// # Errors
///
/// Returns an I/O error when the config directory cannot be created or the
/// file cannot be written.
fn save(config: &Config) -> io::Result<()> {
    let path = config_path().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "no config directory available; set $MEMORY_HUB_CONFIG_DIR",
        )
    })?;
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "config path has no parent"))?;
    std::fs::create_dir_all(parent)?;
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| io::Error::other(format!("config serialization failed: {e}")))?;
    // Atomic replace: an interrupted write would otherwise leave a truncated
    // config, which silently reverts the active model to the platform default.
    crate::registry::write_atomically(&path, parent, (json + "\n").as_bytes())
}

/// Resolve the active model entry: the configured choice if set and known,
/// otherwise the platform default.
#[must_use]
pub(crate) fn resolve_active_model() -> &'static ModelEntry {
    memory_hub_embed::active_model()
}

/// Set the active model id in config and persist to disk.
///
/// # Errors
///
/// Returns an I/O error when the config cannot be written.
pub(crate) fn set_active_model(id: &str) -> io::Result<()> {
    let mut config = load();
    config.active_model = Some(id.to_owned());
    save(&config)
}

/// The configured active model id, or `None` when config is absent (platform
/// default is in effect).
#[must_use]
pub(crate) fn configured_model_id() -> Option<String> {
    load().active_model
}

/// Whether embedding is enabled in config (default: `false`).
#[must_use]
#[allow(dead_code)]
pub(crate) fn embedding_enabled() -> bool {
    load().embedding_enabled
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_no_active_model() {
        let config = Config::default();
        assert!(config.active_model.is_none());
    }

    #[test]
    fn config_round_trips_through_serde() {
        let config = Config {
            active_model: Some("bge-m3".to_owned()),
            embedding_enabled: true,
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.active_model.as_deref(), Some("bge-m3"));
        assert!(restored.embedding_enabled);
    }
}
