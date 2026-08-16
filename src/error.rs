//! Typed error boundary shared by the application, adapters, and binaries.
//!
//! Failures preserve a source chain for diagnostics while exposing stable
//! variants for configuration, domain validation, Neo4j, embeddings, and
//! concurrent mutation. Transient Neo4j errors are classified for bounded
//! retries. The MCP adapter maps recoverable application failures to
//! `CallToolResult` structured errors (`isError` with `{ok:false,reason,message}`);
//! domain validation is not JSON-RPC `-32602`.

use crate::domain::DomainError;
use neo4rs::{Error as Neo4jError, Neo4jErrorKind};
use std::error::Error as StdError;
use thiserror::Error;

/// Crate-wide result alias using [`enum@Error`].
pub type Result<T> = std::result::Result<T, Error>;

/// Application error with preserved sources and retry-relevant variants.
#[derive(Debug, Error)]
pub enum Error {
    #[error(transparent)]
    Domain(#[from] DomainError),
    #[error(transparent)]
    Neo4j(#[from] Neo4jError),
    #[error("Neo4j row decoding failed: {0}")]
    Neo4jDecode(#[from] neo4rs::DeError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Dotenv(#[from] dotenvy::Error),
    #[error(transparent)]
    TomlDeserialize(#[from] toml::de::Error),
    #[error(transparent)]
    TomlSerialize(#[from] toml::ser::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
    #[error(transparent)]
    Http(#[from] reqwest::Error),
    #[error(
        "{provider} embedding request failed with HTTP {status}: {body} (request_id={request_id:?})"
    )]
    EmbeddingHttp {
        provider: &'static str,
        status: u16,
        request_id: Option<String>,
        body: String,
    },
    #[error("{0}")]
    Configuration(String),
    #[error("{0}")]
    Embedding(String),
    #[error("{0}")]
    Graph(String),
    #[error("{0}")]
    Operation(String),
    #[error("concurrent mutation changed {0}")]
    ConcurrentMutation(String),
    #[error("{message}: {source}")]
    Context {
        message: String,
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
    pub fn is_transient_neo4j(&self) -> bool {
        let mut current: Option<&(dyn StdError + 'static)> = Some(self);
        while let Some(error) = current {
            let direct = error.downcast_ref::<Neo4jError>();
            let wrapped = error.downcast_ref::<Error>().and_then(|error| match error {
                Error::Neo4j(driver) => Some(driver),
                _ => None,
            });
            if direct.or(wrapped).is_some_and(|driver| {
                matches!(driver, Neo4jError::Neo4j(error) if error.kind() == Neo4jErrorKind::Transient)
            }) {
                return true;
            }
            current = error.source();
        }
        false
    }
}

/// Adds operational context while retaining a typed source chain.
pub trait Context<T> {
    fn context(self, message: impl Into<String>) -> Result<T>;

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

#[macro_export]
macro_rules! config_error {
    ($($arg:tt)*) => {
        $crate::error::Error::Configuration(format!($($arg)*))
    };
}

#[macro_export]
macro_rules! embedding_error {
    ($($arg:tt)*) => {
        $crate::error::Error::Embedding(format!($($arg)*))
    };
}

#[macro_export]
macro_rules! graph_error {
    ($($arg:tt)*) => {
        $crate::error::Error::Graph(format!($($arg)*))
    };
}

#[macro_export]
macro_rules! operation_error {
    ($($arg:tt)*) => {
        $crate::error::Error::Operation(format!($($arg)*))
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
}
