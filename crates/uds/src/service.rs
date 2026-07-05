//! UDS request builders for the services klartext speaks.
//!
//! Each function returns the raw UDS request bytes (no transport framing) for one
//! service: the M1 session services (TesterPresent, DiagnosticSessionControl), the
//! M2 read/clear services (ReadDTCInformation, ReadDataByIdentifier,
//! ClearDiagnosticInformation), and the M7 write/actuation services (RoutineControl,
//! WriteDataByIdentifier) used by service functions. The HSFZ transport wraps the
//! bytes for the wire. The fixed-shape builders are allocation-free; the
//! variable-length write/actuation builders return an owned `Vec`.

use crate::{SUPPRESS_POSITIVE_RESPONSE, sid};

/// Well-known data identifiers (DIDs) for ReadDataByIdentifier (report §1.5).
///
/// The full DID set is ECU- and model-specific — [verify against capture].
pub mod did {
    /// 0xF190 — VIN (17 ASCII characters); the canonical "read the VIN" DID.
    pub const VIN: u16 = 0xF190;
    /// 0x172A — BMW IP configuration; example manufacturer DID.
    pub const IP_CONFIG: u16 = 0x172A;
    /// 0x3F07 — BMW gateway VCM installed-ECU list (the SVT fitted list). The job
    /// `STATUS_VCM_GET_ECU_LIST_ALL` reads this; the response is decoded by
    /// [`crate::decode_ecu_list`]. [verify against capture]
    pub const ECU_LIST_ALL: u16 = 0x3F07;
    /// 0x3F06 — BMW gateway VCM vehicle order (Fahrzeugauftrag / FA). Read job
    /// `STATUS_VCM_GET_FA`. Decoded by `klartext_semantic::decode_vehicle_order`.
    pub const VEHICLE_ORDER: u16 = 0x3F06;
    /// 0x100B — BMW gateway VCM integration level (I-Stufe). Read job
    /// `STATUS_VCM_I_STUFE_LESEN`. Value is ASCII. [verify against capture]
    pub const I_STUFE: u16 = 0x100B;
}

/// ReadDTCInformation sub-functions (report §1.3, ISO 14229-1 §11.3).
///
/// M2 uses only [`dtc_subfn::REPORT_DTC_BY_STATUS_MASK`]; the freeze-frame reads
/// ([`dtc_subfn::REPORT_DTC_SNAPSHOT_BY_DTC`], [`dtc_subfn::REPORT_DTC_EXT_DATA_BY_DTC`],
/// [`dtc_subfn::REPORT_SEVERITY_INFO_OF_DTC`]) are the three ISTA's `FS_LESEN_DETAIL` emits.
pub mod dtc_subfn {
    /// 0x02 — reportDTCByStatusMask: return DTCs matching a status mask.
    pub const REPORT_DTC_BY_STATUS_MASK: u8 = 0x02;
    /// 0x04 — reportDTCSnapshotRecordByDTCNumber: the freeze-frame records.
    pub const REPORT_DTC_SNAPSHOT_BY_DTC: u8 = 0x04;
    /// 0x06 — reportDTCExtendedDataRecordByDTCNumber: counters, occurrence data.
    pub const REPORT_DTC_EXT_DATA_BY_DTC: u8 = 0x06;
    /// 0x09 — reportSeverityInformationOfDTC: severity / fault class.
    pub const REPORT_SEVERITY_INFO_OF_DTC: u8 = 0x09;
}

/// DynamicallyDefineDataIdentifier (0x2C) sub-functions (ISO 14229-1).
///
/// klartext uses these to read a BMW DDE proprietary measurement via the EDIABAS
/// "selektiv lesen" sequence: clear, then define a dynamic DID from the
/// measurement's internal id, then read it. See [`define_dynamic_data_by_identifier`].
pub mod dddi_subfn {
    /// 0x01 — defineByIdentifier: build a dynamic DID from source DID(s).
    pub const DEFINE_BY_IDENTIFIER: u8 = 0x01;
    /// 0x03 — clearDynamicallyDefinedDataIdentifier: drop a dynamic DID.
    pub const CLEAR: u8 = 0x03;
}

/// RoutineControl (0x31) sub-functions (ISO 14229-1).
///
/// A service-function routine is bracketed: [`routine_subfn::START_ROUTINE`] in the
/// function's Main phase, [`routine_subfn::STOP_ROUTINE`] in its Reset phase (the
/// return-to-safe step that must always run). [`routine_subfn::REQUEST_ROUTINE_RESULTS`]
/// polls a running routine.
pub mod routine_subfn {
    /// 0x01 — startRoutine.
    pub const START_ROUTINE: u8 = 0x01;
    /// 0x02 — stopRoutine (the return-control / undo step).
    pub const STOP_ROUTINE: u8 = 0x02;
    /// 0x03 — requestRoutineResults.
    pub const REQUEST_ROUTINE_RESULTS: u8 = 0x03;
}

/// DTC status mask matching any status bit — returns all stored DTCs.
///
/// Passed to [`read_dtc_by_status_mask`] as the broadest fault scan. The report's
/// workshop scan instead uses [`crate::dtc::status::CONFIRMED`] (0x08) for
/// confirmed faults only.
pub const ALL_DTC_STATUS_MASK: u8 = 0xFF;

/// The 3-byte "clear every DTC" group for ClearDiagnosticInformation (report §1).
pub const CLEAR_ALL_DTCS: [u8; 3] = [0xFF, 0xFF, 0xFF];

/// Record-number byte meaning "all records" in a `19 04`/`19 06` request.
///
/// ISO 14229-1 reserves 0xFF as the request-all value for
/// DTCSnapshotRecordNumber and DTCExtendedDataRecordNumber; it is what ISTA's
/// `FS_LESEN_DETAIL` sends. See [`read_dtc_snapshot_by_dtc`].
pub const ALL_DTC_RECORDS: u8 = 0xFF;

/// Build a TesterPresent request (`3E 00`).
///
/// Uses the zero sub-function so the ECU returns a positive `7E 00` — the safest,
/// side-effect-free first contact. For the background keepalive use
/// [`tester_present_suppressed`] instead.
pub fn tester_present() -> [u8; 2] {
    [sid::TESTER_PRESENT, 0x00]
}

/// Build a suppressed TesterPresent request (`3E 80`) for use as a keepalive.
///
/// The suppress-positive-response bit means the ECU performs the keepalive but
/// sends no positive response (it still sends a negative response on error), so a
/// background sender does not have to read a reply for every tick.
pub fn tester_present_suppressed() -> [u8; 2] {
    [sid::TESTER_PRESENT, SUPPRESS_POSITIVE_RESPONSE]
}

/// Build a DiagnosticSessionControl request (`10 <session>`), e.g. `10 03`.
///
/// See [`crate::session`] for the sub-function constants.
pub fn diagnostic_session_control(session: u8) -> [u8; 2] {
    [sid::DIAGNOSTIC_SESSION_CONTROL, session]
}

/// Build a ReadDTCInformation `reportDTCByStatusMask` request (`19 02 <mask>`).
///
/// `mask` is the `DTCStatusMask`: the ECU returns DTCs whose status ANDed with it
/// is non-zero. Use [`ALL_DTC_STATUS_MASK`] for every stored DTC, or a single bit
/// from [`crate::dtc::status`] (e.g. confirmed-only) to narrow the scan.
pub fn read_dtc_by_status_mask(mask: u8) -> [u8; 3] {
    [
        sid::READ_DTC_INFORMATION,
        dtc_subfn::REPORT_DTC_BY_STATUS_MASK,
        mask,
    ]
}

/// Build a reportDTCSnapshotRecordByDTCNumber request (`19 04 <dtc> <record>`).
///
/// `dtc` is the 3-byte code (high, mid, low) big-endian; `record` selects one
/// snapshot record or [`ALL_DTC_RECORDS`] for every record (what ISTA sends). A
/// read — no session or security precondition. The `59 04` response layout is
/// decoded by [`crate::decode_dtc_snapshot`] — [verify against capture].
pub fn read_dtc_snapshot_by_dtc(dtc: [u8; 3], record: u8) -> [u8; 6] {
    [
        sid::READ_DTC_INFORMATION,
        dtc_subfn::REPORT_DTC_SNAPSHOT_BY_DTC,
        dtc[0],
        dtc[1],
        dtc[2],
        record,
    ]
}

/// Build a reportDTCExtendedDataRecordByDTCNumber request (`19 06 <dtc> <record>`).
///
/// `dtc` is the 3-byte code big-endian; `record` selects one extended-data record
/// or [`ALL_DTC_RECORDS`] for every record. A read. The `59 06` response is
/// decoded by [`crate::decode_dtc_extended_data`] — [verify against capture].
pub fn read_dtc_extended_data_by_dtc(dtc: [u8; 3], record: u8) -> [u8; 6] {
    [
        sid::READ_DTC_INFORMATION,
        dtc_subfn::REPORT_DTC_EXT_DATA_BY_DTC,
        dtc[0],
        dtc[1],
        dtc[2],
        record,
    ]
}

/// Build a reportSeverityInformationOfDTC request (`19 09 <dtc>`).
///
/// `dtc` is the 3-byte code big-endian; there is no record byte. A read. The
/// `59 09` response is decoded by [`crate::decode_dtc_severity`] — [verify against
/// capture].
pub fn read_dtc_severity_by_dtc(dtc: [u8; 3]) -> [u8; 5] {
    [
        sid::READ_DTC_INFORMATION,
        dtc_subfn::REPORT_SEVERITY_INFO_OF_DTC,
        dtc[0],
        dtc[1],
        dtc[2],
    ]
}

/// Build a ReadDataByIdentifier request for one DID (`22 <hi> <lo>`).
///
/// See [`did`] for well-known identifiers such as [`did::VIN`].
pub fn read_data_by_identifier(did: u16) -> [u8; 3] {
    let [hi, lo] = did.to_be_bytes();
    [sid::READ_DATA_BY_IDENTIFIER, hi, lo]
}

/// Build a ClearDiagnosticInformation request for one DTC group (`14 <hi><mid><lo>`).
///
/// Pass [`CLEAR_ALL_DTCS`] to clear every DTC. This is a state change, not a read,
/// and must be gated behind explicit confirmation by the caller.
pub fn clear_diagnostic_information(dtc: [u8; 3]) -> [u8; 4] {
    [sid::CLEAR_DIAGNOSTIC_INFORMATION, dtc[0], dtc[1], dtc[2]]
}

/// Build a ClearDiagnosticInformation request that clears every DTC (`14 FF FF FF`).
pub fn clear_all_dtcs() -> [u8; 4] {
    clear_diagnostic_information(CLEAR_ALL_DTCS)
}

/// Build a clearDynamicallyDefinedDataIdentifier request (`2C 03 <hi> <lo>`).
///
/// Drops any prior definition of dynamic DID `dynamic_did`; it leads the DDE
/// measurement-read sequence so the define starts from a clean slate. Defining a
/// dynamic DID is transient ECU state scoped to the session, not a stored write.
pub fn clear_dynamic_data_identifier(dynamic_did: u16) -> [u8; 4] {
    let [hi, lo] = dynamic_did.to_be_bytes();
    [
        sid::DYNAMICALLY_DEFINE_DATA_IDENTIFIER,
        dddi_subfn::CLEAR,
        hi,
        lo,
    ]
}

/// Build a defineByIdentifier request (`2C 01 <dynDID> <srcDID> <pos> <size>`).
///
/// Defines dynamic DID `dynamic_did` to mirror `size` bytes of source DID
/// `source_did` from 1-based `position`. The BMW DDE reads a proprietary
/// measurement by defining `0xF303` from the measurement's internal id, then
/// reading `0xF303`.
///
/// The byte shape is DERIVED from the `d72n47a0` `STATUS_MOTORTEMPERATUR`
/// disassembly (`docs/sgbd-findings.md` §7a), not yet confirmed against a real
/// capture — `position`/`size` come from the measurement's data type.
pub fn define_dynamic_data_by_identifier(
    dynamic_did: u16,
    source_did: u16,
    position: u8,
    size: u8,
) -> [u8; 8] {
    let [dyn_hi, dyn_lo] = dynamic_did.to_be_bytes();
    let [src_hi, src_lo] = source_did.to_be_bytes();
    [
        sid::DYNAMICALLY_DEFINE_DATA_IDENTIFIER,
        dddi_subfn::DEFINE_BY_IDENTIFIER,
        dyn_hi,
        dyn_lo,
        src_hi,
        src_lo,
        position,
        size,
    ]
}

/// Build a RoutineControl request (`31 <subfn> <rid_hi> <rid_lo> [params…]`).
///
/// `subfn` selects start/stop/requestResults (see [`routine_subfn`]); `rid` is the
/// 2-byte routine identifier; `params` are the optional routine-specific bytes
/// (empty for a bare start/stop). The positive response echoes `71 <subfn> <rid>`.
///
/// RoutineControl changes ECU state — the caller runs it only inside an extended
/// session, behind explicit confirmation, and pairs a [`routine_subfn::START_ROUTINE`]
/// with a guaranteed [`routine_subfn::STOP_ROUTINE`].
pub fn routine_control(subfn: u8, rid: u16, params: &[u8]) -> Vec<u8> {
    let [hi, lo] = rid.to_be_bytes();
    let mut request = Vec::with_capacity(4 + params.len());
    request.extend_from_slice(&[sid::ROUTINE_CONTROL, subfn, hi, lo]);
    request.extend_from_slice(params);
    request
}

/// Build a WriteDataByIdentifier request (`2E <did_hi> <did_lo> <data…>`).
///
/// Writes `data` to DID `did`; the positive response echoes `6E <did>`. A stored
/// write — the caller runs it only inside an extended session, behind explicit
/// confirmation, having backed up the original value and intending to read it back.
pub fn write_data_by_identifier(did: u16, data: &[u8]) -> Vec<u8> {
    let [hi, lo] = did.to_be_bytes();
    let mut request = Vec::with_capacity(3 + data.len());
    request.extend_from_slice(&[sid::WRITE_DATA_BY_IDENTIFIER, hi, lo]);
    request.extend_from_slice(data);
    request
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tester_present_encodes_3e00() {
        assert_eq!(tester_present(), [0x3E, 0x00]);
    }

    #[test]
    fn tester_present_suppressed_encodes_3e80() {
        assert_eq!(tester_present_suppressed(), [0x3E, 0x80]);
    }

    #[test]
    fn dsc_default_encodes_1001() {
        assert_eq!(
            diagnostic_session_control(crate::session::DEFAULT),
            [0x10, 0x01]
        );
    }

    #[test]
    fn dsc_extended_encodes_1003() {
        assert_eq!(
            diagnostic_session_control(crate::session::EXTENDED),
            [0x10, 0x03]
        );
    }

    // VERBATIM read shapes from the report (§3): the standard workshop scan and
    // the canonical VIN read.
    #[test]
    fn read_dtc_confirmed_encodes_190208() {
        assert_eq!(
            read_dtc_by_status_mask(crate::dtc::status::CONFIRMED),
            [0x19, 0x02, 0x08]
        );
    }

    #[test]
    fn read_dtc_all_encodes_1902ff() {
        assert_eq!(
            read_dtc_by_status_mask(ALL_DTC_STATUS_MASK),
            [0x19, 0x02, 0xFF]
        );
    }

    #[test]
    fn read_did_vin_encodes_22f190() {
        assert_eq!(read_data_by_identifier(did::VIN), [0x22, 0xF1, 0x90]);
    }

    // Freeze-frame reads (M11). Byte shapes are the ISO 14229-1 §11.3 sub-function
    // layouts ISTA's FS_LESEN_DETAIL emits (docs/…-m11-freeze-frames-design.md §3.1),
    // read off the d72n47a0 disassembly — the DTC bytes are big-endian, record 0xFF
    // = all. [verify against capture].
    #[test]
    fn read_dtc_snapshot_encodes_1904_dtc_record() {
        assert_eq!(
            read_dtc_snapshot_by_dtc([0x4A, 0x12, 0x34], ALL_DTC_RECORDS),
            [0x19, 0x04, 0x4A, 0x12, 0x34, 0xFF]
        );
    }

    #[test]
    fn read_dtc_extended_data_encodes_1906_dtc_record() {
        assert_eq!(
            read_dtc_extended_data_by_dtc([0x4A, 0x12, 0x34], ALL_DTC_RECORDS),
            [0x19, 0x06, 0x4A, 0x12, 0x34, 0xFF]
        );
    }

    #[test]
    fn read_dtc_severity_encodes_1909_dtc_no_record_byte() {
        assert_eq!(
            read_dtc_severity_by_dtc([0x4A, 0x12, 0x34]),
            [0x19, 0x09, 0x4A, 0x12, 0x34]
        );
    }

    #[test]
    fn clear_all_encodes_14ffffff() {
        assert_eq!(clear_all_dtcs(), [0x14, 0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn clear_one_dtc_encodes_14_and_code() {
        assert_eq!(
            clear_diagnostic_information([0x4A, 0x12, 0x34]),
            [0x14, 0x4A, 0x12, 0x34]
        );
    }

    // DynamicallyDefineDataIdentifier (0x2C). Byte shapes are DERIVED from the
    // d72n47a0 STATUS_MOTORTEMPERATUR disassembly (docs/sgbd-findings.md §7a), the
    // DDE "selektiv lesen" sequence — not yet confirmed against a real capture.
    #[test]
    fn clear_dynamic_did_encodes_2c03() {
        // clearDynamicallyDefinedDataIdentifier for dynamic DID 0xF303.
        assert_eq!(
            clear_dynamic_data_identifier(0xF303),
            [0x2C, 0x03, 0xF3, 0x03]
        );
    }

    #[test]
    fn define_dynamic_did_by_identifier_encodes_2c01() {
        // Define dyn DID 0xF303 from source DID 0x4BC3, position 1, size 2 (u16):
        // 2C 01 F3 03 4B C3 01 02.
        assert_eq!(
            define_dynamic_data_by_identifier(0xF303, 0x4BC3, 0x01, 0x02),
            [0x2C, 0x01, 0xF3, 0x03, 0x4B, 0xC3, 0x01, 0x02]
        );
    }

    // Write/actuation services (M7). The byte SHAPE is ISO 14229-1; the routine ids,
    // DIDs, and params below are illustrative (not BMW data) — a service function's
    // real ids come from the SGBD/oracle in `klartext-semantic`.
    #[test]
    fn routine_control_start_no_params_encodes_3101() {
        // startRoutine, RID 0x0F0C, no params: 31 01 0F 0C.
        assert_eq!(
            routine_control(routine_subfn::START_ROUTINE, 0x0F0C, &[]),
            vec![0x31, 0x01, 0x0F, 0x0C]
        );
    }

    #[test]
    fn routine_control_start_with_params_appends_them() {
        // startRoutine, RID 0x0203, one param byte 0x01: 31 01 02 03 01.
        assert_eq!(
            routine_control(routine_subfn::START_ROUTINE, 0x0203, &[0x01]),
            vec![0x31, 0x01, 0x02, 0x03, 0x01]
        );
    }

    #[test]
    fn routine_control_stop_encodes_3102() {
        // stopRoutine (the return-control step), RID 0x0F0C: 31 02 0F 0C.
        assert_eq!(
            routine_control(routine_subfn::STOP_ROUTINE, 0x0F0C, &[]),
            vec![0x31, 0x02, 0x0F, 0x0C]
        );
    }

    #[test]
    fn write_data_by_identifier_encodes_2e_did_and_data() {
        // Write DID 0xA0F7 with one reset byte 0x00: 2E A0 F7 00.
        assert_eq!(
            write_data_by_identifier(0xA0F7, &[0x00]),
            vec![0x2E, 0xA0, 0xF7, 0x00]
        );
    }
}
