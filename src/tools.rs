//! Graph mutations and in-process helpers behind five MCP mutation handlers.
//!
//! MCP calls `write`, `revise`, `withdraw`, `judge`, and `place` here. Writes are set-valued,
//! corrections record `SUPERSEDES` in one transaction, withdrawal is soft
//! (`validTo`), and membership edits keep endpoint closure. Class/Property
//! records stay global (`layers=[]`, `stub=false`). `CONTRADICTS` and
//! `SUPERSEDES` are system-owned. A state change records exactly one Episode.

use crate::domain::{
    DomainError, EntityInput, EntityRef, ObjectInput, ObjectValue, PredicateRef, SpikeRank,
    WithdrawalScope,
};
use crate::graph::{
    acquire_fact_locks_in_txn, create_episode_in_txn, endpoint_json, ensure_property_in_txn,
    fact_text, fetch_all, fetch_all_txn, fetch_one, fetch_one_txn, merge_literal_in_txn,
    merge_node_in_txn, node_json, path_to_json, rel_json, safe_label, safe_rel, structural_rel_for,
    Episode, MergedNode, NodeSpec, FIXED_RELS,
};
use crate::layers::validate_layer_ids;
use crate::merge::merge_suggestions_in_txn;
use crate::payload::finish_mutation;
use crate::{
    error::{Error, Result},
    operation_error,
};
use neo4rs::{query, Graph, Node, Path, Relation, Txn};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tokio::time::{sleep, Duration};
use uuid::Uuid;

const SCHEMA_STRUCTURAL_RELS: &[&str] = &[
    "INSTANCE_OF",
    "SUBCLASS_OF",
    "SUBPROPERTY_OF",
    "DOMAIN",
    "RANGE",
];
const SYSTEM_OWNED_RELS: &[&str] = &["CONTRADICTS", "SUPERSEDES"];
const FACT_LOCK_SCOPE: &str = "@fact";
const LAYERS_PROPERTY: &str = "mindreader:property/layers";
const PREDICATE_USAGE_PROPERTY: &str = "mindreader:property/predicate-usage";
const CONTRADICTS_PROPERTY: &str = "mindreader:property/CONTRADICTS";
const MAX_WRITE_FACTS: usize = 20;

/// Subject, predicate-usage, membership, and optional CONTRADICTS guards for one write triple.
fn write_fact_lock_requests(
    subject_iri: &str,
    prop_iri: &str,
    object_iri: &str,
    contradicts: bool,
) -> Vec<(String, String, String)> {
    let mut locks = vec![
        (
            prop_iri.to_string(),
            PREDICATE_USAGE_PROPERTY.into(),
            FACT_LOCK_SCOPE.into(),
        ),
        (
            subject_iri.to_string(),
            prop_iri.to_string(),
            FACT_LOCK_SCOPE.into(),
        ),
        (
            subject_iri.to_string(),
            LAYERS_PROPERTY.into(),
            FACT_LOCK_SCOPE.into(),
        ),
        (
            object_iri.to_string(),
            LAYERS_PROPERTY.into(),
            FACT_LOCK_SCOPE.into(),
        ),
    ];
    if contradicts {
        locks.push((
            object_iri.to_string(),
            CONTRADICTS_PROPERTY.into(),
            FACT_LOCK_SCOPE.into(),
        ));
    }
    locks
}

/// Write locks plus a membership guard on the previous object so revise cannot race withdraw.
fn revision_fact_lock_requests(
    subject_iri: &str,
    prop_iri: &str,
    old_iri: &str,
    new_iri: &str,
    contradicts: bool,
) -> Vec<(String, String, String)> {
    let mut locks = write_fact_lock_requests(subject_iri, prop_iri, new_iri, contradicts);
    locks.push((
        old_iri.to_string(),
        LAYERS_PROPERTY.into(),
        FACT_LOCK_SCOPE.into(),
    ));
    locks
}

/// Subject-wide (`*`) or predicate-specific lock, plus predicate-usage when a property is named.
fn withdrawal_fact_lock_requests(
    subject_iri: &str,
    predicate: Option<&str>,
) -> Vec<(String, String, String)> {
    let mut locks = vec![(
        subject_iri.to_string(),
        predicate.unwrap_or("*").to_string(),
        FACT_LOCK_SCOPE.into(),
    )];
    if let Some(predicate) = predicate {
        locks.push((
            predicate.to_string(),
            PREDICATE_USAGE_PROPERTY.into(),
            FACT_LOCK_SCOPE.into(),
        ));
    }
    locks
}

/// Block client writes of CONTRADICTS and SUPERSEDES as ordinary predicates.
fn reject_system_owned_predicate(predicate: &str) -> Result<()> {
    if structural_rel_for(predicate)
        .as_deref()
        .is_some_and(|rel| SYSTEM_OWNED_RELS.contains(&rel))
    {
        return Err(DomainError::InvalidInput(format!(
            "predicate {predicate:?} is system-owned and cannot be mutated directly"
        ))
        .into());
    }
    Ok(())
}

/// Retry only typed Neo4j transients; ambiguous commits stay non-retryable.
fn is_transient_neo4j_error(error: &Error) -> bool {
    error.is_transient_neo4j()
}

/// Validate, sort, and stringify the request `scope` used as Cypher `$layers`.
fn normalize_layers(raw: Vec<String>) -> Result<Vec<String>> {
    Ok(validate_layer_ids(raw)?
        .into_iter()
        .map(|layer| layer.into_string())
        .collect())
}

/// Require 1..=20 facts before the write transaction starts.
fn validate_write_args(args: &WriteArgs) -> Result<()> {
    if args.facts.is_empty() || args.facts.len() > MAX_WRITE_FACTS {
        return Err(DomainError::InvalidInput(format!(
            "write facts must contain between 1 and {MAX_WRITE_FACTS} items"
        ))
        .into());
    }
    Ok(())
}

/// Canonicalize subject, predicate, and object IRIs and reject system-owned predicates.
fn prepare_write_fact(fact: WriteFact) -> Result<PreparedWriteFact> {
    let predicate = PredicateRef::parse(&fact.p)?;
    reject_system_owned_predicate(predicate.iri())?;
    let spike = SpikeRank::parse(fact.spike)?.map(|rank| rank.as_str().to_string());
    let subject_spec = node_spec(EntityRef::from_input(fact.s)?);
    let subject_kind = "element".to_string();
    let subject_iri = EntityRef {
        iri: subject_spec.iri.clone(),
        name: subject_spec.name.clone(),
        labels: subject_spec.labels.clone(),
    }
    .resolved_iri(&subject_kind);
    let object_value = ObjectValue::from_input(fact.o)?;
    let object_iri = object_value.resolved_iri();
    let prop_iri = predicate.iri().to_string();
    let structural = structural_rel_for(&prop_iri);
    Ok(PreparedWriteFact {
        subject_spec,
        subject_kind,
        subject_iri,
        object_value,
        object_iri,
        prop_iri,
        structural,
        spike,
        contradicts: fact.contradicts,
    })
}

/// Union named memberships; either empty list is global and wins.
fn merge_memberships(current: &[String], incoming: &[String]) -> Vec<String> {
    if current.is_empty() || incoming.is_empty() {
        return Vec::new();
    }
    let mut merged = current.to_vec();
    merged.extend_from_slice(incoming);
    merged.sort();
    merged.dedup();
    merged
}

/// Drop the selected memberships; `None` means the record is unchanged or not in this scope.
fn remove_memberships(current: &[String], selected: &[String]) -> Option<Vec<String>> {
    if selected.is_empty() {
        return current.is_empty().then(Vec::new);
    }
    if current.is_empty() {
        return None;
    }
    let remaining = current
        .iter()
        .filter(|layer| !selected.contains(layer))
        .cloned()
        .collect::<Vec<_>>();
    (remaining != current).then_some(remaining)
}

#[cfg(test)]
fn effective_weight(subject: i64, relationship: i64, object: i64) -> i64 {
    subject.saturating_add(relationship).saturating_add(object)
}

/// Mint a fresh `mindreader:relationship/…` IRI for a newly created fact identity.
fn relationship_iri() -> String {
    format!("mindreader:relationship/{}", Uuid::new_v4())
}

/// One set-valued triple inside a `write` `facts[]` item.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteFact {
    /// Subject node (`kind=node` plus `iri` or `name`).
    pub s: EntityInput,
    /// Predicate local name or property IRI.
    pub p: String,
    /// Object node or typed literal.
    pub o: ObjectInput,
    /// Optional Spike label attached to the subject and, for Element objects, an ABOUT fact.
    #[serde(default)]
    pub spike: Option<String>,
    /// When true, add current CONTRADICTS edges from this object to other current values.
    #[serde(default)]
    pub contradicts: bool,
}

/// `write` arguments: `facts[]` plus call-level `scope`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WriteArgs {
    /// Atomic, input-ordered batch of 1–20 set-valued triples.
    pub facts: Vec<WriteFact>,
    /// Call-level visibility union written as memberships on new or merged facts.
    pub scope: Vec<String>,
}

/// Graph-ready write triple after domain validation and IRI minting.
struct PreparedWriteFact {
    subject_spec: NodeSpec,
    subject_kind: String,
    subject_iri: String,
    object_value: ObjectValue,
    object_iri: String,
    prop_iri: String,
    structural: Option<String>,
    spike: Option<String>,
    contradicts: bool,
}

/// Resolved correction plan used internally by `revise`.
#[derive(Debug, Clone)]
struct RevisionPlan {
    pub s: EntityInput,
    pub p: String,
    pub old: ObjectInput,
    pub new: ObjectInput,
    pub layers: Vec<String>,
    pub spike: Option<String>,
    pub contradicts: bool,
    pub reason: Option<String>,
}

/// Outcome of one revise attempt, including no-op same-object corrections.
struct RevisionResult {
    noop: bool,
    scope: Vec<String>,
    episode: Option<Episode>,
    current_target: Value,
    previous_target: Value,
    fact: Option<Value>,
    alternatives: Vec<Value>,
    unify: Vec<Value>,
}

/// Resolved withdrawal selector used internally by `withdraw`.
#[derive(Debug, Clone)]
struct WithdrawalSelector {
    pub kind: String,
    pub s: EntityInput,
    pub p: Option<String>,
    pub o: Option<ObjectInput>,
}

#[derive(Debug, Clone)]
/// Withdrawal width plus the request `scope` used to select memberships.
struct WithdrawalPlan {
    pub target: WithdrawalSelector,
    pub layers: Vec<String>,
    pub reason: Option<String>,
}

/// Soft-withdrawal outcome; `episode` is `None` when every selected fact was a no-op.
struct WithdrawalResult {
    scope: Vec<String>,
    episode: Option<Episode>,
    withdrawn_targets: Vec<Value>,
    reason: Option<String>,
}

/// Pasteable handle: `kind` is `node` or `fact`, plus a stable IRI.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TargetArgs {
    /// `"node"` or `"fact"` depending on the tool.
    pub kind: String,
    /// Stable identity pasted from a prior result handle.
    pub iri: String,
}

/// Correct one current fact by pasteable fact handle.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviseArgs {
    /// Memberships moved off the selected fact handle; empty is global-only.
    pub scope: Vec<String>,
    /// Pasteable current fact handle (`kind=fact`).
    pub target: TargetArgs,
    /// Replacement object; the previous object stays as history via SUPERSEDES.
    pub new: ObjectInput,
    #[serde(default)]
    pub spike: Option<String>,
    /// When true, add CONTRADICTS from the new object to other current values.
    #[serde(default)]
    pub contradicts: bool,
    /// Optional audit note stored on the Episode and replacement fact.
    #[serde(default)]
    pub reason: Option<String>,
}

/// Soft-withdraw by fact handle or subject (+ optional predicate).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WithdrawArgs {
    /// Visibility union used to select which memberships to remove.
    pub scope: Vec<String>,
    /// One current fact handle; mutually exclusive with `subject`.
    #[serde(default)]
    pub target: Option<TargetArgs>,
    /// Subject-wide or predicate-wide withdrawal when `target` is omitted.
    #[serde(default)]
    pub subject: Option<EntityInput>,
    #[serde(default)]
    pub p: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// One explicit ±1 rating.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JudgeRating {
    /// Pasteable node or current fact handle.
    pub target: TargetArgs,
    /// `strengthen` (+1) or `weaken` (−1).
    pub mode: String,
}

/// Batched strengthen/weaken on node or fact handles.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct JudgeArgs {
    /// Visibility union; hidden or historical handles fail as a precondition.
    pub scope: Vec<String>,
    /// Atomic 1–20 unique ratings recorded as one Episode.
    pub ratings: Vec<JudgeRating>,
}

/// One membership edit inside an atomic `place` batch.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaceEdit {
    /// Node or current fact handle whose stored `layers` change.
    pub target: TargetArgs,
    /// Memberships to add; must not overlap `remove`.
    #[serde(default)]
    pub add: Vec<String>,
    /// Memberships to remove; emptying a named record makes it global.
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Atomic membership edits: `scope` is visibility; `edits` are the change.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaceArgs {
    /// Visibility used to load targets; not the membership change itself.
    pub scope: Vec<String>,
    /// Atomic 1–20 unique membership edits validated against the batch final state.
    pub edits: Vec<PlaceEdit>,
}

/// Convert a validated entity reference into a MERGE `NodeSpec`.
fn node_spec(entity: EntityRef) -> NodeSpec {
    NodeSpec {
        iri: entity.iri,
        name: entity.name,
        labels: entity.labels,
    }
}

/// MERGE a literal or entity object; the bool is true when the object is a literal.
async fn merge_object_in_txn(txn: &mut Txn, value: ObjectValue) -> Result<(MergedNode, bool)> {
    match value {
        ObjectValue::Literal { value, datatype } => {
            Ok((merge_literal_in_txn(txn, &value, &datatype).await?, true))
        }
        ObjectValue::Entity(entity) => Ok((
            merge_node_in_txn(txn, &node_spec(entity), "element", &[]).await?,
            false,
        )),
    }
}

/// Union requested memberships onto an existing named node; global and schema nodes stay `[]`.
async fn apply_node_memberships_txn(
    txn: &mut Txn,
    node: &MergedNode,
    requested: &[String],
) -> Result<bool> {
    let row = fetch_one_txn(
        txn,
        query(
            r#"
            MATCH (n:Entity {iri: $iri})
            WITH n, n.layers AS before
            SET n.layers = CASE
              WHEN size($layers) = 0 THEN []
              WHEN $created THEN $layers
              WHEN size(before) = 0 THEN []
              ELSE reduce(acc = before, layer IN $layers |
                CASE WHEN layer IN acc THEN acc ELSE acc + layer END)
            END
            RETURN before, n.layers AS after
            "#,
        )
        .param("iri", node.iri.clone())
        .param("layers", requested.to_vec())
        .param("created", node.created),
    )
    .await?
    .ok_or_else(|| operation_error!("missing node while applying layers: {}", node.iri))?;
    let before = row.get::<Vec<String>>("before")?;
    let after = row.get::<Vec<String>>("after")?;
    Ok(before != after)
}

/// Current (`validTo` null) fact identity used to plan membership edits.
#[derive(Debug, Clone)]
struct CurrentFact {
    rel_id: i64,
    iri: String,
    layers: Vec<String>,
}

/// Remaining memberships for one current fact after a revise/withdraw selection.
#[derive(Debug, Clone, PartialEq, Eq)]
struct FactMembershipChange {
    rel_id: i64,
    remaining: Vec<String>,
}

/// Compute remaining memberships for revise/withdraw; omit facts that do not intersect `selected`.
fn plan_fact_membership_changes(
    currents: &[CurrentFact],
    selected: &[String],
) -> Vec<FactMembershipChange> {
    currents
        .iter()
        .filter_map(|current| {
            remove_memberships(&current.layers, selected).map(|remaining| FactMembershipChange {
                rel_id: current.rel_id,
                remaining,
            })
        })
        .collect()
}

/// Pick the visible current fact that intersects `selected_layers`, optionally by handle IRI.
fn select_revision_current(
    currents: &[CurrentFact],
    selected_layers: &[String],
    expected_fact_iri: Option<&str>,
) -> Option<CurrentFact> {
    currents
        .iter()
        .find(|current| expected_fact_iri.is_none_or(|iri| current.iri == iri))
        .filter(|current| remove_memberships(&current.layers, selected_layers).is_some())
        .cloned()
}

/// Current exact `(s, property, o)` identities, using a structural type when the predicate maps to one.
async fn find_current_pairs_txn(
    txn: &mut Txn,
    s: &str,
    prop_iri: &str,
    structural: Option<&str>,
    o: &str,
) -> Result<Vec<CurrentFact>> {
    let rows = if let Some(rel) = structural {
        let rel = safe_rel(rel)?;
        let cypher = format!(
            "MATCH (s:Entity {{iri: $s}})-[r:{rel}]->(o:Entity {{iri: $o}}) \
             WHERE r.validTo IS NULL RETURN id(r) AS rid, r.iri AS iri, \
             r.layers AS layers"
        );
        fetch_all_txn(
            txn,
            query(&cypher)
                .param("s", s.to_string())
                .param("o", o.to_string()),
        )
        .await?
    } else {
        fetch_all_txn(
            txn,
            query(
                "MATCH (s:Entity {iri: $s})-[r:ASSERTS]->(o:Entity {iri: $o}) \
                 WHERE r.validTo IS NULL AND r.propertyIri = $p \
                 RETURN id(r) AS rid, r.iri AS iri, r.layers AS layers",
            )
            .param("s", s.to_string())
            .param("o", o.to_string())
            .param("p", prop_iri.to_string()),
        )
        .await?
    };
    rows.into_iter()
        .map(|row| {
            Ok(CurrentFact {
                rel_id: row.get("rid")?,
                iri: row.get("iri")?,
                layers: row.get("layers")?,
            })
        })
        .collect()
}

/// Parameters for creating or membership-merging one current relationship.
struct RelationWrite<'a> {
    rel_type: &'a str,
    s: &'a str,
    o: &'a str,
    prop_iri: &'a str,
    layers: &'a [String],
    episode: &'a Episode,
    reason: Option<&'a str>,
    fact_text: &'a str,
}

/// Reassert an exact triple by merging memberships, or CREATE a new fact identity.
async fn ensure_relation_txn(txn: &mut Txn, write: &RelationWrite<'_>) -> Result<(String, bool)> {
    let current = find_current_pairs_txn(
        txn,
        write.s,
        write.prop_iri,
        Some(write.rel_type).filter(|rel| *rel != "ASSERTS"),
        write.o,
    )
    .await?;
    if current.len() > 1 {
        return Err(operation_error!(
            "multiple current relationship identities for ({}, {}, {})",
            write.s,
            write.prop_iri,
            write.o
        ));
    }
    if let Some(current) = current.first() {
        let merged = merge_memberships(&current.layers, write.layers);
        if merged == current.layers {
            return Ok((current.iri.clone(), false));
        }
        fetch_one_txn(
            txn,
            query(
                "MATCH ()-[r]->() WHERE id(r) = $rid AND r.validTo IS NULL \
                 SET r.layers = $layers, r.layersUpdatedAt = datetime(), \
                     r.layerEpisodeId = $episode RETURN r.iri AS iri",
            )
            .param("rid", current.rel_id)
            .param("layers", merged)
            .param("episode", write.episode.iri.clone()),
        )
        .await?
        .ok_or_else(|| operation_error!("relationship disappeared while merging layers"))?;
        return Ok((current.iri.clone(), true));
    }

    let rel_type = safe_rel(write.rel_type)?;
    let iri = relationship_iri();
    // `rel_type` is allowlisted; interpolating it is the only dynamic Cypher identifier here.
    let cypher = format!(
        r#"
        MATCH (s:Entity {{iri: $s}}), (o:Entity {{iri: $o}})
        CREATE (s)-[r:{rel_type} {{
            iri: $iri,
            propertyIri: $p,
            layers: $layers,
            weight: 0,
            validFrom: datetime(),
            episodeId: $episode,
            factText: $factText
        }}]->(o)
        SET r.reason = $reason
        RETURN r.iri AS iri
        "#
    );
    fetch_one_txn(
        txn,
        query(&cypher)
            .param("s", write.s.to_string())
            .param("o", write.o.to_string())
            .param("iri", iri.clone())
            .param("p", write.prop_iri.to_string())
            .param("layers", write.layers.to_vec())
            .param("episode", write.episode.iri.clone())
            .param("reason", write.reason.map(str::to_string))
            .param("factText", write.fact_text.to_string()),
    )
    .await?
    .ok_or_else(|| operation_error!("failed to create relationship {iri}"))?;
    Ok((iri, true))
}

/// Reload agent-facing node JSON after membership or weight updates in this transaction.
async fn refreshed_node_json_txn(txn: &mut Txn, iri: &str) -> Result<Value> {
    let row = fetch_one_txn(
        txn,
        query("MATCH (n:Entity {iri: $iri}) RETURN n").param("iri", iri.to_string()),
    )
    .await?
    .ok_or_else(|| operation_error!("missing node {iri}"))?;
    node_json(&row.get::<Node>("n")?)
}

/// Other current values of the same subject+predicate visible in `layers` (set-valued alternatives).
async fn find_conflicts_txn(
    txn: &mut Txn,
    s: &str,
    prop_iri: &str,
    structural: Option<&str>,
    o_iri: &str,
    layers: &[String],
) -> Result<Vec<Value>> {
    let rel_type = structural.unwrap_or("ASSERTS");
    let is_structural = structural.is_some();
    fetch_all_txn(
        txn,
        query(
            r#"
            MATCH (s:Entity {iri: $s})-[r]->(o:Entity)
            WHERE r.validTo IS NULL AND o.iri <> $o
              AND (size(s.layers) = 0 OR any(layer IN s.layers WHERE layer IN $layers))
              AND (size(r.layers) = 0 OR any(layer IN r.layers WHERE layer IN $layers))
              AND (size(o.layers) = 0 OR any(layer IN o.layers WHERE layer IN $layers))
              AND (($isStructural AND type(r) = $relType)
                OR (NOT $isStructural AND type(r) = 'ASSERTS' AND r.propertyIri = $p))
            RETURN r, o, r.propertyIri AS p
            "#,
        )
        .param("s", s.to_string())
        .param("o", o_iri.to_string())
        .param("layers", layers.to_vec())
        .param("p", prop_iri.to_string())
        .param("relType", rel_type.to_string())
        .param("isStructural", is_structural),
    )
    .await?
    .into_iter()
    .map(|row| {
        let relationship: Relation = row.get("r")?;
        let object: Node = row.get("o")?;
        let property = row.get::<String>("p")?;
        let object_iri = object.get::<String>("iri")?;
        let relationship = rel_json(&relationship, s, &object_iri)?;
        let object = endpoint_json(&object)?;
        Ok(json!({
            "relationship": relationship,
            "o": object,
            "p": property,
        }))
    })
    .collect()
}

/// Idempotently add current CONTRADICTS edges from a new object to each conflicting value.
async fn ensure_contradictions_txn(
    txn: &mut Txn,
    new_o: &str,
    conflicts: &[Value],
    layers: &[String],
    episode: &Episode,
) -> Result<bool> {
    let mut changed = false;
    let mut old_objects = conflicts
        .iter()
        .filter_map(|conflict| conflict.pointer("/o/iri").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<Vec<_>>();
    old_objects.sort();
    old_objects.dedup();
    for old_o in old_objects {
        let text = format!("{new_o} CONTRADICTS {old_o}");
        let (_, relation_changed) = ensure_relation_txn(
            txn,
            &RelationWrite {
                rel_type: "CONTRADICTS",
                s: new_o,
                o: &old_o,
                prop_iri: CONTRADICTS_PROPERTY,
                layers,
                episode,
                reason: None,
                fact_text: &text,
            },
        )
        .await?;
        changed |= relation_changed;
    }
    Ok(changed)
}

/// Batched set-valued write with one Episode when anything changes.
pub async fn memory_write(graph: &Graph, args: WriteArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_write_once(graph, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

/// One write attempt: lock, MERGE endpoints, merge memberships or no-op, one Episode if changed.
async fn memory_write_once(graph: &Graph, args: WriteArgs) -> Result<Value> {
    validate_write_args(&args)?;
    let layers = normalize_layers(args.scope)?;
    let prepared = args
        .facts
        .into_iter()
        .map(prepare_write_fact)
        .collect::<Result<Vec<_>>>()?;
    let mut locks = Vec::new();
    for fact in &prepared {
        locks.extend(write_fact_lock_requests(
            &fact.subject_iri,
            &fact.prop_iri,
            &fact.object_iri,
            fact.contradicts,
        ));
    }
    let mut txn = graph.start_txn().await?;
    let write = async {
        acquire_fact_locks_in_txn(&mut txn, &locks).await?;
        let episode = create_episode_in_txn(&mut txn, "write", None).await?;
        let mut any_changed = false;
        let mut created_iris = Vec::new();
        let mut items = Vec::with_capacity(prepared.len());
        let mut alternatives = Vec::new();
        for fact in prepared {
            let item = write_prepared_fact_txn(&mut txn, fact, &layers, &episode).await?;
            any_changed |= !item.noop;
            created_iris.extend(item.created_iris);
            if !item.alternatives.is_empty() {
                let target = item
                    .json
                    .get("target")
                    .cloned()
                    .ok_or_else(|| operation_error!("write fact is missing target"))?;
                alternatives.push(json!({
                    "target": target,
                    "conflicts": item.alternatives,
                }));
            }
            items.push(item.json);
        }
        let merge_suggestions = if any_changed {
            created_iris.sort();
            created_iris.dedup();
            merge_suggestions_in_txn(&mut txn, &created_iris, &layers).await?
        } else {
            Vec::new()
        };
        Ok::<_, Error>((any_changed, episode, items, merge_suggestions, alternatives))
    }
    .await;
    let (changed, episode, items, merge_suggestions, alternatives) = match write {
        Ok(value) => {
            if value.0 {
                txn.commit()
                    .await
                    .map_err(|source| Error::AmbiguousCommit {
                        operation: "write",
                        source,
                    })?;
            } else {
                txn.rollback().await?;
            }
            value
        }
        Err(error) => {
            let _ = txn.rollback().await;
            return Err(error);
        }
    };
    finish_mutation(
        json!({
            "ok": true,
            "noop": !changed,
            "scope": layers,
            "episode": if changed {
                json!({ "iri": episode.iri, "at": episode.at, "tool": episode.tool })
            } else {
                Value::Null
            },
            "review": review_payloads(&merge_suggestions, &alternatives),
            "facts": items.clone(),
        }),
        &items,
        &[],
        None,
        None,
    )
}

/// Review bag: advisory unify pairs plus set-valued alternatives (never auto-applied).
fn review_payloads(merge_suggestions: &[Value], alternatives: &[Value]) -> Value {
    json!({ "unify": merge_suggestions, "alternatives": alternatives })
}

#[cfg(test)]
mod review_tests {
    use super::review_payloads;
    use serde_json::json;

    #[test]
    fn review_payloads_keeps_alternatives_when_conflicts_are_empty() {
        let alternatives = vec![json!({
            "target": {"kind":"fact","iri":"mindreader:relationship/a"},
            "conflicts": [{"p":"observed"}]
        })];
        let review = review_payloads(&[], &alternatives);
        assert_eq!(review["alternatives"].as_array().unwrap().len(), 1);
        assert!(review["unify"].as_array().unwrap().is_empty());
    }
}

/// One write item: whether it changed, the fact envelope, created IRIs, and alternatives.
struct PreparedFactResult {
    noop: bool,
    json: Value,
    created_iris: Vec<String>,
    alternatives: Vec<Value>,
}

/// MERGE endpoints, merge or create the fact, optionally attach ABOUT and CONTRADICTS.
async fn write_prepared_fact_txn(
    txn: &mut Txn,
    fact: PreparedWriteFact,
    layers: &[String],
    episode: &Episode,
) -> Result<PreparedFactResult> {
    let subject = merge_node_in_txn(
        txn,
        &fact.subject_spec,
        &fact.subject_kind,
        &fact.spike.clone().into_iter().collect::<Vec<_>>(),
    )
    .await?;
    let (object, object_is_literal) = merge_object_in_txn(txn, fact.object_value).await?;
    let (_, property_created, _) = ensure_property_in_txn(txn, &fact.prop_iri).await?;
    let mut changed = property_created;
    let subject_scope = schema_node_scope(&subject.labels, layers);
    let object_scope = schema_node_scope(&object.labels, layers);
    changed |= apply_node_memberships_txn(txn, &subject, subject_scope).await?;
    changed |= apply_node_memberships_txn(txn, &object, object_scope).await?;
    let fact_text_value = fact_text(&subject.name, &subject.iri, &fact.prop_iri, &object);
    let relation_scope = schema_edge_scope(
        fact.structural.as_deref(),
        &subject.labels,
        &object.labels,
        layers,
    );
    let (relationship_iri, relationship_changed) = ensure_relation_txn(
        txn,
        &RelationWrite {
            rel_type: fact.structural.as_deref().unwrap_or("ASSERTS"),
            s: &subject.iri,
            o: &object.iri,
            prop_iri: &fact.prop_iri,
            layers: relation_scope,
            episode,
            reason: None,
            fact_text: &fact_text_value,
        },
    )
    .await?;
    changed |= relationship_changed;
    let need_about = fact.spike.is_some()
        && !object_is_literal
        && object.labels.iter().any(|label| label == "Element")
        && fact.structural.as_deref() != Some("ABOUT");
    if need_about {
        let about_text = fact_text(
            &subject.name,
            &subject.iri,
            "mindreader:property/ABOUT",
            &object,
        );
        let (_, about_changed) = ensure_relation_txn(
            txn,
            &RelationWrite {
                rel_type: "ABOUT",
                s: &subject.iri,
                o: &object.iri,
                prop_iri: "mindreader:property/ABOUT",
                layers,
                episode,
                reason: None,
                fact_text: &about_text,
            },
        )
        .await?;
        changed |= about_changed;
    }
    let conflicts = find_conflicts_txn(
        txn,
        &subject.iri,
        &fact.prop_iri,
        fact.structural.as_deref(),
        &object.iri,
        layers,
    )
    .await?;
    if fact.contradicts {
        changed |= ensure_contradictions_txn(txn, &object.iri, &conflicts, layers, episode).await?;
    }
    let subject_json = refreshed_node_json_txn(txn, &subject.iri).await?;
    let object_json = refreshed_node_json_txn(txn, &object.iri).await?;
    let mut created_iris = Vec::new();
    if subject.created {
        created_iris.push(subject.iri.clone());
    }
    if object.created && !object_is_literal {
        created_iris.push(object.iri.clone());
    }
    if property_created {
        created_iris.push(fact.prop_iri.clone());
    }
    Ok(PreparedFactResult {
        noop: !changed,
        json: {
            let mut item = crate::graph::fact_envelope(
                subject_json,
                &fact.prop_iri,
                object_json,
                &json!({ "kind": "fact", "iri": relationship_iri, "weight": 0 }),
                relation_scope,
                fact.spike.clone().map(Value::String),
            )?;
            item["noop"] = json!(!changed);
            item["conflicts"] = if fact.contradicts {
                json!(conflicts)
            } else {
                json!([])
            };
            item
        },
        created_iris,
        alternatives: conflicts,
    })
}

/// Remove selected memberships; empty remaining memberships set `validTo` (soft withdraw).
async fn change_fact_memberships_txn(
    txn: &mut Txn,
    current: &CurrentFact,
    selected: &[String],
    episode: &Episode,
    reason: Option<&str>,
) -> Result<bool> {
    let Some(remaining) = remove_memberships(&current.layers, selected) else {
        return Ok(false);
    };
    if remaining.is_empty() {
        txn.run(
            query(
                "MATCH ()-[r]->() WHERE id(r) = $rid AND r.validTo IS NULL \
                 SET r.validTo = datetime(), r.withdrawnBy = $episode, \
                     r.reason = coalesce($reason, r.reason)",
            )
            .param("rid", current.rel_id)
            .param("episode", episode.iri.clone())
            .param("reason", reason.map(str::to_string)),
        )
        .await?;
    } else {
        txn.run(
            query(
                "MATCH ()-[r]->() WHERE id(r) = $rid AND r.validTo IS NULL \
                 SET r.layers = $layers, r.layersUpdatedAt = datetime(), \
                     r.layerEpisodeId = $episode, r.reason = coalesce($reason, r.reason)",
            )
            .param("rid", current.rel_id)
            .param("layers", remaining)
            .param("episode", episode.iri.clone())
            .param("reason", reason.map(str::to_string)),
        )
        .await?;
    }
    Ok(true)
}

/// Apply planned remaining memberships in one UNWIND; empty remaining lists set `validTo`.
async fn change_fact_memberships_batch_txn(
    txn: &mut Txn,
    changes: &[FactMembershipChange],
    episode: &Episode,
    reason: Option<&str>,
) -> Result<()> {
    if changes.is_empty() {
        return Ok(());
    }
    let expected = i64::try_from(changes.len())
        .map_err(|_| operation_error!("too many fact membership changes to batch"))?;
    let ids = changes
        .iter()
        .map(|change| change.rel_id)
        .collect::<Vec<_>>();
    let remaining = changes
        .iter()
        .map(|change| change.remaining.clone())
        .collect::<Vec<_>>();
    let row = fetch_one_txn(
        txn,
        query(
            r#"
            UNWIND range(0, size($ids) - 1) AS i
            WITH $ids[i] AS rid, $remaining[i] AS remaining
            MATCH ()-[r]->()
            WHERE id(r) = rid AND r.validTo IS NULL
            FOREACH (_ IN CASE WHEN size(remaining) = 0 THEN [1] ELSE [] END |
              SET r.validTo = datetime(), r.withdrawnBy = $episode,
                  r.reason = coalesce($reason, r.reason)
            )
            FOREACH (_ IN CASE WHEN size(remaining) > 0 THEN [1] ELSE [] END |
              SET r.layers = remaining, r.layersUpdatedAt = datetime(),
                  r.layerEpisodeId = $episode,
                  r.reason = coalesce($reason, r.reason)
            )
            RETURN count(r) AS updated
            "#,
        )
        .param("ids", ids)
        .param("remaining", remaining)
        .param("episode", episode.iri.clone())
        .param("reason", reason.map(str::to_string)),
    )
    .await?
    .ok_or_else(|| operation_error!("fact membership batch returned no result"))?;
    let updated = row.get::<i64>("updated")?;
    if updated != expected {
        return Err(operation_error!(
            "fact membership batch updated {updated} relationships, expected {expected}"
        ));
    }
    Ok(())
}

/// Retry wrapper around one SUPERSEDES correction.
async fn apply_revision(
    graph: &Graph,
    args: RevisionPlan,
    expected_fact_iri: Option<&str>,
) -> Result<RevisionResult> {
    for attempt in 0..3_u64 {
        match apply_revision_once(graph, args.clone(), expected_fact_iri).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

/// Move selected memberships off the old fact, write the replacement, and record SUPERSEDES.
async fn apply_revision_once(
    graph: &Graph,
    args: RevisionPlan,
    expected_fact_iri: Option<&str>,
) -> Result<RevisionResult> {
    let layers = normalize_layers(args.layers)?;
    let predicate = PredicateRef::parse(&args.p)?;
    reject_system_owned_predicate(predicate.iri())?;
    let spike = SpikeRank::parse(args.spike)?.map(|rank| rank.as_str().to_string());
    let subject_spec = node_spec(EntityRef::from_input(args.s)?);
    let subject_kind = "element".to_string();
    let subject_iri = EntityRef {
        iri: subject_spec.iri.clone(),
        name: subject_spec.name.clone(),
        labels: subject_spec.labels.clone(),
    }
    .resolved_iri(&subject_kind);
    let old_value = ObjectValue::from_input(args.old)?;
    let old_iri = old_value.resolved_iri();
    let new_value = ObjectValue::from_input(args.new)?;
    let new_iri = new_value.resolved_iri();
    let prop_iri = predicate.iri().to_string();
    let structural = structural_rel_for(&prop_iri);
    let locks = revision_fact_lock_requests(
        &subject_iri,
        &prop_iri,
        &old_iri,
        &new_iri,
        args.contradicts,
    );
    let mut txn = graph.start_txn().await?;
    acquire_fact_locks_in_txn(&mut txn, &locks).await?;
    let old_currents = find_current_pairs_txn(
        &mut txn,
        &subject_iri,
        &prop_iri,
        structural.as_deref(),
        &old_iri,
    )
    .await?;
    let old_current = select_revision_current(&old_currents, &layers, expected_fact_iri)
        .ok_or_else(|| {
            let selected = expected_fact_iri
                .map(|iri| format!("fact handle {iri}"))
                .unwrap_or_else(|| format!("fact ({subject_iri}, {prop_iri}, {old_iri})"));
            if old_currents.iter().any(|current| {
                expected_fact_iri.is_none_or(|iri| current.iri == iri) && current.layers.is_empty()
            }) && !layers.is_empty()
            {
                DomainError::Precondition(format!("{selected} is global; retry with scope: []"))
            } else {
                DomainError::Precondition(format!(
                    "cannot revise the selected memberships of non-current {selected}"
                ))
            }
        })?;
    if old_iri == new_iri {
        txn.rollback().await?;
        let target_iri = expected_fact_iri.unwrap_or(&old_current.iri);
        let target = json!({ "kind": "fact", "iri": target_iri });
        return Ok(RevisionResult {
            noop: true,
            scope: layers,
            episode: None,
            current_target: target.clone(),
            previous_target: target,
            fact: None,
            alternatives: Vec::new(),
            unify: Vec::new(),
        });
    }
    let result = async {
        let subject = merge_node_in_txn(
            &mut txn,
            &subject_spec,
            &subject_kind,
            &spike.clone().into_iter().collect::<Vec<_>>(),
        )
        .await?;
        let (new_object, new_object_is_literal) = merge_object_in_txn(&mut txn, new_value).await?;
        let (_, property_created, _) = ensure_property_in_txn(&mut txn, &prop_iri).await?;
        let episode = create_episode_in_txn(&mut txn, "revise", args.reason.as_deref()).await?;
        apply_node_memberships_txn(&mut txn, &subject, &layers).await?;
        apply_node_memberships_txn(&mut txn, &new_object, &layers).await?;
        change_fact_memberships_txn(
            &mut txn,
            &old_current,
            &layers,
            &episode,
            args.reason.as_deref(),
        )
        .await?;
        let new_text = fact_text(&subject.name, &subject.iri, &prop_iri, &new_object);
        let (new_relationship_iri, _) = ensure_relation_txn(
            &mut txn,
            &RelationWrite {
                rel_type: structural.as_deref().unwrap_or("ASSERTS"),
                s: &subject.iri,
                o: &new_object.iri,
                prop_iri: &prop_iri,
                layers: &layers,
                episode: &episode,
                reason: args.reason.as_deref(),
                fact_text: &new_text,
            },
        )
        .await?;
        let supersedes_text = format!("{} SUPERSEDES {old_iri}", new_object.iri);
        ensure_relation_txn(
            &mut txn,
            &RelationWrite {
                rel_type: "SUPERSEDES",
                s: &new_object.iri,
                o: &old_iri,
                prop_iri: "mindreader:property/SUPERSEDES",
                layers: &layers,
                episode: &episode,
                reason: args.reason.as_deref(),
                fact_text: &supersedes_text,
            },
        )
        .await?;
        let conflicts = find_conflicts_txn(
            &mut txn,
            &subject.iri,
            &prop_iri,
            structural.as_deref(),
            &new_object.iri,
            &layers,
        )
        .await?;
        if args.contradicts {
            ensure_contradictions_txn(&mut txn, &new_object.iri, &conflicts, &layers, &episode)
                .await?;
        }
        let subject_json = refreshed_node_json_txn(&mut txn, &subject.iri).await?;
        let new_json = refreshed_node_json_txn(&mut txn, &new_object.iri).await?;
        let mut created_iris = Vec::new();
        if subject.created {
            created_iris.push(subject.iri.clone());
        }
        if new_object.created && !new_object_is_literal {
            created_iris.push(new_object.iri.clone());
        }
        if property_created {
            created_iris.push(prop_iri.clone());
        }
        let merge_suggestions = merge_suggestions_in_txn(&mut txn, &created_iris, &layers).await?;
        Ok::<_, Error>((
            episode,
            subject_json,
            new_json,
            new_relationship_iri,
            conflicts,
            args.contradicts,
            merge_suggestions,
        ))
    }
    .await;
    let (episode, subject, new, relationship_iri, alternatives, contradicts, merge_suggestions) =
        match result {
            Ok(value) => {
                txn.commit()
                    .await
                    .map_err(|source| Error::AmbiguousCommit {
                        operation: "revise",
                        source,
                    })?;
                value
            }
            Err(error) => {
                let _ = txn.rollback().await;
                return Err(error);
            }
        };
    let current_target = json!({ "kind": "fact", "iri": relationship_iri });
    let previous_target = json!({
        "kind": "fact",
        "iri": expected_fact_iri.unwrap_or(&old_current.iri),
    });
    let conflicts = if contradicts {
        Value::Array(alternatives.clone())
    } else {
        json!([])
    };
    let fact = json!({
            "target": current_target.clone(),
            "s": subject,
            "p": prop_iri,
            "o": new,
            "scope": layers.clone(),
            "spike": spike,
            "conflicts": conflicts,
    });
    Ok(RevisionResult {
        noop: false,
        scope: layers,
        episode: Some(episode),
        current_target,
        previous_target,
        fact: Some(fact),
        alternatives,
        unify: merge_suggestions,
    })
}

/// Retry wrapper around one soft-withdrawal attempt.
async fn apply_withdrawal(
    graph: &Graph,
    args: WithdrawalPlan,
    expected_fact_iri: Option<&str>,
) -> Result<WithdrawalResult> {
    for attempt in 0..3_u64 {
        match apply_withdrawal_once(graph, args.clone(), expected_fact_iri).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

/// Soft-withdraw selected memberships of a fact, predicate, or subject slice.
async fn apply_withdrawal_once(
    graph: &Graph,
    args: WithdrawalPlan,
    expected_fact_iri: Option<&str>,
) -> Result<WithdrawalResult> {
    let layers = normalize_layers(args.layers)?;
    let scope = WithdrawalScope::parse(&args.target.kind)?;
    let subject = EntityRef::from_input(args.target.s)?;
    let subject_iri = subject.resolved_iri("element");
    let predicate = args
        .target
        .p
        .map(PredicateRef::parse)
        .transpose()?
        .map(|predicate| predicate.iri().to_string());
    if let Some(predicate) = &predicate {
        reject_system_owned_predicate(predicate)?;
    }
    let object_iri = args
        .target
        .o
        .map(ObjectValue::from_input)
        .transpose()?
        .map(|object| object.resolved_iri());
    match scope {
        WithdrawalScope::Fact if predicate.is_none() || object_iri.is_none() => {
            return Err(DomainError::InvalidInput(
                "fact withdrawal requires target.p and target.o".into(),
            )
            .into())
        }
        WithdrawalScope::Predicate if predicate.is_none() || object_iri.is_some() => {
            return Err(DomainError::InvalidInput(
                "predicate withdrawal requires target.p and forbids target.o".into(),
            )
            .into())
        }
        WithdrawalScope::Subject if predicate.is_some() || object_iri.is_some() => {
            return Err(DomainError::InvalidInput(
                "subject withdrawal forbids target.p and target.o".into(),
            )
            .into())
        }
        _ => {}
    }
    let protected = SCHEMA_STRUCTURAL_RELS
        .iter()
        .chain(SYSTEM_OWNED_RELS)
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    let locks = withdrawal_fact_lock_requests(&subject_iri, predicate.as_deref());
    let mut txn = graph.start_txn().await?;
    acquire_fact_locks_in_txn(&mut txn, &locks).await?;
    let rows = match scope {
        WithdrawalScope::Subject => {
            fetch_all_txn(
                &mut txn,
                query(
                    r#"
                    MATCH (s:Entity {iri: $s})-[r]->(o:Entity)
                    WHERE r.validTo IS NULL
                      AND NOT type(r) IN $protected
                      AND NOT s:Class AND NOT s:Property
                      AND NOT o:Class AND NOT o:Property
                      AND (size(s.layers) = 0
                           OR any(layer IN s.layers WHERE layer IN $layers))
                      AND (size(r.layers) = 0
                           OR any(layer IN r.layers WHERE layer IN $layers))
                      AND (size(o.layers) = 0
                           OR any(layer IN o.layers WHERE layer IN $layers))
                    RETURN id(r) AS rid, r.iri AS iri, r.layers AS layers
                    "#,
                )
                .param("s", subject_iri.clone())
                .param("protected", protected)
                .param("layers", layers.clone()),
            )
            .await?
        }
        WithdrawalScope::Fact | WithdrawalScope::Predicate => {
            let predicate = predicate.as_deref().expect("validated predicate");
            let structural = structural_rel_for(predicate);
            let rel_clause = structural
                .as_deref()
                .map(safe_rel)
                .transpose()?
                .map(|rel| format!(":{rel}"));
            let object_clause = object_iri
                .as_ref()
                .map(|_| "AND o.iri = $o")
                .unwrap_or_default();
            let target_clause = expected_fact_iri
                .map(|_| "AND r.iri = $targetIri")
                .unwrap_or_default();
            let cypher = if let Some(rel) = rel_clause {
                format!(
                    "MATCH (s:Entity {{iri: $s}})-[r{rel}]->(o:Entity) \
                     WHERE r.validTo IS NULL {object_clause} {target_clause} \
                       AND (size(s.layers) = 0 OR any(layer IN s.layers WHERE layer IN $layers)) \
                       AND (size(r.layers) = 0 OR any(layer IN r.layers WHERE layer IN $layers)) \
                       AND (size(o.layers) = 0 OR any(layer IN o.layers WHERE layer IN $layers)) \
                     RETURN id(r) AS rid, r.iri AS iri, r.layers AS layers"
                )
            } else {
                format!(
                    "MATCH (s:Entity {{iri: $s}})-[r:ASSERTS]->(o:Entity) \
                     WHERE r.validTo IS NULL AND r.propertyIri = $p {object_clause} {target_clause} \
                       AND (size(s.layers) = 0 OR any(layer IN s.layers WHERE layer IN $layers)) \
                       AND (size(r.layers) = 0 OR any(layer IN r.layers WHERE layer IN $layers)) \
                       AND (size(o.layers) = 0 OR any(layer IN o.layers WHERE layer IN $layers)) \
                     RETURN id(r) AS rid, r.iri AS iri, r.layers AS layers"
                )
            };
            let mut q = query(&cypher)
                .param("s", subject_iri.clone())
                .param("p", predicate.to_string())
                .param("layers", layers.clone());
            if let Some(object) = &object_iri {
                q = q.param("o", object.clone());
            }
            if let Some(target_iri) = expected_fact_iri {
                q = q.param("targetIri", target_iri.to_string());
            }
            fetch_all_txn(&mut txn, q).await?
        }
    };
    let currents = rows
        .into_iter()
        .map(|row| {
            Ok(CurrentFact {
                rel_id: row.get("rid")?,
                iri: row.get("iri")?,
                layers: row.get("layers")?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let changes = plan_fact_membership_changes(&currents, &layers);
    let withdrawn_targets = changes
        .iter()
        .filter_map(|change| {
            currents
                .iter()
                .find(|current| current.rel_id == change.rel_id)
                .map(|current| json!({ "kind": "fact", "iri": current.iri }))
        })
        .collect::<Vec<_>>();
    if changes.is_empty() {
        if !currents.is_empty()
            && currents.iter().all(|current| current.layers.is_empty())
            && !layers.is_empty()
        {
            txn.rollback().await?;
            return Err(DomainError::Precondition(
                "fact target is global; retry with scope: []".into(),
            )
            .into());
        }
        txn.rollback().await?;
        return Ok(WithdrawalResult {
            scope: layers,
            episode: None,
            withdrawn_targets: Vec::new(),
            reason: args.reason,
        });
    }
    let episode = create_episode_in_txn(&mut txn, "withdraw", args.reason.as_deref()).await?;
    change_fact_memberships_batch_txn(&mut txn, &changes, &episode, args.reason.as_deref()).await?;
    txn.commit()
        .await
        .map_err(|source| Error::AmbiguousCommit {
            operation: "withdraw",
            source,
        })?;
    Ok(WithdrawalResult {
        scope: layers,
        episode: Some(episode),
        withdrawn_targets,
        reason: args.reason,
    })
}

/// True when the node is a Class or Property catalog record (always global).
fn schema_node_labels(labels: &[String]) -> bool {
    labels
        .iter()
        .any(|label| label == "Class" || label == "Property")
}

/// Force catalog nodes to global memberships regardless of the call `scope`.
fn schema_node_scope<'a>(labels: &[String], scope: &'a [String]) -> &'a [String] {
    if schema_node_labels(labels) {
        &[]
    } else {
        scope
    }
}

/// Force schema-definition edges (`INSTANCE_OF`, `SUBCLASS_OF`, …) to stay global.
fn schema_edge_scope<'a>(
    structural: Option<&str>,
    subject_labels: &[String],
    object_labels: &[String],
    scope: &'a [String],
) -> &'a [String] {
    match structural {
        Some("INSTANCE_OF") if schema_node_labels(object_labels) => &[],
        Some("SUBCLASS_OF" | "SUBPROPERTY_OF") => &[],
        Some("DOMAIN" | "RANGE") if schema_node_labels(subject_labels) => &[],
        _ => scope,
    }
}

/// Accept only pasteable `node` or `fact` handles with a non-empty IRI.
fn validate_target(target: &TargetArgs) -> Result<()> {
    if !matches!(target.kind.as_str(), "node" | "fact") {
        return Err(DomainError::InvalidInput("target.kind must be node or fact".into()).into());
    }
    if target.iri.trim().is_empty() {
        return Err(DomainError::InvalidInput("target.iri cannot be empty".into()).into());
    }
    Ok(())
}

/// Endpoint closure: a global fact needs global endpoints; a named fact needs global or covering endpoints.
fn membership_allows(record: &[String], required: &[String]) -> bool {
    if required.is_empty() {
        return record.is_empty();
    }
    record.is_empty() || required.iter().all(|layer| record.contains(layer))
}

/// Input-ordered node lookup for `recall` `iris`; misses stay `found: false`.
const RECALL_IRI_NODES_QUERY: &str = r#"
UNWIND range(0, size($iris) - 1) AS inputIndex
WITH inputIndex, $iris[inputIndex] AS iri
OPTIONAL MATCH (n:Entity {iri: iri})
WHERE size(n.layers) = 0
   OR any(layer IN n.layers WHERE layer IN $layers)
RETURN inputIndex, iri, n IS NOT NULL AS found, n
ORDER BY inputIndex ASC
"#;

/// Incident current facts for each found IRI, bounded per lookup before hops=1 copies them up.
const RECALL_IRI_FACTS_QUERY: &str = r#"
UNWIND range(0, size($iris) - 1) AS inputIndex
WITH inputIndex, $iris[inputIndex] AS iri
MATCH (root:Entity {iri: iri})
WHERE size(root.layers) = 0
   OR any(layer IN root.layers WHERE layer IN $layers)
CALL {
  WITH root
  MATCH (root)-[r]-(other:Entity)
  WHERE r.validTo IS NULL
    AND (size(r.layers) = 0
         OR any(layer IN r.layers WHERE layer IN $layers))
    AND (size(other.layers) = 0
         OR any(layer IN other.layers WHERE layer IN $layers))
  WITH r, startNode(r) AS s, endNode(r) AS o,
       r.propertyIri AS property
  ORDER BY s.iri ASC, property ASC, o.iri ASC, r.iri ASC
  LIMIT $limit
  RETURN s, r, o, property
}
RETURN inputIndex, s, r, o, property
ORDER BY inputIndex ASC, s.iri ASC, property ASC, o.iri ASC, r.iri ASC
"#;

/// `recall` `iris` path: ordered lookups plus a globally bounded fact set.
/// `recall` `iris`: input-ordered node lookups plus optional hops=1 incident facts.
pub async fn memory_recall_iris(
    graph: &Graph,
    iris: Vec<String>,
    scope: Vec<String>,
    hops: u32,
    fact_limit: u32,
) -> Result<Value> {
    let layers = normalize_layers(scope)?;
    if !(1..=20).contains(&iris.len()) {
        return Err(
            DomainError::InvalidInput("recall iris must contain 1..=20 node IRIs".into()).into(),
        );
    }
    if hops > 1 {
        return Err(DomainError::InvalidInput("recall hops must be 0 or 1".into()).into());
    }
    if !(1..=100).contains(&fact_limit) {
        return Err(DomainError::InvalidInput("recall limit must be 1..=100".into()).into());
    }
    let iris = iris
        .into_iter()
        .map(|value| value.trim().to_string())
        .collect::<Vec<_>>();
    let mut lookups = iris
        .iter()
        .map(|iri| json!({ "iri": iri, "found": false, "facts": [] }))
        .collect::<Vec<_>>();
    let mut nodes = Vec::with_capacity(iris.len());
    for row in fetch_all(
        graph,
        query(RECALL_IRI_NODES_QUERY)
            .param("iris", iris.clone())
            .param("layers", layers.clone()),
    )
    .await?
    {
        let index = usize::try_from(row.get::<i64>("inputIndex")?)
            .map_err(|_| operation_error!("recall returned a negative input index"))?;
        if !row.get::<bool>("found")? {
            continue;
        }
        let node = node_json(&row.get::<Node>("n")?)?;
        nodes.push(node.clone());
        let lookup = lookups
            .get_mut(index)
            .ok_or_else(|| operation_error!("recall returned an out-of-range input index"))?;
        lookup["found"] = json!(true);
        lookup["node"] = node;
    }

    let mut facts = Vec::new();
    let rows = fetch_all(
        graph,
        query(RECALL_IRI_FACTS_QUERY)
            .param("iris", iris)
            .param("layers", layers.clone())
            .param("limit", i64::from(fact_limit.saturating_add(1))),
    )
    .await?;
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for row in &rows {
        *counts.entry(row.get::<i64>("inputIndex")?).or_default() += 1;
    }
    let truncated = counts.values().any(|count| *count > fact_limit as usize);
    let mut kept: HashMap<i64, usize> = HashMap::new();
    for row in rows {
        let index = row.get::<i64>("inputIndex")?;
        let subject = row.get::<Node>("s")?;
        let relationship = row.get::<Relation>("r")?;
        let object = row.get::<Node>("o")?;
        let property = row.get::<String>("property")?;
        let taken = kept.entry(index).or_default();
        if *taken >= fact_limit as usize {
            continue;
        }
        *taken += 1;
        let subject_iri = subject.get::<String>("iri")?;
        let object_iri = object.get::<String>("iri")?;
        let relationship = rel_json(&relationship, &subject_iri, &object_iri)?;
        let memberships = relationship
            .get("scope")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .ok_or_else(|| operation_error!("serialized fact scope is not an array"))?;
        let fact = crate::graph::fact_envelope(
            endpoint_json(&subject)?,
            &property,
            endpoint_json(&object)?,
            &relationship,
            &memberships,
            None,
        )?;
        if let Some(lookup_facts) = lookups
            .get_mut(index as usize)
            .and_then(|lookup| lookup.get_mut("facts"))
            .and_then(Value::as_array_mut)
        {
            lookup_facts.push(fact.clone());
        }
        if hops == 1 {
            facts.push(fact);
        }
    }
    Ok(json!({
        "ok": true,
        "mode": "iris",
        "scope": layers,
        "lookups": lookups,
        "facts": facts,
        "nodes": nodes,
        "paths": [],
        "about": [],
        "truncated": truncated,
    }))
}

/// Variable-length walk query for `recall` `around` at the requested depth.
fn recall_around_query(depth: u32) -> String {
    format!(
        r#"
        MATCH path = (start:Entity {{iri: $from}})-[pathRels*1..{depth}]-(x:Entity)
        WHERE all(n IN nodes(path) WHERE
          size(n.layers) = 0
          OR any(layer IN n.layers WHERE layer IN $layers))
          AND all(pathRel IN relationships(path) WHERE
            type(pathRel) IN $rels AND pathRel.validTo IS NULL
            AND (size(pathRel.layers) = 0
                 OR any(layer IN pathRel.layers WHERE layer IN $layers)))
        UNWIND relationships(path) AS r
        WITH path, r,
             r.propertyIri AS property,
             length(path) AS distance,
             [node IN nodes(path) | node.iri] AS pathNodes,
             [pathRel IN relationships(path) | pathRel.iri] AS pathEdgeIris
        WHERE size($predicates) = 0 OR property IN $predicates
        ORDER BY r.iri ASC, distance ASC, pathNodes ASC, pathEdgeIris ASC
        WITH r, property, head(collect(path)) AS path,
             head(collect(pathNodes)) AS witnessNodes, min(distance) AS distance
        WITH distance, path, witnessNodes, startNode(r) AS s, r, endNode(r) AS o,
             property
        ORDER BY distance ASC, s.iri ASC, property ASC, o.iri ASC, r.iri ASC
        LIMIT $limit
        RETURN distance, path, witnessNodes AS pathNodes, s, r, o, property
        "#
    )
}

/// `recall` `around` path with predicate filtering before a deterministic fact limit.
pub async fn memory_recall_around(
    graph: &Graph,
    from: &str,
    scope: Vec<String>,
    predicates: Vec<String>,
    depth: u32,
    limit: u32,
) -> Result<Value> {
    use crate::domain::PredicateRef;
    if !(1..=3).contains(&depth) {
        return Err(DomainError::InvalidInput("recall depth must be 1..=3".into()).into());
    }
    if !(1..=100).contains(&limit) {
        return Err(DomainError::InvalidInput("recall limit must be 1..=100".into()).into());
    }
    let layers = normalize_layers(scope)?;
    let mut wanted = predicates
        .into_iter()
        .map(|value| PredicateRef::parse(value).map(|predicate| predicate.iri().to_string()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    wanted.sort();
    wanted.dedup();
    let start_row = fetch_one(
        graph,
        query(
            "MATCH (n:Entity {iri: $iri}) \
             WHERE size(n.layers) = 0 \
                OR any(layer IN n.layers WHERE layer IN $layers) \
             RETURN n",
        )
        .param("iri", from.to_string())
        .param("layers", layers.clone()),
    )
    .await?;
    let Some(start_row) = start_row else {
        return Ok(json!({
            "ok": true,
            "mode": "around",
            "scope": layers,
            "from": from,
            "facts": [],
            "nodes": [],
            "paths": [],
            "about": [],
            "lookups": [],
            "truncated": false,
        }));
    };
    let start: Node = start_row.get("n")?;
    let rows = fetch_all(
        graph,
        query(&recall_around_query(depth))
            .param("from", from.to_string())
            .param("layers", layers.clone())
            .param(
                "rels",
                FIXED_RELS
                    .iter()
                    .map(|relationship| (*relationship).to_string())
                    .collect::<Vec<_>>(),
            )
            .param("predicates", wanted)
            .param("limit", i64::from(limit.saturating_add(1))),
    )
    .await?;
    let truncated = rows.len() > limit as usize;
    let mut facts = Vec::new();
    let mut paths = Vec::new();
    let mut nodes_by_iri = HashMap::new();
    nodes_by_iri.insert(from.to_string(), node_json(&start)?);
    for row in rows.into_iter().take(limit as usize) {
        let path = row.get::<Path>("path")?;
        let path_nodes = row.get::<Vec<String>>("pathNodes")?;
        let subject = row.get::<Node>("s")?;
        let relationship = row.get::<Relation>("r")?;
        let object = row.get::<Node>("o")?;
        let property = row.get::<String>("property")?;
        let subject_iri = subject.get::<String>("iri")?;
        let object_iri = object.get::<String>("iri")?;
        nodes_by_iri.insert(subject_iri.clone(), node_json(&subject)?);
        nodes_by_iri.insert(object_iri.clone(), node_json(&object)?);
        let (decoded_nodes, path_edges, _) = path_to_json(&path)?;
        for node in decoded_nodes {
            if let Some(iri) = node.get("iri").and_then(Value::as_str) {
                nodes_by_iri.insert(iri.to_string(), node);
            }
        }
        paths.push(json!({ "nodes": path_nodes, "edges": path_edges }));
        let relationship = rel_json(&relationship, &subject_iri, &object_iri)?;
        let memberships = relationship
            .get("scope")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .ok_or_else(|| operation_error!("serialized fact scope is not an array"))?;
        facts.push(crate::graph::fact_envelope(
            endpoint_json(&subject)?,
            &property,
            endpoint_json(&object)?,
            &relationship,
            &memberships,
            None,
        )?);
    }
    let mut nodes = nodes_by_iri.into_iter().collect::<Vec<_>>();
    nodes.sort_by(|left, right| left.0.cmp(&right.0));
    let nodes = nodes.into_iter().map(|(_, node)| node).collect::<Vec<_>>();
    Ok(json!({
        "ok": true,
        "mode": "around",
        "scope": layers,
        "from": from,
        "facts": facts,
        "nodes": nodes,
        "paths": paths,
        "about": [],
        "lookups": [],
        "truncated": truncated,
    }))
}

/// Current and `validTo` facts that share a fact handle's subject and predicate.
const RECALL_HISTORY_FACT_QUERY: &str = r#"
MATCH (s:Entity)-[anchor]->(o:Entity)
WHERE anchor.iri = $iri
  AND (size(s.layers) = 0
       OR any(layer IN s.layers WHERE layer IN $layers))
  AND (size(anchor.layers) = 0
       OR any(layer IN anchor.layers WHERE layer IN $layers))
  AND (size(o.layers) = 0
       OR any(layer IN o.layers WHERE layer IN $layers))
WITH s, anchor.propertyIri AS property
MATCH (s)-[r]->(other:Entity)
WHERE r.propertyIri = property
  AND NOT type(r) IN $protected
  AND (size(r.layers) = 0
       OR any(layer IN r.layers WHERE layer IN $layers))
  AND (size(other.layers) = 0
       OR any(layer IN other.layers WHERE layer IN $layers))
WITH s, r, other, property, r.validTo IS NULL AS current, toString(r.validTo) AS validTo
ORDER BY current DESC, validTo DESC, r.iri ASC
LIMIT $limit
RETURN s, r, other AS o, property, current, validTo
"#;

/// Current and historical incident facts for a node handle (system-owned edges excluded).
const RECALL_HISTORY_NODE_QUERY: &str = r#"
MATCH (n:Entity {iri: $iri})
WHERE size(n.layers) = 0
   OR any(layer IN n.layers WHERE layer IN $layers)
MATCH (n)-[r]-(other:Entity)
WHERE NOT type(r) IN $protected
  AND (size(r.layers) = 0
       OR any(layer IN r.layers WHERE layer IN $layers))
  AND (size(other.layers) = 0
       OR any(layer IN other.layers WHERE layer IN $layers))
WITH startNode(r) AS s, r, endNode(r) AS o,
     r.propertyIri AS property,
     r.validTo IS NULL AS current, toString(r.validTo) AS validTo
ORDER BY current DESC, validTo DESC, r.iri ASC
LIMIT $limit
RETURN s, r, o, property, current, validTo
"#;

/// `recall` `history` path: current and superseded facts for one handle.
pub async fn memory_recall_history(
    graph: &Graph,
    iri: &str,
    scope: Vec<String>,
    limit: u32,
) -> Result<Value> {
    if !(1..=100).contains(&limit) {
        return Err(DomainError::InvalidInput("recall limit must be 1..=100".into()).into());
    }
    let layers = normalize_layers(scope)?;
    let protected = SYSTEM_OWNED_RELS
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    let fact_iri = iri.starts_with("mindreader:relationship/");
    let found_query = if fact_iri {
        "MATCH ()-[r]->() WHERE r.iri = $iri \
         AND (size(r.layers) = 0 \
              OR any(layer IN r.layers WHERE layer IN $layers)) \
         RETURN count(r) AS n"
    } else {
        "MATCH (n:Entity {iri: $iri}) \
         WHERE size(n.layers) = 0 \
            OR any(layer IN n.layers WHERE layer IN $layers) \
         RETURN count(n) AS n"
    };
    let found = fetch_one(
        graph,
        query(found_query)
            .param("iri", iri.to_string())
            .param("layers", layers.clone()),
    )
    .await?
    .ok_or_else(|| operation_error!("history existence query returned no row"))?
    .get::<i64>("n")?
        > 0;
    if !found {
        return Ok(json!({
            "ok": true,
            "mode": "history",
            "scope": layers,
            "from": iri,
            "facts": [],
            "nodes": [],
            "paths": [],
            "about": [],
            "lookups": [{ "iri": iri, "found": false, "facts": [] }],
            "truncated": false,
        }));
    }
    let cypher = if fact_iri {
        RECALL_HISTORY_FACT_QUERY
    } else {
        RECALL_HISTORY_NODE_QUERY
    };
    let rows = fetch_all(
        graph,
        query(cypher)
            .param("iri", iri.to_string())
            .param("layers", layers.clone())
            .param("protected", protected)
            .param("limit", i64::from(limit.saturating_add(1))),
    )
    .await?;
    let truncated = rows.len() > limit as usize;
    let mut facts = Vec::new();
    let mut nodes_by_iri = std::collections::HashMap::new();
    if !fact_iri {
        if let Some(row) = fetch_one(
            graph,
            query(
                "MATCH (n:Entity {iri: $iri}) \
                 WHERE size(n.layers) = 0 \
                    OR any(layer IN n.layers WHERE layer IN $layers) \
                 RETURN n",
            )
            .param("iri", iri.to_string())
            .param("layers", layers.clone()),
        )
        .await?
        {
            let node = row.get::<Node>("n")?;
            nodes_by_iri.insert(iri.to_string(), node_json(&node)?);
        }
    }
    for row in rows.into_iter().take(limit as usize) {
        let subject = row.get::<Node>("s")?;
        let relationship = row.get::<Relation>("r")?;
        let object = row.get::<Node>("o")?;
        let property = row.get::<String>("property")?;
        let current = row.get::<bool>("current")?;
        let subject_iri = subject.get::<String>("iri")?;
        let object_iri = object.get::<String>("iri")?;
        nodes_by_iri.insert(subject_iri.clone(), node_json(&subject)?);
        nodes_by_iri.insert(object_iri.clone(), node_json(&object)?);
        let relationship = rel_json(&relationship, &subject_iri, &object_iri)?;
        let memberships = relationship
            .get("scope")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .ok_or_else(|| operation_error!("serialized fact scope is not an array"))?;
        let mut fact = crate::graph::fact_envelope(
            endpoint_json(&subject)?,
            &property,
            endpoint_json(&object)?,
            &relationship,
            &memberships,
            None,
        )?;
        fact["current"] = json!(current);
        if let Ok(valid_to) = row.get::<String>("validTo") {
            if !valid_to.is_empty() && valid_to != "null" {
                fact["validTo"] = json!(valid_to);
            }
        }
        facts.push(fact);
    }
    let mut nodes = nodes_by_iri.into_iter().collect::<Vec<_>>();
    nodes.sort_by(|left, right| left.0.cmp(&right.0));
    let nodes = nodes.into_iter().map(|(_, node)| node).collect::<Vec<_>>();
    Ok(json!({
        "ok": true,
        "mode": "history",
        "scope": layers,
        "from": iri,
        "facts": facts.clone(),
        "nodes": nodes,
        "paths": [],
        "about": [],
        "lookups": [{ "iri": iri, "found": true, "facts": facts }],
        "truncated": truncated,
    }))
}

/// Load a visible current fact handle, distinguishing global-vs-named scope misses.
async fn load_current_fact(
    graph: &Graph,
    iri: &str,
    layers: &[String],
) -> Result<(EntityInput, String, ObjectInput)> {
    let row = fetch_one(
        graph,
        query(
            r#"
            MATCH (s:Entity)-[r]->(o:Entity)
            WHERE r.iri = $iri AND r.validTo IS NULL
              AND (size(s.layers) = 0
                   OR any(layer IN s.layers WHERE layer IN $layers))
              AND (size(r.layers) = 0
                   OR any(layer IN r.layers WHERE layer IN $layers))
              AND (size(o.layers) = 0
                   OR any(layer IN o.layers WHERE layer IN $layers))
            RETURN s, r, o, r.propertyIri AS p
            "#,
        )
        .param("iri", iri.to_string())
        .param("layers", layers.to_vec()),
    )
    .await?;
    let row = match row {
        Some(row) => row,
        None => {
            let existing = fetch_one(
                graph,
                query(
                    "MATCH ()-[r]->() WHERE r.iri = $iri AND r.validTo IS NULL \
                     RETURN r.layers AS layers",
                )
                .param("iri", iri.to_string()),
            )
            .await?;
            return Err(if let Some(existing) = existing {
                let stored = existing.get::<Vec<String>>("layers")?;
                if stored.is_empty() && !layers.is_empty() {
                    DomainError::Precondition(format!(
                        "fact target {iri} is global; retry with scope: []"
                    ))
                } else {
                    DomainError::Precondition(
                        "fact target is missing, hidden, or historical".into(),
                    )
                }
            } else {
                DomainError::Precondition("fact target is missing, hidden, or historical".into())
            }
            .into());
        }
    };
    let subject: Node = row.get("s")?;
    let object: Node = row.get("o")?;
    let property: String = row.get("p")?;
    Ok((
        entity_input_from_node(&subject)?,
        crate::graph::local_predicate_name(&property),
        object_input_from_node(&object)?,
    ))
}

/// Rebuild a write-shaped subject from a stored node so revise/withdraw can re-MERGE it.
fn entity_input_from_node(node: &Node) -> Result<EntityInput> {
    Ok(EntityInput {
        kind: "node".into(),
        iri: Some(node.get::<String>("iri")?),
        name: node.get::<String>("name").ok(),
        labels: Vec::new(),
    })
}

/// Rebuild a tagged `node` or `literal` object from a stored endpoint.
fn object_input_from_node(node: &Node) -> Result<ObjectInput> {
    let labels: Vec<String> = node
        .labels()
        .into_iter()
        .filter(|label| *label != "Entity")
        .map(str::to_string)
        .collect();
    if labels.iter().any(|label| label == "Literal") {
        return Ok(ObjectInput {
            kind: "literal".into(),
            iri: None,
            name: None,
            labels: Vec::new(),
            value: Some(node.get::<String>("value")?),
            datatype: Some(node.get::<String>("datatype")?),
        });
    }
    Ok(ObjectInput {
        kind: "node".into(),
        iri: Some(node.get::<String>("iri")?),
        name: None,
        labels: Vec::new(),
        value: None,
        datatype: None,
    })
}

/// MCP `revise`: resolve a fact IRI, then membership-selective SUPERSEDES.
pub async fn memory_revise(graph: &Graph, args: ReviseArgs) -> Result<Value> {
    if args.target.kind != "fact" {
        return Err(DomainError::InvalidInput("revise target.kind must be fact".into()).into());
    }
    let layers = normalize_layers(args.scope)?;
    let (s, p, old) = load_current_fact(graph, &args.target.iri, &layers).await?;
    let result = apply_revision(
        graph,
        RevisionPlan {
            s,
            p,
            old,
            new: args.new,
            layers,
            spike: args.spike,
            contradicts: args.contradicts,
            reason: args.reason,
        },
        Some(&args.target.iri),
    )
    .await?;
    let current_target = result.current_target;
    let previous_target = result.previous_target;
    let facts = result.fact.clone().into_iter().collect::<Vec<_>>();
    let fact = result.fact.unwrap_or(Value::Null);
    let alternatives = if result.alternatives.is_empty() {
        Vec::new()
    } else {
        vec![json!({
            "target": current_target.clone(),
            "conflicts": result.alternatives,
        })]
    };
    let episode = result.episode.map_or(
        Value::Null,
        |episode| json!({ "iri": episode.iri, "at": episode.at, "tool": episode.tool }),
    );
    finish_mutation(
        json!({
            "ok": true,
            "scope": result.scope,
            "noop": result.noop,
            "episode": episode,
            "target": current_target.clone(),
            "previousTarget": previous_target.clone(),
            "fact": fact,
            "review": {
                "unify": result.unify,
                "alternatives": alternatives,
            },
        }),
        &facts,
        &[],
        Some(current_target),
        Some(previous_target),
    )
}

/// MCP `withdraw`: soft-withdraw by fact IRI or subject (optional predicate).
pub async fn memory_withdraw(graph: &Graph, args: WithdrawArgs) -> Result<Value> {
    let has_target = args.target.is_some();
    let has_subject = args.subject.is_some();
    if has_target == has_subject {
        return Err(DomainError::InvalidInput(
            "withdraw requires exactly one of target or subject".into(),
        )
        .into());
    }
    let layers = normalize_layers(args.scope.clone())?;
    let withdrawal = if let Some(ref target) = args.target {
        if target.kind != "fact" {
            return Err(
                DomainError::InvalidInput("withdraw target.kind must be fact".into()).into(),
            );
        }
        let (s, p, o) = load_current_fact(graph, &target.iri, &layers).await?;
        WithdrawalPlan {
            target: WithdrawalSelector {
                kind: "fact".into(),
                s,
                p: Some(p),
                o: Some(o),
            },
            layers,
            reason: args.reason,
        }
    } else {
        let subject = args.subject.expect("validated subject");
        WithdrawalPlan {
            target: WithdrawalSelector {
                kind: if args.p.is_some() {
                    "predicate".into()
                } else {
                    "subject".into()
                },
                s: subject,
                p: args.p,
                o: None,
            },
            layers,
            reason: args.reason,
        }
    };
    let expected_fact_iri = args.target.as_ref().map(|target| target.iri.as_str());
    let result = apply_withdrawal(graph, withdrawal, expected_fact_iri).await?;
    let withdrawn = result.withdrawn_targets.len();
    let withdrawn_facts = result
        .withdrawn_targets
        .iter()
        .map(|target| json!({ "target": target }))
        .collect::<Vec<_>>();
    let episode = result.episode.map_or(
        Value::Null,
        |episode| json!({ "iri": episode.iri, "at": episode.at, "tool": episode.tool }),
    );
    finish_mutation(
        json!({
            "ok": true,
            "scope": result.scope,
            "noop": withdrawn == 0,
            "episode": episode,
            "withdrawn": withdrawn,
            "withdrawnTargets": result.withdrawn_targets,
            "reason": result.reason,
        }),
        &withdrawn_facts,
        &[],
        None,
        None,
    )
}

/// Map `strengthen`/`weaken` to a single +1 or -1 weight step.
fn judge_delta(mode: &str) -> Result<i64> {
    match mode {
        "strengthen" => Ok(1),
        "weaken" => Ok(-1),
        _ => {
            Err(DomainError::InvalidInput("judge mode must be strengthen or weaken".into()).into())
        }
    }
}

/// Require 1..=20 unique rateable targets before the judge transaction starts.
fn validate_judge_args(args: &JudgeArgs) -> Result<()> {
    if args.ratings.is_empty() || args.ratings.len() > MAX_WRITE_FACTS {
        return Err(DomainError::InvalidInput(format!(
            "judge ratings must contain between 1 and {MAX_WRITE_FACTS} items"
        ))
        .into());
    }
    validate_layer_ids(args.scope.clone())?;
    let mut seen = HashSet::new();
    for rating in &args.ratings {
        validate_target(&rating.target)?;
        judge_delta(&rating.mode)?;
        if !seen.insert((rating.target.kind.clone(), rating.target.iri.clone())) {
            return Err(DomainError::InvalidInput(format!(
                "judge contains duplicate target {}:{}",
                rating.target.kind, rating.target.iri
            ))
            .into());
        }
    }
    Ok(())
}

/// Apply one ±1 rating to a visible current node or fact; saturate at i64 bounds.
async fn apply_judge_rating_txn(
    txn: &mut Txn,
    scope: &[String],
    rating: &JudgeRating,
) -> Result<(i64, i64)> {
    let delta = judge_delta(&rating.mode)?;
    let strengthen = delta > 0;
    let update = if rating.target.kind == "node" {
        fetch_one_txn(
            txn,
            query(
                r#"
                MATCH (target:Entity {iri: $iri})
                WHERE size(target.layers) = 0
                   OR any(layer IN target.layers WHERE layer IN $scope)
                WITH target, target.weight AS before
                WHERE ($strengthen AND before < $max) OR (NOT $strengthen AND before > $min)
                SET target.weight = CASE WHEN $strengthen THEN before + 1 ELSE before - 1 END
                RETURN before, target.weight AS after
                "#,
            )
            .param("iri", rating.target.iri.clone())
            .param("scope", scope.to_vec())
            .param("strengthen", strengthen)
            .param("max", i64::MAX)
            .param("min", i64::MIN),
        )
        .await?
    } else {
        fetch_one_txn(
            txn,
            query(
                r#"
                MATCH (s:Entity)-[target]->(o:Entity)
                WHERE target.iri = $iri AND target.validTo IS NULL
                  AND (size(s.layers) = 0 OR any(layer IN s.layers WHERE layer IN $scope))
                  AND (size(target.layers) = 0 OR any(layer IN target.layers WHERE layer IN $scope))
                  AND (size(o.layers) = 0 OR any(layer IN o.layers WHERE layer IN $scope))
                WITH target, target.weight AS before
                WHERE ($strengthen AND before < $max) OR (NOT $strengthen AND before > $min)
                SET target.weight = CASE WHEN $strengthen THEN before + 1 ELSE before - 1 END
                RETURN before, target.weight AS after
                "#,
            )
            .param("iri", rating.target.iri.clone())
            .param("scope", scope.to_vec())
            .param("strengthen", strengthen)
            .param("max", i64::MAX)
            .param("min", i64::MIN),
        )
        .await?
    };
    let update = update.ok_or_else(|| {
        DomainError::Precondition(format!(
            "judge target {}:{} is missing, hidden, historical, or at the weight boundary",
            rating.target.kind, rating.target.iri
        ))
    })?;
    let before: i64 = update.get("before")?;
    let after: i64 = update.get("after")?;
    Ok((before, after))
}

/// One judge transaction: lock, apply every ±1 rating, record one Episode, or roll back.
async fn memory_judge_once(graph: &Graph, args: JudgeArgs) -> Result<Value> {
    let scope = normalize_layers(args.scope)?;
    let locks = args
        .ratings
        .iter()
        .map(|rating| {
            (
                rating.target.iri.clone(),
                "mindreader:property/weight".into(),
                FACT_LOCK_SCOPE.into(),
            )
        })
        .collect::<Vec<_>>();
    let mut txn = graph.start_txn().await?;
    let result = async {
        acquire_fact_locks_in_txn(&mut txn, &locks).await?;
        let mut items = Vec::with_capacity(args.ratings.len());
        for (index, rating) in args.ratings.iter().enumerate() {
            let (before, after) = apply_judge_rating_txn(&mut txn, &scope, rating).await?;
            items.push(json!({
                "index": index,
                "target": rating.target,
                "mode": rating.mode,
                "delta": judge_delta(&rating.mode)?,
                "before": before,
                "after": after,
                "status": "changed",
            }));
        }
        let episode = create_episode_in_txn(&mut txn, "judge", None).await?;
        let target_iris = args
            .ratings
            .iter()
            .map(|rating| rating.target.iri.clone())
            .collect::<Vec<_>>();
        let target_kinds = args
            .ratings
            .iter()
            .map(|rating| rating.target.kind.clone())
            .collect::<Vec<_>>();
        let modes = args
            .ratings
            .iter()
            .map(|rating| rating.mode.clone())
            .collect::<Vec<_>>();
        txn.run(
            query(
                "MATCH (e:Entity:Episode {iri: $episode}) \
                 SET e.batchSize = $batchSize, e.targetIris = $targetIris, \
                     e.targetKinds = $targetKinds, e.modes = $modes",
            )
            .param("episode", episode.iri.clone())
            .param("batchSize", items.len() as i64)
            .param("targetIris", target_iris)
            .param("targetKinds", target_kinds)
            .param("modes", modes),
        )
        .await?;
        for rating in &args.ratings {
            let target_match = if rating.target.kind == "node" {
                "MATCH (target:Entity {iri: $iri})"
            } else {
                "MATCH ()-[target]->() WHERE target.iri = $iri"
            };
            txn.run(
                query(&format!(
                    "{target_match} SET target.weightUpdatedAt = datetime(), \
                     target.judgmentEpisodeId = $episode"
                ))
                .param("iri", rating.target.iri.clone())
                .param("episode", episode.iri.clone()),
            )
            .await?;
        }
        Ok::<_, Error>((episode, items))
    }
    .await;
    let (episode, items) = match result {
        Ok(value) => {
            txn.commit()
                .await
                .map_err(|source| Error::AmbiguousCommit {
                    operation: "judge",
                    source,
                })?;
            value
        }
        Err(error) => {
            let _ = txn.rollback().await;
            return Err(error);
        }
    };
    let judge_facts = items
        .iter()
        .filter(|item| item.pointer("/target/kind").and_then(Value::as_str) == Some("fact"))
        .map(|item| json!({ "target": item.get("target") }))
        .collect::<Vec<_>>();
    let judge_nodes = items
        .iter()
        .filter(|item| item.pointer("/target/kind").and_then(Value::as_str) == Some("node"))
        .filter_map(|item| item.get("target").cloned())
        .collect::<Vec<_>>();
    finish_mutation(
        json!({
            "ok": true,
            "scope": scope,
            "noop": false,
            "episode": { "iri": episode.iri, "at": episode.at, "tool": episode.tool },
            "summary": { "requested": items.len(), "changed": items.len(), "noop": 0 },
            "items": items,
        }),
        &judge_facts,
        &judge_nodes,
        None,
        None,
    )
}

/// Apply 1–20 explicit ratings atomically under one `judge` Episode.
pub async fn memory_judge(graph: &Graph, args: JudgeArgs) -> Result<Value> {
    validate_judge_args(&args)?;
    for attempt in 0..3_u64 {
        match memory_judge_once(graph, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

/// Validated membership edit after unique-target and add/remove overlap checks.
#[derive(Clone)]
struct NormalizedPlaceEdit {
    index: usize,
    target: TargetArgs,
    add: Vec<String>,
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
fn normalize_place_args(args: PlaceArgs) -> Result<(Vec<String>, Vec<NormalizedPlaceEdit>)> {
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
        if !membership_allows(&s_layers, &r_layers) {
            let s_labels = row.get::<Vec<String>>("sLabels")?;
            return Err(DomainError::Precondition(format!(
                "place final state would expose fact {r_iri} while endpoint {s_iri} ({}) is hidden",
                hidden_endpoint_kind(&s_iri, &s_labels)
            ))
            .into());
        }
        if !membership_allows(&o_layers, &r_layers) {
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

/// Apply 1–20 membership edits atomically against their combined final state.
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

/// Class/Property catalog used by `recall` (global schema-as-data).
pub async fn list_schema_catalog(graph: &Graph, kind: &str) -> Result<Value> {
    let kind = kind.trim().to_ascii_lowercase();
    if kind != "class" && kind != "property" {
        return Err(DomainError::InvalidInput("kind must be class or property".into()).into());
    }
    let label = safe_label(if kind == "class" { "Class" } else { "Property" })?;
    let rows = fetch_all(
        graph,
        query(&format!(
            "MATCH (n:Entity:{label}) \
             RETURN n.iri AS iri, n.name AS name, n.stub AS stub, \
                    n.layers AS layers, n.weight AS weight \
             ORDER BY n.name, n.iri LIMIT 101"
        )),
    )
    .await?;
    let items = rows
        .into_iter()
        .map(|row| {
            let iri = row.get::<String>("iri")?;
            let name = row.get::<String>("name")?;
            let stub = row.get::<bool>("stub")?;
            let layers = row.get::<Vec<String>>("layers")?;
            let weight = row.get::<i64>("weight")?;
            if stub || !layers.is_empty() {
                return Err(operation_error!(
                    "schema catalog node {iri} must have stub=false and global scope"
                ));
            }
            Ok(json!({
                "kind": "node",
                "iri": iri.clone(),
                "name": name,
                "labels": [label.clone()],
                "scope": layers,
                "weight": weight,
                "stub": stub,
                "target": { "kind": "node", "iri": iri },
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "list": true,
        "kind": kind,
        "items": items,
        "noop": true,
        "episode": Value::Null,
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        effective_weight, merge_memberships, plan_fact_membership_changes, prepare_write_fact,
        recall_around_query, reject_system_owned_predicate, remove_memberships,
        revision_fact_lock_requests, select_revision_current, validate_judge_args,
        validate_write_args, withdrawal_fact_lock_requests, write_fact_lock_requests, CurrentFact,
        FactMembershipChange, JudgeArgs, JudgeRating, PlaceArgs, PlaceEdit, TargetArgs, WriteArgs,
        WriteFact, CONTRADICTS_PROPERTY, LAYERS_PROPERTY, MAX_WRITE_FACTS,
        PREDICATE_USAGE_PROPERTY, RECALL_IRI_FACTS_QUERY, RECALL_IRI_NODES_QUERY,
    };
    use crate::domain::{DomainError, EntityInput, ObjectInput};
    use crate::error::Error;
    use crate::graph::{fact_lock_specs, spike_rank};

    #[test]
    fn spike_rank_order() {
        assert!(spike_rank(Some("Knowledge")) > spike_rank(Some("Insight")));
        assert!(spike_rank(Some("Insight")) > spike_rank(Some("Pattern")));
        assert!(spike_rank(Some("Pattern")) > spike_rank(Some("Signal")));
        assert!(spike_rank(Some("Signal")) > spike_rank(None));
    }

    #[test]
    fn recall_queries_are_set_oriented_bounded_and_deterministic() {
        assert!(RECALL_IRI_NODES_QUERY.contains("UNWIND range(0, size($iris) - 1)"));
        assert!(RECALL_IRI_NODES_QUERY.contains("ORDER BY inputIndex ASC"));
        assert!(RECALL_IRI_FACTS_QUERY.contains("CALL {"));
        assert!(!RECALL_IRI_FACTS_QUERY.contains("collect("));
        assert!(RECALL_IRI_FACTS_QUERY.contains("ORDER BY inputIndex ASC"));
        assert!(RECALL_IRI_FACTS_QUERY.contains("LIMIT $limit"));

        let around = recall_around_query(3);
        let predicate_filter = around.find("property IN $predicates").unwrap();
        let limit = around.find("LIMIT $limit").unwrap();
        assert!(around.contains("pathRels*1..3"));
        assert!(predicate_filter < limit);
        assert!(around.contains("pathNodes ASC, pathEdgeIris ASC"));
        assert!(around.contains("head(collect(path)) AS path"));
        assert!(around.contains("RETURN distance, path, witnessNodes AS pathNodes"));
        assert!(
            around.contains("ORDER BY distance ASC, s.iri ASC, property ASC, o.iri ASC, r.iri ASC")
        );
    }

    #[test]
    fn membership_merge_honors_global_dominance() {
        assert_eq!(
            merge_memberships(&[], &["project:a".into()]),
            Vec::<String>::new()
        );
        assert_eq!(
            merge_memberships(&["project:a".into()], &[]),
            Vec::<String>::new()
        );
        assert_eq!(
            merge_memberships(&["project:b".into()], &["project:a".into()]),
            vec!["project:a".to_string(), "project:b".to_string()]
        );
    }

    #[test]
    fn fact_handle_selection_never_falls_through_to_a_reasserted_identity() {
        let replacement = CurrentFact {
            rel_id: 2,
            iri: "fact:new".into(),
            layers: vec!["project:a".into()],
        };
        assert!(select_revision_current(
            std::slice::from_ref(&replacement),
            &["project:a".into()],
            Some("fact:retired"),
        )
        .is_none());
        assert_eq!(
            select_revision_current(
                std::slice::from_ref(&replacement),
                &["project:a".into()],
                Some("fact:new"),
            )
            .map(|current| current.iri),
            Some("fact:new".into())
        );
    }

    #[test]
    fn judge_batch_prevalidates_modes_and_duplicate_targets() {
        let target = TargetArgs {
            kind: "fact".into(),
            iri: "mindreader:relationship/one".into(),
        };
        let valid = JudgeArgs {
            scope: vec!["project:x".into()],
            ratings: vec![JudgeRating {
                target: target.clone(),
                mode: "strengthen".into(),
            }],
        };
        assert!(validate_judge_args(&valid).is_ok());
        let duplicate = JudgeArgs {
            scope: valid.scope.clone(),
            ratings: vec![valid.ratings[0].clone(), valid.ratings[0].clone()],
        };
        assert!(validate_judge_args(&duplicate).is_err());
        let invalid_mode = JudgeArgs {
            scope: valid.scope,
            ratings: vec![JudgeRating {
                target,
                mode: "maybe".into(),
            }],
        };
        assert!(validate_judge_args(&invalid_mode).is_err());
    }

    #[test]
    fn place_batch_rejects_duplicates_and_normalizes_each_edit() {
        let target = TargetArgs {
            kind: "node".into(),
            iri: "mindreader:element/alice".into(),
        };
        let args = PlaceArgs {
            scope: vec!["project:x".into()],
            edits: vec![PlaceEdit {
                target: target.clone(),
                add: vec!["project:b".into(), "project:a".into(), "project:b".into()],
                remove: Vec::new(),
            }],
        };
        let (scope, edits) = super::normalize_place_args(args).expect("valid place batch");
        assert_eq!(scope, vec!["project:x"]);
        assert_eq!(edits[0].add, vec!["project:a", "project:b"]);

        let duplicate = PlaceArgs {
            scope: vec![],
            edits: vec![
                PlaceEdit {
                    target: target.clone(),
                    add: vec!["project:a".into()],
                    remove: Vec::new(),
                },
                PlaceEdit {
                    target,
                    add: vec!["project:b".into()],
                    remove: Vec::new(),
                },
            ],
        };
        assert!(super::normalize_place_args(duplicate).is_err());
    }

    #[test]
    fn selected_memberships_are_removed_without_globalizing_facts() {
        assert_eq!(
            remove_memberships(
                &["project:a".into(), "project:b".into()],
                &["project:a".into()]
            ),
            Some(vec!["project:b".to_string()])
        );
        assert_eq!(
            remove_memberships(&["project:a".into()], &["project:a".into()]),
            Some(Vec::new())
        );
        assert_eq!(remove_memberships(&[], &["project:a".into()]), None);
        assert_eq!(remove_memberships(&[], &[]), Some(Vec::new()));
    }

    #[test]
    fn write_and_revision_plan_all_known_locks_in_one_batch() {
        let write_locks = write_fact_lock_requests("subject", "property", "new", true);
        assert_eq!(write_locks.len(), 5);
        assert!(write_locks.contains(&("new".into(), CONTRADICTS_PROPERTY.into(), "@fact".into())));
        assert!(write_locks.contains(&(
            "property".into(),
            PREDICATE_USAGE_PROPERTY.into(),
            "@fact".into()
        )));

        let revision_locks = revision_fact_lock_requests("subject", "property", "old", "new", true);
        assert_eq!(revision_locks.len(), 6);
        assert!(revision_locks.contains(&("old".into(), LAYERS_PROPERTY.into(), "@fact".into())));
        assert!(revision_locks.contains(&(
            "new".into(),
            CONTRADICTS_PROPERTY.into(),
            "@fact".into()
        )));
    }

    #[test]
    fn withdrawal_plans_subject_and_predicate_guards_together() {
        assert_eq!(withdrawal_fact_lock_requests("subject", None).len(), 1);
        let locks = withdrawal_fact_lock_requests("subject", Some("property"));
        assert_eq!(locks.len(), 2);
        assert!(locks.contains(&(
            "property".into(),
            PREDICATE_USAGE_PROPERTY.into(),
            "@fact".into()
        )));
    }

    #[test]
    fn broad_withdrawal_plans_only_matching_named_memberships() {
        let currents = vec![
            current_fact(1, &[]),
            current_fact(2, &["project:a"]),
            current_fact(3, &["project:a", "project:b"]),
            current_fact(4, &["project:b"]),
        ];

        assert_eq!(
            plan_fact_membership_changes(&currents, &["project:a".into()]),
            vec![
                FactMembershipChange {
                    rel_id: 2,
                    remaining: Vec::new(),
                },
                FactMembershipChange {
                    rel_id: 3,
                    remaining: vec!["project:b".into()],
                },
            ]
        );
    }

    #[test]
    fn broad_global_withdrawal_plans_only_global_facts() {
        let currents = vec![current_fact(1, &[]), current_fact(2, &["project:a"])];

        assert_eq!(
            plan_fact_membership_changes(&currents, &[]),
            vec![FactMembershipChange {
                rel_id: 1,
                remaining: Vec::new(),
            }]
        );
    }

    fn current_fact(rel_id: i64, layers: &[&str]) -> CurrentFact {
        CurrentFact {
            rel_id,
            iri: format!("mindreader:fact/{rel_id}"),
            layers: layers.iter().map(|layer| (*layer).to_string()).collect(),
        }
    }

    #[test]
    fn effective_weight_saturates_without_panicking() {
        assert_eq!(effective_weight(1, 2, 3), 6);
        assert_eq!(effective_weight(i64::MAX, 1, 1), i64::MAX);
        assert_eq!(effective_weight(i64::MIN, -1, -1), i64::MIN);
    }

    #[test]
    fn system_owned_history_predicates_are_not_client_writable() {
        assert!(reject_system_owned_predicate("SUPERSEDES").is_err());
        assert!(reject_system_owned_predicate("mindreader:property/CONTRADICTS").is_err());
        assert!(reject_system_owned_predicate("worksOn").is_ok());
    }

    #[test]
    fn write_facts_reject_empty_and_over_max() {
        let empty = WriteArgs {
            facts: Vec::new(),
            scope: Vec::new(),
        };
        let over = WriteArgs {
            facts: (0..=MAX_WRITE_FACTS)
                .map(|index| WriteFact {
                    s: EntityInput {
                        kind: "node".into(),
                        iri: None,
                        name: Some(format!("s{index}")),
                        labels: vec!["Element".into()],
                    },
                    p: "worksOn".into(),
                    o: ObjectInput {
                        kind: "node".into(),
                        iri: None,
                        name: Some(format!("o{index}")),
                        labels: vec!["Element".into()],
                        value: None,
                        datatype: None,
                    },
                    spike: None,
                    contradicts: false,
                })
                .collect(),
            scope: Vec::new(),
        };
        assert!(matches!(
            validate_write_args(&empty),
            Err(Error::Domain(DomainError::InvalidInput(_)))
        ));
        assert!(matches!(
            validate_write_args(&over),
            Err(Error::Domain(DomainError::InvalidInput(_)))
        ));
        let one = WriteArgs {
            facts: vec![over.facts[0].clone()],
            scope: Vec::new(),
        };
        assert!(validate_write_args(&one).is_ok());
    }

    #[test]
    fn write_fact_lock_union_bounds_at_max_facts() {
        let mut locks = Vec::new();
        for index in 0..MAX_WRITE_FACTS {
            let fact = prepare_write_fact(WriteFact {
                s: EntityInput {
                    kind: "node".into(),
                    iri: Some(format!("mindreader:element/s{index}")),
                    name: None,
                    labels: vec!["Element".into()],
                },
                p: format!("prop{index}"),
                o: ObjectInput {
                    kind: "node".into(),
                    iri: Some(format!("mindreader:element/o{index}")),
                    name: None,
                    labels: vec!["Element".into()],
                    value: None,
                    datatype: None,
                },
                spike: None,
                contradicts: true,
            })
            .expect("unique contradict facts prepare");
            locks.extend(write_fact_lock_requests(
                &fact.subject_iri,
                &fact.prop_iri,
                &fact.object_iri,
                fact.contradicts,
            ));
        }
        assert_eq!(locks.len(), 100);
        let expanded = fact_lock_specs(&locks);
        assert_eq!(
            expanded.len(),
            160,
            "20 unique contradict facts expand to 160 fact-lock rows, not the unexpanded 100"
        );
    }

    #[test]
    fn target_args_accept_only_pasteable_handles() {
        let node_handle = serde_json::json!({
            "kind": "node",
            "iri": "mindreader:element/alice"
        });
        let expanded_node = serde_json::json!({
            "kind": "node",
            "iri": "mindreader:element/alice",
            "name": "Alice",
            "labels": ["Element"],
            "layers": ["project:x"],
            "weight": 0
        });
        let expanded_fact = serde_json::json!({
            "kind": "fact",
            "iri": "mindreader:relationship/abc",
            "type": "ASSERTS",
            "from": "mindreader:element/alice",
            "to": "mindreader:element/mindreader",
            "propertyIri": "mindreader:property/worksOn",
            "layers": ["project:x"],
            "weight": 0
        });
        let node_target: TargetArgs = serde_json::from_value(node_handle).unwrap();
        assert_eq!(node_target.kind, "node");
        assert_eq!(node_target.iri, "mindreader:element/alice");
        assert!(serde_json::from_value::<TargetArgs>(expanded_node).is_err());
        assert!(serde_json::from_value::<TargetArgs>(expanded_fact).is_err());
    }
}
