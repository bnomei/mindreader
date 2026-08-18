//! Typed error boundary shared by the application, adapters, and binaries.
//!
//! Failures preserve a source chain for diagnostics while exposing stable
//! variants for configuration, domain validation, Neo4j, embeddings, and
//! concurrent mutation. Transient Neo4j errors are classified for bounded
//! retries. The MCP adapter maps recoverable application failures to
//! `CallToolResult` structured errors (`isError` with
//! `{ok:false,reason,message,retryable}`); domain validation is not
//! JSON-RPC `-32602`.

use crate::domain::DomainError;
use neo4rs::{Error as Neo4jError, Neo4jErrorKind};
use std::error::Error as StdError;
use thiserror::Error;

/// Crate-wide result alias using [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Application error with preserved sources and retry-relevant variants.
#[derive(Debug, Error)]
pub enum Error {
    /// Wire or graph-precondition failure from the internal domain validator.
    #[error(transparent)]
    Domain(#[from] DomainError),
    /// Driver-level Neo4j failure; transient kinds may be retried.
    #[error(transparent)]
    Neo4j(#[from] Neo4jError),
    /// A returned row could not be decoded into the expected Bolt types.
    #[error("Neo4j row decoding failed: {0}")]
    Neo4jDecode(#[from] neo4rs::DeError),
    /// Filesystem failure while reading `config.toml`, the colocated `.env`, or similar.
    #[error(transparent)]
    Io(#[from] std::io::Error),
    /// Parse failure of the colocated `.env` secrets file.
    #[error(transparent)]
    Dotenv(#[from] dotenvy::Error),
    /// Invalid `config.toml`.
    #[error(transparent)]
    TomlDeserialize(#[from] toml::de::Error),
    /// Failed to encode default `config.toml` during directory init.
    #[error(transparent)]
    TomlSerialize(#[from] toml::ser::Error),
    /// JSON encode/decode failure at an adapter or tool boundary.
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    /// Transport failure talking to the embedding provider (not an HTTP status body).
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    /// Remote embedding HTTP status with a truncated body; 429 maps to MCP `rate_limited`.
    #[error(
        "{provider} embedding request failed with HTTP {status}: {body} (request_id={request_id:?})"
    )]
    EmbeddingHttp {
        /// Embedding provider id (`openai` or `xai`).
        provider: &'static str,
        /// HTTP status from the embedding API (429 → MCP `rate_limited`).
        status: u16,
        /// Optional `x-request-id` from the provider response.
        request_id: Option<String>,
        /// Truncated response body for diagnostics; never the API key.
        body: String,
    },
    /// Missing or invalid native configuration (including required `NEO4J_PASSWORD`).
    #[error("{0}")]
    Configuration(String),
    /// Embedding provider setup or request failure (MCP `missing_embedding` when no key).
    #[error("{0}")]
    Embedding(String),
    /// Stored activation index space does not match this process.
    #[error("{0}")]
    EmbeddingSpace(String),
    /// Bootstrap, Cypher identifier, or persistence invariant failure.
    #[error("{0}")]
    Graph(String),
    /// Tool or adapter failure that is not a domain validation.
    #[error("{0}")]
    Operation(String),
    /// A concurrent writer invalidated a precondition; MCP may retry.
    #[error("concurrent mutation changed {0}")]
    ConcurrentMutation(String),
    /// Commit returned an error after the transaction may already have applied.
    #[error("{operation} commit result could not be confirmed: {source}")]
    AmbiguousCommit {
        /// Tool or persistence operation whose commit result could not be confirmed.
        operation: &'static str,
        /// Driver error returned after the write may already have applied.
        #[source]
        source: Neo4jError,
    },
    /// Operational wrapper that keeps the typed source for retry classification.
    #[error("{message}: {source}")]
    Context {
        /// Human-readable wrap for logs and MCP `message`.
        message: String,
        /// Typed source retained for [`Error::is_transient_neo4j`].
        #[source]
        source: Box<dyn StdError + Send + Sync>,
    },
}

impl Error {
    /// Adds human-readable context while retaining this error as the source.
    pub fn context(self, message: impl Into<String>) -> Self {
        Self::Context {
            message: message.into(),
            source: Box::new(self),
        }
    }

    /// Walk the source chain for Neo4j transient kinds eligible for retry.
    ///
    /// [`Error::AmbiguousCommit`] is never retryable, even when the driver
    /// source is a transient kind, because the write may already have applied.
    pub fn is_transient_neo4j(&self) -> bool {
        if matches!(self, Self::AmbiguousCommit { .. }) {
            return false;
        }
        let mut current: Option<&(dyn StdError + 'static)> = Some(self);
        while let Some(error) = current {
            if error
                .downcast_ref::<Error>()
                .is_some_and(|error| matches!(error, Error::AmbiguousCommit { .. }))
            {
                return false;
            }
            let direct = error.downcast_ref::<Neo4jError>();
            let wrapped = error.downcast_ref::<Error>().and_then(|error| match error {
                Error::Neo4j(driver) => Some(driver),
                _ => None,
            });
            if direct.or(wrapped).is_some_and(neo4j_is_transient) {
                return true;
            }
            current = error.source();
        }
        false
    }
}

/// Only typed Neo4j `Transient` kinds are retryable; untyped driver strings are not.
fn neo4j_is_transient(error: &Neo4jError) -> bool {
    matches!(error, Neo4jError::Neo4j(error) if error.kind() == Neo4jErrorKind::Transient)
}

/// Adds operational context while retaining a typed source chain.
pub trait Context<T> {
    /// Wrap `Err` with a ready-made context message.
    fn context(self, message: impl Into<String>) -> Result<T>;

    /// Wrap `Err` with a lazily built context message.
    fn with_context<F>(self, message: F) -> Result<T>
    where
        F: FnOnce() -> String;
}

impl<T, E> Context<T> for std::result::Result<T, E>
where
    E: StdError + Send + Sync + 'static,
{
    fn context(self, message: impl Into<String>) -> Result<T> {
        self.map_err(|source| Error::Context {
            message: message.into(),
            source: Box::new(source),
        })
    }

    fn with_context<F>(self, message: F) -> Result<T>
    where
        F: FnOnce() -> String,
    {
        self.map_err(|source| Error::Context {
            message: message(),
            source: Box::new(source),
        })
    }
}

/// Construct a configuration failure from a format string.
#[macro_export]
macro_rules! config_error {
    ($($arg:tt)*) => {
        $crate::Error::Configuration(format!($($arg)*))
    };
}

/// Construct an embedding-provider failure from a format string.
#[macro_export]
macro_rules! embedding_error {
    ($($arg:tt)*) => {
        $crate::Error::Embedding(format!($($arg)*))
    };
}

/// Construct a graph/bootstrap failure from a format string.
#[macro_export]
macro_rules! graph_error {
    ($($arg:tt)*) => {
        $crate::Error::Graph(format!($($arg)*))
    };
}

/// Construct an operation failure from a format string.
#[macro_export]
macro_rules! operation_error {
    ($($arg:tt)*) => {
        $crate::Error::Operation(format!($($arg)*))
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_retains_typed_sources() {
        let error = std::fs::read("/definitely/not/mindreader")
            .context("read fixture")
            .unwrap_err();
        assert!(matches!(error, Error::Context { .. }));
        assert!(error
            .source()
            .and_then(|source| source.downcast_ref::<std::io::Error>())
            .is_some());
    }

    #[test]
    fn error_context_retains_the_typed_error_chain() {
        let error = Error::from(std::io::Error::other("offline")).context("after retries");
        let wrapped = error.source().expect("typed application error");
        assert!(matches!(
            wrapped.downcast_ref::<Error>(),
            Some(Error::Io(_))
        ));
    }

    #[test]
    fn untyped_driver_messages_are_not_retryable() {
        let error = Error::Neo4j(Neo4jError::UnexpectedMessage(
            "FAILURE code=Neo.TransientError.Transaction.DeadlockDetected".into(),
        ));
        assert!(!error.is_transient_neo4j());
    }
}
