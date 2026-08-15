use crate::domain::DomainError;
use crate::graph::{
    acquire_fact_locks_in_txn, create_episode_in_txn, fetch_all_txn, fetch_one_txn, node_json,
    Episode,
};
use anyhow::{anyhow, Result};
use neo4rs::{query, Graph, Node, Relation, Txn};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use tokio::time::{sleep, Duration};

const NON_DOMAIN_LABELS: &[&str] = &[
    "Entity",
    "FactLock",
    "MindreaderMeta",
    "SemanticActivation",
    "TTL",
];
const FORBIDDEN_ENTITY_LABELS: &[&str] = &[
    "Literal",
    "Episode",
    "FactLock",
    "MindreaderMeta",
    "SemanticActivation",
    "TTL",
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
    error.to_string().contains("TransientError")
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
    let mut txn = graph.start_txn().await?;
    let result = merge_in_txn(&mut txn, source, target).await;
    match result {
        Ok(node) => {
            txn.commit()
                .await
                .map_err(|error| anyhow!("commit memory_merge transaction failed: {error}"))?;
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
            (source_iri.into(), "@merge".into(), "@merge".into()),
            (target_iri.into(), "@merge".into(), "@merge".into()),
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

    let affected_facts = fetch_all_txn(
        txn,
        query(
            "MATCH (s:Entity)-[r]->(o:Entity) \
             WHERE s.iri IN [$source, $target] OR o.iri IN [$source, $target] \
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
    consolidate_current_duplicates(txn, target_iri, &episode).await?;
    Ok(node_json(&merged_node))
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
    episode: &Episode,
) -> Result<()> {
    let rows = fetch_all_txn(
        txn,
        query(
            "MATCH (s:Entity)-[r]->(o:Entity) \
             WHERE r.validTo IS NULL AND (s.iri = $target OR o.iri = $target) \
             RETURN s.iri AS s, type(r) AS type, coalesce(r.propertyIri, '') AS property, \
                    o.iri AS o, r",
        )
        .param("target", target_iri.to_string()),
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
) -> Result<Vec<Value>> {
    let mut suggestions = Vec::new();
    for created_iri in created_iris {
        let rows = fetch_all_txn(
            txn,
            query(
                r#"
                MATCH (created:Entity {iri: $iri}), (candidate:Entity)
                WHERE candidate <> created
                  AND created.name IS NOT NULL AND candidate.name IS NOT NULL
                  AND none(label IN labels(created) WHERE label IN $forbidden)
                  AND none(label IN labels(candidate) WHERE label IN $forbidden)
                  AND any(label IN labels(created)
                          WHERE NOT label IN $nonDomain AND label IN labels(candidate))
                  AND apoc.text.fuzzyMatch(toLower(created.name), toLower(candidate.name))
                WITH created, candidate,
                     apoc.text.levenshteinSimilarity(toLower(created.name), toLower(candidate.name)) AS similarity
                RETURN created.iri AS createdIri, created.name AS createdName,
                       candidate.iri AS candidateIri, candidate.name AS candidateName,
                       similarity
                ORDER BY similarity DESC, size(candidate.name) ASC, candidate.iri ASC
                LIMIT 3
                "#,
            )
            .param("iri", created_iri.clone())
            .param(
                "forbidden",
                FORBIDDEN_ENTITY_LABELS
                    .iter()
                    .map(|label| label.to_string())
                    .collect::<Vec<_>>(),
            )
            .param(
                "nonDomain",
                NON_DOMAIN_LABELS
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
            let (source, source_name, target, target_name) =
                if created_name.chars().count() < candidate_name.chars().count() {
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
    use super::merge_memberships;

    #[test]
    fn global_membership_dominates_merge() {
        assert!(merge_memberships(&[], &["project:a".into()]).is_empty());
        assert_eq!(
            merge_memberships(&["project:b".into()], &["project:a".into()]),
            vec!["project:a", "project:b"]
        );
    }
}
