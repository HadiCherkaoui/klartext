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
}
