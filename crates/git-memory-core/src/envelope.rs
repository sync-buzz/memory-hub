use std::collections::{BTreeMap, HashSet};

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
}

/// Code paths required by standalone rebuild and reconciliation.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourcePaths {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub observed: Vec<String>,
}

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ArchiveState {
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub archived_at: Option<String>,
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
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct Freshness {
    #[serde(default)]
    pub state: FreshnessState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Client-owned interpretation of `metadata`.
///
/// Profile versioning is deliberately independent of envelope versioning.
/// Memory retains metadata values but never gives them authority to replace
/// reserved envelope fields.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct ClientProfile {
    pub name: String,
    pub version: FormatVersion,
    #[serde(default)]
    pub metadata: BTreeMap<String, Value>,
}

/// Product-neutral canonical record.
#[derive(Clone, Debug, PartialEq, Serialize)]
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
        }
        if self.archive.archived_at.is_some() && !self.archive.archived {
            return Err(ContractError::invalid(
                "archive.archived_at",
                "archived_at requires archived=true",
            ));
        }
        if let Some(profile) = &self.profile {
            require_non_empty("profile.name", &profile.name)?;
        }
        if let Some(field) = self
            .extensions
            .keys()
            .find(|field| RESERVED_FIELDS.contains(&field.as_str()))
        {
            return Err(ContractError::invalid(
                format!("extensions.{field}"),
                "extension collides with a reserved envelope field",
            ));
        }
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
        let invalid = path.starts_with('/')
            || path.contains('\\')
            || path.split('/').any(|part| part == ".." || part == ".");
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
            "source_paths": {"scope": ["crates/core/"], "observed": ["README.md"]},
            "archive": {"archived": false},
            "freshness": {"state": "fresh", "code_revision": "abc123"},
            "content_hash": ContentHash::for_content(content),
            "profile": {
                "name": "independent-client",
                "version": {"major": 42, "minor": 3},
                "metadata": {"future_entity_shape": {"answer": 42}}
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
}
