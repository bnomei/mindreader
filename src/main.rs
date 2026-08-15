use anyhow::Context;
use mindreader::Mindreader;
use rmcp::{transport::io::stdio, ServiceExt};
use serde_json::json;

const HELP: &str = "mindreader - deterministic Neo4j-backed memory MCP server

Usage: mindreader [OPTIONS]

Options:
  -h, --help     Print help
  -V, --version  Print version";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    match std::env::args().skip(1).collect::<Vec<_>>().as_slice() {
        [] => {}
        [arg] if arg == "-h" || arg == "--help" => {
            println!("{HELP}");
            return Ok(());
        }
        [arg] if arg == "-V" || arg == "--version" => {
            println!("mindreader {}", env!("CARGO_PKG_VERSION"));
            return Ok(());
        }
        args => anyhow::bail!(
            "unknown arguments: {}\n\n{HELP}",
            args.iter()
                .map(|arg| format!("'{arg}'"))
                .collect::<Vec<_>>()
                .join(" ")
        ),
    }

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
            "tools": Mindreader::registered_tool_names()
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
