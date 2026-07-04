//! The MCP server: diagnostic tools over a held car session — reads, plus the one
//! confirmation-gated standard write (`clear_faults`).
//!
//! [`KlartextServer`] is the rmcp [`ServerHandler`] served over stdio. It holds an
//! optional car connection in shared state. The refined (M9) safety invariant:
//! every tool is non-mutating except `clear_faults`, which is standard UDS 0x14 —
//! well-defined, non-physical, reversible-by-reappearance — and refuses to run
//! without `confirm: true`. Physical actuation and derived-unconfirmed WRITE
//! frames are never executable here; they stay in the CLI with a human in the
//! loop. (The M6 dynamic-read `0x2C` define — session-transient read plumbing —
//! is the one derived sequence the read path uses, by the M6 decision.)

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use klartext_semantic::dtc::status_flags;
use klartext_semantic::{
    Catalog, Category, FreezeFrameDefs, Measurement, Measurements, Risk, ServiceFunction,
    ServiceFunctions, build_read_request, did, fold_for_match,
};
use klartext_uds::{Dtc, DtcRecordRegion};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::dto::{
    ClearAllFaultsRequest, ClearAllFaultsResult, ClearFaultsRequest, ClearFaultsResult,
    ConnectRequest, ConnectResult, DisconnectResult, EcuClearInfo, EcuFaultsInfo, ExtDataFieldInfo,
    FaultDescription, FaultDetailResult, FaultInfo, FittedEcuInfo, ListEcusResult,
    ListMeasurementsRequest, ListMeasurementsResult, ListServiceFunctionsRequest,
    ListServiceFunctionsResult, MeasurementInfo, ReadAllFaultsRequest, ReadAllFaultsResult,
    ReadDataRequest, ReadDataResult, ReadFaultDetailRequest, ReadFaultsRequest, ReadFaultsResult,
    ScanEcusRequest, ScanEcusResult, ServiceFunctionInfo, SnapshotFieldInfo,
};
use crate::ecu;
use crate::session::{self, SessionState};

/// Most measurements one `list_measurements` call returns.
///
/// The DDE alone defines ~1800 `SG_FUNKTIONEN` rows; an uncapped listing would
/// flood an AI client's context. The cap is generous for a searched listing and
/// the reply's `total` + note make any truncation explicit, never silent.
const MAX_LISTED_MEASUREMENTS: usize = 200;

/// Parsed SGBD measurement catalogs cached per variant.
///
/// The DDE `SG_FUNKTIONEN` table alone is ~1800 rows; re-parsing the `.prg` on
/// every tool call is wasteful when a live session reads the same ECU repeatedly.
type SgbdCache = Arc<StdMutex<HashMap<String, Arc<Measurements>>>>;

/// The klartext MCP server — reads plus the gated clear; a cloneable shared handle.
#[derive(Clone)]
pub struct KlartextServer {
    config: Arc<ServerConfig>,
    state: SessionState,
    sgbd_cache: SgbdCache,
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
            sgbd_cache: Arc::new(StdMutex::new(HashMap::new())),
            tool_router: Self::tool_router(),
        }
    }

    /// Drop any held car connection during shutdown (aborts keepalive, closes TCP).
    ///
    /// The same effect as the `disconnect` tool, callable from the binary's signal
    /// handler so a killed server never leaves a dangling session to time out.
    pub async fn disconnect_now(&self) -> bool {
        self.state.lock().await.take().is_some()
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
    /// Cached per variant — the `.prg` parse is ~1800 rows on the DDE. An
    /// absent/unreadable SGBD downgrades the read to raw rather than failing.
    fn measurements(&self, variant: Option<&str>) -> Option<Arc<Measurements>> {
        let variant = variant?;
        if let Some(hit) = self
            .sgbd_cache
            .lock()
            .expect("sgbd cache mutex poisoned")
            .get(variant)
            .cloned()
        {
            return Some(hit);
        }
        let path = self.sgbd_path(variant)?;
        match Measurements::from_sgbd(&path) {
            Ok(measurements) => {
                let arc = Arc::new(measurements);
                self.sgbd_cache
                    .lock()
                    .expect("sgbd cache mutex poisoned")
                    .insert(variant.to_string(), Arc::clone(&arc));
                Some(arc)
            }
            Err(error) => {
                tracing::warn!(%error, "SGBD measurement scaling unavailable; raw only");
                None
            }
        }
    }

    /// Load the freeze-frame decode definitions for `variant` (the SGBD stem), or `None`.
    ///
    /// Opens the SGBD `.prg` and extracts the snapshot + extended-data tables. Not
    /// cached — freeze-frame detail is an on-demand, per-fault read, not a hot path.
    /// An absent or unreadable SGBD downgrades the decode to raw rather than failing.
    fn freeze_frame_defs(&self, variant: Option<&str>) -> Option<FreezeFrameDefs> {
        let variant = variant?;
        let path = self.sgbd_path(variant)?;
        match FreezeFrameDefs::from_sgbd(&path) {
            Ok(defs) => Some(defs),
            Err(error) => {
                tracing::warn!(%error, "SGBD freeze-frame decode unavailable; raw only");
                None
            }
        }
    }

    /// Resolve which SGBD variant to use for `address`.
    ///
    /// The ladder (no hardcoding): an explicit `variant` wins; else a learned
    /// per-VIN profile; else a DB-unique candidate whose `.prg` exists in
    /// `--sgbd-dir`. `None` means unresolved — a caller that needs a variant turns
    /// that into a candidate error via [`Self::variant_candidates_error`].
    fn resolve_variant(
        &self,
        address: u8,
        explicit: Option<&str>,
        catalog: Option<&Catalog>,
        vin: Option<&str>,
    ) -> Option<String> {
        if let Some(v) = explicit {
            return Some(v.to_string());
        }
        // A learned profile for this car.
        if let (Some(dir), Some(vin)) = (self.config.profile_dir(), vin)
            && let Some(v) = crate::profile::load(&dir, vin).get(address)
        {
            return Some(v.to_string());
        }
        // A DB-unique candidate whose `.prg` is available.
        if let Some(catalog) = catalog
            && let Ok(variants) = catalog.variants(address)
        {
            let available: Vec<String> = variants
                .into_iter()
                .map(|v| v.name)
                .filter(|name| self.sgbd_path(name).is_some_and(|p| p.exists()))
                .collect();
            if let [only] = available.as_slice() {
                tracing::info!(variant = %only, address, "variant auto-resolved (DB-unique)");
                return Some(only.clone());
            }
        }
        None
    }

    /// The "need a variant" error, listing the DB's candidates for `address`.
    fn variant_candidates_error(&self, address: u8, catalog: Option<&Catalog>) -> McpError {
        let list = catalog
            .and_then(|c| c.variants(address).ok())
            .map(|vs| {
                vs.iter()
                    .map(|v| match &v.title {
                        Some(t) => format!("{} ({t})", v.name),
                        None => v.name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "none in the DB".to_string());
        McpError::invalid_params(
            format!(
                "need a `variant` for ECU 0x{address:02X} and none could be resolved (no explicit \
                 variant, no learned profile, and no single DB candidate with a matching .prg). \
                 Candidates: {list}"
            ),
            None,
        )
    }

    /// Resolve the `variant` for a list tool: explicit, else via `ecu` + the ladder.
    ///
    /// # Errors
    /// Returns an error naming the DB's candidates when neither an explicit variant
    /// nor a resolvable `ecu` is given.
    async fn resolve_list_variant(
        &self,
        variant: Option<&str>,
        ecu: Option<&str>,
    ) -> Result<String, McpError> {
        if let Some(v) = variant {
            return Ok(v.to_string());
        }
        let Some(ecu) = ecu else {
            return Err(McpError::invalid_params(
                "pass `variant` (the ECU SGBD, e.g. \"d72n47a0\") or an `ecu` whose variant \
                 can be resolved",
                None,
            ));
        };
        let catalog = self.catalog();
        let address =
            ecu::resolve(ecu, catalog.as_ref()).map_err(|e| McpError::invalid_params(e, None))?;
        let vin = self.state.lock().await.as_ref().and_then(|c| c.vin.clone());
        self.resolve_variant(address, None, catalog.as_ref(), vin.as_deref())
            .ok_or_else(|| self.variant_candidates_error(address, catalog.as_ref()))
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
            target_ecu: format!("gateway (ZGW 0x{:02X})", klartext_hsfz::ZGW_ADDRESS),
            note: "Session held; one connection reaches every ECU by name/address. \
                   Reads (read_faults/read_data/scan_ecus/read_all_faults) run freely; \
                   clear_faults / clear_all_faults need confirm=true. Call disconnect \
                   when done (the server also disconnects on exit)."
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

        let dtcs = {
            let guard = self.state.lock().await;
            let conn = guard.as_ref().ok_or_else(not_connected)?;
            conn.client
                .read_all_dtcs(address)
                .await
                .map_err(|e| McpError::internal_error(format!("reading DTCs: {e}"), None))?
        };

        // Split real faults from "not tested this cycle" catalog noise (counted).
        let not_tested_count = dtcs.iter().filter(|d| !d.is_relevant()).count();
        let shown: Vec<Dtc> = if req.include_not_tested {
            dtcs
        } else {
            dtcs.into_iter().filter(|d| d.is_relevant()).collect()
        };

        let faults: Vec<FaultInfo> = shown
            .iter()
            .map(|d| fault_info(d, address, catalog.as_ref()))
            .collect();

        Ok(Json(ReadFaultsResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            count: faults.len(),
            faults,
            not_tested_count,
            db_available: catalog.is_some(),
        }))
    }

    /// Read one fault's freeze-frame / snapshot metadata (UDS 19 04 / 06 / 09).
    ///
    /// # Errors
    /// Returns a tool error if not connected, the ECU/code is invalid, an explicit
    /// `variant` cannot be served, or a read fails on the transport.
    #[tool(
        description = "Read a single fault's freeze-frame metadata — the environmental \
        conditions the ECU latched when the fault occurred (mileage, timestamp, RPM, \
        temperatures, ECU state) plus occurrence/healing counters and severity. This is \
        the on-demand detail read (UDS 19 04 snapshot + 19 06 extended data + 19 09 \
        severity), the equivalent of ISTA's FS_LESEN_DETAIL. First call read_faults to \
        get a fault's `code_hex`, then pass it here as `code`. `ecu` as in read_faults. \
        The fields decode to label + value + unit when the ECU SGBD is available (pass \
        `variant`, e.g. \"d72n47a0\", or let it resolve from the ecu, with --sgbd-dir \
        set); otherwise the raw region is returned. NOTE: the response framing is \
        derived from ISO 14229 + disassembly and is pending an on-car capture, so treat \
        the decoded values as provisional."
    )]
    pub async fn read_fault_detail(
        &self,
        Parameters(req): Parameters<ReadFaultDetailRequest>,
    ) -> Result<Json<FaultDetailResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let dtc = parse_dtc_code(&req.code).map_err(|e| McpError::invalid_params(e, None))?;

        // Resolve the variant via the ladder and load the freeze-frame SGBD defs. An
        // explicit variant whose `.prg` is absent is a configuration error the caller
        // must see; a ladder-resolved one that is absent just degrades to raw.
        let conn_vin = self.state.lock().await.as_ref().and_then(|c| c.vin.clone());
        let effective_variant = self.resolve_variant(
            address,
            req.variant.as_deref(),
            catalog.as_ref(),
            conn_vin.as_deref(),
        );
        let defs = self.freeze_frame_defs(effective_variant.as_deref());
        if let (Some(variant), None) = (req.variant.as_deref(), defs.as_ref()) {
            return Err(no_sgbd(variant));
        }

        let detail = {
            let guard = self.state.lock().await;
            let conn = guard.as_ref().ok_or_else(not_connected)?;
            conn.client
                .read_fault_detail(address, dtc)
                .await
                .map_err(|e| {
                    McpError::internal_error(
                        format!("reading fault detail for {}: {e}", req.code),
                        None,
                    )
                })?
        };

        let descriptions = catalog
            .as_ref()
            .and_then(|c| c.describe_dtc(address, dtc).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|d| FaultDescription {
                variant: d.ecu_variant,
                saecode: d.saecode,
                text: d.title_en.or(d.title_de),
            })
            .collect();

        let mut notes = Vec::new();
        let snapshot = decode_snapshot_dtos(
            detail.snapshot.as_ref(),
            defs.as_ref(),
            catalog.as_ref(),
            &mut notes,
        );
        let extended = decode_ext_dtos(detail.extended.as_ref(), defs.as_ref(), &mut notes);
        notes.push(
            "Freeze-frame framing is derived from ISO 14229 + SGBD disassembly and is \
             pending an on-car 0x19 capture — treat decoded values as provisional."
                .to_string(),
        );

        Ok(Json(FaultDetailResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            code_hex: format!("{:02X}{:02X}{:02X}", dtc[0], dtc[1], dtc[2]),
            descriptions,
            snapshot,
            extended,
            severity_hex: detail.severity.map(|s| format!("{:02X}", s.severity)),
            functional_unit_hex: detail
                .severity
                .map(|s| format!("{:02X}", s.functional_unit)),
            sgbd_available: defs.is_some(),
            notes,
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

        let guard = self.state.lock().await;
        let conn = guard.as_ref().ok_or_else(not_connected)?;
        // Record what is about to be discarded (the M2 read path). A failed
        // pre-read means a broken session — never clear blind.
        let dtcs = conn.client.read_all_dtcs(address).await.map_err(|e| {
            McpError::internal_error(format!("pre-read before clearing: {e}"), None)
        })?;
        let codes_cleared: Vec<String> = dtcs.iter().map(dtc_code_hex).collect();
        // The M2 clear path: extended session + the standard `14 FF FF FF`.
        conn.client
            .clear_all_dtcs(address)
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
        // Resolve the variant via the ladder (explicit → learned profile →
        // DB-unique). The VIN, if connected, keys the profile.
        let conn_vin = self.state.lock().await.as_ref().and_then(|c| c.vin.clone());
        let effective_variant = self.resolve_variant(
            address,
            req.variant.as_deref(),
            catalog.as_ref(),
            conn_vin.as_deref(),
        );
        // The per-variant catalog resolves `name` here, then routes the dynamic
        // read and scales the response below. An *explicit* `variant` that cannot
        // be served is a configuration error the caller must see; a ladder-resolved
        // variant whose `.prg` is absent just degrades to a raw read.
        let measurements = self.measurements(effective_variant.as_deref());
        if let (Some(variant), None) = (req.variant.as_deref(), measurements.as_ref()) {
            return Err(no_sgbd(variant));
        }
        let did = resolve_read_target(&req, effective_variant.as_deref(), measurements.as_deref())?;
        // A catalog measurement is bound to its ECU: its request sequence and its
        // scaling formula are that ECU's. Reading it elsewhere would return foreign
        // bytes scaled with the wrong formula under a trusted name — refuse.
        if let Some(measurement) = measurements.as_ref().and_then(|m| m.get(did))
            && let Some(expected) = parse_sg_adr(&measurement.sg_adr)
            && expected != address
        {
            return Err(McpError::invalid_params(
                format!(
                    "measurement 0x{did:04X} ('{}') belongs to ECU 0x{expected:02X} per SGBD \
                     '{}', not '{}' — target ecu \"0x{expected:02X}\", or omit `variant` for \
                     a raw DID read",
                    measurement.name(),
                    req.variant.as_deref().unwrap_or_default(),
                    req.ecu,
                ),
                None,
            ));
        }

        // M6 Part B: a dynamic SG_FUNKTIONEN measurement (SERVICE "22;2C") is read
        // via the 0x2C define + 0x22 read sequence; a static DID is a plain 0x22
        // read. Either way the requested id is reported (not the dynamic 0xF303).
        let (got_did, raw) = {
            let guard = self.state.lock().await;
            let conn = guard.as_ref().ok_or_else(not_connected)?;
            match measurements.as_ref().and_then(|m| m.get(did)) {
                Some(measurement) if measurement.is_dynamic() => {
                    let requests = build_read_request(measurement);
                    let raw = conn
                        .client
                        .read_dynamic_measurement(address, &requests)
                        .await
                        .map_err(|e| {
                            McpError::internal_error(
                                format!("reading measurement 0x{did:04X}: {e}"),
                                None,
                            )
                        })?;
                    (did, raw)
                }
                _ => conn.client.read_did(address, did).await.map_err(|e| {
                    McpError::internal_error(format!("reading DID 0x{did:04X}: {e}"), None)
                })?,
            }
        };

        let decoded = did::decode(got_did, &raw);
        // Standard PIDs (M5) and unknowns are unchanged; a proprietary measurement
        // scales via SG_FUNKTIONEN — from the static read or the dynamic sequence.
        let proprietary = measurements.as_ref().and_then(|m| m.scale(got_did, &raw));
        let scaled_by_sgbd = proprietary.is_some();
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
            // The note must not tell the caller to do what they already did: with a
            // loaded variant, distinguish "not a catalog measurement" from "found
            // but did not scale"; only without a variant is passing one the fix.
            let note = match measurements.as_ref() {
                Some(m) if m.get(got_did).is_some() => {
                    "SGBD measurement found but the response did not scale (unexpected \
                     length) — raw only."
                        .to_string()
                }
                Some(_) => format!(
                    "DID not in SGBD '{}' SG_FUNKTIONEN — raw only. Discover readable \
                     measurements via list_measurements.",
                    req.variant.as_deref().unwrap_or_default()
                ),
                None => "BMW-specific DID — pass `variant` (the ECU SGBD) to scale, else \
                         raw only."
                    .to_string(),
            };
            (None, None, None, note)
        } else {
            (decoded.name.map(String::from), None, None, String::new())
        };

        // Learn the variant for this ECU on a successful SGBD-scaled read, so a
        // later read of the same ECU on this car can resolve it without `variant`.
        if scaled_by_sgbd
            && let (Some(dir), Some(vin), Some(variant)) = (
                self.config.profile_dir(),
                conn_vin.as_deref(),
                effective_variant.as_deref(),
            )
            && let Err(error) = crate::profile::record(&dir, vin, address, variant)
        {
            tracing::warn!(%error, "could not record learned variant");
        }

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

    /// List the ECUs the read tools can target, from the ISTA semantic DB.
    ///
    /// This is the whole per-model BMW map, not the car's fitted set — use
    /// `scan_ecus` for what is actually present. Does not require a connection.
    ///
    /// # Errors
    /// Infallible today; returns `Result` to match the tool signature shape.
    #[tool(
        description = "List the ECUs the read tools can target, named from the \
        ISTA semantic DB (group name, title, SGBD variants). This is the whole \
        per-model BMW map — NOT the car's fitted set; call scan_ecus for what is \
        actually present on this car. Without the DB, target ECUs by raw hex \
        address like 0x12. Does not require a connection."
    )]
    pub async fn list_ecus(&self) -> Result<Json<ListEcusResult>, McpError> {
        let catalog = self.catalog();
        let db_available = catalog.is_some();
        let (ecus, db_error) = match ecu::list(catalog.as_ref()) {
            Ok(ecus) => (ecus, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let note = if !db_available {
            "No semantic DB — target ECUs by raw hex address like 0x12. Build the DB \
             (scripts/build-semantic-db.sh) for names and the full map."
                .to_string()
        } else if db_error.is_some() {
            "The semantic DB is present but the ECU query failed — see db_error.".to_string()
        } else {
            "ECU map from the ISTA semantic DB. Call scan_ecus for the fitted set.".to_string()
        };
        Ok(Json(ListEcusResult {
            ecus,
            db_available,
            note,
            db_error,
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
        let variant = self
            .resolve_list_variant(req.variant.as_deref(), req.ecu.as_deref())
            .await?;
        let measurements = self
            .measurements(Some(&variant))
            .ok_or_else(|| no_sgbd(&variant))?;

        // Fold with the same function read_data's name resolution uses, so a term
        // that matches here also resolves there (Unicode case + ß≡ss, not ASCII).
        let query = req.search.as_deref().map(fold_for_match);
        let matching: Vec<&Measurement> = measurements
            .all()
            .into_iter()
            .filter(|m| match &query {
                None => true,
                Some(q) => [&m.arg, &m.result_name, &m.description]
                    .iter()
                    .any(|field| fold_for_match(field).contains(q)),
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
            variant,
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
        let variant = self
            .resolve_list_variant(req.variant.as_deref(), req.ecu.as_deref())
            .await?;
        let functions = self
            .service_functions(&variant)
            .ok_or_else(|| no_sgbd(&variant))?;

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
            variant,
            count: infos.len(),
            functions: infos,
            note: "Read-only catalog. Derived frames are UNCONFIRMED ([verify against \
                   capture]); a human runs only low-risk derived functions in the CLI behind \
                   --confirm. This server never executes a service function."
                .to_string(),
        }))
    }

    /// Discover the ECUs actually fitted on this car by reading the gateway SVT.
    ///
    /// # Errors
    /// Returns a tool error if not connected or the SVT read fails.
    #[tool(
        description = "Discover the ECUs actually FITTED on this car by reading the \
        gateway's installed-ECU list (SVT) — not the full generic model map. Requires \
        a prior connect. Results are cached for the session; pass rescan=true to \
        re-read. Use this before read_all_faults so you reason about the real car."
    )]
    pub async fn scan_ecus(
        &self,
        Parameters(req): Parameters<ScanEcusRequest>,
    ) -> Result<Json<ScanEcusResult>, McpError> {
        let catalog = self.catalog();
        let mut guard = self.state.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;

        let (addrs, cached) = match (req.rescan, conn.fitted()) {
            (false, Some(fitted)) => (fitted.to_vec(), true),
            _ => {
                let list = conn.client.read_ecu_list().await.map_err(|e| {
                    McpError::internal_error(format!("reading the gateway SVT: {e}"), None)
                })?;
                conn.set_fitted(list.addresses.clone());
                (list.addresses, false)
            }
        };
        let ecus = addrs
            .iter()
            .map(|&address| {
                let (group_name, title) = ecu_names(address, catalog.as_ref());
                FittedEcuInfo {
                    address_hex: format!("0x{address:02X}"),
                    group_name,
                    title,
                }
            })
            .collect();
        let note = if cached {
            "Cached fitted-ECU list from an earlier read this session — pass rescan=true \
             to re-read."
                .to_string()
        } else {
            format!(
                "Read {} installed ECU(s) from the gateway SVT.",
                addrs.len()
            )
        };
        Ok(Json(ScanEcusResult { ecus, note }))
    }

    /// Read faults from every fitted ECU in one call.
    ///
    /// # Errors
    /// Returns a tool error if not connected.
    #[tool(
        description = "Read faults from EVERY fitted ECU in one call. Scans (or \
        reuses the cached fitted list), then reads and decodes each ECU's DTCs, \
        splitting real faults from 'not tested this cycle' catalog noise (counted, \
        not shown). Requires connect. This is the whole-car health check; for one \
        ECU's not-tested entries in full, use read_faults with include_not_tested."
    )]
    pub async fn read_all_faults(
        &self,
        Parameters(req): Parameters<ReadAllFaultsRequest>,
    ) -> Result<Json<ReadAllFaultsResult>, McpError> {
        let catalog = self.catalog();
        let scanned = {
            let mut guard = self.state.lock().await;
            let conn = guard.as_mut().ok_or_else(not_connected)?;
            let addrs: Vec<u8> = match (req.rescan, conn.fitted()) {
                (false, Some(fitted)) => fitted.to_vec(),
                _ => {
                    let list = conn.client.read_ecu_list().await.map_err(|e| {
                        McpError::internal_error(format!("reading the gateway SVT: {e}"), None)
                    })?;
                    conn.set_fitted(list.addresses.clone());
                    list.addresses
                }
            };
            conn.client
                .scan_faults(&addrs, self.config.scan_concurrency())
                .await
        };

        let mut total_relevant = 0usize;
        let ecus: Vec<EcuFaultsInfo> = scanned
            .into_iter()
            .map(|ef| {
                total_relevant += ef.relevant.len();
                let (_group, title) = ecu_names(ef.address, catalog.as_ref());
                EcuFaultsInfo {
                    address_hex: format!("0x{:02X}", ef.address),
                    title,
                    faults: ef
                        .relevant
                        .iter()
                        .map(|d| fault_info(d, ef.address, catalog.as_ref()))
                        .collect(),
                    not_tested_count: ef.not_tested,
                    error: ef.error,
                }
            })
            .collect();

        Ok(Json(ReadAllFaultsResult {
            ecus,
            total_relevant,
            db_available: catalog.is_some(),
            note: "Whole-car scan: real faults per fitted ECU; 'not tested this cycle' entries \
                   are counted only. Read one ECU in full with read_faults (include_not_tested)."
                .to_string(),
        }))
    }

    /// Clear stored faults on every fitted ECU — the whole-car write; confirm-gated.
    ///
    /// # Errors
    /// Returns a tool error when `confirm` is false, or when not connected.
    #[tool(description = "Clear stored fault codes on EVERY fitted ECU — the \
        whole-car version of clear_faults. Standard UDS 0x14 per ECU; the ONLY write \
        this server exposes, batched. REQUIRES confirm=true. It discards EVERY ECU's \
        freeze-frame/snapshot data and can reset OBD readiness monitors car-wide, so \
        run read_all_faults first, tell the human exactly what is stored across the \
        car, and pass confirm=true only on their explicit go-ahead. Each ECU is \
        pre-read (codes recorded), cleared, and re-read to verify. Cannot actuate, \
        run service functions, or code.")]
    pub async fn clear_all_faults(
        &self,
        Parameters(req): Parameters<ClearAllFaultsRequest>,
    ) -> Result<Json<ClearAllFaultsResult>, McpError> {
        // Blast-radius rule: refuse the state change before touching anything.
        if !req.confirm {
            return Err(McpError::invalid_params(
                "refusing to clear faults across the whole car: this erases EVERY fitted ECU's \
                 stored DTCs together with their freeze-frame data and can reset OBD readiness \
                 monitors car-wide. Run read_all_faults, confirm with the human, then re-call \
                 with confirm=true."
                    .to_string(),
                None,
            ));
        }
        let reports = {
            let mut guard = self.state.lock().await;
            let conn = guard.as_mut().ok_or_else(not_connected)?;
            let addrs: Vec<u8> = match (req.rescan, conn.fitted()) {
                (false, Some(fitted)) => fitted.to_vec(),
                _ => {
                    let list = conn.client.read_ecu_list().await.map_err(|e| {
                        McpError::internal_error(format!("reading the gateway SVT: {e}"), None)
                    })?;
                    conn.set_fitted(list.addresses.clone());
                    list.addresses
                }
            };
            conn.client.clear_faults_all(&addrs).await
        };

        let mut cleared_clean = 0usize;
        let ecus: Vec<EcuClearInfo> = reports
            .into_iter()
            .map(|r| {
                if r.verified_clean {
                    cleared_clean += 1;
                }
                EcuClearInfo {
                    address_hex: format!("0x{:02X}", r.address),
                    codes_before: r.before.iter().map(dtc_code_hex).collect(),
                    verified_clean: r.verified_clean,
                    error: r.error,
                }
            })
            .collect();

        Ok(Json(ClearAllFaultsResult {
            ecus,
            cleared_clean,
            note: "Whole-car clear done. Every ECU's freeze-frames are discarded and readiness \
                   monitors may reset; a still-active fault sets its code again on a later drive. \
                   Re-run read_all_faults to verify."
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
                "BMW F-series diagnostics: reads, plus exactly two confirmation-gated \
                 writes (clear_faults per ECU, clear_all_faults whole-car) — both the \
                 standard UDS 0x14. Call connect first (discovers the gateway or uses a \
                 configured IP, reads the VIN). One connection reaches every ECU. \
                 scan_ecus finds the ECUs actually FITTED on this car (from the gateway \
                 SVT); list_ecus is the whole per-model map. read_faults targets one ECU by \
                 hex address (\"0x12\"), ISTA group name (\"d_0012\"), or variant name \
                 (\"d72n47a0\") and splits real faults from not-tested noise; \
                 read_all_faults does that across the whole car. read_data reads one \
                 live value; list_measurements discovers an ECU's SGBD measurements \
                 (oil/coolant temperatures, DPF soot/ash mass, regeneration status, \
                 RPM, …). A `variant` (the ECU SGBD) can be passed explicitly, resolved \
                 from a learned per-VIN profile, or from a single DB candidate — or pass \
                 `ecu` and let the server resolve it. list_service_functions lists \
                 resets/actuations/calibrations with risk tiers — for reasoning only; it \
                 never runs them. clear_faults / clear_all_faults erase stored DTCs and \
                 refuse without confirm=true — they discard freeze-frames and can reset \
                 readiness monitors, so read first and get the human's go-ahead. This \
                 server cannot actuate components, run service functions, code, or send \
                 any derived-unconfirmed write frame — those stay in the CLI with a human \
                 in the loop. It disconnects the car session automatically on exit. Fault \
                 text and the ECU map come from the ISTA SQLiteDB; reads still work (raw) \
                 without it."
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
    variant: Option<&str>,
    measurements: Option<&Measurements>,
) -> Result<u16, McpError> {
    match (req.did.as_deref(), req.name.as_deref()) {
        (Some(did), None) => parse_hex_u16(did).map_err(|e| McpError::invalid_params(e, None)),
        (None, Some(name)) => {
            let Some(variant) = variant else {
                return Err(McpError::invalid_params(
                    "reading by `name` needs a `variant` (the ECU SGBD, e.g. \"d72n47a0\") — pass \
                     one, or an `ecu` whose variant can be resolved — to load the catalog",
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
///
/// The ECU address is normalized to the `0x12` form the server's own `ecu`
/// parameters accept, so a listed entry round-trips into read_data/read_faults;
/// an unparsable `SG_ADR` cell passes through verbatim.
fn measurement_info(measurement: &Measurement) -> MeasurementInfo {
    let ecu_address = match parse_sg_adr(&measurement.sg_adr) {
        Some(address) => format!("0x{address:02X}"),
        None => measurement.sg_adr.clone(),
    };
    MeasurementInfo {
        id_hex: format!("{:04X}", measurement.id),
        name: measurement.name().to_string(),
        arg: measurement.arg.clone(),
        result_name: measurement.result_name.clone(),
        unit: measurement.unit.clone(),
        ecu_address,
    }
}

/// Parse an SGBD `SG_ADR` cell (bare hex like "12", optionally 0x-prefixed).
///
/// `None` — no routing check or normalization possible — for the `-` placeholder,
/// an empty cell, or non-hex content; a read then proceeds unchecked rather than
/// failing on odd table data.
fn parse_sg_adr(s: &str) -> Option<u8> {
    let t = s.trim();
    let t = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u8::from_str_radix(t, 16).ok()
}

/// The 3-byte DTC as six hex digits, e.g. "D9040A" — one format for read + clear.
fn dtc_code_hex(dtc: &Dtc) -> String {
    format!("{:02X}{:02X}{:02X}", dtc.code[0], dtc.code[1], dtc.code[2])
}

/// Build a decoded [`FaultInfo`] for a DTC at `address`, with DB text when available.
fn fault_info(dtc: &Dtc, address: u8, catalog: Option<&Catalog>) -> FaultInfo {
    let descriptions = catalog
        .and_then(|c| c.describe_dtc(address, dtc.code).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|desc| FaultDescription {
            variant: desc.ecu_variant,
            saecode: desc.saecode,
            text: desc.title_en.or(desc.title_de),
        })
        .collect();
    FaultInfo {
        code_hex: dtc_code_hex(dtc),
        status_hex: format!("{:02X}", dtc.status),
        status_flags: status_flags(dtc.status)
            .into_iter()
            .map(String::from)
            .collect(),
        descriptions,
    }
}

/// The 3-byte DTC parsed from a hex string like `"240000"` (optional `0x`/spaces).
///
/// # Errors
/// Returns a human message when `code` is not six hex digits (three bytes).
fn parse_dtc_code(code: &str) -> Result<[u8; 3], String> {
    let trimmed = code.trim();
    let body = trimmed
        .strip_prefix("0x")
        .or_else(|| trimmed.strip_prefix("0X"))
        .unwrap_or(trimmed);
    let hex: String = body.chars().filter(|c| !c.is_whitespace()).collect();
    if hex.len() != 6 || !hex.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(format!(
            "invalid DTC code {code:?}: expected 6 hex digits (3 bytes), e.g. \"240000\""
        ));
    }
    // Each 2-char slice is valid hex (checked above), so the parse cannot fail.
    let byte = |i: usize| u8::from_str_radix(&hex[i..i + 2], 16).expect("validated hex digits");
    Ok([byte(0), byte(2), byte(4)])
}

/// Format bytes as space-separated hex, e.g. `[0x52, 0x05]` → `"52 05"`.
fn hex_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Decode a snapshot region into DTOs, appending human notes for the empty/raw cases.
fn decode_snapshot_dtos(
    region: Option<&DtcRecordRegion>,
    defs: Option<&FreezeFrameDefs>,
    catalog: Option<&Catalog>,
    notes: &mut Vec<String>,
) -> Vec<SnapshotFieldInfo> {
    let Some(region) = region else {
        notes.push("No freeze-frame (19 04) stored for this DTC.".to_string());
        return Vec::new();
    };
    let Some(defs) = defs else {
        notes.push(format!(
            "Freeze-frame present ({} raw bytes) but no SGBD variant to decode it — \
             pass `variant` or set --sgbd-dir.",
            region.body.len()
        ));
        return Vec::new();
    };
    let decoded = defs.snapshot.decode(region, catalog);
    if let Some(tail) = &decoded.undecoded_tail {
        notes.push(format!(
            "Stopped decoding the snapshot at an unrecognized identifier; {} trailing \
             byte(s) left raw.",
            tail.len()
        ));
    }
    decoded
        .fields
        .into_iter()
        .map(|f| SnapshotFieldInfo {
            id_hex: format!("{:04X}", f.uwnr),
            label: f.label,
            value: f.value,
            unit: f.unit,
            available: f.available,
            raw_hex: hex_bytes(&f.raw),
        })
        .collect()
}

/// Decode an extended-data region into DTOs, appending notes for the empty/raw cases.
fn decode_ext_dtos(
    region: Option<&DtcRecordRegion>,
    defs: Option<&FreezeFrameDefs>,
    notes: &mut Vec<String>,
) -> Vec<ExtDataFieldInfo> {
    let Some(region) = region else {
        notes.push("No extended data (19 06) stored for this DTC.".to_string());
        return Vec::new();
    };
    let Some(defs) = defs else {
        return Vec::new();
    };
    let decoded = defs.extended.decode(region);
    if let Some(tail) = &decoded.undecoded_tail {
        notes.push(format!(
            "Stopped decoding extended data at an unknown record; {} trailing byte(s) \
             left raw.",
            tail.len()
        ));
    }
    decoded
        .records
        .into_iter()
        .map(|r| ExtDataFieldInfo {
            record_hex: format!("{:02X}", r.record),
            label: r.label,
            value: r.value,
            raw_hex: hex_bytes(&r.raw),
        })
        .collect()
}

/// The canonical group name and title for `address`, from the DB (both `None` if absent).
fn ecu_names(address: u8, catalog: Option<&Catalog>) -> (Option<String>, Option<String>) {
    catalog
        .and_then(|c| c.ecus().ok())
        .and_then(|slots| slots.into_iter().find(|s| s.address == address))
        .map(|slot| (Some(slot.group_name), slot.title))
        .unwrap_or((None, None))
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

#[cfg(test)]
mod tests {
    use klartext_sgbd::Table;

    use super::*;

    /// A two-measurement catalog: motor temp (unique names) + two rows sharing
    /// the description "Statuswort" (the real-DDE ambiguity shape).
    fn test_measurements() -> Measurements {
        let columns = [
            "ARG",
            "ID",
            "RESULTNAME",
            "INFO",
            "EINHEIT",
            "LABEL",
            "L/H",
            "DATENTYP",
            "NAME",
            "MUL",
            "DIV",
            "ADD",
            "SG_ADR",
            "SERVICE",
            "ARG_TABELLE",
            "RES_TABELLE",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let row = |arg: &str, id: &str, result: &str, info: &str| {
            vec![
                arg,
                id,
                result,
                info,
                "degC",
                "-",
                "-",
                "unsigned int",
                "-",
                "1",
                "-",
                "0",
                "12",
                "22;2C",
                "-",
                "-",
            ]
            .into_iter()
            .map(str::to_string)
            .collect::<Vec<_>>()
        };
        Measurements::from_table(&Table {
            name: "SG_FUNKTIONEN".to_string(),
            columns,
            rows: vec![
                row(
                    "ITMOT",
                    "0x4BC3",
                    "STAT_MOTORTEMPERATUR_WERT",
                    "Motortemperatur",
                ),
                row("B_a", "0x1000", "STAT_A_WERT", "Statuswort"),
                row("B_b", "0x0999", "STAT_B_WERT", "Statuswort"),
            ],
        })
    }

    fn request(did: Option<&str>, name: Option<&str>, variant: Option<&str>) -> ReadDataRequest {
        ReadDataRequest {
            ecu: "0x12".to_string(),
            did: did.map(String::from),
            name: name.map(String::from),
            variant: variant.map(String::from),
        }
    }

    #[test]
    fn resolve_read_target_takes_exactly_one_identifier() {
        let m = test_measurements();
        for req in [
            request(None, None, None),
            request(Some("F190"), Some("ITMOT"), None),
        ] {
            let err = resolve_read_target(&req, req.variant.as_deref(), Some(&m)).unwrap_err();
            assert!(err.message.contains("exactly one"), "{}", err.message);
        }
    }

    #[test]
    fn resolve_read_target_parses_a_hex_did() {
        assert_eq!(
            resolve_read_target(&request(Some("0x4BC3"), None, None), None, None).unwrap(),
            0x4BC3
        );
    }

    #[test]
    fn resolve_read_target_by_name_needs_a_variant_and_a_catalog() {
        let m = test_measurements();
        let err =
            resolve_read_target(&request(None, Some("ITMOT"), None), None, Some(&m)).unwrap_err();
        assert!(err.message.contains("variant"), "{}", err.message);
        let err = resolve_read_target(
            &request(None, Some("ITMOT"), Some("d72n47a0")),
            Some("d72n47a0"),
            None,
        )
        .unwrap_err();
        assert!(err.message.contains("no SGBD"), "{}", err.message);
    }

    #[test]
    fn resolve_read_target_resolves_a_unique_name() {
        let m = test_measurements();
        let req = request(None, Some("Motortemperatur"), Some("d72n47a0"));
        assert_eq!(
            resolve_read_target(&req, req.variant.as_deref(), Some(&m)).unwrap(),
            0x4BC3
        );
    }

    #[test]
    fn resolve_read_target_errors_with_candidate_ids_on_an_ambiguous_name() {
        // The §12b contract: never guess between same-named measurements — error
        // and hand back the ids so the caller re-reads by `did`.
        let m = test_measurements();
        let req = request(None, Some("Statuswort"), Some("d72n47a0"));
        let err = resolve_read_target(&req, req.variant.as_deref(), Some(&m)).unwrap_err();
        assert!(err.message.contains("ambiguous"), "{}", err.message);
        assert!(err.message.contains("0999"), "{}", err.message);
        assert!(err.message.contains("1000"), "{}", err.message);
    }

    #[test]
    fn resolve_read_target_reports_unknown_names_helpfully() {
        let m = test_measurements();
        let req = request(None, Some("Kein solcher Wert"), Some("d72n47a0"));
        let err = resolve_read_target(&req, req.variant.as_deref(), Some(&m)).unwrap_err();
        assert!(err.message.contains("list_measurements"), "{}", err.message);
    }

    #[test]
    fn parse_sg_adr_reads_bare_and_prefixed_hex_and_skips_placeholders() {
        assert_eq!(parse_sg_adr("12"), Some(0x12));
        assert_eq!(parse_sg_adr("0x40"), Some(0x40));
        assert_eq!(parse_sg_adr(" 12 "), Some(0x12));
        assert_eq!(parse_sg_adr("-"), None);
        assert_eq!(parse_sg_adr(""), None);
        assert_eq!(parse_sg_adr("gateway"), None);
    }
}
