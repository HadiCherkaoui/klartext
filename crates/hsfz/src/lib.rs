//! BMW HSFZ transport (F-series ENET) for klartext: frame encode/decode and an
//! async TCP connection. Implemented from `docs/protocol-reference.md` — HSFZ is
//! proprietary and niche, so there is no crate for it.
//!
//! Concrete types only — no `Transport` trait until DoIP exists (CLAUDE.md).

mod conn;
mod discover;
mod frame;

pub use conn::HsfzConnection;
pub use discover::{Gateway, discover, link_local_bind_ip};
pub use frame::{HEADER_LEN, HsfzFrame, MAX_FRAME_LEN, control, read_frame, write_frame};

use std::net::{Ipv4Addr, SocketAddr};
use std::time::Duration;

use thiserror::Error;

/// TCP diagnostic port (§2.3). [verify against capture] — ICOM setups reassign it.
pub const DIAG_PORT: u16 = 6801;
/// UDP/TCP control & identification port (§2.3). [verify against capture].
pub const CONTROL_PORT: u16 = 6811;
/// Tester (client) logical address (§2.4). Convention 0xF4; hardcoded in M1.
pub const TESTER_ADDRESS: u8 = 0xF4;
/// Central gateway (ZGW) logical address (§2.4). [verify against capture] for your F20.
pub const ZGW_ADDRESS: u8 = 0x10;
/// Default TCP connect timeout (ms). ediabaslib uses 5000, stock EDIABAS.INI
/// uses 20000 — a conflict, so it is configurable. [verify against capture].
pub const CONNECT_TIMEOUT_DEFAULT_MS: u64 = 5000;

/// Errors from the HSFZ transport.
#[derive(Debug, Error)]
pub enum Error {
    #[error("failed to connect to gateway {peer}")]
    Connect {
        peer: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("timed out after {timeout:?} connecting to gateway {peer}")]
    ConnectTimeout { peer: SocketAddr, timeout: Duration },
    #[error("timed out after {timeout:?} waiting for a gateway response")]
    ReadTimeout { timeout: Duration },
    #[error(
        "HSFZ length {length} exceeds the {} byte sanity cap — likely a misframe \
         (wrong endianness, or the length convention is off by two)",
        MAX_FRAME_LEN
    )]
    FrameTooLarge { length: u32 },
    #[error("truncated HSFZ frame: have {have} byte(s), need {need}")]
    Truncated { have: usize, need: usize },
    #[error("failed to bind the discovery socket to {bind_ip} — is it your ENET link-local IP?")]
    DiscoveryBind {
        bind_ip: Ipv4Addr,
        #[source]
        source: std::io::Error,
    },
    #[error("I/O error on the HSFZ connection")]
    Io(#[source] std::io::Error),
}
