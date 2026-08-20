use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::ContractError;

/// Generic action selected for a policy event.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    Off,
    Warn,
    Block,
    Repair,
    RequireFullRebuild,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicySource {
    Default,
    Project,
    Client,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EffectivePolicy {
    pub event: String,
    pub mode: PolicyMode,
    pub source: PolicySource,
}

/// One configuration layer. Event names remain generic strings so future
/// Memory versions can add events without expanding a client-owned enum.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct PolicyConfig(BTreeMap<String, PolicyMode>);

impl PolicyConfig {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Set one event in this configuration layer.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError`] when the event name is not canonical.
    pub fn insert(
        &mut self,
        event: impl Into<String>,
        mode: PolicyMode,
    ) -> Result<Option<PolicyMode>, ContractError> {
        let event = event.into();
        validate_event(&event)?;
        validate_mode(&event, mode)?;
        Ok(self.0.insert(event, mode))
    }

    #[must_use]
    pub fn get(&self, event: &str) -> Option<PolicyMode> {
        self.0.get(event).copied()
    }

    pub(crate) fn validate(&self) -> Result<(), ContractError> {
        for (event, mode) in &self.0 {
            validate_event(event)?;
            validate_mode(event, *mode)?;
        }
        Ok(())
    }
}

/// Resolves policy precedence in one place: client, then project, then the
/// declared default. Overrides for undeclared events are rejected as typos.
#[derive(Clone, Debug)]
pub struct PolicyResolver {
    defaults: PolicyConfig,
    project: PolicyConfig,
}

impl PolicyResolver {
    /// Build a resolver from a complete default layer and project overrides.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError`] when an event name is invalid or a project
    /// override does not have a declared default.
    pub fn new(defaults: PolicyConfig, project: PolicyConfig) -> Result<Self, ContractError> {
        defaults.validate()?;
        project.validate()?;
        validate_known_events(&defaults, &project)?;
        Ok(Self { defaults, project })
    }

    /// Safe standalone defaults from the architecture contract.
    #[must_use]
    pub fn memory_hub_defaults() -> Self {
        let defaults = PolicyConfig(
            [
                (
                    "reconcile_divergence".into(),
                    PolicyMode::RequireFullRebuild,
                ),
                ("memory_push_stale".into(), PolicyMode::Warn),
                ("code_push_stale".into(), PolicyMode::Warn),
                ("dangling_links".into(), PolicyMode::Block),
                ("index_lag".into(), PolicyMode::Repair),
            ]
            .into_iter()
            .collect(),
        );
        Self {
            defaults,
            project: PolicyConfig::default(),
        }
    }

    /// Replace project overrides while retaining this resolver's defaults.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError`] for invalid or undeclared override events.
    pub fn with_project(self, project: PolicyConfig) -> Result<Self, ContractError> {
        Self::new(self.defaults, project)
    }

    /// Resolve one event using client, project, then default precedence.
    ///
    /// # Errors
    ///
    /// Returns [`ContractError`] when the event is invalid, undeclared, or the
    /// client layer contains an invalid or undeclared event.
    pub fn resolve(
        &self,
        event: &str,
        client: Option<&PolicyConfig>,
    ) -> Result<EffectivePolicy, ContractError> {
        validate_event(event)?;
        let default = self
            .defaults
            .get(event)
            .ok_or_else(|| ContractError::unknown_policy(event))?;

        if let Some(client) = client {
            client.validate()?;
            validate_known_events(&self.defaults, client)?;
            if let Some(mode) = client.get(event) {
                return Ok(EffectivePolicy {
                    event: event.to_owned(),
                    mode,
                    source: PolicySource::Client,
                });
            }
        }
        if let Some(mode) = self.project.get(event) {
            return Ok(EffectivePolicy {
                event: event.to_owned(),
                mode,
                source: PolicySource::Project,
            });
        }
        Ok(EffectivePolicy {
            event: event.to_owned(),
            mode: default,
            source: PolicySource::Default,
        })
    }
}

fn validate_known_events(
    defaults: &PolicyConfig,
    overrides: &PolicyConfig,
) -> Result<(), ContractError> {
    if let Some(event) = overrides
        .0
        .keys()
        .find(|event| defaults.get(event).is_none())
    {
        return Err(ContractError::unknown_policy(event));
    }
    Ok(())
}

fn validate_event(event: &str) -> Result<(), ContractError> {
    let valid = !event.is_empty()
        && event.len() <= 128
        && event.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' || byte == b'.'
        });
    if valid {
        Ok(())
    } else {
        Err(ContractError::invalid(
            format!("policy.{event}"),
            "event must contain only lowercase ASCII letters, digits, `_`, or `.`",
        ))
    }
}

fn validate_mode(event: &str, mode: PolicyMode) -> Result<(), ContractError> {
    let supported = match event {
        "reconcile_divergence" => matches!(mode, PolicyMode::RequireFullRebuild),
        "memory_push_stale" | "code_push_stale" | "dangling_links" => {
            matches!(mode, PolicyMode::Off | PolicyMode::Warn | PolicyMode::Block)
        }
        "index_lag" => matches!(
            mode,
            PolicyMode::Warn | PolicyMode::Repair | PolicyMode::Block
        ),
        _ => true,
    };
    if supported {
        Ok(())
    } else {
        Err(ContractError::invalid(
            format!("policy.{event}"),
            "mode is not supported for this policy event",
        ))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::{PolicyConfig, PolicyMode, PolicyResolver, PolicySource};
    use crate::ContractErrorKind;

    #[test]
    fn precedence_and_source_are_explicit() {
        let mut project = PolicyConfig::new();
        project.insert("dangling_links", PolicyMode::Warn).unwrap();
        let resolver = PolicyResolver::memory_hub_defaults()
            .with_project(project)
            .unwrap();

        let project_value = resolver.resolve("dangling_links", None).unwrap();
        assert_eq!(project_value.mode, PolicyMode::Warn);
        assert_eq!(project_value.source, PolicySource::Project);

        let mut client = PolicyConfig::new();
        client.insert("dangling_links", PolicyMode::Off).unwrap();
        let client_value = resolver.resolve("dangling_links", Some(&client)).unwrap();
        assert_eq!(client_value.mode, PolicyMode::Off);
        assert_eq!(client_value.source, PolicySource::Client);

        let default_value = resolver.resolve("index_lag", Some(&client)).unwrap();
        assert_eq!(default_value.mode, PolicyMode::Repair);
        assert_eq!(default_value.source, PolicySource::Default);
    }

    #[test]
    fn serde_rejects_an_unknown_mode() {
        let error = serde_json::from_str::<PolicyConfig>(r#"{"index_lag":"guess"}"#).unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }

    #[test]
    fn undeclared_override_is_a_machine_readable_error() {
        let mut project = PolicyConfig::new();
        project.insert("typo_event", PolicyMode::Block).unwrap();
        let error = PolicyResolver::memory_hub_defaults()
            .with_project(project)
            .unwrap_err();
        assert_eq!(error.kind, ContractErrorKind::UnknownPolicy);
        assert_eq!(error.field, "policy.typo_event");
    }

    #[test]
    fn builtin_events_reject_modes_their_behavior_cannot_honor() {
        let mut project = PolicyConfig::new();
        assert!(
            project
                .insert("dangling_links", PolicyMode::Repair)
                .is_err()
        );

        let effective = PolicyResolver::memory_hub_defaults()
            .resolve("reconcile_divergence", None)
            .unwrap();
        assert_eq!(effective.mode, PolicyMode::RequireFullRebuild);
    }
}
