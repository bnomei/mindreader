//! Layer-id validation and visibility-union policy.
//!
//! MCP scoped tools take a `scope` array of layer ids. Empty `scope` is
//! global-only. Named ids form an OR union. Graph records store memberships
//! in `layers`; empty memberships are global and visible in every request.
//! Relationship visibility also requires visible endpoints (enforced in Cypher).

use crate::domain::{DomainError, LayerId};

/// Parse MCP `scope` (or stored membership) strings, dedupe, and sort.
pub fn validate_layer_ids<I, S>(layers: I) -> Result<Vec<LayerId>, DomainError>
where
    I: IntoIterator<Item = S>,
    S: Into<String>,
{
    let mut layers = layers
        .into_iter()
        .map(|layer| LayerId::parse(layer.into()))
        .collect::<Result<Vec<_>, _>>()?;
    layers.sort();
    layers.dedup();
    Ok(layers)
}

/// Validate, sort, deduplicate, and stringify a request scope for Cypher parameters.
pub(crate) fn normalize_scope(layers: Vec<String>) -> Result<Vec<String>, DomainError> {
    Ok(validate_layer_ids(layers)?
        .into_iter()
        .map(LayerId::into_string)
        .collect())
}

/// Decide whether a record's memberships intersect a requested layer scope.
///
/// Records with no memberships are global. A global record is visible in every
/// request scope; otherwise at least one record membership must occur in the
/// requested scope.
#[cfg(test)]
fn record_is_visible(record_memberships: &[LayerId], requested: &[LayerId]) -> bool {
    if record_memberships.is_empty() {
        return true;
    }

    record_memberships
        .iter()
        .any(|membership| requested.contains(membership))
}

/// Visibility does not imply mutability: global records require global scope.
pub(crate) fn record_is_mutable(record_memberships: &[String], requested: &[String]) -> bool {
    if record_memberships.is_empty() {
        requested.is_empty()
    } else if requested.is_empty() {
        false
    } else {
        record_memberships
            .iter()
            .any(|membership| requested.contains(membership))
    }
}

/// Endpoint closure for a relationship membership set.
///
/// Global relationships require global endpoints. Named relationships require
/// an endpoint that is global or belongs to every relationship membership.
pub(crate) fn memberships_cover(
    endpoint_memberships: &[String],
    relationship_memberships: &[String],
) -> bool {
    if relationship_memberships.is_empty() {
        endpoint_memberships.is_empty()
    } else {
        endpoint_memberships.is_empty()
            || relationship_memberships
                .iter()
                .all(|layer| endpoint_memberships.contains(layer))
    }
}

#[cfg(test)]
mod tests {
    use super::{
        memberships_cover, normalize_scope, record_is_mutable, record_is_visible,
        validate_layer_ids,
    };
    use crate::domain::LayerId;

    fn layer(value: &str) -> LayerId {
        LayerId::parse(value).unwrap()
    }

    #[test]
    fn request_layers_are_validated_deduplicated_and_sorted() {
        let layers = validate_layer_ids(["project:zeta", "project:alpha", "project:zeta"]).unwrap();
        assert_eq!(
            layers.iter().map(LayerId::as_str).collect::<Vec<_>>(),
            ["project:alpha", "project:zeta"]
        );
        assert!(validate_layer_ids(["project:Alpha"]).is_err());
        assert_eq!(
            normalize_scope(vec!["project:zeta".into(), "project:alpha".into()]).unwrap(),
            ["project:alpha", "project:zeta"]
        );
    }

    #[test]
    fn empty_request_scope_is_global_only() {
        assert!(record_is_visible(&[], &[]));
        assert!(!record_is_visible(&[layer("project:alpha")], &[]));
    }

    #[test]
    fn nonempty_scope_sees_global_or_intersecting_records() {
        let requested = validate_layer_ids(["project:beta", "project:alpha"]).unwrap();
        assert!(record_is_visible(&[], &requested));
        assert!(record_is_visible(&[layer("project:alpha")], &requested));
        assert!(record_is_visible(
            &[layer("project:other"), layer("project:beta")],
            &requested
        ));
        assert!(!record_is_visible(&[layer("project:other")], &requested));

        assert_eq!(
            requested.iter().map(LayerId::as_str).collect::<Vec<_>>(),
            ["project:alpha", "project:beta"]
        );
    }

    #[test]
    fn mutation_scope_and_endpoint_closure_are_distinct_from_visibility() {
        assert!(record_is_mutable(&[], &[]));
        assert!(!record_is_mutable(&[], &["project:a".into()]));
        assert!(!record_is_mutable(&["project:a".into()], &[]));
        assert!(record_is_mutable(
            &["project:a".into()],
            &["project:a".into(), "project:b".into()]
        ));

        assert!(memberships_cover(&[], &[]));
        assert!(!memberships_cover(&["project:a".into()], &[]));
        assert!(memberships_cover(&[], &["project:a".into()]));
        assert!(memberships_cover(
            &["project:a".into(), "project:b".into()],
            &["project:a".into(), "project:b".into()]
        ));
        assert!(!memberships_cover(
            &["project:a".into()],
            &["project:a".into(), "project:b".into()]
        ));
    }
}
