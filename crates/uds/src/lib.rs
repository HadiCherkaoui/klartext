//! UDS (ISO 14229) message layer for klartext — pure, no I/O.
//!
//! This crate turns UDS service requests into bytes and parses raw response
//! payloads back into typed values. It is transport-agnostic: everything here
//! produces or consumes the UDS payload, and the HSFZ transport
//! (`klartext-hsfz`) is what carries those bytes on the wire.
//!
//! The surface is organised as:
//!
//! - [`service`] — request builders (TesterPresent, DiagnosticSessionControl,
//!   ReadDTCInformation, ReadDataByIdentifier, ClearDiagnosticInformation),
//! - [`dtc`] — the [`Dtc`] type and the read-service response decoders,
//! - [`nrc`] — the [`Nrc`] negative-response-code table as a typed error,
//! - [`parse`] — the first-pass split of a payload into positive vs negative.
//!
//! [`parse`] stays deliberately dumb (it keeps the NRC byte raw); callers lift it
//! to a typed [`Nrc`] at the service boundary via [`UdsResponse::negative_nrc`].

use thiserror::Error;

pub mod dtc;
pub mod nrc;
pub mod service;

pub use dtc::{
    Dtc, DtcRecordRegion, DtcSeverity, decode_dtc_extended_data, decode_dtc_severity,
    decode_dtc_snapshot, decode_dtcs, decode_read_data_by_identifier,
};
pub use nrc::Nrc;
pub use service::{
    ALL_DTC_RECORDS, ALL_DTC_STATUS_MASK, CLEAR_ALL_DTCS, clear_all_dtcs,
    clear_diagnostic_information, clear_dynamic_data_identifier, define_dynamic_data_by_identifier,
    diagnostic_session_control, read_data_by_identifier, read_dtc_by_status_mask,
    read_dtc_extended_data_by_dtc, read_dtc_severity_by_dtc, read_dtc_snapshot_by_dtc,
    routine_control, tester_present, tester_present_suppressed, write_data_by_identifier,
};

/// UDS service IDs klartext speaks.
pub mod sid {
    /// DiagnosticSessionControl (0x10).
    pub const DIAGNOSTIC_SESSION_CONTROL: u8 = 0x10;
    /// ClearDiagnosticInformation (0x14).
    pub const CLEAR_DIAGNOSTIC_INFORMATION: u8 = 0x14;
    /// ReadDTCInformation (0x19).
    pub const READ_DTC_INFORMATION: u8 = 0x19;
    /// ReadDataByIdentifier (0x22).
    pub const READ_DATA_BY_IDENTIFIER: u8 = 0x22;
    /// DynamicallyDefineDataIdentifier (0x2C).
    pub const DYNAMICALLY_DEFINE_DATA_IDENTIFIER: u8 = 0x2C;
    /// WriteDataByIdentifier (0x2E) — a stored write; gate behind confirmation.
    pub const WRITE_DATA_BY_IDENTIFIER: u8 = 0x2E;
    /// RoutineControl (0x31) — start/stop a service routine; gate behind confirmation.
    pub const ROUTINE_CONTROL: u8 = 0x31;
    /// TesterPresent (0x3E).
    pub const TESTER_PRESENT: u8 = 0x3E;
    /// Negative-response marker: the first byte of a `7F <sid> <nrc>` response.
    pub const NEGATIVE_RESPONSE: u8 = 0x7F;
}

/// Diagnostic-session sub-functions for DiagnosticSessionControl (0x10).
pub mod session {
    /// defaultSession — always active at power-up.
    pub const DEFAULT: u8 = 0x01;
    /// programmingSession — flashing (out of scope).
    pub const PROGRAMMING: u8 = 0x02;
    /// extendedDiagnosticSession — enables writes/IO control/routines/DTC clear.
    pub const EXTENDED: u8 = 0x03;
    /// safetySystemDiagnosticSession.
    pub const SAFETY_SYSTEM: u8 = 0x04;
}

/// Added to a request SID to form its positive-response SID (e.g. 0x10 -> 0x50).
pub const POSITIVE_RESPONSE_OFFSET: u8 = 0x40;

/// suppressPosRspMsgIndicationBit. Set in a sub-function byte to make the ECU act
/// but suppress the positive response (e.g. `3E 80`); a negative response is
/// still sent on error.
pub const SUPPRESS_POSITIVE_RESPONSE: u8 = 0x80;

/// NRC 0x78: request received, ECU still working — the tester must keep waiting
/// (re-arm its read timeout to P2*). See [`P2_STAR_SERVER_MAX_DEFAULT_MS`].
pub const NRC_RESPONSE_PENDING: u8 = 0x78;

/// ISO 14229-2:2013 default P2_server_max (ms) — time the ECU has to *start* a
/// response. The real F20 ECUs report their own value in the `10 03` response.
/// [verify against capture]
pub const P2_SERVER_MAX_DEFAULT_MS: u64 = 50;

/// ISO 14229-2:2013 default P2*_server_max (ms) — the extended budget after an
/// NRC 0x78 "response pending". [verify against capture]
pub const P2_STAR_SERVER_MAX_DEFAULT_MS: u64 = 5000;

/// The positive-response SID expected for a given request SID (request + 0x40).
pub fn positive_response_sid(request_sid: u8) -> u8 {
    request_sid.wrapping_add(POSITIVE_RESPONSE_OFFSET)
}

/// A parsed UDS response: the first-pass split into positive vs negative.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UdsResponse {
    /// Positive response. `sid` is `request_sid + 0x40`; `data` is the rest.
    Positive { sid: u8, data: Vec<u8> },
    /// Negative response (`7F <rejected_sid> <nrc>`), with the NRC byte raw.
    Negative { rejected_sid: u8, nrc: u8 },
}

impl UdsResponse {
    /// True for a negative response with NRC 0x78 (response pending).
    ///
    /// The caller should keep reading for the real response rather than resend.
    pub fn is_response_pending(&self) -> bool {
        matches!(self, UdsResponse::Negative { nrc, .. } if *nrc == NRC_RESPONSE_PENDING)
    }

    /// The typed [`Nrc`] for a negative response, or `None` if positive.
    pub fn negative_nrc(&self) -> Option<Nrc> {
        match self {
            UdsResponse::Negative { nrc, .. } => Some(Nrc::from(*nrc)),
            UdsResponse::Positive { .. } => None,
        }
    }
}

/// Errors parsing or decoding a UDS response payload.
#[derive(Debug, Error)]
pub enum UdsError {
    /// The payload had no bytes (expected at least the SID).
    #[error("empty UDS response (expected at least one byte)")]
    Empty,
    /// A negative response was shorter than the `7F <sid> <nrc>` triple.
    #[error("truncated negative response: expected `7F <sid> <nrc>` (3 bytes), got {0}")]
    TruncatedNegative(usize),
    /// The positive-response SID did not match the request.
    #[error("unexpected response SID: expected 0x{expected_sid:02X}, got 0x{got:02X}")]
    UnexpectedResponse { expected_sid: u8, got: u8 },
    /// A ReadDTCInformation response echoed a different sub-function (a desync).
    #[error(
        "unexpected ReadDTCInformation sub-function: expected 0x{expected:02X}, got 0x{got:02X}"
    )]
    UnexpectedSubfunction { expected: u8, got: u8 },
    /// A positive response was too short to decode its fixed header.
    #[error("response 0x{sid:02X} too short: need {need} byte(s) after the SID, got {got}")]
    ShortResponse { sid: u8, need: usize, got: usize },
    /// A ReadDTCInformation record region was not a whole number of 4-byte DTCs.
    #[error("malformed DTC records: {len} byte(s) is not a multiple of 4")]
    MalformedDtcRecords { len: usize },
}

/// Parse a raw UDS response payload, with HSFZ/DoIP framing already stripped.
///
/// Splits the payload into [`UdsResponse::Positive`] or [`UdsResponse::Negative`]
/// without interpreting the data — decoding a specific service's bytes is the job
/// of [`decode_dtcs`] / [`decode_read_data_by_identifier`], and typing the NRC is
/// [`UdsResponse::negative_nrc`].
///
/// # Errors
/// Returns [`UdsError::Empty`] on an empty payload, and
/// [`UdsError::TruncatedNegative`] on a negative response shorter than 3 bytes.
pub fn parse(payload: &[u8]) -> Result<UdsResponse, UdsError> {
    match payload.first().copied() {
        None => Err(UdsError::Empty),
        Some(sid::NEGATIVE_RESPONSE) => {
            if payload.len() < 3 {
                return Err(UdsError::TruncatedNegative(payload.len()));
            }
            Ok(UdsResponse::Negative {
                rejected_sid: payload[1],
                nrc: payload[2],
            })
        }
        Some(sid) => Ok(UdsResponse::Positive {
            sid,
            data: payload[1..].to_vec(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positive_response_sid_adds_0x40() {
        assert_eq!(positive_response_sid(0x3E), 0x7E);
        assert_eq!(positive_response_sid(0x10), 0x50);
        assert_eq!(positive_response_sid(0x19), 0x59);
        assert_eq!(positive_response_sid(0x22), 0x62);
    }

    // Response parsing. Bare UDS byte strings are VERBATIM from the report.

    #[test]
    fn parse_positive_tester_present_7e00() {
        let r = parse(&[0x7E, 0x00]).unwrap();
        assert_eq!(
            r,
            UdsResponse::Positive {
                sid: 0x7E,
                data: vec![0x00]
            }
        );
    }

    #[test]
    fn parse_positive_dsc_with_timing() {
        // 50 03 00 32 13 88 — DSC-extended positive: P2=0x0032=50ms, P2*=0x1388.
        let r = parse(&[0x50, 0x03, 0x00, 0x32, 0x13, 0x88]).unwrap();
        assert_eq!(
            r,
            UdsResponse::Positive {
                sid: 0x50,
                data: vec![0x03, 0x00, 0x32, 0x13, 0x88],
            }
        );
    }

    #[test]
    fn parse_negative_maps_to_typed_nrc() {
        // 7F 10 22 — DSC rejected, NRC 0x22 conditionsNotCorrect.
        let r = parse(&[0x7F, 0x10, 0x22]).unwrap();
        assert_eq!(
            r,
            UdsResponse::Negative {
                rejected_sid: 0x10,
                nrc: 0x22,
            }
        );
        assert_eq!(r.negative_nrc(), Some(Nrc::ConditionsNotCorrect));
    }

    #[test]
    fn positive_has_no_nrc() {
        assert_eq!(parse(&[0x7E, 0x00]).unwrap().negative_nrc(), None);
    }

    #[test]
    fn response_pending_is_detected() {
        assert!(parse(&[0x7F, 0x3E, 0x78]).unwrap().is_response_pending());
        assert!(!parse(&[0x7E, 0x00]).unwrap().is_response_pending());
    }

    #[test]
    fn parse_empty_is_error() {
        assert!(matches!(parse(&[]), Err(UdsError::Empty)));
    }

    #[test]
    fn parse_truncated_negative_is_error() {
        assert!(matches!(
            parse(&[0x7F, 0x10]),
            Err(UdsError::TruncatedNegative(2))
        ));
    }
}
