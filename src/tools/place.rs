//! Auditable layer-membership edits for visible nodes and current facts.
//!
//! Request `scope` is visibility only; each edit’s `add`/`remove` changes the
//! stored `layers` property. Emptying a named membership list makes the record
//! global (`[]`), which is not soft withdrawal. Batches are 1–20 unique targets
//! in one transaction; endpoint closure is checked against the batch’s final
//! memberships. At most one Episode is recorded when anything changes.

use super::facts::{
    is_transient_neo4j_error, normalize_layers, validate_target, PlaceArgs, TargetArgs,
    FACT_LOCK_SCOPE, LAYERS_PROPERTY, MAX_WRITE_FACTS,
};
use crate::domain::DomainError;
use crate::error::{Error, Result};
use crate::graph::{
    acquire_fact_locks_in_txn, create_episode_in_txn, fetch_all_txn, fetch_one_txn,
};
use crate::layers::memberships_cover;
use crate::payload::finish_mutation;
use neo4rs::{query, Graph, Txn};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tokio::time::{sleep, Duration};

#[derive(Clone)]
pub(super) struct NormalizedPlaceEdit {
    index: usize,
    target: TargetArgs,
    pub(super) add: Vec<String>,
    remove: Vec<String>,
}

/// Membership list before and after one edit, used for closure validation and the result item.
struct PlannedPlaceEdit {
    normalized: NormalizedPlaceEdit,
    before: Vec<String>,
    after: Vec<String>,
    added: Vec<String>,
    removed: Vec<String>,
}

/// Validate 1..=20 unique membership edits; `scope` stays visibility, not the change.
pub(super) fn normalize_place_args(
    args: PlaceArgs,
) -> Result<(Vec<String>, Vec<NormalizedPlaceEdit>)> {
    if args.edits.is_empty() || args.edits.len() > MAX_WRITE_FACTS {
        return Err(DomainError::InvalidInput(format!(
            "place edits must contain between 1 and {MAX_WRITE_FACTS} items"
        ))
        .into());
    }
    let scope = normalize_layers(args.scope)?;
    let mut seen = HashSet::new();
    let mut edits = Vec::with_capacity(args.edits.len());
    for (index, edit) in args.edits.into_iter().enumerate() {
        validate_target(&edit.target)?;
        if !seen.insert((edit.target.kind.clone(), edit.target.iri.clone())) {
            return Err(DomainError::InvalidInput(format!(
                "place contains duplicate target {}:{}",
                edit.target.kind, edit.target.iri
            ))
            .into());
        }
        let add = normalize_layers(edit.add)?;
        let remove = normalize_layers(edit.remove)?;
        if add.is_empty() && remove.is_empty() {
            return Err(DomainError::InvalidInput(format!(
                "place edit {index} requires at least one add or remove layer"
            ))
            .into());
        }
        if add.iter().any(|layer| remove.contains(layer)) {
            return Err(DomainError::InvalidInput(format!(
                "place edit {index} adds and removes the same layer"
            ))
            .into());
        }
        edits.push(NormalizedPlaceEdit {
            index,
            target: edit.target,
            add,
            remove,
        });
    }
    Ok((scope, edits))
}

/// Load stored `layers` for a visible non-schema node or current fact.
async fn load_place_memberships_txn(
    txn: &mut Txn,
    scope: &[String],
    target: &TargetArgs,
) -> Result<Vec<String>> {
    let row = if target.kind == "node" {
        fetch_one_txn(
            txn,
            query(
                r#"
                MATCH (target:Entity {iri: $iri})
                WHERE size(target.layers) = 0
                   OR any(layer IN target.layers WHERE layer IN $scope)
                WITH target
                WHERE NOT target:Class AND NOT target:Property
                RETURN target.layers AS memberships
                "#,
            )
            .param("iri", target.iri.clone())
            .param("scope", scope.to_vec()),
        )
        .await?
    } else {
        fetch_one_txn(
            txn,
            query(
                r#"
                MATCH (s:Entity)-[target]->(o:Entity)
                WHERE target.iri = $iri AND target.validTo IS NULL
                  AND NOT s:Class AND NOT s:Property
                  AND NOT o:Class AND NOT o:Property
                  AND (size(s.layers) = 0 OR any(layer IN s.layers WHERE layer IN $scope))
                  AND (size(target.layers) = 0 OR any(layer IN target.layers WHERE layer IN $scope))
                  AND (size(o.layers) = 0 OR any(layer IN o.layers WHERE layer IN $scope))
                RETURN target.layers AS memberships
                "#,
            )
            .param("iri", target.iri.clone())
            .param("scope", scope.to_vec()),
        )
        .await?
    };
    row.ok_or_else(|| {
        Error::from(DomainError::Precondition(format!(
            "place target {}:{} is missing, hidden, or historical",
            target.kind, target.iri
        )))
    })?
    .get::<Vec<String>>("memberships")
    .map_err(Into::into)
}

/// Lock every current fact and endpoint that the batch could change or expose.
async fn acquire_place_closure_locks_txn(
    txn: &mut Txn,
    edits: &[NormalizedPlaceEdit],
) -> Result<()> {
    let node_iris = edits
        .iter()
        .filter(|edit| edit.target.kind == "node")
        .map(|edit| edit.target.iri.clone())
        .collect::<Vec<_>>();
    let fact_iris = edits
        .iter()
        .filter(|edit| edit.target.kind == "fact")
        .map(|edit| edit.target.iri.clone())
        .collect::<Vec<_>>();
    let rows = fetch_all_txn(
        txn,
        query(
            r#"
            MATCH (s:Entity)-[r]->(o:Entity)
            WHERE r.validTo IS NULL
              AND (s.iri IN $nodeIris OR o.iri IN $nodeIris OR r.iri IN $factIris)
            RETURN s.iri AS sIri, r.iri AS rIri, o.iri AS oIri
            "#,
        )
        .param("nodeIris", node_iris)
        .param("factIris", fact_iris),
    )
    .await?;
    let mut iris = edits
        .iter()
        .map(|edit| edit.target.iri.clone())
        .collect::<Vec<_>>();
    for row in rows {
        iris.push(row.get("sIri")?);
        iris.push(row.get("rIri")?);
        iris.push(row.get("oIri")?);
    }
    iris.sort();
    iris.dedup();
    let locks = iris
        .into_iter()
        .map(|iri| (iri, LAYERS_PROPERTY.into(), FACT_LOCK_SCOPE.into()))
        .collect::<Vec<_>>();
    acquire_fact_locks_in_txn(txn, &locks).await
}

/// Compute the membership list after add/remove; emptying a named record makes it global.
///
/// `add` on a global record (`layers=[]`) replaces `[]` with the named set.
fn planned_memberships(before: &[String], edit: &NormalizedPlaceEdit) -> PlannedPlaceEdit {
    let mut after = before
        .iter()
        .filter(|layer| !edit.remove.contains(layer))
        .cloned()
        .collect::<Vec<_>>();
    after.extend(edit.add.iter().cloned());
    after.sort();
    after.dedup();
    let added = after
        .iter()
        .filter(|layer| !before.contains(layer))
        .cloned()
        .collect();
    let removed = before
        .iter()
        .filter(|layer| !after.contains(layer))
        .cloned()
        .collect();
    PlannedPlaceEdit {
        normalized: edit.clone(),
        before: before.to_vec(),
        after,
        added,
        removed,
    }
}

/// Label a hidden endpoint as `literal` or `node` in closure-precondition messages.
fn hidden_endpoint_kind(iri: &str, labels: &[String]) -> &'static str {
    if labels.iter().any(|label| label == "Literal") || iri.starts_with("mindreader:literal/") {
        "literal"
    } else {
        "node"
    }
}

/// Reject a batch whose final memberships would expose a fact while an endpoint is hidden.
async fn validate_place_closure_txn(txn: &mut Txn, planned: &[PlannedPlaceEdit]) -> Result<()> {
    let node_iris = planned
        .iter()
        .filter(|edit| edit.normalized.target.kind == "node")
        .map(|edit| edit.normalized.target.iri.clone())
        .collect::<Vec<_>>();
    let fact_iris = planned
        .iter()
        .filter(|edit| edit.normalized.target.kind == "fact")
        .map(|edit| edit.normalized.target.iri.clone())
        .collect::<Vec<_>>();
    let after = planned
        .iter()
        .map(|edit| (edit.normalized.target.iri.clone(), edit.after.clone()))
        .collect::<HashMap<_, _>>();
    let rows = fetch_all_txn(
        txn,
        query(
            r#"
            MATCH (s:Entity)-[r]->(o:Entity)
            WHERE r.validTo IS NULL
              AND (s.iri IN $nodeIris OR o.iri IN $nodeIris OR r.iri IN $factIris)
            RETURN s.iri AS sIri, s.layers AS sLayers, labels(s) AS sLabels,
                   r.iri AS rIri, r.layers AS rLayers,
                   o.iri AS oIri, o.layers AS oLayers, labels(o) AS oLabels
            "#,
        )
        .param("nodeIris", node_iris)
        .param("factIris", fact_iris),
    )
    .await?;
    for row in rows {
        let s_iri: String = row.get("sIri")?;
        let r_iri: String = row.get("rIri")?;
        let o_iri: String = row.get("oIri")?;
        let stored_s_layers = row.get::<Vec<String>>("sLayers")?;
        let stored_r_layers = row.get::<Vec<String>>("rLayers")?;
        let stored_o_layers = row.get::<Vec<String>>("oLayers")?;
        let s_layers = after.get(&s_iri).cloned().unwrap_or(stored_s_layers);
        let r_layers = after.get(&r_iri).cloned().unwrap_or(stored_r_layers);
        let o_layers = after.get(&o_iri).cloned().unwrap_or(stored_o_layers);
        if !memberships_cover(&s_layers, &r_layers) {
            let s_labels = row.get::<Vec<String>>("sLabels")?;
            return Err(DomainError::Precondition(format!(
                "place final state would expose fact {r_iri} while endpoint {s_iri} ({}) is hidden",
                hidden_endpoint_kind(&s_iri, &s_labels)
            ))
            .into());
        }
        if !memberships_cover(&o_layers, &r_layers) {
            let o_labels = row.get::<Vec<String>>("oLabels")?;
            return Err(DomainError::Precondition(format!(
                "place final state would expose fact {r_iri} while endpoint {o_iri} ({}) is hidden",
                hidden_endpoint_kind(&o_iri, &o_labels)
            ))
            .into());
        }
    }
    Ok(())
}

/// One place transaction: lock, plan final memberships, enforce endpoint closure, commit or roll back.
async fn memory_place_once(
    graph: &Graph,
    scope: Vec<String>,
    edits: Vec<NormalizedPlaceEdit>,
) -> Result<Value> {
    let locks = edits
        .iter()
        .map(|edit| {
            (
                edit.target.iri.clone(),
                LAYERS_PROPERTY.into(),
                FACT_LOCK_SCOPE.into(),
            )
        })
        .collect::<Vec<_>>();
    let mut txn = graph.start_txn().await?;
    let result = async {
        acquire_fact_locks_in_txn(&mut txn, &locks).await?;
        acquire_place_closure_locks_txn(&mut txn, &edits).await?;
        let mut planned = Vec::with_capacity(edits.len());
        for edit in &edits {
            let before = load_place_memberships_txn(&mut txn, &scope, &edit.target).await?;
            planned.push(planned_memberships(&before, edit));
        }
        let changed = planned
            .iter()
            .filter(|edit| edit.before != edit.after)
            .count();
        if changed == 0 {
            return Ok::<_, Error>((None, planned));
        }
        validate_place_closure_txn(&mut txn, &planned).await?;
        let episode = create_episode_in_txn(&mut txn, "place", None).await?;
        let changed_edits = planned
            .iter()
            .filter(|edit| edit.before != edit.after)
            .collect::<Vec<_>>();
        txn.run(
            query(
                "MATCH (e:Entity:Episode {iri: $episode}) \
                 SET e.batchSize = $batchSize, e.targetIris = $targetIris, \
                     e.targetKinds = $targetKinds, e.beforeLayersJson = $beforeLayersJson, \
                     e.afterLayersJson = $afterLayersJson, e.addedLayersJson = $addedLayersJson, \
                     e.removedLayersJson = $removedLayersJson",
            )
            .param("episode", episode.iri.clone())
            .param("batchSize", changed_edits.len() as i64)
            .param(
                "targetIris",
                changed_edits
                    .iter()
                    .map(|edit| edit.normalized.target.iri.clone())
                    .collect::<Vec<_>>(),
            )
            .param(
                "targetKinds",
                changed_edits
                    .iter()
                    .map(|edit| edit.normalized.target.kind.clone())
                    .collect::<Vec<_>>(),
            )
            .param(
                "beforeLayersJson",
                changed_edits
                    .iter()
                    .map(|edit| serde_json::to_string(&edit.before))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            )
            .param(
                "afterLayersJson",
                changed_edits
                    .iter()
                    .map(|edit| serde_json::to_string(&edit.after))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            )
            .param(
                "addedLayersJson",
                changed_edits
                    .iter()
                    .map(|edit| serde_json::to_string(&edit.added))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            )
            .param(
                "removedLayersJson",
                changed_edits
                    .iter()
                    .map(|edit| serde_json::to_string(&edit.removed))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            ),
        )
        .await?;
        for edit in &changed_edits {
            let target_match = if edit.normalized.target.kind == "node" {
                "MATCH (target:Entity {iri: $iri})"
            } else {
                "MATCH ()-[target]->() WHERE target.iri = $iri AND target.validTo IS NULL"
            };
            fetch_one_txn(
                &mut txn,
                query(&format!(
                    "{target_match} SET target.layers = $memberships, \
                     target.layersUpdatedAt = datetime(), target.layerEpisodeId = $episode \
                     RETURN target.iri AS iri"
                ))
                .param("iri", edit.normalized.target.iri.clone())
                .param("memberships", edit.after.clone())
                .param("episode", episode.iri.clone()),
            )
            .await?
            .ok_or_else(|| {
                DomainError::Precondition(format!(
                    "place target {} changed concurrently",
                    edit.normalized.target.iri
                ))
            })?;
        }
        Ok((Some(episode), planned))
    }
    .await;
    let (episode, planned) = match result {
        Ok((None, planned)) => {
            txn.rollback().await?;
            (None, planned)
        }
        Ok((Some(episode), planned)) => {
            txn.commit()
                .await
                .map_err(|source| Error::AmbiguousCommit {
                    operation: "place",
                    source,
                })?;
            (Some(episode), planned)
        }
        Err(error) => {
            let _ = txn.rollback().await;
            return Err(error);
        }
    };
    let changed = planned
        .iter()
        .filter(|edit| edit.before != edit.after)
        .count();
    let items = planned
        .into_iter()
        .map(|edit| {
            let status = if edit.before == edit.after {
                "noop"
            } else {
                "changed"
            };
            json!({
                "index": edit.normalized.index,
                "target": edit.normalized.target,
                "status": status,
                "before": edit.before,
                "memberships": edit.after,
                "added": edit.added,
                "removed": edit.removed,
            })
        })
        .collect::<Vec<_>>();
    let place_facts = items
        .iter()
        .filter(|item| item.pointer("/target/kind").and_then(Value::as_str) == Some("fact"))
        .map(|item| json!({ "target": item.get("target") }))
        .collect::<Vec<_>>();
    let place_nodes = items
        .iter()
        .filter(|item| item.pointer("/target/kind").and_then(Value::as_str) == Some("node"))
        .filter_map(|item| item.get("target").cloned())
        .collect::<Vec<_>>();
    finish_mutation(
        json!({
            "ok": true,
            "scope": scope,
            "noop": changed == 0,
            "episode": episode.map(|episode| json!({
                "iri": episode.iri, "at": episode.at, "tool": episode.tool
            })).unwrap_or(Value::Null),
            "summary": { "requested": items.len(), "changed": changed, "noop": items.len() - changed },
            "items": items,
        }),
        &place_facts,
        &place_nodes,
        None,
        None,
    )
}

/// Atomic `place`: `scope` is visibility only; each edit’s `add`/`remove` changes stored `layers`.
///
/// Endpoint closure is checked against the batch’s final memberships; at most one Episode.
pub async fn memory_place(graph: &Graph, args: PlaceArgs) -> Result<Value> {
    let (scope, edits) = normalize_place_args(args)?;
    for attempt in 0..3_u64 {
        match memory_place_once(graph, scope.clone(), edits.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}
