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
    EcuList, P2_STAR_SERVER_MAX_DEFAULT_MS, clear_diagnostic_information, decode_dtc_extended_data,
    decode_dtc_severity, decode_dtc_snapshot, decode_dtcs, decode_ecu_list,
    decode_read_data_by_identifier, read_data_by_identifier, read_dtc_by_status_mask,
    read_dtc_extended_data_by_dtc, read_dtc_severity_by_dtc, read_dtc_snapshot_by_dtc,
    service::did, session, sid, tester_present,
};

use crate::error::ClientError;
use crate::session::Session;

/// The link-local broadcast address discovery probes by default (report §2.5).
pub const DEFAULT_BROADCAST: Ipv4Addr = Ipv4Addr::new(169, 254, 255, 255);

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

/// The ISO-standardized identification DIDs (protocol-reference §1.5). The same
/// allocation for any UDS ECU, so reading this set stays generic across BMW; an
/// ECU serves only some of them, so a negative answer for one is normal (skipped).
pub const IDENTIFICATION_DIDS: [u16; 12] = [
    0xF190, // VIN
    0xF187, // vehicleManufacturerSparePartNumber
    0xF188, // vehicleManufacturerECUSoftwareNumber
    0xF189, // vehicleManufacturerECUSoftwareVersionNumber
    0xF191, // vehicleManufacturerECUHardwareNumber
    0xF192, // systemSupplierECUHardwareNumber
    0xF193, // systemSupplierECUHardwareVersionNumber
    0xF194, // systemSupplierECUSoftwareNumber
    0xF195, // systemSupplierECUSoftwareVersionNumber
    0xF197, // systemName
    0xF19E, // ASAMODXFileIdentifier
    0xF18C, // ECUSerialNumber
];

/// One identification DID's raw value from an ECU. Naming/text rendering is the
/// surface's job (`klartext_semantic::did::decode`), keeping the client protocol-pure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdField {
    /// The DID read.
    pub did: u16,
    /// The raw value bytes, exactly as the ECU returned them.
    pub raw: Vec<u8>,
}

/// One ECU's identification block: the standardized DIDs it actually served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcuIdentification {
    /// The ECU's diagnostic address.
    pub address: u8,
    /// The DIDs that answered (negatives are skipped).
    pub fields: Vec<IdField>,
}

/// The whole-vehicle identity: VIN, raw FA, I-Stufe, the SVT address list, and each
/// fitted ECU's identification block. FA decode and ECU naming happen at the surface
/// (semantic layer) so this stays DB-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VehicleIdentity {
    /// The vehicle VIN (gateway `22 F190`), if it answered.
    pub vin: Option<String>,
    /// The raw vehicle-order (FA) bytes (`22 3F06`); decode with the semantic layer.
    pub vehicle_order_raw: Vec<u8>,
    /// The integration level (`22 100B`), if it answered.
    pub i_stufe: Option<String>,
    /// The installed diagnostic addresses from the SVT (`22 3F07`).
    pub ecus: Vec<u8>,
    /// Each installed ECU's identification block.
    pub identification: Vec<EcuIdentification>,
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

    /// Sends a raw UDS request to `target` and returns the raw response payload.
    ///
    /// A thin passthrough to the managed [`Session`], exposing the one primitive the
    /// BEST/2 job engine's live exchange bridge needs without leaking the session type.
    ///
    /// # Errors
    /// As [`Session::request`].
    pub async fn request(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ClientError> {
        self.session.request(target, uds).await
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

    /// Read the gateway's installed-ECU list (the SVT) — UDS `22 3F 07` to the ZGW.
    ///
    /// Returns the diagnostic addresses the gateway reports as installed. Names are
    /// resolved separately from the semantic DB. This is the discovery source; there
    /// is no probe fallback. The response framing is DERIVED from the
    /// `STATUS_VCM_GET_ECU_LIST_ALL` disassembly — [verify against capture].
    ///
    /// # Errors
    /// As [`crate::Session::request`] (transport / negative), and [`ClientError::Uds`]
    /// if the positive response cannot be decoded.
    pub async fn read_ecu_list(&self) -> Result<EcuList, ClientError> {
        let (_did, data) = self.read_did(ZGW_ADDRESS, did::ECU_LIST_ALL).await?;
        Ok(decode_ecu_list(&data)?)
    }

    /// Read the gateway's integration level (I-Stufe) — UDS `22 10 0B`.
    ///
    /// Returns the current integration level (e.g. `F025-23-07-530`), or `None` if the
    /// gateway rejects the DID or the payload is unparsable. The VCM packs the value as
    /// binary records (see [`decode_i_stufe`]); the response layout is confirmed against
    /// a car capture (2026-07-10).
    ///
    /// # Errors
    /// As [`crate::Session::request`] on a transport error, and [`ClientError::Uds`]
    /// if a positive response cannot be decoded. A negative response is not an error
    /// (it yields `None`).
    pub async fn read_i_stufe(&self) -> Result<Option<String>, ClientError> {
        let Some(resp) = self
            .request_optional(ZGW_ADDRESS, &read_data_by_identifier(did::I_STUFE))
            .await?
        else {
            return Ok(None);
        };
        let (_did, raw) = decode_read_data_by_identifier(&resp)?;
        Ok(decode_i_stufe(&raw))
    }

    /// Read the raw vehicle order (FA) bytes — UDS `22 3F 06`.
    ///
    /// Returns the raw FA region; field decode is the semantic layer's job
    /// (`klartext_semantic::decode_vehicle_order`). Framing DERIVED — [verify against capture].
    ///
    /// # Errors
    /// As [`DiagnosticClient::read_did`]: a transport error, [`ClientError::Uds`] if the
    /// response cannot be decoded, or [`ClientError::UnexpectedDid`] on a mismatched echo.
    pub async fn read_vehicle_order(&self) -> Result<Vec<u8>, ClientError> {
        let (_did, raw) = self.read_did(ZGW_ADDRESS, did::VEHICLE_ORDER).await?;
        Ok(raw)
    }

    /// Read one ECU's identification block: the standardized DIDs it serves (raw).
    ///
    /// Issues each of [`IDENTIFICATION_DIDS`] to `target`; a DID the ECU does not
    /// serve answers negatively and is skipped (not an error). Returns raw bytes —
    /// naming/text is the surface's job (`klartext_semantic::did::decode`).
    ///
    /// # Errors
    /// As [`crate::Session::request`] on a transport error, and [`ClientError::Uds`]
    /// if a positive response cannot be decoded. A DID answered negatively (or with a
    /// mismatched echo) is skipped, not an error.
    pub async fn read_ecu_identification(
        &self,
        target: u8,
    ) -> Result<EcuIdentification, ClientError> {
        let mut fields = Vec::new();
        for did in IDENTIFICATION_DIDS {
            let Some(resp) = self
                .request_optional(target, &read_data_by_identifier(did))
                .await?
            else {
                continue; // ECU does not serve this DID
            };
            let (got, raw) = decode_read_data_by_identifier(&resp)?;
            if got != did {
                continue; // desynced echo; skip rather than mislabel
            }
            fields.push(IdField { did, raw });
        }
        Ok(EcuIdentification {
            address: target,
            fields,
        })
    }

    /// Read the whole vehicle identity: SVT list, per-ECU identification, VIN, FA, I-Stufe.
    ///
    /// All autonomous-safe `0x22` reads. The SVT list is the discovery source (no
    /// probe). A per-ECU identification failure is recorded as an empty block for that
    /// ECU rather than aborting the whole read.
    ///
    /// # Errors
    /// [`ClientError`] if the SVT read itself fails (there is no probe fallback); a
    /// missing VIN / FA / I-Stufe degrades to `None`/empty, not an error.
    pub async fn identify_vehicle(&self) -> Result<VehicleIdentity, ClientError> {
        let list = self.read_ecu_list().await?;
        let vin = match self.read_did(ZGW_ADDRESS, did::VIN).await {
            Ok((_, raw)) => String::from_utf8(raw).ok().filter(|s| !s.is_empty()),
            Err(ClientError::Negative { .. }) => None,
            Err(e) => return Err(e),
        };
        let i_stufe = self.read_i_stufe().await?;
        let vehicle_order_raw = match self.read_vehicle_order().await {
            Ok(raw) => raw,
            Err(ClientError::Negative { .. }) => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut identification = Vec::with_capacity(list.addresses.len());
        for &address in &list.addresses {
            let block = self
                .read_ecu_identification(address)
                .await
                .unwrap_or(EcuIdentification {
                    address,
                    fields: Vec::new(),
                });
            identification.push(block);
        }
        Ok(VehicleIdentity {
            vin,
            vehicle_order_raw,
            i_stufe,
            ecus: list.addresses,
            identification,
        })
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

/// Decode a gateway I-Stufe payload to the current integration level.
///
/// The VCM (`62 100B`) answers with 8-byte records — 4 ASCII series chars, a binary
/// year and month, then a big-endian `u16` patch — ordered current, previous, factory.
/// This returns the current (first) record formatted `SERIES-YY-MM-PPP` (e.g.
/// `F025-23-07-530`), or `None` when the payload is shorter than one record or its
/// series field is not printable ASCII.
///
/// The layout is confirmed against a car capture (2026-07-10); the DID was previously
/// documented as a plain ASCII string, which no F-series VCM actually sends.
fn decode_i_stufe(raw: &[u8]) -> Option<String> {
    // One record: [series: 4 ASCII bytes][year: u8][month: u8][patch: u16 big-endian].
    let record = raw.get(..8)?;
    let series = &record[..4];
    if !series.iter().all(u8::is_ascii_graphic) {
        return None;
    }
    let series = std::str::from_utf8(series).ok()?;
    let (year, month) = (record[4], record[5]);
    let patch = u16::from_be_bytes([record[6], record[7]]);
    Some(format!("{series}-{year:02}-{month:02}-{patch:03}"))
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
    /// DDE (0x12) answers; any other target is silent.
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

    /// Connect a [`DiagnosticClient`] to a loopback mock listening at `addr`.
    async fn client(addr: std::net::SocketAddr) -> DiagnosticClient {
        DiagnosticClient::connect(addr.ip(), &dde_client_config(addr))
            .await
            .unwrap()
    }

    /// A loopback gateway answering a `(target, request) -> response` table.
    ///
    /// A frame matches an entry only when BOTH its addressed ECU and its request
    /// payload equal the entry's; the reply swaps SRC/TGT via [`reply_from_ecu`],
    /// as the real gateway does. An unmatched request to a target that *does*
    /// appear in the table is rejected with `7F 22 31` (requestOutOfRange), so the
    /// identification negative-skip path is exercised; a request to a target absent
    /// from the table stays silent. Keyed by target so multi-ECU tests share it.
    async fn spawn_gateway_multi(table: &[(u8, Vec<u8>, Vec<u8>)]) -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let table: Vec<(u8, Vec<u8>, Vec<u8>)> = table.to_vec();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC || frame.payload == [0x3E, 0x80] {
                    continue;
                }
                let (_tester, ecu) = frame.addr.unwrap();
                let reply = if let Some((_, _, resp)) = table
                    .iter()
                    .find(|(t, req, _)| *t == ecu && *req == frame.payload)
                {
                    resp.clone()
                } else if table.iter().any(|(t, _, _)| *t == ecu) {
                    vec![0x7F, 0x22, 0x31] // known target, unserved DID: requestOutOfRange
                } else {
                    continue; // unknown target: silence
                };
                let _ = write_frame(&mut stream, &reply_from_ecu(&frame, reply)).await;
            }
        });
        addr
    }

    #[tokio::test]
    async fn read_ecu_list_decodes_svt_addresses() {
        // Gateway at 0x10 answers 22 3F 07 with: 62 3F 07 | count 0x0003 | 10 12 40
        let addr = spawn_gateway_multi(&[(
            ZGW_ADDRESS,
            vec![0x22, 0x3F, 0x07],
            vec![0x62, 0x3F, 0x07, 0x00, 0x03, 0x10, 0x12, 0x40],
        )])
        .await;
        let client = client(addr).await;
        let list = client.read_ecu_list().await.unwrap();
        assert_eq!(list.count, 3);
        assert_eq!(list.addresses, vec![0x10, 0x12, 0x40]);
    }

    #[tokio::test]
    async fn read_i_stufe_decodes_the_binary_vcm_record() {
        // 22 100B -> 62 100B + one 8-byte record: "F020" + year 21 (0x15) + month 11
        // (0x0B) + patch 500 (0x01F4, big-endian). The VCM packs it binary, not ASCII —
        // these bytes are not valid UTF-8, which is why the old from_utf8 decode
        // returned null on the real car.
        let mut resp = vec![0x62, 0x10, 0x0B];
        resp.extend_from_slice(&[0x46, 0x30, 0x32, 0x30, 0x15, 0x0B, 0x01, 0xF4]);
        let addr = spawn_gateway_multi(&[(ZGW_ADDRESS, vec![0x22, 0x10, 0x0B], resp)]).await;
        let client = client(addr).await;
        assert_eq!(
            client.read_i_stufe().await.unwrap().as_deref(),
            Some("F020-21-11-500")
        );
    }

    #[test]
    fn decode_i_stufe_takes_the_current_record_and_degrades_safely() {
        // Three 8-byte records (current, previous, factory) + a trailing block; the
        // current level is the first record. Series stays a generic model code.
        let raw = [
            0x46, 0x30, 0x32, 0x35, 0x17, 0x07, 0x02, 0x12, // F025-23-07-530 (current)
            0x46, 0x30, 0x32, 0x35, 0x14, 0x07, 0x02, 0x1C, // F025-20-07-540 (previous)
            0x00, 0x14, 0x6E, 0x0E, // trailing non-record bytes
        ];
        assert_eq!(decode_i_stufe(&raw).as_deref(), Some("F025-23-07-530"));
        // Shorter than one record, or a non-printable series, degrades to None.
        assert!(decode_i_stufe(&[0x46, 0x30, 0x32]).is_none());
        assert!(decode_i_stufe(&[0x00, 0x14, 0x6E, 0x0E, 0x01, 0x02, 0x03, 0x04]).is_none());
    }

    #[tokio::test]
    async fn read_ecu_identification_collects_answered_dids_and_skips_negatives() {
        // The ECU answers F190 (VIN) and F197 (system name) but rejects the rest.
        let mut vin = vec![0x62, 0xF1, 0x90];
        vin.extend_from_slice(b"WBA1K2C50EV000000");
        let mut sysname = vec![0x62, 0xF1, 0x97];
        sysname.extend_from_slice(b"DDE");
        let addr = spawn_gateway_multi(&[
            (0x12, vec![0x22, 0xF1, 0x90], vin),
            (0x12, vec![0x22, 0xF1, 0x97], sysname),
        ])
        .await;
        let client = client(addr).await;
        let ident = client.read_ecu_identification(0x12).await.unwrap();
        assert_eq!(ident.address, 0x12);
        // Only the two answered DIDs are present (negatives skipped); raw bytes only —
        // naming is the surface's job (did::decode), not the client's.
        let vin_field = ident.fields.iter().find(|f| f.did == 0xF190).unwrap();
        assert_eq!(vin_field.raw, b"WBA1K2C50EV000000");
        assert!(ident.fields.iter().any(|f| f.did == 0xF197));
        assert!(!ident.fields.iter().any(|f| f.did == 0xF187)); // rejected -> skipped
    }

    #[tokio::test]
    async fn identify_vehicle_aggregates_svt_and_identification() {
        let mut vin = vec![0x62, 0xF1, 0x90];
        vin.extend_from_slice(b"WBA1K2C50EV000000");
        // Gateway 0x10: SVT lists 0x12 only; VIN; I-Stufe; FA raw.
        let addr = spawn_gateway_multi(&[
            (
                0x10,
                vec![0x22, 0x3F, 0x07],
                vec![0x62, 0x3F, 0x07, 0x00, 0x01, 0x12],
            ),
            (0x10, vec![0x22, 0xF1, 0x90], vin.clone()),
            (0x10, vec![0x22, 0x10, 0x0B], {
                let mut r = vec![0x62, 0x10, 0x0B];
                // "F020" + year 21 + month 11 + patch 500, binary-packed (real VCM format).
                r.extend_from_slice(&[0x46, 0x30, 0x32, 0x30, 0x15, 0x0B, 0x01, 0xF4]);
                r
            }),
            (
                0x10,
                vec![0x22, 0x3F, 0x06],
                vec![0x62, 0x3F, 0x06, 0xAA, 0xBB],
            ),
            (0x12, vec![0x22, 0xF1, 0x90], vin),
        ])
        .await;
        let client = client(addr).await;
        let id = client.identify_vehicle().await.unwrap();
        assert_eq!(id.vin.as_deref(), Some("WBA1K2C50EV000000"));
        assert_eq!(id.ecus, vec![0x12]);
        assert_eq!(id.i_stufe.as_deref(), Some("F020-21-11-500"));
        assert_eq!(id.vehicle_order_raw, vec![0xAA, 0xBB]);
        assert_eq!(id.identification.len(), 1);
        assert_eq!(id.identification[0].address, 0x12);
    }

    #[tokio::test]
    async fn request_forwards_bare_uds_to_the_session() {
        // `request` is a raw passthrough: a `22 F1 90` read to the DDE returns the
        // ECU's response bytes unchanged (no decode, no DID-echo validation here).
        let mut vin = vec![0x62, 0xF1, 0x90];
        vin.extend_from_slice(b"WBA1K2C50EV000000");
        let addr = spawn_gateway_multi(&[(DDE, vec![0x22, 0xF1, 0x90], vin.clone())]).await;
        let client = client(addr).await;
        let response = client.request(DDE, &[0x22, 0xF1, 0x90]).await.unwrap();
        assert_eq!(response, vin);
    }
}
