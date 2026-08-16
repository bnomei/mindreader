//! Database-side retrieval ranking and bounded fact assembly for `memory_search`.
//!
//! Full-text wakeup indexes and optional label filters produce candidates;
//! ranking is Spike category first (Knowledge > Insight > Pattern > Signal),
//! then shared subject+relationship+object weight within that category, then
//! text score. Layer filters require visible endpoints and current
//! relationships (`validTo` null). Retrieval never mutates weights.

use crate::error::Result;
use crate::graph::{endpoint_json, fetch_all, node_json, rel_json, spike_label, spike_rank};
use crate::layers::validate_layer_ids;
use neo4rs::{query, Graph, Node, Relation};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashSet;

/// Arguments for full-text / label-scoped fact retrieval under a layer union.
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

fn normalize_layers(raw: Vec<String>) -> Result<Vec<String>> {
    Ok(validate_layer_ids(raw)?
        .into_iter()
        .map(|layer| layer.into_string())
        .collect())
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

fn ranked_query(text_mode: bool) -> String {
    let candidates = if text_mode {
        TEXT_CANDIDATES
    } else {
        LABEL_CANDIDATES
    };
    format!("{candidates}{RANK_AND_LIMIT}")
}

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

/// Rank current visible facts for a text and/or label query under the request layer union.
pub async fn memory_search(graph: &Graph, args: SearchArgs) -> Result<Value> {
    let layers = normalize_layers(args.layers)?;
    let limit = args.limit.unwrap_or(20).clamp(1, 100) as i64;
    let labels = args.labels.unwrap_or_default();
    let trimmed = args.text.unwrap_or_default().trim().to_string();
    if trimmed.is_empty() && labels.is_empty() {
        return Ok(json!({
            "query": Value::Null,
            "mode": "wakeup",
            "facts": [],
            "spike": [],
            "layers": layers,
        }));
    }

    let text_mode = !trimmed.is_empty();
    let mut ranked = query(&ranked_query(text_mode))
        .param("layers", layers.clone())
        .param("labels", labels.clone())
        .param("labelCount", labels.len() as i64)
        .param("limit", limit)
        .param("minWeight", i64::MIN)
        .param("maxWeight", i64::MAX);
    if text_mode {
        ranked = ranked.param("q", lucene_escape(&trimmed));
    }

    let mut facts = Vec::new();
    let mut element_iris = HashSet::new();
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
        element_iris.insert(subject_iri.clone());
        if object.labels().contains(&"Element") {
            element_iris.insert(object_iri.clone());
        }
        let effective_weight = row
            .get::<String>("effectiveWeight")
            .ok()
            .and_then(|value| value.parse::<i64>().ok())
            .unwrap_or(0);
        facts.push(json!({
            "s": endpoint_json(&subject),
            "p": row.get::<String>("property")?,
            "o": endpoint_json(&object),
            "relationship": rel_json(&relationship, &subject_iri, &object_iri),
            "layers": relationship.get::<Vec<String>>("layers").unwrap_or_default(),
            "spike": spike_from_rank(row.get::<i64>("spikeRank").unwrap_or(0)),
            "score": row.get::<f64>("score")?,
            "effectiveWeight": effective_weight,
        }));
    }
    let spike = spike_context(
        graph,
        &layers,
        element_iris.into_iter().collect::<Vec<_>>(),
        limit,
    )
    .await?;
    Ok(json!({
        "query": if trimmed.is_empty() { Value::Null } else { json!(trimmed) },
        "mode": "wakeup",
        "facts": facts,
        "spike": spike,
        "layers": layers,
    }))
}

#[cfg(test)]
mod tests {
    use super::{lucene_escape, ranked_query, spike_from_rank};

    #[test]
    fn search_helpers_preserve_contract_values() {
        assert_eq!(lucene_escape("C++"), "\"C\\+\\+\"");
        assert_eq!(spike_from_rank(4).as_deref(), Some("Knowledge"));
        assert_eq!(spike_from_rank(0), None);
        let query = ranked_query(true);
        assert!(query.contains("ORDER BY spikeRank DESC, effectiveWeight DESC, score DESC"));
        assert!(query.contains("LIMIT $limit"));
        assert!(!query.contains("collect("));
    }
}
