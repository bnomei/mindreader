//! Process entry for the stdio MCP server.
//!
//! Stdout is protocol-only. Initialize and `tools/list` do not wait on Neo4j;
//! the first tool call (or a background warmup) bootstraps the graph.
//! `--help` and `--version` may print to stdout and exit before serve.

use mindreader::operation_error;
use mindreader::{Context, Mindreader, Result};
use rmcp::{transport::io::stdio, ServiceExt};
use serde_json::json;

/// Stdout-legal `--help` text; only printed on the explicit CLI exit.
const HELP: &str = "mindreader - deterministic Neo4j-backed memory MCP server

Usage: mindreader [OPTIONS]

Options:
  -h, --help     Print help
  -V, --version  Print version";

#[tokio::main]
async fn main() -> Result<()> {
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
        args => {
            return Err(operation_error!(
                "unknown arguments: {}\n\n{HELP}",
                args.iter()
                    .map(|arg| format!("'{arg}'"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ));
        }
    }

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
    service.waiting().await.context("wait for stdio service")?;
    Ok(())
}
