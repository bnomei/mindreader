//! Mindreader: RDFS schema-as-data agent memory over Neo4j.
//!
//! This crate owns graph semantics. The stdio MCP adapter, smoke suite, and
//! bench call these modules for tool behavior and visibility policy instead of
//! inventing their own. MCP exposes eight tools: ordinary and semantic recall,
//! write, revise, withdraw, judge, place, and unify.
//!
//! Facts are explicit triples with Episode provenance, optional effective-time
//! intervals independent of transaction validity, request `scope` (stored as
//! `layers` memberships), signed judgment weights, soft withdrawal (`validTo`),
//! and intentional same-kind unify.

mod config;
mod domain;
mod embeddings;
mod error;
mod graph;
mod iri;
mod layers;
mod merge;
mod payload;
mod search;
mod semantic;
mod server;
mod service;
/// Graph mutations and closed-world `recall` walks used by [`service`].
mod tools;
mod vocabulary;

/// Error context extension used by the executable boundary.
pub use error::Context;
/// Application error returned while starting or serving the executable.
pub use error::Error;
/// Application result returned while starting or serving the executable.
pub use error::Result;
/// Stdio MCP server: eight tools, lazy Neo4j, and the process-local invoke limiter.
pub use server::Mindreader;

/// Explicitly unstable internals used only by feature-gated smoke tests and benchmarks.
#[cfg(feature = "developer-tools")]
pub mod developer {
    /// Native configuration types for developer binaries.
    pub mod config {
        pub use crate::config::{Config, EmbeddingSpace, SemanticConfig};
    }

    /// Wire/domain input records used to build live graph fixtures.
    pub mod domain {
        pub use crate::domain::{EntityInput, ObjectInput};
    }

    /// Embedding seam used by deterministic smoke and CPU benchmarks.
    pub mod embeddings {
        pub use crate::embeddings::{normalize_vector, EmbeddingProvider};
    }

    /// Error types and context helpers used by developer binaries.
    pub mod error {
        pub use crate::error::{Context, Error, Result};
    }

    /// Neo4j primitives needed by the live smoke and graph benchmark.
    pub mod graph {
        pub use crate::graph::{
            acquire_fact_locks_in_txn, bootstrap, connect, fetch_one, merge_node_in_txn,
            require_embedding_space, MergedNode, NodeSpec, SpaceReplace, MODEL_VERSION,
        };
    }

    /// Duplicate-candidate query used by the release graph benchmark.
    pub mod merge {
        pub use crate::merge::merge_suggestions_in_txn;
    }

    /// Deterministic semantic runtime construction for the live smoke suite.
    pub mod semantic {
        pub use crate::semantic::SemanticRuntime;
    }

    /// Application operations exercised by live developer binaries.
    pub mod service {
        pub use crate::domain::{EffectiveInterval, EffectiveUpdate};
        pub use crate::payload::ToolOutput;
        pub use crate::service::{
            JudgeArgs, MemoryService, PlaceArgs, RecallArgs, ReviseArgs, SemanticSearchArgs,
            UnifyArgs, WithdrawArgs, WriteArgs,
        };
        pub use crate::tools::{JudgeRating, PlaceEdit, TargetArgs, WriteFact};
    }

    /// MCP server used to verify eager developer connections.
    pub use crate::server::Mindreader;
}
