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
}

/// Whether a fault is failing *right now*, per ISTA's own rule.
///
/// Deliberately three-valued. "Not currently failing" and "we cannot tell" are
/// different answers, and collapsing them would let an agent report a car as
/// healthy when the relevant test simply has not run since the memory was
/// cleared. See [`Dtc::presence`] for the rule and its provenance.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Failing now: testFailed is set and the test HAS completed this cycle.
    Present,
    /// Stored but not currently failing: testFailed clear, test has run this cycle.
    Absent,
    /// The test has not completed this operation cycle — nothing can be concluded.
    Unknown,
}

/// Which memory a fault entry came from — ISTA's `EcuDTCType` `"F"` / `"I"`.
///
/// ISTA reads two distinct stores and merges them into one list per ECU, tagging
/// each entry with this discriminator (`VehicleIdent.cs:3518`): the `19 02` fault
/// memory (`FS_LESEN`) and the `22 2000` info memory (`IS_LESEN`, the
/// Infospeicher). A later slice that merges the two lists marks each entry's origin
/// with this; nothing consumes it yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FaultSource {
    /// From the `19 02` fault memory (`FS_LESEN`) — ISTA's `EcuDTCType = "F"`.
    FaultMemory,
    /// From the `22 2000` info memory (`IS_LESEN`) — ISTA's `EcuDTCType = "I"`.
    InfoMemory,
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
    /// True if the fault is pending (0x04).
    pub fn pending(self) -> bool {
        self.status & status::PENDING != 0
    }

    /// True if the fault is confirmed/stored (0x08).
    pub fn confirmed(self) -> bool {
        self.status & status::CONFIRMED != 0
    }

    /// ISTA's own present / absent / unknown verdict for this fault.
    ///
    /// This replaces klartext's invented `RELEVANT_MASK = 0xAF` (deleted
    /// 2026-07-18), which had no counterpart anywhere in ISTA's 147 assemblies.
    /// ISTA decides presence in `Fault.SetExisting()` by switching `F_VORHANDEN_NR`
    /// over an explicit value list, and states the same rule arithmetically in
    /// `TestplanCalculator.CalculateProryItem` for UDS ECUs:
    ///
    /// ```text
    /// int num  = F_VORHANDEN_NR & 1;
    /// int num2 = F_VORHANDEN_NR & 0x40;
    /// if (num == 1 && num2 == 0) { IsFaultPresent = true; }
    /// ```
    ///
    /// `F_VORHANDEN_NR` is derived from the raw status byte through a per-SGBD mask
    /// (`0x4D` or `0x6D`), but bits 0 and 6 are members of BOTH masks — so the two
    /// bits this rule tests survive the mask unchanged and klartext can compute
    /// ISTA's exact verdict from the `19 02` status byte alone. No `.prg` read, no
    /// per-variant mask, no second round trip.
    ///
    /// Checked against the owner's own car: the DAB antenna fault reported status
    /// `0x2F` → bit 0 set, bit 6 clear → [`Presence::Present`], which is correct —
    /// that antenna is genuinely disconnected.
    pub fn presence(self) -> Presence {
        if self.status & status::TEST_NOT_COMPLETED_THIS_OPERATION_CYCLE != 0 {
            Presence::Unknown
        } else if self.status & status::TEST_FAILED != 0 {
            Presence::Present
        } else {
            Presence::Absent
        }
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
/// the header (sub-function echo + availability mask) is missing,
/// [`UdsError::UnexpectedSubfunction`] if the echoed sub-function is not 0x02 (a
/// desync), and [`UdsError::MalformedDtcRecords`] if the record region is not a
/// whole number of 4-byte records.
pub fn decode_dtcs(payload: &[u8]) -> Result<Vec<Dtc>, UdsError> {
    let expected = positive_response_sid(sid::READ_DTC_INFORMATION);
    let body = expect_positive(payload, expected)?;

    // body = [sub-function echo, statusAvailabilityMask, records…]
    let records = body.get(2..).ok_or(UdsError::ShortResponse {
        sid: expected,
        need: 2,
        got: body.len(),
    })?;
    let subfn = crate::service::dtc_subfn::REPORT_DTC_BY_STATUS_MASK;
    if body[0] != subfn {
        return Err(UdsError::UnexpectedSubfunction {
            expected: subfn,
            got: body[0],
        });
    }
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

/// Bytes per info-memory record: 3-byte location code + 1-byte status.
const INFO_RECORD_LEN: usize = 4;

/// The BMW secondary/info memory (Infospeicher) read via `22 2000` (`IS_LESEN`).
///
/// A store DISTINCT from the `19 02` fault memory (its own service, location/type
/// tables, and an event-vs-fault flag) that ISTA shows alongside faults.
///
/// LAYOUT CONFIRMED from `IS_LESEN` bytecode (2026-07-18), superseding an earlier
/// DERIVED guess that cost a real off-by-one bug: `62 20 00` then back-to-back
/// `[location: 3][status: 1]` records. **There is no version byte on the wire.**
/// The job computes its record count as `(len(response) - 3) / 4` with a stride of
/// 4 — arithmetic BYTE-IDENTICAL to `FS_LESEN`'s (`IS_LESEN` @ `000563`-`00059A`
/// vs `FS_LESEN` @ `0007BD`-`0007F4` in `d72n47a0.prg`), and `IS_LESEN` itself is
/// byte-identical between `d72n47a0` and `cas4_2`. EDIABAS's `F_VERSION` result is
/// the compile-time constant `3`, emitted at op offset `000007` BEFORE the request
/// is even built — a format-generation marker, never a response byte.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InfoMemory {
    /// The info entries: each a 3-byte location code
    /// (`F_ORT_NR`, DTC-shaped) + 1-byte ISO-14229 status. Reuses [`Dtc`] as the
    /// record shape is identical; a code's text decodes via the semantic catalog as
    /// for a fault.
    pub entries: Vec<Dtc>,
    /// The full payload after `62 2000`, kept verbatim for the capture gate.
    pub raw: Vec<u8>,
}

/// Decode a `22 2000` info-memory (`IS_LESEN`) positive response.
///
/// The response is `62 20 00 <record…>`, each record `[location: 3][status: 1]` —
/// the same shape and the same `(len - 3) / 4` count arithmetic `FS_LESEN` uses.
/// Parses as many whole 4-byte records as the region holds; a trailing partial is
/// ignored and the full payload kept in [`InfoMemory::raw`]. LENIENT by design (it
/// never errors on record framing) — no `22 2000` response has been captured on a
/// car yet, so `raw` stays available for that.
///
/// # Errors
/// [`UdsError::Empty`] on no bytes, [`UdsError::UnexpectedResponse`] if the SID is
/// not the 0x62 positive response, and [`UdsError::ShortResponse`] if the two-byte
/// DID echo is missing.
pub fn decode_info_memory(payload: &[u8]) -> Result<InfoMemory, UdsError> {
    let expected = positive_response_sid(sid::READ_DATA_BY_IDENTIFIER);
    let body = expect_positive(payload, expected)?;
    // body = [DID-hi 0x20, DID-lo 0x00, record…] — NO version byte (see the type doc).
    let rest = body.get(2..).ok_or(UdsError::ShortResponse {
        sid: expected,
        need: 2,
        got: body.len(),
    })?;
    let entries = rest
        .chunks_exact(INFO_RECORD_LEN)
        .map(|r| Dtc {
            code: [r[0], r[1], r[2]],
            status: r[3],
        })
        .collect();
    Ok(InfoMemory {
        entries,
        raw: rest.to_vec(),
    })
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

    #[test]
    fn decode_dtcs_rejects_wrong_subfunction() {
        // A 59 04 (snapshot) echo where a 59 02 status-mask response was expected — a desync.
        assert!(matches!(
            decode_dtcs(&[0x59, 0x04, 0xFF]),
            Err(UdsError::UnexpectedSubfunction {
                expected: 0x02,
                got: 0x04
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

    // Info memory (22 2000 / IS_LESEN). Layout CONFIRMED from IS_LESEN bytecode:
    // records begin immediately after the DID echo, 4 bytes each, count
    // (len - 3) / 4 — the same arithmetic FS_LESEN uses. NO version byte.
    #[test]
    fn decode_info_memory_parses_entries_with_no_version_byte() {
        // 62 2000 | C90D60 status 08 | A6CF10 status 2F
        let payload = [
            0x62, 0x20, 0x00, 0xC9, 0x0D, 0x60, 0x08, 0xA6, 0xCF, 0x10, 0x2F,
        ];
        let info = decode_info_memory(&payload).unwrap();
        assert_eq!(
            info.entries,
            vec![
                Dtc {
                    code: [0xC9, 0x0D, 0x60],
                    status: 0x08
                },
                Dtc {
                    code: [0xA6, 0xCF, 0x10],
                    status: 0x2F
                },
            ]
        );
        // raw keeps the whole region after 62 2000 for the capture gate.
        assert_eq!(
            info.raw,
            vec![0xC9, 0x0D, 0x60, 0x08, 0xA6, 0xCF, 0x10, 0x2F]
        );
    }

    /// The regression guard for the off-by-one this decoder shipped with until
    /// 2026-07-18: it consumed a leading "version" byte that does not exist on the
    /// wire, so every record was read one byte early.
    ///
    /// These bytes are the OLD test's payload. Under the buggy decoder they parsed
    /// as two clean entries; under the fixed one the leading 0x03 is the first
    /// record's high code byte, which is exactly the corruption the bug caused.
    #[test]
    fn decode_info_memory_does_not_consume_a_version_byte() {
        let payload = [
            0x62, 0x20, 0x00, 0x03, 0xC9, 0x0D, 0x60, 0x08, 0xA6, 0xCF, 0x10, 0x2F,
        ];
        let info = decode_info_memory(&payload).unwrap();
        assert_eq!(
            info.entries.first(),
            Some(&Dtc {
                code: [0x03, 0xC9, 0x0D],
                status: 0x60
            }),
            "the byte after the DID echo is record data, never a version"
        );
    }

    #[test]
    fn decode_info_memory_is_lenient_on_layout() {
        // Empty info memory: the DID echo alone, no records.
        let empty = decode_info_memory(&[0x62, 0x20, 0x00]).unwrap();
        assert!(empty.entries.is_empty());
        // A trailing partial record is ignored, raw preserved for the capture gate.
        let partial =
            decode_info_memory(&[0x62, 0x20, 0x00, 0xC9, 0x0D, 0x60, 0x08, 0xAA]).unwrap();
        assert_eq!(partial.entries.len(), 1);
        assert_eq!(partial.raw.last(), Some(&0xAA));
    }

    #[test]
    fn decode_info_memory_rejects_wrong_sid_and_short() {
        assert!(matches!(
            decode_info_memory(&[0x7F, 0x22, 0x31]),
            Err(UdsError::UnexpectedResponse { .. })
        ));
        assert!(matches!(
            decode_info_memory(&[0x62, 0x20]),
            Err(UdsError::ShortResponse { .. })
        ));
    }

    /// ISTA's presence rule, exercised over the full status-byte space rather than
    /// a handful of samples: bit 6 (testNotCompletedThisOperationCycle) always wins
    /// and yields Unknown; otherwise bit 0 (testFailed) decides Present vs Absent.
    /// A sampled test would pass against several plausible-but-wrong rules — e.g.
    /// one that also consulted bit 4 or bit 5 — so this walks all 256 values.
    #[test]
    fn presence_follows_istas_two_bit_rule_across_every_status_byte() {
        for status in 0u8..=0xFF {
            let dtc = Dtc {
                code: [0, 0, 1],
                status,
            };
            let expected = if status & 0x40 != 0 {
                Presence::Unknown
            } else if status & 0x01 != 0 {
                Presence::Present
            } else {
                Presence::Absent
            };
            assert_eq!(dtc.presence(), expected, "status 0x{status:02X}");
        }
    }

    /// The values ISTA's own `Fault.SetExisting()` enumerates, verbatim. If the
    /// arithmetic rule above ever drifted from the value list ISTA actually
    /// switches on, this is what would catch it.
    #[test]
    fn presence_matches_istas_enumerated_value_lists() {
        let p = |status| {
            Dtc {
                code: [0, 0, 1],
                status,
            }
            .presence()
        };
        // #YesSmall — present
        for status in [0x05, 0x09, 0x0D, 0x21, 0x25, 0x29, 0x2D] {
            assert_eq!(p(status), Presence::Present, "0x{status:02X}");
        }
        // #NoSmall — absent
        for status in [0x04, 0x08, 0x0C, 0x20, 0x24, 0x28, 0x2C] {
            assert_eq!(p(status), Presence::Absent, "0x{status:02X}");
        }
        // No case in ISTA's switch: anything with bit 6 set stays unknown.
        for status in [0x40, 0x50, 0x4D, 0x6D, 0xFF] {
            assert_eq!(p(status), Presence::Unknown, "0x{status:02X}");
        }
        // The owner's real DAB antenna fault, from car session 1.
        assert_eq!(p(0x2F), Presence::Present);
    }

    /// The two fault sources are a distinct, `Copy` two-value discriminator — the
    /// shape a later merged-list slice relies on (ISTA's `EcuDTCType` "F"/"I").
    #[test]
    fn fault_source_discriminates_fault_and_info_memory() {
        let fault = FaultSource::FaultMemory;
        let info = FaultSource::InfoMemory;
        assert_ne!(fault, info);
        // Copy: reading `fault` after this line must still be valid.
        let _copy = fault;
        assert_eq!(fault, FaultSource::FaultMemory);
    }
}
