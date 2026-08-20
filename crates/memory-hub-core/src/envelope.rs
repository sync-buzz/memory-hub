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
    "content_ref",
    "folder",
    "is_folder",
    "profile",
];

/// Hash of the exact UTF-8 content stored in an envelope.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ContentHash(String);

impl ContentHash {
    #[must_use]
    pub fn for_content(content: &str) -> Self {
        Self::for_bytes(content.as_bytes())
    }

    /// Digest of content that is not necessarily text.
    ///
    /// A reference record's digest describes a file somebody else owns, and
    /// nothing says that file decodes as UTF-8. Hashing the bytes is what lets
    /// a scan notice that a diagram or a PDF was edited, moved or restored on
    /// the same terms as a Markdown file.
    #[must_use]
    pub fn for_bytes(content: &[u8]) -> Self {
        let digest = Sha256::digest(content);
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

/// Where the content of a reference record lives.
///
/// A locator, not a copy. The bytes belong to whoever owns that location —
/// a documentation folder a team edits in their IDE, say — and Memory writes
/// nothing into them and keeps no second copy that could go stale.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContentRef {
    /// Location of the content, relative to the repository root.
    pub path: String,
    /// Whether the last scan found anything at `path`, and if not, why.
    ///
    /// Never a reason to remove the record: memory does not branch and code
    /// does, so the corpus holds the union of every branch's documents and
    /// the checked-out branch decides which of them are real right now.
    /// Deleting on absence would destroy a feature branch's documentation
    /// every time somebody switched to `main`.
    #[serde(default, skip_serializing_if = "Presence::is_present")]
    pub presence: Presence,
    /// Compatible fields introduced by future envelope minor versions.
    #[serde(flatten)]
    pub extensions: BTreeMap<String, Value>,
}

/// Whether a reference record's content is here, and if not, why not.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Presence {
    /// The content was there the last time anybody looked.
    #[default]
    Present,
    /// The checked-out commit does not have this document at all.
    ///
    /// Routine: another branch has it, or this clone has not pulled it. The
    /// record is hidden rather than shown as broken, because showing it would
    /// fill the interface with noise in proportion to how many branches are in
    /// flight.
    NotOnBranch,
    /// The checked-out commit has this document, but the working tree does
    /// not.
    ///
    /// Somebody deleted the file and has not committed it. That is a
    /// deliberate act on the branch that owns the document, and the only case
    /// where a person is asked whether the record should go too.
    Removed,
}

impl Presence {
    #[must_use]
    pub const fn is_present(&self) -> bool {
        matches!(self, Self::Present)
    }

    /// Whether the content is not here, whatever the reason.
    #[must_use]
    pub const fn is_absent(&self) -> bool {
        !self.is_present()
    }

    /// The stable name a client branches on.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Present => "present",
            Self::NotOnBranch => "not_on_branch",
            Self::Removed => "removed",
        }
    }
}

impl ContentRef {
    #[must_use]
    pub fn new(path: impl Into<String>) -> Self {
        Self {
            path: path.into(),
            presence: Presence::Present,
            extensions: BTreeMap::new(),
        }
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
    /// For an inline record, the digest of `content`. For a reference record,
    /// the digest of the content that was last resolved from `content_ref` —
    /// a statement about the outside world, and the unit a write agrees on.
    pub content_hash: ContentHash,
    /// Present when the content lives outside this record. `content` is then
    /// empty: the bytes are somebody else's, and a cached copy would be a
    /// second version of the truth that goes stale without saying so.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_ref: Option<ContentRef>,
    /// What the content is, as an IANA media type.
    ///
    /// Answered before the content is fetched, because that is when it is
    /// asked: a client drawing a list decides on an editor, a viewer or a
    /// player from this, and asking it to read a video to find out it is a
    /// video is the wrong order.
    ///
    /// Absent means nobody said. A reader that needs a default should assume
    /// `text/plain` for an inline record — its content is a `String` — and
    /// nothing at all for a reference record, whose bytes belong to somebody
    /// else and may be anything.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media_type: Option<String>,
    /// Where this record sits in the hierarchy, as a path of segments.
    ///
    /// A name, never a location. In `refs` the tree stays flat and hashed, so
    /// none of the problems of a physical hierarchy — case-insensitive
    /// filesystems, path length, unicode normalization, reserved names — come
    /// from this field. Hierarchy is physical only where it already was
    /// without us.
    ///
    /// Absent means the root. Folders are implicit: one exists while a record
    /// is in it, so there are no empty folders and no orphans on delete.
    ///
    /// For a reference record this is the directory of `content_ref.path` and
    /// may not disagree with it — one fact, one place. Moving such a record
    /// means moving its file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub folder: Option<String>,
    /// Whether this record is the folder it is filed in.
    ///
    /// A folder's own title and text, held by an ordinary record rather than
    /// by a kind of its own. The folder it stands for is `folder` — the one it
    /// is in — and never a path of its own: a path named by a second field
    /// follows nothing, while `folder` already moves correctly because every
    /// other record moves by it.
    ///
    /// One folder, one such record. The rule is enforced where records are
    /// written, which is everywhere: a record whose bytes are a file in an
    /// attached folder still lives in `refs`, so no file can raise this by
    /// appearing.
    ///
    /// Nothing else changes. It is a document — listed, searched and counted
    /// as one — and the folder it is in exists for the ordinary reason, that a
    /// record is in it.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub is_folder: bool,
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
    content_ref: Option<ContentRef>,
    #[serde(default)]
    media_type: Option<String>,
    #[serde(default)]
    folder: Option<String>,
    #[serde(default)]
    is_folder: bool,
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
            media_type: None,
            tags: Vec::new(),
            links: Vec::new(),
            source_paths: SourcePaths::default(),
            archive: ArchiveState::default(),
            freshness: Freshness::default(),
            content_ref: None,
            folder: None,
            is_folder: false,
            profile: None,
            extensions: BTreeMap::new(),
        };
        envelope.validate()?;
        Ok(envelope)
    }

    /// Construct a record whose content lives outside it.
    ///
    /// `content_hash` is what was last resolved from `path`. Passing the
    /// digest of an empty string is the honest value for a locator nothing has
    /// read yet.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError`] when `key` or `kind` is empty, or when `path`
    /// is not a normalized repository-relative path.
    pub fn reference(
        key: impl Into<String>,
        kind: impl Into<String>,
        path: impl Into<String>,
        content_hash: ContentHash,
    ) -> Result<Self, ContractError> {
        let reference: String = path.into();
        let envelope = Self {
            envelope_version: CURRENT_ENVELOPE_VERSION,
            key: key.into(),
            kind: kind.into(),
            content: String::new(),
            content_hash,
            folder: folder_of(reference.as_str()),
            is_folder: false,
            content_ref: Some(ContentRef::new(reference)),
            title: None,
            media_type: None,
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

    /// Check the folder, and that it agrees with the locator when there is one.
    ///
    /// The invariant for a reference record is local and exact: the folder is
    /// the directory the file is in. Storing a rebased value — the directory
    /// relative to whatever root the folder was attached at — would be a
    /// second version of one fact, unverifiable from here, and it would shift
    /// under every record the day somebody attaches a nested root without
    /// moving a single file.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError`] for a folder that is not a normalized
    /// repository-relative directory, or one that disagrees with
    /// `content_ref.path`.
    fn validate_folder(&self) -> Result<(), ContractError> {
        if let Some(folder) = &self.folder {
            validate_paths("folder", std::slice::from_ref(folder))?;
        }
        let Some(reference) = &self.content_ref else {
            return Ok(());
        };
        let derived = folder_of(&reference.path);
        if self.folder != derived {
            return Err(ContractError::invalid(
                "folder",
                match derived {
                    Some(derived) => format!(
                        "a reference record's folder is the directory of its content: \
                         expected `{derived}`"
                    ),
                    None => "content at the repository root has no folder".to_owned(),
                },
            ));
        }
        Ok(())
    }

    /// Whether this record's content lives outside it.
    #[must_use]
    pub const fn is_reference(&self) -> bool {
        self.content_ref.is_some()
    }

    /// Recompute the digest after changing `content`.
    ///
    /// Only meaningful for an inline record; a reference record's digest is
    /// set from what was resolved, not from what is held here.
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
        match &self.content_ref {
            // The digest describes what this record holds, so it can be
            // checked against it.
            None => {
                if self.content_hash != ContentHash::for_content(&self.content) {
                    return Err(ContractError::invalid(
                        "content_hash",
                        "content_hash does not match content",
                    ));
                }
            }
            // The digest describes what was last read from somewhere else.
            // Nothing here can confirm it, and `content` is required to stay
            // empty so no stale copy can pretend to be the truth.
            Some(reference) => {
                if !self.content.is_empty() {
                    return Err(ContractError::invalid(
                        "content",
                        "a reference record keeps no copy of its content",
                    ));
                }
                validate_paths("content_ref.path", std::slice::from_ref(&reference.path))?;
                validate_extensions(
                    "content_ref.extensions",
                    &reference.extensions,
                    &["path", "presence"],
                )?;
            }
        }
        self.validate_folder()?;
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
            content_ref: raw.content_ref,
            media_type: raw.media_type,
            folder: raw.folder,
            is_folder: raw.is_folder,
            profile: raw.profile,
            extensions: raw.extensions,
        };
        envelope.validate().map_err(serde::de::Error::custom)?;
        Ok(envelope)
    }
}

/// Check a value that is about to be used as a locator.
///
/// The envelope validator refuses a bad locator, but it only runs once the
/// record exists — and writing a reference record's content happens *before*
/// the record, on purpose. Anything that builds a locator has to be able to
/// refuse it before it touches the filesystem, which is what this is for.
///
/// # Errors
///
/// Returns [`ContractError`] when `path` is not a normalized,
/// repository-relative path.
pub fn validate_locator(field: &str, path: &str) -> Result<(), ContractError> {
    validate_paths(field, std::slice::from_ref(&path.to_owned()))
}

/// The directory part of a repository-relative path, if it has one.
#[must_use]
pub fn folder_of(path: &str) -> Option<String> {
    path.rsplit_once('/')
        .map(|(directory, _)| directory.to_owned())
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

    use super::{ClientProfile, ContentHash, ContentRef, Envelope, FormatVersion};
    use crate::{CURRENT_ENVELOPE_VERSION, ContractErrorKind};

    fn reference() -> Envelope {
        Envelope::reference(
            "guide",
            "doc",
            "docs/guide.md",
            ContentHash::for_content("what the file said last time"),
        )
        .unwrap()
    }

    /// The digest of an inline record describes what the record holds and can
    /// be checked against it. The digest of a reference record describes what
    /// was last read from somewhere else, and nothing here can confirm it.
    #[test]
    fn a_reference_record_digest_is_not_checked_against_its_own_body() {
        let envelope = reference();
        assert!(envelope.is_reference());
        assert!(envelope.content.is_empty());
        assert_ne!(
            envelope.content_hash,
            ContentHash::for_content(&envelope.content),
            "the digest is about the outside, and validation accepted that"
        );
        envelope.validate().unwrap();
    }

    /// A cached body would be a second version of the truth that goes stale
    /// without saying so.
    #[test]
    fn a_reference_record_may_not_keep_a_copy_of_its_content() {
        let mut envelope = reference();
        envelope.content = "a stale copy".to_owned();
        let error = envelope.validate().unwrap_err();
        assert_eq!(error.kind, ContractErrorKind::InvalidField);
        assert_eq!(error.field, "content");
    }

    #[test]
    fn a_locator_must_be_a_normalized_repository_relative_path() {
        for path in ["/etc/passwd", "../outside.md", "docs/../secrets.md", ""] {
            let envelope =
                Envelope::reference("guide", "doc", path, ContentHash::for_content("anything"));
            assert!(envelope.is_err(), "accepted {path}");
        }
    }

    #[test]
    fn a_reference_record_round_trips() {
        let envelope = reference();
        let wire = serde_json::to_value(&envelope).unwrap();
        assert_eq!(wire["content_ref"]["path"], "docs/guide.md");
        assert_eq!(serde_json::from_value::<Envelope>(wire).unwrap(), envelope);
    }

    /// The field is additive: an envelope written before it existed still
    /// parses, and still means what it meant.
    #[test]
    fn an_envelope_without_a_locator_is_unchanged() {
        let inline = Envelope::new("note", "note", "body").unwrap();
        let wire = serde_json::to_value(&inline).unwrap();
        assert!(
            wire.get("content_ref").is_none(),
            "nothing is written for a record that has no locator"
        );
        assert!(!inline.is_reference());
        assert_eq!(serde_json::from_value::<Envelope>(wire).unwrap(), inline);
    }

    #[test]
    fn a_locator_extension_may_not_collide_with_the_path() {
        let mut envelope = reference();
        let mut reference_field = ContentRef::new("docs/guide.md");
        reference_field
            .extensions
            .insert("path".to_owned(), json!("elsewhere.md"));
        envelope.content_ref = Some(reference_field);
        assert_eq!(
            envelope.validate().unwrap_err().field,
            "content_ref.extensions.path"
        );
    }

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
