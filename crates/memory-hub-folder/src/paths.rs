//! Turning a record id into a file name, and back.
//!
//! A key like `decisions/auth` becomes `decisions/auth.json`, so the folder
//! mirrors the shape a person already has in their head. That means a key is
//! also a path, which is why every component is checked: a key is data, and
//! data that becomes a path without being checked is how a write ends up
//! outside the folder it was meant for.

use std::path::{Component, Path, PathBuf};

use memory_hub_engine::{RecordId, StoreError, StoreErrorKind};

/// Extension every record file carries.
pub(crate) const RECORD_EXTENSION: &str = "json";

/// The path of `id` relative to the records folder.
pub(crate) fn record_path(id: &RecordId) -> Result<PathBuf, StoreError> {
    match id {
        RecordId::Plaintext(key) => {
            check_key(key)?;
            Ok(PathBuf::from(format!("{key}.{RECORD_EXTENSION}")))
        }
    }
}

/// The id a path under the records folder stands for.
///
/// Returns `None` for anything that is not a record file, because a folder may
/// hold whatever a person put there and finding a stray file is not an error.
pub(crate) fn record_id(relative: &Path) -> Option<RecordId> {
    if relative.extension()?.to_str()? != RECORD_EXTENSION {
        return None;
    }
    let without_extension = relative.with_extension("");
    let mut parts = Vec::new();
    for component in without_extension.components() {
        match component {
            Component::Normal(part) => parts.push(part.to_str()?.to_owned()),
            _ => return None,
        }
    }
    if parts.is_empty() {
        return None;
    }
    Some(RecordId::plaintext(parts.join("/")))
}

/// A key is a path, so it is checked like one.
fn check_key(key: &str) -> Result<(), StoreError> {
    if key.is_empty() {
        return Err(invalid("key", key, "a key must not be empty"));
    }
    if key.starts_with('/') || key.ends_with('/') {
        return Err(invalid("key", key, "a key must not start or end with `/`"));
    }
    for component in key.split('/') {
        check_component("key component", component)?;
    }
    Ok(())
}

/// Refuse anything that would not stay inside the folder, or would not survive
/// a round trip through a file name.
fn check_component(field: &str, component: &str) -> Result<(), StoreError> {
    let rejected = component.is_empty()
        || component == "."
        || component == ".."
        || component.contains('\\')
        || component.contains('\0')
        || component.chars().any(char::is_control)
        // Windows and macOS disagree about these, and a corpus that opens on
        // one machine and not another is worse than a key refused up front.
        || component.contains(':')
        || component.ends_with(' ')
        || component.ends_with('.');
    if rejected {
        return Err(invalid(
            field,
            component,
            "a component must be a plain file name",
        ));
    }
    Ok(())
}

fn invalid(field: &str, value: &str, message: &str) -> StoreError {
    StoreError::new(
        StoreErrorKind::InvalidArgument,
        message,
        serde_json::json!({"field": field, "value": value}),
    )
}
