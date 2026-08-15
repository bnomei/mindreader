use anyhow::Context;
use mindreader::Mindreader;
use rmcp::{transport::io::stdio, ServiceExt};
use serde_json::json;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // MCP stdio: never write protocol-breaking bytes to stdout.
    // Do not block initialize/list_tools on Neo4j.
    eprintln!(
        "{}",
        json!({
            "level": "info",
            "event": "startup",
            "message": "serving MCP on stdio (Neo4j connects lazily)"
        })
    );
    let server = Mindreader::from_env().context("failed to load mindreader config")?;
    eprintln!(
        "{}",
        json!({
            "level": "info",
            "event": "config",
            "tools": Mindreader::registered_tool_names(),
            "project": server.project
        })
    );
    let warmup = server.clone();
    tokio::spawn(async move {
        if let Err(e) = warmup.ensure_connected().await {
            eprintln!(
                "{}",
                json!({
                    "level": "error",
                    "event": "neo4j_warmup_failed",
                    "error": format!("{e:#}")
                })
            );
        }
    });
    let service = server.serve(stdio()).await.context("serve stdio")?;
    service.waiting().await?;
    Ok(())
}
