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
    /// Known names: built-in aliases and/or the ISTA group name.
    pub names: Vec<String>,
    /// The ISTA group name (e.g. `d_0012`), when the DB provides it.
    pub group_name: Option<String>,
    /// Origin of this entry: `builtin`, `db`, or `builtin+db`.
    pub source: String,
}

/// Result of `list_ecus`: the targetable ECUs and their source.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ListEcusResult {
    /// Targetable ECUs, ordered by address.
    pub ecus: Vec<EcuInfo>,
    /// Whether the semantic DB was available to enrich the list.
    pub db_available: bool,
    /// Human note about the source of the list.
    pub note: String,
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
    /// ECU: a name (e.g. "DME"), a hex address ("0x12"), or an ISTA group name
    /// ("d_0012"). Call list_ecus to discover targetable ECUs.
    pub ecu: String,
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
    /// Whether the semantic DB was available for descriptions.
    pub db_available: bool,
}

/// Arguments for `clear_faults`: the target ECU and the explicit confirmation.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClearFaultsRequest {
    /// ECU: a name (e.g. "DME"), a hex address ("0x12"), or an ISTA group name
    /// ("d_0012") — see list_ecus.
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
    /// ECU: a name ("DME"), hex address ("0x12"), or ISTA group name ("d_0012").
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
    /// The ECU SGBD variant (the `.prg` stem, e.g. "d72n47a0"). Required — the
    /// measurement catalog is per-ECU, read from that SGBD; the server must have
    /// `--sgbd-dir`.
    pub variant: String,
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
    /// The ECU diagnostic address as written in the SGBD (e.g. "12").
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
    /// The ECU SGBD variant (the `.prg` stem, e.g. "d72n47a0"). Required — the control
    /// catalog is per-ECU, read from that SGBD; the server must have `--sgbd-dir`.
    pub variant: String,
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
