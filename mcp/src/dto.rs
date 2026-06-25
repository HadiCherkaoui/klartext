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

/// Target ECU + DID for `read_data`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadDataRequest {
    /// ECU: a name ("DME"), hex address ("0x12"), or ISTA group name ("d_0012").
    pub ecu: String,
    /// Data identifier to read, hex (e.g. "F190" for the VIN, with or without 0x).
    pub did: String,
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
