use crate::config::{Config, EmbeddingSpace};
use crate::domain::literal_iri;
use crate::iri::{
    default_lower_for_kind, kind_for_label, kind_from_iri, label_for_kind, mint_iri, name_from_iri,
    property_iri,
};
use crate::{
    error::{Context, Error, Result},
    graph_error,
};
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

pub const MODEL_MARKER_KEY: &str = "model";
pub const MODEL_VERSION: i64 = 4;
pub const SEMANTIC_INDEX: &str = "semantic_activation_embeddings";
const EMBEDDING_MARKER_KEY: &str = "embedding";

const RESET_REQUIRED: &str = "recreate the Neo4j database or volume before starting Mindreader";
const REQUIRED_FULLTEXT_INDEXES: &[(&str, &str)] =
    &[("wakeup_nodes", "NODE"), ("wakeup_facts", "RELATIONSHIP")];
const WAKEUP_NODE_PROPERTIES: &[&str] = &["name", "iri", "searchText", "value"];

pub async fn connect(cfg: &Config) -> Result<Graph> {
    let password = cfg.neo4j_password()?;
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
    let mut last_error = None;
    for attempt in 1..=3 {
        for endpoint in &endpoints {
            match Graph::new(endpoint.as_str(), cfg.user.as_str(), password).await {
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
                    errors.push(format!("attempt={attempt} endpoint={endpoint} error={err}"));
                    last_error = Some(err);
                }
            }
        }
        if attempt < 3 {
            sleep(Duration::from_millis(250 * attempt as u64)).await;
        }
    }

    let message = format!("neo4j connect failed after retries: {}", errors.join(" | "));
    match last_error {
        Some(error) => Err(Error::from(error).context(message)),
        None => Err(graph_error!("{message}")),
    }
}

pub async fn bootstrap(graph: &Graph, embedding: Option<&EmbeddingSpace>) -> Result<()> {
    ensure_model_marker(graph).await?;
    verify_required_apoc(graph).await?;

    graph
        .run(query(
            "CREATE CONSTRAINT entity_iri IF NOT EXISTS FOR (n:Entity) REQUIRE n.iri IS UNIQUE",
        ))
        .await
        .context("create iri uniqueness constraint")?;
    graph
        .run(query(
            "CREATE CONSTRAINT fact_lock_key IF NOT EXISTS FOR (n:FactLock) REQUIRE n.key IS UNIQUE",
        ))
        .await
        .context("create fact lock key uniqueness constraint")?;

    graph
        .run(query(
            "CREATE FULLTEXT INDEX wakeup_nodes IF NOT EXISTS FOR (n:Entity) ON EACH [n.name, n.iri, n.searchText, n.value]",
        ))
        .await
        .context("create required wakeup_nodes full-text index")?;
    graph
        .run(query(
            "CREATE FULLTEXT INDEX wakeup_facts IF NOT EXISTS FOR ()-[r:ASSERTS|ABOUT|INSTANCE_OF|SUBCLASS_OF|SUBPROPERTY_OF|DOMAIN|RANGE|EVIDENCE_FOR|DERIVED_FROM|SUPPORTS]-() ON EACH [r.factText]",
        ))
        .await
        .context("create required wakeup_facts full-text index")?;

    if let Some(embedding) = embedding {
        ensure_semantic_index(graph, embedding).await?;
    }

    graph
        .run(query(
            r#"
            UNWIND [
              {iri: 'mindreader:class/Class', name: 'Class'},
              {iri: 'mindreader:class/Property', name: 'Property'},
              {iri: 'mindreader:class/Element', name: 'Element'}
            ] AS row
            MERGE (c:Entity:Class {iri: row.iri})
            ON CREATE SET c.name = row.name, c.createdAt = datetime(),
              c.weight = 0, c.weightText = '0', c.layers = []
            SET c.searchText = trim(coalesce(c.name, row.name) + ' ' + c.iri)
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
            ON CREATE SET p.name = row.name, p.createdAt = datetime(),
              p.weight = 0, p.weightText = '0', p.layers = []
            SET p.searchText = trim(coalesce(p.name, row.name) + ' ' + p.iri)
            "#,
        ))
        .await
        .context("seed structural properties")?;

    graph
        .run(query("CALL db.awaitIndexes($timeout_seconds)").param("timeout_seconds", 300_i64))
        .await
        .context("wait for required Neo4j indexes to become online")?;
    verify_required_constraints(graph).await?;
    verify_required_fulltext_indexes(graph).await?;
    if embedding.is_some() {
        verify_semantic_index(graph).await?;
    }

    Ok(())
}

async fn verify_required_apoc(graph: &Graph) -> Result<()> {
    let functions = fetch_all(
        graph,
        query(
            "SHOW FUNCTIONS YIELD name WHERE name IN [\
             'apoc.text.fuzzyMatch', 'apoc.text.levenshteinSimilarity'] \
             RETURN collect(name) AS names",
        ),
    )
    .await
    .context("inspect required APOC functions")?
    .into_iter()
    .next()
    .and_then(|row| row.get::<Vec<String>>("names").ok())
    .unwrap_or_default();
    for name in ["apoc.text.fuzzyMatch", "apoc.text.levenshteinSimilarity"] {
        if !functions.iter().any(|actual| actual == name) {
            return Err(graph_error!(
                "required APOC function {name} is unavailable; install matching APOC Core"
            ));
        }
    }

    let procedures = fetch_all(
        graph,
        query(
            "SHOW PROCEDURES YIELD name WHERE name IN [\
             'apoc.config.list', 'apoc.merge.node', 'apoc.refactor.mergeNodes', \
             'apoc.ttl.expireIn'] \
             RETURN collect(name) AS names",
        ),
    )
    .await
    .context("inspect required APOC procedures")?
    .into_iter()
    .next()
    .and_then(|row| row.get::<Vec<String>>("names").ok())
    .unwrap_or_default();
    for name in [
        "apoc.config.list",
        "apoc.merge.node",
        "apoc.refactor.mergeNodes",
        "apoc.ttl.expireIn",
    ] {
        if !procedures.iter().any(|actual| actual == name) {
            return Err(graph_error!(
                "required APOC procedure {name} is unavailable; install matching APOC Core and APOC Extended"
            ));
        }
    }
    let ttl_enabled = fetch_one(
        graph,
        query(
            "CALL apoc.config.list() YIELD key, value \
             WHERE key = 'apoc.ttl.enabled' \
             RETURN toString(value) AS value",
        ),
    )
    .await
    .context("read APOC TTL configuration")?
    .and_then(|row| row.get::<String>("value").ok())
    .is_some_and(|value| value.eq_ignore_ascii_case("true"));
    if !ttl_enabled {
        return Err(graph_error!(
            "APOC TTL is disabled; set apoc.ttl.enabled=true before starting Mindreader"
        ));
    }
    Ok(())
}

async fn ensure_semantic_index(graph: &Graph, embedding: &EmbeddingSpace) -> Result<()> {
    if !(1..=4096).contains(&embedding.dimensions) {
        return Err(graph_error!(
            "embedding dimensions must be between 1 and 4096"
        ));
    }
    let marker = fetch_one(
        graph,
        query(
            "MATCH (m:MindreaderMeta {key: $key}) \
             RETURN m.provider AS provider, m.model AS model, m.dimensions AS dimensions",
        )
        .param("key", EMBEDDING_MARKER_KEY),
    )
    .await
    .context("read embedding-space marker")?;
    let matches = marker.as_ref().is_some_and(|row| {
        row.get::<String>("provider").ok().as_deref() == Some(embedding.provider.as_str())
            && row.get::<String>("model").ok().as_deref() == Some(embedding.model.as_str())
            && row.get::<i64>("dimensions").ok() == Some(embedding.dimensions as i64)
    });
    if !matches {
        graph
            .run(query(&format!("DROP INDEX {SEMANTIC_INDEX} IF EXISTS")))
            .await
            .context("drop incompatible semantic activation index")?;
        graph
            .run(query("MATCH (a:SemanticActivation) DETACH DELETE a"))
            .await
            .context("discard activations from an incompatible embedding space")?;
        graph
            .run(
                query(
                    "MERGE (m:MindreaderMeta {key: $key}) \
                     SET m.provider = $provider, m.model = $model, m.dimensions = $dimensions",
                )
                .param("key", EMBEDDING_MARKER_KEY)
                .param("provider", embedding.provider.clone())
                .param("model", embedding.model.clone())
                .param("dimensions", embedding.dimensions as i64),
            )
            .await
            .context("record active embedding space")?;
    }
    let create = format!(
        "CREATE VECTOR INDEX {SEMANTIC_INDEX} IF NOT EXISTS \
         FOR (a:SemanticActivation) ON (a.embedding) \
         OPTIONS {{indexConfig: {{`vector.dimensions`: {}, \
         `vector.similarity_function`: 'cosine'}}}}",
        embedding.dimensions
    );
    graph
        .run(query(&create))
        .await
        .context("create semantic activation vector index")?;
    Ok(())
}

async fn verify_semantic_index(graph: &Graph) -> Result<()> {
    let row = fetch_one(
        graph,
        query(
            "SHOW VECTOR INDEXES YIELD name, state, entityType, labelsOrTypes, properties \
             WHERE name = $name \
             RETURN state, entityType, labelsOrTypes, properties",
        )
        .param("name", SEMANTIC_INDEX),
    )
    .await
    .context("inspect semantic activation vector index")?
    .ok_or_else(|| graph_error!("required vector index {SEMANTIC_INDEX} is missing"))?;
    let state = row.get::<String>("state")?;
    let entity_type = row.get::<String>("entityType")?;
    let labels = row.get::<Vec<String>>("labelsOrTypes")?;
    let properties = row.get::<Vec<String>>("properties")?;
    if state != "ONLINE"
        || entity_type != "NODE"
        || !same_string_members(&labels, &["SemanticActivation"])
        || !same_string_members(&properties, &["embedding"])
    {
        return Err(graph_error!(
            "required vector index {SEMANTIC_INDEX} is incompatible or not online"
        ));
    }
    Ok(())
}

async fn verify_required_constraints(graph: &Graph) -> Result<()> {
    let rows = fetch_all(
        graph,
        query(
            r#"
            SHOW CONSTRAINTS
            YIELD name, type, entityType, labelsOrTypes, properties
            WHERE name IN ['mindreader_meta_key', 'entity_iri', 'fact_lock_key']
            RETURN name, type AS constraintType, entityType, labelsOrTypes, properties
            "#,
        ),
    )
    .await
    .context("inspect required Neo4j constraints")?;

    for (name, label, property) in [
        ("mindreader_meta_key", "MindreaderMeta", "key"),
        ("entity_iri", "Entity", "iri"),
        ("fact_lock_key", "FactLock", "key"),
    ] {
        let row = rows
            .iter()
            .find(|row| matches!(row.get::<String>("name"), Ok(actual) if actual == name))
            .ok_or_else(|| graph_error!("required Neo4j constraint {name} is missing"))?;
        let constraint_type = row
            .get::<String>("constraintType")
            .with_context(|| format!("read type of Neo4j constraint {name}"))?;
        let entity_type = row
            .get::<String>("entityType")
            .with_context(|| format!("read entity type of Neo4j constraint {name}"))?;
        let labels_or_types = row
            .get::<Vec<String>>("labelsOrTypes")
            .with_context(|| format!("read labels of Neo4j constraint {name}"))?;
        let properties = row
            .get::<Vec<String>>("properties")
            .with_context(|| format!("read properties of Neo4j constraint {name}"))?;
        if constraint_type != "UNIQUENESS"
            || entity_type != "NODE"
            || !same_string_members(&labels_or_types, &[label])
            || !same_string_members(&properties, &[property])
        {
            return Err(graph_error!(
                "required Neo4j constraint {name} has an incompatible definition: type={constraint_type}, entityType={entity_type}, labelsOrTypes={labels_or_types:?}, properties={properties:?}; {RESET_REQUIRED}"
            ));
        }
    }
    Ok(())
}

async fn ensure_model_marker(graph: &Graph) -> Result<()> {
    let markers = fetch_all(
        graph,
        query("MATCH (m:MindreaderMeta {key: $key}) RETURN m.version AS version")
            .param("key", MODEL_MARKER_KEY),
    )
    .await
    .context("read Mindreader database model marker")?;

    if markers.len() > 1 {
        return Err(graph_error!(
            "multiple Mindreader database model markers found; {RESET_REQUIRED}"
        ));
    }

    if let Some(marker) = markers.first() {
        let version = marker.get::<i64>("version").map_err(|_| {
            graph_error!("invalid Mindreader database model marker; {RESET_REQUIRED}")
        })?;
        validate_model_version(version)?;
        ensure_model_marker_constraint(graph).await?;
        return Ok(());
    }

    let node_count = fetch_one(graph, query("MATCH (n) RETURN count(n) AS nodeCount"))
        .await
        .context("check whether the Neo4j database is empty")?
        .ok_or_else(|| graph_error!("Neo4j did not return a node count"))?
        .get::<i64>("nodeCount")
        .context("read Neo4j node count")?;

    if node_count > 0 {
        let concurrent_markers = fetch_all(
            graph,
            query("MATCH (m:MindreaderMeta {key: $key}) RETURN m.version AS version")
                .param("key", MODEL_MARKER_KEY),
        )
        .await
        .context("recheck model marker after concurrent bootstrap activity")?;
        if concurrent_markers.len() == 1
            && concurrent_markers[0].get::<i64>("version").ok() == Some(MODEL_VERSION)
        {
            ensure_model_marker_constraint(graph).await?;
            return Ok(());
        }
        return Err(graph_error!(
            "found {node_count} pre-existing node(s) without a Mindreader database model marker; no data migration is supported, so {RESET_REQUIRED}"
        ));
    }

    ensure_model_marker_constraint(graph).await?;

    graph
        .run(
            query(
                "MERGE (m:MindreaderMeta {key: $key}) ON CREATE SET m.version = $version, m.createdAt = datetime()",
            )
            .param("key", MODEL_MARKER_KEY)
            .param("version", MODEL_VERSION),
        )
        .await
        .context("create Mindreader database model marker")?;

    Ok(())
}

fn validate_model_version(version: i64) -> Result<()> {
    if version == MODEL_VERSION {
        return Ok(());
    }
    Err(graph_error!(
        "Mindreader database model version {version} is incompatible with required version {MODEL_VERSION}; {RESET_REQUIRED}"
    ))
}

async fn ensure_model_marker_constraint(graph: &Graph) -> Result<()> {
    graph
        .run(query(
            "CREATE CONSTRAINT mindreader_meta_key IF NOT EXISTS FOR (m:MindreaderMeta) REQUIRE m.key IS UNIQUE",
        ))
        .await
        .context("create model marker key uniqueness constraint")?;
    Ok(())
}

async fn verify_required_fulltext_indexes(graph: &Graph) -> Result<()> {
    let rows = fetch_all(
        graph,
        query(
            r#"
            SHOW INDEXES
            YIELD name, type, entityType, labelsOrTypes, properties, state
            WHERE name IN ['wakeup_nodes', 'wakeup_facts']
            RETURN name, type AS indexType, entityType, labelsOrTypes, properties, state
            "#,
        ),
    )
    .await
    .context("inspect required Neo4j full-text indexes")?;

    for (required_name, required_entity_type) in REQUIRED_FULLTEXT_INDEXES {
        let row = rows
            .iter()
            .find(|row| matches!(row.get::<String>("name"), Ok(name) if name == *required_name))
            .ok_or_else(|| {
                graph_error!("required Neo4j full-text index {required_name} is missing")
            })?;
        let index_type = row
            .get::<String>("indexType")
            .with_context(|| format!("read type of Neo4j index {required_name}"))?;
        let entity_type = row
            .get::<String>("entityType")
            .with_context(|| format!("read entity type of Neo4j index {required_name}"))?;
        let state = row
            .get::<String>("state")
            .with_context(|| format!("read state of Neo4j index {required_name}"))?;
        let labels_or_types = row
            .get::<Vec<String>>("labelsOrTypes")
            .with_context(|| format!("read labels or types of Neo4j index {required_name}"))?;
        let properties = row
            .get::<Vec<String>>("properties")
            .with_context(|| format!("read properties of Neo4j index {required_name}"))?;

        let (expected_labels_or_types, expected_properties): (&[&str], &[&str]) =
            match *required_name {
                "wakeup_nodes" => (&["Entity"], WAKEUP_NODE_PROPERTIES),
                "wakeup_facts" => (WAKEUP_RELS, &["factText"]),
                _ => unreachable!("required full-text index catalog is exhaustive"),
            };

        if index_type != "FULLTEXT"
            || entity_type != *required_entity_type
            || state != "ONLINE"
            || !same_string_members(&labels_or_types, expected_labels_or_types)
            || !same_string_members(&properties, expected_properties)
        {
            return Err(graph_error!(
                "required Neo4j index {required_name} has an incompatible definition or is not ready: type={index_type}, entityType={entity_type}, labelsOrTypes={labels_or_types:?}, properties={properties:?}, state={state}; {RESET_REQUIRED}"
            ));
        }
    }

    Ok(())
}

fn same_string_members(actual: &[String], expected: &[&str]) -> bool {
    actual.len() == expected.len()
        && expected
            .iter()
            .all(|expected| actual.iter().any(|actual| actual == expected))
}

pub fn safe_label(label: &str) -> Result<String> {
    let Some(first) = label.chars().next() else {
        return Err(graph_error!("invalid label: {label}"));
    };
    if !first.is_ascii_alphabetic() || !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
    {
        return Err(graph_error!("invalid label: {label}"));
    }
    let _ = LABEL_OK;
    Ok(label.to_string())
}

pub fn safe_rel(rel: &str) -> Result<String> {
    let up = rel.to_ascii_uppercase();
    let Some(first) = up.chars().next() else {
        return Err(graph_error!("invalid relationship type: {rel}"));
    };
    if !first.is_ascii_uppercase()
        || !up
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(graph_error!("invalid relationship type: {rel}"));
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

/// Execute a query through an existing transaction and return its first row.
///
/// Keeping the transaction handle in the row stream is required by neo4rs; callers
/// must not issue graph-level queries while the transaction is open.
pub async fn fetch_one_txn(txn: &mut Txn, q: neo4rs::Query) -> Result<Option<Row>> {
    let mut stream = txn.execute(q).await?;
    Ok(stream.next(txn.handle()).await?)
}

/// Execute a query through an existing transaction and exhaust its row stream.
pub async fn fetch_all_txn(txn: &mut Txn, q: neo4rs::Query) -> Result<Vec<Row>> {
    let mut stream = txn.execute(q).await?;
    let mut rows = Vec::new();
    while let Some(row) = stream.next(txn.handle()).await? {
        rows.push(row);
    }
    Ok(rows)
}

// neo4rs 0.8 can decode negative Bolt integers as unsigned values. `weight`
// remains the numeric source of truth in Neo4j; `weightText` is its canonical
// decimal mirror at the driver boundary so signed values round-trip correctly.
fn node_weight(node: &Node) -> i64 {
    node.get::<String>("weightText")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| node.get::<i64>("weight").unwrap_or(0))
}

fn relation_weight(rel: &Relation) -> i64 {
    rel.get::<String>("weightText")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| rel.get::<i64>("weight").unwrap_or(0))
}

fn unbounded_relation_weight(rel: &UnboundedRelation) -> i64 {
    rel.get::<String>("weightText")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or_else(|| rel.get::<i64>("weight").unwrap_or(0))
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
    obj["layers"] = json!(node.get::<Vec<String>>("layers").unwrap_or_default());
    obj["weight"] = json!(node_weight(node));
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
    if let Ok(v) = rel.get::<String>("iri") {
        obj["iri"] = json!(v);
    }
    if let Ok(v) = rel.get::<String>("episodeId") {
        obj["episodeId"] = json!(v);
    }
    if let Ok(v) = rel.get::<String>("reason") {
        obj["reason"] = json!(v);
    }
    obj["layers"] = json!(rel.get::<Vec<String>>("layers").unwrap_or_default());
    obj["weight"] = json!(relation_weight(rel));
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
    if let Ok(v) = rel.get::<String>("iri") {
        obj["iri"] = json!(v);
    }
    if let Ok(v) = rel.get::<String>("episodeId") {
        obj["episodeId"] = json!(v);
    }
    if let Ok(v) = rel.get::<String>("reason") {
        obj["reason"] = json!(v);
    }
    obj["layers"] = json!(rel.get::<Vec<String>>("layers").unwrap_or_default());
    obj["weight"] = json!(unbounded_relation_weight(rel));
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct FactLockSpec {
    key: String,
    subject_iri: String,
    predicate_iri: String,
    layer: String,
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

fn resolved_node_parts(
    spec: &NodeSpec,
    default_kind: &str,
    extra_labels: &[String],
) -> Result<(String, String, Vec<String>)> {
    let kind = infer_kind(spec, default_kind);
    let iri = if let Some(iri) = spec.iri.as_deref().filter(|value| !value.is_empty()) {
        iri.to_string()
    } else {
        let seed = spec.name.as_deref().unwrap_or("unnamed");
        mint_iri(&kind, seed, default_lower_for_kind(&kind))
    };
    let name = spec.name.clone().unwrap_or_else(|| name_from_iri(&iri));
    let mut labels = Vec::new();
    if let Some(label) = label_for_kind(&kind) {
        labels.push(label.to_string());
    }
    labels.extend(spec.labels.iter().cloned());
    labels.extend(extra_labels.iter().cloned());
    labels.retain(|label| label != "Entity");
    let mut seen = std::collections::HashSet::new();
    labels.retain(|label| seen.insert(label.clone()));
    for label in &labels {
        safe_label(label)?;
    }
    Ok((iri, name, labels))
}

/// MERGE an entity using only the supplied transaction.
///
/// `searchText` deliberately contains intrinsic node data only. Fact text belongs
/// on relationships and must not accumulate on nodes when facts are replaced or
/// retracted.
pub async fn merge_node_in_txn(
    txn: &mut Txn,
    spec: &NodeSpec,
    default_kind: &str,
    extra_labels: &[String],
) -> Result<MergedNode> {
    let (iri, name, labels) = resolved_node_parts(spec, default_kind, extra_labels)?;
    let creation_marker = Uuid::new_v4().to_string();
    let row = fetch_one_txn(
        txn,
        query(
            r#"
            CALL apoc.merge.node(
              ['Entity'],
              {iri: $iri},
              {name: $name, createdAt: datetime(), weight: 0, weightText: '0', layers: [],
               mindreaderCreateMarker: $creationMarker},
              {}
            ) YIELD node
            WITH node, node.mindreaderCreateMarker = $creationMarker AS created
            REMOVE node.mindreaderCreateMarker
            SET node:$($labels)
            SET node.name = coalesce(node.name, $name),
                node.searchText = trim(coalesce(node.name, $name) + ' ' + node.iri + ' ' + coalesce(node.value, ''))
            RETURN node, created
            "#,
        )
        .param("iri", iri.clone())
        .param("name", name.clone())
        .param("labels", labels)
        .param("creationMarker", creation_marker),
    )
    .await?
    .ok_or_else(|| graph_error!("failed to MERGE node {iri}"))?;
    let created = row.get::<bool>("created").unwrap_or(false);

    let node: Node = row.get("node")?;
    let labels = node
        .labels()
        .into_iter()
        .filter(|label| *label != "Entity")
        .map(str::to_string)
        .collect();
    Ok(MergedNode {
        iri,
        name,
        labels,
        created,
        json: node_json(&node),
    })
}

/// MERGE a literal using the canonical domain-level literal IRI algorithm.
pub async fn merge_literal_in_txn(
    txn: &mut Txn,
    value: &str,
    datatype: &str,
) -> Result<MergedNode> {
    let iri = literal_iri(value, datatype);
    let row = fetch_one_txn(
        txn,
        query(
            r#"
            OPTIONAL MATCH (existing:Entity {iri: $iri})
            MERGE (n:Entity:Literal {iri: $iri})
            ON CREATE SET n.name = $name, n.value = $value, n.datatype = $datatype,
              n.createdAt = datetime(), n.weight = 0, n.weightText = '0', n.layers = []
            ON MATCH SET n.name = coalesce(n.name, $name),
              n.value = coalesce(n.value, $value), n.datatype = coalesce(n.datatype, $datatype)
            SET n.searchText = trim(coalesce(n.name, $name) + ' ' + n.iri + ' ' + coalesce(n.value, $value))
            RETURN n, existing IS NULL AS created
            "#,
        )
        .param("iri", iri.clone())
        .param("name", value.to_string())
        .param("value", value.to_string())
        .param("datatype", datatype.to_string()),
    )
    .await?
    .ok_or_else(|| graph_error!("failed to MERGE literal {iri}"))?;
    let node: Node = row.get("n")?;
    let created = row.get::<bool>("created").unwrap_or(false);
    Ok(MergedNode {
        iri,
        name: value.to_string(),
        labels: vec!["Literal".into()],
        created,
        json: node_json(&node),
    })
}

/// Ensure a property stub exists without leaving the caller's transaction.
pub async fn ensure_property_in_txn(
    txn: &mut Txn,
    property: &str,
) -> Result<(String, bool, Value)> {
    let iri = property_iri(property);
    let name = name_from_iri(&iri);
    let row = fetch_one_txn(
        txn,
        query(
            r#"
            OPTIONAL MATCH (existing:Entity {iri: $iri})
            MERGE (n:Entity:Property {iri: $iri})
            ON CREATE SET n.name = $name, n.createdAt = datetime(), n.stub = true,
              n.weight = 0, n.weightText = '0', n.layers = []
            ON MATCH SET n.name = coalesce(n.name, $name)
            SET n.searchText = trim(coalesce(n.name, $name) + ' ' + n.iri + ' ' + coalesce(n.value, ''))
            RETURN n, existing IS NULL AS created
            "#,
        )
        .param("iri", iri.clone())
        .param("name", name),
    )
    .await?
    .ok_or_else(|| graph_error!("failed to MERGE property {iri}"))?;
    let node: Node = row.get("n")?;
    let created = row.get::<bool>("created").unwrap_or(false);
    Ok((iri, created, node_json(&node)))
}

fn lock_key(subject_iri: &str, predicate_iri: &str, layer: &str) -> String {
    let mut hasher = Sha256::new();
    for part in [subject_iri, predicate_iri, layer] {
        hasher.update((part.len() as u64).to_be_bytes());
        hasher.update(part.as_bytes());
    }
    hasher
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn fact_lock_specs(facts: &[(String, String, String)]) -> Vec<FactLockSpec> {
    let mut locks = Vec::with_capacity(facts.len() * 2);
    for (subject_iri, predicate_iri, layer) in facts {
        for predicate_iri in ["*", predicate_iri.as_str()] {
            locks.push(FactLockSpec {
                key: lock_key(subject_iri, predicate_iri, layer),
                subject_iri: subject_iri.clone(),
                predicate_iri: predicate_iri.to_string(),
                layer: layer.clone(),
            });
        }
    }
    locks.sort_by(|left, right| left.key.cmp(&right.key));
    locks.dedup_by(|left, right| left.key == right.key);
    locks
}

/// Acquire deterministic subject and predicate guards for every fact tuple.
///
/// Each `(subject, predicate, layer)` request acquires both `(subject, "*",
/// layer)` and `(subject, predicate, layer)`. Sorting the collision-resistant
/// keys before updating lock nodes gives concurrent writers a consistent lock
/// order and prevents stale precondition reads.
pub async fn acquire_fact_locks_in_txn(
    txn: &mut Txn,
    facts: &[(String, String, String)],
) -> Result<()> {
    for lock in fact_lock_specs(facts) {
        fetch_one_txn(
            txn,
            query(
                r#"
                MERGE (lock:FactLock {key: $key})
                ON CREATE SET lock.subjectIri = $subjectIri,
                  lock.predicateIri = $predicateIri, lock.layer = $layer,
                  lock.createdAt = datetime(), lock.revision = 0
                SET lock.revision = lock.revision + 1, lock.acquiredAt = datetime()
                RETURN lock.key AS key
                "#,
            )
            .param("key", lock.key)
            .param("subjectIri", lock.subject_iri)
            .param("predicateIri", lock.predicate_iri)
            .param("layer", lock.layer),
        )
        .await?
        .ok_or_else(|| graph_error!("failed to acquire fact lock"))?;
    }
    Ok(())
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
        CREATE (e:Entity:Episode {
            iri: $iri,
            tool: $tool,
            at: datetime(),
            createdAt: datetime(),
            name: $iri,
            weight: 0,
            weightText: '0'
        })
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
        .ok_or_else(|| graph_error!("failed to create episode"))?;
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
        .or({
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
            "layers": node.get::<Vec<String>>("layers").unwrap_or_default(),
            "weight": node_weight(node),
        });
    }
    json!({
        "iri": iri,
        "name": node.get::<String>("name").ok(),
        "labels": labels,
        "layers": node.get::<Vec<String>>("layers").unwrap_or_default(),
        "weight": node_weight(node),
    })
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
    use super::{
        fact_lock_specs, lock_key, safe_label, safe_rel, same_string_members, spike_rank,
        validate_model_version, MODEL_VERSION,
    };

    #[test]
    fn model_v4_requires_recreating_older_databases() {
        assert_eq!(MODEL_VERSION, 4);
        assert!(validate_model_version(4).is_ok());
        assert_eq!(
            validate_model_version(3).unwrap_err().to_string(),
            "Mindreader database model version 3 is incompatible with required version 4; recreate the Neo4j database or volume before starting Mindreader"
        );
    }

    #[test]
    fn fact_locks_are_complete_deduplicated_and_deterministically_ordered() {
        let facts = vec![
            (
                "mindreader:element/b".into(),
                "mindreader:property/Z".into(),
                "global".into(),
            ),
            (
                "mindreader:element/a".into(),
                "mindreader:property/A".into(),
                "project:test".into(),
            ),
            (
                "mindreader:element/b".into(),
                "mindreader:property/Z".into(),
                "global".into(),
            ),
        ];
        let locks = fact_lock_specs(&facts);
        assert_eq!(
            locks.len(),
            4,
            "each unique fact gets a subject and predicate lock"
        );
        assert!(locks.windows(2).all(|pair| pair[0].key < pair[1].key));
        assert!(locks.iter().all(|lock| lock.key.len() == 64));
        assert_eq!(
            locks
                .iter()
                .filter(|lock| lock.predicate_iri == "*")
                .count(),
            2
        );

        let mut reversed = facts;
        reversed.reverse();
        assert_eq!(locks, fact_lock_specs(&reversed));
    }

    #[test]
    fn fact_lock_key_is_length_delimited() {
        assert_ne!(lock_key("ab", "c", "d"), lock_key("a", "bc", "d"));
        assert_ne!(lock_key("a", "b", "cd"), lock_key("a", "bc", "d"));
    }

    #[test]
    fn index_definition_members_are_order_independent_and_exact() {
        assert!(same_string_members(
            &["iri".into(), "name".into()],
            &["name", "iri"]
        ));
        assert!(!same_string_members(&["name".into()], &["name", "iri"]));
        assert!(!same_string_members(
            &["name".into(), "iri".into(), "legacy".into()],
            &["name", "iri"]
        ));
    }

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
