//! Explicit shared weight ratings for visible current nodes and facts.
//!
//! Each rating changes weight by exactly `+1` or `-1`. Batches are 1–20 unique
//! targets in one atomic transaction and one Episode. Retrieval never changes
//! weight; weights do not decay. Search uses weight only within the same Spike
//! category.

use super::facts::{
    is_transient_neo4j_error, normalize_layers, validate_target, JudgeArgs, JudgeRating,
    FACT_LOCK_SCOPE, MAX_WRITE_FACTS,
};
use crate::domain::DomainError;
use crate::error::{Error, Result};
use crate::graph::{acquire_fact_locks_in_txn, create_episode_in_txn, fetch_one_txn};
use crate::layers::validate_layer_ids;
use crate::payload::finish_mutation;
use neo4rs::{query, Graph, Txn};
use serde_json::{json, Value};
use std::collections::HashSet;
use tokio::time::{sleep, Duration};

fn judge_delta(mode: &str) -> Result<i64> {
    match mode {
        "strengthen" => Ok(1),
        "weaken" => Ok(-1),
        _ => {
            Err(DomainError::InvalidInput("judge mode must be strengthen or weaken".into()).into())
        }
    }
}

/// Require 1..=20 unique rateable targets before the judge transaction starts.
pub(super) fn validate_judge_args(args: &JudgeArgs) -> Result<()> {
    if args.ratings.is_empty() || args.ratings.len() > MAX_WRITE_FACTS {
        return Err(DomainError::InvalidInput(format!(
            "judge ratings must contain between 1 and {MAX_WRITE_FACTS} items"
        ))
        .into());
    }
    validate_layer_ids(args.scope.clone())?;
    let mut seen = HashSet::new();
    for rating in &args.ratings {
        validate_target(&rating.target)?;
        judge_delta(&rating.mode)?;
        if !seen.insert((rating.target.kind.clone(), rating.target.iri.clone())) {
            return Err(DomainError::InvalidInput(format!(
                "judge contains duplicate target {}:{}",
                rating.target.kind, rating.target.iri
            ))
            .into());
        }
    }
    Ok(())
}

/// Apply one ±1 judge-weight step on a visible current node or fact.
///
/// At i64 bounds the MATCH returns no row and the rating fails.
async fn apply_judge_rating_txn(
    txn: &mut Txn,
    scope: &[String],
    rating: &JudgeRating,
) -> Result<(i64, i64)> {
    let delta = judge_delta(&rating.mode)?;
    let strengthen = delta > 0;
    let update = if rating.target.kind == "node" {
        fetch_one_txn(
            txn,
            query(
                r#"
                MATCH (target:Entity {iri: $iri})
                WHERE size(target.layers) = 0
                   OR any(layer IN target.layers WHERE layer IN $scope)
                WITH target, target.weight AS before
                WHERE ($strengthen AND before < $max) OR (NOT $strengthen AND before > $min)
                SET target.weight = CASE WHEN $strengthen THEN before + 1 ELSE before - 1 END
                RETURN before, target.weight AS after
                "#,
            )
            .param("iri", rating.target.iri.clone())
            .param("scope", scope.to_vec())
            .param("strengthen", strengthen)
            .param("max", i64::MAX)
            .param("min", i64::MIN),
        )
        .await?
    } else {
        fetch_one_txn(
            txn,
            query(
                r#"
                MATCH (s:Entity)-[target]->(o:Entity)
                WHERE target.iri = $iri AND target.validTo IS NULL
                  AND (size(s.layers) = 0 OR any(layer IN s.layers WHERE layer IN $scope))
                  AND (size(target.layers) = 0 OR any(layer IN target.layers WHERE layer IN $scope))
                  AND (size(o.layers) = 0 OR any(layer IN o.layers WHERE layer IN $scope))
                WITH target, target.weight AS before
                WHERE ($strengthen AND before < $max) OR (NOT $strengthen AND before > $min)
                SET target.weight = CASE WHEN $strengthen THEN before + 1 ELSE before - 1 END
                RETURN before, target.weight AS after
                "#,
            )
            .param("iri", rating.target.iri.clone())
            .param("scope", scope.to_vec())
            .param("strengthen", strengthen)
            .param("max", i64::MAX)
            .param("min", i64::MIN),
        )
        .await?
    };
    let update = update.ok_or_else(|| {
        DomainError::Precondition(format!(
            "judge target {}:{} is missing, hidden, historical, or at the weight boundary",
            rating.target.kind, rating.target.iri
        ))
    })?;
    let before: i64 = update.get("before")?;
    let after: i64 = update.get("after")?;
    Ok((before, after))
}

/// One judge transaction: lock, apply every ±1 rating, record one Episode, or roll back.
async fn memory_judge_once(graph: &Graph, args: JudgeArgs) -> Result<Value> {
    let scope = normalize_layers(args.scope)?;
    let locks = args
        .ratings
        .iter()
        .map(|rating| {
            (
                rating.target.iri.clone(),
                "mindreader:property/weight".into(),
                FACT_LOCK_SCOPE.into(),
            )
        })
        .collect::<Vec<_>>();
    let mut txn = graph.start_txn().await?;
    let result = async {
        acquire_fact_locks_in_txn(&mut txn, &locks).await?;
        let mut items = Vec::with_capacity(args.ratings.len());
        for (index, rating) in args.ratings.iter().enumerate() {
            let (before, after) = apply_judge_rating_txn(&mut txn, &scope, rating).await?;
            items.push(json!({
                "index": index,
                "target": rating.target,
                "mode": rating.mode,
                "delta": judge_delta(&rating.mode)?,
                "before": before,
                "after": after,
                "status": "changed",
            }));
        }
        let episode = create_episode_in_txn(&mut txn, "judge", None).await?;
        let target_iris = args
            .ratings
            .iter()
            .map(|rating| rating.target.iri.clone())
            .collect::<Vec<_>>();
        let target_kinds = args
            .ratings
            .iter()
            .map(|rating| rating.target.kind.clone())
            .collect::<Vec<_>>();
        let modes = args
            .ratings
            .iter()
            .map(|rating| rating.mode.clone())
            .collect::<Vec<_>>();
        txn.run(
            query(
                "MATCH (e:Entity:Episode {iri: $episode}) \
                 SET e.batchSize = $batchSize, e.targetIris = $targetIris, \
                     e.targetKinds = $targetKinds, e.modes = $modes",
            )
            .param("episode", episode.iri.clone())
            .param("batchSize", items.len() as i64)
            .param("targetIris", target_iris)
            .param("targetKinds", target_kinds)
            .param("modes", modes),
        )
        .await?;
        for rating in &args.ratings {
            let target_match = if rating.target.kind == "node" {
                "MATCH (target:Entity {iri: $iri})"
            } else {
                "MATCH ()-[target]->() WHERE target.iri = $iri"
            };
            txn.run(
                query(&format!(
                    "{target_match} SET target.weightUpdatedAt = datetime(), \
                     target.judgmentEpisodeId = $episode"
                ))
                .param("iri", rating.target.iri.clone())
                .param("episode", episode.iri.clone()),
            )
            .await?;
        }
        Ok::<_, Error>((episode, items))
    }
    .await;
    let (episode, items) = match result {
        Ok(value) => {
            txn.commit()
                .await
                .map_err(|source| Error::AmbiguousCommit {
                    operation: "judge",
                    source,
                })?;
            value
        }
        Err(error) => {
            let _ = txn.rollback().await;
            return Err(error);
        }
    };
    let judge_facts = items
        .iter()
        .filter(|item| item.pointer("/target/kind").and_then(Value::as_str) == Some("fact"))
        .map(|item| json!({ "target": item.get("target") }))
        .collect::<Vec<_>>();
    let judge_nodes = items
        .iter()
        .filter(|item| item.pointer("/target/kind").and_then(Value::as_str) == Some("node"))
        .filter_map(|item| item.get("target").cloned())
        .collect::<Vec<_>>();
    finish_mutation(
        json!({
            "ok": true,
            "scope": scope,
            "noop": false,
            "episode": { "iri": episode.iri, "at": episode.at, "tool": episode.tool },
            "summary": { "requested": items.len(), "changed": items.len(), "noop": 0 },
            "items": items,
        }),
        &judge_facts,
        &judge_nodes,
        None,
        None,
    )
}

/// Atomic `judge`: each rating changes a visible current node or fact’s shared signed weight by exactly +1 or −1.
///
/// Hidden or historical handles fail; one Episode for the batch.
pub async fn memory_judge(graph: &Graph, args: JudgeArgs) -> Result<Value> {
    validate_judge_args(&args)?;
    for attempt in 0..3_u64 {
        match memory_judge_once(graph, args.clone()).await {
            Err(error) if attempt < 2 && is_transient_neo4j_error(&error) => {
                sleep(Duration::from_millis(25 * (attempt + 1))).await;
            }
            result => return result,
        }
    }
    unreachable!("bounded retry loop always returns")
}
