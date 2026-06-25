//! klartext-mcp — a read-only stdio MCP server over klartext-client/-semantic.
//!
//! Exposed as a library so integration tests can drive the tools in-process; the
//! `klartext-mcp` binary (`main.rs`) is a thin wrapper that serves over stdio. The
//! tool surface is intentionally **read-only** — no clear, actuation, or coding —
//! applying the blast-radius rule to the autonomous-agent surface.

pub mod config;
pub mod dto;
pub mod ecu;
pub mod server;
pub mod session;

#[doc(inline)]
pub use server::KlartextServer;
