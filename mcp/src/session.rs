//! The car connection held in server state: establish (discover/connect + VIN),
//! and a per-connection cache of the fitted-ECU scan.
//!
//! [`Connection`] wraps a [`DiagnosticClient`] (which demuxes every ECU over one
//! HSFZ connection and runs the background keepalive). [`establish`] opens the
//! session to the gateway and reads the VIN. Switching ECUs is free — each read
//! carries its own target address, so there is no retarget-reconnect. `Drop` on
//! the held client aborts the keepalive and closes the socket.

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

/// A live diagnostic connection; the client reaches any ECU by per-read target.
#[derive(Debug)]
pub struct Connection {
    /// The gateway IP the client is connected to.
    pub gateway_ip: IpAddr,
    /// The VIN, if one was obtained.
    pub vin: Option<String>,
    /// Where [`Connection::vin`] came from.
    pub vin_source: VinSource,
    /// The managed UDS session (demuxes every ECU, runs its own keepalive).
    pub client: DiagnosticClient,
    /// The fitted-ECU addresses from the last scan this session, if any.
    fitted: Option<Vec<u8>>,
}

impl Connection {
    /// The cached fitted-ECU list from a prior `scan_ecus`, if one ran.
    pub fn fitted(&self) -> Option<&[u8]> {
        self.fitted.as_deref()
    }

    /// Cache the fitted-ECU list from a scan (replacing any prior cache).
    pub fn set_fitted(&mut self, addresses: Vec<u8>) {
        self.fitted = Some(addresses);
    }
}

/// Shared, mutable server connection state. `None` = not connected.
pub type SessionState = Arc<Mutex<Option<Connection>>>;

/// Establish a session to the gateway (ZGW) and read the VIN.
///
/// `gateway_ip` takes the direct connect path; `None` auto-discovers on the link.
/// The returned [`Connection`] can reach any ECU by passing its target address.
///
/// # Errors
/// Returns a human message if discovery finds no (or several) gateways, or the TCP
/// connect fails.
pub async fn establish(
    config: &ServerConfig,
    gateway_ip: Option<IpAddr>,
) -> Result<Connection, String> {
    let client_config = config.client_config();
    let (client, gateway, ip): (DiagnosticClient, Option<Gateway>, IpAddr) = match gateway_ip {
        Some(ip) => {
            let client = DiagnosticClient::connect(ip, &client_config)
                .await
                .map_err(|e| format!("connect to {ip} failed: {e}"))?;
            (client, None, ip)
        }
        None => {
            let (client, gateway) = DiagnosticClient::discover_and_connect(
                config.bind,
                config.broadcast,
                config.discovery_wait(),
                &client_config,
            )
            .await
            .map_err(|e| format!("gateway discovery/connect failed: {e}"))?;
            let ip = gateway.ip;
            (client, Some(gateway), ip)
        }
    };

    // Authoritative VIN via DID F190 on the gateway; fall back to discovery's.
    let did_vin = match client.read_did(ZGW_ADDRESS, DID_VIN).await {
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
        client,
        fitted: None,
    })
}
