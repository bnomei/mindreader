use crate::config::GLOBAL_LAYER;
use crate::graph::{
    create_episode, ensure_property, fetch_all, fetch_one, get_node, merge_literal, merge_node,
    node_json, path_to_json, rel_json, safe_rel, structural_rel_for, Episode, FIXED_RELS, NodeSpec,
    SPIKE,
};
use crate::iri::{class_iri, is_iri, name_from_iri, property_iri};
use crate::layers::{assert_writable_layer, default_write_layer, visible_layers, LayerError};
use anyhow::{anyhow, Result};
use neo4rs::{query, Graph, Node, Path, Relation};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

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
        _ => Err(anyhow!("subject/object must be a string IRI/name or {{iri,name,labels}}")),
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
        Value::Object(map) if map.contains_key("value") && !map.contains_key("iri") && !map.contains_key("name") => {
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
              AND (r.layer IS NULL OR r.layer IN $layers)
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

pub async fn memory_search(graph: &Graph, _project: &str, args: SearchArgs) -> Result<Value> {
    let limit = args.limit.unwrap_or(20).clamp(1, 100) as i64;
    let labels = args.labels.unwrap_or_default();
    let text = args.text.unwrap_or_default();
    let trimmed = text.trim().to_string();

    if !trimmed.is_empty() {
        let escaped = lucene_escape(&trimmed);
        match fetch_all(
            graph,
            query(
                r#"
                CALL db.index.fulltext.queryNodes('entity_fulltext', $q) YIELD node, score
                WHERE $labelCount = 0 OR any(l IN $labels WHERE l IN labels(node))
                RETURN node, score
                ORDER BY score DESC
                LIMIT $limit
                "#,
            )
            .param("q", escaped)
            .param("labels", labels.clone())
            .param("labelCount", labels.len() as i64)
            .param("limit", limit),
        )
        .await
        {
            Ok(rows) => {
                let nodes: Vec<Value> = rows
                    .into_iter()
                    .filter_map(|row| {
                        let node: Node = row.get("node").ok()?;
                        let mut j = node_json(&node);
                        if let Ok(score) = row.get::<f64>("score") {
                            j["score"] = json!(score);
                        }
                        Some(j)
                    })
                    .collect();
                return Ok(json!({
                    "query": trimmed,
                    "mode": "fulltext",
                    "nodes": nodes,
                }));
            }
            Err(_) => {}
        }
    }

    let rows = fetch_all(
        graph,
        query(
            r#"
            MATCH (n:Entity)
            WHERE ($text = '' OR toLower(coalesce(n.name, '')) CONTAINS toLower($text)
                   OR toLower(n.iri) CONTAINS toLower($text))
              AND ($labelCount = 0 OR any(l IN $labels WHERE l IN labels(n)))
            RETURN n
            LIMIT $limit
            "#,
        )
        .param("text", trimmed.clone())
        .param("labels", labels.clone())
        .param("labelCount", labels.len() as i64)
        .param("limit", limit),
    )
    .await?;
    let nodes: Vec<Value> = rows
        .into_iter()
        .filter_map(|row| row.get::<Node>("n").ok().map(|n| node_json(&n)))
        .collect();
    Ok(json!({
        "query": if trimmed.is_empty() { Value::Null } else { json!(trimmed) },
        "mode": if trimmed.is_empty() { "labels" } else { "contains" },
        "nodes": nodes,
    }))
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
        rs.into_iter().map(|r| safe_rel(&r)).collect::<Result<Vec<_>>>()?
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
          AND (r.layer IS NULL OR r.layer IN $layers)
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

    let current = find_current(
        graph,
        &subject.iri,
        &prop_iri,
        structural.as_deref(),
        &layer,
    )
    .await?;

    if let Some(cur) = &current {
        if cur.o_iri == object.iri {
            return Ok(json!({
                "noop": true,
                "s": subject.json,
                "p": prop_iri,
                "o": object.json,
                "layer": layer,
                "propertyStub": minted_stub,
                "property": prop_json,
            }));
        }
    }

    let episode = create_episode(graph, "memory_assert", None).await?;
    let mut superseded = Value::Null;

    if let Some(cur) = &current {
        close_rel(graph, cur.rel_id, Some(&episode.iri)).await?;
        create_supersedes(
            graph,
            &object.iri,
            &cur.o_iri,
            &prop_iri,
            &layer,
            &episode,
        )
        .await?;
        superseded = json!({
            "from": cur.o_iri,
            "to": object.iri,
            "propertyIri": prop_iri,
            "layer": layer,
        });
    }

    if let Some(rel_type) = &structural {
        create_structural(
            graph,
            &subject.iri,
            rel_type,
            &object.iri,
            &prop_iri,
            &layer,
            &episode,
            None,
        )
        .await?;
    } else {
        create_asserts(
            graph,
            &subject.iri,
            &object.iri,
            &prop_iri,
            &layer,
            &episode,
            None,
        )
        .await?;
    }

    if let Some(sp) = &spike {
        if !o_is_literal && object.labels.iter().any(|l| l == "Element") {
            let already_about = structural.as_deref() == Some("ABOUT");
            if !already_about {
                let about_current =
                    find_current(graph, &subject.iri, "mindreader:property/ABOUT", Some("ABOUT"), &layer)
                        .await?;
                let skip = about_current
                    .as_ref()
                    .map(|c| c.o_iri == object.iri)
                    .unwrap_or(false);
                if !skip {
                    if let Some(old) = about_current {
                        if old.o_iri != object.iri {
                            close_rel(graph, old.rel_id, Some(&episode.iri)).await?;
                        }
                    }
                    create_structural(
                        graph,
                        &subject.iri,
                        "ABOUT",
                        &object.iri,
                        "mindreader:property/ABOUT",
                        &layer,
                        &episode,
                        None,
                    )
                    .await?;
                }
            }
        }
        let _ = sp;
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
) -> Result<Option<CurrentFact>> {
    let row = if let Some(rel) = structural {
        let rel = safe_rel(rel)?;
        let q = format!(
            r#"
            MATCH (s:Entity {{iri: $s}})-[r:{rel}]->(o:Entity)
            WHERE r.validTo IS NULL AND r.layer = $layer
            RETURN id(r) AS rid, o.iri AS oiri
            LIMIT 1
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
                RETURN id(r) AS rid, o.iri AS oiri
                LIMIT 1
                "#,
            )
            .param("s", s.to_string())
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

async fn close_rel(graph: &Graph, rel_id: i64, episode_id: Option<&str>) -> Result<()> {
    graph
        .run(
            query(
                r#"
                MATCH ()-[r]->()
                WHERE id(r) = $rid AND r.validTo IS NULL
                SET r.validTo = datetime()
                SET r.retractedBy = $episode
                "#,
            )
            .param("rid", rel_id)
            .param("episode", episode_id.map(|s| s.to_string())),
        )
        .await?;
    Ok(())
}

async fn create_asserts(
    graph: &Graph,
    s: &str,
    o: &str,
    prop_iri: &str,
    layer: &str,
    episode: &Episode,
    reason: Option<&str>,
) -> Result<()> {
    graph
        .run(
            query(
                r#"
                MATCH (s:Entity {iri: $s}), (o:Entity {iri: $o})
                CREATE (s)-[r:ASSERTS {
                    propertyIri: $p,
                    layer: $layer,
                    validFrom: datetime(),
                    episodeId: $episode
                }]->(o)
                SET r.reason = $reason
                "#,
            )
            .param("s", s.to_string())
            .param("o", o.to_string())
            .param("p", prop_iri.to_string())
            .param("layer", layer.to_string())
            .param("episode", episode.iri.clone())
            .param("reason", reason.map(|s| s.to_string())),
        )
        .await?;
    Ok(())
}

async fn create_structural(
    graph: &Graph,
    s: &str,
    rel_type: &str,
    o: &str,
    prop_iri: &str,
    layer: &str,
    episode: &Episode,
    reason: Option<&str>,
) -> Result<()> {
    let rel = safe_rel(rel_type)?;
    let q = format!(
        r#"
        MATCH (s:Entity {{iri: $s}}), (o:Entity {{iri: $o}})
        CREATE (s)-[r:{rel} {{
            propertyIri: $p,
            layer: $layer,
            validFrom: datetime(),
            episodeId: $episode
        }}]->(o)
        SET r.reason = $reason
        "#
    );
    graph
        .run(
            query(&q)
                .param("s", s.to_string())
                .param("o", o.to_string())
                .param("p", prop_iri.to_string())
                .param("layer", layer.to_string())
                .param("episode", episode.iri.clone())
                .param("reason", reason.map(|s| s.to_string())),
        )
        .await?;
    Ok(())
}

async fn create_supersedes(
    graph: &Graph,
    new_o: &str,
    old_o: &str,
    prop_iri: &str,
    layer: &str,
    episode: &Episode,
) -> Result<()> {
    graph
        .run(
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
            .param("episode", episode.iri.clone()),
        )
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

pub async fn count_historical_asserts(
    graph: &Graph,
    s: &str,
    p: &str,
    layer: &str,
) -> Result<i64> {
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
    if args.s.is_some() || args.p.is_some() || args.o.is_some() {
        assert_writable_layer(&layer, project)?;
    }

    let episode = create_episode(
        graph,
        "memory_retract",
        args.reason.as_deref(),
    )
    .await?;

    let mut closed = 0i64;

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
                    WHERE r.validTo IS NULL AND ($anyLayer OR r.layer = $layer)
                    SET r.validTo = datetime(), r.retractedBy = $episode, r.reason = coalesce($reason, r.reason)
                    RETURN count(r) AS n
                    "#
                )
            } else {
                format!(
                    r#"
                    MATCH (s:Entity {{iri: $s}})-[r:{rel}]->(o:Entity)
                    WHERE r.validTo IS NULL AND ($anyLayer OR r.layer = $layer)
                    SET r.validTo = datetime(), r.retractedBy = $episode, r.reason = coalesce($reason, r.reason)
                    RETURN count(r) AS n
                    "#
                )
            };
            let mut qy = query(&q)
                .param("s", s_iri)
                .param("layer", layer.clone())
                .param("anyLayer", args.layer.is_none())
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
                WHERE r.validTo IS NULL AND r.propertyIri = $p AND ($anyLayer OR r.layer = $layer)
                SET r.validTo = datetime(), r.retractedBy = $episode, r.reason = coalesce($reason, r.reason)
                RETURN count(r) AS n
                "#
            } else {
                r#"
                MATCH (s:Entity {iri: $s})-[r:ASSERTS]->(o:Entity)
                WHERE r.validTo IS NULL AND r.propertyIri = $p AND ($anyLayer OR r.layer = $layer)
                SET r.validTo = datetime(), r.retractedBy = $episode, r.reason = coalesce($reason, r.reason)
                RETURN count(r) AS n
                "#
            };
            let mut qy = query(q)
                .param("s", s_iri)
                .param("p", prop)
                .param("layer", layer.clone())
                .param("anyLayer", args.layer.is_none())
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
        let row = fetch_one(
            graph,
            query(
                r#"
                MATCH (n:Entity {iri: $iri})-[r]->(o)
                WHERE r.validTo IS NULL AND (r.layer IS NULL OR r.layer IN $layers OR r.layer = $layer)
                SET r.validTo = datetime(), r.retractedBy = $episode, r.reason = coalesce($reason, r.reason)
                RETURN count(r) AS n
                "#,
            )
            .param("iri", iri.to_string())
            .param("layer", layer.clone())
            .param("layers", visible_layers(project))
            .param("episode", episode.iri.clone())
            .param("reason", args.reason.clone()),
        )
        .await?;
        closed = row.and_then(|r| r.get::<i64>("n").ok()).unwrap_or(0);
    } else {
        return Err(anyhow!(
            "memory_retract needs iri, or s+p (optional o, layer, reason)"
        ));
    }

    Ok(json!({
        "retracted": closed,
        "soft": true,
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
    let kind = args.kind.to_ascii_lowercase();
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
    let name = args
        .name
        .clone()
        .unwrap_or_else(|| name_from_iri(&iri));
    let label = if kind == "class" { "Class" } else { "Property" };
    let spec = NodeSpec {
        iri: Some(iri.clone()),
        name: Some(name.clone()),
        labels: vec![label.into()],
    };
    let node = merge_node(graph, &spec, &kind, &[]).await?;
    graph
        .run(
            query("MATCH (n:Entity {iri: $iri}) SET n.stub = false").param("iri", iri.clone()),
        )
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
    if let Some(cur) = find_current(graph, s, prop_iri, Some(rel), layer).await? {
        if cur.o_iri == o {
            return Ok(());
        }
        close_rel(graph, cur.rel_id, Some(&episode.iri)).await?;
    }
    create_structural(graph, s, rel, o, prop_iri, layer, episode, None).await
}

pub fn map_tool_error(err: anyhow::Error) -> rmcp::model::ErrorData {
    use rmcp::model::ErrorData as McpError;
    if let Some(layer) = err.downcast_ref::<LayerError>() {
        return McpError::invalid_params(layer.0.clone(), None);
    }
    McpError::internal_error(err.to_string(), None)
}

