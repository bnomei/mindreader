//! Typed application boundary for memory operations.
//!
//! Transport adapters depend on this service instead of coordinating graph
//! transactions or layer policy themselves. Methods delegate to `tools`,
//! `search`, `semantic`, and `merge` with a shared Neo4j handle and optional
//! semantic runtime.

use crate::config::Config;
use crate::error::Result;
use crate::merge::{self, MergeArgs};
use crate::search::{self, SearchArgs};
use crate::semantic::{self, SemanticRuntime, SemanticSearchArgs};
use crate::tools::{
    self, AssertArgs, FeedbackArgs, GetArgs, LayersArgs, ReplaceArgs, RetractArgs, SchemaArgs,
    StatsArgs, TraverseArgs,
};
use neo4rs::Graph;
use serde_json::Value;
use std::path::PathBuf;

/// Shared graph handle plus optional embedding runtime for all twelve memory tools.
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
}
