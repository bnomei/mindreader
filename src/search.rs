//! Ranked fact retrieval and the graph-free `memory_recall` input contract.
//!
//! Text and non-schema label queries rank current `ASSERTS`/`ABOUT` facts:
//! Spike category first, then subject+fact+object weight, then text score.
//! `validate_recall_args` enforces exactly one selector (`text`, `iris`,
//! `labels`, `around`, or `history`) without advertising schema unions. Catalog labels
//! (`Class`/`Property`) do not enter this rank path. Retrieval never changes
//! weights.

use crate::domain::DomainError;
use crate::error::{Error, Result};
use crate::graph::{
    endpoint_json, fact_envelope, fetch_all, node_json, rel_json, spike_label, spike_rank,
};
use crate::iri::{is_iri, property_iri, split_camel_case};
use crate::layers::validate_layer_ids;
use neo4rs::{query, Graph, Node, Relation};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

/// In-process ranked search (`layers` here is the request visibility union).
#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SearchArgs {
    pub layers: Vec<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<u32>,
}

/// MCP `memory_recall` arguments. Runtime accepts exactly one selector.
#[derive(Debug, Clone, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct RecallArgs {
    pub scope: Vec<String>,
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub iris: Option<Vec<String>>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub around: Option<String>,
    #[serde(default)]
    pub hops: Option<u32>,
    #[serde(default)]
    pub p: Option<Vec<String>>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub history: Option<String>,
    #[serde(default)]
    pub detail: Option<String>,
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
            "memory_recall requires exactly one of text, iris, labels, around, or history".into(),
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
            "memory_recall hops applies only to the iris selector, not {selector}"
        ))
        .into());
    }
    if selector != "around" && args.p.is_some() {
        return Err(DomainError::InvalidInput(format!(
            "memory_recall p applies only to the around selector, not {selector}"
        ))
        .into());
    }
    if selector != "around" && args.depth.is_some() {
        return Err(DomainError::InvalidInput(format!(
            "memory_recall depth applies only to the around selector, not {selector}"
        ))
        .into());
    }
    if selector != "history" && args.history.is_some() {
        return Err(DomainError::InvalidInput(format!(
            "memory_recall history applies only to the history selector, not {selector}"
        ))
        .into());
    }
    if let Some(values) = &args.iris {
        if !(1..=20).contains(&values.len()) {
            return Err(DomainError::InvalidInput(
                "memory_recall iris must contain 1..=20 node IRIs".into(),
            )
            .into());
        }
        let mut seen = HashSet::new();
        for value in values {
            let iri = value.trim();
            if !is_iri(iri) || iri.starts_with("mindreader:relationship/") {
                return Err(DomainError::InvalidInput(format!(
                    "memory_recall iris accepts node IRIs, not {value:?}"
                ))
                .into());
            }
            if !seen.insert(iri) {
                return Err(DomainError::InvalidInput(format!(
                    "memory_recall iris contains duplicate IRI {iri:?}"
                ))
                .into());
            }
        }
    }
    if let Some(values) = &args.labels {
        if values.is_empty() || values.iter().any(|value| value.trim().is_empty()) {
            return Err(DomainError::InvalidInput(
                "memory_recall labels must contain non-empty labels".into(),
            )
            .into());
        }
    }
    if let Some(values) = &args.p {
        if values.is_empty() || values.iter().any(|value| value.trim().is_empty()) {
            return Err(DomainError::InvalidInput(
                "memory_recall p must contain non-empty predicates".into(),
            )
            .into());
        }
    }
    if let Some(iri) = around {
        if !is_iri(iri) || iri.starts_with("mindreader:relationship/") {
            return Err(DomainError::InvalidInput(format!(
                "memory_recall around requires a node IRI, not {iri:?}"
            ))
            .into());
        }
    }
    if let Some(iri) = history {
        if !is_iri(iri) {
            return Err(DomainError::InvalidInput(format!(
                "memory_recall history requires a node or fact IRI, not {iri:?}"
            ))
            .into());
        }
    }
    if let Some(hops) = args.hops {
        if hops != 0 && hops != 1 {
            return Err(
                DomainError::InvalidInput("memory_recall hops must be 0 or 1".into()).into(),
            );
        }
    }
    if let Some(depth) = args.depth {
        if !(1..=3).contains(&depth) {
            return Err(
                DomainError::InvalidInput("memory_recall depth must be 1..=3".into()).into(),
            );
        }
    }
    if let Some(limit) = args.limit {
        if limit == 0 || limit > 100 {
            return Err(Error::from(DomainError::InvalidInput(
                "memory_recall limit must be 1..=100".into(),
            )));
        }
    }
    Ok(())
}

/// Fact IRI from a result envelope (`target.iri`, else legacy `relationship.iri`).
pub fn fact_handle_iri(fact: &Value) -> Option<&str> {
    fact.pointer("/target/iri")
        .or_else(|| fact.pointer("/relationship/iri"))
        .and_then(Value::as_str)
}

/// True when every label is `Class` or `Property` (catalog path, not ranked search).
pub fn is_schema_catalog_labels(labels: &[String]) -> bool {
    !labels.is_empty()
        && labels.iter().all(|label| {
            let trimmed = label.trim();
            trimmed == "Class" || trimmed == "Property"
        })
}

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

fn spike_from_rank(rank: i64) -> Option<String> {
    match rank {
        4 => Some("Knowledge".into()),
        3 => Some("Insight".into()),
        2 => Some("Pattern".into()),
        1 => Some("Signal".into()),
        _ => None,
    }
}

const TEXT_CANDIDATES: &str = r#"
CALL {
  CALL db.index.fulltext.queryNodes('wakeup_nodes', $q) YIELD node, score
  MATCH (node)-[relationship]-(other:Entity)
  RETURN startNode(relationship) AS s, relationship AS r,
         endNode(relationship) AS o, score AS indexScore
  UNION ALL
  CALL db.index.fulltext.queryRelationships('wakeup_facts', $q)
  YIELD relationship, score
  RETURN startNode(relationship) AS s, relationship AS r,
         endNode(relationship) AS o, score AS indexScore
  UNION ALL
  MATCH (p:Entity:Property)
  WHERE toLower(coalesce(p.name, '')) = toLower($predicateName)
     OR p.iri = $predicateIri
     OR p.iri ENDS WITH ('/' + $predicateName)
  MATCH (s:Entity)-[r:ASSERTS]->(o:Entity)
  WHERE r.validTo IS NULL AND r.propertyIri = p.iri
  RETURN s, r, o, 1.0 AS indexScore
}
WITH s, r, o, max(indexScore) AS indexScore
"#;

const LABEL_CANDIDATES: &str = r#"
MATCH (s:Entity)-[r]->(o:Entity)
WITH s, r, o, 1.0 AS indexScore
"#;

const RANK_AND_LIMIT: &str = r#"
WHERE r.validTo IS NULL
  AND (type(r) = 'ASSERTS' OR type(r) = 'ABOUT')
  AND (size(coalesce(s.layers, [])) = 0
       OR any(layer IN coalesce(s.layers, []) WHERE layer IN $layers))
  AND (size(coalesce(r.layers, [])) = 0
       OR any(layer IN coalesce(r.layers, []) WHERE layer IN $layers))
  AND (size(coalesce(o.layers, [])) = 0
       OR any(layer IN coalesce(o.layers, []) WHERE layer IN $layers))
  AND ($labelCount = 0
       OR any(label IN $labels WHERE label IN labels(s) OR label IN labels(o)))
WITH s, r, o, indexScore,
     CASE
       WHEN s:Knowledge THEN 4
       WHEN s:Insight THEN 3
       WHEN s:Pattern THEN 2
       WHEN s:Signal THEN 1
       ELSE 0
     END AS ownSpikeRank
OPTIONAL MATCH (sp:Entity)-[a:ABOUT]->(s)
WHERE a.validTo IS NULL
  AND (sp:Knowledge OR sp:Insight OR sp:Pattern OR sp:Signal)
  AND (size(coalesce(sp.layers, [])) = 0
       OR any(layer IN coalesce(sp.layers, []) WHERE layer IN $layers))
  AND (size(coalesce(a.layers, [])) = 0
       OR any(layer IN coalesce(a.layers, []) WHERE layer IN $layers))
  AND (size(coalesce(s.layers, [])) = 0
       OR any(layer IN coalesce(s.layers, []) WHERE layer IN $layers))
WITH s, r, o, indexScore, ownSpikeRank,
     max(CASE
       WHEN sp:Knowledge THEN 4
       WHEN sp:Insight THEN 3
       WHEN sp:Pattern THEN 2
       WHEN sp:Signal THEN 1
       ELSE 0
     END) AS attachedSpikeRank,
     coalesce(toInteger(s.weightText), s.weight, 0) AS subjectWeight,
     coalesce(toInteger(r.weightText), r.weight, 0) AS relationshipWeight,
     coalesce(toInteger(o.weightText), o.weight, 0) AS objectWeight
WITH s, r, o, indexScore,
     CASE WHEN ownSpikeRank > 0 THEN ownSpikeRank ELSE attachedSpikeRank END AS spikeRank,
     CASE
       WHEN subjectWeight > 0 AND relationshipWeight > $maxWeight - subjectWeight THEN $maxWeight
       WHEN subjectWeight < 0 AND relationshipWeight < $minWeight - subjectWeight THEN $minWeight
       ELSE subjectWeight + relationshipWeight
     END AS subjectRelationshipWeight,
     objectWeight
WITH s, r, o, spikeRank,
     CASE
       WHEN subjectRelationshipWeight > 0 AND objectWeight > $maxWeight - subjectRelationshipWeight THEN $maxWeight
       WHEN subjectRelationshipWeight < 0 AND objectWeight < $minWeight - subjectRelationshipWeight THEN $minWeight
       ELSE subjectRelationshipWeight + objectWeight
     END AS effectiveWeight,
     CASE WHEN indexScore > 1.0 THEN indexScore ELSE 1.0 END AS score,
     coalesce(r.propertyIri, 'mindreader:property/' + type(r)) AS property
ORDER BY spikeRank DESC, effectiveWeight DESC, score DESC,
         s.iri ASC, property ASC, o.iri ASC, r.iri ASC
LIMIT $limit
RETURN s, r, o, property, spikeRank, toString(effectiveWeight) AS effectiveWeight, score
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

/// Neighbor `ABOUT` facts that give Spike context around ranked subjects.
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
              AND (sp:Knowledge OR sp:Insight OR sp:Pattern OR sp:Signal)
              AND (size(coalesce(sp.layers, [])) = 0
                   OR any(layer IN coalesce(sp.layers, []) WHERE layer IN $layers))
              AND (size(coalesce(a.layers, [])) = 0
                   OR any(layer IN coalesce(a.layers, []) WHERE layer IN $layers))
              AND (size(coalesce(el.layers, [])) = 0
                   OR any(layer IN coalesce(el.layers, []) WHERE layer IN $layers))
            WITH sp, a, el,
                 CASE
                   WHEN sp:Knowledge THEN 4
                   WHEN sp:Insight THEN 3
                   WHEN sp:Pattern THEN 2
                   WHEN sp:Signal THEN 1
                   ELSE 0
                 END AS spikeRank,
                 coalesce(toInteger(sp.weightText), sp.weight, 0) AS spikeWeight,
                 coalesce(toInteger(a.weightText), a.weight, 0) AS relationshipWeight,
                 coalesce(toInteger(el.weightText), el.weight, 0) AS elementWeight
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
            RETURN sp, a, el, toString(effectiveWeight) AS effectiveWeight
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
        let combined = row
            .get::<String>("effectiveWeight")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or_else(|| {
                node_weight(&sp)
                    .saturating_add(relation_weight(&about_rel))
                    .saturating_add(node_weight(&element))
            });
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

// Prefer `weightText` so signed weights survive neo4rs 0.8 integer decoding.
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

/// Rank current visible `ASSERTS`/`ABOUT` facts for text and/or non-schema labels.
pub async fn memory_search(graph: &Graph, args: SearchArgs) -> Result<Value> {
    let layers = normalize_layers(args.layers)?;
    let limit = args.limit.unwrap_or(20).clamp(1, 100) as i64;
    let labels = args.labels.unwrap_or_default();
    let trimmed = args.text.unwrap_or_default().trim().to_string();
    if trimmed.is_empty() && labels.is_empty() {
        return Ok(json!({
            "ok": true,
            "mode": "labels",
            "facts": [],
            "nodes": [],
            "paths": [],
            "about": [],
            "lookups": [],
            "scope": layers,
            "truncated": false,
        }));
    }

    let text_mode = !trimmed.is_empty();
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
            .param("q", lucene_query(&trimmed))
            .param("predicateName", trimmed.clone())
            .param("predicateIri", property_iri(&trimmed));
    }

    let mut facts = Vec::new();
    for row in fetch_all(graph, ranked).await? {
        let (Ok(subject), Ok(relationship), Ok(object)) = (
            row.get::<Node>("s"),
            row.get::<Relation>("r"),
            row.get::<Node>("o"),
        ) else {
            continue;
        };
        let subject_iri = subject.get::<String>("iri").unwrap_or_default();
        let object_iri = object.get::<String>("iri").unwrap_or_default();
        let effective_weight = row
            .get::<String>("effectiveWeight")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        let property = row.get::<String>("property")?;
        let relationship = rel_json(&relationship, &subject_iri, &object_iri);
        let scope = relationship
            .get("scope")
            .cloned()
            .unwrap_or_else(|| json!([]));
        let scope_vec = scope
            .as_array()
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let mut fact = fact_envelope(
            endpoint_json(&subject),
            &property,
            endpoint_json(&object),
            &relationship,
            &scope_vec,
            spike_from_rank(row.get::<i64>("spikeRank").unwrap_or(0)).map(Value::String),
        );
        fact["score"] = json!(row.get::<f64>("score")?);
        fact["effectiveWeight"] = json!(effective_weight);
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
    Ok(json!({
        "ok": true,
        "mode": if text_mode { "text" } else { "labels" },
        "facts": facts,
        "nodes": [],
        "paths": [],
        "about": spike,
        "lookups": [],
        "scope": layers,
        "truncated": truncated,
    }))
}

#[cfg(test)]
mod tests {
    use super::{
        lucene_escape, lucene_query, ranked_query, spike_from_rank, validate_recall_args,
        RecallArgs,
    };

    #[test]
    fn search_helpers_preserve_contract_values() {
        assert_eq!(lucene_escape("C++"), "\"C\\+\\+\"");
        assert_eq!(
            lucene_query("graphModel"),
            "(\"graphModel\" OR \"graph Model\")"
        );
        assert!(ranked_query(true).contains("r.propertyIri = p.iri"));
        assert_eq!(spike_from_rank(4).as_deref(), Some("Knowledge"));
        assert_eq!(spike_from_rank(0), None);
        let query = ranked_query(true);
        assert!(query.contains("ORDER BY spikeRank DESC, effectiveWeight DESC, score DESC"));
        assert!(query.contains("LIMIT $limit"));
        assert!(!query.contains("collect("));
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
            iris: Some(vec![
                "mindreader:element/alice".into(),
                " mindreader:element/alice ".into(),
            ]),
            ..base
        })
        .is_err());
    }
}
