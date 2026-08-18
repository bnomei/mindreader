//! MCP stdio adapter: eight tools, host-compatible schemas, lazy Neo4j.
//!
//! Advertises `recall`, `recall_semantic`, `write`, `revise`, `withdraw`,
//! `judge`, `place`, and `unify` as
//! plain tagged object schemas (no `anyOf`/`oneOf`/`allOf`). Initialize and
//! `tools/list` do not wait on Neo4j. Recoverable failures are structured
//! `isError` results. The 120/min burst-20 limiter and 45s timeout apply only
//! to `#[tool]` handlers through the process-local invoke path.

use crate::config::Config;
use crate::domain::DomainError;
use crate::error::{Error, Result as AppResult};
use crate::graph;
use crate::merge::UnifyArgs;
use crate::payload::ToolOutput;
use crate::search::RecallArgs;
use crate::semantic::{SemanticSearchArgs, MAX_SEMANTIC_TEXT_BYTES};
use crate::service::MemoryService;
use crate::tools::{JudgeArgs, PlaceArgs, ReviseArgs, WithdrawArgs, WriteArgs};
use neo4rs::Error as Neo4jError;
use rmcp::{
    handler::server::wrapper::Parameters,
    model::{
        CallToolResult, ErrorData as McpError, Implementation, InitializeRequestParams,
        InitializeResult, ProtocolVersion, ServerCapabilities, ServerInfo,
    },
    service::RequestContext,
    tool, tool_handler, tool_router, RoleServer, ServerHandler,
};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::error::Error as StdError;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::OnceCell;
use tokio::time::timeout;

/// Per-invoke ceiling applied only to MCP `#[tool]` handlers via [`Mindreader::invoke`].
const INVOKE_TIMEOUT: Duration = Duration::from_secs(45);
/// Sustained token refill for the process-local MCP invoke limiter.
const RATE_LIMIT_PER_MINUTE: f64 = 120.0;
/// Burst capacity of the same limiter (20 tokens).
const RATE_LIMIT_BURST: f64 = 20.0;
/// Protocol versions this process will negotiate; initialize rejects anything else.
///
/// `2026-07-28` is preferred. `2025-11-25` is accepted so hosts that have not
/// adopted the newer initialize version (including Grok) can still complete the
/// handshake. The serve layer echoes the requested version when it is listed here.
const SUPPORTED_PROTOCOL_VERSIONS: &[ProtocolVersion] =
    &[ProtocolVersion::V_2026_07_28, ProtocolVersion::V_2025_11_25];

/// Reject initialize requests that are not in [`SUPPORTED_PROTOCOL_VERSIONS`].
fn require_supported_protocol(requested: ProtocolVersion) -> Result<(), McpError> {
    if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
        Ok(())
    } else {
        Err(McpError::unsupported_protocol_version(
            requested,
            SUPPORTED_PROTOCOL_VERSIONS,
        ))
    }
}

/// Wrap a plain object schema; callers must not introduce `anyOf`/`oneOf`/`allOf`.
fn object_schema(value: serde_json::Value) -> Arc<rmcp::model::JsonObject> {
    Arc::new(rmcp::model::object(value))
}

/// `scope` array: empty is global-only; named ids are an OR union of kebab-case layers.
fn scope_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "array",
        "description": "Visibility union. [] selects global/unlayered records only. Named records match any requested layer. IDs use lowercase kebab-case with colon namespaces, for example project:mindreader or analysis:hypothesis; colons are naming, not hierarchy. When a task or host supplies scope, copy its exact values; a modified or invented ID is a different layer.",
        "uniqueItems": true,
        "items": {
            "type": "string",
            "pattern": "^[a-z0-9]+(?:-[a-z0-9]+)*(?::[a-z0-9]+(?:-[a-z0-9]+)*)*$"
        }
    })
}

/// Membership add/remove list using the same layer-id grammar as `scope`.
fn layer_list_schema(description: &str) -> serde_json::Value {
    let mut schema = scope_schema();
    schema["description"] = Value::String(description.to_string());
    schema
}

/// Closed `{kind, iri}` handle schema; `kind` is an enum so hosts never see a union.
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

/// Pasteable node-or-fact handle advertised to judge and place.
fn target_schema() -> serde_json::Value {
    target_schema_with_kinds(
        &["node", "fact"],
        "A pasteable current node or fact handle returned by Mindreader.",
    )
}

/// Pasteable node handle (`kind=node`) used by unify review and unify inputs.
fn node_target_schema(description: &str) -> serde_json::Value {
    target_schema_with_kinds(&["node"], description)
}

/// Pasteable current fact handle (`kind=fact`) used by revise, withdraw, and results.
fn fact_target_schema() -> serde_json::Value {
    target_schema_with_kinds(
        &["fact"],
        "A pasteable current fact handle returned by recall, write, or revise.",
    )
}

/// ASCII Neo4j label token; interpolated labels are still allowlisted at runtime.
fn label_schema() -> serde_json::Value {
    json!({
        "type": "string",
        "minLength": 1,
        "pattern": "^[A-Za-z][A-Za-z0-9_]*$"
    })
}

/// Non-empty unique predicate names or IRIs (around-mode `p`).
fn predicate_list_schema(description: &str) -> serde_json::Value {
    json!({
        "type": "array",
        "description": description,
        "minItems": 1,
        "uniqueItems": true,
        "items": { "type": "string", "minLength": 1 }
    })
}

/// Tagged `kind=node` subject bag; runtime requires `iri` or `name`.
fn entity_input_schema(description: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": description,
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

/// Tagged `node` or `literal` bag without schema unions; mixed fields fail at runtime.
fn object_input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "description": "The assertion object. Node values use kind=node with iri/name/labels; literal values use kind=literal with value and optional datatype.",
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

/// Half-open world-time interval. Presence is explicit even when both bounds are open.
fn effective_interval_schema(nullable: bool) -> serde_json::Value {
    json!({
        "type": if nullable { json!(["object", "null"]) } else { json!("object") },
        "description": "State-only half-open effective interval [from,to). Bounds are complete timezone-qualified RFC 3339 instants such as 2024-01-01T00:00:00Z, not date-only strings; omitted bounds are open. Model point events with event/date facts instead.",
        "additionalProperties": false,
        "properties": {
            "from": { "type": "string", "format": "date-time" },
            "to": { "type": "string", "format": "date-time" }
        }
    })
}

/// Host-compatible `write` input: `facts[]` plus call-level `scope`.
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
                "description": "Exactly 1–20 facts total. Split larger sets into additional calls before invoking and put focal durable claims first. This input-ordered batch is atomic and rolls back if any item fails.",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "s": entity_input_schema("The assertion subject: who or what the fact is about. Use iri for an established identity; otherwise use name and optional labels. Runtime validation requires at least one of iri or name."),
                        "p": {
                            "type": "string",
                            "minLength": 1,
                            "description": "Predicate name or Property IRI describing the relationship from subject s to object o. Reuse established vocabulary for the same concept."
                        },
                        "o": object_input_schema(),
                        "spike": {
                            "type": "string",
                            "enum": ["Signal", "Pattern", "Insight", "Knowledge"],
                            "description": "Optional commitment for this exact fact: Signal raw evidence, Pattern recurrence, Insight interpretation, Knowledge a fact worth relying on. Omit rather than invent."
                        },
                        "contradicts": {
                            "type": "boolean",
                            "description": "Set true only when this fact and visible current alternatives are directly incompatible and should remain current."
                        },
                        "effective": effective_interval_schema(true)
                    },
                    "required": ["s", "p", "o"]
                }
            },
            "scope": scope_schema()
        },
        "required": ["facts", "scope"]
    }))
}

/// Shared `isError` fields merged into every advertised output schema.
fn error_envelope_properties() -> serde_json::Map<String, Value> {
    let Value::Object(map) = json!({
        "ok": { "type": "boolean" },
        "reason": { "type": "string" },
        "message": { "type": "string" },
        "retryable": { "type": "boolean" },
        "retryAfterMs": { "type": "integer", "minimum": 1 }
    }) else {
        unreachable!("literal object")
    };
    map
}

/// Successful tool fields plus the recoverable-error envelope; `ok` is required.
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

/// Object-map helper so output schemas stay literal JSON without extra cloning.
fn props(value: Value) -> serde_json::Map<String, Value> {
    match value {
        Value::Object(map) => map,
        _ => serde_json::Map::new(),
    }
}

/// Agent-facing node or literal envelope, including mutability and pasteable target.
fn node_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
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
        "required": ["kind", "iri"]
    })
}

/// Serialized fact relationship used in paths, conflicts, and `about` rows.
fn relationship_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
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
            "reason": { "type": "string" },
            "spike": { "type": ["string", "null"], "enum": ["Signal", "Pattern", "Insight", "Knowledge", null] },
            "effective": effective_interval_schema(true)
        },
        "required": ["kind", "type", "iri", "from", "to", "propertyIri", "scope", "weight"]
    })
}

/// Compact path witnesses returned by recall surfaces; concise omits edge IRIs.
fn compact_witness_path_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "nodes": { "type": "array", "items": { "type": "string" } },
            "edges": {
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "iri": { "type": "string" },
                        "from": { "type": "string" },
                        "p": { "type": "string" },
                        "to": { "type": "string" }
                    },
                    "required": ["from", "p", "to"]
                }
            }
        },
        "required": ["nodes", "edges"]
    })
}

/// One set-valued alternative shown in `review.alternatives` (not a contradiction until asked).
fn conflict_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "relationship": relationship_schema(),
            "o": node_schema(),
            "p": { "type": "string" }
        },
        "required": ["relationship", "o", "p"]
    })
}

/// Agent-facing fact envelope: pasteable target, endpoints, memberships, and judgment flags.
fn fact_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "target": fact_target_schema(),
            "s": node_schema(),
            "p": { "type": "string" },
            "o": node_schema(),
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
            "validTo": { "type": "string" },
            "effective": effective_interval_schema(true),
            "transactionCurrent": { "type": "boolean" },
            "transaction": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "from": { "type": "string", "format": "date-time" },
                    "to": { "type": ["string", "null"], "format": "date-time" }
                },
                "required": ["from", "to"]
            }
        },
        "required": ["target", "s", "p", "o"]
    })
}

/// Recall fact items; `target` and operation flags are optional so concise payloads stay valid.
fn recall_fact_schema() -> Value {
    let mut schema = fact_schema();
    schema["required"] = json!(["s", "p", "o"]);
    schema
}

/// Fact envelope or null, used when a no-op revise returns no replacement body.
fn nullable_fact_schema() -> Value {
    let mut schema = fact_schema();
    schema["type"] = json!(["object", "null"]);
    schema
}

/// Neighbor Spike context attached to ranked recall (`about[]`).
fn about_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
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

/// Provenance Episode for a state-changing mutation; null on no-op.
fn episode_schema() -> Value {
    json!({
        "type": ["object", "null"],
        "additionalProperties": false,
        "properties": {
            "iri": { "type": "string" },
            "at": { "type": "string" },
            "tool": {
                "type": "string",
                "enum": [
                    "judge", "place", "revise", "unify", "withdraw", "write"
                ]
            }
        },
        "required": ["iri", "at", "tool"]
    })
}

/// `{kind: node, iri}` handle used in unify inputs and the paste bag.
fn node_handle_schema(description: &str) -> Value {
    target_schema_with_kinds(&["node"], description)
}

/// Neutral paste bag; empty arrays and nulls are unused roles, not commands.
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

/// Recall verbosity: answer-only `concise` or operation-ready `detailed`.
fn detail_schema() -> Value {
    json!({
        "type": "string",
        "enum": ["concise", "detailed"],
        "default": "detailed",
        "description": "concise returns answer-bearing graph content without handles, ranking, memberships, or operation eligibility. detailed is the full operation and audit envelope."
    })
}

/// Advisory unify suggestions and set-valued alternatives attached to write/revise.
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
                        "source": node_target_schema("Pasteable absorbed node handle."),
                        "sourceName": { "type": "string" },
                        "target": node_target_schema("Pasteable surviving node handle."),
                        "targetName": { "type": "string" },
                        "similarity": { "type": "number", "minimum": 0, "maximum": 1 }
                    },
                    "required": ["source", "sourceName", "target", "targetName", "similarity"]
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

/// Batch counts for judge and place (`requested` / `changed` / `noop`).
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

/// Token balance and last refill instant for the process-local limiter.
struct TokenBucketState {
    tokens: f64,
    last: Instant,
}

impl TokenBucket {
    /// Start full (burst 20) so the first 20 invokes do not wait on refill.
    fn new() -> Self {
        Self {
            inner: Mutex::new(TokenBucketState {
                tokens: RATE_LIMIT_BURST,
                last: Instant::now(),
            }),
        }
    }

    /// Take one token from the process-local 120/min burst-20 bucket.
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
    /// Filled on first invoke or warmup; initialize/`tools/list` leave it empty.
    service: Arc<OnceCell<MemoryService>>,
    /// Native config used on first Neo4j connect; unused by initialize/`tools/list`.
    cfg: Config,
    /// Process-local 120/min burst-20 bucket; never wraps in-process `tools::*`.
    limiter: Arc<TokenBucket>,
}

impl Mindreader {
    /// Construct the eight-tool router without connecting to Neo4j.
    fn from_config(cfg: Config) -> Self {
        Self {
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
        Fut: std::future::Future<Output = AppResult<ToolOutput>>,
    {
        if let Err(retry_after_ms) = self.limiter.try_acquire() {
            return Ok(structured_error_with_retry_after(
                "rate_limited",
                "MCP invoke rate limit exceeded (120/min, burst 20)",
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

/// Recoverable tool failure as `isError` with `{ok:false,reason,message,retryable}`.
fn structured_error(reason: &str, message: impl std::fmt::Display) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "ok": false,
        "reason": reason,
        "message": message.to_string(),
        "retryable": error_retryable(reason),
    }))
}

/// Rate-limit `isError` that includes `retryAfterMs` from the token bucket.
fn structured_error_with_retry_after(
    reason: &str,
    message: impl std::fmt::Display,
    retry_after_ms: u64,
) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "ok": false,
        "reason": reason,
        "message": message.to_string(),
        "retryable": error_retryable(reason),
        "retryAfterMs": retry_after_ms,
    }))
}

/// Only failures that are known safe to repeat are advertised as retryable.
fn error_retryable(reason: &str) -> bool {
    matches!(
        reason,
        "connect_failed" | "rate_limited" | "concurrent_mutation"
    )
}

/// Lazy Neo4j connect/bootstrap failure advertised as retryable `connect_failed`.
fn map_connect_error(error: Error) -> CallToolResult {
    structured_error("connect_failed", error)
}

/// Stamp `ok:true` on success objects, or map application errors to structured `isError`.
fn map_tool_result(result: AppResult<ToolOutput>) -> CallToolResult {
    match result {
        Ok(value) => {
            let mut value = value.into_object();
            value.insert("ok".into(), Value::Bool(true));
            CallToolResult::structured(Value::Object(value))
        }
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

/// Walk a boxed source chain so `Error::Context` still yields a stable MCP `reason`.
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

/// Host-compatible `recall` input: `scope` plus exactly one runtime selector.
fn schema_memory_recall() -> Arc<rmcp::model::JsonObject> {
    object_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "scope": scope_schema(),
            "text": {
                "type": "string",
                "minLength": 1,
                "description": "Lexical fact query. Use concrete entity and action terms; do not embed parameter syntax such as effectiveAt in the text. Use only when text is the selected recall mode."
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
                "description": "IRI mode only. 0 still returns incident facts on lookups[i].facts. 1 also fills top-level facts[] when detail is detailed. Concise IRI recall keeps its answer in lookups without duplicate top-level facts.",
                "enum": [0, 1],
                "default": 0
            },
            "p": predicate_list_schema("Around mode only: predicate names or IRIs to filter before limiting. Omit this filter until the stored predicate vocabulary is known, especially for counts and comparisons."),
            "depth": {
                "type": "integer",
                "description": "Around mode only: traversal depth.",
                "minimum": 1,
                "maximum": 3,
                "default": 1
            },
            "direction": {
                "type": "string",
                "description": "Around mode only: require every traversed edge to follow this direction from the start node.",
                "enum": ["both", "outgoing", "incoming"],
                "default": "both"
            },
            "history": {
                "type": "string",
                "minLength": 1,
                "description": "One node or fact IRI. Returns current and historical facts for that identity."
            },
            "effectiveAt": {
                "type": "string",
                "format": "date-time",
                "description": "Optional state-as-of filter for text, non-catalog labels, iris, or around. Only explicitly time-qualified facts effective at this instant match; unknown-time facts are excluded. Do not use it merely to search for dated point events."
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

/// Host-compatible `recall_semantic` input; `text` is required and embedded remotely.
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
            "effectiveAt": {
                "type": "string",
                "format": "date-time",
                "description": "Optional state-as-of filter. Direct, activation, and structural facts must be explicitly effective at this instant; unknown-time facts are excluded. Do not use it merely to search for dated point events."
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

/// Host-compatible `revise` input: fact handle plus replacement object.
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
            "reason": { "type": "string" },
            "effective": effective_interval_schema(true)
        },
        "required": ["scope", "target", "new"]
    }))
}

/// Host-compatible `withdraw` input; runtime requires exactly one of `target` or `subject`.
fn schema_memory_withdraw() -> Arc<rmcp::model::JsonObject> {
    object_schema(json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "scope": scope_schema(),
            "target": fact_target_schema(),
            "subject": entity_input_schema("Subject node whose visible outgoing facts will be withdrawn. Use iri for an established identity; otherwise use name and optional labels. Runtime validation requires at least one of iri or name."),
            "p": { "type": "string", "minLength": 1 },
            "reason": { "type": "string", "description": "Optional audit reason." }
        },
        "required": ["scope"]
    }))
}

/// Return one existing MCP input schema for an in-process developer adapter.
///
/// The LongMemEval harness uses this seam to present the exact same arguments to
/// OpenAI without constructing an MCP router or duplicating the wire contract.
#[cfg(feature = "developer-tools")]
pub fn developer_input_schema(name: &str) -> Option<Value> {
    let schema = match name {
        "recall" => schema_memory_recall(),
        "recall_semantic" => schema_memory_recall_semantic(),
        "write" => schema_memory_write(),
        "revise" => schema_memory_revise(),
        "withdraw" => schema_memory_withdraw(),
        _ => return None,
    };
    Some(Value::Object(schema.as_ref().clone()))
}

/// Map one application failure into the direct developer adapter's JSON dialect.
#[cfg(feature = "developer-tools")]
pub fn developer_error_payload(error: &Error) -> Value {
    let reason = classify_tool_error(error);
    let unknown = matches!(error, Error::AmbiguousCommit { .. });
    json!({
        "ok": false,
        "reason": reason,
        "message": error.to_string(),
        "retryable": !unknown && (error_retryable(reason) || error.is_transient_neo4j()),
        "outcome": if unknown { "unknown" } else { "not_applied" }
    })
}

/// Host-compatible `judge` input: 1–20 unique `strengthen`/`weaken` ratings.
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

/// Host-compatible `place` input: visibility `scope` plus membership `edits`.
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

/// Host-compatible `unify` input; no `scope` because unify is database-wide.
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

/// Closed-world recall output: facts, lookups, catalog nodes, and the paste bag.
fn schema_out_memory_recall() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "mode": { "type": "string", "enum": ["text", "iris", "labels", "catalog", "around", "history"] },
        "detail": { "type": "string", "enum": ["concise", "detailed"] },
        "handles": handles_schema(),
        "scope": scope_schema(),
        "facts": { "type": "array", "items": recall_fact_schema() },
        "nodes": { "type": "array", "items": node_schema() },
        "paths": {
            "type": "array",
            "description": "For around recall, paths[i] is the deterministic best compact witness path for facts[i].",
            "items": compact_witness_path_schema()
        },
        "revisions": {
            "type": "array",
            "description": "History-only exact correction events, newest first. SUPERSEDES metadata is audit-only and never pasteable.",
            "items": {
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "replacement": fact_target_schema(),
                    "previous": fact_target_schema(),
                    "scope": scope_schema(),
                    "episode": episode_schema(),
                    "supersedes": {
                        "type": "object",
                        "additionalProperties": false,
                        "properties": {
                            "iri": { "type": "string" },
                            "from": { "type": "string" },
                            "to": { "type": "string" },
                            "reason": { "type": ["string", "null"] }
                        },
                        "required": ["iri", "from", "to", "reason"]
                    }
                },
                "required": ["replacement", "previous", "scope", "episode", "supersedes"]
            }
        },
        "revisionsTruncated": { "type": "boolean" },
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
                    "facts": { "type": "array", "items": recall_fact_schema() },
                    "truncated": { "type": "boolean" }
                },
                "required": ["iri", "found", "truncated"]
            }
        },
        "from": { "type": "string" },
        "truncated": { "type": "boolean" }
    })))
}

/// Semantic recall output; `mode` is always `semantic` and activations may have been updated.
fn schema_out_memory_recall_semantic() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "mode": { "type": "string", "enum": ["semantic"] },
        "detail": { "type": "string", "enum": ["concise", "detailed"] },
        "handles": handles_schema(),
        "scope": scope_schema(),
        "facts": { "type": "array", "items": recall_fact_schema() },
        "nodes": { "type": "array", "items": node_schema() },
        "paths": {
            "type": "array",
            "items": compact_witness_path_schema()
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
                    "facts": { "type": "array", "items": recall_fact_schema() }
                },
                "required": ["iri", "found"]
            }
        },
        "from": { "type": "string" },
        "truncated": { "type": "boolean" }
    })))
}

/// Write output: one Episode when changed, written facts, and advisory review.
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

/// Revise output: current and previous fact handles plus optional replacement body.
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

/// Withdraw output: soft-retired fact handles; Episode is null when nothing changed.
fn schema_out_memory_withdraw() -> Arc<rmcp::model::JsonObject> {
    schema_out(props(json!({
        "scope": scope_schema(),
        "noop": { "type": "boolean" },
        "episode": episode_schema(),
        "withdrawn": { "type": "integer" },
        "withdrawnTargets": { "type": "array", "items": fact_target_schema() },
        "reason": { "type": ["string", "null"] },
        "handles": handles_schema()
    })))
}

/// Judge output: per-rating before/after weights and a single Episode.
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

/// Place output: per-edit before/after memberships and a single Episode if any changed.
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

/// Unify output: surviving node after a permanent same-kind merge.
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
        name = "recall",
        title = "Recall visible memory",
        description = "Use proactively before acting or deciding whenever current work may depend on durable context from prior sessions: decisions, rationale, preferences, standing instructions, constraints, identities, relationships, conventions, commitments, project state, or lessons. Also use when resuming or revisiting work; the user need not request recall. This read makes no external calls or graph writes. Text recall safely combines an exact phrase with bounded OR-keyword matching of terms at least four Unicode characters long. Pass exactly one of text, iris (1–20 node IRIs), labels, around, or history. For comparisons and counts, retrieve every named anchor and avoid an around predicate filter until stored vocabulary is known. limit defaults to 20 and is at most 100. hops applies only to iris; p, direction, and depth only to around, where they constrain every traversed edge. effectiveAt is only a state-as-of filter: it excludes unknown-time facts and is invalid for history, point-event lookup, and Class/Property catalog recall. history accepts one node or fact IRI and returns exact revision events plus transaction/effective-time facts. Use concise for answer-only content or detailed when handles and operation/audit metadata may be needed. Class or Property labels read the global catalog.",
        input_schema = schema_memory_recall(),
        output_schema = schema_out_memory_recall(),
        annotations(title = "Recall visible memory", read_only_hint = true, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    /// Closed-world read: one selector, no embedding, no graph writes; limited and timed via `invoke`.
    async fn recall(
        &self,
        Parameters(args): Parameters<RecallArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.recall(args).await })
            .await
    }

    #[tool(
        name = "recall_semantic",
        title = "Recall by meaning",
        description = "Use only when autonomous recall is warranted but lexical recall cannot find conceptually related knowledge. Do not use it as the default first recall. Every call combines exact and bounded keyword candidates with matching semantic activations, so untouched topics can surface lexically before later paraphrases reuse their activation group. Exact evidence outweighs keyword-only evidence, and repeated activations cannot amplify a fact by accumulation. Grounded anchors may surface bounded degree-normalized one-hop ASSERTS context; expanded facts stay weaker than anchors and never teach activations. A query with no results creates no activation. text is required, limited to 32 KiB UTF-8, and sent to the configured embedding provider; labels optionally filter results. Optional effectiveAt filters direct, activation, and structural ordinary facts to explicitly qualified intervals containing that world-time instant. limit defaults to 20 and is at most 100. Use concise for answer-only content or detailed when handles and operation/audit metadata may be needed. The call may maintain expiring semantic activations, so it is neither read-only nor idempotent.",
        input_schema = schema_memory_recall_semantic(),
        output_schema = schema_out_memory_recall_semantic(),
        annotations(title = "Recall by meaning", read_only_hint = false, destructive_hint = false, idempotent_hint = false, open_world_hint = true)
    )]
    /// Embeds `text` and may persist expiring activations; not read-only; limited and timed via `invoke`.
    async fn recall_semantic(
        &self,
        Parameters(args): Parameters<SemanticSearchArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.recall_semantic(args).await })
            .await
    }

    #[tool(
        name = "write",
        title = "Write facts",
        description = "Use proactively whenever discussion, investigation, decision-making, implementation, debugging, review, or handoff establishes knowledge another agent or session should reuse: identities and relationships, preferences and standing instructions, decisions and rationale, requirements and constraints, conventions, durable commitments, stable project facts, or reusable signals, patterns, and insights. The user need not ask. Do not store secrets, chatter, transient status, raw dumps, or unsupported inference. Preserve the focal durable assertion before adjacent detail. facts contains exactly 1–20 input-ordered items under one call-level scope; split larger sets before calling and put focal claims first. The atomic batch records one Episode only when something changes. An ordinary state fact may include a complete timezone-qualified half-open effective interval [from,to), independent of transaction history; structural relationships reject it, and point events should use event/date facts instead. spike classifies only that exact fact and never creates ABOUT context; write ABOUT explicitly when intended. Exact subject/property/object/effective-interval reassertions merge memberships, and an explicit spike reclassifies that fact. Review review.unify and review.alternatives as advisory queues.",
        input_schema = schema_memory_write(),
        output_schema = schema_out_memory_write(),
        annotations(title = "Write facts", read_only_hint = false, destructive_hint = false, idempotent_hint = true, open_world_hint = false)
    )]
    async fn write(
        &self,
        Parameters(args): Parameters<WriteArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.write(args).await })
            .await
    }

    #[tool(
        name = "revise",
        title = "Revise a fact",
        description = "Use when current work establishes that one exact current fact or its effective interval is wrong and its replacement is known, whether or not the user requested a correction. Recall or reuse its fact-only target, then supply the new object. scope selects the memberships moved from the previous fact; unrelated values and memberships remain. The replacement preserves the previous fact's spike classification unless a new spike is supplied. Omitted effective inherits the old interval, explicit null clears temporal qualification, and an object replaces the interval. The replacement and SUPERSEDES history commit in one transaction, and the response returns both current target and previousTarget.",
        input_schema = schema_memory_revise(),
        output_schema = schema_out_memory_revise(),
        annotations(title = "Revise a fact", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn revise(
        &self,
        Parameters(args): Parameters<ReviseArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.revise(args).await })
            .await
    }

    #[tool(
        name = "withdraw",
        title = "Withdraw facts",
        description = "Use when current work establishes that a stored fact is obsolete, no longer true, or should not remain current and no replacement is known. Prefer one recalled current fact target; use subject with optional p only when every visible current fact in that slice should be withdrawn. Exactly one of target or subject is required. The operation preserves validity history, never hard-deletes nodes or facts, and records one Episode only when something changes.",
        input_schema = schema_memory_withdraw(),
        output_schema = schema_out_memory_withdraw(),
        annotations(title = "Withdraw facts", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn withdraw(
        &self,
        Parameters(args): Parameters<WithdrawArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.withdraw(args).await })
            .await
    }

    #[tool(
        name = "judge",
        title = "Judge retrieved targets",
        description = "Use after the agent actually relied on a recalled node or fact and it materially helped, distracted, or misled the work. Judge retrieval utility, not truth: revise or withdraw a false or stale claim instead, and do not rate every result. ratings contains 1–20 unique targets; strengthen or weaken changes each shared weight by exactly +1 or -1. The input-ordered batch is atomic, records one Episode, and rolls back fully on any failure. Recall itself never changes weight.",
        input_schema = schema_memory_judge(),
        output_schema = schema_out_memory_judge(),
        annotations(title = "Judge retrieved targets", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn judge(
        &self,
        Parameters(args): Parameters<JudgeArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.judge(args).await })
            .await
    }

    #[tool(
        name = "place",
        title = "Place layer memberships",
        description = "Use when the agent determines that existing memory belongs in additional, fewer, or different project, team, task, or analysis layers without changing the fact itself. This changes visibility membership, not authorization. Apply 1–20 unique node or fact edits atomically; scope controls current visibility and each edit supplies add and/or remove. Include literal fact endpoints in the same batch as {kind:\"node\", iri}. The batch is validated against its combined final endpoint-closure state, records one Episode if changed, and rolls back fully on failure.",
        input_schema = schema_memory_place(),
        output_schema = schema_out_memory_place(),
        annotations(title = "Place layer memberships", read_only_hint = false, destructive_hint = true, idempotent_hint = true, open_world_hint = false)
    )]
    async fn place(
        &self,
        Parameters(args): Parameters<PlaceArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.place(args).await })
            .await
    }

    #[tool(
        name = "unify",
        title = "Permanently unify nodes",
        description = "Use only when current evidence confirms that two same-kind nodes represent the same identity, after reviewing any review.unify suggestion and deciding which IRI and name must survive. Never unify merely because names are similar. source and target are pasteable node handles {kind:\"node\", iri}. This database-wide operation permanently absorbs source into target; reverse the pair when the other identity should survive. It has no scope because all memberships and history must be reconciled.",
        input_schema = schema_memory_unify(),
        output_schema = schema_out_memory_unify(),
        annotations(title = "Permanently unify nodes", read_only_hint = false, destructive_hint = true, idempotent_hint = false, open_world_hint = false)
    )]
    async fn unify(
        &self,
        Parameters(args): Parameters<UnifyArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.invoke(|service| async move { service.unify(args).await })
            .await
    }
}

#[tool_handler]
impl ServerHandler for Mindreader {
    /// Advertise tools-only capabilities, preferred protocol 2026-07-28, and the eight-tool instructions.
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_protocol_version(ProtocolVersion::V_2026_07_28)
            .with_server_info(Implementation::new(
                "mindreader",
                env!("CARGO_PKG_VERSION"),
            ))
            .with_instructions(
                "The agent owns memory: proactively recall when work may depend on prior decisions, preferences, constraints, identities, conventions, project state, or lessons; proactively write future-useful facts[], rationale, and insights learned during work. scope is an OR-union ([] is global-only). Use recall_semantic only after lexical recall is insufficient. Paste returned target handles into mutations. Never store secrets, transient chatter, or raw dumps; never assert CONTRADICTS/SUPERSEDES.",
            )
    }

    /// Negotiate `2026-07-28` or `2025-11-25`; unknown versions fail initialize.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(SUPPORTED_PROTOCOL_VERSIONS)
    }

    /// Handshake without Neo4j: accept a supported version and echo it on [`Self::get_info`].
    async fn initialize(
        &self,
        request: InitializeRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<InitializeResult, McpError> {
        require_supported_protocol(request.protocol_version.clone())?;
        let mut info = self.get_info();
        info.protocol_version = request.protocol_version;
        Ok(info)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        classify_tool_error, map_connect_error, map_tool_result, require_supported_protocol,
        structured_error, Mindreader, TokenBucket,
    };
    use crate::config::Config;
    use crate::domain::DomainError;
    use crate::error::Error;
    use crate::payload::ToolOutput;
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

    fn assert_object_schemas_are_closed(value: &Value, tool: &str) {
        match value {
            Value::Object(map) => {
                let is_object = map
                    .get("type")
                    .is_some_and(|schema_type| match schema_type {
                        Value::String(value) => value == "object",
                        Value::Array(values) => values.iter().any(|value| value == "object"),
                        _ => false,
                    });
                if is_object {
                    assert_eq!(
                        map.get("additionalProperties"),
                        Some(&Value::Bool(false)),
                        "tool {tool} contains an open object schema: {value}"
                    );
                }
                for child in map.values() {
                    assert_object_schemas_are_closed(child, tool);
                }
            }
            Value::Array(values) => {
                for child in values {
                    assert_object_schemas_are_closed(child, tool);
                }
            }
            _ => {}
        }
    }

    #[test]
    fn registers_eight_tools() {
        let names = Mindreader::registered_tool_names();
        let expected = [
            "judge",
            "place",
            "recall",
            "recall_semantic",
            "revise",
            "unify",
            "withdraw",
            "write",
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
            .find(|tool| tool.name == "write")
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
            item_props["effective"]["type"],
            serde_json::json!(["object", "null"])
        );
        assert_eq!(item_props["effective"]["additionalProperties"], false);
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
            .find(|tool| tool.name == "revise")
            .unwrap()
            .schema_as_json_value();
        let revise_required = revise_schema["required"].as_array().unwrap();
        for name in ["scope", "target", "new"] {
            assert!(revise_required.iter().any(|value| value == name));
        }
        assert_eq!(revise_schema["properties"]["new"]["type"], "object");
        assert_eq!(
            revise_schema["properties"]["effective"]["type"],
            serde_json::json!(["object", "null"])
        );
        assert_eq!(
            revise_schema["properties"]["target"]["properties"]["kind"]["enum"],
            serde_json::json!(["fact"])
        );

        let withdraw_schema = tools
            .iter()
            .find(|tool| tool.name == "withdraw")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(
            withdraw_schema["properties"]["target"]["properties"]["kind"]["enum"],
            serde_json::json!(["fact"])
        );

        let judge_schema = tools
            .iter()
            .find(|tool| tool.name == "judge")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(
            judge_schema["properties"]["ratings"]["items"]["properties"]["mode"]["enum"],
            serde_json::json!(["strengthen", "weaken"])
        );
        assert_eq!(judge_schema["properties"]["ratings"]["maxItems"], 20);

        let place_schema = tools
            .iter()
            .find(|tool| tool.name == "place")
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
    fn recall_schemas_advertise_effective_time_without_schema_unions() {
        let tools: Vec<_> = Mindreader::tool_router().list_all();
        for name in ["recall", "recall_semantic"] {
            let schema = tools
                .iter()
                .find(|tool| tool.name == name)
                .unwrap()
                .schema_as_json_value();
            assert_eq!(schema["properties"]["effectiveAt"]["format"], "date-time");
            assert_eq!(schema["properties"]["effectiveAt"]["type"], "string");
        }
    }

    #[test]
    fn scoped_tools_require_scope() {
        let router = Mindreader::tool_router();
        for tool in router.list_all() {
            let schema = tool.schema_as_json_value();
            let required = schema["required"].as_array().cloned().unwrap_or_default();
            if tool.name == "unify" {
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
    fn get_info_prefers_2026_07_28_and_discovery_lists_host_compat() {
        let info = test_server().get_info();
        assert_eq!(
            info.protocol_version,
            rmcp::model::ProtocolVersion::V_2026_07_28
        );
        let tools = info.capabilities.tools.expect("tools capability");
        assert_ne!(tools.list_changed, Some(true));
        assert!(info.capabilities.prompts.is_none());
        assert!(info.capabilities.resources.is_none());
        let versions = test_server().supported_protocol_versions();
        assert_eq!(
            versions.as_ref(),
            &[
                rmcp::model::ProtocolVersion::V_2026_07_28,
                rmcp::model::ProtocolVersion::V_2025_11_25,
            ]
        );

        let discovery = rmcp::model::DiscoverResult::from_server_info(
            versions.into_owned(),
            test_server().get_info(),
        );
        let discovery = serde_json::to_value(discovery).expect("serialize discovery");
        assert_eq!(discovery["resultType"], "complete");
        assert_eq!(
            discovery["supportedVersions"],
            serde_json::json!(["2026-07-28", "2025-11-25"])
        );
    }

    #[test]
    fn accepts_2025_11_25_and_rejects_older_or_unknown_initialize_protocols() {
        assert!(require_supported_protocol(rmcp::model::ProtocolVersion::V_2026_07_28).is_ok());
        assert!(require_supported_protocol(rmcp::model::ProtocolVersion::V_2025_11_25).is_ok());
        for version in rmcp::model::ProtocolVersion::KNOWN_VERSIONS {
            if version != &rmcp::model::ProtocolVersion::V_2026_07_28
                && version != &rmcp::model::ProtocolVersion::V_2025_11_25
            {
                let error = require_supported_protocol(version.clone())
                    .expect_err("older protocol must be rejected");
                assert_eq!(error.message, "Unsupported protocol version");
            }
        }
        let unknown = serde_json::from_str::<rmcp::model::ProtocolVersion>("\"2099-01-01\"")
            .expect("deserialize open protocol version");
        require_supported_protocol(unknown).expect_err("unknown protocol must be rejected");
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
                "recall" => {
                    assert_eq!(annotations.read_only_hint, Some(true));
                    assert_eq!(annotations.destructive_hint, Some(false));
                    assert_eq!(annotations.idempotent_hint, Some(true));
                    assert_eq!(annotations.open_world_hint, Some(false));
                }
                "recall_semantic" => {
                    assert_eq!(annotations.read_only_hint, Some(false));
                    assert_eq!(annotations.destructive_hint, Some(false));
                    assert_eq!(annotations.idempotent_hint, Some(false));
                    assert_eq!(annotations.open_world_hint, Some(true));
                }
                "write" => {
                    assert_eq!(annotations.read_only_hint, Some(false));
                    assert_eq!(annotations.destructive_hint, Some(false));
                    assert_eq!(annotations.idempotent_hint, Some(true));
                    assert_eq!(annotations.open_world_hint, Some(false));
                }
                "place" => {
                    assert_eq!(annotations.read_only_hint, Some(false));
                    assert_eq!(annotations.destructive_hint, Some(true));
                    assert_eq!(annotations.idempotent_hint, Some(true));
                    assert_eq!(annotations.open_world_hint, Some(false));
                }
                "revise" | "withdraw" | "judge" => {
                    assert_eq!(annotations.read_only_hint, Some(false));
                    assert_eq!(annotations.destructive_hint, Some(true));
                    assert_eq!(annotations.idempotent_hint, Some(false));
                    assert_eq!(annotations.open_world_hint, Some(false));
                }
                "unify" => {
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
            assert_object_schemas_are_closed(&schema, &tool.name);
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
            .find(|tool| tool.name == "recall")
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
            .find(|tool| tool.name == "recall_semantic")
            .unwrap()
            .schema_as_json_value();
        assert_eq!(semantic["required"], serde_json::json!(["scope", "text"]));
        assert_eq!(semantic["properties"]["limit"]["maximum"], 100);
        assert_eq!(semantic["properties"]["limit"]["default"], 20);
        assert!(semantic["properties"].get("iris").is_none());
        assert!(semantic["properties"].get("around").is_none());
    }

    #[test]
    fn output_schemas_use_only_current_field_names() {
        for tool in Mindreader::tool_router().list_all() {
            let output = tool.output_schema.as_ref().expect("output schema");
            let rendered = serde_json::to_string(output).expect("serialize output schema");
            for legacy in [
                "layers",
                "mergeSuggestions",
                "next",
                "targets",
                "ratings",
                "retracted",
                "propertyStub",
                "propertyCreated",
                "unifySuggestions",
                "siblings",
            ] {
                assert!(
                    !rendered.contains(&format!("\"{legacy}\":")),
                    "{} advertises legacy output field {legacy}",
                    tool.name
                );
            }
        }
        let tools: Vec<_> = Mindreader::tool_router().list_all();
        let revise = tools
            .iter()
            .find(|tool| tool.name == "revise")
            .unwrap()
            .output_schema
            .as_ref()
            .unwrap();
        assert!(revise["properties"].get("previousTarget").is_some());
        let withdraw = tools
            .iter()
            .find(|tool| tool.name == "withdraw")
            .unwrap()
            .output_schema
            .as_ref()
            .unwrap();
        assert!(withdraw["properties"].get("withdrawnTargets").is_some());
        assert!(withdraw["properties"].get("withdrawn").is_some());
    }

    #[test]
    fn instructions_and_descriptions_define_autonomous_when_to_call() {
        let info = test_server().get_info();
        let instructions = info.instructions.expect("instructions");
        assert!(instructions.len() <= 512, "instructions exceed 512 chars");
        assert!(instructions.contains("agent owns memory"));
        assert!(instructions.contains("proactively recall"));
        assert!(instructions.contains("proactively write"));
        assert!(instructions.contains("OR-union") || instructions.contains("OR union"));
        assert!(instructions.contains("Recall") || instructions.contains("recall"));
        assert!(instructions.contains("recall_semantic"));
        assert!(instructions.contains("facts[]"));
        assert!(instructions.contains("target"));
        assert!(instructions.contains("CONTRADICTS"));
        let tools = Mindreader::tool_router().list_all();
        for tool in &tools {
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
        let recall = tools
            .iter()
            .find(|tool| tool.name == "recall")
            .unwrap()
            .description
            .as_deref()
            .unwrap();
        assert!(recall.contains("proactively"));
        assert!(recall.contains("user need not request recall"));
        let write = tools
            .iter()
            .find(|tool| tool.name == "write")
            .unwrap()
            .description
            .as_deref()
            .unwrap();
        assert!(write.contains("proactively"));
        assert!(write.contains("The user need not ask"));
        assert!(write.contains("future") || write.contains("another agent or session"));
    }

    #[test]
    fn map_tool_result_returns_structured_error() {
        let success = map_tool_result(Ok(ToolOutput::from_value(
            serde_json::json!({ "scope": [] }),
        )
        .unwrap()));
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
        assert!(space_body.get("outcome").is_none());

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
        assert!(body.get("outcome").is_none());
        let retryable = structured_error("connect_failed", "offline");
        let body = retryable
            .structured_content
            .expect("structured connect error");
        assert_eq!(body["retryable"], true);
        assert!(body.get("outcome").is_none());
    }

    #[test]
    fn token_bucket_reports_a_bounded_retry_delay() {
        let limiter = TokenBucket::new();
        for _ in 0..20 {
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
