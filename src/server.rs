use crate::config::Config;
use crate::domain::ProjectId;
use crate::graph;
use crate::service::MemoryService;
use crate::tools::{
    self, AssertArgs, GetArgs, ReplaceArgs, RetractArgs, SchemaArgs, SearchArgs, StatsArgs,
    TraverseArgs,
};
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

fn schema_memory_stats() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {}
    }))
}

fn entity_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "An entity reference. Runtime validation requires at least one of iri or name.",
        "properties": {
            "kind": { "type": "string", "enum": ["entity"] },
            "iri": { "type": "string", "minLength": 1 },
            "name": { "type": "string", "minLength": 1 },
            "labels": { "type": "array", "items": { "type": "string", "pattern": "^[A-Za-z][A-Za-z0-9_]*$" } }
        },
        "required": ["kind"]
    })
}

fn object_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "A tagged entity or literal. Entity values use iri/name/labels; literal values use value and optional datatype.",
        "properties": {
            "kind": { "type": "string", "enum": ["entity", "literal"] },
            "iri": { "type": "string", "minLength": 1 },
            "name": { "type": "string", "minLength": 1 },
            "labels": { "type": "array", "items": { "type": "string", "pattern": "^[A-Za-z][A-Za-z0-9_]*$" } },
            "value": { "type": "string" },
            "datatype": { "type": "string", "minLength": 1, "default": "xsd:string" }
        },
        "required": ["kind"]
    })
}

fn schema_memory_assert() -> Arc<rmcp::model::JsonObject> {
    // Host wrappers drop the whole server if any inputSchema uses anyOf.
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "s": entity_input_schema(),
            "p": { "type": "string", "minLength": 1 },
            "o": object_input_schema(),
            "layer": { "type": "string" },
            "spike": {
                "type": "string",
                "enum": ["Signal", "Pattern", "Insight", "Knowledge"]
            },
            "contradicts": { "type": "boolean" }
        },
        "required": ["s", "p", "o"]
    }))
}

fn schema_memory_replace() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "s": entity_input_schema(),
            "p": { "type": "string", "minLength": 1 },
            "old": object_input_schema(),
            "new": object_input_schema(),
            "layer": { "type": "string" },
            "spike": {
                "type": "string",
                "enum": ["Signal", "Pattern", "Insight", "Knowledge"]
            },
            "contradicts": { "type": "boolean" },
            "reason": { "type": "string" }
        },
        "required": ["s", "p", "old", "new"]
    }))
}

fn schema_memory_retract() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "target": {
                "type": "object",
                "description": "A tagged retract scope. fact requires p and o; predicate requires p; subject uses only s.",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["fact", "predicate", "subject"]
                    },
                    "s": entity_input_schema(),
                    "p": { "type": "string", "minLength": 1 },
                    "o": object_input_schema()
                },
                "required": ["kind", "s"]
            },
            "layer": { "type": "string" },
            "reason": { "type": "string" }
        },
        "required": ["target"]
    }))
}

fn schema_memory_schema() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "kind": { "type": "string", "enum": ["class", "property"] },
            "name": { "type": "string", "minLength": 1 },
            "iri": { "type": "string", "minLength": 1 },
            "subClassOf": { "type": "string", "minLength": 1 },
            "subPropertyOf": { "type": "string", "minLength": 1 },
            "domain": { "type": "string", "minLength": 1 },
            "range": { "type": "string", "minLength": 1 }
        },
        "required": ["kind"]
    }))
}

#[derive(Clone)]
pub struct Mindreader {
    pub tool_router: ToolRouter<Self>,
    service: Arc<OnceCell<MemoryService>>,
    pub project: String,
    cfg: Config,
}

impl Mindreader {
    fn from_config(cfg: Config) -> Self {
        Self {
            tool_router: Self::tool_router(),
            service: Arc::new(OnceCell::new()),
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
        self.service
            .get_or_try_init(|| async {
                let g = graph::connect(&self.cfg).await?;
                graph::bootstrap(&g).await?;
                let project = ProjectId::parse(self.cfg.project.clone())?;
                Ok::<_, anyhow::Error>(MemoryService::new(g, project))
            })
            .await?;
        Ok(())
    }

    async fn service(&self) -> Result<&MemoryService, McpError> {
        self.ensure_connected().await.map_err(map_err)?;
        self.service
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
        description = "Use when you already have an IRI and need that node. hops=0 is the node; hops=1 adds current visible neighbors. Do not use this to discover unknown things — that is memory_search. Do not use hops to walk the graph — that is memory_traverse.",
        input_schema = schema_memory_get()
    )]
    async fn memory_get(
        &self,
        Parameters(args): Parameters<GetArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service.get(args).await.map_err(map_err).and_then(ok)
    }

    #[tool(
        name = "memory_search",
        description = "Use this first when you do not already have an IRI and need what we currently know about a person, thing, or topic. Returns current layer-visible facts (s,p,o) plus ABOUT SPIKE, ranked Knowledge > Insight > Pattern > Signal. Not a node directory and not a dump of the graph. Skip this if you already have the IRI — use memory_get.",
        input_schema = schema_memory_search()
    )]
    async fn memory_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service.search(args).await.map_err(map_err).and_then(ok)
    }

    #[tool(
        name = "memory_traverse",
        description = "Use after search or get, when you have a starting IRI and need to walk typed edges (ABOUT, ASSERTS, DERIVED_FROM, CONTRADICTS, SUPERSEDES, INSTANCE_OF, and the other fixed rels). Depth is hard-capped at 3. Not for keyword lookup and not for writing.",
        input_schema = schema_memory_traverse()
    )]
    async fn memory_traverse(
        &self,
        Parameters(args): Parameters<TraverseArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service.traverse(args).await.map_err(map_err).and_then(ok)
    }

    #[tool(
        name = "memory_stats",
        description = "Operational graph-model readiness plus counters for nodes, active/historical edges, episodes, and per-layer active edge totals visible to this project.",
        input_schema = schema_memory_stats()
    )]
    async fn memory_stats(
        &self,
        Parameters(args): Parameters<StatsArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service.stats(args).await.map_err(map_err).and_then(ok)
    }

    #[tool(
        name = "memory_assert",
        description = "Use to add one exact fact as a triple (s, p, o). Facts are set-valued: another current object for the same subject and property remains current. Reasserting the exact current triple is a no-op. Optional spike labels this as Signal, Pattern, Insight, or Knowledge ABOUT an Element. Optional contradicts=true records a fight with another visible layer's current (s,p). CONTRADICTS and SUPERSEDES are system-owned. Encode a triple — do not dump prose or markdown.",
        input_schema = schema_memory_assert()
    )]
    async fn memory_assert(
        &self,
        Parameters(args): Parameters<AssertArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service.assert(args).await.map_err(map_err).and_then(ok)
    }

    #[tool(
        name = "memory_replace",
        description = "Use to correct one exact current fact. old must identify a current object; only that triple is closed, new is added if needed, unrelated current objects remain, and SUPERSEDES history is recorded atomically. Use memory_assert when adding another valid value instead.",
        input_schema = schema_memory_replace()
    )]
    async fn memory_replace(
        &self,
        Parameters(args): Parameters<ReplaceArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service.replace(args).await.map_err(map_err).and_then(ok)
    }

    #[tool(
        name = "memory_retract",
        description = "Use to withdraw current facts you no longer stand behind. Retraction is soft (validTo); nodes and system-owned history are preserved. target.kind=fact closes one exact triple, predicate intentionally closes all objects for one subject/property, and subject intentionally closes retractable outgoing facts for one subject. Omit layer to use this project's write layer. Use memory_replace for corrections.",
        input_schema = schema_memory_retract()
    )]
    async fn memory_retract(
        &self,
        Parameters(args): Parameters<RetractArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service.retract(args).await.map_err(map_err).and_then(ok)
    }

    #[tool(
        name = "memory_schema",
        description = "Use before asserting a new kind of thing or relation that is not already in the graph. Declares a Class or Property as RDFS schema-as-data (writes global). Optional subClassOf, subPropertyOf, domain, range. Do not mint a one-off property if search already shows a similar one.",
        input_schema = schema_memory_schema()
    )]
    async fn memory_schema(
        &self,
        Parameters(args): Parameters<SchemaArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service
            .declare_schema(args)
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
            capabilities: ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build(),
            server_info: Implementation {
                name: "mindreader".into(),
                title: None,
                version: env!("CARGO_PKG_VERSION").into(),
                icons: None,
                website_url: None,
            },
            instructions: Some(
                "Mindreader: RDFS schema-as-data memory over Neo4j. Tools: memory_get, memory_search, memory_traverse, memory_stats, memory_assert, memory_replace, memory_retract, memory_schema. project_id is env-only (MINDREADER_PROJECT). No raw Cypher."
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
    use serde_json::Value;

    #[test]
    fn registers_eight_tools() {
        let names = Mindreader::registered_tool_names();
        let expected = [
            "memory_assert",
            "memory_get",
            "memory_replace",
            "memory_retract",
            "memory_schema",
            "memory_search",
            "memory_stats",
            "memory_traverse",
        ];
        assert_eq!(names, expected);
        let router = Mindreader::tool_router();
        for name in expected {
            assert!(router.has_route(name), "missing route {name}");
        }
        assert_eq!(router.map.len(), 8);
    }

    #[test]
    fn mutation_schemas_advertise_tagged_inputs() {
        let router = Mindreader::tool_router();
        let tools: Vec<_> = router.list_all();
        let assert_schema = tools
            .iter()
            .find(|tool| tool.name == "memory_assert")
            .unwrap()
            .schema_as_json_value();
        let assert_props = assert_schema
            .get("properties")
            .and_then(|p| p.as_object())
            .unwrap();
        assert_eq!(
            assert_props["s"].get("type").and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(
            assert_props["o"].get("type").and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(
            assert_props["contradicts"]
                .get("type")
                .and_then(Value::as_str),
            Some("boolean")
        );
        assert_eq!(
            assert_props["s"]["properties"]["kind"]["enum"],
            serde_json::json!(["entity"])
        );
        assert_eq!(
            assert_props["o"]["properties"]["kind"]["enum"],
            serde_json::json!(["entity", "literal"])
        );
        assert_eq!(assert_props["p"]["minLength"], 1);
        assert_eq!(
            assert_props["s"]["properties"]["labels"]["items"]["pattern"],
            "^[A-Za-z][A-Za-z0-9_]*$"
        );
        let required = assert_schema
            .get("required")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        assert!(required.iter().any(|v| v.as_str() == Some("s")));
        assert!(required.iter().any(|v| v.as_str() == Some("p")));
        assert!(required.iter().any(|v| v.as_str() == Some("o")));
        assert!(!required.iter().any(|v| v.as_str() == Some("contradicts")));

        let replace_schema = tools
            .iter()
            .find(|tool| tool.name == "memory_replace")
            .unwrap()
            .schema_as_json_value();
        let replace_required = replace_schema["required"].as_array().unwrap();
        for name in ["s", "p", "old", "new"] {
            assert!(replace_required.iter().any(|value| value == name));
        }
        assert_eq!(replace_schema["properties"]["old"]["type"], "object");
        assert_eq!(replace_schema["properties"]["new"]["type"], "object");
        assert_eq!(replace_schema["properties"]["p"]["minLength"], 1);

        let retract_schema = tools
            .iter()
            .find(|tool| tool.name == "memory_retract")
            .unwrap()
            .schema_as_json_value();
        let target = &retract_schema["properties"]["target"];
        assert_eq!(target["type"], "object");
        assert_eq!(
            target["properties"]["kind"]["enum"],
            serde_json::json!(["fact", "predicate", "subject"])
        );
        assert_eq!(target["properties"]["s"]["type"], "object");
        assert_eq!(target["properties"]["o"]["type"], "object");
        assert_eq!(target["properties"]["p"]["minLength"], 1);
    }

    #[test]
    fn tool_input_schemas_are_objects() {
        fn assert_no_union_keywords(value: &Value, tool: &str) {
            match value {
                Value::Object(map) => {
                    assert!(
                        !map.contains_key("anyOf"),
                        "tool {tool} contains anyOf: {value}"
                    );
                    assert!(
                        !map.contains_key("oneOf"),
                        "tool {tool} contains oneOf: {value}"
                    );
                    for child in map.values() {
                        assert_no_union_keywords(child, tool);
                    }
                }
                Value::Array(values) => {
                    for child in values {
                        assert_no_union_keywords(child, tool);
                    }
                }
                _ => {}
            }
        }

        let router = Mindreader::tool_router();
        for tool in router.list_all() {
            let schema = tool.schema_as_json_value();
            assert_no_union_keywords(&schema, &tool.name);
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
