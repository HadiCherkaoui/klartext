//! The MCP server: read-only diagnostic tools over a held car session.
//!
//! [`KlartextServer`] is the rmcp [`ServerHandler`] served over stdio. It holds an
//! optional car connection in shared state and exposes only non-mutating tools.
//! This module starts with `disconnect`; the read tools are added alongside it as
//! the milestone progresses.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use klartext_semantic::dtc::status_flags;
use klartext_semantic::{
    Catalog, Category, Measurements, Risk, ServiceFunction, ServiceFunctions, build_read_request,
    did,
};
use klartext_uds::ALL_DTC_STATUS_MASK;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::dto::{
    ConnectRequest, ConnectResult, DisconnectResult, FaultDescription, FaultInfo, ListEcusResult,
    ListServiceFunctionsRequest, ListServiceFunctionsResult, ReadDataRequest, ReadDataResult,
    ReadFaultsRequest, ReadFaultsResult, ServiceFunctionInfo,
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

    /// Resolve the SGBD `.prg` path for `variant`, or `None` when it cannot be served.
    ///
    /// Requires `--sgbd-dir`; `variant` must be a bare file name (no path parts) so a
    /// client cannot escape the directory via `..` or an absolute path.
    fn sgbd_path(&self, variant: &str) -> Option<PathBuf> {
        let dir = self.config.sgbd_dir.as_deref()?;
        if variant.is_empty() || Path::new(variant).file_name() != Some(OsStr::new(variant)) {
            tracing::warn!(variant, "ignoring SGBD variant: must be a bare file name");
            return None;
        }
        Some(dir.join(format!("{variant}.prg")))
    }

    /// Load proprietary measurements for `variant` (the SGBD `.prg` stem), or `None`.
    ///
    /// An absent/unreadable SGBD downgrades the read to raw rather than failing.
    fn measurements(&self, variant: Option<&str>) -> Option<Measurements> {
        let path = self.sgbd_path(variant?)?;
        match Measurements::from_sgbd(&path) {
            Ok(measurements) => Some(measurements),
            Err(error) => {
                tracing::warn!(%error, "SGBD measurement scaling unavailable; raw only");
                None
            }
        }
    }

    /// Load the service-function catalog for `variant` (the SGBD `.prg` stem), or `None`.
    ///
    /// Offline discovery, read-only: it reads the ECU's control tables and job list
    /// from the SGBD; it never connects to the car and never executes anything.
    fn service_functions(&self, variant: &str) -> Option<ServiceFunctions> {
        let path = self.sgbd_path(variant)?;
        match ServiceFunctions::from_sgbd(&path) {
            Ok(functions) => Some(functions),
            Err(error) => {
                tracing::warn!(%error, "service-function catalog unavailable");
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
        \"F190\" for the VIN, \"F40C\" for engine RPM). Standard OBD-II / SAE J1979 \
        PIDs in the 0xF4xx range return a scaled engineering value + unit (e.g. \
        coolant 0xF405 in °C, RPM 0xF40C in rpm); ISO-standard identification DIDs \
        (0xF1xx) are named. A BMW-proprietary measurement is scaled to value + unit \
        when you pass `variant` (the ECU SGBD name, e.g. \"d72n47a0\") and the server \
        has --sgbd-dir; otherwise it returns the raw value. Raw bytes are always \
        included."
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
        let measurements = self.measurements(req.variant.as_deref());
        // M6 Part B: a dynamic SG_FUNKTIONEN measurement (SERVICE "22;2C") is read
        // via the 0x2C define + 0x22 read sequence; a static DID is a plain 0x22
        // read. Either way the requested id is reported (not the dynamic 0xF303).
        let (got_did, raw) = match measurements.as_ref().and_then(|m| m.get(did)) {
            Some(measurement) if measurement.is_dynamic() => {
                let requests = build_read_request(measurement);
                let raw = conn
                    .client
                    .read_dynamic_measurement(&requests)
                    .await
                    .map_err(|e| {
                        McpError::internal_error(
                            format!("reading measurement 0x{did:04X}: {e}"),
                            None,
                        )
                    })?;
                (did, raw)
            }
            _ => conn.client.read_did(did).await.map_err(|e| {
                McpError::internal_error(format!("reading DID 0x{did:04X}: {e}"), None)
            })?,
        };

        let decoded = did::decode(got_did, &raw);
        // Standard PIDs (M5) and unknowns are unchanged; a proprietary measurement
        // scales via SG_FUNKTIONEN — from the static read or the dynamic sequence.
        let proprietary = measurements.and_then(|m| m.scale(got_did, &raw));
        let raw_hex = raw
            .iter()
            .map(|b| format!("{b:02X}"))
            .collect::<Vec<_>>()
            .join(" ");

        let (name, scaled_value, unit, note) = if let Some(scaled) = &decoded.scaled {
            (
                decoded.name.map(String::from),
                Some(scaled.value),
                Some(scaled.unit.to_string()),
                "Standard OBD-II PID (SAE J1979); value scaled to engineering units.".to_string(),
            )
        } else if let Some(measurement) = proprietary {
            (
                Some(measurement.name),
                Some(measurement.value),
                Some(measurement.unit),
                "BMW-proprietary measurement scaled via the ECU SGBD (SG_FUNKTIONEN).".to_string(),
            )
        } else if decoded.name.is_none() {
            (
                None,
                None,
                None,
                "BMW-specific DID — pass `variant` (the ECU SGBD) to scale, else raw only."
                    .to_string(),
            )
        } else {
            (decoded.name.map(String::from), None, None, String::new())
        };

        Ok(Json(ReadDataResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            did_hex: format!("{got_did:04X}"),
            name,
            value_text: decoded.text,
            scaled_value,
            unit,
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

    /// List one ECU's service functions (resets, actuations, calibrations) — read-only.
    ///
    /// # Errors
    /// Returns a tool error if `--sgbd-dir` is unset or `variant` has no matching SGBD,
    /// or if the `risk` filter is not "low"/"high".
    #[tool(description = "List one ECU's service functions (maintenance resets, \
        adaptations, actuations, calibrations) from its SGBD — READ-ONLY discovery, with \
        no car connection and NO execution. `variant` is the ECU SGBD name (e.g. \
        \"d72n47a0\"); the server needs --sgbd-dir. Optional `risk` filters \"low\" or \
        \"high\". Each entry gives a label, description, category, risk tier, and a frame \
        status: \"derived-unconfirmed\" (a request frame was reconstructed from ISTA \
        disassembly but is NOT hardware-confirmed — treat as [verify against capture]) or \
        \"frame-not-derivable\" (discovery-only; no offline frame). Only LOW-risk derived \
        functions are runnable, and only by a HUMAN in the klartext CLI behind `service \
        run <label> --confirm`; HIGH-risk physical actuation/calibration is human-only in \
        a workshop. This tool cannot run any of them.")]
    pub async fn list_service_functions(
        &self,
        Parameters(req): Parameters<ListServiceFunctionsRequest>,
    ) -> Result<Json<ListServiceFunctionsResult>, McpError> {
        let functions = self.service_functions(&req.variant).ok_or_else(|| {
            McpError::invalid_params(
                format!(
                    "no SGBD for variant '{}' — the server needs --sgbd-dir and a matching \
                     <variant>.prg (a bare file name).",
                    req.variant
                ),
                None,
            )
        })?;

        let risk_filter = match req.risk.as_deref() {
            None => None,
            Some(s) => Some(parse_risk(s).map_err(|e| McpError::invalid_params(e, None))?),
        };

        let infos: Vec<ServiceFunctionInfo> = functions
            .all()
            .iter()
            .filter(|f| risk_filter.is_none_or(|r| f.risk() == r))
            .map(service_function_info)
            .collect();

        Ok(Json(ListServiceFunctionsResult {
            variant: req.variant,
            count: infos.len(),
            functions: infos,
            note: "Read-only catalog. Derived frames are UNCONFIRMED ([verify against \
                   capture]); a human runs only low-risk derived functions in the CLI behind \
                   --confirm. This server never executes a service function."
                .to_string(),
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
                 group name (\"d_0012\"); list_ecus enumerates targetable ECUs. \
                 list_service_functions lists an ECU's resets/actuations/calibrations with \
                 risk tiers and frame status — for reasoning and recommending only; it never \
                 runs them. This server cannot clear faults, actuate, code, or run a service \
                 function — those are intentionally absent and live in the CLI with a human in \
                 the loop. Fault text and the full ECU map come from the ISTA SQLiteDB; reads \
                 still work (raw) without it."
                    .to_string(),
            )
    }
}

/// The clear, non-panicking error returned by read tools with no live session.
fn not_connected() -> McpError {
    McpError::invalid_request("not connected — call connect first", None)
}

/// Parse a `list_service_functions` risk filter word into a [`Risk`].
///
/// # Errors
/// Returns a human message if `s` is neither "low" nor "high" (case-insensitive).
fn parse_risk(s: &str) -> Result<Risk, String> {
    match s.trim().to_ascii_lowercase().as_str() {
        "low" => Ok(Risk::Low),
        "high" => Ok(Risk::High),
        other => Err(format!(
            "invalid risk filter '{other}': use \"low\" or \"high\""
        )),
    }
}

/// The category slug used in the read-only service-function listing.
fn category_slug(category: Category) -> &'static str {
    match category {
        Category::CbsReset => "cbs_reset",
        Category::StatisticReset => "statistic_reset",
        Category::LearnedValueReset => "learned_value_reset",
        Category::ActuatorControl => "actuator_control",
        Category::Calibration => "calibration",
    }
}

/// Map a semantic [`ServiceFunction`] to its read-only listing DTO.
///
/// Never exposes the execution frame bytes — only metadata and guidance. `runnable_in_cli`
/// is true only for a low-risk, derived function (the class a human may run behind
/// `--confirm`); high-risk and not-derivable functions are never runnable.
fn service_function_info(function: &ServiceFunction) -> ServiceFunctionInfo {
    let low = function.risk() == Risk::Low;
    let derived = function.is_derived();
    let guidance = if !low {
        "HIGH-risk physical actuation/calibration — human-only in a workshop with the \
         function's preconditions met. Never run via this tool or casually; not available in \
         the CLI."
            .to_string()
    } else if derived {
        format!(
            "Low-risk and derived (UNCONFIRMED). A human may run it in the CLI: `klartext \
             --sgbd <variant>.prg --target <ecu> service run {} --confirm`. Test low-risk first \
             and verify the effect — the frame is [verify against capture].",
            function.label
        )
    } else {
        "Low-risk but its frame is not derivable offline (discovery-only) — not executable \
         in this build; needs an on-car capture or a BEST/2 interpreter."
            .to_string()
    };
    ServiceFunctionInfo {
        label: function.label.clone(),
        name: function.name.clone(),
        category: category_slug(function.category).to_string(),
        risk: if low { "low" } else { "high" }.to_string(),
        derivation: function.derivation.status().to_string(),
        citation: function.derivation.citation().map(str::to_string),
        runnable_in_cli: low && derived,
        guidance,
    }
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
