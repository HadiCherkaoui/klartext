//! Wire types for the MCP tools: structured, AI-facing request/response shapes.
//!
//! Every type derives `serde` + `schemars::JsonSchema` so rmcp can generate input
//! and output schemas for the tools. `schemars` is used via rmcp's re-export so
//! its version always matches rmcp's.

use rmcp::schemars;
use serde::Serialize;

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
