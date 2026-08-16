//! Release-mode graph performance and ranking regression benchmark.
//!
//! Seeds a large entity set, warms indexes, then samples ranked
//! `ASSERTS`/`ABOUT` search and merge-suggestion latency. Enabled with
//! `developer-tools`; mutates the configured database like the smoke suite.

use mindreader::config::Config;
use mindreader::error::{Context, Result};
use mindreader::graph::{self, acquire_fact_locks_in_txn, fetch_one};
use mindreader::merge::merge_suggestions_in_txn;
use mindreader::operation_error;
use mindreader::search::{self, SearchArgs};
use neo4rs::{query, Graph};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_ENTITIES: i64 = 10_000;
const DEFAULT_SAMPLES: usize = 30;
const WARMUPS: usize = 2;

#[derive(Debug)]
struct Options {
    config_dir: Option<PathBuf>,
    entities: i64,
    samples: usize,
}

fn parse_options() -> Result<Options> {
    let mut config_dir = None;
    let mut entities = DEFAULT_ENTITIES;
    let mut samples = DEFAULT_SAMPLES;
    let mut args = std::env::args().skip(1);
    while let Some(argument) = args.next() {
        match argument.as_str() {
            "--config-dir" => {
                config_dir =
                    Some(PathBuf::from(args.next().ok_or_else(|| {
                        operation_error!("--config-dir requires a path")
                    })?));
            }
            "--entities" => {
                entities = args
                    .next()
                    .ok_or_else(|| operation_error!("--entities requires a positive integer"))?
                    .parse()
                    .context("parse --entities")?;
            }
            "--samples" => {
                samples = args
                    .next()
                    .ok_or_else(|| operation_error!("--samples requires a positive integer"))?
                    .parse()
                    .context("parse --samples")?;
            }
            _ => {
                return Err(operation_error!(
                    "usage: mindreader-bench [--config-dir PATH] [--entities N] [--samples N]"
                ));
            }
        }
    }
    if entities < 1 || samples < 1 {
        return Err(operation_error!(
            "--entities and --samples must be positive"
        ));
    }
    Ok(Options {
        config_dir,
        entities,
        samples,
    })
}

fn summary(mut timings_ms: Vec<f64>) -> Value {
    timings_ms.sort_by(|left, right| left.total_cmp(right));
    let percentile = |numerator: usize, denominator: usize| {
        let index = (timings_ms.len() * numerator)
            .div_ceil(denominator)
            .saturating_sub(1);
        timings_ms[index]
    };
    json!({
        "samples": timings_ms.len(),
        "minMs": timings_ms[0],
        "p50Ms": percentile(1, 2),
        "p95Ms": percentile(95, 100),
        "maxMs": timings_ms[timings_ms.len() - 1],
    })
}

async fn seed(graph: &Graph, prefix: &str, layer: &str, entities: i64) -> Result<(String, String)> {
    graph
        .run(
            query(
                r#"
                UNWIND range(0, $last) AS i
                CREATE (s:Entity:Element {
                  iri: $prefix + '/subject/' + toString(i),
                  name: 'common benchmark subject ' + toString(i),
                  searchText: 'common benchmark subject ' + toString(i),
                  mergeName: 'common benchmark subject ' + toString(i),
                  layers: [$layer], weight: i % 17, weightText: toString(i % 17),
                  createdAt: datetime()
                })
                CREATE (o:Entity:Element {
                  iri: $prefix + '/object/' + toString(i),
                  name: 'benchmark object ' + toString(i),
                  searchText: 'benchmark object ' + toString(i),
                  mergeName: 'benchmark object ' + toString(i),
                  layers: [$layer], weight: i % 11, weightText: toString(i % 11),
                  createdAt: datetime()
                })
                CREATE (s)-[:ASSERTS {
                  iri: $prefix + '/fact/' + toString(i),
                  propertyIri: 'mindreader:property/benchmark',
                  factText: 'common benchmark fact ' + toString(i),
                  layers: [$layer], weight: i % 13, weightText: toString(i % 13),
                  validFrom: datetime()
                }]->(o)
                "#,
            )
            .param("last", entities - 1)
            .param("prefix", prefix.to_string())
            .param("layer", layer.to_string()),
        )
        .await
        .context("seed search benchmark facts")?;

    let created_iri = format!("{prefix}/merge/spaceships");
    let candidate_iri = format!("{prefix}/merge/spaceship");
    graph
        .run(
            query(
                r#"
                CREATE (:Entity:Element {
                  iri: $createdIri, name: 'spaceships', searchText: 'spaceships',
                  mergeName: 'spaceships',
                  layers: [$layer], weight: 0, weightText: '0', createdAt: datetime()
                })
                CREATE (:Entity:Element {
                  iri: $candidateIri, name: 'spaceship', searchText: 'spaceship',
                  mergeName: 'spaceship',
                  layers: [$layer], weight: 0, weightText: '0', createdAt: datetime()
                })
                "#,
            )
            .param("createdIri", created_iri.clone())
            .param("candidateIri", candidate_iri.clone())
            .param("layer", layer.to_string()),
        )
        .await
        .context("seed merge benchmark candidates")?;
    graph
        .run(query(
            "CALL db.index.fulltext.awaitEventuallyConsistentIndexRefresh()",
        ))
        .await
        .context("refresh full-text indexes")?;
    Ok((created_iri, candidate_iri))
}

async fn benchmark_search(
    graph: &Graph,
    layer: &str,
    prefix: &str,
    entities: i64,
    samples: usize,
) -> Result<(Value, Vec<String>)> {
    let args = || SearchArgs {
        layers: vec![layer.to_string()],
        text: Some("common".into()),
        labels: None,
        limit: Some(20),
    };
    for _ in 0..WARMUPS {
        search::memory_search(graph, args()).await?;
    }
    let mut timings = Vec::with_capacity(samples);
    let mut reference = Vec::new();
    for sample in 0..samples {
        let started = Instant::now();
        let result = search::memory_search(graph, args()).await?;
        timings.push(started.elapsed().as_secs_f64() * 1_000.0);
        let relationships = result
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
            .collect::<Vec<_>>();
        if sample == 0 {
            reference = relationships;
        } else if relationships != reference {
            return Err(operation_error!(
                "memory_search returned nondeterministic relationship ordering"
            ));
        }
    }
    let mut expected = (0..entities)
        .map(|index| {
            (
                index,
                index % 17 + index % 13 + index % 11,
                format!("{prefix}/subject/{index}"),
            )
        })
        .collect::<Vec<_>>();
    expected.sort_by(|left, right| right.1.cmp(&left.1).then_with(|| left.2.cmp(&right.2)));
    let expected = expected
        .into_iter()
        .take(20)
        .map(|(index, _, _)| format!("{prefix}/fact/{index}"))
        .collect::<Vec<_>>();
    if reference != expected {
        return Err(operation_error!(
            "memory_search ranking diverged from the benchmark oracle: expected={expected:?} actual={reference:?}"
        ));
    }
    Ok((summary(timings), reference))
}

async fn benchmark_locks(graph: &Graph, layer: &str, samples: usize) -> Result<Value> {
    let mut results = serde_json::Map::new();
    for fact_count in [1_usize, 4, 16] {
        let facts = (0..fact_count)
            .map(|index| {
                (
                    format!("mindreader:benchmark/lock-subject-{index}"),
                    format!("mindreader:property/lock-{index}"),
                    layer.to_string(),
                )
            })
            .collect::<Vec<_>>();
        let mut timings = Vec::with_capacity(samples);
        for _ in 0..samples {
            let mut txn = graph.start_txn().await?;
            let started = Instant::now();
            acquire_fact_locks_in_txn(&mut txn, &facts).await?;
            timings.push(started.elapsed().as_secs_f64() * 1_000.0);
            txn.rollback().await?;
        }
        results.insert(fact_count.to_string(), summary(timings));
    }
    Ok(Value::Object(results))
}

async fn benchmark_suggestions(
    graph: &Graph,
    created_iri: &str,
    candidate_iri: &str,
    layer: &str,
    samples: usize,
) -> Result<(Value, Value)> {
    let mut timings = Vec::with_capacity(samples);
    let mut reference = Value::Null;
    for sample in 0..samples {
        let mut txn = graph.start_txn().await?;
        let started = Instant::now();
        let suggestions =
            merge_suggestions_in_txn(&mut txn, &[created_iri.to_string()], &[layer.to_string()])
                .await?;
        timings.push(started.elapsed().as_secs_f64() * 1_000.0);
        txn.rollback().await?;
        let value = Value::Array(suggestions);
        if sample == 0 {
            reference = value;
        } else if value != reference {
            return Err(operation_error!(
                "merge suggestions returned nondeterministic output"
            ));
        }
    }
    let found_expected = reference.as_array().is_some_and(|suggestions| {
        suggestions.iter().any(|suggestion| {
            suggestion.pointer("/source/iri").and_then(Value::as_str) == Some(created_iri)
                && suggestion.pointer("/target/iri").and_then(Value::as_str) == Some(candidate_iri)
        })
    });
    if !found_expected {
        return Err(operation_error!(
            "merge benchmark did not recommend spaceships -> spaceship: {reference}"
        ));
    }
    Ok((summary(timings), reference))
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(result) => {
            println!("{}", serde_json::to_string_pretty(&result).unwrap());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("BENCH ABORT: {error:#}");
            ExitCode::from(1)
        }
    }
}

async fn run() -> Result<Value> {
    let options = parse_options()?;
    let config = match options.config_dir {
        Some(path) => Config::from_directory(path)?,
        None => Config::from_env()?,
    };
    let graph = graph::connect(&config).await?;
    graph::bootstrap(&graph, None).await?;
    let pristine = fetch_one(
        &graph,
        query(
            "MATCH (n) \
             WITH count(n) AS nodes \
             OPTIONAL MATCH ()-[r]->() \
             RETURN nodes, count(r) AS relationships",
        ),
    )
    .await?
    .ok_or_else(|| operation_error!("benchmark pristine-database check returned no row"))?;
    let existing_nodes = pristine.get::<i64>("nodes")?;
    let existing_relationships = pristine.get::<i64>("relationships")?;
    if existing_nodes != 15 || existing_relationships != 0 {
        return Err(operation_error!(
            "mindreader-bench requires a fresh disposable model-v5 database after bootstrap; found {existing_nodes} nodes and {existing_relationships} relationships"
        ));
    }

    let tag = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .context("system clock is before Unix epoch")?
        .as_nanos();
    let prefix = format!("mindreader:benchmark/{tag}");
    let layer = format!("benchmark:performance-{tag}");
    let seeded = Instant::now();
    let (created_iri, candidate_iri) = seed(&graph, &prefix, &layer, options.entities).await?;
    let seed_ms = seeded.elapsed().as_secs_f64() * 1_000.0;

    let (search, search_order) =
        benchmark_search(&graph, &layer, &prefix, options.entities, options.samples).await?;
    let locks = benchmark_locks(&graph, &layer, options.samples).await?;
    let (suggestions, suggestion_output) = benchmark_suggestions(
        &graph,
        &created_iri,
        &candidate_iri,
        &layer,
        options.samples,
    )
    .await?;

    Ok(json!({
        "workload": {
            "subjectEntities": options.entities,
            "objectEntities": options.entities,
            "mergeCandidateEntities": 2,
            "seededEntities": options.entities.saturating_mul(2).saturating_add(2),
            "facts": options.entities,
            "samples": options.samples,
            "warmups": WARMUPS,
            "seedMs": seed_ms,
            "layer": layer,
        },
        "memorySearch": {
            "latency": search,
            "relationshipOrder": search_order,
        },
        "factLocksByFactCount": locks,
        "mergeSuggestions": {
            "latency": suggestions,
            "output": suggestion_output,
        },
    }))
}

#[cfg(test)]
mod tests {
    use super::summary;

    #[test]
    fn summary_uses_nearest_rank_percentiles() {
        let seven = summary((1..=7).map(f64::from).collect());
        assert_eq!(seven["p50Ms"], 4.0);
        assert_eq!(seven["p95Ms"], 7.0);

        let two = summary(vec![1.0, 2.0]);
        assert_eq!(two["p95Ms"], 2.0);
    }
}
