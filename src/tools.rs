use crate::config::GLOBAL_LAYER;
use crate::graph::{
    create_episode, create_episode_in_txn, endpoint_json, ensure_property, fact_text, fetch_all,
    fetch_one, get_node, merge_literal, merge_node, node_json, path_to_json, rel_json, safe_rel,
    spike_label, spike_rank, structural_rel_for, touch_search_text, Episode, NodeSpec, FIXED_RELS,
    SPIKE,
};
use crate::iri::{class_iri, is_iri, name_from_iri, property_iri};
use crate::layers::{assert_writable_layer, default_write_layer, visible_layers, LayerError};
use anyhow::{anyhow, Result};
use neo4rs::{query, Graph, Node, Path, Relation, Txn};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

const SCHEMA_STRUCTURAL_RELS: &[&str] = &[
    "INSTANCE_OF",
    "SUBCLASS_OF",
    "SUBPROPERTY_OF",
    "DOMAIN",
    "RANGE",
];

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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct AssertArgs {
    pub s: Value,
    pub p: String,
    pub o: Value,
    #[serde(default)]
    pub layer: Option<String>,
    #[serde(default)]
    pub spike: Option<String>,
    #[serde(default)]
    pub contradicts: bool,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct RetractArgs {
    #[serde(default)]
    pub iri: Option<String>,
    #[serde(default)]
    pub s: Option<String>,
    #[serde(default)]
    pub p: Option<String>,
    #[serde(default)]
    pub o: Option<Value>,
    #[serde(default)]
    pub layer: Option<String>,
    #[serde(default)]
    pub reason: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
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

fn parse_node_spec(v: &Value) -> Result<NodeSpec> {
    match v {
        Value::String(s) => {
            if is_iri(s) {
                Ok(NodeSpec {
                    iri: Some(s.clone()),
                    name: None,
                    labels: vec![],
                })
            } else {
                Ok(NodeSpec {
                    iri: None,
                    name: Some(s.clone()),
                    labels: vec![],
                })
            }
        }
        Value::Object(map) => {
            let iri = map
                .get("iri")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let name = map
                .get("name")
                .and_then(|x| x.as_str())
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string());
            let labels = map
                .get("labels")
                .and_then(|x| x.as_array())
                .map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            if iri.is_none() && name.is_none() {
                return Err(anyhow!("node object needs iri or name"));
            }
            Ok(NodeSpec { iri, name, labels })
        }
        _ => Err(anyhow!(
            "subject/object must be a string IRI/name or {{iri,name,labels}}"
        )),
    }
}

enum ObjectKind {
    Node(NodeSpec),
    Literal { value: String, datatype: String },
}

fn parse_object(v: &Value) -> Result<ObjectKind> {
    match v {
        Value::Bool(b) => Ok(ObjectKind::Literal {
            value: b.to_string(),
            datatype: "xsd:boolean".into(),
        }),
        Value::Number(n) => {
            let datatype = if n.is_i64() || n.is_u64() {
                "xsd:integer"
            } else {
                "xsd:double"
            };
            Ok(ObjectKind::Literal {
                value: n.to_string(),
                datatype: datatype.into(),
            })
        }
        Value::Object(map)
            if map.contains_key("value")
                && !map.contains_key("iri")
                && !map.contains_key("name") =>
        {
            let value = match &map["value"] {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            let datatype = map
                .get("datatype")
                .and_then(|x| x.as_str())
                .unwrap_or("xsd:string")
                .to_string();
            Ok(ObjectKind::Literal { value, datatype })
        }
        other => Ok(ObjectKind::Node(parse_node_spec(other)?)),
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
        for index in ["wakeup_nodes", "entity_fulltext"] {
            if let Ok(rows) = fetch_all(
                graph,
                query(
                    r#"
                    CALL db.index.fulltext.queryNodes($index, $q) YIELD node, score
                    RETURN node.iri AS iri, score
                    LIMIT $limit
                    "#,
                )
                .param("index", index.to_string())
                .param("q", escaped.clone())
                .param("limit", limit * 4),
            )
            .await
            {
                for row in rows {
                    if let (Ok(iri), Ok(score)) =
                        (row.get::<String>("iri"), row.get::<f64>("score"))
                    {
                        let e = node_scores.entry(iri).or_insert(0.0);
                        if score > *e {
                            *e = score;
                        }
                    }
                }
            }
        }
        for rel_index in ["wakeup_facts", "wakeup_about"] {
            if let Ok(rows) = fetch_all(
                graph,
                query(
                    r#"
                CALL db.index.fulltext.queryRelationships($index, $q) YIELD relationship, score
                RETURN id(relationship) AS rid, score
                LIMIT $limit
                "#,
                )
                .param("index", rel_index.to_string())
                .param("q", escaped.clone())
                .param("limit", limit * 4),
            )
            .await
            {
                for row in rows {
                    if let (Ok(rid), Ok(score)) = (row.get::<i64>("rid"), row.get::<f64>("score")) {
                        let e = rel_scores.entry(rid).or_insert(0.0);
                        if score > *e {
                            *e = score;
                        }
                    }
                }
            }
        }
    }

    let use_contains = node_scores.is_empty() && rel_scores.is_empty();
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
            LIMIT $limit
            "#,
        )
        .param("layers", layers.clone())
        .param("labels", labels.clone())
        .param("labelCount", labels.len() as i64)
        .param("useContains", use_contains)
        .param("text", needle.clone())
        .param("iris", iris)
        .param("rids", rids)
        .param("limit", limit * 4),
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
        if let Ok(sp_rows) = fetch_all(
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
        .await
        {
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
    });
    facts.truncate(limit as usize);

    spike_list.sort_by(|a, b| {
        let ra = a.get("rank").and_then(|v| v.as_str());
        let rb = b.get("rank").and_then(|v| v.as_str());
        spike_rank(rb).cmp(&spike_rank(ra))
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

    Ok(json!({
        "project": project,
        "layers": layers,
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
            .map(|r| safe_rel(&r))
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
    let layer = args
        .layer
        .clone()
        .unwrap_or_else(|| default_write_layer(project));
    assert_writable_layer(&layer, project)?;

    let spike = match args.spike.as_deref() {
        None => None,
        Some(s) => {
            if !SPIKE.contains(&s) {
                return Err(anyhow!(
                    "spike must be one of Signal|Pattern|Insight|Knowledge"
                ));
            }
            Some(s.to_string())
        }
    };

    let mut s_spec = parse_node_spec(&args.s)?;
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
    let subject = merge_node(graph, &s_spec, &s_kind, &extra).await?;

    let (object, o_is_literal) = match parse_object(&args.o)? {
        ObjectKind::Literal { value, datatype } => {
            (merge_literal(graph, &value, &datatype).await?, true)
        }
        ObjectKind::Node(spec) => (merge_node(graph, &spec, "element", &[]).await?, false),
    };

    let (prop_iri, minted_stub, prop_json) = ensure_property(graph, &args.p).await?;
    let structural = structural_rel_for(&prop_iri);

    let is_contradicts_rel = structural.as_deref() == Some("CONTRADICTS");
    let ft = fact_text(&subject.name, &subject.iri, &prop_iri, &object);
    let visible = visible_layers(project);
    let conflicts = find_conflicts(
        graph,
        &subject.iri,
        &prop_iri,
        structural.as_deref(),
        &layer,
        &object.iri,
        &visible,
    )
    .await?;

    // CONTRADICTS is multi-valued: only the exact (s,p,o,layer) pair is idempotent.
    // Other properties are single-valued per (s,p,layer): return ALL current matches.
    let already_current = if is_contradicts_rel {
        find_current_pair(
            graph,
            &subject.iri,
            &prop_iri,
            structural.as_deref(),
            &layer,
            &object.iri,
        )
        .await?
        .is_some()
    } else {
        false
    };
    let currents = if is_contradicts_rel {
        Vec::new()
    } else {
        find_current(
            graph,
            &subject.iri,
            &prop_iri,
            structural.as_deref(),
            &layer,
        )
        .await?
    };
    let already_current =
        already_current || (!currents.is_empty() && currents.iter().all(|c| c.o_iri == object.iri));

    if already_current {
        let mut episode_json = Value::Null;
        if args.contradicts && !conflicts.is_empty() {
            let episode = create_episode(graph, "memory_assert", None).await?;
            write_contradicts(graph, &object.iri, &conflicts, &layer, &episode).await?;
            episode_json = json!({ "iri": episode.iri, "at": episode.at, "tool": episode.tool });
        }
        return Ok(json!({
            "noop": true,
            "s": subject.json,
            "p": prop_iri,
            "o": object.json,
            "layer": layer,
            "propertyStub": minted_stub,
            "property": prop_json,
            "conflicts": conflicts,
            "episode": episode_json,
        }));
    }

    let mut about_currents = Vec::new();
    let mut need_about = false;
    let mut about_ft = String::new();
    if spike.is_some() && !o_is_literal && object.labels.iter().any(|l| l == "Element") {
        let already_about = structural.as_deref() == Some("ABOUT");
        if !already_about {
            about_currents = find_current(
                graph,
                &subject.iri,
                "mindreader:property/ABOUT",
                Some("ABOUT"),
                &layer,
            )
            .await?;
            let about_already =
                !about_currents.is_empty() && about_currents.iter().all(|c| c.o_iri == object.iri);
            if !about_already {
                need_about = true;
                about_ft = fact_text(
                    &subject.name,
                    &subject.iri,
                    "mindreader:property/ABOUT",
                    &object,
                );
            }
        }
    }

    // Close-all-current + create replacement + SUPERSEDES must be one Neo4j txn.
    // Crash between close and create must not leave validTo set with no replacement.
    let mut txn = graph.start_txn().await?;
    let write = async {
        let episode = create_episode_in_txn(&mut txn, "memory_assert", None).await?;
        let mut superseded = Value::Null;
        if !is_contradicts_rel && !currents.is_empty() {
            let mut from_all: Vec<String> = Vec::new();
            for cur in &currents {
                close_rel_txn(&mut txn, cur.rel_id, Some(&episode.iri)).await?;
                if cur.o_iri != object.iri && !from_all.iter().any(|o| o == &cur.o_iri) {
                    create_supersedes_txn(
                        &mut txn,
                        &object.iri,
                        &cur.o_iri,
                        &prop_iri,
                        &layer,
                        &episode,
                    )
                    .await?;
                    from_all.push(cur.o_iri.clone());
                }
            }
            if !from_all.is_empty() {
                superseded = json!({
                    "from": if from_all.len() == 1 {
                        json!(from_all[0].clone())
                    } else {
                        json!(from_all)
                    },
                    "to": object.iri,
                    "propertyIri": prop_iri,
                    "layer": layer,
                });
            }
        }
        if let Some(rel_type) = &structural {
            create_structural_txn(
                &mut txn,
                &subject.iri,
                rel_type,
                &object.iri,
                &prop_iri,
                &layer,
                &episode,
                None,
                &ft,
            )
            .await?;
        } else {
            create_asserts_txn(
                &mut txn,
                &subject.iri,
                &object.iri,
                &prop_iri,
                &layer,
                &episode,
                None,
                &ft,
            )
            .await?;
        }
        if need_about {
            for old in &about_currents {
                close_rel_txn(&mut txn, old.rel_id, Some(&episode.iri)).await?;
            }
            create_structural_txn(
                &mut txn,
                &subject.iri,
                "ABOUT",
                &object.iri,
                "mindreader:property/ABOUT",
                &layer,
                &episode,
                None,
                &about_ft,
            )
            .await?;
        }
        Ok::<_, anyhow::Error>((episode, superseded))
    }
    .await;
    let (episode, superseded) = match write {
        Ok(v) => {
            txn.commit().await?;
            v
        }
        Err(e) => {
            let _ = txn.rollback().await;
            return Err(e);
        }
    };

    touch_search_text(graph, &subject.iri, Some(&ft)).await?;
    touch_search_text(graph, &object.iri, Some(&ft)).await?;
    if need_about {
        touch_search_text(graph, &subject.iri, Some(&about_ft)).await?;
        touch_search_text(graph, &object.iri, Some(&about_ft)).await?;
    }

    if args.contradicts && !conflicts.is_empty() {
        write_contradicts(graph, &object.iri, &conflicts, &layer, &episode).await?;
    }

    Ok(json!({
        "noop": false,
        "s": subject.json,
        "p": prop_iri,
        "o": object.json,
        "layer": layer,
        "superseded": superseded,
        "episode": { "iri": episode.iri, "at": episode.at, "tool": episode.tool },
        "propertyStub": minted_stub,
        "property": prop_json,
        "spike": spike,
        "conflicts": conflicts,
    }))
}

struct CurrentFact {
    rel_id: i64,
    o_iri: String,
}

async fn find_current(
    graph: &Graph,
    s: &str,
    prop_iri: &str,
    structural: Option<&str>,
    layer: &str,
) -> Result<Vec<CurrentFact>> {
    let rows = if let Some(rel) = structural {
        let rel = safe_rel(rel)?;
        let q = format!(
            r#"
            MATCH (s:Entity {{iri: $s}})-[r:{rel}]->(o:Entity)
            WHERE r.validTo IS NULL AND r.layer = $layer
            RETURN id(r) AS rid, o.iri AS oiri
            "#
        );
        fetch_all(
            graph,
            query(&q)
                .param("s", s.to_string())
                .param("layer", layer.to_string()),
        )
        .await?
    } else {
        fetch_all(
            graph,
            query(
                r#"
                MATCH (s:Entity {iri: $s})-[r:ASSERTS]->(o:Entity)
                WHERE r.validTo IS NULL AND r.layer = $layer AND r.propertyIri = $p
                RETURN id(r) AS rid, o.iri AS oiri
                "#,
            )
            .param("s", s.to_string())
            .param("layer", layer.to_string())
            .param("p", prop_iri.to_string()),
        )
        .await?
    };
    let mut out = Vec::new();
    for r in rows {
        out.push(CurrentFact {
            rel_id: r.get::<i64>("rid")?,
            o_iri: r.get::<String>("oiri")?,
        });
    }
    Ok(out)
}

#[allow(dead_code)]
async fn close_rel(graph: &Graph, rel_id: i64, episode_id: Option<&str>) -> Result<()> {
    graph.run(close_rel_query(rel_id, episode_id)).await?;
    Ok(())
}

#[allow(dead_code)]
async fn create_structural(
    graph: &Graph,
    s: &str,
    rel_type: &str,
    o: &str,
    prop_iri: &str,
    layer: &str,
    episode: &Episode,
    reason: Option<&str>,
    fact_text: &str,
) -> Result<()> {
    graph
        .run(structural_query(
            s, rel_type, o, prop_iri, layer, episode, reason, fact_text,
        )?)
        .await?;
    Ok(())
}

fn close_rel_query(rel_id: i64, episode_id: Option<&str>) -> neo4rs::Query {
    query(
        r#"
        MATCH ()-[r]->()
        WHERE id(r) = $rid AND r.validTo IS NULL
        SET r.validTo = datetime()
        SET r.retractedBy = $episode
        "#,
    )
    .param("rid", rel_id)
    .param("episode", episode_id.map(|s| s.to_string()))
}

fn asserts_query(
    s: &str,
    o: &str,
    prop_iri: &str,
    layer: &str,
    episode: &Episode,
    reason: Option<&str>,
    fact_text: &str,
) -> neo4rs::Query {
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
    .param("s", s.to_string())
    .param("o", o.to_string())
    .param("p", prop_iri.to_string())
    .param("layer", layer.to_string())
    .param("episode", episode.iri.clone())
    .param("reason", reason.map(|s| s.to_string()))
    .param("factText", fact_text.to_string())
}

fn structural_query(
    s: &str,
    rel_type: &str,
    o: &str,
    prop_iri: &str,
    layer: &str,
    episode: &Episode,
    reason: Option<&str>,
    fact_text: &str,
) -> Result<neo4rs::Query> {
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
        .param("s", s.to_string())
        .param("o", o.to_string())
        .param("p", prop_iri.to_string())
        .param("layer", layer.to_string())
        .param("episode", episode.iri.clone())
        .param("reason", reason.map(|s| s.to_string()))
        .param("factText", fact_text.to_string()))
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

async fn close_rel_txn(txn: &mut Txn, rel_id: i64, episode_id: Option<&str>) -> Result<()> {
    txn.run(close_rel_query(rel_id, episode_id)).await?;
    Ok(())
}

async fn create_asserts_txn(
    txn: &mut Txn,
    s: &str,
    o: &str,
    prop_iri: &str,
    layer: &str,
    episode: &Episode,
    reason: Option<&str>,
    fact_text: &str,
) -> Result<()> {
    txn.run(asserts_query(
        s, o, prop_iri, layer, episode, reason, fact_text,
    ))
    .await?;
    Ok(())
}

async fn create_structural_txn(
    txn: &mut Txn,
    s: &str,
    rel_type: &str,
    o: &str,
    prop_iri: &str,
    layer: &str,
    episode: &Episode,
    reason: Option<&str>,
    fact_text: &str,
) -> Result<()> {
    txn.run(structural_query(
        s, rel_type, o, prop_iri, layer, episode, reason, fact_text,
    )?)
    .await?;
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
    let layer = args
        .layer
        .clone()
        .unwrap_or_else(|| default_write_layer(project));
    // Missing layer → default WRITE layer only. Never anyLayer across tenants.
    assert_writable_layer(&layer, project)?;

    let episode = create_episode(graph, "memory_retract", args.reason.as_deref()).await?;

    let mut closed = 0i64;
    let mut skipped_schema = 0i64;

    if let (Some(s), Some(p)) = (args.s.as_deref(), args.p.as_deref()) {
        let s_iri = if is_iri(s) {
            s.to_string()
        } else {
            crate::iri::mint_iri("element", s, true)
        };
        let prop = property_iri(p);
        let structural = structural_rel_for(&prop);
        let o_iri = object_iri_for_retract(args.o.as_ref());

        if let Some(rel) = structural {
            let rel = safe_rel(&rel)?;
            let q = if o_iri.is_some() {
                format!(
                    r#"
                    MATCH (s:Entity {{iri: $s}})-[r:{rel}]->(o:Entity {{iri: $o}})
                    WHERE r.validTo IS NULL AND r.layer = $layer
                    SET r.validTo = datetime(), r.retractedBy = $episode, r.reason = coalesce($reason, r.reason)
                    RETURN count(r) AS n
                    "#
                )
            } else {
                format!(
                    r#"
                    MATCH (s:Entity {{iri: $s}})-[r:{rel}]->(o:Entity)
                    WHERE r.validTo IS NULL AND r.layer = $layer
                    SET r.validTo = datetime(), r.retractedBy = $episode, r.reason = coalesce($reason, r.reason)
                    RETURN count(r) AS n
                    "#
                )
            };
            let mut qy = query(&q)
                .param("s", s_iri)
                .param("layer", layer.clone())
                .param("episode", episode.iri.clone())
                .param("reason", args.reason.clone());
            if let Some(o) = o_iri {
                qy = qy.param("o", o);
            }
            if let Some(row) = fetch_one(graph, qy).await? {
                closed = row.get("n").unwrap_or(0);
            }
        } else {
            let q = if o_iri.is_some() {
                r#"
                MATCH (s:Entity {iri: $s})-[r:ASSERTS]->(o:Entity {iri: $o})
                WHERE r.validTo IS NULL AND r.propertyIri = $p AND r.layer = $layer
                SET r.validTo = datetime(), r.retractedBy = $episode, r.reason = coalesce($reason, r.reason)
                RETURN count(r) AS n
                "#
            } else {
                r#"
                MATCH (s:Entity {iri: $s})-[r:ASSERTS]->(o:Entity)
                WHERE r.validTo IS NULL AND r.propertyIri = $p AND r.layer = $layer
                SET r.validTo = datetime(), r.retractedBy = $episode, r.reason = coalesce($reason, r.reason)
                RETURN count(r) AS n
                "#
            };
            let mut qy = query(q)
                .param("s", s_iri)
                .param("p", prop)
                .param("layer", layer.clone())
                .param("episode", episode.iri.clone())
                .param("reason", args.reason.clone());
            if let Some(o) = o_iri {
                qy = qy.param("o", o);
            }
            if let Some(row) = fetch_one(graph, qy).await? {
                closed = row.get("n").unwrap_or(0);
            }
        }
    } else if let Some(iri) = args.iri.as_deref() {
        let node = get_node(graph, iri).await?;
        if let Some(n) = &node {
            let labels: Vec<String> = n.labels().into_iter().map(|s| s.to_string()).collect();
            if labels.iter().any(|l| l == "Class" || l == "Property") {
                return Err(anyhow!(
                    "refuse to retract Class/Property by iri ({iri}); pass an explicit triple (s, p) for schema/structural rels"
                ));
            }
        }
        let protected: Vec<String> = SCHEMA_STRUCTURAL_RELS
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        let row = fetch_one(
            graph,
            query(
                r#"
                MATCH (n:Entity {iri: $iri})-[r]->(o)
                WHERE r.validTo IS NULL
                  AND r.layer = $layer
                  AND NOT type(r) IN $protected
                  AND NOT n:Class AND NOT n:Property
                  AND NOT o:Class AND NOT o:Property
                SET r.validTo = datetime(), r.retractedBy = $episode, r.reason = coalesce($reason, r.reason)
                RETURN count(r) AS n
                "#,
            )
            .param("iri", iri.to_string())
            .param("layer", layer.clone())
            .param("protected", protected.clone())
            .param("episode", episode.iri.clone())
            .param("reason", args.reason.clone()),
        )
        .await?;
        closed = row.and_then(|r| r.get::<i64>("n").ok()).unwrap_or(0);
        let skip = fetch_one(
            graph,
            query(
                r#"
                MATCH (n:Entity {iri: $iri})-[r]->(o)
                WHERE r.validTo IS NULL
                  AND r.layer = $layer
                  AND (
                    type(r) IN $protected
                    OR n:Class OR n:Property OR o:Class OR o:Property
                  )
                RETURN count(r) AS n
                "#,
            )
            .param("iri", iri.to_string())
            .param("layer", layer.clone())
            .param("protected", protected),
        )
        .await?;
        skipped_schema = skip.and_then(|r| r.get::<i64>("n").ok()).unwrap_or(0);
    } else {
        return Err(anyhow!(
            "memory_retract needs iri, or s+p (optional o, layer, reason)"
        ));
    }

    Ok(json!({
        "retracted": closed,
        "soft": true,
        "layer": layer,
        "skippedSchema": skipped_schema,
        "episode": { "iri": episode.iri, "at": episode.at, "tool": episode.tool },
        "reason": args.reason,
    }))
}

fn object_iri_for_retract(o: Option<&Value>) -> Option<String> {
    o.and_then(|v| match v {
        Value::String(s) => Some(if is_iri(s) {
            s.clone()
        } else {
            crate::iri::mint_iri("element", s, true)
        }),
        Value::Object(map) => map
            .get("iri")
            .and_then(|x| x.as_str())
            .map(|s| s.to_string())
            .or_else(|| {
                map.get("name")
                    .and_then(|x| x.as_str())
                    .map(|n| crate::iri::mint_iri("element", n, true))
            }),
        _ => None,
    })
}

pub async fn memory_schema(graph: &Graph, project: &str, args: SchemaArgs) -> Result<Value> {
    let kind = args.kind.trim().to_ascii_lowercase();
    if kind != "class" && kind != "property" {
        return Err(anyhow!("kind must be class or property"));
    }
    let seed = args
        .iri
        .as_deref()
        .or(args.name.as_deref())
        .ok_or_else(|| anyhow!("memory_schema needs name or iri"))?;
    let iri = if kind == "class" {
        class_iri(seed)
    } else {
        property_iri(seed)
    };
    let name = args.name.clone().unwrap_or_else(|| name_from_iri(&iri));
    let label = if kind == "class" { "Class" } else { "Property" };
    let spec = NodeSpec {
        iri: Some(iri.clone()),
        name: Some(name.clone()),
        labels: vec![label.into()],
    };
    let node = merge_node(graph, &spec, &kind, &[]).await?;
    graph
        .run(query("MATCH (n:Entity {iri: $iri}) SET n.stub = false").param("iri", iri.clone()))
        .await
        .ok();

    let episode = create_episode(graph, "memory_schema", None).await?;
    let layer = GLOBAL_LAYER.to_string();
    let _ = project;

    let mut links = Vec::new();
    if kind == "class" {
        let class_class = merge_node(
            graph,
            &NodeSpec {
                iri: Some("mindreader:class/Class".into()),
                name: Some("Class".into()),
                labels: vec!["Class".into()],
            },
            "class",
            &[],
        )
        .await?;
        ensure_link(
            graph,
            &node.iri,
            "INSTANCE_OF",
            &class_class.iri,
            "mindreader:property/INSTANCE_OF",
            &layer,
            &episode,
        )
        .await?;
        links.push(json!({"rel": "INSTANCE_OF", "to": class_class.iri}));
        if let Some(sup) = args.sub_class_of.as_deref() {
            let parent = merge_node(
                graph,
                &NodeSpec {
                    iri: Some(class_iri(sup)),
                    name: Some(name_from_iri(&class_iri(sup))),
                    labels: vec!["Class".into()],
                },
                "class",
                &[],
            )
            .await?;
            ensure_link(
                graph,
                &node.iri,
                "SUBCLASS_OF",
                &parent.iri,
                "mindreader:property/SUBCLASS_OF",
                &layer,
                &episode,
            )
            .await?;
            links.push(json!({"rel": "SUBCLASS_OF", "to": parent.iri}));
        }
    } else {
        let prop_class = merge_node(
            graph,
            &NodeSpec {
                iri: Some("mindreader:class/Property".into()),
                name: Some("Property".into()),
                labels: vec!["Class".into()],
            },
            "class",
            &[],
        )
        .await?;
        ensure_link(
            graph,
            &node.iri,
            "INSTANCE_OF",
            &prop_class.iri,
            "mindreader:property/INSTANCE_OF",
            &layer,
            &episode,
        )
        .await?;
        links.push(json!({"rel": "INSTANCE_OF", "to": prop_class.iri}));
        if let Some(sup) = args.sub_property_of.as_deref() {
            let parent = merge_node(
                graph,
                &NodeSpec {
                    iri: Some(property_iri(sup)),
                    name: Some(name_from_iri(&property_iri(sup))),
                    labels: vec!["Property".into()],
                },
                "property",
                &[],
            )
            .await?;
            ensure_link(
                graph,
                &node.iri,
                "SUBPROPERTY_OF",
                &parent.iri,
                "mindreader:property/SUBPROPERTY_OF",
                &layer,
                &episode,
            )
            .await?;
            links.push(json!({"rel": "SUBPROPERTY_OF", "to": parent.iri}));
        }
        if let Some(d) = args.domain.as_deref() {
            let cls = merge_node(
                graph,
                &NodeSpec {
                    iri: Some(class_iri(d)),
                    name: Some(name_from_iri(&class_iri(d))),
                    labels: vec!["Class".into()],
                },
                "class",
                &[],
            )
            .await?;
            ensure_link(
                graph,
                &node.iri,
                "DOMAIN",
                &cls.iri,
                "mindreader:property/DOMAIN",
                &layer,
                &episode,
            )
            .await?;
            links.push(json!({"rel": "DOMAIN", "to": cls.iri}));
        }
        if let Some(r) = args.range.as_deref() {
            let cls = merge_node(
                graph,
                &NodeSpec {
                    iri: Some(class_iri(r)),
                    name: Some(name_from_iri(&class_iri(r))),
                    labels: vec!["Class".into()],
                },
                "class",
                &[],
            )
            .await?;
            ensure_link(
                graph,
                &node.iri,
                "RANGE",
                &cls.iri,
                "mindreader:property/RANGE",
                &layer,
                &episode,
            )
            .await?;
            links.push(json!({"rel": "RANGE", "to": cls.iri}));
        }
    }

    Ok(json!({
        "kind": kind,
        "node": node.json,
        "links": links,
        "episode": { "iri": episode.iri, "at": episode.at, "tool": episode.tool },
    }))
}

async fn ensure_link(
    graph: &Graph,
    s: &str,
    rel: &str,
    o: &str,
    prop_iri: &str,
    layer: &str,
    episode: &Episode,
) -> Result<()> {
    let currents = find_current(graph, s, prop_iri, Some(rel), layer).await?;
    if !currents.is_empty() && currents.iter().all(|c| c.o_iri == o) {
        return Ok(());
    }
    let ft = format!("{s} {rel} {o}");
    let mut txn = graph.start_txn().await?;
    let write = async {
        for cur in &currents {
            close_rel_txn(&mut txn, cur.rel_id, Some(&episode.iri)).await?;
        }
        create_structural_txn(&mut txn, s, rel, o, prop_iri, layer, episode, None, &ft).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    match write {
        Ok(()) => {
            txn.commit().await?;
            Ok(())
        }
        Err(e) => {
            let _ = txn.rollback().await;
            Err(e)
        }
    }
}

async fn find_current_pair(
    graph: &Graph,
    s: &str,
    prop_iri: &str,
    structural: Option<&str>,
    layer: &str,
    o: &str,
) -> Result<Option<CurrentFact>> {
    let row = if let Some(rel) = structural {
        let rel = safe_rel(rel)?;
        let q = format!(
            r#"
            MATCH (s:Entity {{iri: $s}})-[r:{rel}]->(o:Entity {{iri: $o}})
            WHERE r.validTo IS NULL AND r.layer = $layer
            RETURN id(r) AS rid, o.iri AS oiri
            "#
        );
        fetch_one(
            graph,
            query(&q)
                .param("s", s.to_string())
                .param("o", o.to_string())
                .param("layer", layer.to_string()),
        )
        .await?
    } else {
        fetch_one(
            graph,
            query(
                r#"
                MATCH (s:Entity {iri: $s})-[r:ASSERTS]->(o:Entity {iri: $o})
                WHERE r.validTo IS NULL AND r.layer = $layer AND r.propertyIri = $p
                RETURN id(r) AS rid, o.iri AS oiri
                "#,
            )
            .param("s", s.to_string())
            .param("o", o.to_string())
            .param("layer", layer.to_string())
            .param("p", prop_iri.to_string()),
        )
        .await?
    };
    Ok(match row {
        Some(r) => Some(CurrentFact {
            rel_id: r.get::<i64>("rid")?,
            o_iri: r.get::<String>("oiri")?,
        }),
        None => None,
    })
}

async fn find_conflicts(
    graph: &Graph,
    s: &str,
    prop_iri: &str,
    structural: Option<&str>,
    write_layer: &str,
    o_iri: &str,
    layers: &[String],
) -> Result<Vec<Value>> {
    let rel_type = structural.unwrap_or("ASSERTS");
    let is_structural = structural.is_some();
    let rows = fetch_all(
        graph,
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
    let mut out = Vec::new();
    for row in rows {
        let layer: String = match row.get("layer") {
            Ok(s) => s,
            Err(_) => continue,
        };
        let o: Node = match row.get("o") {
            Ok(n) => n,
            Err(_) => continue,
        };
        let p: String = row.get("p").unwrap_or_else(|_| prop_iri.to_string());
        out.push(json!({
            "layer": layer,
            "o": endpoint_json(&o),
            "p": p,
        }));
    }
    Ok(out)
}

async fn write_contradicts(
    graph: &Graph,
    new_o: &str,
    conflicts: &[Value],
    layer: &str,
    episode: &Episode,
) -> Result<()> {
    for c in conflicts {
        let Some(old_o) = c
            .get("o")
            .and_then(|o| o.get("iri"))
            .and_then(|v| v.as_str())
        else {
            continue;
        };
        let ft = format!("{new_o} CONTRADICTS {old_o}");
        graph
            .run(
                query(
                    r#"
                    MATCH (n:Entity {iri: $new}), (old:Entity {iri: $old})
                    OPTIONAL MATCH (n)-[existing:CONTRADICTS]->(old)
                    WHERE existing.validTo IS NULL
                    WITH n, old, existing
                    WHERE existing IS NULL
                    CREATE (n)-[r:CONTRADICTS {
                        propertyIri: 'mindreader:property/CONTRADICTS',
                        layer: $layer,
                        validFrom: datetime(),
                        episodeId: $episode,
                        factText: $factText
                    }]->(old)
                    "#,
                )
                .param("new", new_o.to_string())
                .param("old", old_o.to_string())
                .param("layer", layer.to_string())
                .param("episode", episode.iri.clone())
                .param("factText", ft),
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
    McpError::internal_error(err.to_string(), None)
}

#[cfg(test)]
mod tests {
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
    fn find_current_returns_all_matches() {
        let src = include_str!("tools.rs");
        let start = src.find("async fn find_current(").expect("find_current");
        let pair = src
            .find("async fn find_current_pair(")
            .expect("find_current_pair");
        let end = src
            .find("async fn find_conflicts(")
            .expect("find_conflicts");
        assert!(
            !src[start..pair].contains("LIMIT 1"),
            "find_current must return/close ALL current (s,p,layer) matches"
        );
        assert!(
            !src[pair..end].contains("LIMIT 1"),
            "find_current_pair must not LIMIT 1; CONTRADICTS stays multi-valued via pair match, not close-all"
        );
    }
}
