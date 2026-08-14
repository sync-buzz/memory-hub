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
pub const CURRENT_ENVELOPE_VERSION: FormatVersion = FormatVersion::new(1, 0);
