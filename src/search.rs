//! Ranked fact retrieval and the graph-free `recall` input contract.
//!
//! Text and non-schema label queries rank current ordinary `ASSERTS` facts:
//! Spike category first, then subject+fact+object weight, then text score.
//! `validate_recall_args` enforces exactly one selector (`text`, `iris`,
//! `labels`, `around`, or `history`) without advertising schema unions. Catalog labels
//! (`Class`/`Property`) do not enter this rank path. Retrieval never changes
//! weights.

use crate::domain::DomainError;
use crate::error::{Error, Result};
use crate::graph::{endpoint_json, fact_envelope, fetch_all, node_json, rel_json, spike_rank};
use crate::iri::{is_iri, property_iri, split_camel_case};
use crate::layers::validate_layer_ids;
use neo4rs::{query, Graph, Node, Relation};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// Maximum unique terms admitted to the keyword fallback query.
const MAX_KEYWORD_TOKENS: usize = 16;
/// Short tokens are usually grammatical glue across supported natural languages.
const MIN_KEYWORD_TOKEN_CHARS: usize = 4;
/// Oversized terms are skipped rather than truncated into a different token.
const MAX_KEYWORD_TOKEN_BYTES: usize = 64;
/// Very long text skips the effectively-unmatchable exact phrase channel.
const MAX_EXACT_PHRASE_BYTES: usize = 512;
const NO_MATCH_QUERY: &str = "\"mindreadernokeywordmatch9f86d081884c\"";
const EXACT_TEXT_WEIGHT: f64 = 2.0;
const KEYWORD_TEXT_WEIGHT: f64 = 1.0;

/// In-process ranked search (`layers` here is the request visibility union).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchArgs {
    /// Request visibility union (named `layers` here; MCP wire field is `scope`).
    pub layers: Vec<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Internal direct-search evidence used by semantic rank fusion.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct TextMatch {
    pub exact: bool,
    pub keyword_confidence: f64,
    pub bundle_eligible: bool,
}

/// Private graph identity used to diversify learned semantic bundles.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct FactGroup {
    pub subject_iri: String,
    pub property_iri: String,
}

/// Internal ranked search result plus channel evidence that must not leak onto the MCP wire.
pub(crate) struct SearchResult {
    pub facts: Vec<Value>,
    pub about: Vec<Value>,
    pub scope: Vec<String>,
    pub mode: &'static str,
    pub truncated: bool,
    pub text_matches: HashMap<String, TextMatch>,
    pub fact_groups: HashMap<String, FactGroup>,
}

impl SearchResult {
    fn into_payload(self) -> Value {
        json!({
            "ok": true,
            "mode": self.mode,
            "facts": self.facts,
            "nodes": [],
            "paths": [],
            "about": self.about,
            "lookups": [],
            "scope": self.scope,
            "truncated": self.truncated,
        })
    }
}

/// MCP `recall` arguments. Runtime accepts exactly one selector.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecallArgs {
    /// Request visibility union; empty is global-only.
    pub scope: Vec<String>,
    /// Lexical selector; mutually exclusive with the other four selectors.
    #[serde(default)]
    pub text: Option<String>,
    /// 1–20 node IRIs; hops applies only here.
    #[serde(default)]
    pub iris: Option<Vec<String>>,
    /// Catalog (`Class`/`Property`) or ranked non-schema label filter.
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    /// Starting node IRI for bounded graph walk; `p` and `depth` apply only here.
    #[serde(default)]
    pub around: Option<String>,
    /// IRI mode only: `0` lookup facts, `1` also fill top-level `facts[]`.
    #[serde(default)]
    pub hops: Option<u32>,
    /// Around mode only: predicate names or IRIs applied before the fact limit.
    #[serde(default)]
    pub p: Option<Vec<String>>,
    /// Around mode only: traversal depth 1..=3.
    #[serde(default)]
    pub depth: Option<u32>,
    /// Around mode only: `both`, `outgoing`, or `incoming` at every hop.
    #[serde(default)]
    pub direction: Option<String>,
    /// Node or fact IRI whose current and `validTo` facts are returned.
    #[serde(default)]
    pub history: Option<String>,
    /// `concise` or `detailed`; omitted defaults to detailed.
    #[serde(default)]
    pub detail: Option<String>,
    /// Maximum facts; default 20, at most 100.
    #[serde(default)]
    pub limit: Option<u32>,
}

/// Graph-free recall input contract (runtime XOR, no schema union).
pub fn validate_recall_args(args: &RecallArgs) -> Result<()> {
    validate_layer_ids(args.scope.clone())?;
    let text = args
        .text
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let iris = args.iris.as_ref().map(Vec::len).unwrap_or(0);
    let labels = args.labels.as_ref().map(Vec::len).unwrap_or(0);
    let around = args
        .around
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let history = args
        .history
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let selectors = [
        text.is_some(),
        iris > 0,
        labels > 0,
        around.is_some(),
        history.is_some(),
    ]
    .into_iter()
    .filter(|selected| *selected)
    .count();
    if selectors != 1 {
        return Err(DomainError::InvalidInput(
            "recall requires exactly one of text, iris, labels, around, or history".into(),
        )
        .into());
    }
    let selector = if text.is_some() {
        "text"
    } else if iris > 0 {
        "iris"
    } else if labels > 0 {
        "labels"
    } else if around.is_some() {
        "around"
    } else {
        "history"
    };
    crate::payload::Detail::parse(args.detail.as_deref())?;
    if selector != "iris" && args.hops.is_some() {
        return Err(DomainError::InvalidInput(format!(
            "recall hops applies only to the iris selector, not {selector}"
        ))
        .into());
    }
    if selector != "around" && args.p.is_some() {
        return Err(DomainError::InvalidInput(format!(
            "recall p applies only to the around selector, not {selector}"
        ))
        .into());
    }
    if selector != "around" && args.depth.is_some() {
        return Err(DomainError::InvalidInput(format!(
            "recall depth applies only to the around selector, not {selector}"
        ))
        .into());
    }
    if selector != "around" && args.direction.is_some() {
        return Err(DomainError::InvalidInput(format!(
            "recall direction applies only to the around selector, not {selector}"
        ))
        .into());
    }
    if selector != "history" && args.history.is_some() {
        return Err(DomainError::InvalidInput(format!(
            "recall history applies only to the history selector, not {selector}"
        ))
        .into());
    }
    if let Some(values) = &args.iris {
        if !(1..=20).contains(&values.len()) {
            return Err(DomainError::InvalidInput(
                "recall iris must contain 1..=20 node IRIs".into(),
            )
            .into());
        }
        let mut seen = HashSet::new();
        for value in values {
            let iri = value.trim();
            if !is_iri(iri) || iri.starts_with("mindreader:relationship/") {
                return Err(DomainError::InvalidInput(format!(
                    "recall iris accepts node IRIs, not {value:?}"
                ))
                .into());
            }
            if !seen.insert(iri) {
                return Err(DomainError::InvalidInput(format!(
                    "recall iris contains duplicate IRI {iri:?}"
                ))
                .into());
            }
        }
    }
    if let Some(values) = &args.labels {
        if values.is_empty() || values.iter().any(|value| value.trim().is_empty()) {
            return Err(DomainError::InvalidInput(
                "recall labels must contain non-empty labels".into(),
            )
            .into());
        }
    }
    if let Some(values) = &args.p {
        if values.is_empty() || values.iter().any(|value| value.trim().is_empty()) {
            return Err(DomainError::InvalidInput(
                "recall p must contain non-empty predicates".into(),
            )
            .into());
        }
    }
    if let Some(iri) = around {
        if !is_iri(iri) || iri.starts_with("mindreader:relationship/") {
            return Err(DomainError::InvalidInput(format!(
                "recall around requires a node IRI, not {iri:?}"
            ))
            .into());
        }
    }
    if let Some(iri) = history {
        if !is_iri(iri) {
            return Err(DomainError::InvalidInput(format!(
                "recall history requires a node or fact IRI, not {iri:?}"
            ))
            .into());
        }
    }
    if let Some(hops) = args.hops {
        if hops != 0 && hops != 1 {
            return Err(DomainError::InvalidInput("recall hops must be 0 or 1".into()).into());
        }
    }
    if let Some(depth) = args.depth {
        if !(1..=3).contains(&depth) {
            return Err(DomainError::InvalidInput("recall depth must be 1..=3".into()).into());
        }
    }
    if let Some(direction) = args.direction.as_deref() {
        if !matches!(direction, "both" | "outgoing" | "incoming") {
            return Err(DomainError::InvalidInput(
                "recall direction must be both, outgoing, or incoming".into(),
            )
            .into());
        }
    }
    if let Some(limit) = args.limit {
        if limit == 0 || limit > 100 {
            return Err(Error::from(DomainError::InvalidInput(
                "recall limit must be 1..=100".into(),
            )));
        }
    }
    Ok(())
}

/// Required fact IRI from the canonical pasteable target envelope.
pub fn fact_handle_iri(fact: &Value) -> Result<&str> {
    fact.pointer("/target/iri")
        .and_then(Value::as_str)
        .filter(|iri| !iri.is_empty())
        .ok_or_else(|| crate::graph_error!("fact result is missing target.iri"))
}

/// True when every label is `Class` or `Property` (catalog path, not ranked search).
pub fn is_schema_catalog_labels(labels: &[String]) -> bool {
    !labels.is_empty()
        && labels.iter().all(|label| {
            let trimmed = label.trim();
            trimmed == "Class" || trimmed == "Property"
        })
}

/// Validate, sort, and stringify the request `scope` used as Cypher `$layers`.
fn normalize_layers(raw: Vec<String>) -> Result<Vec<String>> {
    Ok(validate_layer_ids(raw)?
        .into_iter()
        .map(|layer| layer.into_string())
        .collect())
}

/// Quote a Lucene phrase and escape operators so user text cannot change query shape.
fn lucene_query(text: &str) -> String {
    let escaped = lucene_escape(text);
    let split = split_camel_case(text);
    if split != text {
        format!("({escaped} OR {})", lucene_escape(&split))
    } else {
        escaped
    }
}

/// Extract unique Unicode-alphanumeric terms with the search channel's length bounds.
fn keyword_tokens(text: &str, limit: usize) -> Vec<String> {
    let split = split_camel_case(text);
    let mut tokens = Vec::new();
    let mut seen = HashSet::new();
    let mut current = String::new();
    let push_token =
        |current: &mut String, tokens: &mut Vec<String>, seen: &mut HashSet<String>| {
            if current.is_empty() || tokens.len() >= limit {
                current.clear();
                return;
            }
            let token = current.to_lowercase();
            current.clear();
            if token.chars().count() >= MIN_KEYWORD_TOKEN_CHARS
                && token.len() <= MAX_KEYWORD_TOKEN_BYTES
                && seen.insert(token.clone())
            {
                tokens.push(token);
            }
        };
    for character in split.chars() {
        if character.is_alphanumeric() {
            current.push(character);
        } else {
            push_token(&mut current, &mut tokens, &mut seen);
        }
    }
    push_token(&mut current, &mut tokens, &mut seen);
    tokens
}

/// Build a bounded OR query from unique Unicode-alphanumeric terms.
fn lucene_keyword_query(text: &str) -> String {
    let tokens = keyword_tokens(text, MAX_KEYWORD_TOKENS);
    if tokens.is_empty() {
        return NO_MATCH_QUERY.into();
    }
    tokens
        .iter()
        .map(|token| lucene_escape(token))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Fraction of bounded query terms present anywhere in a relationship's indexed text.
fn keyword_coverage(query_tokens: &[String], fact_text: &str) -> f64 {
    if query_tokens.is_empty() {
        return 0.0;
    }
    let fact_tokens = keyword_tokens(fact_text, usize::MAX)
        .into_iter()
        .collect::<HashSet<_>>();
    let matched = query_tokens
        .iter()
        .filter(|token| fact_tokens.contains(*token))
        .count();
    matched as f64 / query_tokens.len() as f64
}

/// Exact phrase query, bounded so long natural-language input uses keywords only.
fn lucene_exact_query(text: &str) -> String {
    if text.len() > MAX_EXACT_PHRASE_BYTES {
        NO_MATCH_QUERY.into()
    } else {
        lucene_query(text)
    }
}

/// Phrase-quote user text and escape Lucene operators so they stay literals.
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

/// Full-text node, fact, and exact-property candidates for ranked text recall.
const TEXT_CANDIDATES: &str = r#"
CALL {
  CALL db.index.fulltext.queryNodes('wakeup_nodes', $qExact) YIELD node, score
  MATCH (node)-[relationship]-(other:Entity)
  RETURN startNode(relationship) AS s, relationship AS r,
         endNode(relationship) AS o,
         0.0 AS factScore, 0 AS factExact,
         score * $exactTextWeight AS endpointScore
  UNION ALL
  CALL db.index.fulltext.queryRelationships('wakeup_facts', $qExact)
  YIELD relationship, score
  RETURN startNode(relationship) AS s, relationship AS r,
         endNode(relationship) AS o,
         score * $exactTextWeight AS factScore, 1 AS factExact,
         0.0 AS endpointScore
  UNION ALL
  CALL db.index.fulltext.queryNodes('wakeup_nodes', $qKeywords) YIELD node, score
  MATCH (node)-[relationship]-(other:Entity)
  RETURN startNode(relationship) AS s, relationship AS r,
         endNode(relationship) AS o,
         0.0 AS factScore, 0 AS factExact,
         score * $keywordTextWeight AS endpointScore
  UNION ALL
  CALL db.index.fulltext.queryRelationships('wakeup_facts', $qKeywords)
  YIELD relationship, score
  RETURN startNode(relationship) AS s, relationship AS r,
         endNode(relationship) AS o,
         score * $keywordTextWeight AS factScore, 0 AS factExact,
         0.0 AS endpointScore
  UNION ALL
  MATCH (p:Entity:Property)
  WHERE toLower(coalesce(p.name, '')) = toLower($predicateName)
     OR p.iri = $predicateIri
     OR p.iri ENDS WITH ('/' + $predicateName)
  MATCH (s:Entity)-[r:ASSERTS]->(o:Entity)
  WHERE r.validTo IS NULL AND r.propertyIri = p.iri
  RETURN s, r, o,
         1.0 AS factScore, 1 AS factExact,
         0.0 AS endpointScore
}
WITH s, r, o,
     max(factScore) AS factScore, max(factExact) AS factExact,
     max(endpointScore) AS endpointScore
WITH s, r, o,
     CASE WHEN factScore > 0.0 THEN factScore ELSE endpointScore END AS indexScore,
     factExact AS relationshipExact,
     CASE WHEN factScore > 0.0 THEN 1 ELSE 0 END AS relationshipMatched,
     coalesce(r.factText, '') AS factText
"#;

/// Unfiltered current-edge scan used when ranking by labels only.
const LABEL_CANDIDATES: &str = r#"
MATCH (s:Entity)-[r]->(o:Entity)
WITH s, r, o, 1.0 AS indexScore,
     0 AS relationshipExact, 0 AS relationshipMatched, '' AS factText
"#;

/// Shared rank tail: current ordinary assertions, endpoint closure, Spike then weight then score.
const RANK_AND_LIMIT: &str = r#"
WHERE r.validTo IS NULL
  AND type(r) = 'ASSERTS'
  AND (size(s.layers) = 0
       OR any(layer IN s.layers WHERE layer IN $layers))
  AND (size(r.layers) = 0
       OR any(layer IN r.layers WHERE layer IN $layers))
  AND (size(o.layers) = 0
       OR any(layer IN o.layers WHERE layer IN $layers))
  AND ($labelCount = 0
       OR any(label IN $labels WHERE label IN labels(s) OR label IN labels(o)))
WITH s, r, o, indexScore, relationshipExact, relationshipMatched, factText,
     CASE
       WHEN r.spike = 'Knowledge' THEN 4
       WHEN r.spike = 'Insight' THEN 3
       WHEN r.spike = 'Pattern' THEN 2
       WHEN r.spike = 'Signal' THEN 1
       ELSE 0
     END AS ownSpikeRank
OPTIONAL MATCH (sp:Entity)-[a:ABOUT]->(s)
WHERE a.validTo IS NULL
  AND a.spike IS NOT NULL
  AND (size(sp.layers) = 0
       OR any(layer IN sp.layers WHERE layer IN $layers))
  AND (size(a.layers) = 0
       OR any(layer IN a.layers WHERE layer IN $layers))
  AND (size(s.layers) = 0
       OR any(layer IN s.layers WHERE layer IN $layers))
WITH s, r, o, indexScore, relationshipExact, relationshipMatched, factText,
     ownSpikeRank,
     max(CASE
       WHEN a.spike = 'Knowledge' THEN 4
       WHEN a.spike = 'Insight' THEN 3
       WHEN a.spike = 'Pattern' THEN 2
       WHEN a.spike = 'Signal' THEN 1
       ELSE 0
     END) AS attachedSpikeRank,
     s.weight AS subjectWeight,
     r.weight AS relationshipWeight,
     o.weight AS objectWeight
WITH s, r, o, indexScore, relationshipExact, relationshipMatched, factText,
     CASE WHEN ownSpikeRank > 0 THEN ownSpikeRank ELSE attachedSpikeRank END AS spikeRank,
     CASE
       WHEN subjectWeight > 0 AND relationshipWeight > $maxWeight - subjectWeight THEN $maxWeight
       WHEN subjectWeight < 0 AND relationshipWeight < $minWeight - subjectWeight THEN $minWeight
       ELSE subjectWeight + relationshipWeight
     END AS subjectRelationshipWeight,
     objectWeight
WITH s, r, o, relationshipExact, relationshipMatched, factText, spikeRank,
     CASE
       WHEN subjectRelationshipWeight > 0 AND objectWeight > $maxWeight - subjectRelationshipWeight THEN $maxWeight
       WHEN subjectRelationshipWeight < 0 AND objectWeight < $minWeight - subjectRelationshipWeight THEN $minWeight
       ELSE subjectRelationshipWeight + objectWeight
     END AS effectiveWeight,
     indexScore AS score,
     r.propertyIri AS property
ORDER BY spikeRank DESC, effectiveWeight DESC, score DESC,
         s.iri ASC, property ASC, o.iri ASC, r.iri ASC
LIMIT $limit
RETURN s, r, o, property, spikeRank, effectiveWeight, score,
       relationshipExact, relationshipMatched, factText
"#;

/// Assemble the closed-world rank query: text index or label filter, then Spike/weight/score.
fn ranked_query(text_mode: bool) -> String {
    let candidates = if text_mode {
        TEXT_CANDIDATES
    } else {
        LABEL_CANDIDATES
    };
    format!("{candidates}{RANK_AND_LIMIT}")
}

/// Explicit `ABOUT` facts that give Spike context around ranked subjects.
async fn spike_context(
    graph: &Graph,
    layers: &[String],
    about_iris: Vec<String>,
    limit: i64,
) -> Result<Vec<Value>> {
    if about_iris.is_empty() {
        return Ok(Vec::new());
    }
    let mut spike_list = Vec::new();
    let mut seen_spike = HashSet::new();
    for row in fetch_all(
        graph,
        query(
            r#"
            MATCH (sp:Entity)-[a:ABOUT]->(el:Entity)
            WHERE a.validTo IS NULL AND el.iri IN $iris
              AND a.spike IS NOT NULL
              AND (size(sp.layers) = 0
                   OR any(layer IN sp.layers WHERE layer IN $layers))
              AND (size(a.layers) = 0
                   OR any(layer IN a.layers WHERE layer IN $layers))
              AND (size(el.layers) = 0
                   OR any(layer IN el.layers WHERE layer IN $layers))
            WITH sp, a, el,
                 CASE
                   WHEN a.spike = 'Knowledge' THEN 4
                   WHEN a.spike = 'Insight' THEN 3
                   WHEN a.spike = 'Pattern' THEN 2
                   WHEN a.spike = 'Signal' THEN 1
                   ELSE 0
                 END AS spikeRank,
                 sp.weight AS spikeWeight,
                 a.weight AS relationshipWeight,
                 el.weight AS elementWeight
            WITH sp, a, el, spikeRank,
                 CASE
                   WHEN spikeWeight > 0 AND relationshipWeight > $maxWeight - spikeWeight THEN $maxWeight
                   WHEN spikeWeight < 0 AND relationshipWeight < $minWeight - spikeWeight THEN $minWeight
                   ELSE spikeWeight + relationshipWeight
                 END AS spikeRelationshipWeight,
                 elementWeight
            WITH sp, a, el, spikeRank,
                 CASE
                   WHEN spikeRelationshipWeight > 0 AND elementWeight > $maxWeight - spikeRelationshipWeight THEN $maxWeight
                   WHEN spikeRelationshipWeight < 0 AND elementWeight < $minWeight - spikeRelationshipWeight THEN $minWeight
                   ELSE spikeRelationshipWeight + elementWeight
                 END AS effectiveWeight
            WITH sp, a, el, spikeRank, effectiveWeight
            ORDER BY spikeRank DESC, effectiveWeight DESC, el.iri ASC, sp.iri ASC
            LIMIT $limit
            RETURN sp, a, el, effectiveWeight
            "#,
        )
        .param("layers", layers.to_vec())
        .param("iris", about_iris)
        .param("minWeight", i64::MIN)
        .param("maxWeight", i64::MAX)
        .param("limit", limit),
    )
    .await?
    {
        let sp = row.get::<Node>("sp")?;
        let about_rel = row.get::<Relation>("a")?;
        let element = row.get::<Node>("el")?;
        let Some(rank) = about_rel.get::<String>("spike").ok() else {
            continue;
        };
        let about = element.get::<String>("iri")?;
        let sp_iri = sp.get::<String>("iri")?;
        let relationship = rel_json(&about_rel, &sp_iri, &about)?;
        let combined = row.get::<i64>("effectiveWeight")?;
        let rel_iri = about_rel.get::<String>("iri")?;
        if seen_spike.insert(rel_iri) {
            spike_list.push(json!({
                "node": node_json(&sp)?,
                "about": about,
                "rank": rank,
                "relationship": relationship,
                "effectiveWeight": combined,
            }));
        }
    }
    spike_list.sort_by(|left, right| {
        spike_rank(right.get("rank").and_then(Value::as_str))
            .cmp(&spike_rank(left.get("rank").and_then(Value::as_str)))
            .then_with(|| {
                right
                    .get("effectiveWeight")
                    .and_then(Value::as_i64)
                    .cmp(&left.get("effectiveWeight").and_then(Value::as_i64))
            })
            .then_with(|| {
                left.get("about")
                    .and_then(Value::as_str)
                    .cmp(&right.get("about").and_then(Value::as_str))
            })
            .then_with(|| {
                left.pointer("/node/iri")
                    .and_then(Value::as_str)
                    .cmp(&right.pointer("/node/iri").and_then(Value::as_str))
            })
    });
    Ok(spike_list)
}

/// Rank current visible ordinary facts for text and/or non-schema labels.
pub async fn memory_search(graph: &Graph, args: SearchArgs) -> Result<Value> {
    Ok(memory_search_with_matches(graph, args)
        .await?
        .into_payload())
}

/// Execute ranked search while retaining private exact-vs-keyword evidence for semantic fusion.
pub(crate) async fn memory_search_with_matches(
    graph: &Graph,
    args: SearchArgs,
) -> Result<SearchResult> {
    let layers = normalize_layers(args.layers)?;
    let limit = args.limit.unwrap_or(20).clamp(1, 100) as i64;
    let labels = args.labels.unwrap_or_default();
    let trimmed = args.text.unwrap_or_default().trim().to_string();
    if trimmed.is_empty() && labels.is_empty() {
        return Ok(SearchResult {
            facts: Vec::new(),
            about: Vec::new(),
            scope: layers,
            mode: "labels",
            truncated: false,
            text_matches: HashMap::new(),
            fact_groups: HashMap::new(),
        });
    }

    let text_mode = !trimmed.is_empty();
    let query_tokens = keyword_tokens(&trimmed, MAX_KEYWORD_TOKENS);
    let query_limit = limit.saturating_add(1);
    let mut ranked = query(&ranked_query(text_mode))
        .param("layers", layers.clone())
        .param("labels", labels.clone())
        .param("labelCount", labels.len() as i64)
        .param("limit", query_limit)
        .param("minWeight", i64::MIN)
        .param("maxWeight", i64::MAX);
    if text_mode {
        ranked = ranked
            .param("qExact", lucene_exact_query(&trimmed))
            .param("qKeywords", lucene_keyword_query(&trimmed))
            .param("exactTextWeight", EXACT_TEXT_WEIGHT)
            .param("keywordTextWeight", KEYWORD_TEXT_WEIGHT)
            .param("predicateName", trimmed.clone())
            .param("predicateIri", property_iri(&trimmed));
    }

    let mut facts = Vec::new();
    let mut text_matches = HashMap::new();
    let mut fact_groups = HashMap::new();
    for row in fetch_all(graph, ranked).await? {
        let subject = row.get::<Node>("s")?;
        let relationship = row.get::<Relation>("r")?;
        let object = row.get::<Node>("o")?;
        let subject_iri = subject.get::<String>("iri")?;
        let object_iri = object.get::<String>("iri")?;
        let fact_iri = relationship.get::<String>("iri")?;
        let effective_weight = row.get::<i64>("effectiveWeight")?;
        let property = row.get::<String>("property")?;
        let scope_vec = relationship.get::<Vec<String>>("layers")?;
        let relationship = rel_json(&relationship, &subject_iri, &object_iri)?;
        let mut fact = fact_envelope(
            endpoint_json(&subject)?,
            &property,
            endpoint_json(&object)?,
            &relationship,
            &scope_vec,
        )?;
        fact["score"] = json!(row.get::<f64>("score")?);
        fact["effectiveWeight"] = json!(effective_weight);
        if row.get::<i64>("relationshipMatched")? != 0 {
            let exact = row.get::<i64>("relationshipExact")? != 0;
            let keyword_confidence = if exact {
                1.0
            } else {
                keyword_coverage(&query_tokens, &row.get::<String>("factText")?)
            };
            text_matches.insert(
                fact_iri.clone(),
                TextMatch {
                    exact,
                    keyword_confidence,
                    bundle_eligible: exact || keyword_confidence == 1.0,
                },
            );
        }
        fact_groups.insert(
            fact_iri,
            FactGroup {
                subject_iri,
                property_iri: property,
            },
        );
        facts.push(fact);
    }
    let truncated = facts.len() > limit as usize;
    facts.truncate(limit as usize);
    let mut element_iris = HashSet::new();
    for fact in &facts {
        if let Some(iri) = fact.pointer("/s/iri").and_then(Value::as_str) {
            element_iris.insert(iri.to_string());
        }
        let object_is_element = fact
            .pointer("/o/labels")
            .and_then(Value::as_array)
            .is_some_and(|labels| labels.iter().any(|label| label == "Element"));
        if object_is_element {
            if let Some(iri) = fact.pointer("/o/iri").and_then(Value::as_str) {
                element_iris.insert(iri.to_string());
            }
        }
    }
    let spike = spike_context(
        graph,
        &layers,
        element_iris.into_iter().collect::<Vec<_>>(),
        limit,
    )
    .await?;
    Ok(SearchResult {
        facts,
        about: spike,
        scope: layers,
        mode: if text_mode { "text" } else { "labels" },
        truncated,
        text_matches,
        fact_groups,
    })
}

#[cfg(test)]
mod tests {
    use super::{
        fact_handle_iri, keyword_coverage, keyword_tokens, lucene_escape, lucene_exact_query,
        lucene_keyword_query, lucene_query, ranked_query, validate_recall_args, RecallArgs,
        MAX_EXACT_PHRASE_BYTES, MAX_KEYWORD_TOKENS,
    };
    use serde_json::json;

    #[test]
    fn search_helpers_preserve_contract_values() {
        assert_eq!(lucene_escape("C++"), "\"C\\+\\+\"");
        assert_eq!(
            lucene_query("graphModel"),
            "(\"graphModel\" OR \"graph Model\")"
        );
        assert_eq!(
            lucene_keyword_query("Graph graphModel / C++ 2026"),
            "\"graph\" OR \"model\" OR \"2026\""
        );
        assert_eq!(
            lucene_keyword_query("the und per con"),
            super::NO_MATCH_QUERY
        );
        assert_eq!(lucene_keyword_query("!!!"), super::NO_MATCH_QUERY);
        assert_eq!(
            lucene_exact_query(&"x".repeat(MAX_EXACT_PHRASE_BYTES + 1)),
            super::NO_MATCH_QUERY
        );
        assert!(ranked_query(true).contains("r.propertyIri = p.iri"));
        assert!(ranked_query(true).contains("$qExact"));
        assert!(ranked_query(true).contains("$qKeywords"));
        assert!(ranked_query(true).contains(
            "CASE WHEN factScore > 0.0 THEN factScore ELSE endpointScore END AS indexScore"
        ));
        let query = ranked_query(true);
        assert!(query.contains("AND type(r) = 'ASSERTS'"));
        assert!(query.contains("WHEN r.spike = 'Knowledge'"));
        assert!(query.contains("WHEN a.spike = 'Knowledge'"));
        assert!(!query.contains("WHEN s:Knowledge"));
        assert!(query.contains("ORDER BY spikeRank DESC, effectiveWeight DESC, score DESC"));
        assert!(query.contains("LIMIT $limit"));
        assert!(!query.contains("collect("));
    }

    #[test]
    fn keyword_coverage_measures_unique_query_term_overlap() {
        let query = keyword_tokens("marlin circles beneath the boat", MAX_KEYWORD_TOKENS);
        assert_eq!(query, ["marlin", "circles", "beneath", "boat"]);
        assert_eq!(
            keyword_coverage(&query, "The marlin circles beneath Santiago's skiff"),
            0.75
        );
        assert_eq!(keyword_coverage(&query, "The fisherman returns"), 0.0);
    }

    #[test]
    fn fact_handle_accepts_only_the_canonical_target() {
        assert_eq!(
            fact_handle_iri(&json!({"target": {"kind": "fact", "iri": "fact:1"}})).unwrap(),
            "fact:1"
        );
        assert!(fact_handle_iri(&json!({"relationship": {"iri": "fact:legacy"}})).is_err());
    }

    #[test]
    fn recall_args_require_exactly_one_selector() {
        let mut args = RecallArgs {
            scope: vec!["project:x".into()],
            text: None,
            iris: None,
            labels: None,
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: None,
            limit: None,
        };
        assert!(validate_recall_args(&args).is_err());
        args.text = Some("Alice".into());
        assert!(validate_recall_args(&args).is_ok());
        args.around = Some("mindreader:element/alice".into());
        assert!(validate_recall_args(&args).is_err());
        args.around = None;
        args.hops = Some(2);
        assert!(validate_recall_args(&args).is_err());
        args.text = None;
        args.iris = Some(vec!["mindreader:element/alice".into()]);
        args.hops = Some(1);
        assert!(validate_recall_args(&args).is_ok());
        args.iris = Some(vec!["mindreader:relationship/fact".into()]);
        assert!(validate_recall_args(&args).is_err());
        args.iris = None;
        args.hops = None;
        args.history = Some("mindreader:relationship/fact".into());
        assert!(validate_recall_args(&args).is_ok());
        args.detail = Some("verbose".into());
        assert!(validate_recall_args(&args).is_err());
    }

    #[test]
    fn recall_args_reject_selector_inapplicable_fields_and_bad_bounds() {
        let base = RecallArgs {
            scope: vec![],
            text: Some("Alice".into()),
            iris: None,
            labels: None,
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: None,
            limit: Some(20),
        };
        assert!(validate_recall_args(&base).is_ok());
        assert!(validate_recall_args(&RecallArgs {
            hops: Some(1),
            ..base.clone()
        })
        .is_err());
        assert!(validate_recall_args(&RecallArgs {
            limit: Some(101),
            ..base.clone()
        })
        .is_err());
        assert!(validate_recall_args(&RecallArgs {
            text: None,
            around: Some("mindreader:element/alice".into()),
            direction: Some("outgoing".into()),
            ..base.clone()
        })
        .is_ok());
        assert!(validate_recall_args(&RecallArgs {
            text: None,
            around: Some("mindreader:element/alice".into()),
            direction: Some("sideways".into()),
            ..base.clone()
        })
        .is_err());
        assert!(validate_recall_args(&RecallArgs {
            text: None,
            iris: Some(vec![
                "mindreader:element/alice".into(),
                " mindreader:element/alice ".into(),
            ]),
            ..base
        })
        .is_err());
    }
}
