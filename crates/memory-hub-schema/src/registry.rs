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

/// A relationship target that names a kind the registry holds no definition for.
///
/// Produced by [`SchemaRegistry::from_type_definitions_lenient`] when a
/// `relationships.*.target` points at a kind that is not in the set. The strict
/// [`SchemaRegistry::from_type_definitions`] refuses such a set outright; the
/// lenient constructor records these instead, so a registry can be read from a
/// corpus that already carries a dangling target — and the type that carries it
/// can be removed to heal the corpus.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DanglingTarget {
    /// The kind whose relationship points at a missing target.
    pub kind: String,
    /// The relation name on that kind.
    pub relation: String,
    /// The target kind that is not defined.
    pub target: String,
}

/// A collection of [`TypeDefinition`]s, looked up by kind name.
#[derive(Clone, Debug, Default)]
pub struct SchemaRegistry {
    types: BTreeMap<String, TypeDefinition>,
    dangling: Vec<DanglingTarget>,
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

    /// Build a registry without refusing a dangling relationship target.
    ///
    /// Like [`from_type_definitions`](Self::from_type_definitions) for every
    /// structural check — self-validation, duplicate kinds — but where the strict
    /// constructor returns the first [`ValidationError`] for a `target` naming a
    /// kind that is not in the set, this one records it in
    /// [`dangling_targets`](Self::dangling_targets) and builds the registry
    /// anyway. The definitions are kept intact: a dangling target is a fact
    /// about the corpus, not a reason to hide the type that carries it.
    ///
    /// This is the recovery path. A corpus can arrive at a dangling target by a
    /// deletion that predates the guard in the transaction policy that now
    /// prevents it, and a registry that cannot be built from a corpus that
    /// exists is one the system cannot read its way out of — every reader, the
    /// policy itself, and the command that removes a type all need a registry to
    /// answer. The strict constructor stays the contract for a clean corpus;
    /// this one is taken when that one has already failed.
    ///
    /// # Errors
    ///
    /// Returns the first [`ValidationError`] from self-validation or a
    /// duplicate kind. A dangling target is not an error here.
    pub fn from_type_definitions_lenient(
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
        registry.dangling = registry.collect_dangling_targets();
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

    /// The dangling relationship targets this registry was built with, if any.
    ///
    /// Empty for a registry built through the strict
    /// [`from_type_definitions`](Self::from_type_definitions) — that one
    /// refuses a dangling target rather than recording it. Populated only by
    /// [`from_type_definitions_lenient`](Self::from_type_definitions_lenient),
    /// the recovery path. A reader uses this to tell a person which type to
    /// remove.
    #[must_use]
    pub fn dangling_targets(&self) -> &[DanglingTarget] {
        &self.dangling
    }

    /// Whether this registry was built from a corpus carrying a dangling target.
    ///
    /// True only for a registry built through
    /// [`from_type_definitions_lenient`](Self::from_type_definitions_lenient)
    /// whose corpus had a `relationships.*.target` naming a kind not in the
    /// set. The transaction policy gates recovery on this: a clean corpus keeps
    /// the strict refusal of a new dangling target, a broken one relaxes so it
    /// can be read and healed.
    #[must_use]
    pub fn is_broken(&self) -> bool {
        !self.dangling.is_empty()
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
        self.validate_record_shallow(envelope, strict, true)
    }

    /// Validate a record, optionally without the fields its type declares.
    ///
    /// `fields` is false for a record that is not a document of its kind — the
    /// one carrying `is_folder`, which is the folder its type's documents are
    /// filed in rather than one of them. The kind still has to exist, and the
    /// envelope is still checked; what is skipped is the product fields, which
    /// describe documents and have nothing to say about a folder.
    ///
    /// # Errors
    ///
    /// Returns [`ValidationError`] on the same terms as
    /// [`validate_record`](Self::validate_record).
    pub fn validate_record_shallow(
        &self,
        envelope: &Envelope,
        strict: bool,
        fields: bool,
    ) -> Result<(), ValidationError> {
        match self.get(&envelope.kind) {
            Some(definition) if fields => definition.validate(envelope),
            Some(definition) => definition.validate_envelope(envelope),
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

    /// Every `relationships.*.target` naming a kind not in the set.
    ///
    /// The collection behind
    /// [`from_type_definitions_lenient`](Self::from_type_definitions_lenient).
    /// [`validate_cross_type_targets`](Self::validate_cross_type_targets) is
    /// the strict form — it returns the first of these as an error — and this
    /// is the lenient one, so the two answer the same question on the same terms
    /// and diverge only in whether they stop.
    fn collect_dangling_targets(&self) -> Vec<DanglingTarget> {
        let mut dangling = Vec::new();
        for (kind_name, definition) in &self.types {
            for (relation, rel_def) in &definition.relationships {
                if rel_def.target == "any" {
                    continue;
                }
                if !self.types.contains_key(&rel_def.target) {
                    dangling.push(DanglingTarget {
                        kind: kind_name.clone(),
                        relation: relation.clone(),
                        target: rel_def.target.clone(),
                    });
                }
            }
        }
        dangling
    }
}
