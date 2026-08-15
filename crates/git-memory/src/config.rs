//! Minimal configuration store for the active embedding model selection.
//!
//! Persists the user's `git memory model use <id>` choice in a JSON file under
//! the config directory:
//!
//!   1. `$GIT_MEMORY_CONFIG_DIR/config.json` (explicit override),
//!   2. `dirs::config_dir()/git-memory/config.json`.
//!
//! When no config exists, [`resolve_active_model`] falls back to
//! [`platform_default_model`](git_memory_embed::platform_default_model).

use std::io;
use std::path::PathBuf;

use git_memory_embed::{ModelEntry, find_model, platform_default_model};
use serde::{Deserialize, Serialize};

const ENV_CONFIG_DIR: &str = "GIT_MEMORY_CONFIG_DIR";
const CONFIG_FILE: &str = "config.json";

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct Config {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_model: Option<String>,
}

/// Resolve the config directory according to the precedence in the module doc.
fn config_dir() -> Option<PathBuf> {
    if let Ok(env) = std::env::var(ENV_CONFIG_DIR)
        && !env.is_empty()
    {
        return Some(PathBuf::from(env));
    }
    dirs::config_dir().map(|d| d.join("git-memory"))
}

fn config_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(CONFIG_FILE))
}

/// Load config from disk, returning a default if the file is absent.
///
/// If the file exists but cannot be parsed, the error is logged via `tracing`
/// and a default config is returned — this prevents a corrupted config from
/// blocking all operations, but makes the failure visible.
fn load() -> Config {
    let Some(path) = config_path() else {
        return Config::default();
    };
    match std::fs::read_to_string(&path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(config) => config,
            Err(error) => {
                tracing::warn!(
                    path = %path.display(),
                    error = %error,
                    "config file is corrupted — falling back to defaults"
                );
                Config::default()
            }
        },
        Err(_) => Config::default(),
    }
}

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
            "no config directory available; set $GIT_MEMORY_CONFIG_DIR",
        )
    })?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let json = serde_json::to_string_pretty(config)
        .map_err(|e| io::Error::other(format!("config serialization failed: {e}")))?;
    std::fs::write(&path, json + "\n")
}

/// Resolve the active model entry: the configured choice if set and known,
/// otherwise the platform default.
#[must_use]
pub(crate) fn resolve_active_model() -> &'static ModelEntry {
    let config = load();
    if let Some(id) = &config.active_model
        && let Some(entry) = find_model(id)
    {
        return entry;
    }
    platform_default_model()
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
        };
        let json = serde_json::to_string(&config).unwrap();
        let restored: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.active_model.as_deref(), Some("bge-m3"));
    }
}
