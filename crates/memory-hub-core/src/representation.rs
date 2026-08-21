use serde::{Deserialize, Serialize};

use crate::{ContractError, Envelope};

/// Durable representation discriminator.
///
/// One variant, and the tag stays: every record on disk carries
/// `"representation": "plaintext"`, and a store that stopped writing it would
/// be writing a format it can no longer read. The tag is what a second
/// representation would be added to; until one exists it is one word that
/// makes the format say what it is rather than leave it to be inferred.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(tag = "representation", rename_all = "snake_case")]
pub enum StoredRecord {
    Plaintext { envelope: Box<Envelope> },
}

impl StoredRecord {
    /// Validate the durable representation before writing it.
    ///
    /// # Errors
    ///
    /// Returns the validation error from the plaintext envelope.
    pub fn validate(&self) -> Result<(), ContractError> {
        match self {
            Self::Plaintext { envelope } => envelope.validate(),
        }
    }
}
