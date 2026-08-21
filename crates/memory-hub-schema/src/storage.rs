//! Where a type keeps its documents.
//!
//! A type either keeps its bodies in its records, or names a directory of the
//! working tree and keeps them there as files. The directory is written in the
//! definition itself — the path, not a label standing for one — so reading a
//! type answers the question outright instead of sending the reader to a second
//! place that has to agree with this one.
//!
//! The path is checked here for shape only. Whether the directory exists, and
//! what is in it, are questions about the working tree, which the schema has
//! never seen: they are answered where the folder is read.

use crate::{TYPE_KIND, ValidationError, ValidationErrorKind};

/// Where a type's documents are.
///
/// Absent means "with the records" — a type that says nothing about storage is
/// a type whose bodies sit in its records, which is what every type was before
/// storage became a choice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypeStorage {
    /// Bodies live with the envelopes, wherever the host keeps records.
    WithRecords,
    /// Bodies are files in this directory of the working tree, relative to the
    /// project root.
    Folder(String),
}

impl TypeStorage {
    /// The directory, when the type named one.
    #[must_use]
    pub fn folder(&self) -> Option<&str> {
        match self {
            Self::WithRecords => None,
            Self::Folder(folder) => Some(folder),
        }
    }

    /// Whether the content of this type lives outside its records.
    #[must_use]
    pub const fn is_external(&self) -> bool {
        matches!(self, Self::Folder(_))
    }
}

/// Resolve what a type declared.
///
/// `kind_name` is needed because the type registry is the one type that cannot
/// choose: reading a storage requires reading the registry, and reading the
/// registry requires already knowing where it is.
pub(crate) fn resolve(
    kind_name: &str,
    declared: Option<&str>,
) -> Result<TypeStorage, ValidationError> {
    let Some(folder) = declared else {
        return Ok(TypeStorage::WithRecords);
    };

    if kind_name == TYPE_KIND {
        return Err(ValidationError::with_data(
            ValidationErrorKind::InvalidTypeDefinition,
            "storage",
            "`__type__` cannot name a folder: the registry is the load point \
             and always lives where records live",
            serde_json::json!({"kind_name": kind_name}),
        ));
    }

    check_folder(folder)?;
    Ok(TypeStorage::Folder(folder.to_owned()))
}

/// How a path that fails [`check_folder`] is described.
pub const STORAGE_FOLDER_RULE: &str = "a type's storage is a normalized, project-relative \
                                       directory, and not `.git`";

/// A directory of the working tree, said the way a locator says one.
///
/// The same rule a `content_ref` path is held to, because a document of this
/// type is written under this directory and the two would otherwise disagree
/// about the same string. Two rules on top of it: no trailing slash, so one
/// directory has one spelling, and not Git's own directory, which is inside the
/// project without being part of it.
fn check_folder(folder: &str) -> Result<(), ValidationError> {
    let well_formed = !folder.ends_with('/')
        && memory_hub_core::validate_locator("storage", folder).is_ok()
        && folder.split('/').next() != Some(".git");
    if well_formed {
        return Ok(());
    }
    Err(ValidationError::with_data(
        ValidationErrorKind::InvalidTypeDefinition,
        "storage",
        STORAGE_FOLDER_RULE,
        serde_json::json!({"storage": folder}),
    ))
}
