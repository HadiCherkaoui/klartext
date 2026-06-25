//! Server configuration from CLI args + environment (read-only; mirrors the CLI).
//!
//! The car connection is established lazily by the `connect` tool — never at
//! startup — so this only carries the settings needed to discover/connect and to
//! locate the ISTA semantic database.

use std::net::{IpAddr, Ipv4Addr};
use std::path::PathBuf;
use std::time::Duration;

use clap::Parser;
use klartext_client::{ClientConfig, DEFAULT_BROADCAST};
use klartext_hsfz::{CONNECT_TIMEOUT_DEFAULT_MS, DIAG_PORT, TESTER_ADDRESS};
use klartext_uds::P2_STAR_SERVER_MAX_DEFAULT_MS;

/// Runtime configuration for the klartext MCP server.
#[derive(Debug, Clone, Parser)]
#[command(
    version,
    about = "Read-only BMW F-series diagnostics as MCP tools over stdio."
)]
pub struct ServerConfig {
    /// Default gateway IP for `connect` to use directly (skips discovery). The
    /// `connect` tool's argument overrides this.
    #[arg(long, env = "KLARTEXT_GATEWAY")]
    pub gateway_ip: Option<IpAddr>,

    /// Bind discovery to this link-local source IP (default: auto-detect).
    #[arg(long, env = "KLARTEXT_BIND")]
    pub bind: Option<Ipv4Addr>,

    /// Broadcast address to probe during discovery.
    #[arg(long, default_value_t = DEFAULT_BROADCAST)]
    pub broadcast: Ipv4Addr,

    /// TCP diagnostic port.
    #[arg(long, default_value_t = DIAG_PORT)]
    pub port: u16,

    /// Per-read timeout in ms (P2*).
    #[arg(long, default_value_t = P2_STAR_SERVER_MAX_DEFAULT_MS)]
    pub timeout: u64,

    /// TCP connect timeout in ms.
    #[arg(long, default_value_t = CONNECT_TIMEOUT_DEFAULT_MS)]
    pub connect_timeout: u64,

    /// Milliseconds to listen for discovery replies.
    #[arg(long, default_value_t = 2000)]
    pub discovery_wait: u64,

    /// Path to the ISTA-derived semantic database (read-only) for fault/DID/ECU text.
    #[arg(
        long,
        env = "KLARTEXT_SEMANTIC_DB",
        default_value = "data/klartext-semantic.db"
    )]
    pub semantic_db: PathBuf,
}

impl ServerConfig {
    /// Build a client config targeting `ecu` (a logical address behind the ZGW).
    pub fn client_config(&self, ecu: u8) -> ClientConfig {
        ClientConfig {
            port: self.port,
            ecu,
            tester: TESTER_ADDRESS,
            connect_timeout: Duration::from_millis(self.connect_timeout),
            read_timeout: Duration::from_millis(self.timeout),
        }
    }

    /// The discovery listen window as a [`Duration`].
    pub fn discovery_wait(&self) -> Duration {
        Duration::from_millis(self.discovery_wait)
    }
}
