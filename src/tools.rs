use crate::config::GLOBAL_LAYER;
use crate::domain::{
    DomainError, EntityInput, EntityRef, ObjectInput, ObjectValue, PredicateRef, RetractScope,
    SpikeRank,
};
use crate::graph::{
    acquire_fact_locks_in_txn, create_episode_in_txn, endpoint_json, ensure_property_in_txn,
    fact_text, fetch_all, fetch_all_txn, fetch_one, fetch_one_txn, get_node, merge_literal_in_txn,
    merge_node_in_txn, node_json, path_to_json, rel_json, safe_rel, spike_label, spike_rank,
    structural_rel_for, Episode, NodeSpec, FIXED_RELS, MODEL_MARKER_KEY, MODEL_VERSION,
};
use crate::iri::{class_iri, name_from_iri, property_iri};
use crate::layers::{assert_writable_layer, default_write_layer, visible_layers, LayerError};
use anyhow::{anyhow, Result};
use neo4rs::{query, Error as Neo4jDriverError, Graph, Neo4jErrorKind, Node, Path, Relation, Txn};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use tokio::time::{sleep, Duration};

const SCHEMA_STRUCTURAL_RELS: &[&str] = &[
    "INSTANCE_OF",
    "SUBCLASS_OF",
    "SUBPROPERTY_OF",
    "DOMAIN",
    "RANGE",
];
const SYSTEM_OWNED_RELS: &[&str] = &["CONTRADICTS", "SUPERSEDES"];

fn reject_system_owned_predicate(predicate: &str) -> Result<()> {
    if structural_rel_for(predicate)
        .as_deref()
        .is_some_and(|rel| SYSTEM_OWNED_RELS.contains(&rel))
    {
        return Err(DomainError::InvalidInput(format!(
            "predicate {predicate:?} is system-owned and cannot be mutated directly"
        ))
        .into());
    }
    Ok(())
}

fn is_transient_neo4j_error(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<Neo4jDriverError>()
            .is_some_and(|driver| {
                matches!(driver, Neo4jDriverError::Neo4j(error) if error.kind() == Neo4jErrorKind::Transient)
            })
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GetArgs {
    pub iri: String,
    #[serde(default)]
    pub hops: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchArgs {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema, Default)]
pub struct StatsArgs {}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TraverseArgs {
    pub from: String,
    #[serde(default)]
    pub rels: Option<Vec<String>>,
    #[serde(default)]
    pub depth: Option<u32>,
    #[serde(default)]
    pub limit: Option<u32>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct AssertArgs {
    pub s: EntityInput,
    pub p: String,
    pub o: ObjectInput,
    #[serde(default)]
    pub layer: Option<String>,
    #[serde(default)]
    pub spike: Option<String>,
    #[serde(default)]
    pub contradicts: bool,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct ReplaceArgs {
    pub s: EntityInput,
    pub p: String,
    pub old: ObjectInput,
    pub new: ObjectInput,
    #[serde(default)]
    pub layer: Option<String>,
    #[serde(default)]
    pub spike: Option<String>,
    #[serde(default)]
    pub contradicts: bool,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RetractTargetArgs {
    pub kind: String,
    pub s: EntityInput,
    #[serde(default)]
    pub p: Option<String>,
    #[serde(default)]
    pub o: Option<ObjectInput>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RetractArgs {
    pub target: RetractTargetArgs,
    #[serde(default)]
    pub layer: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct SchemaArgs {
    pub kind: String,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub iri: Option<String>,
    #[serde(default, rename = "subClassOf")]
    pub sub_class_of: Option<String>,
    #[serde(default, rename = "subPropertyOf")]
    pub sub_property_of: Option<String>,
    #[serde(default)]
    pub domain: Option<String>,
    #[serde(default)]
    pub range: Option<String>,
}

fn node_spec(entity: EntityRef) -> NodeSpec {
    NodeSpec {
        iri: entity.iri,
        name: entity.name,
        labels: entity.labels,
    }
}

pub async fn memory_get(graph: &Graph, project: &str, args: GetArgs) -> Result<Value> {
    let hops = if args.hops == Some(1) { 1 } else { 0 };
    let layers = visible_layers(project);
    let Some(node) = get_node(graph, &args.iri).await? else {
        return Ok(json!({ "found": false, "iri": args.iri }));
    };
    if hops == 0 {
        return Ok(json!({ "found": true, "node": node_json(&node), "hops": 0 }));
    }

    let rows = fetch_all(
        graph,
        query(
            r#"
            MATCH (n:Entity {iri: $iri})
            OPTIONAL MATCH (n)-[r]-(m:Entity)
            WHERE r IS NULL OR (
              r.validTo IS NULL
              AND (r.layer IN $layers)
            )
            RETURN n, r, m, CASE WHEN r IS NULL THEN false ELSE startNode(r) = n END AS outgoing
            "#,
        )
        .param("iri", args.iri.clone())
        .param("layers", layers.clone()),
    )
    .await?;

    let mut neighbors = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let rel: Option<Relation> = row.get("r").ok();
        let other: Option<Node> = row.get("m").ok();
        let (Some(r), Some(m)) = (rel, other) else {
            continue;
        };
        let key = r.id();
        if !seen.insert(key) {
            continue;
        }
        let outgoing: bool = row.get("outgoing").unwrap_or(false);
        let m_iri = m.get::<String>("iri").unwrap_or_default();
        let (from, to) = if outgoing {
            (args.iri.clone(), m_iri)
        } else {
            (m_iri, args.iri.clone())
        };
        neighbors.push(json!({
            "edge": rel_json(&r, &from, &to),
            "node": node_json(&m),
            "direction": if outgoing { "out" } else { "in" },
        }));
    }

    Ok(json!({
        "found": true,
        "node": node_json(&node),
        "hops": 1,
        "neighbors": neighbors,
        "layers": layers,
    }))
}

pub async fn memory_search(graph: &Graph, project: &str, args: SearchArgs) -> Result<Value> {
    let layers = visible_layers(project);
    let limit = args.limit.unwrap_or(20).clamp(1, 100) as i64;
    // Bound relationship fan-out before materialization. Rust applies the final
    // SPIKE/full-text ordering and deterministic tie-break within this window.
    let candidate_limit = (limit * 20).clamp(100, 2_000);
    let labels = args.labels.unwrap_or_default();
    let text = args.text.unwrap_or_default();
    let trimmed = text.trim().to_string();
    let needle = trimmed.to_ascii_lowercase();

    if trimmed.is_empty() && labels.is_empty() {
        return Ok(json!({
            "query": Value::Null,
            "mode": "wakeup",
            "facts": [],
            "spike": [],
            "layers": layers,
        }));
    }

    let mut node_scores: HashMap<String, f64> = HashMap::new();
    let mut rel_scores: HashMap<i64, f64> = HashMap::new();

    if !trimmed.is_empty() {
        let escaped = lucene_escape(&trimmed);
        let rows = fetch_all(
            graph,
            query(
                r#"
                CALL db.index.fulltext.queryNodes('wakeup_nodes', $q) YIELD node, score
                RETURN node.iri AS iri, score
                ORDER BY score DESC, iri ASC
                LIMIT $limit
                "#,
            )
            .param("q", escaped.clone())
            .param("limit", limit * 4),
        )
        .await?;
        for row in rows {
            if let (Ok(iri), Ok(score)) = (row.get::<String>("iri"), row.get::<f64>("score")) {
                let entry = node_scores.entry(iri).or_insert(0.0);
                if score > *entry {
                    *entry = score;
                }
            }
        }

        let rows = fetch_all(
            graph,
            query(
                r#"
                CALL db.index.fulltext.queryRelationships('wakeup_facts', $q) YIELD relationship, score
                RETURN id(relationship) AS rid, score
                ORDER BY score DESC, rid ASC
                LIMIT $limit
                "#,
            )
            .param("q", escaped)
            .param("limit", limit * 4),
        )
        .await?;
        for row in rows {
            if let (Ok(rid), Ok(score)) = (row.get::<i64>("rid"), row.get::<f64>("score")) {
                let entry = rel_scores.entry(rid).or_insert(0.0);
                if score > *entry {
                    *entry = score;
                }
            }
        }
    }

    let use_contains = trimmed.is_empty();
    let iris: Vec<String> = node_scores.keys().cloned().collect();
    let rids: Vec<i64> = rel_scores.keys().cloned().collect();

    let rows = fetch_all(
        graph,
        query(
            r#"
            MATCH (s:Entity)-[r]->(o:Entity)
            WHERE r.validTo IS NULL
              AND (r.layer IN $layers)
              AND (type(r) = 'ASSERTS' OR type(r) = 'ABOUT')
              AND ($labelCount = 0 OR any(l IN $labels WHERE l IN labels(s) OR l IN labels(o)))
              AND (
                ($useContains AND (
                  $text = ''
                  OR toLower(coalesce(r.factText, '')) CONTAINS $text
                  OR toLower(coalesce(s.name, '')) CONTAINS $text
                  OR toLower(s.iri) CONTAINS $text
                  OR toLower(coalesce(s.searchText, '')) CONTAINS $text
                  OR toLower(coalesce(s.value, '')) CONTAINS $text
                  OR toLower(coalesce(o.name, '')) CONTAINS $text
                  OR toLower(o.iri) CONTAINS $text
                  OR toLower(coalesce(o.value, '')) CONTAINS $text
                  OR toLower(coalesce(o.searchText, '')) CONTAINS $text
                ))
                OR (NOT $useContains AND (s.iri IN $iris OR o.iri IN $iris OR id(r) IN $rids))
              )
            RETURN s, r, o, id(r) AS rid
            ORDER BY
              CASE WHEN id(r) IN $rids THEN 0 ELSE 1 END,
              s.iri ASC,
              coalesce(r.propertyIri, type(r)) ASC,
              o.iri ASC,
              r.layer ASC,
              id(r) ASC
            LIMIT $candidateLimit
            "#,
        )
        .param("layers", layers.clone())
        .param("labels", labels.clone())
        .param("labelCount", labels.len() as i64)
        .param("useContains", use_contains)
        .param("text", needle.clone())
        .param("iris", iris)
        .param("rids", rids)
        .param("candidateLimit", candidate_limit),
    )
    .await?;

    let mut facts = Vec::new();
    let mut seen_facts = HashSet::new();
    let mut element_iris: HashSet<String> = HashSet::new();

    for row in rows {
        let s: Node = match row.get("s") {
            Ok(n) => n,
            Err(_) => continue,
        };
        let o: Node = match row.get("o") {
            Ok(n) => n,
            Err(_) => continue,
        };
        let r: Relation = match row.get("r") {
            Ok(n) => n,
            Err(_) => continue,
        };
        let rid: i64 = row.get("rid").unwrap_or(0);
        let s_iri = s.get::<String>("iri").unwrap_or_default();
        let o_iri = o.get::<String>("iri").unwrap_or_default();
        let p = r
            .get::<String>("propertyIri")
            .unwrap_or_else(|_| format!("mindreader:property/{}", r.typ()));
        let layer = r.get::<String>("layer").unwrap_or_else(|_| "global".into());
        let key = format!("{s_iri}|{p}|{o_iri}|{layer}");
        if !seen_facts.insert(key) {
            continue;
        }
        let mut score = 1.0f64;
        if let Some(sc) = node_scores.get(&s_iri) {
            score = score.max(*sc);
        }
        if let Some(sc) = node_scores.get(&o_iri) {
            score = score.max(*sc);
        }
        if let Some(sc) = rel_scores.get(&rid) {
            score = score.max(*sc);
        }
        element_iris.insert(s_iri.clone());
        let o_labels: Vec<String> = o
            .labels()
            .into_iter()
            .filter(|l| *l != "Entity")
            .map(|s| s.to_string())
            .collect();
        if o_labels.iter().any(|l| l == "Element") {
            element_iris.insert(o_iri.clone());
        }
        facts.push(WakeFact {
            s: endpoint_json(&s),
            s_iri,
            s_labels: s
                .labels()
                .into_iter()
                .filter(|l| *l != "Entity")
                .map(|s| s.to_string())
                .collect(),
            p,
            o: endpoint_json(&o),
            layer,
            score,
            spike: None,
        });
    }

    let about_iris: Vec<String> = element_iris.iter().cloned().collect();
    let mut spike_by_about: HashMap<String, (String, Value)> = HashMap::new();
    let mut spike_list: Vec<Value> = Vec::new();
    let mut seen_spike = HashSet::new();

    if !about_iris.is_empty() {
        let sp_rows = fetch_all(
            graph,
            query(
                r#"
                MATCH (sp:Entity)-[a:ABOUT]->(el:Entity)
                WHERE a.validTo IS NULL
                  AND a.layer IN $layers
                  AND el.iri IN $iris
                  AND (sp:Knowledge OR sp:Insight OR sp:Pattern OR sp:Signal)
                RETURN sp, el.iri AS about
                "#,
            )
            .param("layers", layers.clone())
            .param("iris", about_iris),
        )
        .await?;
        for row in sp_rows {
            let sp: Node = match row.get("sp") {
                Ok(n) => n,
                Err(_) => continue,
            };
            let about: String = match row.get("about") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let labels: Vec<String> = sp
                .labels()
                .into_iter()
                .filter(|l| *l != "Entity")
                .map(|s| s.to_string())
                .collect();
            let Some(rank) = spike_label(&labels) else {
                continue;
            };
            let node = node_json(&sp);
            let sp_iri = sp.get::<String>("iri").unwrap_or_default();
            let key = format!("{sp_iri}|{about}");
            if seen_spike.insert(key) {
                spike_list.push(json!({
                    "node": node.clone(),
                    "about": about,
                    "rank": rank,
                }));
            }
            let better = match spike_by_about.get(&about) {
                None => true,
                Some((cur, _)) => spike_rank(Some(&rank)) > spike_rank(Some(cur)),
            };
            if better {
                spike_by_about.insert(about, (rank, node));
            }
        }
    }

    for fact in &mut facts {
        if let Some(own) = spike_label(&fact.s_labels) {
            fact.spike = Some(own);
        } else if let Some((rank, _)) = spike_by_about.get(&fact.s_iri) {
            fact.spike = Some(rank.clone());
        }
    }

    facts.sort_by(|a, b| {
        spike_rank(b.spike.as_deref())
            .cmp(&spike_rank(a.spike.as_deref()))
            .then_with(|| {
                b.score
                    .partial_cmp(&a.score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.s_iri.cmp(&b.s_iri))
            .then_with(|| a.p.cmp(&b.p))
            .then_with(|| a.layer.cmp(&b.layer))
            .then_with(|| {
                a.o.get("iri")
                    .and_then(Value::as_str)
                    .cmp(&b.o.get("iri").and_then(Value::as_str))
            })
    });
    facts.truncate(limit as usize);

    spike_list.sort_by(|a, b| {
        let ra = a.get("rank").and_then(|v| v.as_str());
        let rb = b.get("rank").and_then(|v| v.as_str());
        spike_rank(rb)
            .cmp(&spike_rank(ra))
            .then_with(|| {
                a.get("about")
                    .and_then(Value::as_str)
                    .cmp(&b.get("about").and_then(Value::as_str))
            })
            .then_with(|| {
                a.pointer("/node/iri")
                    .and_then(Value::as_str)
                    .cmp(&b.pointer("/node/iri").and_then(Value::as_str))
            })
    });

    let facts_json: Vec<Value> = facts
        .into_iter()
        .map(|f| {
            json!({
                "s": f.s,
                "p": f.p,
                "o": f.o,
                "layer": f.layer,
                "spike": f.spike,
                "score": f.score,
            })
        })
        .collect();

    Ok(json!({
        "query": if trimmed.is_empty() { Value::Null } else { json!(trimmed) },
        "mode": "wakeup",
        "facts": facts_json,
        "spike": spike_list,
        "layers": layers,
    }))
}

pub async fn memory_stats(graph: &Graph, project: &str, _args: StatsArgs) -> Result<Value> {
    let layers = visible_layers(project);
    let row = fetch_one(
        graph,
        query(
            r#"
            MATCH (n:Entity)
            WITH count(n) AS nodes
            CALL {
              MATCH ()-[r]->()
              WHERE r.validTo IS NULL AND r.layer IN $layers
              RETURN count(r) AS activeEdges
            }
            CALL {
              MATCH ()-[r]->()
              WHERE r.validTo IS NOT NULL AND r.layer IN $layers
              RETURN count(r) AS historicalEdges
            }
            CALL {
              MATCH (e:Entity:Episode)
              RETURN count(e) AS episodes
            }
            RETURN nodes, activeEdges, historicalEdges, episodes
            "#,
        )
        .param("layers", layers.clone()),
    )
    .await?;
    let (nodes, active_edges, historical_edges, episodes) = match row {
        Some(r) => (
            r.get::<i64>("nodes").unwrap_or(0),
            r.get::<i64>("activeEdges").unwrap_or(0),
            r.get::<i64>("historicalEdges").unwrap_or(0),
            r.get::<i64>("episodes").unwrap_or(0),
        ),
        None => (0, 0, 0, 0),
    };

    let layer_rows = fetch_all(
        graph,
        query(
            r#"
            MATCH ()-[r]->()
            WHERE r.validTo IS NULL AND r.layer IN $layers
            RETURN r.layer AS layer, count(r) AS count
            ORDER BY count DESC, layer ASC
            "#,
        )
        .param("layers", layers.clone()),
    )
    .await?;
    let by_layer = layer_rows
        .into_iter()
        .map(|r| {
            json!({
                "layer": r.get::<String>("layer").unwrap_or_default(),
                "count": r.get::<i64>("count").unwrap_or(0),
            })
        })
        .collect::<Vec<_>>();

    let marker = fetch_one(
        graph,
        query("MATCH (m:MindreaderMeta {key: $key}) RETURN m.version AS version")
            .param("key", MODEL_MARKER_KEY),
    )
    .await?;
    let marker_version = marker.and_then(|row| row.get::<i64>("version").ok());
    let index_rows = fetch_all(
        graph,
        query(
            "SHOW INDEXES YIELD name, state \
             WHERE name IN ['wakeup_nodes', 'wakeup_facts'] \
             RETURN name, state",
        ),
    )
    .await?;
    let indexes_online = ["wakeup_nodes", "wakeup_facts"].iter().all(|required| {
        index_rows.iter().any(|row| {
            row.get::<String>("name").ok().as_deref() == Some(*required)
                && row.get::<String>("state").ok().as_deref() == Some("ONLINE")
        })
    });
    let constraint_rows = fetch_all(
        graph,
        query(
            "SHOW CONSTRAINTS YIELD name \
             WHERE name IN ['mindreader_meta_key', 'entity_iri', 'fact_lock_key'] \
             RETURN name",
        ),
    )
    .await?;
    let constraints_present = ["mindreader_meta_key", "entity_iri", "fact_lock_key"]
        .iter()
        .all(|required| {
            constraint_rows
                .iter()
                .any(|row| row.get::<String>("name").ok().as_deref() == Some(*required))
        });
    let model_ready =
        marker_version == Some(MODEL_VERSION) && indexes_online && constraints_present;

    Ok(json!({
        "project": project,
        "layers": layers,
        "model": {
            "marker": MODEL_MARKER_KEY,
            "version": marker_version,
            "requiredVersion": MODEL_VERSION,
            "indexesOnline": indexes_online,
            "constraintsPresent": constraints_present,
            "ready": model_ready
        },
        "counts": {
            "nodes": nodes,
            "activeEdges": active_edges,
            "historicalEdges": historical_edges,
            "episodes": episodes
        },
        "activeEdgesByLayer": by_layer
    }))
}

struct WakeFact {
    s: Value,
    s_iri: String,
    s_labels: Vec<String>,
    p: String,
    o: Value,
    layer: String,
    score: f64,
    spike: Option<String>,
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

pub async fn memory_traverse(graph: &Graph, project: &str, args: TraverseArgs) -> Result<Value> {
    let depth = args.depth.unwrap_or(1).clamp(1, 3);
    let limit = args.limit.unwrap_or(50).clamp(1, 200) as i64;
    let layers = visible_layers(project);
    let rels: Vec<String> = if let Some(rs) = args.rels.filter(|r| !r.is_empty()) {
        rs.into_iter()
            .map(|relationship| {
                safe_rel(&relationship).map_err(|_| {
                    DomainError::InvalidInput(format!(
                        "invalid relationship type: {relationship:?}"
                    ))
                    .into()
                })
            })
            .collect::<Result<Vec<_>>>()?
    } else {
        FIXED_RELS.iter().map(|s| (*s).to_string()).collect()
    };

    if get_node(graph, &args.from).await?.is_none() {
        return Ok(json!({
            "found": false,
            "from": args.from,
            "paths": [],
            "nodes": [],
            "edges": [],
        }));
    }

    let q = format!(
        r#"
        MATCH (start:Entity {{iri: $from}})
        MATCH path = (start)-[rels*1..{depth}]-(x)
        WHERE all(r IN relationships(path) WHERE
          type(r) IN $rels
          AND r.validTo IS NULL
          AND (r.layer IN $layers)
        )
        RETURN path
        LIMIT $limit
        "#
    );
    let rows = fetch_all(
        graph,
        query(&q)
            .param("from", args.from.clone())
            .param("rels", rels.clone())
            .param("layers", layers.clone())
            .param("limit", limit),
    )
    .await?;

    let mut nodes_by_iri = serde_json::Map::new();
    let mut edges = Vec::new();
    let mut edge_seen = std::collections::HashSet::new();
    let mut paths = Vec::new();

    for row in rows {
        let path: Path = match row.get("path") {
            Ok(p) => p,
            Err(_) => continue,
        };
        let (pnodes, pedges, iris) = path_to_json(&path);
        for n in pnodes {
            if let Some(iri) = n.get("iri").and_then(|v| v.as_str()) {
                nodes_by_iri.insert(iri.to_string(), n);
            }
        }
        for e in &pedges {
            let key = format!(
                "{}:{}:{}:{}",
                e.get("type").and_then(|v| v.as_str()).unwrap_or(""),
                e.get("from").and_then(|v| v.as_str()).unwrap_or(""),
                e.get("to").and_then(|v| v.as_str()).unwrap_or(""),
                e.get("episodeId").and_then(|v| v.as_str()).unwrap_or("")
            );
            if edge_seen.insert(key) {
                edges.push(e.clone());
            }
        }
        paths.push(json!({ "nodes": iris, "edges": pedges }));
    }

    Ok(json!({
        "found": true,
        "from": args.from,
        "depth": depth,
        "layers": layers,
        "rels": rels,
        "paths": paths,
        "nodes": nodes_by_iri.values().cloned().collect::<Vec<_>>(),
        "edges": edges,
    }))
}

pub async fn memory_assert(graph: &Graph, project: &str, args: AssertArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_assert_once(graph, project, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

async fn memory_assert_once(graph: &Graph, project: &str, args: AssertArgs) -> Result<Value> {
    let predicate = PredicateRef::parse(&args.p)?;
    reject_system_owned_predicate(predicate.iri())?;
    let layer = args
        .layer
        .clone()
        .unwrap_or_else(|| default_write_layer(project));
    assert_writable_layer(&layer, project)?;

    let spike = SpikeRank::parse(args.spike)?.map(|rank| rank.as_str().to_string());

    let mut s_spec = node_spec(EntityRef::from_input(args.s)?);
    if let Some(sp) = &spike {
        if !s_spec.labels.iter().any(|l| l == sp) {
            s_spec.labels.push(sp.clone());
        }
    }
    let s_kind = if let Some(sp) = &spike {
        sp.to_ascii_lowercase()
    } else {
        "element".into()
    };
    let extra: Vec<String> = spike.clone().into_iter().collect();
    let subject_iri = s_spec.iri.clone().unwrap_or_else(|| {
        EntityRef {
            iri: None,
            name: s_spec.name.clone(),
            labels: s_spec.labels.clone(),
        }
        .resolved_iri(&s_kind)
    });
    let object_value = ObjectValue::from_input(args.o)?;
    let prop_iri = predicate.iri().to_string();
    let structural = structural_rel_for(&prop_iri);
    let visible = visible_layers(project);
    let mut txn = graph.start_txn().await?;
    let write = async {
        let mut locks = visible
            .iter()
            .map(|visible_layer| (subject_iri.clone(), prop_iri.clone(), visible_layer.clone()))
            .collect::<Vec<_>>();
        if spike.is_some() {
            locks.push((
                subject_iri.clone(),
                "mindreader:property/ABOUT".into(),
                layer.clone(),
            ));
        }
        if args.contradicts {
            locks.push((
                object_value.resolved_iri(),
                "mindreader:property/CONTRADICTS".into(),
                GLOBAL_LAYER.into(),
            ));
        }
        acquire_fact_locks_in_txn(&mut txn, &locks).await?;

        let subject = merge_node_in_txn(&mut txn, &s_spec, &s_kind, &extra).await?;
        let (object, o_is_literal) = merge_object_in_txn(&mut txn, object_value).await?;
        let (_, minted_stub, prop_json) = ensure_property_in_txn(&mut txn, &prop_iri).await?;
        let conflicts = find_conflicts_txn(
            &mut txn,
            &subject.iri,
            &prop_iri,
            structural.as_deref(),
            &layer,
            &object.iri,
            &visible,
        )
        .await?;
        let already_current = find_current_pair_txn(
            &mut txn,
            &subject.iri,
            &prop_iri,
            structural.as_deref(),
            &layer,
            &object.iri,
        )
        .await?
        .is_some();
        let need_about = spike.is_some()
            && !o_is_literal
            && object.labels.iter().any(|label| label == "Element")
            && structural.as_deref() != Some("ABOUT")
            && find_current_pair_txn(
                &mut txn,
                &subject.iri,
                "mindreader:property/ABOUT",
                Some("ABOUT"),
                &layer,
                &object.iri,
            )
            .await?
            .is_none();
        let missing_contradictions = if args.contradicts {
            missing_contradictions_txn(&mut txn, &object.iri, &conflicts).await?
        } else {
            Vec::new()
        };
        if already_current && !need_about && missing_contradictions.is_empty() {
            return Ok::<_, anyhow::Error>((
                None,
                subject,
                object,
                minted_stub,
                prop_json,
                conflicts,
            ));
        }

        let episode = create_episode_in_txn(&mut txn, "memory_assert", None).await?;
        if !already_current {
            let ft = fact_text(&subject.name, &subject.iri, &prop_iri, &object);
            let fact_write = FactWrite {
                s: &subject.iri,
                o: &object.iri,
                prop_iri: &prop_iri,
                layer: &layer,
                episode: &episode,
                reason: None,
                fact_text: &ft,
            };
            if let Some(rel_type) = &structural {
                create_structural_txn(&mut txn, rel_type, &fact_write).await?;
            } else {
                create_asserts_txn(&mut txn, &fact_write).await?;
            }
        }
        if need_about {
            let about_ft = fact_text(
                &subject.name,
                &subject.iri,
                "mindreader:property/ABOUT",
                &object,
            );
            create_structural_txn(
                &mut txn,
                "ABOUT",
                &FactWrite {
                    s: &subject.iri,
                    o: &object.iri,
                    prop_iri: "mindreader:property/ABOUT",
                    layer: &layer,
                    episode: &episode,
                    reason: None,
                    fact_text: &about_ft,
                },
            )
            .await?;
        }
        write_contradicts_txn(
            &mut txn,
            &object.iri,
            &missing_contradictions,
            &layer,
            &episode,
        )
        .await?;
        Ok::<_, anyhow::Error>((
            Some(episode),
            subject,
            object,
            minted_stub,
            prop_json,
            conflicts,
        ))
    }
    .await;
    let (episode, subject, object, minted_stub, prop_json, conflicts) = match write {
        Ok(v) => {
            if v.0.is_some() {
                txn.commit()
                    .await
                    .map_err(|error| anyhow!("commit memory_assert transaction failed: {error}"))?;
            } else {
                txn.rollback().await?;
            }
            v
        }
        Err(e) => {
            let _ = txn.rollback().await;
            return Err(e);
        }
    };

    Ok(json!({
        "noop": episode.is_none(),
        "s": subject.json,
        "p": prop_iri,
        "o": object.json,
        "layer": layer,
        "episode": episode.map(|episode| json!({ "iri": episode.iri, "at": episode.at, "tool": episode.tool })).unwrap_or(Value::Null),
        "propertyStub": minted_stub,
        "property": prop_json,
        "spike": spike,
        "conflicts": conflicts,
    }))
}

pub async fn memory_replace(graph: &Graph, project: &str, args: ReplaceArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_replace_once(graph, project, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

async fn memory_replace_once(graph: &Graph, project: &str, args: ReplaceArgs) -> Result<Value> {
    let predicate = PredicateRef::parse(&args.p)?;
    reject_system_owned_predicate(predicate.iri())?;
    let layer = args
        .layer
        .clone()
        .unwrap_or_else(|| default_write_layer(project));
    assert_writable_layer(&layer, project)?;
    let spike = SpikeRank::parse(args.spike)?.map(|rank| rank.as_str().to_string());

    let mut subject_spec = node_spec(EntityRef::from_input(args.s)?);
    if let Some(spike) = &spike {
        if !subject_spec.labels.iter().any(|label| label == spike) {
            subject_spec.labels.push(spike.clone());
        }
    }
    let subject_kind = spike
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "element".into());
    let subject_iri = EntityRef {
        iri: subject_spec.iri.clone(),
        name: subject_spec.name.clone(),
        labels: subject_spec.labels.clone(),
    }
    .resolved_iri(&subject_kind);
    let old_value = ObjectValue::from_input(args.old)?;
    let old_iri = old_value.resolved_iri();
    let new_value = ObjectValue::from_input(args.new)?;
    let new_iri = new_value.resolved_iri();
    let prop_iri = predicate.iri().to_string();
    let structural = structural_rel_for(&prop_iri);
    let visible = visible_layers(project);
    let mut txn = graph.start_txn().await?;
    let write = async {
        let mut locks = visible
            .iter()
            .map(|visible_layer| (subject_iri.clone(), prop_iri.clone(), visible_layer.clone()))
            .collect::<Vec<_>>();
        if args.contradicts {
            locks.push((
                new_iri.clone(),
                "mindreader:property/CONTRADICTS".into(),
                GLOBAL_LAYER.into(),
            ));
        }
        acquire_fact_locks_in_txn(&mut txn, &locks).await?;
        let old_currents = find_current_pairs_txn(
            &mut txn,
            &subject_iri,
            &prop_iri,
            structural.as_deref(),
            &layer,
            &old_iri,
        )
        .await?;
        if old_currents.is_empty() {
            return Err(DomainError::Precondition(format!(
                "cannot replace non-current fact ({subject_iri}, {prop_iri}, {old_iri}, {layer})"
            ))
            .into());
        }
        if old_iri == new_iri {
            let old_node = fetch_one_txn(
                &mut txn,
                query("MATCH (n:Entity {iri: $iri}) RETURN n").param("iri", old_iri.clone()),
            )
            .await?
            .ok_or_else(|| anyhow!("current fact points to missing object {old_iri}"))?;
            let old_node: Node = old_node.get("n")?;
            let subject_node = fetch_one_txn(
                &mut txn,
                query("MATCH (n:Entity {iri: $iri}) RETURN n").param("iri", subject_iri.clone()),
            )
            .await?
            .ok_or_else(|| anyhow!("current fact has missing subject {subject_iri}"))?;
            let subject_node: Node = subject_node.get("n")?;
            return Ok::<_, anyhow::Error>((
                None,
                node_json(&subject_node),
                node_json(&old_node),
                false,
                Value::Null,
                Vec::new(),
            ));
        }

        let subject = merge_node_in_txn(
            &mut txn,
            &subject_spec,
            &subject_kind,
            &spike.clone().into_iter().collect::<Vec<_>>(),
        )
        .await?;
        let (new_object, _) = merge_object_in_txn(&mut txn, new_value).await?;
        let (_, minted_stub, prop_json) = ensure_property_in_txn(&mut txn, &prop_iri).await?;
        let new_already_current = find_current_pair_txn(
            &mut txn,
            &subject.iri,
            &prop_iri,
            structural.as_deref(),
            &layer,
            &new_object.iri,
        )
        .await?
        .is_some();
        let conflicts = find_conflicts_txn(
            &mut txn,
            &subject.iri,
            &prop_iri,
            structural.as_deref(),
            &layer,
            &new_object.iri,
            &visible,
        )
        .await?;
        let missing_contradictions = if args.contradicts {
            missing_contradictions_txn(&mut txn, &new_object.iri, &conflicts).await?
        } else {
            Vec::new()
        };
        let new_fact_text = fact_text(&subject.name, &subject.iri, &prop_iri, &new_object);
        let episode =
            create_episode_in_txn(&mut txn, "memory_replace", args.reason.as_deref()).await?;
        for old_current in old_currents {
            close_rel_txn_with_reason(
                &mut txn,
                old_current.rel_id,
                Some(&episode.iri),
                args.reason.as_deref(),
            )
            .await?;
        }
        if !new_already_current {
            let fact_write = FactWrite {
                s: &subject.iri,
                o: &new_object.iri,
                prop_iri: &prop_iri,
                layer: &layer,
                episode: &episode,
                reason: args.reason.as_deref(),
                fact_text: &new_fact_text,
            };
            if let Some(rel_type) = &structural {
                create_structural_txn(&mut txn, rel_type, &fact_write).await?;
            } else {
                create_asserts_txn(&mut txn, &fact_write).await?;
            }
        }
        create_supersedes_txn(
            &mut txn,
            &new_object.iri,
            &old_iri,
            &prop_iri,
            &layer,
            &episode,
        )
        .await?;
        write_contradicts_txn(
            &mut txn,
            &new_object.iri,
            &missing_contradictions,
            &layer,
            &episode,
        )
        .await?;
        Ok::<_, anyhow::Error>((
            Some(episode),
            subject.json,
            new_object.json,
            minted_stub,
            prop_json,
            conflicts,
        ))
    }
    .await;
    let (episode, subject_json, new_json, minted_stub, prop_json, conflicts) = match write {
        Ok(value) => {
            if value.0.is_some() {
                txn.commit().await.map_err(|error| {
                    anyhow!("commit memory_replace transaction failed: {error}")
                })?;
            } else {
                txn.rollback().await?;
            }
            value
        }
        Err(error) => {
            let _ = txn.rollback().await;
            return Err(error);
        }
    };

    Ok(json!({
        "noop": episode.is_none(),
        "s": subject_json,
        "p": prop_iri,
        "old": old_iri,
        "new": new_json,
        "layer": layer,
        "superseded": {
            "from": old_iri,
            "to": new_iri,
            "propertyIri": prop_iri,
        },
        "episode": episode.map(|episode| json!({ "iri": episode.iri, "at": episode.at, "tool": episode.tool })).unwrap_or(Value::Null),
        "propertyStub": minted_stub,
        "property": prop_json,
        "spike": spike,
        "conflicts": conflicts,
    }))
}

struct CurrentFact {
    rel_id: i64,
}

struct FactWrite<'a> {
    s: &'a str,
    o: &'a str,
    prop_iri: &'a str,
    layer: &'a str,
    episode: &'a Episode,
    reason: Option<&'a str>,
    fact_text: &'a str,
}

async fn merge_object_in_txn(
    txn: &mut Txn,
    value: ObjectValue,
) -> Result<(crate::graph::MergedNode, bool)> {
    match value {
        ObjectValue::Literal { value, datatype } => {
            Ok((merge_literal_in_txn(txn, &value, &datatype).await?, true))
        }
        ObjectValue::Entity(entity) => Ok((
            merge_node_in_txn(txn, &node_spec(entity), "element", &[]).await?,
            false,
        )),
    }
}

fn close_rel_query(rel_id: i64, episode_id: Option<&str>) -> neo4rs::Query {
    query(
        r#"
        MATCH ()-[r]->()
        WHERE id(r) = $rid AND r.validTo IS NULL
        SET r.validTo = datetime(), r.retractedBy = $episode,
            r.reason = coalesce($reason, r.reason)
        "#,
    )
    .param("rid", rel_id)
    .param("episode", episode_id.map(|s| s.to_string()))
    .param("reason", Option::<String>::None)
}

fn asserts_query(write: &FactWrite<'_>) -> neo4rs::Query {
    query(
        r#"
        MATCH (s:Entity {iri: $s}), (o:Entity {iri: $o})
        CREATE (s)-[r:ASSERTS {
            propertyIri: $p,
            layer: $layer,
            validFrom: datetime(),
            episodeId: $episode,
            factText: $factText
        }]->(o)
        SET r.reason = $reason
        "#,
    )
    .param("s", write.s.to_string())
    .param("o", write.o.to_string())
    .param("p", write.prop_iri.to_string())
    .param("layer", write.layer.to_string())
    .param("episode", write.episode.iri.clone())
    .param("reason", write.reason.map(|s| s.to_string()))
    .param("factText", write.fact_text.to_string())
}

fn structural_query(rel_type: &str, write: &FactWrite<'_>) -> Result<neo4rs::Query> {
    let rel = safe_rel(rel_type)?;
    let q = format!(
        r#"
        MATCH (s:Entity {{iri: $s}}), (o:Entity {{iri: $o}})
        CREATE (s)-[r:{rel} {{
            propertyIri: $p,
            layer: $layer,
            validFrom: datetime(),
            episodeId: $episode,
            factText: $factText
        }}]->(o)
        SET r.reason = $reason
        "#
    );
    Ok(query(&q)
        .param("s", write.s.to_string())
        .param("o", write.o.to_string())
        .param("p", write.prop_iri.to_string())
        .param("layer", write.layer.to_string())
        .param("episode", write.episode.iri.clone())
        .param("reason", write.reason.map(|s| s.to_string()))
        .param("factText", write.fact_text.to_string()))
}

fn supersedes_query(
    new_o: &str,
    old_o: &str,
    prop_iri: &str,
    layer: &str,
    episode: &Episode,
) -> neo4rs::Query {
    query(
        r#"
        MATCH (n:Entity {iri: $new}), (old:Entity {iri: $old})
        CREATE (n)-[r:SUPERSEDES {
            propertyIri: $p,
            layer: $layer,
            validFrom: datetime(),
            episodeId: $episode
        }]->(old)
        "#,
    )
    .param("new", new_o.to_string())
    .param("old", old_o.to_string())
    .param("p", prop_iri.to_string())
    .param("layer", layer.to_string())
    .param("episode", episode.iri.clone())
}

async fn close_rel_txn_with_reason(
    txn: &mut Txn,
    rel_id: i64,
    episode_id: Option<&str>,
    reason: Option<&str>,
) -> Result<()> {
    txn.run(close_rel_query(rel_id, episode_id).param("reason", reason.map(str::to_string)))
        .await?;
    Ok(())
}

async fn create_asserts_txn(txn: &mut Txn, write: &FactWrite<'_>) -> Result<()> {
    txn.run(asserts_query(write)).await?;
    Ok(())
}

async fn create_structural_txn(txn: &mut Txn, rel_type: &str, write: &FactWrite<'_>) -> Result<()> {
    txn.run(structural_query(rel_type, write)?).await?;
    Ok(())
}

async fn create_supersedes_txn(
    txn: &mut Txn,
    new_o: &str,
    old_o: &str,
    prop_iri: &str,
    layer: &str,
    episode: &Episode,
) -> Result<()> {
    txn.run(supersedes_query(new_o, old_o, prop_iri, layer, episode))
        .await?;
    Ok(())
}

pub async fn count_current_asserts(
    graph: &Graph,
    s: &str,
    p: &str,
    layer: &str,
) -> Result<(i64, Vec<String>)> {
    let prop = property_iri(p);
    let structural = structural_rel_for(&prop);
    let row = if let Some(rel) = structural {
        let rel = safe_rel(&rel)?;
        let q = format!(
            r#"
            MATCH (s:Entity {{iri: $s}})-[r:{rel}]->(o:Entity)
            WHERE r.validTo IS NULL AND r.layer = $layer
            RETURN count(r) AS n, collect(o.iri) AS objects
            "#
        );
        fetch_one(
            graph,
            query(&q)
                .param("s", s.to_string())
                .param("layer", layer.to_string()),
        )
        .await?
    } else {
        fetch_one(
            graph,
            query(
                r#"
                MATCH (s:Entity {iri: $s})-[r:ASSERTS]->(o:Entity)
                WHERE r.validTo IS NULL AND r.layer = $layer AND r.propertyIri = $p
                RETURN count(r) AS n, collect(o.iri) AS objects
                "#,
            )
            .param("s", s.to_string())
            .param("layer", layer.to_string())
            .param("p", prop),
        )
        .await?
    };
    let Some(row) = row else {
        return Ok((0, vec![]));
    };
    let n: i64 = row.get("n").unwrap_or(0);
    let objects: Vec<String> = row.get("objects").unwrap_or_default();
    Ok((n, objects))
}

pub async fn count_historical_asserts(graph: &Graph, s: &str, p: &str, layer: &str) -> Result<i64> {
    let prop = property_iri(p);
    let row = fetch_one(
        graph,
        query(
            r#"
            MATCH (s:Entity {iri: $s})-[r:ASSERTS]->(o:Entity)
            WHERE r.validTo IS NOT NULL AND r.layer = $layer AND r.propertyIri = $p
            RETURN count(r) AS n
            "#,
        )
        .param("s", s.to_string())
        .param("layer", layer.to_string())
        .param("p", prop),
    )
    .await?;
    Ok(row.and_then(|r| r.get::<i64>("n").ok()).unwrap_or(0))
}

pub async fn memory_retract(graph: &Graph, project: &str, args: RetractArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_retract_once(graph, project, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

async fn memory_retract_once(graph: &Graph, project: &str, args: RetractArgs) -> Result<Value> {
    let layer = args
        .layer
        .clone()
        .unwrap_or_else(|| default_write_layer(project));
    assert_writable_layer(&layer, project)?;
    let scope = RetractScope::parse(&args.target.kind)?;
    let subject = EntityRef::from_input(args.target.s)?;
    let subject_iri = subject.resolved_iri("element");
    let predicate = args
        .target
        .p
        .map(PredicateRef::parse)
        .transpose()?
        .map(|predicate| predicate.iri().to_string());
    if let Some(predicate) = &predicate {
        reject_system_owned_predicate(predicate)?;
    }
    let object_iri = args
        .target
        .o
        .map(ObjectValue::from_input)
        .transpose()?
        .map(|object| object.resolved_iri());

    match scope {
        RetractScope::Fact if predicate.is_none() || object_iri.is_none() => {
            return Err(DomainError::InvalidInput(
                "fact retraction requires target.p and target.o".into(),
            )
            .into())
        }
        RetractScope::Predicate if predicate.is_none() || object_iri.is_some() => {
            return Err(DomainError::InvalidInput(
                "predicate retraction requires target.p and forbids target.o".into(),
            )
            .into())
        }
        RetractScope::Subject if predicate.is_some() || object_iri.is_some() => {
            return Err(DomainError::InvalidInput(
                "subject retraction forbids target.p and target.o".into(),
            )
            .into())
        }
        _ => {}
    }

    let protected: Vec<String> = SCHEMA_STRUCTURAL_RELS
        .iter()
        .chain(SYSTEM_OWNED_RELS)
        .map(|value| (*value).to_string())
        .collect();
    let mut txn = graph.start_txn().await?;
    let lock_predicate = predicate.clone().unwrap_or_else(|| "*".into());
    acquire_fact_locks_in_txn(
        &mut txn,
        &[(subject_iri.clone(), lock_predicate, layer.clone())],
    )
    .await?;
    let rows = match scope {
        RetractScope::Subject => {
            fetch_all_txn(
                &mut txn,
                query(
                    r#"
                    MATCH (s:Entity {iri: $s})-[r]->(o:Entity)
                    WHERE r.validTo IS NULL
                      AND r.layer = $layer
                      AND NOT type(r) IN $protected
                      AND NOT s:Class AND NOT s:Property
                      AND NOT o:Class AND NOT o:Property
                    RETURN id(r) AS rid
                    "#,
                )
                .param("s", subject_iri.clone())
                .param("layer", layer.clone())
                .param("protected", protected.clone()),
            )
            .await?
        }
        RetractScope::Fact | RetractScope::Predicate => {
            let predicate = predicate.as_deref().expect("validated predicate");
            if let Some(rel_type) = structural_rel_for(predicate) {
                let rel_type = safe_rel(&rel_type)?;
                let object_clause = if object_iri.is_some() {
                    " {iri: $o}"
                } else {
                    ""
                };
                let cypher = format!(
                    "MATCH (s:Entity {{iri: $s}})-[r:{rel_type}]->(o:Entity{object_clause}) \
                     WHERE r.validTo IS NULL AND r.layer = $layer RETURN id(r) AS rid"
                );
                let mut query = query(&cypher)
                    .param("s", subject_iri.clone())
                    .param("layer", layer.clone());
                if let Some(object) = &object_iri {
                    query = query.param("o", object.clone());
                }
                fetch_all_txn(&mut txn, query).await?
            } else {
                let object_clause = if object_iri.is_some() {
                    "AND o.iri = $o"
                } else {
                    ""
                };
                let cypher = format!(
                    "MATCH (s:Entity {{iri: $s}})-[r:ASSERTS]->(o:Entity) \
                     WHERE r.validTo IS NULL AND r.layer = $layer AND r.propertyIri = $p \
                     {object_clause} RETURN id(r) AS rid"
                );
                let mut query = query(&cypher)
                    .param("s", subject_iri.clone())
                    .param("p", predicate.to_string())
                    .param("layer", layer.clone());
                if let Some(object) = &object_iri {
                    query = query.param("o", object.clone());
                }
                fetch_all_txn(&mut txn, query).await?
            }
        }
    };
    let relationship_ids = rows
        .into_iter()
        .map(|row| row.get::<i64>("rid"))
        .collect::<std::result::Result<Vec<_>, _>>()?;

    if relationship_ids.is_empty() {
        txn.rollback().await?;
        return Ok(json!({
            "retracted": 0,
            "soft": true,
            "layer": layer,
            "episode": Value::Null,
            "reason": args.reason,
        }));
    }

    let episode = create_episode_in_txn(&mut txn, "memory_retract", args.reason.as_deref()).await?;
    for relationship_id in &relationship_ids {
        txn.run(
            close_rel_query(*relationship_id, Some(&episode.iri))
                .param("reason", args.reason.clone()),
        )
        .await?;
    }
    txn.commit()
        .await
        .map_err(|error| anyhow!("commit memory_retract transaction failed: {error}"))?;

    Ok(json!({
        "retracted": relationship_ids.len(),
        "soft": true,
        "layer": layer,
        "episode": { "iri": episode.iri, "at": episode.at, "tool": episode.tool },
        "reason": args.reason,
    }))
}

pub async fn memory_schema(graph: &Graph, project: &str, args: SchemaArgs) -> Result<Value> {
    for attempt in 0..3_u64 {
        match memory_schema_once(graph, project, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}

async fn memory_schema_once(graph: &Graph, project: &str, args: SchemaArgs) -> Result<Value> {
    let kind = args.kind.trim().to_ascii_lowercase();
    if kind != "class" && kind != "property" {
        return Err(DomainError::InvalidInput("kind must be class or property".into()).into());
    }
    let seed = args
        .iri
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .or(args.name.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            DomainError::InvalidInput("memory_schema requires a nonempty name or iri".into())
        })?;
    let iri = if kind == "class" {
        class_iri(seed)
    } else {
        property_iri(seed)
    };
    let name = args
        .name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| name_from_iri(&iri));
    if kind == "class"
        && (args.sub_property_of.is_some() || args.domain.is_some() || args.range.is_some())
    {
        return Err(DomainError::InvalidInput(
            "class schema declarations only accept subClassOf".into(),
        )
        .into());
    }
    if kind == "property" && args.sub_class_of.is_some() {
        return Err(DomainError::InvalidInput(
            "property schema declarations do not accept subClassOf".into(),
        )
        .into());
    }
    for (field, value) in [
        ("subClassOf", args.sub_class_of.as_deref()),
        ("subPropertyOf", args.sub_property_of.as_deref()),
        ("domain", args.domain.as_deref()),
        ("range", args.range.as_deref()),
    ] {
        if value.is_some_and(|value| value.trim().is_empty()) {
            return Err(DomainError::InvalidInput(format!(
                "memory_schema {field} cannot be empty"
            ))
            .into());
        }
    }

    let label = if kind == "class" { "Class" } else { "Property" };
    let subject_spec = NodeSpec {
        iri: Some(iri.clone()),
        name: Some(name.clone()),
        labels: vec![label.into()],
    };
    let layer = GLOBAL_LAYER.to_string();
    let _ = project;
    let mut definitions: Vec<(&str, &str, NodeSpec, &str)> = Vec::new();
    if kind == "class" {
        definitions.push((
            "INSTANCE_OF",
            "mindreader:property/INSTANCE_OF",
            NodeSpec {
                iri: Some("mindreader:class/Class".into()),
                name: Some("Class".into()),
                labels: vec!["Class".into()],
            },
            "class",
        ));
        if let Some(sup) = args.sub_class_of.as_deref() {
            let parent_iri = class_iri(sup);
            definitions.push((
                "SUBCLASS_OF",
                "mindreader:property/SUBCLASS_OF",
                NodeSpec {
                    iri: Some(parent_iri.clone()),
                    name: Some(name_from_iri(&parent_iri)),
                    labels: vec!["Class".into()],
                },
                "class",
            ));
        }
    } else {
        definitions.push((
            "INSTANCE_OF",
            "mindreader:property/INSTANCE_OF",
            NodeSpec {
                iri: Some("mindreader:class/Property".into()),
                name: Some("Property".into()),
                labels: vec!["Class".into()],
            },
            "class",
        ));
        if let Some(sup) = args.sub_property_of.as_deref() {
            let parent_iri = property_iri(sup);
            definitions.push((
                "SUBPROPERTY_OF",
                "mindreader:property/SUBPROPERTY_OF",
                NodeSpec {
                    iri: Some(parent_iri.clone()),
                    name: Some(name_from_iri(&parent_iri)),
                    labels: vec!["Property".into()],
                },
                "property",
            ));
        }
        if let Some(d) = args.domain.as_deref() {
            let class = class_iri(d);
            definitions.push((
                "DOMAIN",
                "mindreader:property/DOMAIN",
                NodeSpec {
                    iri: Some(class.clone()),
                    name: Some(name_from_iri(&class)),
                    labels: vec!["Class".into()],
                },
                "class",
            ));
        }
        if let Some(r) = args.range.as_deref() {
            let class = class_iri(r);
            definitions.push((
                "RANGE",
                "mindreader:property/RANGE",
                NodeSpec {
                    iri: Some(class.clone()),
                    name: Some(name_from_iri(&class)),
                    labels: vec!["Class".into()],
                },
                "class",
            ));
        }
    }

    let mut txn = graph.start_txn().await?;
    let write = async {
        let locks = definitions
            .iter()
            .map(|(_, property, _, _)| (iri.clone(), (*property).into(), layer.clone()))
            .collect::<Vec<_>>();
        acquire_fact_locks_in_txn(&mut txn, &locks).await?;
        let existing_ready = fetch_one_txn(
            &mut txn,
            query(
                "OPTIONAL MATCH (n:Entity {iri: $iri}) \
                 RETURN n IS NOT NULL AND $label IN labels(n) AND coalesce(n.stub, false) = false AS ready",
            )
            .param("iri", iri.clone())
            .param("label", label.to_string()),
        )
        .await?
        .and_then(|row| row.get::<bool>("ready").ok())
        .unwrap_or(false);
        let node = merge_node_in_txn(&mut txn, &subject_spec, &kind, &[]).await?;
        let mut resolved = Vec::new();
        let mut changed = !existing_ready || node.created;
        for (rel, property, target_spec, target_kind) in definitions {
            let target = merge_node_in_txn(&mut txn, &target_spec, target_kind, &[]).await?;
            let current = find_current_pair_txn(
                &mut txn,
                &node.iri,
                property,
                Some(rel),
                &layer,
                &target.iri,
            )
            .await?
            .is_some();
            changed |= target.created || !current;
            resolved.push((rel, property, target, current));
        }
        if !changed {
            return Ok::<_, anyhow::Error>((None, node.json, Vec::new()));
        }

        let episode = create_episode_in_txn(&mut txn, "memory_schema", None).await?;
        txn.run(
            query("MATCH (n:Entity {iri: $iri}) SET n.stub = false")
                .param("iri", node.iri.clone()),
        )
        .await?;
        let mut links = Vec::new();
        for (rel, property, target, current) in resolved {
            if !current {
                let fact_text = format!("{} {rel} {}", node.iri, target.iri);
                create_structural_txn(
                    &mut txn,
                    rel,
                    &FactWrite {
                        s: &node.iri,
                        o: &target.iri,
                        prop_iri: property,
                        layer: &layer,
                        episode: &episode,
                        reason: None,
                        fact_text: &fact_text,
                    },
                )
                .await?;
            }
            links.push(json!({"rel": rel, "to": target.iri}));
        }
        let refreshed = fetch_one_txn(
            &mut txn,
            query("MATCH (n:Entity {iri: $iri}) RETURN n").param("iri", node.iri),
        )
        .await?
        .ok_or_else(|| anyhow!("schema node disappeared inside transaction"))?;
        let refreshed: Node = refreshed.get("n")?;
        Ok::<_, anyhow::Error>((Some(episode), node_json(&refreshed), links))
    }
    .await;
    let (episode, node, links) = match write {
        Ok(value) => {
            if value.0.is_some() {
                txn.commit()
                    .await
                    .map_err(|error| anyhow!("commit memory_schema transaction failed: {error}"))?;
            } else {
                txn.rollback().await?;
            }
            value
        }
        Err(error) => {
            let _ = txn.rollback().await;
            return Err(error);
        }
    };

    Ok(json!({
        "kind": kind,
        "node": node,
        "links": links,
        "noop": episode.is_none(),
        "episode": episode.map(|episode| json!({ "iri": episode.iri, "at": episode.at, "tool": episode.tool })).unwrap_or(Value::Null),
    }))
}

async fn find_current_pairs_txn(
    txn: &mut Txn,
    s: &str,
    prop_iri: &str,
    structural: Option<&str>,
    layer: &str,
    o: &str,
) -> Result<Vec<CurrentFact>> {
    let rows = if let Some(rel) = structural {
        let rel = safe_rel(rel)?;
        let cypher = format!(
            "MATCH (s:Entity {{iri: $s}})-[r:{rel}]->(o:Entity {{iri: $o}}) \
             WHERE r.validTo IS NULL AND r.layer = $layer \
             RETURN id(r) AS rid, o.iri AS oiri"
        );
        fetch_all_txn(
            txn,
            query(&cypher)
                .param("s", s.to_string())
                .param("o", o.to_string())
                .param("layer", layer.to_string()),
        )
        .await?
    } else {
        fetch_all_txn(
            txn,
            query(
                "MATCH (s:Entity {iri: $s})-[r:ASSERTS]->(o:Entity {iri: $o}) \
                 WHERE r.validTo IS NULL AND r.layer = $layer AND r.propertyIri = $p \
                 RETURN id(r) AS rid, o.iri AS oiri",
            )
            .param("s", s.to_string())
            .param("o", o.to_string())
            .param("layer", layer.to_string())
            .param("p", prop_iri.to_string()),
        )
        .await?
    };
    rows.into_iter()
        .map(|row| {
            Ok(CurrentFact {
                rel_id: row.get("rid")?,
            })
        })
        .collect()
}

async fn find_current_pair_txn(
    txn: &mut Txn,
    s: &str,
    prop_iri: &str,
    structural: Option<&str>,
    layer: &str,
    o: &str,
) -> Result<Option<CurrentFact>> {
    Ok(
        find_current_pairs_txn(txn, s, prop_iri, structural, layer, o)
            .await?
            .into_iter()
            .next(),
    )
}

async fn find_conflicts_txn(
    txn: &mut Txn,
    s: &str,
    prop_iri: &str,
    structural: Option<&str>,
    write_layer: &str,
    o_iri: &str,
    layers: &[String],
) -> Result<Vec<Value>> {
    let rel_type = structural.unwrap_or("ASSERTS");
    let is_structural = structural.is_some();
    let rows = fetch_all_txn(
        txn,
        query(
            r#"
            MATCH (s:Entity {iri: $s})-[r]->(o:Entity)
            WHERE r.validTo IS NULL
              AND r.layer IN $layers
              AND r.layer <> $layer
              AND o.iri <> $o
              AND (
                ($isStructural AND type(r) = $relType)
                OR (NOT $isStructural AND type(r) = 'ASSERTS' AND r.propertyIri = $p)
              )
            RETURN r.layer AS layer, o, coalesce(r.propertyIri, $p) AS p
            "#,
        )
        .param("s", s.to_string())
        .param("o", o_iri.to_string())
        .param("layer", write_layer.to_string())
        .param("layers", layers.to_vec())
        .param("p", prop_iri.to_string())
        .param("relType", rel_type.to_string())
        .param("isStructural", is_structural),
    )
    .await?;
    rows.into_iter()
        .map(|row| {
            let layer: String = row.get("layer")?;
            let object: Node = row.get("o")?;
            let property: String = row.get("p").unwrap_or_else(|_| prop_iri.into());
            Ok(json!({
                "layer": layer,
                "o": endpoint_json(&object),
                "p": property,
            }))
        })
        .collect()
}

async fn missing_contradictions_txn(
    txn: &mut Txn,
    new_o: &str,
    conflicts: &[Value],
) -> Result<Vec<String>> {
    let old_objects = conflicts
        .iter()
        .filter_map(|conflict| conflict.pointer("/o/iri").and_then(Value::as_str))
        .map(str::to_string)
        .collect::<HashSet<_>>();
    let mut missing = Vec::new();
    for old_o in old_objects {
        let row = fetch_one_txn(
            txn,
            query(
                "MATCH (n:Entity {iri: $new}), (old:Entity {iri: $old}) \
                 OPTIONAL MATCH (n)-[r:CONTRADICTS]->(old) \
                 WHERE r.validTo IS NULL RETURN count(r) AS n",
            )
            .param("new", new_o.to_string())
            .param("old", old_o.clone()),
        )
        .await?;
        if row.and_then(|row| row.get::<i64>("n").ok()).unwrap_or(0) == 0 {
            missing.push(old_o);
        }
    }
    missing.sort();
    Ok(missing)
}

async fn write_contradicts_txn(
    txn: &mut Txn,
    new_o: &str,
    old_objects: &[String],
    layer: &str,
    episode: &Episode,
) -> Result<()> {
    for old_o in old_objects {
        txn.run(
            query(
                r#"
                MATCH (n:Entity {iri: $new}), (old:Entity {iri: $old})
                CREATE (n)-[:CONTRADICTS {
                    propertyIri: 'mindreader:property/CONTRADICTS',
                    layer: $layer,
                    validFrom: datetime(),
                    episodeId: $episode,
                    factText: $factText
                }]->(old)
                "#,
            )
            .param("new", new_o.to_string())
            .param("old", old_o.clone())
            .param("layer", layer.to_string())
            .param("episode", episode.iri.clone())
            .param("factText", format!("{new_o} CONTRADICTS {old_o}")),
        )
        .await?;
    }
    Ok(())
}

pub async fn count_current_contradicts(graph: &Graph, from: &str, to: &str) -> Result<i64> {
    let row = fetch_one(
        graph,
        query(
            r#"
            MATCH (a:Entity {iri: $from})-[r:CONTRADICTS]->(b:Entity {iri: $to})
            WHERE r.validTo IS NULL
            RETURN count(r) AS n
            "#,
        )
        .param("from", from.to_string())
        .param("to", to.to_string()),
    )
    .await?;
    Ok(row.and_then(|r| r.get::<i64>("n").ok()).unwrap_or(0))
}

pub fn map_tool_error(err: anyhow::Error) -> rmcp::model::ErrorData {
    use rmcp::model::ErrorData as McpError;
    if let Some(layer) = err.downcast_ref::<LayerError>() {
        return McpError::invalid_params(layer.0.clone(), None);
    }
    if let Some(domain) = err.downcast_ref::<DomainError>() {
        let reason = match domain {
            DomainError::InvalidInput(_) => "invalid_input",
            DomainError::Precondition(_) => "precondition_failed",
        };
        return McpError::invalid_params(domain.to_string(), Some(json!({ "reason": reason })));
    }
    McpError::internal_error(err.to_string(), None)
}

#[cfg(test)]
mod tests {
    use super::{map_tool_error, reject_system_owned_predicate};
    use crate::domain::DomainError;
    use crate::graph::spike_rank;

    #[test]
    fn spike_rank_order() {
        assert!(spike_rank(Some("Knowledge")) > spike_rank(Some("Insight")));
        assert!(spike_rank(Some("Insight")) > spike_rank(Some("Pattern")));
        assert!(spike_rank(Some("Pattern")) > spike_rank(Some("Signal")));
        assert!(spike_rank(Some("Signal")) > spike_rank(None));
    }

    #[test]
    fn null_layer_never_treated_as_visible() {
        let src = include_str!("tools.rs");
        let code = src.split("mod tests").next().unwrap_or(src);
        assert!(
            !code.contains("a.layer IS NULL") && !code.contains("r.layer IS NULL"),
            "read filters must not treat a missing layer as visible"
        );
    }

    #[test]
    fn transactional_pair_lookup_returns_all_matches() {
        let src = include_str!("tools.rs");
        let start = src
            .find("async fn find_current_pairs_txn(")
            .expect("find_current_pairs_txn");
        let end = src
            .find("async fn find_current_pair_txn(")
            .expect("find_current_pair_txn");
        assert!(
            !src[start..end].contains("LIMIT 1"),
            "replacement must close every duplicate current exact fact"
        );
    }

    #[test]
    fn system_owned_history_predicates_are_not_client_writable() {
        assert!(reject_system_owned_predicate("SUPERSEDES").is_err());
        assert!(reject_system_owned_predicate("mindreader:property/CONTRADICTS").is_err());
        assert!(reject_system_owned_predicate("worksOn").is_ok());
    }

    #[test]
    fn domain_errors_are_mcp_invalid_params() {
        let error = map_tool_error(DomainError::Precondition("missing old fact".into()).into());
        assert_eq!(error.code.0, -32602);
        assert_eq!(
            error.data.and_then(|data| data.get("reason").cloned()),
            Some(serde_json::json!("precondition_failed"))
        );
    }
}
