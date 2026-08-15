use crate::config::Config;
use crate::graph;
use neo4rs::Graph;
use crate::tools::{
    self, AssertArgs, GetArgs, RetractArgs, SchemaArgs, SearchArgs, TraverseArgs,
};
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{CallToolResult, ErrorData as McpError, ServerCapabilities, ServerInfo},
    tool, tool_handler, tool_router, ServerHandler,
};

#[derive(Clone)]
pub struct Mindreader {
    pub tool_router: ToolRouter<Self>,
    pub graph: Graph,
    pub project: String,
}

impl Mindreader {
    pub async fn connect() -> anyhow::Result<Self> {
        crate::config::load_env();
        let cfg = Config::from_env()?;
        let graph = graph::connect(&cfg).await?;
        graph::bootstrap(&graph).await?;
        Ok(Self {
            tool_router: Self::tool_router(),
            graph,
            project: cfg.project,
        })
    }

    pub fn registered_tool_names() -> Vec<String> {
        let router = Self::tool_router();
        let mut names: Vec<String> = router.map.keys().map(|k| k.to_string()).collect();
        names.sort();
        names
    }
}

fn ok(v: serde_json::Value) -> Result<CallToolResult, McpError> {
    Ok(CallToolResult::structured(v))
}

fn map_err(e: anyhow::Error) -> McpError {
    tools::map_tool_error(e)
}

#[tool_router]
impl Mindreader {
    #[tool(
        name = "memory_get",
        description = "Get a memory node by IRI. hops=0 returns the node; hops=1 includes current visible neighbors."
    )]
    async fn memory_get(
        &self,
        Parameters(args): Parameters<GetArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::memory_get(&self.graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }

    #[tool(
        name = "memory_search",
        description = "Search memory nodes by text and/or labels. Hop-capped; use memory_traverse to walk."
    )]
    async fn memory_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::memory_search(&self.graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }

    #[tool(
        name = "memory_traverse",
        description = "Traverse from a node along fixed relationship types. depth is hard-capped at 3."
    )]
    async fn memory_traverse(
        &self,
        Parameters(args): Parameters<TraverseArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::memory_traverse(&self.graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }

    #[tool(
        name = "memory_assert",
        description = "Assert (s, p, o) on a writable layer. Idempotent; different o supersedes. Optional spike label + ABOUT."
    )]
    async fn memory_assert(
        &self,
        Parameters(args): Parameters<AssertArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::memory_assert(&self.graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }

    #[tool(
        name = "memory_retract",
        description = "Soft-retract a current fact by iri or by (s, p, o, layer). Never hard-deletes nodes."
    )]
    async fn memory_retract(
        &self,
        Parameters(args): Parameters<RetractArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::memory_retract(&self.graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }

    #[tool(
        name = "memory_schema",
        description = "Declare a Class or Property (RDFS schema-as-data). Optional subClassOf, subPropertyOf, domain, range."
    )]
    async fn memory_schema(
        &self,
        Parameters(args): Parameters<SchemaArgs>,
    ) -> Result<CallToolResult, McpError> {
        tools::memory_schema(&self.graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }
}

#[tool_handler]
impl ServerHandler for Mindreader {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            instructions: Some(
                "Mindreader: RDFS schema-as-data memory over Neo4j. Tools: memory_get, memory_search, memory_traverse, memory_assert, memory_retract, memory_schema. project_id is env-only (MINDREADER_PROJECT). No raw Cypher."
                    .into(),
            ),
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            ..Default::default()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::Mindreader;

    #[test]
    fn registers_six_tools() {
        let names = Mindreader::registered_tool_names();
        let expected = [
            "memory_assert",
            "memory_get",
            "memory_retract",
            "memory_schema",
            "memory_search",
            "memory_traverse",
        ];
        assert_eq!(names, expected);
        let router = Mindreader::tool_router();
        for name in expected {
            assert!(router.has_route(name), "missing route {name}");
        }
        assert_eq!(router.map.len(), 6);
    }
}
