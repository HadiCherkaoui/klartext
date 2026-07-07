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
//! payload length for the SHORT form; when those bits are zero the frame is a
//! LONG form carrying an 8-bit or 16-bit length header instead. `target` and
//! `source` are the ECU and tester addresses. The trailing byte is an additive
//! checksum. Both [`encode`] and [`decode`] handle all three forms per
//! `TelLengthBmwFast`: a read job's request is always short, but an ECU's
//! multi-result response can exceed 63 bytes and need a long form.
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

/// Builds a BMW-FAST telegram wrapping `uds` for `target`/`source`.
///
/// Chooses the on-wire form by payload length, symmetric with [`decode`]. The
/// SHORT form (`uds.len() <= 63`) packs the length into the format byte's low 6
/// bits: `[0x80|len][target][source][uds…][checksum]`. The 8-BIT LONG form
/// (`64..=255`) clears those bits and carries a `u8` length at index 3:
/// `[0x80][target][source][len][uds…][checksum]`. The 16-BIT LONG form
/// (`256..=65535`) puts a `0x00` marker at index 3 and a big-endian `u16` length
/// at indices 4..6: `[0x80][target][source][0x00][len_hi][len_lo][uds…][checksum]`.
/// The checksum is the additive (wrapping `u8`) sum of every preceding byte
/// (`CalcChecksumBmwFast`, EdInterfaceBase.cs:933-941) in EVERY form, and the form
/// selection mirrors `TelLengthBmwFast` (EdInterfaceBase.cs:881-905). This is the
/// exact frame a real ECU verifies — a read job's request is always short, but an
/// ECU's multi-result response can exceed 63 bytes and need a long form.
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
/// Panics in debug builds if `uds.len() > 65535`: no BMW-FAST form can encode a
/// payload beyond a 16-bit length. No real ECU response approaches this, so it is
/// a precondition, not a runtime error.
pub fn encode(target: u8, source: u8, uds: &[u8]) -> Vec<u8> {
    // BMW-FAST caps a single telegram's payload at a 16-bit length; nothing this
    // codec frames (a read request or an ECU response) approaches that. Debug-only
    // precondition per the documented contract.
    debug_assert!(
        uds.len() <= 0xFFFF,
        "BMW-FAST frames at most 65535 UDS bytes, got {}",
        uds.len()
    );
    // Reserve for the largest header (6 bytes, the 16-bit form) plus the checksum.
    let mut frame = Vec::with_capacity(uds.len() + 7);
    if uds.len() <= 0x3F {
        // Short form: length in the format byte's low 6 bits; header 3 bytes.
        frame.push(0x80 | (uds.len() & 0x3F) as u8);
        frame.push(target);
        frame.push(source);
    } else if uds.len() <= 0xFF {
        // 8-bit long form: fmt low 6 bits clear, u8 length at index 3; header 4.
        frame.push(0x80);
        frame.push(target);
        frame.push(source);
        frame.push((uds.len() & 0xFF) as u8);
    } else {
        // 16-bit long form: 0x00 marker at index 3, big-endian u16 length at
        // indices 4..6; header 6 bytes.
        frame.push(0x80);
        frame.push(target);
        frame.push(source);
        frame.push(0x00);
        frame.push(((uds.len() >> 8) & 0xFF) as u8);
        frame.push((uds.len() & 0xFF) as u8);
    }
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

/// Decodes a VM-built REQUEST telegram into its header and UDS, checksum OPTIONAL.
///
/// The VM's `xsend` emits `[0x80|len][target][source][uds…]` WITHOUT the trailing
/// additive checksum: in EDIABAS the interface appends that on transmit, not the job
/// bytecode (`CalcChecksumBmwFast`, EdInterfaceBase.cs — see the module docs). The
/// live bridge speaks bare UDS onward to a client that frames HSFZ itself, so it
/// needs only the header + UDS and must accept a request whose checksum byte is
/// absent. Unlike [`decode`], this therefore does NOT require or verify a trailing
/// checksum: it parses the length header, requires the frame to hold the declared
/// telegram body, and slices the UDS payload — a present checksum byte is left off
/// the payload (the slice stops at `tel_length`), neither required nor verified.
///
/// # Errors
/// Returns [`TelegramError::TooShort`] if the buffer cannot supply the length
/// header, or [`TelegramError::BadLength`] if it is shorter than the declared
/// telegram body (checksum excluded).
pub(crate) fn decode_request(frame: &[u8]) -> Result<Telegram, TelegramError> {
    let (data_offset, tel_length) = header_and_length(frame)?;
    // The checksum is optional here, so the body alone must be present — not the
    // body-plus-checksum that `decode` demands. A shorter buffer is genuinely
    // truncated (the header declared a longer telegram than arrived).
    if frame.len() < tel_length {
        return Err(TelegramError::BadLength {
            declared: tel_length,
            actual: frame.len(),
        });
    }
    Ok(Telegram {
        target: frame[1],
        source: frame[2],
        uds: frame[data_offset..tel_length].to_vec(),
    })
}

/// Returns the UDS service byte (`uds[0]`) of ANY telegram form, if present.
///
/// A cheap peek that does not validate the declared length or any checksum,
/// letting a caller classify a frame by its service ID before deciding to fully
/// decode it. It computes the payload offset with the SAME header logic
/// [`decode`] and the request decode use — short form at index 3, 8-bit long
/// form at index 4, 16-bit long form at index 6 — so the byte it returns is
/// exactly the byte a decode will treat as the SID. That agreement is a SAFETY
/// property: the read-only gate classifies frames by this byte, and a long-form
/// write whose 8-bit LENGTH byte happens to equal a read SID must classify by
/// its true SID at index 4, never by the length byte at index 3 (the Task 8
/// review's gate bypass). Returns `None` when the frame is too short to locate
/// or hold its service byte — the gate treats that as a hard error, not a pass.
pub fn peek_sid(frame: &[u8]) -> Option<u8> {
    // One source of truth for the payload offset (`header_and_length`), so the
    // peeked SID can never disagree with the byte a decode slices as `uds[0]`.
    let (data_offset, _) = header_and_length(frame).ok()?;
    frame.get(data_offset).copied()
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
    use super::{Telegram, TelegramError, decode, decode_request, encode, peek_sid};

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
    fn decode_request_accepts_the_checksum_less_vm_frame() {
        // The VM's `xsend` emits the checksum-LESS short frame (the interface, not
        // the job bytecode, appends the additive checksum). `decode_request` accepts
        // it and slices the UDS, where the strict `decode` rejects it as too short.
        let vm = [0x83, 0x12, 0xF1, 0x22, 0x45, 0x17];
        assert!(matches!(decode(&vm), Err(TelegramError::BadLength { .. })));
        let t = decode_request(&vm).unwrap();
        assert_eq!(t.target, 0x12);
        assert_eq!(t.source, 0xF1);
        assert_eq!(t.uds, vec![0x22, 0x45, 0x17]);
    }

    #[test]
    fn decode_request_ignores_a_present_checksum_and_rejects_truncation() {
        // A fully-framed frame still decodes (the trailing checksum is left off the
        // payload, not verified); a frame shorter than its declared body is rejected.
        let framed = encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
        assert_eq!(decode_request(&framed).unwrap().uds, vec![0x22, 0x45, 0x17]);
        assert!(matches!(
            decode_request(&[0x83, 0x12]),
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
    fn peek_sid_reads_the_true_sid_of_every_telegram_form() {
        // Form-awareness (the Task 8 review's gate-bypass fix): the peeked byte
        // must be the SAME byte `decode_request` slices as `uds[0]`, whatever the
        // form. Short form: SID at index 3.
        let short = encode(0x12, 0xF1, &[0x2E, 0x10, 0x01]);
        assert_eq!(peek_sid(&short), Some(0x2E));
        // 8-bit long form (fmt low-6 == 0, non-zero length at index 3): SID at
        // index 4 — NOT the length byte, even when that byte (0x22 = a 34-byte
        // payload) happens to spell a read SID. This exact shape bypassed the gate.
        let mut long8 = vec![0x80, 0x12, 0xF1, 0x22, 0x2E, 0x10, 0x01];
        long8.resize(4 + 0x22, 0x00);
        assert_eq!(peek_sid(&long8), Some(0x2E));
        // 16-bit long form (zero at index 3, length at [4][5]): SID at index 6.
        let long16 = [0x80, 0x12, 0xF1, 0x00, 0x00, 0x03, 0x2E, 0x10, 0x01];
        assert_eq!(peek_sid(&long16), Some(0x2E));
        // A long-form frame cut before its SID offset peeks None, never a guess.
        assert_eq!(peek_sid(&[0x80, 0x12, 0xF1, 0x22]), None);
        assert_eq!(peek_sid(&[0x80, 0x12, 0xF1, 0x00, 0x00, 0x03]), None);
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

    #[test]
    fn encode_decode_roundtrips_the_short_form() {
        // A 63-byte payload is the largest the 6-bit short length can hold; the
        // format byte packs the length in its low 6 bits (0x80 | 63 = 0xBF).
        let uds: Vec<u8> = (0u8..63).collect();
        let frame = encode(0xF1, 0x12, &uds);
        assert_eq!(frame[0], 0x80 | 63);
        assert_eq!(&frame[1..3], &[0xF1, 0x12]);
        assert_eq!(frame.len(), 3 + 63 + 1); // header + payload + checksum
        let decoded = decode(&frame).unwrap();
        assert_eq!(
            decoded,
            Telegram {
                target: 0xF1,
                source: 0x12,
                uds,
            }
        );
    }

    #[test]
    fn encode_decode_roundtrips_the_8bit_long_form() {
        // 100 bytes exceeds the 6-bit short length, so encode emits the 8-bit long
        // form: fmt 0x80 (low 6 bits clear), the u8 length at index 3, header 4.
        let uds: Vec<u8> = (0u8..100).collect();
        let frame = encode(0xF1, 0x12, &uds);
        assert_eq!(frame[0], 0x80);
        assert_eq!(&frame[1..3], &[0xF1, 0x12]);
        assert_eq!(frame[3], 100); // the 8-bit payload length
        assert_eq!(frame.len(), 4 + 100 + 1);
        let decoded = decode(&frame).unwrap();
        assert_eq!(
            decoded,
            Telegram {
                target: 0xF1,
                source: 0x12,
                uds,
            }
        );
    }

    #[test]
    fn encode_decode_roundtrips_the_16bit_long_form() {
        // 300 bytes exceeds a u8 length, so encode emits the 16-bit long form: fmt
        // 0x80, a 0x00 marker at index 3, the big-endian u16 length at [4],[5],
        // header 6. 300 = 0x012C.
        let uds: Vec<u8> = (0..300u16).map(|i| i as u8).collect();
        let frame = encode(0xF1, 0x12, &uds);
        assert_eq!(frame[0], 0x80);
        assert_eq!(&frame[1..3], &[0xF1, 0x12]);
        assert_eq!(frame[3], 0x00); // marks the 16-bit length that follows
        assert_eq!(&frame[4..6], &[0x01, 0x2C]); // 300, big-endian
        assert_eq!(frame.len(), 6 + 300 + 1);
        let decoded = decode(&frame).unwrap();
        assert_eq!(
            decoded,
            Telegram {
                target: 0xF1,
                source: 0x12,
                uds,
            }
        );
    }

    #[cfg(debug_assertions)]
    #[test]
    #[should_panic(expected = "65535")]
    fn encode_rejects_a_payload_beyond_a_u16_length_in_debug() {
        // Every payload up to a 16-bit length frames; a longer one cannot be
        // represented in any BMW-FAST form. No real ECU response approaches this.
        let _ = encode(0x12, 0xF1, &vec![0u8; 0x1_0000]);
    }
}
