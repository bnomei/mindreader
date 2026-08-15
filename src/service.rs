//! Typed application boundary for memory operations.
//!
//! Transport adapters depend on this service instead of coordinating graph
//! transactions or project-layer policy themselves.

use crate::domain::ProjectId;
use crate::tools::{
    self, AssertArgs, GetArgs, ReplaceArgs, RetractArgs, SchemaArgs, SearchArgs, StatsArgs,
    TraverseArgs,
};
use anyhow::Result;
use neo4rs::Graph;
use serde_json::Value;

#[derive(Clone)]
pub struct MemoryService {
    graph: Graph,
    project: ProjectId,
}

impl MemoryService {
    pub fn new(graph: Graph, project: ProjectId) -> Self {
        Self { graph, project }
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    pub fn project(&self) -> &ProjectId {
        &self.project
    }

    pub async fn get(&self, args: GetArgs) -> Result<Value> {
        tools::memory_get(&self.graph, self.project.as_str(), args).await
    }

    pub async fn search(&self, args: SearchArgs) -> Result<Value> {
        tools::memory_search(&self.graph, self.project.as_str(), args).await
    }

    pub async fn traverse(&self, args: TraverseArgs) -> Result<Value> {
        tools::memory_traverse(&self.graph, self.project.as_str(), args).await
    }

    pub async fn stats(&self, args: StatsArgs) -> Result<Value> {
        tools::memory_stats(&self.graph, self.project.as_str(), args).await
    }

    pub async fn assert(&self, args: AssertArgs) -> Result<Value> {
        tools::memory_assert(&self.graph, self.project.as_str(), args).await
    }

    pub async fn replace(&self, args: ReplaceArgs) -> Result<Value> {
        tools::memory_replace(&self.graph, self.project.as_str(), args).await
    }

    pub async fn retract(&self, args: RetractArgs) -> Result<Value> {
        tools::memory_retract(&self.graph, self.project.as_str(), args).await
    }

    pub async fn declare_schema(&self, args: SchemaArgs) -> Result<Value> {
        tools::memory_schema(&self.graph, self.project.as_str(), args).await
    }
}
