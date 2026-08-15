use crate::domain::DomainError;
use crate::graph::{
    acquire_fact_locks_in_txn, create_episode_in_txn, fetch_all_txn, fetch_one_txn, node_json,
    structural_rel_for, Episode,
};
use anyhow::{anyhow, Context, Result};
use neo4rs::{query, Error as Neo4jDriverError, Graph, Neo4jErrorKind, Node, Relation, Txn};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use tokio::time::{sleep, Duration};

const FORBIDDEN_ENTITY_LABELS: &[&str] = &[
    "Literal",
    "Episode",
    "FactLock",
    "MindreaderMeta",
    "SemanticActivation",
    "TTL",
];
const CANONICAL_KIND_LABELS: &[&str] = &[
    "Class",
    "Property",
    "Element",
    "Signal",
    "Pattern",
    "Insight",
    "Knowledge",
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

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct MergeArgs {
    pub source: String,
    pub target: String,
}

pub async fn memory_merge(graph: &Graph, args: MergeArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_merge_once(graph, &args).await {
            Err(error) if attempt < 2 && is_transient(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

fn is_transient(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<Neo4jDriverError>()
            .is_some_and(|driver| {
                matches!(driver, Neo4jDriverError::Neo4j(error) if error.kind() == Neo4jErrorKind::Transient)
            })
    })
}

async fn memory_merge_once(graph: &Graph, args: &MergeArgs) -> Result<Value> {
    let source = args.source.trim();
    let target = args.target.trim();
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
                .context("commit memory_merge transaction failed")?;
            Ok(node)
        }
        Err(error) => {
            let _ = txn.rollback().await;
            Err(error)
        }
    }
}

async fn merge_in_txn(txn: &mut Txn, source_iri: &str, target_iri: &str) -> Result<Value> {
    acquire_fact_locks_in_txn(
        txn,
        &[
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
        ],
    )
    .await?;
    acquire_fact_locks_in_txn(
        txn,
        &[
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
        ],
    )
    .await?;
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

    let affected_facts = fetch_all_txn(
        txn,
        query(
            "MATCH (s:Entity)-[r]->(o:Entity) \
             WHERE s.iri IN [$source, $target] OR o.iri IN [$source, $target] \
                OR r.propertyIri IN [$source, $target] \
             RETURN DISTINCT s.iri AS subject, \
               coalesce(r.propertyIri, 'mindreader:property/' + type(r)) AS property",
        )
        .param("source", source_iri.to_string())
        .param("target", target_iri.to_string()),
    )
    .await?
    .into_iter()
    .map(|row| {
        Ok::<_, anyhow::Error>((
            row.get::<String>("subject")?,
            row.get::<String>("property")?,
            "@fact".into(),
        ))
    })
    .collect::<Result<Vec<_>>>()?;
    acquire_fact_locks_in_txn(txn, &affected_facts).await?;

    let source_layers = source.get::<Vec<String>>("layers").unwrap_or_default();
    let target_layers = target.get::<Vec<String>>("layers").unwrap_or_default();
    let layers = merge_memberships(&target_layers, &source_layers);
    let weight = node_weight(&target).saturating_add(node_weight(&source));
    let episode = create_episode_in_txn(txn, "memory_merge", None).await?;

    let source_relationships = fetch_all_txn(
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
    .and_then(|row| row.get::<Vec<String>>("iris").ok())
    .unwrap_or_default();

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
                node.weightText = toString($weight),
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
    .ok_or_else(|| anyhow!("APOC did not return the merged target node"))?;
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
                 coalesce(r.propertyIri, 'mindreader:property/' + type(r)) AS property
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
    Ok(node_json(&merged_node))
}

fn canonical_kinds(node: &Node) -> BTreeSet<String> {
    node.labels()
        .into_iter()
        .filter(|label| CANONICAL_KIND_LABELS.contains(label))
        .map(str::to_string)
        .collect()
}

fn require_same_kind(source: &Node, target: &Node) -> Result<String> {
    let source_kinds = canonical_kinds(source);
    let target_kinds = canonical_kinds(target);
    if source_kinds.len() != 1 || source_kinds != target_kinds {
        return Err(DomainError::InvalidInput(
            "source and target must have the same single canonical kind".into(),
        )
        .into());
    }
    Ok(source_kinds.into_iter().next().expect("one kind checked"))
}

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

fn node_weight(node: &Node) -> i64 {
    node.get::<String>("weightText")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| node.get::<i64>("weight").unwrap_or(0))
}

fn relation_weight(relation: &Relation) -> i64 {
    relation
        .get::<String>("weightText")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| relation.get::<i64>("weight").unwrap_or(0))
}

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
             RETURN s.iri AS s, type(r) AS type, coalesce(r.propertyIri, '') AS property, \
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
        let iri = relation.get::<String>("iri").unwrap_or_default();
        if iri.is_empty() {
            continue;
        }
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
                layers: relation.get::<Vec<String>>("layers").unwrap_or_default(),
                weight: relation_weight(&relation),
                episodes,
            });
    }
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
        txn.run(
            query(
                "MATCH ()-[r]->() WHERE r.iri = $iri AND r.validTo IS NULL \
                 SET r.layers = $layers, r.weight = $weight, r.weightText = toString($weight), \
                     r.provenanceEpisodeIds = $provenance, r.mergeEpisodeId = $episode",
            )
            .param("iri", survivor.clone())
            .param("layers", layers)
            .param("weight", weight)
            .param("provenance", provenance.into_iter().collect::<Vec<_>>())
            .param("episode", episode.iri.clone()),
        )
        .await?;
        let retired = facts
            .iter()
            .skip(1)
            .map(|fact| fact.iri.clone())
            .collect::<Vec<_>>();
        txn.run(
            query(
                "MATCH ()-[r]->() WHERE r.iri IN $iris AND r.validTo IS NULL \
                 SET r.validTo = datetime(), r.mergedInto = $survivor, \
                     r.mergeEpisodeId = $episode",
            )
            .param("iris", retired)
            .param("survivor", survivor)
            .param("episode", episode.iri.clone()),
        )
        .await?;
    }
    Ok(())
}

pub async fn merge_suggestions_in_txn(
    txn: &mut Txn,
    created_iris: &[String],
    layers: &[String],
) -> Result<Vec<Value>> {
    let mut suggestions = Vec::new();
    let created_set = created_iris.iter().cloned().collect::<BTreeSet<_>>();
    for created_iri in created_iris {
        let rows = fetch_all_txn(
            txn,
            query(
                r#"
                MATCH (created:Entity {iri: $iri}), (candidate:Entity)
                WHERE candidate <> created
                  AND created.name IS NOT NULL AND candidate.name IS NOT NULL
                  AND (size(coalesce(candidate.layers, [])) = 0
                       OR any(layer IN coalesce(candidate.layers, []) WHERE layer IN $layers))
                  AND none(label IN labels(created) WHERE label IN $forbidden)
                  AND none(label IN labels(candidate) WHERE label IN $forbidden)
                  AND size([label IN labels(created) WHERE label IN $canonicalKinds]) = 1
                  AND size([label IN labels(candidate) WHERE label IN $canonicalKinds]) = 1
                  AND all(label IN labels(created)
                          WHERE NOT label IN $canonicalKinds OR label IN labels(candidate))
                  AND all(label IN labels(candidate)
                          WHERE NOT label IN $canonicalKinds OR label IN labels(created))
                  AND apoc.text.fuzzyMatch(toLower(created.name), toLower(candidate.name))
                WITH created, candidate,
                     apoc.text.levenshteinSimilarity(toLower(created.name), toLower(candidate.name)) AS similarity
                RETURN created.iri AS createdIri, created.name AS createdName,
                       candidate.iri AS candidateIri, candidate.name AS candidateName,
                       head([label IN labels(created) WHERE label IN $canonicalKinds]) AS canonicalKind,
                       similarity
                ORDER BY similarity DESC, size(candidate.name) ASC, candidate.iri ASC
                LIMIT 3
                "#,
            )
            .param("iri", created_iri.clone())
            .param("layers", layers.to_vec())
            .param(
                "forbidden",
                FORBIDDEN_ENTITY_LABELS
                    .iter()
                    .map(|label| label.to_string())
                    .collect::<Vec<_>>(),
            )
            .param(
                "canonicalKinds",
                CANONICAL_KIND_LABELS
                    .iter()
                    .map(|label| label.to_string())
                    .collect::<Vec<_>>(),
            ),
        )
        .await?;
        for row in rows {
            let created_iri: String = row.get("createdIri")?;
            let created_name: String = row.get("createdName")?;
            let candidate_iri: String = row.get("candidateIri")?;
            let candidate_name: String = row.get("candidateName")?;
            let canonical_kind: String = row.get("canonicalKind")?;
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
            suggestions.push(json!({
                "source": { "iri": source, "name": source_name },
                "target": { "iri": target, "name": target_name },
                "similarity": row.get::<f64>("similarity")?,
                "merge": { "source": source, "target": target },
            }));
        }
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
    suggestions.dedup_by(|left, right| left["merge"] == right["merge"]);
    Ok(suggestions)
}

#[cfg(test)]
mod tests {
    use super::{is_bootstrap_seeded, merge_memberships, require_compatible_property_merge};

    #[test]
    fn global_membership_dominates_merge() {
        assert!(merge_memberships(&[], &["project:a".into()]).is_empty());
        assert_eq!(
            merge_memberships(&["project:b".into()], &["project:a".into()]),
            vec!["project:a", "project:b"]
        );
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
}
