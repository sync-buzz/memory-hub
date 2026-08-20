//! Which storage a type's records live in.
//!
//! A type names a storage the project declared — `docs`, `media`, `main` — and
//! that is the whole of it. What that name *is* — a folder, refs, a database —
//! is the project's business, written once where the project declares its
//! storages, not repeated in every type that uses one.
//!
//! The name is checked here for shape only. Whether a storage by that name
//! exists is a question about the project, and the schema has never seen the
//! project: it is answered where the declaration is read.

use crate::{TYPE_KIND, ValidationError, ValidationErrorKind};

/// The storage a type names, or the one it gets by not naming any.
///
/// Absent means "wherever records live" — the storage the project declared as
/// holding records. A type that says nothing about storage is a type whose
/// bodies sit in its records, which is what every type was before storage
/// became a choice.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TypeStorage {
    /// Bodies live with the envelopes, in whichever storage holds records.
    WithRecords,
    /// Bodies live in the named storage.
    Named(String),
}

impl TypeStorage {
    /// The storage name, when the type named one.
    #[must_use]
    pub fn name(&self) -> Option<&str> {
        match self {
            Self::WithRecords => None,
            Self::Named(name) => Some(name),
        }
    }

    /// Whether the content of this type lives outside its records.
    #[must_use]
    pub const fn is_external(&self) -> bool {
        matches!(self, Self::Named(_))
    }
}

/// Resolve what a type declared.
///
/// `kind_name` is needed because the type registry is the one type that cannot
/// choose: reading a storage name requires reading the registry, and reading
/// the registry requires already knowing where it is.
pub(crate) fn resolve(
    kind_name: &str,
    declared: Option<&str>,
) -> Result<TypeStorage, ValidationError> {
    let Some(name) = declared else {
        return Ok(TypeStorage::WithRecords);
    };

    if kind_name == TYPE_KIND {
        return Err(ValidationError::with_data(
            ValidationErrorKind::InvalidTypeDefinition,
            "storage",
            "`__type__` cannot name a storage: the registry is the load point \
             and always lives where records live",
            serde_json::json!({"kind_name": kind_name}),
        ));
    }

    check_name(name)?;
    Ok(TypeStorage::Named(name.to_owned()))
}

/// Whether a string is shaped like a storage name.
///
/// Public because the rule has two readers: a type naming a storage, and the
/// project declaring one. Stated twice, the two would drift, and a name
/// accepted where it is declared and refused where it is used is a project
/// nobody can fix.
///
/// Shape only. A name that is well-formed and points at nothing is a different
/// failure, reported by whoever holds the project's declaration — and reported
/// with the list of names that do exist, which this module cannot know.
#[must_use]
pub fn is_storage_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-' || byte == b'_'
        })
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase())
}

/// How a name that fails [`is_storage_name`] is described, so both readers of
/// the rule say the same thing about it.
pub const STORAGE_NAME_RULE: &str = "a storage name starts with a letter and holds only \
                                     lowercase letters, digits, `-` and `_`";

fn check_name(name: &str) -> Result<(), ValidationError> {
    if is_storage_name(name) {
        return Ok(());
    }
    Err(ValidationError::with_data(
        ValidationErrorKind::InvalidTypeDefinition,
        "storage",
        STORAGE_NAME_RULE,
        serde_json::json!({"storage": name}),
    ))
}
