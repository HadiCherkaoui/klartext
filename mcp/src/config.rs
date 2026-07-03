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
    about = "BMW F-series diagnostics as MCP tools over stdio: reads, plus the \
             confirmation-gated clear_faults."
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

    /// Directory of ECU SGBD `.prg` files (read-only) enabling proprietary
    /// measurement scaling. BYO-data; omit to keep BMW-specific DIDs raw.
    #[arg(long, env = "KLARTEXT_SGBD_DIR")]
    pub sgbd_dir: Option<PathBuf>,

    /// Per-ECU presence-probe timeout in ms for the whole-car scan.
    #[arg(long, default_value_t = 300)]
    pub probe_timeout: u64,

    /// How many ECUs to probe/read at once during a scan (1 = strictly sequential).
    #[arg(long, default_value_t = 8)]
    pub scan_concurrency: usize,

    /// Directory for learned per-VIN variant profiles (address → SGBD variant).
    /// Defaults to `$XDG_STATE_HOME/klartext/profiles` (else `~/.local/state/...`).
    #[arg(long, env = "KLARTEXT_PROFILE_DIR")]
    pub profile_dir: Option<PathBuf>,

    /// Disable reading and writing the learned variant profile entirely.
    #[arg(long, default_value_t = false)]
    pub no_profile: bool,
}

impl ServerConfig {
    /// Build the client config (one connection reaches every ECU by per-read target).
    pub fn client_config(&self) -> ClientConfig {
        ClientConfig {
            port: self.port,
            tester: TESTER_ADDRESS,
            connect_timeout: Duration::from_millis(self.connect_timeout),
            read_timeout: Duration::from_millis(self.timeout),
        }
    }

    /// The whole-car scan tuning from `--probe-timeout` / `--scan-concurrency`.
    pub fn scan_options(&self) -> klartext_client::ScanOptions {
        klartext_client::ScanOptions {
            probe_timeout: Duration::from_millis(self.probe_timeout),
            concurrency: self.scan_concurrency,
        }
    }

    /// The discovery listen window as a [`Duration`].
    pub fn discovery_wait(&self) -> Duration {
        Duration::from_millis(self.discovery_wait)
    }

    /// The effective profile directory (default under XDG state home), or `None`
    /// when profiles are disabled with `--no-profile`.
    pub fn profile_dir(&self) -> Option<PathBuf> {
        if self.no_profile {
            return None;
        }
        Some(self.profile_dir.clone().unwrap_or_else(|| {
            let base = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    let home = std::env::var_os("HOME")
                        .map(PathBuf::from)
                        .unwrap_or_default();
                    home.join(".local/state")
                });
            base.join("klartext/profiles")
        }))
    }
}
