use crate::config::{Config, SemanticConfig};
use crate::embeddings::{build_provider, normalize_vector, EmbeddingProvider};
use crate::graph::{endpoint_json, fetch_all, rel_json, spike_label, SEMANTIC_INDEX};
use crate::layers::validate_layer_ids;
use crate::tools::{memory_search, SearchArgs};
use anyhow::{anyhow, Context, Result};
use neo4rs::{query, Graph, Node, Relation};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SemanticSearchArgs {
    pub text: String,
    pub layers: Vec<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Clone)]
pub struct SemanticRuntime {
    provider: Arc<dyn EmbeddingProvider>,
    config: SemanticConfig,
}

impl SemanticRuntime {
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

    pub fn new(provider: Arc<dyn EmbeddingProvider>, config: SemanticConfig) -> Self {
        Self { provider, config }
    }
}

#[derive(Debug, Clone)]
struct Activation {
    element_id: String,
    embedding: Vec<f64>,
    result_refs: Vec<String>,
    similarity: f64,
}

pub async fn memory_semantic_search(
    graph: &Graph,
    runtime: Option<&SemanticRuntime>,
    secrets_path: PathBuf,
    args: SemanticSearchArgs,
) -> Result<Value> {
    let runtime = runtime.ok_or_else(|| {
        anyhow!(
            "semantic search requires OPENAI_API_KEY or XAI_API_KEY in {} or the process environment",
            secrets_path.display()
        )
    })?;
    let text = args.text.trim().to_string();
    if text.is_empty() {
        return Err(anyhow!("memory_semantic_search text must not be empty"));
    }
    let layers = validate_layer_ids(args.layers)?
        .into_iter()
        .map(|layer| layer.into_string())
        .collect::<Vec<_>>();
    let labels = args.labels.unwrap_or_default();
    let limit = args.limit.unwrap_or(20).clamp(1, 100) as usize;
    let embedding = runtime.provider.embed(&text).await?;

    let activations = query_activations(graph, runtime, &embedding).await?;
    let direct = memory_search(
        graph,
        SearchArgs {
            layers: layers.clone(),
            text: Some(text.clone()),
            labels: Some(labels.clone()),
            limit: Some(100),
        },
    )
    .await?;
    let direct_facts = direct
        .get("facts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut facts_by_iri = HashMap::new();
    let mut direct_order = Vec::new();
    for fact in direct_facts {
        if let Some(iri) = fact.pointer("/relationship/iri").and_then(Value::as_str) {
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

    persist_activation(graph, runtime, &embedding, &result_refs, &activations).await?;

    Ok(json!({
        "query": text,
        "mode": "semantic",
        "facts": facts,
        "spike": direct.get("spike").cloned().unwrap_or_else(|| json!([])),
        "layers": layers,
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
             RETURN elementId(node) AS elementId, node.embedding AS embedding, \
                    node.resultRefs AS resultRefs, score \
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
                embedding: row.get("embedding")?,
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
        facts.push((
            iri,
            json!({
                "s": s_json,
                "p": p,
                "o": o_json,
                "relationship": relation,
                "layers": r.get::<Vec<String>>("layers").unwrap_or_default(),
                "spike": spike_label(&subject_labels),
                "score": 0.0,
                "effectiveWeight": effective_weight,
            }),
        ));
    }
    Ok(facts)
}

async fn persist_activation(
    graph: &Graph,
    runtime: &SemanticRuntime,
    embedding: &[f64],
    result_refs: &[String],
    neighbors: &[Activation],
) -> Result<()> {
    let convergence = neighbors
        .iter()
        .filter(|activation| {
            activation.similarity >= runtime.config.convergence_similarity_threshold
                && jaccard(&activation.result_refs, result_refs)
                    >= runtime.config.convergence_result_overlap_threshold
        })
        .max_by(|left, right| {
            left.similarity
                .partial_cmp(&right.similarity)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    let ttl_ms = runtime
        .config
        .ttl_days
        .checked_mul(86_400_000)
        .and_then(|milliseconds| i64::try_from(milliseconds).ok())
        .ok_or_else(|| anyhow!("semantic TTL is too large"))?;
    if let Some(existing) = convergence {
        let midpoint = existing
            .embedding
            .iter()
            .zip(embedding)
            .map(|(left, right)| left + right)
            .collect::<Vec<_>>();
        let midpoint = normalize_vector(midpoint, runtime.dimensions(), "semantic centroid")?;
        let updated = fetch_all(
            graph,
            query(
                r#"
                MATCH (a:SemanticActivation)
                WHERE elementId(a) = $elementId AND a.ttl >= timestamp()
                SET a.resultRefs = $resultRefs
                WITH a
                CALL db.create.setNodeVectorProperty(a, 'embedding', $embedding)
                WITH a
                CALL apoc.ttl.expireIn(a, $ttl, 'ms')
                RETURN elementId(a) AS elementId
                "#,
            )
            .param("elementId", existing.element_id.clone())
            .param("resultRefs", result_refs.to_vec())
            .param("embedding", midpoint)
            .param("ttl", ttl_ms),
        )
        .await?;
        if !updated.is_empty() {
            return Ok(());
        }
    }
    fetch_all(
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
}
