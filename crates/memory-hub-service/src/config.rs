//! Which storages a project has, and what each of them holds.
//!
//! A project declares its storages by name — `main`, `docs`, `media` — and a
//! type points at one of those names. The name answers "which of mine"; `kind`
//! answers "what is it". Neither question is answered by looking somewhere
//! else, which is the whole reason this file exists rather than a default
//! buried in the engine.
//!
//! It lives on disk beside the project rather than as a record, because reading
//! a record means already knowing where records are. It is meant to be
//! committed: a colleague who clones the repository should learn where the
//! memory is from the repository, not from being told.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::ServiceError;

/// Where the declaration lives, relative to the project root.
pub const CONFIG_PATH: &str = ".memory/config.json";

/// Version of the declaration format itself.
const CONFIG_VERSION: u32 = 1;

/// What a storage is.
///
/// Open by design: a name this build does not know survives parsing and is
/// refused by name, so a project written by a newer build says what it wanted
/// instead of failing as malformed JSON.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageKind {
    /// Git objects under private refs. Invisible in the working tree, versioned
    /// and pushable, and only Memory writes it.
    Refs,
    /// A folder of record files. Visible, needs no Git, keeps no past.
    Folder,
    /// A directory of the working tree, holding documents people edit.
    RepoFolder,
}

impl StorageKind {
    /// Whether this kind can hold records at all.
    ///
    /// A repository folder cannot: its files are documents somebody else owns,
    /// and an envelope written among them would be a file nobody asked for.
    #[must_use]
    pub const fn can_hold_records(self) -> bool {
        matches!(self, Self::Refs | Self::Folder)
    }

    /// Whether a `path` is required, and what it means.
    #[must_use]
    pub const fn needs_path(self) -> bool {
        matches!(self, Self::Folder | Self::RepoFolder)
    }
}

/// What a storage is used for.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Holds {
    /// Envelopes: keys, kinds, titles, links, freshness.
    Records,
    /// Bodies of the types that name this storage.
    Content,
}

/// One declared storage.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StorageConfig {
    pub kind: StorageKind,
    /// Location, relative to the project root. Required by the kinds that have
    /// one and meaningless to the kinds that do not.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    pub holds: BTreeSet<Holds>,
    /// Name to give a document this storage creates, with `*` where the
    /// record's key goes.
    ///
    /// A default, never a filter: what the storage already holds is read
    /// whatever it is called. Absent means `*.md`, which is what a folder of
    /// notes is, and a caller that wants `.txt` or `.html` says so when it
    /// creates the document.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub new_files: Option<String>,
}

/// What a document is called when nobody says otherwise.
pub const DEFAULT_NEW_FILES: &str = "*.md";

/// Where a folder storage lives when nobody says otherwise.
///
/// The same directory the declaration is in: records, index and configuration
/// are one thing to copy, one thing to ignore, and one thing to delete.
pub const DEFAULT_RECORDS_PATH: &str = ".memory";

impl StorageConfig {
    /// Records under private Git refs: versioned, pushable, and invisible in
    /// the working tree.
    #[must_use]
    pub fn refs() -> Self {
        Self {
            kind: StorageKind::Refs,
            path: None,
            holds: [Holds::Records, Holds::Content].into_iter().collect(),
            new_files: None,
        }
    }

    /// Records as files in a folder: visible, needing no Git, keeping no past.
    #[must_use]
    pub fn folder(path: impl Into<String>) -> Self {
        Self {
            kind: StorageKind::Folder,
            path: Some(path.into()),
            holds: [Holds::Records, Holds::Content].into_iter().collect(),
            new_files: None,
        }
    }

    /// A directory of the working tree, holding documents people edit.
    #[must_use]
    pub fn repo_folder(path: impl Into<String>) -> Self {
        Self {
            kind: StorageKind::RepoFolder,
            path: Some(path.into()),
            holds: [Holds::Content].into_iter().collect(),
            new_files: None,
        }
    }

    /// The naming pattern for documents this storage creates.
    #[must_use]
    pub fn new_files(&self) -> &str {
        self.new_files.as_deref().unwrap_or(DEFAULT_NEW_FILES)
    }

    #[must_use]
    pub fn holds(&self, what: Holds) -> bool {
        self.holds.contains(&what)
    }
}

/// Every storage a project has.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectConfig {
    pub config_version: u32,
    pub storages: BTreeMap<String, StorageConfig>,
}

impl ProjectConfig {
    /// Build a declaration and check it holds together.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] describing the rule that refused it.
    pub fn new(storages: BTreeMap<String, StorageConfig>) -> Result<Self, ServiceError> {
        let config = Self {
            config_version: CONFIG_VERSION,
            storages,
        };
        config.validate()?;
        Ok(config)
    }

    /// Read the declaration a project keeps.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the file is absent, unreadable, of an
    /// unknown version, or does not hold together.
    pub fn load(project: &Path) -> Result<Self, ServiceError> {
        let path = Self::path_in(project);
        let text = fs::read_to_string(&path).map_err(|error| {
            ServiceError::new(
                "not_initialised",
                "this project has no memory configuration — run `init` first",
                serde_json::json!({
                    "path": path.display().to_string(),
                    "detail": error.to_string(),
                }),
            )
        })?;
        let config: Self = serde_json::from_str(&text).map_err(|error| {
            ServiceError::new(
                "invalid_argument",
                "the memory configuration cannot be read",
                serde_json::json!({
                    "path": path.display().to_string(),
                    "detail": error.to_string(),
                }),
            )
        })?;
        if config.config_version != CONFIG_VERSION {
            return Err(ServiceError::new(
                "unsupported",
                "this memory configuration was written by a different version",
                serde_json::json!({
                    "found": config.config_version,
                    "supported": CONFIG_VERSION,
                }),
            ));
        }
        config.validate()?;
        Ok(config)
    }

    /// Write the declaration, refusing to overwrite one that is already there.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when a configuration already exists or the file
    /// cannot be written.
    pub fn save_new(&self, project: &Path) -> Result<PathBuf, ServiceError> {
        let path = Self::path_in(project);
        if path.exists() {
            return Err(ServiceError::new(
                "conflict",
                "this project already has a memory configuration",
                serde_json::json!({"path": path.display().to_string()}),
            ));
        }
        self.save(project)
    }

    /// Add a storage to a project that already has a declaration.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the name is taken, when the result would
    /// not hold together, or when the file cannot be written.
    pub fn declare(&self, name: &str, storage: StorageConfig) -> Result<Self, ServiceError> {
        if self.storages.contains_key(name) {
            return Err(ServiceError::new(
                "conflict",
                format!("this project already declares a storage named `{name}`"),
                serde_json::json!({"field": "name", "name": name}),
            ));
        }
        let mut storages = self.storages.clone();
        storages.insert(name.to_owned(), storage);
        Self::new(storages)
    }

    /// Write a declaration over the one already there.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when the file cannot be written.
    pub fn save(&self, project: &Path) -> Result<PathBuf, ServiceError> {
        let path = Self::path_in(project);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| write_failed(&path, &error))?;
        }
        let mut bytes = serde_json::to_vec_pretty(self).map_err(|error| {
            ServiceError::new(
                "internal",
                "the memory configuration could not be serialised",
                serde_json::json!({"detail": error.to_string()}),
            )
        })?;
        bytes.push(b'\n');
        fs::write(&path, &bytes).map_err(|error| write_failed(&path, &error))?;
        Ok(path)
    }

    #[must_use]
    pub fn path_in(project: &Path) -> PathBuf {
        project.join(CONFIG_PATH)
    }

    /// The storage records live in, and its name.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] when no storage holds records — which a
    /// validated configuration never is.
    pub fn record_storage(&self) -> Result<(&str, &StorageConfig), ServiceError> {
        self.storages
            .iter()
            .find(|(_, storage)| storage.holds(Holds::Records))
            .map(|(name, storage)| (name.as_str(), storage))
            .ok_or_else(|| {
                ServiceError::new(
                    "invalid_argument",
                    "no storage in this project holds records",
                    serde_json::json!({"field": "storages"}),
                )
            })
    }

    /// Look up a storage a type named.
    ///
    /// # Errors
    ///
    /// Returns [`ServiceError`] naming what is declared, because a type
    /// pointing at a storage that is not there is almost always a typo.
    pub fn storage(&self, name: &str) -> Result<&StorageConfig, ServiceError> {
        self.storages.get(name).ok_or_else(|| {
            ServiceError::new(
                "invalid_argument",
                format!("no storage named `{name}` is declared by this project"),
                serde_json::json!({
                    "field": "storage",
                    "storage": name,
                    "declared": self.storages.keys().collect::<Vec<_>>(),
                }),
            )
        })
    }

    /// Check every rule a declaration has to satisfy.
    fn validate(&self) -> Result<(), ServiceError> {
        if self.storages.is_empty() {
            return Err(invalid("storages", "a project must declare a storage"));
        }

        for (name, storage) in &self.storages {
            check_name(name)?;
            if storage.holds.is_empty() {
                return Err(invalid(
                    &format!("storages.{name}.holds"),
                    "a storage that holds nothing has no reason to be declared",
                ));
            }
            if storage.holds(Holds::Records) && !storage.kind.can_hold_records() {
                return Err(invalid(
                    &format!("storages.{name}.holds"),
                    "this kind of storage cannot hold records — its files belong to somebody else",
                ));
            }
            match (storage.kind.needs_path(), &storage.path) {
                (true, None) => {
                    return Err(invalid(
                        &format!("storages.{name}.path"),
                        "this kind of storage must say where it is",
                    ));
                }
                (false, Some(_)) => {
                    return Err(invalid(
                        &format!("storages.{name}.path"),
                        "this kind of storage has no path",
                    ));
                }
                _ => {}
            }
            if let Some(path) = &storage.path {
                check_path(name, path)?;
            }
            if let Some(pattern) = &storage.new_files {
                check_new_files(name, pattern)?;
            }
        }

        let holders: Vec<&str> = self
            .storages
            .iter()
            .filter(|(_, storage)| storage.holds(Holds::Records))
            .map(|(name, _)| name.as_str())
            .collect();
        match holders.len() {
            1 => Ok(()),
            0 => Err(invalid(
                "storages",
                "exactly one storage must hold records, and none does",
            )),
            _ => Err(ServiceError::new(
                "invalid_argument",
                "exactly one storage must hold records, and more than one does",
                serde_json::json!({"field": "storages", "holders": holders}),
            )),
        }
    }
}

/// A name is written by a person and read by a person.
///
/// The rule itself lives in the schema crate, where a type reads it to point
/// at a storage. One rule, because a name a type may write is exactly a name a
/// project may declare.
fn check_name(name: &str) -> Result<(), ServiceError> {
    if memory_hub_schema::is_storage_name(name) {
        return Ok(());
    }
    Err(ServiceError::new(
        "invalid_argument",
        memory_hub_schema::STORAGE_NAME_RULE,
        serde_json::json!({"field": "storages", "name": name}),
    ))
}

/// A path in the declaration is project-relative and stays inside the project.
fn check_path(name: &str, path: &str) -> Result<(), ServiceError> {
    let invalid_path = path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path.contains('\\')
        || path.contains("//")
        || path.bytes().any(|byte| byte.is_ascii_control())
        || path
            .split('/')
            .any(|part| part == ".." || part == "." || part.is_empty());
    if invalid_path {
        return Err(invalid(
            &format!("storages.{name}.path"),
            "a path must be a normalized project-relative directory",
        ));
    }
    // Git's own directory is inside the project and is not part of it.
    if path.split('/').next() == Some(".git") {
        return Err(invalid(
            &format!("storages.{name}.path"),
            "`.git` is Git's own directory, not a folder of the project",
        ));
    }
    Ok(())
}

/// The pattern names files, so it may not carry a separator, and it must have
/// a `*`: that is where the record's key goes, and without one every document
/// this storage created would be written to the same file.
fn check_new_files(name: &str, pattern: &str) -> Result<(), ServiceError> {
    if pattern.is_empty() || pattern.contains('/') || pattern.contains('\\') {
        return Err(invalid(
            &format!("storages.{name}.new_files"),
            "a name pattern names files within the storage and carries no separator",
        ));
    }
    if !pattern.contains('*') {
        return Err(invalid(
            &format!("storages.{name}.new_files"),
            "a name pattern must contain `*`: it is where the record's key goes",
        ));
    }
    Ok(())
}

fn invalid(field: &str, message: &str) -> ServiceError {
    ServiceError::new(
        "invalid_argument",
        message,
        serde_json::json!({"field": field}),
    )
}

fn write_failed(path: &Path, error: &std::io::Error) -> ServiceError {
    ServiceError::new(
        "internal",
        "the memory configuration could not be written",
        serde_json::json!({
            "path": path.display().to_string(),
            "detail": error.to_string(),
        }),
    )
}
