//! UDS request builders for the services klartext speaks.
//!
//! Each function returns the raw UDS request bytes (no transport framing) for one
//! service: the M1 session services (TesterPresent, DiagnosticSessionControl) and
//! the M2 read/clear services (ReadDTCInformation, ReadDataByIdentifier,
//! ClearDiagnosticInformation). They are pure and allocation-free — the HSFZ
//! transport wraps the bytes for the wire.

use crate::{SUPPRESS_POSITIVE_RESPONSE, sid};

/// Well-known data identifiers (DIDs) for ReadDataByIdentifier (report §1.5).
///
/// The full DID set is ECU- and model-specific — [verify against capture].
pub mod did {
    /// 0xF190 — VIN (17 ASCII characters); the canonical "read the VIN" DID.
    pub const VIN: u16 = 0xF190;
    /// 0x172A — BMW IP configuration; example manufacturer DID.
    pub const IP_CONFIG: u16 = 0x172A;
}

/// ReadDTCInformation sub-functions (report §1.3); M2 uses only the first.
pub mod dtc_subfn {
    /// 0x02 — reportDTCByStatusMask: return DTCs matching a status mask.
    pub const REPORT_DTC_BY_STATUS_MASK: u8 = 0x02;
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

/// DTC status mask matching any status bit — returns all stored DTCs.
///
/// Passed to [`read_dtc_by_status_mask`] as the broadest fault scan. The report's
/// workshop scan instead uses [`crate::dtc::status::CONFIRMED`] (0x08) for
/// confirmed faults only.
pub const ALL_DTC_STATUS_MASK: u8 = 0xFF;

/// The 3-byte "clear every DTC" group for ClearDiagnosticInformation (report §1).
pub const CLEAR_ALL_DTCS: [u8; 3] = [0xFF, 0xFF, 0xFF];

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
/// See [`session`] for the sub-function constants.
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
}
