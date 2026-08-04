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
//! `confirm: true`. Physical actuation and derived-unconfirmed WRITE frames are not
//! executable here today; no surface runs them yet. (The M6 dynamic-read `0x2C`
//! define — session-transient read plumbing — is the one derived sequence the read
//! path uses, by the M6 decision.)
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
use klartext_client::{
    ClearSequenceReport, DiagnosticClient, SupplierJobReport, SupplierJobRunner,
    VehicleComposition, VinCheck, compare_vin, supplier_clear_jobs,
};
use klartext_semantic::dtc::status_flags;
use klartext_semantic::{
    Catalog, Category, EcuSlot, FixedFunction, FreezeFrameDefs, Measurement,
    MeasurementCatalogEntry, Measurements, Risk, ServiceFunction, ServiceFunctionCatalogEntry,
    ServiceFunctions, build_read_request, did, fold_for_match, misrouted_dynamic_measurement,
};
use klartext_service::{Hold, JobRunner, Phase, ServiceReport, Teardown, hold_for, invocations};
use klartext_uds::{Dtc, DtcRecordRegion, FaultSource, Presence};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{Implementation, ServerCapabilities, ServerInfo};
use rmcp::{ErrorData as McpError, Json, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::config::ServerConfig;
use crate::dto::{
    ClampCycleInfo, ClearAllFaultsRequest, ClearAllFaultsResult, ClearFaultsRequest,
    ClearFaultsResult, ConfiguredEcuInfo, ConnectRequest, ConnectResult, DetailDepth,
    DisconnectResult, EcuClearInfo, EcuFaultsInfo, EcuIdentDto, ExtDataFieldInfo, FaultDescription,
    FaultDetailResult, FaultDocDto, FaultHelpRequest, FaultHelpResult, FaultInfo, IdFieldDto,
    ListEcusResult, ListMeasurementsRequest, ListMeasurementsResult, ListServiceFunctionIdsRequest,
    ListServiceFunctionIdsResult, ListServiceFunctionsRequest, ListServiceFunctionsResult,
    MeasurementInfo, NamedValue, PhaseOutcomeDto, ReadAllFaultsRequest, ReadAllFaultsResult,
    ReadDataRequest, ReadDataResult, ReadFaultDetailRequest, ReadFaultsRequest, ReadFaultsResult,
    RunJobRequest, RunJobResult, RunServiceFunctionRequest, RunServiceFunctionResult,
    ScanEcusRequest, ScanEcusResult, ServiceFunctionCatalogInfo, ServiceFunctionInfo,
    SnapshotFieldInfo, StopServiceRequest, StopServiceResult, SupplierJobInfo,
    VehicleIdentityResult, VehicleOrderDto,
};
use crate::ecu;
use crate::session::{self, Connection, HeldService, SessionState};

/// Most measurements one `list_measurements` call returns.
///
/// The DDE alone defines ~1800 `SG_FUNKTIONEN` rows; an uncapped listing would
/// flood an AI client's context. The cap is generous for a searched listing and
/// the reply's `total` + note make any truncation explicit, never silent.
/// The job every group SGBD exposes to identify the ECU behind it, and the result
/// it emits. ISTA reads exactly this result to set `ECU_SGBD`
/// (`RheingoldDiagnostics` `DoAfterIdentProcessing` :223322).
const IDENT_JOB: &str = "IDENTIFIKATION";
/// The group ident job's variant result name.
const IDENT_RESULT: &str = "VARIANTE";
/// The per-entry info-memory detail job — the `IS_LESEN` store's counterpart to
/// `FS_LESEN_DETAIL`, which ISTA runs while iterating `sg.INFO`
/// (`RheingoldDiagnostics` :226004).
const INFO_DETAIL_JOB: &str = "IS_LESEN_DETAIL";
/// The info-memory store read. Its request frame is a PER-ECU choice — 334 shipped
/// SGBDs emit `22 20 00`, 255 emit `19 17 0C 01` — so running the job is how
/// klartext sends the right one without choosing.
const INFO_READ_JOB: &str = "IS_LESEN";

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
    /// handler so a killed server never leaves a dangling session to time out. Any
    /// service function still HELD (owner ruling 2) is torn down first, so a signalled
    /// shutdown never strands an actuated component.
    pub async fn disconnect_now(&self) -> bool {
        self.take_session_and_stop_held().await
    }

    /// Run any outstanding held-service teardown, then take (drop) the session.
    ///
    /// Returns whether a live session was present. Owner ruling 2's "stop MUST also
    /// fire on disconnect": a component left energised by a held `run_service_function`
    /// is returned to safe here — against the still-open session — before the
    /// connection is dropped. Best-effort: a teardown that cannot run is logged to
    /// STDERR (never stdout, which carries the JSON-RPC stream) and the session is
    /// dropped regardless, since a caller on the disconnect path is already leaving.
    async fn take_session_and_stop_held(&self) -> bool {
        let taken = self.state.lock().await.take();
        let Some(conn) = taken else {
            return false;
        };
        if let Some(held) = conn.held().cloned() {
            self.teardown_held(&conn, &held).await;
        }
        true
    }

    /// Tear a held service function down against the live session, best-effort.
    ///
    /// Re-resolves the function's `Reset` invocations from the catalog and its SGBD
    /// and runs [`klartext_service::stop_service`] over the confirmed-write bridge on
    /// the still-open `conn`. Every failure — no DB, an unloadable SGBD, a failing
    /// teardown job — is logged to STDERR and swallowed: this runs on the disconnect
    /// path, where a best-effort return-to-safe is the goal and there is no caller to
    /// return an error to.
    async fn teardown_held(&self, conn: &Connection, held: &HeldService) {
        let Some(catalog) = self.catalog() else {
            tracing::error!(
                function_id = held.function_id,
                "stop-on-disconnect: no semantic DB; a held actuation cannot be torn down"
            );
            return;
        };
        let rows = match catalog.job_parameters_for_function(&held.variant, held.function_id) {
            Ok(rows) => rows,
            Err(error) => {
                tracing::error!(
                    function_id = held.function_id,
                    %error,
                    "stop-on-disconnect: cannot load the function; component may stay forced"
                );
                return;
            }
        };
        let invs = invocations(&rows);
        let Some(path) = self.sgbd_path(&held.variant) else {
            tracing::error!(
                function_id = held.function_id,
                variant = %held.variant,
                "stop-on-disconnect: no SGBD; component may stay forced"
            );
            return;
        };
        let ecu = match Ecu::open(&path) {
            Ok(ecu) => ecu,
            Err(error) => {
                tracing::error!(
                    function_id = held.function_id,
                    %error,
                    "stop-on-disconnect: cannot load SGBD; component may stay forced"
                );
                return;
            }
        };
        let bridge = ConfirmedWriteBridge {
            ecu: &ecu,
            client: &conn.client,
        };
        let report =
            klartext_service::stop_service(&bridge, held.function_id, held.address, &invs).await;
        if let Teardown::Failed(error) = &report.teardown {
            tracing::error!(
                function_id = held.function_id,
                %error,
                "stop-on-disconnect: teardown FAILED — component may still be actuating"
            );
        } else {
            tracing::info!(
                function_id = held.function_id,
                "stop-on-disconnect: held actuation returned to safe"
            );
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
    /// Describe the car the way ISTA's supplier gates read it.
    ///
    /// ISTA evaluates those gates against live `VecInfo` — the ECUs actually
    /// identified on this car — so the groups and variants come from the fitted list,
    /// not from the per-model catalog. A variant is included only when the M10 ladder
    /// RESOLVES it (explicit → learned per-VIN profile → single DB candidate): the
    /// catalog's full candidate list would over-approximate, and firing a supplier
    /// write on an ECU the car does not have is the failure mode that matters.
    ///
    /// SALAPA is `None` — klartext does not decode the FA option list yet
    /// (`klartext_semantic::decode_vehicle_order` returns an empty `options` pending
    /// an on-car capture of the 214-byte vector), and a gate that needs an option code
    /// must report "unknown" rather than guess. Today that means ISTA's `D_KBM` step
    /// can never fire here; the report says so per job.
    fn vehicle_composition(
        &self,
        fitted: &[u8],
        catalog: Option<&Catalog>,
        vin: Option<&str>,
    ) -> VehicleComposition {
        let mut groups = Vec::new();
        let mut sgbds = Vec::new();
        for &address in fitted {
            if let (Some(group), _) = ecu_names(address, catalog) {
                groups.push(group);
            }
            if let Some(variant) = self.resolve_variant(address, None, catalog, vin) {
                sgbds.push(variant);
            }
        }
        VehicleComposition {
            sgbds,
            groups,
            sa_codes: None,
        }
    }

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

    /// The path to a group SGBD (`.grp`), guarded like [`Self::sgbd_path`].
    fn grp_path(&self, group: &str) -> Option<PathBuf> {
        let dir = self.config.sgbd_dir.as_deref()?;
        if group.is_empty() || Path::new(group).file_name() != Some(OsStr::new(group)) {
            tracing::warn!(group, "ignoring SGBD group: must be a bare file name");
            return None;
        }
        Some(dir.join(format!("{group}.grp")))
    }

    /// The group SGBDs to try identifying `address` with, UDS-era (`g_`) first.
    ///
    /// The DB records several groups per address — `0x78` is both `d_klima` and
    /// `g_klima` — because one address is served by different generations. `g_`
    /// first is not a guess about the ECU: the `d_` groups are the K-line-era jobs
    /// whose interface-configuration opcodes (`xsetpar`/`xawlen`/`shmset`) this VM
    /// does not implement, so they cannot run over HSFZ anyway. A group that
    /// identifies the wrong way round simply finds no assignment row and yields no
    /// `VARIANTE`, so the order is a preference, never a decision.
    fn ident_groups_for(address: u8, catalog: Option<&Catalog>) -> Vec<String> {
        catalog
            .and_then(|c| c.ecus().ok())
            .and_then(|slots| slots.into_iter().find(|s| s.address == address))
            .map(Self::order_ident_groups)
            .unwrap_or_default()
    }

    /// Orders one address's recorded groups for the ident rung: `g_` first, each
    /// name once, DB order otherwise preserved. Split out so the rule is testable
    /// without a database.
    fn order_ident_groups(slot: EcuSlot) -> Vec<String> {
        let mut groups = Vec::with_capacity(1 + slot.extra_groups.len());
        for group in std::iter::once(slot.group_name).chain(slot.extra_groups) {
            if !groups
                .iter()
                .any(|g: &String| g.eq_ignore_ascii_case(&group))
            {
                groups.push(group);
            }
        }
        groups.sort_by_key(|g| !g.to_ascii_lowercase().starts_with("g_"));
        groups
    }

    /// Identify `address`'s variant by running its group SGBD's `IDENTIFIKATION`.
    ///
    /// This is the rung ISTA itself stands on. It does not guess a variant either:
    /// it runs the group SGBD's ident job and takes the job result `VARIANTE`
    /// (`RheingoldDiagnostics` `DoAfterIdentProcessing`: `mECU.ECU_SGBD =
    /// identJob.getStringResult("VARIANTE")`, :223322). The job reads UDS `22 F150`
    /// from the ECU and looks the answer up in the shared group table
    /// (`t_grtb`'s `ZuordnungsTabelleUDS`, keyed `"<address> <ident index>"`), so
    /// the variant is BMW's own data, not a klartext inference.
    ///
    /// A resolved variant is recorded in the learned per-VIN profile, so this costs
    /// one round trip per ECU per car rather than one per read.
    ///
    /// Everything here degrades to `None`: no `--sgbd-dir`, no group in the DB, a
    /// `.grp` that will not load, an ident job this VM cannot yet run (166 of the
    /// 427 shipped groups still use K-line-era opcodes), a car that does not
    /// answer, or a job that reaches `eoj` without emitting `VARIANTE`. The caller
    /// then reports the same "need a variant" error as before — the rung can only
    /// add resolutions, never take one away.
    async fn ident_variant(
        &self,
        address: u8,
        groups: Vec<String>,
        vin: Option<&str>,
    ) -> Option<String> {
        for group in groups {
            let Some(path) = self.grp_path(&group) else {
                continue;
            };
            let Ok(ecu) = Ecu::open(&path) else {
                continue;
            };
            // The ident read is UDS 0x22 — a read, so the read-only gate passes it.
            let results = {
                let guard = self.state.lock().await;
                let Some(conn) = guard.as_ref() else {
                    return None; // not connected: no rung to stand on
                };
                let gate = GatedExchange::read_only(TelegramExchange::new(SessionBridge {
                    client: &conn.client,
                }));
                match ecu.run_job(IDENT_JOB, address, b"", &gate).await {
                    Ok(results) => results,
                    Err(e) => {
                        tracing::debug!(%group, address, error = %e, "group ident did not run");
                        continue;
                    }
                }
            };
            let variant =
                results
                    .iter_sets()
                    .flatten()
                    .find_map(|(name, value)| match (name, value) {
                        (IDENT_RESULT, ResultData::Text(v)) if !v.trim().is_empty() => {
                            Some(v.trim().to_string())
                        }
                        _ => None,
                    });
            if let Some(variant) = variant {
                tracing::info!(variant = %variant, %group, address, "variant identified (group ident)");
                if let (Some(dir), Some(vin)) = (self.config.profile_dir(), vin)
                    && let Err(e) = crate::profile::record(&dir, vin, address, &variant)
                {
                    tracing::warn!(error = %e, "could not record the identified variant");
                }
                return Some(variant);
            }
        }
        None
    }

    /// Runs the offline ladder, and on a miss returns the groups to identify with.
    ///
    /// Split from [`Self::resolve_variant_live`] because a `&Catalog` is not `Send`
    /// and rmcp boxes every tool future as `Send`: the catalog must be finished with
    /// BEFORE the ident job's awaits, so this sync half hands the async half nothing
    /// but owned data.
    fn variant_or_ident_groups(
        &self,
        address: u8,
        explicit: Option<&str>,
        catalog: Option<&Catalog>,
        vin: Option<&str>,
    ) -> Result<String, Vec<String>> {
        match self.resolve_variant(address, explicit, catalog, vin) {
            Some(variant) => Ok(variant),
            None => Err(Self::ident_groups_for(address, catalog)),
        }
    }

    /// [`Self::resolve_variant`] plus the live group-ident rung when it comes up empty.
    ///
    /// Takes the [`Self::variant_or_ident_groups`] plan rather than a catalog, so the
    /// offline rungs (explicit → learned profile → DB-unique) are always tried first
    /// and the car is only ever run when nothing cheaper answered.
    async fn resolve_variant_live(
        &self,
        address: u8,
        plan: Result<String, Vec<String>>,
        vin: Option<&str>,
    ) -> Option<String> {
        match plan {
            Ok(variant) => Some(variant),
            Err(groups) => self.ident_variant(address, groups, vin).await,
        }
    }

    /// Read one INFO-MEMORY entry's detail by running the ECU's `IS_LESEN_DETAIL`.
    ///
    /// The info-memory counterpart of the `19 09`/`19 06`/`19 04` reads, and ISTA
    /// does it exactly this way: `apiJob(sg.ECU_SGBD, "IS_LESEN_DETAIL",
    /// item.F_ORT.ToString(), …)` while iterating `sg.INFO`
    /// (`RheingoldDiagnostics` `doECUReadISDetails` :226004). `F_ORT` is an
    /// integer there (`getintResult`), so the single positional argument is the
    /// code in **decimal** — which is also what the job's own `parl` expects.
    ///
    /// The job builds its own frames (`22 2000`, then `22 20 <position>` for the
    /// entry it matched), so klartext transmits no hand-rolled info-memory read and
    /// invents no record layout. All of it is UDS `0x22`, so the read-only gate
    /// passes it unchanged.
    ///
    /// Degrades to an empty list, never an error: no `--sgbd-dir` or unresolved
    /// variant, a `.prg` that will not load or has no such job (only 342 of 1405
    /// ECUs document an info memory), a car that does not answer, or a VM fault.
    /// The caller's `source` field already says the entry is an info-memory one.
    async fn info_memory_detail(
        &self,
        address: u8,
        dtc: [u8; 3],
        variant: Option<&str>,
    ) -> Vec<NamedValue> {
        let Some(path) = variant.and_then(|v| self.sgbd_path(v)) else {
            return Vec::new();
        };
        let Ok(ecu) = Ecu::open(&path) else {
            return Vec::new();
        };
        // ISTA's `item.F_ORT.ToString()`: the code as a decimal integer.
        let code = u32::from_be_bytes([0, dtc[0], dtc[1], dtc[2]]);
        let args = code.to_string().into_bytes();
        let results = {
            let guard = self.state.lock().await;
            let Some(conn) = guard.as_ref() else {
                return Vec::new();
            };
            let gate = GatedExchange::read_only(TelegramExchange::new(SessionBridge {
                client: &conn.client,
            }));
            match ecu.run_job(INFO_DETAIL_JOB, address, &args, &gate).await {
                Ok(results) => results,
                Err(e) => {
                    tracing::debug!(address, error = %e, "IS_LESEN_DETAIL did not run");
                    return Vec::new();
                }
            }
        };
        results
            .iter_sets()
            .flatten()
            .map(|(name, value)| named_value(name, value))
            .take(MAX_RUN_JOB_RESULTS)
            .collect()
    }

    /// Read the info-memory entries by running the ECU's own `IS_LESEN` job.
    ///
    /// klartext's direct read hardcodes `22 2000`, and that frame is only right for
    /// part of the fleet: of the 885 shipped `IS_LESEN` jobs, **334 emit `22 20 00`
    /// but 255 emit `19 17 0C 01`** (ISO 14229 `reportUserDefMemoryDTCByStatusMask`),
    /// split generationally — `acsm3/4/5` use the former, `acsm6/7`, `adcam_*` and
    /// `bat48_*` the latter. On one of those ECUs the hardcoded frame draws a
    /// negative and klartext concludes "this ECU has no info memory" when it has one.
    ///
    /// The job settles it without klartext choosing: the same job name builds
    /// whichever request its own ECU speaks and decodes the matching response, so
    /// this path is correct for both families and invents nothing. Verified offline
    /// on both — `d72n47a0` emits `22 20 00`, `acsm6` emits `19 17 0C 01`, and both
    /// yield the same named results (`crates/best/tests/info_memory_read.rs`).
    ///
    /// Returns each entry's `F_HEX_CODE` record (`[code: 3][status: 1]`, the shape
    /// klartext already decodes) paired with the ECU's own `F_ORT_TEXT` — BMW's
    /// authored description for that entry, which the raw `22 2000` read cannot give
    /// at all. `None` when there is no SGBD, no such job, or the car did not answer.
    async fn info_memory_via_job(
        &self,
        address: u8,
        variant: Option<&str>,
    ) -> Option<Vec<InfoEntry>> {
        let path = variant.and_then(|v| self.sgbd_path(v))?;
        let ecu = Ecu::open(&path).ok()?;
        let results = {
            let guard = self.state.lock().await;
            let conn = guard.as_ref()?;
            let gate = GatedExchange::read_only(TelegramExchange::new(SessionBridge {
                client: &conn.client,
            }));
            match ecu.run_job(INFO_READ_JOB, address, b"", &gate).await {
                Ok(results) => results,
                Err(e) => {
                    tracing::debug!(address, error = %e, "IS_LESEN did not run");
                    return None;
                }
            }
        };
        // One result SET per entry, so a code and its text must be paired WITHIN a
        // set rather than across the flattened stream.
        let entries: Vec<InfoEntry> = results
            .iter_sets()
            .filter_map(|set| {
                let (mut dtc, mut text) = (None, None);
                for (name, value) in set {
                    match (name, value) {
                        ("F_HEX_CODE", ResultData::Binary(bytes)) if bytes.len() >= 4 => {
                            dtc = Some(Dtc {
                                code: [bytes[0], bytes[1], bytes[2]],
                                status: bytes[3],
                            });
                        }
                        ("F_ORT_TEXT", ResultData::Text(t)) if !t.trim().is_empty() => {
                            text = Some(t.trim().to_string());
                        }
                        _ => {}
                    }
                }
                dtc.map(|dtc| InfoEntry { dtc, text })
            })
            .collect();
        Some(entries)
    }

    /// Decode the info half, preferring the ECU's own `IS_LESEN` results.
    ///
    /// `job` is what [`Self::info_memory_via_job`] returned; `direct` is whatever the
    /// hardcoded `22 2000` read produced. The job wins when it ran — it sends the
    /// right frame for this ECU and carries BMW's own text — and `direct` is the
    /// unchanged fallback when it did not.
    ///
    /// Deliberately synchronous: a `&Catalog` is not `Send` and rmcp boxes every tool
    /// future as `Send`, so the catalog must not be held across the job's awaits.
    fn info_entries_from(
        job: Option<Vec<InfoEntry>>,
        address: u8,
        direct: &[Dtc],
        direct_supported: bool,
        catalog: Option<&Catalog>,
    ) -> (Vec<FaultInfo>, bool) {
        match job {
            Some(entries) => {
                let decoded = entries
                    .iter()
                    .map(|e| {
                        fault_info_with_text(
                            &e.dtc,
                            address,
                            catalog,
                            FaultSource::InfoMemory,
                            e.text.as_deref(),
                        )
                    })
                    .collect();
                (decoded, true)
            }
            None => (
                direct
                    .iter()
                    .map(|d| fault_info(d, address, catalog, FaultSource::InfoMemory))
                    .collect(),
                direct_supported,
            ),
        }
    }

    /// Resolve each supplier job's SGBD name to `(name, address, .prg path)`.
    ///
    /// Done before the clear sequence starts because [`SupplierBridge`] must be
    /// `Sync` and `Catalog` is not. A name resolves either as a VARIANT (its `.prg`
    /// exists and the DB records it at an address) or as a GROUP (the DB says which
    /// addresses it serves, and the group's own `IDENTIFIKATION` names the fitted
    /// variant). A name that resolves as neither is simply absent from the result,
    /// and its job is then reported as not run with the reason.
    fn supplier_target_plan(
        &self,
        jobs: &[SupplierJobReport],
        catalog: Option<&Catalog>,
    ) -> Vec<(String, u8, Option<String>, Vec<String>)> {
        let mut wanted: Vec<String> = Vec::new();
        for report in jobs {
            let name = report.job.ecu.to_string();
            if !wanted.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
                wanted.push(name);
            }
        }
        let slots = catalog.and_then(|c| c.ecus().ok()).unwrap_or_default();
        // (name, address, variant-if-known) — a `None` variant means "identify it".
        let mut plan: Vec<(String, u8, Option<String>, Vec<String>)> = Vec::new();
        for name in wanted {
            // A VARIANT: its `.prg` exists and the DB records it at an address.
            if self.sgbd_path(&name).is_some_and(|p| p.exists())
                && let Some(slot) = slots.iter().find(|slot| {
                    catalog
                        .and_then(|c| c.variants(slot.address).ok())
                        .is_some_and(|vs| vs.iter().any(|v| v.name.eq_ignore_ascii_case(&name)))
                })
            {
                plan.push((name.clone(), slot.address, Some(name), Vec::new()));
                continue;
            }
            // Otherwise a GROUP: the DB says which address it serves, and the
            // group's own IDENTIFIKATION will name the fitted variant.
            if let Some(slot) = slots.iter().find(|slot| {
                slot.group_name.eq_ignore_ascii_case(&name)
                    || slot
                        .extra_groups
                        .iter()
                        .any(|g| g.eq_ignore_ascii_case(&name))
            }) {
                let groups = Self::ident_groups_for(slot.address, catalog);
                plan.push((name, slot.address, None, groups));
            }
        }
        plan
    }

    /// Finish [`Self::supplier_target_plan`]: identify the group entries and keep
    /// the ones whose `.prg` is on disk. Takes no catalog, so nothing non-`Sync`
    /// crosses the ident job's awaits.
    async fn resolve_supplier_targets(
        &self,
        plan: Vec<(String, u8, Option<String>, Vec<String>)>,
    ) -> Vec<(String, u8, PathBuf)> {
        let mut targets = Vec::new();
        for (name, address, known, groups) in plan {
            let variant = match known {
                Some(variant) => Some(variant),
                None => self.ident_variant(address, groups, None).await,
            };
            if let Some(path) = variant
                .and_then(|v| self.sgbd_path(&v))
                .filter(|p| p.exists())
            {
                targets.push((name, address, path));
            }
        }
        targets
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
                "need a `variant` for ECU 0x{address:02X} and none could be resolved: no \
                 explicit variant, no learned profile, no single DB candidate with a matching \
                 .prg, and the group SGBD's IDENTIFIKATION job did not identify it either \
                 (not connected, no --sgbd-dir, no group for this address, the ECU did not \
                 answer, or its group is one of the legacy K-line-era ones this VM cannot run \
                 yet). Candidates: {list}"
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
        with a background keepalive. Returns the gateway IP and VIN. Calling this \
        again is how you recover a dropped link (there is no auto-reconnect); when \
        you do, the VIN is re-checked against the held session. If it is a DIFFERENT \
        car this call FAILS and both sessions are closed — everything you learned \
        about the previous car is void and must not be carried across; simply call \
        connect again to start fresh on the new car. `vin_check` reports `match`, or \
        `unreadable` when the VIN could not be read, which proves nothing either way \
        and does not abort.")]
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
        // klartext has no automatic reconnect, so re-calling connect IS its
        // reconnect — the same human-driven one ISTA has (§C.6: no backoff, no
        // retry ceiling, a button press each time). Car session 1 showed the case
        // that reaches here: the gateway sends FIN+RST at ignition-off, every later
        // read fails, and the agent's only recovery is to connect again. That makes
        // this the one place a held session's identity can go stale, so run ISTA's
        // reconnect VIN check here — the VIN carried over from the previous session
        // against the one this connect just read.
        let previous_vin = self.state.lock().await.as_ref().and_then(|c| c.vin.clone());
        // Before this reconnect replaces (or a VIN mismatch clears) the old session,
        // tear down any held actuation against the STILL-LIVE old session — an
        // explicit return-to-safe, not a reliance on the ECU's S3 revert once the
        // socket drops (review finding 1b/1c). Best-effort: on a dead-link reconnect
        // (the FIN+RST case) the 0x31 will not land, which is fine — the car is gone
        // and S3 already reverted. `previous_vin` is captured above, so dropping the
        // old session here does not cost the VIN check below.
        self.take_session_and_stop_held().await;
        let conn = session::establish(&self.config, gateway_ip)
            .await
            .map_err(|e| McpError::internal_error(e, None))?;
        // `establish` already walked the VIN ladder for this session, so compare
        // that read rather than putting a second identical read on the wire.
        let vin_check = previous_vin
            .as_deref()
            .map(|previous| compare_vin(previous, conn.vin.as_deref()));

        // ISTA parity: a VIN mismatch is a HARD ABORT with a full disconnect
        // (`VciConnLossVM`, `ConnectionLossError::VehicleVinNotMatch`). Owner-ruled
        // 2026-07-19 to follow ISTA rather than complete-and-warn.
        //
        // Both connections are dropped: the new one is never installed, and the old
        // one was already torn down and taken at the top of this call (any held
        // actuation returned to safe there). Leaving the OLD session live would be
        // the worst outcome — an agent that ignored the error would carry on reading
        // a car the cable is no longer attached to. `Unreadable` does NOT abort:
        // nothing was proven either way, and refusing on "we could not tell" would
        // strand a session whenever an ECU is merely slow.
        if let Some(VinCheck::Mismatch { expected, found }) = &vin_check {
            let message = format!(
                "refusing to connect: this is a DIFFERENT car. The session was opened on VIN \
                 {expected} and this gateway reports {found}. Everything learned about the \
                 previous car is void — do not carry findings across. Both sessions have been \
                 closed. If you intended to switch cars, call connect again; this refusal only \
                 applies while the previous VIN is still held."
            );
            *self.state.lock().await = None;
            return Err(McpError::invalid_params(message, None));
        }

        let result = ConnectResult {
            connected: true,
            gateway_ip: conn.gateway_ip.to_string(),
            vin: conn.vin.clone(),
            vin_source: conn.vin_source.as_str().to_string(),
            target_ecu: format!("gateway (ZGW 0x{:02X})", klartext_hsfz::ZGW_ADDRESS),
            vin_check: vin_check.as_ref().map(vin_check_tag).map(str::to_string),
            note: connect_note(vin_check.as_ref()),
        };
        *self.state.lock().await = Some(conn);
        Ok(Json(result))
    }

    /// Read and decode one ECU's fault memory AND info memory, as ISTA reads them.
    ///
    /// # Errors
    /// Returns a tool error if not connected, the ECU is unknown, or the fault read
    /// fails. The secondary info-memory read never fails the call — a negative or
    /// silent `22 2000` degrades to `info_supported = false`.
    #[tool(
        description = "Read and decode stored faults from one ECU — BOTH its 19 02 \
        fault memory AND its 22 2000 info memory (Infospeicher), the way ISTA reads \
        them together. Requires a prior connect. `ecu` is a hex address (\"0x12\"), an \
        ISTA group name (\"d_0012\"), or a variant name (\"d72n47a0\") — see list_ecus. \
        `faults` carries the fault-memory DTCs, `info_entries` the info-memory entries; \
        every entry is marked with `source` (\"fault_memory\" / \"info_memory\") and \
        decoded the same way — raw code, ISO status flags, presence verdict, and human \
        description text when the semantic DB is available. `info_supported`=false just \
        means the ECU keeps no info memory (the normal case for most ECUs), never an \
        error. `detail` (\"none\" default / \"relevant\" / \"all\") is accepted but \
        per-fault freeze-frame detail is fetched via read_fault_detail, not inline."
    )]
    pub async fn read_faults(
        &self,
        Parameters(req): Parameters<ReadFaultsRequest>,
    ) -> Result<Json<ReadFaultsResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;

        // ISTA never reads the 19 02 fault memory without the 22 2000 info memory:
        // one request each, merged into one per-ECU view (research §A.6/§E.6). The
        // per-fault freeze-frame detail is NOT bundled — it is 2× the fault count in
        // requests and stays behind read_fault_detail / the `detail` opt-in.
        let bundle = {
            let guard = self.state.lock().await;
            let conn = guard.as_ref().ok_or_else(not_connected)?;
            conn.client
                .read_ecu_faults(address)
                .await
                .map_err(|e| McpError::internal_error(format!("reading faults: {e}"), None))?
        };

        // NO client-side status filter (parity P0.2): the request is `19 02 0C`, so
        // the ECU has already filtered to pending|confirmed, and ISTA surfaces every
        // DTC it receives. Each fault carries ISTA's own presence verdict instead.
        let present_count = bundle
            .faults
            .iter()
            .filter(|d| d.presence() == Presence::Present)
            .count();
        let faults: Vec<FaultInfo> = bundle
            .faults
            .iter()
            .map(|d| fault_info(d, address, catalog.as_ref(), FaultSource::FaultMemory))
            .collect();
        // Prefer the ECU's OWN `IS_LESEN` for the info half whenever its SGBD is at
        // hand. Two things the hardcoded `22 2000` read cannot do: it is the wrong
        // frame for the 255 of 885 shipped jobs that emit `19 17 0C 01` (so those
        // ECUs get reported as having no info memory at all), and it yields only
        // code+status where the job yields BMW's own authored text per entry.
        let conn_vin = self.state.lock().await.as_ref().and_then(|c| c.vin.clone());
        // Offline variant rungs only — identifying costs a round trip and enrichment
        // is not what the caller asked for.
        let variant = self
            .variant_or_ident_groups(address, None, catalog.as_ref(), conn_vin.as_deref())
            .ok();
        let job = self.info_memory_via_job(address, variant.as_deref()).await;
        let (info_entries, info_supported) = Self::info_entries_from(
            job,
            address,
            &bundle.info,
            bundle.info_supported,
            catalog.as_ref(),
        );

        Ok(Json(ReadFaultsResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            count: faults.len(),
            faults,
            present_count,
            info_entries,
            info_supported,
            db_available: catalog.is_some(),
            note: fault_bundle_note(req.detail),
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
        set); otherwise the raw region is returned. The 19 09 severity read is skipped \
        when the ECU SGBD declares F_SEVERITY=nein (e.g. the DDE d72n47a0), matching \
        ISTA — severity is then null. Applies to FAULT-MEMORY codes only: the ECU's \
        stores are checked first and `source` reports which one holds the code, so a \
        read_faults entry whose source is \"info_memory\" comes back with source \
        \"info_memory\", its `info_detail` filled from the ECU's own IS_LESEN_DETAIL \
        job, and no freeze frame — rather than a rejected read. NOTE: the response \
        framing is derived from ISO 14229 + disassembly and is pending an on-car \
        capture, so treat the decoded values as provisional."
    )]
    pub async fn read_fault_detail(
        &self,
        Parameters(req): Parameters<ReadFaultDetailRequest>,
    ) -> Result<Json<FaultDetailResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let dtc = parse_dtc_code(&req.code).map_err(|e| McpError::invalid_params(e, None))?;

        // Resolve the variant via the ladder — including the live group-ident rung —
        // and load the freeze-frame SGBD defs. An explicit variant whose `.prg` is
        // absent is a configuration error the caller must see; a ladder-resolved one
        // that is absent just degrades to raw.
        let conn_vin = self.state.lock().await.as_ref().and_then(|c| c.vin.clone());
        let plan = self.variant_or_ident_groups(
            address,
            req.variant.as_deref(),
            catalog.as_ref(),
            conn_vin.as_deref(),
        );
        let effective_variant = self
            .resolve_variant_live(address, plan, conn_vin.as_deref())
            .await;
        let defs = self.freeze_frame_defs(effective_variant.as_deref());
        if let (Some(variant), None) = (req.variant.as_deref(), defs.as_ref()) {
            return Err(no_sgbd(variant));
        }

        // ISTA's F_SEVERITY gate for the 19 09 read, read from the same SGBD `defs`
        // (klartext-client cannot depend on klartext-sgbd; the semantic layer resolves
        // it). `None` when the .prg is absent → the client sends 19 09 anyway (a wrong
        // guess costs one negative round trip).
        let severity_supported = defs.as_ref().and_then(|d| d.severity_supported);

        let detail = {
            let guard = self.state.lock().await;
            let conn = guard.as_ref().ok_or_else(not_connected)?;
            conn.client
                .read_fault_detail(address, dtc, severity_supported)
                .await
                .map_err(|e| {
                    McpError::internal_error(
                        format!("reading fault detail for {}: {e}", req.code),
                        None,
                    )
                })?
        };

        let descriptions = describe_faults(catalog.as_ref(), address, dtc);

        // The info-memory store has its own detail job; run it rather than leave
        // the entry with nothing but a status byte.
        let info_detail = if detail.source == Some(FaultSource::InfoMemory) {
            self.info_memory_detail(address, dtc, effective_variant.as_deref())
                .await
        } else {
            Vec::new()
        };

        let mut notes = Vec::new();
        let snapshot = decode_snapshot_dtos(
            detail.snapshot.as_ref(),
            defs.as_ref(),
            catalog.as_ref(),
            &mut notes,
        );
        let extended = decode_ext_dtos(detail.extended.as_ref(), defs.as_ref(), &mut notes);
        // Say plainly why a non-fault-memory code carries no freeze frame, so the
        // empty result cannot read as "this fault has no stored detail".
        match detail.source {
            Some(FaultSource::FaultMemory) => notes.push(
                "Freeze-frame framing is derived from ISO 14229 + SGBD disassembly and is \
                 pending an on-car 0x19 capture — treat decoded values as provisional."
                    .to_string(),
            ),
            Some(FaultSource::InfoMemory) => notes.push(if info_detail.is_empty() {
                "This code is an INFO-MEMORY (Infospeicher) entry, not a fault-memory \
                 fault, so no freeze frame was read: the 19 09/06/04 services address the \
                 fault memory only and would be rejected. Its own detail job \
                 (IS_LESEN_DETAIL, what ISTA runs for this store) could not run here — no \
                 SGBD for this ECU, no such job in it, or the ECU did not answer — so only \
                 the code and status from read_faults are available."
                    .to_string()
            } else {
                "This code is an INFO-MEMORY (Infospeicher) entry. Its detail is in \
                 `info_detail`, read with the ECU's own IS_LESEN_DETAIL job (what ISTA \
                 runs for this store); `snapshot`/`extended`/`severity` stay empty because \
                 those are the 19 xx services, which address the fault memory only."
                    .to_string()
            }),
            None => notes.push(
                "The ECU reports this code in NEITHER its fault memory (19 02 0C) nor \
                 its info memory (22 2000), so no detail read was sent. Re-read the ECU \
                 with read_faults — the code may have been cleared, or belong to a \
                 different ECU."
                    .to_string(),
            ),
        }

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
            source: detail.source.map(fault_source_tag).map(str::to_string),
            info_detail,
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
        // Best-effort ISTA-tree enrichment from the current I-Stufe (identify has
        // no factory level in hand; scan_ecus reads the factory record and is the
        // precise source for the dated-variant split).
        let tree = ecu_tree_for_level(catalog.as_ref(), identity.i_stufe.as_deref());
        let ecus = named
            .into_iter()
            .map(|n| {
                let entry = tree.as_ref().and_then(|(_, map)| map.get(&n.address));
                ConfiguredEcuInfo {
                    address_hex: format!("0x{:02X}", n.address),
                    group_name: n.name,
                    title: n.title,
                    responding: None,
                    ista_name: entry.and_then(|e| e.name.clone()),
                    bus: entry.and_then(|e| e.bus_label.clone().or_else(|| e.bus.clone())),
                    minimal: entry.map(|e| e.minimal),
                }
            })
            .collect();

        let fa = klartext_semantic::decode_vehicle_order(&identity.vehicle_order_raw);
        let vehicle_order = VehicleOrderDto {
            standard_fa: fa.standard_fa(),
            version: fa.version,
            baureihe: fa.baureihe,
            typ_schluessel: fa.typ_schluessel,
            lackcode: fa.lackcode,
            polstercode: fa.polstercode,
            build_date: fa.build_date,
            options: fa.options,
            e_worte: fa.e_worte,
            ho_worte: fa.ho_worte,
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

    /// Clear one ECU's stored fault codes — one of the server's two standard
    /// writes; gated on `confirm`.
    ///
    /// The refined M9 safety invariant: a standard, well-defined, non-physical,
    /// reversible diagnostic operation (UDS 0x14, the M2 clear path) may run behind
    /// explicit confirmation. Nothing here actuates a component, and NO ECU reset
    /// follows — ISTA's own clear (`VehicleIdent.ClearErrorInfoMemoryVehicle`) sends
    /// no UDS 0x11 either (parity audit P0.1).
    ///
    /// # Errors
    /// Returns a tool error when `confirm` is false (the refusal explains what a
    /// clear discards), when not connected or the ECU is unknown, or when the
    /// pre-read or the clear itself fails.
    #[tool(description = "Clear stored fault codes (DTCs) on one ECU — UDS \
        ClearDiagnosticInformation (0x14, all DTC groups), a standard, well-defined \
        diagnostic operation. REQUIRES confirm=true; without it the call refuses and \
        explains. Clearing also discards the faults' freeze-frame/snapshot data and \
        can reset OBD readiness monitors, so call read_faults first, tell the human \
        what is stored, and pass confirm=true only on their explicit go-ahead. \
        Reversible only in that a still-active fault sets its code again on a later \
        drive cycle. The ECU is NOT reset afterward — nothing reboots. `ecu` as in \
        read_faults. The result echoes the codes that were stored before the clear. \
        This server still cannot actuate components or code.")]
    pub async fn clear_faults(
        &self,
        Parameters(req): Parameters<ClearFaultsRequest>,
    ) -> Result<Json<ClearFaultsResult>, McpError> {
        // Blast-radius rule: refuse the state change before touching anything —
        // even the connection check — unless explicitly confirmed.
        if !req.confirm {
            return Err(McpError::invalid_params(
                format!(
                    "refusing to clear fault codes on '{}': clearing erases stored DTCs \
                     together with their freeze-frame/snapshot data and can reset OBD \
                     readiness monitors. Call read_faults first, confirm intent with \
                     the human, then re-call with confirm=true.",
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
            note: "Cleared. The ECU was not reset — ISTA does not reset after clearing \
                   either. Freeze-frame/snapshot data is discarded and readiness monitors \
                   may reset; a still-active fault will set its code again on a later \
                   drive cycle. Re-run read_faults to verify."
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
        // DB-unique → the group ident job on the car). The VIN, if connected, keys
        // the profile.
        let conn_vin = self.state.lock().await.as_ref().and_then(|c| c.vin.clone());
        let plan = self.variant_or_ident_groups(
            address,
            req.variant.as_deref(),
            catalog.as_ref(),
            conn_vin.as_deref(),
        );
        let effective_variant = self
            .resolve_variant_live(address, plan, conn_vin.as_deref())
            .await;
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
    /// live session; an invalid-request error naming the refused service ID when the
    /// job emits a write (the read-only gate refused it before the car was touched);
    /// and an internal error for any other run fault.
    #[tool(
        description = "Run a read-only EDIABAS job (e.g. STATUS_LESEN) and return \
        its named result sets. Executes the ECU's own bytecode over a read-only gate \
        that refuses any write/actuation service at the transmit boundary — a \
        write-emitting job is rejected before any frame reaches the car; this server \
        has no path to execute it. Requires a prior \
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
        let plan = self.variant_or_ident_groups(
            address,
            req.variant.as_deref(),
            catalog.as_ref(),
            conn_vin.as_deref(),
        );
        let variant = self
            .resolve_variant_live(address, plan, conn_vin.as_deref())
            .await
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

    /// Run an ECU service function — a reset, adaptation, ACTUATION, or calibration.
    ///
    /// The one WRITE path to a moving component. It runs ISTA's own function-keyed,
    /// rank-ordered phase cycle (`Preset → Main → hold → Reset`) for `function_id`
    /// through the ECU's BEST/2 bytecode, every job wrapped in a
    /// [`GatedExchange::confirmed_write`] — the transmit seam that admits a
    /// write/actuation service (`0x2E`/`0x2F`/`0x31`/`0x14`/`0x27`) only for this
    /// confirmed call, while still refusing flashing. The `ecu`/`variant` resolve as
    /// in [`Self::run_job`]; the function's phase jobs and hold come from ISTA's
    /// catalog.
    ///
    /// Refuses unless `confirm` is relayed from the human, checked before the car is
    /// touched — the primary safety gate, since klartext (like ISTA) machine-checks
    /// no precondition and surfaces the function's operator text as advice instead. A
    /// held (`Activation == 0`) actuation stays energised after this returns
    /// (`held = true`) until [`Self::stop_service`] or [`Self::disconnect`] runs its
    /// teardown; a failed cycle still tears down at once (owner ruling 1).
    ///
    /// # Errors
    /// Returns an invalid-params error when `confirm` is false, the ECU or variant
    /// cannot be resolved, the SGBD cannot be loaded, the semantic DB is absent, or
    /// the function is unknown for the variant; a not-connected error with no live
    /// session; and an internal error when the catalog read faults.
    #[tool(description = "Run an ECU service function — a maintenance reset, \
        adaptation, ACTUATION, or calibration — as ISTA's own phase cycle (Preset, \
        Main, hold, Reset) through the ECU's bytecode. THIS WRITES TO THE CAR AND CAN \
        MOVE A PHYSICAL COMPONENT, alter calibration, or drop terminal 15 (electrical \
        power) — it is not a read. REQUIRES confirm=true; without it the call refuses \
        and explains. Choose the component with `function_id` (an ISTA fixed-function \
        id); one EDIABAS job drives many components, so the id is required and a wrong \
        one actuates the wrong part. Before confirming, show the human the returned \
        operator text (preparing/processing/post) — ISTA's own instructions and \
        preconditions — and get their explicit go-ahead; this server does NOT \
        machine-check preconditions (neither does ISTA). A HELD actuation (a function \
        with no timed duration) stays ENERGISED after this returns (held=true) until \
        you call stop_service (same ecu + function_id) or disconnect. A failed cycle \
        still runs the safe teardown. Requires a prior connect; `ecu` and `variant` as \
        in run_job.")]
    pub async fn run_service_function(
        &self,
        Parameters(req): Parameters<RunServiceFunctionRequest>,
    ) -> Result<Json<RunServiceFunctionResult>, McpError> {
        // Refuse the actuation before touching anything — even the connection check —
        // unless explicitly confirmed. With the invented precondition gate removed
        // (owner ruling 3), this human confirmation is the primary safety mechanism.
        if !req.confirm {
            return Err(McpError::invalid_params(
                format!(
                    "refusing to run service function {} on '{}': this ACTUATES a component \
                     (it can move a part, alter calibration, or drop terminal 15). Call \
                     list_service_functions, show the human the function's operator text, then \
                     re-call with confirm=true only on their explicit go-ahead.",
                    req.function_id, req.ecu
                ),
                None,
            ));
        }
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        // Resolve the variant via the M10 ladder, exactly as run_job: a service write
        // NEEDS its SGBD bytecode, so an unresolved variant is a hard error.
        let conn_vin = self.state.lock().await.as_ref().and_then(|c| c.vin.clone());
        let plan = self.variant_or_ident_groups(
            address,
            req.variant.as_deref(),
            catalog.as_ref(),
            conn_vin.as_deref(),
        );
        let variant = self
            .resolve_variant_live(address, plan, conn_vin.as_deref())
            .await
            .ok_or_else(|| self.variant_candidates_error(address, catalog.as_ref()))?;
        let path = self.sgbd_path(&variant).ok_or_else(|| no_sgbd(&variant))?;
        let ecu = Ecu::open(&path).map_err(|e| {
            McpError::invalid_params(format!("cannot load SGBD variant '{variant}': {e}"), None)
        })?;

        // The function's phase jobs come from ISTA's catalog — with no DB there is
        // nothing to run. Load every phase/job/rank invocation, then the hold params.
        let catalog = catalog.ok_or_else(|| {
            McpError::invalid_params(
                "no semantic DB — a service function's phase jobs come from ISTA's catalog \
                 (build scripts/build-semantic-db.sh)",
                None,
            )
        })?;
        let invs = function_invocations(&catalog, &variant, req.function_id)?;
        // ISTA's post-Main hold: a reset-bearing Activation>0 function holds for its
        // duration; Activation==0 holds until stopped; a function with no Reset never
        // holds (research Q2, via `hold_for`).
        let has_reset = invs.iter().any(|i| i.phase == Phase::Reset);
        let fixed = catalog.fixed_function(req.function_id).ok().flatten();
        let hold = hold_for(fixed.as_ref(), has_reset);
        let (preparing_text, processing_text, post_text) = fixed
            .map(|f| (f.preparing_text, f.processing_text, f.post_text))
            .unwrap_or((None, None, None));

        // Run under the session lock, exactly as run_job: the bridge borrows the
        // client across the cycle's awaits, one car serialises its traffic, and the
        // confirmed-write gate is the OUTERMOST layer of every job the cycle sends.
        let report = {
            let mut guard = self.state.lock().await;
            let conn = guard.as_mut().ok_or_else(not_connected)?;
            // A DIFFERENT function already held would be ORPHANED: the single held
            // slot tracks one actuation, so starting a second held function
            // overwrites the first — leaving its component energised with no
            // teardown path (disconnect/stop_service would only ever reach the
            // second, and the session's keepalive prevents the ECU's own S3
            // revert). ISTA holds one component at a time; refuse and point the
            // caller at the outstanding one. Checked under the SAME lock as the run
            // and the slot write below, so two concurrent calls cannot both pass.
            if let Some(other) = conflicting_held(conn.held(), req.function_id, address) {
                return Err(McpError::invalid_params(
                    format!(
                        "refusing to run service function {} on '{}': function {} is still \
                         HELD (actuating) on this session. klartext tracks ONE held actuation \
                         at a time — starting a second would leave function {}'s component \
                         energised with no way to stop it. Call stop_service for function {} \
                         first (or disconnect to tear it down), then retry.",
                        req.function_id, req.ecu, other, other, other
                    ),
                    None,
                ));
            }
            let report = {
                let bridge = ConfirmedWriteBridge {
                    ecu: &ecu,
                    client: &conn.client,
                };
                klartext_service::run_service(&bridge, req.function_id, address, &invs, hold).await
            };
            // Track the one outstanding held actuation so disconnect (or stop_service)
            // can return it to safe; clear a same-function hold that did not re-hold.
            if report.held {
                conn.set_held(Some(HeldService {
                    function_id: req.function_id,
                    address,
                    variant: variant.clone(),
                }));
            } else if should_clear_held(
                conn.held(),
                req.function_id,
                address,
                matches!(report.teardown, Teardown::Failed(_)),
            ) {
                // A same-function re-run that did not re-hold AND left the component
                // safe: the hold is over, clear it. A FAILED teardown keeps the slot
                // so disconnect still retries (review finding B).
                conn.set_held(None);
            }
            report
        };

        let (phases, teardown, teardown_error) = service_report_dto(&report);
        Ok(Json(RunServiceFunctionResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            variant,
            function_id: req.function_id,
            note: service_run_note(&report),
            title: report.title,
            phases,
            succeeded: report.succeeded,
            teardown,
            teardown_error,
            held: report.held,
            preparing_text,
            processing_text,
            post_text,
        }))
    }

    /// Stop a HELD service function by running its return-to-safe teardown.
    ///
    /// The stop half of a held actuation (owner ruling 2): [`Self::run_service_function`]
    /// with an `Activation == 0` function leaves the component energised
    /// (`held = true`), and this runs ONLY that function's `Reset` phase — never
    /// `Main` — so it is returned to safe without re-actuating. The same teardown
    /// runs automatically on [`Self::disconnect`]. Refuses unless `confirm` is relayed.
    ///
    /// # Errors
    /// Returns an invalid-params error when `confirm` is false, the ECU or variant
    /// cannot be resolved, the SGBD cannot be loaded, the semantic DB is absent, or
    /// the function is unknown; a not-connected error with no live session; and an
    /// internal error when the catalog read faults.
    #[tool(
        description = "Stop a HELD service function (one run_service_function left \
        actuating, held=true) by running ONLY its return-to-safe teardown — Main never \
        re-runs, so the component is de-energised without re-actuating. REQUIRES \
        confirm=true. Pass the same `ecu` and `function_id` you started. A held \
        actuation is ALSO torn down automatically on disconnect, but call this \
        explicitly when you are done with it rather than leaving a component forced. \
        Requires a prior connect."
    )]
    pub async fn stop_service(
        &self,
        Parameters(req): Parameters<StopServiceRequest>,
    ) -> Result<Json<StopServiceResult>, McpError> {
        // Confirm before touching the car, as run_service_function does.
        if !req.confirm {
            return Err(McpError::invalid_params(
                format!(
                    "refusing to stop service function {} on '{}' without confirm=true: the \
                     teardown returns the component to its safe (de-energised) state; re-call \
                     with confirm=true.",
                    req.function_id, req.ecu
                ),
                None,
            ));
        }
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let conn_vin = self.state.lock().await.as_ref().and_then(|c| c.vin.clone());
        let plan = self.variant_or_ident_groups(
            address,
            req.variant.as_deref(),
            catalog.as_ref(),
            conn_vin.as_deref(),
        );
        let variant = self
            .resolve_variant_live(address, plan, conn_vin.as_deref())
            .await
            .ok_or_else(|| self.variant_candidates_error(address, catalog.as_ref()))?;
        let path = self.sgbd_path(&variant).ok_or_else(|| no_sgbd(&variant))?;
        let ecu = Ecu::open(&path).map_err(|e| {
            McpError::invalid_params(format!("cannot load SGBD variant '{variant}': {e}"), None)
        })?;
        let catalog = catalog.ok_or_else(|| {
            McpError::invalid_params(
                "no semantic DB — a service function's teardown jobs come from ISTA's catalog",
                None,
            )
        })?;
        let invs = function_invocations(&catalog, &variant, req.function_id)?;

        let report = {
            let mut guard = self.state.lock().await;
            let conn = guard.as_mut().ok_or_else(not_connected)?;
            let report = {
                let bridge = ConfirmedWriteBridge {
                    ecu: &ecu,
                    client: &conn.client,
                };
                klartext_service::stop_service(&bridge, req.function_id, address, &invs).await
            };
            // A stop that left the component safe clears its tracked hold; a failed
            // teardown keeps the slot so disconnect retries. Same pure rule as the
            // run path, keyed on (function, address) so a stop on the wrong ECU
            // never clears a hold on another (review finding: address key).
            if should_clear_held(
                conn.held(),
                req.function_id,
                address,
                matches!(report.teardown, Teardown::Failed(_)),
            ) {
                conn.set_held(None);
            }
            report
        };

        let (phases, teardown, teardown_error) = service_report_dto(&report);
        let note = match &report.teardown {
            Teardown::Failed(_) => "The teardown FAILED — the component may still be actuating; \
                 retry stop_service, or power-cycle the ECU."
                .to_string(),
            Teardown::NotDefined => "This function defines no teardown (no Reset phase) — nothing \
                 to stop."
                .to_string(),
            _ => "Stopped — the component was returned to safe.".to_string(),
        };
        Ok(Json(StopServiceResult {
            ecu: req.ecu,
            address: format!("0x{address:02X}"),
            variant,
            function_id: req.function_id,
            title: report.title,
            phases,
            succeeded: report.succeeded,
            teardown,
            teardown_error,
            note,
        }))
    }

    /// Close the diagnostic session and release the car connection.
    ///
    /// Any service function still HELD by `run_service_function` is torn down first
    /// (owner ruling 2), so disconnecting never strands an actuated component.
    ///
    /// # Errors
    /// Infallible today; returns `Result` to match the tool signature shape.
    #[tool(description = "Close the diagnostic session and release the car \
        connection. If a service function is still HELD (run_service_function returned \
        held=true), its return-to-safe teardown runs first, so disconnecting never \
        leaves a component actuated. Safe to call when not connected.")]
    pub async fn disconnect(&self) -> Result<Json<DisconnectResult>, McpError> {
        let was_connected = self.take_session_and_stop_held().await;
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
        \"frame-not-derivable\" (discovery-only; no offline frame). This tool is \
        READ-ONLY discovery and never runs anything. To EXECUTE a function, use \
        run_service_function (behind confirm=true), which runs it by ISTA catalog \
        function_id. `confirmed_write_eligible` marks the LOW-risk, derived functions \
        — the safest class to run; HIGH-risk physical actuation/calibration should only \
        be run with the human present.")]
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
                   capture]). Execution is via run_service_function (confirm=true), keyed \
                   by the ISTA catalog function_id; confirmed_write_eligible flags the \
                   low-risk, derived functions."
                .to_string(),
        }))
    }

    /// List the ISTA catalog functions run_service_function can run — read-only discovery.
    ///
    /// The DISCOVERY companion to [`Self::run_service_function`]: that tool takes an
    /// integer catalog `function_id`, and this is the only place an agent finds it.
    /// Deliberately distinct from [`Self::list_service_functions`], whose string
    /// `label` drives the SGBD-derived path — the two are different identifier
    /// spaces, so mixing them would let an agent pass a label where an id is
    /// required. A pure semantic-DB read: no car connection, no execution.
    ///
    /// # Errors
    /// Returns a tool error when neither `variant` nor a resolvable `ecu` is given,
    /// when the semantic DB is absent (the catalog is the sole source of these ids),
    /// or when the catalog read faults.
    #[tool(
        description = "List the ISTA service functions run_service_function can run on one \
        ECU, each with the `function_id` to pass it — READ-ONLY discovery, no car \
        connection and NO execution. THIS is how you find the integer `function_id` \
        run_service_function requires; it is a DIFFERENT identifier from \
        list_service_functions' string `label` (that lists the SGBD-derived catalog — \
        this lists the runnable catalog functions and their ids). Give `variant` (the \
        ECU SGBD, e.g. \"d72n47a0\") or an `ecu` (hex address / ISTA group / variant \
        name) to resolve one; the server needs a --semantic-db. Each entry gives: \
        `function_id` (pass THIS to run_service_function), `title`, `has_reset` and \
        `hold` (whether a run HOLDS the component — \"until_stop\" needs a later \
        stop_service; \"timed\" tears down within the call), and `preparing_text`, \
        ISTA's own operator instruction you MUST show the human before confirming a run."
    )]
    pub async fn list_service_function_ids(
        &self,
        Parameters(req): Parameters<ListServiceFunctionIdsRequest>,
    ) -> Result<Json<ListServiceFunctionIdsResult>, McpError> {
        let variant = self
            .resolve_list_variant(req.variant.as_deref(), req.ecu.as_deref())
            .await?;
        // The runnable catalog (and its function_ids) exists ONLY in the semantic DB;
        // unlike list_service_functions there is no SGBD fallback for these ids.
        let catalog = self.catalog().ok_or_else(|| {
            McpError::invalid_params(
                "no semantic DB — the runnable service-function catalog and its function_ids \
                 come from ISTA's catalog (build scripts/build-semantic-db.sh)",
                None,
            )
        })?;
        let entries = catalog
            .service_functions_for_variant(&variant)
            .map_err(|e| {
                McpError::internal_error(
                    format!("reading service functions for '{variant}': {e}"),
                    None,
                )
            })?;
        let functions: Vec<ServiceFunctionCatalogInfo> =
            entries.into_iter().map(service_catalog_info).collect();
        Ok(Json(ListServiceFunctionIdsResult {
            variant,
            count: functions.len(),
            functions,
            note: "Pass a listed `function_id` to run_service_function (confirm=true) to run \
                   it; show the human its `preparing_text` first. A function whose `hold` is \
                   \"until_stop\" leaves the component energised until stop_service."
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
        // Best-effort ISTA-tree enrichment: resolve the platform's bordnet from
        // the factory I-Stufe and annotate each address with ISTA's short name,
        // bus, and core (minimal-configuration) flag. The level read happens
        // before the catalog is consulted so no DB handle is held across an await.
        let level = if catalog.is_some() {
            conn.client
                .read_i_stufe_levels()
                .await
                .ok()
                .flatten()
                .map(|l| l.factory.unwrap_or(l.current))
        } else {
            None
        };
        let tree = ecu_tree_for_level(catalog.as_ref(), level.as_deref());
        let ecus = addrs
            .iter()
            .map(|&address| {
                let (group_name, title) = ecu_names(address, catalog.as_ref());
                let entry = tree.as_ref().and_then(|(_, map)| map.get(&address));
                ConfiguredEcuInfo {
                    address_hex: format!("0x{address:02X}"),
                    group_name,
                    title,
                    responding: responding.as_ref().map(|r| r.contains(&address)),
                    ista_name: entry.and_then(|e| e.name.clone()),
                    bus: entry.and_then(|e| e.bus_label.clone().or_else(|| e.bus.clone())),
                    minimal: entry.map(|e| e.minimal),
                }
            })
            .collect();
        let subset = match responding_count {
            Some(n) => format!(" {n} of them are actively responding (22 3F08)."),
            None => " The gateway did not report a responding subset (22 3F08).".to_string(),
        };
        let ista_view = match &tree {
            Some((series, map)) => {
                let core = addrs
                    .iter()
                    .filter(|a| map.get(a).is_some_and(|t| t.minimal))
                    .count();
                format!(
                    " ISTA bordnet {series}: per-ECU ista_name/bus attached; {core} of the \
                     configured ECUs are minimal-configuration (the always-shown core of \
                     ISTA's ~11-box view)."
                )
            }
            None => String::new(),
        };
        let note = if cached {
            format!(
                "Cached CONFIGURED ECU list (VCM 22 3F07) from earlier this session — pass \
                 rescan=true to re-read. This is the stored superset, not ISTA's post-filtered \
                 view.{subset}{ista_view}"
            )
        } else {
            format!(
                "Read {} CONFIGURED ECU(s) from the gateway (VCM 22 3F07 — the stored superset, \
                 NOT ISTA's per-model-filtered ~11).{subset}{ista_view}",
                addrs.len()
            )
        };
        Ok(Json(ScanEcusResult {
            ecus,
            configured_count: addrs.len(),
            responding_count,
            bordnet_series: tree.map(|(series, _)| series),
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
        ECU in full with read_faults."
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
            conn.client.scan_faults(&addrs).await
        };

        // Enrich the info half the way read_faults does — the ECU's own IS_LESEN
        // sends the right frame for it and carries BMW's authored text. Done for
        // every ECU BEFORE any catalog is touched, because a `&Catalog` is not
        // `Send` and these are awaits. Offline variant rungs only: identifying 32
        // ECUs to enrich a sweep would spend 32 round trips on cosmetics.
        let conn_vin = self.state.lock().await.as_ref().and_then(|c| c.vin.clone());
        let mut info_jobs: HashMap<u8, Option<Vec<InfoEntry>>> = HashMap::new();
        for ef in &scanned {
            let variant = self
                .variant_or_ident_groups(ef.address, None, catalog.as_ref(), conn_vin.as_deref())
                .ok();
            if variant.is_none() {
                continue; // no SGBD for this ECU: keep the direct read untouched
            }
            let job = self
                .info_memory_via_job(ef.address, variant.as_deref())
                .await;
            info_jobs.insert(ef.address, job);
        }

        let mut total_faults = 0usize;
        let mut total_present = 0usize;
        let ecus: Vec<EcuFaultsInfo> = scanned
            .into_iter()
            .map(|ef| {
                total_faults += ef.faults.len();
                total_present += ef
                    .faults
                    .iter()
                    .filter(|d| d.presence() == Presence::Present)
                    .count();
                let (_group, title) = ecu_names(ef.address, catalog.as_ref());
                let (info_entries, info_supported) = Self::info_entries_from(
                    info_jobs.remove(&ef.address).flatten(),
                    ef.address,
                    &ef.info,
                    ef.info_supported,
                    catalog.as_ref(),
                );
                EcuFaultsInfo {
                    address_hex: format!("0x{:02X}", ef.address),
                    title,
                    faults: ef
                        .faults
                        .iter()
                        .map(|d| {
                            fault_info(d, ef.address, catalog.as_ref(), FaultSource::FaultMemory)
                        })
                        .collect(),
                    info_entries,
                    info_supported,
                    error: ef.error,
                }
            })
            .collect();

        let mut note = String::from(
            "Whole-car scan reading BOTH stores per ECU: `faults` is the 19 02 fault \
             memory (request 19 02 0C — the ECU returns only pending/confirmed and this \
             server applies no further filter), `info_entries` the 22 2000 info memory \
             ISTA shows alongside faults (each entry's `source` marks which). Check each \
             fault's `presence` for whether it is failing right now; `unknown` means the \
             ECU's test has not run this operation cycle, so it is not evidence of health.",
        );
        if let Some(caveat) = detail_stub_note(req.detail) {
            note.push(' ');
            note.push_str(caveat);
        }

        Ok(Json(ReadAllFaultsResult {
            ecus,
            total_faults,
            total_present,
            db_available: catalog.is_some(),
            note,
        }))
    }

    /// Run ISTA's whole-vehicle clear sequence — the whole-car write; confirm-gated.
    ///
    /// # Errors
    /// Returns a tool error when `confirm` is false, or when not connected.
    #[tool(
        description = "Run ISTA's whole-vehicle clear sequence: ONE functional \
        (broadcast) UDS 0x14 clear, a physical clear for any faulted ECU that stayed \
        silent, the gateway's combined fault store, then a terminal-15 cycle, a \
        re-identification and a verification read. REQUIRES confirm=true. \
        \
        TAKES OVER 15 SECONDS AND DROPS THE CAR'S IGNITION. ISTA ends every clear by \
        switching terminal 15 OFF for 15 seconds and back ON — that is what resets \
        the instrument cluster, and klartext does the same. Tell the human BEFORE \
        calling: the car's ignition will drop, the dash will go dark and come back, \
        and the call will not return for at least 15 seconds. Do not call it on a \
        moving vehicle. If the result reports terminal_15_restored=false, say so \
        immediately and prominently — terminal 15 may still be down and the car may \
        not start until the ignition is cycled by hand. \
        \
        It discards EVERY ECU's freeze-frame/snapshot data and can reset OBD \
        readiness monitors car-wide, so run read_all_faults first, tell the human \
        exactly what is stored across the car, and pass confirm=true only on their \
        explicit go-ahead. Every ECU is pre-read (codes recorded) before anything is \
        erased, and an ECU whose fault memory cannot be read is never cleared. No ECU \
        is reset — ISTA sends no UDS 0x11 here. Cannot actuate other components, run \
        service functions, or code."
    )]
    pub async fn clear_all_faults(
        &self,
        Parameters(req): Parameters<ClearAllFaultsRequest>,
    ) -> Result<Json<ClearAllFaultsResult>, McpError> {
        // Blast-radius rule: refuse the state change before touching anything.
        if !req.confirm {
            return Err(McpError::invalid_params(
                "refusing to clear faults across the whole car: this erases EVERY fitted ECU's \
                 stored DTCs together with their freeze-frame data and can reset OBD readiness \
                 monitors car-wide, and it ends by dropping the car's terminal 15 for 15 seconds \
                 (the ignition goes off and back on, which is how the instrument cluster resets). \
                 Run read_all_faults, tell the human what is stored AND that the ignition will \
                 drop, then re-call with confirm=true."
                    .to_string(),
                None,
            ));
        }
        let catalog = self.catalog();
        // The supplier info-memory jobs ISTA runs alongside the fault clear are
        // addressed by SGBD NAME. Resolve those names to fitted ECUs BEFORE the
        // sequence starts — the runner has to be `Sync` and `Catalog` is not, and
        // a group name may need its own ident job, which is an await.
        let (addrs, vehicle) = {
            let mut guard = self.state.lock().await;
            let conn = guard.as_mut().ok_or_else(not_connected)?;
            let (addrs, _) = fitted_addrs(conn, req.rescan).await?;
            let vehicle = self.vehicle_composition(&addrs, catalog.as_ref(), conn.vin.as_deref());
            (addrs, vehicle)
        };
        let planned = supplier_clear_jobs(&vehicle);
        let plan = self.supplier_target_plan(&planned, catalog.as_ref());
        let targets = self.resolve_supplier_targets(plan).await;

        let report = {
            let guard = self.state.lock().await;
            let conn = guard.as_ref().ok_or_else(not_connected)?;
            let bridge = SupplierBridge {
                client: &conn.client,
                targets,
            };
            conn.client
                .clear_faults_all(&addrs, &vehicle, Some(&bridge))
                .await
        };

        let mut cleared_clean = 0usize;
        let ecus: Vec<EcuClearInfo> = report
            .ecu_verdicts()
            .into_iter()
            .map(|v| {
                if v.verified_clean {
                    cleared_clean += 1;
                }
                let (_group, title) = ecu_names(v.address, catalog.as_ref());
                EcuClearInfo {
                    address_hex: format!("0x{:02X}", v.address),
                    title,
                    codes_before: v.before.iter().map(dtc_code_hex).collect(),
                    answered_broadcast: v.answered_broadcast,
                    cleared_physically: v.cleared_physically,
                    codes_after: v.after.iter().map(dtc_code_hex).collect(),
                    verified_clean: v.verified_clean,
                    error: v.error,
                }
            })
            .collect();

        let (broadcast_answered, broadcast_error) = match &report.functional {
            Ok(addrs) => (addrs.iter().map(|a| format!("0x{a:02X}")).collect(), None),
            Err(e) => (Vec::new(), Some(e.clone())),
        };
        let (reidentified, reident_error) = match &report.reident {
            Ok(addrs) => (addrs.iter().map(|a| format!("0x{a:02X}")).collect(), None),
            Err(e) => (Vec::new(), Some(e.clone())),
        };
        let clamp_cycle = match &report.clamp_cycle {
            Ok(()) => ClampCycleInfo {
                cycled: true,
                terminal_15_restored: true,
                error: None,
            },
            Err(failure) => ClampCycleInfo {
                cycled: false,
                terminal_15_restored: failure.restored,
                error: Some(failure.message.clone()),
            },
        };
        let supplier_jobs: Vec<SupplierJobInfo> = report
            .supplier_jobs
            .iter()
            .map(|r| SupplierJobInfo {
                ecu: r.job.ecu.to_string(),
                job: r.job.job.to_string(),
                arg: r.job.arg.to_string(),
                not_run: r.not_run.clone(),
            })
            .collect();

        let note = clear_all_note(&report, &clamp_cycle, cleared_clean, ecus.len());

        Ok(Json(ClearAllFaultsResult {
            ecus,
            cleared_clean,
            broadcast_answered,
            broadcast_error,
            supplier_jobs,
            gateway_store_cleared: report.gateway_zfs.is_ok(),
            gateway_store_error: report.gateway_zfs.as_ref().err().cloned(),
            clamp_cycle,
            reidentified,
            reident_error,
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
                "BMW F-series diagnostics: reads, plus two confirmation-gated write \
                 tools. clear_faults clears ONE ECU and sends the standard UDS 0x14 \
                 clear and nothing else. clear_all_faults runs ISTA's whole-vehicle \
                 sequence: a broadcast 0x14, physical clears for stragglers, the \
                 gateway's combined store, and then a TERMINAL-15 CYCLE — it switches \
                 the car's ignition off for 15 seconds and back on, so that call \
                 blocks for over 15 seconds and the dash goes dark and returns. \
                 Warn the human before calling it, and never call it on a moving \
                 vehicle. No ECU is reset by either (ISTA sends no UDS 0x11 here). \
                 Call connect first (discovers the gateway or uses a \
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
                 readiness monitors, so read first and get the human's go-ahead. Apart \
                 from clear_all_faults' terminal-15 cycle this server cannot actuate \
                 components, run service functions, code, or send any \
                 derived-unconfirmed write frame — none of that is executable yet. \
                 It disconnects the car session automatically on exit. Fault \
                 text and the ECU map come from the ISTA SQLiteDB; reads still work (raw) \
                 without it."
                    .to_string(),
            )
    }
}

/// Build the human note for a finished clear sequence.
///
/// Leads with whatever needs acting on. A terminal 15 that may still be DOWN is the
/// only outcome here that can leave the car unable to start, so it goes first and in
/// plain words; everything else is reported after.
fn clear_all_note(
    report: &ClearSequenceReport,
    clamp: &ClampCycleInfo,
    cleared_clean: usize,
    total: usize,
) -> String {
    let mut parts = Vec::new();
    if !clamp.terminal_15_restored {
        parts.push(
            "URGENT: terminal 15 may still be DOWN — the car may not start. Tell the human to \
             cycle the ignition (press start/stop) now."
                .to_string(),
        );
    }
    parts.push(format!(
        "Whole-car clear done: {cleared_clean} of {total} ECUs verified clean."
    ));
    if let Ok(answered) = &report.functional {
        parts.push(format!(
            "One functional (broadcast) clear reached {} ECU(s); {} needed a physical clear.",
            answered.len(),
            report.stragglers.len()
        ));
    } else {
        parts.push(format!(
            "The functional (broadcast) clear FAILED, so all {} faulted ECUs were cleared \
             physically instead.",
            report.stragglers.len()
        ));
    }
    if clamp.cycled {
        parts.push(
            "Terminal 15 was cycled off and back on (ISTA does this after every clear — it is \
             what resets the instrument cluster)."
                .to_string(),
        );
    }
    let not_run = report
        .supplier_jobs
        .iter()
        .filter(|j| j.not_run.is_some())
        .count();
    if not_run > 0 {
        parts.push(format!(
            "{not_run} supplier-specific info-memory store(s) that ISTA would also clear were NOT \
             cleared — klartext cannot run those EDIABAS jobs yet; see supplier_jobs. They are \
             separate from the fault memories above, which were cleared."
        ));
    }
    parts.push(
        "Every ECU's freeze-frames are discarded and readiness monitors may reset; a still-active \
         fault sets its code again on a later drive. No ECU was reset — ISTA sends no UDS 0x11 \
         here either. Re-run read_all_faults to verify."
            .to_string(),
    );
    parts.join(" ")
}

/// The wire tag for a VIN check outcome: `match`, `mismatch`, or `unreadable`.
fn vin_check_tag(check: &VinCheck) -> &'static str {
    match check {
        VinCheck::Match => "match",
        VinCheck::Mismatch { .. } => "mismatch",
        VinCheck::Unreadable => "unreadable",
    }
}

/// The `connect` note, which on a re-connect leads with the identity outcome.
///
/// `check` is `None` on a first connect (no previous session VIN to compare).
///
/// **The `Mismatch` arm is unreachable by construction** since 2026-07-19: a
/// mismatch now aborts `connect` with an error before any [`ConnectResult`] is
/// built, matching ISTA's hard abort (`VciConnLossVM.cs:40-53`). The arm is kept
/// because the match must stay exhaustive, and its text is kept accurate so that
/// re-widening the abort later does not silently ship a stale message.
fn connect_note(check: Option<&VinCheck>) -> String {
    const HELD: &str = "Session held; one connection reaches every ECU by name/address. \
         Reads (read_faults/read_data/scan_ecus/read_all_faults) run freely; \
         clear_faults / clear_all_faults need confirm=true. Call disconnect \
         when done (the server also disconnects on exit).";
    match check {
        Some(VinCheck::Mismatch { expected, found }) => format!(
            "DIFFERENT VEHICLE. The previous session was on VIN {expected}; this one is \
             {found}. Every fault, measurement and ECU list you gathered before belongs \
             to the other car — discard it and re-read from scratch. Do NOT clear faults \
             on the strength of what the previous car showed. {HELD}"
        ),
        Some(VinCheck::Unreadable) => format!(
            "Could not confirm this is the same car: no ECU answered the VIN read, so the \
             previous session's VIN could not be checked against it. That is not evidence \
             of a different vehicle — but nothing confirms it is the same one either. \
             Re-read what you need rather than trusting earlier results. {HELD}"
        ),
        Some(VinCheck::Match) => {
            format!("Same vehicle as the previous session (VIN matches). {HELD}")
        }
        None => HELD.to_string(),
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
/// free of a `klartext-client` dependency.
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

/// Runs one EDIABAS job through the BEST/2 VM under the CONFIRMED-WRITE gate.
///
/// The [`klartext_service::JobRunner`] the service-function cycle drives on the car.
/// Each `run` composes the SAME stack `run_job` builds — the VM over a
/// [`TelegramExchange`] over the live-session [`SessionBridge`] — but wraps it in
/// [`GatedExchange::confirmed_write`] instead of `read_only`: the transmit seam that
/// admits the gated write/actuation services (`0x2E`/`0x2F`/`0x31`/`0x14`/`0x27`)
/// for a caller holding the human's `confirm`, while still refusing flashing
/// (`0x34`–`0x37`) even here. Only the policy differs from the read path; the gate
/// itself is unchanged. `klartext-best` never depends on `klartext-client` — the VM
/// and the client meet only here, through the bridge.
struct ConfirmedWriteBridge<'a> {
    /// The loaded ECU whose bytecode runs each job.
    ecu: &'a Ecu,
    /// The live diagnostic client each job's telegrams are forwarded to.
    client: &'a DiagnosticClient,
}

#[async_trait::async_trait]
impl JobRunner for ConfirmedWriteBridge<'_> {
    async fn run(&self, job: &str, target: u8, args: &str) -> Result<(), String> {
        // One gate per job-run, mirroring run_job. The confirmed-write policy is the
        // ONLY difference from the read path, and this seam is the sole way a write
        // reaches the car.
        let gate = GatedExchange::confirmed_write(TelegramExchange::new(SessionBridge {
            client: self.client,
        }));
        self.ecu
            .run_job(job, target, args.as_bytes(), &gate)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Runs the clear sequence's supplier info-memory jobs, resolving each by SGBD name.
///
/// ISTA addresses these by name (`apiJob("FEM_20", "IS_LOESCHEN_TMS", "0x01", …)`)
/// and lets EDIABAS map the name to an ECU. klartext resolves the name the same two
/// ways it can mean — a variant (`<sgbd>.prg`, address from the semantic DB) or a
/// group (`<sgbd>.grp`, whose own `IDENTIFIKATION` says which variant is fitted) —
/// but does it ALL UP FRONT, in [`KlartextServer::resolve_supplier_targets`],
/// because `Catalog` is not `Sync` and this runner must be.
///
/// Each job runs under the CONFIRMED-WRITE gate, which is correct: the human's
/// `confirm` for the clear these belong to has already been taken. A failure is
/// returned as a message and recorded against that job — ISTA ignores these
/// results entirely, so one failing must not stop the sequence.
struct SupplierBridge<'a> {
    /// The live client each job's telegrams are forwarded to.
    client: &'a DiagnosticClient,
    /// Pre-resolved `(sgbd name, address, variant .prg stem)`, owned so no
    /// non-`Sync` catalog handle is held.
    targets: Vec<(String, u8, PathBuf)>,
}

#[async_trait::async_trait]
impl SupplierJobRunner for SupplierBridge<'_> {
    async fn run(&self, sgbd: &str, job: &str, arg: &str) -> Result<(), String> {
        let (_, address, path) = self
            .targets
            .iter()
            .find(|(name, _, _)| name.eq_ignore_ascii_case(sgbd))
            .ok_or_else(|| format!("could not resolve SGBD '{sgbd}' to a fitted ECU"))?;
        let ecu = Ecu::open(path).map_err(|e| format!("cannot load SGBD for '{sgbd}': {e}"))?;
        let gate = GatedExchange::confirmed_write(TelegramExchange::new(SessionBridge {
            client: self.client,
        }));
        ecu.run_job(job, *address, arg.as_bytes(), &gate)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
}

/// Load a service function's ordered invocations from the catalog, or a tool error.
///
/// The shared front half of both service-write tools: reads the function's every
/// phase/job/rank row for `variant` and groups them into ordered invocations. An
/// empty result means the function is unknown for this variant — surfaced as a clear
/// error naming it, rather than a silent no-op that would read as success.
///
/// # Errors
/// Returns an internal error when the catalog read faults, and an invalid-params
/// error when the function has no invocations for `variant`.
/// The `function_id` of a held actuation that starting `(requested, addr)` would
/// orphan.
///
/// `Some(other)` means REFUSE — a held actuation on a DIFFERENT (function, address)
/// occupies the one held slot, and proceeding would overwrite it, leaving its
/// component energised with no teardown. `None` means proceed: nothing is held, or
/// the EXACT same (function, address) is held — re-running that targets the same
/// component and cannot strand a second one. The address is part of the key because
/// a slot for function A on ECU X must not be overwritten by A on ECU Y.
fn conflicting_held(existing: Option<&HeldService>, requested: i64, addr: u8) -> Option<i64> {
    existing
        .filter(|h| h.function_id != requested || h.address != addr)
        .map(|h| h.function_id)
}

/// Whether a completed run/stop for `(fn_id, addr)` should CLEAR the held slot.
///
/// Clears only when the slot IS this exact `(function, address)` AND the component
/// is confirmed safe — `teardown_failed` false. A FAILED teardown keeps the slot so
/// the disconnect backstop still retries the return-to-safe: clearing it would drop
/// the obligation for a component that may still be forced. Keyed on address too, so
/// a stop for function A on ECU Y never clears a hold of A on ECU X.
fn should_clear_held(
    existing: Option<&HeldService>,
    fn_id: i64,
    addr: u8,
    teardown_failed: bool,
) -> bool {
    !teardown_failed && existing.is_some_and(|h| h.function_id == fn_id && h.address == addr)
}

fn function_invocations(
    catalog: &Catalog,
    variant: &str,
    function_id: i64,
) -> Result<Vec<klartext_service::Invocation>, McpError> {
    let rows = catalog
        .job_parameters_for_function(variant, function_id)
        .map_err(|e| {
            McpError::internal_error(format!("loading service function {function_id}: {e}"), None)
        })?;
    let invs = invocations(&rows);
    if invs.is_empty() {
        return Err(McpError::invalid_params(
            format!(
                "service function {function_id} is not defined for variant '{variant}' — call \
                 list_service_functions for this ECU's functions"
            ),
            None,
        ));
    }
    Ok(invs)
}

/// Render a service-function [`ServiceReport`]'s phases and teardown for the surface.
///
/// Returns the ordered per-phase DTOs, the teardown status slug
/// (`not_defined`/`ran`/`deferred`/`failed`), and the teardown failure message when
/// it failed.
fn service_report_dto(report: &ServiceReport) -> (Vec<PhaseOutcomeDto>, String, Option<String>) {
    let phases = report
        .phases
        .iter()
        .map(|o| PhaseOutcomeDto {
            phase: phase_slug(o.phase).to_string(),
            job: o.job.clone(),
            rank: o.rank,
            args: o.args.clone(),
            error: o.error.clone(),
        })
        .collect();
    let (teardown, teardown_error) = match &report.teardown {
        Teardown::NotDefined => ("not_defined".to_string(), None),
        Teardown::Ran => ("ran".to_string(), None),
        Teardown::Deferred => ("deferred".to_string(), None),
        Teardown::Failed(error) => ("failed".to_string(), Some(error.clone())),
    };
    (phases, teardown, teardown_error)
}

/// The wire slug for a service-function [`Phase`].
fn phase_slug(phase: Phase) -> &'static str {
    match phase {
        Phase::Preset => "preset",
        Phase::Main => "main",
        Phase::Reset => "reset",
    }
}

/// The human note for a `run_service_function` result, keyed on its outcome.
fn service_run_note(report: &ServiceReport) -> String {
    if report.held {
        "The component is now ACTUATED and HELD — it stays forced until you call \
         stop_service with the same ecu and function_id, or disconnect (which also runs \
         the teardown). Show the human the operator text."
            .to_string()
    } else if matches!(report.teardown, Teardown::Failed(_)) {
        "The return-to-safe (teardown) step FAILED — the component may still be \
         actuating. Retry stop_service, or power-cycle the ECU."
            .to_string()
    } else if report.succeeded {
        // Distinguish a teardown klartext RAN from one that never existed. Car
        // session 2 §3.3: an ECU-self-reverting function (has_reset:false,
        // hold:none) reported "Ran and returned to safe" while the wire showed one
        // 2F and no return-to-safe frame — true in effect, but it claimed an action
        // klartext did not take.
        match report.teardown {
            Teardown::NotDefined => {
                "Ran. This function defines NO teardown phase, so klartext sent no \
                 return-to-safe frame — the ECU is expected to revert on its own when \
                 its activation time expires."
                    .to_string()
            }
            _ => "Ran, and klartext transmitted the function's return-to-safe (Reset) \
                  phase."
                .to_string(),
        }
    } else {
        "The function did not complete; its safe teardown ran, so the component was \
         returned to safe."
            .to_string()
    }
}

/// Map a [`RunError`] from `run_job` onto the MCP error the caller sees.
///
/// A read-only-gate refusal — a job whose bytecode emitted a write, so the gate
/// blocked it before any frame reached the car — becomes an invalid-request that
/// names the refused service ID: the P2 line is read-only, so no write-emitting job
/// is executable here. Every other fault is an internal error carrying the job's
/// context. (The gate blocks the write regardless of how the job masks the
/// resulting trap; a masked job may instead surface a different, non-`Refused`
/// error — but no write frame is ever transmitted either way.)
fn run_error_to_mcp(job: &str, error: RunError) -> McpError {
    if let RunError::Exchange(ExchangeError::Refused { sid, .. }) = &error {
        return McpError::invalid_request(
            format!(
                "job '{job}' emits UDS service 0x{sid:02X} (a write/actuation); the read-only \
                 gate refused it before any frame reached the car. This server has no path to \
                 execute a write-emitting job."
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

/// Resolve the ISTA ECU tree for an I-Stufe level, keyed by address.
///
/// Maps the level's series through the extract's bordnet list
/// ([`klartext_semantic::bordnet_series_for`]) and loads that platform's tree.
/// `None` when there is no catalog, no level, the extract predates `ecu_tree`
/// (pre-v5), or the series does not resolve — enrichment degrades, the caller's
/// listing never fails on it.
fn ecu_tree_for_level(
    catalog: Option<&Catalog>,
    level: Option<&str>,
) -> Option<(String, HashMap<u8, klartext_semantic::EcuTreeEntry>)> {
    let cat = catalog?;
    let level = level?;
    let known = cat.ecu_tree_series().ok()?;
    let series = klartext_semantic::bordnet_series_for(level, &known)?;
    let entries = cat.ecu_tree(&series).ok()?;
    if entries.is_empty() {
        return None;
    }
    let map = entries.into_iter().map(|e| (e.address, e)).collect();
    Some((series, map))
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

/// Build a decoded [`FaultInfo`] for a DTC at `address`, tagged with its `source`
/// memory, with DB text when available.
fn fault_info(dtc: &Dtc, address: u8, catalog: Option<&Catalog>, source: FaultSource) -> FaultInfo {
    fault_info_with_text(dtc, address, catalog, source, None)
}

/// As [`fault_info`], plus the ECU's own description for the entry when there is one.
///
/// `ecu_text` is the `F_ORT_TEXT` an `IS_LESEN` run reported. It is listed FIRST,
/// under the `"ecu"` pseudo-variant, because it comes from this very ECU's SGBD
/// rather than a per-variant DB lookup that may not know the code at all — for
/// info-memory entries it is often the only description that exists.
fn fault_info_with_text(
    dtc: &Dtc,
    address: u8,
    catalog: Option<&Catalog>,
    source: FaultSource,
    ecu_text: Option<&str>,
) -> FaultInfo {
    let mut descriptions = Vec::new();
    if let Some(text) = ecu_text {
        descriptions.push(FaultDescription {
            variant: "ecu".to_string(),
            saecode: None,
            text: Some(text.to_string()),
        });
    }
    descriptions.extend(describe_faults(catalog, address, dtc.code));
    FaultInfo {
        code_hex: dtc_code_hex(dtc),
        status_hex: format!("{:02X}", dtc.status),
        status_flags: status_flags(dtc.status)
            .into_iter()
            .map(String::from)
            .collect(),
        source: fault_source_tag(source),
        presence: match dtc.presence() {
            Presence::Present => "present",
            Presence::Absent => "absent",
            Presence::Unknown => "unknown",
        },
        descriptions,
    }
}

/// One info-memory entry as the ECU's own `IS_LESEN` job reports it.
///
/// The job emits one result SET per entry, carrying both the raw record and BMW's
/// authored text for it, so the two travel together.
#[derive(Debug, Clone)]
struct InfoEntry {
    /// The `F_HEX_CODE` record: 3-byte code + 1-byte ISO status.
    dtc: Dtc,
    /// The ECU's own `F_ORT_TEXT`, when it gave one.
    text: Option<String>,
}

/// The wire tag for which memory a fault entry came from — ISTA's `EcuDTCType`.
///
/// `"fault_memory"` for the `19 02` store (`"F"`), `"info_memory"` for the `22 2000`
/// Infospeicher (`"I"`), matching `VehicleIdent.cs:3518`.
fn fault_source_tag(source: FaultSource) -> &'static str {
    match source {
        FaultSource::FaultMemory => "fault_memory",
        FaultSource::InfoMemory => "info_memory",
    }
}

/// The `read_faults` note: the two stores it merges, plus the detail-depth caveat.
fn fault_bundle_note(detail: DetailDepth) -> String {
    let mut note = String::from(
        "Includes both stores: `faults` is the 19 02 fault memory, `info_entries` the \
         22 2000 info memory (Infospeicher) ISTA shows alongside faults — each entry's \
         `source` says which. `info_supported`=false just means the ECU keeps no info \
         memory, not an error.",
    );
    if let Some(caveat) = detail_stub_note(detail) {
        note.push(' ');
        note.push_str(caveat);
    }
    note
}

/// The per-fault-detail caveat, when a `detail` depth was requested.
///
/// `read_faults`/`read_all_faults` ACCEPT a `detail` depth so the surface is stable,
/// but the per-fault freeze-frame reads (`19 04`/`06`/`09`) are NOT fetched inline —
/// they are `2 ×` the fault count in requests and unbounded (research §E.1/§F.2), so
/// they stay behind [`KlartextServer::read_fault_detail`]. `None` for the default
/// [`DetailDepth::None`]: the base bundle carries no caveat.
fn detail_stub_note(detail: DetailDepth) -> Option<&'static str> {
    match detail {
        DetailDepth::None => None,
        DetailDepth::Relevant | DetailDepth::All => Some(
            "Per-fault freeze-frame detail was requested but is not fetched inline (its \
             cost is 2x the fault count and unbounded) — call read_fault_detail with a \
             fault's code_hex for its snapshot/extended/severity data.",
        ),
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
/// Never exposes the execution frame bytes — only metadata and guidance.
/// `confirmed_write_eligible` is true only for a low-risk, derived function — the
/// class planned for a future confirmed-write tool; high-risk and not-derivable
/// functions are never eligible.
fn service_function_info(function: &ServiceFunction) -> ServiceFunctionInfo {
    let low = function.risk() == Risk::Low;
    let derived = function.is_derived();
    let guidance = if !low {
        "HIGH-risk physical actuation/calibration — run only with the human present, via \
         run_service_function (confirm=true), with the function's preconditions met. It \
         moves a component or alters calibration."
            .to_string()
    } else if derived {
        "Low-risk and derived (UNCONFIRMED, [verify against capture]) — the safest class; \
         run via run_service_function (confirm=true)."
            .to_string()
    } else {
        "Low-risk; its offline frame is not derivable here (discovery-only). Execution \
         goes through run_service_function by ISTA catalog function_id, not this SGBD \
         frame."
            .to_string()
    };
    ServiceFunctionInfo {
        label: function.label.clone(),
        name: function.name.clone(),
        category: category_slug(function.category).to_string(),
        risk: if low { "low" } else { "high" }.to_string(),
        derivation: function.derivation.status().to_string(),
        citation: function.derivation.citation().map(str::to_string),
        confirmed_write_eligible: low && derived,
        guidance,
    }
}

/// Map a semantic [`ServiceFunctionCatalogEntry`] to its discovery DTO.
///
/// Classifies the post-Main hold with the SAME rule execution uses
/// ([`klartext_service::hold_for`]) — rebuilding the minimal [`FixedFunction`] the
/// classifier reads (activation + duration; the operator text does not affect it) —
/// so discovery can never disagree with what [`KlartextServer::run_service_function`]
/// will actually do with the same `function_id`.
fn service_catalog_info(entry: ServiceFunctionCatalogEntry) -> ServiceFunctionCatalogInfo {
    let ff = FixedFunction {
        function_id: entry.function_id,
        activation: entry.activation,
        activation_duration_ms: entry.activation_duration_ms,
        preparing_text: None,
        processing_text: None,
        post_text: None,
    };
    let (hold, hold_ms) = match hold_for(Some(&ff), entry.has_reset) {
        Hold::None => ("none", None),
        Hold::Timed(d) => (
            "timed",
            Some(i64::try_from(d.as_millis()).unwrap_or(i64::MAX)),
        ),
        Hold::UntilStop => ("until_stop", None),
    };
    ServiceFunctionCatalogInfo {
        function_id: entry.function_id,
        title: entry.title,
        has_reset: entry.has_reset,
        hold: hold.to_string(),
        hold_ms,
        preparing_text: entry.preparing_text,
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
    use klartext_client::{ClampCycleFailure, SupplierJob, SupplierJobReport};
    use klartext_sgbd::Table;

    use super::*;

    /// A finished sequence with nothing to report, as a base for the note tests.
    fn quiet_report() -> ClearSequenceReport {
        ClearSequenceReport {
            before: Vec::new(),
            functional: Ok(vec![0x12]),
            stragglers: Vec::new(),
            supplier_jobs: Vec::new(),
            gateway_zfs: Ok(()),
            clamp_cycle: Ok(()),
            reident: Ok(vec![0x12]),
            verification: Vec::new(),
        }
    }

    /// A terminal 15 that may still be DOWN is the one outcome here that can leave
    /// the car unable to start. It must LEAD the note in plain words — an agent
    /// summarising the result must not be able to relay it as a routine success.
    #[test]
    fn a_failed_terminal_15_restore_leads_the_note() {
        let mut report = quiet_report();
        report.clamp_cycle = Err(ClampCycleFailure {
            restored: false,
            message: "terminal-15 cycle failed during the on phase".to_string(),
        });
        let clamp = ClampCycleInfo {
            cycled: false,
            terminal_15_restored: false,
            error: Some("failed".to_string()),
        };
        let note = clear_all_note(&report, &clamp, 1, 1);
        assert!(note.starts_with("URGENT"), "{note}");
        assert!(note.contains("may not start"), "{note}");
        assert!(note.contains("cycle the ignition"), "{note}");

        // ...and a cycle that DID complete must not cry wolf.
        let ok = clear_all_note(&quiet_report(), &clamp_ok(), 1, 1);
        assert!(!ok.contains("URGENT"), "{ok}");
        assert!(ok.contains("Terminal 15 was cycled"), "{ok}");
    }

    fn clamp_ok() -> ClampCycleInfo {
        ClampCycleInfo {
            cycled: true,
            terminal_15_restored: true,
            error: None,
        }
    }

    /// Supplier stores klartext could not clear must be named as NOT cleared, and
    /// distinguished from the fault memories that were — otherwise "clear done" reads
    /// as "everything is clear" when it is not.
    #[test]
    fn the_note_says_when_supplier_stores_were_left_uncleared() {
        let mut report = quiet_report();
        report.supplier_jobs = vec![SupplierJobReport {
            job: SupplierJob {
                ecu: "FEM_20",
                job: "IS_LOESCHEN_TMS",
                arg: "0x01",
            },
            not_run: Some("no path to run it".to_string()),
        }];
        let note = clear_all_note(&report, &clamp_ok(), 1, 1);
        assert!(note.contains("1 supplier-specific"), "{note}");
        assert!(note.contains("NOT"), "{note}");

        // With no supplier store gated in, the note must not mention them at all.
        let quiet = clear_all_note(&quiet_report(), &clamp_ok(), 1, 1);
        assert!(!quiet.contains("supplier-specific"), "{quiet}");
    }

    /// A failed broadcast is not a failed clear — every faulted ECU is then cleared
    /// physically instead. The note must say which route ran.
    #[test]
    fn the_note_distinguishes_a_failed_broadcast_from_a_failed_clear() {
        let mut report = quiet_report();
        report.functional = Err("timed out".to_string());
        let note = clear_all_note(&report, &clamp_ok(), 0, 0);
        assert!(note.contains("broadcast) clear FAILED"), "{note}");
        assert!(note.contains("cleared physically instead"), "{note}");

        // The succeeding case reports the split instead, not a failure.
        let ok = clear_all_note(&quiet_report(), &clamp_ok(), 1, 1);
        assert!(!ok.contains("FAILED"), "{ok}");
        assert!(ok.contains("reached 1 ECU(s)"), "{ok}");
    }

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

    /// The group-ident rung's ORDERING rule. The DB records several groups per
    /// address (0x78 is both `d_klima` and `g_klima`, because one address is served
    /// by different generations), and the `g_` ones must be tried first: the `d_`
    /// groups are the K-line-era jobs whose interface-configuration opcodes this VM
    /// does not implement, so they cannot run over HSFZ at all.
    /// The job's entries REPLACE the direct read's when it ran, and the direct
    /// read's survive untouched when it did not. Shared by read_faults and the
    /// whole-car sweep, so this pins both.
    #[test]
    fn the_info_half_prefers_the_job_and_falls_back_to_the_direct_read() {
        let direct = [Dtc {
            code: [0xAA, 0xBB, 0xCC],
            status: 0x08,
        }];

        // The job ran: its entries win, and info memory is reported present even
        // though the direct read said otherwise (the wrong-frame case).
        let job = vec![InfoEntry {
            dtc: Dtc {
                code: [0x27, 0x7F, 0x00],
                status: 0x20,
            },
            text: Some("Recovery aufgetreten".to_string()),
        }];
        let (entries, supported) =
            KlartextServer::info_entries_from(Some(job), 0x12, &direct, false, None);
        assert!(supported, "a job that ran proves an info memory exists");
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].code_hex, "277F00");
        assert_eq!(
            entries[0]
                .descriptions
                .first()
                .and_then(|d| d.text.as_deref()),
            Some("Recovery aufgetreten"),
            "the ECU's own text must survive into the surfaced entry"
        );

        // The job did not run: the direct read's answer is passed through as-is.
        let (entries, supported) =
            KlartextServer::info_entries_from(None, 0x12, &direct, true, None);
        assert!(supported);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].code_hex, "AABBCC");
        assert!(entries[0].descriptions.is_empty());

        // …including the "no info memory" answer.
        let (entries, supported) = KlartextServer::info_entries_from(None, 0x12, &[], false, None);
        assert!(!supported);
        assert!(entries.is_empty());
    }

    /// The ECU's own `F_ORT_TEXT` must reach the caller, and must come FIRST.
    ///
    /// The raw `22 2000` read yields code+status only; the ECU's own `IS_LESEN`
    /// yields BMW's authored text per entry, and for an info-memory code that is
    /// often the only description there is — the per-variant DB lookup may not know
    /// the code at all.
    #[test]
    fn an_ecu_authored_description_is_listed_before_the_db_lookup() {
        let dtc = Dtc {
            code: [0x27, 0x7F, 0x00],
            status: 0x20,
        };
        let with_text = fault_info_with_text(
            &dtc,
            0x12,
            None,
            FaultSource::InfoMemory,
            Some("DDE-Steuergeraet intern: Recovery aufgetreten"),
        );
        assert_eq!(
            with_text.descriptions.first().map(|d| d.variant.as_str()),
            Some("ecu"),
            "the ECU's own text must lead"
        );
        assert_eq!(
            with_text.descriptions[0].text.as_deref(),
            Some("DDE-Steuergeraet intern: Recovery aufgetreten")
        );
        assert_eq!(with_text.source, "info_memory");

        // Without one, nothing is invented — the plain path is unchanged.
        let without = fault_info(&dtc, 0x12, None, FaultSource::InfoMemory);
        assert!(without.descriptions.is_empty());
        assert_eq!(without.code_hex, with_text.code_hex);
        assert_eq!(without.status_flags, with_text.status_flags);
    }

    /// car-session-2 §3.3: the note claimed an action klartext did not take. An
    /// ECU-self-reverting function (has_reset:false / hold:none) reported "Ran and
    /// returned to safe" while the wire showed one 2F and no return-to-safe frame.
    /// True in effect, wrong about who did it — so the two cases must read
    /// differently.
    #[test]
    fn a_run_note_distinguishes_a_transmitted_teardown_from_an_absent_one() {
        let report = |teardown| ServiceReport {
            title: None,
            phases: Vec::new(),
            teardown,
            succeeded: true,
            held: false,
        };
        let absent = service_run_note(&report(Teardown::NotDefined));
        let transmitted = service_run_note(&report(Teardown::Ran));
        assert_ne!(absent, transmitted);
        assert!(
            absent.contains("NO teardown") && absent.contains("revert on its own"),
            "an undefined teardown must say klartext sent nothing: {absent}"
        );
        assert!(
            transmitted.contains("transmitted"),
            "a real teardown must say klartext sent it: {transmitted}"
        );
        // Neither may claim the old, ambiguous phrasing.
        assert!(!absent.contains("Ran and returned to safe"));
    }

    #[test]
    fn ident_groups_put_the_uds_era_group_first() {
        let slot = |address, group: &str, extra: &[&str]| EcuSlot {
            address,
            group_name: group.to_string(),
            extra_groups: extra.iter().map(|s| (*s).to_string()).collect(),
            title: None,
        };
        // The catalog hands back the canonical group plus the extras, in DB order.
        let groups = KlartextServer::order_ident_groups(slot(0x78, "d_klima", &["g_klima"]));
        assert_eq!(groups, vec!["g_klima".to_string(), "d_klima".to_string()]);

        // Already-first stays first, and duplicates collapse.
        let groups = KlartextServer::order_ident_groups(slot(0x12, "g_motor", &["g_motor"]));
        assert_eq!(groups, vec!["g_motor".to_string()]);

        // An address with only a legacy group still yields it — the job simply will
        // not run, which `ident_variant` degrades to "unresolved", not an error.
        let groups = KlartextServer::order_ident_groups(slot(0x10, "d_0010", &[]));
        assert_eq!(groups, vec!["d_0010".to_string()]);
    }

    /// Without a car there is nothing to identify against, so the rung must be a
    /// clean no-op: `resolve_variant_live` falls back to exactly what the offline
    /// ladder said, never an error and never a hang.
    #[tokio::test]
    async fn the_ident_rung_is_a_no_op_when_not_connected() {
        use clap::Parser;
        let server = KlartextServer::new(ServerConfig::parse_from(["klartext-mcp"]));
        // Offline ladder resolves -> passed straight through, no car touched.
        assert_eq!(
            server
                .resolve_variant_live(0x12, Ok("d72n47a0".to_string()), None)
                .await
                .as_deref(),
            Some("d72n47a0")
        );
        // Offline ladder missed, and there is no session to identify over.
        assert_eq!(
            server
                .resolve_variant_live(0x78, Err(vec!["g_klima".to_string()]), None)
                .await,
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
            title: None,
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
    fn run_error_refused_names_the_refused_service_id() {
        // The safety seam: a write-emitting job trips the read-only gate, and the
        // resulting Refused surfaces as an invalid-request that names the gated SID —
        // never a generic 500.
        let err = run_error_to_mcp(
            "STEUERN_X",
            RunError::Exchange(ExchangeError::Refused {
                sid: 0x2E,
                frame: vec![0x84, 0x12, 0xF1, 0x2E],
            }),
        );
        assert!(err.message.contains("2E"), "{}", err.message);
        assert!(err.message.contains("read-only"), "{}", err.message);
    }

    #[test]
    fn run_error_other_faults_map_to_internal_error() {
        // A non-refusal fault (here a missing job) is an internal error carrying the
        // job's context, and never mislabeled as a gate refusal.
        let err = run_error_to_mcp("NOPE", RunError::JobNotFound("NOPE".to_string()));
        assert!(err.message.contains("NOPE"), "{}", err.message);
        assert!(!err.message.contains("read-only"), "{}", err.message);
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

    /// The held-slot orphan guard: a single held slot can only track one actuation,
    /// so starting a DIFFERENT held function must be refused, or the first
    /// component is left energised with no teardown path. This is the pure decision
    /// behind `run_service_function`'s guard; the refusal itself is exercised e2e in
    /// `run_service_function_refuses_while_a_different_function_is_held`.
    #[test]
    fn conflicting_held_refuses_anything_but_the_exact_same_function_and_address() {
        let held = HeldService {
            function_id: 7,
            address: 0x12,
            variant: "d72n47a0".to_string(),
        };
        // Nothing held: never a conflict.
        assert_eq!(conflicting_held(None, 7, 0x12), None);
        // A DIFFERENT function is held → refuse, naming the outstanding one (7).
        assert_eq!(conflicting_held(Some(&held), 9, 0x12), Some(7));
        // The EXACT same (function, address) held → proceed: same component.
        assert_eq!(conflicting_held(Some(&held), 7, 0x12), None);
        // SAME function id on a DIFFERENT address → still refuse: proceeding would
        // overwrite the slot and orphan the hold on 0x12. A mutation dropping the
        // `|| address` term (function_id only) lets this through and fails here.
        assert_eq!(conflicting_held(Some(&held), 7, 0x40), Some(7));
    }

    #[test]
    fn should_clear_held_only_on_the_exact_hold_that_ended_safe() {
        let held = HeldService {
            function_id: 7,
            address: 0x12,
            variant: "d72n47a0".to_string(),
        };
        // The exact (fn, addr) and teardown did NOT fail → clear.
        assert!(should_clear_held(Some(&held), 7, 0x12, false));
        // Review finding B: same (fn, addr) but teardown FAILED → keep the slot, so
        // disconnect still retries a component that may still be forced. Dropping the
        // `!teardown_failed` guard (as the old inline clear did) fails here.
        assert!(!should_clear_held(Some(&held), 7, 0x12, true));
        // Wrong function, or right function on the WRONG address → never clear this
        // hold (a stop on ECU 0x40 must not clear a hold on 0x12).
        assert!(!should_clear_held(Some(&held), 9, 0x12, false));
        assert!(!should_clear_held(Some(&held), 7, 0x40, false));
        // Nothing held → nothing to clear.
        assert!(!should_clear_held(None, 7, 0x12, false));
    }
}
