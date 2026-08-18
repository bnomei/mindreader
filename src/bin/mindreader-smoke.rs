//! Live Neo4j integration suite for the eight memory tools and graph contracts.
//!
//! Mutates the configured database and leaves fixtures in place; use a
//! development or disposable instance only. Enabled with the `developer-tools`
//! feature. Semantic coverage always uses a deterministic smoke embedding
//! provider and never calls a remote service.

use async_trait::async_trait;
use mindreader::developer::config::{Config, EmbeddingSpace, SemanticConfig};
use mindreader::developer::domain::{EntityInput, ObjectInput};
use mindreader::developer::embeddings::{normalize_vector, EmbeddingProvider};
use mindreader::developer::error::{Context, Error, Result};
use mindreader::developer::graph::{
    self, acquire_fact_locks_in_txn, fetch_one, merge_node_in_txn, require_embedding_space,
    MergedNode, NodeSpec,
};
use mindreader::developer::semantic::SemanticRuntime;
use mindreader::developer::service::{
    EffectiveInterval, EffectiveUpdate, JudgeArgs, JudgeRating, MemoryService, PlaceArgs,
    PlaceEdit, RecallArgs, ReviseArgs, SemanticSearchArgs, TargetArgs, ToolOutput, UnifyArgs,
    WithdrawArgs, WriteArgs, WriteFact,
};
use mindreader::developer::Mindreader;
use mindreader::operation_error;
use neo4rs::{query, Graph};
use serde_json::Value;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

struct Report {
    next: u32,
    failed: u32,
}

/// Deterministic 3-d fixture used for every semantic smoke request.
struct SmokeEmbedding;

#[async_trait]
impl EmbeddingProvider for SmokeEmbedding {
    async fn embed(&self, text: &str) -> Result<Vec<f64>> {
        let bytes = text.as_bytes();
        normalize_vector(
            vec![
                1.0,
                bytes.len() as f64 + 1.0,
                bytes.iter().map(|byte| *byte as f64).sum::<f64>() + 1.0,
            ],
            3,
            "smoke",
        )
    }

    fn provider(&self) -> &'static str {
        "smoke"
    }

    fn model(&self) -> &str {
        "deterministic"
    }

    fn dimensions(&self) -> usize {
        3
    }
}

impl Report {
    fn new() -> Self {
        Self { next: 1, failed: 0 }
    }

    /// Print one numbered PASS/FAIL step; increments `failed` on assertion failure.
    fn check(&mut self, name: &str, ok: bool, detail: impl std::fmt::Display) {
        let status = if ok { "PASS" } else { "FAIL" };
        println!("{status} {} {name}", self.next);
        let detail = detail.to_string();
        if !detail.is_empty() {
            println!("     {detail}");
        }
        if !ok {
            self.failed += 1;
        }
        self.next += 1;
    }
}

/// Element subject identified by display name (IRI is minted on write).
fn entity(name: impl Into<String>) -> EntityInput {
    EntityInput {
        kind: "node".into(),
        iri: None,
        name: Some(name.into()),
        labels: vec!["Element".into()],
    }
}

/// Element subject with one custom label for isolated retrieval fixtures.
fn labeled_entity(name: impl Into<String>, label: &str) -> EntityInput {
    EntityInput {
        kind: "node".into(),
        iri: None,
        name: Some(name.into()),
        labels: vec!["Element".into(), label.into()],
    }
}

/// Subject identified by an existing node IRI.
fn entity_iri(iri: impl Into<String>) -> EntityInput {
    EntityInput {
        kind: "node".into(),
        iri: Some(iri.into()),
        name: None,
        labels: Vec::new(),
    }
}

/// Element object identified by display name.
fn object(name: impl Into<String>) -> ObjectInput {
    ObjectInput {
        kind: "node".into(),
        iri: None,
        name: Some(name.into()),
        labels: vec!["Element".into()],
        value: None,
        datatype: None,
    }
}

/// Element object with one custom label for isolated retrieval fixtures.
fn labeled_object(name: impl Into<String>, label: &str) -> ObjectInput {
    ObjectInput {
        kind: "node".into(),
        iri: None,
        name: Some(name.into()),
        labels: vec!["Element".into(), label.into()],
        value: None,
        datatype: None,
    }
}

/// Object identified by an existing node IRI.
fn object_iri(iri: impl Into<String>) -> ObjectInput {
    ObjectInput {
        kind: "node".into(),
        iri: Some(iri.into()),
        name: None,
        labels: Vec::new(),
        value: None,
        datatype: None,
    }
}

/// One-triple `write` under the given request `scope`.
fn write_args(s: EntityInput, p: impl AsRef<str>, o: ObjectInput, scope: Vec<String>) -> WriteArgs {
    WriteArgs {
        facts: vec![WriteFact {
            s,
            p: p.as_ref().to_string(),
            o,
            spike: None,
            contradicts: false,
            effective: None,
        }],
        scope,
    }
}

/// Fact handle IRI from a write or recall envelope (`facts[0].target` or top-level `target`).
fn relationship_iri(value: &Value) -> Result<String> {
    value
        .pointer("/facts/0/target/iri")
        .or_else(|| value.pointer("/target/iri"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| operation_error!("response has no relationship IRI: {value}"))
}

/// Subject IRI of the first returned fact.
fn subject_iri(value: &Value) -> Result<String> {
    value
        .pointer("/facts/0/s/iri")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| operation_error!("response has no subject IRI: {value}"))
}

/// Object IRI of the first returned fact.
fn object_result_iri(value: &Value) -> Result<String> {
    value
        .pointer("/facts/0/o/iri")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| operation_error!("response has no object IRI: {value}"))
}

/// Pasteable fact IRIs from top-level `facts[]`, in result order.
fn fact_relationships(value: &Value) -> Result<Vec<String>> {
    let facts = value
        .get("facts")
        .and_then(Value::as_array)
        .ok_or_else(|| operation_error!("recall response has no facts array: {value}"))?;
    facts
        .iter()
        .map(|fact| {
            fact.pointer("/target/iri")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| operation_error!("fact has no target.iri: {fact}"))
        })
        .collect()
}

/// Incident fact IRIs from `lookups[0].facts` (iris hops=0 path).
fn lookup_fact_iris(value: &Value) -> Result<Vec<String>> {
    let facts = value
        .pointer("/lookups/0/facts")
        .and_then(Value::as_array)
        .ok_or_else(|| operation_error!("recall lookup has no facts array: {value}"))?;
    facts
        .iter()
        .map(|fact| {
            fact.pointer("/target/iri")
                .and_then(Value::as_str)
                .map(str::to_string)
                .ok_or_else(|| operation_error!("lookup fact has no target.iri: {fact}"))
        })
        .collect()
}

/// Insert a live activation with the deterministic smoke embedding and an APOC TTL lease.
async fn seed_semantic_activation(
    graph: &Graph,
    embedding: &[f64],
    result_refs: &[String],
    ttl_ms: i64,
) -> Result<(String, i64)> {
    let row = fetch_one(
        graph,
        query(
            r#"
            CREATE (a:SemanticActivation:SemanticActivationV4:TTL {resultRefs: $resultRefs})
            WITH a
            CALL db.create.setNodeVectorProperty(a, 'embedding', $embedding)
            WITH a
            CALL apoc.ttl.expireIn(a, $ttl, 'ms')
            RETURN elementId(a) AS elementId, a.ttl AS ttl
            "#,
        )
        .param("embedding", embedding.to_vec())
        .param("resultRefs", result_refs.to_vec())
        .param("ttl", ttl_ms),
    )
    .await?
    .ok_or_else(|| operation_error!("semantic activation seed returned no row"))?;
    Ok((row.get("elementId")?, row.get("ttl")?))
}

/// Read the remaining APOC TTL on a seeded activation (used to prove refresh).
async fn semantic_activation_ttl(graph: &Graph, element_id: &str) -> Result<Option<i64>> {
    fetch_one(
        graph,
        query(
            "MATCH (a:SemanticActivation:TTL) WHERE elementId(a) = $elementId RETURN a.ttl AS ttl",
        )
        .param("elementId", element_id.to_string()),
    )
    .await?
    .map(|row| row.get("ttl").map_err(Into::into))
    .transpose()
}

/// Rank index of a fact handle in `facts[]`; used to assert Spike/weight order.
fn fact_position(value: &Value, relationship: &str) -> Option<usize> {
    value
        .get("facts")
        .and_then(Value::as_array)?
        .iter()
        .position(|fact| fact.pointer("/target/iri").and_then(Value::as_str) == Some(relationship))
}

/// Closed-world `recall` text selector at detailed/limit=100.
async fn search(service: &MemoryService, scope: Vec<String>, text: &str) -> Result<Value> {
    service
        .recall(RecallArgs {
            scope,
            text: Some(text.into()),
            iris: None,
            labels: None,
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("detailed".into()),
            limit: Some(100),
            effective_at: None,
        })
        .await
        .map(ToolOutput::into_value)
}

/// Closed-world detailed text recall at one world-time instant.
async fn search_effective(
    service: &MemoryService,
    scope: Vec<String>,
    text: &str,
    effective_at: &str,
) -> Result<Value> {
    service
        .recall(RecallArgs {
            scope,
            text: Some(text.into()),
            iris: None,
            labels: None,
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("detailed".into()),
            limit: Some(100),
            effective_at: Some(effective_at.into()),
        })
        .await
        .map(ToolOutput::into_value)
}

/// MERGE one entity in its own transaction (lock/concurrency fixtures).
async fn merge_node_once(graph: neo4rs::Graph, spec: NodeSpec) -> Result<MergedNode> {
    let mut txn = graph.start_txn().await?;
    let node = merge_node_in_txn(&mut txn, &spec, "Element", &[]).await?;
    txn.commit().await.context("commit smoke node upsert")?;
    Ok(node)
}

/// Stored memberships, current-ness (`validTo` null), and weight for a fact IRI.
async fn relation_state(
    graph: &neo4rs::Graph,
    iri: &str,
) -> Result<Option<(Vec<String>, bool, i64)>> {
    let row = fetch_one(
        graph,
        query(
            "MATCH ()-[r]->() WHERE r.iri = $iri \
             RETURN r.layers AS layers, r.validTo IS NULL AS current, \
                    r.weight AS weight",
        )
        .param("iri", iri.to_string()),
    )
    .await?;
    row.map(|row| {
        Ok((
            row.get::<Vec<String>>("layers")?,
            row.get::<bool>("current")?,
            row.get::<i64>("weight")?,
        ))
    })
    .transpose()
}

/// Stored `layers` memberships for a node IRI.
async fn node_layers(graph: &Graph, iri: &str) -> Result<Option<Vec<String>>> {
    fetch_one(
        graph,
        query("MATCH (n:Entity {iri: $iri}) RETURN n.layers AS layers")
            .param("iri", iri.to_string()),
    )
    .await?
    .map(|row| row.get("layers").map_err(Into::into))
    .transpose()
}

/// Count Episode nodes recorded by one MCP tool name (no-ops must not increment).
async fn episode_count(graph: &Graph, tool: &str) -> Result<i64> {
    let row = fetch_one(
        graph,
        query("MATCH (e:Entity:Episode {tool: $tool}) RETURN count(e) AS count")
            .param("tool", tool.to_string()),
    )
    .await?
    .ok_or_else(|| operation_error!("episode count returned no row for {tool}"))?;
    Ok(row.get("count")?)
}

/// Count current CONTRADICTS edges between two object IRIs.
async fn current_contradicts_count(graph: &Graph, from: &str, to: &str) -> Result<i64> {
    let row = fetch_one(
        graph,
        query(
            "MATCH (a:Entity {iri: $from})-[r:CONTRADICTS]->(b:Entity {iri: $to}) \
             WHERE r.validTo IS NULL RETURN count(r) AS count",
        )
        .param("from", from.to_string())
        .param("to", to.to_string()),
    )
    .await?
    .ok_or_else(|| operation_error!("contradiction count returned no row"))?;
    Ok(row.get("count")?)
}

/// Model marker plus embedding-space marker after bootstrap (smoke embedding space).
async fn bootstrap_state(graph: &Graph) -> Result<Value> {
    let row = fetch_one(
        graph,
        query(
            "MATCH (model:MindreaderMeta {key: 'model'}), \
                    (embedding:MindreaderMeta {key: 'embedding'}) \
             RETURN model.version AS version, embedding.provider AS provider, \
                    embedding.model AS model, embedding.dimensions AS dimensions",
        ),
    )
    .await?
    .ok_or_else(|| operation_error!("bootstrap metadata is missing"))?;
    Ok(serde_json::json!({
        "version": row.get::<i64>("version")?,
        "provider": row.get::<String>("provider")?,
        "model": row.get::<String>("model")?,
        "dimensions": row.get::<i64>("dimensions")?,
    }))
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(0) => {
            println!("ALL PASS");
            ExitCode::SUCCESS
        }
        Ok(_) => {
            println!("SOME STEPS FAILED");
            ExitCode::from(1)
        }
        Err(error) => {
            eprintln!("SMOKE ABORT: {error:#}");
            ExitCode::from(1)
        }
    }
}

/// Connect, exercise the eight tools and graph contracts, and leave fixtures in place.
async fn run() -> Result<u32> {
    let mut args = std::env::args().skip(1);
    let cfg = match (args.next().as_deref(), args.next(), args.next()) {
        (None, None, None) => Config::from_env()?,
        (Some("--config-dir"), Some(path), None) => Config::from_directory(path)?,
        _ => {
            return Err(operation_error!(
                "usage: mindreader-smoke [--config-dir PATH]"
            ));
        }
    };
    println!("mindreader-smoke: uri={}", cfg.uri);
    println!(
        "registered tools: {}",
        Mindreader::registered_tool_names().join(", ")
    );

    let mut report = Report::new();
    let mut tool_names = Mindreader::registered_tool_names();
    tool_names.sort();
    let mut expected_tool_names = vec![
        "judge".to_string(),
        "place".to_string(),
        "recall".to_string(),
        "recall_semantic".to_string(),
        "revise".to_string(),
        "unify".to_string(),
        "withdraw".to_string(),
        "write".to_string(),
    ];
    expected_tool_names.sort();
    report.check(
        "MCP registers the eight-tool contract",
        tool_names == expected_tool_names,
        format!("tools={tool_names:?}"),
    );

    let graph = graph::connect(&cfg).await?;
    let embedding_space = EmbeddingSpace {
        provider: "smoke".into(),
        model: "deterministic".into(),
        dimensions: 3,
    };
    graph::bootstrap(&graph, Some(&embedding_space), graph::SpaceReplace::Refuse).await?;
    let stats = bootstrap_state(&graph).await?;
    report.check(
        "bootstrap records the current graph and embedding models",
        stats.get("version").and_then(Value::as_i64) == Some(graph::MODEL_VERSION)
            && stats.get("provider").and_then(Value::as_str) == Some("smoke")
            && stats.get("model").and_then(Value::as_str) == Some("deterministic")
            && stats.get("dimensions").and_then(Value::as_i64) == Some(3),
        &stats,
    );

    let semantic_runtime = SemanticRuntime::new(
        Arc::new(SmokeEmbedding),
        SemanticConfig {
            neighbor_limit: 100,
            ..SemanticConfig::default()
        },
    );
    let service =
        MemoryService::with_semantic_runtime(graph.clone(), semantic_runtime, cfg.secrets_path());

    let tag = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos()
        .to_string();
    let layer_a = format!("project:smoke-a-{tag}");
    let layer_b = format!("project:smoke-b-{tag}");
    let layer_c = format!("project:smoke-c-{tag}");
    let property = format!("smokeProperty{tag}");

    let lock_subjects = vec![
        format!("mindreader:element/lock-a-{tag}"),
        format!("mindreader:element/lock-b-{tag}"),
    ];
    let mut lock_facts = vec![
        (
            lock_subjects[0].clone(),
            format!("mindreader:property/lock-a-{tag}"),
            layer_a.clone(),
        ),
        (
            lock_subjects[1].clone(),
            format!("mindreader:property/lock-b-{tag}"),
            layer_a.clone(),
        ),
    ];
    lock_facts.push(lock_facts[0].clone());
    for iteration in 0..2 {
        if iteration == 1 {
            lock_facts.reverse();
        }
        let mut txn = graph.start_txn().await?;
        acquire_fact_locks_in_txn(&mut txn, &lock_facts).await?;
        txn.commit().await.context("commit smoke fact-lock batch")?;
    }
    let lock_state = fetch_one(
        &graph,
        query(
            "MATCH (lock:FactLock) \
             WHERE lock.subjectIri IN $subjects AND lock.layer = $layer \
             RETURN count(lock) AS count, min(lock.revision) AS minRevision, \
                    max(lock.revision) AS maxRevision",
        )
        .param("subjects", lock_subjects)
        .param("layer", layer_a.clone()),
    )
    .await?
    .ok_or_else(|| operation_error!("fact-lock smoke query returned no row"))?;
    report.check(
        "logical fact locks batch, deduplicate, and preserve deterministic revisions",
        lock_state.get::<i64>("count").unwrap_or(0) == 4
            && lock_state.get::<i64>("minRevision").unwrap_or(0) == 2
            && lock_state.get::<i64>("maxRevision").unwrap_or(0) == 2,
        format!("state={lock_state:?}"),
    );

    let relabeled_iri = format!("mindreader:element/apoc-label-{tag}");
    let initial = merge_node_once(
        graph.clone(),
        NodeSpec {
            iri: Some(relabeled_iri.clone()),
            name: Some(format!("apoc-label-{tag}")),
            labels: Vec::new(),
        },
    )
    .await?;
    let relabeled = merge_node_once(
        graph.clone(),
        NodeSpec {
            iri: Some(relabeled_iri),
            name: Some(format!("replacement-name-{tag}")),
            labels: vec!["Reviewed".into()],
        },
    )
    .await?;
    let concurrent_iri = format!("mindreader:element/apoc-concurrent-{tag}");
    let concurrent_spec = NodeSpec {
        iri: Some(concurrent_iri),
        name: Some(format!("apoc-concurrent-{tag}")),
        labels: vec!["Concurrent".into()],
    };
    let (left, right) = tokio::try_join!(
        merge_node_once(graph.clone(), concurrent_spec.clone()),
        merge_node_once(graph.clone(), concurrent_spec),
    )?;
    report.check(
        "APOC entity upserts add labels and report one concurrent creator",
        initial.created
            && !relabeled.created
            && relabeled
                .json
                .get("labels")
                .and_then(Value::as_array)
                .is_some_and(|labels| labels.iter().any(|label| label == "Reviewed"))
            && relabeled.json.get("name") == initial.json.get("name")
            && left.created as usize + right.created as usize == 1,
        format!("initial={initial:?} relabeled={relabeled:?} concurrent=({left:?}, {right:?})"),
    );

    let schema = service
        .write(write_args(
            entity(format!("schema-seed-subject-{tag}")),
            property.clone(),
            object(format!("schema-seed-object-{tag}")),
            Vec::new(),
        ))
        .await?;
    let schema_property = fetch_one(
        &graph,
        query(
            "MATCH (property:Entity:Property {iri: $iri}) \
             RETURN property.iri AS iri, property.layers AS layers, \
                    property.stub AS stub, property.weight AS weight",
        )
        .param("iri", format!("mindreader:property/{property}")),
    )
    .await?
    .ok_or_else(|| operation_error!("write did not declare property {property}"))?;
    report.check(
        "write declares global properties under its mutation Episode",
        schema_property
            .get::<Vec<String>>("layers")
            .is_ok_and(|layers| layers.is_empty())
            && schema_property.get::<bool>("stub").is_ok_and(|stub| !stub)
            && schema_property
                .get::<i64>("weight")
                .is_ok_and(|weight| weight == 0)
            && schema
                .pointer("/episode/iri")
                .and_then(Value::as_str)
                .is_some()
            && schema.pointer("/facts/0/property").is_none()
            && schema.pointer("/facts/0/propertyStub").is_none(),
        format!("write={schema} property={schema_property:?}"),
    );

    // Keep this one alphanumeric token so keyword fallback cannot match fixtures
    // from earlier persistent smoke runs through a generic "visibility" term.
    let visibility_token = format!("visibility{tag}");
    let global = service
        .write(write_args(
            entity(format!("{visibility_token}-global-subject")),
            property.clone(),
            object(format!("{visibility_token}-global-object")),
            Vec::new(),
        ))
        .await?;
    let in_a = service
        .write(write_args(
            entity(format!("{visibility_token}-a-subject")),
            property.clone(),
            object(format!("{visibility_token}-a-object")),
            vec![layer_a.clone()],
        ))
        .await?;
    let in_b = service
        .write(write_args(
            entity(format!("{visibility_token}-b-subject")),
            property.clone(),
            object(format!("{visibility_token}-b-object")),
            vec![layer_b.clone()],
        ))
        .await?;
    let global_rel = relationship_iri(&global)?;
    let a_rel = relationship_iri(&in_a)?;
    let b_rel = relationship_iri(&in_b)?;
    let only_global = fact_relationships(&search(&service, Vec::new(), &visibility_token).await?)?;
    let seen_a =
        fact_relationships(&search(&service, vec![layer_a.clone()], &visibility_token).await?)?;
    let seen_b =
        fact_relationships(&search(&service, vec![layer_b.clone()], &visibility_token).await?)?;
    let seen_ab = fact_relationships(
        &search(
            &service,
            vec![layer_a.clone(), layer_b.clone()],
            &visibility_token,
        )
        .await?,
    )?;
    report.check(
        "request scopes dynamically expose global plus intersecting layers",
        only_global == vec![global_rel.clone()]
            && seen_a.contains(&global_rel)
            && seen_a.contains(&a_rel)
            && !seen_a.contains(&b_rel)
            && seen_b.contains(&global_rel)
            && !seen_b.contains(&a_rel)
            && seen_b.contains(&b_rel)
            && [global_rel.clone(), a_rel.clone(), b_rel.clone()]
                .iter()
                .all(|iri| seen_ab.contains(iri)),
        format!("global={only_global:?} A={seen_a:?} B={seen_b:?} AB={seen_ab:?}"),
    );

    let temporal_subject = format!("temporal-state-{tag}");
    let temporal_property = format!("residesIn{tag}");
    let temporal_rome = format!("temporal-rome-{tag}");
    let temporal_lisbon = format!("temporal-lisbon-{tag}");
    let temporal_unknown = format!("temporal-unknown-{tag}");
    let temporal_write = service
        .write(WriteArgs {
            facts: vec![
                WriteFact {
                    s: entity(temporal_subject.clone()),
                    p: temporal_property.clone(),
                    o: object(temporal_rome.clone()),
                    spike: None,
                    contradicts: false,
                    effective: Some(EffectiveInterval {
                        from: Some("2020-01-01T00:00:00Z".into()),
                        to: Some("2022-01-01T00:00:00Z".into()),
                    }),
                },
                WriteFact {
                    s: entity(temporal_subject.clone()),
                    p: temporal_property.clone(),
                    o: object(temporal_lisbon.clone()),
                    spike: None,
                    contradicts: false,
                    effective: Some(EffectiveInterval {
                        from: Some("2022-01-01T00:00:00Z".into()),
                        to: Some("2024-01-01T00:00:00Z".into()),
                    }),
                },
                WriteFact {
                    s: entity(temporal_subject.clone()),
                    p: temporal_property.clone(),
                    o: object(temporal_rome.clone()),
                    spike: None,
                    contradicts: false,
                    effective: Some(EffectiveInterval {
                        from: Some("2024-01-01T00:00:00Z".into()),
                        to: None,
                    }),
                },
                WriteFact {
                    s: entity(temporal_subject.clone()),
                    p: temporal_property.clone(),
                    o: object(temporal_unknown.clone()),
                    spike: None,
                    contradicts: false,
                    effective: None,
                },
            ],
            scope: vec![layer_a.clone()],
        })
        .await?;
    let temporal_subject_iri = subject_iri(&temporal_write)?;
    let first_rome_target = temporal_write
        .pointer("/facts/0/target/iri")
        .and_then(Value::as_str)
        .ok_or_else(|| operation_error!("first temporal fact has no target"))?;
    let second_rome_target = temporal_write
        .pointer("/facts/2/target/iri")
        .and_then(Value::as_str)
        .ok_or_else(|| operation_error!("second temporal fact has no target"))?
        .to_string();
    let object_names = |value: &Value| {
        let mut names = value
            .get("facts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|fact| fact.pointer("/o/name").and_then(Value::as_str))
            .map(str::to_string)
            .collect::<Vec<_>>();
        names.sort();
        names
    };
    let temporal_2021 = search_effective(
        &service,
        vec![layer_a.clone()],
        &temporal_subject,
        "2021-12-31T23:59:59Z",
    )
    .await?;
    let temporal_2022 = search_effective(
        &service,
        vec![layer_a.clone()],
        &temporal_subject,
        "2022-01-01T00:00:00Z",
    )
    .await?;
    let temporal_unfiltered = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: None,
            iris: Some(vec![temporal_subject_iri.clone()]),
            labels: None,
            around: None,
            hops: Some(1),
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    report.check(
        "effective intervals are distinct half-open fact identities and exclude unknown time",
        first_rome_target != second_rome_target
            && object_names(&temporal_2021) == vec![temporal_rome.clone()]
            && object_names(&temporal_2022) == vec![temporal_lisbon.clone()]
            && object_names(&temporal_unfiltered).len() == 4
            && !object_names(&temporal_2022).contains(&temporal_unknown),
        format!(
            "write={temporal_write} at2021={temporal_2021} at2022={temporal_2022} all={temporal_unfiltered}"
        ),
    );
    let temporal_iri = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: None,
            iris: Some(vec![temporal_subject_iri.clone()]),
            labels: None,
            around: None,
            hops: Some(1),
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: Some("2022-01-01T00:00:00Z".into()),
        })
        .await?;
    let temporal_around = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: None,
            iris: None,
            labels: None,
            around: Some(temporal_subject_iri),
            hops: None,
            p: Some(vec![temporal_property.clone()]),
            depth: Some(1),
            direction: Some("outgoing".into()),
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: Some("2022-01-01T00:00:00Z".into()),
        })
        .await?;
    let temporal_semantic = service
        .recall_semantic(SemanticSearchArgs {
            scope: vec![layer_a.clone()],
            text: temporal_subject.clone(),
            labels: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: Some("2022-01-01T00:00:00Z".into()),
        })
        .await?;
    let iri_names = temporal_iri
        .pointer("/lookups/0/facts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|fact| fact.pointer("/o/name").and_then(Value::as_str))
        .collect::<Vec<_>>();
    report.check(
        "effectiveAt applies consistently to IRI, around, and semantic recall",
        iri_names == vec![temporal_lisbon.as_str()]
            && object_names(&temporal_around) == vec![temporal_lisbon.clone()]
            && object_names(&temporal_semantic) == vec![temporal_lisbon.clone()],
        format!("iri={temporal_iri} around={temporal_around} semantic={temporal_semantic}"),
    );

    let temporal_rome_iri = temporal_write
        .pointer("/facts/2/o/iri")
        .and_then(Value::as_str)
        .ok_or_else(|| operation_error!("temporal Rome fact has no object IRI"))?;
    let temporal_revision = service
        .revise(ReviseArgs {
            scope: vec![layer_a.clone()],
            target: TargetArgs {
                kind: "fact".into(),
                iri: second_rome_target.clone(),
            },
            replacement: object_iri(temporal_rome_iri),
            spike: None,
            contradicts: false,
            reason: Some("correct effective state boundary".into()),
            effective: EffectiveUpdate::Set(EffectiveInterval {
                from: Some("2025-01-01T00:00:00Z".into()),
                to: None,
            }),
        })
        .await?;
    let temporal_replacement = relationship_iri(&temporal_revision)?;
    let temporal_history = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: None,
            iris: None,
            labels: None,
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: Some(temporal_replacement),
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    let temporal_2024 = search_effective(
        &service,
        vec![layer_a.clone()],
        &temporal_subject,
        "2024-06-01T00:00:00Z",
    )
    .await?;
    let temporal_2025 = search_effective(
        &service,
        vec![layer_a.clone()],
        &temporal_subject,
        "2025-01-01T00:00:00Z",
    )
    .await?;
    report.check(
        "same-object revision corrects effective time while history preserves both clocks",
        object_names(&temporal_2024).is_empty()
            && object_names(&temporal_2025) == vec![temporal_rome]
            && temporal_history
                .get("facts")
                .and_then(Value::as_array)
                .is_some_and(|facts| {
                    facts.iter().any(|fact| {
                        fact.get("transactionCurrent").and_then(Value::as_bool) == Some(false)
                            && fact.pointer("/effective/from").and_then(Value::as_str)
                                == Some("2024-01-01T00:00:00Z")
                            && fact.pointer("/transaction/from").is_some()
                    }) && facts.iter().any(|fact| {
                        fact.get("transactionCurrent").and_then(Value::as_bool) == Some(true)
                            && fact.pointer("/effective/from").and_then(Value::as_str)
                                == Some("2025-01-01T00:00:00Z")
                    })
                }),
        format!(
            "revision={temporal_revision} history={temporal_history} at2024={temporal_2024} at2025={temporal_2025}"
        ),
    );

    let temporal_lisbon_target = temporal_write
        .pointer("/facts/1/target/iri")
        .and_then(Value::as_str)
        .ok_or_else(|| operation_error!("temporal Lisbon fact has no target"))?;
    let inherited_revision = service
        .revise(ReviseArgs {
            scope: vec![layer_a.clone()],
            target: TargetArgs {
                kind: "fact".into(),
                iri: temporal_lisbon_target.to_string(),
            },
            replacement: object(format!("temporal-porto-{tag}")),
            spike: None,
            contradicts: false,
            reason: Some("replace value while preserving effective interval".into()),
            effective: EffectiveUpdate::Inherit,
        })
        .await?;
    let inherited_target = relationship_iri(&inherited_revision)?;
    let cleared_revision = service
        .revise(ReviseArgs {
            scope: vec![layer_a.clone()],
            target: TargetArgs {
                kind: "fact".into(),
                iri: inherited_target,
            },
            replacement: inherited_revision
                .pointer("/fact/o/iri")
                .and_then(Value::as_str)
                .map(object_iri)
                .ok_or_else(|| operation_error!("inherited revision has no object IRI"))?,
            spike: None,
            contradicts: false,
            reason: Some("clear unsupported effective interval".into()),
            effective: EffectiveUpdate::Clear,
        })
        .await?;
    let temporal_2023_after_clear = search_effective(
        &service,
        vec![layer_a.clone()],
        &temporal_subject,
        "2023-01-01T00:00:00Z",
    )
    .await?;
    report.check(
        "revision inherits an omitted interval and explicit null clears it",
        inherited_revision.pointer("/fact/effective/from").and_then(Value::as_str)
            == Some("2022-01-01T00:00:00Z")
            && inherited_revision.pointer("/fact/effective/to").and_then(Value::as_str)
                == Some("2024-01-01T00:00:00Z")
            && cleared_revision
                .pointer("/fact/effective")
                .is_some_and(Value::is_null)
            && object_names(&temporal_2023_after_clear).is_empty(),
        format!(
            "inherited={inherited_revision} cleared={cleared_revision} at2023={temporal_2023_after_clear}"
        ),
    );

    let batch_token = format!("batch-{tag}");
    let batch = service
        .write(WriteArgs {
            facts: vec![
                WriteFact {
                    s: entity(format!("{batch_token}-one")),
                    p: property.clone(),
                    o: object(format!("{batch_token}-a")),
                    spike: None,
                    contradicts: false,
                    effective: None,
                },
                WriteFact {
                    s: entity(format!("{batch_token}-two")),
                    p: property.clone(),
                    o: object(format!("{batch_token}-b")),
                    spike: None,
                    contradicts: false,
                    effective: None,
                },
                WriteFact {
                    s: entity(format!("{batch_token}-three")),
                    p: property.clone(),
                    o: object(format!("{batch_token}-c")),
                    spike: None,
                    contradicts: false,
                    effective: None,
                },
            ],
            scope: vec![layer_a.clone()],
        })
        .await?;
    let batch_episode = batch.pointer("/episode/iri").and_then(Value::as_str);
    report.check(
        "one write facts[] call records one Episode for three triples",
        batch.get("noop").and_then(Value::as_bool) == Some(false)
            && batch
                .get("facts")
                .and_then(Value::as_array)
                .is_some_and(|facts| facts.len() == 3)
            && batch_episode.is_some(),
        &batch,
    );
    let batch_iris = (0..3)
        .map(|index| {
            let subject = batch
                .pointer(&format!("/facts/{index}/s/iri"))
                .and_then(Value::as_str)
                .ok_or_else(|| operation_error!("batch fact {index} missing subject"))?;
            let object = batch
                .pointer(&format!("/facts/{index}/o/iri"))
                .and_then(Value::as_str)
                .ok_or_else(|| operation_error!("batch fact {index} missing object"))?;
            Ok::<_, Error>((subject.to_string(), object.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    let batch_noop = service
        .write(WriteArgs {
            facts: batch_iris
                .into_iter()
                .map(|(subject, object)| WriteFact {
                    s: entity_iri(subject),
                    p: property.clone(),
                    o: object_iri(object),
                    spike: None,
                    contradicts: false,
                    effective: None,
                })
                .collect(),
            scope: vec![layer_a.clone()],
        })
        .await?;
    report.check(
        "all-noop write facts[] rolls back without an Episode",
        batch_noop.get("noop").and_then(Value::as_bool) == Some(true)
            && batch_noop.get("episode").is_some_and(Value::is_null)
            && batch_noop
                .get("facts")
                .and_then(Value::as_array)
                .is_some_and(|facts| {
                    facts.len() == 3
                        && facts
                            .iter()
                            .all(|fact| fact.get("noop") == Some(&Value::Bool(true)))
                }),
        &batch_noop,
    );

    let merge_token = format!("merge-{tag}");
    let merged_a = service
        .write(write_args(
            entity(format!("{merge_token}-subject")),
            property.clone(),
            object(format!("{merge_token}-old")),
            vec![layer_a.clone()],
        ))
        .await?;
    let merged_b = service
        .write(write_args(
            entity_iri(subject_iri(&merged_a)?),
            property.clone(),
            object_iri(object_result_iri(&merged_a)?),
            vec![layer_b.clone()],
        ))
        .await?;
    let merged_rel = relationship_iri(&merged_a)?;
    let merged_state = relation_state(&graph, &merged_rel).await?;
    report.check(
        "exact semantic fact unions memberships under one stable relationship IRI",
        relationship_iri(&merged_b)? == merged_rel
            && merged_state == Some((vec![layer_a.clone(), layer_b.clone()], true, 0))
            && merged_b
                .pointer("/facts/0/scope")
                .and_then(Value::as_array)
                .is_some_and(|scope| {
                    scope
                        == &vec![
                            Value::String(layer_a.clone()),
                            Value::String(layer_b.clone()),
                        ]
                }),
        format!("first={merged_a} second={merged_b} state={merged_state:?}"),
    );

    let global_wins_1 = service
        .write(write_args(
            entity(format!("global-wins-{tag}-subject")),
            property.clone(),
            object(format!("global-wins-{tag}-object")),
            Vec::new(),
        ))
        .await?;
    let global_wins_2 = service
        .write(write_args(
            entity_iri(subject_iri(&global_wins_1)?),
            property.clone(),
            object_iri(object_result_iri(&global_wins_1)?),
            vec![layer_a.clone()],
        ))
        .await?;
    let global_wins_rel = relationship_iri(&global_wins_1)?;
    report.check(
        "global membership wins over later named assertions",
        relationship_iri(&global_wins_2)? == global_wins_rel
            && global_wins_2.get("noop").and_then(Value::as_bool) == Some(true)
            && relation_state(&graph, &global_wins_rel).await? == Some((Vec::new(), true, 0)),
        &global_wins_2,
    );

    let contradiction_old_left = service
        .write(write_args(
            entity(format!("contradiction-left-{tag}")),
            format!("contradictionLeft{tag}"),
            object(format!("contradiction-old-{tag}")),
            vec![layer_a.clone()],
        ))
        .await?;
    let contradiction_old_iri = object_result_iri(&contradiction_old_left)?;
    let contradiction_old_right = service
        .write(write_args(
            entity(format!("contradiction-right-{tag}")),
            format!("contradictionRight{tag}"),
            object_iri(contradiction_old_iri.clone()),
            vec![layer_a.clone()],
        ))
        .await?;
    let contradiction_new_name = format!("contradiction-new-{tag}");
    let (contradiction_left, contradiction_right) = tokio::try_join!(
        service.write(WriteArgs {
            facts: vec![WriteFact {
                s: entity_iri(subject_iri(&contradiction_old_left)?),
                p: format!("contradictionLeft{tag}"),
                o: object(contradiction_new_name.clone()),
                spike: None,
                contradicts: true,
                effective: None,
            }],
            scope: vec![layer_a.clone()],
        },),
        service.write(WriteArgs {
            facts: vec![WriteFact {
                s: entity_iri(subject_iri(&contradiction_old_right)?),
                p: format!("contradictionRight{tag}"),
                o: object(contradiction_new_name),
                spike: None,
                contradicts: true,
                effective: None,
            }],
            scope: vec![layer_a.clone()],
        },),
    )?;
    let contradiction_new_iri = object_result_iri(&contradiction_left)?;
    let contradiction_count =
        current_contradicts_count(&graph, &contradiction_new_iri, &contradiction_old_iri).await?;
    report.check(
        "concurrent contradiction writes preserve one exact current relationship",
        object_result_iri(&contradiction_right)? == contradiction_new_iri
            && contradiction_count == 1,
        format!(
            "left={contradiction_left} right={contradiction_right} count={contradiction_count}"
        ),
    );

    let replacement = service
        .revise(ReviseArgs {
            scope: vec![layer_a.clone()],
            target: TargetArgs {
                kind: "fact".into(),
                iri: merged_rel.clone(),
            },
            replacement: object(format!("{merge_token}-new")),
            spike: None,
            contradicts: false,
            reason: Some("smoke scoped revision".into()),
            effective: Default::default(),
        })
        .await?;
    let replacement_rel = relationship_iri(&replacement)?;
    let replace_a =
        fact_relationships(&search(&service, vec![layer_a.clone()], &merge_token).await?)?;
    let replace_b =
        fact_relationships(&search(&service, vec![layer_b.clone()], &merge_token).await?)?;
    report.check(
        "revise moves only selected memberships and preserves unrelated scope",
        relation_state(&graph, &merged_rel).await? == Some((vec![layer_b.clone()], true, 0))
            && relation_state(&graph, &replacement_rel).await?
                == Some((vec![layer_a.clone()], true, 0))
            && replace_a.contains(&replacement_rel)
            && !replace_a.contains(&merged_rel)
            && replace_b.contains(&merged_rel)
            && !replace_b.contains(&replacement_rel),
        format!(
            "old={:?} new={:?} A={replace_a:?} B={replace_b:?}",
            relation_state(&graph, &merged_rel).await?,
            relation_state(&graph, &replacement_rel).await?
        ),
    );
    let revision_history = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone(), layer_b.clone()],
            text: None,
            iris: None,
            labels: None,
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: Some(replacement_rel.clone()),
            detail: Some("concise".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    report.check(
        "history exposes exact non-pasteable SUPERSEDES revision events",
        revision_history
            .pointer("/revisions/0/replacement/iri")
            .and_then(Value::as_str)
            == Some(replacement_rel.as_str())
            && revision_history
                .pointer("/revisions/0/previous/iri")
                .and_then(Value::as_str)
                == Some(merged_rel.as_str())
            && revision_history
                .pointer("/revisions/0/supersedes/iri")
                .and_then(Value::as_str)
                .is_some()
            && revision_history
                .pointer("/revisions/0/scope/0")
                .and_then(Value::as_str)
                == Some(layer_a.as_str())
            && revision_history
                .pointer("/lookups/0/facts")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty),
        &revision_history,
    );

    let withdrawn = service
        .withdraw(WithdrawArgs {
            scope: vec![layer_b.clone()],
            target: Some(TargetArgs {
                kind: "fact".into(),
                iri: merged_rel.clone(),
            }),
            subject: None,
            p: None,
            reason: Some("smoke final scoped withdrawal".into()),
        })
        .await?;
    report.check(
        "withdraw retires a fact when its last named membership is removed",
        withdrawn.get("withdrawn").and_then(Value::as_u64) == Some(1)
            && relation_state(&graph, &merged_rel).await?
                == Some((vec![layer_b.clone()], false, 0)),
        format!(
            "response={withdrawn} state={:?}",
            relation_state(&graph, &merged_rel).await?
        ),
    );

    let broad_subject_name = format!("broad-withdraw-{tag}");
    let broad_a = service
        .write(write_args(
            entity(broad_subject_name.clone()),
            property.clone(),
            object(format!("broad-a-{tag}")),
            vec![layer_a.clone()],
        ))
        .await?;
    let broad_ab = service
        .write(write_args(
            entity(broad_subject_name.clone()),
            property.clone(),
            object(format!("broad-ab-{tag}")),
            vec![layer_a.clone()],
        ))
        .await?;
    service
        .write(write_args(
            entity(broad_subject_name.clone()),
            property.clone(),
            object_iri(object_result_iri(&broad_ab)?),
            vec![layer_b.clone()],
        ))
        .await?;
    let broad_b = service
        .write(write_args(
            entity(broad_subject_name),
            property.clone(),
            object(format!("broad-b-{tag}")),
            vec![layer_b.clone()],
        ))
        .await?;
    let broad_a_rel = relationship_iri(&broad_a)?;
    let broad_ab_rel = relationship_iri(&broad_ab)?;
    let broad_b_rel = relationship_iri(&broad_b)?;
    let broad_withdrawal = service
        .withdraw(WithdrawArgs {
            scope: vec![layer_a.clone()],
            target: None,
            subject: Some(entity_iri(subject_iri(&broad_a)?)),
            p: None,
            reason: Some("smoke broad withdrawal batch".into()),
        })
        .await?;
    report.check(
        "broad withdrawal batches retirement and surviving membership updates",
        broad_withdrawal.get("withdrawn").and_then(Value::as_u64) == Some(2)
            && relation_state(&graph, &broad_a_rel).await?
                == Some((vec![layer_a.clone()], false, 0))
            && relation_state(&graph, &broad_ab_rel).await?
                == Some((vec![layer_b.clone()], true, 0))
            && relation_state(&graph, &broad_b_rel).await?
                == Some((vec![layer_b.clone()], true, 0)),
        format!(
            "response={broad_withdrawal} a={:?} ab={:?} b={:?}",
            relation_state(&graph, &broad_a_rel).await?,
            relation_state(&graph, &broad_ab_rel).await?,
            relation_state(&graph, &broad_b_rel).await?
        ),
    );

    let rank_token = format!("rank-{tag}");
    let low = service
        .write(write_args(
            entity(format!("{rank_token}-low-subject")),
            property.clone(),
            object(format!("{rank_token}-low-object")),
            vec![layer_a.clone()],
        ))
        .await?;
    let high = service
        .write(write_args(
            entity(format!("{rank_token}-high-subject")),
            property.clone(),
            object(format!("{rank_token}-high-object")),
            vec![layer_a.clone()],
        ))
        .await?;
    let low_rel = relationship_iri(&low)?;
    let high_rel = relationship_iri(&high)?;
    let high_subject = subject_iri(&high)?;
    let strengthened_node = service
        .judge(JudgeArgs {
            scope: vec![layer_a.clone()],
            ratings: vec![JudgeRating {
                target: TargetArgs {
                    kind: "node".into(),
                    iri: high_subject,
                },
                mode: "strengthen".into(),
            }],
        })
        .await?;
    let strengthened_rel = service
        .judge(JudgeArgs {
            scope: vec![layer_a.clone()],
            ratings: vec![JudgeRating {
                target: TargetArgs {
                    kind: "fact".into(),
                    iri: high_rel.clone(),
                },
                mode: "strengthen".into(),
            }],
        })
        .await?;
    let weakened_rel = service
        .judge(JudgeArgs {
            scope: vec![layer_a.clone()],
            ratings: vec![JudgeRating {
                target: TargetArgs {
                    kind: "fact".into(),
                    iri: low_rel.clone(),
                },
                mode: "weaken".into(),
            }],
        })
        .await?;
    let ranked = search(&service, vec![layer_a.clone()], &rank_token).await?;
    report.check(
        "node and fact judgments are signed and affect ranking within a tier",
        strengthened_node
            .pointer("/items/0/after")
            .and_then(Value::as_i64)
            == Some(1)
            && strengthened_rel
                .pointer("/items/0/after")
                .and_then(Value::as_i64)
                == Some(1)
            && weakened_rel
                .pointer("/items/0/after")
                .and_then(Value::as_i64)
                == Some(-1)
            && fact_position(&ranked, &high_rel) == Some(0)
            && fact_position(&ranked, &high_rel) < fact_position(&ranked, &low_rel),
        format!(
            "node={strengthened_node} high={strengthened_rel} low={weakened_rel} ranked={ranked}"
        ),
    );

    let mut concurrent_judgments = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let service = service.clone();
        let layer = layer_a.clone();
        let relationship = high_rel.clone();
        concurrent_judgments.spawn(async move {
            service
                .judge(JudgeArgs {
                    scope: vec![layer],
                    ratings: vec![JudgeRating {
                        target: TargetArgs {
                            kind: "fact".into(),
                            iri: relationship,
                        },
                        mode: "strengthen".into(),
                    }],
                })
                .await
        });
    }
    let mut concurrent_successes = 0;
    while let Some(result) = concurrent_judgments.join_next().await {
        result.context("join concurrent judgment task")??;
        concurrent_successes += 1;
    }
    report.check(
        "concurrent judgments do not lose updates",
        concurrent_successes == 8
            && relation_state(&graph, &high_rel).await? == Some((vec![layer_a.clone()], true, 9)),
        format!(
            "successes={concurrent_successes} state={:?}",
            relation_state(&graph, &high_rel).await?
        ),
    );

    let judge_episodes_before = episode_count(&graph, "judge").await?;
    let judged = service
        .judge(JudgeArgs {
            scope: vec![layer_a.clone()],
            ratings: vec![
                JudgeRating {
                    target: TargetArgs {
                        kind: "fact".into(),
                        iri: high_rel.clone(),
                    },
                    mode: "strengthen".into(),
                },
                JudgeRating {
                    target: TargetArgs {
                        kind: "fact".into(),
                        iri: low_rel.clone(),
                    },
                    mode: "weaken".into(),
                },
            ],
        })
        .await?;
    let judge_episodes_after = episode_count(&graph, "judge").await?;
    let high_after_batch = relation_state(&graph, &high_rel).await?;
    let rollback = service
        .judge(JudgeArgs {
            scope: vec![layer_a.clone()],
            ratings: vec![
                JudgeRating {
                    target: TargetArgs {
                        kind: "fact".into(),
                        iri: high_rel.clone(),
                    },
                    mode: "strengthen".into(),
                },
                JudgeRating {
                    target: TargetArgs {
                        kind: "fact".into(),
                        iri: format!("mindreader:relationship/missing-{tag}"),
                    },
                    mode: "strengthen".into(),
                },
            ],
        })
        .await;
    report.check(
        "judge batches atomically under one Episode and rolls back mixed failures",
        judged.pointer("/episode/tool").and_then(Value::as_str) == Some("judge")
            && judged
                .get("items")
                .and_then(Value::as_array)
                .is_some_and(|items| items.len() == 2)
            && judge_episodes_after == judge_episodes_before + 1
            && rollback.is_err()
            && relation_state(&graph, &high_rel).await? == high_after_batch
            && episode_count(&graph, "judge").await? == judge_episodes_after,
        format!(
            "judged={judged} rollback={rollback:?} high={high_after_batch:?} episodes={judge_episodes_before}->{judge_episodes_after}"
        ),
    );

    let closure_token = format!("closure-{tag}");
    let closure = service
        .write(write_args(
            entity(format!("{closure_token}-subject")),
            property.clone(),
            object(format!("{closure_token}-object")),
            vec![layer_a.clone()],
        ))
        .await?;
    let closure_rel = relationship_iri(&closure)?;
    let closure_subject = subject_iri(&closure)?;
    let closure_object = object_result_iri(&closure)?;
    let premature_relation_add = service
        .place(PlaceArgs {
            scope: vec![layer_a.clone()],
            edits: vec![PlaceEdit {
                target: TargetArgs {
                    kind: "fact".into(),
                    iri: closure_rel.clone(),
                },
                add: vec![layer_c.clone()],
                remove: Vec::new(),
            }],
        })
        .await;
    let subject_add = service
        .place(PlaceArgs {
            scope: vec![layer_a.clone()],
            edits: vec![PlaceEdit {
                target: TargetArgs {
                    kind: "node".into(),
                    iri: closure_subject.clone(),
                },
                add: vec![layer_c.clone()],
                remove: Vec::new(),
            }],
        })
        .await?;
    let object_add = service
        .place(PlaceArgs {
            scope: vec![layer_a.clone()],
            edits: vec![PlaceEdit {
                target: TargetArgs {
                    kind: "node".into(),
                    iri: closure_object.clone(),
                },
                add: vec![layer_c.clone()],
                remove: Vec::new(),
            }],
        })
        .await?;
    let relation_add = service
        .place(PlaceArgs {
            scope: vec![layer_a.clone()],
            edits: vec![PlaceEdit {
                target: TargetArgs {
                    kind: "fact".into(),
                    iri: closure_rel.clone(),
                },
                add: vec![layer_c.clone()],
                remove: Vec::new(),
            }],
        })
        .await?;
    let invalid_endpoint_remove = service
        .place(PlaceArgs {
            scope: vec![layer_a.clone(), layer_c.clone()],
            edits: vec![PlaceEdit {
                target: TargetArgs {
                    kind: "node".into(),
                    iri: closure_subject.clone(),
                },
                add: Vec::new(),
                remove: vec![layer_a.clone()],
            }],
        })
        .await;
    report.check(
        "place succeeds only when endpoint/fact closure is preserved",
        premature_relation_add.is_err()
            && subject_add.get("noop").and_then(Value::as_bool) == Some(false)
            && object_add.get("noop").and_then(Value::as_bool) == Some(false)
            && relation_add.pointer("/items/0/memberships").and_then(Value::as_array).is_some_and(|values| values.len() == 2)
            && relation_state(&graph, &closure_rel).await?
                == Some((vec![layer_a.clone(), layer_c.clone()], true, 0))
            && invalid_endpoint_remove.is_err(),
        format!("premature={premature_relation_add:?} subject={subject_add} object={object_add} relation={relation_add} invalidRemove={invalid_endpoint_remove:?}"),
    );

    let place = service
        .write(write_args(
            entity(format!("place-batch-{tag}-subject")),
            property.clone(),
            object(format!("place-batch-{tag}-object")),
            vec![layer_a.clone()],
        ))
        .await?;
    let place_subject = subject_iri(&place)?;
    let place_object = object_result_iri(&place)?;
    let place_fact = relationship_iri(&place)?;
    let place_episodes_before = episode_count(&graph, "place").await?;
    let placed = service
        .place(PlaceArgs {
            scope: vec![layer_a.clone()],
            edits: [
                ("node", place_subject.clone()),
                ("node", place_object.clone()),
                ("fact", place_fact.clone()),
            ]
            .into_iter()
            .map(|(kind, iri)| PlaceEdit {
                target: TargetArgs {
                    kind: kind.into(),
                    iri,
                },
                add: vec![layer_c.clone()],
                remove: Vec::new(),
            })
            .collect(),
        })
        .await?;
    let place_episodes_after = episode_count(&graph, "place").await?;
    let place_rollback = service
        .place(PlaceArgs {
            scope: vec![layer_a.clone(), layer_c.clone()],
            edits: vec![
                PlaceEdit {
                    target: TargetArgs {
                        kind: "node".into(),
                        iri: place_subject.clone(),
                    },
                    add: Vec::new(),
                    remove: vec![layer_c.clone()],
                },
                PlaceEdit {
                    target: TargetArgs {
                        kind: "node".into(),
                        iri: format!("mindreader:element/missing-{tag}"),
                    },
                    add: vec![layer_c.clone()],
                    remove: Vec::new(),
                },
            ],
        })
        .await;
    report.check(
        "place validates combined final closure, records one Episode, and rolls back mixed failures",
        placed.pointer("/episode/tool").and_then(Value::as_str) == Some("place")
            && placed
                .get("items")
                .and_then(Value::as_array)
                .is_some_and(|items| items.len() == 3)
            && place_episodes_after == place_episodes_before + 1
            && relation_state(&graph, &place_fact).await?
                == Some((vec![layer_a.clone(), layer_c.clone()], true, 0))
            && place_rollback.is_err()
            && node_layers(&graph, &place_subject).await?
                == Some(vec![layer_a.clone(), layer_c.clone()])
            && episode_count(&graph, "place").await? == place_episodes_after,
        format!(
            "placed={placed} rollback={place_rollback:?} episodes={place_episodes_before}->{place_episodes_after}"
        ),
    );

    let concurrent_place = service
        .write(write_args(
            entity(format!("place-concurrent-{tag}-subject")),
            property.clone(),
            object(format!("place-concurrent-{tag}-object")),
            vec![layer_a.clone()],
        ))
        .await?;
    let concurrent_subject = subject_iri(&concurrent_place)?;
    let concurrent_object = object_result_iri(&concurrent_place)?;
    let concurrent_fact = relationship_iri(&concurrent_place)?;
    service
        .place(PlaceArgs {
            scope: vec![layer_a.clone()],
            edits: vec![
                PlaceEdit {
                    target: TargetArgs {
                        kind: "node".into(),
                        iri: concurrent_subject.clone(),
                    },
                    add: vec![layer_b.clone()],
                    remove: Vec::new(),
                },
                PlaceEdit {
                    target: TargetArgs {
                        kind: "node".into(),
                        iri: concurrent_object,
                    },
                    add: vec![layer_b.clone()],
                    remove: Vec::new(),
                },
            ],
        })
        .await?;
    let fact_edit = PlaceArgs {
        scope: vec![layer_a.clone(), layer_b.clone()],
        edits: vec![PlaceEdit {
            target: TargetArgs {
                kind: "fact".into(),
                iri: concurrent_fact.clone(),
            },
            add: vec![layer_b.clone()],
            remove: vec![layer_a.clone()],
        }],
    };
    let endpoint_edit = PlaceArgs {
        scope: vec![layer_a.clone(), layer_b.clone()],
        edits: vec![PlaceEdit {
            target: TargetArgs {
                kind: "node".into(),
                iri: concurrent_subject.clone(),
            },
            add: Vec::new(),
            remove: vec![layer_b.clone()],
        }],
    };
    let (fact_edit_result, endpoint_edit_result) =
        tokio::join!(service.place(fact_edit), service.place(endpoint_edit),);
    let concurrent_successes =
        usize::from(fact_edit_result.is_ok()) + usize::from(endpoint_edit_result.is_ok());
    let concurrent_fact_layers = relation_state(&graph, &concurrent_fact)
        .await?
        .ok_or_else(|| operation_error!("concurrent placement fact disappeared"))?
        .0;
    let concurrent_subject_layers = node_layers(&graph, &concurrent_subject)
        .await?
        .ok_or_else(|| operation_error!("concurrent placement subject disappeared"))?;
    report.check(
        "concurrent fact and endpoint placement serializes closure decisions",
        concurrent_successes == 1
            && (concurrent_fact_layers.is_empty()
                || concurrent_subject_layers.is_empty()
                || concurrent_fact_layers
                    .iter()
                    .all(|layer| concurrent_subject_layers.contains(layer))),
        format!(
            "factEdit={fact_edit_result:?} endpointEdit={endpoint_edit_result:?} factLayers={concurrent_fact_layers:?} subjectLayers={concurrent_subject_layers:?}"
        ),
    );

    let schema_property_iri = format!("mindreader:property/{property}");
    let schema_place = service
        .place(PlaceArgs {
            scope: Vec::new(),
            edits: vec![PlaceEdit {
                target: TargetArgs {
                    kind: "node".into(),
                    iri: schema_property_iri.clone(),
                },
                add: vec![layer_a.clone()],
                remove: Vec::new(),
            }],
        })
        .await;
    report.check(
        "place keeps Class and Property schema records global",
        schema_place.is_err(),
        format!("schemaPlace={schema_place:?}"),
    );

    let exact = service
        .recall(RecallArgs {
            scope: vec![layer_b],
            text: None,
            iris: Some(vec![subject_iri(&in_b)?]),
            labels: None,
            around: None,
            hops: Some(1),
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    report.check(
        "stable relationship IRI round-trips through scoped retrieval",
        exact
            .pointer("/lookups/0/facts")
            .and_then(Value::as_array)
            .is_some_and(|facts| {
                facts
                    .iter()
                    .any(|fact| fact.pointer("/target/iri").and_then(Value::as_str) == Some(&b_rel))
            }),
        &exact,
    );

    let missing_iri = format!("mindreader:element/recall-missing-{tag}");
    let recall_order = vec![
        closure_object.clone(),
        missing_iri.clone(),
        closure_subject.clone(),
    ];
    let recalled = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone(), layer_c.clone()],
            text: None,
            iris: Some(recall_order.clone()),
            labels: None,
            around: None,
            hops: Some(1),
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: None,
            limit: Some(1),
            effective_at: None,
        })
        .await?;
    let lookup_order = recalled
        .get("lookups")
        .and_then(Value::as_array)
        .ok_or_else(|| operation_error!("IRI recall has no lookups array: {recalled}"))?
        .iter()
        .map(|lookup| {
            lookup
                .get("iri")
                .and_then(Value::as_str)
                .ok_or_else(|| operation_error!("recall lookup has no iri: {lookup}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let filtered_around = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone(), layer_c.clone()],
            text: None,
            iris: None,
            labels: None,
            around: Some(closure_subject.clone()),
            hops: None,
            p: Some(vec![format!("not-{property}")]),
            depth: Some(1),
            direction: None,
            history: None,
            detail: None,
            limit: Some(1),
            effective_at: None,
        })
        .await?;
    let witnessed_around = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone(), layer_c.clone()],
            text: None,
            iris: None,
            labels: None,
            around: Some(closure_subject.clone()),
            hops: None,
            p: Some(vec![property.clone()]),
            depth: Some(1),
            direction: None,
            history: None,
            detail: None,
            limit: Some(1),
            effective_at: None,
        })
        .await?;
    let route_property = format!("route-{tag}");
    let detour_property = format!("detour-{tag}");
    let route_a = format!("route-{tag}-a");
    let route_b = format!("route-{tag}-b");
    let route_c = format!("route-{tag}-c");
    let route_d = format!("route-{tag}-d");
    let route_e = format!("route-{tag}-e");
    for (subject, predicate, target) in [
        (&route_a, &route_property, &route_b),
        (&route_b, &route_property, &route_c),
        (&route_b, &detour_property, &route_d),
        (&route_d, &route_property, &route_e),
    ] {
        service
            .write(write_args(
                entity(subject.clone()),
                predicate,
                object(target.clone()),
                vec![layer_a.clone()],
            ))
            .await?;
    }
    let route_a_iri = format!("mindreader:element/{route_a}");
    let route_c_iri = format!("mindreader:element/{route_c}");
    let route_outgoing = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: None,
            iris: None,
            labels: None,
            around: Some(route_a_iri),
            hops: None,
            p: Some(vec![route_property.clone()]),
            depth: Some(3),
            direction: Some("outgoing".into()),
            history: None,
            detail: Some("concise".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    let route_incoming = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: None,
            iris: None,
            labels: None,
            around: Some(route_c_iri.clone()),
            hops: None,
            p: Some(vec![route_property.clone()]),
            depth: Some(3),
            direction: Some("incoming".into()),
            history: None,
            detail: Some("concise".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    let route_wrong_direction = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: None,
            iris: None,
            labels: None,
            around: Some(route_c_iri),
            hops: None,
            p: Some(vec![route_property.clone()]),
            depth: Some(3),
            direction: Some("outgoing".into()),
            history: None,
            detail: Some("concise".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    let route_property_iri = format!("mindreader:property/{route_property}");
    let route_paths_are_filtered = |value: &Value| {
        value
            .get("paths")
            .and_then(Value::as_array)
            .is_some_and(|paths| {
                paths.len() == 2
                    && paths.iter().all(|path| {
                        path.get("edges")
                            .and_then(Value::as_array)
                            .is_some_and(|edges| {
                                !edges.is_empty()
                                    && edges.iter().all(|edge| {
                                        edge.get("p").and_then(Value::as_str)
                                            == Some(route_property_iri.as_str())
                                    })
                            })
                    })
            })
    };
    report.check(
        "around constrains every witness edge by predicate and direction",
        route_outgoing
            .get("facts")
            .and_then(Value::as_array)
            .is_some_and(|facts| facts.len() == 2)
            && route_paths_are_filtered(&route_outgoing)
            && route_incoming
                .get("facts")
                .and_then(Value::as_array)
                .is_some_and(|facts| facts.len() == 2)
            && route_paths_are_filtered(&route_incoming)
            && route_wrong_direction
                .get("facts")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty),
        format!(
            "outgoing={route_outgoing} incoming={route_incoming} wrong={route_wrong_direction}"
        ),
    );
    let catalog_before = service
        .recall(RecallArgs {
            scope: Vec::new(),
            text: None,
            iris: None,
            labels: Some(vec!["Property".into()]),
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: None,
            limit: Some(100),
            effective_at: None,
        })
        .await?;
    let judged_schema_iri = catalog_before
        .pointer("/nodes/0/iri")
        .and_then(Value::as_str)
        .ok_or_else(|| operation_error!("Property catalog has no first node: {catalog_before}"))?
        .to_string();
    let judged_schema_weight = catalog_before
        .pointer("/nodes/0/weight")
        .and_then(Value::as_i64)
        .ok_or_else(|| {
            operation_error!("Property catalog has no first weight: {catalog_before}")
        })?;
    let schema_judgment = service
        .judge(JudgeArgs {
            scope: Vec::new(),
            ratings: vec![JudgeRating {
                target: TargetArgs {
                    kind: "node".into(),
                    iri: judged_schema_iri.clone(),
                },
                mode: "strengthen".into(),
            }],
        })
        .await?;
    let catalog = service
        .recall(RecallArgs {
            scope: Vec::new(),
            text: None,
            iris: None,
            labels: Some(vec!["Property".into()]),
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: None,
            limit: Some(100),
            effective_at: None,
        })
        .await?;
    report.check(
        "recall preserves IRI order and misses, enforces one fact budget, and returns filtered witness paths",
        recalled.get("mode").and_then(Value::as_str) == Some("iris")
            && lookup_order == recall_order.iter().map(String::as_str).collect::<Vec<_>>()
            && recalled
                .get("lookups")
                .and_then(Value::as_array)
                .and_then(|lookups| lookups.get(1))
                .and_then(|lookup| lookup.get("found"))
                .and_then(Value::as_bool)
                == Some(false)
            && recalled
                .get("lookups")
                .and_then(Value::as_array)
                .is_some_and(|lookups| {
                    lookups.iter().filter(|lookup| {
                        lookup.get("found").and_then(Value::as_bool) == Some(true)
                    }).all(|lookup| {
                        lookup
                            .get("facts")
                            .and_then(Value::as_array)
                            .is_some_and(|facts| facts.len() <= 1)
                    })
                })
            && filtered_around
                .get("facts")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            && witnessed_around
                .get("facts")
                .and_then(Value::as_array)
                .is_some_and(|facts| facts.len() == 1)
            && witnessed_around
                .get("paths")
                .and_then(Value::as_array)
                .is_some_and(|paths| {
                    paths.len() == 1
                        && paths[0]
                            .get("nodes")
                            .and_then(Value::as_array)
                            .is_some_and(|nodes| {
                                nodes.first().and_then(Value::as_str)
                                    == Some(closure_subject.as_str())
                            })
                        && paths[0]
                            .get("edges")
                            .and_then(Value::as_array)
                            .is_some_and(|edges| edges.len() == 1)
                }),
        format!(
            "iris={recalled} filtered={filtered_around} witnessed={witnessed_around}"
        ),
    );
    report.check(
        "recall catalog emits pasteable node handles in the normalized schema",
        catalog.get("mode").and_then(Value::as_str) == Some("catalog")
            && schema_judgment
                .pointer("/items/0/after")
                .and_then(Value::as_i64)
                == Some(judged_schema_weight + 1)
            && catalog
                .get("nodes")
                .and_then(Value::as_array)
                .is_some_and(|nodes| {
                    !nodes.is_empty()
                        && nodes.iter().all(|node| {
                            node.get("kind").and_then(Value::as_str) == Some("node")
                                && node.pointer("/target/kind").and_then(Value::as_str)
                                    == Some("node")
                                && node
                                    .get("scope")
                                    .and_then(Value::as_array)
                                    .is_some_and(Vec::is_empty)
                                && node.get("stub").and_then(Value::as_bool) == Some(false)
                                && node.get("weight").and_then(Value::as_i64).is_some()
                        })
                })
            && catalog
                .get("nodes")
                .and_then(Value::as_array)
                .and_then(|nodes| {
                    nodes.iter().find(|node| {
                        node.get("iri").and_then(Value::as_str) == Some(judged_schema_iri.as_str())
                    })
                })
                .and_then(|node| node.get("weight"))
                .and_then(Value::as_i64)
                == Some(judged_schema_weight + 1),
        &catalog,
    );

    let hops0 = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone(), layer_c.clone()],
            text: None,
            iris: Some(vec![closure_subject.clone()]),
            labels: None,
            around: None,
            hops: Some(0),
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("concise".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    let history_iri = relationship_iri(&witnessed_around)?;
    let history = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone(), layer_c.clone()],
            text: None,
            iris: None,
            labels: None,
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: Some(history_iri.clone()),
            detail: None,
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    report.check(
        "concise IRI recall returns answer-only incident facts and history walks a fact",
        hops0
            .pointer("/lookups/0/facts")
            .and_then(Value::as_array)
            .is_some_and(|facts| !facts.is_empty())
            && hops0.get("detail").and_then(Value::as_str) == Some("concise")
            && hops0.pointer("/lookups/0/facts/0/target").is_none()
            && hops0.get("handles").is_none()
            && hops0.get("mode").is_none()
            && hops0.get("scope").is_none()
            && history.get("mode").and_then(Value::as_str) == Some("history")
            && history
                .get("facts")
                .and_then(Value::as_array)
                .is_some_and(|facts| {
                    facts.iter().any(|fact| {
                        fact.pointer("/target/iri").and_then(Value::as_str)
                            == Some(history_iri.as_str())
                            && fact.get("current").and_then(Value::as_bool) == Some(true)
                    })
                }),
        format!("hops0={hops0} history={history}"),
    );

    report.check(
        "query-time embedding space rejects a mismatched process space",
        require_embedding_space(&graph, &embedding_space)
            .await
            .is_ok()
            && require_embedding_space(
                &graph,
                &EmbeddingSpace {
                    provider: "openai".into(),
                    model: "text-embedding-3-small".into(),
                    dimensions: 1536,
                },
            )
            .await
            .is_err(),
        "smoke space accepted; openai/1536 rejected",
    );

    let spike_name = format!("spike-id-{tag}");
    let spiked = service
        .write(WriteArgs {
            facts: vec![WriteFact {
                s: entity(spike_name.clone()),
                p: property.clone(),
                o: object(format!("spike-obj-{tag}")),
                spike: Some("Knowledge".into()),
                contradicts: false,
                effective: None,
            }],
            scope: vec![layer_a.clone()],
        })
        .await?;
    let spiked_iri = subject_iri(&spiked)?;
    let spiked_relationship = relationship_iri(&spiked)?;
    let implicit_about_count = fetch_one(
        &graph,
        query(
            "MATCH (:Entity {iri: $iri})-[a:ABOUT]->(:Entity) \
             WHERE a.validTo IS NULL RETURN count(a) AS count",
        )
        .param("iri", spiked_iri.clone()),
    )
    .await?
    .ok_or_else(|| operation_error!("implicit ABOUT count returned no row"))?
    .get::<i64>("count")?;
    report.check(
        "Knowledge classifies only the exact fact and creates no implicit ABOUT",
        spiked_iri == format!("mindreader:element/spike-id-{tag}")
            && spiked.pointer("/facts/0/spike").and_then(Value::as_str) == Some("Knowledge")
            && implicit_about_count == 0,
        format!("iri={spiked_iri} spiked={spiked}"),
    );

    let explicit_about = service
        .write(WriteArgs {
            facts: vec![WriteFact {
                s: entity(format!("explicit-context-{tag}")),
                p: "ABOUT".into(),
                o: ObjectInput {
                    kind: "node".into(),
                    iri: Some(spiked_iri.clone()),
                    name: None,
                    labels: Vec::new(),
                    value: None,
                    datatype: None,
                },
                spike: Some("Insight".into()),
                contradicts: false,
                effective: None,
            }],
            scope: vec![layer_a.clone()],
        })
        .await?;
    let explicit_about_iri = relationship_iri(&explicit_about)?;
    let spiked_recall = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: Some(spike_name),
            iris: None,
            labels: None,
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    report.check(
        "explicit ABOUT appears only as ranked context, never as an ordinary fact",
        spiked_recall
            .get("facts")
            .and_then(Value::as_array)
            .is_some_and(|facts| {
                facts.iter().any(|fact| {
                    fact.pointer("/target/iri").and_then(Value::as_str)
                        == Some(spiked_relationship.as_str())
                }) && facts
                    .iter()
                    .all(|fact| fact.get("p").and_then(Value::as_str) != Some("ABOUT"))
            })
            && spiked_recall
                .get("about")
                .and_then(Value::as_array)
                .is_some_and(|about| {
                    about.iter().any(|context| {
                        context.pointer("/relationship/iri").and_then(Value::as_str)
                            == Some(explicit_about_iri.as_str())
                            && context.get("rank").and_then(Value::as_str) == Some("Insight")
                    })
                }),
        format!("write={explicit_about} recall={spiked_recall}"),
    );

    let fanout_subject = format!("fanout-{tag}");
    let mut fanout_facts = Vec::new();
    for index in 0..20 {
        fanout_facts.push(WriteFact {
            s: entity(fanout_subject.clone()),
            p: property.clone(),
            o: object(format!("fanout-{tag}-{index}")),
            spike: None,
            contradicts: false,
            effective: None,
        });
    }
    service
        .write(WriteArgs {
            facts: fanout_facts,
            scope: vec![layer_a.clone()],
        })
        .await?;
    let mut extra_fanout = Vec::new();
    for index in 20..25 {
        extra_fanout.push(WriteFact {
            s: entity(fanout_subject.clone()),
            p: property.clone(),
            o: object(format!("fanout-{tag}-{index}")),
            spike: None,
            contradicts: false,
            effective: None,
        });
    }
    service
        .write(WriteArgs {
            facts: extra_fanout,
            scope: vec![layer_a.clone()],
        })
        .await?;
    let fanout_iri = format!("mindreader:element/{fanout_subject}");
    let hops0_budget = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: None,
            iris: Some(vec![fanout_iri.clone()]),
            labels: None,
            around: None,
            hops: Some(0),
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    let hops1_budget = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: None,
            iris: Some(vec![fanout_iri]),
            labels: None,
            around: None,
            hops: Some(1),
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    let hops0_iris = lookup_fact_iris(&hops0_budget)?;
    let hops1_iris = lookup_fact_iris(&hops1_budget)?;
    report.check(
        "iris hops 0 and 1 share a per-root fact budget and truncate at 20",
        hops0_iris == hops1_iris
            && hops0_iris.len() == 20
            && hops0_budget.get("truncated").and_then(Value::as_bool) == Some(true)
            && hops1_budget.get("truncated").and_then(Value::as_bool) == Some(true),
        format!("hops0={hops0_budget} hops1={hops1_budget}"),
    );

    let camel_subject = format!("camel-{tag}");
    service
        .write(write_args(
            entity(camel_subject.clone()),
            "graphModel",
            object(format!("camel-object-{tag}")),
            vec![layer_a.clone()],
        ))
        .await?;
    let camel_recall = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: Some("graphModel".into()),
            iris: None,
            labels: None,
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    report.check(
        "text recall finds a camelCase predicate via the Property catalog",
        camel_recall
            .get("facts")
            .and_then(Value::as_array)
            .is_some_and(|facts| {
                facts.iter().any(|fact| {
                    fact.get("p").and_then(Value::as_str) == Some("graphModel")
                        || fact
                            .get("p")
                            .and_then(Value::as_str)
                            .is_some_and(|value| value.ends_with("graphModel"))
                })
            }),
        &camel_recall,
    );

    let cold_subject = format!("cold-keyword-{tag}");
    let cold_write = service
        .write(write_args(
            entity(cold_subject),
            "coldStartBehavior",
            object(format!(
                "resilient server startup without database availability {tag}"
            )),
            vec![layer_a.clone()],
        ))
        .await?;
    let cold_target = relationship_iri(&cold_write)?;
    let cold_query = "can this server start while its database is unavailable".to_string();
    let cold_lexical = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: Some(cold_query.clone()),
            iris: None,
            labels: None,
            around: None,
            hops: None,
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    let cold_semantic = service
        .recall_semantic(SemanticSearchArgs {
            scope: vec![layer_a.clone()],
            text: cold_query,
            labels: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    report.check(
        "cold text and semantic recall use keyword candidates when the full phrase is absent",
        fact_relationships(&cold_lexical)?.contains(&cold_target)
            && fact_relationships(&cold_semantic)?.contains(&cold_target),
        format!("lexical={cold_lexical} semantic={cold_semantic}"),
    );

    let semantic_fanout_label = format!("SemanticFanout{tag}");
    let semantic_fanout_subject = format!("semantic-fanout-{tag}");
    let semantic_fanout = service
        .write(WriteArgs {
            facts: vec![
                WriteFact {
                    s: labeled_entity(semantic_fanout_subject.clone(), &semantic_fanout_label),
                    p: "contains".into(),
                    o: labeled_object(format!("generic-alpha-{tag}"), &semantic_fanout_label),
                    spike: None,
                    contradicts: false,
                    effective: None,
                },
                WriteFact {
                    s: labeled_entity(semantic_fanout_subject.clone(), &semantic_fanout_label),
                    p: "contains".into(),
                    o: labeled_object(format!("generic-beta-{tag}"), &semantic_fanout_label),
                    spike: None,
                    contradicts: false,
                    effective: None,
                },
                WriteFact {
                    s: labeled_entity(semantic_fanout_subject.clone(), &semantic_fanout_label),
                    p: "contains".into(),
                    o: labeled_object(format!("generic-gamma-{tag}"), &semantic_fanout_label),
                    spike: None,
                    contradicts: false,
                    effective: None,
                },
                WriteFact {
                    s: labeled_entity(semantic_fanout_subject.clone(), &semantic_fanout_label),
                    p: "reducesTargetTo".into(),
                    o: labeled_object(format!("answer-skeleton-{tag}"), &semantic_fanout_label),
                    spike: None,
                    contradicts: false,
                    effective: None,
                },
            ],
            scope: vec![layer_a.clone()],
        })
        .await?;
    let semantic_fanout_subject_iri = subject_iri(&semantic_fanout)?;
    let semantic_fanout_generic = semantic_fanout
        .pointer("/facts/0/target/iri")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            operation_error!("semantic fanout write has no generic fact: {semantic_fanout}")
        })?
        .to_string();
    let semantic_fanout_specific = semantic_fanout
        .pointer("/facts/3/target/iri")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            operation_error!("semantic fanout write has no specific fact: {semantic_fanout}")
        })?
        .to_string();
    let semantic_fanout_seed = service
        .recall_semantic(SemanticSearchArgs {
            scope: vec![layer_a.clone()],
            text: format!("{semantic_fanout_subject} reduces target skeleton"),
            labels: Some(vec![semantic_fanout_label.clone()]),
            detail: Some("detailed".into()),
            limit: Some(5),
            effective_at: None,
        })
        .await?;
    let semantic_fanout_activation = fetch_one(
        &graph,
        query(
            r#"
            MATCH (a:SemanticActivationV4)
            WHERE $specific IN a.resultRefs
            UNWIND a.resultRefs AS ref
            MATCH (s:Entity)-[r]->(:Entity) WHERE r.iri = ref
            RETURN size(a.resultRefs) AS refs,
                   count(CASE WHEN s.iri = $subject AND r.propertyIri = $generic THEN 1 END)
                     AS repeatedGroup
            "#,
        )
        .param("specific", semantic_fanout_specific.clone())
        .param("subject", semantic_fanout_subject_iri)
        .param("generic", "mindreader:property/contains"),
    )
    .await?
    .ok_or_else(|| operation_error!("semantic fanout activation was not persisted"))?;
    let semantic_fanout_warm = service
        .recall_semantic(SemanticSearchArgs {
            scope: vec![layer_a.clone()],
            text: "what remains after the devouring is complete".into(),
            labels: Some(vec![semantic_fanout_label]),
            detail: Some("detailed".into()),
            limit: Some(5),
            effective_at: None,
        })
        .await?;
    report.check(
        "relationship evidence defeats endpoint fanout and only fully grounded facts teach",
        semantic_fanout_seed
            .pointer("/facts/0/target/iri")
            .and_then(Value::as_str)
            == Some(semantic_fanout_specific.as_str())
            && semantic_fanout_activation.get::<i64>("refs").unwrap_or_default() == 1
            && semantic_fanout_activation
                .get::<i64>("repeatedGroup")
                .unwrap_or(i64::MAX)
                == 0
            && fact_relationships(&semantic_fanout_warm)?
                .contains(&semantic_fanout_specific)
            && fact_relationships(&semantic_fanout_warm)?.contains(&semantic_fanout_generic),
        format!(
            "seed={semantic_fanout_seed} activation={semantic_fanout_activation:?} warm={semantic_fanout_warm}"
        ),
    );

    let activation_only_label = format!("ActivationOnly{tag}");
    let activation_only_write = service
        .write(write_args(
            labeled_entity(format!("activation-source-{tag}"), &activation_only_label),
            "recalls",
            labeled_object(format!("activation-answer-{tag}"), &activation_only_label),
            vec![layer_a.clone()],
        ))
        .await?;
    let activation_only_ref = relationship_iri(&activation_only_write)?;
    let activation_only_text = format!("groundlessparaphrase{tag}");
    let activation_only_embedding = SmokeEmbedding.embed(&activation_only_text).await?;
    seed_semantic_activation(
        &graph,
        &activation_only_embedding,
        std::slice::from_ref(&activation_only_ref),
        600_000,
    )
    .await?;
    let activation_only_count_before = fetch_one(
        &graph,
        query("MATCH (a:SemanticActivationV4) RETURN count(a) AS count"),
    )
    .await?
    .ok_or_else(|| operation_error!("activation-only count returned no row"))?
    .get::<i64>("count")?;
    let activation_only_recall = service
        .recall_semantic(SemanticSearchArgs {
            scope: vec![layer_a.clone()],
            text: activation_only_text,
            labels: Some(vec![activation_only_label]),
            detail: Some("detailed".into()),
            limit: Some(5),
            effective_at: None,
        })
        .await?;
    let activation_only_count_after = fetch_one(
        &graph,
        query("MATCH (a:SemanticActivationV4) RETURN count(a) AS count"),
    )
    .await?
    .ok_or_else(|| operation_error!("activation-only count returned no row"))?
    .get::<i64>("count")?;
    report.check(
        "activation-only recall returns learned facts without spawning another bundle",
        fact_relationships(&activation_only_recall)?.contains(&activation_only_ref)
            && activation_only_count_after == activation_only_count_before,
        format!(
            "result={activation_only_recall} activations={activation_only_count_before}->{activation_only_count_after}"
        ),
    );

    let activation_count_before_empty = fetch_one(
        &graph,
        query("MATCH (a:SemanticActivation) RETURN count(a) AS count"),
    )
    .await?
    .ok_or_else(|| operation_error!("semantic activation count returned no row"))?
    .get::<i64>("count")?;
    let empty_semantic = service
        .recall_semantic(SemanticSearchArgs {
            scope: vec![layer_a.clone()],
            text: format!("no-match-{tag}"),
            labels: Some(vec![format!("NoSuchSemanticLabel{tag}")]),
            detail: Some("concise".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    let activation_count_after_empty = fetch_one(
        &graph,
        query("MATCH (a:SemanticActivation) RETURN count(a) AS count"),
    )
    .await?
    .ok_or_else(|| operation_error!("semantic activation count returned no row"))?
    .get::<i64>("count")?;
    report.check(
        "semantic misses return empty without persisting empty activations",
        empty_semantic
            .get("facts")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
            && activation_count_after_empty == activation_count_before_empty,
        format!(
            "result={empty_semantic} activations={activation_count_before_empty}->{activation_count_after_empty}"
        ),
    );

    let merge_short = service
        .write(write_args(
            entity(format!("merge-{tag}")),
            "mindreader:property/merge-smoke",
            object(format!("merge-object-{tag}")),
            vec![layer_a.clone()],
        ))
        .await?;
    let merge_long = service
        .write(write_args(
            entity(format!("merge-{tag}s")),
            "mindreader:property/merge-smoke",
            object(format!("merge-object-{tag}")),
            vec![layer_a.clone()],
        ))
        .await?;
    let short_iri = subject_iri(&merge_short)?;
    let long_iri = subject_iri(&merge_long)?;
    let merge_survivor_relationship = std::cmp::min(
        relationship_iri(&merge_short)?,
        relationship_iri(&merge_long)?,
    );
    let suggested = merge_long
        .pointer("/review/unify")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.pointer("/source/iri").and_then(Value::as_str) == Some(long_iri.as_str())
                    && item.pointer("/target/iri").and_then(Value::as_str)
                        == Some(short_iri.as_str())
            })
        });
    let (survivor, merge_judgment) = tokio::try_join!(
        service.unify(UnifyArgs::from_iris(long_iri.clone(), short_iri.clone())),
        service.judge(JudgeArgs {
            scope: vec![layer_a.clone()],
            ratings: vec![JudgeRating {
                target: TargetArgs {
                    kind: "fact".into(),
                    iri: merge_survivor_relationship.clone(),
                },
                mode: "strengthen".into(),
            }],
        })
    )?;
    let removed = service
        .recall(RecallArgs {
            scope: vec![layer_a.clone()],
            text: None,
            iris: Some(vec![long_iri]),
            labels: None,
            around: None,
            hops: Some(0),
            p: None,
            depth: None,
            direction: None,
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
            effective_at: None,
        })
        .await?;
    report.check(
        "merge suggestions prefer the shorter name and unify keeps only the target",
        suggested
            && survivor.pointer("/node/iri").and_then(Value::as_str) == Some(short_iri.as_str())
            && removed.pointer("/lookups/0/found").and_then(Value::as_bool) == Some(false)
            && relation_state(&graph, &merge_survivor_relationship)
                .await?
                .is_some_and(|(_, current, weight)| current && weight == 1),
        format!(
            "suggestions={} survivor={survivor} judgment={merge_judgment} removed={removed}",
            merge_long["review"]["unify"],
        ),
    );

    let same_txn_short_name = format!("007-{tag}");
    let same_txn_long_name = format!("007s-{tag}");
    let same_txn_merge = service
        .write(write_args(
            entity(same_txn_long_name.clone()),
            "mindreader:property/same-transaction-merge-smoke",
            object(same_txn_short_name.clone()),
            vec![layer_a.clone()],
        ))
        .await?;
    report.check(
        "merge suggestions include similar entities created in the same transaction",
        same_txn_merge
            .pointer("/review/unify")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.get("sourceName").and_then(Value::as_str)
                        == Some(same_txn_long_name.as_str())
                        && item.get("targetName").and_then(Value::as_str)
                            == Some(same_txn_short_name.as_str())
                })
            }),
        &same_txn_merge["review"]["unify"],
    );

    let target_property_name = format!("mergeProperty{tag}");
    let source_property_name = format!("mergeProperty{tag}s");
    let target_property_iri = format!("mindreader:property/{target_property_name}");
    let source_property_iri = format!("mindreader:property/{source_property_name}");
    let property_fact = service
        .write(write_args(
            entity(format!("property-merge-subject-{tag}")),
            target_property_iri.clone(),
            object(format!("property-merge-object-{tag}")),
            vec![layer_a.clone()],
        ))
        .await?;
    service
        .write(write_args(
            entity_iri(subject_iri(&property_fact)?),
            source_property_iri.clone(),
            object_iri(object_result_iri(&property_fact)?),
            vec![layer_a.clone()],
        ))
        .await?;
    service
        .unify(UnifyArgs::from_iris(
            source_property_iri.clone(),
            target_property_iri.clone(),
        ))
        .await?;
    let property_state = fetch_one(
        &graph,
        query(
            "MATCH (s:Entity {iri: $subject})-[r]->(o:Entity {iri: $object}) \
             WHERE r.validTo IS NULL \
             RETURN count(r) AS count, collect(r.propertyIri) AS properties, \
                    collect(r.factText) AS factTexts",
        )
        .param("subject", subject_iri(&property_fact)?)
        .param("object", object_result_iri(&property_fact)?),
    )
    .await?
    .ok_or_else(|| operation_error!("property merge aggregate returned no row"))?;
    let wrong_kind = service
        .unify(UnifyArgs::from_iris(
            short_iri.clone(),
            target_property_iri.clone(),
        ))
        .await;
    let incompatible_property = service
        .unify(UnifyArgs::from_iris(
            target_property_iri.clone(),
            "mindreader:property/ABOUT",
        ))
        .await;
    let system_property = service
        .unify(UnifyArgs::from_iris(
            target_property_iri.clone(),
            "mindreader:property/CONTRADICTS",
        ))
        .await;
    report.check(
        "property merges preserve predicate representation and reject incompatible or system-owned kinds",
        property_state.get::<i64>("count")? == 1
            && property_state.get::<Vec<String>>("properties")? == vec![target_property_iri]
            && property_state
                .get::<Vec<String>>("factTexts")
                .context("property merge state is missing factTexts")?
                .iter()
                .all(|text| !text.contains(&source_property_name))
            && wrong_kind.is_err()
            && incompatible_property.is_err()
            && system_property.is_err(),
        format!(
            "propertyState={property_state:?} wrongKind={wrong_kind:?} incompatible={incompatible_property:?} system={system_property:?}"
        ),
    );

    let semantic_text = format!("merge-{tag}");
    let semantic_embedding = SmokeEmbedding.embed(&semantic_text).await?;
    let (contributing_activation_id, contributing_ttl_before) = seed_semantic_activation(
        &graph,
        &semantic_embedding,
        std::slice::from_ref(&replacement_rel),
        600_000,
    )
    .await?;
    let unresolved_ref = format!("mindreader:relationship/missing-{tag}");
    let (unresolved_activation_id, unresolved_ttl_before) =
        seed_semantic_activation(&graph, &semantic_embedding, &[unresolved_ref], 600_000).await?;
    let semantic_args = SemanticSearchArgs {
        scope: vec![layer_a],
        text: semantic_text,
        labels: None,
        detail: None,
        limit: Some(20),
        effective_at: None,
    };
    let semantic_first = service.recall_semantic(semantic_args.clone()).await?;
    let activation_after_first = fetch_one(
        &graph,
        query(
            "MATCH (a:SemanticActivationV4:TTL) \
             RETURN count(a) AS count, max(a.ttl) > timestamp() AS live, \
                    max(size(a.resultRefs)) AS maxRefs",
        ),
    )
    .await?
    .ok_or_else(|| operation_error!("semantic activation aggregate returned no row"))?;
    let semantic_second = service.recall_semantic(semantic_args).await?;
    let activation = fetch_one(
        &graph,
        query(
            "MATCH (a:SemanticActivationV4:TTL) \
             RETURN count(a) AS count, max(a.ttl) > timestamp() AS live, \
                    max(size(a.resultRefs)) AS maxRefs",
        ),
    )
    .await?
    .ok_or_else(|| operation_error!("semantic activation aggregate returned no row"))?;
    let contributing_ttl_after =
        semantic_activation_ttl(&graph, &contributing_activation_id).await?;
    let unresolved_ttl_after = semantic_activation_ttl(&graph, &unresolved_activation_id).await?;
    report.check(
        "semantic search refreshes every contributing activation but not unresolved neighbors",
        semantic_first
            .pointer("/facts/0/rank")
            .and_then(Value::as_u64)
            == Some(1)
            && semantic_second
                .pointer("/facts/0/rank")
                .and_then(Value::as_u64)
                == Some(1)
            && activation.get::<i64>("count").unwrap_or(0)
                == activation_after_first.get::<i64>("count").unwrap_or(-1)
            && activation.get::<i64>("count").unwrap_or(0) >= 1
            && activation.get::<bool>("live").unwrap_or(false)
            && activation.get::<i64>("maxRefs").unwrap_or(i64::MAX) <= 3
            && contributing_ttl_after.is_some_and(|ttl| ttl > contributing_ttl_before)
            && unresolved_ttl_after == Some(unresolved_ttl_before),
        format!(
            "first={semantic_first} second={semantic_second} activationAfterFirst={activation_after_first:?} activationAfterSecond={activation:?} contributingTtl={contributing_ttl_before}->{contributing_ttl_after:?} unresolvedTtl={unresolved_ttl_before}->{unresolved_ttl_after:?}"
        ),
    );

    Ok(report.failed)
}
