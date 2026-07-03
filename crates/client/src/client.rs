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
    read_data_by_identifier, read_dtc_by_status_mask, session, sid, tester_present,
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
    /// The value is raw, unscaled bytes; naming and decoding are the semantic
    /// layer's job (`klartext-semantic`).
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

    /// Read a dynamic (`SERVICE = "22;2C"`) measurement, returning its raw value.
    ///
    /// `requests` is the output of `klartext-semantic`'s `build_read_request`: the
    /// ordered UDS payloads (clear, define, read) for one DDE proprietary
    /// measurement. Each is sent in turn; the value is the `0x22` read's `62 ..`
    /// response with the 3-byte DID echo stripped, ready for scaling.
    ///
    /// Defining a dynamic DID is transient, session-scoped ECU state — not a stored
    /// write — so this stays an autonomous-safe read with no confirmation gate.
    ///
    /// # Errors
    /// As [`crate::Session::request`] (a transport error, or a negative response to
    /// the clear/define/read), [`ClientError::Uds`] if the read response cannot be
    /// decoded, and [`ClientError::NoMeasurementRead`] if `requests` has no `0x22`
    /// read step.
    pub async fn read_dynamic_measurement(
        &mut self,
        requests: &[Vec<u8>],
    ) -> Result<Vec<u8>, ClientError> {
        let mut value = None;
        for request in requests {
            let response = self.session.request(request).await?;
            // The 0x22 read carries the value (`62 F3 03 <raw>`); the 0x2C clear and
            // define steps only need to succeed — `Session::request` already errors
            // on a negative response.
            if request.first() == Some(&sid::READ_DATA_BY_IDENTIFIER) {
                let (_did, raw) = decode_read_data_by_identifier(&response)?;
                value = Some(raw);
            }
        }
        value.ok_or(ClientError::NoMeasurementRead)
    }

    /// Reset a Condition-Based-Service counter, then read the CBS block back.
    ///
    /// A state change — the *decision* to run it must be gated behind explicit user
    /// confirmation by the caller (it is never autonomous and never exposed over
    /// MCP). Enters the extended session BMW requires for a write, sends
    /// `reset_request` (a `0x2E` write to the CBS DID), then sends `read_back_request`
    /// (a `0x22` read of the same DID) and returns its raw block bytes so the caller
    /// can confirm the write landed.
    ///
    /// The requests come from `klartext-semantic`'s `build_cbs_reset_request` /
    /// `build_cbs_read_request`; their frames are DERIVED from the `CBS_RESET`
    /// disassembly, not a capture — [verify against capture].
    ///
    /// # Errors
    /// As [`crate::Session::request`] (a transport error, or a negative response to
    /// the session change, the write, or the read-back), and [`ClientError::Uds`] if
    /// the read-back response cannot be decoded.
    pub async fn reset_cbs(
        &mut self,
        reset_request: &[u8],
        read_back_request: &[u8],
    ) -> Result<Vec<u8>, ClientError> {
        self.session.enter_session(session::EXTENDED).await?;
        self.session.request(reset_request).await?;
        let response = self.session.request(read_back_request).await?;
        let (_did, block) = decode_read_data_by_identifier(&response)?;
        Ok(block)
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use klartext_hsfz::{HsfzFrame, control, read_frame, write_frame};
    use tokio::net::TcpListener;

    use super::*;

    /// A loopback DDE mock that answers the dynamic-measurement `2C`/`22` sequence
    /// for engine temperature (id `0x4BC3`, u16) with raw `0E 2F`.
    async fn spawn_dde_gateway() -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                let reply = match frame.payload.as_slice() {
                    [0x3E, 0x80] => continue, // keepalive — no reply
                    [0x2C, 0x03, 0xF3, 0x03] => vec![0x6C, 0x03, 0xF3, 0x03], // clear
                    [0x2C, 0x01, 0xF3, 0x03, 0x4B, 0xC3, 0x01, 0x02] => {
                        vec![0x6C, 0x01, 0xF3, 0x03] // define
                    }
                    [0x22, 0xF3, 0x03] => vec![0x62, 0xF3, 0x03, 0x0E, 0x2F], // read -> raw
                    _ => continue,
                };
                let reply = HsfzFrame::diagnostic(0x10, 0xF4, reply);
                let _ = write_frame(&mut stream, &reply).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn read_dynamic_measurement_runs_clear_define_read() {
        let addr = spawn_dde_gateway().await;
        let config = ClientConfig {
            port: addr.port(),
            ecu: 0x12,
            ..ClientConfig::default()
        };
        let mut client = DiagnosticClient::connect(addr.ip(), &config).await.unwrap();

        // The derived DDE sequence (clear, define, read) for id 0x4BC3 (u16).
        let requests = vec![
            vec![0x2C, 0x03, 0xF3, 0x03],
            vec![0x2C, 0x01, 0xF3, 0x03, 0x4B, 0xC3, 0x01, 0x02],
            vec![0x22, 0xF3, 0x03],
        ];
        let raw = client.read_dynamic_measurement(&requests).await.unwrap();
        // The value is the bytes after the `62 F3 03` echo — ready for scaling.
        assert_eq!(raw, vec![0x0E, 0x2F]);
    }

    /// A loopback DDE mock for the CBS reset path: it accepts the extended session,
    /// acknowledges the engine-oil CBS write (`6E 10 01`), and answers the read-back
    /// (`62 10 01 <ANZ_CBS> <block>`). Frames are the DERIVED CBS_RESET fixture.
    async fn spawn_cbs_gateway() -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                let reply = match frame.payload.as_slice() {
                    [0x3E, 0x80] => continue, // keepalive — no reply
                    [0x10, 0x03] => vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88], // extended session
                    // CBS write (engine oil) → positive 0x6E echo.
                    [0x2E, 0x10, 0x01, ..] => vec![0x6E, 0x10, 0x01],
                    // CBS read-back → 62 10 01, ANZ_CBS=1, then a one-byte block (oil 100 %).
                    [0x22, 0x10, 0x01] => vec![0x62, 0x10, 0x01, 0x01, 0x64],
                    _ => continue,
                };
                let reply = HsfzFrame::diagnostic(0x10, 0xF4, reply);
                let _ = write_frame(&mut stream, &reply).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn reset_cbs_enters_session_writes_then_reads_back() {
        let addr = spawn_cbs_gateway().await;
        let config = ClientConfig {
            port: addr.port(),
            ecu: 0x12,
            ..ClientConfig::default()
        };
        let mut client = DiagnosticClient::connect(addr.ip(), &config).await.unwrap();

        // The DERIVED engine-oil CBS_RESET write + the 22 10 01 read-back.
        let reset = vec![
            0x2E, 0x10, 0x01, 0x01, 0x01, 0x64, 0x1F, 0x80, 0x00, 0x0F, 0xFF, 0x0F, 0x3F, 0xFF,
            0x00,
        ];
        let read_back = [0x22, 0x10, 0x01];
        let block = client.reset_cbs(&reset, &read_back).await.unwrap();
        // The read-back block after the `62 10 01` echo: ANZ_CBS=1, oil availability 0x64.
        assert_eq!(block, vec![0x01, 0x64]);
    }
}
