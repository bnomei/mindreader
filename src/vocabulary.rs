//! Canonical graph vocabulary shared by persistence and memory operations.
//!
//! Relationship classification is behavioral policy: it controls which
//! predicates receive dedicated Neo4j relationship types, which records are
//! client-mutable, which facts are indexed, and which seeded identities may be
//! absorbed by `unify`. Keep those sets here instead of redefining them beside
//! individual Cypher queries.

use crate::iri::{name_from_iri, property_iri};

/// Canonical catalog identity for system-generated contradiction facts.
pub(crate) const CONTRADICTS_PROPERTY_IRI: &str = "mindreader:property/CONTRADICTS";
/// Canonical catalog identity for system-generated revision history.
pub(crate) const SUPERSEDES_PROPERTY_IRI: &str = "mindreader:property/SUPERSEDES";

/// Every fixed relationship type recognized by the current graph model.
pub(crate) const FIXED_RELATIONSHIPS: &[&str] = &[
    "INSTANCE_OF",
    "SUBCLASS_OF",
    "SUBPROPERTY_OF",
    "DOMAIN",
    "RANGE",
    "ASSERTS",
    "ABOUT",
    "EVIDENCE_FOR",
    "DERIVED_FROM",
    "SUPPORTS",
    "CONTRADICTS",
    "SUPERSEDES",
];

/// Fixed relationships represented by their own Neo4j type rather than `ASSERTS`.
pub(crate) const STRUCTURAL_RELATIONSHIPS: &[&str] = &[
    "INSTANCE_OF",
    "SUBCLASS_OF",
    "SUBPROPERTY_OF",
    "DOMAIN",
    "RANGE",
    "ABOUT",
    "EVIDENCE_FOR",
    "DERIVED_FROM",
    "SUPPORTS",
    "CONTRADICTS",
    "SUPERSEDES",
];

/// Schema-definition relationships that are always global and client-immutable.
pub(crate) const SCHEMA_RELATIONSHIPS: &[&str] = &[
    "INSTANCE_OF",
    "SUBCLASS_OF",
    "SUBPROPERTY_OF",
    "DOMAIN",
    "RANGE",
];

/// Relationships created only by Mindreader's correction/contradiction logic.
pub(crate) const SYSTEM_RELATIONSHIPS: &[&str] = &["CONTRADICTS", "SUPERSEDES"];

/// Relationship types included in the wakeup full-text fact index.
pub(crate) const SEARCHABLE_RELATIONSHIPS: &[&str] = &[
    "ASSERTS",
    "ABOUT",
    "INSTANCE_OF",
    "SUBCLASS_OF",
    "SUBPROPERTY_OF",
    "DOMAIN",
    "RANGE",
    "EVIDENCE_FOR",
    "DERIVED_FROM",
    "SUPPORTS",
];

/// Bootstrap identities are permanent catalog anchors and may only survive `unify`.
pub(crate) const BOOTSTRAP_SEEDED_IRIS: &[&str] = &[
    "mindreader:class/Class",
    "mindreader:class/Property",
    "mindreader:class/Element",
    "mindreader:property/ABOUT",
    "mindreader:property/INSTANCE_OF",
    "mindreader:property/SUBCLASS_OF",
    "mindreader:property/SUBPROPERTY_OF",
    "mindreader:property/DOMAIN",
    "mindreader:property/RANGE",
    "mindreader:property/EVIDENCE_FOR",
    "mindreader:property/DERIVED_FROM",
    "mindreader:property/SUPPORTS",
    CONTRADICTS_PROPERTY_IRI,
    SUPERSEDES_PROPERTY_IRI,
];

/// Map a property name/IRI to its dedicated Neo4j relationship type, if any.
pub(crate) fn structural_relationship_for(property: &str) -> Option<String> {
    let iri = property_iri(property);
    let name = name_from_iri(&iri);
    let candidate = name.to_ascii_uppercase();
    if STRUCTURAL_RELATIONSHIPS.contains(&candidate.as_str()) {
        return Some(candidate);
    }
    STRUCTURAL_RELATIONSHIPS
        .iter()
        .any(|relationship| iri == format!("mindreader:property/{relationship}"))
        .then(|| name.to_ascii_uppercase())
}

/// True when a property is reserved for Mindreader-generated history.
pub(crate) fn is_system_relationship(property: &str) -> bool {
    structural_relationship_for(property)
        .as_deref()
        .is_some_and(|relationship| SYSTEM_RELATIONSHIPS.contains(&relationship))
}

/// True when a catalog identity seeded by bootstrap must not be absorbed.
pub(crate) fn is_bootstrap_seeded(iri: &str) -> bool {
    BOOTSTRAP_SEEDED_IRIS.contains(&iri)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relationship_categories_are_consistent() {
        assert!(STRUCTURAL_RELATIONSHIPS
            .iter()
            .all(|relationship| FIXED_RELATIONSHIPS.contains(relationship)));
        assert!(SCHEMA_RELATIONSHIPS
            .iter()
            .all(|relationship| STRUCTURAL_RELATIONSHIPS.contains(relationship)));
        assert!(SYSTEM_RELATIONSHIPS
            .iter()
            .all(|relationship| STRUCTURAL_RELATIONSHIPS.contains(relationship)));
        assert!(SEARCHABLE_RELATIONSHIPS
            .iter()
            .all(|relationship| FIXED_RELATIONSHIPS.contains(relationship)));
        assert!(!SEARCHABLE_RELATIONSHIPS.contains(&"SUPERSEDES"));
        assert!(!SEARCHABLE_RELATIONSHIPS.contains(&"CONTRADICTS"));
    }

    #[test]
    fn property_classification_accepts_names_and_iris() {
        assert_eq!(
            structural_relationship_for("ABOUT").as_deref(),
            Some("ABOUT")
        );
        assert_eq!(
            structural_relationship_for("mindreader:property/ABOUT").as_deref(),
            Some("ABOUT")
        );
        assert!(structural_relationship_for("custom").is_none());
        assert!(is_system_relationship("mindreader:property/SUPERSEDES"));
    }

    #[test]
    fn bootstrap_catalog_is_permanent() {
        assert!(is_bootstrap_seeded("mindreader:class/Element"));
        assert!(is_bootstrap_seeded("mindreader:property/ABOUT"));
        assert!(!is_bootstrap_seeded("mindreader:property/custom"));
    }
}
