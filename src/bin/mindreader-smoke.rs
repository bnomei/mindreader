use anyhow::{anyhow, Context, Result};
use mindreader::config::Config;
use mindreader::domain::{EntityInput, ObjectInput};
use mindreader::graph::{self, fetch_one};
use mindreader::tools::{
    self, AssertArgs, FeedbackArgs, GetArgs, LayersArgs, ReplaceArgs, RetractArgs,
    RetractTargetArgs, SchemaArgs, SearchArgs, StatsArgs, TargetArgs,
};
use mindreader::Mindreader;
use neo4rs::query;
use serde_json::Value;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

struct Report {
    next: u32,
    failed: u32,
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
        .ok_or_else(|| anyhow!("response has no relationship IRI: {value}"))
}

fn subject_iri(value: &Value) -> Result<String> {
    value
        .pointer("/s/iri")
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("response has no subject IRI: {value}"))
}

fn object_result_iri(value: &Value) -> Result<String> {
    value
        .pointer("/o/iri")
        .or_else(|| value.pointer("/new/iri"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| anyhow!("response has no object IRI: {value}"))
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
    tools::memory_search(
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
    mindreader::config::load_env();
    let cfg = Config::from_env()?;
    println!("mindreader-smoke: uri={}", cfg.uri);
    println!(
        "registered tools: {}",
        Mindreader::registered_tool_names().join(", ")
    );

    let mut report = Report::new();
    let tool_names = Mindreader::registered_tool_names();
    report.check(
        "MCP registers the ten-tool contract",
        tool_names.len() == 10
            && tool_names.contains(&"memory_feedback".into())
            && tool_names.contains(&"memory_layers".into()),
        format!("tools={tool_names:?}"),
    );

    let graph = graph::connect(&cfg).await?;
    graph::bootstrap(&graph).await?;
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
                == Some((vec![layer_a, layer_c], true, 0))
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

    Ok(report.failed)
}
