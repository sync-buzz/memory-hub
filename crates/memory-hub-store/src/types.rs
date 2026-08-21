use memory_hub_engine::{RecordId, Revision, StoreError, StoreErrorKind};
use sha2::{Digest, Sha256};

/// Git-specific handling of the otherwise opaque revision token.
///
/// The token is a commit id here; nothing outside this crate may rely on that,
/// which is why the conversion lives behind a crate-private trait instead of on
/// the shared type.
pub(crate) trait GitRevision {
    fn from_oid(oid: git2::Oid) -> Revision;

    fn oid(&self) -> Result<git2::Oid, StoreError>;
}

impl GitRevision for Revision {
    fn from_oid(oid: git2::Oid) -> Self {
        Self::new(oid.to_string())
    }

    fn oid(&self) -> Result<git2::Oid, StoreError> {
        git2::Oid::from_str(self.as_str()).map_err(|_| {
            StoreError::new(
                StoreErrorKind::InvalidArgument,
                "revision is not a Git object id",
                serde_json::json!({"field": "revision", "revision": self.as_str()}),
            )
        })
    }
}

/// Where a record's blob sits in the Git tree.
///
/// Names are hashed, so the tree is flat and leaks no keys. Folders are a
/// property of the record, never of its placement here.
pub(crate) trait GitRecordId {
    fn tree_name(&self) -> String;
}

impl GitRecordId for RecordId {
    fn tree_name(&self) -> String {
        match self {
            Self::Plaintext(key) => format!("r-p-{:x}", Sha256::digest(key.as_bytes())),
        }
    }
}
