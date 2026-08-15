use anyhow::Context;
use mindreader::Mindreader;
use rmcp::{transport::io::stdio, ServiceExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // MCP stdio: never write protocol-breaking bytes to stdout.
    // Do not block initialize/list_tools on Neo4j.
    eprintln!("mindreader: serving MCP on stdio (Neo4j connects lazily)");
    let server = Mindreader::from_env().context("failed to load mindreader config")?;
    eprintln!(
        "mindreader: tools={} project={}",
        Mindreader::registered_tool_names().join(","),
        server.project
    );
    let warmup = server.clone();
    tokio::spawn(async move {
        if let Err(e) = warmup.ensure_connected().await {
            eprintln!("mindreader: neo4j warmup failed: {e:#}");
        }
    });
    let service = server.serve(stdio()).await.context("serve stdio")?;
    service.waiting().await?;
    Ok(())
}
