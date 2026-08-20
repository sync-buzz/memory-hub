use serde::{Deserialize, Serialize};

use crate::ContractError;

/// A two-part durable format version.
///
/// Readers accept newer minor versions and retain their unknown fields. A
/// different major version is incompatible.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub struct FormatVersion {
    pub major: u16,
    pub minor: u16,
}

impl FormatVersion {
    #[must_use]
    pub const fn new(major: u16, minor: u16) -> Self {
        Self { major, minor }
    }

    pub(crate) fn require_major(self, field: &str, supported: u16) -> Result<(), ContractError> {
        if self.major == supported {
            Ok(())
        } else {
            Err(ContractError::incompatible_version(
                field, self.major, supported,
            ))
        }
    }
}

/// Envelope version emitted by this build.
///
/// Minor 1 added `content_ref` — content that lives outside the record — and
/// `folder`. Minor 2 added `is_folder`, which marks the record that is the
/// folder it is filed in.
///
/// `folder` and `is_folder` are additive in the ordinary sense: a reader of an
/// earlier minor keeps them as unknown fields and loses nothing it used to
/// have. What a minor-1 reader loses with `is_folder` is knowledge that a
/// folder has a description — it sees an ordinary document, which is what it
/// is. `content_ref` is not additive, and saying so plainly is worth more than
/// the tidy claim it replaces. A record that has one keeps `content` empty
/// while `content_hash` describes a file outside it, and a minor-0 reader
/// applies `content_hash == sha256(content)` unconditionally — so it does not
/// skip the record, it fails to decode it. The break arrives the day a project
/// declares its first reference type, never before.
pub const CURRENT_ENVELOPE_VERSION: FormatVersion = FormatVersion::new(1, 2);
