//! Mindreader: RDFS schema-as-data agent memory over Neo4j.
//!
//! This crate owns graph semantics. The stdio MCP adapter, smoke suite, and
//! bench call these modules instead of writing their own Cypher or visibility
//! policy. MCP exposes eight tools: ordinary and semantic recall, write,
//! revise, withdraw, judge, place, and unify.
//!
//! Facts are explicit triples with Episode provenance, request `scope`
//! (stored as `layers` memberships), signed feedback weights, soft retraction
//! (`validTo`), and intentional same-kind unify.

pub mod config;
pub mod domain;
pub mod embeddings;
pub mod error;
pub mod graph;
pub mod iri;
pub mod layers;
pub mod merge;
pub mod search;
pub mod semantic;
pub mod server;
pub mod service;
pub mod tools;

pub use server::Mindreader;
