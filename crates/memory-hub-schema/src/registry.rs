use std::collections::BTreeMap;

use memory_hub_core::Envelope;

use crate::{TypeDefinition, TypeStorage, ValidationError, ValidationErrorKind};

/// Resolves a record key to its kind, for cross-type link validation.
///
/// Implementations are supplied by the store layer; the schema crate itself
/// has no store dependency.
pub trait KindResolver {
    fn resolve_kind(&self, key: &str) -> Option<String>;
}

/// A collection of [`TypeDefinition`]s, looked up by kind name.
#[derive(Clone, Debug, Default)]
pub struct SchemaRegistry {
    types: BTreeMap<String, TypeDefinition>,
}

impl SchemaRegistry {
    /// Create an empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a registry from parsed type definitions.
    ///
    /// Each definition is validated with [`TypeDefinition::validate_self`];
    /// cross-type relationship targets are checked against the full set.
    ///
    /// # Errors
    ///
    /// Returns the first [`ValidationError`] from self-validation or a dangling
    /// relationship target.
    pub fn from_type_definitions(
        definitions: impl IntoIterator<Item = TypeDefinition>,
    ) -> Result<Self, ValidationError> {
        let mut registry = Self::new();
        for definition in definitions {
            definition.validate_self()?;
            if registry.types.contains_key(&definition.kind_name) {
                return Err(ValidationError::with_data(
                    ValidationErrorKind::InvalidTypeDefinition,
                    "kind_name",
                    format!(
                        "duplicate type definition for kind `{}`",
                        definition.kind_name
                    ),
                    serde_json::json!({"kind_name": definition.kind_name}),
                ));
            }
            registry
                .types
                .insert(definition.kind_name.clone(), definition);
        }
        registry.validate_cross_type_targets()?;
        Ok(registry)
    }

    /// Look up a type definition by kind name.
    #[must_use]
    pub fn get(&self, kind: &str) -> Option<&TypeDefinition> {
        self.types.get(kind)
    }

    /// Whether the registry is empty (no type definitions loaded).
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.types.is_empty()
    }

    /// Number of registered type definitions.
    #[must_use]
    pub fn len(&self) -> usize {
        self.types.len()
    }

    /// Iterate over all registered type definitions.
    pub fn iter(&self) -> impl Iterator<Item = (&String, &TypeDefinition)> {
        self.types.iter()
    }

    /// Where records of `kind` live.
    ///
    /// The registry itself is never consulted for `__type__`. Learning a place
    /// means reading the registry, and reading the registry means already
    /// knowing where it is — so that one answer is fixed in code, not in data.
    ///
    /// A kind with no definition answers the default as well. Strict mode
    /// rejects such a record before it reaches a backend, and non-strict mode
    /// deliberately accepts it — either way the answer is the storage every
    /// type had before storage was a choice.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] if the type declares a storage this build
    /// cannot honour. A registry built through
    /// [`from_type_definitions`](Self::from_type_definitions) has already
    /// rejected those, so this is reachable only for a definition validated
    /// elsewhere.
    pub fn storage_for(&self, kind: &str) -> Result<TypeStorage, ValidationError> {
        if kind == crate::TYPE_KIND {
            return Ok(TypeStorage::WithRecords);
        }
        self.get(kind)
            .map_or_else(|| Ok(TypeStorage::WithRecords), TypeDefinition::storage)
    }

    /// Validate a single envelope against the registry.
    ///
    /// In strict mode (default), an unknown `kind` — one with no matching type
    /// definition — is rejected. When `strict` is `false`, unknown kinds pass
    /// without validation. Known kinds always run full validation.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] for an unknown kind (strict) or any
    /// validation failure reported by [`TypeDefinition::validate`].
    pub fn validate_record(
        &self,
        envelope: &Envelope,
        strict: bool,
    ) -> Result<(), ValidationError> {
        match self.get(&envelope.kind) {
            Some(definition) => definition.validate(envelope),
            None if strict => Err(ValidationError::with_data(
                ValidationErrorKind::UnknownKind,
                "kind",
                format!("kind `{}` has no type definition", envelope.kind),
                serde_json::json!({"kind": envelope.kind}),
            )),
            None => Ok(()),
        }
    }

    /// Validate an envelope including cross-type link target matching.
    ///
    /// In addition to [`validate_record`](Self::validate_record), each link
    /// with a declared relation is checked: the target record's kind (resolved
    /// via `resolver`) must match the relationship's `target`, unless the
    /// target is `any`.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] for any failure from [`validate_record`] or
    /// a link whose target kind does not match the declared `target`.
    pub fn validate_record_with_resolver(
        &self,
        envelope: &Envelope,
        strict: bool,
        resolver: &dyn KindResolver,
    ) -> Result<(), ValidationError> {
        let definition = match self.get(&envelope.kind) {
            Some(definition) => definition,
            None if strict => {
                return Err(ValidationError::with_data(
                    ValidationErrorKind::UnknownKind,
                    "kind",
                    format!("kind `{}` has no type definition", envelope.kind),
                    serde_json::json!({"kind": envelope.kind}),
                ));
            }
            None => return Ok(()),
        };

        definition.validate_envelope(envelope)?;
        definition.validate_extensions(envelope)?;
        Self::validate_links_targets(definition, envelope, resolver)?;
        Ok(())
    }

    fn validate_links_targets(
        definition: &TypeDefinition,
        envelope: &Envelope,
        resolver: &dyn KindResolver,
    ) -> Result<(), ValidationError> {
        for (index, link) in envelope.links.iter().enumerate() {
            let Some(relation) = &link.relation else {
                continue;
            };
            let Some(rel_def) = definition.relationships.get(relation) else {
                return Err(ValidationError::with_data(
                    ValidationErrorKind::InvalidLinks,
                    format!("links[{index}].relation"),
                    format!(
                        "relation `{relation}` is not declared in type `{}`",
                        definition.kind_name
                    ),
                    serde_json::json!({"relation": relation, "kind": definition.kind_name}),
                ));
            };
            if rel_def.target == "any" {
                continue;
            }
            match resolver.resolve_kind(&link.key) {
                Some(target_kind) if target_kind == rel_def.target => {}
                Some(target_kind) => {
                    return Err(ValidationError::with_data(
                        ValidationErrorKind::InvalidLinks,
                        format!("links[{index}].key"),
                        format!(
                            "link target kind `{target_kind}` does not match declared target `{}`",
                            rel_def.target
                        ),
                        serde_json::json!({
                            "link_key": link.key,
                            "relation": relation,
                            "expected_target": rel_def.target,
                            "actual_target": target_kind,
                        }),
                    ));
                }
                None => {
                    return Err(ValidationError::with_data(
                        ValidationErrorKind::InvalidLinks,
                        format!("links[{index}].key"),
                        "link target record could not be resolved",
                        serde_json::json!({"link_key": link.key, "relation": relation}),
                    ));
                }
            }
        }
        Ok(())
    }

    fn validate_cross_type_targets(&self) -> Result<(), ValidationError> {
        for (kind_name, definition) in &self.types {
            for (relation, rel_def) in &definition.relationships {
                if rel_def.target == "any" {
                    continue;
                }
                if !self.types.contains_key(&rel_def.target) {
                    return Err(ValidationError::with_data(
                        ValidationErrorKind::InvalidTypeDefinition,
                        format!("{kind_name}.relationships.{relation}.target"),
                        format!("target kind `{}` is not defined", rel_def.target),
                        serde_json::json!({"target": rel_def.target}),
                    ));
                }
            }
        }
        Ok(())
    }
}
