use crate::config::Config;
use crate::graph;
use crate::merge::MergeArgs;
use crate::semantic::SemanticSearchArgs;
use crate::service::MemoryService;
use crate::tools::{
    self, AssertArgs, FeedbackArgs, GetArgs, LayersArgs, ReplaceArgs, RetractArgs, SchemaArgs,
    SearchArgs, StatsArgs, TraverseArgs,
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

fn layers_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "array",
        "description": "@layers visibility union. [] selects global/unlayered records only. Named records match any requested layer. IDs use lowercase kebab-case with colon namespaces, for example project:mindreader or analysis:hypothesis; colons are naming, not hierarchy.",
        "items": {
            "type": "string",
            "pattern": "^[a-z0-9]+(?:-[a-z0-9]+)*(?::[a-z0-9]+(?:-[a-z0-9]+)*)*$"
        }
    })
}

fn target_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "A stable node or relationship feedback/audit target returned by retrieval.",
        "properties": {
            "kind": { "type": "string", "enum": ["node", "relationship"] },
            "iri": { "type": "string", "minLength": 1 }
        },
        "required": ["kind", "iri"]
    })
}

fn schema_memory_get() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "iri": { "type": "string" },
            "layers": layers_schema(),
            "hops": { "type": "integer" }
        },
        "required": ["iri", "layers"]
    }))
}

fn schema_memory_search() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "text": { "type": "string" },
            "labels": { "type": "array", "items": { "type": "string" } },
            "limit": { "type": "integer" },
            "layers": layers_schema()
        },
        "required": ["layers"]
    }))
}

fn schema_memory_semantic_search() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "text": { "type": "string", "minLength": 1 },
            "layers": layers_schema(),
            "labels": { "type": "array", "items": { "type": "string" } },
            "limit": { "type": "integer", "minimum": 1, "maximum": 100 }
        },
        "required": ["text", "layers"]
    }))
}

fn schema_memory_merge() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "source": { "type": "string", "minLength": 1 },
            "target": { "type": "string", "minLength": 1 }
        },
        "required": ["source", "target"]
    }))
}

fn schema_memory_traverse() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "from": { "type": "string" },
            "layers": layers_schema(),
            "rels": { "type": "array", "items": { "type": "string" } },
            "depth": { "type": "integer" },
            "limit": { "type": "integer" }
        },
        "required": ["from", "layers"]
    }))
}

fn schema_memory_stats() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": { "layers": layers_schema() },
        "required": ["layers"]
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
            "layers": layers_schema(),
            "spike": {
                "type": "string",
                "enum": ["Signal", "Pattern", "Insight", "Knowledge"]
            },
            "contradicts": { "type": "boolean" }
        },
        "required": ["s", "p", "o", "layers"]
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
            "layers": layers_schema(),
            "spike": {
                "type": "string",
                "enum": ["Signal", "Pattern", "Insight", "Knowledge"]
            },
            "contradicts": { "type": "boolean" },
            "reason": { "type": "string" }
        },
        "required": ["s", "p", "old", "new", "layers"]
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
            "layers": layers_schema(),
            "reason": { "type": "string" }
        },
        "required": ["target", "layers"]
    }))
}

fn schema_memory_feedback() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "layers": layers_schema(),
            "target": target_schema(),
            "mode": { "type": "string", "enum": ["strengthen", "weaken"] }
        },
        "required": ["layers", "target", "mode"]
    }))
}

fn schema_memory_layers() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "layers": layers_schema(),
            "target": target_schema(),
            "add": { "type": "array", "items": layers_schema()["items"].clone() },
            "remove": { "type": "array", "items": layers_schema()["items"].clone() }
        },
        "required": ["layers", "target"]
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
    cfg: Config,
}

impl Mindreader {
    fn from_config(cfg: Config) -> Self {
        Self {
            tool_router: Self::tool_router(),
            service: Arc::new(OnceCell::new()),
            cfg,
        }
    }

    /// Build the server without talking to Neo4j so MCP initialize/list_tools
    /// can run immediately.
    pub fn from_env() -> anyhow::Result<Self> {
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
                let embedding_space = self.cfg.embedding.as_ref().map(|value| value.space());
                graph::bootstrap(&g, embedding_space.as_ref()).await?;
                MemoryService::new(g, &self.cfg)
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
        description = "Use when you already have an IRI and need that visible node. hops=0 returns it; hops=1 adds current visible neighbors. @layers: [] is global-only; multiple lowercase colon-namespaced layers are an OR union. Every returned edge and endpoint must be visible.",
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
        description = "Use first to recover facts and stable feedback targets. @layers: [] is global-only; multiple lowercase colon-namespaced layers form an OR union. Ranking is Knowledge > Insight > Pattern > Signal, then subject + relationship + object weight, then text relevance.",
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
        name = "memory_semantic_search",
        description = "Embed a query, combine current direct matches with nearby remembered result bundles, and return current visible facts with a 1-based rank. The requested @layers and labels filter direct and recalled facts identically.",
        input_schema = schema_memory_semantic_search()
    )]
    async fn memory_semantic_search(
        &self,
        Parameters(args): Parameters<SemanticSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service
            .semantic_search(args)
            .await
            .map_err(map_err)
            .and_then(ok)
    }

    #[tool(
        name = "memory_merge",
        description = "Permanently merge a source into a target of the same canonical kind across all memberships and history. The target IRI and name survive. Use advisory mergeSuggestions for direction, but review them first and reverse them when appropriate.",
        input_schema = schema_memory_merge()
    )]
    async fn memory_merge(
        &self,
        Parameters(args): Parameters<MergeArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service.merge(args).await.map_err(map_err).and_then(ok)
    }

    #[tool(
        name = "memory_traverse",
        description = "Walk typed edges from a visible IRI, depth capped at 3. @layers: [] is global-only; multiple lowercase colon-namespaced layers form an OR union. Every path relationship and endpoint is filtered by the same scope.",
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
        description = "Return model readiness and graph counters under the requested visibility union. @layers: [] is global-only; multiple lowercase colon-namespaced layers are ORed.",
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
        description = "Add one exact set-valued triple. layers are memberships inherited by the relationship and endpoints; [] makes the fact global, while repeated assertions merge named memberships and an existing global fact stays global. IDs use lowercase kebab-case colon namespaces. Exact unchanged reassertions are no-ops.",
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
        description = "Correct one exact fact in the listed memberships. Named layers move only those memberships from old to new; [] replaces a global fact. The old relationship retires when its last named membership is removed, and SUPERSEDES is recorded atomically.",
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
        description = "Withdraw selected fact memberships without deleting nodes or history. Named layers remove only those memberships and retire an edge after its last one; [] retracts global facts only. Use memory_replace for corrections.",
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
        name = "memory_feedback",
        description = "After using a retrieved node or relationship, explicitly strengthen (+1) or weaken (-1) its shared signed weight. Feedback may happen many turns later. The stable target must still be current and visible in @layers; retrieval never changes weight automatically and there is no time decay.",
        input_schema = schema_memory_feedback()
    )]
    async fn memory_feedback(
        &self,
        Parameters(args): Parameters<FeedbackArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service.feedback(args).await.map_err(map_err).and_then(ok)
    }

    #[tool(
        name = "memory_layers",
        description = "Audit one visible node or current relationship membership with atomic add/remove arrays. Empty membership means global. This tool changes only the target, never propagates, and rejects a final state that would expose a relationship without both endpoints.",
        input_schema = schema_memory_layers()
    )]
    async fn memory_layers(
        &self,
        Parameters(args): Parameters<LayersArgs>,
    ) -> Result<CallToolResult, McpError> {
        let service = self.service().await?;
        service.layers(args).await.map_err(map_err).and_then(ok)
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
                "Mindreader: RDFS schema-as-data memory over Neo4j. Twelve tools provide scoped direct and semantic retrieval, fact writes, advisory entity deduplication, explicit merging, feedback, layer auditing, and global schema. @layers uses [] for global-only and lowercase kebab-case colon namespaces for named OR-union visibility. No raw Cypher."
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
    fn registers_twelve_tools() {
        let names = Mindreader::registered_tool_names();
        let expected = [
            "memory_assert",
            "memory_feedback",
            "memory_get",
            "memory_layers",
            "memory_merge",
            "memory_replace",
            "memory_retract",
            "memory_schema",
            "memory_search",
            "memory_semantic_search",
            "memory_stats",
            "memory_traverse",
        ];
        assert_eq!(names, expected);
        let router = Mindreader::tool_router();
        for name in expected {
            assert!(router.has_route(name), "missing route {name}");
        }
        assert_eq!(router.map.len(), 12);
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
        assert!(required.iter().any(|v| v.as_str() == Some("layers")));
        assert!(!required.iter().any(|v| v.as_str() == Some("contradicts")));

        let replace_schema = tools
            .iter()
            .find(|tool| tool.name == "memory_replace")
            .unwrap()
            .schema_as_json_value();
        let replace_required = replace_schema["required"].as_array().unwrap();
        for name in ["s", "p", "old", "new", "layers"] {
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

        let feedback_schema = tools
            .iter()
            .find(|tool| tool.name == "memory_feedback")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(
            feedback_schema["properties"]["mode"]["enum"],
            serde_json::json!(["strengthen", "weaken"])
        );
        assert_eq!(
            feedback_schema["properties"]["target"]["properties"]["kind"]["enum"],
            serde_json::json!(["node", "relationship"])
        );
    }

    #[test]
    fn scoped_tools_require_layers() {
        let router = Mindreader::tool_router();
        for tool in router.list_all() {
            let schema = tool.schema_as_json_value();
            let required = schema["required"].as_array().cloned().unwrap_or_default();
            if tool.name == "memory_schema" || tool.name == "memory_merge" {
                assert!(!required.iter().any(|value| value == "layers"));
            } else {
                assert!(
                    required.iter().any(|value| value == "layers"),
                    "{} must require layers",
                    tool.name
                );
            }
        }
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
