//! Semantic recall: embed a query, fuse it with remembered activations, and persist a new bundle.
//!
//! Separate from closed-world `recall`. Under the request `scope`, fuses ranked
//! direct ordinary `ASSERTS` hits with live TTL activation bundles (RRF) and
//! weaker non-learning one-hop structural context. Query text goes to the
//! configured embedding provider; a missing runtime fails as `missing_embedding`.
//! Successful recalls may refresh, converge, or mint activations, so this path
//! is intentionally not read-only.

use crate::config::{Config, EmbeddingSpace, SemanticConfig};
use crate::domain::{normalize_rfc3339, DomainError};
use crate::embeddings::{build_provider, normalize_vector, EmbeddingProvider};
use crate::graph::{
    endpoint_json, fact_envelope, fetch_all, fetch_one, rel_json, require_embedding_space,
    SEMANTIC_INDEX,
};
use crate::layers::{normalize_scope, validate_layer_ids};
use crate::search::{memory_search_with_matches, FactGroup, SearchArgs, SearchResult, TextMatch};
use crate::{
    embedding_error,
    error::{Context, Result},
    operation_error,
};
use neo4rs::{query, Graph, Node, Relation};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::Arc;

/// Maximum UTF-8 byte length accepted for semantic query text.
pub const MAX_SEMANTIC_TEXT_BYTES: usize = 32 * 1024;
/// Keep learned activation bundles focused instead of caching an entire response page.
const MAX_ACTIVATION_RESULT_REFS: usize = 3;
/// Bound ephemeral graph expansion independently of the caller's result limit.
const MAX_STRUCTURAL_ANCHORS: usize = 16;
/// Bound each anchor endpoint's visible one-hop candidates.
const MAX_STRUCTURAL_FACTS_PER_ENDPOINT: i64 = 16;
/// Structural context must remain weaker than the evidence for its anchor.
const STRUCTURAL_EVIDENCE_SCALE: f64 = 0.25;

/// Arguments for the side-effectful `recall_semantic` operation.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SemanticSearchArgs {
    /// Request visibility union; empty is global-only.
    pub scope: Vec<String>,
    /// Query text sent to the embedding provider (trimmed, byte-bounded).
    pub text: String,
    /// Optional labels used to filter fused facts; catalog labels are not special here.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// `concise` or `detailed`; omitted defaults to detailed.
    #[serde(default)]
    pub detail: Option<String>,
    /// Maximum fused facts; default 20, at most 100.
    #[serde(default)]
    pub limit: Option<u32>,
    /// Optional world-time instant; only explicitly qualified state facts can match.
    #[serde(default, rename = "effectiveAt")]
    pub effective_at: Option<String>,
}

/// Process-wide embedding provider plus fusion tunables. Absent at [`SemanticRuntime::from_config`] when no API key selected a provider.
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

    /// Selected embedding backend id (`openai`, `xai`, or a fixture).
    pub fn provider(&self) -> &'static str {
        self.provider.provider()
    }

    /// Provider model id that must match the stored embedding-space marker.
    pub fn model(&self) -> &str {
        self.provider.model()
    }

    /// Vector length the Neo4j activation index and HTTP provider must share.
    pub fn dimensions(&self) -> usize {
        self.provider.dimensions()
    }

    /// Construct a runtime with an explicit provider (tests and smoke fixtures).
    #[cfg(feature = "developer-tools")]
    pub fn new(provider: Arc<dyn EmbeddingProvider>, config: SemanticConfig) -> Self {
        Self { provider, config }
    }
}

/// One live activation neighbor: Neo4j element id, remembered fact IRIs, and cosine score.
#[derive(Debug, Clone)]
struct Activation {
    element_id: String,
    result_refs: Vec<String>,
    similarity: f64,
}

/// One visible one-hop fact reached from a grounded semantic anchor.
struct StructuralCandidate {
    anchor_iri: String,
    fact_iri: String,
    property_iri: String,
    degree: usize,
    fact: Value,
}

/// Trim query text and reject empty or oversized UTF-8 payloads before any HTTP call.
fn validate_semantic_text(text: &str) -> Result<String> {
    let text = text.trim();
    if text.is_empty() {
        return Err(
            DomainError::InvalidInput("recall_semantic text must not be empty".into()).into(),
        );
    }
    if text.len() > MAX_SEMANTIC_TEXT_BYTES {
        return Err(DomainError::InvalidInput(format!(
            "recall_semantic text must not exceed {MAX_SEMANTIC_TEXT_BYTES} UTF-8 bytes"
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
                "recall_semantic labels must contain non-empty labels".into(),
            )
            .into());
        }
        let mut seen = HashSet::new();
        for label in labels {
            if !seen.insert(label.trim()) {
                return Err(DomainError::InvalidInput(format!(
                    "recall_semantic labels contains duplicate label {:?}",
                    label.trim()
                ))
                .into());
            }
        }
    }
    if let Some(limit) = args.limit {
        if !(1..=100).contains(&limit) {
            return Err(
                DomainError::InvalidInput("recall_semantic limit must be 1..=100".into()).into(),
            );
        }
    }
    if let Some(effective_at) = args.effective_at.as_deref() {
        normalize_rfc3339(effective_at, "recall_semantic effectiveAt")?;
    }
    crate::payload::Detail::parse(args.detail.as_deref())?;
    Ok(())
}

/// Embed the query, fuse direct `ASSERTS` hits with live activations and structural context, then persist a bundle.
///
/// Requires a [`SemanticRuntime`]; otherwise `missing_embedding`. Default limit is 20 (max 100).
/// Persistence writes at most three bundle-eligible direct fact handles and may converge embeddings.
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
    let layers = normalize_scope(args.scope)?;
    let labels = args.labels.unwrap_or_default();
    let limit = args.limit.unwrap_or(20) as usize;
    let effective_at = args
        .effective_at
        .as_deref()
        .map(|value| normalize_rfc3339(value, "recall_semantic effectiveAt"))
        .transpose()?;
    let embedding = runtime.provider.embed(&text).await?;

    // Direct recall is capped at 100 so fusion can rerank before the caller limit.
    let direct_args = SearchArgs {
        layers: layers.clone(),
        text: Some(text.clone()),
        labels: Some(labels.clone()),
        limit: Some(100),
        effective_at: effective_at.clone(),
    };
    let (activations, direct_result) = tokio::try_join!(
        query_activations(graph, runtime, &embedding),
        memory_search_with_matches(graph, direct_args),
    )?;
    let SearchResult {
        facts: direct_facts,
        about: direct_about,
        text_matches: direct_matches,
        mut fact_groups,
        ..
    } = direct_result;
    let mut facts_by_iri = HashMap::new();
    let mut direct_order = Vec::new();
    for fact in direct_facts {
        let iri = crate::search::fact_handle_iri(&fact)?.to_string();
        direct_order.push(iri.clone());
        facts_by_iri.insert(iri, fact);
    }

    let recalled_iris = activations
        .iter()
        .flat_map(|activation| activation.result_refs.iter().cloned())
        .collect::<HashSet<_>>();
    let missing = recalled_iris
        .into_iter()
        .filter(|iri| !facts_by_iri.contains_key(iri))
        .collect::<Vec<_>>();
    for (iri, fact, group) in
        resolve_facts(graph, &layers, &labels, missing, effective_at.as_deref()).await?
    {
        fact_groups.insert(iri.clone(), group);
        facts_by_iri.insert(iri, fact);
    }
    let contributing_activation_ids = contributing_activation_ids(&activations, &facts_by_iri);

    let mut fused = HashMap::<String, f64>::new();
    add_direct_ranked(
        &mut fused,
        &direct_order,
        runtime.config.direct_weight,
        runtime.config.keyword_weight,
        runtime.config.rrf_k,
        &facts_by_iri,
        &direct_matches,
    );
    let mut structural_anchor_scores = direct_order
        .iter()
        .filter(|iri| {
            direct_matches
                .get(*iri)
                .is_some_and(|evidence| evidence.bundle_eligible)
        })
        .filter_map(|iri| fused.get(iri).map(|score| (iri.clone(), *score)))
        .collect::<HashMap<_, _>>();
    let mut activation_scores = HashMap::<String, f64>::new();
    for activation in &activations {
        add_ranked_max(
            &mut activation_scores,
            &activation.result_refs,
            activation_evidence(
                activation.similarity,
                runtime.config.recall_similarity_threshold,
                runtime.config.keyword_weight,
            ),
            runtime.config.rrf_k,
            &facts_by_iri,
        );
    }
    for (iri, score) in activation_scores {
        structural_anchor_scores
            .entry(iri.clone())
            .and_modify(|current| *current = current.max(score))
            .or_insert(score);
        *fused.entry(iri).or_default() += score;
    }
    let structural_anchors = top_structural_anchors(&structural_anchor_scores);
    let structural_candidates = resolve_structural_facts(
        graph,
        &layers,
        &labels,
        structural_anchors
            .iter()
            .map(|(iri, _)| iri.clone())
            .collect(),
        effective_at.as_deref(),
    )
    .await?;
    add_structural_ranked(
        &mut fused,
        &mut facts_by_iri,
        &structural_anchor_scores,
        structural_candidates,
        STRUCTURAL_EVIDENCE_SCALE,
    );
    let mut ranked = fused.into_iter().collect::<Vec<_>>();
    ranked.sort_by(|left, right| compare_fused(left, right, &direct_matches));
    let ranked_refs = ranked
        .iter()
        .map(|(iri, _)| iri.clone())
        .collect::<Vec<_>>();
    let truncated = ranked.len() > limit;
    ranked.truncate(limit);
    let facts = ranked
        .into_iter()
        .enumerate()
        .filter_map(|(index, (iri, fused_score))| {
            facts_by_iri.remove(&iri).map(|mut fact| {
                fact["rank"] = json!(index + 1);
                fact["score"] = json!(fused_score);
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
    let about = direct_about
        .iter()
        .filter(|item| {
            item.get("about")
                .and_then(Value::as_str)
                .is_some_and(|iri| endpoint_iris.contains(iri))
        })
        .take(limit)
        .cloned()
        .collect::<Vec<_>>();

    if !ranked_refs.is_empty() {
        // Persist only bundle-eligible direct handles; structural and activation-only hits must not train the next bundle.
        let direct_ranked_refs = ranked_refs
            .into_iter()
            .filter(|iri| {
                direct_matches
                    .get(iri)
                    .is_some_and(|evidence| evidence.bundle_eligible)
            })
            .collect::<Vec<_>>();
        let activation_result_refs = select_activation_refs(&direct_ranked_refs, &fact_groups);
        persist_activation(
            graph,
            runtime,
            &embedding,
            &activation_result_refs,
            &activations,
            &contributing_activation_ids,
        )
        .await?;
    }

    crate::payload::finish_recall(
        json!({
            "ok": true,
            "mode": "semantic",
            "facts": facts,
            "nodes": [],
            "paths": [],
            "about": about,
            "lookups": [],
            "scope": layers,
            "truncated": truncated,
        }),
        &layers,
        crate::payload::Detail::parse(args.detail.as_deref())?,
    )
}

/// Convert admitted cosine similarity into bounded evidence above its threshold floor.
fn activation_evidence(similarity: f64, threshold: f64, keyword_weight: f64) -> f64 {
    keyword_weight * ((similarity - threshold) / (1.0 - threshold)).clamp(0.0, 1.0)
}

/// Sort fused scores descending; on ties prefer a fact with a direct text match, then IRI.
fn compare_fused(
    left: &(String, f64),
    right: &(String, f64),
    direct_matches: &HashMap<String, TextMatch>,
) -> Ordering {
    right
        .1
        .partial_cmp(&left.1)
        .unwrap_or(Ordering::Equal)
        .then_with(|| {
            direct_matches
                .contains_key(&right.0)
                .cmp(&direct_matches.contains_key(&left.0))
        })
        .then_with(|| left.0.cmp(&right.0))
}

/// Keep a three-fact bundle representative without discarding set-valued groups entirely.
fn select_activation_refs(
    ranked_iris: &[String],
    groups: &HashMap<String, FactGroup>,
) -> Vec<String> {
    let mut selected = Vec::with_capacity(MAX_ACTIVATION_RESULT_REFS);
    let mut selected_iris = HashSet::new();
    let mut group_counts = HashMap::<&FactGroup, usize>::new();
    for iri in ranked_iris {
        if selected.len() == MAX_ACTIVATION_RESULT_REFS || selected_iris.contains(iri) {
            continue;
        }
        if let Some(group) = groups.get(iri) {
            let count = group_counts.entry(group).or_default();
            if *count >= 2 {
                continue;
            }
            *count += 1;
        }
        selected_iris.insert(iri.clone());
        selected.push(iri.clone());
    }
    if selected.len() < MAX_ACTIVATION_RESULT_REFS {
        for iri in ranked_iris {
            if selected.len() == MAX_ACTIVATION_RESULT_REFS {
                break;
            }
            if selected_iris.insert(iri.clone()) {
                selected.push(iri.clone());
            }
        }
    }
    selected
}

/// Direct reciprocal-rank contribution with stronger exact than fallback keyword evidence.
fn add_direct_ranked(
    fused: &mut HashMap<String, f64>,
    iris: &[String],
    exact_weight: f64,
    keyword_weight: f64,
    rrf_k: f64,
    facts: &HashMap<String, Value>,
    matches: &HashMap<String, TextMatch>,
) {
    let mut direct_rank = 0;
    for iri in iris {
        if facts.contains_key(iri) {
            let Some(evidence) = matches.get(iri) else {
                continue;
            };
            let weight = if evidence.exact {
                exact_weight
            } else {
                keyword_weight * evidence.keyword_confidence
            };
            if weight <= 0.0 {
                continue;
            }
            *fused.entry(iri.clone()).or_default() += weight / (rrf_k + direct_rank as f64 + 1.0);
            direct_rank += 1;
        }
    }
}

/// Keep the strongest bounded anchor set for ephemeral one-hop graph expansion.
fn top_structural_anchors(scores: &HashMap<String, f64>) -> Vec<(String, f64)> {
    let mut ranked = scores
        .iter()
        .map(|(iri, score)| (iri.clone(), *score))
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.0.cmp(&right.0))
    });
    ranked.truncate(MAX_STRUCTURAL_ANCHORS);
    ranked
}

/// Add non-learning structural context without boosting facts that already have evidence.
fn add_structural_ranked(
    fused: &mut HashMap<String, f64>,
    facts: &mut HashMap<String, Value>,
    anchor_scores: &HashMap<String, f64>,
    candidates: Vec<StructuralCandidate>,
    scale: f64,
) {
    let mut unique = HashMap::<(String, String), StructuralCandidate>::new();
    for candidate in candidates {
        let key = (candidate.anchor_iri.clone(), candidate.fact_iri.clone());
        unique
            .entry(key)
            .and_modify(|current| {
                if candidate.degree < current.degree {
                    current.degree = candidate.degree;
                }
            })
            .or_insert(candidate);
    }
    let mut group_sizes = HashMap::<(String, String), usize>::new();
    for candidate in unique.values() {
        *group_sizes
            .entry((candidate.anchor_iri.clone(), candidate.property_iri.clone()))
            .or_default() += 1;
    }
    let mut structural_scores = HashMap::<String, f64>::new();
    for (_, candidate) in unique {
        let Some(anchor_score) = anchor_scores.get(&candidate.anchor_iri) else {
            continue;
        };
        facts
            .entry(candidate.fact_iri.clone())
            .or_insert(candidate.fact);
        if fused.contains_key(&candidate.fact_iri) {
            continue;
        }
        let group_size = group_sizes
            .get(&(candidate.anchor_iri, candidate.property_iri))
            .copied()
            .unwrap_or(1);
        let score = anchor_score * scale
            / (candidate.degree.max(1) as f64).sqrt()
            / (group_size as f64).sqrt();
        structural_scores
            .entry(candidate.fact_iri)
            .and_modify(|current| *current = current.max(score))
            .or_insert(score);
    }
    for (iri, score) in structural_scores {
        fused.entry(iri).or_insert(score);
    }
}

/// Keep only the strongest activation contribution per fact to avoid popularity amplification.
fn add_ranked_max(
    fused: &mut HashMap<String, f64>,
    iris: &[String],
    weight: f64,
    rrf_k: f64,
    facts: &HashMap<String, Value>,
) {
    for (index, iri) in iris.iter().enumerate() {
        if facts.contains_key(iri) {
            let contribution = weight / (rrf_k + index as f64 + 1.0);
            fused
                .entry(iri.clone())
                .and_modify(|current| *current = current.max(contribution))
                .or_insert(contribution);
        }
    }
}

/// Activation element ids whose remembered fact IRIs resolved under this `scope`.
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

/// Vector-search live activations above the recall similarity threshold.
async fn query_activations(
    graph: &Graph,
    runtime: &SemanticRuntime,
    embedding: &[f64],
) -> Result<Vec<Activation>> {
    require_embedding_space(
        graph,
        &EmbeddingSpace {
            provider: runtime.provider().into(),
            model: runtime.model().into(),
            dimensions: runtime.dimensions(),
        },
    )
    .await?;
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
                result_refs: row.get::<Vec<String>>("resultRefs")?,
                similarity: row.get("score")?,
            })
        })
        .collect()
}

/// Load current visible ordinary fact envelopes for activation result IRIs.
async fn resolve_facts(
    graph: &Graph,
    layers: &[String],
    labels: &[String],
    relationship_iris: Vec<String>,
    effective_at: Option<&str>,
) -> Result<Vec<(String, Value, FactGroup)>> {
    if relationship_iris.is_empty() {
        return Ok(Vec::new());
    }
    let rows = fetch_all(
        graph,
        query(
            r#"
            MATCH (s:Entity)-[r]->(o:Entity)
            WHERE r.iri IN $iris AND r.validTo IS NULL
              AND type(r) = 'ASSERTS'
              AND ($effectiveAt IS NULL
                   OR (coalesce(r.effectiveQualified, false)
                       AND (r.effectiveFrom IS NULL
                            OR r.effectiveFrom <= datetime($effectiveAt))
                       AND (r.effectiveTo IS NULL
                            OR datetime($effectiveAt) < r.effectiveTo)))
              AND (size(s.layers) = 0
                   OR any(layer IN s.layers WHERE layer IN $layers))
              AND (size(r.layers) = 0
                   OR any(layer IN r.layers WHERE layer IN $layers))
              AND (size(o.layers) = 0
                   OR any(layer IN o.layers WHERE layer IN $layers))
              AND ($labelCount = 0
                   OR any(label IN $labels WHERE label IN labels(s) OR label IN labels(o)))
            RETURN s, r, o
            "#,
        )
        .param("iris", relationship_iris)
        .param("layers", layers.to_vec())
        .param("labels", labels.to_vec())
        .param("labelCount", labels.len() as i64)
        .param("effectiveAt", effective_at.map(str::to_string)),
    )
    .await?;
    let mut facts = Vec::new();
    for row in rows {
        facts.push(resolved_fact(row.get("s")?, row.get("r")?, row.get("o")?)?);
    }
    Ok(facts)
}

/// Load bounded current visible one-hop facts around grounded semantic anchors.
async fn resolve_structural_facts(
    graph: &Graph,
    layers: &[String],
    labels: &[String],
    anchor_iris: Vec<String>,
    effective_at: Option<&str>,
) -> Result<Vec<StructuralCandidate>> {
    if anchor_iris.is_empty() {
        return Ok(Vec::new());
    }
    let rows = fetch_all(
        graph,
        query(
            r#"
            UNWIND $anchorIris AS anchorIri
            MATCH (anchorS:Entity)-[anchor:ASSERTS]->(anchorO:Entity)
            WHERE anchor.iri = anchorIri AND anchor.validTo IS NULL
              AND ($effectiveAt IS NULL
                   OR (coalesce(anchor.effectiveQualified, false)
                       AND (anchor.effectiveFrom IS NULL
                            OR anchor.effectiveFrom <= datetime($effectiveAt))
                       AND (anchor.effectiveTo IS NULL
                            OR datetime($effectiveAt) < anchor.effectiveTo)))
              AND (size(anchorS.layers) = 0
                   OR any(layer IN anchorS.layers WHERE layer IN $layers))
              AND (size(anchor.layers) = 0
                   OR any(layer IN anchor.layers WHERE layer IN $layers))
              AND (size(anchorO.layers) = 0
                   OR any(layer IN anchorO.layers WHERE layer IN $layers))
              AND ($labelCount = 0
                   OR any(label IN $labels
                          WHERE label IN labels(anchorS) OR label IN labels(anchorO)))
            UNWIND [anchorS, anchorO] AS shared
            CALL {
              WITH anchor, shared
              MATCH (shared)-[degreeRelationship:ASSERTS]-(degreeOther:Entity)
              WHERE degreeRelationship <> anchor AND degreeRelationship.validTo IS NULL
                AND ($effectiveAt IS NULL
                     OR (coalesce(degreeRelationship.effectiveQualified, false)
                         AND (degreeRelationship.effectiveFrom IS NULL
                              OR degreeRelationship.effectiveFrom <= datetime($effectiveAt))
                         AND (degreeRelationship.effectiveTo IS NULL
                              OR datetime($effectiveAt) < degreeRelationship.effectiveTo)))
                AND (size(shared.layers) = 0
                     OR any(layer IN shared.layers WHERE layer IN $layers))
                AND (size(degreeRelationship.layers) = 0
                     OR any(layer IN degreeRelationship.layers WHERE layer IN $layers))
                AND (size(degreeOther.layers) = 0
                     OR any(layer IN degreeOther.layers WHERE layer IN $layers))
                AND ($labelCount = 0
                     OR any(label IN $labels
                            WHERE label IN labels(shared) OR label IN labels(degreeOther)))
              RETURN count(degreeRelationship) AS degree
            }
            CALL {
              WITH anchor, shared
              MATCH (shared)-[candidate:ASSERTS]-(other:Entity)
              WHERE candidate <> anchor AND candidate.validTo IS NULL
                AND ($effectiveAt IS NULL
                     OR (coalesce(candidate.effectiveQualified, false)
                         AND (candidate.effectiveFrom IS NULL
                              OR candidate.effectiveFrom <= datetime($effectiveAt))
                         AND (candidate.effectiveTo IS NULL
                              OR datetime($effectiveAt) < candidate.effectiveTo)))
                AND (size(shared.layers) = 0
                     OR any(layer IN shared.layers WHERE layer IN $layers))
                AND (size(candidate.layers) = 0
                     OR any(layer IN candidate.layers WHERE layer IN $layers))
                AND (size(other.layers) = 0
                     OR any(layer IN other.layers WHERE layer IN $layers))
                AND ($labelCount = 0
                     OR any(label IN $labels
                            WHERE label IN labels(shared) OR label IN labels(other)))
              WITH candidate ORDER BY candidate.iri ASC
              LIMIT $perEndpoint
              RETURN startNode(candidate) AS s, candidate AS r, endNode(candidate) AS o
            }
            RETURN anchorIri, s, r, o, degree
            "#,
        )
        .param("anchorIris", anchor_iris)
        .param("layers", layers.to_vec())
        .param("labels", labels.to_vec())
        .param("labelCount", labels.len() as i64)
        .param("perEndpoint", MAX_STRUCTURAL_FACTS_PER_ENDPOINT)
        .param("effectiveAt", effective_at.map(str::to_string)),
    )
    .await?;
    let mut candidates = Vec::new();
    for row in rows {
        let anchor_iri = row.get::<String>("anchorIri")?;
        let degree = usize::try_from(row.get::<i64>("degree")?)
            .map_err(|_| operation_error!("structural endpoint degree is negative"))?;
        let (fact_iri, fact, group) = resolved_fact(row.get("s")?, row.get("r")?, row.get("o")?)?;
        candidates.push(StructuralCandidate {
            anchor_iri,
            fact_iri,
            property_iri: group.property_iri,
            degree,
            fact,
        });
    }
    Ok(candidates)
}

/// Serialize one current ordinary relationship exactly as semantic recall returns it.
fn resolved_fact(s: Node, r: Relation, o: Node) -> Result<(String, Value, FactGroup)> {
    let iri = r.get::<String>("iri")?;
    let s_json = endpoint_json(&s)?;
    let o_json = endpoint_json(&o)?;
    let s_iri = s.get::<String>("iri")?;
    let o_iri = o.get::<String>("iri")?;
    let relation = rel_json(&r, &s_iri, &o_iri)?;
    let property_iri = r.get::<String>("propertyIri")?;
    let effective_weight = s_json["weight"]
        .as_i64()
        .ok_or_else(|| operation_error!("serialized subject weight is not an integer"))?
        .saturating_add(
            relation["weight"]
                .as_i64()
                .ok_or_else(|| operation_error!("serialized fact weight is not an integer"))?,
        )
        .saturating_add(
            o_json["weight"]
                .as_i64()
                .ok_or_else(|| operation_error!("serialized object weight is not an integer"))?,
        );
    let scope = r.get::<Vec<String>>("layers")?;
    let mut fact = fact_envelope(s_json, &property_iri, o_json, &relation, &scope)?;
    fact["score"] = json!(0.0);
    fact["effectiveWeight"] = json!(effective_weight);
    Ok((
        iri,
        fact,
        FactGroup {
            subject_iri: s_iri,
            property_iri,
        },
    ))
}

/// Refresh recalled TTLs, then either converge into a neighbor or mint a new activation.
async fn persist_activation(
    graph: &Graph,
    runtime: &SemanticRuntime,
    embedding: &[f64],
    result_refs: &[String],
    neighbors: &[Activation],
    contributing_activation_ids: &[String],
) -> Result<()> {
    require_embedding_space(
        graph,
        &EmbeddingSpace {
            provider: runtime.provider().into(),
            model: runtime.model().into(),
            dimensions: runtime.dimensions(),
        },
    )
    .await?;
    let ttl_ms = runtime
        .config
        .ttl_days
        .checked_mul(86_400_000)
        .and_then(|milliseconds| i64::try_from(milliseconds).ok())
        .ok_or_else(|| operation_error!("semantic TTL is too large"))?;
    let convergence = if result_refs.is_empty() {
        None
    } else {
        select_convergence(neighbors, result_refs, &runtime.config)
    };
    let mut refreshed_activation_ids = contributing_activation_ids.to_vec();
    if let Some(existing) = convergence {
        refreshed_activation_ids.push(existing.element_id.clone());
    }
    refreshed_activation_ids.sort();
    refreshed_activation_ids.dedup();
    refresh_recalled_activations(graph, &refreshed_activation_ids, ttl_ms).await?;
    if result_refs.is_empty() {
        return Ok(());
    }
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
                MATCH (a:SemanticActivationV4)
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

/// Choose the nearest neighbor that is similar enough and overlaps enough to merge.
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

/// Extend APOC TTL on still-live activations that contributed to this recall.
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
            MATCH (a:SemanticActivationV4:TTL)
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

/// Load a still-live activation vector so convergence can average embeddings.
async fn load_activation_embedding(graph: &Graph, element_id: &str) -> Result<Option<Vec<f64>>> {
    fetch_one(
        graph,
        query(
            "MATCH (a:SemanticActivationV4) \
             WHERE elementId(a) = $elementId AND a.ttl >= timestamp() \
             RETURN a.embedding AS embedding",
        )
        .param("elementId", element_id.to_string()),
    )
    .await?
    .map(|row| row.get::<Vec<f64>>("embedding").map_err(Into::into))
    .transpose()
}

/// Insert a new activation node, write its embedding, and start its TTL lease.
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
            CREATE (a:SemanticActivation:SemanticActivationV4:TTL {resultRefs: $resultRefs})
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

/// Result-set overlap; two empty sets count as complete overlap for convergence.
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
            detail: None,
            limit: Some(100),
            effective_at: None,
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
        add_ranked_max(
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
    fn activation_fusion_uses_the_strongest_bundle_per_fact() {
        let facts = HashMap::from([("fact".into(), json!({}))]);
        let mut fused = HashMap::new();
        add_ranked_max(&mut fused, &["fact".into()], 0.9, 15.0, &facts);
        add_ranked_max(&mut fused, &["fact".into()], 0.8, 15.0, &facts);
        assert_eq!(fused["fact"], 0.9 / 16.0);
    }

    #[test]
    fn activation_evidence_is_zero_at_admission_and_bounded_by_keywords() {
        let threshold = 0.65;
        let keyword_weight = 0.5;
        assert_eq!(
            activation_evidence(threshold, threshold, keyword_weight),
            0.0
        );
        assert_eq!(activation_evidence(1.0, threshold, keyword_weight), 0.5);
        assert!(activation_evidence(0.9, threshold, keyword_weight) < keyword_weight);
    }

    #[test]
    fn fused_score_ties_prefer_direct_evidence() {
        let direct_matches = HashMap::from([(
            "direct".into(),
            TextMatch {
                keyword_confidence: 1.0,
                ..TextMatch::default()
            },
        )]);
        let mut ranked = [("activation".into(), 0.5), ("direct".into(), 0.5)];
        ranked.sort_by(|left, right| compare_fused(left, right, &direct_matches));
        assert_eq!(ranked[0].0, "direct");
    }

    #[test]
    fn direct_fusion_weights_exact_evidence_above_keywords() {
        let facts = HashMap::from([
            ("endpoint-only".into(), json!({})),
            ("exact".into(), json!({})),
            ("keyword".into(), json!({})),
        ]);
        let matches = HashMap::from([
            (
                "exact".into(),
                TextMatch {
                    exact: true,
                    ..TextMatch::default()
                },
            ),
            (
                "keyword".into(),
                TextMatch {
                    keyword_confidence: 0.5,
                    ..TextMatch::default()
                },
            ),
        ]);
        let mut fused = HashMap::new();
        add_direct_ranked(
            &mut fused,
            &["endpoint-only".into(), "exact".into(), "keyword".into()],
            2.0,
            0.5,
            15.0,
            &facts,
            &matches,
        );
        assert!(!fused.contains_key("endpoint-only"));
        assert_eq!(fused["exact"], 2.0 / 16.0);
        assert!(fused["exact"] > fused["keyword"]);
        assert_eq!(fused["keyword"], (0.5 * 0.5) / 17.0);
    }

    #[test]
    fn structural_context_is_bounded_penalized_and_does_not_boost_evidence() {
        let anchors = HashMap::from([("anchor".into(), 0.1)]);
        let mut fused = HashMap::from([("existing".into(), 0.2)]);
        let mut facts = HashMap::new();
        let candidate = |fact: &str, property: &str, degree: usize| StructuralCandidate {
            anchor_iri: "anchor".into(),
            fact_iri: fact.into(),
            property_iri: property.into(),
            degree,
            fact: json!({"target": fact}),
        };
        add_structural_ranked(
            &mut fused,
            &mut facts,
            &anchors,
            vec![
                candidate("group-a", "shared-property", 4),
                candidate("group-b", "shared-property", 4),
                candidate("diverse", "other-property", 1),
                candidate("existing", "other-property", 1),
                candidate("group-a", "shared-property", 9),
            ],
            0.5,
        );

        assert_eq!(fused["existing"], 0.2);
        assert_eq!(fused["diverse"], 0.05 / 2.0_f64.sqrt());
        assert_eq!(fused["group-a"], 0.05 / 2.0 / 2.0_f64.sqrt());
        assert_eq!(fused["group-a"], fused["group-b"]);
        assert!(facts
            .keys()
            .all(|iri| { ["group-a", "group-b", "diverse", "existing"].contains(&iri.as_str()) }));
    }

    #[test]
    fn penalized_structural_context_stays_below_strong_partial_direct_evidence() {
        let mut facts = HashMap::from([
            ("anchor".into(), json!({})),
            ("partial-direct".into(), json!({})),
        ]);
        let matches = HashMap::from([
            (
                "anchor".into(),
                TextMatch {
                    exact: true,
                    bundle_eligible: true,
                    ..TextMatch::default()
                },
            ),
            (
                "partial-direct".into(),
                TextMatch {
                    keyword_confidence: 2.0 / 3.0,
                    ..TextMatch::default()
                },
            ),
        ]);
        let mut fused = HashMap::new();
        add_direct_ranked(
            &mut fused,
            &["anchor".into(), "partial-direct".into()],
            2.0,
            0.5,
            15.0,
            &facts,
            &matches,
        );
        let anchors = HashMap::from([("anchor".into(), fused["anchor"])]);
        add_structural_ranked(
            &mut fused,
            &mut facts,
            &anchors,
            vec![StructuralCandidate {
                anchor_iri: "anchor".into(),
                fact_iri: "structural".into(),
                property_iri: "other-property".into(),
                degree: 4,
                fact: json!({}),
            }],
            STRUCTURAL_EVIDENCE_SCALE,
        );

        assert!(fused["partial-direct"] > fused["structural"]);
        assert!(fused["anchor"] > fused["structural"]);
    }

    #[test]
    fn structural_anchor_selection_is_strongest_then_stable_and_bounded() {
        let mut scores = (0..=MAX_STRUCTURAL_ANCHORS)
            .map(|index| (format!("anchor-{index:02}"), index as f64))
            .collect::<HashMap<_, _>>();
        scores.insert("anchor-tie-b".into(), 100.0);
        scores.insert("anchor-tie-a".into(), 100.0);
        let ranked = top_structural_anchors(&scores);

        assert_eq!(ranked.len(), MAX_STRUCTURAL_ANCHORS);
        assert_eq!(ranked[0].0, "anchor-tie-a");
        assert_eq!(ranked[1].0, "anchor-tie-b");
        assert!(!ranked.iter().any(|(iri, _)| iri == "anchor-00"));
    }

    #[test]
    fn activation_selection_preserves_rank_while_limiting_group_monopoly() {
        let ranked = ["a1", "a2", "a3", "b1"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let group_a = FactGroup {
            subject_iri: "subject:a".into(),
            property_iri: "property:a".into(),
        };
        let group_b = FactGroup {
            subject_iri: "subject:b".into(),
            property_iri: "property:b".into(),
        };
        let groups = HashMap::from([
            ("a1".into(), group_a.clone()),
            ("a2".into(), group_a.clone()),
            ("a3".into(), group_a),
            ("b1".into(), group_b),
        ]);
        assert_eq!(
            select_activation_refs(&ranked, &groups),
            vec!["a1", "a2", "b1"]
        );
    }

    #[test]
    fn activation_selection_fills_one_group_and_deduplicates_refs() {
        let ranked = ["a1", "a1", "a2", "a3"]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>();
        let group = FactGroup {
            subject_iri: "subject:a".into(),
            property_iri: "property:a".into(),
        };
        let groups = HashMap::from([
            ("a1".into(), group.clone()),
            ("a2".into(), group.clone()),
            ("a3".into(), group),
        ]);
        assert_eq!(
            select_activation_refs(&ranked, &groups),
            vec!["a1", "a2", "a3"]
        );
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
