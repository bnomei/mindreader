use crate::iri::{default_lower_for_kind, is_iri, kind_for_label, mint_iri, slugify};
use schemars::JsonSchema;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::fmt;

#[derive(Debug)]
pub enum DomainError {
    InvalidInput(String),
    Precondition(String),
}

impl fmt::Display for DomainError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidInput(message) | Self::Precondition(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for DomainError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectId(String);

impl ProjectId {
    pub fn parse(value: impl Into<String>) -> Result<Self, DomainError> {
        let value = value.into();
        if !value.starts_with("project:") || value.len() == "project:".len() {
            return Err(DomainError::InvalidInput(
                "MINDREADER_PROJECT must use the form project:<slug>".into(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntityRef {
    pub iri: Option<String>,
    pub name: Option<String>,
    pub labels: Vec<String>,
}

impl EntityRef {
    pub fn from_input(input: EntityInput) -> Result<Self, DomainError> {
        if input.kind != "entity" {
            return Err(DomainError::InvalidInput(
                "entity input kind must be \"entity\"".into(),
            ));
        }
        validate_entity_parts(input.iri, input.name, input.labels)
    }

    pub fn resolved_iri(&self, fallback_kind: &str) -> String {
        if let Some(iri) = &self.iri {
            return iri.clone();
        }
        let kind = self
            .labels
            .iter()
            .find_map(|label| kind_for_label(label))
            .unwrap_or(fallback_kind);
        mint_iri(
            kind,
            self.name.as_deref().expect("validated entity has a name"),
            default_lower_for_kind(kind),
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObjectValue {
    Entity(EntityRef),
    Literal { value: String, datatype: String },
}

impl ObjectValue {
    pub fn from_input(input: ObjectInput) -> Result<Self, DomainError> {
        match input.kind.as_str() {
            "entity" => {
                if input.value.is_some() || input.datatype.is_some() {
                    return Err(DomainError::InvalidInput(
                        "entity objects cannot contain value or datatype".into(),
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
                "object kind must be entity or literal, got {other:?}"
            ))),
        }
    }

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
            "entity input requires iri or name".into(),
        ));
    }
    if let Some(value) = &iri {
        if !is_iri(value) {
            return Err(DomainError::InvalidInput(format!(
                "invalid entity IRI: {value}"
            )));
        }
    }
    if labels.iter().any(|label| label.trim().is_empty()) {
        return Err(DomainError::InvalidInput(
            "entity labels cannot be empty".into(),
        ));
    }
    Ok(EntityRef { iri, name, labels })
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpikeRank {
    Signal,
    Pattern,
    Insight,
    Knowledge,
}

impl SpikeRank {
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RetractScope {
    Fact,
    Predicate,
    Subject,
}

impl RetractScope {
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
    use super::{literal_iri, EntityInput, EntityRef, ObjectInput, ObjectValue};

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
}
