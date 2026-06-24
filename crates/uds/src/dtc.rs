//! Decoded DTCs and positive-response decoders for the read services.
//!
//! [`decode_dtcs`] turns a ReadDTCInformation (0x19) positive response into a
//! list of [`Dtc`]s, and [`decode_read_data_by_identifier`] turns a
//! ReadDataByIdentifier (0x22) positive response into its `(DID, raw bytes)`.
//! Both are pure and operate on the UDS payload with HSFZ framing already
//! stripped. Scaling and the *meaning* of a DTC code or DID value are the next
//! milestone — this layer returns raw bytes only.

use crate::{UdsError, positive_response_sid, sid};

/// Bit masks for the one-byte UDS DTC status, from report §1.5.
///
/// The same bits double as the `DTCStatusMask` in a ReadDTCInformation request
/// (`19 02 <mask>`): the ECU returns DTCs whose status ANDed with the mask is
/// non-zero. [`crate::ALL_DTC_STATUS_MASK`] (0xFF) therefore matches any stored
/// DTC, while [`CONFIRMED`] alone is the classic "what is actually wrong" scan.
pub mod status {
    /// 0x01 — the test failed the last time it ran.
    pub const TEST_FAILED: u8 = 0x01;
    /// 0x02 — the test failed at least once this operation cycle.
    pub const TEST_FAILED_THIS_OPERATION_CYCLE: u8 = 0x02;
    /// 0x04 — failure detected but not yet confirmed (pending).
    pub const PENDING: u8 = 0x04;
    /// 0x08 — the fault is confirmed/stored.
    pub const CONFIRMED: u8 = 0x08;
    /// 0x10 — the test has not completed since DTCs were last cleared.
    pub const TEST_NOT_COMPLETED_SINCE_CLEAR: u8 = 0x10;
    /// 0x20 — the test failed at least once since DTCs were last cleared.
    pub const TEST_FAILED_SINCE_CLEAR: u8 = 0x20;
    /// 0x40 — the test has not completed this operation cycle.
    pub const TEST_NOT_COMPLETED_THIS_OPERATION_CYCLE: u8 = 0x40;
    /// 0x80 — the ECU requests the warning indicator (e.g. a dash lamp).
    pub const WARNING_INDICATOR_REQUESTED: u8 = 0x80;
}

/// A diagnostic trouble code: a 3-byte code and its 1-byte status (report §1.5).
///
/// The mapping from the raw 3-byte code to BMW's displayed fault number is
/// semantic and deferred to the next milestone — [verify against capture].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dtc {
    /// The 3-byte DTC, high byte first.
    pub code: [u8; 3],
    /// The status byte; interpret with the [`status`] masks or the helpers.
    pub status: u8,
}

impl Dtc {
    /// True if the test failed the last time it ran (status bit 0x01).
    pub fn test_failed(self) -> bool {
        self.status & status::TEST_FAILED != 0
    }

    /// True if the test failed at least once this operation cycle (0x02).
    pub fn test_failed_this_operation_cycle(self) -> bool {
        self.status & status::TEST_FAILED_THIS_OPERATION_CYCLE != 0
    }

    /// True if the fault is pending (0x04).
    pub fn pending(self) -> bool {
        self.status & status::PENDING != 0
    }

    /// True if the fault is confirmed/stored (0x08).
    pub fn confirmed(self) -> bool {
        self.status & status::CONFIRMED != 0
    }

    /// True if the test has not completed since the last clear (0x10).
    pub fn test_not_completed_since_clear(self) -> bool {
        self.status & status::TEST_NOT_COMPLETED_SINCE_CLEAR != 0
    }

    /// True if the test failed at least once since the last clear (0x20).
    pub fn test_failed_since_clear(self) -> bool {
        self.status & status::TEST_FAILED_SINCE_CLEAR != 0
    }

    /// True if the test has not completed this operation cycle (0x40).
    pub fn test_not_completed_this_operation_cycle(self) -> bool {
        self.status & status::TEST_NOT_COMPLETED_THIS_OPERATION_CYCLE != 0
    }

    /// True if the ECU requests the warning indicator (0x80).
    pub fn warning_indicator_requested(self) -> bool {
        self.status & status::WARNING_INDICATOR_REQUESTED != 0
    }
}

impl core::fmt::Display for Dtc {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{:02X}{:02X}{:02X}/{:02X}",
            self.code[0], self.code[1], self.code[2], self.status
        )
    }
}

/// Bytes per DTC record in a 0x19/0x02 response: 3 code + 1 status (report §1.5).
const DTC_RECORD_LEN: usize = 4;

/// Decode a ReadDTCInformation `reportDTCByStatusMask` positive response.
///
/// The response is `59 02 <statusAvailabilityMask> <record…>`, where each record
/// is `[code: 3][status: 1]`. The sub-function echo (`02`) and the availability
/// mask are consumed; the remaining bytes are split into [`Dtc`]s. An empty list
/// (no records) is a valid `Ok(vec![])`. The record framing is corroborated but
/// BMW-proprietary in practice — [verify against capture].
///
/// # Errors
/// Returns [`UdsError::Empty`] on no bytes, [`UdsError::UnexpectedResponse`] if
/// the first byte is not the 0x59 positive SID, [`UdsError::ShortResponse`] if
/// the header (sub-function echo + availability mask) is missing, and
/// [`UdsError::MalformedDtcRecords`] if the record region is not a whole number
/// of 4-byte records.
pub fn decode_dtcs(payload: &[u8]) -> Result<Vec<Dtc>, UdsError> {
    let expected = positive_response_sid(sid::READ_DTC_INFORMATION);
    let body = expect_positive(payload, expected)?;

    // body = [sub-function echo, statusAvailabilityMask, records…]
    let records = body.get(2..).ok_or(UdsError::ShortResponse {
        sid: expected,
        need: 2,
        got: body.len(),
    })?;
    if records.len() % DTC_RECORD_LEN != 0 {
        return Err(UdsError::MalformedDtcRecords { len: records.len() });
    }

    let dtcs = records
        .chunks_exact(DTC_RECORD_LEN)
        .map(|r| Dtc {
            code: [r[0], r[1], r[2]],
            status: r[3],
        })
        .collect();
    Ok(dtcs)
}

/// Decode a ReadDataByIdentifier positive response into `(DID, raw value)`.
///
/// The response is `62 <DID-hi> <DID-lo> <data…>`; the returned bytes are the
/// raw value, unscaled. This decodes a single DID record (M2 requests one DID at
/// a time); multi-DID responses are deferred.
///
/// # Errors
/// Returns [`UdsError::Empty`] on no bytes, [`UdsError::UnexpectedResponse`] if
/// the first byte is not the 0x62 positive SID, and [`UdsError::ShortResponse`]
/// if the two-byte DID is missing.
pub fn decode_read_data_by_identifier(payload: &[u8]) -> Result<(u16, Vec<u8>), UdsError> {
    let expected = positive_response_sid(sid::READ_DATA_BY_IDENTIFIER);
    let body = expect_positive(payload, expected)?;

    let did = body.get(..2).ok_or(UdsError::ShortResponse {
        sid: expected,
        need: 2,
        got: body.len(),
    })?;
    let did = u16::from_be_bytes([did[0], did[1]]);
    Ok((did, body[2..].to_vec()))
}

/// Check the positive-response SID and return the bytes after it.
fn expect_positive(payload: &[u8], expected_sid: u8) -> Result<&[u8], UdsError> {
    match payload.first().copied() {
        None => Err(UdsError::Empty),
        Some(sid) if sid == expected_sid => Ok(&payload[1..]),
        Some(got) => Err(UdsError::UnexpectedResponse { expected_sid, got }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // DERIVED from the report's 0x19/0x02 layout (§1.5). No capture exists yet,
    // so the record bytes are synthetic but follow the documented framing.
    #[test]
    fn decode_dtcs_parses_code_and_status() {
        // 59 02 FF | 4A 12 34 status=08 | A0 B1 C2 status=2C
        let payload = [
            0x59, 0x02, 0xFF, 0x4A, 0x12, 0x34, 0x08, 0xA0, 0xB1, 0xC2, 0x2C,
        ];
        let dtcs = decode_dtcs(&payload).unwrap();
        assert_eq!(
            dtcs,
            vec![
                Dtc {
                    code: [0x4A, 0x12, 0x34],
                    status: 0x08
                },
                Dtc {
                    code: [0xA0, 0xB1, 0xC2],
                    status: 0x2C
                },
            ]
        );
        assert!(dtcs[0].confirmed());
        assert!(!dtcs[0].pending());
        assert_eq!(dtcs[0].to_string(), "4A1234/08");
    }

    #[test]
    fn decode_dtcs_accepts_empty_list() {
        // 59 02 FF with no records is a valid "no faults" answer.
        assert_eq!(decode_dtcs(&[0x59, 0x02, 0xFF]).unwrap(), vec![]);
    }

    #[test]
    fn decode_dtcs_rejects_partial_record() {
        // One trailing byte: 3 record bytes, not a multiple of 4.
        let payload = [0x59, 0x02, 0xFF, 0x4A, 0x12, 0x34];
        assert!(matches!(
            decode_dtcs(&payload),
            Err(UdsError::MalformedDtcRecords { len: 3 })
        ));
    }

    #[test]
    fn decode_dtcs_rejects_wrong_sid() {
        // 7F is not the 0x59 positive SID.
        assert!(matches!(
            decode_dtcs(&[0x7F, 0x19, 0x31]),
            Err(UdsError::UnexpectedResponse {
                expected_sid: 0x59,
                got: 0x7F
            })
        ));
    }

    // VERBATIM UDS shape from the report (§3): 22 F1 90 -> 62 F1 90 <VIN ascii>.
    #[test]
    fn decode_did_returns_did_and_raw_value() {
        let mut payload = vec![0x62, 0xF1, 0x90];
        payload.extend_from_slice(b"WBA1234567890ABCD"); // 17-char VIN
        let (did, value) = decode_read_data_by_identifier(&payload).unwrap();
        assert_eq!(did, 0xF190);
        assert_eq!(value, b"WBA1234567890ABCD");
    }

    #[test]
    fn decode_did_rejects_short_response() {
        // 62 F1 with no low DID byte.
        assert!(matches!(
            decode_read_data_by_identifier(&[0x62, 0xF1]),
            Err(UdsError::ShortResponse {
                sid: 0x62,
                need: 2,
                got: 1
            })
        ));
    }
}
