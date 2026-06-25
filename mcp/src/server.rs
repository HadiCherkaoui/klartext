//! The MCP server: read-only diagnostic tools over a held car session.
//!
//! [`KlartextServer`] is the rmcp [`ServerHandler`] served over stdio. It holds an
//! optional car connection in shared state and exposes only non-mutating tools.
//! This module starts with `disconnect`; the read tools are added alongside it as
//! the milestone progresses.

use std::sync::Arc;

use klartext_semantic::Catalog;
use klartext_semantic::did;
use klartext_semantic::dtc::status_flags;
use klartext_uds::ALL_DTC_STATUS_MASK;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::dto::{
    ConnectRequest, ConnectResult, DisconnectResult, FaultDescription, FaultInfo, ListEcusResult,
    ReadDataRequest, ReadDataResult, ReadFaultsRequest, ReadFaultsResult,
};
use crate::ecu;
use crate::session::{self, SessionState};

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
    /// Connect to the car's gateway and start a read-only session.
    ///
    /// # Errors
    /// Returns a tool error if the gateway IP is invalid or discovery/connect fails.
    #[tool(description = "Connect to the car's gateway over HSFZ and start a \
        read-only diagnostic session. Call this first. Discovers the gateway on the \
        link (or uses the provided/configured gateway IP), reads the VIN, and holds \
        the session open with a background keepalive. Returns the gateway IP and VIN.")]
    pub async fn connect(
        &self,
        Parameters(req): Parameters<ConnectRequest>,
    ) -> Result<Json<ConnectResult>, McpError> {
        let gateway_ip = match req.gateway_ip.as_deref() {
            Some(s) => Some(s.parse().map_err(|_| {
                McpError::invalid_params(format!("invalid gateway_ip '{s}'"), None)
            })?),
            None => self.config.gateway_ip,
        };
        let conn = session::establish(&self.config, gateway_ip)
            .await
            .map_err(|e| McpError::internal_error(e, None))?;
        let result = ConnectResult {
            connected: true,
            gateway_ip: conn.gateway_ip.to_string(),
            vin: conn.vin.clone(),
            vin_source: conn.vin_source.as_str().to_string(),
            target_ecu: format!("ZGW (0x{:02X})", conn.target),
            note: "Read-only session held. Use read_faults/read_data; call disconnect when done."
                .to_string(),
        };
        *self.state.lock().await = Some(conn);
        Ok(Json(result))
    }

    /// Read and decode stored fault codes (DTCs) from one ECU.
    ///
    /// # Errors
    /// Returns a tool error if not connected, the ECU is unknown, or the read fails.
    #[tool(
        description = "Read and decode stored fault codes (DTCs) from one ECU. \
        Requires a prior connect. `ecu` is a name (\"DME\"), a hex address (\"0x12\"), \
        or an ISTA group name (\"d_0012\") — see list_ecus. Returns each fault's raw \
        code, decoded ISO status flags, and human description text (when the semantic \
        DB is available)."
    )]
    pub async fn read_faults(
        &self,
        Parameters(req): Parameters<ReadFaultsRequest>,
    ) -> Result<Json<ReadFaultsResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;

        let mut guard = self.state.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        session::ensure_target(conn, &self.config, address)
            .await
            .map_err(|e| McpError::internal_error(e, None))?;
        let dtcs = conn
            .client
            .read_dtcs(ALL_DTC_STATUS_MASK)
            .await
            .map_err(|e| McpError::internal_error(format!("reading DTCs: {e}"), None))?;

        let faults: Vec<FaultInfo> = dtcs
            .iter()
            .map(|d| {
                let descriptions = catalog
                    .as_ref()
                    .and_then(|c| c.describe_dtc(address, d.code).ok())
                    .unwrap_or_default()
                    .into_iter()
                    .map(|desc| FaultDescription {
                        variant: desc.ecu_variant,
                        saecode: desc.saecode,
                        text: desc.title_en.or(desc.title_de),
                    })
                    .collect();
                FaultInfo {
                    code_hex: format!("{:02X}{:02X}{:02X}", d.code[0], d.code[1], d.code[2]),
                    status_hex: format!("{:02X}", d.status),
                    status_flags: status_flags(d.status)
                        .into_iter()
                        .map(String::from)
                        .collect(),
                    descriptions,
                }
            })
            .collect();

        Ok(Json(ReadFaultsResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            count: faults.len(),
            faults,
            db_available: catalog.is_some(),
        }))
    }

    /// Read and decode one data identifier (DID) from an ECU.
    ///
    /// # Errors
    /// Returns a tool error if not connected, the ECU/DID is invalid, or the read fails.
    #[tool(
        description = "Read and decode one data identifier (DID) from an ECU. \
        Requires a prior connect. `ecu` as in read_faults; `did` is hex (e.g. \
        \"F190\" for the VIN). ISO-standard identification DIDs (0xF1xx) are named; \
        other DIDs return the raw value (BMW-specific scaling is out of scope)."
    )]
    pub async fn read_data(
        &self,
        Parameters(req): Parameters<ReadDataRequest>,
    ) -> Result<Json<ReadDataResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let did = parse_hex_u16(&req.did).map_err(|e| McpError::invalid_params(e, None))?;

        let mut guard = self.state.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        session::ensure_target(conn, &self.config, address)
            .await
            .map_err(|e| McpError::internal_error(e, None))?;
        let (got_did, raw) =
            conn.client.read_did(did).await.map_err(|e| {
                McpError::internal_error(format!("reading DID 0x{did:04X}: {e}"), None)
            })?;

        let decoded = did::decode(got_did, &raw);
        let raw_hex = raw
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");
        let note = if decoded.name.is_none() {
            "BMW-specific DID — name/scaling not in the SQLiteDB; raw value only.".to_string()
        } else {
            String::new()
        };
        Ok(Json(ReadDataResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            did_hex: format!("{got_did:04X}"),
            name: decoded.name.map(String::from),
            value_text: decoded.text,
            raw_hex,
            note,
        }))
    }

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

/// The clear, non-panicking error returned by read tools with no live session.
fn not_connected() -> McpError {
    McpError::invalid_request("not connected — call connect first", None)
}

/// Parse a hex `u16` DID with or without a `0x` prefix.
///
/// # Errors
/// Returns a human message if `s` is not valid hexadecimal in `u16` range.
fn parse_hex_u16(s: &str) -> Result<u16, String> {
    let t = s.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u16::from_str_radix(t, 16).map_err(|e| format!("invalid DID hex '{s}': {e}"))
}
