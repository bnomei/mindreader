//! MCP stdio adapter: eight tools, host-compatible schemas, lazy Neo4j.
//!
//! Advertises `memory_recall`, `memory_write`, `memory_revise`,
//! `memory_recall_semantic`, `memory_withdraw`, `memory_judge`, `memory_place`,
//! and `memory_unify` as
//! plain tagged object schemas (no `anyOf`/`oneOf`/`allOf`). Initialize and
//! `tools/list` do not wait on Neo4j. Recoverable failures are structured
//! `isError` results. The 120/min burst-40 limiter and 45s timeout apply only
//! to `#[tool]` handlers through the process-local invoke path.

use crate::config::Config;
use crate::domain::DomainError;
use crate::error::{Error, Result as AppResult};
use crate::graph;
use crate::merge::MergeArgs;
use crate::search::RecallArgs;
use crate::semantic::{SemanticSearchArgs, MAX_SEMANTIC_TEXT_BYTES};
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
const RATE_LIMIT_BURST: f64 = 40.0;

fn object_schema(value: serde_json::Value) -> Arc<rmcp::model::JsonObject> {
    Arc::new(rmcp::model::object(value))
}

fn scope_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "array",
        "description": "Visibility union. [] selects global/unlayered records only. Named records match any requested layer. IDs use lowercase kebab-case with colon namespaces, for example project:mindreader or analysis:hypothesis; colons are naming, not hierarchy.",
        "uniqueItems": true,
        "items": {
            "type": "string",
            "pattern": "^[a-z0-9]+(?:-[a-z0-9]+)*(?::[a-z0-9]+(?:-[a-z0-9]+)*)*$"
        }
    })
}

fn layer_list_schema(description: &str) -> serde_json::Value {
    let mut schema = scope_schema();
    schema["description"] = Value::String(description.to_string());
    schema
}

fn target_schema_with_kinds(kinds: &[&str], description: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": description,
        "additionalProperties": false,
        "properties": {
            "kind": { "type": "string", "enum": kinds },
            "iri": { "type": "string", "minLength": 1 }
        },
        "required": ["kind", "iri"]
    })
}

fn target_schema() -> serde_json::Value {
    target_schema_with_kinds(
        &["node", "fact"],
        "A pasteable current node or fact handle returned by Mindreader.",
    )
}

fn fact_target_schema() -> serde_json::Value {
    target_schema_with_kinds(
        &["fact"],
        "A pasteable current fact handle returned by recall, write, or revise.",
    )
}

fn label_schema() -> serde_json::Value {
    json!({
        "type": "string",
        "minLength": 1,
        "pattern": "^[A-Za-z][A-Za-z0-9_]*$"
    })
}

fn predicate_list_schema(description: &str) -> serde_json::Value {
    json!({
        "type": "array",
        "description": description,
        "minItems": 1,
        "uniqueItems": true,
        "items": { "type": "string", "minLength": 1 }
    })
}

fn entity_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "A node reference. Runtime validation requires at least one of iri or name.",
        "additionalProperties": false,
        "properties": {
            "kind": { "type": "string", "enum": ["node"] },
            "iri": { "type": "string", "minLength": 1 },
            "name": { "type": "string", "minLength": 1 },
            "labels": { "type": "array", "uniqueItems": true, "items": label_schema() }
        },
        "required": ["kind"]
    })
}

fn object_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "A tagged node or literal. Node values use iri/name/labels; literal values use value and optional datatype.",
        "additionalProperties": false,
        "properties": {
            "kind": { "type": "string", "enum": ["node", "literal"] },
            "iri": { "type": "string", "minLength": 1 },
            "name": { "type": "string", "minLength": 1 },
            "labels": { "type": "array", "uniqueItems": true, "items": label_schema() },
            "value": { "type": "string" },
            "datatype": { "type": "string", "minLength": 1, "default": "xsd:string" }
        },
        "required": ["kind"]
    })
}

fn schema_memory_write() -> Arc<rmcp::model::JsonObject> {
    // Host wrappers drop the whole server if any inputSchema uses anyOf.
    object_schema(serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "facts": {
                "type": "array",
                "minItems": 1,
                "maxItems": 20,
                "description": "Atomic, input-ordered batch. The whole call rolls back if any item is invalid or fails.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
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

fn error_envelope_properties() -> serde_json::Map<String, Value> {
    let Value::Object(map) = json!({
        "ok": { "type": "boolean" },
        "reason": { "type": "string" },
        "message": { "type": "string" },
        "retryable": { "type": "boolean" },
        "outcome": { "type": "string", "enum": ["not_applied", "unknown"] },
        "retryAfterMs": { "type": "integer", "minimum": 1 }
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
        "additionalProperties": false,
        "properties": properties,
        "required": ["ok"]
    }))
}

fn props(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    }
}

fn node_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": { "type": "string", "enum": ["node", "literal"] },
            "iri": { "type": "string" },
            "name": { "type": ["string", "null"] },
            "labels": { "type": "array", "items": { "type": "string" } },
            "value": { "type": "string" },
            "datatype": { "type": "string" },
            "scope": scope_schema(),
            "weight": { "type": "integer" },
            "stub": { "type": "boolean" },
            "tool": { "type": "string" },
            "rateable": { "type": "boolean" },
            "mutable": { "type": "boolean" },
            "target": target_schema_with_kinds(&["node"], "Pasteable node handle.")
        },
        "required": ["iri"]
    })
}

fn relationship_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "kind": { "type": "string", "enum": ["fact"] },
            "type": { "type": "string" },
            "iri": { "type": "string" },
            "from": { "type": "string" },
            "to": { "type": "string" },
            "propertyIri": { "type": "string" },
            "scope": scope_schema(),
            "weight": { "type": "integer" },
            "episodeId": { "type": "string" },
            "reason": { "type": "string" }
        },
        "required": ["iri"]
    })
}

fn conflict_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "relationship": relationship_schema(),
            "o": node_schema(),
            "p": { "type": "string" }
        },
        "required": ["relationship", "o", "p"]
    })
}

fn fact_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "target": fact_target_schema(),
            "s": node_schema(),
            "p": { "type": "string" },
            "o": node_schema(),
            "relationship": relationship_schema(),
            "scope": scope_schema(),
            "spike": { "type": ["string", "null"], "enum": ["Signal", "Pattern", "Insight", "Knowledge", null] },
            "score": { "type": "number" },
            "effectiveWeight": { "type": "integer" },
            "weight": { "type": "integer" },
            "rank": { "type": "integer", "minimum": 1 },
            "conflicts": { "type": "array", "items": conflict_schema() },
            "noop": { "type": "boolean" },
            "current": { "type": "boolean" },
            "rateable": { "type": "boolean" },
            "mutable": { "type": "boolean" },
            "validTo": { "type": "string" }
        },
        "required": ["target", "s", "p", "o"]
    })
}

fn nullable_fact_schema() -> Value {
    let mut schema = fact_schema();
    schema["type"] = json!(["object", "null"]);
    schema
}

fn about_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "node": node_schema(),
            "about": { "type": "string" },
            "rank": { "type": "string", "enum": ["Signal", "Pattern", "Insight", "Knowledge"] },
            "relationship": relationship_schema(),
            "effectiveWeight": { "type": "integer" }
        },
        "required": ["node", "about", "rank", "relationship", "effectiveWeight"]
    })
}

fn episode_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "properties": {
            "iri": { "type": "string" },
            "at": { "type": "string" },
            "tool": {
                "type": "string",
                "enum": [
                    "memory_judge", "memory_place", "memory_revise", "memory_unify",
                    "memory_withdraw", "memory_write"
                ]
            }
        },
        "required": ["iri", "at", "tool"]
    })
}

fn node_handle_schema(description: &str) -> Value {
    target_schema_with_kinds(&["node"], description)
}

fn handles_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "description": "Pasteable identities from this result. Empty arrays and nulls are unused roles, not commands.",
        "properties": {
            "facts": { "type": "array", "items": fact_target_schema() },
            "nodes": { "type": "array", "items": node_handle_schema("Pasteable node handle.") },
            "current": {
                "type": ["object", "null"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "type": "string", "enum": ["node", "fact"] },
                    "iri": { "type": "string", "minLength": 1 }
                }
            },
            "retired": {
                "type": ["object", "null"],
                "additionalProperties": false,
                "properties": {
                    "kind": { "type": "string", "enum": ["node", "fact"] },
                    "iri": { "type": "string", "minLength": 1 }
                }
            },
            "unify": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "source": node_handle_schema("Absorbed node handle."),
                        "target": node_handle_schema("Surviving node handle.")
                    },
                    "required": ["source", "target"]
                }
            }
        },
        "required": ["facts", "nodes", "current", "retired", "unify"]
    })
}

fn detail_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["concise", "detailed"],
        "default": "detailed",
        "description": "concise returns handles plus thin s/p/o lines. detailed is the full envelope."
    })
}

fn review_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "unify": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "source": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": ["node"] },
                                "iri": { "type": "string" },
                                "name": { "type": ["string", "null"] }
                            },
                            "required": ["kind", "iri"]
                        },
                        "target": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": ["node"] },
                                "iri": { "type": "string" },
                                "name": { "type": ["string", "null"] }
                            },
                            "required": ["kind", "iri"]
                        },
                        "similarity": { "type": "number", "minimum": 0, "maximum": 1 }
                    },
                    "required": ["source", "target", "similarity"]
                }
            },
            "alternatives": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "target": fact_target_schema(),
                        "conflicts": { "type": "array", "items": conflict_schema() }
                    },
                    "required": ["target", "conflicts"]
                }
            }
        },
        "required": ["unify", "alternatives"]
    })
}

fn summary_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "requested": { "type": "integer", "minimum": 1, "maximum": 20 },
            "changed": { "type": "integer", "minimum": 0, "maximum": 20 },
            "noop": { "type": "integer", "minimum": 0, "maximum": 20 }
        },
        "required": ["requested", "changed", "noop"]
    })
}

/// Process-local token bucket for MCP `#[tool]` handlers only.
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

    /// Take one token from the process-local 120/min burst-40 bucket.
    fn try_acquire(&self) -> std::result::Result<(), u64> {
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
            Ok(())
        } else {
            let tokens_per_second = RATE_LIMIT_PER_MINUTE / 60.0;
            let retry_after_ms = ((1.0 - state.tokens) / tokens_per_second * 1_000.0)
                .ceil()
                .max(1.0) as u64;
            Err(retry_after_ms)
        }
    }
}

/// Stdio MCP server: eight-tool router, lazy Neo4j service, and invoke limiter.
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
                graph::bootstrap(
                    &g,
                    embedding_space.as_ref(),
                    crate::graph::SpaceReplace::Allow,
                )
                .await?;
                MemoryService::new(g, &self.cfg)
            })
            .await?;
        Ok(())
    }

    /// Sorted names of the eight advertised MCP tools.
    pub fn registered_tool_names() -> Vec<String> {
        let router = Self::tool_router();
        let mut names: Vec<String> = router.map.keys().map(|k| k.to_string()).collect();
        names.sort();
        names
    }

    /// Rate-limit and time-box one MCP `#[tool]` call after lazy Neo4j connect.
    async fn invoke<F, Fut>(&self, op: F) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(MemoryService) -> Fut,
        Fut: std::future::Future<Output = AppResult<Value>>,
    {
        if let Err(retry_after_ms) = self.limiter.try_acquire() {
            return Ok(structured_error_with_retry_after(
                "rate_limited",
                "MCP invoke rate limit exceeded (120/min, burst 40)",
                retry_after_ms,
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

/// Recoverable tool failure as `isError` with `{ok:false,reason,message,retryable,outcome}`.
fn structured_error(reason: &str, message: impl std::fmt::Display) -> CallToolResult {
    let (retryable, outcome) = error_retry_metadata(reason);
    CallToolResult::structured_error(json!({
        "ok": false,
        "reason": reason,
        "message": message.to_string(),
        "retryable": retryable,
        "outcome": outcome,
    }))
}

fn structured_error_with_retry_after(
    reason: &str,
    message: impl std::fmt::Display,
    retry_after_ms: u64,
) -> CallToolResult {
    let (retryable, outcome) = error_retry_metadata(reason);
    CallToolResult::structured_error(json!({
        "ok": false,
        "reason": reason,
        "message": message.to_string(),
        "retryable": retryable,
        "outcome": outcome,
        "retryAfterMs": retry_after_ms,
    }))
}

fn error_retry_metadata(reason: &str) -> (bool, &'static str) {
    match reason {
        "connect_failed" | "rate_limited" | "concurrent_mutation" => (true, "not_applied"),
        "invalid_input"
        | "precondition_failed"
        | "missing_embedding"
        | "embedding_space"
        | "embedding_http" => (false, "not_applied"),
        // A timeout or undifferentiated operation failure can include an
        // ambiguous commit. Never encourage an automatic retry.
        "timeout" | "operation" => (false, "unknown"),
        _ => (false, "unknown"),
    }
}

fn map_connect_error(error: Error) -> CallToolResult {
    structured_error("connect_failed", error)
}

fn map_tool_result(result: AppResult<Value>) -> CallToolResult {
    match result {
        Ok(Value::Object(mut value)) => {
            value.insert("ok".into(), Value::Bool(true));
            CallToolResult::structured(Value::Object(value))
        }
        Ok(value) => CallToolResult::structured(json!({ "ok": true, "result": value })),
        Err(error) => structured_error(classify_tool_error(&error), error),
    }
}

/// Map application errors to MCP `reason` codes; domain validation stays `invalid_input`.
fn classify_tool_error(error: &Error) -> &'static str {
    match error {
        Error::Domain(DomainError::InvalidInput(_)) => "invalid_input",
        Error::Domain(DomainError::Precondition(_)) => "precondition_failed",
        Error::ConcurrentMutation(_) => "concurrent_mutation",
        // Setup and missing-key failures; HTTP status variants map separately.
        Error::Embedding(_) => "missing_embedding",
        Error::EmbeddingSpace(_) => "embedding_space",
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
            "text": {
                "type": "string",
                "minLength": 1,
                "description": "Lexical fact query. Use only when text is the selected recall mode."
            },
            "iris": {
                "type": "array",
                "description": "One to 20 node IRIs. Results preserve input order and report misses.",
                "minItems": 1,
                "maxItems": 20,
                "uniqueItems": true,
                "items": { "type": "string", "minLength": 1 }
            },
            "labels": {
                "type": "array",
                "description": "Node labels to catalog or filter. Class and Property select the global schema catalog.",
                "minItems": 1,
                "uniqueItems": true,
                "items": label_schema()
            },
            "around": {
                "type": "string",
                "minLength": 1,
                "description": "Starting node IRI for bounded graph recall."
            },
            "hops": {
                "type": "integer",
                "description": "IRI mode only. 0 still returns incident fact handles on lookups[i].facts. 1 also fills top-level facts[] when detail is detailed. Concise iris recall keeps facts[] empty.",
                "enum": [0, 1],
                "default": 0
            },
            "p": predicate_list_schema("Around mode only: predicate names or IRIs to filter before limiting."),
            "depth": {
                "type": "integer",
                "description": "Around mode only: traversal depth.",
                "minimum": 1,
                "maximum": 3,
                "default": 1
            },
            "history": {
                "type": "string",
                "minLength": 1,
                "description": "One node or fact IRI. Returns current and historical facts for that identity."
            },
            "detail": detail_schema(),
            "limit": {
                "type": "integer",
                "description": "Maximum returned facts across the selected mode.",
                "minimum": 1,
                "maximum": 100,
                "default": 20
            }
        },
        "required": ["scope"]
    }))
}

fn schema_memory_recall_semantic() -> Arc<rmcp::model::JsonObject> {
    object_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "scope": scope_schema(),
            "text": {
                "type": "string",
                "minLength": 1,
                "description": format!("Conceptual query sent to the configured embedding provider; maximum {MAX_SEMANTIC_TEXT_BYTES} UTF-8 bytes.")
            },
            "labels": {
                "type": "array",
                "description": "Optional labels used to filter semantic results.",
                "minItems": 1,
                "uniqueItems": true,
                "items": label_schema()
            },
            "detail": detail_schema(),
            "limit": {
                "type": "integer",
                "description": "Maximum fused semantic results.",
                "minimum": 1,
                "maximum": 100,
                "default": 20
            }
        },
        "required": ["scope", "text"]
    }))
}

fn schema_memory_revise() -> Arc<rmcp::model::JsonObject> {
    object_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "scope": scope_schema(),
            "target": fact_target_schema(),
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
            "target": fact_target_schema(),
            "subject": entity_input_schema(),
            "p": { "type": "string", "minLength": 1 },
            "reason": { "type": "string", "description": "Optional audit reason." }
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
                "description": "Atomic, input-ordered ratings. Duplicate targets are invalid; any failure rolls back every rating.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
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
            "edits": {
                "type": "array",
                "description": "Atomic, input-ordered membership edits. Duplicate targets, add/remove overlap, and closure violations reject the whole batch. Include literal fact endpoints in the same batch as {kind:\"node\", iri}.",
                "minItems": 1,
                "maxItems": 20,
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "target": target_schema(),
                        "add": layer_list_schema("Layer memberships to add."),
                        "remove": layer_list_schema("Layer memberships to remove.")
                    },
                    "required": ["target"]
                }
            }
        },
        "required": ["scope", "edits"]
    }))
}

fn schema_memory_unify() -> Arc<rmcp::model::JsonObject> {
    object_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "source": node_handle_schema("Same-kind node that will be permanently absorbed."),
            "target": node_handle_schema("Same-kind surviving node whose identity and name remain.")
        },
        "required": ["source", "target"]
    }))
}

fn schema_out_memory_recall() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "mode": { "type": "string", "enum": ["text", "iris", "labels", "catalog", "around", "history"] },
        "detail": { "type": "string", "enum": ["concise", "detailed"] },
        "handles": handles_schema(),
        "scope": scope_schema(),
        "facts": { "type": "array", "items": fact_schema() },
        "nodes": { "type": "array", "items": node_schema() },
        "paths": {
            "type": "array",
            "description": "For around recall, paths[i] is the deterministic shortest witness path for facts[i].",
            "items": {
                "type": "object",
                "properties": {
                    "nodes": { "type": "array", "items": { "type": "string" } },
                    "edges": { "type": "array", "items": relationship_schema() }
                },
                "required": ["nodes", "edges"]
            }
        },
        "about": { "type": "array", "items": about_schema() },
        "lookups": {
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "iri": { "type": "string" },
                    "found": { "type": "boolean" },
                    "node": node_schema(),
                    "facts": { "type": "array", "items": fact_schema() }
                },
                "required": ["iri", "found"]
            }
        },
        "from": { "type": "string" },
        "truncated": { "type": "boolean" }
    })))
}

fn schema_out_memory_recall_semantic() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "mode": { "type": "string", "enum": ["semantic"] },
        "detail": { "type": "string", "enum": ["concise", "detailed"] },
        "handles": handles_schema(),
        "scope": scope_schema(),
        "facts": { "type": "array", "items": fact_schema() },
        "nodes": { "type": "array", "items": node_schema() },
        "paths": {
            "type": "array",
            "items": {
                "type": "object",
                "properties": {
                    "nodes": { "type": "array", "items": { "type": "string" } },
                    "edges": { "type": "array", "items": relationship_schema() }
                },
                "required": ["nodes", "edges"]
            }
        },
        "about": { "type": "array", "items": about_schema() },
        "lookups": {
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "iri": { "type": "string" },
                    "found": { "type": "boolean" },
                    "node": node_schema(),
                    "facts": { "type": "array", "items": fact_schema() }
                },
                "required": ["iri", "found"]
            }
        },
        "from": { "type": "string" },
        "truncated": { "type": "boolean" }
    })))
}

fn schema_out_memory_write() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "scope": scope_schema(),
        "noop": { "type": "boolean" },
        "episode": episode_schema(),
        "facts": { "type": "array", "items": fact_schema() },
        "review": review_schema(),
        "handles": handles_schema()
    })))
}

fn schema_out_memory_revise() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "scope": scope_schema(),
        "noop": { "type": "boolean" },
        "episode": episode_schema(),
        "target": fact_target_schema(),
        "previousTarget": fact_target_schema(),
        "fact": nullable_fact_schema(),
        "review": review_schema(),
        "handles": handles_schema()
    })))
}

fn schema_out_memory_withdraw() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "scope": scope_schema(),
        "noop": { "type": "boolean" },
        "episode": episode_schema(),
        "retracted": { "type": "integer" },
        "withdrawnTargets": { "type": "array", "items": fact_target_schema() },
        "reason": { "type": ["string", "null"] },
        "handles": handles_schema()
    })))
}

fn schema_out_memory_judge() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "scope": scope_schema(),
        "noop": { "type": "boolean" },
        "episode": episode_schema(),
        "summary": summary_schema(),
        "items": {
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "index": { "type": "integer", "minimum": 0, "maximum": 19 },
                    "target": target_schema(),
                    "mode": { "type": "string", "enum": ["strengthen", "weaken"] },
                    "delta": { "type": "integer", "enum": [-1, 1] },
                    "before": { "type": "integer" },
                    "after": { "type": "integer" },
                    "status": { "type": "string", "enum": ["changed", "noop"] }
                },
                "required": ["index", "target", "mode", "delta", "before", "after", "status"]
            }
        },
        "handles": handles_schema()
    })))
}

fn schema_out_memory_place() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "scope": scope_schema(),
        "noop": { "type": "boolean" },
        "episode": episode_schema(),
        "summary": summary_schema(),
        "items": {
            "type": "array",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "index": { "type": "integer", "minimum": 0, "maximum": 19 },
                    "target": target_schema(),
                    "before": scope_schema(),
                    "memberships": scope_schema(),
                    "added": scope_schema(),
                    "removed": scope_schema(),
                    "status": { "type": "string", "enum": ["changed", "noop"] }
                },
                "required": ["index", "target", "before", "memberships", "added", "removed", "status"]
            }
        },
        "handles": handles_schema()
    })))
}

fn schema_out_memory_unify() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "noop": { "type": "boolean" },
        "episode": episode_schema(),
        "node": node_schema(),
        "handles": handles_schema()
    })))
}

#[tool_router]
impl Mindreader {
    #[tool(
        name = "memory_recall",
        title = "Recall visible memory",
        description = "Use to read visible memory without external calls or graph writes. Pass exactly one of text, iris (1–20 node IRIs), labels, around, or history; other selector fields are rejected. limit defaults to 20 and is at most 100. hops applies only to iris (0 still returns incident fact handles on lookups[i].facts; 1 also fills top-level facts[] when detail is detailed). p and depth apply only to around; history walks current and validTo/SUPERSEDES facts for one node or fact IRI. detail is concise or detailed. Class or Property labels read the global catalog.",
        input_schema = schema_memory_recall(),
        output_schema = schema_out_memory_recall(),
        annotations(title = "Recall visible memory", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn memory_recall(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.recall(args).await })
            .await
    }

    #[tool(
        name = "memory_recall_semantic",
        title = "Recall by meaning",
        description = "Use for conceptual recall when lexical memory_recall is insufficient. text is required, limited to 32 KiB UTF-8, and sent to the configured embedding provider; labels optionally filter results. limit defaults to 20 and is at most 100. detail is concise or detailed. The call maintains expiring semantic activations, so it is neither read-only nor idempotent.",
        input_schema = schema_memory_recall_semantic(),
        output_schema = schema_out_memory_recall_semantic(),
        annotations(title = "Recall by meaning", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true)
    )]
    async fn memory_recall_semantic(
        &self,
        Parameters(args): Parameters<SemanticSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.recall_semantic(args).await })
            .await
    }

    #[tool(
        name = "memory_write",
        title = "Write facts",
        description = "Use to add durable triples after recall. facts contains 1–20 input-ordered items under one call-level scope; the batch is atomic and records one Episode only when something changes. Exact reassertions are no-ops. Review review.unify before memory_unify and review.alternatives without assuming another set-valued fact is wrong.",
        input_schema = schema_memory_write(),
        output_schema = schema_out_memory_write(),
        annotations(title = "Write facts", read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
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
        title = "Revise a fact",
        description = "Use to correct one current fact by pasting its fact-only target and supplying the new object. scope selects the memberships moved from the previous fact; unrelated values and memberships remain. The replacement and SUPERSEDES history commit in one transaction and the response returns both current target and previousTarget.",
        input_schema = schema_memory_revise(),
        output_schema = schema_out_memory_revise(),
        annotations(title = "Revise a fact", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
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
        title = "Withdraw facts",
        description = "Use to soft-withdraw either one current fact target or every visible current fact for subject, optionally restricted by p. Exactly one of target or subject is required. The operation sets validity history, never hard-deletes nodes or facts, and records one Episode only when something changes.",
        input_schema = schema_memory_withdraw(),
        output_schema = schema_out_memory_withdraw(),
        annotations(title = "Withdraw facts", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
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
        title = "Judge retrieved targets",
        description = "Use after recalled nodes or facts helped or hurt. ratings contains 1–20 unique targets; strengthen or weaken changes each shared weight by exactly +1 or -1. The input-ordered batch is atomic, records one Episode, and rolls back fully on any failure. Recall itself never changes weight.",
        input_schema = schema_memory_judge(),
        output_schema = schema_out_memory_judge(),
        annotations(title = "Judge retrieved targets", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
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
        title = "Place layer memberships",
        description = "Use to atomically change layer memberships for 1–20 unique node or fact targets. scope controls visibility; each edits item supplies add and/or remove. Include literal fact endpoints in the same batch as {kind:\"node\", iri}. The whole input-ordered batch is validated against its combined final endpoint-closure state, records one Episode if changed, and rolls back fully on failure.",
        input_schema = schema_memory_place(),
        output_schema = schema_out_memory_place(),
        annotations(title = "Place layer memberships", read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
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
        title = "Permanently unify nodes",
        description = "Use only after reviewing review.unify and confirming two same-kind nodes are identical. source and target are pasteable node handles {kind:\"node\", iri}. This database-wide operation permanently absorbs source into target; target IRI and name survive. Reverse the pair when the other identity should survive. It has no scope because all memberships and history must be reconciled.",
        input_schema = schema_memory_unify(),
        output_schema = schema_out_memory_unify(),
        annotations(title = "Permanently unify nodes", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
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
                "scope is an OR-union ([] is global-only). One memory_recall selector (text, iris, labels, around, or history), or skip recall when the write is already exact. Use memory_recall_semantic only for conceptual retrieval because it calls the embedding provider and updates activations. Write facts[] (1-20). Paste returned target handles into revise, withdraw, judge, place, or unify. Review review.unify before unifying. Never send Cypher or assert CONTRADICTS/SUPERSEDES.",
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
        TokenBucket,
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
    fn registers_eight_tools() {
        let names = Mindreader::registered_tool_names();
        let expected = [
            "memory_judge",
            "memory_place",
            "memory_recall",
            "memory_recall_semantic",
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
        assert_eq!(router.map.len(), 8);
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
            serde_json::json!(["fact"])
        );

        let withdraw_schema = tools
            .iter()
            .find(|tool| tool.name == "memory_withdraw")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(
            withdraw_schema["properties"]["target"]["properties"]["kind"]["enum"],
            serde_json::json!(["fact"])
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
        assert_eq!(judge_schema["properties"]["ratings"]["maxItems"], 20);

        let place_schema = tools
            .iter()
            .find(|tool| tool.name == "memory_place")
            .unwrap()
            .schema_as_json_value();
        assert!(place_schema["properties"].get("target").is_none());
        assert!(place_schema["properties"].get("add").is_none());
        assert!(place_schema["properties"].get("remove").is_none());
        assert_eq!(place_schema["properties"]["edits"]["minItems"], 1);
        assert_eq!(place_schema["properties"]["edits"]["maxItems"], 20);
        assert_eq!(
            place_schema["required"],
            serde_json::json!(["scope", "edits"])
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
            assert!(
                tool.title.as_ref().is_some_and(|title| !title.is_empty()),
                "{} missing top-level title",
                tool.name
            );
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
                    assert_eq!(annotations.read_only_hint, Some(true));
                    assert_eq!(annotations.destructive_hint, Some(false));
                    assert_eq!(annotations.idempotent_hint, Some(true));
                    assert_eq!(annotations.open_world_hint, Some(false));
                }
                "memory_recall_semantic" => {
                    assert_eq!(annotations.read_only_hint, Some(false));
                    assert_eq!(annotations.destructive_hint, Some(false));
                    assert_eq!(annotations.idempotent_hint, Some(false));
                    assert_eq!(annotations.open_world_hint, Some(true));
                }
                "memory_write" => {
                    assert_eq!(annotations.read_only_hint, Some(false));
                    assert_eq!(annotations.destructive_hint, Some(false));
                    assert_eq!(annotations.idempotent_hint, Some(true));
                    assert_eq!(annotations.open_world_hint, Some(false));
                }
                "memory_place" => {
                    assert_eq!(annotations.read_only_hint, Some(false));
                    assert_eq!(annotations.destructive_hint, Some(true));
                    assert_eq!(annotations.idempotent_hint, Some(true));
                    assert_eq!(annotations.open_world_hint, Some(false));
                }
                "memory_revise" | "memory_withdraw" | "memory_judge" => {
                    assert_eq!(annotations.read_only_hint, Some(false));
                    assert_eq!(annotations.destructive_hint, Some(true));
                    assert_eq!(annotations.idempotent_hint, Some(false));
                    assert_eq!(annotations.open_world_hint, Some(false));
                }
                "memory_unify" => {
                    assert_eq!(annotations.read_only_hint, Some(false));
                    assert_eq!(annotations.destructive_hint, Some(true));
                    assert_eq!(annotations.idempotent_hint, Some(false));
                    assert_eq!(annotations.open_world_hint, Some(false));
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
            assert!(
                schema["required"]
                    .as_array()
                    .is_some_and(|required| required.iter().any(|value| value == "ok")),
                "tool {} output must require ok",
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
        assert_eq!(recall["properties"]["limit"]["maximum"], 100);
        assert_eq!(recall["properties"]["limit"]["default"], 20);
        assert_eq!(recall["properties"]["iris"]["minItems"], 1);
        assert_eq!(recall["properties"]["iris"]["maxItems"], 20);
        assert_eq!(recall["properties"]["depth"]["minimum"], 1);
        assert_eq!(recall["properties"]["depth"]["maximum"], 3);
        assert_eq!(
            recall["properties"]["detail"]["enum"],
            serde_json::json!(["concise", "detailed"])
        );
        assert!(recall["properties"].get("history").is_some());
        assert!(recall["properties"].get("semantic").is_none());

        let semantic = tools
            .iter()
            .find(|tool| tool.name == "memory_recall_semantic")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(semantic["required"], serde_json::json!(["scope", "text"]));
        assert_eq!(semantic["properties"]["limit"]["maximum"], 100);
        assert_eq!(semantic["properties"]["limit"]["default"], 20);
        assert!(semantic["properties"].get("iris").is_none());
        assert!(semantic["properties"].get("around").is_none());
    }

    #[test]
    fn output_schemas_use_only_v04_field_names() {
        for tool in Mindreader::tool_router().list_all() {
            let output = tool.output_schema.as_ref().expect("output schema");
            let properties = output
                .get("properties")
                .and_then(Value::as_object)
                .expect("output properties");
            for legacy in ["layers", "mergeSuggestions", "next", "targets", "ratings"] {
                assert!(
                    !properties.contains_key(legacy),
                    "{} advertises legacy output field {legacy}",
                    tool.name
                );
            }
        }
        let tools: Vec<_> = Mindreader::tool_router().list_all();
        let revise = tools
            .iter()
            .find(|tool| tool.name == "memory_revise")
            .unwrap()
            .output_schema
            .as_ref()
            .unwrap();
        assert!(revise["properties"].get("previousTarget").is_some());
        let withdraw = tools
            .iter()
            .find(|tool| tool.name == "memory_withdraw")
            .unwrap()
            .output_schema
            .as_ref()
            .unwrap();
        assert!(withdraw["properties"].get("withdrawnTargets").is_some());
    }

    #[test]
    fn instructions_and_descriptions_start_with_when_to_call() {
        let info = test_server().get_info();
        let instructions = info.instructions.expect("instructions");
        assert!(instructions.len() <= 512, "instructions exceed 512 chars");
        assert!(instructions.contains("OR-union") || instructions.contains("OR union"));
        assert!(instructions.contains("Recall") || instructions.contains("recall"));
        assert!(instructions.contains("memory_recall_semantic"));
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
        let success = map_tool_result(Ok(serde_json::json!({ "scope": [] })));
        assert_eq!(success.is_error, Some(false));
        assert_eq!(
            success
                .structured_content
                .as_ref()
                .and_then(|value| value.get("ok")),
            Some(&Value::Bool(true))
        );

        let invalid = map_tool_result(Err(DomainError::InvalidInput("bad layers".into()).into()));
        assert_eq!(reason_of(invalid), "invalid_input");

        let embedding = map_tool_result(Err(Error::Embedding("missing key".into())));
        assert_eq!(reason_of(embedding), "missing_embedding");

        let space = map_tool_result(Err(Error::EmbeddingSpace("dim mismatch".into())));
        let space_body = space
            .structured_content
            .expect("structured embedding_space");
        assert_eq!(space_body["reason"], "embedding_space");
        assert_eq!(space_body["retryable"], false);
        assert_eq!(space_body["outcome"], "not_applied");

        let wrapped_space = map_tool_result(Err(Error::EmbeddingSpace("dim mismatch".into())
            .context("query semantic activation vector index")));
        assert_eq!(reason_of(wrapped_space), "embedding_space");

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

        let timeout = structured_error("timeout", "late");
        let body = timeout.structured_content.expect("structured timeout");
        assert_eq!(body["retryable"], false);
        assert_eq!(body["outcome"], "unknown");
        let retryable = structured_error("connect_failed", "offline");
        let body = retryable
            .structured_content
            .expect("structured connect error");
        assert_eq!(body["retryable"], true);
        assert_eq!(body["outcome"], "not_applied");
    }

    #[test]
    fn token_bucket_reports_a_bounded_retry_delay() {
        let limiter = TokenBucket::new();
        for _ in 0..40 {
            assert_eq!(limiter.try_acquire(), Ok(()));
        }
        let retry_after_ms = limiter.try_acquire().expect_err("burst is exhausted");
        assert!((1..=500).contains(&retry_after_ms));
    }

    #[test]
    fn token_bucket_allows_an_eight_call_sequential_burst() {
        let limiter = TokenBucket::new();
        for _ in 0..8 {
            assert_eq!(limiter.try_acquire(), Ok(()));
        }
    }
}
