//! Validated domain values for layer ids, nodes, literals, spikes, and retract width.
//!
//! These types run before Neo4j work: kebab-case layer ids, `node`/`literal`
//! object shapes, predicate IRIs, Spike ranks, and retract scopes. Graph
//! adapters turn the validated values into Cypher parameters.

use crate::iri::{default_lower_for_kind, is_iri, kind_for_label, mint_iri, slugify};
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::fmt;
use thiserror::Error;

/// Caller-facing domain failure: bad input versus violated graph precondition.
#[derive(Debug, Error)]
pub enum DomainError {
    /// Wire or argument validation failed before any graph write.
    #[error("{0}")]
    InvalidInput(String),
    /// The graph exists but the requested handle, membership, or closure is unusable.
    #[error("{0}")]
    Precondition(String),
}

/// A validated graph visibility layer identifier.
///
/// Layer identifiers consist of lowercase kebab-case segments separated by
/// colons, for example `project:mindreader` or `team:platform:graph-memory`.
/// Global visibility is represented by an empty membership list.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, JsonSchema)]
#[schemars(transparent)]
pub struct LayerId(String);

impl LayerId {
    /// Parse a kebab-case colon-namespaced id; the token `global` is invalid (use `[]`).
    pub fn parse(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if value == "global" || !is_valid_layer_id(&value) {
            return Err(DomainError::InvalidInput(format!(
                "invalid layer {value:?}: expected lowercase kebab-case segments separated by colons; use an empty layer list for global visibility"
            )));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

fn is_valid_layer_id(value: &str) -> bool {
    value.split(':').all(|segment| {
        !segment.is_empty()
            && !segment.starts_with('-')
            && !segment.ends_with('-')
            && !segment.contains("--")
            && segment
                .bytes()
                .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
    })
}

impl fmt::Display for LayerId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl AsRef<str> for LayerId {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl TryFrom<String> for LayerId {
    type Error = DomainError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl TryFrom<&str> for LayerId {
    type Error = DomainError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse(value)
    }
}

impl Serialize for LayerId {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for LayerId {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let value = String::deserialize(deserializer)?;
        Self::parse(value).map_err(serde::de::Error::custom)
    }
}

/// Canonical property IRI for a fact predicate (accepts local names or full IRIs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredicateRef(String);

impl PredicateRef {
    /// Canonicalize a local name or IRI into a property IRI with a nonempty local name.
    pub fn parse(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        let trimmed = value.trim();
        if trimmed.is_empty() {
            return Err(DomainError::InvalidInput(
                "predicate cannot be empty".into(),
            ));
        }
        let iri = crate::iri::property_iri(trimmed);
        if crate::iri::name_from_iri(&iri).trim().is_empty() {
            return Err(DomainError::InvalidInput(format!(
                "predicate has no local name: {trimmed:?}"
            )));
        }
        Ok(Self(iri))
    }

    pub fn iri(&self) -> &str {
        &self.0
    }
}

/// Pasteable node handle used by `memory_unify`.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct NodeHandle {
    pub kind: String,
    pub iri: String,
}

impl NodeHandle {
    /// Build a `{kind: node, iri}` handle; IRI checks happen in [`Self::iri`].
    pub fn from_iri(iri: impl Into<String>) -> Self {
        Self {
            kind: "node".into(),
            iri: iri.into(),
        }
    }

    /// Return the node IRI after rejecting non-node kinds and relationship IRIs.
    pub fn iri(&self) -> Result<&str, DomainError> {
        if self.kind != "node" {
            return Err(DomainError::InvalidInput(
                "memory_unify source and target kind must be \"node\"".into(),
            ));
        }
        let iri = self.iri.trim();
        if iri.is_empty() || !is_iri(iri) || iri.starts_with("mindreader:relationship/") {
            return Err(DomainError::InvalidInput(format!(
                "memory_unify requires a node IRI, not {:?}",
                self.iri
            )));
        }
        Ok(iri)
    }
}

/// Wire-shape subject for writes: `kind` must be `"node"`, plus `iri` or `name`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct EntityInput {
    pub kind: String,
    #[serde(default)]
    pub iri: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
}

/// Wire-shape object: one tagged bag (`node` or `literal`) without schema unions.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ObjectInput {
    pub kind: String,
    #[serde(default)]
    pub iri: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub datatype: Option<String>,
}

/// Validated node reference after `kind=node` and IRI/name checks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRef {
    pub iri: Option<String>,
    pub name: Option<String>,
    pub labels: Vec<String>,
}

impl EntityRef {
    /// Require `kind=node` and at least one of `iri` or `name`.
    pub fn from_input(input: EntityInput) -> Result<Self, DomainError> {
        if input.kind != "node" {
            return Err(DomainError::InvalidInput(
                "node input kind must be \"node\"".into(),
            ));
        }
        validate_entity_parts(input.iri, input.name, input.labels)
    }

    /// Prefer an explicit IRI; otherwise mint from name, labels, and kind defaults.
    pub fn resolved_iri(&self, fallback_kind: &str) -> String {
        if let Some(iri) = &self.iri {
            return iri.clone();
        }
        let kind = crate::iri::identity_kind_from_labels(&self.labels)
            .and_then(kind_for_label)
            .unwrap_or(fallback_kind);
        mint_iri(
            kind,
            self.name.as_deref().expect("validated entity has a name"),
            default_lower_for_kind(kind),
        )
    }
}

/// Validated fact object: a node reference or a typed literal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectValue {
    Entity(EntityRef),
    Literal { value: String, datatype: String },
}

impl ObjectValue {
    /// Accept a tagged `node` or `literal` bag and reject mixed fields.
    pub fn from_input(input: ObjectInput) -> Result<Self, DomainError> {
        match input.kind.as_str() {
            "node" => {
                if input.value.is_some() || input.datatype.is_some() {
                    return Err(DomainError::InvalidInput(
                        "node objects cannot contain value or datatype".into(),
                    ));
                }
                Ok(Self::Entity(validate_entity_parts(
                    input.iri,
                    input.name,
                    input.labels,
                )?))
            }
            "literal" => {
                if input.iri.is_some() || input.name.is_some() || !input.labels.is_empty() {
                    return Err(DomainError::InvalidInput(
                        "literal objects cannot contain iri, name, or labels".into(),
                    ));
                }
                let value = input.value.ok_or_else(|| {
                    DomainError::InvalidInput("literal objects require value".into())
                })?;
                let datatype = input.datatype.unwrap_or_else(|| "xsd:string".into());
                if datatype.trim().is_empty() {
                    return Err(DomainError::InvalidInput(
                        "literal datatype cannot be empty".into(),
                    ));
                }
                Ok(Self::Literal { value, datatype })
            }
            other => Err(DomainError::InvalidInput(format!(
                "object kind must be node or literal, got {other:?}"
            ))),
        }
    }

    /// Entity IRI via [`EntityRef::resolved_iri`], or a deterministic literal IRI.
    pub fn resolved_iri(&self) -> String {
        match self {
            Self::Entity(entity) => entity.resolved_iri("element"),
            Self::Literal { value, datatype } => literal_iri(value, datatype),
        }
    }
}

fn validate_entity_parts(
    iri: Option<String>,
    name: Option<String>,
    labels: Vec<String>,
) -> Result<EntityRef, DomainError> {
    let iri = iri.filter(|value| !value.trim().is_empty());
    let name = name.filter(|value| !value.trim().is_empty());
    if iri.is_none() && name.is_none() {
        return Err(DomainError::InvalidInput(
            "node input requires iri or name".into(),
        ));
    }
    if let Some(value) = &iri {
        if !is_iri(value) {
            return Err(DomainError::InvalidInput(format!(
                "invalid entity IRI: {value}"
            )));
        }
    }
    for label in &labels {
        let Some(first) = label.chars().next() else {
            return Err(DomainError::InvalidInput(
                "entity labels cannot be empty".into(),
            ));
        };
        if !first.is_ascii_alphabetic()
            || !label
                .chars()
                .all(|character| character.is_ascii_alphanumeric() || character == '_')
        {
            return Err(DomainError::InvalidInput(format!(
                "invalid entity label: {label:?}"
            )));
        }
    }
    Ok(EntityRef { iri, name, labels })
}

/// Deterministic `mindreader:literal/…` IRI from datatype and value content.
pub fn literal_iri(value: &str, datatype: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(format!("{datatype}:{value}").as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest
        .iter()
        .take(6)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let slug = slugify(value, true);
    let slug = if slug.len() > 40 { &slug[..40] } else { &slug };
    format!("mindreader:literal/{slug}-{hex}")
}

/// Epistemic Spike label used in retrieval ranking (Knowledge highest).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpikeRank {
    Signal,
    Pattern,
    Insight,
    Knowledge,
}

impl SpikeRank {
    /// Parse an optional Spike label; omitted means the subject is unranked.
    pub fn parse(value: Option<String>) -> Result<Option<Self>, DomainError> {
        value
            .map(|value| match value.as_str() {
                "Signal" => Ok(Self::Signal),
                "Pattern" => Ok(Self::Pattern),
                "Insight" => Ok(Self::Insight),
                "Knowledge" => Ok(Self::Knowledge),
                _ => Err(DomainError::InvalidInput(
                    "spike must be one of Signal|Pattern|Insight|Knowledge".into(),
                )),
            })
            .transpose()
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Signal => "Signal",
            Self::Pattern => "Pattern",
            Self::Insight => "Insight",
            Self::Knowledge => "Knowledge",
        }
    }
}

/// In-process retract width: one fact, all facts for a predicate, or a whole subject.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetractScope {
    Fact,
    Predicate,
    Subject,
}

impl RetractScope {
    /// Parse retract width: `fact`, `predicate`, or `subject`.
    pub fn parse(value: &str) -> Result<Self, DomainError> {
        match value {
            "fact" => Ok(Self::Fact),
            "predicate" => Ok(Self::Predicate),
            "subject" => Ok(Self::Subject),
            _ => Err(DomainError::InvalidInput(
                "retract target kind must be fact, predicate, or subject".into(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        literal_iri, EntityInput, EntityRef, LayerId, ObjectInput, ObjectValue, PredicateRef,
    };

    #[test]
    fn layer_ids_use_lowercase_colon_separated_kebab_case() {
        for valid in [
            "project:mindreader",
            "project:graph-memory",
            "team2:platform-7:memory",
        ] {
            assert_eq!(LayerId::parse(valid).unwrap().as_str(), valid);
        }

        for invalid in [
            "",
            "global",
            "Global",
            "project:",
            ":project",
            "project::memory",
            "project:graph_memory",
            "project:graph memory",
            "project:-graph",
            "project:graph-",
            "project:graph--memory",
        ] {
            assert!(LayerId::parse(invalid).is_err(), "accepted {invalid:?}");
        }
    }

    #[test]
    fn layer_ids_validate_during_deserialization() {
        assert_eq!(
            serde_json::from_str::<LayerId>(r#""project:graph-memory""#)
                .unwrap()
                .as_str(),
            "project:graph-memory"
        );
        assert!(serde_json::from_str::<LayerId>(r#""project:Graph""#).is_err());
    }

    #[test]
    fn tagged_inputs_reject_ambiguous_shapes() {
        assert!(EntityRef::from_input(EntityInput {
            kind: "literal".into(),
            iri: None,
            name: Some("x".into()),
            labels: vec![],
        })
        .is_err());
        assert!(ObjectValue::from_input(ObjectInput {
            kind: "literal".into(),
            iri: Some("mindreader:element/x".into()),
            name: None,
            labels: vec![],
            value: Some("x".into()),
            datatype: None,
        })
        .is_err());
    }

    #[test]
    fn literal_identity_is_stable_and_datatype_sensitive() {
        assert_eq!(
            literal_iri("42", "xsd:integer"),
            literal_iri("42", "xsd:integer")
        );
        assert_ne!(
            literal_iri("42", "xsd:integer"),
            literal_iri("42", "xsd:string")
        );
    }

    #[test]
    fn predicates_are_nonempty_and_canonical() {
        assert!(PredicateRef::parse("   ").is_err());
        assert_eq!(
            PredicateRef::parse(" worksOn ").unwrap().iri(),
            "mindreader:property/worksOn"
        );
    }
}
