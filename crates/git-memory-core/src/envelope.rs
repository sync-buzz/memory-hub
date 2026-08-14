use std::collections::{BTreeMap, HashSet};
use std::fmt;

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{CURRENT_ENVELOPE_VERSION, ContractError, FormatVersion};

const RESERVED_FIELDS: &[&str] = &[
    "envelope_version",
    "key",
    "kind",
    "content",
    "title",
    "tags",
    "links",
    "source_paths",
    "archive",
    "freshness",
    "content_hash",
    "profile",
];

/// Hash of the exact UTF-8 content stored in an envelope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    #[must_use]
    pub fn for_content(content: &str) -> Self {
        let digest = Sha256::digest(content.as_bytes());
        Self(format!("sha256:{digest:x}"))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl<'de> Deserialize<'de> for ContentHash {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        let digest = value.strip_prefix("sha256:").unwrap_or_default();
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(serde::de::Error::custom(
                "content_hash must be `sha256:` followed by 64 lowercase hex digits",
            ));
        }
        Ok(Self(value))
    }
}

/// A typed relation to another record. The target kind is intentionally not
/// required: clients may introduce kinds without changing the Memory format.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordLink {
    pub key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// Code paths required by standalone rebuild and reconciliation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourcePaths {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed: Vec<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArchiveState {
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FreshnessState {
    #[default]
    Unverified,
    Fresh,
    Stale,
    Invalid,
}

/// Canonical freshness inputs. `code_revision` is the code snapshot against
/// which this record was last evaluated; timestamps are descriptive only.
#[derive(Clone, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Freshness {
    #[serde(default)]
    pub state: FreshnessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// Client-owned interpretation of `metadata`.
///
/// Profile versioning is deliberately independent of envelope versioning.
/// Memory retains metadata values but never gives them authority to replace
/// reserved envelope fields.
#[derive(Clone, Deserialize, PartialEq, Serialize)]
pub struct ClientProfile {
    pub name: String,
    pub version: FormatVersion,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// Product-neutral canonical record.
#[derive(Clone, PartialEq, Serialize)]
pub struct Envelope {
    pub envelope_version: FormatVersion,
    pub key: String,
    pub kind: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<RecordLink>,
    #[serde(default)]
    pub source_paths: SourcePaths,
    #[serde(default)]
    pub archive: ArchiveState,
    #[serde(default)]
    pub freshness: Freshness,
    pub content_hash: ContentHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<ClientProfile>,
    /// Compatible fields introduced by future envelope minor versions.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

impl fmt::Debug for Freshness {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Freshness")
            .field("state", &self.state)
            .field("code_revision", &self.code_revision)
            .field("validated_at", &self.validated_at)
            .field("reason", &self.reason.as_ref().map(|_| "<redacted>"))
            .field("extension_count", &self.extensions.len())
            .finish_non_exhaustive()
    }
}

impl fmt::Debug for ClientProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ClientProfile")
            .field("name", &self.name)
            .field("version", &self.version)
            .field("metadata_count", &self.metadata.len())
            .field("extension_count", &self.extensions.len())
            .finish()
    }
}

impl fmt::Debug for Envelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Envelope")
            .field("envelope_version", &self.envelope_version)
            .field("key", &"<redacted>")
            .field("kind", &self.kind)
            .field("content", &"<redacted>")
            .field("content_hash", &self.content_hash)
            .field("tag_count", &self.tags.len())
            .field("link_count", &self.links.len())
            .field(
                "source_path_count",
                &(self.source_paths.scope.len() + self.source_paths.observed.len()),
            )
            .field("archived", &self.archive.archived)
            .field("freshness_state", &self.freshness.state)
            .field("has_profile", &self.profile.is_some())
            .field("extension_count", &self.extensions.len())
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct RawEnvelope {
    envelope_version: FormatVersion,
    key: String,
    kind: String,
    content: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    tags: Vec<String>,
    #[serde(default)]
    links: Vec<RecordLink>,
    #[serde(default)]
    source_paths: SourcePaths,
    #[serde(default)]
    archive: ArchiveState,
    #[serde(default)]
    freshness: Freshness,
    content_hash: ContentHash,
    #[serde(default)]
    profile: Option<ClientProfile>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

impl Envelope {
    /// Construct a current-version envelope and its content hash.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError`] when `key` or `kind` is empty.
    pub fn new(
        key: impl Into<String>,
        kind: impl Into<String>,
        content: impl Into<String>,
    ) -> Result<Self, ContractError> {
        let content = content.into();
        let envelope = Self {
            envelope_version: CURRENT_ENVELOPE_VERSION,
            key: key.into(),
            kind: kind.into(),
            content_hash: ContentHash::for_content(&content),
            content,
            title: None,
            tags: Vec::new(),
            links: Vec::new(),
            source_paths: SourcePaths::default(),
            archive: ArchiveState::default(),
            freshness: Freshness::default(),
            profile: None,
            extensions: BTreeMap::new(),
        };
        envelope.validate()?;
        Ok(envelope)
    }

    /// Recompute the digest after changing `content`.
    pub fn refresh_content_hash(&mut self) {
        self.content_hash = ContentHash::for_content(&self.content);
    }

    /// Validate before every durable write.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError`] for an incompatible major version, stale
    /// content hash, invalid path, duplicate tag, or malformed nested field.
    pub fn validate(&self) -> Result<(), ContractError> {
        self.envelope_version
            .require_major("envelope_version", CURRENT_ENVELOPE_VERSION.major)?;
        require_non_empty("key", &self.key)?;
        require_non_empty("kind", &self.kind)?;
        if self.content_hash != ContentHash::for_content(&self.content) {
            return Err(ContractError::invalid(
                "content_hash",
                "content_hash does not match content",
            ));
        }
        validate_unique_non_empty("tags", &self.tags)?;
        validate_paths("source_paths.scope", &self.source_paths.scope)?;
        validate_paths("source_paths.observed", &self.source_paths.observed)?;
        for (index, link) in self.links.iter().enumerate() {
            require_non_empty(&format!("links[{index}].key"), &link.key)?;
            validate_extensions(
                &format!("links[{index}].extensions"),
                &link.extensions,
                &["key", "relation"],
            )?;
        }
        validate_extensions(
            "source_paths.extensions",
            &self.source_paths.extensions,
            &["scope", "observed"],
        )?;
        validate_extensions(
            "archive.extensions",
            &self.archive.extensions,
            &["archived", "archived_at"],
        )?;
        validate_extensions(
            "freshness.extensions",
            &self.freshness.extensions,
            &["state", "code_revision", "validated_at", "reason"],
        )?;
        if self.archive.archived_at.is_some() && !self.archive.archived {
            return Err(ContractError::invalid(
                "archive.archived_at",
                "archived_at requires archived=true",
            ));
        }
        if let Some(profile) = &self.profile {
            require_non_empty("profile.name", &profile.name)?;
            validate_extensions(
                "profile.extensions",
                &profile.extensions,
                &["name", "version", "metadata"],
            )?;
        }
        validate_extensions("extensions", &self.extensions, RESERVED_FIELDS)?;
        Ok(())
    }
}

impl<'de> Deserialize<'de> for Envelope {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = RawEnvelope::deserialize(deserializer)?;
        let envelope = Self {
            envelope_version: raw.envelope_version,
            key: raw.key,
            kind: raw.kind,
            content: raw.content,
            title: raw.title,
            tags: raw.tags,
            links: raw.links,
            source_paths: raw.source_paths,
            archive: raw.archive,
            freshness: raw.freshness,
            content_hash: raw.content_hash,
            profile: raw.profile,
            extensions: raw.extensions,
        };
        envelope.validate().map_err(serde::de::Error::custom)?;
        Ok(envelope)
    }
}

fn require_non_empty(field: &str, value: &str) -> Result<(), ContractError> {
    if value.trim().is_empty() {
        Err(ContractError::invalid(field, "value must not be empty"))
    } else {
        Ok(())
    }
}

fn validate_extensions(
    field: &str,
    extensions: &BTreeMap<String, Value>,
    reserved: &[&str],
) -> Result<(), ContractError> {
    if let Some(name) = extensions
        .keys()
        .find(|name| reserved.contains(&name.as_str()))
    {
        Err(ContractError::invalid(
            format!("{field}.{name}"),
            "extension collides with a reserved field",
        ))
    } else {
        Ok(())
    }
}

fn validate_unique_non_empty(field: &str, values: &[String]) -> Result<(), ContractError> {
    let mut seen = HashSet::new();
    for (index, value) in values.iter().enumerate() {
        require_non_empty(&format!("{field}[{index}]"), value)?;
        if !seen.insert(value) {
            return Err(ContractError::invalid(
                format!("{field}[{index}]"),
                "duplicate value",
            ));
        }
    }
    Ok(())
}

fn validate_paths(field: &str, paths: &[String]) -> Result<(), ContractError> {
    validate_unique_non_empty(field, paths)?;
    for (index, path) in paths.iter().enumerate() {
        let bytes = path.as_bytes();
        let drive_absolute = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
        let without_directory_suffix = path.strip_suffix('/').unwrap_or(path);
        let invalid = path.starts_with('/')
            || drive_absolute
            || path.contains('\\')
            || path.bytes().any(|byte| byte.is_ascii_control())
            || path.contains("//")
            || without_directory_suffix.is_empty()
            || without_directory_suffix
                .split('/')
                .any(|part| part == ".." || part == "." || part.is_empty());
        if invalid {
            return Err(ContractError::invalid(
                format!("{field}[{index}]"),
                "path must be normalized and repository-relative",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::collections::BTreeMap;

    use serde_json::{Value, json};

    use super::{ClientProfile, ContentHash, Envelope, FormatVersion};
    use crate::{CURRENT_ENVELOPE_VERSION, ContractErrorKind};

    #[test]
    fn compatible_unknown_fields_and_profile_metadata_round_trip() {
        let content = "Remember the seam.";
        let input = json!({
            "envelope_version": {"major": 1, "minor": 7},
            "key": "architecture/seam",
            "kind": "note",
            "content": content,
            "links": [{"key": "architecture/root", "future_link_field": "kept"}],
            "source_paths": {
                "scope": ["crates/core/"],
                "observed": ["README.md"],
                "future_path_field": ["kept"]
            },
            "archive": {"archived": false, "future_archive_field": 1},
            "freshness": {"state": "fresh", "code_revision": "abc123", "future_freshness_field": true},
            "content_hash": ContentHash::for_content(content),
            "profile": {
                "name": "independent-client",
                "version": {"major": 42, "minor": 3},
                "metadata": {"future_entity_shape": {"answer": 42}},
                "future_profile_field": "kept"
            },
            "future_memory_field": {"kept": true}
        });

        let envelope: Envelope = serde_json::from_value(input.clone()).unwrap();
        let output = serde_json::to_value(envelope).unwrap();

        assert_eq!(output["future_memory_field"], input["future_memory_field"]);
        assert_eq!(
            output["profile"]["metadata"]["future_entity_shape"],
            input["profile"]["metadata"]["future_entity_shape"]
        );
        assert_eq!(output["envelope_version"]["minor"], 7);
        assert_eq!(output["profile"]["version"]["major"], 42);
        assert_eq!(output["archive"]["future_archive_field"], 1);
        assert_eq!(output["freshness"]["future_freshness_field"], true);
        assert_eq!(output["links"][0]["future_link_field"], "kept");
        assert_eq!(output["source_paths"]["future_path_field"][0], "kept");
        assert_eq!(output["profile"]["future_profile_field"], "kept");
    }

    #[test]
    fn profile_metadata_cannot_replace_reserved_fields() {
        let mut envelope = Envelope::new("one", "note", "body").unwrap();
        envelope.profile = Some(ClientProfile {
            name: "client".into(),
            version: FormatVersion::new(9, 0),
            metadata: [("key".to_owned(), Value::String("client-key".into()))]
                .into_iter()
                .collect(),
            extensions: BTreeMap::new(),
        });

        let wire = serde_json::to_value(&envelope).unwrap();
        assert_eq!(wire["key"], "one");
        assert_eq!(wire["profile"]["metadata"]["key"], "client-key");
        envelope.validate().unwrap();
    }

    #[test]
    fn incompatible_envelope_major_is_rejected_during_decode() {
        let mut value =
            serde_json::to_value(Envelope::new("one", "note", "body").unwrap()).unwrap();
        value["envelope_version"]["major"] = json!(CURRENT_ENVELOPE_VERSION.major + 1);

        let error = serde_json::from_value::<Envelope>(value).unwrap_err();
        assert!(error.to_string().contains("unsupported major version"));
    }

    #[test]
    fn a_stale_hash_is_rejected_before_a_write() {
        let mut envelope = Envelope::new("one", "note", "body").unwrap();
        envelope.content = "edited".into();
        let error = envelope.validate().unwrap_err();
        assert_eq!(error.kind, ContractErrorKind::InvalidField);
        assert_eq!(error.field, "content_hash");
        envelope.refresh_content_hash();
        envelope.validate().unwrap();
    }

    #[test]
    fn paths_are_portable_repository_relative_values() {
        let mut envelope = Envelope::new("one", "note", "body").unwrap();
        for invalid in ["C:/absolute", "a//b", "./relative"] {
            envelope.source_paths.observed = vec![invalid.into()];
            assert!(envelope.validate().is_err(), "accepted {invalid}");
        }
        envelope.source_paths.observed = vec!["directory/".into()];
        envelope.validate().unwrap();
    }
}
