//! Set-valued `write`, membership-selective `revise`, and soft `withdraw`.
//!
//! Also owns the shared MCP argument types reused by `judge` and `place`.
//! Exact fact identity (subject, property, object, effective qualification and
//! interval) merges memberships on reassert; other objects or intervals stay
//! current. `revise` moves only the requested memberships and records
//! `SUPERSEDES` in the same transaction. Withdrawal sets `validTo` and never
//! hard-deletes. `CONTRADICTS` and `SUPERSEDES` are system-owned. Any real
//! change records exactly one Episode; all-noop rolls back with `episode: null`.

use crate::domain::{
    DomainError, EffectiveInterval, EffectiveUpdate, EntityInput, EntityRef,
    NormalizedEffectiveInterval, ObjectInput, ObjectValue, PredicateRef, SpikeRank,
    WithdrawalScope,
};
use crate::graph::{
    acquire_fact_locks_in_txn, create_episode_in_txn, endpoint_json, ensure_property_in_txn,
    fact_text, fetch_all_txn, fetch_one, fetch_one_txn, merge_literal_in_txn, merge_node_in_txn,
    node_json, rel_json, safe_rel, Episode, MergedNode, NodeSpec,
};
pub(super) use crate::layers::normalize_scope as normalize_layers;
use crate::merge::merge_suggestions_in_txn;
use crate::payload::finish_mutation;
use crate::vocabulary::{
    is_system_relationship, structural_relationship_for, CONTRADICTS_PROPERTY_IRI,
    SCHEMA_RELATIONSHIPS, SUPERSEDES_PROPERTY_IRI, SYSTEM_RELATIONSHIPS,
};
use crate::{
    error::{Error, Result},
    operation_error,
};
use neo4rs::{query, Graph, Node, Relation, Txn};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::time::{sleep, Duration};
use uuid::Uuid;

pub(super) const FACT_LOCK_SCOPE: &str = "@fact";
pub(super) const LAYERS_PROPERTY: &str = "mindreader:property/layers";
pub(super) const PREDICATE_USAGE_PROPERTY: &str = "mindreader:property/predicate-usage";
/// Shared 1–20 batch cap for write facts, judge ratings, and place edits.
pub(super) const MAX_WRITE_FACTS: usize = 20;

/// Subject, predicate-usage, membership, and optional CONTRADICTS guards for one write triple.
pub(super) fn write_fact_lock_requests(
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
            CONTRADICTS_PROPERTY_IRI.into(),
            FACT_LOCK_SCOPE.into(),
        ));
    }
    locks
}

/// Write locks plus a membership guard on the previous object so revise cannot race withdraw.
pub(super) fn revision_fact_lock_requests(
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
pub(super) fn withdrawal_fact_lock_requests(
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
pub(super) fn reject_system_owned_predicate(predicate: &str) -> Result<()> {
    if is_system_relationship(predicate) {
        return Err(DomainError::InvalidInput(format!(
            "predicate {predicate:?} is system-owned and cannot be mutated directly"
        ))
        .into());
    }
    Ok(())
}

/// Retry only typed Neo4j transients; ambiguous commits stay non-retryable.
pub(super) fn is_transient_neo4j_error(error: &Error) -> bool {
    error.is_transient_neo4j()
}

pub(super) fn validate_target(target: &TargetArgs) -> Result<()> {
    if !matches!(target.kind.as_str(), "node" | "fact") {
        return Err(DomainError::InvalidInput("target.kind must be node or fact".into()).into());
    }
    if target.iri.trim().is_empty() {
        return Err(DomainError::InvalidInput("target.iri cannot be empty".into()).into());
    }
    Ok(())
}

pub(super) fn serialized_scope(record: &Value) -> Result<Vec<String>> {
    record
        .get("scope")
        .and_then(Value::as_array)
        .ok_or_else(|| operation_error!("serialized scope is not an array"))?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| operation_error!("serialized scope contains a non-string value"))
        })
        .collect()
}

/// Require 1..=20 facts before the write transaction starts.
pub(super) fn validate_write_args(args: &WriteArgs) -> Result<()> {
    if args.facts.is_empty() || args.facts.len() > MAX_WRITE_FACTS {
        return Err(DomainError::InvalidInput(format!(
            "write facts must contain between 1 and {MAX_WRITE_FACTS} items"
        ))
        .into());
    }
    Ok(())
}

/// Canonicalize subject, predicate, and object IRIs and reject system-owned predicates.
pub(super) fn prepare_write_fact(fact: WriteFact) -> Result<PreparedWriteFact> {
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
    let structural = structural_relationship_for(&prop_iri);
    if structural.is_some() && fact.effective.is_some() {
        return Err(DomainError::InvalidInput(
            "write effective applies only to ordinary state facts, not structural relationships"
                .into(),
        )
        .into());
    }
    let effective = fact
        .effective
        .map(|interval| interval.normalize("write facts[].effective"))
        .transpose()?;
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
        effective,
    })
}

/// Union named memberships; either empty list is global and wins.
pub(super) fn merge_memberships(current: &[String], incoming: &[String]) -> Vec<String> {
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
pub(super) fn remove_memberships(current: &[String], selected: &[String]) -> Option<Vec<String>> {
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
pub(super) fn effective_weight(subject: i64, relationship: i64, object: i64) -> i64 {
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
    /// Optional Spike stored on this exact fact.
    #[serde(default)]
    pub spike: Option<String>,
    /// When true, add current CONTRADICTS edges from this object to other current values.
    #[serde(default)]
    pub contradicts: bool,
    /// Optional half-open world-time interval; absent/null means temporally unknown.
    #[serde(default)]
    pub effective: Option<EffectiveInterval>,
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
pub(super) struct PreparedWriteFact {
    subject_spec: NodeSpec,
    subject_kind: String,
    pub(super) subject_iri: String,
    object_value: ObjectValue,
    pub(super) object_iri: String,
    pub(super) prop_iri: String,
    structural: Option<String>,
    spike: Option<String>,
    pub(super) contradicts: bool,
    effective: Option<NormalizedEffectiveInterval>,
}

/// Resolved correction plan used internally by `revise`.
#[derive(Debug, Clone)]
struct RevisionPlan {
    pub s: EntityInput,
    pub p: String,
    pub old: ObjectInput,
    pub replacement: ObjectInput,
    pub layers: Vec<String>,
    pub spike: Option<String>,
    pub contradicts: bool,
    pub reason: Option<String>,
    pub effective: EffectiveUpdate,
    pub previous_effective: Option<NormalizedEffectiveInterval>,
}

/// Outcome of one revise attempt, including no-op same-object corrections.
struct RevisionResult {
    noop: bool,
    scope: Vec<String>,
    episode: Option<Episode>,
    current_target: TargetArgs,
    previous_target: TargetArgs,
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
    withdrawn_targets: Vec<TargetArgs>,
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

impl TargetArgs {
    /// Build an internal pasteable fact handle from a stored relationship IRI.
    fn fact(iri: impl Into<String>) -> Self {
        Self {
            kind: "fact".into(),
            iri: iri.into(),
        }
    }
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
    pub replacement: ObjectInput,
    /// Optional Spike on the replacement fact; omitted keeps the previous Spike.
    #[serde(default)]
    pub spike: Option<String>,
    /// When true, add CONTRADICTS from the replacement object to other current values.
    #[serde(default)]
    pub contradicts: bool,
    /// Optional audit note stored on the Episode and replacement fact.
    #[serde(default)]
    pub reason: Option<String>,
    /// Omitted inherits the selected fact's interval; null clears it; an object replaces it.
    #[serde(default)]
    pub effective: EffectiveUpdate,
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
    /// Predicate local name or property IRI; subject-wide withdrawal when omitted.
    #[serde(default)]
    pub p: Option<String>,
    /// Optional audit note stored on the Episode and withdrawn facts.
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
pub(super) struct CurrentFact {
    pub(super) rel_id: i64,
    pub(super) iri: String,
    pub(super) layers: Vec<String>,
    pub(super) spike: Option<String>,
    pub(super) effective: Option<NormalizedEffectiveInterval>,
}

/// Remaining memberships for one current fact after a revise/withdraw selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct FactMembershipChange {
    pub(super) rel_id: i64,
    pub(super) remaining: Vec<String>,
}

/// Compute remaining memberships for revise/withdraw; omit facts that do not intersect `selected`.
pub(super) fn plan_fact_membership_changes(
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
pub(super) fn select_revision_current(
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
    effective: Option<&NormalizedEffectiveInterval>,
) -> Result<Vec<CurrentFact>> {
    let rows = if let Some(rel) = structural {
        let rel = safe_rel(rel)?;
        let cypher = format!(
            "MATCH (s:Entity {{iri: $s}})-[r:{rel}]->(o:Entity {{iri: $o}}) \
             WHERE r.validTo IS NULL RETURN id(r) AS rid, r.iri AS iri, \
             r.layers AS layers, r.spike AS spike, \
             false AS effectiveQualified, null AS effectiveFrom, null AS effectiveTo"
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
                   AND coalesce(r.effectiveQualified, false) = $effectiveQualified \
                   AND (($effectiveFrom IS NULL AND r.effectiveFrom IS NULL) \
                        OR ($effectiveFrom IS NOT NULL \
                            AND r.effectiveFrom = datetime($effectiveFrom))) \
                   AND (($effectiveTo IS NULL AND r.effectiveTo IS NULL) \
                        OR ($effectiveTo IS NOT NULL \
                            AND r.effectiveTo = datetime($effectiveTo))) \
                 RETURN id(r) AS rid, r.iri AS iri, r.layers AS layers, r.spike AS spike, \
                        coalesce(r.effectiveQualified, false) AS effectiveQualified, \
                        toString(r.effectiveFrom) AS effectiveFrom, \
                        toString(r.effectiveTo) AS effectiveTo",
            )
            .param("s", s.to_string())
            .param("o", o.to_string())
            .param("p", prop_iri.to_string())
            .param("effectiveQualified", effective.is_some())
            .param(
                "effectiveFrom",
                effective.and_then(|interval| interval.from.clone()),
            )
            .param(
                "effectiveTo",
                effective.and_then(|interval| interval.to.clone()),
            ),
        )
        .await?
    };
    rows.into_iter()
        .map(|row| {
            Ok(CurrentFact {
                rel_id: row.get("rid")?,
                iri: row.get("iri")?,
                layers: row.get("layers")?,
                spike: row.get::<String>("spike").ok(),
                effective: row
                    .get::<bool>("effectiveQualified")
                    .unwrap_or(false)
                    .then(|| NormalizedEffectiveInterval {
                        from: row.get::<String>("effectiveFrom").ok(),
                        to: row.get::<String>("effectiveTo").ok(),
                    }),
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
    /// `Some` classifies or reclassifies the exact fact; `None` preserves an existing value.
    spike: Option<&'a str>,
    /// Ordinary ASSERTS identity qualifier; structural relationships are always atemporal.
    effective: Option<&'a NormalizedEffectiveInterval>,
}

/// Reassert an exact triple by merging memberships, or CREATE a new fact identity.
async fn ensure_relation_txn(
    txn: &mut Txn,
    write: &RelationWrite<'_>,
) -> Result<(String, bool, Option<String>)> {
    let current = find_current_pairs_txn(
        txn,
        write.s,
        write.prop_iri,
        Some(write.rel_type).filter(|rel| *rel != "ASSERTS"),
        write.o,
        write.effective,
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
        let next_spike = write.spike.or(current.spike.as_deref());
        if merged == current.layers && next_spike == current.spike.as_deref() {
            return Ok((current.iri.clone(), false, current.spike.clone()));
        }
        fetch_one_txn(
            txn,
            query(
                "MATCH ()-[r]->() WHERE id(r) = $rid AND r.validTo IS NULL \
                 SET r.layers = $layers, r.layersUpdatedAt = datetime(), \
                     r.layerEpisodeId = $episode, r.spike = $spike \
                 RETURN r.iri AS iri",
            )
            .param("rid", current.rel_id)
            .param("layers", merged)
            .param("episode", write.episode.iri.clone())
            .param("spike", next_spike.map(str::to_string)),
        )
        .await?
        .ok_or_else(|| operation_error!("relationship disappeared while merging layers"))?;
        return Ok((current.iri.clone(), true, next_spike.map(str::to_string)));
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
            effectiveQualified: $effectiveQualified,
            episodeId: $episode,
            factText: $factText,
            spike: $spike
        }}]->(o)
        SET r.reason = $reason,
            r.effectiveFrom = CASE WHEN $effectiveFrom IS NULL
              THEN null ELSE datetime($effectiveFrom) END,
            r.effectiveTo = CASE WHEN $effectiveTo IS NULL
              THEN null ELSE datetime($effectiveTo) END
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
            .param("factText", write.fact_text.to_string())
            .param("spike", write.spike.map(str::to_string))
            .param("effectiveQualified", write.effective.is_some())
            .param(
                "effectiveFrom",
                write.effective.and_then(|interval| interval.from.clone()),
            )
            .param(
                "effectiveTo",
                write.effective.and_then(|interval| interval.to.clone()),
            ),
    )
    .await?
    .ok_or_else(|| operation_error!("failed to create relationship {iri}"))?;
    Ok((iri, true, write.spike.map(str::to_string)))
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

/// Reload one relationship after membership merging so mutation responses show complete state.
async fn refreshed_relation_json_txn(
    txn: &mut Txn,
    iri: &str,
    subject_iri: &str,
    object_iri: &str,
) -> Result<Value> {
    let row = fetch_one_txn(
        txn,
        query("MATCH ()-[r]->() WHERE r.iri = $iri RETURN r").param("iri", iri.to_string()),
    )
    .await?
    .ok_or_else(|| operation_error!("missing relationship {iri}"))?;
    rel_json(&row.get::<Relation>("r")?, subject_iri, object_iri)
}

/// Other current values of the same subject+predicate visible in this request `scope` (set-valued alternatives).
async fn find_conflicts_txn(
    txn: &mut Txn,
    s: &str,
    prop_iri: &str,
    structural: Option<&str>,
    o_iri: &str,
    layers: &[String],
    effective: Option<&NormalizedEffectiveInterval>,
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
              AND ($isStructural
                OR ($effectiveQualified = false
                    AND coalesce(r.effectiveQualified, false) = false)
                OR ($effectiveQualified = true
                    AND coalesce(r.effectiveQualified, false) = true
                    AND ($effectiveTo IS NULL OR r.effectiveFrom IS NULL
                         OR r.effectiveFrom < datetime($effectiveTo))
                    AND (r.effectiveTo IS NULL OR $effectiveFrom IS NULL
                         OR datetime($effectiveFrom) < r.effectiveTo)))
            RETURN r, o, r.propertyIri AS p
            "#,
        )
        .param("s", s.to_string())
        .param("o", o_iri.to_string())
        .param("layers", layers.to_vec())
        .param("p", prop_iri.to_string())
        .param("relType", rel_type.to_string())
        .param("isStructural", is_structural)
        .param("effectiveQualified", effective.is_some())
        .param(
            "effectiveFrom",
            effective.and_then(|interval| interval.from.clone()),
        )
        .param(
            "effectiveTo",
            effective.and_then(|interval| interval.to.clone()),
        ),
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
        let (_, relation_changed, _) = ensure_relation_txn(
            txn,
            &RelationWrite {
                rel_type: "CONTRADICTS",
                s: new_o,
                o: &old_o,
                prop_iri: CONTRADICTS_PROPERTY_IRI,
                layers,
                episode,
                reason: None,
                fact_text: &text,
                spike: None,
                effective: None,
            },
        )
        .await?;
        changed |= relation_changed;
    }
    Ok(changed)
}

/// Batched set-valued `write`: merge exact facts or CREATE a new identity under call `scope`.
///
/// All-noop rolls back with `episode: null`; any change records one Episode.
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
                alternatives.push(json!({
                    "target": item.target,
                    "conflicts": item.alternatives,
                }));
            }
            items.push(item.fact);
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

/// One write item: whether it changed, the fact envelope, created IRIs, and alternatives.
struct PreparedFactResult {
    noop: bool,
    fact: Value,
    target: TargetArgs,
    created_iris: Vec<String>,
    alternatives: Vec<Value>,
}

/// MERGE endpoints, merge or create the classified fact, and optionally attach CONTRADICTS.
async fn write_prepared_fact_txn(
    txn: &mut Txn,
    fact: PreparedWriteFact,
    layers: &[String],
    episode: &Episode,
) -> Result<PreparedFactResult> {
    let subject = merge_node_in_txn(txn, &fact.subject_spec, &fact.subject_kind, &[]).await?;
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
    let (relationship_iri, relationship_changed, _) = ensure_relation_txn(
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
            spike: fact.spike.as_deref(),
            effective: fact.effective.as_ref(),
        },
    )
    .await?;
    changed |= relationship_changed;
    let relationship_json =
        refreshed_relation_json_txn(txn, &relationship_iri, &subject.iri, &object.iri).await?;
    let relationship_scope = serialized_scope(&relationship_json)?;
    let conflicts = find_conflicts_txn(
        txn,
        &subject.iri,
        &fact.prop_iri,
        fact.structural.as_deref(),
        &object.iri,
        layers,
        fact.effective.as_ref(),
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
    let target = TargetArgs::fact(relationship_iri);
    Ok(PreparedFactResult {
        noop: !changed,
        fact: {
            let mut item = crate::graph::fact_envelope(
                subject_json,
                &fact.prop_iri,
                object_json,
                &relationship_json,
                &relationship_scope,
            )?;
            item["noop"] = json!(!changed);
            // Review alternatives are every other current value; CONTRADICTS is opt-in.
            item["conflicts"] = if fact.contradicts {
                json!(conflicts)
            } else {
                json!([])
            };
            item
        },
        target,
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
    let replacement_value = ObjectValue::from_input(args.replacement)?;
    let replacement_iri = replacement_value.resolved_iri();
    let prop_iri = predicate.iri().to_string();
    let structural = structural_relationship_for(&prop_iri);
    if structural.is_some() && !matches!(&args.effective, EffectiveUpdate::Inherit) {
        return Err(DomainError::InvalidInput(
            "revise effective applies only to ordinary state facts, not structural relationships"
                .into(),
        )
        .into());
    }
    let replacement_effective = match args.effective.clone() {
        EffectiveUpdate::Inherit => args.previous_effective.clone(),
        EffectiveUpdate::Clear => None,
        EffectiveUpdate::Set(interval) => Some(interval.normalize("revise effective")?),
    };
    let locks = revision_fact_lock_requests(
        &subject_iri,
        &prop_iri,
        &old_iri,
        &replacement_iri,
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
        args.previous_effective.as_ref(),
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
    if old_iri == replacement_iri && old_current.effective == replacement_effective {
        // Same object and interval is a no-op: roll back, no Episode, no SUPERSEDES.
        txn.rollback().await?;
        let target_iri = expected_fact_iri.unwrap_or(&old_current.iri);
        let target = TargetArgs::fact(target_iri);
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
    let effective_spike = spike.clone().or_else(|| old_current.spike.clone());
    let result = async {
        let subject = merge_node_in_txn(&mut txn, &subject_spec, &subject_kind, &[]).await?;
        let (replacement_object, replacement_is_literal) =
            merge_object_in_txn(&mut txn, replacement_value).await?;
        let (_, property_created, _) = ensure_property_in_txn(&mut txn, &prop_iri).await?;
        let episode = create_episode_in_txn(&mut txn, "revise", args.reason.as_deref()).await?;
        apply_node_memberships_txn(&mut txn, &subject, &layers).await?;
        apply_node_memberships_txn(&mut txn, &replacement_object, &layers).await?;
        change_fact_memberships_txn(
            &mut txn,
            &old_current,
            &layers,
            &episode,
            args.reason.as_deref(),
        )
        .await?;
        let replacement_text =
            fact_text(&subject.name, &subject.iri, &prop_iri, &replacement_object);
        let (replacement_relationship_iri, _, _) = ensure_relation_txn(
            &mut txn,
            &RelationWrite {
                rel_type: structural.as_deref().unwrap_or("ASSERTS"),
                s: &subject.iri,
                o: &replacement_object.iri,
                prop_iri: &prop_iri,
                layers: &layers,
                episode: &episode,
                reason: args.reason.as_deref(),
                fact_text: &replacement_text,
                spike: effective_spike.as_deref(),
                effective: replacement_effective.as_ref(),
            },
        )
        .await?;
        let supersedes_text = format!("{} SUPERSEDES {old_iri}", replacement_object.iri);
        let (supersedes_iri, _, _) = ensure_relation_txn(
            &mut txn,
            &RelationWrite {
                rel_type: "SUPERSEDES",
                s: &replacement_object.iri,
                o: &old_iri,
                prop_iri: SUPERSEDES_PROPERTY_IRI,
                layers: &layers,
                episode: &episode,
                reason: args.reason.as_deref(),
                fact_text: &supersedes_text,
                spike: None,
                effective: None,
            },
        )
        .await?;
        txn.run(
            query(
                "MATCH (e:Entity:Episode {iri: $episode}) \
                 SET e.previousFactIri = $previousFactIri, \
                     e.replacementFactIri = $replacementFactIri, \
                     e.supersedesIri = $supersedesIri, \
                     e.selectedScope = $selectedScope",
            )
            .param("episode", episode.iri.clone())
            .param("previousFactIri", old_current.iri.clone())
            .param("replacementFactIri", replacement_relationship_iri.clone())
            .param("supersedesIri", supersedes_iri)
            .param("selectedScope", layers.clone()),
        )
        .await?;
        let conflicts = find_conflicts_txn(
            &mut txn,
            &subject.iri,
            &prop_iri,
            structural.as_deref(),
            &replacement_object.iri,
            &layers,
            replacement_effective.as_ref(),
        )
        .await?;
        if args.contradicts {
            ensure_contradictions_txn(
                &mut txn,
                &replacement_object.iri,
                &conflicts,
                &layers,
                &episode,
            )
            .await?;
        }
        let subject_json = refreshed_node_json_txn(&mut txn, &subject.iri).await?;
        let replacement_json = refreshed_node_json_txn(&mut txn, &replacement_object.iri).await?;
        let relationship_json = refreshed_relation_json_txn(
            &mut txn,
            &replacement_relationship_iri,
            &subject.iri,
            &replacement_object.iri,
        )
        .await?;
        let mut created_iris = Vec::new();
        if subject.created {
            created_iris.push(subject.iri.clone());
        }
        if replacement_object.created && !replacement_is_literal {
            created_iris.push(replacement_object.iri.clone());
        }
        if property_created {
            created_iris.push(prop_iri.clone());
        }
        let merge_suggestions = merge_suggestions_in_txn(&mut txn, &created_iris, &layers).await?;
        Ok::<_, Error>((
            episode,
            subject_json,
            replacement_json,
            replacement_relationship_iri,
            relationship_json,
            conflicts,
            args.contradicts,
            merge_suggestions,
        ))
    }
    .await;
    let (
        episode,
        subject,
        replacement,
        relationship_iri,
        relationship,
        alternatives,
        contradicts,
        merge_suggestions,
    ) = match result {
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
    let current_target = TargetArgs::fact(relationship_iri);
    let previous_target = TargetArgs::fact(expected_fact_iri.unwrap_or(&old_current.iri));
    let conflicts = if contradicts {
        Value::Array(alternatives.clone())
    } else {
        json!([])
    };
    let mut fact =
        crate::graph::fact_envelope(subject, &prop_iri, replacement, &relationship, &layers)?;
    fact["conflicts"] = conflicts;
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
    let protected = SCHEMA_RELATIONSHIPS
        .iter()
        .chain(SYSTEM_RELATIONSHIPS)
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
            let structural = structural_relationship_for(predicate);
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
                spike: None,
                effective: None,
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
                .map(|current| TargetArgs::fact(current.iri.clone()))
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
async fn load_current_fact(
    graph: &Graph,
    iri: &str,
    layers: &[String],
) -> Result<(
    EntityInput,
    String,
    ObjectInput,
    Option<NormalizedEffectiveInterval>,
)> {
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
            RETURN s, r, o, r.propertyIri AS p,
                   coalesce(r.effectiveQualified, false) AS effectiveQualified,
                   toString(r.effectiveFrom) AS effectiveFrom,
                   toString(r.effectiveTo) AS effectiveTo
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
    let effective = row
        .get::<bool>("effectiveQualified")
        .unwrap_or(false)
        .then(|| NormalizedEffectiveInterval {
            from: row.get::<String>("effectiveFrom").ok(),
            to: row.get::<String>("effectiveTo").ok(),
        });
    Ok((
        entity_input_from_node(&subject)?,
        crate::graph::local_predicate_name(&property),
        object_input_from_node(&object)?,
        effective,
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
    let (s, p, old, previous_effective) =
        load_current_fact(graph, &args.target.iri, &layers).await?;
    let result = apply_revision(
        graph,
        RevisionPlan {
            s,
            p,
            old,
            replacement: args.replacement,
            layers,
            spike: args.spike,
            contradicts: args.contradicts,
            reason: args.reason,
            effective: args.effective,
            previous_effective,
        },
        Some(&args.target.iri),
    )
    .await?;
    let current_target = result.current_target;
    let previous_target = result.previous_target;
    let current_target_value = json!(current_target);
    let previous_target_value = json!(previous_target);
    let facts = result.fact.clone().into_iter().collect::<Vec<_>>();
    let fact = result.fact.unwrap_or(Value::Null);
    let alternatives = if result.alternatives.is_empty() {
        Vec::new()
    } else {
        vec![json!({
            "target": current_target_value.clone(),
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
            "target": current_target_value.clone(),
            "previousTarget": previous_target_value.clone(),
            "fact": fact,
            "review": {
                "unify": result.unify,
                "alternatives": alternatives,
            },
        }),
        &facts,
        &[],
        Some(current_target_value),
        Some(previous_target_value),
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
        let (s, p, o, _) = load_current_fact(graph, &target.iri, &layers).await?;
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

/// Force schema-definition edges to stay global when they define Class/Property catalog structure.
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
