//! Pure DTC semantics — code-number bridge and ISO 14229 status decoding.

/// Convert a raw 3-byte UDS DTC into its 24-bit ISTA code number.
///
/// BMW F-series uses a 3-byte DTC (high/mid/low, report §1.5); ISTA's
/// `XEP_FAULTCODES.CODE` is that same value as a 24-bit big-endian integer
/// (e.g. `D9 04 0A` → 14222346). This is the key used by [`crate::catalog`].
///
/// The raw-DTC ↔ ISTA-code relationship is corroborated against the DB but
/// unconfirmed on the wire — [verify against capture].
pub fn code_number(code: [u8; 3]) -> u32 {
    u32::from(code[0]) << 16 | u32::from(code[1]) << 8 | u32::from(code[2])
}

/// Decode a 1-byte DTC status into the set ISO 14229 flag names, low bit first.
///
/// Pure ISO 14229-1 §D.2 (report §1.5); independent of any database. The bit
/// values are reused from [`klartext_uds::dtc::status`] so the masks live in one
/// place. An all-clear status returns an empty list.
pub fn status_flags(status: u8) -> Vec<&'static str> {
    use klartext_uds::dtc::status as bit;

    // (mask, ISO name), low bit first.
    const FLAGS: [(u8, &str); 8] = [
        (bit::TEST_FAILED, "testFailed"),
        (
            bit::TEST_FAILED_THIS_OPERATION_CYCLE,
            "testFailedThisOperationCycle",
        ),
        (bit::PENDING, "pendingDTC"),
        (bit::CONFIRMED, "confirmedDTC"),
        (
            bit::TEST_NOT_COMPLETED_SINCE_CLEAR,
            "testNotCompletedSinceLastClear",
        ),
        (bit::TEST_FAILED_SINCE_CLEAR, "testFailedSinceLastClear"),
        (
            bit::TEST_NOT_COMPLETED_THIS_OPERATION_CYCLE,
            "testNotCompletedThisOperationCycle",
        ),
        (
            bit::WARNING_INDICATOR_REQUESTED,
            "warningIndicatorRequested",
        ),
    ];

    FLAGS
        .iter()
        .filter(|(mask, _)| status & mask != 0)
        .map(|&(_, name)| name)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_number_packs_three_bytes_big_endian() {
        // A raw 3-byte DTC is read as a 24-bit big-endian integer: this is the
        // key form ISTA's fault table uses (e.g. D9 04 0A -> 14222346).
        assert_eq!(code_number([0xD9, 0x04, 0x0A]), 14_222_346);
    }

    #[test]
    fn status_flags_lists_set_iso_bits_low_first() {
        // 0x2C = pending (0x04) | confirmed (0x08) | testFailedSinceLastClear (0x20).
        assert_eq!(
            status_flags(0x2C),
            vec!["pendingDTC", "confirmedDTC", "testFailedSinceLastClear"]
        );
    }

    #[test]
    fn status_flags_decodes_test_failed_and_warning() {
        // 0x81 = testFailed (0x01) | warningIndicatorRequested (0x80).
        assert_eq!(
            status_flags(0x81),
            vec!["testFailed", "warningIndicatorRequested"]
        );
    }

    #[test]
    fn status_flags_empty_when_no_bits_set() {
        assert!(status_flags(0x00).is_empty());
    }
}
