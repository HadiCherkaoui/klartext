//! klartext-mcp — a stdio MCP server over klartext-client/-semantic.
//!
//! Exposed as a library so integration tests can drive the tools in-process; the
//! `klartext-mcp` binary (`main.rs`) is a thin wrapper that serves over stdio. The
//! tool surface applies the blast-radius rule to the autonomous-agent surface:
//! reads, plus exactly **one** standard, non-physical, confirmation-gated write —
//! `clear_faults` (UDS 0x14). No actuation, no coding, and no derived-unconfirmed
//! WRITE frame is ever executable here; those stay in the CLI with a human in the
//! loop. (The M6 dynamic-read `0x2C` define — session-transient read plumbing —
//! is the one derived sequence reads legitimately use.)

pub mod config;
pub mod dto;
pub mod ecu;
pub mod profile;
pub mod server;
pub mod session;

#[doc(inline)]
pub use server::KlartextServer;
