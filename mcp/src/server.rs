//! The MCP server: read-only diagnostic tools over a held car session.
//!
//! [`KlartextServer`] is the rmcp [`ServerHandler`] served over stdio. It holds an
//! optional car connection in shared state and exposes only non-mutating tools.
//! This module starts with `disconnect`; the read tools are added alongside it as
//! the milestone progresses.

use std::sync::Arc;

use klartext_semantic::Catalog;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::dto::{DisconnectResult, ListEcusResult};
use crate::ecu;

/// Shared server state: an optional held car connection (`None` = not connected).
type SessionState = Arc<Mutex<Option<()>>>;

/// The klartext read-only MCP server; a cloneable handle over shared state.
#[derive(Clone)]
pub struct KlartextServer {
    config: Arc<ServerConfig>,
    state: SessionState,
    tool_router: ToolRouter<KlartextServer>,
}

impl std::fmt::Debug for KlartextServer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KlartextServer")
            .field("config", &self.config)
            .finish_non_exhaustive()
    }
}

impl KlartextServer {
    /// Build the server from `config`. Does **not** connect to the car.
    pub fn new(config: ServerConfig) -> Self {
        Self {
            config: Arc::new(config),
            state: Arc::new(Mutex::new(None)),
            tool_router: Self::tool_router(),
        }
    }

    /// The names of the tools this server advertises.
    pub fn advertised_tools(&self) -> Vec<String> {
        self.tool_router
            .list_all()
            .into_iter()
            .map(|t| t.name.to_string())
            .collect()
    }

    /// Open the semantic catalog read-only, or `None` when unavailable.
    ///
    /// A missing or unreadable DB downgrades reads to raw codes/names rather than
    /// failing; the absence is logged to stderr.
    fn catalog(&self) -> Option<Catalog> {
        match Catalog::open(&self.config.semantic_db) {
            Ok(catalog) => Some(catalog),
            Err(error) => {
                tracing::warn!(%error, "semantic DB unavailable; using raw codes/names only");
                None
            }
        }
    }
}

#[tool_router]
impl KlartextServer {
    /// Close the diagnostic session and release the car connection.
    ///
    /// # Errors
    /// Infallible today; returns `Result` to match the tool signature shape.
    #[tool(description = "Close the diagnostic session and release the car \
        connection. Safe to call when not connected.")]
    pub async fn disconnect(&self) -> Result<Json<DisconnectResult>, McpError> {
        let was_connected = self.state.lock().await.take().is_some();
        Ok(Json(DisconnectResult { was_connected }))
    }

    /// List the ECUs the read tools can target.
    ///
    /// # Errors
    /// Infallible today; returns `Result` to match the tool signature shape.
    #[tool(description = "List the ECUs the read tools can target: built-in BMW \
        aliases plus, when the ISTA semantic DB is present, the full per-model ECU \
        address map. Does not require a connection.")]
    pub async fn list_ecus(&self) -> Result<Json<ListEcusResult>, McpError> {
        let catalog = self.catalog();
        let db_available = catalog.is_some();
        let ecus = ecu::list(catalog.as_ref());
        let note = if db_available {
            "Built-in aliases merged with the ISTA ECU map.".to_string()
        } else {
            "Built-in aliases only (no semantic DB). Target other ECUs by raw hex \
             address like 0x12."
                .to_string()
        };
        Ok(Json(ListEcusResult {
            ecus,
            db_available,
            note,
        }))
    }
}

#[tool_handler]
impl ServerHandler for KlartextServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(
                "Read-only BMW F-series diagnostics. Call connect first (discovers the \
                 gateway or uses a configured IP, reads the VIN). Then read_faults and \
                 read_data target an ECU by name (\"DME\"), hex address (\"0x12\"), or ISTA \
                 group name (\"d_0012\"); list_ecus enumerates targetable ECUs. This server \
                 cannot clear faults, actuate, or code — those are intentionally absent and \
                 live in the CLI with a human in the loop. Fault text and the full ECU map \
                 come from the ISTA SQLiteDB; reads still work (raw) without it."
                    .to_string(),
            )
    }
}
