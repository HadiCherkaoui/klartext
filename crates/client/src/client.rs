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
    ALL_DTC_RECORDS, ALL_DTC_STATUS_MASK, CLEAR_ALL_DTCS, Dtc, DtcRecordRegion, DtcSeverity,
    P2_STAR_SERVER_MAX_DEFAULT_MS, clear_diagnostic_information, decode_dtc_extended_data,
    decode_dtc_severity, decode_dtc_snapshot, decode_dtcs, decode_read_data_by_identifier,
    read_data_by_identifier, read_dtc_by_status_mask, read_dtc_extended_data_by_dtc,
    read_dtc_severity_by_dtc, read_dtc_snapshot_by_dtc, session, sid, tester_present,
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
/// diagnostic port, the tester source address, and the ISO default connect/read
/// timeouts. The target ECU is no longer part of the config — one connection
/// serves every ECU, and each request carries its own target address.
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// TCP diagnostic port (6801).
    pub port: u16,
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
            tester: TESTER_ADDRESS,
            connect_timeout: Duration::from_millis(CONNECT_TIMEOUT_DEFAULT_MS),
            read_timeout: Duration::from_millis(P2_STAR_SERVER_MAX_DEFAULT_MS),
        }
    }
}

/// The outcome of a presence probe against one ECU address.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The ECU answered — it is fitted and reachable.
    Present {
        /// True if it answered positively (`7E 00`); false if it answered with a
        /// negative response (which still proves presence).
        answered_positive: bool,
        /// How long the answer took.
        latency: Duration,
    },
    /// No answer within the probe timeout — treat as not fitted.
    Silent,
}

/// The raw freeze-frame reads for one fault: snapshot, extended data, severity.
///
/// The three UDS reads ISTA's `FS_LESEN_DETAIL` performs (`19 04`/`19 06`/`19 09`).
/// Each field is `None` when the ECU has no such record for the DTC (a negative
/// response, not an error). The regions are raw — decoding them into labeled fields
/// is the semantic layer's job (`klartext_semantic::snapshot`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultDetailRaw {
    /// The `59 04` snapshot record region, if the fault has one.
    pub snapshot: Option<DtcRecordRegion>,
    /// The `59 06` extended-data record region, if the fault has one.
    pub extended: Option<DtcRecordRegion>,
    /// The `59 09` severity information, if the ECU reports it.
    pub severity: Option<DtcSeverity>,
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
            // One connection serves every ECU; the keepalive targets the gateway.
            session: Session::open(conn, config.tester, ZGW_ADDRESS),
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

    /// Read DTCs from `target` whose status matches `mask` (0x19/0x02).
    ///
    /// # Errors
    /// As [`crate::Session::request`], plus [`ClientError::Uds`] if the response
    /// cannot be decoded.
    pub async fn read_dtcs(&self, target: u8, mask: u8) -> Result<Vec<Dtc>, ClientError> {
        let response = self
            .session
            .request(target, &read_dtc_by_status_mask(mask))
            .await?;
        Ok(decode_dtcs(&response)?)
    }

    /// Read every stored DTC from `target` (status mask 0xFF).
    ///
    /// # Errors
    /// As [`DiagnosticClient::read_dtcs`].
    pub async fn read_all_dtcs(&self, target: u8) -> Result<Vec<Dtc>, ClientError> {
        self.read_dtcs(target, ALL_DTC_STATUS_MASK).await
    }

    /// Read a fault's freeze-frame detail from `target`: snapshot, extended, severity.
    ///
    /// Issues the three ISTA `FS_LESEN_DETAIL` reads for one `dtc` — `19 04`
    /// (snapshot), `19 06` (extended data), `19 09` (severity) — each requesting all
    /// records (`0xFF`). A **negative** response to any one means the ECU has no such
    /// record for the fault and yields `None`, not an error. These are reads:
    /// autonomous-safe, no session or confirmation gate.
    ///
    /// The regions are raw; decode them with `klartext_semantic::snapshot`. The wire
    /// framing is DERIVED, pending an on-car capture — [verify against capture].
    ///
    /// # Errors
    /// As [`crate::Session::request`] on a transport error, and [`ClientError::Uds`]
    /// if a positive response cannot be decoded. A negative response is not an error.
    pub async fn read_fault_detail(
        &self,
        target: u8,
        dtc: [u8; 3],
    ) -> Result<FaultDetailRaw, ClientError> {
        let snapshot = self
            .request_optional(target, &read_dtc_snapshot_by_dtc(dtc, ALL_DTC_RECORDS))
            .await?
            .map(|resp| decode_dtc_snapshot(&resp))
            .transpose()?;
        let extended = self
            .request_optional(target, &read_dtc_extended_data_by_dtc(dtc, ALL_DTC_RECORDS))
            .await?
            .map(|resp| decode_dtc_extended_data(&resp))
            .transpose()?;
        let severity = self
            .request_optional(target, &read_dtc_severity_by_dtc(dtc))
            .await?
            .map(|resp| decode_dtc_severity(&resp))
            .transpose()?;
        Ok(FaultDetailRaw {
            snapshot,
            extended,
            severity,
        })
    }

    /// Send `request` to `target`, mapping a negative response to `None`.
    ///
    /// A negative response here means "the ECU has no such record" (e.g. no snapshot
    /// for this DTC) — a normal outcome for the freeze-frame reads, not an error.
    /// Transport and other errors still propagate.
    async fn request_optional(
        &self,
        target: u8,
        request: &[u8],
    ) -> Result<Option<Vec<u8>>, ClientError> {
        match self.session.request(target, request).await {
            Ok(response) => Ok(Some(response)),
            Err(ClientError::Negative { .. }) => Ok(None),
            Err(other) => Err(other),
        }
    }

    /// Read one data identifier from `target`, returning `(DID, raw value)` (0x22).
    ///
    /// The value is raw, unscaled bytes; naming and decoding are the semantic
    /// layer's job (`klartext-semantic`).
    ///
    /// # Errors
    /// As [`crate::Session::request`], plus [`ClientError::Uds`] if the response
    /// cannot be decoded, and [`ClientError::UnexpectedDid`] if the response echoes
    /// a different DID (a desynced stream — retry).
    pub async fn read_did(&self, target: u8, did: u16) -> Result<(u16, Vec<u8>), ClientError> {
        let response = self
            .session
            .request(target, &read_data_by_identifier(did))
            .await?;
        let (got, raw) = decode_read_data_by_identifier(&response)?;
        // Guard against a desynced stream (a late response to a prior, timed-out
        // request landing on this one): the echo must be the DID we asked for.
        if got != did {
            return Err(ClientError::UnexpectedDid {
                requested: did,
                got,
            });
        }
        Ok((got, raw))
    }

    /// Clear one DTC group on `target` — a state change; gate behind confirmation.
    ///
    /// Enters the extended session first, which BMW requires before a clear.
    ///
    /// # Errors
    /// As [`crate::Session::request`]; a rejected clear surfaces as
    /// [`ClientError::Negative`].
    pub async fn clear_dtcs(&self, target: u8, dtc: [u8; 3]) -> Result<(), ClientError> {
        self.session
            .enter_session(target, session::EXTENDED)
            .await?;
        self.session
            .request(target, &clear_diagnostic_information(dtc))
            .await?;
        Ok(())
    }

    /// Clear every DTC on `target` (`14 FF FF FF`) — gate behind confirmation.
    ///
    /// # Errors
    /// As [`DiagnosticClient::clear_dtcs`].
    pub async fn clear_all_dtcs(&self, target: u8) -> Result<(), ClientError> {
        self.clear_dtcs(target, CLEAR_ALL_DTCS).await
    }

    /// Send a TesterPresent to `target` and confirm the positive response.
    ///
    /// # Errors
    /// As [`crate::Session::request`].
    pub async fn tester_present(&self, target: u8) -> Result<(), ClientError> {
        self.session.request(target, &tester_present()).await?;
        Ok(())
    }

    /// Probe whether `target` is fitted, with a short per-probe `timeout`.
    ///
    /// Sends a side-effect-free TesterPresent (`3E 00`). *Any* answer — a positive
    /// `7E 00` or a negative response — proves the ECU is present and routing to it
    /// works; a read timeout means it is absent (or asleep). Only a fatal transport
    /// error (a closed connection) is returned as `Err`, so a whole-car scan can
    /// treat a `Silent` result as "not fitted" without aborting.
    ///
    /// # Errors
    /// [`ClientError::ConnectionClosed`] if the session's connection dropped.
    pub async fn probe(&self, target: u8, timeout: Duration) -> Result<ProbeOutcome, ClientError> {
        let started = tokio::time::Instant::now();
        match self
            .session
            .request_with_timeout(target, &tester_present(), timeout)
            .await
        {
            Ok(_) => Ok(ProbeOutcome::Present {
                answered_positive: true,
                latency: started.elapsed(),
            }),
            Err(ClientError::Negative { .. }) => Ok(ProbeOutcome::Present {
                answered_positive: false,
                latency: started.elapsed(),
            }),
            Err(ClientError::Hsfz(klartext_hsfz::Error::ReadTimeout { .. })) => {
                Ok(ProbeOutcome::Silent)
            }
            Err(ClientError::RequestInFlight { .. }) => Ok(ProbeOutcome::Silent),
            Err(other) => Err(other),
        }
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
        &self,
        target: u8,
        requests: &[Vec<u8>],
    ) -> Result<Vec<u8>, ClientError> {
        let mut value = None;
        for request in requests {
            let response = self.session.request(target, request).await?;
            // The 0x22 read carries the value (`62 F3 03 <raw>`); the 0x2C clear and
            // define steps only need to succeed — `Session::request` already errors
            // on a negative response.
            if request.first() == Some(&sid::READ_DATA_BY_IDENTIFIER) {
                let (got, raw) = decode_read_data_by_identifier(&response)?;
                // The dynamic DID is in the request bytes (`22 <hi> <lo>`); the
                // echo must match, or the stream is desynced (a late response).
                let requested = u16::from_be_bytes([request[1], request[2]]);
                if got != requested {
                    return Err(ClientError::UnexpectedDid { requested, got });
                }
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
        &self,
        target: u8,
        reset_request: &[u8],
        read_back_request: &[u8],
    ) -> Result<Vec<u8>, ClientError> {
        self.session
            .enter_session(target, session::EXTENDED)
            .await?;
        self.session.request(target, reset_request).await?;
        let response = self.session.request(target, read_back_request).await?;
        let (_did, block) = decode_read_data_by_identifier(&response)?;
        Ok(block)
    }

    /// Run a single-shot low-risk service reset, returning the ECU's positive response.
    ///
    /// A state change — the *decision* to run it must be gated behind explicit user
    /// confirmation by the caller (never autonomous, never over MCP). Enters the
    /// extended session BMW requires for a write, sends `request` (a derived `0x2E`
    /// write or `0x31` routine that resets a diagnostic counter/statistic), and
    /// returns the raw positive-response bytes so the caller can surface them.
    ///
    /// Unlike an actuator, a diagnostic-statistic reset is one-shot: it latches no
    /// component, so no return-control bracket is needed (contrast an actuation, which
    /// must always run its stop/return phase). `request` is a low-risk service
    /// function's derived frame — DERIVED from disassembly, not a capture, so the
    /// on-car effect is the real confirmation. [verify against capture].
    ///
    /// # Errors
    /// As [`crate::Session::request`] (a transport error, or a negative response to
    /// the session change or the reset).
    pub async fn run_service_reset(
        &self,
        target: u8,
        request: &[u8],
    ) -> Result<Vec<u8>, ClientError> {
        self.session
            .enter_session(target, session::EXTENDED)
            .await?;
        let response = self.session.request(target, request).await?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use klartext_hsfz::{HsfzFrame, control, read_frame, write_frame};
    use tokio::net::TcpListener;

    use super::*;

    /// The DDE address these mocks answer for; requests to other targets stay silent.
    const DDE: u8 = 0x12;

    /// Reply to `frame` from the ECU it addressed (swap SRC/TGT, as the gateway does).
    fn reply_from_ecu(frame: &HsfzFrame, uds: Vec<u8>) -> HsfzFrame {
        let (tester, ecu) = frame.addr.expect("diagnostic frame carries addresses");
        HsfzFrame::diagnostic(ecu, tester, uds)
    }

    /// A loopback DDE mock that answers `3E 00` and the dynamic-measurement `2C`/`22`
    /// sequence for engine temperature (id `0x4BC3`, u16) with raw `0E 2F`. Only the
    /// DDE (0x12) answers; any other target is silent (so a probe there times out).
    async fn spawn_dde_gateway() -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                let (_tester, ecu) = frame.addr.unwrap();
                if frame.payload == [0x3E, 0x80] || ecu != DDE {
                    continue; // keepalive, or an absent ECU — no reply
                }
                let uds = match frame.payload.as_slice() {
                    [0x3E, 0x00] => vec![0x7E, 0x00],
                    [0x2C, 0x03, 0xF3, 0x03] => vec![0x6C, 0x03, 0xF3, 0x03], // clear
                    [0x2C, 0x01, 0xF3, 0x03, 0x4B, 0xC3, 0x01, 0x02] => {
                        vec![0x6C, 0x01, 0xF3, 0x03]
                    } // define
                    [0x22, 0xF3, 0x03] => vec![0x62, 0xF3, 0x03, 0x0E, 0x2F], // read -> raw
                    _ => continue,
                };
                let _ = write_frame(&mut stream, &reply_from_ecu(&frame, uds)).await;
            }
        });
        addr
    }

    fn dde_client_config(addr: std::net::SocketAddr) -> ClientConfig {
        ClientConfig {
            port: addr.port(),
            ..ClientConfig::default()
        }
    }

    #[tokio::test]
    async fn read_dynamic_measurement_runs_clear_define_read() {
        let addr = spawn_dde_gateway().await;
        let client = DiagnosticClient::connect(addr.ip(), &dde_client_config(addr))
            .await
            .unwrap();

        // The derived DDE sequence (clear, define, read) for id 0x4BC3 (u16).
        let requests = vec![
            vec![0x2C, 0x03, 0xF3, 0x03],
            vec![0x2C, 0x01, 0xF3, 0x03, 0x4B, 0xC3, 0x01, 0x02],
            vec![0x22, 0xF3, 0x03],
        ];
        let raw = client
            .read_dynamic_measurement(DDE, &requests)
            .await
            .unwrap();
        // The value is the bytes after the `62 F3 03` echo — ready for scaling.
        assert_eq!(raw, vec![0x0E, 0x2F]);
    }

    /// A DDE mock for the freeze-frame reads. For DTC 24 00 00 it answers all three
    /// (19 04/06/09); for DTC DE AD 00 it rejects all three (7F 19 31 = no record).
    /// Frames are the DERIVED fixture, following the ISO 14229-1 record framing.
    async fn spawn_fault_detail_gateway() -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC || frame.payload == [0x3E, 0x80] {
                    continue;
                }
                let uds = match frame.payload.as_slice() {
                    // Snapshot: DTC + status, then record 01 with 1 identifier
                    // (coolant 0x5205 = 0x7B) — 59 04 24 00 00 08 01 01 52 05 7B.
                    [0x19, 0x04, 0x24, 0x00, 0x00, 0xFF] => {
                        vec![
                            0x59, 0x04, 0x24, 0x00, 0x00, 0x08, 0x01, 0x01, 0x52, 0x05, 0x7B,
                        ]
                    }
                    // Extended data: record 0x02 (HFK) = 0x1F.
                    [0x19, 0x06, 0x24, 0x00, 0x00, 0xFF] => {
                        vec![0x59, 0x06, 0x24, 0x00, 0x00, 0x08, 0x02, 0x1F]
                    }
                    // Severity: availMask FF, severity 20, funcUnit 10, DTC, status.
                    [0x19, 0x09, 0x24, 0x00, 0x00] => {
                        vec![0x59, 0x09, 0xFF, 0x20, 0x10, 0x24, 0x00, 0x00, 0x08]
                    }
                    // A fault with no stored detail rejects all three reads.
                    [0x19, 0x04, 0xDE, 0xAD, 0x00, 0xFF]
                    | [0x19, 0x06, 0xDE, 0xAD, 0x00, 0xFF]
                    | [0x19, 0x09, 0xDE, 0xAD, 0x00] => vec![0x7F, 0x19, 0x31],
                    _ => continue,
                };
                let _ = write_frame(&mut stream, &reply_from_ecu(&frame, uds)).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn read_fault_detail_reads_snapshot_extended_and_severity() {
        let addr = spawn_fault_detail_gateway().await;
        let client = DiagnosticClient::connect(addr.ip(), &dde_client_config(addr))
            .await
            .unwrap();

        let detail = client
            .read_fault_detail(DDE, [0x24, 0x00, 0x00])
            .await
            .unwrap();
        let snapshot = detail.snapshot.expect("snapshot present");
        assert_eq!(snapshot.dtc, [0x24, 0x00, 0x00]);
        assert_eq!(snapshot.status, 0x08);
        assert_eq!(snapshot.body, vec![0x01, 0x01, 0x52, 0x05, 0x7B]);
        assert_eq!(
            detail.extended.expect("extended present").body,
            vec![0x02, 0x1F]
        );
        assert_eq!(detail.severity.expect("severity present").severity, 0x20);
    }

    #[tokio::test]
    async fn read_fault_detail_maps_no_snapshot_to_none() {
        let addr = spawn_fault_detail_gateway().await;
        let client = DiagnosticClient::connect(addr.ip(), &dde_client_config(addr))
            .await
            .unwrap();

        // DTC DE AD 00: the ECU rejects every detail read — a normal "no record",
        // so the call succeeds with all three fields None (not an error).
        let detail = client
            .read_fault_detail(DDE, [0xDE, 0xAD, 0x00])
            .await
            .expect("a negative response is not an error");
        assert_eq!(detail.snapshot, None);
        assert_eq!(detail.extended, None);
        assert_eq!(detail.severity, None);
    }

    /// A gateway that echoes the WRONG DID: any `22 XX XX` gets `62 F1 90 …`. This
    /// models a desynced stream (a late response to a prior request landing here).
    async fn spawn_wrong_echo_gateway() -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC || frame.payload == [0x3E, 0x80] {
                    continue;
                }
                if frame.payload.first() == Some(&0x22) {
                    let uds = vec![0x62, 0xF1, 0x90, 0xAB]; // always echoes F190
                    let _ = write_frame(&mut stream, &reply_from_ecu(&frame, uds)).await;
                }
            }
        });
        addr
    }

    #[tokio::test]
    async fn read_did_rejects_a_mismatched_did_echo() {
        let addr = spawn_wrong_echo_gateway().await;
        let client = DiagnosticClient::connect(addr.ip(), &dde_client_config(addr))
            .await
            .unwrap();
        // Ask for 0xF405 but the gateway echoes 0xF190 — a desynced stream.
        let err = client.read_did(DDE, 0xF405).await.unwrap_err();
        assert!(
            matches!(
                err,
                ClientError::UnexpectedDid {
                    requested: 0xF405,
                    got: 0xF190
                }
            ),
            "expected UnexpectedDid, got {err:?}"
        );
    }

    #[tokio::test]
    async fn probe_reports_present_on_answer_and_silent_on_timeout() {
        let addr = spawn_dde_gateway().await;
        let client = DiagnosticClient::connect(addr.ip(), &dde_client_config(addr))
            .await
            .unwrap();

        let present = client.probe(DDE, Duration::from_millis(300)).await.unwrap();
        assert!(matches!(
            present,
            ProbeOutcome::Present {
                answered_positive: true,
                ..
            }
        ));

        // 0x18 is not fitted on this mock — the probe times out fast.
        let silent = client
            .probe(0x18, Duration::from_millis(150))
            .await
            .unwrap();
        assert!(matches!(silent, ProbeOutcome::Silent));
    }

    /// A loopback DDE mock for the CBS reset path: accepts the extended session,
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
                if frame.payload == [0x3E, 0x80] {
                    continue; // keepalive
                }
                let uds = match frame.payload.as_slice() {
                    [0x10, 0x03] => vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88], // extended session
                    [0x2E, 0x10, 0x01, ..] => vec![0x6E, 0x10, 0x01],         // CBS write ack
                    [0x22, 0x10, 0x01] => vec![0x62, 0x10, 0x01, 0x01, 0x64], // read-back
                    _ => continue,
                };
                let _ = write_frame(&mut stream, &reply_from_ecu(&frame, uds)).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn reset_cbs_enters_session_writes_then_reads_back() {
        let addr = spawn_cbs_gateway().await;
        let client = DiagnosticClient::connect(addr.ip(), &dde_client_config(addr))
            .await
            .unwrap();

        // The DERIVED engine-oil CBS_RESET write + the 22 10 01 read-back.
        let reset = vec![
            0x2E, 0x10, 0x01, 0x01, 0x01, 0x64, 0x1F, 0x80, 0x00, 0x0F, 0xFF, 0x0F, 0x3F, 0xFF,
            0x00,
        ];
        let read_back = [0x22, 0x10, 0x01];
        let block = client.reset_cbs(DDE, &reset, &read_back).await.unwrap();
        // The read-back block after the `62 10 01` echo: ANZ_CBS=1, oil availability 0x64.
        assert_eq!(block, vec![0x01, 0x64]);
    }

    /// A loopback mock for the generic reset path: accepts the extended session and
    /// echoes a single-shot statistic reset (`2E 5F 84` → `6E 5F 84`).
    async fn spawn_reset_gateway() -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                if frame.payload == [0x3E, 0x80] {
                    continue; // keepalive
                }
                let uds = match frame.payload.as_slice() {
                    [0x10, 0x03] => vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88], // extended session
                    [0x2E, 0x5F, 0x84] => vec![0x6E, 0x5F, 0x84],             // MSA2 history reset
                    _ => continue,
                };
                let _ = write_frame(&mut stream, &reply_from_ecu(&frame, uds)).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn run_service_reset_enters_session_then_sends_derived_frame() {
        let addr = spawn_reset_gateway().await;
        let client = DiagnosticClient::connect(addr.ip(), &dde_client_config(addr))
            .await
            .unwrap();

        // The DERIVED MSA2 statistic-reset frame (STEUERN_MSA2HISTORIERESET).
        let response = client
            .run_service_reset(DDE, &[0x2E, 0x5F, 0x84])
            .await
            .unwrap();
        // The ECU's positive response echoes the written DID.
        assert_eq!(response, vec![0x6E, 0x5F, 0x84]);
    }
}
