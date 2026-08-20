//! Installation registry for shared memory-hub lifecycle.
//!
//! Tracks which consumers (Sync, other clients) depend on the memory-hub
//! installation and which repositories use it. Lives at
//! `config_dir()/memory-hub/registry.json`, overridable via
//! `$MEMORY_HUB_CONFIG_DIR`.
//!
//! The registry stores **no** project content, keys, or credentials — only
//! installation metadata, consumer names with required major versions, and
//! known repository paths (for uninstall warnings).

use std::collections::BTreeSet;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

const ENV_CONFIG_DIR: &str = "MEMORY_HUB_CONFIG_DIR";
const REGISTRY_FILE: &str = "registry.json";
const REGISTRY_SCHEMA_VERSION: u32 = 1;

/// Resolve the config directory according to the precedence:
/// 1. `$MEMORY_HUB_CONFIG_DIR` (explicit override)
/// 2. `dirs::config_dir()/memory-hub`
fn config_dir() -> Option<PathBuf> {
    if let Ok(env) = std::env::var(ENV_CONFIG_DIR)
        && !env.is_empty()
    {
        return Some(PathBuf::from(env));
    }
    dirs::config_dir().map(|d| d.join("memory-hub"))
}

fn registry_path() -> Option<PathBuf> {
    config_dir().map(|d| d.join(REGISTRY_FILE))
}

/// A registered consumer of the memory-hub installation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ConsumerEntry {
    /// Consumer name (e.g. "sync", "custom-client").
    pub name: String,
    /// Required memory interface major version.
    pub required_major: u16,
    /// ISO 8601 timestamp of registration.
    pub registered_at: String,
}

/// Installation metadata recorded in the registry.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct InstallationRecord {
    pub version: String,
    pub binary_path: String,
    /// ISO 8601 timestamp of installation.
    pub installed_at: String,
    /// Optional checksum of the binary (`sha256:...`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checksum: Option<String>,
}

/// The installation registry — a JSON file tracking consumers and repositories.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct InstallationRegistry {
    pub schema_version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub installation: Option<InstallationRecord>,
    #[serde(default)]
    pub consumers: Vec<ConsumerEntry>,
    #[serde(default)]
    pub repositories: BTreeSet<String>,
}

impl InstallationRegistry {
    /// Create a new empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self {
            schema_version: REGISTRY_SCHEMA_VERSION,
            installation: None,
            consumers: Vec::new(),
            repositories: BTreeSet::new(),
        }
    }

    /// Load the registry from disk, returning a default if absent.
    ///
    /// If the file exists but cannot be parsed, the error is returned so the
    /// caller can decide whether to warn or fail.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] when the file exists but cannot be read or parsed.
    pub fn load() -> io::Result<Self> {
        match registry_path() {
            Some(path) => Self::load_from_path(&path),
            None => Ok(Self::new()),
        }
    }

    /// Load the registry from a specific path.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] when the file exists but cannot be read or parsed.
    pub fn load_from_path(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(contents) => {
                let registry: Self = serde_json::from_str(&contents)
                    .map_err(|e| io::Error::other(format!("registry JSON is corrupt: {e}")))?;
                if registry.schema_version != REGISTRY_SCHEMA_VERSION {
                    return Err(io::Error::other(format!(
                        "registry schema version {} is unsupported (expected {})",
                        registry.schema_version, REGISTRY_SCHEMA_VERSION
                    )));
                }
                Ok(registry)
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Self::new()),
            Err(error) => Err(error),
        }
    }

    /// Persist the registry to disk.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] when the config directory cannot be created or the
    /// file cannot be written.
    pub fn save(&self) -> io::Result<()> {
        let path = registry_path().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "no config directory available; set $MEMORY_HUB_CONFIG_DIR",
            )
        })?;
        self.save_to_path(&path)
    }

    /// Persist the registry to a specific path.
    ///
    /// # Errors
    ///
    /// Returns [`io::Error`] when the parent directory cannot be created or the
    /// file cannot be written.
    pub fn save_to_path(&self, path: &Path) -> io::Result<()> {
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "registry path has no parent")
        })?;
        std::fs::create_dir_all(parent)?;
        let json = serde_json::to_string_pretty(self)
            .map_err(|e| io::Error::other(format!("registry serialization failed: {e}")))?;
        // Written through a temporary file: a registry truncated by an
        // interrupted write fails to load, and a failed load blocks uninstall
        // and consumer registration.
        write_atomically(path, parent, (json + "\n").as_bytes())
    }

    /// Register or update a consumer. Idempotent — re-registering the same
    /// consumer updates `registered_at` and `required_major`.
    pub fn register_consumer(&mut self, name: &str, required_major: u16, registered_at: &str) {
        self.consumers.retain(|c| c.name != name);
        self.consumers.push(ConsumerEntry {
            name: name.to_owned(),
            required_major,
            registered_at: registered_at.to_owned(),
        });
        self.consumers.sort_by(|a, b| a.name.cmp(&b.name));
    }

    /// Unregister a consumer. Returns `true` if the consumer was found and
    /// removed.
    pub fn unregister_consumer(&mut self, name: &str) -> bool {
        let before = self.consumers.len();
        self.consumers.retain(|c| c.name != name);
        self.consumers.len() < before
    }

    /// Look up a consumer by name.
    #[must_use]
    #[allow(dead_code)]
    pub fn find_consumer(&self, name: &str) -> Option<&ConsumerEntry> {
        self.consumers.iter().find(|c| c.name == name)
    }

    /// Check whether the installation is compatible with a required major
    /// version. When no installation record exists, returns `true` (no
    /// installation to check against — first-time setup).
    ///
    /// Compatibility is based on the memory interface major version, which
    /// matches the crate major version.
    #[must_use]
    #[allow(dead_code)]
    pub fn is_compatible(&self, required_major: u16) -> bool {
        let Some(installation) = &self.installation else {
            return true;
        };
        let installed_major = installation
            .version
            .split('.')
            .next()
            .and_then(|s| s.parse::<u16>().ok());
        match installed_major {
            Some(installed_major) => installed_major == required_major,
            None => true,
        }
    }

    /// Register a repository path (for uninstall warnings).
    pub fn register_repository(&mut self, path: impl Into<String>) {
        self.repositories.insert(path.into());
    }

    /// Unregister a repository path.
    pub fn unregister_repository(&mut self, path: &str) -> bool {
        self.repositories.remove(path)
    }

    /// Whether any consumers are registered.
    #[must_use]
    pub fn has_consumers(&self) -> bool {
        !self.consumers.is_empty()
    }

    /// Number of registered consumers.
    #[must_use]
    pub fn consumer_count(&self) -> usize {
        self.consumers.len()
    }

    /// Record or update the installation metadata.
    pub fn set_installation(&mut self, record: InstallationRecord) {
        self.installation = Some(record);
    }

    /// Clear the installation record (used by uninstall).
    pub fn clear_installation(&mut self) {
        self.installation = None;
    }
}

/// Replace `path` with `contents` through a temporary file in `parent`, so a
/// crash leaves either the previous file or the new one — never a truncated
/// mix.
pub(crate) fn write_atomically(path: &Path, parent: &Path, contents: &[u8]) -> io::Result<()> {
    use std::io::Write;

    let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
    temporary.write_all(contents)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// Generate an ISO 8601 timestamp for the current UTC time.
#[must_use]
pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format_timestamp(secs)
}

/// Format a Unix timestamp as ISO 8601 (UTC).
fn format_timestamp(secs: u64) -> String {
    let days = secs / 86_400;
    let remainder = secs % 86_400;
    let hour = remainder / 3600;
    let minute = (remainder % 3600) / 60;
    let second = remainder % 60;
    let (year, month, day) = days_to_ymd(days);
    format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}Z")
}

/// Convert days since 1970-01-01 to (year, month, day) using the Howard
/// Hinnant algorithm.
///
/// The input is a Unix-epoch day count taken from the system clock, so the
/// arithmetic stays far inside every type used here.
fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let year_of_era = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = if month <= 2 {
        year_of_era + 1
    } else {
        year_of_era
    };
    (year, month, day)
}

/// Detect the binary path of the currently running memory-hub.
#[must_use]
pub fn current_binary_path() -> Option<String> {
    std::env::current_exe()
        .ok()
        .map(|p| p.to_string_lossy().into_owned())
}

/// Compute the SHA-256 checksum of a file.
///
/// # Errors
///
/// Returns [`io::Error`] when the file cannot be read.
pub fn file_checksum(path: &Path) -> io::Result<String> {
    use sha2::Digest;
    let data = std::fs::read(path)?;
    let digest = sha2::Sha256::digest(&data);
    Ok(format!("sha256:{digest:x}"))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn empty_registry_is_compatible_with_anything() {
        let registry = InstallationRegistry::new();
        assert!(registry.is_compatible(1));
        assert!(registry.is_compatible(2));
        assert!(registry.is_compatible(99));
    }

    #[test]
    fn register_consumer_is_idempotent() {
        let mut registry = InstallationRegistry::new();
        registry.register_consumer("sync", 1, "2026-01-01T00:00:00Z");
        assert_eq!(registry.consumer_count(), 1);
        registry.register_consumer("sync", 1, "2026-02-01T00:00:00Z");
        assert_eq!(registry.consumer_count(), 1);
        assert_eq!(
            registry.find_consumer("sync").unwrap().registered_at,
            "2026-02-01T00:00:00Z"
        );
    }

    #[test]
    fn unregister_consumer_removes_it() {
        let mut registry = InstallationRegistry::new();
        registry.register_consumer("sync", 1, "2026-01-01T00:00:00Z");
        registry.register_consumer("other", 1, "2026-01-01T00:00:00Z");
        assert!(registry.unregister_consumer("sync"));
        assert!(!registry.has_consumers() || registry.consumer_count() == 1);
        assert!(registry.find_consumer("sync").is_none());
        assert!(registry.find_consumer("other").is_some());
    }

    #[test]
    fn unregister_missing_consumer_returns_false() {
        let mut registry = InstallationRegistry::new();
        assert!(!registry.unregister_consumer("nonexistent"));
    }

    #[test]
    fn is_compatible_with_same_major() {
        let mut registry = InstallationRegistry::new();
        registry.set_installation(InstallationRecord {
            version: "0.1.0".into(),
            binary_path: "/usr/local/bin/memory-hub".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            checksum: None,
        });
        assert!(registry.is_compatible(0));
    }

    #[test]
    fn is_incompatible_with_different_major() {
        let mut registry = InstallationRegistry::new();
        registry.set_installation(InstallationRecord {
            version: "1.0.0".into(),
            binary_path: "/usr/local/bin/memory-hub".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            checksum: None,
        });
        assert!(!registry.is_compatible(2));
        assert!(registry.is_compatible(1));
    }

    #[test]
    fn registry_round_trips_through_serde() {
        let mut registry = InstallationRegistry::new();
        registry.set_installation(InstallationRecord {
            version: "0.1.0".into(),
            binary_path: "/usr/local/bin/memory-hub".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            checksum: Some("sha256:abc".into()),
        });
        registry.register_consumer("sync", 1, "2026-01-01T00:00:00Z");
        registry.register_repository("/home/user/project");

        let json = serde_json::to_string_pretty(&registry).unwrap();
        let restored: InstallationRegistry = serde_json::from_str(&json).unwrap();

        assert_eq!(restored.schema_version, 1);
        assert!(restored.installation.is_some());
        assert_eq!(restored.consumer_count(), 1);
        assert!(restored.repositories.contains("/home/user/project"));
    }

    #[test]
    fn repositories_are_deduplicated() {
        let mut registry = InstallationRegistry::new();
        registry.register_repository("/path/a");
        registry.register_repository("/path/a");
        registry.register_repository("/path/b");
        assert_eq!(registry.repositories.len(), 2);
    }

    #[test]
    fn unregister_repository_returns_true_when_present() {
        let mut registry = InstallationRegistry::new();
        registry.register_repository("/path/a");
        assert!(registry.unregister_repository("/path/a"));
        assert!(!registry.unregister_repository("/path/a"));
    }

    #[test]
    fn format_timestamp_known_value() {
        // 2026-01-01T00:00:00Z = 1767225600 seconds since epoch
        let ts = format_timestamp(1_767_225_600);
        assert_eq!(ts, "2026-01-01T00:00:00Z");
    }

    #[test]
    fn consumers_sorted_by_name() {
        let mut registry = InstallationRegistry::new();
        registry.register_consumer("zeta", 1, "2026-01-01T00:00:00Z");
        registry.register_consumer("alpha", 1, "2026-01-01T00:00:00Z");
        registry.register_consumer("mid", 1, "2026-01-01T00:00:00Z");
        let names: Vec<&str> = registry.consumers.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "mid", "zeta"]);
    }

    #[test]
    fn load_from_temp_dir() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(REGISTRY_FILE);
        let mut registry = InstallationRegistry::new();
        registry.register_consumer("sync", 1, "2026-01-01T00:00:00Z");
        registry.save_to_path(&path).unwrap();

        let loaded = InstallationRegistry::load_from_path(&path).unwrap();
        assert_eq!(loaded.consumer_count(), 1);
        assert!(loaded.find_consumer("sync").is_some());
    }

    #[test]
    fn load_returns_default_when_file_absent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(REGISTRY_FILE);
        let registry = InstallationRegistry::load_from_path(&path).unwrap();
        assert_eq!(registry.consumer_count(), 0);
    }

    #[test]
    fn load_rejects_wrong_schema_version() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(REGISTRY_FILE);
        let bad_json = r#"{"schema_version": 99, "consumers": []}"#;
        std::fs::write(&path, bad_json).unwrap();
        let result = InstallationRegistry::load_from_path(&path);
        assert!(result.is_err());
    }

    #[test]
    fn clear_installation_removes_record() {
        let mut registry = InstallationRegistry::new();
        registry.set_installation(InstallationRecord {
            version: "0.1.0".into(),
            binary_path: "/usr/local/bin/memory-hub".into(),
            installed_at: "2026-01-01T00:00:00Z".into(),
            checksum: None,
        });
        registry.clear_installation();
        assert!(registry.installation.is_none());
    }
}
