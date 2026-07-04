//! Wire types for the MCP tools: structured, AI-facing request/response shapes.
//!
//! Every type derives `serde` + `schemars::JsonSchema` so rmcp can generate input
//! and output schemas for the tools. `schemars` is used via rmcp's re-export so
//! its version always matches rmcp's.

use rmcp::schemars;
use serde::{Deserialize, Serialize};

/// Result of `disconnect`: whether a live connection was dropped.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct DisconnectResult {
    /// Whether a live connection was dropped (`false` if already disconnected).
    pub was_connected: bool,
}

/// One targetable ECU for `list_ecus`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct EcuInfo {
    /// Diagnostic address as hex, e.g. `0x12`.
    pub address_hex: String,
    /// The canonical ISTA group name, e.g. `d_0012`.
    pub group_name: String,
    /// Other ISTA group names at this address (e.g. `g_motor`).
    pub extra_groups: Vec<String>,
    /// A human title for the ECU, when the DB has one.
    pub title: Option<String>,
    /// The SGBD variant names ISTA records at this address (for read_data etc.).
    pub variants: Vec<String>,
}

/// Result of `list_ecus`: the targetable ECUs from the semantic DB.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListEcusResult {
    /// Targetable ECUs, ordered by address.
    pub ecus: Vec<EcuInfo>,
    /// Whether the semantic DB was available to build the list.
    pub db_available: bool,
    /// Human note about the source of the list.
    pub note: String,
    /// Set when the DB was present but the ECU query failed (surfaced, not swallowed).
    pub db_error: Option<String>,
}

/// Arguments for `connect`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConnectRequest {
    /// Optional gateway IP override, e.g. "169.254.39.12". Omit to use the
    /// configured gateway or auto-discover on the link.
    #[serde(default)]
    pub gateway_ip: Option<String>,
}

/// Result of `connect`: the gateway, VIN, and initially held target.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ConnectResult {
    /// Whether a session is now held.
    pub connected: bool,
    /// The gateway IP the session is connected to.
    pub gateway_ip: String,
    /// The VIN, if one was obtained.
    pub vin: Option<String>,
    /// Where the VIN came from: `did_f190`, `discovery`, or `unknown`.
    pub vin_source: String,
    /// The ECU the session initially targets (the ZGW).
    pub target_ecu: String,
    /// Human note about the held session.
    pub note: String,
}

/// Target ECU for `read_faults`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFaultsRequest {
    /// ECU: a hex address ("0x12"), an ISTA group name ("d_0012"), or a variant
    /// name ("d72n47a0"). Call list_ecus to discover targetable ECUs.
    pub ecu: String,
    /// Include "not tested this cycle" catalog entries (status 0x40/0x50 noise).
    /// Default false — those are suppressed and only counted.
    #[serde(default)]
    pub include_not_tested: bool,
}

/// One per-variant human description for a fault.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FaultDescription {
    /// The ISTA ECU variant the description belongs to.
    pub variant: String,
    /// The SAE J2012 code, when the fault carries one.
    pub saecode: Option<String>,
    /// The fault text (English preferred, else German), when present.
    pub text: Option<String>,
}

/// One decoded fault: raw code/status plus ISO flags and descriptions.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FaultInfo {
    /// The 3-byte DTC as hex, e.g. "D9040A".
    pub code_hex: String,
    /// The status byte as hex, e.g. "08".
    pub status_hex: String,
    /// Decoded ISO 14229 status flag names.
    pub status_flags: Vec<String>,
    /// Per-variant fault descriptions from the semantic DB (empty without it).
    pub descriptions: Vec<FaultDescription>,
}

/// Result of `read_faults`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReadFaultsResult {
    /// The ECU spec that was requested.
    pub ecu: String,
    /// The resolved diagnostic address as hex.
    pub address: String,
    /// Number of faults returned.
    pub count: usize,
    /// The decoded faults.
    pub faults: Vec<FaultInfo>,
    /// Count of "not tested this cycle" entries suppressed (unless include_not_tested).
    pub not_tested_count: usize,
    /// Whether the semantic DB was available for descriptions.
    pub db_available: bool,
}

/// Target ECU and fault code for `read_fault_detail`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadFaultDetailRequest {
    /// ECU: a hex address ("0x12"), an ISTA group name ("d_0012"), or a variant
    /// name ("d72n47a0"). Call list_ecus to discover targetable ECUs.
    pub ecu: String,
    /// The 3-byte DTC as hex, e.g. "240000" — the `code_hex` from read_faults.
    pub code: String,
    /// The ECU SGBD variant (e.g. "d72n47a0") that decodes the freeze-frame fields.
    /// Optional: resolved from the ECU when omitted; without it the fields stay raw.
    #[serde(default)]
    pub variant: Option<String>,
}

/// One decoded freeze-frame (snapshot) field for `read_fault_detail`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct SnapshotFieldInfo {
    /// The 2-byte environmental-condition identifier as hex, e.g. "5205".
    pub id_hex: String,
    /// The human label (English from the DB, else the SGBD text, else `UW …`).
    pub label: String,
    /// The scaled value, when available and decodable.
    pub value: Option<f64>,
    /// The engineering unit, when the field has one.
    pub unit: Option<String>,
    /// False when the ECU reported the "not available" sentinel (e.g. no mileage).
    pub available: bool,
    /// The raw field bytes as hex.
    pub raw_hex: String,
}

/// One decoded extended-data record for `read_fault_detail`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ExtDataFieldInfo {
    /// The extended-data record number as hex, e.g. "02".
    pub record_hex: String,
    /// The record's SGBD name (e.g. "HFK", the occurrence/frequency counter).
    pub label: String,
    /// The record's integer value, when the record carries data.
    pub value: Option<i64>,
    /// The raw record bytes as hex.
    pub raw_hex: String,
}

/// Result of `read_fault_detail`: the fault plus its freeze-frame metadata.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FaultDetailResult {
    /// The ECU spec that was requested.
    pub ecu: String,
    /// The resolved diagnostic address as hex.
    pub address: String,
    /// The 3-byte DTC as hex, e.g. "240000".
    pub code_hex: String,
    /// Per-variant fault descriptions from the semantic DB (empty without it).
    pub descriptions: Vec<FaultDescription>,
    /// The freeze-frame (`19 04`) fields captured when the fault latched.
    pub snapshot: Vec<SnapshotFieldInfo>,
    /// The extended-data (`19 06`) records (occurrence/healing counters).
    pub extended: Vec<ExtDataFieldInfo>,
    /// The DTC severity byte (`19 09`) as hex, when the ECU reports it.
    pub severity_hex: Option<String>,
    /// The DTC functional-unit byte (`19 09`) as hex, when the ECU reports it.
    pub functional_unit_hex: Option<String>,
    /// Whether the SGBD was available to decode the fields (else they are raw).
    pub sgbd_available: bool,
    /// Human notes: whether records were present, undecoded tails, capture caveat.
    pub notes: Vec<String>,
}

/// Arguments for `clear_faults`: the target ECU and the explicit confirmation.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClearFaultsRequest {
    /// ECU: a hex address ("0x12"), an ISTA group name ("d_0012"), or a variant
    /// name ("d72n47a0") — see list_ecus.
    pub ecu: String,
    /// Must be `true` to clear. Defaults to false; without it the tool refuses and
    /// explains what clearing discards. Set it only after reading the faults and
    /// getting the human's explicit go-ahead.
    #[serde(default)]
    pub confirm: bool,
}

/// Result of a confirmed `clear_faults`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ClearFaultsResult {
    /// The ECU spec that was requested.
    pub ecu: String,
    /// The resolved diagnostic address as hex.
    pub address: String,
    /// Whether the ECU accepted the clear.
    pub cleared: bool,
    /// The 3-byte DTCs (hex) stored immediately before the clear — the record of
    /// what was discarded.
    pub codes_cleared: Vec<String>,
    /// Number of codes that were stored before the clear.
    pub count: usize,
    /// What the clear discarded and how to verify (re-read after a drive cycle).
    pub note: String,
}

/// Target ECU + value (a DID or a measurement name) for `read_data`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadDataRequest {
    /// ECU: a hex address ("0x12"), ISTA group name ("d_0012"), or variant name.
    pub ecu: String,
    /// Data identifier to read, hex (e.g. "F190" for the VIN, with or without 0x).
    /// Pass exactly one of `did` or `name`.
    #[serde(default)]
    pub did: Option<String>,
    /// A measurement name instead of a hex DID: the arg ("ITOEL"), result name, or
    /// description ("Motortemperatur") of a `list_measurements` entry. Needs
    /// `variant` to load that catalog. Pass exactly one of `did` or `name`.
    #[serde(default)]
    pub name: Option<String>,
    /// Optional SGBD variant (the ECU `.prg` stem, e.g. "d72n47a0"). With the
    /// server's `--sgbd-dir`, a DID that is a `SG_FUNKTIONEN` measurement id is
    /// scaled to an engineering value + unit (required when passing `name`); omit
    /// for standard/raw behavior.
    #[serde(default)]
    pub variant: Option<String>,
}

/// Arguments for `list_measurements`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListMeasurementsRequest {
    /// The ECU SGBD variant (the `.prg` stem, e.g. "d72n47a0"). Optional if `ecu`
    /// is given and the variant can be resolved (a learned profile, or a single
    /// DB candidate with a matching `.prg`); the server must have `--sgbd-dir`.
    #[serde(default)]
    pub variant: Option<String>,
    /// The ECU to resolve a `variant` for when `variant` is omitted: a hex address
    /// ("0x12"), group name, or variant name.
    #[serde(default)]
    pub ecu: Option<String>,
    /// Optional case-insensitive substring filter over the name, short label, and
    /// result name. The SGBD's terms are mostly German (e.g. "Öltemperatur",
    /// "Rußmasse", "Regeneration"). Big ECUs define ~1800 measurements and one
    /// call returns at most a capped page — search to find signals.
    #[serde(default)]
    pub search: Option<String>,
}

/// One live measurement in the read-only catalog listing.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct MeasurementInfo {
    /// The measurement id, e.g. "4517" — pass as read_data's `did` (with this
    /// `variant`) to read the value.
    pub id_hex: String,
    /// Human name of the signal (the SGBD description; falls back to the result
    /// name when blank), e.g. "gefilterte Öltemperatur".
    pub name: String,
    /// The short job argument, e.g. "ITOEL" — the most precise `name` for read_data.
    pub arg: String,
    /// The EDIABAS result name, e.g. "STAT_MOTOROEL_TEMPERATUR_WERT".
    pub result_name: String,
    /// Engineering unit of the scaled value, e.g. "degC" ("-" when unitless).
    pub unit: String,
    /// The ECU this measurement belongs to, as read_data/read_faults accept it
    /// (e.g. "0x12"); pass it as `ecu`.
    pub ecu_address: String,
}

/// Result of `list_measurements`: one ECU's readable live values, from its SGBD.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListMeasurementsResult {
    /// The SGBD variant the catalog was read from.
    pub variant: String,
    /// The listed measurements (after any search filter), sorted by id, capped.
    pub measurements: Vec<MeasurementInfo>,
    /// Number of measurements returned (at most the per-call cap).
    pub count: usize,
    /// Number of measurements matching the search before the cap.
    pub total: usize,
    /// How to read a listed value, and whether the cap truncated this listing.
    pub note: String,
}

/// Arguments for `list_service_functions`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ListServiceFunctionsRequest {
    /// The ECU SGBD variant (the `.prg` stem, e.g. "d72n47a0"). Optional if `ecu`
    /// is given and the variant can be resolved (a learned profile, or a single
    /// DB candidate with a matching `.prg`); the server must have `--sgbd-dir`.
    #[serde(default)]
    pub variant: Option<String>,
    /// The ECU to resolve a `variant` for when `variant` is omitted: a hex address
    /// ("0x12"), group name, or variant name.
    #[serde(default)]
    pub ecu: Option<String>,
    /// Optional risk filter: "low" (counter/adaptation/statistic resets) or "high"
    /// (physical actuation / calibration). Omit to list every risk tier.
    #[serde(default)]
    pub risk: Option<String>,
}

/// One service function in the read-only listing (no execution frame is exposed).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ServiceFunctionInfo {
    /// Short label the CLI's `service run <label>` uses, e.g. "Oel", "MSA2Hist".
    pub label: String,
    /// Human description of what the function does.
    pub name: String,
    /// Operation class: "cbs_reset", "statistic_reset", "learned_value_reset",
    /// "actuator_control", or "calibration".
    pub category: String,
    /// Blast-radius risk: "low" (no component moves; reversible) or "high" (moves a
    /// component or alters combustion/calibration behavior).
    pub risk: String,
    /// Frame status: "derived-unconfirmed" (a frame was derived from ISTA disassembly
    /// but is NOT hardware-confirmed — treat as `[verify against capture]`) or
    /// "frame-not-derivable" (no frame could be derived offline; discovery-only).
    pub derivation: String,
    /// Disassembly citation for a derived frame (job + address + SGBD), when present.
    pub citation: Option<String>,
    /// Whether a human may run this in the CLI (`service run … --confirm`): true only
    /// for a low-risk, derived function. High-risk and not-derivable are never runnable.
    pub runnable_in_cli: bool,
    /// Guidance for an AI caller: how a human runs it, or why it must not be run.
    pub guidance: String,
}

/// Result of `list_service_functions`: the read-only control catalog for one ECU.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListServiceFunctionsResult {
    /// The SGBD variant the catalog was read from.
    pub variant: String,
    /// The listed functions (after any risk filter), in discovery order.
    pub functions: Vec<ServiceFunctionInfo>,
    /// Number of functions returned.
    pub count: usize,
    /// Read-only-status and unconfirmed-frame caveat for the caller.
    pub note: String,
}

/// Result of `read_data`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReadDataResult {
    /// The ECU spec that was requested.
    pub ecu: String,
    /// The resolved diagnostic address as hex.
    pub address: String,
    /// The DID that was read, as four hex digits.
    pub did_hex: String,
    /// The signal name: a standard OBD-II PID name or ISO-standard DID name.
    pub name: Option<String>,
    /// A text rendering of the value, when the bytes are printable.
    pub value_text: Option<String>,
    /// The scaled engineering value, for a recognized standard OBD-II PID.
    pub scaled_value: Option<f64>,
    /// The unit of `scaled_value` (e.g. "°C", "rpm", "km/h"), when scaled.
    pub unit: Option<String>,
    /// The raw value bytes as spaced hex. Always present.
    pub raw_hex: String,
    /// Human note (e.g. for standard PIDs or BMW-specific DIDs).
    pub note: String,
}

/// Arguments for `scan_ecus`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScanEcusRequest {
    /// Re-read the gateway SVT even if a fitted list is cached from this session.
    #[serde(default)]
    pub rescan: bool,
}

/// One fitted ECU from the gateway's installed-ECU list (SVT).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FittedEcuInfo {
    /// Diagnostic address as hex, e.g. `0x12`.
    pub address_hex: String,
    /// Canonical ISTA group name, when the DB has one.
    pub group_name: Option<String>,
    /// A human title, when the DB has one.
    pub title: Option<String>,
}

/// Result of `scan_ecus`: the ECUs the gateway reports as installed (SVT).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ScanEcusResult {
    /// The fitted ECUs, ordered by address.
    pub ecus: Vec<FittedEcuInfo>,
    /// Human note (SVT read vs session cache).
    pub note: String,
}

/// Arguments for `read_all_faults`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadAllFaultsRequest {
    /// Re-read the fitted list (SVT) before reading (else use the session cache).
    #[serde(default)]
    pub rescan: bool,
}

/// One ECU's faults in a whole-car read.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EcuFaultsInfo {
    /// Diagnostic address as hex.
    pub address_hex: String,
    /// A human title, when the DB has one.
    pub title: Option<String>,
    /// Decoded relevant faults (not-tested noise is only counted).
    pub faults: Vec<FaultInfo>,
    /// Count of not-tested-this-cycle entries suppressed.
    pub not_tested_count: usize,
    /// Set if this ECU could not be read (the scan continued).
    pub error: Option<String>,
}

/// Result of `read_all_faults`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReadAllFaultsResult {
    /// Per-ECU faults, ordered by address; ECUs with no relevant fault are included
    /// with an empty list so the caller sees the whole scanned set.
    pub ecus: Vec<EcuFaultsInfo>,
    /// Total relevant faults across all ECUs.
    pub total_relevant: usize,
    /// Whether the semantic DB was available for fault text.
    pub db_available: bool,
    /// Human note (per-ECU read_faults shows the not-tested entries in full).
    pub note: String,
}

/// Arguments for `clear_all_faults`: whole-car clear, confirmation-gated.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClearAllFaultsRequest {
    /// Must be `true`. Without it the tool refuses and explains what a whole-car
    /// clear discards (every ECU's freeze-frames; readiness monitors may reset).
    #[serde(default)]
    pub confirm: bool,
    /// Re-read the fitted list (SVT) before clearing (else use the session cache).
    #[serde(default)]
    pub rescan: bool,
}

/// One ECU's clear outcome in a whole-car clear.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EcuClearInfo {
    /// Diagnostic address as hex.
    pub address_hex: String,
    /// The DTC codes (hex) stored before the clear — the record of what was discarded.
    pub codes_before: Vec<String>,
    /// Whether the post-clear re-read showed no relevant fault.
    pub verified_clean: bool,
    /// Set if this ECU's clear failed (others were still processed).
    pub error: Option<String>,
}

/// Result of a confirmed `clear_all_faults`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ClearAllFaultsResult {
    /// Per-ECU clear outcomes, ordered by address.
    pub ecus: Vec<EcuClearInfo>,
    /// How many ECUs were cleared clean.
    pub cleared_clean: usize,
    /// Human note (verify guidance).
    pub note: String,
}

// ── identify_vehicle: the whole-vehicle identity in one read ──────────────────

/// One identification DID value, named and rendered at the surface.
///
/// The client returns raw bytes only; the name and text come from
/// `klartext_semantic::did::decode`, keeping the client protocol-pure.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct IdFieldDto {
    /// The identification DID as four hex digits, e.g. "F190" (VIN).
    pub did_hex: String,
    /// The DID's ISO/standard name, when known (e.g. "VIN", "systemName").
    pub name: Option<String>,
    /// A text rendering of the value, when the bytes are printable.
    pub text: Option<String>,
    /// The raw value bytes as spaced hex. Always present.
    pub raw_hex: String,
}

/// One ECU's identification block for the MCP surface.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EcuIdentDto {
    /// Diagnostic address as hex, e.g. "0x12".
    pub address_hex: String,
    /// Canonical ISTA group name, when the DB has one.
    pub name: Option<String>,
    /// The standardized identification DIDs the ECU served (raw + decoded).
    pub fields: Vec<IdFieldDto>,
}

/// The decoded vehicle order (FA) for the MCP surface; fields are capture-gated.
///
/// Only `version` and `raw_hex` are decoded today; the header fields and the option
/// list stay `None`/empty until the FA byte layout is confirmed against a capture.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct VehicleOrderDto {
    /// The FA format version, when the raw region carries it.
    pub version: Option<u16>,
    /// Model series (Baureihe) — capture-gated, `None` until the FA layout is confirmed.
    pub baureihe: Option<String>,
    /// Type key (Typschlüssel) — capture-gated.
    pub typ_schluessel: Option<String>,
    /// Paint code (Lackcode) — capture-gated.
    pub lackcode: Option<String>,
    /// Upholstery code (Polstercode) — capture-gated.
    pub polstercode: Option<String>,
    /// Build date — capture-gated.
    pub build_date: Option<String>,
    /// Option/SA codes — capture-gated, empty until the FA layout is confirmed.
    pub options: Vec<String>,
    /// The raw FA region as spaced hex. Always present.
    pub raw_hex: String,
}

/// Result of `identify_vehicle`: the whole-vehicle identity in one read.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct VehicleIdentityResult {
    /// The vehicle VIN, when the gateway answered `22 F190`.
    pub vin: Option<String>,
    /// The integration level (I-Stufe), when the gateway answered `22 100B`.
    pub i_stufe: Option<String>,
    /// The decoded vehicle order (FA); most fields are capture-gated.
    pub vehicle_order: VehicleOrderDto,
    /// The fitted ECUs from the gateway SVT, named from the semantic DB.
    pub ecus: Vec<FittedEcuInfo>,
    /// Each fitted ECU's identification block (part numbers, system name, serial).
    pub identification: Vec<EcuIdentDto>,
    /// Human notes: the derived-framing / capture-gated caveat.
    pub notes: Vec<String>,
}

// ── fault_help: a fault's ISTA documentation, DB-only (no car) ─────────────────

/// One ISTA document linked to a fault (link+title layer).
///
/// Sourced from the semantic DB's `fault_doc ⋈ infoobject` join. The document prose
/// is a deferred layer — this carries the title and the pointers to it, not the body.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FaultDocDto {
    /// The document title (English preferred, German fallback), when present.
    pub title: Option<String>,
    /// ISTA info type: "FKB" is a fault description; other values are procedures.
    pub infotype: Option<String>,
    /// The ISTA document number, when present.
    pub docnumber: Option<String>,
    /// True when ISTA flags the document safety-relevant.
    pub safety_relevant: bool,
    /// Stable ISTA INFOOBJECT id — the handle the deferred prose layer will resolve.
    pub infoobject_id: i64,
}

/// Arguments for `fault_help`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FaultHelpRequest {
    /// ECU as hex address (e.g. `0x12`), ISTA group name, or variant name.
    pub ecu: String,
    /// The 3-byte DTC as hex, e.g. `4B1234` (a `code_hex` from read_faults).
    pub code: String,
}

/// Result of `fault_help`: the fault text plus its linked ISTA documents.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FaultHelpResult {
    /// The ECU spec that was requested.
    pub ecu: String,
    /// The 3-byte DTC as hex, e.g. `4B1234`.
    pub code_hex: String,
    /// Per-variant fault descriptions from the semantic DB (empty without it).
    pub descriptions: Vec<FaultDescription>,
    /// The ISTA documents linked to this fault (empty without the repair-doc extract).
    pub docs: Vec<FaultDocDto>,
    /// The rendered FKB fault-description prose (German markdown), when the doc
    /// store is built. Empty otherwise — the `docs` pointers still apply.
    pub body: Vec<String>,
    /// Human note about the doc source and the title-only nature of the result.
    pub note: String,
}
