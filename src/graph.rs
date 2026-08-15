use crate::config::Config;
use crate::iri::{
    default_lower_for_kind, kind_for_label, kind_from_iri, label_for_kind, mint_iri, name_from_iri,
    property_iri, slugify,
};
use anyhow::{anyhow, Context, Result};
use neo4rs::{query, Graph, Node, Path, Relation, Row, Txn, UnboundedRelation};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::time::{sleep, Duration};
use uuid::Uuid;

pub const FIXED_RELS: &[&str] = &[
    "INSTANCE_OF",
    "SUBCLASS_OF",
    "SUBPROPERTY_OF",
    "DOMAIN",
    "RANGE",
    "ASSERTS",
    "ABOUT",
    "EVIDENCE_FOR",
    "DERIVED_FROM",
    "SUPPORTS",
    "CONTRADICTS",
    "SUPERSEDES",
];

pub const STRUCTURAL: &[&str] = &[
    "INSTANCE_OF",
    "SUBCLASS_OF",
    "SUBPROPERTY_OF",
    "DOMAIN",
    "RANGE",
    "ABOUT",
    "EVIDENCE_FOR",
    "DERIVED_FROM",
    "SUPPORTS",
    "CONTRADICTS",
    "SUPERSEDES",
];

pub const SPIKE: &[&str] = &["Signal", "Pattern", "Insight", "Knowledge"];

pub const WAKEUP_RELS: &[&str] = &[
    "ASSERTS",
    "ABOUT",
    "INSTANCE_OF",
    "SUBCLASS_OF",
    "SUBPROPERTY_OF",
    "DOMAIN",
    "RANGE",
    "EVIDENCE_FOR",
    "DERIVED_FROM",
    "SUPPORTS",
];

const LABEL_OK: &str = "label";

pub async fn connect(cfg: &Config) -> Result<Graph> {
    let stripped = cfg
        .uri
        .trim_start_matches("bolt://")
        .trim_start_matches("neo4j://")
        .trim_start_matches("bolt+s://")
        .trim_start_matches("neo4j+s://")
        .to_string();
    let mut endpoints = vec![cfg.uri.clone()];
    if stripped != cfg.uri {
        endpoints.push(stripped);
    }

    let mut errors = Vec::new();
    for attempt in 1..=3 {
        for endpoint in &endpoints {
            match Graph::new(endpoint.as_str(), cfg.user.as_str(), cfg.password.as_str()).await {
                Ok(g) => {
                    if attempt > 1 {
                        eprintln!(
                            "{}",
                            json!({
                                "level": "info",
                                "event": "neo4j_connect_recovered",
                                "attempt": attempt,
                                "endpoint": endpoint
                            })
                        );
                    }
                    return Ok(g);
                }
                Err(err) => {
                    errors.push(format!("attempt={attempt} endpoint={endpoint} error={err}"))
                }
            }
        }
        if attempt < 3 {
            sleep(Duration::from_millis(250 * attempt as u64)).await;
        }
    }

    Err(anyhow!(
        "neo4j connect failed after retries: {}",
        errors.join(" | ")
    ))
}

pub async fn bootstrap(graph: &Graph) -> Result<()> {
    graph
        .run(query(
            "CREATE CONSTRAINT entity_iri IF NOT EXISTS FOR (n:Entity) REQUIRE n.iri IS UNIQUE",
        ))
        .await
        .context("create iri uniqueness constraint")?;
    graph
        .run(query(
            "CREATE INDEX entity_name IF NOT EXISTS FOR (n:Entity) ON (n.name)",
        ))
        .await
        .ok();
    if let Err(err) = graph
        .run(query(
            "CREATE FULLTEXT INDEX entity_fulltext IF NOT EXISTS FOR (n:Entity) ON EACH [n.name, n.iri]",
        ))
        .await
    {
        eprintln!("fulltext index skipped: {err}");
    }
    // Do not ALTER entity_fulltext — Neo4j cannot add properties in place.
    if let Err(err) = graph
        .run(query(
            "CREATE FULLTEXT INDEX wakeup_nodes IF NOT EXISTS FOR (n:Entity) ON EACH [n.name, n.iri, n.searchText, n.value]",
        ))
        .await
    {
        eprintln!("wakeup_nodes fulltext skipped: {err}");
    }
    if let Err(err) = graph
        .run(query(
            "CREATE FULLTEXT INDEX wakeup_facts IF NOT EXISTS FOR ()-[r:ASSERTS|ABOUT|INSTANCE_OF|SUBCLASS_OF|SUBPROPERTY_OF|DOMAIN|RANGE|EVIDENCE_FOR|DERIVED_FROM|SUPPORTS]-() ON EACH [r.factText]",
        ))
        .await
    {
        eprintln!("wakeup_facts multi-type fulltext skipped: {err}");
        if let Err(err2) = graph
            .run(query(
                "CREATE FULLTEXT INDEX wakeup_facts IF NOT EXISTS FOR ()-[r:ASSERTS]-() ON EACH [r.factText]",
            ))
            .await
        {
            eprintln!("wakeup_facts ASSERTS fulltext skipped: {err2}");
        }
    }
    if let Err(err) = graph
        .run(query(
            "CREATE FULLTEXT INDEX wakeup_about IF NOT EXISTS FOR ()-[r:ABOUT]-() ON EACH [r.factText]",
        ))
        .await
    {
        eprintln!("wakeup_about fulltext skipped: {err}");
    }
    graph
        .run(query(
            r#"
            MATCH (n:Entity)
            WHERE n.searchText IS NULL
            SET n.searchText = trim(coalesce(n.name, '') + ' ' + coalesce(n.iri, '') + ' ' + coalesce(n.value, ''))
            "#,
        ))
        .await
        .ok();
    graph
        .run(query(
            r#"
            MATCH (s:Entity)-[r]->(o:Entity)
            WHERE r.factText IS NULL
              AND type(r) IN ['ASSERTS','ABOUT','INSTANCE_OF','SUBCLASS_OF','SUBPROPERTY_OF','DOMAIN','RANGE','EVIDENCE_FOR','DERIVED_FROM','SUPPORTS']
            SET r.factText = trim(
              coalesce(s.name, s.iri) + ' ' +
              coalesce(last(split(coalesce(r.propertyIri, type(r)), '/')), type(r)) + ' ' +
              coalesce(o.value, o.name, o.iri)
            )
            "#,
        ))
        .await
        .ok();

    graph
        .run(query(
            r#"
            UNWIND [
              {iri: 'mindreader:class/Class', name: 'Class'},
              {iri: 'mindreader:class/Property', name: 'Property'},
              {iri: 'mindreader:class/Element', name: 'Element'}
            ] AS row
            MERGE (c:Entity:Class {iri: row.iri})
            ON CREATE SET c.name = row.name, c.createdAt = datetime()
            "#,
        ))
        .await
        .context("seed Class/Property/Element")?;

    graph
        .run(query(
            r#"
            UNWIND [
              {iri: 'mindreader:property/ABOUT', name: 'ABOUT'},
              {iri: 'mindreader:property/INSTANCE_OF', name: 'INSTANCE_OF'},
              {iri: 'mindreader:property/SUBCLASS_OF', name: 'SUBCLASS_OF'},
              {iri: 'mindreader:property/SUBPROPERTY_OF', name: 'SUBPROPERTY_OF'},
              {iri: 'mindreader:property/DOMAIN', name: 'DOMAIN'},
              {iri: 'mindreader:property/RANGE', name: 'RANGE'},
              {iri: 'mindreader:property/EVIDENCE_FOR', name: 'EVIDENCE_FOR'},
              {iri: 'mindreader:property/DERIVED_FROM', name: 'DERIVED_FROM'},
              {iri: 'mindreader:property/SUPPORTS', name: 'SUPPORTS'},
              {iri: 'mindreader:property/CONTRADICTS', name: 'CONTRADICTS'},
              {iri: 'mindreader:property/SUPERSEDES', name: 'SUPERSEDES'}
            ] AS row
            MERGE (p:Entity:Property {iri: row.iri})
            ON CREATE SET p.name = row.name, p.createdAt = datetime()
            "#,
        ))
        .await
        .context("seed structural properties")?;
    Ok(())
}

pub fn safe_label(label: &str) -> Result<String> {
    let Some(first) = label.chars().next() else {
        return Err(anyhow!("invalid label: {label}"));
    };
    if !first.is_ascii_alphabetic() || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(anyhow!("invalid label: {label}"));
    }
    let _ = LABEL_OK;
    Ok(label.to_string())
}

pub fn safe_rel(rel: &str) -> Result<String> {
    let up = rel.to_ascii_uppercase();
    let Some(first) = up.chars().next() else {
        return Err(anyhow!("invalid relationship type: {rel}"));
    };
    if !first.is_ascii_uppercase()
        || !up
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(anyhow!("invalid relationship type: {rel}"));
    }
    Ok(up)
}

pub fn structural_rel_for(property: &str) -> Option<String> {
    let iri = property_iri(property);
    let name = name_from_iri(&iri);
    let candidate = name.to_ascii_uppercase();
    if STRUCTURAL.contains(&candidate.as_str()) {
        return Some(candidate);
    }
    if STRUCTURAL
        .iter()
        .any(|s| iri == format!("mindreader:property/{s}"))
    {
        return Some(name.to_ascii_uppercase());
    }
    None
}

pub fn is_spike(label: &str) -> bool {
    SPIKE.contains(&label)
}

pub async fn fetch_one(graph: &Graph, q: neo4rs::Query) -> Result<Option<Row>> {
    let mut stream = graph.execute(q).await?;
    Ok(stream.next().await?)
}

pub async fn fetch_all(graph: &Graph, q: neo4rs::Query) -> Result<Vec<Row>> {
    let mut stream = graph.execute(q).await?;
    let mut rows = Vec::new();
    while let Some(row) = stream.next().await? {
        rows.push(row);
    }
    Ok(rows)
}

pub fn node_json(node: &Node) -> Value {
    let labels: Vec<String> = node
        .labels()
        .into_iter()
        .filter(|l| *l != "Entity")
        .map(|s| s.to_string())
        .collect();
    let iri = node.get::<String>("iri").unwrap_or_default();
    let name = node.get::<String>("name").ok();
    let mut obj = json!({
        "iri": iri,
        "name": name,
        "labels": labels,
    });
    if let Ok(v) = node.get::<String>("value") {
        obj["value"] = json!(v);
    }
    if let Ok(v) = node.get::<String>("datatype") {
        obj["datatype"] = json!(v);
    }
    if let Ok(v) = node.get::<bool>("stub") {
        obj["stub"] = json!(v);
    }
    if let Ok(v) = node.get::<String>("tool") {
        obj["tool"] = json!(v);
    }
    obj
}

pub fn rel_json(rel: &Relation, from: &str, to: &str) -> Value {
    let mut obj = json!({
        "type": rel.typ(),
        "from": from,
        "to": to,
    });
    if let Ok(v) = rel.get::<String>("propertyIri") {
        obj["propertyIri"] = json!(v);
    }
    if let Ok(v) = rel.get::<String>("layer") {
        obj["layer"] = json!(v);
    }
    if let Ok(v) = rel.get::<String>("episodeId") {
        obj["episodeId"] = json!(v);
    }
    if let Ok(v) = rel.get::<String>("reason") {
        obj["reason"] = json!(v);
    }
    obj
}

fn unbounded_rel_json(rel: &UnboundedRelation, from: &str, to: &str) -> Value {
    let mut obj = json!({
        "type": rel.typ(),
        "from": from,
        "to": to,
    });
    if let Ok(v) = rel.get::<String>("propertyIri") {
        obj["propertyIri"] = json!(v);
    }
    if let Ok(v) = rel.get::<String>("layer") {
        obj["layer"] = json!(v);
    }
    if let Ok(v) = rel.get::<String>("episodeId") {
        obj["episodeId"] = json!(v);
    }
    if let Ok(v) = rel.get::<String>("reason") {
        obj["reason"] = json!(v);
    }
    obj
}

fn node_iri(node: &Node) -> String {
    node.get::<String>("iri").unwrap_or_default()
}

pub fn path_to_json(path: &Path) -> (Vec<Value>, Vec<Value>, Vec<String>) {
    let nodes = path.nodes();
    let rels = path.rels();
    let indices = path.indices();
    let node_jsons: Vec<Value> = nodes.iter().map(node_json).collect();
    let iris: Vec<String> = nodes.iter().map(node_iri).collect();

    let mut edges = Vec::new();
    let mut cursor = 0usize;
    let mut k = 0usize;
    while k + 1 < indices.len() {
        let rel_signed = indices[k];
        let next = indices[k + 1] as usize;
        k += 2;
        let abs = rel_signed.unsigned_abs() as usize;
        if abs == 0 || abs > rels.len() || next >= nodes.len() || cursor >= nodes.len() {
            continue;
        }
        let (from_i, to_i) = if rel_signed >= 0 {
            (cursor, next)
        } else {
            (next, cursor)
        };
        edges.push(unbounded_rel_json(
            &rels[abs - 1],
            &node_iri(&nodes[from_i]),
            &node_iri(&nodes[to_i]),
        ));
        cursor = next;
    }
    if edges.is_empty() {
        for (i, rel) in rels.iter().enumerate() {
            let from = nodes.get(i).map(node_iri).unwrap_or_default();
            let to = nodes.get(i + 1).map(node_iri).unwrap_or_default();
            edges.push(unbounded_rel_json(rel, &from, &to));
        }
    }
    (node_jsons, edges, iris)
}

#[derive(Debug, Clone)]
pub struct MergedNode {
    pub iri: String,
    pub name: String,
    pub labels: Vec<String>,
    pub created: bool,
    pub json: Value,
}

#[derive(Debug, Clone, Default)]
pub struct NodeSpec {
    pub iri: Option<String>,
    pub name: Option<String>,
    pub labels: Vec<String>,
}

pub fn infer_kind(spec: &NodeSpec, fallback: &str) -> String {
    if let Some(iri) = &spec.iri {
        if let Some(k) = kind_from_iri(iri) {
            return k;
        }
    }
    for l in &spec.labels {
        if let Some(k) = kind_for_label(l) {
            return k.to_string();
        }
    }
    fallback.to_string()
}

pub async fn merge_node(
    graph: &Graph,
    spec: &NodeSpec,
    default_kind: &str,
    extra_labels: &[String],
) -> Result<MergedNode> {
    let kind = infer_kind(spec, default_kind);
    let iri = if let Some(iri) = spec.iri.as_deref().filter(|s| !s.is_empty()) {
        iri.to_string()
    } else {
        let seed = spec.name.as_deref().unwrap_or("unnamed");
        mint_iri(&kind, seed, default_lower_for_kind(&kind))
    };
    let name = spec.name.clone().unwrap_or_else(|| name_from_iri(&iri));
    let mut labels = Vec::new();
    if let Some(l) = label_for_kind(&kind) {
        labels.push(l.to_string());
    }
    labels.extend(spec.labels.iter().cloned());
    labels.extend(extra_labels.iter().cloned());
    labels.retain(|l| l != "Entity");
    let mut seen = std::collections::HashSet::new();
    labels.retain(|l| seen.insert(l.clone()));
    for l in &labels {
        safe_label(l)?;
    }

    let row = fetch_one(
        graph,
        query(
            r#"
            OPTIONAL MATCH (existing:Entity {iri: $iri})
            MERGE (n:Entity {iri: $iri})
            ON CREATE SET n.name = $name, n.createdAt = datetime(),
              n.searchText = trim($name + ' ' + $iri)
            ON MATCH SET n.name = coalesce(n.name, $name),
              n.searchText = coalesce(n.searchText, trim(coalesce(n.name, $name) + ' ' + $iri))
            RETURN n, existing IS NULL AS created
            "#,
        )
        .param("iri", iri.clone())
        .param("name", name.clone()),
    )
    .await?
    .ok_or_else(|| anyhow!("failed to MERGE node {iri}"))?;
    let created: bool = row.get("created").unwrap_or(false);

    if !labels.is_empty() {
        let set_labels = labels
            .iter()
            .map(|l| safe_label(l))
            .collect::<Result<Vec<_>>>()?;
        let set = set_labels
            .iter()
            .map(|l| format!("n:{l}"))
            .collect::<Vec<_>>()
            .join(" SET ");
        let q = format!("MATCH (n:Entity {{iri: $iri}}) SET {set} RETURN n");
        graph.run(query(&q).param("iri", iri.clone())).await?;
    }

    let after = fetch_one(
        graph,
        query("MATCH (n:Entity {iri: $iri}) RETURN n").param("iri", iri.clone()),
    )
    .await?
    .ok_or_else(|| anyhow!("missing node after MERGE {iri}"))?;
    let node: Node = after.get("n")?;
    let labels = node
        .labels()
        .into_iter()
        .filter(|l| *l != "Entity")
        .map(|s| s.to_string())
        .collect();
    Ok(MergedNode {
        iri,
        name,
        labels,
        created,
        json: node_json(&node),
    })
}

pub async fn merge_literal(graph: &Graph, value: &str, datatype: &str) -> Result<MergedNode> {
    let mut hasher = Sha256::new();
    hasher.update(format!("{datatype}:{value}").as_bytes());
    let digest = hasher.finalize();
    let hex: String = digest.iter().take(6).map(|b| format!("{b:02x}")).collect();
    let slug = slugify(value, true);
    let slug = if slug.len() > 40 { &slug[..40] } else { &slug };
    let iri = format!("mindreader:literal/{slug}-{hex}");
    let row = fetch_one(
        graph,
        query(
            r#"
            OPTIONAL MATCH (existing:Entity {iri: $iri})
            MERGE (n:Entity:Literal {iri: $iri})
            ON CREATE SET n.name = $name, n.value = $value, n.datatype = $datatype, n.createdAt = datetime(),
              n.searchText = trim($name + ' ' + $iri + ' ' + $value)
            ON MATCH SET n.searchText = coalesce(n.searchText, trim(coalesce(n.name, $name) + ' ' + $iri + ' ' + coalesce(n.value, $value)))
            RETURN n, existing IS NULL AS created
            "#,
        )
        .param("iri", iri.clone())
        .param("name", value.to_string())
        .param("value", value.to_string())
        .param("datatype", datatype.to_string()),
    )
    .await?
    .ok_or_else(|| anyhow!("failed to MERGE literal {iri}"))?;
    let node: Node = row.get("n")?;
    let created: bool = row.get("created").unwrap_or(false);
    Ok(MergedNode {
        iri,
        name: value.to_string(),
        labels: vec!["Literal".into()],
        created,
        json: node_json(&node),
    })
}

pub async fn ensure_property(graph: &Graph, p: &str) -> Result<(String, bool, Value)> {
    let iri = property_iri(p);
    let name = name_from_iri(&iri);
    let row = fetch_one(
        graph,
        query(
            r#"
            OPTIONAL MATCH (existing:Entity {iri: $iri})
            MERGE (n:Entity:Property {iri: $iri})
            ON CREATE SET n.name = $name, n.createdAt = datetime(), n.stub = true
            RETURN n, existing IS NULL AS created
            "#,
        )
        .param("iri", iri.clone())
        .param("name", name),
    )
    .await?
    .ok_or_else(|| anyhow!("failed to MERGE property {iri}"))?;
    let node: Node = row.get("n")?;
    let created: bool = row.get("created").unwrap_or(false);
    Ok((iri, created, node_json(&node)))
}

#[derive(Debug, Clone)]
pub struct Episode {
    pub iri: String,
    pub at: String,
    pub tool: String,
}

fn episode_query(iri: &str, tool: &str, note: Option<&str>) -> neo4rs::Query {
    query(
        r#"
        CREATE (e:Entity:Episode {iri: $iri, tool: $tool, at: datetime(), createdAt: datetime(), name: $iri})
        SET e.note = $note
        RETURN e.iri AS iri, toString(e.at) AS at
        "#,
    )
    .param("iri", iri.to_string())
    .param("tool", tool.to_string())
    .param("note", note.map(|s| s.to_string()))
}

fn episode_from_row(row: &Row, tool: &str) -> Result<Episode> {
    Ok(Episode {
        iri: row.get::<String>("iri")?,
        at: row.get::<String>("at").unwrap_or_default(),
        tool: tool.to_string(),
    })
}

pub async fn create_episode(graph: &Graph, tool: &str, note: Option<&str>) -> Result<Episode> {
    let iri = mint_iri("episode", &Uuid::new_v4().to_string(), true);
    let row = fetch_one(graph, episode_query(&iri, tool, note))
        .await?
        .ok_or_else(|| anyhow!("failed to create episode"))?;
    episode_from_row(&row, tool)
}

pub async fn create_episode_in_txn(
    txn: &mut Txn,
    tool: &str,
    note: Option<&str>,
) -> Result<Episode> {
    let iri = mint_iri("episode", &Uuid::new_v4().to_string(), true);
    let mut stream = txn.execute(episode_query(&iri, tool, note)).await?;
    let row = stream
        .next(txn.handle())
        .await?
        .ok_or_else(|| anyhow!("failed to create episode"))?;
    episode_from_row(&row, tool)
}

pub async fn get_node(graph: &Graph, iri: &str) -> Result<Option<Node>> {
    let row = fetch_one(
        graph,
        query("MATCH (n:Entity {iri: $iri}) RETURN n").param("iri", iri.to_string()),
    )
    .await?;
    Ok(match row {
        Some(r) => Some(r.get("n")?),
        None => None,
    })
}

pub fn fact_text(s_name: &str, s_iri: &str, prop_iri: &str, o: &MergedNode) -> String {
    let s_part = if !s_name.is_empty() { s_name } else { s_iri };
    let p_part = name_from_iri(prop_iri);
    let o_part = o
        .json
        .get("value")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            if !o.name.is_empty() {
                Some(o.name.as_str())
            } else {
                None
            }
        })
        .unwrap_or(o.iri.as_str());
    format!("{s_part} {p_part} {o_part}")
}

pub fn endpoint_json(node: &Node) -> Value {
    let labels: Vec<String> = node
        .labels()
        .into_iter()
        .filter(|l| *l != "Entity")
        .map(|s| s.to_string())
        .collect();
    let iri = node.get::<String>("iri").unwrap_or_default();
    if labels.iter().any(|l| l == "Literal") {
        let value = node
            .get::<String>("value")
            .ok()
            .or_else(|| node.get::<String>("name").ok())
            .unwrap_or_default();
        let datatype = node
            .get::<String>("datatype")
            .unwrap_or_else(|_| "xsd:string".into());
        return json!({
            "iri": iri,
            "value": value,
            "datatype": datatype,
        });
    }
    json!({
        "iri": iri,
        "name": node.get::<String>("name").ok(),
        "labels": labels,
    })
}

pub async fn touch_search_text(graph: &Graph, iri: &str, extra: Option<&str>) -> Result<()> {
    graph
        .run(
            query(
                r#"
                MATCH (n:Entity {iri: $iri})
                WITH n, trim(coalesce(n.name, '') + ' ' + n.iri + ' ' + coalesce(n.value, '')) AS base
                SET n.searchText = CASE
                  WHEN $extra IS NULL OR $extra = '' THEN coalesce(n.searchText, base)
                  WHEN coalesce(n.searchText, '') CONTAINS $extra THEN n.searchText
                  ELSE trim(coalesce(n.searchText, base) + ' ' + $extra)
                END
                "#,
            )
            .param("iri", iri.to_string())
            .param("extra", extra.map(|s| s.to_string())),
        )
        .await?;
    Ok(())
}

pub fn spike_label(labels: &[String]) -> Option<String> {
    for rank in ["Knowledge", "Insight", "Pattern", "Signal"] {
        if labels.iter().any(|l| l == rank) {
            return Some(rank.to_string());
        }
    }
    None
}

pub fn spike_rank(label: Option<&str>) -> i32 {
    match label {
        Some("Knowledge") => 4,
        Some("Insight") => 3,
        Some("Pattern") => 2,
        Some("Signal") => 1,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::{safe_label, safe_rel, spike_rank};

    #[test]
    fn spike_rank_orders() {
        assert!(spike_rank(Some("Knowledge")) > spike_rank(Some("Insight")));
        assert!(spike_rank(Some("Insight")) > spike_rank(Some("Pattern")));
        assert!(spike_rank(Some("Pattern")) > spike_rank(Some("Signal")));
        assert!(spike_rank(Some("Signal")) > spike_rank(None));
    }

    #[test]
    fn safe_label_rejects_injection_shapes() {
        assert!(safe_label("Element").is_ok());
        assert!(safe_label("").is_err());
        assert!(safe_label("1Element").is_err());
        assert!(safe_label("Element-Bad").is_err());
        assert!(safe_label("Element SET n.pwned=true").is_err());
    }

    #[test]
    fn safe_rel_rejects_injection_shapes() {
        assert_eq!(safe_rel("asserts").unwrap(), "ASSERTS");
        assert!(safe_rel("").is_err());
        assert!(safe_rel("ASSERTS-BAD").is_err());
        assert!(safe_rel("ASSERTS MATCH (n)").is_err());
    }
}
