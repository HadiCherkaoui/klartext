//! The ephemeral car connection held in server state, with establish + retarget.
//!
//! [`Connection`] wraps a [`DiagnosticClient`] (which runs the background
//! keepalive) and remembers which ECU it currently targets. [`establish`] opens a
//! session to the gateway and reads the VIN; [`ensure_target`] retargets by
//! reconnecting when a read needs a different ECU than the held one. One diagnostic
//! connection is held at a time, so switching ECUs drops the old session and opens
//! a fresh one — its `Drop` aborts the previous keepalive.

use std::net::IpAddr;
use std::sync::Arc;

use klartext_client::{DiagnosticClient, Gateway};
use klartext_hsfz::ZGW_ADDRESS;
use tokio::sync::Mutex;

use crate::config::ServerConfig;

/// The DID for the VIN (ISO 14229 vehicleIdentificationNumber).
const DID_VIN: u16 = 0xF190;

/// Where a reported VIN came from.
#[derive(Debug, Clone, Copy)]
pub enum VinSource {
    /// Read authoritatively from the ZGW via DID 0xF190.
    DidF190,
    /// Best-effort from the discovery (0x11) announcement body.
    Discovery,
    /// No VIN could be obtained.
    Unknown,
}

impl VinSource {
    /// The wire string for this source (`did_f190` / `discovery` / `unknown`).
    pub fn as_str(self) -> &'static str {
        match self {
            VinSource::DidF190 => "did_f190",
            VinSource::Discovery => "discovery",
            VinSource::Unknown => "unknown",
        }
    }
}

/// A live diagnostic connection; the client targets [`Connection::target`].
#[derive(Debug)]
pub struct Connection {
    /// The gateway IP the client is connected to.
    pub gateway_ip: IpAddr,
    /// The VIN, if one was obtained.
    pub vin: Option<String>,
    /// Where [`Connection::vin`] came from.
    pub vin_source: VinSource,
    /// The ECU address the held client currently targets.
    pub target: u8,
    /// The managed UDS session (runs its own keepalive).
    pub client: DiagnosticClient,
}

/// Shared, mutable server connection state. `None` = not connected.
pub type SessionState = Arc<Mutex<Option<Connection>>>;

/// Establish a session to the gateway (ZGW) and read the VIN.
///
/// `gateway_ip` takes the direct connect path; `None` auto-discovers on the link.
/// The returned [`Connection`] targets the ZGW (0x10).
///
/// # Errors
/// Returns a human message if discovery finds no (or several) gateways, or the TCP
/// connect fails.
pub async fn establish(
    config: &ServerConfig,
    gateway_ip: Option<IpAddr>,
) -> Result<Connection, String> {
    let zgw_config = config.client_config(ZGW_ADDRESS);
    let (mut client, gateway, ip): (DiagnosticClient, Option<Gateway>, IpAddr) = match gateway_ip {
        Some(ip) => {
            let client = DiagnosticClient::connect(ip, &zgw_config)
                .await
                .map_err(|e| format!("connect to {ip} failed: {e}"))?;
            (client, None, ip)
        }
        None => {
            let (client, gateway) = DiagnosticClient::discover_and_connect(
                config.bind,
                config.broadcast,
                config.discovery_wait(),
                &zgw_config,
            )
            .await
            .map_err(|e| format!("gateway discovery/connect failed: {e}"))?;
            let ip = gateway.ip;
            (client, Some(gateway), ip)
        }
    };

    // Authoritative VIN via DID F190; fall back to discovery's best-effort VIN.
    let did_vin = match client.read_did(DID_VIN).await {
        Ok((_, raw)) => klartext_semantic::did::decode(DID_VIN, &raw).text,
        Err(error) => {
            tracing::warn!(%error, "VIN read via DID F190 failed; trying discovery VIN");
            None
        }
    };
    let discovery_vin = gateway.as_ref().and_then(|g| g.vin.clone());
    let (vin, vin_source) = match (did_vin, discovery_vin) {
        (Some(v), _) => (Some(v), VinSource::DidF190),
        (None, Some(v)) => (Some(v), VinSource::Discovery),
        (None, None) => (None, VinSource::Unknown),
    };

    Ok(Connection {
        gateway_ip: ip,
        vin,
        vin_source,
        target: ZGW_ADDRESS,
        client,
    })
}

/// Ensure the held connection targets `address`, reconnecting if it differs.
///
/// Reuses the warm session when the target matches; otherwise drops it (which
/// aborts its keepalive on `Drop`) and opens a fresh client to the same gateway.
///
/// # Errors
/// Returns a human message if the reconnect to `address` fails.
pub async fn ensure_target(
    conn: &mut Connection,
    config: &ServerConfig,
    address: u8,
) -> Result<(), String> {
    if conn.target == address {
        return Ok(());
    }
    let client = DiagnosticClient::connect(conn.gateway_ip, &config.client_config(address))
        .await
        .map_err(|e| format!("reconnect to ECU 0x{address:02X} failed: {e}"))?;
    conn.client = client;
    conn.target = address;
    Ok(())
}
