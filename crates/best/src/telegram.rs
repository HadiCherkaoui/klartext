//! BMW-FAST telegram codec: frame and parse UDS payloads for the BEST/2 VM.
//!
//! A BEST/2 job's `xsend` opcode emits a full BMW-FAST telegram
//! `[0x80|len][target][source][uds…][checksum]`, whereas `klartext-client`'s
//! `Session::request` speaks bare UDS. This codec is the translation seam
//! between the two: [`encode`] builds the on-wire telegram a real ECU expects,
//! and [`decode`] parses a telegram back into its header + UDS payload.
//!
//! ## Frame layout
//! The first byte is a format byte whose low 6 bits (`fmt & 0x3F`) carry the
//! payload length for the SHORT form. `target` and `source` are the ECU and
//! tester addresses. The trailing byte is an additive checksum. A read job's
//! request is always short form (its UDS payload fits in 63 bytes), so
//! [`encode`] only emits the short form; [`decode`] additionally understands the
//! two LONG forms (`fmt & 0x3F == 0`, with an 8-bit or 16-bit length header) that
//! `TelLengthBmwFast` defines, for robustness against arbitrary inbound frames.
//!
//! ## The checksum is ADDITIVE, not XOR
//! A real ECU verifies the checksum of what we transmit. The checksum is the
//! wrapping `u8` SUM of every preceding telegram byte, per `CalcChecksumBmwFast`
//! (EdInterfaceBase.cs:933-941: `sum += data[i]`) — NOT an XOR. The length rules
//! mirror `TelLengthBmwFast` (EdInterfaceBase.cs:881-905).
//!
//! Cites are to the EDIABASLib reference (facts only; no code is copied).

/// A decoded BMW-FAST telegram: the addressing header plus the UDS payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Telegram {
    /// The destination address (an ECU address on a request, `0xF1` on a reply).
    pub target: u8,
    /// The source address (`0xF1` for the tester on a request, the ECU on a reply).
    pub source: u8,
    /// The raw UDS service bytes carried by the telegram (SID first).
    pub uds: Vec<u8>,
}

/// An error decoding a BMW-FAST telegram.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TelegramError {
    /// The buffer is too short to even read its length header.
    #[error("telegram too short to read its length header")]
    TooShort,
    /// The buffer is shorter than the length its header declares (incl. checksum).
    #[error("telegram length mismatch: header declares {declared} bytes, got {actual}")]
    BadLength {
        /// The full frame length the header implies, including the checksum byte.
        declared: usize,
        /// The actual buffer length received.
        actual: usize,
    },
    /// The trailing additive checksum did not match the recomputed value.
    #[error("bad additive checksum: expected 0x{expected:02X}, found 0x{found:02X}")]
    BadChecksum {
        /// The checksum recomputed over the telegram body.
        expected: u8,
        /// The checksum byte actually present in the frame.
        found: u8,
    },
}

/// Builds a short-form BMW-FAST telegram wrapping `uds` for `target`/`source`.
///
/// Produces `[0x80|len][target][source][uds…][checksum]`, where `len` is the
/// UDS payload length in the format byte's low 6 bits and `checksum` is the
/// additive (wrapping `u8`) sum of every preceding byte (`CalcChecksumBmwFast`,
/// EdInterfaceBase.cs:933-941). This is the exact frame a real ECU verifies.
///
/// # Examples
/// ```
/// use klartext_best::encode;
/// // A static 0x22 read of DID 0x4517 to ECU 0x12 from tester 0xF1.
/// let frame = encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
/// assert_eq!(&frame[..6], &[0x83, 0x12, 0xF1, 0x22, 0x45, 0x17]);
/// assert_eq!(frame[6], 0x04); // additive checksum, wrapping u8
/// ```
///
/// # Panics
/// Panics in debug builds if `uds.len() > 63`: the short-form length field is 6
/// bits, so a longer payload cannot be represented. A read job never emits a
/// longer single frame, so this is a precondition, not a runtime error.
pub fn encode(target: u8, source: u8, uds: &[u8]) -> Vec<u8> {
    // The short-form length lives in the format byte's low 6 bits (`0x3F`), so a
    // payload longer than 63 bytes cannot be represented; a read job never emits
    // one. Debug-only precondition per the documented contract.
    debug_assert!(
        uds.len() <= 0x3F,
        "BMW-FAST short form holds at most 63 UDS bytes, got {}",
        uds.len()
    );
    let len = (uds.len() & 0x3F) as u8;
    let mut frame = Vec::with_capacity(uds.len() + 4);
    frame.push(0x80 | len);
    frame.push(target);
    frame.push(source);
    frame.extend_from_slice(uds);
    let cksum = checksum(&frame);
    frame.push(cksum);
    frame
}

/// Decodes a BMW-FAST telegram into its header and UDS payload.
///
/// Computes the telegram length per `TelLengthBmwFast`
/// (EdInterfaceBase.cs:881-905) — short form or either long form — verifies the
/// additive checksum, and returns the `target`/`source` header with the UDS
/// payload sliced out after the (form-dependent) length header.
///
/// # Examples
/// ```
/// use klartext_best::{decode, encode};
/// let frame = encode(0x12, 0xF1, &[0x62, 0x45, 0x17]);
/// let tel = decode(&frame).unwrap();
/// assert_eq!(tel.target, 0x12);
/// assert_eq!(tel.source, 0xF1);
/// assert_eq!(tel.uds, vec![0x62, 0x45, 0x17]);
/// ```
///
/// # Errors
/// Returns [`TelegramError::TooShort`] if the buffer cannot supply the length
/// header, [`TelegramError::BadLength`] if it is shorter than the declared
/// telegram plus checksum, or [`TelegramError::BadChecksum`] if the trailing
/// additive checksum does not match.
pub fn decode(frame: &[u8]) -> Result<Telegram, TelegramError> {
    let (data_offset, tel_length) = header_and_length(frame)?;
    // The wire frame is the telegram body plus one trailing checksum byte. Unlike
    // `TelLengthBmwFast` (which clamps a partial buffer to its length), a codec
    // validating a complete frame must REJECT one that is short.
    let full_len = tel_length + 1;
    if frame.len() < full_len {
        return Err(TelegramError::BadLength {
            declared: full_len,
            actual: frame.len(),
        });
    }
    // `tel_length >= 4` for every accepted form, so the header bytes below and the
    // `data_offset..tel_length` payload slice are always in bounds here.
    let expected = checksum(&frame[..tel_length]);
    let found = frame[tel_length];
    if expected != found {
        return Err(TelegramError::BadChecksum { expected, found });
    }
    Ok(Telegram {
        target: frame[1],
        source: frame[2],
        uds: frame[data_offset..tel_length].to_vec(),
    })
}

/// Returns the UDS service byte (`uds[0]`) of a short-form telegram, if present.
///
/// A cheap peek that does not validate length or checksum, letting a caller
/// classify a frame by its service ID before deciding to fully decode it. Assumes
/// the short-form layout [`encode`] produces (UDS payload at index 3); returns
/// `None` if the frame is too short to hold a service byte.
pub fn peek_sid(frame: &[u8]) -> Option<u8> {
    // The short-form UDS payload begins at index 3; its first byte is the SID.
    frame.get(3).copied()
}

/// The additive BMW-FAST checksum: the wrapping `u8` sum of `bytes`.
///
/// Per `CalcChecksumBmwFast` (EdInterfaceBase.cs:933-941), `sum += data[i]` over
/// the telegram body — an additive sum, NOT an XOR. A real ECU verifies it, so
/// [`encode`] must produce it exactly.
fn checksum(bytes: &[u8]) -> u8 {
    bytes.iter().fold(0u8, |sum, &b| sum.wrapping_add(b))
}

/// Computes `(data_offset, tel_length)` for `frame` per `TelLengthBmwFast`.
///
/// `tel_length` is the telegram length WITHOUT the trailing checksum; `data_offset`
/// is where the UDS payload begins — 3 for the short form, 4 or 6 for the long
/// forms (the length-header widths). Mirrors `TelLengthBmwFast`
/// (EdInterfaceBase.cs:881-905) and `DataLengthBmwFast`'s offsets
/// (EdInterfaceBase.cs:907-931).
///
/// # Errors
/// Returns [`TelegramError::TooShort`] when the buffer cannot supply the bytes the
/// selected form needs to read its length.
fn header_and_length(frame: &[u8]) -> Result<(usize, usize), TelegramError> {
    let fmt = *frame.first().ok_or(TelegramError::TooShort)?;
    let short_len = usize::from(fmt & 0x3F);
    if short_len != 0 {
        // Short form: header is [fmt][target][source]; payload at index 3.
        return Ok((3, short_len + 3));
    }
    // Long form: index 3 is an 8-bit length, or 0 to select a 16-bit length in
    // the following two bytes.
    let len_byte = *frame.get(3).ok_or(TelegramError::TooShort)?;
    if len_byte == 0 {
        let hi = usize::from(*frame.get(4).ok_or(TelegramError::TooShort)?);
        let lo = usize::from(*frame.get(5).ok_or(TelegramError::TooShort)?);
        Ok((6, (hi << 8) + lo + 6))
    } else {
        Ok((4, usize::from(len_byte) + 4))
    }
}

#[cfg(test)]
mod tests {
    use super::{TelegramError, decode, encode, peek_sid};

    #[test]
    fn encode_matches_the_observed_static_read_telegram() {
        // Frozen contract: 83 12 F1 22 45 17 is a static 0x22 read of DID 0x4517
        // to target 0x12 from source 0xF1. 0x83 = 0x80|3 (three UDS bytes).
        // The additive checksum (CalcChecksumBmwFast, EdInterfaceBase.cs:933) of
        // [0x83,0x12,0xF1,0x22,0x45,0x17] = 0x83+0x12+0xF1+0x22+0x45+0x17 mod 256.
        let frame = encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
        assert_eq!(&frame[..6], &[0x83, 0x12, 0xF1, 0x22, 0x45, 0x17]);
        let sum = frame[..6].iter().fold(0u8, |a, &b| a.wrapping_add(b));
        assert_eq!(frame[6], sum);
        assert_eq!(frame.len(), 7);
    }

    #[test]
    fn decode_roundtrips_encode() {
        let frame = encode(0x12, 0xF1, &[0x62, 0x45, 0x17, 0x0A, 0xBC]);
        let t = decode(&frame).unwrap();
        assert_eq!(t.target, 0x12);
        assert_eq!(t.source, 0xF1);
        assert_eq!(t.uds, vec![0x62, 0x45, 0x17, 0x0A, 0xBC]);
    }

    #[test]
    fn decode_rejects_a_bad_additive_checksum() {
        let mut frame = encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
        let last = frame.len() - 1;
        frame[last] ^= 0xFF;
        assert!(matches!(
            decode(&frame),
            Err(TelegramError::BadChecksum { .. })
        ));
    }

    #[test]
    fn decode_rejects_a_truncated_frame() {
        assert!(matches!(
            decode(&[0x83, 0x12]),
            Err(TelegramError::BadLength { .. } | TelegramError::TooShort)
        ));
    }

    #[test]
    fn peek_sid_reads_the_uds_service_byte() {
        let frame = encode(0x12, 0xF1, &[0x2E, 0x10, 0x01]);
        assert_eq!(peek_sid(&frame), Some(0x2E));
    }

    #[test]
    fn peek_sid_is_none_when_no_service_byte_present() {
        assert_eq!(peek_sid(&[0x83, 0x12]), None);
    }

    #[test]
    fn decode_handles_the_8bit_long_form() {
        // fmt low-6 == 0 selects the long form; a non-zero byte at index 3 is an
        // 8-bit payload length (TelLengthBmwFast: telLength = dataBuffer[3] + 4,
        // dataOffset 4). Payload [62 45 17] ⇒ len byte 0x03; additive checksum.
        let frame = [0x80, 0x12, 0xF1, 0x03, 0x62, 0x45, 0x17, 0x44];
        let t = decode(&frame).unwrap();
        assert_eq!(t.target, 0x12);
        assert_eq!(t.source, 0xF1);
        assert_eq!(t.uds, vec![0x62, 0x45, 0x17]);
    }

    #[test]
    fn decode_handles_the_16bit_long_form() {
        // Long form with a zero byte at index 3 selects a 16-bit length at [4],[5]
        // (TelLengthBmwFast: telLength = (b4<<8)+b5+6, dataOffset 6). Payload
        // [62 45 17] ⇒ length 0x0003; additive checksum over the body.
        let frame = [0x80, 0x12, 0xF1, 0x00, 0x00, 0x03, 0x62, 0x45, 0x17, 0x44];
        let t = decode(&frame).unwrap();
        assert_eq!(t.target, 0x12);
        assert_eq!(t.source, 0xF1);
        assert_eq!(t.uds, vec![0x62, 0x45, 0x17]);
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "63")]
    fn encode_rejects_an_over_long_payload_in_debug() {
        // The short-form length field is 6 bits; a 64-byte payload cannot be
        // represented and trips the debug precondition.
        let _ = encode(0x12, 0xF1, &[0u8; 64]);
    }
}
