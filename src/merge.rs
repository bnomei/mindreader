//! Permanent same-kind unify and advisory duplicate suggestions.
//!
//! MCP `memory_unify` calls [`memory_unify`]: source memberships, history, and
//! edges move onto a surviving target of the same canonical kind, with no
//! `scope` filter. Bootstrap-seeded Class/Property IRIs cannot be sources.
//! Writes may attach [`merge_suggestions_in_txn`] results as review-only
//! `{source,target}` IRI pairs.

use crate::domain::{DomainError, NodeHandle};
use crate::graph::{
    acquire_fact_locks_in_txn, create_episode_in_txn, fetch_all_txn, fetch_one_txn, node_json,
    structural_rel_for, Episode,
};
use crate::iri::identity_kind_from_labels;
use crate::payload::{finish_mutation, unify_review_item};
use crate::{
    error::{Error, Result},
    operation_error,
};
use neo4rs::{query, BoltType, Graph, Node, Relation, Txn};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use tokio::time::{sleep, Duration};

const FORBIDDEN_ENTITY_LABELS: &[&str] = &[
    "Literal",
    "Episode",
    "FactLock",
    "MindreaderMeta",
    "SemanticActivation",
    "TTL",
];
const FACT_LOCK_SCOPE: &str = "@fact";
const LAYERS_PROPERTY: &str = "mindreader:property/layers";
const PREDICATE_USAGE_PROPERTY: &str = "mindreader:property/predicate-usage";
const WEIGHT_PROPERTY: &str = "mindreader:property/weight";
const SYSTEM_OWNED_RELS: &[&str] = &["CONTRADICTS", "SUPERSEDES"];
const BOOTSTRAP_SEEDED_IRIS: &[&str] = &[
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
    "mindreader:property/CONTRADICTS",
    "mindreader:property/SUPERSEDES",
];

fn lucene_fuzzy_term(name: &str) -> String {
    let mut query = String::with_capacity(name.len() + 3);
    for character in name.to_lowercase().chars() {
        if character.is_whitespace() || "+-&|!(){}[]^\"~*?:\\/".contains(character) {
            query.push('\\');
        }
        query.push(character);
    }
    query.push_str("~2");
    query
}

/// MCP `memory_unify` arguments: surviving `target` node absorbs `source`.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UnifyArgs {
    pub source: NodeHandle,
    pub target: NodeHandle,
}

impl UnifyArgs {
    /// Build unify arguments from two node IRIs (smoke, bench, and in-process callers).
    pub fn from_iris(source: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: NodeHandle::from_iri(source),
            target: NodeHandle::from_iri(target),
        }
    }
}

/// Permanently unify two same-kind nodes; MCP name is `memory_unify`.
pub async fn memory_unify(graph: &Graph, args: UnifyArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_unify_once(graph, &args).await {
            Err(error) if attempt < 2 && is_transient(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

fn is_transient(error: &Error) -> bool {
    error.is_transient_neo4j() || matches!(error, Error::ConcurrentMutation(_))
}

/// One unify attempt: validate kinds, move history in one transaction, commit or roll back.
async fn memory_unify_once(graph: &Graph, args: &UnifyArgs) -> Result<Value> {
    let source = args.source.iri()?.to_string();
    let target = args.target.iri()?.to_string();
    let source = source.as_str();
    let target = target.as_str();
    if source.is_empty() || target.is_empty() {
        return Err(DomainError::InvalidInput("source and target must not be empty".into()).into());
    }
    if source == target {
        return Err(DomainError::InvalidInput("source and target must be different".into()).into());
    }
    if is_bootstrap_seeded(source) {
        return Err(DomainError::InvalidInput(
            "bootstrap-seeded Class and Property IRIs cannot be merge sources".into(),
        )
        .into());
    }
    let mut txn = graph.start_txn().await?;
    let result = merge_in_txn(&mut txn, source, target).await;
    match result {
        Ok(node) => {
            txn.commit()
                .await
                .map_err(|source| Error::AmbiguousCommit {
                    operation: "memory_unify",
                    source,
                })?;
            Ok(node)
        }
        Err(error) => {
            let _ = txn.rollback().await;
            Err(error)
        }
    }
}

async fn affected_fact_locks_in_txn(
    txn: &mut Txn,
    source_iri: &str,
    target_iri: &str,
) -> Result<Vec<(String, String, String)>> {
    let mut locks = fetch_all_txn(
        txn,
        query(
            "MATCH (s:Entity)-[r]->(o:Entity) \
             WHERE s.iri IN [$source, $target] OR o.iri IN [$source, $target] \
                OR r.propertyIri IN [$source, $target] \
             WITH DISTINCT s, r \
             UNWIND [ \
               {subject: s.iri, property: r.propertyIri}, \
               {subject: r.iri, property: $weightProperty} \
             ] AS guard \
             RETURN DISTINCT guard.subject AS subject, guard.property AS property",
        )
        .param("source", source_iri.to_string())
        .param("target", target_iri.to_string())
        .param("weightProperty", WEIGHT_PROPERTY),
    )
    .await?
    .into_iter()
    .map(|row| {
        Ok::<_, Error>((
            row.get::<String>("subject")?,
            row.get::<String>("property")?,
            FACT_LOCK_SCOPE.into(),
        ))
    })
    .collect::<Result<Vec<_>>>()?;
    locks.sort();
    locks.dedup();
    Ok(locks)
}

async fn merge_in_txn(txn: &mut Txn, source_iri: &str, target_iri: &str) -> Result<Value> {
    let initial_affected = affected_fact_locks_in_txn(txn, source_iri, target_iri).await?;
    let mut locks = vec![
        (
            source_iri.into(),
            PREDICATE_USAGE_PROPERTY.into(),
            FACT_LOCK_SCOPE.into(),
        ),
        (
            target_iri.into(),
            PREDICATE_USAGE_PROPERTY.into(),
            FACT_LOCK_SCOPE.into(),
        ),
        (
            source_iri.into(),
            LAYERS_PROPERTY.into(),
            FACT_LOCK_SCOPE.into(),
        ),
        (
            source_iri.into(),
            WEIGHT_PROPERTY.into(),
            FACT_LOCK_SCOPE.into(),
        ),
        (
            target_iri.into(),
            LAYERS_PROPERTY.into(),
            FACT_LOCK_SCOPE.into(),
        ),
        (
            target_iri.into(),
            WEIGHT_PROPERTY.into(),
            FACT_LOCK_SCOPE.into(),
        ),
    ];
    locks.extend(initial_affected.iter().cloned());
    acquire_fact_locks_in_txn(txn, &locks).await?;
    let locked_affected = affected_fact_locks_in_txn(txn, source_iri, target_iri).await?;
    if locked_affected != initial_affected {
        return Err(Error::ConcurrentMutation(
            "the relationships affected by memory_unify; retry the operation".into(),
        ));
    }
    let row = fetch_one_txn(
        txn,
        query(
            "MATCH (source:Entity {iri: $source}), (target:Entity {iri: $target}) \
             RETURN source, target",
        )
        .param("source", source_iri.to_string())
        .param("target", target_iri.to_string()),
    )
    .await?
    .ok_or_else(|| {
        DomainError::Precondition(format!(
            "source {source_iri} and target {target_iri} must both exist"
        ))
    })?;
    let source: Node = row.get("source")?;
    let target: Node = row.get("target")?;
    reject_internal_node(&source, "source")?;
    reject_internal_node(&target, "target")?;
    let kind = require_same_kind(&source, &target)?;
    let property_merge = kind == "Property";
    if property_merge {
        require_compatible_property_merge(source_iri, target_iri)?;
    }

    let source_layers = source.get::<Vec<String>>("layers")?;
    let target_layers = target.get::<Vec<String>>("layers")?;
    let layers = merge_memberships(&target_layers, &source_layers);
    let weight = node_weight(&target)?.saturating_add(node_weight(&source)?);
    let episode = create_episode_in_txn(txn, "memory_unify", None).await?;

    let source_relationships_row = fetch_all_txn(
        txn,
        query(
            "MATCH (source:Entity {iri: $source})-[r]-() \
             RETURN collect(DISTINCT r.iri) AS iris",
        )
        .param("source", source_iri.to_string()),
    )
    .await?
    .into_iter()
    .next()
    .ok_or_else(|| operation_error!("source relationship lookup returned no row"))?;
    let source_relationships = source_relationships_row.get::<Vec<String>>("iris")?;

    txn.run(
        query(
            "MATCH (source:Entity {iri: $source})-[r]-(target:Entity {iri: $target}) \
             WHERE r.validTo IS NULL \
             SET r.validTo = datetime(), r.mergeEpisodeId = $episode",
        )
        .param("source", source_iri.to_string())
        .param("target", target_iri.to_string())
        .param("episode", episode.iri.clone()),
    )
    .await?;

    let merged = fetch_one_txn(
        txn,
        query(
            r#"
            MATCH (source:Entity {iri: $source}), (target:Entity {iri: $target})
            WITH source, target
            CALL apoc.refactor.mergeNodes(
              [target, source],
              {properties: 'discard', mergeRels: false,
               produceSelfRel: true, preserveExistingSelfRels: true}
            ) YIELD node
            SET node.layers = $layers,
                node.weight = $weight,
                node.mergeName = toLower(coalesce(node.name, '')),
                node.searchText = trim(coalesce(node.name, '') + ' ' + node.iri + ' ' + coalesce(node.value, '')),
                node.mergeEpisodeId = $episode
            RETURN node
            "#,
        )
        .param("source", source_iri.to_string())
        .param("target", target_iri.to_string())
        .param("layers", layers)
        .param("weight", weight)
        .param("episode", episode.iri.clone()),
    )
    .await?
    .ok_or_else(|| operation_error!("APOC did not return the merged target node"))?;
    let merged_node: Node = merged.get("node")?;

    if !source_relationships.is_empty() {
        txn.run(
            query(
                "MATCH ()-[r]->() WHERE r.iri IN $iris \
                 SET r.mergeEpisodeId = $episode",
            )
            .param("iris", source_relationships)
            .param("episode", episode.iri.clone()),
        )
        .await?;
    }
    if property_merge {
        txn.run(
            query(
                "MATCH ()-[r]->() WHERE r.propertyIri = $source \
                 SET r.propertyIri = $target, r.mergeEpisodeId = $episode",
            )
            .param("source", source_iri.to_string())
            .param("target", target_iri.to_string())
            .param("episode", episode.iri.clone()),
        )
        .await?;
    }
    txn.run(
        query(
            r#"
            MATCH (s:Entity)-[r]->(o:Entity)
            WHERE r.mergeEpisodeId = $episode
               OR ($propertyMerge AND r.propertyIri = $target)
            WITH s, r, o,
                 r.propertyIri AS property
            WITH s, r, o,
                 CASE
                   WHEN property CONTAINS '/' THEN last(split(property, '/'))
                   WHEN property CONTAINS ':' THEN last(split(property, ':'))
                   ELSE property
                 END AS propertyName
            SET r.factText = trim(
              coalesce(s.name, s.iri) + ' ' + propertyName + ' ' +
              coalesce(o.value, o.name, o.iri)
            )
            "#,
        )
        .param("episode", episode.iri.clone())
        .param("propertyMerge", property_merge)
        .param("target", target_iri.to_string()),
    )
    .await?;
    consolidate_current_duplicates(txn, target_iri, property_merge, &episode).await?;
    let node = node_json(&merged_node)?;
    let current = node.get("target").cloned();
    Ok(finish_mutation(
        json!({
            "ok": true,
            "noop": false,
            "node": node.clone(),
            "episode": {
                "iri": episode.iri,
                "at": episode.at,
                "tool": episode.tool,
            },
        }),
        &[],
        &[node],
        current,
        None,
    ))
}

/// Minimum Levenshtein similarity for advisory unify suggestions.
const MERGE_SIMILARITY_FLOOR: f64 = 0.85;
/// Stricter floor for Property suggestions.
const PROPERTY_MERGE_SIMILARITY_FLOOR: f64 = 0.92;

/// Whether an advisory unify suggestion clears the kind-specific similarity floor.
pub(crate) fn merge_similarity_accepted(kind: &str, similarity: f64) -> bool {
    if kind == "Property" {
        similarity >= PROPERTY_MERGE_SIMILARITY_FLOOR
    } else {
        similarity >= MERGE_SIMILARITY_FLOOR
    }
}

/// Require both nodes to share the same identity kind (Class, Property, Element, or one Spike).
fn require_same_kind(source: &Node, target: &Node) -> Result<String> {
    let source_kind = identity_kind_from_labels(source.labels());
    let target_kind = identity_kind_from_labels(target.labels());
    match (source_kind, target_kind) {
        (Some(source_kind), Some(target_kind)) if source_kind == target_kind => {
            Ok(source_kind.to_string())
        }
        _ => Err(DomainError::InvalidInput(
            "source and target must have the same single canonical kind".into(),
        )
        .into()),
    }
}

/// Forbid merging system-owned Properties or Properties with different structural types.
fn require_compatible_property_merge(source_iri: &str, target_iri: &str) -> Result<()> {
    let source_rel = structural_rel_for(source_iri);
    let target_rel = structural_rel_for(target_iri);
    if source_rel
        .as_deref()
        .is_some_and(|rel| SYSTEM_OWNED_RELS.contains(&rel))
        || target_rel
            .as_deref()
            .is_some_and(|rel| SYSTEM_OWNED_RELS.contains(&rel))
    {
        return Err(DomainError::InvalidInput(
            "system-owned CONTRADICTS and SUPERSEDES Properties cannot be merged".into(),
        )
        .into());
    }
    if source_rel != target_rel {
        return Err(DomainError::InvalidInput(
            "Properties can be merged only when both use the same structural relationship type"
                .into(),
        )
        .into());
    }
    Ok(())
}

fn is_bootstrap_seeded(iri: &str) -> bool {
    BOOTSTRAP_SEEDED_IRIS.contains(&iri)
}

/// Reject Literal, Episode, lock, meta, and activation nodes as unify endpoints.
fn reject_internal_node(node: &Node, field: &str) -> Result<()> {
    if node.labels().iter().any(|label| {
        FORBIDDEN_ENTITY_LABELS
            .iter()
            .any(|internal| label == internal)
    }) {
        return Err(DomainError::InvalidInput(format!(
            "{field} must be a user-visible non-literal entity"
        ))
        .into());
    }
    Ok(())
}

fn node_weight(node: &Node) -> Result<i64> {
    Ok(node.get::<i64>("weight")?)
}

fn relation_weight(relation: &Relation) -> Result<i64> {
    Ok(relation.get::<i64>("weight")?)
}

/// Union named memberships; either empty list is global and wins.
fn merge_memberships(left: &[String], right: &[String]) -> Vec<String> {
    if left.is_empty() || right.is_empty() {
        return Vec::new();
    }
    left.iter()
        .chain(right)
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

#[derive(Clone)]
struct DuplicateFact {
    iri: String,
    layers: Vec<String>,
    weight: i64,
    episodes: Vec<String>,
}

#[derive(Debug, PartialEq)]
struct SurvivorUpdate {
    iri: String,
    layers: Vec<String>,
    weight: i64,
    provenance: Vec<String>,
}

#[derive(Debug, PartialEq)]
struct DuplicateRetirement {
    iri: String,
    survivor: String,
}

fn duplicate_consolidation_plan(
    groups: &mut BTreeMap<(String, String, String, String), Vec<DuplicateFact>>,
) -> (Vec<SurvivorUpdate>, Vec<DuplicateRetirement>) {
    let mut updates = Vec::new();
    let mut retirements = Vec::new();
    for facts in groups.values_mut().filter(|facts| facts.len() > 1) {
        facts.sort_by(|left, right| left.iri.cmp(&right.iri));
        let survivor = facts[0].iri.clone();
        let mut layers = facts[0].layers.clone();
        let mut weight = 0_i64;
        let mut provenance = BTreeSet::new();
        for fact in facts.iter() {
            layers = merge_memberships(&layers, &fact.layers);
            weight = weight.saturating_add(fact.weight);
            provenance.extend(fact.episodes.iter().cloned());
        }
        updates.push(SurvivorUpdate {
            iri: survivor.clone(),
            layers,
            weight,
            provenance: provenance.into_iter().collect(),
        });
        retirements.extend(facts.iter().skip(1).map(|fact| DuplicateRetirement {
            iri: fact.iri.clone(),
            survivor: survivor.clone(),
        }));
    }
    (updates, retirements)
}

fn survivor_update_params(updates: &[SurvivorUpdate]) -> Vec<HashMap<String, BoltType>> {
    updates
        .iter()
        .map(|update| {
            HashMap::from([
                ("iri".into(), update.iri.clone().into()),
                ("layers".into(), update.layers.clone().into()),
                ("weight".into(), update.weight.into()),
                ("provenance".into(), update.provenance.clone().into()),
            ])
        })
        .collect()
}

fn duplicate_retirement_params(
    retirements: &[DuplicateRetirement],
) -> Vec<HashMap<String, String>> {
    retirements
        .iter()
        .map(|retirement| {
            HashMap::from([
                ("iri".into(), retirement.iri.clone()),
                ("survivor".into(), retirement.survivor.clone()),
            ])
        })
        .collect()
}

async fn consolidate_current_duplicates(
    txn: &mut Txn,
    target_iri: &str,
    include_property_facts: bool,
    episode: &Episode,
) -> Result<()> {
    let rows = fetch_all_txn(
        txn,
        query(
            "MATCH (s:Entity)-[r]->(o:Entity) \
             WHERE r.validTo IS NULL AND (s.iri = $target OR o.iri = $target \
                OR ($includePropertyFacts AND r.propertyIri = $target)) \
             RETURN s.iri AS s, type(r) AS type, r.propertyIri AS property, \
                    o.iri AS o, r",
        )
        .param("target", target_iri.to_string())
        .param("includePropertyFacts", include_property_facts),
    )
    .await?;
    let mut groups: BTreeMap<(String, String, String, String), Vec<DuplicateFact>> =
        BTreeMap::new();
    for row in rows {
        let relation: Relation = row.get("r")?;
        let iri = relation.get::<String>("iri")?;
        let mut episodes = relation
            .get::<Vec<String>>("provenanceEpisodeIds")
            .unwrap_or_default();
        if let Ok(original) = relation.get::<String>("episodeId") {
            episodes.push(original);
        }
        episodes.sort();
        episodes.dedup();
        groups
            .entry((
                row.get("s")?,
                row.get("type")?,
                row.get("property")?,
                row.get("o")?,
            ))
            .or_default()
            .push(DuplicateFact {
                iri,
                layers: relation.get::<Vec<String>>("layers")?,
                weight: relation_weight(&relation)?,
                episodes,
            });
    }
    let (updates, retirements) = duplicate_consolidation_plan(&mut groups);
    if !updates.is_empty() {
        let expected = updates.len() as i64;
        let row = fetch_one_txn(
            txn,
            query(
                "UNWIND $updates AS update \
                 MATCH ()-[r]->() WHERE r.iri = update.iri AND r.validTo IS NULL \
                 SET r.layers = update.layers, r.weight = update.weight, \
                     r.provenanceEpisodeIds = update.provenance, r.mergeEpisodeId = $episode \
                 RETURN count(r) AS updated",
            )
            .param("updates", survivor_update_params(&updates))
            .param("episode", episode.iri.clone()),
        )
        .await?
        .ok_or_else(|| operation_error!("duplicate survivor update returned no count"))?;
        let updated = row.get::<i64>("updated")?;
        if updated != expected {
            return Err(operation_error!(
                "duplicate survivor update affected {updated} relationships; expected {expected}"
            ));
        }
    }
    if !retirements.is_empty() {
        let expected = retirements.len() as i64;
        let row = fetch_one_txn(
            txn,
            query(
                "UNWIND $retirements AS retirement \
                 MATCH ()-[r]->() WHERE r.iri = retirement.iri AND r.validTo IS NULL \
                 SET r.validTo = datetime(), r.mergedInto = retirement.survivor, \
                     r.mergeEpisodeId = $episode \
                 RETURN count(r) AS retired",
            )
            .param("retirements", duplicate_retirement_params(&retirements))
            .param("episode", episode.iri.clone()),
        )
        .await?
        .ok_or_else(|| operation_error!("duplicate retirement returned no count"))?;
        let retired = row.get::<i64>("retired")?;
        if retired != expected {
            return Err(operation_error!(
                "duplicate retirement affected {retired} relationships; expected {expected}"
            ));
        }
    }
    Ok(())
}

/// Advisory fuzzy name matches for newly created IRIs; never auto-merges.
pub async fn merge_suggestions_in_txn(
    txn: &mut Txn,
    created_iris: &[String],
    layers: &[String],
) -> Result<Vec<Value>> {
    if created_iris.is_empty() {
        return Ok(Vec::new());
    }
    let mut suggestions = Vec::new();
    let created_set = created_iris.iter().cloned().collect::<BTreeSet<_>>();
    let created_specs = fetch_all_txn(
        txn,
        query(
            "UNWIND $createdIris AS iri \
             MATCH (created:Entity {iri: iri}) \
             WHERE created.name IS NOT NULL AND created.name <> '' \
             RETURN created.iri AS iri, created.name AS name",
        )
        .param("createdIris", created_iris.to_vec()),
    )
    .await?
    .into_iter()
    .map(|row| {
        let iri = row.get::<String>("iri")?;
        let name = row.get::<String>("name")?;
        Ok(HashMap::from([
            ("iri".to_string(), iri),
            ("nameQuery".to_string(), lucene_fuzzy_term(&name)),
        ]))
    })
    .collect::<Result<Vec<_>>>()?;
    if created_specs.is_empty() {
        return Ok(Vec::new());
    }
    let rows = fetch_all_txn(
        txn,
        query(
            r#"
            UNWIND $created AS spec
            MATCH (created:Entity {iri: spec.iri})
            CALL {
              WITH created, spec
              CALL db.index.fulltext.queryNodes('merge_candidate_names', spec.nameQuery)
              YIELD node
              RETURN node AS candidate
              UNION
              WITH created, spec
              MATCH (candidate:Entity)
              WHERE candidate.iri IN $createdIris
              RETURN candidate
            }
            WITH created, candidate
            WHERE candidate <> created
              AND candidate.name IS NOT NULL
              AND (size(coalesce(candidate.layers, [])) = 0
                   OR any(layer IN coalesce(candidate.layers, []) WHERE layer IN $layers))
              AND none(label IN labels(created) WHERE label IN $forbidden)
              AND none(label IN labels(candidate) WHERE label IN $forbidden)
              AND apoc.text.fuzzyMatch(toLower(created.name), toLower(candidate.name))
            WITH created, candidate,
                 CASE
                   WHEN created:Class THEN 'Class'
                   WHEN created:Property THEN 'Property'
                   WHEN created:Element THEN 'Element'
                   WHEN created:Signal AND NOT created:Pattern AND NOT created:Insight AND NOT created:Knowledge THEN 'Signal'
                   WHEN created:Pattern AND NOT created:Signal AND NOT created:Insight AND NOT created:Knowledge THEN 'Pattern'
                   WHEN created:Insight AND NOT created:Signal AND NOT created:Pattern AND NOT created:Knowledge THEN 'Insight'
                   WHEN created:Knowledge AND NOT created:Signal AND NOT created:Pattern AND NOT created:Insight THEN 'Knowledge'
                   ELSE null
                 END AS createdKind,
                 CASE
                   WHEN candidate:Class THEN 'Class'
                   WHEN candidate:Property THEN 'Property'
                   WHEN candidate:Element THEN 'Element'
                   WHEN candidate:Signal AND NOT candidate:Pattern AND NOT candidate:Insight AND NOT candidate:Knowledge THEN 'Signal'
                   WHEN candidate:Pattern AND NOT candidate:Signal AND NOT candidate:Insight AND NOT candidate:Knowledge THEN 'Pattern'
                   WHEN candidate:Insight AND NOT candidate:Signal AND NOT candidate:Pattern AND NOT candidate:Knowledge THEN 'Insight'
                   WHEN candidate:Knowledge AND NOT candidate:Signal AND NOT candidate:Pattern AND NOT candidate:Insight THEN 'Knowledge'
                   ELSE null
                 END AS candidateKind,
                 apoc.text.levenshteinSimilarity(
                   toLower(created.name), toLower(candidate.name)
                 ) AS similarity
            WHERE createdKind IS NOT NULL
              AND createdKind = candidateKind
              AND similarity >= $similarityFloor
              AND (createdKind <> 'Property' OR similarity >= $propertyFloor)
            ORDER BY created.iri ASC, similarity DESC,
                     size(candidate.name) ASC, candidate.iri ASC
            WITH created, collect({
              candidateIri: candidate.iri,
              candidateName: candidate.name,
              canonicalKind: createdKind,
              similarity: similarity
            })[..3] AS candidates
            UNWIND candidates AS candidate
            RETURN created.iri AS createdIri, created.name AS createdName,
                   candidate.candidateIri AS candidateIri,
                   candidate.candidateName AS candidateName,
                   candidate.canonicalKind AS canonicalKind,
                   candidate.similarity AS similarity
            "#,
        )
        .param("created", created_specs)
        .param("createdIris", created_iris.to_vec())
        .param("layers", layers.to_vec())
        .param(
            "forbidden",
            FORBIDDEN_ENTITY_LABELS
                .iter()
                .map(|label| label.to_string())
                .collect::<Vec<_>>(),
        )
        .param("similarityFloor", MERGE_SIMILARITY_FLOOR)
        .param("propertyFloor", PROPERTY_MERGE_SIMILARITY_FLOOR),
    )
    .await?;
    for row in rows {
        let created_iri: String = row.get("createdIri")?;
        let created_name: String = row.get("createdName")?;
        let candidate_iri: String = row.get("candidateIri")?;
        let candidate_name: String = row.get("candidateName")?;
        let canonical_kind: String = row.get("canonicalKind")?;
        let similarity: f64 = row.get("similarity")?;
        if !merge_similarity_accepted(&canonical_kind, similarity) {
            continue;
        }
        if canonical_kind == "Property"
            && require_compatible_property_merge(&created_iri, &candidate_iri).is_err()
        {
            continue;
        }
        let candidate_was_created = created_set.contains(&candidate_iri);
        let created_first_on_tie = candidate_was_created && created_iri < candidate_iri;
        let created_is_target = is_bootstrap_seeded(&created_iri)
            || (!is_bootstrap_seeded(&candidate_iri)
                && (created_name.chars().count() < candidate_name.chars().count()
                    || (created_name.chars().count() == candidate_name.chars().count()
                        && created_first_on_tie)));
        let (source, source_name, target, target_name) = if created_is_target {
            (candidate_iri, candidate_name, created_iri, created_name)
        } else {
            (created_iri, created_name, candidate_iri, candidate_name)
        };
        suggestions.push(unify_review_item(
            &source,
            &source_name,
            &target,
            &target_name,
            row.get::<f64>("similarity")?,
        ));
    }
    suggestions.sort_by(|left, right| {
        right["similarity"]
            .as_f64()
            .partial_cmp(&left["similarity"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                left["source"]["iri"]
                    .as_str()
                    .cmp(&right["source"]["iri"].as_str())
            })
            .then_with(|| {
                left["target"]["iri"]
                    .as_str()
                    .cmp(&right["target"]["iri"].as_str())
            })
    });
    suggestions.dedup_by(|left, right| {
        left["source"]["iri"] == right["source"]["iri"]
            && left["target"]["iri"] == right["target"]["iri"]
    });
    Ok(suggestions)
}

#[cfg(test)]
mod tests {
    use super::{
        duplicate_consolidation_plan, is_bootstrap_seeded, is_transient, lucene_fuzzy_term,
        merge_memberships, merge_similarity_accepted, require_compatible_property_merge,
        DuplicateFact, DuplicateRetirement, SurvivorUpdate,
    };
    use crate::error::Error;
    use std::collections::BTreeMap;

    #[test]
    fn global_membership_dominates_merge() {
        assert!(merge_memberships(&[], &["project:a".into()]).is_empty());
        assert_eq!(
            merge_memberships(&["project:b".into()], &["project:a".into()]),
            vec!["project:a", "project:b"]
        );
    }

    #[test]
    fn merge_similarity_accepted_drops_false_friends() {
        assert!(!merge_similarity_accepted("Element", 0.667));
        assert!(merge_similarity_accepted("Element", 0.96));
        assert!(!merge_similarity_accepted("Property", 0.85));
        assert!(merge_similarity_accepted("Property", 0.96));
    }

    #[test]
    fn property_merges_preserve_relationship_representation() {
        assert!(require_compatible_property_merge(
            "mindreader:property/custom-a",
            "mindreader:property/custom-b"
        )
        .is_ok());
        assert!(
            require_compatible_property_merge("example:ABOUT", "mindreader:property/ABOUT").is_ok()
        );
        assert!(require_compatible_property_merge(
            "mindreader:property/custom",
            "mindreader:property/ABOUT"
        )
        .is_err());
        assert!(require_compatible_property_merge(
            "example:CONTRADICTS",
            "mindreader:property/CONTRADICTS"
        )
        .is_err());
    }

    #[test]
    fn bootstrap_seeded_entities_are_permanent_targets_only() {
        assert!(is_bootstrap_seeded("mindreader:class/Element"));
        assert!(is_bootstrap_seeded("mindreader:property/ABOUT"));
        assert!(!is_bootstrap_seeded("mindreader:property/custom"));
    }

    #[test]
    fn affected_set_drift_is_retryable() {
        assert!(is_transient(&Error::ConcurrentMutation(
            "merge relationships".into()
        )));
    }

    #[test]
    fn fuzzy_candidate_terms_escape_whole_names() {
        assert_eq!(lucene_fuzzy_term("Spaceships"), "spaceships~2");
        assert_eq!(lucene_fuzzy_term("space ship"), "space\\ ship~2");
        assert_eq!(
            lucene_fuzzy_term("C++ / A(B)"),
            "c\\+\\+\\ \\/\\ a\\(b\\)~2"
        );
        assert_eq!(lucene_fuzzy_term("007s"), "007s~2");
    }

    #[test]
    fn duplicate_plan_aggregates_every_group_before_batched_writes() {
        let mut groups = BTreeMap::from([
            (
                (
                    "s-a".into(),
                    "REL".into(),
                    "property-a".into(),
                    "o-a".into(),
                ),
                vec![
                    DuplicateFact {
                        iri: "rel-b".into(),
                        layers: vec!["project:b".into()],
                        weight: 7,
                        episodes: vec!["episode-b".into(), "episode-shared".into()],
                    },
                    DuplicateFact {
                        iri: "rel-a".into(),
                        layers: vec!["project:a".into()],
                        weight: i64::MAX,
                        episodes: vec!["episode-a".into(), "episode-shared".into()],
                    },
                ],
            ),
            (
                (
                    "s-b".into(),
                    "REL".into(),
                    "property-b".into(),
                    "o-b".into(),
                ),
                vec![
                    DuplicateFact {
                        iri: "rel-c".into(),
                        layers: Vec::new(),
                        weight: -3,
                        episodes: vec!["episode-c".into()],
                    },
                    DuplicateFact {
                        iri: "rel-d".into(),
                        layers: vec!["project:d".into()],
                        weight: 1,
                        episodes: vec!["episode-d".into()],
                    },
                ],
            ),
            (
                (
                    "s-c".into(),
                    "REL".into(),
                    "property-c".into(),
                    "o-c".into(),
                ),
                vec![DuplicateFact {
                    iri: "rel-single".into(),
                    layers: vec!["project:single".into()],
                    weight: 4,
                    episodes: vec!["episode-single".into()],
                }],
            ),
        ]);

        let (updates, retirements) = duplicate_consolidation_plan(&mut groups);

        assert_eq!(
            updates,
            vec![
                SurvivorUpdate {
                    iri: "rel-a".into(),
                    layers: vec!["project:a".into(), "project:b".into()],
                    weight: i64::MAX,
                    provenance: vec![
                        "episode-a".into(),
                        "episode-b".into(),
                        "episode-shared".into()
                    ],
                },
                SurvivorUpdate {
                    iri: "rel-c".into(),
                    layers: Vec::new(),
                    weight: -2,
                    provenance: vec!["episode-c".into(), "episode-d".into()],
                },
            ]
        );
        assert_eq!(
            retirements,
            vec![
                DuplicateRetirement {
                    iri: "rel-b".into(),
                    survivor: "rel-a".into(),
                },
                DuplicateRetirement {
                    iri: "rel-d".into(),
                    survivor: "rel-c".into(),
                },
            ]
        );
    }
}
