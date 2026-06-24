//! The diagnostic client: connect/discover orchestration and typed read/clear.
//!
//! [`DiagnosticClient`] is the entry point the CLI (and the future MCP server)
//! drive. It connects — by auto-discovery ([`DiagnosticClient::discover_and_connect`],
//! the default) or directly to a known IP ([`DiagnosticClient::connect`], the
//! fallback) — opens a managed [`crate::Session`], and exposes the M2 services:
//! [`read_dtcs`](DiagnosticClient::read_dtcs),
//! [`read_did`](DiagnosticClient::read_did), and the confirmation-gated
//! [`clear_dtcs`](DiagnosticClient::clear_dtcs).
//!
//! Reads are autonomous-safe. Clearing DTCs is a state change: this layer enters
//! the extended session and issues the clear, but the *decision* to clear must be
//! gated behind explicit user confirmation by the caller.

use std::net::{IpAddr, Ipv4Addr};
use std::time::Duration;

use klartext_hsfz::{
    CONNECT_TIMEOUT_DEFAULT_MS, CONTROL_PORT, DIAG_PORT, Gateway, HsfzConnection, TESTER_ADDRESS,
    ZGW_ADDRESS, discover, link_local_bind_ip,
};
use klartext_uds::{
    ALL_DTC_STATUS_MASK, CLEAR_ALL_DTCS, Dtc, P2_STAR_SERVER_MAX_DEFAULT_MS,
    clear_diagnostic_information, decode_dtcs, decode_read_data_by_identifier,
    read_data_by_identifier, read_dtc_by_status_mask, session, tester_present,
};

use crate::error::ClientError;
use crate::session::Session;

/// The link-local broadcast address discovery probes by default (report §2.5).
pub const DEFAULT_BROADCAST: Ipv4Addr = Ipv4Addr::new(169, 254, 255, 255);

/// Default time to listen for discovery replies.
pub const DEFAULT_DISCOVERY_WAIT: Duration = Duration::from_millis(2000);

/// Connection settings shared by the direct and discovery connect paths.
///
/// [`ClientConfig::default`] uses the report's conventional values: the HSFZ
/// diagnostic port, the ZGW as the target ECU, the tester source address, and the
/// ISO default connect/read timeouts. Override `ecu` to target a module behind the
/// gateway.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// TCP diagnostic port (6801).
    pub port: u16,
    /// Target ECU logical address behind the ZGW.
    pub ecu: u8,
    /// Tester (source) logical address.
    pub tester: u8,
    /// TCP connect timeout.
    pub connect_timeout: Duration,
    /// Per-read timeout (P2*).
    pub read_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DIAG_PORT,
            ecu: ZGW_ADDRESS,
            tester: TESTER_ADDRESS,
            connect_timeout: Duration::from_millis(CONNECT_TIMEOUT_DEFAULT_MS),
            read_timeout: Duration::from_millis(P2_STAR_SERVER_MAX_DEFAULT_MS),
        }
    }
}

/// A connected diagnostic client over a managed UDS session.
#[derive(Debug)]
pub struct DiagnosticClient {
    session: Session,
}

impl DiagnosticClient {
    /// Connect directly to a known gateway IP, skipping discovery (M1 fallback).
    ///
    /// # Errors
    /// Returns [`ClientError::Hsfz`] if the TCP connection cannot be established.
    pub async fn connect(ip: IpAddr, config: &ClientConfig) -> Result<Self, ClientError> {
        let conn =
            HsfzConnection::connect(ip, config.port, config.connect_timeout, config.read_timeout)
                .await?;
        Ok(Self {
            session: Session::open(conn, config.tester, config.ecu),
        })
    }

    /// Auto-discover the gateway on the link, then connect (the default path).
    ///
    /// `bind` overrides the link-local source IP to broadcast from; `None`
    /// auto-detects it. Returns the connected client and the discovered
    /// [`Gateway`] (for its IP and best-effort VIN).
    ///
    /// # Errors
    /// Returns [`ClientError::NoLinkLocalInterface`] if no bind address is given
    /// and none can be detected, [`ClientError::NoGatewayFound`] if nothing
    /// answers, [`ClientError::AmbiguousGateway`] if several do, and
    /// [`ClientError::Hsfz`] on a discovery or connect failure.
    pub async fn discover_and_connect(
        bind: Option<Ipv4Addr>,
        broadcast: Ipv4Addr,
        discovery_wait: Duration,
        config: &ClientConfig,
    ) -> Result<(Self, Gateway), ClientError> {
        let bind_ip = match bind {
            Some(ip) => ip,
            None => link_local_bind_ip().ok_or(ClientError::NoLinkLocalInterface)?,
        };
        let mut gateways = discover(bind_ip, broadcast, CONTROL_PORT, discovery_wait).await?;
        let gateway = match gateways.len() {
            0 => return Err(ClientError::NoGatewayFound { bind_ip }),
            1 => gateways.remove(0),
            count => return Err(ClientError::AmbiguousGateway { count }),
        };
        let client = Self::connect(gateway.ip, config).await?;
        Ok((client, gateway))
    }

    /// Read DTCs whose status matches `mask` (ReadDTCInformation 0x19/0x02).
    ///
    /// # Errors
    /// As [`crate::Session::request`], plus [`ClientError::Uds`] if the response
    /// cannot be decoded.
    pub async fn read_dtcs(&mut self, mask: u8) -> Result<Vec<Dtc>, ClientError> {
        let response = self.session.request(&read_dtc_by_status_mask(mask)).await?;
        Ok(decode_dtcs(&response)?)
    }

    /// Read every stored DTC (status mask 0xFF).
    ///
    /// # Errors
    /// As [`DiagnosticClient::read_dtcs`].
    pub async fn read_all_dtcs(&mut self) -> Result<Vec<Dtc>, ClientError> {
        self.read_dtcs(ALL_DTC_STATUS_MASK).await
    }

    /// Read one data identifier, returning `(DID, raw value)` (0x22).
    ///
    /// The value is raw, unscaled bytes — meaning is the next milestone.
    ///
    /// # Errors
    /// As [`crate::Session::request`], plus [`ClientError::Uds`] if the response
    /// cannot be decoded.
    pub async fn read_did(&mut self, did: u16) -> Result<(u16, Vec<u8>), ClientError> {
        let response = self.session.request(&read_data_by_identifier(did)).await?;
        Ok(decode_read_data_by_identifier(&response)?)
    }

    /// Clear one DTC group — a state change; gate behind explicit confirmation.
    ///
    /// Enters the extended session first, which BMW requires before a clear.
    ///
    /// # Errors
    /// As [`crate::Session::request`]; a rejected clear surfaces as
    /// [`ClientError::Negative`].
    pub async fn clear_dtcs(&mut self, dtc: [u8; 3]) -> Result<(), ClientError> {
        self.session.enter_session(session::EXTENDED).await?;
        self.session
            .request(&clear_diagnostic_information(dtc))
            .await?;
        Ok(())
    }

    /// Clear every DTC (`14 FF FF FF`) — a state change; gate behind confirmation.
    ///
    /// # Errors
    /// As [`DiagnosticClient::clear_dtcs`].
    pub async fn clear_all_dtcs(&mut self) -> Result<(), ClientError> {
        self.clear_dtcs(CLEAR_ALL_DTCS).await
    }

    /// Send a TesterPresent and confirm the positive response (connectivity check).
    ///
    /// # Errors
    /// As [`crate::Session::request`].
    pub async fn tester_present(&mut self) -> Result<(), ClientError> {
        self.session.request(&tester_present()).await?;
        Ok(())
    }
}
