//! klartext-mcp binary: serve the read-only diagnostic tools over stdio.
//!
//! CRITICAL: stdout carries only the JSON-RPC stream. ALL logging goes to stderr,
//! since any stray stdout write corrupts the transport and the client disconnects.

use anyhow::Result;
use clap::Parser;
use klartext_mcp::KlartextServer;
use klartext_mcp::config::ServerConfig;
use rmcp::ServiceExt;
use rmcp::transport::stdio;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    // ALL logging to stderr — stdout is the JSON-RPC transport only.
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let config = ServerConfig::parse();
    tracing::info!(
        semantic_db = %config.semantic_db.display(),
        "klartext-mcp starting (read-only)"
    );

    let service = KlartextServer::new(config)
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!(error = %e, "failed to start MCP server"))?;
    service.waiting().await?;
    Ok(())
}
