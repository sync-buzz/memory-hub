//! Where a project keeps its records.
//!
//! Told by the host when it opens the project, and nowhere else. memory-hub is
//! an engine something else embeds, and a product that embeds it is the one
//! that knows whether its users want their memory in Git or in a folder they
//! can open. Written to a file instead, that answer would be a second copy of a
//! decision the host makes every time it opens the project — free to disagree
//! with it, and read by a project the host never asked about.

/// Where a project's records live.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RecordsIn {
    /// Git's own metadata: objects under private refs. Invisible in the working
    /// tree, versioned and pushable, and only Memory writes it.
    GitMetadata,
    /// A directory of record files, relative to the project root. Visible,
    /// needing no Git, keeping no past.
    Directory(String),
}

/// Where a folder of records goes when the host names no other place.
pub const DEFAULT_RECORDS_PATH: &str = ".memory";

impl RecordsIn {
    /// A directory of record files at the usual place.
    #[must_use]
    pub fn directory() -> Self {
        Self::Directory(DEFAULT_RECORDS_PATH.to_owned())
    }
}
