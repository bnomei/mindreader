//! MCP stdio adapter: tool registration, host-compatible schemas, lazy Neo4j.
//!
//! Advertises all twelve memory tools with plain tagged object input and output
//! schemas (no `anyOf`/`oneOf`/`allOf`, which break some hosts). Initialize and
//! `tools/list` do not require Neo4j; the first tool call (or explicit connect)
//! bootstraps the graph and [`MemoryService`]. Recoverable tool failures return
//! structured `isError` results. Rate limiting and the 45s invoke timeout apply
//! only to MCP handlers.

use crate::config::Config;
use crate::domain::DomainError;
use crate::error::{Error, Result as AppResult};
use crate::graph;
use crate::merge::MergeArgs;
use crate::search::SearchArgs;
use crate::semantic::{SemanticSearchArgs, MAX_SEMANTIC_TEXT_BYTES};
use crate::service::MemoryService;
use crate::tools::{
    AssertArgs, FeedbackArgs, GetArgs, LayersArgs, ReplaceArgs, RetractArgs, SchemaArgs, StatsArgs,
    TraverseArgs,
};
use neo4rs::Error as Neo4jError;
use rmcp::{
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::{
        CallToolResult, ErrorData as McpError, Implementation, ProtocolVersion, ServerCapabilities,
        ServerInfo,
    },
    tool, tool_handler, tool_router, ServerHandler,
};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::error::Error as StdError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;
use tokio::time::timeout;

const INVOKE_TIMEOUT: Duration = Duration::from_secs(45);
const RATE_LIMIT_PER_MINUTE: f64 = 120.0;
const RATE_LIMIT_BURST: f64 = 20.0;

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
            "hops": { "type": "integer", "enum": [0, 1], "minimum": 0, "maximum": 1 }
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
            "limit": { "type": "integer", "minimum": 1, "maximum": 100 },
            "layers": layers_schema()
        },
        "required": ["layers"]
    }))
}

fn schema_memory_semantic_search() -> Arc<rmcp::model::JsonObject> {
    object_schema(serde_json::json!({
        "type": "object",
        "properties": {
            "text": {
                "type": "string",
                "minLength": 1,
                "description": format!("Query text; runtime limit is {MAX_SEMANTIC_TEXT_BYTES} UTF-8 bytes.")
            },
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
            "depth": { "type": "integer", "minimum": 1, "maximum": 3 },
            "limit": { "type": "integer", "minimum": 1, "maximum": 200 }
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
        "additionalProperties": false,
        "properties": {
            "facts": {
                "type": "array",
                "minItems": 1,
                "maxItems": 20,
                "items": {
                    "type": "object",
                    "properties": {
                        "s": entity_input_schema(),
                        "p": { "type": "string", "minLength": 1 },
                        "o": object_input_schema(),
                        "spike": {
                            "type": "string",
                            "enum": ["Signal", "Pattern", "Insight", "Knowledge"]
                        },
                        "contradicts": { "type": "boolean" }
                    },
                    "required": ["s", "p", "o"]
                }
            },
            "layers": layers_schema()
        },
        "required": ["facts", "layers"]
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
            "list": { "type": "boolean" },
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

fn error_envelope_properties() -> serde_json::Map<String, Value> {
    let Value::Object(map) = json!({
        "ok": { "type": "boolean" },
        "reason": { "type": "string" },
        "message": { "type": "string" },
        "episode": {}
    }) else {
        unreachable!("literal object")
    };
    map
}

fn schema_out(mut properties: serde_json::Map<String, Value>) -> Arc<rmcp::model::JsonObject> {
    for (key, value) in error_envelope_properties() {
        properties.entry(key).or_insert(value);
    }
    object_schema(json!({
        "type": "object",
        "properties": properties
    }))
}

fn props(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    }
}

fn schema_out_memory_get() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "found": { "type": "boolean" },
        "iri": { "type": "string" },
        "node": { "type": "object" },
        "neighbors": { "type": "array" },
        "hops": { "type": "integer" },
        "layers": { "type": "array" }
    })))
}

fn schema_out_memory_search() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "facts": { "type": "array" },
        "spike": { "type": "array" },
        "layers": { "type": "array" }
    })))
}

fn schema_out_memory_semantic_search() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "facts": { "type": "array" },
        "spike": { "type": "array" },
        "layers": { "type": "array" }
    })))
}

fn schema_out_memory_merge() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "node": { "type": "object" }
    })))
}

fn schema_out_memory_traverse() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "from": { "type": "string" },
        "layers": { "type": "array" },
        "rels": { "type": "array" },
        "paths": { "type": "array" },
        "nodes": { "type": "array" },
        "edges": { "type": "array" },
        "depth": { "type": "integer" }
    })))
}

fn schema_out_memory_stats() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "layers": { "type": "array" },
        "ready": { "type": "boolean" }
    })))
}

fn schema_out_memory_assert() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "noop": { "type": "boolean" },
        "layers": { "type": "array" },
        "mergeSuggestions": { "type": "array" },
        "facts": { "type": "array" }
    })))
}

fn schema_out_memory_replace() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "noop": { "type": "boolean" },
        "layers": { "type": "array" },
        "mergeSuggestions": { "type": "array" }
    })))
}

fn schema_out_memory_retract() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "retracted": { "type": "integer" },
        "layers": { "type": "array" }
    })))
}

fn schema_out_memory_feedback() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "weight": { "type": "integer" },
        "target": { "type": "object" }
    })))
}

fn schema_out_memory_layers() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "noop": { "type": "boolean" },
        "target": { "type": "object" },
        "before": { "type": "array" },
        "layers": { "type": "array" }
    })))
}

fn schema_out_memory_schema() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "list": { "type": "boolean" },
        "kind": { "type": "string" },
        "items": { "type": "array" },
        "noop": { "type": "boolean" },
        "node": { "type": "object" },
        "links": { "type": "array" },
        "mergeSuggestions": { "type": "array" }
    })))
}

struct TokenBucket {
    inner: Mutex<TokenBucketState>,
}

struct TokenBucketState {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    fn new() -> Self {
        Self {
            inner: Mutex::new(TokenBucketState {
                tokens: RATE_LIMIT_BURST,
                last: Instant::now(),
            }),
        }
    }

    fn try_acquire(&self) -> bool {
        let mut state = self
            .inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let now = Instant::now();
        let elapsed = now.saturating_duration_since(state.last).as_secs_f64();
        state.tokens =
            (state.tokens + elapsed * (RATE_LIMIT_PER_MINUTE / 60.0)).min(RATE_LIMIT_BURST);
        state.last = now;
        if state.tokens >= 1.0 {
            state.tokens -= 1.0;
            true
        } else {
            false
        }
    }
}

/// MCP server handle: tool router, lazy `MemoryService`, and loaded config.
#[derive(Clone)]
pub struct Mindreader {
    pub tool_router: ToolRouter<Self>,
    service: Arc<OnceCell<MemoryService>>,
    cfg: Config,
    limiter: Arc<TokenBucket>,
}

impl Mindreader {
    fn from_config(cfg: Config) -> Self {
        Self {
            tool_router: Self::tool_router(),
            service: Arc::new(OnceCell::new()),
            cfg,
            limiter: Arc::new(TokenBucket::new()),
        }
    }

    /// Build the server without talking to Neo4j so MCP initialize/list_tools
    /// can run immediately.
    pub fn from_env() -> AppResult<Self> {
        Ok(Self::from_config(Config::from_env()?))
    }

    /// Eager connect (tests / smoke). Prefer `from_env` + serve for MCP.
    pub async fn connect() -> AppResult<Self> {
        let this = Self::from_env()?;
        this.ensure_connected().await?;
        Ok(this)
    }

    /// Connect, bootstrap the model, and initialize [`MemoryService`] once.
    pub async fn ensure_connected(&self) -> AppResult<()> {
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

    /// Sorted list of registered MCP tool names (for startup logs and tests).
    pub fn registered_tool_names() -> Vec<String> {
        let router = Self::tool_router();
        let mut names: Vec<String> = router.map.keys().map(|k| k.to_string()).collect();
        names.sort();
        names
    }

    async fn invoke<F, Fut>(&self, op: F) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(MemoryService) -> Fut,
        Fut: std::future::Future<Output = AppResult<Value>>,
    {
        if !self.limiter.try_acquire() {
            return Ok(structured_error(
                "rate_limited",
                "MCP invoke rate limit exceeded (120/min, burst 20)",
            ));
        }
        if let Err(error) = self.ensure_connected().await {
            return Ok(map_connect_error(error));
        }
        let Some(service) = self.service.get() else {
            return Ok(structured_error(
                "connect_failed",
                "neo4j not connected after bootstrap",
            ));
        };
        let service = service.clone();
        match timeout(INVOKE_TIMEOUT, op(service)).await {
            Ok(result) => Ok(map_tool_result(result)),
            Err(_) => Ok(structured_error(
                "timeout",
                "tool invoke exceeded 45 seconds",
            )),
        }
    }
}

fn structured_error(reason: &str, message: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "ok": false,
        "reason": reason,
        "message": message.to_string(),
    }))
}

fn map_connect_error(error: Error) -> CallToolResult {
    structured_error("connect_failed", error)
}

fn map_tool_result(result: AppResult<Value>) -> CallToolResult {
    match result {
        Ok(value) => CallToolResult::structured(value),
        Err(error) => structured_error(classify_tool_error(&error), error),
    }
}

fn classify_tool_error(error: &Error) -> &'static str {
    match error {
        Error::Domain(DomainError::InvalidInput(_)) => "invalid_input",
        Error::Domain(DomainError::Precondition(_)) => "precondition_failed",
        Error::ConcurrentMutation(_) => "concurrent_mutation",
        Error::Embedding(_) => "missing_embedding",
        Error::EmbeddingHttp { status: 429, .. } => "rate_limited",
        Error::EmbeddingHttp { .. } => "embedding_http",
        Error::Neo4j(_)
        | Error::Neo4jDecode(_)
        | Error::Graph(_)
        | Error::Io(_)
        | Error::Json(_) => "operation",
        Error::Context { source, .. } => classify_boxed_source(source.as_ref()),
        _ => "operation",
    }
}

fn classify_boxed_source(source: &(dyn StdError + Send + Sync + 'static)) -> &'static str {
    if let Some(error) = source.downcast_ref::<Error>() {
        return classify_tool_error(error);
    }
    if let Some(domain) = source.downcast_ref::<DomainError>() {
        return match domain {
            DomainError::InvalidInput(_) => "invalid_input",
            DomainError::Precondition(_) => "precondition_failed",
        };
    }
    if source.downcast_ref::<Neo4jError>().is_some() {
        return "operation";
    }
    if let Some(cause) = source.source() {
        if let Some(error) = cause.downcast_ref::<Error>() {
            return classify_tool_error(error);
        }
        if let Some(domain) = cause.downcast_ref::<DomainError>() {
            return match domain {
                DomainError::InvalidInput(_) => "invalid_input",
                DomainError::Precondition(_) => "precondition_failed",
            };
        }
        if cause.downcast_ref::<Neo4jError>().is_some() {
            return "operation";
        }
    }
    "operation"
}

#[tool_router]
impl Mindreader {
    #[tool(
        name = "memory_get",
        description = "Use when you already have an IRI and need that visible node. hops=0 returns it; hops=1 adds current visible neighbors. @layers: [] is global-only; multiple lowercase colon-namespaced layers are an OR union. Every returned edge and endpoint must be visible.",
        input_schema = schema_memory_get(),
        output_schema = schema_out_memory_get(),
        annotations(title = "Get a visible node", read_only_hint = true, open_world_hint = false)
    )]
    async fn memory_get(
        &self,
        Parameters(args): Parameters<GetArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.get(args).await })
            .await
    }

    #[tool(
        name = "memory_search",
        description = "Use first to recover facts and stable feedback targets. @layers: [] is global-only; multiple lowercase colon-namespaced layers form an OR union. Ranking is Knowledge > Insight > Pattern > Signal, then subject + relationship + object weight, then text relevance.",
        input_schema = schema_memory_search(),
        output_schema = schema_out_memory_search(),
        annotations(title = "Search current facts", read_only_hint = true, open_world_hint = false)
    )]
    async fn memory_search(
        &self,
        Parameters(args): Parameters<SearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.search(args).await })
            .await
    }

    #[tool(
        name = "memory_semantic_search",
        description = "Use when conceptual recall is needed and sending the query text to the configured embedding API is acceptable. Embeds the query, combines current direct matches with nearby remembered result bundles, and writes expiring activations. The requested @layers and labels filter direct and recalled facts identically.",
        input_schema = schema_memory_semantic_search(),
        output_schema = schema_out_memory_semantic_search(),
        annotations(title = "Semantic recall", read_only_hint = false, open_world_hint = true)
    )]
    async fn memory_semantic_search(
        &self,
        Parameters(args): Parameters<SemanticSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.semantic_search(args).await })
            .await
    }

    #[tool(
        name = "memory_merge",
        description = "Use after reviewing mergeSuggestions when two same-kind entities are truly identical. Permanently merge a source into a target across all memberships and history. The target IRI and name survive. Reverse the suggested direction when the other IRI should survive.",
        input_schema = schema_memory_merge(),
        output_schema = schema_out_memory_merge(),
        annotations(title = "Permanently merge entities", destructive_hint = true)
    )]
    async fn memory_merge(
        &self,
        Parameters(args): Parameters<MergeArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.merge(args).await })
            .await
    }

    #[tool(
        name = "memory_traverse",
        description = "Use when you have a visible IRI and need typed paths beyond one hop, depth capped at 3. @layers: [] is global-only; multiple lowercase colon-namespaced layers form an OR union. Every path relationship and endpoint is filtered by the same scope.",
        input_schema = schema_memory_traverse(),
        output_schema = schema_out_memory_traverse(),
        annotations(title = "Walk typed edges", read_only_hint = true, open_world_hint = false)
    )]
    async fn memory_traverse(
        &self,
        Parameters(args): Parameters<TraverseArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.traverse(args).await })
            .await
    }

    #[tool(
        name = "memory_stats",
        description = "Use when you need model readiness and graph counters under the requested visibility union. @layers: [] is global-only; multiple lowercase colon-namespaced layers are ORed.",
        input_schema = schema_memory_stats(),
        output_schema = schema_out_memory_stats(),
        annotations(title = "Report graph counters", read_only_hint = true, open_world_hint = false)
    )]
    async fn memory_stats(
        &self,
        Parameters(args): Parameters<StatsArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.stats(args).await })
            .await
    }

    #[tool(
        name = "memory_assert",
        description = "Use when adding one or more durable triples after search. Pass facts[] (1–20 items) and call-level layers. layers are memberships inherited by each relationship and its endpoints; [] makes facts global, while repeated assertions merge named memberships and an existing global fact stays global. Exact unchanged reassertions are no-ops. Review mergeSuggestions before merging.",
        input_schema = schema_memory_assert(),
        output_schema = schema_out_memory_assert(),
        annotations(title = "Assert facts", destructive_hint = false, idempotent_hint = true)
    )]
    async fn memory_assert(
        &self,
        Parameters(args): Parameters<AssertArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.assert(args).await })
            .await
    }

    #[tool(
        name = "memory_replace",
        description = "Use to correct one exact fact instead of reasserting. Named layers move only those memberships from old to new; [] replaces a global fact. The old relationship retires when its last named membership is removed, and SUPERSEDES is recorded atomically.",
        input_schema = schema_memory_replace(),
        output_schema = schema_out_memory_replace(),
        annotations(title = "Replace a fact", destructive_hint = true, idempotent_hint = false)
    )]
    async fn memory_replace(
        &self,
        Parameters(args): Parameters<ReplaceArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.replace(args).await })
            .await
    }

    #[tool(
        name = "memory_retract",
        description = "Use to withdraw selected fact memberships without deleting nodes or history. Named layers remove only those memberships and retire an edge after its last one; [] retracts global facts only. Use memory_replace for corrections.",
        input_schema = schema_memory_retract(),
        output_schema = schema_out_memory_retract(),
        annotations(title = "Retract memberships", destructive_hint = true, idempotent_hint = false)
    )]
    async fn memory_retract(
        &self,
        Parameters(args): Parameters<RetractArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.retract(args).await })
            .await
    }

    #[tool(
        name = "memory_feedback",
        description = "Use after a retrieved node or relationship helped or hurt. Explicitly strengthen (+1) or weaken (-1) its shared signed weight. The stable target must still be current and visible in @layers; retrieval never changes weight automatically and there is no time decay.",
        input_schema = schema_memory_feedback(),
        output_schema = schema_out_memory_feedback(),
        annotations(title = "Apply explicit feedback", destructive_hint = true, idempotent_hint = false)
    )]
    async fn memory_feedback(
        &self,
        Parameters(args): Parameters<FeedbackArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.feedback(args).await })
            .await
    }

    #[tool(
        name = "memory_layers",
        description = "Use to audit one visible node or current relationship membership with atomic add/remove arrays. Empty membership means global. This tool changes only the target, never propagates, and rejects a final state that would expose a relationship without both endpoints.",
        input_schema = schema_memory_layers(),
        output_schema = schema_out_memory_layers(),
        annotations(title = "Audit layer memberships", destructive_hint = true, idempotent_hint = false)
    )]
    async fn memory_layers(
        &self,
        Parameters(args): Parameters<LayersArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.layers(args).await })
            .await
    }

    #[tool(
        name = "memory_schema",
        description = "Use list=true to catalog existing Class or Property records (no Episode). Use without list to declare a missing Class or Property as global RDFS schema-as-data. Optional subClassOf, subPropertyOf, domain, range. Do not mint a one-off property if search or the catalog already shows a similar one.",
        input_schema = schema_memory_schema(),
        output_schema = schema_out_memory_schema(),
        annotations(title = "List or declare schema", destructive_hint = false, idempotent_hint = true)
    )]
    async fn memory_schema(
        &self,
        Parameters(args): Parameters<SchemaArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.declare_schema(args).await })
            .await
    }
}

#[tool_handler]
impl ServerHandler for Mindreader {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2025_11_25)
            .with_server_info(Implementation::new(
                "mindreader",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "Choose @layers as an OR-union ([] is global-only). Search first to recover facts and targets. If a Class or Property is missing, call memory_schema with list=true. Assert with facts[] (1–20 triples, call-level layers). Review mergeSuggestions before merging. Correct with memory_replace; do not reassert. Never send Cypher or write CONTRADICTS/SUPERSEDES.",
            )
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(&[
            ProtocolVersion::V_2024_11_05,
            ProtocolVersion::V_2025_03_26,
            ProtocolVersion::V_2025_06_18,
            ProtocolVersion::V_2025_11_25,
        ])
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_tool_error, map_connect_error, map_tool_result, structured_error, Mindreader,
    };
    use crate::config::Config;
    use crate::domain::DomainError;
    use crate::error::Error;
    use rmcp::ServerHandler;
    use serde_json::Value;

    fn test_server() -> Mindreader {
        Mindreader::from_config(Config::stub())
    }

    fn reason_of(result: rmcp::model::CallToolResult) -> String {
        assert_eq!(result.is_error, Some(true));
        result
            .structured_content
            .as_ref()
            .and_then(|value| value.get("reason"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string()
    }

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
                assert!(
                    !map.contains_key("allOf"),
                    "tool {tool} contains allOf: {value}"
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
        assert!(assert_props.get("s").is_none());
        assert!(assert_props.get("p").is_none());
        assert!(assert_props.get("o").is_none());
        assert!(assert_props.get("spike").is_none());
        assert!(assert_props.get("contradicts").is_none());
        let facts = &assert_props["facts"];
        assert_eq!(facts.get("maxItems").and_then(Value::as_u64), Some(20));
        assert_eq!(facts.get("minItems").and_then(Value::as_u64), Some(1));
        let item_props = facts["items"]["properties"].as_object().unwrap();
        assert_eq!(
            item_props["s"].get("type").and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(
            item_props["o"].get("type").and_then(Value::as_str),
            Some("object")
        );
        assert_eq!(
            item_props["contradicts"]
                .get("type")
                .and_then(Value::as_str),
            Some("boolean")
        );
        assert_eq!(
            item_props["s"]["properties"]["kind"]["enum"],
            serde_json::json!(["entity"])
        );
        assert_eq!(
            item_props["o"]["properties"]["kind"]["enum"],
            serde_json::json!(["entity", "literal"])
        );
        assert_eq!(item_props["p"]["minLength"], 1);
        let required = assert_schema
            .get("required")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(required, vec![Value::from("facts"), Value::from("layers")]);

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
        let semantic = router
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "memory_semantic_search")
            .expect("semantic search tool")
            .schema_as_json_value();
        assert!(semantic["properties"]["text"]["maxLength"].is_null());
        assert!(semantic["properties"]["text"]["description"]
            .as_str()
            .is_some_and(|description| description.contains("32768 UTF-8 bytes")));
    }

    #[test]
    fn tool_input_schemas_are_objects() {
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
            if let Some(output) = tool.output_schema.as_ref() {
                let output = Value::Object((**output).clone());
                assert_no_union_keywords(&output, &tool.name);
            }
        }
    }

    #[test]
    fn get_info_advertises_2025_11_25_without_list_changed() {
        let info = test_server().get_info();
        assert_eq!(
            info.protocol_version,
            rmcp::model::ProtocolVersion::V_2025_11_25
        );
        let tools = info.capabilities.tools.expect("tools capability");
        assert_ne!(tools.list_changed, Some(true));
        assert!(info.capabilities.prompts.is_none());
        assert!(info.capabilities.resources.is_none());
        let versions = test_server().supported_protocol_versions();
        assert_eq!(
            versions.as_ref(),
            &[
                rmcp::model::ProtocolVersion::V_2024_11_05,
                rmcp::model::ProtocolVersion::V_2025_03_26,
                rmcp::model::ProtocolVersion::V_2025_06_18,
                rmcp::model::ProtocolVersion::V_2025_11_25,
            ]
        );
        assert!(!versions
            .iter()
            .any(|version| version.as_str() == "2026-07-28"));
    }

    #[test]
    fn tool_annotations_are_present() {
        for tool in Mindreader::tool_router().list_all() {
            let annotations = tool
                .annotations
                .as_ref()
                .unwrap_or_else(|| panic!("{} missing annotations", tool.name));
            assert!(
                annotations
                    .title
                    .as_ref()
                    .is_some_and(|title| !title.is_empty()),
                "{} missing title",
                tool.name
            );
            match tool.name.as_ref() {
                "memory_get" | "memory_search" | "memory_traverse" | "memory_stats" => {
                    assert_eq!(annotations.read_only_hint, Some(true));
                    assert_eq!(annotations.open_world_hint, Some(false));
                }
                "memory_semantic_search" => {
                    assert_ne!(annotations.read_only_hint, Some(true));
                    assert_eq!(annotations.open_world_hint, Some(true));
                }
                "memory_assert" | "memory_schema" => {
                    assert_eq!(annotations.destructive_hint, Some(false));
                    assert_eq!(annotations.idempotent_hint, Some(true));
                }
                "memory_replace" | "memory_retract" | "memory_layers" | "memory_feedback" => {
                    assert_eq!(annotations.destructive_hint, Some(true));
                    assert_eq!(annotations.idempotent_hint, Some(false));
                }
                "memory_merge" => {
                    assert_eq!(annotations.destructive_hint, Some(true));
                }
                other => panic!("unexpected tool {other}"),
            }
        }
    }

    #[test]
    fn tool_output_schemas_are_objects() {
        for tool in Mindreader::tool_router().list_all() {
            let schema = tool
                .output_schema
                .as_ref()
                .unwrap_or_else(|| panic!("{} missing outputSchema", tool.name));
            let schema = Value::Object((**schema).clone());
            assert_no_union_keywords(&schema, &tool.name);
            assert_eq!(
                schema.get("type").and_then(Value::as_str),
                Some("object"),
                "tool {} outputSchema.type",
                tool.name
            );
        }
    }

    #[test]
    fn advertised_clamps() {
        let tools: Vec<_> = Mindreader::tool_router().list_all();
        let get = tools
            .iter()
            .find(|tool| tool.name == "memory_get")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(get["properties"]["hops"]["enum"], serde_json::json!([0, 1]));
        let search = tools
            .iter()
            .find(|tool| tool.name == "memory_search")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(search["properties"]["limit"]["minimum"], 1);
        assert_eq!(search["properties"]["limit"]["maximum"], 100);
        let semantic = tools
            .iter()
            .find(|tool| tool.name == "memory_semantic_search")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(semantic["properties"]["limit"]["minimum"], 1);
        assert_eq!(semantic["properties"]["limit"]["maximum"], 100);
        let traverse = tools
            .iter()
            .find(|tool| tool.name == "memory_traverse")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(traverse["properties"]["depth"]["minimum"], 1);
        assert_eq!(traverse["properties"]["depth"]["maximum"], 3);
        assert_eq!(traverse["properties"]["limit"]["minimum"], 1);
        assert_eq!(traverse["properties"]["limit"]["maximum"], 200);
    }

    #[test]
    fn instructions_and_descriptions_start_with_when_to_call() {
        let info = test_server().get_info();
        let instructions = info.instructions.expect("instructions");
        assert!(instructions.len() <= 512, "instructions exceed 512 chars");
        assert!(instructions.contains("OR-union") || instructions.contains("OR union"));
        assert!(instructions.contains("Search first"));
        assert!(instructions.contains("list=true"));
        assert!(instructions.contains("facts[]"));
        assert!(instructions.contains("mergeSuggestions"));
        assert!(instructions.contains("memory_replace"));
        assert!(instructions.contains("CONTRADICTS"));
        for tool in Mindreader::tool_router().list_all() {
            let description = tool
                .description
                .as_ref()
                .unwrap_or_else(|| panic!("{} missing description", tool.name));
            assert!(
                description.starts_with("Use "),
                "{} description must start with when-to-call: {description}",
                tool.name
            );
            if tool.name == "memory_schema" {
                assert!(
                    description.starts_with("Use list=true"),
                    "memory_schema must lead with list=true"
                );
            }
        }
    }

    #[test]
    fn map_tool_result_returns_structured_error() {
        let invalid = map_tool_result(Err(DomainError::InvalidInput("bad layers".into()).into()));
        assert_eq!(reason_of(invalid), "invalid_input");

        let embedding = map_tool_result(Err(Error::Embedding("missing key".into())));
        assert_eq!(reason_of(embedding), "missing_embedding");

        let concurrent = map_tool_result(Err(Error::ConcurrentMutation("fact".into())));
        assert_eq!(reason_of(concurrent), "concurrent_mutation");

        let connect = map_connect_error(Error::Graph("bolt offline".into()));
        assert_eq!(reason_of(connect), "connect_failed");

        let wrapped = map_tool_result(Err(Error::from(DomainError::InvalidInput(
            "wrapped".into(),
        ))
        .context("while validating")));
        assert_eq!(reason_of(wrapped), "invalid_input");
        assert_eq!(
            classify_tool_error(&Error::EmbeddingHttp {
                provider: "openai",
                status: 429,
                request_id: None,
                body: "slow down".into(),
            }),
            "rate_limited"
        );
        let limiter = structured_error("rate_limited", "MCP invoke rate limit exceeded");
        assert_eq!(reason_of(limiter), "rate_limited");
    }
}
