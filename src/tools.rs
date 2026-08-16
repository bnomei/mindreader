//! Mutation arguments and graph behavior for the non-search memory tools.
//!
//! Implements get, traverse, stats, assert, replace, retract, feedback, layers,
//! and schema: set-valued assertions, soft retraction, explicit SUPERSEDES
//! corrections, shared signed weights, membership audits with endpoint closure,
//! and global schema-as-data. CONTRADICTS and SUPERSEDES are system-owned.
//! State-changing mutations create one Episode when they change graph state.

use crate::domain::{
    DomainError, EntityInput, EntityRef, ObjectInput, ObjectValue, PredicateRef, RetractScope,
    SpikeRank,
};
use crate::graph::{
    acquire_fact_locks_in_txn, create_episode_in_txn, endpoint_json, ensure_property_in_txn,
    fact_text, fetch_all, fetch_all_txn, fetch_one, fetch_one_txn, merge_literal_in_txn,
    merge_node_in_txn, node_json, path_to_json, rel_json, safe_rel, structural_rel_for, Episode,
    MergedNode, NodeSpec, FIXED_RELS, MERGE_CANDIDATE_INDEX, MODEL_MARKER_KEY, MODEL_VERSION,
};
#[cfg(test)]
use crate::graph::{spike_label, spike_rank};
use crate::iri::{class_iri, name_from_iri, property_iri};
use crate::layers::validate_layer_ids;
use crate::merge::merge_suggestions_in_txn;
#[cfg(test)]
use crate::search::SearchArgs;
use crate::{
    error::{Context, Error, Result},
    operation_error,
};
use neo4rs::{query, Graph, Node, Path, Relation, Txn};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[cfg(test)]
use std::collections::HashMap;
use std::collections::HashSet;
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

fn assert_fact_lock_requests(
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

fn replace_fact_lock_requests(
    subject_iri: &str,
    prop_iri: &str,
    old_iri: &str,
    new_iri: &str,
    contradicts: bool,
) -> Vec<(String, String, String)> {
    let mut locks = assert_fact_lock_requests(subject_iri, prop_iri, new_iri, contradicts);
    locks.push((
        old_iri.to_string(),
        LAYERS_PROPERTY.into(),
        FACT_LOCK_SCOPE.into(),
    ));
    locks
}

fn retract_fact_lock_requests(
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

fn is_transient_neo4j_error(error: &Error) -> bool {
    error.is_transient_neo4j()
}

fn normalize_layers(raw: Vec<String>) -> Result<Vec<String>> {
    Ok(validate_layer_ids(raw)?
        .into_iter()
        .map(|layer| layer.into_string())
        .collect())
}

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

// See graph.rs: `weightText` preserves signed values across neo4rs 0.8 while
// Cypher arithmetic continues to use the numeric `weight` property.
#[cfg(test)]
fn weight(node: &Node) -> i64 {
    node.get::<String>("weightText")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| node.get::<i64>("weight").unwrap_or(0))
}

#[cfg(test)]
fn relation_weight(relation: &Relation) -> i64 {
    relation
        .get::<String>("weightText")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| relation.get::<i64>("weight").unwrap_or(0))
}

#[cfg(test)]
fn effective_weight(subject: i64, relationship: i64, object: i64) -> i64 {
    subject.saturating_add(relationship).saturating_add(object)
}

fn relationship_iri() -> String {
    format!("mindreader:relationship/{}", Uuid::new_v4())
}

/// Arguments for IRI lookup with optional one-hop neighbor expansion.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetArgs {
    pub iri: String,
    pub layers: Vec<String>,
    #[serde(default)]
    pub hops: Option<u32>,
}

/// Arguments for model readiness and layer-scoped graph counters.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct StatsArgs {
    pub layers: Vec<String>,
}

/// Arguments for bounded typed-edge walks from a visible start IRI.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct TraverseArgs {
    pub from: String,
    pub layers: Vec<String>,
    #[serde(default)]
    pub rels: Option<Vec<String>>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Arguments for set-valued triple assertion (membership merge, optional Spike/CONTRADICTS).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AssertArgs {
    pub s: EntityInput,
    pub p: String,
    pub o: ObjectInput,
    pub layers: Vec<String>,
    #[serde(default)]
    pub spike: Option<String>,
    #[serde(default)]
    pub contradicts: bool,
}

/// Arguments for explicit fact correction with atomic SUPERSEDES history.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReplaceArgs {
    pub s: EntityInput,
    pub p: String,
    pub old: ObjectInput,
    pub new: ObjectInput,
    pub layers: Vec<String>,
    #[serde(default)]
    pub spike: Option<String>,
    #[serde(default)]
    pub contradicts: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Tagged retract target: fact, predicate-wide, or subject-wide soft withdrawal.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RetractTargetArgs {
    pub kind: String,
    pub s: EntityInput,
    #[serde(default)]
    pub p: Option<String>,
    #[serde(default)]
    pub o: Option<ObjectInput>,
}

/// Arguments for soft retraction of selected fact memberships.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RetractArgs {
    pub target: RetractTargetArgs,
    pub layers: Vec<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

/// Stable node or relationship target for feedback and membership audits.
#[derive(Debug, Clone, Deserialize, Serialize, JsonSchema)]
pub struct TargetArgs {
    pub kind: String,
    pub iri: String,
}

/// Arguments for explicit +1/−1 weight feedback on a visible current target.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct FeedbackArgs {
    pub layers: Vec<String>,
    pub target: TargetArgs,
    pub mode: String,
}

/// Arguments for auditable add/remove of layer memberships on a target.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct LayersArgs {
    pub layers: Vec<String>,
    pub target: TargetArgs,
    #[serde(default)]
    pub add: Vec<String>,
    #[serde(default)]
    pub remove: Vec<String>,
}

/// Arguments for global schema-as-data class or property declaration.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SchemaArgs {
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub iri: Option<String>,
    #[serde(default, rename = "subClassOf")]
    pub sub_class_of: Option<String>,
    #[serde(default, rename = "subPropertyOf")]
    pub sub_property_of: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub range: Option<String>,
}

fn node_spec(entity: EntityRef) -> NodeSpec {
    NodeSpec {
        iri: entity.iri,
        name: entity.name,
        labels: entity.labels,
    }
}

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
            WITH n, coalesce(n.layers, []) AS before
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
    let before = row.get::<Vec<String>>("before").unwrap_or_default();
    let after = row.get::<Vec<String>>("after").unwrap_or_default();
    Ok(before != after)
}

#[derive(Debug, Clone)]
struct CurrentFact {
    rel_id: i64,
    iri: String,
    layers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FactMembershipChange {
    rel_id: i64,
    remaining: Vec<String>,
}

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
             coalesce(r.layers, []) AS layers"
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
                 RETURN id(r) AS rid, r.iri AS iri, coalesce(r.layers, []) AS layers",
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
                layers: row.get("layers").unwrap_or_default(),
            })
        })
        .collect()
}

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
    let cypher = format!(
        r#"
        MATCH (s:Entity {{iri: $s}}), (o:Entity {{iri: $o}})
        CREATE (s)-[r:{rel_type} {{
            iri: $iri,
            propertyIri: $p,
            layers: $layers,
            weight: 0,
            weightText: '0',
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

async fn refreshed_node_json_txn(txn: &mut Txn, iri: &str) -> Result<Value> {
    let row = fetch_one_txn(
        txn,
        query("MATCH (n:Entity {iri: $iri}) RETURN n").param("iri", iri.to_string()),
    )
    .await?
    .ok_or_else(|| operation_error!("missing node {iri}"))?;
    Ok(node_json(&row.get::<Node>("n")?))
}

/// Return a visible node by IRI, optionally with current visible one-hop neighbors.
pub async fn memory_get(graph: &Graph, args: GetArgs) -> Result<Value> {
    let layers = normalize_layers(args.layers)?;
    let hops = if args.hops == Some(1) { 1 } else { 0 };
    let node_row = fetch_one(
        graph,
        query(
            r#"
            MATCH (n:Entity {iri: $iri})
            WHERE size(coalesce(n.layers, [])) = 0
               OR any(layer IN coalesce(n.layers, []) WHERE layer IN $layers)
            RETURN n
            "#,
        )
        .param("iri", args.iri.clone())
        .param("layers", layers.clone()),
    )
    .await?;
    let Some(node_row) = node_row else {
        return Ok(json!({ "found": false, "iri": args.iri, "layers": layers }));
    };
    let node: Node = node_row.get("n")?;
    if hops == 0 {
        return Ok(json!({
            "found": true,
            "node": node_json(&node),
            "hops": 0,
            "layers": layers,
        }));
    }

    let rows = fetch_all(
        graph,
        query(
            r#"
            MATCH (n:Entity {iri: $iri})
            OPTIONAL MATCH (n)-[r]-(m:Entity)
            WHERE r IS NULL OR (
              r.validTo IS NULL
              AND (size(coalesce(r.layers, [])) = 0
                   OR any(layer IN coalesce(r.layers, []) WHERE layer IN $layers))
              AND (size(coalesce(m.layers, [])) = 0
                   OR any(layer IN coalesce(m.layers, []) WHERE layer IN $layers))
            )
            RETURN r, m, CASE WHEN r IS NULL THEN false ELSE startNode(r) = n END AS outgoing
            "#,
        )
        .param("iri", args.iri.clone())
        .param("layers", layers.clone()),
    )
    .await?;
    let mut neighbors = Vec::new();
    let mut seen = HashSet::new();
    for row in rows {
        let (Ok(rel), Ok(other)) = (row.get::<Relation>("r"), row.get::<Node>("m")) else {
            continue;
        };
        let rel_iri = rel.get::<String>("iri").unwrap_or_default();
        if !seen.insert(rel_iri) {
            continue;
        }
        let outgoing = row.get::<bool>("outgoing").unwrap_or(false);
        let other_iri = other.get::<String>("iri").unwrap_or_default();
        let (from, to) = if outgoing {
            (args.iri.clone(), other_iri)
        } else {
            (other_iri, args.iri.clone())
        };
        neighbors.push(json!({
            "edge": rel_json(&rel, &from, &to),
            "node": node_json(&other),
            "direction": if outgoing { "out" } else { "in" },
        }));
    }
    Ok(json!({
        "found": true,
        "node": node_json(&node),
        "hops": 1,
        "neighbors": neighbors,
        "layers": layers,
    }))
}

#[cfg(test)]
#[allow(dead_code)]
struct WakeFact {
    s: Value,
    s_iri: String,
    s_labels: Vec<String>,
    p: String,
    o: Value,
    o_iri: String,
    relationship: Value,
    relationship_iri: String,
    layers: Vec<String>,
    score: f64,
    effective_weight: i64,
    spike: Option<String>,
}

#[cfg(test)]
#[allow(dead_code)]
fn lucene_escape(text: &str) -> String {
    let mut out = String::from("\"");
    for ch in text.chars() {
        if "+-&|!(){}[]^\"~*?:\\/".contains(ch) {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

#[cfg(test)]
#[allow(dead_code)]
pub(crate) async fn legacy_memory_search(graph: &Graph, args: SearchArgs) -> Result<Value> {
    let layers = normalize_layers(args.layers)?;
    let limit = args.limit.unwrap_or(20).clamp(1, 100) as usize;
    let labels = args.labels.unwrap_or_default();
    let text = args.text.unwrap_or_default();
    let trimmed = text.trim().to_string();
    let needle = trimmed.to_ascii_lowercase();
    if trimmed.is_empty() && labels.is_empty() {
        return Ok(json!({
            "query": Value::Null,
            "mode": "wakeup",
            "facts": [],
            "spike": [],
            "layers": layers,
        }));
    }

    let mut node_scores: HashMap<String, f64> = HashMap::new();
    let mut rel_scores: HashMap<String, f64> = HashMap::new();
    if !trimmed.is_empty() {
        let escaped = lucene_escape(&trimmed);
        for row in fetch_all(
            graph,
            query(
                "CALL db.index.fulltext.queryNodes('wakeup_nodes', $q) YIELD node, score \
                 RETURN node.iri AS iri, score ORDER BY score DESC, iri ASC",
            )
            .param("q", escaped.clone()),
        )
        .await?
        {
            if let (Ok(iri), Ok(score)) = (row.get::<String>("iri"), row.get::<f64>("score")) {
                let entry = node_scores.entry(iri).or_insert(0.0);
                *entry = entry.max(score);
            }
        }
        for row in fetch_all(
            graph,
            query(
                "CALL db.index.fulltext.queryRelationships('wakeup_facts', $q) \
                 YIELD relationship, score RETURN relationship.iri AS iri, score \
                 ORDER BY score DESC, iri ASC",
            )
            .param("q", escaped),
        )
        .await?
        {
            if let (Ok(iri), Ok(score)) = (row.get::<String>("iri"), row.get::<f64>("score")) {
                let entry = rel_scores.entry(iri).or_insert(0.0);
                *entry = entry.max(score);
            }
        }
    }
    let use_contains = trimmed.is_empty();
    let iris = node_scores.keys().cloned().collect::<Vec<_>>();
    let relation_iris = rel_scores.keys().cloned().collect::<Vec<_>>();
    let rows = fetch_all(
        graph,
        query(
            r#"
            MATCH (s:Entity)-[r]->(o:Entity)
            WHERE r.validTo IS NULL
              AND (type(r) = 'ASSERTS' OR type(r) = 'ABOUT')
              AND (size(coalesce(s.layers, [])) = 0
                   OR any(layer IN coalesce(s.layers, []) WHERE layer IN $layers))
              AND (size(coalesce(r.layers, [])) = 0
                   OR any(layer IN coalesce(r.layers, []) WHERE layer IN $layers))
              AND (size(coalesce(o.layers, [])) = 0
                   OR any(layer IN coalesce(o.layers, []) WHERE layer IN $layers))
              AND ($labelCount = 0 OR any(label IN $labels WHERE label IN labels(s) OR label IN labels(o)))
              AND (
                ($useContains AND (
                  $text = ''
                  OR toLower(coalesce(r.factText, '')) CONTAINS $text
                  OR toLower(coalesce(s.name, '')) CONTAINS $text
                  OR toLower(s.iri) CONTAINS $text
                  OR toLower(coalesce(s.searchText, '')) CONTAINS $text
                  OR toLower(coalesce(s.value, '')) CONTAINS $text
                  OR toLower(coalesce(o.name, '')) CONTAINS $text
                  OR toLower(o.iri) CONTAINS $text
                  OR toLower(coalesce(o.value, '')) CONTAINS $text
                  OR toLower(coalesce(o.searchText, '')) CONTAINS $text
                ))
                OR (NOT $useContains AND (s.iri IN $iris OR o.iri IN $iris OR r.iri IN $relationIris))
              )
            RETURN s, r, o
            "#,
        )
        .param("layers", layers.clone())
        .param("labels", labels.clone())
        .param("labelCount", labels.len() as i64)
        .param("useContains", use_contains)
        .param("text", needle)
        .param("iris", iris)
        .param("relationIris", relation_iris),
    )
    .await?;

    let mut facts = Vec::new();
    let mut seen_facts = HashSet::new();
    let mut element_iris = HashSet::new();
    for row in rows {
        let (Ok(s), Ok(r), Ok(o)) = (
            row.get::<Node>("s"),
            row.get::<Relation>("r"),
            row.get::<Node>("o"),
        ) else {
            continue;
        };
        let relationship_iri = r.get::<String>("iri").unwrap_or_default();
        if relationship_iri.is_empty() || !seen_facts.insert(relationship_iri.clone()) {
            continue;
        }
        let s_iri = s.get::<String>("iri").unwrap_or_default();
        let o_iri = o.get::<String>("iri").unwrap_or_default();
        let p = r
            .get::<String>("propertyIri")
            .unwrap_or_else(|_| format!("mindreader:property/{}", r.typ()));
        let mut score = 1.0_f64;
        if let Some(value) = node_scores.get(&s_iri) {
            score = score.max(*value);
        }
        if let Some(value) = node_scores.get(&o_iri) {
            score = score.max(*value);
        }
        if let Some(value) = rel_scores.get(&relationship_iri) {
            score = score.max(*value);
        }
        element_iris.insert(s_iri.clone());
        let o_labels = o
            .labels()
            .into_iter()
            .filter(|label| *label != "Entity")
            .map(str::to_string)
            .collect::<Vec<_>>();
        if o_labels.iter().any(|label| label == "Element") {
            element_iris.insert(o_iri.clone());
        }
        facts.push(WakeFact {
            s: endpoint_json(&s),
            s_iri,
            s_labels: s
                .labels()
                .into_iter()
                .filter(|label| *label != "Entity")
                .map(str::to_string)
                .collect(),
            p,
            o: endpoint_json(&o),
            o_iri,
            relationship: rel_json(
                &r,
                &s.get::<String>("iri").unwrap_or_default(),
                &o.get::<String>("iri").unwrap_or_default(),
            ),
            relationship_iri,
            layers: r.get::<Vec<String>>("layers").unwrap_or_default(),
            score,
            effective_weight: effective_weight(weight(&s), relation_weight(&r), weight(&o)),
            spike: None,
        });
    }

    let about_iris = element_iris.into_iter().collect::<Vec<_>>();
    let mut spike_by_about: HashMap<String, (String, i64, Value)> = HashMap::new();
    let mut spike_list = Vec::new();
    let mut seen_spike = HashSet::new();
    if !about_iris.is_empty() {
        for row in fetch_all(
            graph,
            query(
                r#"
                MATCH (sp:Entity)-[a:ABOUT]->(el:Entity)
                WHERE a.validTo IS NULL AND el.iri IN $iris
                  AND (sp:Knowledge OR sp:Insight OR sp:Pattern OR sp:Signal)
                  AND (size(coalesce(sp.layers, [])) = 0
                       OR any(layer IN coalesce(sp.layers, []) WHERE layer IN $layers))
                  AND (size(coalesce(a.layers, [])) = 0
                       OR any(layer IN coalesce(a.layers, []) WHERE layer IN $layers))
                  AND (size(coalesce(el.layers, [])) = 0
                       OR any(layer IN coalesce(el.layers, []) WHERE layer IN $layers))
                RETURN sp, a, el
                "#,
            )
            .param("layers", layers.clone())
            .param("iris", about_iris),
        )
        .await?
        {
            let (Ok(sp), Ok(about_rel), Ok(element)) = (
                row.get::<Node>("sp"),
                row.get::<Relation>("a"),
                row.get::<Node>("el"),
            ) else {
                continue;
            };
            let labels = sp
                .labels()
                .into_iter()
                .filter(|label| *label != "Entity")
                .map(str::to_string)
                .collect::<Vec<_>>();
            let Some(rank) = spike_label(&labels) else {
                continue;
            };
            let about = element.get::<String>("iri").unwrap_or_default();
            let sp_iri = sp.get::<String>("iri").unwrap_or_default();
            let relationship = rel_json(&about_rel, &sp_iri, &about);
            let combined =
                effective_weight(weight(&sp), relation_weight(&about_rel), weight(&element));
            let rel_iri = about_rel.get::<String>("iri").unwrap_or_default();
            if seen_spike.insert(rel_iri) {
                spike_list.push(json!({
                    "node": node_json(&sp),
                    "about": about,
                    "rank": rank,
                    "relationship": relationship,
                    "effectiveWeight": combined,
                }));
            }
            let better =
                spike_by_about
                    .get(&about)
                    .is_none_or(|(current_rank, current_weight, _)| {
                        spike_rank(Some(&rank)) > spike_rank(Some(current_rank))
                            || (rank == *current_rank && combined > *current_weight)
                    });
            if better {
                spike_by_about.insert(about, (rank, combined, node_json(&sp)));
            }
        }
    }
    for fact in &mut facts {
        if let Some(own) = spike_label(&fact.s_labels) {
            fact.spike = Some(own);
        } else if let Some((rank, _, _)) = spike_by_about.get(&fact.s_iri) {
            fact.spike = Some(rank.clone());
        }
    }
    facts.sort_by(|a, b| {
        spike_rank(b.spike.as_deref())
            .cmp(&spike_rank(a.spike.as_deref()))
            .then_with(|| b.effective_weight.cmp(&a.effective_weight))
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.s_iri.cmp(&b.s_iri))
            .then_with(|| a.p.cmp(&b.p))
            .then_with(|| a.o_iri.cmp(&b.o_iri))
            .then_with(|| a.relationship_iri.cmp(&b.relationship_iri))
    });
    facts.truncate(limit);
    spike_list.sort_by(|a, b| {
        spike_rank(b.get("rank").and_then(Value::as_str))
            .cmp(&spike_rank(a.get("rank").and_then(Value::as_str)))
            .then_with(|| {
                b.get("effectiveWeight")
                    .and_then(Value::as_i64)
                    .cmp(&a.get("effectiveWeight").and_then(Value::as_i64))
            })
            .then_with(|| {
                a.get("about")
                    .and_then(Value::as_str)
                    .cmp(&b.get("about").and_then(Value::as_str))
            })
            .then_with(|| {
                a.pointer("/node/iri")
                    .and_then(Value::as_str)
                    .cmp(&b.pointer("/node/iri").and_then(Value::as_str))
            })
    });
    let facts = facts
        .into_iter()
        .map(|fact| {
            json!({
                "s": fact.s,
                "p": fact.p,
                "o": fact.o,
                "relationship": fact.relationship,
                "layers": fact.layers,
                "spike": fact.spike,
                "score": fact.score,
                "effectiveWeight": fact.effective_weight,
            })
        })
        .collect::<Vec<_>>();
    Ok(json!({
        "query": if trimmed.is_empty() { Value::Null } else { json!(trimmed) },
        "mode": "wakeup",
        "facts": facts,
        "spike": spike_list,
        "layers": layers,
    }))
}

/// Return model readiness and counters for entities and facts in the layer union.
pub async fn memory_stats(graph: &Graph, args: StatsArgs) -> Result<Value> {
    let layers = normalize_layers(args.layers)?;
    let row = fetch_one(
        graph,
        query(
            r#"
            CALL {
              MATCH (n:Entity)
              WHERE size(coalesce(n.layers, [])) = 0
                 OR any(layer IN coalesce(n.layers, []) WHERE layer IN $layers)
              RETURN count(n) AS nodes
            }
            CALL {
              MATCH (s:Entity)-[r]->(o:Entity)
              WHERE r.validTo IS NULL
                AND (size(coalesce(s.layers, [])) = 0 OR any(layer IN coalesce(s.layers, []) WHERE layer IN $layers))
                AND (size(coalesce(r.layers, [])) = 0 OR any(layer IN coalesce(r.layers, []) WHERE layer IN $layers))
                AND (size(coalesce(o.layers, [])) = 0 OR any(layer IN coalesce(o.layers, []) WHERE layer IN $layers))
              RETURN count(r) AS activeEdges
            }
            CALL {
              MATCH (s:Entity)-[r]->(o:Entity)
              WHERE r.validTo IS NOT NULL
                AND (size(coalesce(s.layers, [])) = 0 OR any(layer IN coalesce(s.layers, []) WHERE layer IN $layers))
                AND (size(coalesce(r.layers, [])) = 0 OR any(layer IN coalesce(r.layers, []) WHERE layer IN $layers))
                AND (size(coalesce(o.layers, [])) = 0 OR any(layer IN coalesce(o.layers, []) WHERE layer IN $layers))
              RETURN count(r) AS historicalEdges
            }
            CALL { MATCH (e:Entity:Episode) RETURN count(e) AS episodes }
            RETURN nodes, activeEdges, historicalEdges, episodes
            "#,
        )
        .param("layers", layers.clone()),
    )
    .await?;
    let (nodes, active_edges, historical_edges, episodes) = row
        .map(|row| {
            (
                row.get::<i64>("nodes").unwrap_or(0),
                row.get::<i64>("activeEdges").unwrap_or(0),
                row.get::<i64>("historicalEdges").unwrap_or(0),
                row.get::<i64>("episodes").unwrap_or(0),
            )
        })
        .unwrap_or_default();
    let by_layer = fetch_all(
        graph,
        query(
            r#"
            MATCH (s:Entity)-[r]->(o:Entity)
            WHERE r.validTo IS NULL
              AND (size(coalesce(s.layers, [])) = 0 OR any(layer IN coalesce(s.layers, []) WHERE layer IN $layers))
              AND (size(coalesce(r.layers, [])) = 0 OR any(layer IN coalesce(r.layers, []) WHERE layer IN $layers))
              AND (size(coalesce(o.layers, [])) = 0 OR any(layer IN coalesce(o.layers, []) WHERE layer IN $layers))
            UNWIND CASE WHEN size(coalesce(r.layers, [])) = 0 THEN [null] ELSE r.layers END AS layer
            RETURN layer, count(r) AS count ORDER BY count DESC, layer ASC
            "#,
        )
        .param("layers", layers.clone()),
    )
    .await?
    .into_iter()
    .map(|row| {
        json!({
            "layer": row.get::<String>("layer").ok(),
            "global": row.get::<String>("layer").is_err(),
            "count": row.get::<i64>("count").unwrap_or(0),
        })
    })
    .collect::<Vec<_>>();
    let marker_version = fetch_one(
        graph,
        query("MATCH (m:MindreaderMeta {key: $key}) RETURN m.version AS version")
            .param("key", MODEL_MARKER_KEY),
    )
    .await?
    .and_then(|row| row.get::<i64>("version").ok());
    let index_rows = fetch_all(
        graph,
        query(
            "SHOW INDEXES YIELD name, state WHERE name IN \
             ['wakeup_nodes', 'wakeup_facts', 'merge_candidate_names'] RETURN name, state",
        ),
    )
    .await?;
    let indexes_online = ["wakeup_nodes", "wakeup_facts", MERGE_CANDIDATE_INDEX]
        .iter()
        .all(|required| {
            index_rows.iter().any(|row| {
                row.get::<String>("name").ok().as_deref() == Some(*required)
                    && row.get::<String>("state").ok().as_deref() == Some("ONLINE")
            })
        });
    let constraint_rows = fetch_all(
        graph,
        query(
            "SHOW CONSTRAINTS YIELD name WHERE name IN \
             ['mindreader_meta_key', 'entity_iri', 'fact_lock_key'] RETURN name",
        ),
    )
    .await?;
    let constraints_present = ["mindreader_meta_key", "entity_iri", "fact_lock_key"]
        .iter()
        .all(|required| {
            constraint_rows
                .iter()
                .any(|row| row.get::<String>("name").ok().as_deref() == Some(*required))
        });
    Ok(json!({
        "layers": layers,
        "model": {
            "marker": MODEL_MARKER_KEY,
            "version": marker_version,
            "requiredVersion": MODEL_VERSION,
            "indexesOnline": indexes_online,
            "constraintsPresent": constraints_present,
            "ready": marker_version == Some(MODEL_VERSION) && indexes_online && constraints_present,
        },
        "counts": {
            "nodes": nodes,
            "activeEdges": active_edges,
            "historicalEdges": historical_edges,
            "episodes": episodes,
        },
        "activeEdgesByLayer": by_layer,
    }))
}

/// Walk typed edges from a visible start IRI (depth capped at 3) under layer closure.
pub async fn memory_traverse(graph: &Graph, args: TraverseArgs) -> Result<Value> {
    let layers = normalize_layers(args.layers)?;
    let depth = args.depth.unwrap_or(1).clamp(1, 3);
    let limit = args.limit.unwrap_or(50).clamp(1, 200) as i64;
    let rels = if let Some(values) = args.rels.filter(|values| !values.is_empty()) {
        values
            .into_iter()
            .map(|relationship| {
                safe_rel(&relationship).map_err(|_| {
                    DomainError::InvalidInput(format!(
                        "invalid relationship type: {relationship:?}"
                    ))
                    .into()
                })
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        FIXED_RELS.iter().map(|rel| (*rel).to_string()).collect()
    };
    let q = format!(
        r#"
        MATCH path = (start:Entity {{iri: $from}})-[pathRels*1..{depth}]-(x:Entity)
        WHERE all(n IN nodes(path) WHERE
          size(coalesce(n.layers, [])) = 0
          OR any(layer IN coalesce(n.layers, []) WHERE layer IN $layers))
          AND all(r IN relationships(path) WHERE
            type(r) IN $rels AND r.validTo IS NULL
            AND (size(coalesce(r.layers, [])) = 0
                 OR any(layer IN coalesce(r.layers, []) WHERE layer IN $layers)))
        RETURN path LIMIT $limit
        "#
    );
    let rows = fetch_all(
        graph,
        query(&q)
            .param("from", args.from.clone())
            .param("layers", layers.clone())
            .param("rels", rels.clone())
            .param("limit", limit),
    )
    .await?;
    if rows.is_empty() {
        let found = fetch_one(
            graph,
            query(
                "MATCH (n:Entity {iri: $iri}) WHERE size(coalesce(n.layers, [])) = 0 \
                 OR any(layer IN coalesce(n.layers, []) WHERE layer IN $layers) RETURN n.iri AS iri",
            )
            .param("iri", args.from.clone())
            .param("layers", layers.clone()),
        )
        .await?
        .is_some();
        if !found {
            return Ok(json!({
                "found": false, "from": args.from, "paths": [], "nodes": [], "edges": [],
                "layers": layers,
            }));
        }
    }
    let mut nodes_by_iri = serde_json::Map::new();
    let mut edges = Vec::new();
    let mut edge_seen = HashSet::new();
    let mut paths = Vec::new();
    for row in rows {
        let Ok(path) = row.get::<Path>("path") else {
            continue;
        };
        let (nodes, path_edges, iris) = path_to_json(&path);
        for node in nodes {
            if let Some(iri) = node.get("iri").and_then(Value::as_str) {
                nodes_by_iri.insert(iri.to_string(), node);
            }
        }
        for edge in &path_edges {
            let key = edge.get("iri").and_then(Value::as_str).unwrap_or_default();
            if edge_seen.insert(key.to_string()) {
                edges.push(edge.clone());
            }
        }
        paths.push(json!({ "nodes": iris, "edges": path_edges }));
    }
    Ok(json!({
        "found": true,
        "from": args.from,
        "depth": depth,
        "layers": layers,
        "rels": rels,
        "paths": paths,
        "nodes": nodes_by_iri.values().cloned().collect::<Vec<_>>(),
        "edges": edges,
    }))
}

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
              AND (size(coalesce(s.layers, [])) = 0 OR any(layer IN coalesce(s.layers, []) WHERE layer IN $layers))
              AND (size(coalesce(r.layers, [])) = 0 OR any(layer IN coalesce(r.layers, []) WHERE layer IN $layers))
              AND (size(coalesce(o.layers, [])) = 0 OR any(layer IN coalesce(o.layers, []) WHERE layer IN $layers))
              AND (($isStructural AND type(r) = $relType)
                OR (NOT $isStructural AND type(r) = 'ASSERTS' AND r.propertyIri = $p))
            RETURN r, o, coalesce(r.propertyIri, $p) AS p
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
        let property = row.get::<String>("p").unwrap_or_else(|_| prop_iri.into());
        Ok(json!({
            "relationship": rel_json(
                &relationship,
                s,
                &object.get::<String>("iri").unwrap_or_default(),
            ),
            "o": endpoint_json(&object),
            "p": property,
        }))
    })
    .collect()
}

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

/// Assert one exact set-valued triple, merging memberships on reassert; no-ops record no Episode.
pub async fn memory_assert(graph: &Graph, args: AssertArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_assert_once(graph, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

async fn memory_assert_once(graph: &Graph, args: AssertArgs) -> Result<Value> {
    let layers = normalize_layers(args.layers)?;
    let predicate = PredicateRef::parse(&args.p)?;
    reject_system_owned_predicate(predicate.iri())?;
    let spike = SpikeRank::parse(args.spike)?.map(|rank| rank.as_str().to_string());
    let mut subject_spec = node_spec(EntityRef::from_input(args.s)?);
    if let Some(spike) = &spike {
        if !subject_spec.labels.contains(spike) {
            subject_spec.labels.push(spike.clone());
        }
    }
    let subject_kind = spike
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "element".into());
    let subject_iri = EntityRef {
        iri: subject_spec.iri.clone(),
        name: subject_spec.name.clone(),
        labels: subject_spec.labels.clone(),
    }
    .resolved_iri(&subject_kind);
    let object_value = ObjectValue::from_input(args.o)?;
    let object_iri = object_value.resolved_iri();
    let prop_iri = predicate.iri().to_string();
    let structural = structural_rel_for(&prop_iri);
    let locks = assert_fact_lock_requests(&subject_iri, &prop_iri, &object_iri, args.contradicts);
    let mut txn = graph.start_txn().await?;
    let write = async {
        acquire_fact_locks_in_txn(&mut txn, &locks).await?;
        let subject = merge_node_in_txn(
            &mut txn,
            &subject_spec,
            &subject_kind,
            &spike.clone().into_iter().collect::<Vec<_>>(),
        )
        .await?;
        let (object, object_is_literal) = merge_object_in_txn(&mut txn, object_value).await?;
        let (_, property_created, property) = ensure_property_in_txn(&mut txn, &prop_iri).await?;
        let episode = create_episode_in_txn(&mut txn, "memory_assert", None).await?;
        let mut changed = property_created;
        changed |= apply_node_memberships_txn(&mut txn, &subject, &layers).await?;
        changed |= apply_node_memberships_txn(&mut txn, &object, &layers).await?;
        let fact_text_value = fact_text(&subject.name, &subject.iri, &prop_iri, &object);
        let (relationship_iri, relationship_changed) = ensure_relation_txn(
            &mut txn,
            &RelationWrite {
                rel_type: structural.as_deref().unwrap_or("ASSERTS"),
                s: &subject.iri,
                o: &object.iri,
                prop_iri: &prop_iri,
                layers: &layers,
                episode: &episode,
                reason: None,
                fact_text: &fact_text_value,
            },
        )
        .await?;
        changed |= relationship_changed;
        let need_about = spike.is_some()
            && !object_is_literal
            && object.labels.iter().any(|label| label == "Element")
            && structural.as_deref() != Some("ABOUT");
        if need_about {
            let about_text = fact_text(
                &subject.name,
                &subject.iri,
                "mindreader:property/ABOUT",
                &object,
            );
            let (_, about_changed) = ensure_relation_txn(
                &mut txn,
                &RelationWrite {
                    rel_type: "ABOUT",
                    s: &subject.iri,
                    o: &object.iri,
                    prop_iri: "mindreader:property/ABOUT",
                    layers: &layers,
                    episode: &episode,
                    reason: None,
                    fact_text: &about_text,
                },
            )
            .await?;
            changed |= about_changed;
        }
        let conflicts = find_conflicts_txn(
            &mut txn,
            &subject.iri,
            &prop_iri,
            structural.as_deref(),
            &object.iri,
            &layers,
        )
        .await?;
        if args.contradicts {
            changed |=
                ensure_contradictions_txn(&mut txn, &object.iri, &conflicts, &layers, &episode)
                    .await?;
        }
        let subject_json = refreshed_node_json_txn(&mut txn, &subject.iri).await?;
        let object_json = refreshed_node_json_txn(&mut txn, &object.iri).await?;
        let mut created_iris = Vec::new();
        if subject.created {
            created_iris.push(subject.iri.clone());
        }
        if object.created && !object_is_literal {
            created_iris.push(object.iri.clone());
        }
        if property_created {
            created_iris.push(prop_iri.clone());
        }
        let merge_suggestions = merge_suggestions_in_txn(&mut txn, &created_iris, &layers).await?;
        Ok::<_, Error>((
            changed,
            episode,
            subject_json,
            object_json,
            property_created,
            property,
            conflicts,
            relationship_iri,
            merge_suggestions,
        ))
    }
    .await;
    let (
        changed,
        episode,
        subject,
        object,
        property_created,
        property,
        conflicts,
        relationship_iri,
        merge_suggestions,
    ) = match write {
        Ok(value) => {
            if value.0 {
                txn.commit()
                    .await
                    .context("commit memory_assert transaction failed")?;
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
    Ok(json!({
        "noop": !changed,
        "s": subject,
        "p": prop_iri,
        "o": object,
        "relationship": { "iri": relationship_iri },
        "layers": layers,
        "episode": if changed { json!({ "iri": episode.iri, "at": episode.at, "tool": episode.tool }) } else { Value::Null },
        "propertyStub": property_created,
        "property": property,
        "spike": spike,
        "conflicts": conflicts,
        "mergeSuggestions": merge_suggestions,
    }))
}

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
                 SET r.validTo = datetime(), r.retractedBy = $episode, \
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
              SET r.validTo = datetime(), r.retractedBy = $episode,
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

/// Correct one exact fact in the listed memberships and record SUPERSEDES in the same transaction.
pub async fn memory_replace(graph: &Graph, args: ReplaceArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_replace_once(graph, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

async fn memory_replace_once(graph: &Graph, args: ReplaceArgs) -> Result<Value> {
    let layers = normalize_layers(args.layers)?;
    let predicate = PredicateRef::parse(&args.p)?;
    reject_system_owned_predicate(predicate.iri())?;
    let spike = SpikeRank::parse(args.spike)?.map(|rank| rank.as_str().to_string());
    let mut subject_spec = node_spec(EntityRef::from_input(args.s)?);
    if let Some(spike) = &spike {
        if !subject_spec.labels.contains(spike) {
            subject_spec.labels.push(spike.clone());
        }
    }
    let subject_kind = spike
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "element".into());
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
    let locks = replace_fact_lock_requests(
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
    let old_current = old_currents
        .first()
        .filter(|current| remove_memberships(&current.layers, &layers).is_some())
        .cloned()
        .ok_or_else(|| {
            DomainError::Precondition(format!(
                "cannot replace the selected memberships of non-current fact ({subject_iri}, {prop_iri}, {old_iri})"
            ))
        })?;
    if old_iri == new_iri {
        txn.rollback().await?;
        return Ok(json!({
            "noop": true,
            "s": subject_iri,
            "p": prop_iri,
            "old": old_iri,
            "new": new_iri,
            "layers": layers,
            "episode": Value::Null,
        }));
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
        let (_, property_created, property) = ensure_property_in_txn(&mut txn, &prop_iri).await?;
        let episode =
            create_episode_in_txn(&mut txn, "memory_replace", args.reason.as_deref()).await?;
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
            property_created,
            property,
            conflicts,
            merge_suggestions,
        ))
    }
    .await;
    let (
        episode,
        subject,
        new,
        relationship_iri,
        property_created,
        property,
        conflicts,
        merge_suggestions,
    ) = match result {
        Ok(value) => {
            txn.commit()
                .await
                .context("commit memory_replace transaction failed")?;
            value
        }
        Err(error) => {
            let _ = txn.rollback().await;
            return Err(error);
        }
    };
    Ok(json!({
        "noop": false,
        "s": subject,
        "p": prop_iri,
        "old": old_iri,
        "new": new,
        "relationship": { "iri": relationship_iri },
        "layers": layers,
        "episode": { "iri": episode.iri, "at": episode.at, "tool": episode.tool },
        "propertyStub": property_created,
        "property": property,
        "spike": spike,
        "conflicts": conflicts,
        "mergeSuggestions": merge_suggestions,
    }))
}

/// Remove selected fact memberships and set `validTo` after the last one is removed.
pub async fn memory_retract(graph: &Graph, args: RetractArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_retract_once(graph, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

async fn memory_retract_once(graph: &Graph, args: RetractArgs) -> Result<Value> {
    let layers = normalize_layers(args.layers)?;
    let scope = RetractScope::parse(&args.target.kind)?;
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
        RetractScope::Fact if predicate.is_none() || object_iri.is_none() => {
            return Err(DomainError::InvalidInput(
                "fact retraction requires target.p and target.o".into(),
            )
            .into())
        }
        RetractScope::Predicate if predicate.is_none() || object_iri.is_some() => {
            return Err(DomainError::InvalidInput(
                "predicate retraction requires target.p and forbids target.o".into(),
            )
            .into())
        }
        RetractScope::Subject if predicate.is_some() || object_iri.is_some() => {
            return Err(DomainError::InvalidInput(
                "subject retraction forbids target.p and target.o".into(),
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
    let locks = retract_fact_lock_requests(&subject_iri, predicate.as_deref());
    let mut txn = graph.start_txn().await?;
    acquire_fact_locks_in_txn(&mut txn, &locks).await?;
    let rows = match scope {
        RetractScope::Subject => {
            fetch_all_txn(
                &mut txn,
                query(
                    r#"
                    MATCH (s:Entity {iri: $s})-[r]->(o:Entity)
                    WHERE r.validTo IS NULL
                      AND NOT type(r) IN $protected
                      AND NOT s:Class AND NOT s:Property
                      AND NOT o:Class AND NOT o:Property
                    RETURN id(r) AS rid, r.iri AS iri, coalesce(r.layers, []) AS layers
                    "#,
                )
                .param("s", subject_iri.clone())
                .param("protected", protected),
            )
            .await?
        }
        RetractScope::Fact | RetractScope::Predicate => {
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
            let cypher = if let Some(rel) = rel_clause {
                format!(
                    "MATCH (s:Entity {{iri: $s}})-[r{rel}]->(o:Entity) \
                     WHERE r.validTo IS NULL {object_clause} \
                     RETURN id(r) AS rid, r.iri AS iri, coalesce(r.layers, []) AS layers"
                )
            } else {
                format!(
                    "MATCH (s:Entity {{iri: $s}})-[r:ASSERTS]->(o:Entity) \
                     WHERE r.validTo IS NULL AND r.propertyIri = $p {object_clause} \
                     RETURN id(r) AS rid, r.iri AS iri, coalesce(r.layers, []) AS layers"
                )
            };
            let mut q = query(&cypher)
                .param("s", subject_iri.clone())
                .param("p", predicate.to_string());
            if let Some(object) = &object_iri {
                q = q.param("o", object.clone());
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
                layers: row.get("layers").unwrap_or_default(),
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let changes = plan_fact_membership_changes(&currents, &layers);
    if changes.is_empty() {
        txn.rollback().await?;
        return Ok(json!({
            "retracted": 0,
            "soft": true,
            "layers": layers,
            "episode": Value::Null,
            "reason": args.reason,
        }));
    }
    let episode = create_episode_in_txn(&mut txn, "memory_retract", args.reason.as_deref()).await?;
    change_fact_memberships_batch_txn(&mut txn, &changes, &episode, args.reason.as_deref()).await?;
    txn.commit()
        .await
        .context("commit memory_retract transaction failed")?;
    Ok(json!({
        "retracted": changes.len(),
        "soft": true,
        "layers": layers,
        "episode": { "iri": episode.iri, "at": episode.at, "tool": episode.tool },
        "reason": args.reason,
    }))
}

fn validate_target(target: &TargetArgs) -> Result<()> {
    if !matches!(target.kind.as_str(), "node" | "relationship") {
        return Err(
            DomainError::InvalidInput("target.kind must be node or relationship".into()).into(),
        );
    }
    if target.iri.trim().is_empty() {
        return Err(DomainError::InvalidInput("target.iri cannot be empty".into()).into());
    }
    Ok(())
}

/// Apply explicit strengthen (+1) or weaken (−1) to a visible current node or relationship weight.
pub async fn memory_feedback(graph: &Graph, args: FeedbackArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_feedback_once(graph, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

async fn memory_feedback_once(graph: &Graph, args: FeedbackArgs) -> Result<Value> {
    validate_target(&args.target)?;
    let layers = normalize_layers(args.layers)?;
    let delta = match args.mode.as_str() {
        "strengthen" => 1_i64,
        "weaken" => -1_i64,
        _ => {
            return Err(
                DomainError::InvalidInput("mode must be strengthen or weaken".into()).into(),
            )
        }
    };
    let strengthen = delta > 0;
    let mut txn = graph.start_txn().await?;
    acquire_fact_locks_in_txn(
        &mut txn,
        &[(
            args.target.iri.clone(),
            "mindreader:property/weight".into(),
            FACT_LOCK_SCOPE.into(),
        )],
    )
    .await?;
    let update = if args.target.kind == "node" {
        fetch_one_txn(
            &mut txn,
            query(
                r#"
                MATCH (target:Entity {iri: $iri})
                WHERE size(coalesce(target.layers, [])) = 0
                   OR any(layer IN coalesce(target.layers, []) WHERE layer IN $layers)
                WITH target, coalesce(target.weight, 0) AS before
                WHERE ($strengthen AND before < $max) OR (NOT $strengthen AND before > $min)
                SET target.weight = CASE WHEN $strengthen THEN before + 1 ELSE before - 1 END,
                    target.weightText = toString(CASE WHEN $strengthen THEN before + 1 ELSE before - 1 END)
                RETURN toString(before) AS before, target.weightText AS after
                "#,
            )
            .param("iri", args.target.iri.clone())
            .param("layers", layers.clone())
            .param("strengthen", strengthen)
            .param("max", i64::MAX)
            .param("min", i64::MIN),
        )
        .await?
    } else {
        fetch_one_txn(
            &mut txn,
            query(
                r#"
                MATCH (s:Entity)-[target]->(o:Entity)
                WHERE target.iri = $iri AND target.validTo IS NULL
                  AND (size(coalesce(s.layers, [])) = 0 OR any(layer IN coalesce(s.layers, []) WHERE layer IN $layers))
                  AND (size(coalesce(target.layers, [])) = 0 OR any(layer IN coalesce(target.layers, []) WHERE layer IN $layers))
                  AND (size(coalesce(o.layers, [])) = 0 OR any(layer IN coalesce(o.layers, []) WHERE layer IN $layers))
                WITH target, coalesce(target.weight, 0) AS before
                WHERE ($strengthen AND before < $max) OR (NOT $strengthen AND before > $min)
                SET target.weight = CASE WHEN $strengthen THEN before + 1 ELSE before - 1 END,
                    target.weightText = toString(CASE WHEN $strengthen THEN before + 1 ELSE before - 1 END)
                RETURN toString(before) AS before, target.weightText AS after
                "#,
            )
            .param("iri", args.target.iri.clone())
            .param("layers", layers.clone())
            .param("strengthen", strengthen)
            .param("max", i64::MAX)
            .param("min", i64::MIN),
        )
        .await?
    };
    let Some(update) = update else {
        txn.rollback().await?;
        return Err(DomainError::Precondition(
            "feedback target is missing, hidden, historical, or its weight cannot be incremented without overflow"
                .into(),
        )
        .into());
    };
    let before_text = update
        .get::<String>("before")
        .unwrap_or_else(|_| "0".into());
    let after_text = update
        .get::<String>("after")
        .unwrap_or_else(|_| before_text.clone());
    let before = before_text.parse::<i64>().unwrap_or(0);
    let after = after_text.parse::<i64>().unwrap_or(before);
    let episode = create_episode_in_txn(&mut txn, "memory_feedback", Some(&args.mode)).await?;
    txn.run(
        query(
            "MATCH (e:Entity:Episode {iri: $episode}) \
             SET e.targetIri = $targetIri, e.targetKind = $targetKind, \
                 e.mode = $mode, e.delta = CASE WHEN $strengthen THEN 1 ELSE -1 END, \
                 e.beforeWeight = toInteger($before), \
                 e.afterWeight = toInteger($after)",
        )
        .param("episode", episode.iri.clone())
        .param("targetIri", args.target.iri.clone())
        .param("targetKind", args.target.kind.clone())
        .param("mode", args.mode.clone())
        .param("strengthen", strengthen)
        .param("before", before_text)
        .param("after", after_text),
    )
    .await?;
    let target_match = if args.target.kind == "node" {
        "MATCH (target:Entity {iri: $iri})"
    } else {
        "MATCH ()-[target]->() WHERE target.iri = $iri"
    };
    txn.run(
        query(&format!(
            "{target_match} SET target.weightUpdatedAt = datetime(), target.feedbackEpisodeId = $episode"
        ))
        .param("iri", args.target.iri.clone())
        .param("episode", episode.iri.clone()),
    )
    .await?;
    txn.commit()
        .await
        .context("commit memory_feedback transaction failed")?;
    Ok(json!({
        "target": args.target,
        "mode": args.mode,
        "delta": delta,
        "before": before,
        "weight": after,
        "layers": layers,
        "episode": { "iri": episode.iri, "at": episode.at, "tool": episode.tool },
    }))
}

fn membership_allows(record: &[String], required: &[String]) -> bool {
    if required.is_empty() {
        return record.is_empty();
    }
    record.is_empty() || required.iter().all(|layer| record.contains(layer))
}

/// Add or remove layer memberships on a target while preserving relationship endpoint closure.
pub async fn memory_layers(graph: &Graph, args: LayersArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_layers_once(graph, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

async fn memory_layers_once(graph: &Graph, args: LayersArgs) -> Result<Value> {
    validate_target(&args.target)?;
    let scope = normalize_layers(args.layers)?;
    let add = normalize_layers(args.add)?;
    let remove = normalize_layers(args.remove)?;
    if add.is_empty() && remove.is_empty() {
        return Err(DomainError::InvalidInput(
            "memory_layers requires at least one add or remove layer".into(),
        )
        .into());
    }
    if add.iter().any(|layer| remove.contains(layer)) {
        return Err(DomainError::InvalidInput(
            "the same layer cannot be added and removed atomically".into(),
        )
        .into());
    }
    let relationship_fact = if args.target.kind == "relationship" {
        Some(
            fetch_one(
                graph,
                query(
                    "MATCH (s:Entity)-[target]->() \
                 WHERE target.iri = $iri AND target.validTo IS NULL \
                 RETURN s.iri AS subject, \
                   coalesce(target.propertyIri, 'mindreader:property/' + type(target)) AS property",
                )
                .param("iri", args.target.iri.clone()),
            )
            .await?
            .map(|row| {
                Ok::<_, Error>((
                    row.get::<String>("subject")?,
                    row.get::<String>("property")?,
                ))
            })
            .transpose()?
            .ok_or_else(|| {
                DomainError::Precondition("layer target is missing, hidden, or historical".into())
            })?,
        )
    } else {
        None
    };
    let mut locks = vec![(
        args.target.iri.clone(),
        LAYERS_PROPERTY.into(),
        FACT_LOCK_SCOPE.into(),
    )];
    if let Some((subject, property)) = relationship_fact {
        locks.push((subject, property, FACT_LOCK_SCOPE.into()));
    }
    let mut txn = graph.start_txn().await?;
    acquire_fact_locks_in_txn(&mut txn, &locks).await?;
    let row = if args.target.kind == "node" {
        fetch_one_txn(
            &mut txn,
            query(
                r#"
                MATCH (target:Entity {iri: $iri})
                WHERE size(coalesce(target.layers, [])) = 0
                   OR any(layer IN coalesce(target.layers, []) WHERE layer IN $scope)
                RETURN coalesce(target.layers, []) AS before
                "#,
            )
            .param("iri", args.target.iri.clone())
            .param("scope", scope.clone()),
        )
        .await?
    } else {
        fetch_one_txn(
            &mut txn,
            query(
                r#"
                MATCH (s:Entity)-[target]->(o:Entity)
                WHERE target.iri = $iri AND target.validTo IS NULL
                  AND (size(coalesce(s.layers, [])) = 0 OR any(layer IN coalesce(s.layers, []) WHERE layer IN $scope))
                  AND (size(coalesce(target.layers, [])) = 0 OR any(layer IN coalesce(target.layers, []) WHERE layer IN $scope))
                  AND (size(coalesce(o.layers, [])) = 0 OR any(layer IN coalesce(o.layers, []) WHERE layer IN $scope))
                RETURN coalesce(target.layers, []) AS before
                "#,
            )
            .param("iri", args.target.iri.clone())
            .param("scope", scope.clone()),
        )
        .await?
    };
    let Some(row) = row else {
        txn.rollback().await?;
        return Err(DomainError::Precondition(
            "layer target is missing, hidden, or historical".into(),
        )
        .into());
    };
    let before = row.get::<Vec<String>>("before").unwrap_or_default();
    let mut after = before
        .iter()
        .filter(|layer| !remove.contains(layer))
        .cloned()
        .collect::<Vec<_>>();
    after.extend(add);
    after.sort();
    after.dedup();
    if after == before {
        txn.rollback().await?;
        return Ok(json!({
            "noop": true,
            "target": args.target,
            "before": before,
            "layers": after,
            "scope": scope,
            "episode": Value::Null,
        }));
    }
    if args.target.kind == "node" {
        let incident = fetch_all_txn(
            &mut txn,
            query(
                r#"
                MATCH (target:Entity {iri: $iri})-[r]-(other:Entity)
                WHERE r.validTo IS NULL
                RETURN coalesce(r.layers, []) AS relationLayers
                "#,
            )
            .param("iri", args.target.iri.clone()),
        )
        .await?;
        for row in incident {
            let relation_layers = row.get::<Vec<String>>("relationLayers").unwrap_or_default();
            if !membership_allows(&after, &relation_layers) {
                txn.rollback().await?;
                return Err(DomainError::Precondition(
                    "layer edit would expose a relationship while the target endpoint is hidden"
                        .into(),
                )
                .into());
            }
        }
    } else {
        let endpoints = fetch_one_txn(
            &mut txn,
            query(
                "MATCH (s:Entity)-[r]->(o:Entity) WHERE r.iri = $iri AND r.validTo IS NULL \
                 RETURN coalesce(s.layers, []) AS sLayers, coalesce(o.layers, []) AS oLayers",
            )
            .param("iri", args.target.iri.clone()),
        )
        .await?
        .ok_or_else(|| DomainError::Precondition("relationship is no longer current".into()))?;
        let s_layers = endpoints.get::<Vec<String>>("sLayers").unwrap_or_default();
        let o_layers = endpoints.get::<Vec<String>>("oLayers").unwrap_or_default();
        if !membership_allows(&s_layers, &after) || !membership_allows(&o_layers, &after) {
            txn.rollback().await?;
            return Err(DomainError::Precondition(
                "layer edit would expose a relationship while an endpoint is hidden".into(),
            )
            .into());
        }
    }
    let episode = create_episode_in_txn(&mut txn, "memory_layers", None).await?;
    txn.run(
        query(
            "MATCH (e:Entity:Episode {iri: $episode}) \
             SET e.targetIri = $targetIri, e.targetKind = $targetKind, \
                 e.beforeLayers = $before, e.afterLayers = $after, \
                 e.addedLayers = $added, e.removedLayers = $removed",
        )
        .param("episode", episode.iri.clone())
        .param("targetIri", args.target.iri.clone())
        .param("targetKind", args.target.kind.clone())
        .param("before", before.clone())
        .param("after", after.clone())
        .param(
            "added",
            after
                .iter()
                .filter(|layer| !before.contains(layer))
                .cloned()
                .collect::<Vec<_>>(),
        )
        .param(
            "removed",
            before
                .iter()
                .filter(|layer| !after.contains(layer))
                .cloned()
                .collect::<Vec<_>>(),
        ),
    )
    .await?;
    let target_match = if args.target.kind == "node" {
        "MATCH (target:Entity {iri: $iri})"
    } else {
        "MATCH ()-[target]->() WHERE target.iri = $iri AND target.validTo IS NULL"
    };
    fetch_one_txn(
        &mut txn,
        query(&format!(
            "{target_match} SET target.layers = $layers, target.layersUpdatedAt = datetime(), \
             target.layerEpisodeId = $episode RETURN target.iri AS iri"
        ))
        .param("iri", args.target.iri.clone())
        .param("layers", after.clone())
        .param("episode", episode.iri.clone()),
    )
    .await?
    .ok_or_else(|| DomainError::Precondition("layer target changed concurrently".into()))?;
    txn.commit()
        .await
        .context("commit memory_layers transaction failed")?;
    Ok(json!({
        "noop": false,
        "target": args.target,
        "before": before,
        "layers": after,
        "scope": scope,
        "episode": { "iri": episode.iri, "at": episode.at, "tool": episode.tool },
    }))
}

/// Write global RDFS class or property schema-as-data (no `layers` input).
pub async fn memory_schema(graph: &Graph, args: SchemaArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_schema_once(graph, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

async fn memory_schema_once(graph: &Graph, args: SchemaArgs) -> Result<Value> {
    let kind = args.kind.trim().to_ascii_lowercase();
    if kind != "class" && kind != "property" {
        return Err(DomainError::InvalidInput("kind must be class or property".into()).into());
    }
    let seed = args
        .iri
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or(args.name.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            DomainError::InvalidInput("memory_schema requires a nonempty name or iri".into())
        })?;
    let iri = if kind == "class" {
        class_iri(seed)
    } else {
        property_iri(seed)
    };
    let name = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| name_from_iri(&iri));
    if kind == "class"
        && (args.sub_property_of.is_some() || args.domain.is_some() || args.range.is_some())
    {
        return Err(DomainError::InvalidInput(
            "class schema declarations only accept subClassOf".into(),
        )
        .into());
    }
    if kind == "property" && args.sub_class_of.is_some() {
        return Err(DomainError::InvalidInput(
            "property schema declarations do not accept subClassOf".into(),
        )
        .into());
    }
    for (field, value) in [
        ("subClassOf", args.sub_class_of.as_deref()),
        ("subPropertyOf", args.sub_property_of.as_deref()),
        ("domain", args.domain.as_deref()),
        ("range", args.range.as_deref()),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(DomainError::InvalidInput(format!(
                "memory_schema {field} cannot be empty"
            ))
            .into());
        }
    }
    let label = if kind == "class" { "Class" } else { "Property" };
    let subject_spec = NodeSpec {
        iri: Some(iri.clone()),
        name: Some(name),
        labels: vec![label.into()],
    };
    let mut definitions: Vec<(&str, &str, NodeSpec, &str)> = Vec::new();
    if kind == "class" {
        definitions.push((
            "INSTANCE_OF",
            "mindreader:property/INSTANCE_OF",
            NodeSpec {
                iri: Some("mindreader:class/Class".into()),
                name: Some("Class".into()),
                labels: vec!["Class".into()],
            },
            "class",
        ));
        if let Some(parent) = args.sub_class_of.as_deref() {
            let parent = class_iri(parent);
            definitions.push((
                "SUBCLASS_OF",
                "mindreader:property/SUBCLASS_OF",
                NodeSpec {
                    iri: Some(parent.clone()),
                    name: Some(name_from_iri(&parent)),
                    labels: vec!["Class".into()],
                },
                "class",
            ));
        }
    } else {
        definitions.push((
            "INSTANCE_OF",
            "mindreader:property/INSTANCE_OF",
            NodeSpec {
                iri: Some("mindreader:class/Property".into()),
                name: Some("Property".into()),
                labels: vec!["Class".into()],
            },
            "class",
        ));
        if let Some(parent) = args.sub_property_of.as_deref() {
            let parent = property_iri(parent);
            definitions.push((
                "SUBPROPERTY_OF",
                "mindreader:property/SUBPROPERTY_OF",
                NodeSpec {
                    iri: Some(parent.clone()),
                    name: Some(name_from_iri(&parent)),
                    labels: vec!["Property".into()],
                },
                "property",
            ));
        }
        if let Some(domain) = args.domain.as_deref() {
            let domain = class_iri(domain);
            definitions.push((
                "DOMAIN",
                "mindreader:property/DOMAIN",
                NodeSpec {
                    iri: Some(domain.clone()),
                    name: Some(name_from_iri(&domain)),
                    labels: vec!["Class".into()],
                },
                "class",
            ));
        }
        if let Some(range) = args.range.as_deref() {
            let range = class_iri(range);
            definitions.push((
                "RANGE",
                "mindreader:property/RANGE",
                NodeSpec {
                    iri: Some(range.clone()),
                    name: Some(name_from_iri(&range)),
                    labels: vec!["Class".into()],
                },
                "class",
            ));
        }
    }
    let mut txn = graph.start_txn().await?;
    let write = async {
        let mut locks = definitions
            .iter()
            .map(|(_, property, _, _)| {
                (iri.clone(), (*property).to_string(), FACT_LOCK_SCOPE.into())
            })
            .collect::<Vec<_>>();
        locks.push((iri.clone(), LAYERS_PROPERTY.into(), FACT_LOCK_SCOPE.into()));
        locks.extend(definitions.iter().map(|(_, _, target, _)| {
            (
                target
                    .iri
                    .clone()
                    .expect("schema definition targets always have an IRI"),
                LAYERS_PROPERTY.into(),
                FACT_LOCK_SCOPE.into(),
            )
        }));
        acquire_fact_locks_in_txn(&mut txn, &locks).await?;
        let existing_ready = fetch_one_txn(
            &mut txn,
            query(
                "OPTIONAL MATCH (n:Entity {iri: $iri}) RETURN n IS NOT NULL \
                 AND $label IN labels(n) AND coalesce(n.stub, false) = false \
                 AND size(coalesce(n.layers, [])) = 0 AS ready",
            )
            .param("iri", iri.clone())
            .param("label", label.to_string()),
        )
        .await?
        .and_then(|row| row.get::<bool>("ready").ok())
        .unwrap_or(false);
        let node = merge_node_in_txn(&mut txn, &subject_spec, &kind, &[]).await?;
        let mut resolved = Vec::new();
        let mut created_iris = Vec::new();
        if node.created {
            created_iris.push(node.iri.clone());
        }
        let mut changed = !existing_ready || node.created;
        changed |= apply_node_memberships_txn(&mut txn, &node, &[]).await?;
        for (rel, property, target_spec, target_kind) in definitions {
            let target = merge_node_in_txn(&mut txn, &target_spec, target_kind, &[]).await?;
            let target_globalized = apply_node_memberships_txn(&mut txn, &target, &[]).await?;
            let current =
                find_current_pairs_txn(&mut txn, &node.iri, property, Some(rel), &target.iri)
                    .await?;
            if current.len() > 1 {
                return Err(operation_error!(
                    "multiple current schema relationship identities for ({}, {}, {})",
                    node.iri,
                    property,
                    target.iri
                ));
            }
            let relationship_needs_global = current
                .first()
                .is_none_or(|current| !current.layers.is_empty());
            changed |= target.created || target_globalized || relationship_needs_global;
            if target.created {
                created_iris.push(target.iri.clone());
            }
            resolved.push((rel, property, target));
        }
        if !changed {
            return Ok::<_, Error>((None, node_json_from_merged(&node), Vec::new(), Vec::new()));
        }
        let episode = create_episode_in_txn(&mut txn, "memory_schema", None).await?;
        txn.run(
            query("MATCH (n:Entity {iri: $iri}) SET n.stub = false").param("iri", node.iri.clone()),
        )
        .await?;
        let mut links = Vec::new();
        for (rel, property, target) in resolved {
            let text = format!("{} {rel} {}", node.iri, target.iri);
            let (relationship_iri, _) = ensure_relation_txn(
                &mut txn,
                &RelationWrite {
                    rel_type: rel,
                    s: &node.iri,
                    o: &target.iri,
                    prop_iri: property,
                    layers: &[],
                    episode: &episode,
                    reason: None,
                    fact_text: &text,
                },
            )
            .await?;
            links.push(json!({ "rel": rel, "to": target.iri, "iri": relationship_iri }));
        }
        let refreshed = refreshed_node_json_txn(&mut txn, &node.iri).await?;
        let merge_suggestions = merge_suggestions_in_txn(&mut txn, &created_iris, &[]).await?;
        Ok::<_, Error>((Some(episode), refreshed, links, merge_suggestions))
    }
    .await;
    let (episode, node, links, merge_suggestions) = match write {
        Ok(value) => {
            if value.0.is_some() {
                txn.commit()
                    .await
                    .context("commit memory_schema transaction failed")?;
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
    Ok(json!({
        "kind": kind,
        "node": node,
        "links": links,
        "noop": episode.is_none(),
        "episode": episode.map(|episode| json!({ "iri": episode.iri, "at": episode.at, "tool": episode.tool })).unwrap_or(Value::Null),
        "mergeSuggestions": merge_suggestions,
    }))
}

fn node_json_from_merged(node: &MergedNode) -> Value {
    node.json.clone()
}

/// Count current ASSERTS edges for subject/predicate under a layer filter (tests and diagnostics).
pub async fn count_current_asserts(
    graph: &Graph,
    s: &str,
    p: &str,
    layer: &str,
) -> Result<(i64, Vec<String>)> {
    let prop = property_iri(p);
    let structural = structural_rel_for(&prop);
    let global = layer == "global";
    let rel_type = structural.as_deref().unwrap_or("ASSERTS");
    let rel_type = safe_rel(rel_type)?;
    let cypher = format!(
        "MATCH (s:Entity {{iri: $s}})-[r:{rel_type}]->(o:Entity) \
         WHERE r.validTo IS NULL AND ($global AND size(coalesce(r.layers, [])) = 0 \
           OR NOT $global AND $layer IN coalesce(r.layers, [])) \
           AND ($structural OR r.propertyIri = $p) \
         RETURN count(r) AS n, collect(o.iri) AS objects"
    );
    let row = fetch_one(
        graph,
        query(&cypher)
            .param("s", s.to_string())
            .param("p", prop)
            .param("layer", layer.to_string())
            .param("global", global)
            .param("structural", structural.is_some()),
    )
    .await?;
    Ok(row
        .map(|row| {
            (
                row.get::<i64>("n").unwrap_or(0),
                row.get::<Vec<String>>("objects").unwrap_or_default(),
            )
        })
        .unwrap_or_default())
}

/// Count ASSERTS including retracted history for subject/predicate under a layer filter.
pub async fn count_historical_asserts(graph: &Graph, s: &str, p: &str, layer: &str) -> Result<i64> {
    let row = fetch_one(
        graph,
        query(
            "MATCH (s:Entity {iri: $s})-[r:ASSERTS]->(o:Entity) \
             WHERE r.validTo IS NOT NULL AND r.propertyIri = $p \
               AND ($global AND size(coalesce(r.layers, [])) = 0 \
                 OR NOT $global AND $layer IN coalesce(r.layers, [])) RETURN count(r) AS n",
        )
        .param("s", s.to_string())
        .param("p", property_iri(p))
        .param("layer", layer.to_string())
        .param("global", layer == "global"),
    )
    .await?;
    Ok(row.and_then(|row| row.get::<i64>("n").ok()).unwrap_or(0))
}

/// Count current CONTRADICTS edges between two entity IRIs.
pub async fn count_current_contradicts(graph: &Graph, from: &str, to: &str) -> Result<i64> {
    let row = fetch_one(
        graph,
        query(
            "MATCH (a:Entity {iri: $from})-[r:CONTRADICTS]->(b:Entity {iri: $to}) \
             WHERE r.validTo IS NULL RETURN count(r) AS n",
        )
        .param("from", from.to_string())
        .param("to", to.to_string()),
    )
    .await?;
    Ok(row.and_then(|row| row.get::<i64>("n").ok()).unwrap_or(0))
}

/// Map application errors to MCP: domain → invalid params, others → internal error.
pub fn map_tool_error(err: Error) -> rmcp::model::ErrorData {
    use rmcp::model::ErrorData as McpError;
    if let Error::Domain(domain) = &err {
        let reason = match domain {
            DomainError::InvalidInput(_) => "invalid_input",
            DomainError::Precondition(_) => "precondition_failed",
        };
        return McpError::invalid_params(domain.to_string(), Some(json!({ "reason": reason })));
    }
    McpError::internal_error(err.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::{
        assert_fact_lock_requests, effective_weight, map_tool_error, merge_memberships,
        plan_fact_membership_changes, reject_system_owned_predicate, remove_memberships,
        replace_fact_lock_requests, retract_fact_lock_requests, CurrentFact, FactMembershipChange,
        CONTRADICTS_PROPERTY, LAYERS_PROPERTY, PREDICATE_USAGE_PROPERTY,
    };
    use crate::domain::DomainError;
    use crate::graph::spike_rank;

    #[test]
    fn spike_rank_order() {
        assert!(spike_rank(Some("Knowledge")) > spike_rank(Some("Insight")));
        assert!(spike_rank(Some("Insight")) > spike_rank(Some("Pattern")));
        assert!(spike_rank(Some("Pattern")) > spike_rank(Some("Signal")));
        assert!(spike_rank(Some("Signal")) > spike_rank(None));
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
    fn assert_and_replace_plan_all_known_locks_in_one_batch() {
        let assert_locks = assert_fact_lock_requests("subject", "property", "new", true);
        assert_eq!(assert_locks.len(), 5);
        assert!(assert_locks.contains(&(
            "new".into(),
            CONTRADICTS_PROPERTY.into(),
            "@fact".into()
        )));
        assert!(assert_locks.contains(&(
            "property".into(),
            PREDICATE_USAGE_PROPERTY.into(),
            "@fact".into()
        )));

        let replace_locks = replace_fact_lock_requests("subject", "property", "old", "new", true);
        assert_eq!(replace_locks.len(), 6);
        assert!(replace_locks.contains(&("old".into(), LAYERS_PROPERTY.into(), "@fact".into())));
        assert!(replace_locks.contains(&(
            "new".into(),
            CONTRADICTS_PROPERTY.into(),
            "@fact".into()
        )));
    }

    #[test]
    fn retract_plans_subject_and_predicate_guards_together() {
        assert_eq!(retract_fact_lock_requests("subject", None).len(), 1);
        let locks = retract_fact_lock_requests("subject", Some("property"));
        assert_eq!(locks.len(), 2);
        assert!(locks.contains(&(
            "property".into(),
            PREDICATE_USAGE_PROPERTY.into(),
            "@fact".into()
        )));
    }

    #[test]
    fn broad_retract_plans_only_matching_named_memberships() {
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
    fn broad_global_retract_plans_only_global_facts() {
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
    fn domain_errors_are_mcp_invalid_params() {
        let error = map_tool_error(DomainError::Precondition("missing old fact".into()).into());
        assert_eq!(error.code.0, -32602);
        assert_eq!(
            error.data.and_then(|data| data.get("reason").cloned()),
            Some(serde_json::json!("precondition_failed"))
        );
    }
}
