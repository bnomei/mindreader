//! Live Neo4j integration suite for the twelve memory tools and graph contracts.
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
    self, acquire_fact_locks_in_txn, fetch_one, merge_node_in_txn, MergedNode, NodeSpec,
};
use mindreader::merge::{memory_merge, MergeArgs};
use mindreader::operation_error;
use mindreader::search::SearchArgs;
use mindreader::semantic::{memory_semantic_search, SemanticRuntime, SemanticSearchArgs};
use mindreader::tools::{
    self, AssertArgs, FeedbackArgs, GetArgs, LayersArgs, ReplaceArgs, RetractArgs,
    RetractTargetArgs, SchemaArgs, StatsArgs, TargetArgs,
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
        kind: "entity".into(),
        iri: None,
        name: Some(name.into()),
        labels: vec!["Element".into()],
    }
}

fn entity_iri(iri: impl Into<String>) -> EntityInput {
    EntityInput {
        kind: "entity".into(),
        iri: Some(iri.into()),
        name: None,
        labels: Vec::new(),
    }
}

fn object(name: impl Into<String>) -> ObjectInput {
    ObjectInput {
        kind: "entity".into(),
        iri: None,
        name: Some(name.into()),
        labels: vec!["Element".into()],
        value: None,
        datatype: None,
    }
}

fn object_iri(iri: impl Into<String>) -> ObjectInput {
    ObjectInput {
        kind: "entity".into(),
        iri: Some(iri.into()),
        name: None,
        labels: Vec::new(),
        value: None,
        datatype: None,
    }
}

fn relationship_iri(value: &Value) -> Result<String> {
    value
        .pointer("/relationship/iri")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| operation_error!("response has no relationship IRI: {value}"))
}

fn subject_iri(value: &Value) -> Result<String> {
    value
        .pointer("/s/iri")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| operation_error!("response has no subject IRI: {value}"))
}

fn object_result_iri(value: &Value) -> Result<String> {
    value
        .pointer("/o/iri")
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
        .filter_map(|fact| fact.pointer("/relationship/iri").and_then(Value::as_str))
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
            fact.pointer("/relationship/iri").and_then(Value::as_str) == Some(relationship)
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
        "MCP registers the twelve-tool contract",
        tool_names.len() == 12
            && tool_names.contains(&"memory_feedback".into())
            && tool_names.contains(&"memory_layers".into())
            && tool_names.contains(&"memory_merge".into())
            && tool_names.contains(&"memory_semantic_search".into()),
        format!("tools={tool_names:?}"),
    );

    let graph = graph::connect(&cfg).await?;
    let embedding_space = EmbeddingSpace {
        provider: "smoke".into(),
        model: "deterministic".into(),
        dimensions: 3,
    };
    graph::bootstrap(&graph, Some(&embedding_space)).await?;
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
            iri: None,
            sub_class_of: None,
            sub_property_of: None,
            domain: Some("Element".into()),
            range: Some("Element".into()),
        },
    )
    .await?;
    report.check(
        "schema writes remain global and provenance-backed",
        schema
            .pointer("/node/layers")
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
        AssertArgs {
            s: entity(format!("{visibility_token}-global-subject")),
            p: property.clone(),
            o: object(format!("{visibility_token}-global-object")),
            layers: Vec::new(),
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    let in_a = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(format!("{visibility_token}-a-subject")),
            p: property.clone(),
            o: object(format!("{visibility_token}-a-object")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    let in_b = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(format!("{visibility_token}-b-subject")),
            p: property.clone(),
            o: object(format!("{visibility_token}-b-object")),
            layers: vec![layer_b.clone()],
            spike: None,
            contradicts: false,
        },
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

    let merge_token = format!("merge-{tag}");
    let merged_a = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(format!("{merge_token}-subject")),
            p: property.clone(),
            o: object(format!("{merge_token}-old")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    let merged_b = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity_iri(subject_iri(&merged_a)?),
            p: property.clone(),
            o: object_iri(object_result_iri(&merged_a)?),
            layers: vec![layer_b.clone()],
            spike: None,
            contradicts: false,
        },
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
        AssertArgs {
            s: entity(format!("global-wins-{tag}-subject")),
            p: property.clone(),
            o: object(format!("global-wins-{tag}-object")),
            layers: Vec::new(),
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    let global_wins_2 = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity_iri(subject_iri(&global_wins_1)?),
            p: property.clone(),
            o: object_iri(object_result_iri(&global_wins_1)?),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
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
        AssertArgs {
            s: entity(format!("contradiction-left-{tag}")),
            p: format!("contradictionLeft{tag}"),
            o: object(format!("contradiction-old-{tag}")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    let contradiction_old_iri = object_result_iri(&contradiction_old_left)?;
    let contradiction_old_right = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(format!("contradiction-right-{tag}")),
            p: format!("contradictionRight{tag}"),
            o: object_iri(contradiction_old_iri.clone()),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    let contradiction_new_name = format!("contradiction-new-{tag}");
    let (contradiction_left, contradiction_right) = tokio::try_join!(
        tools::memory_assert(
            &graph,
            AssertArgs {
                s: entity_iri(subject_iri(&contradiction_old_left)?),
                p: format!("contradictionLeft{tag}"),
                o: object(contradiction_new_name.clone()),
                layers: vec![layer_a.clone()],
                spike: None,
                contradicts: true,
            },
        ),
        tools::memory_assert(
            &graph,
            AssertArgs {
                s: entity_iri(subject_iri(&contradiction_old_right)?),
                p: format!("contradictionRight{tag}"),
                o: object(contradiction_new_name),
                layers: vec![layer_a.clone()],
                spike: None,
                contradicts: true,
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
        AssertArgs {
            s: entity(broad_subject_name.clone()),
            p: property.clone(),
            o: object(format!("broad-a-{tag}")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    let broad_ab = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(broad_subject_name.clone()),
            p: property.clone(),
            o: object(format!("broad-ab-{tag}")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(broad_subject_name.clone()),
            p: property.clone(),
            o: object_iri(object_result_iri(&broad_ab)?),
            layers: vec![layer_b.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    let broad_b = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(broad_subject_name),
            p: property.clone(),
            o: object(format!("broad-b-{tag}")),
            layers: vec![layer_b.clone()],
            spike: None,
            contradicts: false,
        },
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
        AssertArgs {
            s: entity(format!("{rank_token}-low-subject")),
            p: property.clone(),
            o: object(format!("{rank_token}-low-object")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    let high = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(format!("{rank_token}-high-subject")),
            p: property.clone(),
            o: object(format!("{rank_token}-high-object")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
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
                kind: "relationship".into(),
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
                kind: "relationship".into(),
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
                        kind: "relationship".into(),
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

    let closure_token = format!("closure-{tag}");
    let closure = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(format!("{closure_token}-subject")),
            p: property,
            o: object(format!("{closure_token}-object")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
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
                kind: "relationship".into(),
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
                iri: closure_object,
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
                kind: "relationship".into(),
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
                iri: closure_subject,
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
                == Some((vec![layer_a.clone(), layer_c], true, 0))
            && invalid_endpoint_remove.is_err(),
        format!("premature={premature_relation_add:?} subject={subject_add} object={object_add} relation={relation_add} invalidRemove={invalid_endpoint_remove:?}"),
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

    let merge_short = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(format!("merge-{tag}")),
            p: "mindreader:property/merge-smoke".into(),
            o: object(format!("merge-object-{tag}")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    let merge_long = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(format!("merge-{tag}s")),
            p: "mindreader:property/merge-smoke".into(),
            o: object(format!("merge-object-{tag}")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    let short_iri = subject_iri(&merge_short)?;
    let long_iri = subject_iri(&merge_long)?;
    let merge_survivor_relationship = std::cmp::min(
        relationship_iri(&merge_short)?,
        relationship_iri(&merge_long)?,
    );
    let suggested = merge_long
        .get("mergeSuggestions")
        .and_then(Value::as_array)
        .is_some_and(|items| {
            items.iter().any(|item| {
                item.pointer("/merge/source").and_then(Value::as_str) == Some(long_iri.as_str())
                    && item.pointer("/merge/target").and_then(Value::as_str)
                        == Some(short_iri.as_str())
            })
        });
    let (survivor, merge_feedback) = tokio::try_join!(
        memory_merge(
            &graph,
            MergeArgs {
                source: long_iri.clone(),
                target: short_iri.clone(),
            },
        ),
        tools::memory_feedback(
            &graph,
            FeedbackArgs {
                layers: vec![layer_a.clone()],
                target: TargetArgs {
                    kind: "relationship".into(),
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
            && survivor.get("iri").and_then(Value::as_str) == Some(short_iri.as_str())
            && removed.get("found").and_then(Value::as_bool) == Some(false)
            && relation_state(&graph, &merge_survivor_relationship)
                .await?
                .is_some_and(|(_, current, weight)| current && weight == 1),
        format!(
            "suggestions={} survivor={survivor} feedback={merge_feedback} removed={removed}",
            merge_long["mergeSuggestions"],
        ),
    );

    let same_txn_short_name = format!("007-{tag}");
    let same_txn_long_name = format!("007s-{tag}");
    let same_txn_merge = tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity(same_txn_long_name.clone()),
            p: "mindreader:property/same-transaction-merge-smoke".into(),
            o: object(same_txn_short_name.clone()),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    report.check(
        "merge suggestions include similar entities created in the same transaction",
        same_txn_merge
            .get("mergeSuggestions")
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items.iter().any(|item| {
                    item.pointer("/source/name").and_then(Value::as_str)
                        == Some(same_txn_long_name.as_str())
                        && item.pointer("/target/name").and_then(Value::as_str)
                            == Some(same_txn_short_name.as_str())
                })
            }),
        &same_txn_merge["mergeSuggestions"],
    );

    let target_property_name = format!("mergeProperty{tag}");
    let source_property_name = format!("mergeProperty{tag}s");
    let target_property = tools::memory_schema(
        &graph,
        SchemaArgs {
            kind: "property".into(),
            name: Some(target_property_name.clone()),
            iri: None,
            sub_class_of: None,
            sub_property_of: None,
            domain: None,
            range: None,
        },
    )
    .await?;
    let source_property = tools::memory_schema(
        &graph,
        SchemaArgs {
            kind: "property".into(),
            name: Some(source_property_name.clone()),
            iri: None,
            sub_class_of: None,
            sub_property_of: None,
            domain: None,
            range: None,
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
        AssertArgs {
            s: entity(format!("property-merge-subject-{tag}")),
            p: target_property_iri.clone(),
            o: object(format!("property-merge-object-{tag}")),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    tools::memory_assert(
        &graph,
        AssertArgs {
            s: entity_iri(subject_iri(&property_fact)?),
            p: source_property_iri.clone(),
            o: object_iri(object_result_iri(&property_fact)?),
            layers: vec![layer_a.clone()],
            spike: None,
            contradicts: false,
        },
    )
    .await?;
    memory_merge(
        &graph,
        MergeArgs {
            source: source_property_iri.clone(),
            target: target_property_iri.clone(),
        },
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
        MergeArgs {
            source: short_iri.clone(),
            target: target_property_iri.clone(),
        },
    )
    .await;
    let incompatible_property = memory_merge(
        &graph,
        MergeArgs {
            source: target_property_iri.clone(),
            target: "mindreader:property/ABOUT".into(),
        },
    )
    .await;
    let system_property = memory_merge(
        &graph,
        MergeArgs {
            source: target_property_iri.clone(),
            target: "mindreader:property/CONTRADICTS".into(),
        },
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
        text: semantic_text,
        layers: vec![layer_a],
        labels: None,
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
