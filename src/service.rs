//! Typed application boundary shared by MCP and in-process adapters.
//!
//! [`MemoryService`] holds the Neo4j handle and optional embedding runtime.
//! MCP uses `recall` / `write` / `revise` / `withdraw` / `judge` / `place` /
//! `unify`. Smoke and bench still call the older get/search/assert helpers.
//! This type does not own rate limits or `CallToolResult` mapping.

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

    /// Borrow the Neo4j handle for smoke, bench, and diagnostics.
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// In-process IRI lookup; MCP uses [`Self::recall`] with `iris`.
    pub async fn get(&self, args: GetArgs) -> Result<Value> {
        tools::memory_get(&self.graph, args).await
    }

    /// In-process ranked `ASSERTS`/`ABOUT` search; MCP uses [`Self::recall`].
    pub async fn search(&self, args: SearchArgs) -> Result<Value> {
        search::memory_search(&self.graph, args).await
    }

    /// In-process embedding fusion; MCP uses [`Self::recall`] with `semantic:true`.
    pub async fn semantic_search(&self, args: SemanticSearchArgs) -> Result<Value> {
        semantic::memory_semantic_search(
            &self.graph,
            self.semantic.as_ref(),
            self.secrets_path.clone(),
            args,
        )
        .await
    }

    /// In-process unify; MCP `memory_unify` calls [`Self::unify`].
    pub async fn merge(&self, args: MergeArgs) -> Result<Value> {
        merge::memory_merge(&self.graph, args).await
    }

    /// In-process typed walk; MCP uses [`Self::recall`] with `around`.
    pub async fn traverse(&self, args: TraverseArgs) -> Result<Value> {
        tools::memory_traverse(&self.graph, args).await
    }

    /// Operator counters used by smoke and bench; not registered as an MCP tool.
    pub async fn stats(&self, args: StatsArgs) -> Result<Value> {
        tools::memory_stats(&self.graph, args).await
    }

    /// In-process batched assert; MCP `memory_write` calls [`Self::write`].
    pub async fn assert(&self, args: AssertArgs) -> Result<Value> {
        tools::memory_assert(&self.graph, args).await
    }

    /// In-process triple replace; MCP `memory_revise` resolves a fact handle first.
    pub async fn replace(&self, args: ReplaceArgs) -> Result<Value> {
        tools::memory_replace(&self.graph, args).await
    }

    /// In-process soft retract; MCP `memory_withdraw` wraps fact-handle or subject form.
    pub async fn retract(&self, args: RetractArgs) -> Result<Value> {
        tools::memory_retract(&self.graph, args).await
    }

    /// In-process schema write or catalog; MCP catalogs via [`Self::recall`] labels.
    pub async fn declare_schema(&self, args: SchemaArgs) -> Result<Value> {
        tools::memory_schema(&self.graph, args).await
    }

    /// In-process single-target ±1; MCP `memory_judge` batches ratings.
    pub async fn feedback(&self, args: FeedbackArgs) -> Result<Value> {
        tools::memory_feedback(&self.graph, args).await
    }

    /// In-process membership edit; MCP `memory_place` uses `scope` plus add/remove.
    pub async fn layers(&self, args: LayersArgs) -> Result<Value> {
        tools::memory_layers(&self.graph, args).await
    }

    /// MCP `memory_recall`: dispatch one selector to search, catalog, walk, or semantic fusion.
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

    /// MCP `memory_write`: batched set-valued triples under one `scope`.
    pub async fn write(&self, args: WriteArgs) -> Result<Value> {
        tools::memory_write(&self.graph, args).await
    }

    /// MCP `memory_revise`: membership-selective correction of one fact handle.
    pub async fn revise(&self, args: ReviseArgs) -> Result<Value> {
        tools::memory_revise(&self.graph, args).await
    }

    /// MCP `memory_withdraw`: soft-retract a fact handle or a subject/predicate slice.
    pub async fn withdraw(&self, args: WithdrawArgs) -> Result<Value> {
        tools::memory_withdraw(&self.graph, args).await
    }

    /// MCP `memory_judge`: sequential ±1 ratings on node or fact handles.
    pub async fn judge(&self, args: JudgeArgs) -> Result<Value> {
        tools::memory_judge(&self.graph, args).await
    }

    /// MCP `memory_place`: change stored memberships; `scope` is visibility only.
    pub async fn place(&self, args: PlaceArgs) -> Result<Value> {
        tools::memory_place(&self.graph, args).await
    }

    /// MCP `memory_unify`: permanent same-kind merge with no visibility filter.
    pub async fn unify(&self, args: MergeArgs) -> Result<Value> {
        merge::memory_merge(&self.graph, args).await
    }
}
