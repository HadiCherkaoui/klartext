//! The diagnostic client: connect/discover orchestration and typed read/clear.
//!
//! [`DiagnosticClient`] is the entry point the MCP server drives today, with a
//! future mobile app planned. It connects — by auto-discovery
//! ([`DiagnosticClient::discover_and_connect`], the default) or directly to a
//! known IP ([`DiagnosticClient::connect`], the fallback) — opens a managed
//! [`crate::Session`], and exposes the M2 services:
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
    CONNECT_TIMEOUT_DEFAULT_MS, CONTROL_PORT, DIAG_PORT, Gateway, HsfzConnection,
    READ_TIMEOUT_DEFAULT_MS, TESTER_ADDRESS, ZGW_ADDRESS, discover, link_local_bind_ip,
};
use klartext_uds::{
    ALL_DTC_RECORDS, CLEAR_ALL_DTCS, Dtc, DtcRecordRegion, DtcSeverity, EcuList,
    FUNCTIONAL_ADDRESS_F01, ISTA_DTC_STATUS_MASK, InfoMemory, clear_diagnostic_information,
    decode_dtc_extended_data, decode_dtc_severity, decode_dtc_snapshot, decode_dtcs,
    decode_ecu_list, decode_info_memory, decode_read_data_by_identifier, read_data_by_identifier,
    read_dtc_by_status_mask, read_dtc_extended_data_by_dtc, read_dtc_severity_by_dtc,
    read_dtc_snapshot_by_dtc, routine_control,
    service::{clamp, did, routine_subfn},
    session, sid, tester_present,
};

use crate::error::ClientError;
use crate::session::{MAX_BROADCAST_RESPONDERS, Session};

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
    /// How long an ECU has to answer a request at all — ISTA's ENET
    /// `TimeoutFunction`, not ISO P2* (see [`READ_TIMEOUT_DEFAULT_MS`]). Once an
    /// ECU answers NRC 0x78 the session re-arms on P2* instead.
    pub read_timeout: Duration,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            port: DIAG_PORT,
            tester: TESTER_ADDRESS,
            connect_timeout: Duration::from_millis(CONNECT_TIMEOUT_DEFAULT_MS),
            read_timeout: Duration::from_millis(READ_TIMEOUT_DEFAULT_MS),
        }
    }
}

/// The raw freeze-frame reads for one fault: snapshot, extended data, severity.
///
/// The three UDS reads ISTA's `FS_LESEN_DETAIL` performs, in its transmit order
/// `19 09`/`19 06`/`19 04`. Each field is `None` when the ECU has no such record for
/// the DTC (a negative response, not an error); `severity` is additionally `None`
/// when the `19 09` read was skipped because the ECU's SGBD declares no severity
/// (`F_SEVERITY = nein`). The regions are raw — decoding them into labeled fields is
/// the semantic layer's job (`klartext_semantic::snapshot`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultDetailRaw {
    /// The `59 04` snapshot record region, if the fault has one.
    pub snapshot: Option<DtcRecordRegion>,
    /// The `59 06` extended-data record region, if the fault has one.
    pub extended: Option<DtcRecordRegion>,
    /// The `59 09` severity information, if the ECU reports it.
    pub severity: Option<DtcSeverity>,
}

/// One ECU's fault memory and info memory, read together the way ISTA reads them.
///
/// ISTA never reads the `19 02` fault memory without also reading the `22 2000`
/// info memory (Infospeicher) and merging the two into one per-ECU list with a
/// type discriminator (`VehicleIdent.cs:3535`, research §A.6). This bundles those
/// two cheap BASE reads — one `FS_LESEN` and one `IS_LESEN`, exactly one request
/// each — but NOT the per-fault freeze-frame detail, which is `2 ×` the fault count
/// in requests and stays opt-in in
/// [`read_fault_detail`](DiagnosticClient::read_fault_detail) (research §E.1).
///
/// Fault and info entries stay in separate fields rather than pre-merged: both
/// reuse [`Dtc`] as the record shape, and a surface tags each with its origin
/// (ISTA's `EcuDTCType` `"F"`/`"I"`, `klartext_uds::FaultSource`) when it presents
/// them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcuFaultBundle {
    /// The `19 02 0C` fault-memory DTCs (`FS_LESEN`).
    pub faults: Vec<Dtc>,
    /// The `22 2000` info-memory entries (`IS_LESEN`); empty when unsupported.
    pub info: Vec<Dtc>,
    /// Whether the ECU keeps an info memory at all.
    ///
    /// `false` is the NORMAL case, not an error: only 342/1405 ECUs document
    /// `22 2000`, and one without it answers a negative response, degraded here
    /// rather than surfaced as an error (research §F.3).
    pub info_supported: bool,
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

/// The routine identifier that clears the gateway's combined fault store (ZFS).
///
/// ISTA's `STEUERN_ZFS_LOESCHEN`; read off `zgw_01.prg` offset `000003`,
/// `move S1, [31 01 40 00 FF]`. See
/// [`clear_gateway_combined_store`](DiagnosticClient::clear_gateway_combined_store).
const ZFS_CLEAR_ROUTINE: u16 = 0x4000;

/// The single value byte the ZFS-clear routine carries — **`0x00`, not the `0xFF`
/// the bytecode literal shows.**
///
/// The `FF` at `zgw_01.prg` offset `000003` is a *placeholder* the job overwrites
/// before it transmits. Reading the literal alone gets this wrong, which is exactly
/// the trap the parity mandate names ("the DB says what EXISTS; the binary says what
/// ISTA DOES WITH IT"); klartext shipped `0xFF` from 2026-07-18 until this was
/// disassembled to its `xsend`.
///
/// The store is unambiguous — `zgw_01.prg / STEUERN_ZFS_LOESCHEN`, offsets `000000`
/// through `000031`:
/// ```text
/// 000003 move   S1, [31 01 40 00 FF]   ; the template, placeholder tail
/// 000010 move   S2, S1                 ; working copy
/// 000014 move   L0, #4  / push L0      ; the index
/// 00001E move   L0, #0  / push L0      ; the value
/// 000028 pop    L0                     ; L0 = 0   (value)
/// 00002B pop    L1                     ; L1 = 4   (index)
/// 000031 move   S2[L1], B0             ; S2[4] = 0 — B0 is L0's low byte
/// ```
/// Running the job in klartext's own BEST/2 VM confirms it end to end: the emitted
/// telegram is `85 10 F1 | 31 01 40 00 00`, invariant under the job argument (ISTA
/// passes `string.Empty`). The same VM run over the same `.prg` emits `22 3F 07` for
/// `STATUS_VCM_GET_ECU_LIST_ALL` — a frame klartext has confirmed on the car — so the
/// VM is faithful here, not patching a byte of its own accord.
const ZFS_CLEAR_VALUE: u8 = 0x00;

/// The Car Access System's diagnostic address — rung 2 of [`VIN_LADDER`].
const CAS_ADDRESS: u8 = 0x40;

/// The Footwell Module's diagnostic address — rung 3 of [`VIN_LADDER`].
const FRM_ADDRESS: u8 = 0x72;

/// The ECUs ISTA reads the VIN from, in order; the first non-empty answer wins.
///
/// ISTA's BN2020 ladder — both cars are BN2020 — is `G_ZGW` → `G_CAS` → `G_FRM`
/// (`DiagnosticsBusinessData.ReadVinForGroupCars`, decompiled at
/// `DiagnosticsBusinessData.decompiled.cs:1907-1954`, walked by `ReadVinFromEcus`
/// at `:1998-2023`). The SGBD group names map to these addresses in ISTA's own
/// bordnet topology (the semantic DB's `ecu_tree`: `G_ZGW` → 0x10, `G_CAS` → 0x40,
/// `G_FRM` → 0x72 on both the F20 and the F25).
///
/// All three rungs put the SAME request on the wire, `22 F1 90`, so one read
/// serves every rung — verified by disassembling the shipped SGBDs with klartext's
/// own tooling: `zgw_01.prg/STATUS_VIN_LESEN`,
/// `cas4_2.prg/STATUS_FAHRGESTELLNUMMER` and `rem_20.prg/STATUS_VCM_VIN` (the only
/// shipped SGBD carrying the `G_FRM` rung's job) each move the literal
/// `22 F1 90` into the send register.
pub const VIN_LADDER: [u8; 3] = [ZGW_ADDRESS, CAS_ADDRESS, FRM_ADDRESS];

/// The "no VIN programmed" placeholder an ECU can answer with.
///
/// ISTA rejects it and falls through to the next rung (`ReadVinFromEcus`,
/// `DiagnosticsBusinessData.decompiled.cs:2010`). It is a value the car really
/// produces, not a defensive guess: the same literal is compiled into the F25's
/// own CAS4 SGBD (`cas4_2.prg/STATUS_FAHRGESTELLNUMMER`).
const VIN_SENTINEL: &str = "00000000000000000";

/// The outcome of checking that the car on the other end is still the same car.
///
/// Three outcomes, deliberately not a bool: ISTA distinguishes "a different car"
/// from "could not read a VIN", and the consequences differ — a silent ZGW is not
/// a swapped vehicle. In ISTA a mismatch is a hard abort that tears the session
/// down (`VciConnLossVM.cs:40-53` → `DisconnectDeviceOverLossConnection`), while
/// an unreadable VIN sends no response at all: `CompareSessionVinToEcuJobVin`
/// only assigns inside `if (!string.IsNullOrEmpty(vin))`, so its `BoolResultObject`
/// keeps the default `Result=false, ErrorCode=null` and the reconnect prompt just
/// stays open for the human to try again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VinCheck {
    /// The VIN read matches the one the session was established with.
    Match,
    /// A VIN was read and it is a different vehicle. ISTA aborts and disconnects.
    Mismatch {
        /// The VIN the session was established with.
        expected: String,
        /// The VIN just read from the car.
        found: String,
    },
    /// No VIN could be read, or it was too short to compare. Not a mismatch —
    /// nothing has been proven about which car is on the other end.
    Unreadable,
}

/// Compare a freshly read VIN against the one the session was established with.
///
/// The pure half of [`DiagnosticClient::verify_vin`], split out so a caller that
/// has already read the VIN this session (the MCP `connect` path) can run the
/// check without a second read.
///
/// The comparison is ISTA's connection-loss comparator: **full 17 characters,
/// ordinal, case-SENSITIVE, no trimming** (`CompareSessionVinToEcuJobVin`,
/// `RheingoldSessionController.decompiled.cs:10168-10188`, spec §C.3 — cited there
/// as `Logic.cs:5981-6001`).
///
/// **Do not "fix" this to be case-insensitive.** ISTA has a *second*, different
/// VIN comparator — `VehicleIdent.DoVehicleCheck` (`VehicleIdent.cs:6695`, `:6706`)
/// — which IS case-insensitive and has a last-7-characters fallback. That one is
/// not on this path: the connection-loss flow calls `HandleVCI` directly. The two
/// are not interchangeable, and the strict one is the correct choice here.
/// `Diagnostics.CheckCrossReconnectAllowed` (`Diagnostics.cs:62-86`), a
/// license-gated bypass that makes `DoVehicleCheck` return "same vehicle"
/// regardless of a mismatch, is deliberately NOT ported.
pub fn compare_vin(expected: &str, found: Option<&str>) -> VinCheck {
    let Some(found) = found else {
        return VinCheck::Unreadable;
    };
    // A 7-character read is a short VIN. ISTA expands it to 17 before comparing,
    // via a backend service call (`SVMDProcessorImpl.ResolveVIN7ToVIN17`). klartext
    // has no backend, so it cannot expand one — and comparing a short VIN against a
    // long one would report "different car" for a car that may well match. Say
    // "unreadable" instead, which is what it is.
    if found.chars().count() == 7 {
        return VinCheck::Unreadable;
    }
    if found == expected {
        VinCheck::Match
    } else {
        VinCheck::Mismatch {
            expected: expected.to_string(),
            found: found.to_string(),
        }
    }
}

/// Decode a `22 F190` payload into VIN text, or `None` if it carries none.
///
/// Same filter the semantic layer's `did::decode` applies, so the VIN string a
/// surface reports is unchanged by which path read it: valid UTF-8, non-empty, and
/// free of control characters (an ECU pads an unset VIN with NULs).
fn decode_vin(raw: &[u8]) -> Option<String> {
    std::str::from_utf8(raw)
        .ok()
        .filter(|s| !s.is_empty() && s.chars().all(|c| !c.is_control()))
        .map(str::to_owned)
}

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

/// The vehicle's three integration levels from `22 100B` — current, previous,
/// factory. The factory level carries the construction date (the value ISTA's
/// dated per-platform splits key on); the later records degrade to `None` on a
/// short payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IStufeLevels {
    /// The current integration level (e.g. `F025-23-07-530`).
    pub current: String,
    /// The previous level, if the record is present.
    pub previous: Option<String>,
    /// The factory (construction-time) level, if the record is present.
    pub factory: Option<String>,
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

    /// Read `target`'s faults exactly as ISTA does — `19 02 0C`, pending|confirmed.
    ///
    /// Sends [`ISTA_DTC_STATUS_MASK`], the hard literal every BMW `FS_LESEN` job
    /// transmits. **The ECU therefore does the filtering**, and klartext applies no
    /// status filter of its own; every returned DTC is surfaced, as ISTA surfaces
    /// every one it receives. This read used `0xFF` and then filtered client-side
    /// through an invented `RELEVANT_MASK` until 2026-07-18 (parity audit P0.2/P0.3).
    ///
    /// Returns strictly fewer faults than the old `0xFF` read: an entry whose test
    /// merely has not run this cycle is no longer returned by the ECU at all. Use
    /// [`DiagnosticClient::read_dtcs`] with [`ALL_DTC_STATUS_MASK`] to see those.
    ///
    /// # Errors
    /// As [`DiagnosticClient::read_dtcs`].
    pub async fn read_all_dtcs(&self, target: u8) -> Result<Vec<Dtc>, ClientError> {
        self.read_dtcs(target, ISTA_DTC_STATUS_MASK).await
    }

    /// Read `target`'s secondary/info memory (Infospeicher) — UDS `22 2000`.
    ///
    /// A store distinct from the `19 02` fault memory that ISTA shows alongside
    /// faults (job `IS_LESEN`). Returns `None` if the ECU rejects the DID (not every
    /// ECU keeps one). The response record layout is DERIVED from the DDE SGBD —
    /// [verify against capture]; [`InfoMemory`] keeps the raw payload for the on-car
    /// capture, and each entry's location code decodes as a fault code would.
    ///
    /// # Errors
    /// As [`crate::Session::request`] on a transport error, and [`ClientError::Uds`]
    /// if a positive response cannot be decoded. A negative response is not an error
    /// (it yields `None`).
    pub async fn read_info_memory(&self, target: u8) -> Result<Option<InfoMemory>, ClientError> {
        let Some(resp) = self
            .request_optional(target, &read_data_by_identifier(did::INFO_MEMORY))
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(decode_info_memory(&resp)?))
    }

    /// Read `target`'s fault memory and info memory together, as ISTA does.
    ///
    /// Performs EXACTLY the two cheap base reads ISTA never separates — one
    /// [`read_all_dtcs`](Self::read_all_dtcs) (`19 02 0C`, `FS_LESEN`) and one
    /// [`read_info_memory`](Self::read_info_memory) (`22 2000`, `IS_LESEN`) — and
    /// returns them as an [`EcuFaultBundle`] (research §A.6/§E.3). The per-fault
    /// freeze-frame detail is deliberately NOT bundled: it is unbounded (`2 ×` the
    /// fault count in requests) and stays opt-in in
    /// [`read_fault_detail`](Self::read_fault_detail) (research §E.1).
    ///
    /// An info-memory negative response — the ECU keeps no info memory, the NORMAL
    /// case for all but 342/1405 ECUs — degrades to
    /// [`info_supported`](EcuFaultBundle::info_supported)` = false`, never an error
    /// (research §F.3). This preserves [`read_info_memory`](Self::read_info_memory)'s
    /// own negative→`None` handling exactly. Both reads are autonomous-safe.
    ///
    /// # Errors
    /// As [`read_all_dtcs`](Self::read_all_dtcs) for the fault read — a fault-read
    /// failure propagates, since there is then nothing to report. The OPTIONAL info
    /// read never propagates: see below.
    pub async fn read_ecu_faults(&self, target: u8) -> Result<EcuFaultBundle, ClientError> {
        let faults = self.read_all_dtcs(target).await?;
        // The info read is OPTIONAL and secondary. Both a clean negative (the ECU
        // keeps no info memory — the normal case, only 342/1405 ECUs do) AND a read
        // FAILURE (an ECU that answered `19 02` but ignores `22 2000` and times out
        // rather than NAKing) degrade to `info_supported = false`. Losing a
        // successfully-read fault list because the optional info read was flaky would
        // be worse than surfacing the faults with info unavailable — and the owner's
        // cars time out (car session 1). Research §E.3/§F.3 specced the negative;
        // extending the same intent to a timeout keeps the primary data. The fault
        // read above still propagates, so a genuinely dead ECU is not masked.
        let (info, info_supported) = match self.read_info_memory(target).await {
            Ok(Some(memory)) => (memory.entries, true),
            Ok(None) | Err(_) => (Vec::new(), false),
        };
        Ok(EcuFaultBundle {
            faults,
            info,
            info_supported,
        })
    }

    /// Read a fault's freeze-frame detail from `target`: severity, extended, snapshot.
    ///
    /// Issues ISTA's `FS_LESEN_DETAIL` reads for one `dtc` in ISTA's transmit order —
    /// `19 09` (severity), `19 06` (extended data), `19 04` (snapshot) — each
    /// requesting all records (`0xFF`) (`fs_lesen_detail.txt` @
    /// `000179`/`001ABD`/`003FB4`). A **negative** response to any one means the ECU
    /// has no such record for the fault and yields `None`, not an error. These are
    /// reads: autonomous-safe, no session or confirmation gate.
    ///
    /// `severity_supported` gates the `19 09` read exactly as ISTA does: it is skipped
    /// entirely when the ECU's SGBD `FDetailStruktur` table declares
    /// `F_SEVERITY = nein` — `Some(false)`, the case for `d72n47a0`, where klartext
    /// otherwise sends a request ISTA never sends. `Some(true)` sends it; `None` (the
    /// caller could not read the `.prg` — BYO-data absent) falls back to sending it,
    /// since a wrong guess costs one negative round trip, not a failure. The caller
    /// resolves this from the SGBD; `klartext-client` does not depend on
    /// `klartext-sgbd`. The analogous `F_UWB_ERW` (`19 06`) / `F_UWB_SATZ` (`19 04`)
    /// gates are a follow-up.
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
        severity_supported: Option<bool>,
    ) -> Result<FaultDetailRaw, ClientError> {
        // Skip 19 09 only when the SGBD explicitly says F_SEVERITY = nein; an unknown
        // gate (None) still sends it, matching ISTA's send-and-tolerate fallback.
        let severity = if severity_supported == Some(false) {
            None
        } else {
            self.request_optional(target, &read_dtc_severity_by_dtc(dtc))
                .await?
                .map(|resp| decode_dtc_severity(&resp))
                .transpose()?
        };
        let extended = self
            .request_optional(target, &read_dtc_extended_data_by_dtc(dtc, ALL_DTC_RECORDS))
            .await?
            .map(|resp| decode_dtc_extended_data(&resp))
            .transpose()?;
        let snapshot = self
            .request_optional(target, &read_dtc_snapshot_by_dtc(dtc, ALL_DTC_RECORDS))
            .await?
            .map(|resp| decode_dtc_snapshot(&resp))
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

    /// Read the vehicle's VIN, walking ISTA's ECU ladder — ZGW, then CAS, then FRM.
    ///
    /// The ladder is [`VIN_LADDER`] and the request is `22 F1 90` on every rung,
    /// byte-identical to ISTA's. First non-empty answer wins; a rung that errors,
    /// answers negatively, decodes to nothing, or returns the all-zero
    /// [`VIN_SENTINEL`] falls through to the next one. That a rung failed is not a
    /// failure of the ladder, so this returns `Option`, not `Result`: `None` means
    /// no ECU on the ladder had a VIN, exactly as ISTA's `ReadVinFromEcus` returns
    /// null after swallowing each rung's exception
    /// (`DiagnosticsBusinessData.decompiled.cs:1998-2023`).
    ///
    /// The ladder runs **once**. ISTA's surrounding loop is
    /// `for (int i = 1; i <= retries; i++)` with `retries = RetryCount = 1`
    /// (`VehicleIdent.cs:420`), so a whole-ladder retry would be a klartext
    /// invention.
    pub async fn read_vin(&self) -> Option<String> {
        for address in VIN_LADDER {
            let read = match self.read_did(address, did::VIN).await {
                Ok((_, raw)) => decode_vin(&raw),
                Err(error) => {
                    tracing::debug!(
                        %error,
                        address = format_args!("0x{address:02X}"),
                        "VIN rung did not answer; trying the next ECU"
                    );
                    None
                }
            };
            match read {
                // The all-zero placeholder means "this ECU has no VIN", not "the
                // car's VIN is zeros" — keep walking, as ISTA does.
                Some(vin) if vin != VIN_SENTINEL => return Some(vin),
                _ => continue,
            }
        }
        None
    }

    /// Check that the car on the other end is still the one `expected`.
    ///
    /// Reads the VIN via [`read_vin`](DiagnosticClient::read_vin) and compares it
    /// with [`compare_vin`], whose doc comment carries the (deliberate) choice of
    /// comparator. The three outcomes are distinct and must stay distinct —
    /// [`VinCheck::Unreadable`] is not a mismatch.
    ///
    /// ISTA runs this check when a lost connection is re-established
    /// (`CompareSessionVinToEcuJobVin`), not before every operation; klartext has
    /// no automatic reconnect, so this is the seam a re-connect path calls.
    pub async fn verify_vin(&self, expected: &str) -> VinCheck {
        compare_vin(expected, self.read_vin().await.as_deref())
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

    /// Read the gateway's actively-responding ECU list — UDS `22 3F08`.
    ///
    /// The present-and-answering subset of [`read_ecu_list`](Self::read_ecu_list) (the
    /// configured superset), same framing. Returns `None` if the gateway does not
    /// answer this DID (not every ZGW exposes it) — a truer "really there" signal than
    /// the configured list. Framing DERIVED from
    /// `STATUS_VCM_GET_ECU_LIST_ACTIVE_RESPONSE` — [verify against capture].
    ///
    /// # Errors
    /// As [`crate::Session::request`] on a transport error, and [`ClientError::Uds`]
    /// if a positive response cannot be decoded. A negative response yields `None`.
    pub async fn read_responding_ecu_list(&self) -> Result<Option<EcuList>, ClientError> {
        let Some(resp) = self
            .request_optional(ZGW_ADDRESS, &read_data_by_identifier(did::ECU_LIST_ACTIVE))
            .await?
        else {
            return Ok(None);
        };
        let (_did, data) = decode_read_data_by_identifier(&resp)?;
        Ok(Some(decode_ecu_list(&data)?))
    }

    /// Read the gateway's integration level (I-Stufe) — UDS `22 10 0B`.
    ///
    /// Returns the current integration level (e.g. `F025-23-07-530`), or `None` if the
    /// gateway rejects the DID or the payload is unparsable. The VCM packs the value as
    /// binary records (see [`decode_i_stufe_record`]); the response layout is confirmed against
    /// a car capture (2026-07-10).
    ///
    /// # Errors
    /// As [`crate::Session::request`] on a transport error, and [`ClientError::Uds`]
    /// if a positive response cannot be decoded. A negative response is not an error
    /// (it yields `None`).
    pub async fn read_i_stufe(&self) -> Result<Option<String>, ClientError> {
        Ok(self.read_i_stufe_levels().await?.map(|l| l.current))
    }

    /// Read all three integration levels — current, previous, factory (`22 10 0B`).
    ///
    /// The VCM answers with three 8-byte records in that order (see
    /// [`decode_i_stufe_record`]); the factory level carries the construction date, which
    /// is what ISTA's dated bordnet split keys on. Returns `None` if the gateway
    /// rejects the DID or not even the current record parses; the later records
    /// degrade to `None` individually on a short payload.
    ///
    /// # Errors
    /// As [`DiagnosticClient::read_i_stufe`].
    pub async fn read_i_stufe_levels(&self) -> Result<Option<IStufeLevels>, ClientError> {
        let Some(resp) = self
            .request_optional(ZGW_ADDRESS, &read_data_by_identifier(did::I_STUFE))
            .await?
        else {
            return Ok(None);
        };
        let (_did, raw) = decode_read_data_by_identifier(&resp)?;
        let Some(current) = decode_i_stufe_record(&raw, 0) else {
            return Ok(None);
        };
        Ok(Some(IStufeLevels {
            current,
            previous: decode_i_stufe_record(&raw, 1),
            factory: decode_i_stufe_record(&raw, 2),
        }))
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

    /// Clear every DTC on every ECU with ONE functional broadcast (`14 FF FF FF`).
    ///
    /// ISTA's `FS_LOESCHEN_FUNKTIONAL`: the first step of its clear phase
    /// (`VehicleIdent.DoECUClearFS`), which only then falls back to per-ECU physical
    /// clears for whatever stayed silent. Returns the address of every ECU that
    /// answered, in arrival order — an ECU missing from that list is a straggler for
    /// the caller to clear physically, which is exactly how ISTA picks its stragglers
    /// (it zeroes the fault state of every ECU that answered the broadcast, leaving
    /// the rest to the physical pass).
    ///
    /// **No extended-session prefix.** [`DiagnosticClient::clear_dtcs`] sends `10 03`
    /// first and that is on-car-confirmed for the *physical* clear, but the
    /// functional job does not: `f01.prg/FS_LOESCHEN_FUNKTIONAL` disassembles to a
    /// single `xsend` of the clear with no session control before it. Adding one
    /// would both diverge from ISTA and broadcast a session change to every ECU.
    ///
    /// The quiet period that ends collection is the connection's read timeout. ISTA's
    /// real inter-response timeout is not readable — it lives in the native
    /// `XEnet32/64.dll` — so this is klartext's own choice of an already-tuned value,
    /// not parity. [verify against capture]
    ///
    /// **Blast radius: this reaches every ECU on the car at once.** The frame is the
    /// same idempotent clear klartext already sends per ECU, but the radius is wider
    /// by definition — including ECUs klartext has never addressed. The caller must
    /// hold the human's explicit confirmation, and should pre-read the fault memories
    /// it is about to erase.
    ///
    /// # Errors
    /// As [`crate::Session::request_functional`]. No answer is NOT an error: an empty
    /// list means nobody answered the broadcast.
    pub async fn clear_all_dtcs_functional(&self) -> Result<Vec<u8>, ClientError> {
        let responders = self
            .session
            .request_functional(
                FUNCTIONAL_ADDRESS_F01,
                &clear_diagnostic_information(CLEAR_ALL_DTCS),
                self.session.read_timeout(),
                MAX_BROADCAST_RESPONDERS,
            )
            .await?;
        Ok(responders.into_iter().map(|(address, _)| address).collect())
    }

    /// Clear the gateway's own combined fault store (ZFS) — `31 01 40 00 FF`.
    ///
    /// ISTA's `STEUERN_ZFS_LOESCHEN`, the last wire step of its clear phase, gated on
    /// `G_ZGW` being present — it is on both cars, at [`ZGW_ADDRESS`]. The gateway
    /// keeps a central copy of the vehicle's faults that the per-ECU erase does not
    /// touch.
    ///
    /// The frame is pinned by RUNNING the shipped SGBD's job, not by reading its
    /// template: `zgw_01.prg`'s literal is `31 01 40 00 FF`, but the job overwrites
    /// that trailing placeholder with `0x00` before its `xsend` — see
    /// [`ZFS_CLEAR_VALUE`], which carries the disassembly. RoutineControl
    /// startRoutine, RID [`ZFS_CLEAR_ROUTINE`], one value byte. It is gateway-local,
    /// not a cross-ECU cascade: the routine emits a single telegram per protocol
    /// branch with no ECU-address iteration.
    ///
    /// Whether ISTA precedes it with a session change was not readable from the
    /// bytecode template, so klartext sends the pinned telegram alone.
    /// [verify against capture]
    ///
    /// **A `0x31` RoutineControl on the gateway** — a service write, and the
    /// highest-consequence single frame in the clear sequence (a wrong RID here lands
    /// on the ECU the whole session runs through). Gate behind explicit confirmation.
    ///
    /// # Errors
    /// As [`crate::Session::request`]; a rejected routine surfaces as
    /// [`ClientError::Negative`].
    pub async fn clear_gateway_combined_store(&self) -> Result<(), ClientError> {
        self.session
            .request(
                ZGW_ADDRESS,
                &routine_control(
                    routine_subfn::START_ROUTINE,
                    ZFS_CLEAR_ROUTINE,
                    &[ZFS_CLEAR_VALUE],
                ),
            )
            .await?;
        Ok(())
    }

    /// Perform ISTA's post-clear terminal-15 cycle: OFF → 15 s → ON.
    ///
    /// This is what resets the instrument cluster after an ISTA fault erase. ISTA
    /// runs it silently and unconditionally once the user has asked to clear
    /// (`ClearAndReadErrorInfoMemory` → `DoClampSwitch`, the
    /// `ABL-LIF-KLEMMENSTEUERUNG` module in automatic mode). klartext performs it in
    /// the same place, behind the same confirmation the clear already required.
    ///
    /// # This drops the car's terminal 15 for fifteen seconds
    ///
    /// The call therefore blocks for [`clamp::OFF_DURATION_MS`]. Callers on a
    /// request/response surface must expect a >15 s round trip.
    ///
    /// # Deliberate divergence from ISTA — the restore
    ///
    /// If the ON command fails, klartext retries it once before returning the error,
    /// and reports failure loudly. **ISTA has no such recovery**: zero
    /// `try`/`catch`/`finally` in its 1,552-line clamp module, and its own cancel
    /// path returns without restoring KL15. This changes nothing on the happy path
    /// and is recorded as an agreed divergence in the parity audit — it exists so a
    /// failure here does not knowingly leave a car unable to start. It is NOT a
    /// guarantee: if this process dies during the window, nothing raises KL15, and
    /// whether the CAS does so itself is ECU firmware behaviour no shipped artifact
    /// states.
    ///
    /// # Errors
    /// Returns the underlying error if the OFF command fails (KL15 was never
    /// dropped), or if the ON command fails twice (KL15 may still be DOWN — the
    /// error says so).
    pub async fn cycle_terminal_15(&self) -> Result<(), ClientError> {
        self.cycle_terminal_15_holding(Duration::from_millis(clamp::OFF_DURATION_MS))
            .await
    }

    /// [`cycle_terminal_15`](Self::cycle_terminal_15) with the hold made explicit.
    ///
    /// The seam exists so the tests can exercise the real OFF/ON sequence without
    /// waiting fifteen seconds. Production has exactly one legitimate hold —
    /// [`clamp::OFF_DURATION_MS`], the value ISTA passes as `IN_pause` — so the public
    /// method takes no argument and nothing outside this crate can shorten it.
    pub(crate) async fn cycle_terminal_15_holding(
        &self,
        hold: Duration,
    ) -> Result<(), ClientError> {
        // OFF first. A failure here is the SAFE failure: nothing was dropped.
        self.session
            .request(clamp::TARGET, &clamp::KL15_OFF)
            .await
            .map_err(|e| ClientError::ClampSwitch {
                phase: "off",
                restored: true,
                source: Box::new(e),
            })?;

        tokio::time::sleep(hold).await;

        // ON. Terminal 15 is DOWN until this succeeds, so it gets a second attempt
        // (the divergence documented above) before the error is surfaced.
        match self.session.request(clamp::TARGET, &clamp::KL15_ON).await {
            Ok(_) => Ok(()),
            Err(first) => match self.session.request(clamp::TARGET, &clamp::KL15_ON).await {
                Ok(_) => Ok(()),
                Err(_) => Err(ClientError::ClampSwitch {
                    phase: "on",
                    restored: false,
                    source: Box::new(first),
                }),
            },
        }
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

/// Decode the `index`th record of a gateway I-Stufe payload.
///
/// The VCM (`62 100B`) answers with 8-byte records — 4 ASCII series chars, a binary
/// year and month, then a big-endian `u16` patch — ordered current (0), previous (1),
/// factory (2). Returns the record formatted `SERIES-YY-MM-PPP` (e.g.
/// `F025-23-07-530`), or `None` when the payload is too short for the record or its
/// series field is not printable ASCII.
///
/// The layout is confirmed against a car capture (2026-07-10); the DID was previously
/// documented as a plain ASCII string, which no F-series VCM actually sends.
fn decode_i_stufe_record(raw: &[u8], index: usize) -> Option<String> {
    // One record: [series: 4 ASCII bytes][year: u8][month: u8][patch: u16 big-endian].
    let record = raw.get(index * 8..index * 8 + 8)?;
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
pub(crate) mod tests {
    use std::net::Ipv4Addr;
    use std::sync::{Arc, Mutex};
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
            .read_fault_detail(DDE, [0x24, 0x00, 0x00], Some(true))
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
        // so the call succeeds with all three fields None (not an error). `None`
        // severity support means the 19 09 read is still sent, then rejected.
        let detail = client
            .read_fault_detail(DDE, [0xDE, 0xAD, 0x00], None)
            .await
            .expect("a negative response is not an error");
        assert_eq!(detail.snapshot, None);
        assert_eq!(detail.extended, None);
        assert_eq!(detail.severity, None);
    }

    /// ISTA's `FS_LESEN_DETAIL` transmits `19 09` → `19 06` → `19 04`
    /// (`fs_lesen_detail.txt` @ `000179`/`001ABD`/`003FB4`). The recording mock's
    /// frame log proves the transmit order; reordering the sends back to the old
    /// `04`/`06`/`09` flips this census and fails the assertion.
    #[tokio::test]
    async fn read_fault_detail_issues_severity_then_extended_then_snapshot() {
        let dtc = [0x24, 0x00, 0x00];
        let (addr, log) = spawn_gateway_recording(&[
            (
                DDE,
                vec![0x19, 0x09, 0x24, 0x00, 0x00],
                vec![0x59, 0x09, 0xFF, 0x20, 0x10, 0x24, 0x00, 0x00, 0x08],
            ),
            (
                DDE,
                vec![0x19, 0x06, 0x24, 0x00, 0x00, 0xFF],
                vec![0x59, 0x06, 0x24, 0x00, 0x00, 0x08, 0x02, 0x1F],
            ),
            (
                DDE,
                vec![0x19, 0x04, 0x24, 0x00, 0x00, 0xFF],
                vec![
                    0x59, 0x04, 0x24, 0x00, 0x00, 0x08, 0x01, 0x01, 0x52, 0x05, 0x7B,
                ],
            ),
        ])
        .await;
        let client = DiagnosticClient::connect(addr.ip(), &dde_client_config(addr))
            .await
            .unwrap();

        // Some(true): severity is supported, so all three reads go out.
        let detail = client
            .read_fault_detail(DDE, dtc, Some(true))
            .await
            .unwrap();
        assert!(detail.severity.is_some());
        assert!(detail.extended.is_some());
        assert!(detail.snapshot.is_some());

        // The census: every 0x19 sub-function transmitted, in transmit order.
        let subfns: Vec<u8> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, payload)| payload.first() == Some(&0x19))
            .map(|(_, payload)| payload[1])
            .collect();
        assert_eq!(
            subfns,
            vec![0x09, 0x06, 0x04],
            "ISTA's FS_LESEN_DETAIL transmit order is 09 → 06 → 04"
        );
    }

    /// ISTA skips the `19 09` severity read entirely when the ECU's SGBD declares
    /// `F_SEVERITY = nein` (the DDE `d72n47a0`). `Some(false)` gates it: the frame
    /// census proves `19 09` is never transmitted, while `19 06`/`19 04` still are.
    /// Dropping the gate (always sending) makes `19 09` appear and fails this.
    #[tokio::test]
    async fn read_fault_detail_skips_severity_read_when_unsupported() {
        let dtc = [0x24, 0x00, 0x00];
        let (addr, log) = spawn_gateway_recording(&[
            // The DDE would answer 19 09 if asked (a clean negative here); the census
            // below asserts a gated client NEVER asks, so this entry stays unused.
            (
                DDE,
                vec![0x19, 0x09, 0x24, 0x00, 0x00],
                vec![0x7F, 0x19, 0x31],
            ),
            (
                DDE,
                vec![0x19, 0x06, 0x24, 0x00, 0x00, 0xFF],
                vec![0x59, 0x06, 0x24, 0x00, 0x00, 0x08, 0x02, 0x1F],
            ),
            (
                DDE,
                vec![0x19, 0x04, 0x24, 0x00, 0x00, 0xFF],
                vec![
                    0x59, 0x04, 0x24, 0x00, 0x00, 0x08, 0x01, 0x01, 0x52, 0x05, 0x7B,
                ],
            ),
        ])
        .await;
        let client = DiagnosticClient::connect(addr.ip(), &dde_client_config(addr))
            .await
            .unwrap();

        let detail = client
            .read_fault_detail(DDE, dtc, Some(false))
            .await
            .unwrap();
        // Skipped, not merely rejected: no severity value AND no frame on the wire.
        assert_eq!(detail.severity, None);
        let sent_severity = log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, payload)| payload.starts_with(&[0x19, 0x09]));
        assert!(
            !sent_severity,
            "19 09 must not be transmitted when F_SEVERITY=nein"
        );
        // The other two reads are unaffected.
        assert!(detail.extended.is_some());
        assert!(detail.snapshot.is_some());
    }

    /// When the caller cannot read the SGBD (`None` — BYO-data absent), the `19 09`
    /// read falls back to being sent, matching ISTA's tolerate-a-negative default: a
    /// wrong guess costs one round trip, not a failure. Gating `None` as "skip" would
    /// drop the severity value this proves is present.
    #[tokio::test]
    async fn read_fault_detail_sends_severity_read_when_support_unknown() {
        let dtc = [0x24, 0x00, 0x00];
        let (addr, log) = spawn_gateway_recording(&[
            (
                DDE,
                vec![0x19, 0x09, 0x24, 0x00, 0x00],
                vec![0x59, 0x09, 0xFF, 0x20, 0x10, 0x24, 0x00, 0x00, 0x08],
            ),
            (
                DDE,
                vec![0x19, 0x06, 0x24, 0x00, 0x00, 0xFF],
                vec![0x59, 0x06, 0x24, 0x00, 0x00, 0x08, 0x02, 0x1F],
            ),
            (
                DDE,
                vec![0x19, 0x04, 0x24, 0x00, 0x00, 0xFF],
                vec![
                    0x59, 0x04, 0x24, 0x00, 0x00, 0x08, 0x01, 0x01, 0x52, 0x05, 0x7B,
                ],
            ),
        ])
        .await;
        let client = DiagnosticClient::connect(addr.ip(), &dde_client_config(addr))
            .await
            .unwrap();

        let detail = client.read_fault_detail(DDE, dtc, None).await.unwrap();
        assert_eq!(
            detail.severity.expect("severity sent on fallback").severity,
            0x20
        );
        let sent_severity = log
            .lock()
            .unwrap()
            .iter()
            .any(|(_, payload)| payload.starts_with(&[0x19, 0x09]));
        assert!(
            sent_severity,
            "unknown severity support must fall back to sending 19 09"
        );
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
    ///
    /// `pub(crate)`: also reused by `scan::tests`.
    pub(crate) async fn spawn_gateway_multi(
        table: &[(u8, Vec<u8>, Vec<u8>)],
    ) -> std::net::SocketAddr {
        spawn_gateway_recording(table).await.0
    }

    /// Every `(target, payload)` a mock gateway saw, in transmit order.
    pub(crate) type FrameLog = Arc<Mutex<Vec<(u8, Vec<u8>)>>>;

    /// As [`spawn_gateway_multi`], but also returns every `(target, payload)` the
    /// client transmitted, in order.
    ///
    /// The log is what makes an absence assertion possible: a test can prove a
    /// frame was NEVER sent, which no request/response table can show on its own.
    /// TesterPresent keepalives (`3E 80`) are excluded, as they are from the reply
    /// path — they are session plumbing, not the operation under test.
    pub(crate) async fn spawn_gateway_recording(
        table: &[(u8, Vec<u8>, Vec<u8>)],
    ) -> (std::net::SocketAddr, FrameLog) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let table: Vec<(u8, Vec<u8>, Vec<u8>)> = table.to_vec();
        let log: FrameLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC || frame.payload == [0x3E, 0x80] {
                    continue;
                }
                let (_tester, ecu) = frame.addr.unwrap();
                sink.lock().unwrap().push((ecu, frame.payload.clone()));
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
        (addr, log)
    }

    /// A loopback gateway that fans ONE functional request out to several ECUs, and
    /// records every `(target, payload)` the client transmitted.
    ///
    /// A frame addressed to [`FUNCTIONAL_ADDRESS_F01`] draws one `54` reply per
    /// address in `responders`, each from that ECU's own source address — the shape
    /// a real broadcast has, where no reply's source is the address asked. Any other
    /// frame is recorded and left unanswered, so a test can prove it was never sent.
    async fn spawn_broadcast_gateway(responders: &[u8]) -> (std::net::SocketAddr, FrameLog) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let responders: Vec<u8> = responders.to_vec();
        let log: FrameLog = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC || frame.payload == [0x3E, 0x80] {
                    continue;
                }
                let (tester, target) = frame.addr.unwrap();
                sink.lock().unwrap().push((target, frame.payload.clone()));
                // A session control is answered, as a real ECU answers one. Nothing
                // here sends one — that is the point: the census test must fail
                // because an extra frame was TRANSMITTED, not merely because it went
                // unanswered and timed out.
                if frame.payload.first() == Some(&0x10) {
                    let ack = vec![0x50, frame.payload[1]];
                    let _ =
                        write_frame(&mut stream, &HsfzFrame::diagnostic(target, tester, ack)).await;
                    continue;
                }
                if target != FUNCTIONAL_ADDRESS_F01 {
                    continue; // physically addressed: recorded, unanswered
                }
                for &responder in &responders {
                    let reply = HsfzFrame::diagnostic(responder, tester, vec![0x54]);
                    let _ = write_frame(&mut stream, &reply).await;
                }
            }
        });
        (addr, log)
    }

    /// As [`dde_client_config`], with a short read timeout — which is also the quiet
    /// period a broadcast collects for, so a broadcast test ends in milliseconds.
    fn fast_client_config(addr: std::net::SocketAddr) -> ClientConfig {
        ClientConfig {
            port: addr.port(),
            read_timeout: Duration::from_millis(150),
            ..ClientConfig::default()
        }
    }

    // The functional clear is ONE broadcast frame and nothing else. The census is
    // the assertion: `10 03` must NOT appear. klartext's physical clear sends one
    // and that is on-car-confirmed, but `f01.prg/FS_LOESCHEN_FUNKTIONAL` emits a
    // single xsend with no session control — and a broadcast `10 03` would move
    // every ECU on the car into the extended session.
    #[tokio::test]
    async fn clear_all_dtcs_functional_broadcasts_one_clear_with_no_session_prefix() {
        let (addr, log) = spawn_broadcast_gateway(&[0x12, 0x40, 0x60]).await;
        let client = DiagnosticClient::connect(addr.ip(), &fast_client_config(addr))
            .await
            .unwrap();

        let answered = client.clear_all_dtcs_functional().await.unwrap();

        assert_eq!(
            answered,
            vec![0x12, 0x40, 0x60],
            "every ECU that answered the broadcast, by its own address"
        );
        // The target is asserted as the literal 0xDF, not via the constant, so this
        // also pins the functional address itself on the wire.
        assert_eq!(
            *log.lock().unwrap(),
            vec![(0xDF, vec![0x14, 0xFF, 0xFF, 0xFF])],
            "exactly one frame: the clear, functionally addressed, no session prefix"
        );
    }

    // The gateway ZFS clear is the highest-consequence single frame in the sequence
    // — a `0x31` RoutineControl on the ECU the whole session runs through — so its
    // bytes are pinned verbatim against what `zgw_01.prg / STEUERN_ZFS_LOESCHEN`
    // actually TRANSMITS: `31 01 40 00 00`. Note the tail is `00`, not the `FF` the
    // bytecode template shows — the job overwrites that placeholder (`move S2[L1],
    // B0` with L1=4, B0=0) before its `xsend`, and klartext shipped the literal's
    // `FF` until the job was run in its own VM. Asserting the emitted byte is the
    // whole point of this test.
    #[tokio::test]
    async fn clear_gateway_combined_store_sends_the_pinned_zfs_routine() {
        let (addr, log) = spawn_gateway_recording(&[(
            ZGW_ADDRESS,
            vec![0x31, 0x01, 0x40, 0x00, 0x00],
            vec![0x71, 0x01, 0x40, 0x00],
        )])
        .await;
        let client = DiagnosticClient::connect(addr.ip(), &fast_client_config(addr))
            .await
            .unwrap();

        // The census is asserted BEFORE the outcome, so a wrong byte fails on the
        // frame diff rather than on the timeout that a rejected routine produces.
        let outcome = client.clear_gateway_combined_store().await;

        assert_eq!(
            *log.lock().unwrap(),
            vec![(ZGW_ADDRESS, vec![0x31, 0x01, 0x40, 0x00, 0x00])],
            "one routine frame to the gateway, no session prefix"
        );
        outcome.expect("the gateway's positive response must surface as success");
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

    #[tokio::test]
    async fn read_info_memory_decodes_entries_and_handles_rejection() {
        // 22 2000 -> 62 2000 | C90D60 status 2F. No version byte: the layout is
        // confirmed from IS_LESEN bytecode (see `klartext_uds::InfoMemory`).
        let mut ok = vec![0x62, 0x20, 0x00];
        ok.extend_from_slice(&[0xC9, 0x0D, 0x60, 0x2F]);
        let addr = spawn_gateway_multi(&[(0x12, vec![0x22, 0x20, 0x00], ok)]).await;
        let c1 = client(addr).await;
        let info = c1.read_info_memory(0x12).await.unwrap().expect("some");
        assert_eq!(info.entries.len(), 1);
        assert_eq!(info.entries[0].code, [0xC9, 0x0D, 0x60]);
        assert_eq!(info.entries[0].status, 0x2F);

        // An ECU that rejects the DID (7F 22 31) yields None, not an error.
        let addr2 =
            spawn_gateway_multi(&[(0x40, vec![0x22, 0x20, 0x00], vec![0x7F, 0x22, 0x31])]).await;
        let c2 = client(addr2).await;
        assert!(c2.read_info_memory(0x40).await.unwrap().is_none());
    }

    /// The bundle issues BOTH base reads ISTA never separates — one `19 02 0C` fault
    /// read and one `22 2000` info read (research §A.6/§E.3). The frame census is the
    /// proof: dropping either read from `read_ecu_faults` removes its frame and fails
    /// this. A supported info memory also populates `info` and sets `info_supported`.
    #[tokio::test]
    async fn read_ecu_faults_issues_both_the_fault_and_info_reads() {
        let (addr, log) = spawn_gateway_recording(&[
            (
                DDE,
                vec![0x19, 0x02, 0x0C],
                vec![0x59, 0x02, 0x0C, 0x4A, 0x12, 0x34, 0x08],
            ),
            (
                DDE,
                vec![0x22, 0x20, 0x00],
                vec![0x62, 0x20, 0x00, 0xC9, 0x0D, 0x60, 0x2F],
            ),
        ])
        .await;
        let client = client(addr).await;

        let bundle = client.read_ecu_faults(DDE).await.unwrap();
        // One fault from 19 02, one info entry from 22 2000, and info is supported.
        assert_eq!(bundle.faults.len(), 1);
        assert_eq!(bundle.faults[0].code, [0x4A, 0x12, 0x34]);
        assert_eq!(bundle.info.len(), 1);
        assert_eq!(bundle.info[0].code, [0xC9, 0x0D, 0x60]);
        assert!(bundle.info_supported);

        // The census: both base reads were transmitted, neither omitted.
        let sent: Vec<Vec<u8>> = log.lock().unwrap().iter().map(|(_, p)| p.clone()).collect();
        assert!(
            sent.iter().any(|p| p.as_slice() == [0x19, 0x02, 0x0C]),
            "the fault read (19 02 0C) must be issued: {sent:02X?}"
        );
        assert!(
            sent.iter().any(|p| p.as_slice() == [0x22, 0x20, 0x00]),
            "the info read (22 2000) must be issued: {sent:02X?}"
        );
    }

    /// An info-memory NEGATIVE response is the NORMAL case (only 342/1405 ECUs keep
    /// one) and must degrade to `info_supported = false` with the faults still
    /// present — never an error (research §F.3). Mutating the `None` arm of
    /// `read_ecu_faults` to `return Err(..)` makes this fail: the call would error
    /// and the faults would be lost.
    #[tokio::test]
    async fn read_ecu_faults_degrades_an_info_negative_to_unsupported() {
        let addr = spawn_gateway_multi(&[
            (
                DDE,
                vec![0x19, 0x02, 0x0C],
                vec![0x59, 0x02, 0x0C, 0x4A, 0x12, 0x34, 0x08],
            ),
            (DDE, vec![0x22, 0x20, 0x00], vec![0x7F, 0x22, 0x31]),
        ])
        .await;
        let client = client(addr).await;

        let bundle = client
            .read_ecu_faults(DDE)
            .await
            .expect("an info-memory negative response is not an error");
        // The fault survives; the info memory is reported unsupported, not errored.
        assert_eq!(bundle.faults.len(), 1);
        assert!(bundle.info.is_empty());
        assert!(!bundle.info_supported);
    }

    #[tokio::test]
    async fn read_ecu_faults_keeps_the_faults_when_the_info_read_times_out() {
        // An ECU that answers `19 02` but IGNORES `22 2000` (no reply → a timeout,
        // not a clean NAK) must NOT sink the fault list it already returned. The
        // info read is optional; the faults are the primary data, and the owner's
        // cars time out (car session 1). This is the case the mock's NAK-on-unserved
        // cannot exercise, so it needs a bespoke loopback that stays silent on the
        // info read. A regression reverting the `Err(_)` degrade to `?` loses the
        // faults here.
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                // Answer only the fault read; stay SILENT on `22 2000` so it times out.
                if frame.payload == [0x19, 0x02, 0x0C] {
                    let uds = vec![0x59, 0x02, 0x0C, 0x4A, 0x12, 0x34, 0x08];
                    let _ = write_frame(&mut stream, &reply_from_ecu(&frame, uds)).await;
                }
            }
        });
        let config = ClientConfig {
            port: addr.port(),
            read_timeout: Duration::from_millis(150),
            ..ClientConfig::default()
        };
        let client = DiagnosticClient::connect(addr.ip(), &config).await.unwrap();

        let bundle = client
            .read_ecu_faults(DDE)
            .await
            .expect("a timed-out optional info read must not fail the bundle");
        assert_eq!(bundle.faults.len(), 1, "the fault read must survive");
        assert_eq!(bundle.faults[0].code, [0x4A, 0x12, 0x34]);
        assert!(bundle.info.is_empty());
        assert!(
            !bundle.info_supported,
            "a timed-out info read is unsupported"
        );
    }

    #[tokio::test]
    async fn read_responding_ecu_list_reads_3f08_and_handles_rejection() {
        // 22 3F08 -> 62 3F08 | count 2 | 0x12 0x40 (a subset of the configured list).
        let ok = vec![0x62, 0x3F, 0x08, 0x00, 0x02, 0x12, 0x40];
        let addr = spawn_gateway_multi(&[(ZGW_ADDRESS, vec![0x22, 0x3F, 0x08], ok)]).await;
        let c1 = client(addr).await;
        let list = c1.read_responding_ecu_list().await.unwrap().expect("some");
        assert_eq!(list.addresses, vec![0x12, 0x40]);
        // A gateway that does not answer 3F08 yields None, not an error.
        let addr2 =
            spawn_gateway_multi(&[(ZGW_ADDRESS, vec![0x22, 0x3F, 0x08], vec![0x7F, 0x22, 0x31])])
                .await;
        let c2 = client(addr2).await;
        assert!(c2.read_responding_ecu_list().await.unwrap().is_none());
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
        assert_eq!(
            decode_i_stufe_record(&raw, 0).as_deref(),
            Some("F025-23-07-530")
        );
        // Shorter than one record, or a non-printable series, degrades to None.
        assert!(decode_i_stufe_record(&[0x46, 0x30, 0x32], 0).is_none());
        assert!(
            decode_i_stufe_record(&[0x00, 0x14, 0x6E, 0x0E, 0x01, 0x02, 0x03, 0x04], 0).is_none()
        );
        // Indexed records: the previous level is the second 8-byte record; a
        // record past the payload end (the factory level here) degrades to None.
        assert_eq!(
            decode_i_stufe_record(&raw, 1).as_deref(),
            Some("F025-20-07-540")
        );
        assert!(decode_i_stufe_record(&raw, 2).is_none());
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

    /// The VIN request every rung of the ladder emits (`22 F1 90`).
    const VIN_REQUEST: [u8; 3] = [0x22, 0xF1, 0x90];

    /// A `62 F190 <vin>` positive response.
    fn vin_response(vin: &[u8]) -> Vec<u8> {
        let mut resp = VIN_REQUEST.to_vec();
        resp[0] = 0x62;
        resp.extend_from_slice(vin);
        resp
    }

    /// Every VIN request in the frame log, as the target address that received it.
    fn vin_rungs_asked(log: &FrameLog) -> Vec<u8> {
        log.lock()
            .unwrap()
            .iter()
            .filter(|(_, payload)| payload.as_slice() == VIN_REQUEST)
            .map(|(target, _)| *target)
            .collect()
    }

    #[tokio::test]
    async fn read_vin_falls_through_a_silent_rung_to_the_next_ecu() {
        // The ZGW is in the table but rejects the VIN (7F 22 31); the CAS answers.
        // The FRM must never be asked — the first non-empty answer ends the ladder.
        let (addr, log) = spawn_gateway_recording(&[
            (ZGW_ADDRESS, VIN_REQUEST.to_vec(), vec![0x7F, 0x22, 0x31]),
            (
                CAS_ADDRESS,
                VIN_REQUEST.to_vec(),
                vin_response(b"WBA1K2C50EV000000"),
            ),
            (
                FRM_ADDRESS,
                VIN_REQUEST.to_vec(),
                vin_response(b"NEVERREACHED00000"),
            ),
        ])
        .await;
        let client = client(addr).await;

        assert_eq!(
            client.read_vin().await.as_deref(),
            Some("WBA1K2C50EV000000")
        );
        assert_eq!(vin_rungs_asked(&log), vec![ZGW_ADDRESS, CAS_ADDRESS]);
    }

    #[tokio::test]
    async fn read_vin_rejects_the_all_zero_sentinel_and_keeps_walking() {
        // The ZGW answers, but with the "no VIN programmed" placeholder. ISTA treats
        // that as no answer at all and walks on; taking it would report a VIN of
        // seventeen zeros as the car's identity.
        let (addr, log) = spawn_gateway_recording(&[
            (
                ZGW_ADDRESS,
                VIN_REQUEST.to_vec(),
                vin_response(VIN_SENTINEL.as_bytes()),
            ),
            (
                CAS_ADDRESS,
                VIN_REQUEST.to_vec(),
                vin_response(b"WBA1K2C50EV000000"),
            ),
        ])
        .await;
        let client = client(addr).await;

        assert_eq!(
            client.read_vin().await.as_deref(),
            Some("WBA1K2C50EV000000")
        );
        assert_eq!(vin_rungs_asked(&log), vec![ZGW_ADDRESS, CAS_ADDRESS]);
    }

    #[tokio::test]
    async fn read_vin_walks_every_rung_once_then_gives_up() {
        // No rung has a VIN: the ladder yields None (not an error), and each ECU is
        // asked exactly once — ISTA's retries default to 1, so there is no second lap.
        let (addr, log) = spawn_gateway_recording(&[
            (ZGW_ADDRESS, VIN_REQUEST.to_vec(), vec![0x7F, 0x22, 0x31]),
            (CAS_ADDRESS, VIN_REQUEST.to_vec(), vec![0x7F, 0x22, 0x31]),
            (FRM_ADDRESS, VIN_REQUEST.to_vec(), vec![0x7F, 0x22, 0x31]),
        ])
        .await;
        let client = client(addr).await;

        assert_eq!(client.read_vin().await, None);
        assert_eq!(
            vin_rungs_asked(&log),
            vec![ZGW_ADDRESS, CAS_ADDRESS, FRM_ADDRESS]
        );
    }

    #[tokio::test]
    async fn verify_vin_reads_the_ladder_and_reports_the_outcome() {
        let (addr, _log) = spawn_gateway_recording(&[(
            ZGW_ADDRESS,
            VIN_REQUEST.to_vec(),
            vin_response(b"WBA1K2C50EV000000"),
        )])
        .await;
        let client = client(addr).await;

        assert_eq!(
            client.verify_vin("WBA1K2C50EV000000").await,
            VinCheck::Match
        );
        assert_eq!(
            client.verify_vin("WBA1K2C50EV999999").await,
            VinCheck::Mismatch {
                expected: "WBA1K2C50EV999999".to_string(),
                found: "WBA1K2C50EV000000".to_string(),
            }
        );
    }

    #[test]
    fn compare_vin_is_case_sensitive_and_does_not_trim() {
        // ISTA's connection-loss comparator is ordinal string equality on the full
        // 17 characters. The case-INsensitive comparator in ISTA (DoVehicleCheck) is
        // a different code path and must not be substituted here.
        let expected = "WBA1K2C50EV000000";
        assert_eq!(compare_vin(expected, Some(expected)), VinCheck::Match);
        assert!(matches!(
            compare_vin(expected, Some("wba1k2c50ev000000")),
            VinCheck::Mismatch { .. }
        ));
        assert!(matches!(
            compare_vin(expected, Some(" WBA1K2C50EV000000")),
            VinCheck::Mismatch { .. }
        ));
        assert!(matches!(
            compare_vin(expected, Some("WBA1K2C50EV000000 ")),
            VinCheck::Mismatch { .. }
        ));
    }

    #[test]
    fn compare_vin_keeps_unreadable_distinct_from_mismatch() {
        // The distinction is the point: a car that answered with a different VIN is
        // a different car; a car that answered nothing has proven nothing. Collapsing
        // them would report a silent ECU as a swapped vehicle.
        let expected = "WBA1K2C50EV000000";
        assert_eq!(compare_vin(expected, None), VinCheck::Unreadable);
        assert_eq!(
            compare_vin(expected, Some("WBA1K2C50EV999999")),
            VinCheck::Mismatch {
                expected: expected.to_string(),
                found: "WBA1K2C50EV999999".to_string(),
            }
        );
        assert_ne!(
            compare_vin(expected, None),
            compare_vin(expected, Some("WBA1K2C50EV999999"))
        );
    }

    #[test]
    fn compare_vin_treats_a_short_vin_as_unreadable() {
        // ISTA expands a 7-character VIN to 17 through a backend service before
        // comparing. klartext cannot, so a short read is unreadable — never a
        // mismatch, which would be an unearned "different car" verdict.
        assert_eq!(
            compare_vin("WBA1K2C50EV000000", Some("EV00000")),
            VinCheck::Unreadable
        );
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

    /// The happy path, asserted on the WIRE: exactly two frames, the pinned OFF
    /// payload then the pinned ON payload, both to 0x40, in that order.
    ///
    /// `start_paused` makes tokio's clock virtual, so the 15-second hold costs no
    /// real time — but the sleep is still awaited, so removing it would not make
    /// this pass any faster or differently. What this pins is the BYTES and the
    /// ORDER, which is what a wrong CRC or a swapped pair would break.
    #[tokio::test]
    async fn cycle_terminal_15_sends_the_pinned_off_then_on_payloads() {
        let (addr, log) = spawn_gateway_recording(&[
            (
                0x40,
                klartext_uds::service::clamp::KL15_OFF.to_vec(),
                vec![0x71, 0x01, 0x10, 0x01],
            ),
            (
                0x40,
                klartext_uds::service::clamp::KL15_ON.to_vec(),
                vec![0x71, 0x01, 0x10, 0x01],
            ),
        ])
        .await;
        let c = client(addr).await;
        c.cycle_terminal_15_holding(Duration::from_millis(1))
            .await
            .unwrap();

        let sent: Vec<(u8, Vec<u8>)> = log.lock().unwrap().clone();
        assert_eq!(
            sent,
            vec![
                (0x40, vec![0x31, 0x01, 0x10, 0x01, 0x06, 0x06, 0xA8]),
                (0x40, vec![0x31, 0x01, 0x10, 0x01, 0x0A, 0x0A, 0x43]),
            ],
            "the clamp payloads are CRC-protected literals and must go out verbatim"
        );
    }

    /// A failed OFF is the SAFE failure: terminal 15 was never dropped, so the error
    /// must report `restored: true` and must NOT tell the human the car may be dead.
    /// Getting this backwards would send someone to check a car that is fine.
    #[tokio::test]
    async fn a_failed_off_reports_terminal_15_still_up() {
        let (addr, log) = spawn_gateway_recording(&[(
            0x40,
            klartext_uds::service::clamp::KL15_OFF.to_vec(),
            vec![0x7F, 0x31, 0x22],
        )])
        .await;
        let c = client(addr).await;
        let error = c
            .cycle_terminal_15_holding(Duration::from_millis(1))
            .await
            .expect_err("the OFF was rejected");
        assert!(
            matches!(
                error,
                ClientError::ClampSwitch {
                    phase: "off",
                    restored: true,
                    ..
                }
            ),
            "got {error:?}"
        );
        assert!(!error.to_string().contains("MAY STILL BE DOWN"), "{error}");
        // ...and no ON was sent, because nothing was ever switched off.
        let sent: Vec<(u8, Vec<u8>)> = log.lock().unwrap().clone();
        assert_eq!(
            sent.len(),
            1,
            "only the OFF should have been attempted: {sent:02X?}"
        );
    }

    /// A failed ON is the DANGEROUS failure. klartext retries once (the documented
    /// divergence from ISTA, which has no recovery at all) and, if that also fails,
    /// says plainly that terminal 15 may still be down.
    #[tokio::test]
    async fn a_failed_on_is_retried_once_and_then_warns_loudly() {
        let (addr, log) = spawn_gateway_recording(&[
            (
                0x40,
                klartext_uds::service::clamp::KL15_OFF.to_vec(),
                vec![0x71, 0x01, 0x10, 0x01],
            ),
            (
                0x40,
                klartext_uds::service::clamp::KL15_ON.to_vec(),
                vec![0x7F, 0x31, 0x22],
            ),
        ])
        .await;
        let c = client(addr).await;
        let error = c
            .cycle_terminal_15_holding(Duration::from_millis(1))
            .await
            .expect_err("the ON was rejected twice");
        assert!(
            matches!(
                error,
                ClientError::ClampSwitch {
                    phase: "on",
                    restored: false,
                    ..
                }
            ),
            "got {error:?}"
        );
        assert!(error.to_string().contains("MAY STILL BE DOWN"), "{error}");

        // The restore attempt is the whole point of the divergence: the ON must have
        // been tried TWICE, not once.
        let ons = log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p.as_slice() == klartext_uds::service::clamp::KL15_ON)
            .count();
        assert_eq!(ons, 2, "a failed ON must be retried once before giving up");
    }
}
