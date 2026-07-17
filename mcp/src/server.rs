//! The MCP server: diagnostic tools over a held car session — reads, a read-only
//! EDIABAS job runner (`run_job`), plus two confirmation-gated clears
//! (`clear_faults`, `clear_all_faults`) that share one standard UDS 0x14 write frame.
//!
//! [`KlartextServer`] is the rmcp [`ServerHandler`] served over stdio. It holds an
//! optional car connection in shared state. The refined (M9) safety invariant:
//! every tool is non-mutating except the two confirmation-gated clears —
//! `clear_faults` (one ECU) and `clear_all_faults` (the same UDS 0x14 write
//! batched over the fitted ECUs, not a new capability) — both well-defined,
//! non-physical, and reversible-by-reappearance, and both refuse to run without
//! `confirm: true`. Physical actuation and derived-unconfirmed WRITE frames are
//! never executable here; they stay in the CLI with a human in the loop. (The M6
//! dynamic-read `0x2C` define — session-transient read plumbing — is the one
//! derived sequence the read path uses, by the M6 decision.)
//!
//! ## `run_job` and the read-only gate (Item 5 P2)
//! [`KlartextServer::run_job`] executes an ECU's own BEST/2 bytecode for one named
//! EDIABAS job (a `STATUS_*`/measurement READ) and surfaces its result sets. It
//! stays inside the invariant by construction: the job's every ECU exchange is
//! wrapped in a [`GatedExchange::read_only`], which classifies each outgoing UDS
//! service ID and refuses any write/actuation/flashing service *at the transmit
//! boundary*, before the car is touched — so a job whose bytecode emits a write
//! dies at the seam with no frame sent, and only reads (`0x22`/`0x2C`/`0x19`) and
//! session plumbing reach the ECU. This is the P2 read slice; the confirmed-WRITE
//! job path (spec §6) is P3 and deliberately absent.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex as StdMutex};

use klartext_best::{
    BareUdsTransport, Ecu, ExchangeError, GatedExchange, ResultData, ResultSet, RunError,
    TelegramExchange,
};
use klartext_client::DiagnosticClient;
use klartext_semantic::dtc::status_flags;
use klartext_semantic::{
    Catalog, Category, FreezeFrameDefs, Measurement, MeasurementCatalogEntry, Measurements, Risk,
    ServiceFunction, ServiceFunctions, build_read_request, did, fold_for_match,
    misrouted_dynamic_measurement,
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
    ConfiguredEcuInfo, ConnectRequest, ConnectResult, DisconnectResult, EcuClearInfo,
    EcuFaultsInfo, EcuIdentDto, ExtDataFieldInfo, FaultDescription, FaultDetailResult, FaultDocDto,
    FaultHelpRequest, FaultHelpResult, FaultInfo, IdFieldDto, InfoMemoryRequest, InfoMemoryResult,
    ListEcusResult, ListMeasurementsRequest, ListMeasurementsResult, ListServiceFunctionsRequest,
    ListServiceFunctionsResult, MeasurementInfo, NamedValue, ReadAllFaultsRequest,
    ReadAllFaultsResult, ReadDataRequest, ReadDataResult, ReadFaultDetailRequest,
    ReadFaultsRequest, ReadFaultsResult, RunJobRequest, RunJobResult, ScanEcusRequest,
    ScanEcusResult, ServiceFunctionInfo, SnapshotFieldInfo, VehicleIdentityResult, VehicleOrderDto,
};
use crate::ecu;
use crate::session::{self, Connection, SessionState};

/// Most measurements one `list_measurements` call returns.
///
/// The DDE alone defines ~1800 `SG_FUNKTIONEN` rows; an uncapped listing would
/// flood an AI client's context. The cap is generous for a searched listing and
/// the reply's `total` + note make any truncation explicit, never silent.
const MAX_LISTED_MEASUREMENTS: usize = 200;

/// Most named result values one `run_job` call surfaces across all sets.
///
/// A multi-set job (e.g. a per-cylinder read) can emit many values; an uncapped
/// reply would flood an AI client's context. The cap is generous for a real job
/// and the reply's `total` + `note` make any truncation explicit, never silent.
const MAX_RUN_JOB_RESULTS: usize = 200;

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
        Requires a prior connect. `ecu` is a hex address (\"0x12\"), an ISTA group \
        name (\"d_0012\"), or a variant name (\"d72n47a0\") — see list_ecus. Returns \
        each fault's raw code, decoded ISO status flags, and human description text \
        (when the semantic DB is available)."
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

    /// Read an ECU's secondary/info memory (Infospeicher, UDS 22 2000).
    ///
    /// # Errors
    /// Returns a tool error if not connected, the ECU is unknown, or the read fails.
    #[tool(
        description = "Read the ECU's SECONDARY / info memory (Infospeicher) via UDS \
        22 2000 (job IS_LESEN) — a store DISTINCT from the 19 02 fault memory that ISTA \
        shows alongside faults, where info/event entries that don't rise to a stored \
        fault can live. Use it to check for an entry that read_faults does not show. \
        Requires a prior connect. `ecu` as in read_faults. Each entry decodes like a \
        fault (code + ISO status + text). `supported`=false means the ECU keeps no such \
        memory. NOTE: the response record layout is derived from the SGBD and pending an \
        on-car capture — treat entries as provisional and see raw_hex."
    )]
    pub async fn read_info_memory(
        &self,
        Parameters(req): Parameters<InfoMemoryRequest>,
    ) -> Result<Json<InfoMemoryResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let info = {
            let guard = self.state.lock().await;
            let conn = guard.as_ref().ok_or_else(not_connected)?;
            conn.client
                .read_info_memory(address)
                .await
                .map_err(|e| McpError::internal_error(format!("reading info memory: {e}"), None))?
        };
        let (supported, version, entries, raw_hex) = match info {
            Some(m) => (
                true,
                m.version,
                m.entries
                    .iter()
                    .map(|d| fault_info(d, address, catalog.as_ref()))
                    .collect(),
                hex_bytes(&m.raw),
            ),
            None => (false, None, Vec::new(), String::new()),
        };
        let note = if supported {
            "Record layout is derived from the SGBD and pending an on-car capture; \
             entries are provisional — see raw_hex."
                .to_string()
        } else {
            "This ECU does not answer the info-memory read (22 2000).".to_string()
        };
        Ok(Json(InfoMemoryResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            supported,
            version,
            entries,
            raw_hex,
            db_available: catalog.is_some(),
            note,
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

        let descriptions = describe_faults(catalog.as_ref(), address, dtc);

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

    /// Look up a fault's ISTA documentation — its meaning and linked repair procedures.
    ///
    /// DB-only: needs NO car connection (unlike read_fault_detail). Resolves the ECU
    /// and DTC, returns the ISTA fault text, the FKB fault-description prose (in `body`,
    /// when the doc store is built), and the titles/types of every linked ISTA document.
    /// The linked procedure/other documents stay pointers (title, type, doc number,
    /// safety flag, stable id) — their prose is a later phase.
    ///
    /// # Errors
    /// Returns an invalid-params error when the ECU cannot be resolved or `code` is not
    /// a 3-byte hex DTC. It never needs a connection: a missing DB or a pre-item-4
    /// extract degrades to an empty `docs` list with an explanatory note, not an error.
    #[tool(
        description = "Look up an ISTA fault's meaning and its linked repair/diagnosis \
        documents by ECU + code — WITHOUT connecting to the car (pure semantic-DB read). \
        Pass `ecu` (hex like 0x12, group name, or variant) and `code` (the 3-byte DTC hex \
        from read_faults, e.g. 4B1234). Returns the fault text plus each linked ISTA \
        document's title, type (FKB = fault description; others are procedures), doc \
        number, and safety flag. The FKB fault-description prose is returned in `body` \
        when the doc store is built (via scripts/build-semantic-db.sh); the linked \
        procedure documents stay titles/pointers (their prose is a later phase)."
    )]
    pub async fn fault_help(
        &self,
        Parameters(req): Parameters<FaultHelpRequest>,
    ) -> Result<Json<FaultHelpResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let dtc = parse_dtc_code(&req.code).map_err(|e| McpError::invalid_params(e, None))?;

        let descriptions = describe_faults(catalog.as_ref(), address, dtc);

        let docs: Vec<FaultDocDto> = catalog
            .as_ref()
            .and_then(|c| c.fault_help(address, dtc).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|d| FaultDocDto {
                title: d.title,
                infotype: d.infotype,
                docnumber: d.docnumber,
                safety_relevant: d.safety_relevant,
                infoobject_id: d.infoobject_id,
            })
            .collect();

        // The rendered FKB prose, when the Phase 1 doc store (sibling klartext-docs.db)
        // is built; empty otherwise. A missing store or body degrades to empty, never
        // an error — the `docs` pointers above still apply.
        let body = catalog
            .as_ref()
            .and_then(|c| c.fault_body(address, dtc).ok())
            .unwrap_or_default();

        let note = if catalog.is_none() {
            "No semantic DB — build it (scripts/build-semantic-db.sh) for fault docs.".to_string()
        } else if docs.is_empty() {
            "No ISTA documents linked to this fault (or the DB predates the repair-doc \
             extract — rebuild it)."
                .to_string()
        } else {
            format!(
                "{} ISTA document(s) linked; FKB fault-description prose is in `body` when \
                 the doc store is built — linked procedure docs stay titles/pointers.",
                docs.len()
            )
        };

        Ok(Json(FaultHelpResult {
            ecu: req.ecu,
            code_hex: format!("{:02X}{:02X}{:02X}", dtc[0], dtc[1], dtc[2]),
            descriptions,
            docs,
            body,
            note,
        }))
    }

    /// Read the full vehicle identity in one call: VIN, FA, I-Stufe, fitted ECUs, idents.
    ///
    /// All autonomous-safe `0x22` reads. The client returns raw bytes; the ECU names,
    /// the FA decode, and each identification field's name/text are resolved here at
    /// the surface (semantic layer), keeping the client protocol-pure.
    ///
    /// # Errors
    /// Returns a tool error if not connected, or if the gateway SVT read fails — there
    /// is no probe fallback, so a failed installed-ECU read surfaces as an error rather
    /// than a degraded, empty identity.
    #[tool(
        description = "Read the car's full identity in one call: VIN, integration \
        level (I-Stufe), the vehicle order (FA — model/type/paint/upholstery/options, \
        where decodable), the authoritative list of FITTED ECUs by name (from the \
        gateway SVT), and each ECU's identification block (hardware/software part \
        numbers, system name, serial). All standard UDS 0x22 reads — safe and \
        non-mutating. NOTE: the FA field decode and the SVT/identification response \
        framing are derived from disassembly and pending an on-car capture, so treat \
        FA fields as provisional and expect some identification DIDs to be absent."
    )]
    pub async fn identify_vehicle(&self) -> Result<Json<VehicleIdentityResult>, McpError> {
        let catalog = self.catalog();
        let identity = {
            let guard = self.state.lock().await;
            let conn = guard.as_ref().ok_or_else(not_connected)?;
            conn.client.identify_vehicle().await.map_err(|e| {
                McpError::internal_error(format!("reading vehicle identity: {e}"), None)
            })?
        };

        let named = klartext_semantic::name_ecu_list(catalog.as_ref(), &identity.ecus);
        let ecus = named
            .into_iter()
            .map(|n| ConfiguredEcuInfo {
                address_hex: format!("0x{:02X}", n.address),
                group_name: n.name,
                title: n.title,
                responding: None,
            })
            .collect();

        let fa = klartext_semantic::decode_vehicle_order(&identity.vehicle_order_raw);
        let vehicle_order = VehicleOrderDto {
            version: fa.version,
            baureihe: fa.baureihe,
            typ_schluessel: fa.typ_schluessel,
            lackcode: fa.lackcode,
            polstercode: fa.polstercode,
            build_date: fa.build_date,
            options: fa.options,
            raw_hex: hex_bytes(&fa.raw),
        };

        let identification = identity
            .identification
            .into_iter()
            .map(|block| EcuIdentDto {
                address_hex: format!("0x{:02X}", block.address),
                name: klartext_semantic::name_ecu_list(catalog.as_ref(), &[block.address])
                    .into_iter()
                    .next()
                    .and_then(|n| n.name),
                fields: block
                    .fields
                    .into_iter()
                    .map(|f| {
                        // Naming/text lives at the surface (the client returns raw),
                        // same as the existing read_data path.
                        let d = did::decode(f.did, &f.raw);
                        IdFieldDto {
                            did_hex: format!("{:04X}", f.did),
                            name: d.name.map(str::to_owned),
                            text: d.text,
                            raw_hex: hex_bytes(&f.raw),
                        }
                    })
                    .collect(),
            })
            .collect();

        Ok(Json(VehicleIdentityResult {
            vin: identity.vin,
            i_stufe: identity.i_stufe,
            vehicle_order,
            ecus,
            identification,
            notes: vec![
                "SVT/identification framing and FA field decode are derived from \
                 disassembly and pending an on-car capture — treat as provisional."
                    .to_string(),
            ],
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
        let raw_hex = hex_bytes(&raw);

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

    /// Run a read-only EDIABAS job (e.g. `STATUS_LESEN`) and return its named results.
    ///
    /// Executes the ECU's own BEST/2 bytecode for `job` over a
    /// [`GatedExchange::read_only`]: the job's every outgoing UDS service ID is
    /// classified and any write/actuation/flashing service is refused *at the
    /// transmit boundary*, so a job whose bytecode emits a write dies at the seam
    /// with no frame sent. The `ecu` resolves to a transmit address and, via the M10
    /// ladder, to the SGBD `variant` whose bytecode runs; unlike a data read a job
    /// cannot degrade to raw, so an unresolvable or unloadable variant is a hard
    /// error. `args` join with `;` into the EDIABAS argument buffer.
    ///
    /// # Errors
    /// Returns an invalid-params error when the ECU cannot be resolved, no variant
    /// can be resolved, or its SGBD cannot be loaded; a not-connected error with no
    /// live session; an invalid-request error naming the CLI when the job emits a
    /// write (the read-only gate refused it before the car was touched); and an
    /// internal error for any other run fault.
    #[tool(
        description = "Run a read-only EDIABAS job (e.g. STATUS_LESEN) and return \
        its named result sets. Executes the ECU's own bytecode over a read-only gate \
        that refuses any write/actuation service at the transmit boundary — a \
        write-emitting job is rejected before any frame reaches the car (run those \
        from the klartext CLI, where a human is in the loop). Requires a prior \
        connect. `ecu` as in read_faults; `variant` is the ECU SGBD (e.g. \
        \"d72n47a0\"), resolved from the ecu when omitted (the server needs \
        --sgbd-dir). `job` is a job name from the SGBD (a STATUS_* read); `args` are \
        the EDIABAS argument fields joined with ';' (e.g. [\"ARG\", \"<name>\"] -> \
        \"ARG;<name>\"). A dynamic 2C-define measurement (e.g. oil temp) is read via \
        read_data instead — run_job redirects it with guidance. Returns the job's \
        named, typed result sets. NOTE: response \
        scaling runs the ECU's disassembled bytecode and is pending an on-car \
        capture — treat the values as provisional."
    )]
    pub async fn run_job(
        &self,
        Parameters(req): Parameters<RunJobRequest>,
    ) -> Result<Json<RunJobResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        // Resolve the variant via the M10 ladder (explicit → learned profile →
        // DB-unique). A job NEEDS its SGBD bytecode to run — there is no
        // degrade-to-raw here — so an unresolved variant is a hard error.
        let conn_vin = self.state.lock().await.as_ref().and_then(|c| c.vin.clone());
        let variant = self
            .resolve_variant(
                address,
                req.variant.as_deref(),
                catalog.as_ref(),
                conn_vin.as_deref(),
            )
            .ok_or_else(|| self.variant_candidates_error(address, catalog.as_ref()))?;
        // Load the ECU bytecode. A missing --sgbd-dir or non-bare name is `no_sgbd`;
        // a present-but-unreadable `.prg` surfaces its parse error. Either way an
        // explicit-but-unloadable variant is a configuration error the caller sees.
        let path = self.sgbd_path(&variant).ok_or_else(|| no_sgbd(&variant))?;
        let ecu = Ecu::open(&path).map_err(|e| {
            McpError::invalid_params(format!("cannot load SGBD variant '{variant}': {e}"), None)
        })?;
        let arg_bytes = req.args.join(";").into_bytes();

        // STATUS_LESEN is a static reader: it emits a static 0x22 the ECU rejects for a
        // dynamic (2C-define) measurement. Redirect such a read to read_data (which
        // drives the selektiv-lesen sequence) rather than run a doomed request.
        if let Some(measurements) = self.measurements(Some(variant.as_str()))
            && let Some(name) = misrouted_dynamic_measurement(&measurements, &req.job, &req.args)
        {
            return Err(McpError::invalid_params(
                format!(
                    "'{name}' is a dynamic (2C-define) measurement; {} emits a static 0x22 \
                     the ECU rejects. Read it with read_data, which drives the selektiv-lesen \
                     sequence.",
                    req.job
                ),
                None,
            ));
        }

        // Hold the session lock for the whole run: the bridge borrows the client
        // across the job's awaits (a tokio Mutex is safe to hold across await), and
        // one car serializes its diagnostic traffic anyway. The read-only gate is
        // the OUTERMOST layer, so it vetoes the VM's telegram before the bridge
        // translates it and the car is ever touched.
        let results = {
            let guard = self.state.lock().await;
            let conn = guard.as_ref().ok_or_else(not_connected)?;
            let gate = GatedExchange::read_only(TelegramExchange::new(SessionBridge {
                client: &conn.client,
            }));
            ecu.run_job(&req.job, address, &arg_bytes, &gate)
                .await
                .map_err(|e| run_error_to_mcp(&req.job, e))?
        };

        let (sets, total, note) = surface_run_job(&results);
        Ok(Json(RunJobResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            variant,
            job: req.job,
            sets,
            total,
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
        `variant` is the ECU SGBD name (e.g. \"d72n47a0\" for the F2x diesel DDE); \
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
        // Fold with the same function read_data's name resolution uses, so a term
        // that matches here also resolves there (Unicode case + ß≡ss, not ASCII).
        let query = req.search.as_deref().map(fold_for_match);
        let sgbd = self.measurements(Some(&variant)).filter(|m| !m.is_empty());

        let (infos, total, from_catalog) = if let Some(measurements) = &sgbd {
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
            let mut infos: Vec<MeasurementInfo> = matching
                .into_iter()
                .take(MAX_LISTED_MEASUREMENTS)
                .map(measurement_info)
                .collect();
            // Cross-reference ISTA's measurement catalog for the job that reads
            // each result — discovery metadata the SGBD rows don't carry.
            if let Some(catalog) = self.catalog()
                && let Ok(entries) = catalog.measurements(&variant)
            {
                enrich_with_catalog_jobs(&mut infos, &entries);
            }
            (infos, total, false)
        } else {
            // No SGBD measurements (an inline-scaling ECU, or no --sgbd-dir): fall back
            // to ISTA's measurement catalog (the "index") so the ECU still lists — names
            // + units + reading job, though not scalable by read_data.
            let catalog = self.catalog();
            let entries = match catalog.as_ref() {
                Some(c) => c.measurements(&variant).map_err(|e| {
                    McpError::internal_error(format!("reading the measurement catalog: {e}"), None)
                })?,
                None => Vec::new(),
            };
            if entries.is_empty() {
                return Err(no_sgbd(&variant));
            }
            let matching: Vec<&MeasurementCatalogEntry> = entries
                .iter()
                .filter(|e| match &query {
                    None => true,
                    Some(q) => fold_for_match(&e.name).contains(q),
                })
                .collect();
            let total = matching.len();
            let addr = req
                .ecu
                .as_deref()
                .and_then(|e| ecu::resolve(e, catalog.as_ref()).ok())
                .map(|a| format!("0x{a:02X}"))
                .unwrap_or_default();
            let infos: Vec<MeasurementInfo> = matching
                .into_iter()
                .take(MAX_LISTED_MEASUREMENTS)
                .map(|e| catalog_measurement_info(e, &addr))
                .collect();
            (infos, total, true)
        };

        let source_note = if from_catalog {
            " Source: ISTA measurement catalog (this ECU has no SGBD — names + units \
             only, not scalable by read_data)."
        } else {
            " Read a value via read_data: `did` = the entry's id_hex (or `name` = its \
             arg/name) plus this `variant`."
        };
        let note = if infos.len() < total {
            format!(
                "Showing {} of {total} matching measurements — narrow with `search`.{source_note}",
                infos.len()
            )
        } else if from_catalog {
            format!("Read-only measurement index; no car connection was made.{source_note}")
        } else {
            format!("Read-only catalog from the SGBD; no car connection was made.{source_note}")
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

    /// List the gateway's CONFIGURED ECUs (VCM 22 3F07) + the responding subset (3F08).
    ///
    /// # Errors
    /// Returns a tool error if not connected or the VCM list read fails.
    #[tool(
        description = "List this car's CONFIGURED ECUs from the gateway VCM (22 3F07) — \
        the stored 'should be present' superset for the model, NOT the full generic \
        model map and NOT ISTA's post-filtered ~11 view (ISTA reads the same list and \
        reduces it by per-model bus/housing rules). Each ECU carries a `responding` \
        flag from the gateway's actively-responding list (22 3F08) when available — the \
        truer 'really there' signal; `responding_count` summarizes it. Requires a prior \
        connect; results cached per session (rescan=true to re-read)."
    )]
    pub async fn scan_ecus(
        &self,
        Parameters(req): Parameters<ScanEcusRequest>,
    ) -> Result<Json<ScanEcusResult>, McpError> {
        let catalog = self.catalog();
        let mut guard = self.state.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;

        let (addrs, cached) = fitted_addrs(conn, req.rescan).await?;
        // The actively-responding subset (VCM 22 3F08); None if the gateway does not
        // answer it or on any transport hiccup — a best-effort enrichment of the list.
        let responding: Option<std::collections::BTreeSet<u8>> = conn
            .client
            .read_responding_ecu_list()
            .await
            .ok()
            .flatten()
            .map(|list| list.addresses.into_iter().collect());
        let responding_count = responding
            .as_ref()
            .map(|r| addrs.iter().filter(|a| r.contains(a)).count());
        let ecus = addrs
            .iter()
            .map(|&address| {
                let (group_name, title) = ecu_names(address, catalog.as_ref());
                ConfiguredEcuInfo {
                    address_hex: format!("0x{address:02X}"),
                    group_name,
                    title,
                    responding: responding.as_ref().map(|r| r.contains(&address)),
                }
            })
            .collect();
        let subset = match responding_count {
            Some(n) => format!(" {n} of them are actively responding (22 3F08)."),
            None => " The gateway did not report a responding subset (22 3F08).".to_string(),
        };
        let note = if cached {
            format!(
                "Cached CONFIGURED ECU list (VCM 22 3F07) from earlier this session — pass \
                 rescan=true to re-read. This is the stored superset, not ISTA's post-filtered \
                 view.{subset}"
            )
        } else {
            format!(
                "Read {} CONFIGURED ECU(s) from the gateway (VCM 22 3F07 — the stored superset, \
                 NOT ISTA's per-model-filtered ~11).{subset}",
                addrs.len()
            )
        };
        Ok(Json(ScanEcusResult {
            ecus,
            configured_count: addrs.len(),
            responding_count,
            note,
        }))
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
            let (addrs, _) = fitted_addrs(conn, req.rescan).await?;
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
            let (addrs, _) = fitted_addrs(conn, req.rescan).await?;
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

/// Resolve the fitted-ECU addresses: the session cache, or a fresh gateway-SVT read.
///
/// Shared by `scan_ecus`, `read_all_faults`, and `clear_all_faults`. Returns
/// `(addresses, cached)`: `cached` is true when the list came from an earlier read
/// this session, false after a live SVT read (which also refreshes the cache).
/// `rescan` forces the read. Callers that don't surface the source ignore the flag.
///
/// # Errors
/// Returns an internal error if the gateway SVT read fails.
async fn fitted_addrs(conn: &mut Connection, rescan: bool) -> Result<(Vec<u8>, bool), McpError> {
    match (rescan, conn.fitted()) {
        (false, Some(fitted)) => Ok((fitted.to_vec(), true)),
        _ => {
            let list = conn.client.read_ecu_list().await.map_err(|e| {
                McpError::internal_error(format!("reading the gateway SVT: {e}"), None)
            })?;
            conn.set_fitted(list.addresses.clone());
            Ok((list.addresses, false))
        }
    }
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

/// Bridges the BEST/2 engine's bare-UDS transport seam onto the held session.
///
/// [`BareUdsTransport`] is the `client`-free seam the VM's live exchange drives;
/// this couples it to the server's live [`DiagnosticClient`], borrowed under the
/// session lock for the run's duration. Each `(target, uds)` forwards to
/// [`DiagnosticClient::request`], and any client error flattens into the engine's
/// message-only [`ExchangeError::Transport`] — which is how `klartext-best` stays
/// free of a `klartext-client` dependency. Mirrors the CLI's bridge (Task 7).
struct SessionBridge<'a> {
    /// The live diagnostic client each bare-UDS request is forwarded to.
    client: &'a DiagnosticClient,
}

#[async_trait::async_trait]
impl BareUdsTransport for SessionBridge<'_> {
    async fn call(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
        self.client
            .request(target, uds)
            .await
            .map_err(|e| ExchangeError::Transport(format!("{e}")))
    }
}

/// Map a [`RunError`] from `run_job` onto the MCP error the caller sees.
///
/// A read-only-gate refusal — a job whose bytecode emitted a write, so the gate
/// blocked it before any frame reached the car — becomes an invalid-request that
/// names the gated service ID and points at the CLI, the P2 line: the agent reads,
/// a human runs writes. Every other fault is an internal error carrying the job's
/// context. (The gate blocks the write regardless of how the job masks the
/// resulting trap; a masked job may instead surface a different, non-`Refused`
/// error — but no write frame is ever transmitted either way.)
fn run_error_to_mcp(job: &str, error: RunError) -> McpError {
    if let RunError::Exchange(ExchangeError::Refused { sid, .. }) = &error {
        return McpError::invalid_request(
            format!(
                "job '{job}' emits UDS service 0x{sid:02X} (a write/actuation); the read-only \
                 gate refused it before any frame reached the car. Run a write-emitting job from \
                 the klartext CLI, where a human is in the loop."
            ),
            None,
        );
    }
    McpError::internal_error(format!("running job '{job}': {error}"), None)
}

/// Surface a job's result sets into DTOs, applying the [`MAX_RUN_JOB_RESULTS`] cap.
///
/// Returns the (possibly capped) sets, the full `total` across every set before the
/// cap, and a truncation note — `None` unless the cap dropped values, so a
/// truncation is never silent (mirrors `list_measurements`). The cap bounds the
/// total values surfaced, not per set, and stops at a set boundary once reached.
fn surface_run_job(results: &ResultSet) -> (Vec<Vec<NamedValue>>, usize, Option<String>) {
    let total: usize = results.iter_sets().map(Iterator::count).sum();
    let mut sets: Vec<Vec<NamedValue>> = Vec::new();
    let mut emitted = 0usize;
    for set in results.iter_sets() {
        if emitted >= MAX_RUN_JOB_RESULTS {
            break;
        }
        let mut out = Vec::new();
        for (name, value) in set {
            if emitted >= MAX_RUN_JOB_RESULTS {
                break;
            }
            out.push(named_value(name, value));
            emitted += 1;
        }
        sets.push(out);
    }
    let note = (emitted < total).then(|| {
        format!(
            "Showing {emitted} of {total} result values (capped at {MAX_RUN_JOB_RESULTS}) — this \
             job emitted more values than the per-call limit."
        )
    });
    (sets, total, note)
}

/// Render one EDIABAS [`ResultData`] as a [`NamedValue`] with its type tag.
///
/// The tag is the EDIABAS result type the store opcode picked (`B`/`W`/`D`/`I`/`R`/
/// `S`/`Y`); a binary result renders as spaced uppercase hex, the rest directly.
fn named_value(name: &str, value: &ResultData) -> NamedValue {
    let (rendered, kind) = match value {
        ResultData::Byte(b) => (b.to_string(), "B"),
        ResultData::Word(w) => (w.to_string(), "W"),
        ResultData::Dword(d) => (d.to_string(), "D"),
        ResultData::Int(n) => (n.to_string(), "I"),
        ResultData::Real(r) => (r.to_string(), "R"),
        ResultData::Text(t) => (t.clone(), "S"),
        ResultData::Binary(bytes) => (hex_bytes(bytes), "Y"),
    };
    NamedValue {
        name: name.to_string(),
        value: rendered,
        kind: kind.to_string(),
    }
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
        source: "sgbd".to_string(),
        job: None,
    }
}

/// Fill each SGBD-sourced entry's `job` from the ISTA measurement catalog.
///
/// Joins on the EDIABAS result name — the SGBD's result rows and ISTA's
/// `XEP_ECURESULTS.NAME` share that namespace. Entries the catalog does not know
/// keep `job: None`; with a pre-v4 semantic DB (no `measurement` table) the entry
/// list is empty and this is a no-op.
fn enrich_with_catalog_jobs(infos: &mut [MeasurementInfo], entries: &[MeasurementCatalogEntry]) {
    let jobs: HashMap<&str, &str> = entries
        .iter()
        .filter_map(|e| e.job.as_deref().map(|job| (e.name.as_str(), job)))
        .collect();
    for info in infos.iter_mut() {
        if info.job.is_none()
            && let Some(job) = jobs.get(info.result_name.as_str())
        {
            info.job = Some((*job).to_string());
        }
    }
}

/// Build a [`MeasurementInfo`] from an ISTA measurement-catalog entry.
///
/// Used when the ECU has no SGBD (an inline-scaling module): carries the result
/// name, unit, and reading job from ISTA's index, with no SGBD id/arg (so it is
/// discovery only — `read_data` cannot scale it). `source` is `"ista_catalog"`.
fn catalog_measurement_info(entry: &MeasurementCatalogEntry, ecu_address: &str) -> MeasurementInfo {
    MeasurementInfo {
        id_hex: String::new(),
        name: entry.name.clone(),
        arg: String::new(),
        result_name: entry.name.clone(),
        unit: entry.unit.clone().unwrap_or_else(|| "-".to_string()),
        ecu_address: ecu_address.to_string(),
        source: "ista_catalog".to_string(),
        job: entry.job.clone(),
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

/// Decode a DTC `code` at `address` into its semantic [`FaultDescription`]s.
///
/// The one mapping shared by every fault surface (`read_faults`,
/// `read_fault_detail`, `fault_help`): each catalog row becomes its ECU variant,
/// SAE code, and English-else-German title. A missing catalog or an absent entry
/// yields an empty list rather than an error.
fn describe_faults(catalog: Option<&Catalog>, address: u8, code: [u8; 3]) -> Vec<FaultDescription> {
    catalog
        .and_then(|c| c.describe_dtc(address, code).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|d| FaultDescription {
            variant: d.ecu_variant,
            saecode: d.saecode,
            text: d.title_en.or(d.title_de),
        })
        .collect()
}

/// Build a decoded [`FaultInfo`] for a DTC at `address`, with DB text when available.
fn fault_info(dtc: &Dtc, address: u8, catalog: Option<&Catalog>) -> FaultInfo {
    let descriptions = describe_faults(catalog, address, dtc.code);
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
    fn resolve_variant_uses_a_learned_profile_keyed_by_vin() {
        // Verifies session-1 finding 2 is NOT a bug: the M10 ladder auto-resolves a
        // variant from the learned per-VIN profile that a successful scaled read_data
        // records — so a later read of the same ECU on the same car needs no explicit
        // `variant`. The first read of a many-candidate ECU still needs one (no profile
        // yet, and the DB can't disambiguate) — that is the expected first-use state.
        use clap::Parser;
        let dir = tempfile::tempdir().unwrap();
        let mut config = ServerConfig::parse_from(["klartext-mcp"]);
        config.profile_dir = Some(dir.path().to_path_buf());
        config.no_profile = false;
        let server = KlartextServer::new(config);
        let vin = "WBAVIN0000000012";

        // First use: no profile yet, no catalog to disambiguate -> unresolved.
        assert_eq!(server.resolve_variant(0x12, None, None, Some(vin)), None);
        // Seed the profile the way a successful scaled read_data does.
        crate::profile::record(dir.path(), vin, 0x12, "d72n47a0").unwrap();
        // Now the ladder resolves it from the learned profile — no explicit variant.
        assert_eq!(
            server
                .resolve_variant(0x12, None, None, Some(vin))
                .as_deref(),
            Some("d72n47a0")
        );
        // A different VIN does not inherit this car's profile.
        assert_eq!(
            server.resolve_variant(0x12, None, None, Some("OTHERVIN00000000")),
            None
        );
        // An explicit variant always wins over the learned one.
        assert_eq!(
            server
                .resolve_variant(0x12, Some("explicit"), None, Some(vin))
                .as_deref(),
            Some("explicit")
        );

        // With profiles disabled (--no-profile), the learned branch is skipped.
        let mut off = ServerConfig::parse_from(["klartext-mcp"]);
        off.profile_dir = Some(dir.path().to_path_buf());
        off.no_profile = true;
        let server_off = KlartextServer::new(off);
        assert_eq!(
            server_off.resolve_variant(0x12, None, None, Some(vin)),
            None
        );
    }

    #[test]
    fn sgbd_listing_gains_the_catalog_job_by_result_name() {
        // The catalog cross-reference: an SGBD entry whose EDIABAS result name the
        // ISTA measurement catalog knows gains that job; unknown names (and entries
        // whose catalog row has no job) stay None. A pre-v4 DB yields no entries,
        // which must be a no-op rather than an error.
        let catalog_entry = |name: &str, job: Option<&str>| MeasurementCatalogEntry {
            name: name.to_string(),
            unit: None,
            mul: None,
            offset: None,
            round: None,
            format: None,
            job: job.map(String::from),
        };
        let m = test_measurements();
        let mut infos: Vec<MeasurementInfo> = m.all().into_iter().map(measurement_info).collect();
        assert!(infos.iter().all(|i| i.job.is_none()));

        enrich_with_catalog_jobs(
            &mut infos,
            &[
                catalog_entry("STAT_MOTORTEMPERATUR_WERT", Some("STATUS_MESSWERTE_BLOCK")),
                catalog_entry("STAT_A_WERT", None),
            ],
        );
        let by_result = |infos: &[MeasurementInfo], result: &str| {
            infos
                .iter()
                .find(|i| i.result_name == result)
                .unwrap()
                .job
                .clone()
        };
        assert_eq!(
            by_result(&infos, "STAT_MOTORTEMPERATUR_WERT").as_deref(),
            Some("STATUS_MESSWERTE_BLOCK")
        );
        assert_eq!(by_result(&infos, "STAT_A_WERT"), None);
        assert_eq!(by_result(&infos, "STAT_B_WERT"), None);

        // Empty catalog (pre-v4 DB): nothing changes, nothing fails.
        enrich_with_catalog_jobs(&mut infos, &[]);
        assert_eq!(
            by_result(&infos, "STAT_MOTORTEMPERATUR_WERT").as_deref(),
            Some("STATUS_MESSWERTE_BLOCK")
        );
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

    #[test]
    fn run_error_refused_maps_to_a_cli_hint() {
        // The safety seam: a write-emitting job trips the read-only gate, and the
        // resulting Refused surfaces as an invalid-request that names the gated SID
        // and points the caller at the CLI — never a generic 500.
        let err = run_error_to_mcp(
            "STEUERN_X",
            RunError::Exchange(ExchangeError::Refused {
                sid: 0x2E,
                frame: vec![0x84, 0x12, 0xF1, 0x2E],
            }),
        );
        assert!(err.message.contains("2E"), "{}", err.message);
        assert!(err.message.contains("CLI"), "{}", err.message);
    }

    #[test]
    fn run_error_other_faults_map_to_internal_error() {
        // A non-refusal fault (here a missing job) is an internal error carrying the
        // job's context, and never mislabeled as a gate refusal.
        let err = run_error_to_mcp("NOPE", RunError::JobNotFound("NOPE".to_string()));
        assert!(err.message.contains("NOPE"), "{}", err.message);
        assert!(!err.message.contains("CLI"), "{}", err.message);
    }

    #[test]
    fn run_job_results_surface_named_values_with_type_tags() {
        // Every ResultData variant renders to a NamedValue with its EDIABAS type tag
        // (B/W/D/I/R/S/Y), so the AI client sees name + value + kind for each result.
        let mut rs = ResultSet::new();
        rs.push_named("STAT_WERT", ResultData::Real(89.96));
        rs.push_named("STAT_EINH", ResultData::Text("degC".into()));
        rs.push_named("RAW", ResultData::Binary(vec![0x0A, 0xBC]));
        let (sets, total, note) = surface_run_job(&rs);
        assert_eq!(total, 3);
        assert!(note.is_none());
        let set = &sets[0];
        assert_eq!(set[0].name, "STAT_WERT");
        assert_eq!(set[0].kind, "R");
        assert_eq!(set[1].kind, "S");
        assert_eq!(set[2].value, "0A BC");
        assert_eq!(set[2].kind, "Y");
    }

    #[test]
    fn surface_run_job_preserves_set_structure() {
        // A multi-set job (e.g. per-cylinder) keeps its set boundaries end to end,
        // so the caller can still tell one set's values from the next.
        let mut rs = ResultSet::new();
        rs.push_named("A", ResultData::Byte(1));
        rs.new_set();
        rs.push_named("B", ResultData::Byte(2));
        rs.push_named("C", ResultData::Byte(3));
        let (sets, total, note) = surface_run_job(&rs);
        assert_eq!(total, 3);
        assert!(note.is_none());
        assert_eq!(sets.len(), 2);
        assert_eq!(sets[0].len(), 1);
        assert_eq!(sets[1].len(), 2);
    }

    #[test]
    fn surface_run_job_truncates_with_a_note_never_silently() {
        // More values than the per-call cap: the reply carries the full `total`, caps
        // the surfaced values, and explains the truncation — never a silent drop.
        let mut rs = ResultSet::new();
        for i in 0..(MAX_RUN_JOB_RESULTS + 5) {
            rs.push_named(
                &format!("V{i}"),
                ResultData::Word(u16::try_from(i).unwrap()),
            );
        }
        let (sets, total, note) = surface_run_job(&rs);
        assert_eq!(total, MAX_RUN_JOB_RESULTS + 5);
        assert_eq!(
            sets.iter().map(Vec::len).sum::<usize>(),
            MAX_RUN_JOB_RESULTS
        );
        let note = note.expect("a truncation note");
        assert!(note.contains(&MAX_RUN_JOB_RESULTS.to_string()), "{note}");
    }
}
