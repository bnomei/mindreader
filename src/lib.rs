//! Mindreader: RDFS schema-as-data agent memory over Neo4j.
//!
//! Library functions are the source of truth; the stdio MCP server and the
//! live smoke binary both call them.

pub mod config;
pub mod domain;
pub mod graph;
pub mod iri;
pub mod layers;
pub mod server;
pub mod service;
pub mod tools;

pub use server::Mindreader;
