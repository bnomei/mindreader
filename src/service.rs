//! Typed application boundary shared by MCP and in-process adapters.
//!
//! [`MemoryService`] holds the Neo4j handle and optional embedding runtime.
//! MCP uses `recall` / `recall_semantic` / `write` / `revise` / `withdraw` /
//! `judge` / `place` / `unify`. This type does not own rate limits or
//! `CallToolResult` mapping.

use crate::config::Config;
use crate::error::Result;
use crate::merge;
pub use crate::merge::UnifyArgs;
pub use crate::search::RecallArgs;
use crate::search::{self, is_schema_catalog_labels, validate_recall_args, SearchArgs};
pub use crate::semantic::SemanticSearchArgs;
use crate::semantic::{self, SemanticRuntime};
use crate::tools;
pub use crate::tools::{
    JudgeArgs, JudgeRating, PlaceArgs, PlaceEdit, ReviseArgs, TargetArgs, WithdrawArgs, WriteArgs,
    WriteFact,
};
use neo4rs::Graph;
use serde_json::{json, Value};
use std::path::PathBuf;

/// Shared graph handle plus optional embedding runtime for memory tools.
#[derive(Clone)]
pub struct MemoryService {
    /// Connected Neo4j handle; MCP constructs this only after lazy bootstrap.
    graph: Graph,
    /// Present when embedding credentials were loaded; otherwise semantic recall fails closed.
    semantic: Option<SemanticRuntime>,
    /// Colocated `.env` path used in `missing_embedding` diagnostics (never logged as contents).
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

    /// Build a service with a deterministic semantic runtime for developer tools.
    #[cfg(feature = "developer-tools")]
    pub fn with_semantic_runtime(
        graph: Graph,
        semantic: SemanticRuntime,
        secrets_path: PathBuf,
    ) -> Self {
        Self {
            graph,
            semantic: Some(semantic),
            secrets_path,
        }
    }

    /// Borrow the Neo4j handle for smoke, bench, and diagnostics.
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// Side-effectful embedding fusion for MCP `recall_semantic`.
    pub async fn recall_semantic(&self, args: SemanticSearchArgs) -> Result<Value> {
        semantic::memory_semantic_search(
            &self.graph,
            self.semantic.as_ref(),
            self.secrets_path.clone(),
            args,
        )
        .await
    }

    /// MCP `recall`: dispatch one closed-world selector to search, catalog, or walk.
    pub async fn recall(&self, args: RecallArgs) -> Result<Value> {
        validate_recall_args(&args)?;
        let scope = args.scope.clone();
        let detail = crate::payload::Detail::parse(args.detail.as_deref())?;
        let has_iris = args.iris.is_some();
        let result = if let Some(history) = args
            .history
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            tools::memory_recall_history(
                &self.graph,
                history,
                scope.clone(),
                args.limit.unwrap_or(20),
            )
            .await?
        } else if let Some(around) = args
            .around
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            tools::memory_recall_around(
                &self.graph,
                around,
                scope.clone(),
                args.p.unwrap_or_default(),
                args.depth.unwrap_or(1),
                args.limit.unwrap_or(20),
            )
            .await?
        } else if let Some(iris) = args
            .iris
            .filter(|values| values.iter().any(|value| !value.trim().is_empty()))
        {
            tools::memory_recall_iris(
                &self.graph,
                iris,
                scope.clone(),
                args.hops.unwrap_or(0),
                args.limit.unwrap_or(20),
            )
            .await?
        } else if let Some(labels) = args
            .labels
            .filter(|values| values.iter().any(|value| !value.trim().is_empty()))
        {
            if is_schema_catalog_labels(&labels) {
                let mut items = Vec::new();
                if labels.iter().any(|label| label.trim() == "Class") {
                    let catalog = tools::list_schema_catalog(&self.graph, "class").await?;
                    let nodes = catalog
                        .get("items")
                        .and_then(Value::as_array)
                        .ok_or_else(|| crate::operation_error!("Class catalog is missing items"))?;
                    items.extend(nodes.iter().cloned());
                }
                if labels.iter().any(|label| label.trim() == "Property") {
                    let catalog = tools::list_schema_catalog(&self.graph, "property").await?;
                    let nodes =
                        catalog
                            .get("items")
                            .and_then(Value::as_array)
                            .ok_or_else(|| {
                                crate::operation_error!("Property catalog is missing items")
                            })?;
                    items.extend(nodes.iter().cloned());
                }
                let truncated = items.len() > args.limit.unwrap_or(20) as usize;
                items.truncate(args.limit.unwrap_or(20) as usize);
                json!({
                    "ok": true,
                    "mode": "catalog",
                    "scope": scope,
                    "facts": [],
                    "nodes": items,
                    "paths": [],
                    "about": [],
                    "lookups": [],
                    "truncated": truncated,
                })
            } else {
                search::memory_search(
                    &self.graph,
                    SearchArgs {
                        layers: scope.clone(),
                        text: None,
                        labels: Some(labels),
                        limit: args.limit,
                    },
                )
                .await?
            }
        } else {
            search::memory_search(
                &self.graph,
                SearchArgs {
                    layers: scope.clone(),
                    text: args.text,
                    labels: None,
                    limit: args.limit,
                },
            )
            .await?
        };
        let mut result = result;
        if has_iris && detail == crate::payload::Detail::Concise {
            crate::payload::omit_iris_top_level_facts(&mut result);
        }
        crate::payload::finish_recall(result, &scope, detail)
    }

    /// MCP `write`: batched set-valued triples under one `scope`.
    pub async fn write(&self, args: WriteArgs) -> Result<Value> {
        tools::memory_write(&self.graph, args).await
    }

    /// MCP `revise`: membership-selective correction of one fact handle.
    pub async fn revise(&self, args: ReviseArgs) -> Result<Value> {
        tools::memory_revise(&self.graph, args).await
    }

    /// MCP `withdraw`: soft-withdraw a fact handle or a subject/predicate slice.
    pub async fn withdraw(&self, args: WithdrawArgs) -> Result<Value> {
        tools::memory_withdraw(&self.graph, args).await
    }

    /// MCP `judge`: sequential ±1 ratings on node or fact handles.
    pub async fn judge(&self, args: JudgeArgs) -> Result<Value> {
        tools::memory_judge(&self.graph, args).await
    }

    /// MCP `place`: change stored memberships; `scope` is visibility only.
    pub async fn place(&self, args: PlaceArgs) -> Result<Value> {
        tools::memory_place(&self.graph, args).await
    }

    /// MCP `unify`: permanent same-kind merge with no visibility filter.
    pub async fn unify(&self, args: UnifyArgs) -> Result<Value> {
        merge::memory_unify(&self.graph, args).await
    }
}
