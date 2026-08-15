use crate::config::Config;
use crate::graph;
use crate::tools::{
    self, AssertArgs, GetArgs, RetractArgs, SchemaArgs, SearchArgs, TraverseArgs,
};
use neo4rs::Graph;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ErrorData as McpError, Implementation, InitializeRequestParam,
        ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    service::{RequestContext, RoleServer},
    tool, tool_handler, tool_router, ServerHandler,
};
use std::sync::Arc;
use tokio::sync::OnceCell;

fn object_schema(value: serde_json::Value) -> Arc<rmcp::model::JsonObject> {
    Arc::new(rmcp::model::object(value))
}

fn schema_memory_get() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "iri": { "type": "string" },
            "hops": { "type": "integer" }
        },
        "required": ["iri"]
    }))
}

fn schema_memory_search() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "text": { "type": "string" },
            "labels": { "type": "array", "items": { "type": "string" } },
            "limit": { "type": "integer" }
        }
    }))
}

fn schema_memory_traverse() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "from": { "type": "string" },
            "rels": { "type": "array", "items": { "type": "string" } },
            "depth": { "type": "integer" },
            "limit": { "type": "integer" }
        },
        "required": ["from"]
    }))
}

fn schema_memory_assert() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "s": {
                "description": "subject: IRI string, name, or {iri,name,labels}",
                "anyOf": [
                    { "type": "string" },
                    {
                        "type": "object",
                        "properties": {
                            "iri": { "type": "string" },
                            "name": { "type": "string" },
                            "labels": { "type": "array", "items": { "type": "string" } }
                        }
                    }
                ]
            },
            "p": { "type": "string" },
            "o": {
                "description": "object: IRI string, name, {iri,name,labels}, or literal",
                "anyOf": [
                    { "type": "string" },
                    { "type": "number" },
                    { "type": "boolean" },
                    {
                        "type": "object",
                        "properties": {
                            "iri": { "type": "string" },
                            "name": { "type": "string" },
                            "labels": { "type": "array", "items": { "type": "string" } },
                            "value": { "type": "string" },
                            "datatype": { "type": "string" }
                        }
                    }
                ]
            },
            "layer": { "type": "string" },
            "spike": { "type": "string" },
            "contradicts": { "type": "boolean", "description": "if true, write CONTRADICTS to conflicting objects on other visible layers" }
        },
        "required": ["s", "p", "o"]
    }))
}

fn schema_memory_retract() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "iri": { "type": "string" },
            "s": { "type": "string" },
            "p": { "type": "string" },
            "o": { "description": "object value", "type": "object" },
            "layer": { "type": "string" },
            "reason": { "type": "string" }
        }
    }))
}

fn schema_memory_schema() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "kind": { "type": "string", "description": "class or property" },
            "name": { "type": "string" },
            "iri": { "type": "string" },
            "subClassOf": { "type": "string" },
            "subPropertyOf": { "type": "string" },
            "domain": { "type": "string" },
            "range": { "type": "string" }
        },
        "required": ["kind"]
    }))
}

#[derive(Clone)]
pub struct Mindreader {
    pub tool_router: ToolRouter<Self>,
    graph: Arc<OnceCell<Graph>>,
    pub project: String,
    cfg: Config,
}

impl Mindreader {
    fn from_config(cfg: Config) -> Self {
        Self {
            tool_router: Self::tool_router(),
            graph: Arc::new(OnceCell::new()),
            project: cfg.project.clone(),
            cfg,
        }
    }

    /// Build the server without talking to Neo4j so MCP initialize/list_tools
    /// can run immediately.
    pub fn from_env() -> anyhow::Result<Self> {
        crate::config::load_env();
        Ok(Self::from_config(Config::from_env()?))
    }

    /// Eager connect (tests / smoke). Prefer `from_env` + serve for MCP.
    pub async fn connect() -> anyhow::Result<Self> {
        let this = Self::from_env()?;
        this.ensure_connected().await?;
        Ok(this)
    }

    pub async fn ensure_connected(&self) -> anyhow::Result<()> {
        self.graph
            .get_or_try_init(|| async {
                let g = graph::connect(&self.cfg).await?;
                graph::bootstrap(&g).await?;
                Ok::<_, anyhow::Error>(g)
            })
            .await?;
        Ok(())
    }

    async fn graph(&self) -> Result<&Graph, McpError> {
        self.ensure_connected().await.map_err(map_err)?;
        self.graph
            .get()
            .ok_or_else(|| McpError::internal_error("neo4j not connected", None))
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
        description = "Get a memory node by IRI. hops=0 returns the node; hops=1 includes current visible neighbors.",
        input_schema = schema_memory_get()
    )]
    async fn memory_get(
        &self,
        Parameters(args): Parameters<GetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.graph().await?;
        tools::memory_get(graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }

    #[tool(
        name = "memory_search",
        description = "Wake-up search: current layer-visible facts (s,p,o) plus ABOUT SPIKE. Not a node directory.",
        input_schema = schema_memory_search()
    )]
    async fn memory_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.graph().await?;
        tools::memory_search(graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }

    #[tool(
        name = "memory_traverse",
        description = "Traverse from a node along fixed relationship types. depth is hard-capped at 3.",
        input_schema = schema_memory_traverse()
    )]
    async fn memory_traverse(
        &self,
        Parameters(args): Parameters<TraverseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.graph().await?;
        tools::memory_traverse(graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }

    #[tool(
        name = "memory_assert",
        description = "Assert (s, p, o) on a writable layer. Idempotent; different o supersedes. Returns conflicts[] across visible layers. Optional spike and contradicts.",
        input_schema = schema_memory_assert()
    )]
    async fn memory_assert(
        &self,
        Parameters(args): Parameters<AssertArgs>,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.graph().await?;
        tools::memory_assert(graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }

    #[tool(
        name = "memory_retract",
        description = "Soft-retract a current fact by iri or by (s, p, o, layer). Never hard-deletes nodes.",
        input_schema = schema_memory_retract()
    )]
    async fn memory_retract(
        &self,
        Parameters(args): Parameters<RetractArgs>,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.graph().await?;
        tools::memory_retract(graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }

    #[tool(
        name = "memory_schema",
        description = "Declare a Class or Property (RDFS schema-as-data). Optional subClassOf, subPropertyOf, domain, range.",
        input_schema = schema_memory_schema()
    )]
    async fn memory_schema(
        &self,
        Parameters(args): Parameters<SchemaArgs>,
    ) -> Result<CallToolResult, McpError> {
        let graph = self.graph().await?;
        tools::memory_schema(graph, &self.project, args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }
}

#[tool_handler]
impl ServerHandler for Mindreader {
    fn get_info(&self) -> ServerInfo {
        ServerInfo {
            protocol_version: ProtocolVersion::V_2024_11_05,
            capabilities: ServerCapabilities::builder().enable_tools().build(),
            server_info: Implementation {
                name: "mindreader".into(),
                title: None,
                version: env!("CARGO_PKG_VERSION").into(),
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "Mindreader: RDFS schema-as-data memory over Neo4j. Tools: memory_get, memory_search, memory_traverse, memory_assert, memory_retract, memory_schema. project_id is env-only (MINDREADER_PROJECT). No raw Cypher."
                    .into(),
            ),
        }
    }

    async fn initialize(
        &self,
        request: InitializeRequestParam,
        context: RequestContext<RoleServer>,
    ) -> Result<ServerInfo, McpError> {
        let requested = request.protocol_version.clone();
        if context.peer.peer_info().is_none() {
            context.peer.set_peer_info(request);
        }
        let mut info = self.get_info();
        // Echo a protocol the client asked for so hosts on 2025-03-26 / 2025-06-18
        // do not treat a pinned 2024-11-05 reply as "connected, no tools".
        info.protocol_version = requested;
        Ok(info)
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

    #[test]
    fn assert_schema_has_contradicts() {
        let router = Mindreader::tool_router();
        let tools: Vec<_> = router.list_all();
        let assert = tools.iter().find(|t| t.name == "memory_assert").unwrap();
        let schema = assert.schema_as_json_value();
        let props = schema.get("properties").and_then(|p| p.as_object()).unwrap();
        assert!(props.contains_key("contradicts"), "contradicts missing: {schema}");
        assert_eq!(
            props["contradicts"].get("type").and_then(|v| v.as_str()),
            Some("boolean")
        );
        let required = schema
            .get("required")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(required.iter().any(|v| v.as_str() == Some("s")));
        assert!(required.iter().any(|v| v.as_str() == Some("p")));
        assert!(required.iter().any(|v| v.as_str() == Some("o")));
        assert!(!required.iter().any(|v| v.as_str() == Some("contradicts")));

        // Schema catalog / memory_schema list is NOT in this pass.
    }

    #[test]
    fn tool_input_schemas_are_objects() {
        let router = Mindreader::tool_router();
        for tool in router.list_all() {
            let schema = tool.schema_as_json_value();
            assert_eq!(
                schema.get("type").and_then(|v| v.as_str()),
                Some("object"),
                "tool {} inputSchema.type",
                tool.name
            );
            let props = schema
                .get("properties")
                .and_then(|p| p.as_object())
                .unwrap_or_else(|| panic!("tool {} missing properties", tool.name));
            for (k, v) in props {
                assert!(
                    v.is_object(),
                    "tool {} property {} must be an object schema, got {v}",
                    tool.name,
                    k
                );
            }
        }
    }
}
