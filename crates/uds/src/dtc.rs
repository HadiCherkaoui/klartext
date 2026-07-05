//! Decoded DTCs and positive-response decoders for the read services.
//!
//! [`decode_dtcs`] turns a ReadDTCInformation (0x19) positive response into a
//! list of [`Dtc`]s, and [`decode_read_data_by_identifier`] turns a
//! ReadDataByIdentifier (0x22) positive response into its `(DID, raw bytes)`.
//! Both are pure and operate on the UDS payload with HSFZ framing already
//! stripped. This layer returns raw bytes only; the *meaning* of a DTC code or
//! DID value is the semantic layer's job (`klartext-semantic`).

use crate::{UdsError, positive_response_sid, sid};

/// Bit masks for the one-byte UDS DTC status, from report §1.5.
///
/// The same bits double as the `DTCStatusMask` in a ReadDTCInformation request
/// (`19 02 <mask>`): the ECU returns DTCs whose status ANDed with the mask is
/// non-zero. [`crate::ALL_DTC_STATUS_MASK`] (0xFF) therefore matches any stored
/// DTC, while [`status::CONFIRMED`] alone is the classic "what is actually wrong" scan.
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

    /// Bits that mark a DTC as a *real* fault worth surfacing.
    ///
    /// Any of testFailed (0x01), testFailedThisOperationCycle (0x02), pending
    /// (0x04), confirmed (0x08), testFailedSinceLastClear (0x20), or
    /// warningIndicatorRequested (0x80). The complement (0x50 =
    /// testNotCompletedSinceLastClear | testNotCompletedThisOperationCycle) is
    /// "not tested this cycle" catalog noise: a `19 02 FF` scan of an idle ECU
    /// returns many such entries (the FEM returned ~147 with the engine off). A
    /// status of only those bits — or all zero — is not a stored fault.
    pub const RELEVANT_MASK: u8 = 0xAF;
}

/// A diagnostic trouble code: a 3-byte code and its 1-byte status (report §1.5).
///
/// Turning the raw 3-byte code into BMW's fault text is the semantic layer's job
/// (`klartext-semantic` reads it as a 24-bit number); the wire form still awaits
/// confirmation — [verify against capture].
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

    /// True if this DTC is a real fault worth surfacing.
    ///
    /// See [`status::RELEVANT_MASK`]. False for "not tested this cycle" catalog
    /// noise and for an all-clear status.
    pub fn is_relevant(self) -> bool {
        self.status & status::RELEVANT_MASK != 0
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

/// The DTC and status a `59 04`/`59 06` response echoes, plus its raw record region.
///
/// A snapshot (`19 04`) or extended-data (`19 06`) positive response is
/// `59 <subfn> <DTC:3> <statusOfDTC:1> <records…>`. This holds the echoed DTC and
/// status; `body` is the record region left **unparsed**, because the width of each
/// record's data is not on the wire — it comes from the ECU's SGBD definition, so
/// the record walk is the semantic layer's job (`klartext-semantic`).
///
/// The framing after the status byte is DERIVED from ISO 14229-1 §11.3 and the DDE
/// disassembly, with no `0x19` capture yet — [verify against capture].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtcRecordRegion {
    /// The 3-byte DTC the response echoed (high, mid, low).
    pub dtc: [u8; 3],
    /// The DTC status byte; interpret with the [`status`] masks.
    pub status: u8,
    /// The raw record region after the status byte, for the semantic decoder.
    pub body: Vec<u8>,
}

/// Severity / fault-class information from a `59 09` response.
///
/// ISO 14229-1 reportSeverityInformationOfDTC returns a severity byte and a
/// functional-unit byte alongside the DTC and its status. The exact BMW layout is
/// DERIVED from the ISO order — [verify against capture].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtcSeverity {
    /// The 3-byte DTC (high, mid, low).
    pub dtc: [u8; 3],
    /// The DTC status byte.
    pub status: u8,
    /// The DTCSeverity byte (severity-class bits).
    pub severity: u8,
    /// The DTCFunctionalUnit byte.
    pub functional_unit: u8,
}

/// Bytes before the record region in a `59 04`/`59 06`: subfn + DTC(3) + status.
const DTC_DETAIL_HEADER_LEN: usize = 5;

/// Bytes in a `59 09` body: subfn + availMask + severity + funcUnit + DTC(3) + status.
const DTC_SEVERITY_BODY_LEN: usize = 8;

/// Decode a `59 04` reportDTCSnapshotRecordByDTCNumber positive response.
///
/// Returns the echoed DTC + status and the raw snapshot record region; the
/// definition-driven field walk is `klartext-semantic`'s job. The record framing is
/// DERIVED — [verify against capture].
///
/// # Errors
/// [`UdsError::Empty`] on no bytes, [`UdsError::UnexpectedResponse`] if the SID is
/// not 0x59, [`UdsError::UnexpectedSubfunction`] if the echoed sub-function is not
/// 0x04 (a desync), and [`UdsError::ShortResponse`] if the header is missing.
pub fn decode_dtc_snapshot(payload: &[u8]) -> Result<DtcRecordRegion, UdsError> {
    decode_dtc_record_region(
        payload,
        crate::service::dtc_subfn::REPORT_DTC_SNAPSHOT_BY_DTC,
    )
}

/// Decode a `59 06` reportDTCExtendedDataRecordByDTCNumber positive response.
///
/// Returns the echoed DTC + status and the raw extended-data record region; the
/// per-record length comes from the SGBD definition, so the walk is the semantic
/// layer's job. The record framing is DERIVED — [verify against capture].
///
/// # Errors
/// As [`decode_dtc_snapshot`], but the echoed sub-function must be 0x06.
pub fn decode_dtc_extended_data(payload: &[u8]) -> Result<DtcRecordRegion, UdsError> {
    decode_dtc_record_region(
        payload,
        crate::service::dtc_subfn::REPORT_DTC_EXT_DATA_BY_DTC,
    )
}

/// Shared decoder for the `59 04`/`59 06` `subfn + DTC + status + region` shape.
fn decode_dtc_record_region(
    payload: &[u8],
    expected_subfn: u8,
) -> Result<DtcRecordRegion, UdsError> {
    let expected = positive_response_sid(sid::READ_DTC_INFORMATION);
    let body = expect_positive(payload, expected)?;

    // body = [sub-function echo, DTC hi, DTC mid, DTC lo, status, records…]
    if body.len() < DTC_DETAIL_HEADER_LEN {
        return Err(UdsError::ShortResponse {
            sid: expected,
            need: DTC_DETAIL_HEADER_LEN,
            got: body.len(),
        });
    }
    if body[0] != expected_subfn {
        return Err(UdsError::UnexpectedSubfunction {
            expected: expected_subfn,
            got: body[0],
        });
    }
    Ok(DtcRecordRegion {
        dtc: [body[1], body[2], body[3]],
        status: body[4],
        body: body[DTC_DETAIL_HEADER_LEN..].to_vec(),
    })
}

/// Decode a `59 09` reportSeverityInformationOfDTC positive response.
///
/// Parses the severity and functional-unit bytes with the echoed DTC + status.
/// The layout is DERIVED from ISO 14229-1 — [verify against capture].
///
/// # Errors
/// [`UdsError::Empty`], [`UdsError::UnexpectedResponse`] (SID not 0x59),
/// [`UdsError::UnexpectedSubfunction`] (echo not 0x09), and
/// [`UdsError::ShortResponse`] if the fixed record is missing.
pub fn decode_dtc_severity(payload: &[u8]) -> Result<DtcSeverity, UdsError> {
    let expected = positive_response_sid(sid::READ_DTC_INFORMATION);
    let body = expect_positive(payload, expected)?;

    // body = [subfn 0x09, DTCStatusAvailabilityMask, DTCSeverity, DTCFunctionalUnit,
    //         DTC hi, DTC mid, DTC lo, statusOfDTC]
    if body.len() < DTC_SEVERITY_BODY_LEN {
        return Err(UdsError::ShortResponse {
            sid: expected,
            need: DTC_SEVERITY_BODY_LEN,
            got: body.len(),
        });
    }
    let subfn = crate::service::dtc_subfn::REPORT_SEVERITY_INFO_OF_DTC;
    if body[0] != subfn {
        return Err(UdsError::UnexpectedSubfunction {
            expected: subfn,
            got: body[0],
        });
    }
    Ok(DtcSeverity {
        severity: body[2],
        functional_unit: body[3],
        dtc: [body[4], body[5], body[6]],
        status: body[7],
    })
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

    // Freeze-frame decoders (M11). No 0x19 detail capture exists yet, so these
    // response bytes are DERIVED from the ISO 14229-1 §11.3 record framing (design
    // doc §3.2–3.4) — synthetic but following the documented shape, as decode_dtcs is.
    #[test]
    fn decode_snapshot_splits_dtc_status_and_region() {
        // 59 04 | DTC 4A1234 | status 08 | record 01 | 1 identifier | UWNR 5205 | data 7C
        let payload = [
            0x59, 0x04, 0x4A, 0x12, 0x34, 0x08, 0x01, 0x01, 0x52, 0x05, 0x7C,
        ];
        let region = decode_dtc_snapshot(&payload).unwrap();
        assert_eq!(region.dtc, [0x4A, 0x12, 0x34]);
        assert_eq!(region.status, 0x08);
        // Everything after the status byte is the semantic layer's record region.
        assert_eq!(region.body, vec![0x01, 0x01, 0x52, 0x05, 0x7C]);
    }

    #[test]
    fn decode_snapshot_accepts_empty_region() {
        // A DTC with no stored snapshot: header only, no records.
        let region = decode_dtc_snapshot(&[0x59, 0x04, 0x4A, 0x12, 0x34, 0x08]).unwrap();
        assert_eq!(region.dtc, [0x4A, 0x12, 0x34]);
        assert!(region.body.is_empty());
    }

    #[test]
    fn decode_snapshot_rejects_wrong_subfunction() {
        // A 59 02 (status-mask) response where a 59 04 was expected — a desync.
        assert!(matches!(
            decode_dtc_snapshot(&[0x59, 0x02, 0x4A, 0x12, 0x34, 0x08]),
            Err(UdsError::UnexpectedSubfunction {
                expected: 0x04,
                got: 0x02
            })
        ));
    }

    #[test]
    fn decode_snapshot_rejects_short_header() {
        assert!(matches!(
            decode_dtc_snapshot(&[0x59, 0x04, 0x4A, 0x12]),
            Err(UdsError::ShortResponse {
                sid: 0x59,
                need: 5,
                got: 3
            })
        ));
    }

    #[test]
    fn decode_extended_data_splits_dtc_status_and_region() {
        // 59 06 | DTC 4A1234 | status 08 | record 02 (HFK) | 1 byte 1F
        let payload = [0x59, 0x06, 0x4A, 0x12, 0x34, 0x08, 0x02, 0x1F];
        let region = decode_dtc_extended_data(&payload).unwrap();
        assert_eq!(region.dtc, [0x4A, 0x12, 0x34]);
        assert_eq!(region.status, 0x08);
        assert_eq!(region.body, vec![0x02, 0x1F]);
    }

    #[test]
    fn decode_extended_data_rejects_wrong_subfunction() {
        assert!(matches!(
            decode_dtc_extended_data(&[0x59, 0x04, 0x4A, 0x12, 0x34, 0x08]),
            Err(UdsError::UnexpectedSubfunction {
                expected: 0x06,
                got: 0x04
            })
        ));
    }

    #[test]
    fn decode_severity_parses_severity_unit_dtc_status() {
        // 59 09 | availMask FF | severity 20 | funcUnit 10 | DTC 4A1234 | status 08
        let payload = [0x59, 0x09, 0xFF, 0x20, 0x10, 0x4A, 0x12, 0x34, 0x08];
        let sev = decode_dtc_severity(&payload).unwrap();
        assert_eq!(sev.severity, 0x20);
        assert_eq!(sev.functional_unit, 0x10);
        assert_eq!(sev.dtc, [0x4A, 0x12, 0x34]);
        assert_eq!(sev.status, 0x08);
    }

    #[test]
    fn decode_severity_rejects_short_record() {
        assert!(matches!(
            decode_dtc_severity(&[0x59, 0x09, 0xFF, 0x20]),
            Err(UdsError::ShortResponse { sid: 0x59, .. })
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

    #[test]
    fn relevant_mask_partitions_stored_faults_from_not_tested_noise() {
        use status::RELEVANT_MASK;
        // Relevant bits: testFailed|thisCycle|pending|confirmed|failedSinceClear|warning.
        assert_eq!(RELEVANT_MASK, 0xAF);
        // The two "not completed" bits are exactly the complement.
        assert_eq!(RELEVANT_MASK | 0x50, 0xFF);

        let confirmed = Dtc {
            code: [0, 0, 1],
            status: 0x08,
        };
        let failed = Dtc {
            code: [0, 0, 2],
            status: 0x01,
        };
        let warn = Dtc {
            code: [0, 0, 3],
            status: 0x80,
        };
        assert!(confirmed.is_relevant() && failed.is_relevant() && warn.is_relevant());

        // Catalog noise: only "not tested this / since" bits, or all-clear.
        let not_tested = Dtc {
            code: [0, 0, 4],
            status: 0x40,
        };
        let not_tested_since = Dtc {
            code: [0, 0, 5],
            status: 0x50,
        };
        let all_clear = Dtc {
            code: [0, 0, 6],
            status: 0x00,
        };
        assert!(!not_tested.is_relevant());
        assert!(!not_tested_since.is_relevant());
        assert!(!all_clear.is_relevant());
    }
}
