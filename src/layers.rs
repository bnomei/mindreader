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

/// Clone the already-validated request `scope`; empty remains global-only.
pub fn visible_layers(requested: &[LayerId]) -> Vec<LayerId> {
    requested.to_vec()
}

/// Decide whether a record's memberships intersect a requested layer scope.
///
/// Records with no memberships are global. A global record is visible in every
/// request scope; otherwise at least one record membership must occur in the
/// requested scope.
pub fn record_is_visible(record_memberships: &[LayerId], requested: &[LayerId]) -> bool {
    if record_memberships.is_empty() {
        return true;
    }

    record_memberships
        .iter()
        .any(|membership| requested.contains(membership))
}

#[cfg(test)]
mod tests {
    use super::{record_is_visible, validate_layer_ids, visible_layers};
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
    }

    #[test]
    fn empty_request_scope_is_global_only() {
        let layers = visible_layers(&[]);
        assert!(layers.is_empty());
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
            visible_layers(&requested)
                .iter()
                .map(LayerId::as_str)
                .collect::<Vec<_>>(),
            ["project:alpha", "project:beta"]
        );
    }
}
