//! Typed application boundary for memory operations.
//!
//! Transport adapters depend on this service instead of coordinating graph
//! transactions or layer policy themselves. Methods delegate to `tools`,
//! `search`, `semantic`, and `merge` with a shared Neo4j handle and optional
//! semantic runtime.

use crate::config::Config;
use crate::error::Result;
use crate::merge::{self, MergeArgs};
use crate::search::{self, is_schema_catalog_labels, validate_recall_args, RecallArgs, SearchArgs};
use crate::semantic::{self, SemanticRuntime, SemanticSearchArgs};
use crate::tools::{
    self, AssertArgs, FeedbackArgs, GetArgs, JudgeArgs, LayersArgs, PlaceArgs, ReplaceArgs,
    RetractArgs, ReviseArgs, SchemaArgs, StatsArgs, TraverseArgs, WithdrawArgs, WriteArgs,
};
use neo4rs::Graph;
use serde_json::{json, Value};
use std::path::PathBuf;

/// Shared graph handle plus optional embedding runtime for memory tools.
#[derive(Clone)]
pub struct MemoryService {
    graph: Graph,
    semantic: Option<SemanticRuntime>,
    secrets_path: PathBuf,
}

impl MemoryService {
    /// Build a service from an already-connected graph and loaded config.
    pub fn new(graph: Graph, config: &Config) -> Result<Self> {
        Ok(Self {
            graph,
            semantic: SemanticRuntime::from_config(config)?,
            secrets_path: config.secrets_path(),
        })
    }

    /// Borrow the underlying Neo4j graph (smoke tests and diagnostics).
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    pub async fn get(&self, args: GetArgs) -> Result<Value> {
        tools::memory_get(&self.graph, args).await
    }

    pub async fn search(&self, args: SearchArgs) -> Result<Value> {
        search::memory_search(&self.graph, args).await
    }

    pub async fn semantic_search(&self, args: SemanticSearchArgs) -> Result<Value> {
        semantic::memory_semantic_search(
            &self.graph,
            self.semantic.as_ref(),
            self.secrets_path.clone(),
            args,
        )
        .await
    }

    pub async fn merge(&self, args: MergeArgs) -> Result<Value> {
        merge::memory_merge(&self.graph, args).await
    }

    pub async fn traverse(&self, args: TraverseArgs) -> Result<Value> {
        tools::memory_traverse(&self.graph, args).await
    }

    pub async fn stats(&self, args: StatsArgs) -> Result<Value> {
        tools::memory_stats(&self.graph, args).await
    }

    pub async fn assert(&self, args: AssertArgs) -> Result<Value> {
        tools::memory_assert(&self.graph, args).await
    }

    pub async fn replace(&self, args: ReplaceArgs) -> Result<Value> {
        tools::memory_replace(&self.graph, args).await
    }

    pub async fn retract(&self, args: RetractArgs) -> Result<Value> {
        tools::memory_retract(&self.graph, args).await
    }

    pub async fn declare_schema(&self, args: SchemaArgs) -> Result<Value> {
        tools::memory_schema(&self.graph, args).await
    }

    pub async fn feedback(&self, args: FeedbackArgs) -> Result<Value> {
        tools::memory_feedback(&self.graph, args).await
    }

    pub async fn layers(&self, args: LayersArgs) -> Result<Value> {
        tools::memory_layers(&self.graph, args).await
    }

    pub async fn recall(&self, args: RecallArgs) -> Result<Value> {
        validate_recall_args(&args)?;
        let scope = args.scope.clone();
        if args.semantic {
            return semantic::memory_semantic_search(
                &self.graph,
                self.semantic.as_ref(),
                self.secrets_path.clone(),
                SemanticSearchArgs {
                    text: args.text.unwrap_or_default(),
                    layers: scope,
                    labels: None,
                    limit: args.limit,
                },
            )
            .await;
        }
        if let Some(around) = args
            .around
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            return tools::memory_recall_around(
                &self.graph,
                around,
                scope,
                args.p.unwrap_or_default(),
                args.depth.unwrap_or(1),
                args.limit.unwrap_or(50),
            )
            .await;
        }
        if let Some(iris) = args
            .iris
            .filter(|values| values.iter().any(|value| !value.trim().is_empty()))
        {
            return tools::memory_recall_iris(&self.graph, iris, scope, args.hops.unwrap_or(0))
                .await;
        }
        if let Some(labels) = args
            .labels
            .filter(|values| values.iter().any(|value| !value.trim().is_empty()))
        {
            if is_schema_catalog_labels(&labels) {
                let mut items = Vec::new();
                if labels.iter().any(|label| label.trim() == "Class") {
                    let catalog = tools::list_schema_catalog(&self.graph, "class").await?;
                    if let Some(nodes) = catalog.get("items").and_then(Value::as_array) {
                        items.extend(nodes.iter().cloned());
                    }
                }
                if labels.iter().any(|label| label.trim() == "Property") {
                    let catalog = tools::list_schema_catalog(&self.graph, "property").await?;
                    if let Some(nodes) = catalog.get("items").and_then(Value::as_array) {
                        items.extend(nodes.iter().cloned());
                    }
                }
                return Ok(json!({
                    "scope": scope,
                    "semantic": false,
                    "facts": [],
                    "nodes": items,
                }));
            }
            return search::memory_search(
                &self.graph,
                SearchArgs {
                    layers: scope,
                    text: None,
                    labels: Some(labels),
                    limit: args.limit,
                },
            )
            .await;
        }
        search::memory_search(
            &self.graph,
            SearchArgs {
                layers: scope,
                text: args.text,
                labels: None,
                limit: args.limit,
            },
        )
        .await
    }

    pub async fn write(&self, args: WriteArgs) -> Result<Value> {
        tools::memory_write(&self.graph, args).await
    }

    pub async fn revise(&self, args: ReviseArgs) -> Result<Value> {
        tools::memory_revise(&self.graph, args).await
    }

    pub async fn withdraw(&self, args: WithdrawArgs) -> Result<Value> {
        tools::memory_withdraw(&self.graph, args).await
    }

    pub async fn judge(&self, args: JudgeArgs) -> Result<Value> {
        tools::memory_judge(&self.graph, args).await
    }

    pub async fn place(&self, args: PlaceArgs) -> Result<Value> {
        tools::memory_place(&self.graph, args).await
    }

    pub async fn unify(&self, args: MergeArgs) -> Result<Value> {
        merge::memory_merge(&self.graph, args).await
    }
}
