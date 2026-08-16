//! Live Neo4j integration suite for the eight memory tools and graph contracts.
//!
//! Mutates the configured database and leaves fixtures in place; use a
//! development or disposable instance only. Enabled with the `developer-tools`
//! feature. Includes a deterministic smoke embedding provider when remote keys
//! are absent.

use async_trait::async_trait;
use mindreader::config::{Config, EmbeddingSpace, SemanticConfig};
use mindreader::domain::{EntityInput, ObjectInput};
use mindreader::embeddings::{normalize_vector, EmbeddingProvider};
use mindreader::error::{Context, Result};
use mindreader::graph::{
    self, acquire_fact_locks_in_txn, fetch_one, merge_node_in_txn, require_embedding_space,
    MergedNode, NodeSpec,
};
use mindreader::merge::{memory_merge, MergeArgs};
use mindreader::operation_error;
use mindreader::search::{RecallArgs, SearchArgs};
use mindreader::semantic::{memory_semantic_search, SemanticRuntime, SemanticSearchArgs};
use mindreader::service::MemoryService;
use mindreader::tools::{
    self, AssertArgs, AssertFact, FeedbackArgs, GetArgs, JudgeArgs, JudgeRating, LayersArgs,
    PlaceArgs, PlaceEdit, ReplaceArgs, RetractArgs, RetractTargetArgs, SchemaArgs, StatsArgs,
    TargetArgs,
};
use mindreader::Mindreader;
use neo4rs::{query, Graph};
use serde_json::Value;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

struct Report {
    next: u32,
    failed: u32,
}

/// Deterministic 3-d fixture used when no remote embedding key is configured.
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

fn entity(name: impl Into<String>) -> EntityInput {
    EntityInput {
        kind: "node".into(),
        iri: None,
        name: Some(name.into()),
        labels: vec!["Element".into()],
    }
}

fn entity_iri(iri: impl Into<String>) -> EntityInput {
    EntityInput {
        kind: "node".into(),
        iri: Some(iri.into()),
        name: None,
        labels: Vec::new(),
    }
}

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

fn assert_args(
    s: EntityInput,
    p: impl AsRef<str>,
    o: ObjectInput,
    scope: Vec<String>,
) -> AssertArgs {
    AssertArgs {
        facts: vec![AssertFact {
            s,
            p: p.as_ref().to_string(),
            o,
            spike: None,
            contradicts: false,
        }],
        scope,
    }
}

fn relationship_iri(value: &Value) -> Result<String> {
    value
        .pointer("/facts/0/target/iri")
        .or_else(|| value.pointer("/facts/0/relationship/iri"))
        .or_else(|| value.pointer("/relationship/iri"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| operation_error!("response has no relationship IRI: {value}"))
}

fn subject_iri(value: &Value) -> Result<String> {
    value
        .pointer("/facts/0/s/iri")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| operation_error!("response has no subject IRI: {value}"))
}

fn object_result_iri(value: &Value) -> Result<String> {
    value
        .pointer("/facts/0/o/iri")
        .or_else(|| value.pointer("/new/iri"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| operation_error!("response has no object IRI: {value}"))
}

fn fact_relationships(value: &Value) -> Vec<String> {
    value
        .get("facts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|fact| {
            fact.pointer("/target/iri")
                .or_else(|| fact.pointer("/relationship/iri"))
                .and_then(Value::as_str)
        })
        .map(str::to_string)
        .collect()
}

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
            CREATE (a:SemanticActivation:TTL {resultRefs: $resultRefs})
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

fn fact_position(value: &Value, relationship: &str) -> Option<usize> {
    value
        .get("facts")
        .and_then(Value::as_array)?
        .iter()
        .position(|fact| {
            fact.pointer("/target/iri")
                .or_else(|| fact.pointer("/relationship/iri"))
                .and_then(Value::as_str)
                == Some(relationship)
        })
}

async fn search(graph: &neo4rs::Graph, scope: Vec<String>, text: &str) -> Result<Value> {
    mindreader::search::memory_search(
        graph,
        SearchArgs {
            layers: scope,
            text: Some(text.into()),
            labels: None,
            limit: Some(100),
        },
    )
    .await
}

async fn merge_node_once(graph: neo4rs::Graph, spec: NodeSpec) -> Result<MergedNode> {
    let mut txn = graph.start_txn().await?;
    let node = merge_node_in_txn(&mut txn, &spec, "Element", &[]).await?;
    txn.commit().await.context("commit smoke node upsert")?;
    Ok(node)
}

async fn relation_state(
    graph: &neo4rs::Graph,
    iri: &str,
) -> Result<Option<(Vec<String>, bool, i64)>> {
    let row = fetch_one(
        graph,
        query(
            "MATCH ()-[r]->() WHERE r.iri = $iri \
             RETURN coalesce(r.layers, []) AS layers, r.validTo IS NULL AS current, \
                    coalesce(r.weightText, toString(coalesce(r.weight, 0))) AS weight",
        )
        .param("iri", iri.to_string()),
    )
    .await?;
    row.map(|row| {
        Ok((
            row.get::<Vec<String>>("layers").unwrap_or_default(),
            row.get::<bool>("current").unwrap_or(false),
            row.get::<String>("weight")
                .ok()
                .and_then(|value| value.parse().ok())
                .unwrap_or(0),
        ))
    })
    .transpose()
}

async fn node_layers(graph: &Graph, iri: &str) -> Result<Option<Vec<String>>> {
    fetch_one(
        graph,
        query("MATCH (n:Entity {iri: $iri}) RETURN coalesce(n.layers, []) AS layers")
            .param("iri", iri.to_string()),
    )
    .await?
    .map(|row| row.get("layers").map_err(Into::into))
    .transpose()
}

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
    let tool_names = Mindreader::registered_tool_names();
    report.check(
        "MCP registers the eight-tool contract",
        tool_names.len() == 8
            && tool_names.contains(&"memory_recall".into())
            && tool_names.contains(&"memory_recall_semantic".into())
            && tool_names.contains(&"memory_write".into())
            && tool_names.contains(&"memory_revise".into())
            && tool_names.contains(&"memory_unify".into()),
        format!("tools={tool_names:?}"),
    );

    let graph = graph::connect(&cfg).await?;
    let embedding_space = EmbeddingSpace {
        provider: "smoke".into(),
        model: "deterministic".into(),
        dimensions: 3,
    };
    graph::bootstrap(
        &graph,
        Some(&embedding_space),
        mindreader::graph::SpaceReplace::Refuse,
    )
    .await?;
    let stats = tools::memory_stats(&graph, StatsArgs { layers: Vec::new() }).await?;
    report.check(
        "bootstrap is ready in global-only scope",
        stats.pointer("/model/ready").and_then(Value::as_bool) == Some(true)
            && stats
                .get("layers")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty),
        &stats,
    );

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
            labels: vec!["Knowledge".into()],
        },
    )
    .await?;
    let concurrent_iri = format!("mindreader:element/apoc-concurrent-{tag}");
    let concurrent_spec = NodeSpec {
        iri: Some(concurrent_iri),
        name: Some(format!("apoc-concurrent-{tag}")),
        labels: vec!["Pattern".into()],
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
                .is_some_and(|labels| labels.iter().any(|label| label == "Knowledge"))
            && relabeled.json.get("name") == initial.json.get("name")
            && left.created as usize + right.created as usize == 1,
        format!("initial={initial:?} relabeled={relabeled:?} concurrent=({left:?}, {right:?})"),
    );

    let schema = tools::memory_schema(
        &graph,
        SchemaArgs {
            kind: "property".into(),
            name: Some(property.clone()),
            domain: Some("Element".into()),
            range: Some("Element".into()),
            ..SchemaArgs::default()
        },
    )
    .await?;
    report.check(
        "schema writes remain global and provenance-backed",
        schema
            .pointer("/node/scope")
            .and_then(Value::as_array)
            .is_some_and(Vec::is_empty)
            && schema
                .pointer("/episode/iri")
                .and_then(Value::as_str)
                .is_some(),
        &schema,
    );

    let visibility_token = format!("visibility-{tag}");
    let global = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("{visibility_token}-global-subject")),
            property.clone(),
            object(format!("{visibility_token}-global-object")),
            Vec::new(),
        ),
    )
    .await?;
    let in_a = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("{visibility_token}-a-subject")),
            property.clone(),
            object(format!("{visibility_token}-a-object")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let in_b = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("{visibility_token}-b-subject")),
            property.clone(),
            object(format!("{visibility_token}-b-object")),
            vec![layer_b.clone()],
        ),
    )
    .await?;
    let global_rel = relationship_iri(&global)?;
    let a_rel = relationship_iri(&in_a)?;
    let b_rel = relationship_iri(&in_b)?;
    let only_global = fact_relationships(&search(&graph, Vec::new(), &visibility_token).await?);
    let seen_a =
        fact_relationships(&search(&graph, vec![layer_a.clone()], &visibility_token).await?);
    let seen_b =
        fact_relationships(&search(&graph, vec![layer_b.clone()], &visibility_token).await?);
    let seen_ab = fact_relationships(
        &search(
            &graph,
            vec![layer_a.clone(), layer_b.clone()],
            &visibility_token,
        )
        .await?,
    );
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

    let batch_token = format!("batch-{tag}");
    let batch = tools::memory_assert(
        &graph,
        AssertArgs {
            facts: vec![
                AssertFact {
                    s: entity(format!("{batch_token}-one")),
                    p: property.clone(),
                    o: object(format!("{batch_token}-a")),
                    spike: None,
                    contradicts: false,
                },
                AssertFact {
                    s: entity(format!("{batch_token}-two")),
                    p: property.clone(),
                    o: object(format!("{batch_token}-b")),
                    spike: None,
                    contradicts: false,
                },
                AssertFact {
                    s: entity(format!("{batch_token}-three")),
                    p: property.clone(),
                    o: object(format!("{batch_token}-c")),
                    spike: None,
                    contradicts: false,
                },
            ],
            scope: vec![layer_a.clone()],
        },
    )
    .await?;
    let batch_episode = batch.pointer("/episode/iri").and_then(Value::as_str);
    report.check(
        "one memory_assert facts[] call records one Episode for three triples",
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
            Ok::<_, mindreader::error::Error>((subject.to_string(), object.to_string()))
        })
        .collect::<Result<Vec<_>>>()?;
    let batch_noop = tools::memory_assert(
        &graph,
        AssertArgs {
            facts: batch_iris
                .into_iter()
                .map(|(subject, object)| AssertFact {
                    s: entity_iri(subject),
                    p: property.clone(),
                    o: object_iri(object),
                    spike: None,
                    contradicts: false,
                })
                .collect(),
            scope: vec![layer_a.clone()],
        },
    )
    .await?;
    report.check(
        "all-noop memory_assert facts[] rolls back without an Episode",
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
    let merged_a = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("{merge_token}-subject")),
            property.clone(),
            object(format!("{merge_token}-old")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let merged_b = tools::memory_assert(
        &graph,
        assert_args(
            entity_iri(subject_iri(&merged_a)?),
            property.clone(),
            object_iri(object_result_iri(&merged_a)?),
            vec![layer_b.clone()],
        ),
    )
    .await?;
    let merged_rel = relationship_iri(&merged_a)?;
    let merged_state = relation_state(&graph, &merged_rel).await?;
    report.check(
        "exact semantic fact unions memberships under one stable relationship IRI",
        relationship_iri(&merged_b)? == merged_rel
            && merged_state == Some((vec![layer_a.clone(), layer_b.clone()], true, 0)),
        format!("first={merged_a} second={merged_b} state={merged_state:?}"),
    );

    let global_wins_1 = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("global-wins-{tag}-subject")),
            property.clone(),
            object(format!("global-wins-{tag}-object")),
            Vec::new(),
        ),
    )
    .await?;
    let global_wins_2 = tools::memory_assert(
        &graph,
        assert_args(
            entity_iri(subject_iri(&global_wins_1)?),
            property.clone(),
            object_iri(object_result_iri(&global_wins_1)?),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let global_wins_rel = relationship_iri(&global_wins_1)?;
    report.check(
        "global membership wins over later named assertions",
        relationship_iri(&global_wins_2)? == global_wins_rel
            && global_wins_2.get("noop").and_then(Value::as_bool) == Some(true)
            && relation_state(&graph, &global_wins_rel).await? == Some((Vec::new(), true, 0)),
        &global_wins_2,
    );

    let contradiction_old_left = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("contradiction-left-{tag}")),
            format!("contradictionLeft{tag}"),
            object(format!("contradiction-old-{tag}")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let contradiction_old_iri = object_result_iri(&contradiction_old_left)?;
    let contradiction_old_right = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("contradiction-right-{tag}")),
            format!("contradictionRight{tag}"),
            object_iri(contradiction_old_iri.clone()),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let contradiction_new_name = format!("contradiction-new-{tag}");
    let (contradiction_left, contradiction_right) = tokio::try_join!(
        tools::memory_assert(
            &graph,
            AssertArgs {
                facts: vec![AssertFact {
                    s: entity_iri(subject_iri(&contradiction_old_left)?),
                    p: format!("contradictionLeft{tag}"),
                    o: object(contradiction_new_name.clone()),
                    spike: None,
                    contradicts: true,
                }],
                scope: vec![layer_a.clone()],
            },
        ),
        tools::memory_assert(
            &graph,
            AssertArgs {
                facts: vec![AssertFact {
                    s: entity_iri(subject_iri(&contradiction_old_right)?),
                    p: format!("contradictionRight{tag}"),
                    o: object(contradiction_new_name),
                    spike: None,
                    contradicts: true,
                }],
                scope: vec![layer_a.clone()],
            },
        ),
    )?;
    let contradiction_new_iri = object_result_iri(&contradiction_left)?;
    let contradiction_count =
        tools::count_current_contradicts(&graph, &contradiction_new_iri, &contradiction_old_iri)
            .await?;
    report.check(
        "concurrent contradiction writes preserve one exact current relationship",
        object_result_iri(&contradiction_right)? == contradiction_new_iri
            && contradiction_count == 1,
        format!(
            "left={contradiction_left} right={contradiction_right} count={contradiction_count}"
        ),
    );

    let merged_subject = subject_iri(&merged_a)?;
    let merged_old = object_result_iri(&merged_a)?;
    let replacement = tools::memory_replace(
        &graph,
        ReplaceArgs {
            s: entity_iri(merged_subject.clone()),
            p: property.clone(),
            old: object_iri(merged_old.clone()),
            new: object(format!("{merge_token}-new")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
            reason: Some("smoke scoped replacement".into()),
        },
    )
    .await?;
    let replacement_rel = relationship_iri(&replacement)?;
    let replace_a = fact_relationships(&search(&graph, vec![layer_a.clone()], &merge_token).await?);
    let replace_b = fact_relationships(&search(&graph, vec![layer_b.clone()], &merge_token).await?);
    report.check(
        "replace moves only selected memberships and preserves unrelated scope",
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

    let retracted = tools::memory_retract(
        &graph,
        RetractArgs {
            target: RetractTargetArgs {
                kind: "fact".into(),
                s: entity_iri(merged_subject),
                p: Some(property.clone()),
                o: Some(object_iri(merged_old)),
            },
            layers: vec![layer_b.clone()],
            reason: Some("smoke final scoped retract".into()),
        },
    )
    .await?;
    report.check(
        "retract retires a fact when its last named membership is removed",
        retracted.get("retracted").and_then(Value::as_u64) == Some(1)
            && relation_state(&graph, &merged_rel).await?
                == Some((vec![layer_b.clone()], false, 0)),
        format!(
            "response={retracted} state={:?}",
            relation_state(&graph, &merged_rel).await?
        ),
    );

    let broad_subject_name = format!("broad-retract-{tag}");
    let broad_a = tools::memory_assert(
        &graph,
        assert_args(
            entity(broad_subject_name.clone()),
            property.clone(),
            object(format!("broad-a-{tag}")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let broad_ab = tools::memory_assert(
        &graph,
        assert_args(
            entity(broad_subject_name.clone()),
            property.clone(),
            object(format!("broad-ab-{tag}")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    tools::memory_assert(
        &graph,
        assert_args(
            entity(broad_subject_name.clone()),
            property.clone(),
            object_iri(object_result_iri(&broad_ab)?),
            vec![layer_b.clone()],
        ),
    )
    .await?;
    let broad_b = tools::memory_assert(
        &graph,
        assert_args(
            entity(broad_subject_name),
            property.clone(),
            object(format!("broad-b-{tag}")),
            vec![layer_b.clone()],
        ),
    )
    .await?;
    let broad_a_rel = relationship_iri(&broad_a)?;
    let broad_ab_rel = relationship_iri(&broad_ab)?;
    let broad_b_rel = relationship_iri(&broad_b)?;
    let broad_retract = tools::memory_retract(
        &graph,
        RetractArgs {
            target: RetractTargetArgs {
                kind: "subject".into(),
                s: entity_iri(subject_iri(&broad_a)?),
                p: None,
                o: None,
            },
            layers: vec![layer_a.clone()],
            reason: Some("smoke broad retract batch".into()),
        },
    )
    .await?;
    report.check(
        "broad retract batches retirement and surviving membership updates",
        broad_retract.get("retracted").and_then(Value::as_u64) == Some(2)
            && relation_state(&graph, &broad_a_rel).await?
                == Some((vec![layer_a.clone()], false, 0))
            && relation_state(&graph, &broad_ab_rel).await?
                == Some((vec![layer_b.clone()], true, 0))
            && relation_state(&graph, &broad_b_rel).await?
                == Some((vec![layer_b.clone()], true, 0)),
        format!(
            "response={broad_retract} a={:?} ab={:?} b={:?}",
            relation_state(&graph, &broad_a_rel).await?,
            relation_state(&graph, &broad_ab_rel).await?,
            relation_state(&graph, &broad_b_rel).await?
        ),
    );

    let rank_token = format!("rank-{tag}");
    let low = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("{rank_token}-low-subject")),
            property.clone(),
            object(format!("{rank_token}-low-object")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let high = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("{rank_token}-high-subject")),
            property.clone(),
            object(format!("{rank_token}-high-object")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let low_rel = relationship_iri(&low)?;
    let high_rel = relationship_iri(&high)?;
    let high_subject = subject_iri(&high)?;
    let strengthened_node = tools::memory_feedback(
        &graph,
        FeedbackArgs {
            layers: vec![layer_a.clone()],
            target: TargetArgs {
                kind: "node".into(),
                iri: high_subject,
            },
            mode: "strengthen".into(),
        },
    )
    .await?;
    let strengthened_rel = tools::memory_feedback(
        &graph,
        FeedbackArgs {
            layers: vec![layer_a.clone()],
            target: TargetArgs {
                kind: "fact".into(),
                iri: high_rel.clone(),
            },
            mode: "strengthen".into(),
        },
    )
    .await?;
    let weakened_rel = tools::memory_feedback(
        &graph,
        FeedbackArgs {
            layers: vec![layer_a.clone()],
            target: TargetArgs {
                kind: "fact".into(),
                iri: low_rel.clone(),
            },
            mode: "weaken".into(),
        },
    )
    .await?;
    let ranked = search(&graph, vec![layer_a.clone()], &rank_token).await?;
    report.check(
        "node and relationship feedback are signed and affect ranking within a tier",
        strengthened_node.get("weight").and_then(Value::as_i64) == Some(1)
            && strengthened_rel.get("weight").and_then(Value::as_i64) == Some(1)
            && weakened_rel.get("weight").and_then(Value::as_i64) == Some(-1)
            && fact_position(&ranked, &high_rel) == Some(0)
            && fact_position(&ranked, &high_rel) < fact_position(&ranked, &low_rel),
        format!(
            "node={strengthened_node} high={strengthened_rel} low={weakened_rel} ranked={ranked}"
        ),
    );

    let mut concurrent_feedback = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let graph = graph.clone();
        let layer = layer_a.clone();
        let relationship = high_rel.clone();
        concurrent_feedback.spawn(async move {
            tools::memory_feedback(
                &graph,
                FeedbackArgs {
                    layers: vec![layer],
                    target: TargetArgs {
                        kind: "fact".into(),
                        iri: relationship,
                    },
                    mode: "strengthen".into(),
                },
            )
            .await
        });
    }
    let mut concurrent_successes = 0;
    while let Some(result) = concurrent_feedback.join_next().await {
        result.context("join concurrent feedback task")??;
        concurrent_successes += 1;
    }
    report.check(
        "concurrent feedback increments do not lose updates",
        concurrent_successes == 8
            && relation_state(&graph, &high_rel).await? == Some((vec![layer_a.clone()], true, 9)),
        format!(
            "successes={concurrent_successes} state={:?}",
            relation_state(&graph, &high_rel).await?
        ),
    );

    let judge_episodes_before = episode_count(&graph, "memory_judge").await?;
    let judged = tools::memory_judge(
        &graph,
        JudgeArgs {
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
        },
    )
    .await?;
    let judge_episodes_after = episode_count(&graph, "memory_judge").await?;
    let high_after_batch = relation_state(&graph, &high_rel).await?;
    let rollback = tools::memory_judge(
        &graph,
        JudgeArgs {
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
        },
    )
    .await;
    report.check(
        "memory_judge batches atomically under one Episode and rolls back mixed failures",
        judged.pointer("/episode/tool").and_then(Value::as_str) == Some("memory_judge")
            && judged
                .get("items")
                .and_then(Value::as_array)
                .is_some_and(|items| items.len() == 2)
            && judge_episodes_after == judge_episodes_before + 1
            && rollback.is_err()
            && relation_state(&graph, &high_rel).await? == high_after_batch
            && episode_count(&graph, "memory_judge").await? == judge_episodes_after,
        format!(
            "judged={judged} rollback={rollback:?} high={high_after_batch:?} episodes={judge_episodes_before}->{judge_episodes_after}"
        ),
    );

    let closure_token = format!("closure-{tag}");
    let closure = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("{closure_token}-subject")),
            property.clone(),
            object(format!("{closure_token}-object")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let closure_rel = relationship_iri(&closure)?;
    let closure_subject = subject_iri(&closure)?;
    let closure_object = object_result_iri(&closure)?;
    let premature_relation_add = tools::memory_layers(
        &graph,
        LayersArgs {
            layers: vec![layer_a.clone()],
            target: TargetArgs {
                kind: "fact".into(),
                iri: closure_rel.clone(),
            },
            add: vec![layer_c.clone()],
            remove: Vec::new(),
        },
    )
    .await;
    let subject_add = tools::memory_layers(
        &graph,
        LayersArgs {
            layers: vec![layer_a.clone()],
            target: TargetArgs {
                kind: "node".into(),
                iri: closure_subject.clone(),
            },
            add: vec![layer_c.clone()],
            remove: Vec::new(),
        },
    )
    .await?;
    let object_add = tools::memory_layers(
        &graph,
        LayersArgs {
            layers: vec![layer_a.clone()],
            target: TargetArgs {
                kind: "node".into(),
                iri: closure_object.clone(),
            },
            add: vec![layer_c.clone()],
            remove: Vec::new(),
        },
    )
    .await?;
    let relation_add = tools::memory_layers(
        &graph,
        LayersArgs {
            layers: vec![layer_a.clone()],
            target: TargetArgs {
                kind: "fact".into(),
                iri: closure_rel.clone(),
            },
            add: vec![layer_c.clone()],
            remove: Vec::new(),
        },
    )
    .await?;
    let invalid_endpoint_remove = tools::memory_layers(
        &graph,
        LayersArgs {
            layers: vec![layer_a.clone(), layer_c.clone()],
            target: TargetArgs {
                kind: "node".into(),
                iri: closure_subject.clone(),
            },
            add: Vec::new(),
            remove: vec![layer_a.clone()],
        },
    )
    .await;
    report.check(
        "memory_layers succeeds only when endpoint/relationship closure is preserved",
        premature_relation_add.is_err()
            && subject_add.get("noop").and_then(Value::as_bool) == Some(false)
            && object_add.get("noop").and_then(Value::as_bool) == Some(false)
            && relation_add.get("layers").and_then(Value::as_array).is_some_and(|values| values.len() == 2)
            && relation_state(&graph, &closure_rel).await?
                == Some((vec![layer_a.clone(), layer_c.clone()], true, 0))
            && invalid_endpoint_remove.is_err(),
        format!("premature={premature_relation_add:?} subject={subject_add} object={object_add} relation={relation_add} invalidRemove={invalid_endpoint_remove:?}"),
    );

    let place = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("place-batch-{tag}-subject")),
            property.clone(),
            object(format!("place-batch-{tag}-object")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let place_subject = subject_iri(&place)?;
    let place_object = object_result_iri(&place)?;
    let place_fact = relationship_iri(&place)?;
    let place_episodes_before = episode_count(&graph, "memory_place").await?;
    let placed = tools::memory_place(
        &graph,
        PlaceArgs {
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
        },
    )
    .await?;
    let place_episodes_after = episode_count(&graph, "memory_place").await?;
    let place_rollback = tools::memory_place(
        &graph,
        PlaceArgs {
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
        },
    )
    .await;
    report.check(
        "memory_place validates combined final closure, records one Episode, and rolls back mixed failures",
        placed.pointer("/episode/tool").and_then(Value::as_str) == Some("memory_place")
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
            && episode_count(&graph, "memory_place").await? == place_episodes_after,
        format!(
            "placed={placed} rollback={place_rollback:?} episodes={place_episodes_before}->{place_episodes_after}"
        ),
    );

    let concurrent_place = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("place-concurrent-{tag}-subject")),
            property.clone(),
            object(format!("place-concurrent-{tag}-object")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let concurrent_subject = subject_iri(&concurrent_place)?;
    let concurrent_object = object_result_iri(&concurrent_place)?;
    let concurrent_fact = relationship_iri(&concurrent_place)?;
    tools::memory_place(
        &graph,
        PlaceArgs {
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
        },
    )
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
    let (fact_edit_result, endpoint_edit_result) = tokio::join!(
        tools::memory_place(&graph, fact_edit),
        tools::memory_place(&graph, endpoint_edit),
    );
    let concurrent_successes =
        usize::from(fact_edit_result.is_ok()) + usize::from(endpoint_edit_result.is_ok());
    let concurrent_fact_layers = relation_state(&graph, &concurrent_fact)
        .await?
        .map(|state| state.0)
        .unwrap_or_default();
    let concurrent_subject_layers = node_layers(&graph, &concurrent_subject)
        .await?
        .unwrap_or_default();
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

    let schema_place = tools::memory_place(
        &graph,
        PlaceArgs {
            scope: Vec::new(),
            edits: vec![PlaceEdit {
                target: TargetArgs {
                    kind: "node".into(),
                    iri: format!("mindreader:property/{property}"),
                },
                add: vec![layer_a.clone()],
                remove: Vec::new(),
            }],
        },
    )
    .await;
    report.check(
        "memory_place keeps Class and Property schema records global",
        schema_place.is_err(),
        format!("schemaPlace={schema_place:?}"),
    );

    let exact = tools::memory_get(
        &graph,
        GetArgs {
            iri: subject_iri(&in_b)?,
            layers: vec![layer_b],
            hops: Some(1),
        },
    )
    .await?;
    report.check(
        "stable relationship IRI round-trips through scoped retrieval",
        exact
            .get("neighbors")
            .and_then(Value::as_array)
            .is_some_and(|neighbors| {
                neighbors.iter().any(|neighbor| {
                    neighbor.pointer("/edge/iri").and_then(Value::as_str) == Some(&b_rel)
                })
            }),
        &exact,
    );

    let service = MemoryService::new(graph.clone(), &cfg)?;
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
            history: None,
            detail: None,
            limit: Some(1),
        })
        .await?;
    let lookup_order = recalled
        .get("lookups")
        .and_then(Value::as_array)
        .map(|lookups| {
            lookups
                .iter()
                .filter_map(|lookup| lookup.get("iri").and_then(Value::as_str))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
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
            history: None,
            detail: None,
            limit: Some(1),
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
            history: None,
            detail: None,
            limit: Some(1),
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
            history: None,
            detail: None,
            limit: Some(100),
        })
        .await?;
    report.check(
        "memory_recall preserves IRI order and misses, enforces one fact budget, and returns filtered witness paths",
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
        "memory_recall catalog emits pasteable node handles in the normalized schema",
        catalog.get("mode").and_then(Value::as_str) == Some("catalog")
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
                        })
                }),
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
            history: None,
            detail: Some("concise".into()),
            limit: Some(20),
        })
        .await?;
    let history_iri = witnessed_around
        .pointer("/facts/0/target/iri")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
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
            history: Some(history_iri.clone()),
            detail: None,
            limit: Some(20),
        })
        .await?;
    report.check(
        "memory_recall hops 0 still returns incident fact handles, concise detail, and history walks a fact",
        hops0.get("mode").and_then(Value::as_str) == Some("iris")
            && hops0
                .get("facts")
                .and_then(Value::as_array)
                .is_some_and(Vec::is_empty)
            && hops0
                .pointer("/lookups/0/facts")
                .and_then(Value::as_array)
                .is_some_and(|facts| !facts.is_empty())
            && hops0.get("detail").and_then(Value::as_str) == Some("concise")
            && hops0
                .pointer("/handles/facts")
                .and_then(Value::as_array)
                .is_some_and(|facts| !facts.is_empty())
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
    let spiked = tools::memory_assert(
        &graph,
        AssertArgs {
            facts: vec![AssertFact {
                s: entity(spike_name.clone()),
                p: property.clone(),
                o: object(format!("spike-obj-{tag}")),
                spike: Some("Knowledge".into()),
                contradicts: false,
            }],
            scope: vec![layer_a.clone()],
        },
    )
    .await?;
    let spiked_iri = spiked
        .pointer("/facts/0/s/iri")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    report.check(
        "name-only Knowledge spike mints an Element IRI and keeps the extra label",
        spiked_iri == format!("mindreader:element/spike-id-{tag}")
            && spiked
                .pointer("/facts/0/s/labels")
                .and_then(Value::as_array)
                .is_some_and(|labels| labels.iter().any(|label| label == "Knowledge")),
        format!("iri={spiked_iri} spiked={spiked}"),
    );

    let fanout_subject = format!("fanout-{tag}");
    let mut fanout_facts = Vec::new();
    for index in 0..20 {
        fanout_facts.push(AssertFact {
            s: entity(fanout_subject.clone()),
            p: property.clone(),
            o: object(format!("fanout-{tag}-{index}")),
            spike: None,
            contradicts: false,
        });
    }
    tools::memory_assert(
        &graph,
        AssertArgs {
            facts: fanout_facts,
            scope: vec![layer_a.clone()],
        },
    )
    .await?;
    let mut extra_fanout = Vec::new();
    for index in 20..25 {
        extra_fanout.push(AssertFact {
            s: entity(fanout_subject.clone()),
            p: property.clone(),
            o: object(format!("fanout-{tag}-{index}")),
            spike: None,
            contradicts: false,
        });
    }
    tools::memory_assert(
        &graph,
        AssertArgs {
            facts: extra_fanout,
            scope: vec![layer_a.clone()],
        },
    )
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
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
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
            history: None,
            detail: Some("detailed".into()),
            limit: Some(20),
        })
        .await?;
    let lookup_iris = |value: &Value| -> Vec<String> {
        value
            .pointer("/lookups/0/facts")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|fact| {
                fact.pointer("/target/iri")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .collect()
    };
    let hops0_iris = lookup_iris(&hops0_budget);
    let hops1_iris = lookup_iris(&hops1_budget);
    report.check(
        "iris hops 0 and 1 share a per-root fact budget and truncate at 20",
        hops0_iris == hops1_iris
            && hops0_iris.len() == 20
            && hops0_budget.get("truncated").and_then(Value::as_bool) == Some(true)
            && hops1_budget.get("truncated").and_then(Value::as_bool) == Some(true),
        format!("hops0={hops0_budget} hops1={hops1_budget}"),
    );

    let camel_subject = format!("camel-{tag}");
    tools::memory_assert(
        &graph,
        assert_args(
            entity(camel_subject.clone()),
            "graphModel",
            object(format!("camel-object-{tag}")),
            vec![layer_a.clone()],
        ),
    )
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
            history: None,
            detail: Some("concise".into()),
            limit: Some(20),
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

    let merge_short = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("merge-{tag}")),
            "mindreader:property/merge-smoke",
            object(format!("merge-object-{tag}")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    let merge_long = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("merge-{tag}s")),
            "mindreader:property/merge-smoke",
            object(format!("merge-object-{tag}")),
            vec![layer_a.clone()],
        ),
    )
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
    let (survivor, merge_feedback) = tokio::try_join!(
        memory_merge(
            &graph,
            MergeArgs::from_iris(long_iri.clone(), short_iri.clone()),
        ),
        tools::memory_feedback(
            &graph,
            FeedbackArgs {
                layers: vec![layer_a.clone()],
                target: TargetArgs {
                    kind: "fact".into(),
                    iri: merge_survivor_relationship.clone(),
                },
                mode: "strengthen".into(),
            },
        )
    )?;
    let removed = tools::memory_get(
        &graph,
        GetArgs {
            iri: long_iri,
            layers: vec![layer_a.clone()],
            hops: Some(0),
        },
    )
    .await?;
    report.check(
        "merge suggestions prefer the shorter name and memory_merge keeps only the target",
        suggested
            && survivor.pointer("/node/iri").and_then(Value::as_str) == Some(short_iri.as_str())
            && removed.get("found").and_then(Value::as_bool) == Some(false)
            && relation_state(&graph, &merge_survivor_relationship)
                .await?
                .is_some_and(|(_, current, weight)| current && weight == 1),
        format!(
            "suggestions={} survivor={survivor} feedback={merge_feedback} removed={removed}",
            merge_long["review"]["unify"],
        ),
    );

    let same_txn_short_name = format!("007-{tag}");
    let same_txn_long_name = format!("007s-{tag}");
    let same_txn_merge = tools::memory_assert(
        &graph,
        assert_args(
            entity(same_txn_long_name.clone()),
            "mindreader:property/same-transaction-merge-smoke",
            object(same_txn_short_name.clone()),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    report.check(
        "merge suggestions include similar entities created in the same transaction",
        same_txn_merge
            .pointer("/review/unify")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.pointer("/source/name").and_then(Value::as_str)
                        == Some(same_txn_long_name.as_str())
                        && item.pointer("/target/name").and_then(Value::as_str)
                            == Some(same_txn_short_name.as_str())
                })
            }),
        &same_txn_merge["review"]["unify"],
    );

    let target_property_name = format!("mergeProperty{tag}");
    let source_property_name = format!("mergeProperty{tag}s");
    let target_property = tools::memory_schema(
        &graph,
        SchemaArgs {
            kind: "property".into(),
            name: Some(target_property_name.clone()),
            ..SchemaArgs::default()
        },
    )
    .await?;
    let source_property = tools::memory_schema(
        &graph,
        SchemaArgs {
            kind: "property".into(),
            name: Some(source_property_name.clone()),
            ..SchemaArgs::default()
        },
    )
    .await?;
    let target_property_iri = target_property
        .pointer("/node/iri")
        .and_then(Value::as_str)
        .ok_or_else(|| operation_error!("target property has no IRI: {target_property}"))?
        .to_string();
    let source_property_iri = source_property
        .pointer("/node/iri")
        .and_then(Value::as_str)
        .ok_or_else(|| operation_error!("source property has no IRI: {source_property}"))?
        .to_string();
    let property_fact = tools::memory_assert(
        &graph,
        assert_args(
            entity(format!("property-merge-subject-{tag}")),
            target_property_iri.clone(),
            object(format!("property-merge-object-{tag}")),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    tools::memory_assert(
        &graph,
        assert_args(
            entity_iri(subject_iri(&property_fact)?),
            source_property_iri.clone(),
            object_iri(object_result_iri(&property_fact)?),
            vec![layer_a.clone()],
        ),
    )
    .await?;
    memory_merge(
        &graph,
        MergeArgs::from_iris(source_property_iri.clone(), target_property_iri.clone()),
    )
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
    let wrong_kind = memory_merge(
        &graph,
        MergeArgs::from_iris(short_iri.clone(), target_property_iri.clone()),
    )
    .await;
    let incompatible_property = memory_merge(
        &graph,
        MergeArgs::from_iris(target_property_iri.clone(), "mindreader:property/ABOUT"),
    )
    .await;
    let system_property = memory_merge(
        &graph,
        MergeArgs::from_iris(
            target_property_iri.clone(),
            "mindreader:property/CONTRADICTS",
        ),
    )
    .await;
    report.check(
        "property merges preserve predicate representation and reject incompatible or system-owned kinds",
        property_state.get::<i64>("count").unwrap_or(0) == 1
            && property_state
                .get::<Vec<String>>("properties")
                .unwrap_or_default()
                == vec![target_property_iri]
            && property_state
                .get::<Vec<String>>("factTexts")
                .unwrap_or_default()
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
    let semantic_runtime = SemanticRuntime::new(
        Arc::new(SmokeEmbedding),
        SemanticConfig {
            neighbor_limit: 100,
            ..SemanticConfig::default()
        },
    );
    let semantic_args = SemanticSearchArgs {
        scope: vec![layer_a],
        text: semantic_text,
        labels: None,
        detail: None,
        limit: Some(20),
    };
    let semantic_first = memory_semantic_search(
        &graph,
        Some(&semantic_runtime),
        cfg.secrets_path(),
        semantic_args.clone(),
    )
    .await?;
    let activation_after_first = fetch_one(
        &graph,
        query(
            "MATCH (a:SemanticActivation:TTL) \
             RETURN count(a) AS count, max(a.ttl) > timestamp() AS live",
        ),
    )
    .await?
    .ok_or_else(|| operation_error!("semantic activation aggregate returned no row"))?;
    let semantic_second = memory_semantic_search(
        &graph,
        Some(&semantic_runtime),
        cfg.secrets_path(),
        semantic_args,
    )
    .await?;
    let activation = fetch_one(
        &graph,
        query(
            "MATCH (a:SemanticActivation:TTL) \
             RETURN count(a) AS count, max(a.ttl) > timestamp() AS live",
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
            && contributing_ttl_after.is_some_and(|ttl| ttl > contributing_ttl_before)
            && unresolved_ttl_after == Some(unresolved_ttl_before),
        format!(
            "first={semantic_first} second={semantic_second} activationAfterFirst={activation_after_first:?} activationAfterSecond={activation:?} contributingTtl={contributing_ttl_before}->{contributing_ttl_after:?} unresolvedTtl={unresolved_ttl_before}->{unresolved_ttl_after:?}"
        ),
    );

    Ok(report.failed)
}
