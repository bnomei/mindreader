//! Mindreader: RDFS schema-as-data agent memory over Neo4j.
//!
//! The library is the source of truth for graph semantics. Transport adapters
//! (stdio MCP), live smoke coverage, and benchmarks call these modules rather
//! than embedding their own Cypher or layer policy.
//!
//! Memory is stored as explicit graph triples with provenance episodes,
//! request-scoped multi-layer visibility, shared feedback weights, soft
//! retraction (`validTo`), and intentional same-kind entity merging.

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
