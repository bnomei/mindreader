//! Semantic recall: embed a query and fuse it with remembered activations.
//!
//! Exposed separately from closed-world `memory_recall`. Combines ranked
//! direct `ASSERTS`/`ABOUT` hits with TTL activation bundles via reciprocal
//! rank fusion, still under the request `scope`. Query text is sent to the
//! configured embedding provider; without a key this path fails as
//! `missing_embedding`. Activations expire and may converge, so this operation
//! is intentionally not read-only.

use crate::config::{Config, SemanticConfig};
use crate::domain::DomainError;
use crate::embeddings::{build_provider, normalize_vector, EmbeddingProvider};
use crate::graph::{
    endpoint_json, fact_envelope, fetch_all, fetch_one, rel_json, spike_label, SEMANTIC_INDEX,
};
use crate::layers::validate_layer_ids;
use crate::search::{memory_search, SearchArgs};
use crate::{
    embedding_error,
    error::{Context, Result},
    operation_error,
};
use neo4rs::{query, Graph, Node, Relation};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

/// Maximum UTF-8 byte length accepted for semantic query text.
pub const MAX_SEMANTIC_TEXT_BYTES: usize = 32 * 1024;

/// Arguments for the side-effectful `memory_recall_semantic` operation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SemanticSearchArgs {
    pub scope: Vec<String>,
    pub text: String,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Optional embedding provider plus semantic fusion tunables for a process.
#[derive(Clone)]
pub struct SemanticRuntime {
    provider: Arc<dyn EmbeddingProvider>,
    config: SemanticConfig,
}

impl SemanticRuntime {
    /// Build a runtime when embedding credentials are present; otherwise `None`.
    pub fn from_config(config: &Config) -> Result<Option<Self>> {
        let Some(selected) = config.embedding.as_ref() else {
            return Ok(None);
        };
        Ok(Some(Self {
            provider: Arc::from(build_provider(selected)?),
            config: config.semantic.clone(),
        }))
    }

    pub fn provider(&self) -> &'static str {
        self.provider.provider()
    }

    pub fn model(&self) -> &str {
        self.provider.model()
    }

    pub fn dimensions(&self) -> usize {
        self.provider.dimensions()
    }

    /// Construct a runtime with an explicit provider (tests and smoke fixtures).
    pub fn new(provider: Arc<dyn EmbeddingProvider>, config: SemanticConfig) -> Self {
        Self { provider, config }
    }
}

#[derive(Debug, Clone)]
struct Activation {
    element_id: String,
    result_refs: Vec<String>,
    similarity: f64,
}

fn validate_semantic_text(text: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(DomainError::InvalidInput(
            "memory_recall_semantic text must not be empty".into(),
        )
        .into());
    }
    if text.len() > MAX_SEMANTIC_TEXT_BYTES {
        return Err(DomainError::InvalidInput(format!(
            "memory_recall_semantic text must not exceed {MAX_SEMANTIC_TEXT_BYTES} UTF-8 bytes"
        ))
        .into());
    }
    Ok(text.to_string())
}

/// Validate the semantic recall wire contract before embedding or graph work.
pub fn validate_semantic_search_args(args: &SemanticSearchArgs) -> Result<()> {
    validate_layer_ids(args.scope.clone())?;
    validate_semantic_text(&args.text)?;
    if let Some(labels) = &args.labels {
        if labels.iter().any(|label| label.trim().is_empty()) {
            return Err(DomainError::InvalidInput(
                "memory_recall_semantic labels must contain non-empty labels".into(),
            )
            .into());
        }
        let mut seen = HashSet::new();
        for label in labels {
            if !seen.insert(label.trim()) {
                return Err(DomainError::InvalidInput(format!(
                    "memory_recall_semantic labels contains duplicate label {:?}",
                    label.trim()
                ))
                .into());
            }
        }
    }
    if let Some(limit) = args.limit {
        if !(1..=100).contains(&limit) {
            return Err(DomainError::InvalidInput(
                "memory_recall_semantic limit must be 1..=100".into(),
            )
            .into());
        }
    }
    Ok(())
}

/// Embed the query, fuse ranked direct hits with activations, return current facts.
pub async fn memory_semantic_search(
    graph: &Graph,
    runtime: Option<&SemanticRuntime>,
    secrets_path: PathBuf,
    args: SemanticSearchArgs,
) -> Result<Value> {
    validate_semantic_search_args(&args)?;
    let runtime = runtime.ok_or_else(|| {
        embedding_error!(
            "semantic search requires OPENAI_API_KEY or XAI_API_KEY in {} or the process environment",
            secrets_path.display()
        )
    })?;
    let text = validate_semantic_text(&args.text)?;
    let layers = validate_layer_ids(args.scope)?
        .into_iter()
        .map(|layer| layer.into_string())
        .collect::<Vec<_>>();
    let labels = args.labels.unwrap_or_default();
    let limit = args.limit.unwrap_or(20) as usize;
    let embedding = runtime.provider.embed(&text).await?;

    let direct_args = SearchArgs {
        layers: layers.clone(),
        text: Some(text.clone()),
        labels: Some(labels.clone()),
        limit: Some(100),
    };
    let (activations, direct) = tokio::try_join!(
        query_activations(graph, runtime, &embedding),
        memory_search(graph, direct_args),
    )?;
    let direct_facts = direct
        .get("facts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut facts_by_iri = HashMap::new();
    let mut direct_order = Vec::new();
    for fact in direct_facts {
        if let Some(iri) = crate::search::fact_handle_iri(&fact) {
            direct_order.push(iri.to_string());
            facts_by_iri.insert(iri.to_string(), fact);
        }
    }

    let recalled_iris = activations
        .iter()
        .flat_map(|activation| activation.result_refs.iter().cloned())
        .collect::<HashSet<_>>();
    let missing = recalled_iris
        .into_iter()
        .filter(|iri| !facts_by_iri.contains_key(iri))
        .collect::<Vec<_>>();
    for (iri, fact) in resolve_facts(graph, &layers, &labels, missing).await? {
        facts_by_iri.insert(iri, fact);
    }
    let contributing_activation_ids = contributing_activation_ids(&activations, &facts_by_iri);

    let mut fused = HashMap::<String, f64>::new();
    add_ranked(
        &mut fused,
        &direct_order,
        runtime.config.direct_weight,
        runtime.config.rrf_k,
        &facts_by_iri,
    );
    for activation in &activations {
        add_ranked(
            &mut fused,
            &activation.result_refs,
            activation.similarity,
            runtime.config.rrf_k,
            &facts_by_iri,
        );
    }
    let mut ranked = fused.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    let truncated = ranked.len() > limit;
    ranked.truncate(limit);
    let result_refs = ranked
        .iter()
        .map(|(iri, _)| iri.clone())
        .collect::<Vec<_>>();
    let facts = ranked
        .into_iter()
        .enumerate()
        .filter_map(|(index, (iri, _))| {
            facts_by_iri.remove(&iri).map(|mut fact| {
                fact["rank"] = json!(index + 1);
                fact
            })
        })
        .collect::<Vec<_>>();
    let endpoint_iris = facts
        .iter()
        .flat_map(|fact| {
            [
                fact.pointer("/s/iri").and_then(Value::as_str),
                fact.pointer("/o/iri").and_then(Value::as_str),
            ]
        })
        .flatten()
        .collect::<HashSet<_>>();
    let about = direct
        .get("about")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|item| {
            item.get("about")
                .and_then(Value::as_str)
                .is_some_and(|iri| endpoint_iris.contains(iri))
        })
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();

    persist_activation(
        graph,
        runtime,
        &embedding,
        &result_refs,
        &activations,
        &contributing_activation_ids,
    )
    .await?;

    Ok(json!({
        "ok": true,
        "mode": "semantic",
        "facts": facts,
        "nodes": [],
        "paths": [],
        "about": about,
        "lookups": [],
        "scope": layers,
        "truncated": truncated,
    }))
}

fn add_ranked(
    fused: &mut HashMap<String, f64>,
    iris: &[String],
    weight: f64,
    rrf_k: f64,
    facts: &HashMap<String, Value>,
) {
    for (index, iri) in iris.iter().enumerate() {
        if facts.contains_key(iri) {
            *fused.entry(iri.clone()).or_default() += weight / (rrf_k + index as f64 + 1.0);
        }
    }
}

fn contributing_activation_ids(
    activations: &[Activation],
    facts: &HashMap<String, Value>,
) -> Vec<String> {
    activations
        .iter()
        .filter(|activation| {
            activation
                .result_refs
                .iter()
                .any(|iri| facts.contains_key(iri))
        })
        .map(|activation| activation.element_id.clone())
        .collect()
}

async fn query_activations(
    graph: &Graph,
    runtime: &SemanticRuntime,
    embedding: &[f64],
) -> Result<Vec<Activation>> {
    let rows = fetch_all(
        graph,
        query(&format!(
            "CALL db.index.vector.queryNodes('{SEMANTIC_INDEX}', $neighbors, $embedding) \
             YIELD node, score \
             WHERE node.ttl >= timestamp() AND score >= $threshold \
             RETURN elementId(node) AS elementId, node.resultRefs AS resultRefs, score \
             ORDER BY score DESC, elementId ASC"
        ))
        .param("neighbors", runtime.config.neighbor_limit as i64)
        .param("embedding", embedding.to_vec())
        .param("threshold", runtime.config.recall_similarity_threshold),
    )
    .await
    .context("query semantic activation vector index")?;
    rows.into_iter()
        .map(|row| {
            Ok(Activation {
                element_id: row.get("elementId")?,
                result_refs: row.get::<Vec<String>>("resultRefs").unwrap_or_default(),
                similarity: row.get("score")?,
            })
        })
        .collect()
}

async fn resolve_facts(
    graph: &Graph,
    layers: &[String],
    labels: &[String],
    relationship_iris: Vec<String>,
) -> Result<Vec<(String, Value)>> {
    if relationship_iris.is_empty() {
        return Ok(Vec::new());
    }
    let rows = fetch_all(
        graph,
        query(
            r#"
            MATCH (s:Entity)-[r]->(o:Entity)
            WHERE r.iri IN $iris AND r.validTo IS NULL
              AND (type(r) = 'ASSERTS' OR type(r) = 'ABOUT')
              AND (size(coalesce(s.layers, [])) = 0
                   OR any(layer IN coalesce(s.layers, []) WHERE layer IN $layers))
              AND (size(coalesce(r.layers, [])) = 0
                   OR any(layer IN coalesce(r.layers, []) WHERE layer IN $layers))
              AND (size(coalesce(o.layers, [])) = 0
                   OR any(layer IN coalesce(o.layers, []) WHERE layer IN $layers))
              AND ($labelCount = 0
                   OR any(label IN $labels WHERE label IN labels(s) OR label IN labels(o)))
            RETURN s, r, o
            "#,
        )
        .param("iris", relationship_iris)
        .param("layers", layers.to_vec())
        .param("labels", labels.to_vec())
        .param("labelCount", labels.len() as i64),
    )
    .await?;
    let mut facts = Vec::new();
    for row in rows {
        let s: Node = row.get("s")?;
        let r: Relation = row.get("r")?;
        let o: Node = row.get("o")?;
        let iri = r.get::<String>("iri")?;
        let s_json = endpoint_json(&s);
        let o_json = endpoint_json(&o);
        let s_iri = s.get::<String>("iri").unwrap_or_default();
        let o_iri = o.get::<String>("iri").unwrap_or_default();
        let relation = rel_json(&r, &s_iri, &o_iri);
        let p = r
            .get::<String>("propertyIri")
            .unwrap_or_else(|_| format!("mindreader:property/{}", r.typ()));
        let subject_labels = s
            .labels()
            .into_iter()
            .filter(|label| *label != "Entity")
            .map(str::to_string)
            .collect::<Vec<_>>();
        let effective_weight = s_json["weight"]
            .as_i64()
            .unwrap_or(0)
            .saturating_add(relation["weight"].as_i64().unwrap_or(0))
            .saturating_add(o_json["weight"].as_i64().unwrap_or(0));
        let scope = r.get::<Vec<String>>("layers").unwrap_or_default();
        let mut fact = fact_envelope(
            s_json,
            &p,
            o_json,
            &relation,
            &scope,
            spike_label(&subject_labels).map(Value::String),
        );
        fact["score"] = json!(0.0);
        fact["effectiveWeight"] = json!(effective_weight);
        facts.push((iri, fact));
    }
    Ok(facts)
}

async fn persist_activation(
    graph: &Graph,
    runtime: &SemanticRuntime,
    embedding: &[f64],
    result_refs: &[String],
    neighbors: &[Activation],
    contributing_activation_ids: &[String],
) -> Result<()> {
    let ttl_ms = runtime
        .config
        .ttl_days
        .checked_mul(86_400_000)
        .and_then(|milliseconds| i64::try_from(milliseconds).ok())
        .ok_or_else(|| operation_error!("semantic TTL is too large"))?;
    let convergence = select_convergence(neighbors, result_refs, &runtime.config);
    let mut refreshed_activation_ids = contributing_activation_ids.to_vec();
    if let Some(existing) = convergence {
        refreshed_activation_ids.push(existing.element_id.clone());
    }
    refreshed_activation_ids.sort();
    refreshed_activation_ids.dedup();
    refresh_recalled_activations(graph, &refreshed_activation_ids, ttl_ms).await?;
    if let Some(existing) = convergence {
        let Some(existing_embedding) =
            load_activation_embedding(graph, &existing.element_id).await?
        else {
            return create_activation(graph, embedding, result_refs, ttl_ms).await;
        };
        let midpoint = existing_embedding
            .iter()
            .zip(embedding)
            .map(|(left, right)| left + right)
            .collect::<Vec<_>>();
        let midpoint = normalize_vector(midpoint, runtime.dimensions(), "semantic centroid")?;
        let updated = fetch_one(
            graph,
            query(
                r#"
                MATCH (a:SemanticActivation)
                WHERE elementId(a) = $elementId AND a.ttl >= timestamp()
                SET a.resultRefs = $resultRefs
                WITH a
                CALL db.create.setNodeVectorProperty(a, 'embedding', $embedding)
                RETURN elementId(a) AS elementId
                "#,
            )
            .param("elementId", existing.element_id.clone())
            .param("resultRefs", result_refs.to_vec())
            .param("embedding", midpoint),
        )
        .await?;
        if updated.is_some() {
            return Ok(());
        }
    }
    create_activation(graph, embedding, result_refs, ttl_ms).await
}

fn select_convergence<'a>(
    neighbors: &'a [Activation],
    result_refs: &[String],
    config: &SemanticConfig,
) -> Option<&'a Activation> {
    neighbors
        .iter()
        .filter(|activation| {
            activation.similarity >= config.convergence_similarity_threshold
                && jaccard(&activation.result_refs, result_refs)
                    >= config.convergence_result_overlap_threshold
        })
        .max_by(|left, right| {
            left.similarity
                .partial_cmp(&right.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        })
}

async fn refresh_recalled_activations(
    graph: &Graph,
    element_ids: &[String],
    ttl_ms: i64,
) -> Result<()> {
    if element_ids.is_empty() {
        return Ok(());
    }
    fetch_one(
        graph,
        query(
            r#"
            UNWIND $elementIds AS activationId
            MATCH (a:SemanticActivation:TTL)
            WHERE elementId(a) = activationId AND a.ttl >= timestamp()
            WITH DISTINCT a
            CALL apoc.ttl.expireIn(a, $ttl, 'ms')
            RETURN count(a) AS refreshed
            "#,
        )
        .param("elementIds", element_ids.to_vec())
        .param("ttl", ttl_ms),
    )
    .await
    .context("refresh recalled semantic activation TTLs")?;
    Ok(())
}

async fn load_activation_embedding(graph: &Graph, element_id: &str) -> Result<Option<Vec<f64>>> {
    fetch_one(
        graph,
        query(
            "MATCH (a:SemanticActivation) \
             WHERE elementId(a) = $elementId AND a.ttl >= timestamp() \
             RETURN a.embedding AS embedding",
        )
        .param("elementId", element_id.to_string()),
    )
    .await?
    .map(|row| row.get::<Vec<f64>>("embedding").map_err(Into::into))
    .transpose()
}

async fn create_activation(
    graph: &Graph,
    embedding: &[f64],
    result_refs: &[String],
    ttl_ms: i64,
) -> Result<()> {
    fetch_one(
        graph,
        query(
            r#"
            CREATE (a:SemanticActivation:TTL {resultRefs: $resultRefs})
            WITH a
            CALL db.create.setNodeVectorProperty(a, 'embedding', $embedding)
            WITH a
            CALL apoc.ttl.expireIn(a, $ttl, 'ms')
            RETURN elementId(a) AS elementId
            "#,
        )
        .param("resultRefs", result_refs.to_vec())
        .param("embedding", embedding.to_vec())
        .param("ttl", ttl_ms),
    )
    .await?;
    Ok(())
}

fn jaccard(left: &[String], right: &[String]) -> f64 {
    let left = left.iter().collect::<HashSet<_>>();
    let right = right.iter().collect::<HashSet<_>>();
    if left.is_empty() && right.is_empty() {
        return 1.0;
    }
    let intersection = left.intersection(&right).count() as f64;
    let union = left.union(&right).count() as f64;
    intersection / union
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn semantic_text_is_trimmed_and_byte_bounded() {
        assert_eq!(validate_semantic_text("  recall  ").unwrap(), "recall");
        assert!(matches!(
            validate_semantic_text("   ").unwrap_err(),
            crate::error::Error::Domain(DomainError::InvalidInput(_))
        ));
        assert!(validate_semantic_text(&"x".repeat(MAX_SEMANTIC_TEXT_BYTES)).is_ok());
        assert!(matches!(
            validate_semantic_text(&"é".repeat(MAX_SEMANTIC_TEXT_BYTES / 2 + 1)).unwrap_err(),
            crate::error::Error::Domain(DomainError::InvalidInput(_))
        ));
    }

    #[test]
    fn semantic_args_validate_scope_labels_and_limit() {
        let args = SemanticSearchArgs {
            scope: vec!["project:mindreader".into()],
            text: "recall".into(),
            labels: Some(vec!["Element".into()]),
            limit: Some(100),
        };
        assert!(validate_semantic_search_args(&args).is_ok());
        assert!(validate_semantic_search_args(&SemanticSearchArgs {
            limit: Some(101),
            ..args.clone()
        })
        .is_err());
        assert!(validate_semantic_search_args(&SemanticSearchArgs {
            labels: Some(vec!["Element".into(), " Element ".into()]),
            ..args
        })
        .is_err());
    }

    #[test]
    fn overlap_is_jaccard_similarity() {
        assert_eq!(jaccard(&[], &[]), 1.0);
        assert_eq!(
            jaccard(&["a".into(), "b".into()], &["b".into(), "c".into()]),
            1.0 / 3.0
        );
    }

    #[test]
    fn rank_fusion_ignores_unresolvable_references() {
        let mut facts = HashMap::new();
        facts.insert("a".into(), json!({}));
        let mut fused = HashMap::new();
        add_ranked(
            &mut fused,
            &["missing".into(), "a".into()],
            2.0,
            60.0,
            &facts,
        );
        assert!(!fused.contains_key("missing"));
        assert!(fused["a"] > 0.0);
    }

    #[test]
    fn only_activations_with_resolved_visible_facts_contribute() {
        let activations = vec![
            Activation {
                element_id: "visible".into(),
                result_refs: vec!["missing".into(), "fact".into()],
                similarity: 0.9,
            },
            Activation {
                element_id: "unresolved".into(),
                result_refs: vec!["missing".into()],
                similarity: 0.99,
            },
            Activation {
                element_id: "empty".into(),
                result_refs: Vec::new(),
                similarity: 1.0,
            },
        ];
        let facts = HashMap::from([("fact".into(), json!({}))]);

        assert_eq!(
            contributing_activation_ids(&activations, &facts),
            vec!["visible"]
        );
    }

    #[test]
    fn convergence_selects_highest_similarity_and_last_equal_tie() {
        let config = SemanticConfig {
            convergence_similarity_threshold: 0.8,
            convergence_result_overlap_threshold: 0.5,
            ..SemanticConfig::default()
        };
        let neighbors = vec![
            Activation {
                element_id: "a".into(),
                result_refs: vec!["one".into(), "two".into()],
                similarity: 0.95,
            },
            Activation {
                element_id: "b".into(),
                result_refs: vec!["one".into(), "two".into()],
                similarity: 0.95,
            },
            Activation {
                element_id: "c".into(),
                result_refs: vec!["other".into()],
                similarity: 0.99,
            },
        ];
        assert_eq!(
            select_convergence(&neighbors, &["one".into(), "two".into()], &config,)
                .map(|activation| activation.element_id.as_str()),
            Some("b")
        );
        assert!(select_convergence(&neighbors, &["missing".into()], &config).is_none());
    }
}
