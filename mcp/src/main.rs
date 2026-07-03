//! klartext-mcp binary: serve the diagnostic tools (reads + gated clear) over stdio.
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
        "klartext-mcp starting (reads + confirmation-gated clear_faults)"
    );

    let server = KlartextServer::new(config);
    let shutdown = server.clone();
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!(error = %e, "failed to start MCP server"))?;

    // Serve until the client closes the transport or the process is signalled;
    // either way, drop any held car session so it never dangles server-side.
    tokio::select! {
        result = service.waiting() => {
            result?;
            tracing::info!("client closed the transport");
        }
        () = shutdown_signal() => {
            tracing::info!("received shutdown signal");
        }
    }
    if shutdown.disconnect_now().await {
        tracing::info!("dropped the held car session on shutdown");
    }
    Ok(())
}

/// Resolve when the process receives SIGINT (Ctrl-C) or, on Unix, SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(error) => tracing::warn!(%error, "could not install the SIGTERM handler"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {}
        () = terminate => {}
    }
}
