//! The MCP server: diagnostic tools over a held car session — reads, plus the one
//! confirmation-gated standard write (`clear_faults`).
//!
//! [`KlartextServer`] is the rmcp [`ServerHandler`] served over stdio. It holds an
//! optional car connection in shared state. The refined (M9) safety invariant:
//! every tool is non-mutating except `clear_faults`, which is standard UDS 0x14 —
//! well-defined, non-physical, reversible-by-reappearance — and refuses to run
//! without `confirm: true`. Physical actuation and derived-unconfirmed frames are
//! never executable here; they stay in the CLI with a human in the loop.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use klartext_semantic::dtc::status_flags;
use klartext_semantic::{
    Catalog, Category, Measurement, Measurements, Risk, ServiceFunction, ServiceFunctions,
    build_read_request, did,
};
use klartext_uds::ALL_DTC_STATUS_MASK;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::dto::{
    ClearFaultsRequest, ClearFaultsResult, ConnectRequest, ConnectResult, DisconnectResult,
    FaultDescription, FaultInfo, ListEcusResult, ListMeasurementsRequest, ListMeasurementsResult,
    ListServiceFunctionsRequest, ListServiceFunctionsResult, MeasurementInfo, ReadDataRequest,
    ReadDataResult, ReadFaultsRequest, ReadFaultsResult, ServiceFunctionInfo,
};
use crate::ecu;
use crate::session::{self, SessionState};

/// Most measurements one `list_measurements` call returns.
///
/// The DDE alone defines ~1800 `SG_FUNKTIONEN` rows; an uncapped listing would
/// flood an AI client's context. The cap is generous for a searched listing and
/// the reply's `total` + note make any truncation explicit, never silent.
const MAX_LISTED_MEASUREMENTS: usize = 200;

/// The klartext MCP server — reads plus the gated clear; a cloneable shared handle.
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
    /// Connect to the car's gateway and start a diagnostic session.
    ///
    /// # Errors
    /// Returns a tool error if the gateway IP is invalid or discovery/connect fails.
    #[tool(description = "Connect to the car's gateway over HSFZ and start a \
        diagnostic session (reads, plus the confirmation-gated clear_faults). Call \
        this first. Discovers the gateway on the link (or uses the \
        provided/configured gateway IP), reads the VIN, and holds the session open \
        with a background keepalive. Returns the gateway IP and VIN.")]
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
            note: "Session held. Reads (read_faults/read_data) run freely; \
                   clear_faults needs confirm=true. Call disconnect when done."
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

    /// Clear one ECU's stored fault codes — the server's only write; gated on `confirm`.
    ///
    /// The refined M9 safety invariant: a standard, well-defined, non-physical,
    /// reversible diagnostic operation (UDS 0x14, the M2 clear path) may run behind
    /// explicit confirmation. Nothing here actuates a component and no derived
    /// frame is sent — those stay human-executed in the CLI.
    ///
    /// # Errors
    /// Returns a tool error when `confirm` is false (the refusal explains what a
    /// clear discards), when not connected or the ECU is unknown, or when the
    /// pre-read or the clear itself fails.
    #[tool(description = "Clear stored fault codes (DTCs) on one ECU — UDS \
        ClearDiagnosticInformation (0x14, all DTC groups), a standard, well-defined \
        diagnostic operation and the ONLY write this server exposes. REQUIRES \
        confirm=true; without it the call refuses and explains. Clearing also \
        discards the faults' freeze-frame/snapshot data and can reset OBD readiness \
        monitors, so call read_faults first, tell the human what is stored, and pass \
        confirm=true only on their explicit go-ahead. Reversible only in that a \
        still-active fault sets its code again on a later drive cycle. `ecu` as in \
        read_faults. The result echoes the codes that were stored before the clear. \
        This server still cannot actuate components, run service functions, or code.")]
    pub async fn clear_faults(
        &self,
        Parameters(req): Parameters<ClearFaultsRequest>,
    ) -> Result<Json<ClearFaultsResult>, McpError> {
        // Blast-radius rule: refuse the state change before touching anything —
        // even the connection check — unless explicitly confirmed (CLI parity).
        if !req.confirm {
            return Err(McpError::invalid_params(
                format!(
                    "refusing to clear fault codes on '{}': clearing erases stored DTCs \
                     together with their freeze-frame/snapshot data and can reset OBD \
                     readiness monitors. Call read_faults first, confirm intent with the \
                     human, then re-call with confirm=true.",
                    req.ecu
                ),
                None,
            ));
        }
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;

        let mut guard = self.state.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        session::ensure_target(conn, &self.config, address)
            .await
            .map_err(|e| McpError::internal_error(e, None))?;
        // Record what is about to be discarded (the M2 read path). A failed
        // pre-read means a broken session — never clear blind.
        let dtcs = conn
            .client
            .read_dtcs(ALL_DTC_STATUS_MASK)
            .await
            .map_err(|e| {
                McpError::internal_error(format!("pre-read before clearing: {e}"), None)
            })?;
        let codes_cleared: Vec<String> = dtcs
            .iter()
            .map(|d| format!("{:02X}{:02X}{:02X}", d.code[0], d.code[1], d.code[2]))
            .collect();
        // The M2 clear path: extended session + the standard `14 FF FF FF`.
        conn.client
            .clear_all_dtcs()
            .await
            .map_err(|e| McpError::internal_error(format!("clearing DTCs: {e}"), None))?;

        Ok(Json(ClearFaultsResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            cleared: true,
            count: codes_cleared.len(),
            codes_cleared,
            note: "Cleared. Freeze-frame/snapshot data is discarded and readiness \
                   monitors may reset; a still-active fault will set its code again on a \
                   later drive cycle. Re-run read_faults to verify."
                .to_string(),
        }))
    }

    /// Read and decode one data identifier (DID) or named measurement from an ECU.
    ///
    /// # Errors
    /// Returns a tool error if not connected, the ECU/DID/name is invalid, or the
    /// read fails.
    #[tool(
        description = "Read and decode one live value from an ECU. Requires a prior \
        connect. `ecu` as in read_faults. Identify the value by exactly one of: \
        `did` — hex (e.g. \"F190\" for the VIN, \"F40C\" for engine RPM) — or `name` \
        — a measurement discovered via list_measurements (its arg like \"ITOEL\", or \
        its name like \"Motortemperatur\"; needs `variant`). Standard OBD-II / SAE \
        J1979 PIDs in the 0xF4xx range return a scaled engineering value + unit \
        (e.g. coolant 0xF405 in °C, RPM 0xF40C in rpm); ISO-standard identification \
        DIDs (0xF1xx) are named. A BMW-proprietary measurement is scaled to value + \
        unit when you pass `variant` (the ECU SGBD name, e.g. \"d72n47a0\") and the \
        server has --sgbd-dir; otherwise it returns the raw value. Raw bytes are \
        always included."
    )]
    pub async fn read_data(
        &self,
        Parameters(req): Parameters<ReadDataRequest>,
    ) -> Result<Json<ReadDataResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        // The per-variant catalog resolves `name` here, then routes the dynamic
        // read and scales the response below.
        let measurements = self.measurements(req.variant.as_deref());
        let did = resolve_read_target(&req, measurements.as_ref())?;

        let mut guard = self.state.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        session::ensure_target(conn, &self.config, address)
            .await
            .map_err(|e| McpError::internal_error(e, None))?;
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

    /// List one ECU's live measurements from its SGBD — read-only discovery.
    ///
    /// # Errors
    /// Returns a tool error if `--sgbd-dir` is unset or `variant` has no matching SGBD.
    #[tool(description = "List one ECU's live measurements (temperatures, \
        pressures, DPF soot/ash load, regeneration status, engine RPM, …) from its \
        SGBD SG_FUNKTIONEN table — READ-ONLY discovery, with no car connection. \
        `variant` is the ECU SGBD name (e.g. \"d72n47a0\" for the F2x diesel DME); \
        the server needs --sgbd-dir. Use `search` (case-insensitive substring; the \
        terms are mostly German, e.g. \"Öltemperatur\", \"Kühlmittel\", \"Rußmasse\", \
        \"Regeneration\", \"Drehzahl\") to find signals — big ECUs define ~1800 \
        measurements and one call returns at most 200 (`total` shows the full match \
        count). Then read a value with read_data, passing the entry's `id_hex` as \
        `did` (or its arg/name as `name`) plus the same `variant`, and get the \
        scaled engineering value + unit.")]
    pub async fn list_measurements(
        &self,
        Parameters(req): Parameters<ListMeasurementsRequest>,
    ) -> Result<Json<ListMeasurementsResult>, McpError> {
        let measurements = self
            .measurements(Some(&req.variant))
            .ok_or_else(|| no_sgbd(&req.variant))?;

        let query = req.search.as_deref().map(str::to_lowercase);
        let matching: Vec<&Measurement> = measurements
            .all()
            .into_iter()
            .filter(|m| match &query {
                None => true,
                Some(q) => [&m.arg, &m.result_name, &m.description]
                    .iter()
                    .any(|field| field.to_lowercase().contains(q)),
            })
            .collect();
        let total = matching.len();
        let infos: Vec<MeasurementInfo> = matching
            .into_iter()
            .take(MAX_LISTED_MEASUREMENTS)
            .map(measurement_info)
            .collect();

        let note = if infos.len() < total {
            format!(
                "Showing {} of {total} matching measurements — narrow with `search`. \
                 Read a value via read_data: `did` = the entry's id_hex (or `name` = its \
                 arg/name) plus this `variant`.",
                infos.len()
            )
        } else {
            "Read-only catalog from the SGBD; no car connection was made. Read a value \
             via read_data: `did` = the entry's id_hex (or `name` = its arg/name) plus \
             this `variant`."
                .to_string()
        };
        Ok(Json(ListMeasurementsResult {
            variant: req.variant,
            count: infos.len(),
            total,
            measurements: infos,
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
        let functions = self
            .service_functions(&req.variant)
            .ok_or_else(|| no_sgbd(&req.variant))?;

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
                "BMW F-series diagnostics: reads, plus exactly one confirmation-gated \
                 write (clear_faults). Call connect first (discovers the gateway or uses a \
                 configured IP, reads the VIN). read_faults and read_data target an ECU by \
                 name (\"DME\"), hex address (\"0x12\"), or ISTA group name (\"d_0012\"); \
                 list_ecus enumerates targetable ECUs. list_measurements discovers an ECU's \
                 live values (oil/coolant temperatures, DPF soot/ash mass, regeneration \
                 status, RPM, …) from its SGBD; read one via read_data by `did` = its \
                 id_hex or `name` = its arg/name, plus `variant`. list_service_functions \
                 lists resets/actuations/calibrations with risk tiers and frame status — \
                 for reasoning and recommending only; it never runs them. clear_faults \
                 erases one ECU's stored DTCs (standard UDS 0x14) and refuses without \
                 confirm=true — it also discards freeze-frames and can reset readiness \
                 monitors, so read first and get the human's go-ahead. This server cannot \
                 actuate components, run service functions, code, or send any \
                 derived-unconfirmed frame — those stay in the CLI with a human in the \
                 loop. Fault text and the full ECU map come from the ISTA SQLiteDB; reads \
                 still work (raw) without it."
                    .to_string(),
            )
    }
}

/// The clear, non-panicking error returned by read tools with no live session.
fn not_connected() -> McpError {
    McpError::invalid_request("not connected — call connect first", None)
}

/// The clear error returned when `variant` cannot be resolved to an SGBD `.prg`.
fn no_sgbd(variant: &str) -> McpError {
    McpError::invalid_params(
        format!(
            "no SGBD for variant '{variant}' — the server needs --sgbd-dir and a matching \
             <variant>.prg (a bare file name)."
        ),
        None,
    )
}

/// Resolve `read_data`'s target id: exactly one of a hex `did` or a measurement `name`.
///
/// A `name` resolves through the `variant` SGBD catalog (see
/// `Measurements::find_by_name`); descriptions are not unique in real data, so an
/// ambiguous name errors with the candidate ids instead of guessing.
///
/// # Errors
/// Returns an invalid-params error when both or neither identifier is given, the
/// hex does not parse, `name` comes without a loadable `variant` catalog, or the
/// name matches no (or several) measurements.
fn resolve_read_target(
    req: &ReadDataRequest,
    measurements: Option<&Measurements>,
) -> Result<u16, McpError> {
    match (req.did.as_deref(), req.name.as_deref()) {
        (Some(did), None) => parse_hex_u16(did).map_err(|e| McpError::invalid_params(e, None)),
        (None, Some(name)) => {
            let Some(variant) = req.variant.as_deref() else {
                return Err(McpError::invalid_params(
                    "reading by `name` needs `variant` (the ECU SGBD, e.g. \"d72n47a0\") to \
                     load the measurement catalog",
                    None,
                ));
            };
            let catalog = measurements.ok_or_else(|| no_sgbd(variant))?;
            match catalog.find_by_name(name).as_slice() {
                [] => Err(McpError::invalid_params(
                    format!(
                        "no measurement named '{name}' in variant '{variant}' — call \
                         list_measurements (try `search`) and use an entry's arg/name, or its \
                         id_hex as `did`"
                    ),
                    None,
                )),
                [only] => Ok(only.id),
                several => Err(McpError::invalid_params(
                    format!(
                        "measurement name '{name}' is ambiguous in variant '{variant}': \
                         matches ids {} — pass one as `did`",
                        several
                            .iter()
                            .map(|m| format!("{:04X}", m.id))
                            .collect::<Vec<_>>()
                            .join(", ")
                    ),
                    None,
                )),
            }
        }
        _ => Err(McpError::invalid_params(
            "pass exactly one of `did` (hex) or `name` (a list_measurements entry)",
            None,
        )),
    }
}

/// Map a semantic [`Measurement`] to its read-only listing DTO.
fn measurement_info(measurement: &Measurement) -> MeasurementInfo {
    MeasurementInfo {
        id_hex: format!("{:04X}", measurement.id),
        name: measurement.name().to_string(),
        arg: measurement.arg.clone(),
        result_name: measurement.result_name.clone(),
        unit: measurement.unit.clone(),
        ecu_address: measurement.sg_adr.clone(),
    }
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
