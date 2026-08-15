use anyhow::Context;
use mindreader::Mindreader;
use rmcp::{transport::io::stdio, ServiceExt};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // MCP stdio: never write protocol-breaking bytes to stdout.
    eprintln!("mindreader: connecting to Neo4j and serving MCP on stdio");
    let server = Mindreader::connect()
        .await
        .context("failed to connect / bootstrap mindreader")?;
    eprintln!(
        "mindreader: tools={} project={}",
        Mindreader::registered_tool_names().join(","),
        server.project
    );
    let service = server.serve(stdio()).await.context("serve stdio")?;
    service.waiting().await?;
    Ok(())
}
