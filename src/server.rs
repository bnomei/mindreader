//! MCP stdio adapter: seven tools, host-compatible schemas, lazy Neo4j.
//!
//! Advertises `memory_recall`, `memory_write`, `memory_revise`,
//! `memory_withdraw`, `memory_judge`, `memory_place`, and `memory_unify` as
//! plain tagged object schemas (no `anyOf`/`oneOf`/`allOf`). Initialize and
//! `tools/list` do not wait on Neo4j. Recoverable failures are structured
//! `isError` results. The 120/min burst-20 limiter and 45s timeout apply only
//! to `#[tool]` handlers through the process-local invoke path.

use crate::config::Config;
use crate::domain::DomainError;
use crate::error::{Error, Result as AppResult};
use crate::graph;
use crate::merge::MergeArgs;
use crate::search::RecallArgs;
use crate::semantic::MAX_SEMANTIC_TEXT_BYTES;
use crate::service::MemoryService;
use crate::tools::{JudgeArgs, PlaceArgs, ReviseArgs, WithdrawArgs, WriteArgs};
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
    scope_schema()
}

fn scope_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "array",
        "description": "Visibility union. [] selects global/unlayered records only. Named records match any requested layer. IDs use lowercase kebab-case with colon namespaces, for example project:mindreader or analysis:hypothesis; colons are naming, not hierarchy.",
        "items": {
            "type": "string",
            "pattern": "^[a-z0-9]+(?:-[a-z0-9]+)*(?::[a-z0-9]+(?:-[a-z0-9]+)*)*$"
        }
    })
}

fn target_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "A pasteable node or fact handle from recall/write.",
        "properties": {
            "kind": { "type": "string", "enum": ["node", "fact"] },
            "iri": { "type": "string", "minLength": 1 }
        },
        "required": ["kind", "iri"]
    })
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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
        "description": "A node reference. Runtime validation requires at least one of iri or name.",
        "properties": {
            "kind": { "type": "string", "enum": ["node"] },
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
        "description": "A tagged node or literal. Node values use iri/name/labels; literal values use value and optional datatype.",
        "properties": {
            "kind": { "type": "string", "enum": ["node", "literal"] },
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
            "scope": scope_schema()
        },
        "required": ["facts", "scope"]
    }))
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
fn schema_out_memory_search() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "facts": { "type": "array" },
        "spike": { "type": "array" },
        "layers": { "type": "array" }
    })))
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[allow(dead_code)]
fn schema_out_memory_retract() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "retracted": { "type": "integer" },
        "layers": { "type": "array" }
    })))
}

#[allow(dead_code)]
fn schema_out_memory_feedback() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "weight": { "type": "integer" },
        "target": { "type": "object" }
    })))
}

#[allow(dead_code)]
fn schema_out_memory_layers() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "noop": { "type": "boolean" },
        "target": { "type": "object" },
        "before": { "type": "array" },
        "layers": { "type": "array" }
    })))
}

#[allow(dead_code)]
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

/// Stdio MCP server: seven-tool router, lazy Neo4j service, and invoke limiter.
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

    /// Sorted names of the seven advertised MCP tools.
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

fn schema_memory_recall() -> Arc<rmcp::model::JsonObject> {
    object_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "scope": scope_schema(),
            "text": { "type": "string" },
            "iris": { "type": "array", "items": { "type": "string", "minLength": 1 } },
            "labels": { "type": "array", "items": { "type": "string" } },
            "around": { "type": "string", "minLength": 1 },
            "semantic": { "type": "boolean" },
            "hops": { "type": "integer", "enum": [0, 1] },
            "p": { "type": "array", "items": { "type": "string", "minLength": 1 } },
            "depth": { "type": "integer", "minimum": 1, "maximum": 3 },
            "limit": { "type": "integer", "minimum": 1, "maximum": 200 }
        },
        "required": ["scope"]
    }))
}

fn schema_memory_write() -> Arc<rmcp::model::JsonObject> {
    schema_memory_assert()
}

fn schema_memory_revise() -> Arc<rmcp::model::JsonObject> {
    object_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "scope": scope_schema(),
            "target": target_schema(),
            "new": object_input_schema(),
            "spike": { "type": "string", "enum": ["Signal", "Pattern", "Insight", "Knowledge"] },
            "contradicts": { "type": "boolean" },
            "reason": { "type": "string" }
        },
        "required": ["scope", "target", "new"]
    }))
}

fn schema_memory_withdraw() -> Arc<rmcp::model::JsonObject> {
    object_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "scope": scope_schema(),
            "target": target_schema(),
            "subject": entity_input_schema(),
            "p": { "type": "string", "minLength": 1 },
            "reason": { "type": "string" }
        },
        "required": ["scope"]
    }))
}

fn schema_memory_judge() -> Arc<rmcp::model::JsonObject> {
    object_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "scope": scope_schema(),
            "ratings": {
                "type": "array",
                "minItems": 1,
                "maxItems": 20,
                "items": {
                    "type": "object",
                    "properties": {
                        "target": target_schema(),
                        "mode": { "type": "string", "enum": ["strengthen", "weaken"] }
                    },
                    "required": ["target", "mode"]
                }
            }
        },
        "required": ["scope", "ratings"]
    }))
}

fn schema_memory_place() -> Arc<rmcp::model::JsonObject> {
    object_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "scope": scope_schema(),
            "target": target_schema(),
            "add": { "type": "array", "items": scope_schema()["items"].clone() },
            "remove": { "type": "array", "items": scope_schema()["items"].clone() }
        },
        "required": ["scope", "target"]
    }))
}

fn schema_out_memory_recall() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "scope": { "type": "array" },
        "semantic": { "type": "boolean" },
        "facts": { "type": "array" },
        "nodes": { "type": "array" },
        "paths": { "type": "array" }
    })))
}

fn schema_out_memory_write() -> Arc<rmcp::model::JsonObject> {
    schema_out_memory_assert()
}

fn schema_out_memory_revise() -> Arc<rmcp::model::JsonObject> {
    schema_out_memory_replace()
}

fn schema_out_memory_withdraw() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "retracted": { "type": "integer" },
        "scope": { "type": "array" },
        "targets": { "type": "array" }
    })))
}

fn schema_out_memory_judge() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "scope": { "type": "array" },
        "ratings": { "type": "array" }
    })))
}

fn schema_out_memory_place() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "target": { "type": "object" },
        "memberships": { "type": "array" }
    })))
}

fn schema_out_memory_unify() -> Arc<rmcp::model::JsonObject> {
    schema_out_memory_merge()
}

#[tool_router]
impl Mindreader {
    #[tool(
        name = "memory_recall",
        description = "Use to recover visible memory. Pass exactly one of text, iris, labels, or around. labels Class or Property lists schema nodes. semantic:true is conceptual recall and sends query text to the embedding provider. hops is 0 or 1 with iris. around+p filters by predicate name, not Neo4j type.",
        input_schema = schema_memory_recall(),
        output_schema = schema_out_memory_recall(),
        annotations(title = "Recall visible memory", read_only_hint = false, open_world_hint = true)
    )]
    async fn memory_recall(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.recall(args).await })
            .await
    }

    #[tool(
        name = "memory_write",
        description = "Use when adding durable triples after recall. Pass facts[] (1–20) and call-level scope. [] makes facts global. Exact reassertions are no-ops. Review next.unify before memory_unify. Optional per-fact contradicts:true links conflicting current values.",
        input_schema = schema_memory_write(),
        output_schema = schema_out_memory_write(),
        annotations(title = "Write facts", destructive_hint = false, idempotent_hint = true)
    )]
    async fn memory_write(
        &self,
        Parameters(args): Parameters<WriteArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.write(args).await })
            .await
    }

    #[tool(
        name = "memory_revise",
        description = "Use to correct one current fact by pasting its target handle and the new object. scope moves only those memberships; SUPERSEDES is recorded atomically. Do not re-write a correction.",
        input_schema = schema_memory_revise(),
        output_schema = schema_out_memory_revise(),
        annotations(title = "Revise a fact", destructive_hint = true, idempotent_hint = false)
    )]
    async fn memory_revise(
        &self,
        Parameters(args): Parameters<ReviseArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.revise(args).await })
            .await
    }

    #[tool(
        name = "memory_withdraw",
        description = "Use to soft-withdraw a fact handle or every current fact for a subject (optional p). Does not delete nodes or history.",
        input_schema = schema_memory_withdraw(),
        output_schema = schema_out_memory_withdraw(),
        annotations(title = "Withdraw facts", destructive_hint = true, idempotent_hint = false)
    )]
    async fn memory_withdraw(
        &self,
        Parameters(args): Parameters<WithdrawArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.withdraw(args).await })
            .await
    }

    #[tool(
        name = "memory_judge",
        description = "Use after a recalled node or fact helped or hurt. Pass ratings[] of strengthen or weaken. Retrieval never changes weight automatically.",
        input_schema = schema_memory_judge(),
        output_schema = schema_out_memory_judge(),
        annotations(title = "Judge retrieved targets", destructive_hint = true, idempotent_hint = false)
    )]
    async fn memory_judge(
        &self,
        Parameters(args): Parameters<JudgeArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.judge(args).await })
            .await
    }

    #[tool(
        name = "memory_place",
        description = "Use to change which layers a node or fact belongs to. scope is visibility, not the edit. Put memberships in add and remove.",
        input_schema = schema_memory_place(),
        output_schema = schema_out_memory_place(),
        annotations(title = "Place layer memberships", destructive_hint = true, idempotent_hint = false)
    )]
    async fn memory_place(
        &self,
        Parameters(args): Parameters<PlaceArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.place(args).await })
            .await
    }

    #[tool(
        name = "memory_unify",
        description = "Use after reviewing next.unify when two same-kind nodes are truly identical. Permanently merge source into target. The target IRI and name survive. Reverse the pair when the other IRI should survive.",
        input_schema = schema_memory_merge(),
        output_schema = schema_out_memory_unify(),
        annotations(title = "Permanently unify nodes", destructive_hint = true)
    )]
    async fn memory_unify(
        &self,
        Parameters(args): Parameters<MergeArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.unify(args).await })
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
                "scope is an OR-union ([] is global-only). Recall before write. Catalog Classes/Properties with labels Class or Property. semantic:true sends query text to the embed provider. Write facts[] (1-20). Paste target into revise, withdraw, judge, or place. Review unify {source,target}. Never send Cypher or write CONTRADICTS/SUPERSEDES.",
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
    fn registers_seven_tools() {
        let names = Mindreader::registered_tool_names();
        let expected = [
            "memory_judge",
            "memory_place",
            "memory_recall",
            "memory_revise",
            "memory_unify",
            "memory_withdraw",
            "memory_write",
        ];
        assert_eq!(names, expected);
        let router = Mindreader::tool_router();
        for name in expected {
            assert!(router.has_route(name), "missing route {name}");
        }
        assert_eq!(router.map.len(), 7);
    }

    #[test]
    fn mutation_schemas_advertise_tagged_inputs() {
        let router = Mindreader::tool_router();
        let tools: Vec<_> = router.list_all();
        let assert_schema = tools
            .iter()
            .find(|tool| tool.name == "memory_write")
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
            serde_json::json!(["node"])
        );
        assert_eq!(
            item_props["o"]["properties"]["kind"]["enum"],
            serde_json::json!(["node", "literal"])
        );
        assert_eq!(item_props["p"]["minLength"], 1);
        let required = assert_schema
            .get("required")
            .and_then(|r| r.as_array())
            .cloned()
            .unwrap_or_default();
        assert_eq!(required, vec![Value::from("facts"), Value::from("scope")]);

        let revise_schema = tools
            .iter()
            .find(|tool| tool.name == "memory_revise")
            .unwrap()
            .schema_as_json_value();
        let revise_required = revise_schema["required"].as_array().unwrap();
        for name in ["scope", "target", "new"] {
            assert!(revise_required.iter().any(|value| value == name));
        }
        assert_eq!(revise_schema["properties"]["new"]["type"], "object");
        assert_eq!(
            revise_schema["properties"]["target"]["properties"]["kind"]["enum"],
            serde_json::json!(["node", "fact"])
        );

        let judge_schema = tools
            .iter()
            .find(|tool| tool.name == "memory_judge")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(
            judge_schema["properties"]["ratings"]["items"]["properties"]["mode"]["enum"],
            serde_json::json!(["strengthen", "weaken"])
        );
    }

    #[test]
    fn scoped_tools_require_scope() {
        let router = Mindreader::tool_router();
        for tool in router.list_all() {
            let schema = tool.schema_as_json_value();
            let required = schema["required"].as_array().cloned().unwrap_or_default();
            if tool.name == "memory_unify" {
                assert!(!required.iter().any(|value| value == "scope"));
            } else {
                assert!(
                    required.iter().any(|value| value == "scope"),
                    "{} must require scope",
                    tool.name
                );
            }
        }
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
                "memory_recall" => {
                    assert_ne!(annotations.read_only_hint, Some(true));
                    assert_eq!(annotations.open_world_hint, Some(true));
                }
                "memory_write" => {
                    assert_eq!(annotations.destructive_hint, Some(false));
                    assert_eq!(annotations.idempotent_hint, Some(true));
                }
                "memory_revise" | "memory_withdraw" | "memory_place" | "memory_judge" => {
                    assert_eq!(annotations.destructive_hint, Some(true));
                    assert_eq!(annotations.idempotent_hint, Some(false));
                }
                "memory_unify" => {
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
        let recall = tools
            .iter()
            .find(|tool| tool.name == "memory_recall")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(
            recall["properties"]["hops"]["enum"],
            serde_json::json!([0, 1])
        );
        assert_eq!(recall["properties"]["limit"]["minimum"], 1);
        assert_eq!(recall["properties"]["limit"]["maximum"], 200);
        assert_eq!(recall["properties"]["depth"]["minimum"], 1);
        assert_eq!(recall["properties"]["depth"]["maximum"], 3);
    }

    #[test]
    fn instructions_and_descriptions_start_with_when_to_call() {
        let info = test_server().get_info();
        let instructions = info.instructions.expect("instructions");
        assert!(instructions.len() <= 512, "instructions exceed 512 chars");
        assert!(instructions.contains("OR-union") || instructions.contains("OR union"));
        assert!(instructions.contains("Recall"));
        assert!(instructions.contains("facts[]"));
        assert!(instructions.contains("target"));
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
