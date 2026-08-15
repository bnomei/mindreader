use anyhow::{Context, Result};
use std::path::Path;

#[derive(Debug, Clone)]
pub struct Config {
    pub uri: String,
    pub user: String,
    pub password: String,
    pub project: String,
}

pub const GLOBAL_LAYER: &str = "global";
pub const DEFAULT_PROJECT: &str = "project:graph-memory";

pub fn load_env() {
    let candidates = [Path::new("/workspace/mindreader/.env"), Path::new(".env")];
    for p in candidates {
        if p.exists() {
            let _ = dotenvy::from_filename(p);
        }
    }
    let _ = dotenvy::dotenv();
}

impl Config {
    pub fn from_env() -> Result<Self> {
        load_env();
        let password = std::env::var("NEO4J_PASSWORD")
            .context("NEO4J_PASSWORD is not set (see .env / /workspace/neo4j/STATUS.md)")?;
        Ok(Self {
            uri: std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://127.0.0.1:7687".into()),
            user: std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".into()),
            password,
            project: std::env::var("MINDREADER_PROJECT").unwrap_or_else(|_| DEFAULT_PROJECT.into()),
        })
    }
}
