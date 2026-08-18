//! Closed-world `recall` walks: `iris`, `around`, `history`, and the schema catalog.
//!
//! Ranked text/label search lives in [`crate::search`]; this module handles
//! input-ordered IRI lookups, hub-aware neighborhood paths, revision/history
//! envelopes (including Episode `selectedScope` visibility), and Class/Property
//! catalog listing. Class and Property nodes stay global (`layers=[]`,
//! `stub=false`). These paths are read-only and never change judgment weights.

use super::facts::{normalize_layers, serialized_scope};
use crate::domain::{normalize_rfc3339, DomainError};
use crate::graph::{
    endpoint_json, fetch_all, fetch_one, node_json, path_to_json, rel_json, safe_label,
};
use crate::search::RecallArgs;
use crate::vocabulary::{FIXED_RELATIONSHIPS, SYSTEM_RELATIONSHIPS};
use crate::{error::Result, operation_error};
use neo4rs::{query, Graph, Node, Path, Relation};
use serde_json::{json, Value};
use std::collections::HashMap;

/// Input-ordered node lookup for `recall` `iris`; misses stay `found: false`.
pub(super) const RECALL_IRI_NODES_QUERY: &str = r#"
UNWIND range(0, size($iris) - 1) AS inputIndex
WITH inputIndex, $iris[inputIndex] AS iri
OPTIONAL MATCH (n:Entity {iri: iri})
WHERE size(n.layers) = 0
   OR any(layer IN n.layers WHERE layer IN $layers)
RETURN inputIndex, iri, n IS NOT NULL AS found, n
ORDER BY inputIndex ASC
"#;

/// Incident current facts for each found IRI, bounded per lookup before hops=1 copies them up.
pub(super) const RECALL_IRI_FACTS_QUERY: &str = r#"
UNWIND range(0, size($iris) - 1) AS inputIndex
WITH inputIndex, $iris[inputIndex] AS iri
MATCH (root:Entity {iri: iri})
WHERE size(root.layers) = 0
   OR any(layer IN root.layers WHERE layer IN $layers)
CALL {
  WITH root
  MATCH (root)-[r]-(other:Entity)
  WHERE r.validTo IS NULL
    AND type(r) <> 'ABOUT'
    AND ($effectiveAt IS NULL OR type(r) <> 'ASSERTS'
         OR (coalesce(r.effectiveQualified, false)
             AND (r.effectiveFrom IS NULL OR r.effectiveFrom <= datetime($effectiveAt))
             AND (r.effectiveTo IS NULL OR datetime($effectiveAt) < r.effectiveTo)))
    AND (size(r.layers) = 0
         OR any(layer IN r.layers WHERE layer IN $layers))
    AND (size(other.layers) = 0
         OR any(layer IN other.layers WHERE layer IN $layers))
  WITH r, startNode(r) AS s, endNode(r) AS o,
       r.propertyIri AS property
  ORDER BY s.iri ASC, property ASC, o.iri ASC, r.iri ASC
  LIMIT $limit
  RETURN s, r, o, property
}
RETURN inputIndex, s, r, o, property
ORDER BY inputIndex ASC, s.iri ASC, property ASC, o.iri ASC, r.iri ASC
"#;

/// Closed-world `recall` `iris`: input-ordered node lookups in `scope`, at most 20 IRIs.
///
/// `hops=1` copies per-lookup incident facts (`ABOUT` excluded) into the top-level `facts` array.
pub async fn memory_recall_iris(
    graph: &Graph,
    iris: Vec<String>,
    scope: Vec<String>,
    hops: u32,
    fact_limit: u32,
    effective_at: Option<String>,
) -> Result<Value> {
    let layers = normalize_layers(scope)?;
    if !(1..=20).contains(&iris.len()) {
        return Err(
            DomainError::InvalidInput("recall iris must contain 1..=20 node IRIs".into()).into(),
        );
    }
    if hops > 1 {
        return Err(DomainError::InvalidInput("recall hops must be 0 or 1".into()).into());
    }
    if !(1..=100).contains(&fact_limit) {
        return Err(DomainError::InvalidInput("recall limit must be 1..=100".into()).into());
    }
    let iris = iris
        .into_iter()
        .map(|value| value.trim().to_string())
        .collect::<Vec<_>>();
    let effective_at = effective_at
        .as_deref()
        .map(|value| normalize_rfc3339(value, "recall effectiveAt"))
        .transpose()?;
    let mut lookups = iris
        .iter()
        .map(|iri| json!({ "iri": iri, "found": false, "facts": [], "truncated": false }))
        .collect::<Vec<_>>();
    let mut nodes = Vec::with_capacity(iris.len());
    for row in fetch_all(
        graph,
        query(RECALL_IRI_NODES_QUERY)
            .param("iris", iris.clone())
            .param("layers", layers.clone()),
    )
    .await?
    {
        let index = usize::try_from(row.get::<i64>("inputIndex")?)
            .map_err(|_| operation_error!("recall returned a negative input index"))?;
        if !row.get::<bool>("found")? {
            continue;
        }
        let node = node_json(&row.get::<Node>("n")?)?;
        nodes.push(node.clone());
        let lookup = lookups
            .get_mut(index)
            .ok_or_else(|| operation_error!("recall returned an out-of-range input index"))?;
        lookup["found"] = json!(true);
        lookup["node"] = node;
    }

    let mut facts = Vec::new();
    let rows = fetch_all(
        graph,
        query(RECALL_IRI_FACTS_QUERY)
            .param("iris", iris)
            .param("layers", layers.clone())
            .param("limit", i64::from(fact_limit.saturating_add(1)))
            .param("effectiveAt", effective_at),
    )
    .await?;
    let mut counts: HashMap<i64, usize> = HashMap::new();
    for row in &rows {
        *counts.entry(row.get::<i64>("inputIndex")?).or_default() += 1;
    }
    let truncated = counts.values().any(|count| *count > fact_limit as usize);
    for (index, lookup) in lookups.iter_mut().enumerate() {
        lookup["truncated"] = json!(counts
            .get(&(index as i64))
            .is_some_and(|count| *count > fact_limit as usize));
    }
    let mut kept: HashMap<i64, usize> = HashMap::new();
    for row in rows {
        let index = row.get::<i64>("inputIndex")?;
        let subject = row.get::<Node>("s")?;
        let relationship = row.get::<Relation>("r")?;
        let object = row.get::<Node>("o")?;
        let property = row.get::<String>("property")?;
        let taken = kept.entry(index).or_default();
        if *taken >= fact_limit as usize {
            continue;
        }
        *taken += 1;
        let subject_iri = subject.get::<String>("iri")?;
        let object_iri = object.get::<String>("iri")?;
        let relationship = rel_json(&relationship, &subject_iri, &object_iri)?;
        let memberships = serialized_scope(&relationship)?;
        let fact = crate::graph::fact_envelope(
            endpoint_json(&subject)?,
            &property,
            endpoint_json(&object)?,
            &relationship,
            &memberships,
        )?;
        if let Some(lookup_facts) = lookups
            .get_mut(index as usize)
            .and_then(|lookup| lookup.get_mut("facts"))
            .and_then(Value::as_array_mut)
        {
            lookup_facts.push(fact.clone());
        }
        if hops == 1 {
            facts.push(fact);
        }
    }
    Ok(json!({
        "ok": true,
        "mode": "iris",
        "scope": layers,
        "lookups": lookups,
        "facts": facts,
        "nodes": nodes,
        "paths": [],
        "about": [],
        "truncated": truncated,
    }))
}

/// Non-start witness nodes above this eligible degree receive one route penalty.
pub(super) const HUB_DEGREE_THRESHOLD: u32 = 16;

#[derive(Debug, Clone, Copy)]
pub(super) enum RecallDirection {
    Both,
    Outgoing,
    Incoming,
}

impl RecallDirection {
    /// Accept `both`, `outgoing`, or `incoming` for `recall` `around`.
    pub(super) fn parse(value: &str) -> Result<Self> {
        match value {
            "both" => Ok(Self::Both),
            "outgoing" => Ok(Self::Outgoing),
            "incoming" => Ok(Self::Incoming),
            _ => Err(DomainError::InvalidInput(
                "recall direction must be both, outgoing, or incoming".into(),
            )
            .into()),
        }
    }
}

/// Variable-length walk query with whole-path predicate/direction constraints and hub-aware order.
pub(super) fn recall_around_query(depth: u32, direction: RecallDirection) -> String {
    // depth is 1..=3, validated by memory_recall_around before interpolation.
    let (walk, degree_walk) = match direction {
        RecallDirection::Both => (
            format!("(start:Entity {{iri: $from}})-[pathRels*1..{depth}]-(x:Entity)"),
            "(witnessNode)-[hubRel]-(hubOther:Entity)",
        ),
        RecallDirection::Outgoing => (
            format!("(start:Entity {{iri: $from}})-[pathRels*1..{depth}]->(x:Entity)"),
            "(witnessNode)-[hubRel]->(hubOther:Entity)",
        ),
        RecallDirection::Incoming => (
            format!("(start:Entity {{iri: $from}})<-[pathRels*1..{depth}]-(x:Entity)"),
            "(witnessNode)<-[hubRel]-(hubOther:Entity)",
        ),
    };
    let hub_probe_limit = HUB_DEGREE_THRESHOLD + 1;
    format!(
        r#"
        MATCH path = {walk}
        WHERE all(n IN nodes(path) WHERE
          size(n.layers) = 0
          OR any(layer IN n.layers WHERE layer IN $layers))
          AND all(pathRel IN relationships(path) WHERE
            type(pathRel) IN $rels AND pathRel.validTo IS NULL
            AND ($effectiveAt IS NULL OR type(pathRel) <> 'ASSERTS'
                 OR (coalesce(pathRel.effectiveQualified, false)
                     AND (pathRel.effectiveFrom IS NULL
                          OR pathRel.effectiveFrom <= datetime($effectiveAt))
                     AND (pathRel.effectiveTo IS NULL
                          OR datetime($effectiveAt) < pathRel.effectiveTo)))
            AND (size(pathRel.layers) = 0
                 OR any(layer IN pathRel.layers WHERE layer IN $layers))
            AND (size($predicates) = 0 OR pathRel.propertyIri IN $predicates))
        CALL {{
          WITH path
          UNWIND tail(nodes(path)) AS witnessNode
          CALL {{
            WITH witnessNode
            MATCH {degree_walk}
            WHERE type(hubRel) IN $rels AND hubRel.validTo IS NULL
              AND ($effectiveAt IS NULL OR type(hubRel) <> 'ASSERTS'
                   OR (coalesce(hubRel.effectiveQualified, false)
                       AND (hubRel.effectiveFrom IS NULL
                            OR hubRel.effectiveFrom <= datetime($effectiveAt))
                       AND (hubRel.effectiveTo IS NULL
                            OR datetime($effectiveAt) < hubRel.effectiveTo)))
              AND (size(witnessNode.layers) = 0
                   OR any(layer IN witnessNode.layers WHERE layer IN $layers))
              AND (size(hubRel.layers) = 0
                   OR any(layer IN hubRel.layers WHERE layer IN $layers))
              AND (size(hubOther.layers) = 0
                   OR any(layer IN hubOther.layers WHERE layer IN $layers))
              AND (size($predicates) = 0 OR hubRel.propertyIri IN $predicates)
            WITH hubRel LIMIT {hub_probe_limit}
            RETURN count(hubRel) AS eligibleDegree
          }}
          RETURN sum(CASE WHEN eligibleDegree > {HUB_DEGREE_THRESHOLD} THEN 1 ELSE 0 END) AS hubCount
        }}
        UNWIND relationships(path) AS r
        WITH path, r,
             r.propertyIri AS property,
             length(path) AS distance,
             length(path) + hubCount AS routeCost,
             hubCount,
             [node IN nodes(path) | node.iri] AS pathNodes,
             [pathRel IN relationships(path) | pathRel.iri] AS pathEdgeIris
        ORDER BY r.iri ASC, routeCost ASC, hubCount ASC, distance ASC,
                 pathNodes ASC, pathEdgeIris ASC
        WITH r, property, head(collect({{
          path: path,
          nodes: pathNodes,
          routeCost: routeCost,
          hubCount: hubCount,
          distance: distance,
          edgeIris: pathEdgeIris
        }})) AS witness
        WITH witness.distance AS distance, witness.routeCost AS routeCost,
             witness.hubCount AS hubCount, witness.path AS path,
             witness.nodes AS witnessNodes, witness.edgeIris AS witnessEdgeIris,
             startNode(r) AS s, r, endNode(r) AS o,
             property
        ORDER BY routeCost ASC, hubCount ASC, distance ASC,
                 witnessNodes ASC, witnessEdgeIris ASC,
                 s.iri ASC, property ASC, o.iri ASC, r.iri ASC
        LIMIT $limit
        RETURN distance, path, witnessNodes AS pathNodes, s, r, o, property
        "#
    )
}

/// `recall` `around` path with whole-path constraints and hub-aware deterministic ranking.
pub async fn memory_recall_around(graph: &Graph, from: &str, args: &RecallArgs) -> Result<Value> {
    use crate::domain::PredicateRef;
    let depth = args.depth.unwrap_or(1);
    let limit = args.limit.unwrap_or(20);
    if !(1..=3).contains(&depth) {
        return Err(DomainError::InvalidInput("recall depth must be 1..=3".into()).into());
    }
    if !(1..=100).contains(&limit) {
        return Err(DomainError::InvalidInput("recall limit must be 1..=100".into()).into());
    }
    let direction = RecallDirection::parse(args.direction.as_deref().unwrap_or("both"))?;
    let layers = normalize_layers(args.scope.clone())?;
    let effective_at = args
        .effective_at
        .as_deref()
        .map(|value| normalize_rfc3339(value, "recall effectiveAt"))
        .transpose()?;
    let mut wanted = args
        .p
        .clone()
        .unwrap_or_default()
        .into_iter()
        .map(|value| PredicateRef::parse(value).map(|predicate| predicate.iri().to_string()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    wanted.sort();
    wanted.dedup();
    let start_row = fetch_one(
        graph,
        query(
            "MATCH (n:Entity {iri: $iri}) \
             WHERE size(n.layers) = 0 \
                OR any(layer IN n.layers WHERE layer IN $layers) \
             RETURN n",
        )
        .param("iri", from.to_string())
        .param("layers", layers.clone()),
    )
    .await?;
    let Some(start_row) = start_row else {
        return Ok(json!({
            "ok": true,
            "mode": "around",
            "scope": layers,
            "from": from,
            "facts": [],
            "nodes": [],
            "paths": [],
            "about": [],
            "lookups": [],
            "truncated": false,
        }));
    };
    let start: Node = start_row.get("n")?;
    let rows = fetch_all(
        graph,
        query(&recall_around_query(depth, direction))
            .param("from", from.to_string())
            .param("layers", layers.clone())
            .param(
                "rels",
                FIXED_RELATIONSHIPS
                    .iter()
                    .filter(|relationship| **relationship != "ABOUT")
                    .map(|relationship| (*relationship).to_string())
                    .collect::<Vec<_>>(),
            )
            .param("predicates", wanted)
            .param("limit", i64::from(limit.saturating_add(1)))
            .param("effectiveAt", effective_at),
    )
    .await?;
    let truncated = rows.len() > limit as usize;
    let mut facts = Vec::new();
    let mut paths = Vec::new();
    let mut nodes_by_iri = HashMap::new();
    nodes_by_iri.insert(from.to_string(), node_json(&start)?);
    for row in rows.into_iter().take(limit as usize) {
        let path = row.get::<Path>("path")?;
        let path_nodes = row.get::<Vec<String>>("pathNodes")?;
        let subject = row.get::<Node>("s")?;
        let relationship = row.get::<Relation>("r")?;
        let object = row.get::<Node>("o")?;
        let property = row.get::<String>("property")?;
        let subject_iri = subject.get::<String>("iri")?;
        let object_iri = object.get::<String>("iri")?;
        nodes_by_iri.insert(subject_iri.clone(), node_json(&subject)?);
        nodes_by_iri.insert(object_iri.clone(), node_json(&object)?);
        let (decoded_nodes, path_edges, _) = path_to_json(&path)?;
        for node in decoded_nodes {
            if let Some(iri) = node.get("iri").and_then(Value::as_str) {
                nodes_by_iri.insert(iri.to_string(), node);
            }
        }
        paths.push(json!({ "nodes": path_nodes, "edges": path_edges }));
        let relationship = rel_json(&relationship, &subject_iri, &object_iri)?;
        let memberships = serialized_scope(&relationship)?;
        facts.push(crate::graph::fact_envelope(
            endpoint_json(&subject)?,
            &property,
            endpoint_json(&object)?,
            &relationship,
            &memberships,
        )?);
    }
    let mut nodes = nodes_by_iri.into_iter().collect::<Vec<_>>();
    nodes.sort_by(|left, right| left.0.cmp(&right.0));
    let nodes = nodes.into_iter().map(|(_, node)| node).collect::<Vec<_>>();
    Ok(json!({
        "ok": true,
        "mode": "around",
        "scope": layers,
        "from": from,
        "facts": facts,
        "nodes": nodes,
        "paths": paths,
        "about": [],
        "lookups": [],
        "truncated": truncated,
    }))
}

/// Current and `validTo` facts that share a fact handle's subject and predicate.
///
/// A `validTo` fact stays visible when its `layers` intersect `scope` or a `revise`
/// Episode lists it with intersecting `selectedScope`.
const RECALL_HISTORY_FACT_QUERY: &str = r#"
MATCH (s:Entity)-[anchor]->(o:Entity)
WHERE anchor.iri = $iri
  AND (size(s.layers) = 0
       OR any(layer IN s.layers WHERE layer IN $layers))
  AND (size(o.layers) = 0
       OR any(layer IN o.layers WHERE layer IN $layers))
  AND ((size(anchor.layers) = 0
        OR any(layer IN anchor.layers WHERE layer IN $layers))
       OR EXISTS {
         MATCH (event:Entity:Episode {tool: 'revise'})
         WHERE anchor.iri IN [event.previousFactIri, event.replacementFactIri]
           AND (size(event.selectedScope) = 0
                OR any(layer IN event.selectedScope WHERE layer IN $layers))
       })
WITH s, anchor.propertyIri AS property
MATCH (s)-[r]->(other:Entity)
WHERE r.propertyIri = property
  AND type(r) <> 'ABOUT'
  AND NOT type(r) IN $protected
  AND ((size(r.layers) = 0
        OR any(layer IN r.layers WHERE layer IN $layers))
       OR EXISTS {
         MATCH (event:Entity:Episode {tool: 'revise'})
         WHERE r.iri IN [event.previousFactIri, event.replacementFactIri]
           AND (size(event.selectedScope) = 0
                OR any(layer IN event.selectedScope WHERE layer IN $layers))
       })
  AND (size(other.layers) = 0
       OR any(layer IN other.layers WHERE layer IN $layers))
WITH s, r, other, property, r.validTo IS NULL AS current,
     toString(r.validFrom) AS transactionFrom, toString(r.validTo) AS validTo
ORDER BY current DESC, validTo DESC, r.iri ASC
LIMIT $limit
RETURN s, r, other AS o, property, current, transactionFrom, validTo
"#;

/// Current and historical incident facts for a node handle (system-owned edges excluded).
const RECALL_HISTORY_NODE_QUERY: &str = r#"
MATCH (n:Entity {iri: $iri})
WHERE size(n.layers) = 0
   OR any(layer IN n.layers WHERE layer IN $layers)
MATCH (n)-[r]-(other:Entity)
WHERE type(r) <> 'ABOUT'
  AND NOT type(r) IN $protected
  AND ((size(r.layers) = 0
        OR any(layer IN r.layers WHERE layer IN $layers))
       OR EXISTS {
         MATCH (event:Entity:Episode {tool: 'revise'})
         WHERE r.iri IN [event.previousFactIri, event.replacementFactIri]
           AND (size(event.selectedScope) = 0
                OR any(layer IN event.selectedScope WHERE layer IN $layers))
       })
  AND (size(other.layers) = 0
       OR any(layer IN other.layers WHERE layer IN $layers))
WITH startNode(r) AS s, r, endNode(r) AS o,
     r.propertyIri AS property,
     r.validTo IS NULL AS current,
     toString(r.validFrom) AS transactionFrom, toString(r.validTo) AS validTo
ORDER BY current DESC, validTo DESC, r.iri ASC
LIMIT $limit
RETURN s, r, o, property, current, transactionFrom, validTo
"#;

/// Exact revision events for the fact family selected by one fact handle.
const RECALL_REVISIONS_FACT_QUERY: &str = r#"
MATCH (anchorS:Entity)-[anchor]->(anchorO:Entity)
WHERE anchor.iri = $iri
  AND (size(anchorS.layers) = 0
       OR any(layer IN anchorS.layers WHERE layer IN $layers))
  AND (size(anchorO.layers) = 0
       OR any(layer IN anchorO.layers WHERE layer IN $layers))
WITH anchorS, anchor.propertyIri AS property
MATCH (event:Entity:Episode {tool: 'revise'})
WHERE size(event.selectedScope) = 0
   OR any(layer IN event.selectedScope WHERE layer IN $layers)
MATCH (anchorS)-[family]->(familyObject:Entity)
WHERE family.propertyIri = property
  AND family.iri IN [event.previousFactIri, event.replacementFactIri]
  AND (size(familyObject.layers) = 0
       OR any(layer IN familyObject.layers WHERE layer IN $layers))
MATCH ()-[previous]->() WHERE previous.iri = event.previousFactIri
MATCH ()-[replacement]->() WHERE replacement.iri = event.replacementFactIri
MATCH (supersedesS:Entity)-[supersedes:SUPERSEDES]->(supersedesO:Entity)
WHERE supersedes.iri = event.supersedesIri
RETURN DISTINCT event, toString(event.at) AS episodeAt,
       previous.iri AS previousFactIri,
       replacement.iri AS replacementFactIri,
       supersedesS, supersedes, supersedesO
ORDER BY event.at DESC, event.iri ASC
LIMIT $limit
"#;

/// Exact revision events involving any ordinary fact incident to one node handle.
const RECALL_REVISIONS_NODE_QUERY: &str = r#"
MATCH (n:Entity {iri: $iri})
WHERE size(n.layers) = 0
   OR any(layer IN n.layers WHERE layer IN $layers)
MATCH (event:Entity:Episode {tool: 'revise'})
WHERE size(event.selectedScope) = 0
   OR any(layer IN event.selectedScope WHERE layer IN $layers)
MATCH (n)-[family]-(familyObject:Entity)
WHERE family.iri IN [event.previousFactIri, event.replacementFactIri]
  AND (size(familyObject.layers) = 0
       OR any(layer IN familyObject.layers WHERE layer IN $layers))
MATCH ()-[previous]->() WHERE previous.iri = event.previousFactIri
MATCH ()-[replacement]->() WHERE replacement.iri = event.replacementFactIri
MATCH (supersedesS:Entity)-[supersedes:SUPERSEDES]->(supersedesO:Entity)
WHERE supersedes.iri = event.supersedesIri
RETURN DISTINCT event, toString(event.at) AS episodeAt,
       previous.iri AS previousFactIri,
       replacement.iri AS replacementFactIri,
       supersedesS, supersedes, supersedesO
ORDER BY event.at DESC, event.iri ASC
LIMIT $limit
"#;

/// Closed-world `recall` `history`: current and `validTo` facts plus `revise` Episodes for one node or fact handle.
///
/// Includes facts visible only via Episode `selectedScope`.
pub async fn memory_recall_history(
    graph: &Graph,
    iri: &str,
    scope: Vec<String>,
    limit: u32,
) -> Result<Value> {
    if !(1..=100).contains(&limit) {
        return Err(DomainError::InvalidInput("recall limit must be 1..=100".into()).into());
    }
    let layers = normalize_layers(scope)?;
    let protected = SYSTEM_RELATIONSHIPS
        .iter()
        .map(|value| (*value).to_string())
        .collect::<Vec<_>>();
    let fact_iri = iri.starts_with("mindreader:relationship/");
    let found_query = if fact_iri {
        "MATCH (s:Entity)-[r]->(o:Entity) WHERE r.iri = $iri \
         AND (size(s.layers) = 0 \
              OR any(layer IN s.layers WHERE layer IN $layers)) \
         AND (size(o.layers) = 0 \
              OR any(layer IN o.layers WHERE layer IN $layers)) \
         AND ((size(r.layers) = 0 \
               OR any(layer IN r.layers WHERE layer IN $layers)) \
              OR EXISTS { \
                MATCH (event:Entity:Episode {tool: 'revise'}) \
                WHERE r.iri IN [event.previousFactIri, event.replacementFactIri] \
                  AND (size(event.selectedScope) = 0 \
                       OR any(layer IN event.selectedScope WHERE layer IN $layers)) \
              }) \
         RETURN count(r) AS n"
    } else {
        "MATCH (n:Entity {iri: $iri}) \
         WHERE size(n.layers) = 0 \
            OR any(layer IN n.layers WHERE layer IN $layers) \
         RETURN count(n) AS n"
    };
    let found = fetch_one(
        graph,
        query(found_query)
            .param("iri", iri.to_string())
            .param("layers", layers.clone()),
    )
    .await?
    .ok_or_else(|| operation_error!("history existence query returned no row"))?
    .get::<i64>("n")?
        > 0;
    if !found {
        return Ok(json!({
            "ok": true,
            "mode": "history",
            "scope": layers,
            "from": iri,
            "facts": [],
            "nodes": [],
            "paths": [],
            "revisions": [],
            "revisionsTruncated": false,
            "about": [],
            "lookups": [{ "iri": iri, "found": false, "facts": [], "truncated": false }],
            "truncated": false,
        }));
    }
    let cypher = if fact_iri {
        RECALL_HISTORY_FACT_QUERY
    } else {
        RECALL_HISTORY_NODE_QUERY
    };
    let rows = fetch_all(
        graph,
        query(cypher)
            .param("iri", iri.to_string())
            .param("layers", layers.clone())
            .param("protected", protected)
            .param("limit", i64::from(limit.saturating_add(1))),
    )
    .await?;
    let facts_truncated = rows.len() > limit as usize;
    let revision_rows = fetch_all(
        graph,
        query(if fact_iri {
            RECALL_REVISIONS_FACT_QUERY
        } else {
            RECALL_REVISIONS_NODE_QUERY
        })
        .param("iri", iri.to_string())
        .param("layers", layers.clone())
        .param("limit", i64::from(limit.saturating_add(1))),
    )
    .await?;
    let revisions_truncated = revision_rows.len() > limit as usize;
    let mut facts = Vec::new();
    let mut nodes_by_iri = std::collections::HashMap::new();
    if !fact_iri {
        if let Some(row) = fetch_one(
            graph,
            query(
                "MATCH (n:Entity {iri: $iri}) \
                 WHERE size(n.layers) = 0 \
                    OR any(layer IN n.layers WHERE layer IN $layers) \
                 RETURN n",
            )
            .param("iri", iri.to_string())
            .param("layers", layers.clone()),
        )
        .await?
        {
            let node = row.get::<Node>("n")?;
            nodes_by_iri.insert(iri.to_string(), node_json(&node)?);
        }
    }
    for row in rows.into_iter().take(limit as usize) {
        let subject = row.get::<Node>("s")?;
        let relationship = row.get::<Relation>("r")?;
        let object = row.get::<Node>("o")?;
        let property = row.get::<String>("property")?;
        let current = row.get::<bool>("current")?;
        let subject_iri = subject.get::<String>("iri")?;
        let object_iri = object.get::<String>("iri")?;
        nodes_by_iri.insert(subject_iri.clone(), node_json(&subject)?);
        nodes_by_iri.insert(object_iri.clone(), node_json(&object)?);
        let relationship = rel_json(&relationship, &subject_iri, &object_iri)?;
        let memberships = serialized_scope(&relationship)?;
        let mut fact = crate::graph::fact_envelope(
            endpoint_json(&subject)?,
            &property,
            endpoint_json(&object)?,
            &relationship,
            &memberships,
        )?;
        fact["current"] = json!(current);
        fact["transactionCurrent"] = json!(current);
        let transaction_from = row.get::<String>("transactionFrom")?;
        let transaction_to = row
            .get::<String>("validTo")
            .ok()
            .filter(|value| !value.is_empty() && value != "null");
        fact["transaction"] = json!({
            "from": transaction_from,
            "to": transaction_to,
        });
        if let Ok(valid_to) = row.get::<String>("validTo") {
            if !valid_to.is_empty() && valid_to != "null" {
                fact["validTo"] = json!(valid_to);
            }
        }
        facts.push(fact);
    }
    let mut revisions = Vec::new();
    for row in revision_rows.into_iter().take(limit as usize) {
        let event = row.get::<Node>("event")?;
        let supersedes_subject = row.get::<Node>("supersedesS")?;
        let supersedes_relationship = row.get::<Relation>("supersedes")?;
        let supersedes_object = row.get::<Node>("supersedesO")?;
        let supersedes_from = supersedes_subject.get::<String>("iri")?;
        let supersedes_to = supersedes_object.get::<String>("iri")?;
        let supersedes = rel_json(&supersedes_relationship, &supersedes_from, &supersedes_to)?;
        revisions.push(json!({
            "replacement": {
                "kind": "fact",
                "iri": row.get::<String>("replacementFactIri")?,
            },
            "previous": {
                "kind": "fact",
                "iri": row.get::<String>("previousFactIri")?,
            },
            "scope": event.get::<Vec<String>>("selectedScope")?,
            "episode": {
                "iri": event.get::<String>("iri")?,
                "at": row.get::<String>("episodeAt")?,
                "tool": "revise",
            },
            "supersedes": {
                "iri": supersedes["iri"],
                "from": supersedes["from"],
                "to": supersedes["to"],
                "reason": supersedes.get("reason").cloned().unwrap_or(Value::Null),
            },
        }));
    }
    let mut nodes = nodes_by_iri.into_iter().collect::<Vec<_>>();
    nodes.sort_by(|left, right| left.0.cmp(&right.0));
    let nodes = nodes.into_iter().map(|(_, node)| node).collect::<Vec<_>>();
    Ok(json!({
        "ok": true,
        "mode": "history",
        "scope": layers,
        "from": iri,
        "facts": facts.clone(),
        "nodes": nodes,
        "paths": [],
        "revisions": revisions,
        "revisionsTruncated": revisions_truncated,
        "about": [],
        "lookups": [{
            "iri": iri,
            "found": true,
            "facts": [],
            "truncated": facts_truncated,
        }],
        "truncated": facts_truncated || revisions_truncated,
    }))
}

/// Load a visible current fact handle, distinguishing global-vs-named scope misses.
pub async fn list_schema_catalog(graph: &Graph, kind: &str) -> Result<Value> {
    let kind = kind.trim().to_ascii_lowercase();
    if kind != "class" && kind != "property" {
        return Err(DomainError::InvalidInput("kind must be class or property".into()).into());
    }
    let label = safe_label(if kind == "class" { "Class" } else { "Property" })?;
    let rows = fetch_all(
        graph,
        query(&format!(
            "MATCH (n:Entity:{label}) \
             RETURN n.iri AS iri, n.name AS name, n.stub AS stub, \
                    n.layers AS layers, n.weight AS weight \
             ORDER BY n.name, n.iri LIMIT 101"
        )),
    )
    .await?;
    let items = rows
        .into_iter()
        .map(|row| {
            let iri = row.get::<String>("iri")?;
            let name = row.get::<String>("name")?;
            let stub = row.get::<bool>("stub")?;
            let layers = row.get::<Vec<String>>("layers")?;
            let weight = row.get::<i64>("weight")?;
            if stub || !layers.is_empty() {
                return Err(operation_error!(
                    "schema catalog node {iri} must have stub=false and global scope"
                ));
            }
            Ok(json!({
                "kind": "node",
                "iri": iri.clone(),
                "name": name,
                "labels": [label.clone()],
                "scope": layers,
                "weight": weight,
                "stub": stub,
                "target": { "kind": "node", "iri": iri },
            }))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(json!({
        "list": true,
        "kind": kind,
        "items": items,
        "noop": true,
        "episode": Value::Null,
    }))
}
