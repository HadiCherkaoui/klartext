//! HSFZ frame encode/decode — pure, no I/O, fully unit-testable.
//!
//! Wire layout (big-endian), from `docs/protocol-reference.md` §2.1:
//!
//! ```text
//! [ LENGTH : u32 ] [ CONTROL : u16 ] [ body : LENGTH bytes ]
//! ```
//!
//! `LENGTH` counts only the bytes AFTER the 6-byte length+control header. For a
//! diagnostic frame (control 0x01/0x02) the body is `[SRC][TGT][UDS...]`, so
//! `LENGTH = 2 + len(UDS)`; for a bare discovery frame (control 0x11) the body
//! is empty, so `LENGTH = 0`. Total wire size = `6 + LENGTH`.
//!
//! The report contradicts itself on whether the control word is counted (§2.1
//! line 167 vs line 173). The bare discovery datagram `00 00 00 00 00 11`
//! (`LENGTH = 0` *with* a control word present) is decisive: the control word is
//! NOT counted. This matches Scapy's `post_build` (`len(pay) + 2`). [verified
//! 2026-07-03] against a real F20 capture — e.g. the VIN response
//! `00 00 00 16 00 01 12 f4 62 f1 90 <17-byte VIN>` has `LENGTH = 0x16 = 2+3+17`,
//! and responses swap SRC/TGT (request `f4 12` → response `12 f4`).

use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::time::timeout;

use crate::Error;

/// Header bytes before the length-counted body: `u32` length + `u16` control.
pub const HEADER_LEN: usize = 6;

/// Defensive cap on a decoded `LENGTH` before we trust it. A diagnostic exchange
/// is at most a few KB; a value near this cap means a misframe (wrong endianness
/// or the off-by-two length convention). Rejecting it up front turns a potential
/// hang (a read waiting forever for bytes that never come) into a clear error.
pub const MAX_FRAME_LEN: u32 = 64 * 1024;

/// HSFZ control words (message types), from §2.2.
pub mod control {
    /// Diagnostic message (carries UDS plus SRC/TGT). TCP 6801.
    pub const DIAGNOSTIC: u16 = 0x0001;
    /// Acknowledge / echo of a diagnostic message.
    pub const ACK: u16 = 0x0002;
    /// Vehicle identification / announcement (discovery). UDP 6811.
    pub const IDENTIFICATION: u16 = 0x0011;
    /// Keepalive / alive check.
    pub const ALIVE_CHECK: u16 = 0x0012;
}

/// One HSFZ frame.
///
/// `addr` holds the (source, target) logical addresses when the control word
/// carries them — diagnostic frames (0x01/0x02) and the 2-byte alive check
/// (0x12). Other control words (identification 0x11, error frames, …) keep all
/// their bytes in `payload` and leave `addr` as `None`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HsfzFrame {
    pub control: u16,
    pub addr: Option<(u8, u8)>,
    pub payload: Vec<u8>,
}

impl HsfzFrame {
    /// A diagnostic (control 0x01) frame carrying `uds` from `source` to `target`.
    pub fn diagnostic(source: u8, target: u8, uds: impl Into<Vec<u8>>) -> Self {
        Self {
            control: control::DIAGNOSTIC,
            addr: Some((source, target)),
            payload: uds.into(),
        }
    }

    /// The bare identification/discovery request (control 0x11, empty body) —
    /// the verbatim `00 00 00 00 00 11` datagram.
    pub fn identification_request() -> Self {
        Self {
            control: control::IDENTIFICATION,
            addr: None,
            payload: Vec::new(),
        }
    }

    /// Does this control word carry SRC/TGT bytes ahead of the payload? Mirrors
    /// Scapy's `_has_srctgt_addrs`: diagnostic (0x01/0x02) always, and the alive
    /// check (0x12) only in its 2-byte form.
    fn control_has_addrs(control: u16, body_len: usize) -> bool {
        matches!(control, control::DIAGNOSTIC | control::ACK)
            || (control == control::ALIVE_CHECK && body_len == 2)
    }

    /// Encode to wire bytes: `[len:u32 BE][control:u16 BE][body]`.
    pub fn encode(&self) -> Vec<u8> {
        let mut body = Vec::with_capacity(2 + self.payload.len());
        if let Some((src, tgt)) = self.addr {
            body.push(src);
            body.push(tgt);
        }
        body.extend_from_slice(&self.payload);

        let mut out = Vec::with_capacity(HEADER_LEN + body.len());
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(&self.control.to_be_bytes());
        out.extend_from_slice(&body);
        out
    }

    /// Decode one complete frame from the front of `buf`.
    ///
    /// `buf` must hold the whole frame (`6 + LENGTH` bytes). Returns an error —
    /// never panics — on a buffer too short for the header, an implausibly large
    /// length, a body shorter than the length claims, or a diagnostic frame
    /// missing its address bytes.
    pub fn decode(buf: &[u8]) -> Result<HsfzFrame, Error> {
        if buf.len() < HEADER_LEN {
            return Err(Error::Truncated {
                have: buf.len(),
                need: HEADER_LEN,
            });
        }
        let length = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
        if length > MAX_FRAME_LEN {
            return Err(Error::FrameTooLarge { length });
        }
        let control = u16::from_be_bytes([buf[4], buf[5]]);

        let body_len = length as usize;
        let need = HEADER_LEN + body_len;
        if buf.len() < need {
            return Err(Error::Truncated {
                have: buf.len(),
                need,
            });
        }
        let body = &buf[HEADER_LEN..need];

        let (addr, payload) = if Self::control_has_addrs(control, body_len) {
            if body_len < 2 {
                // An addressed control word with no room for SRC/TGT — malformed.
                return Err(Error::Truncated {
                    have: need,
                    need: HEADER_LEN + 2,
                });
            }
            (Some((body[0], body[1])), body[2..].to_vec())
        } else {
            (None, body.to_vec())
        };

        Ok(HsfzFrame {
            control,
            addr,
            payload,
        })
    }
}

/// Read one HSFZ frame from an async reader, reassembling across TCP segments.
///
/// Reads the 4-byte length, sanity-caps it against [`MAX_FRAME_LEN`] (so a
/// misframed length can't make the read block forever), then reads the control
/// word + body and hands the whole buffer to [`HsfzFrame::decode`]. Uses
/// `read_exact` throughout — never a single `read` that assumes one frame per
/// segment. Each underlying read is bounded by `read_timeout`.
///
/// This is the single source of frame reassembly: the [`crate::HsfzConnection`]
/// one-shot path and a managed session built on a split read half both call it,
/// so the wire framing lives in exactly one place.
///
/// # Errors
/// Returns [`Error::ReadTimeout`] if a read does not complete within
/// `read_timeout`, [`Error::FrameTooLarge`] if the length exceeds the cap,
/// [`Error::Truncated`] if the body is malformed, and [`Error::Io`] on a socket
/// error (including a clean EOF mid-frame).
pub async fn read_frame(
    reader: &mut (impl AsyncRead + Unpin),
    read_timeout: Duration,
) -> Result<HsfzFrame, Error> {
    let mut len_buf = [0u8; 4];
    read_exact_timed(reader, &mut len_buf, read_timeout).await?;
    let length = u32::from_be_bytes(len_buf);
    if length > MAX_FRAME_LEN {
        return Err(Error::FrameTooLarge { length });
    }

    // The length-counted region is preceded by the 2-byte control word.
    let rest_len = 2 + length as usize;
    let mut rest = vec![0u8; rest_len];
    read_exact_timed(reader, &mut rest, read_timeout).await?;

    // Hand the whole frame to the pure decoder (single source of truth).
    let mut whole = Vec::with_capacity(HEADER_LEN + length as usize);
    whole.extend_from_slice(&len_buf);
    whole.extend_from_slice(&rest);
    HsfzFrame::decode(&whole)
}

/// Encode `frame` and write it to an async writer, flushing before returning.
///
/// The counterpart to [`read_frame`]: both the [`crate::HsfzConnection`] one-shot
/// path and a managed session's shared write half send through here, so frame
/// encoding lives in one place. One `write_all` of the whole frame means a
/// concurrent sender (e.g. a keepalive) cannot interleave bytes mid-frame.
///
/// # Errors
/// Returns [`Error::Io`] if the write or flush fails.
pub async fn write_frame(
    writer: &mut (impl AsyncWrite + Unpin),
    frame: &HsfzFrame,
) -> Result<(), Error> {
    let bytes = frame.encode();
    writer.write_all(&bytes).await.map_err(Error::Io)?;
    writer.flush().await.map_err(Error::Io)?;
    Ok(())
}

/// `read_exact` bounded by `read_timeout`, mapping a timeout to a clear error.
async fn read_exact_timed(
    reader: &mut (impl AsyncRead + Unpin),
    buf: &mut [u8],
    read_timeout: Duration,
) -> Result<(), Error> {
    match timeout(read_timeout, reader.read_exact(buf)).await {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(source)) => Err(Error::Io(source)),
        Err(_) => Err(Error::ReadTimeout {
            timeout: read_timeout,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // VERBATIM: `00 00 00 00 00 11` is the one fully-verbatim wire datagram in
    // the report (§2.5). It also proves the decisive property below.
    #[test]
    fn discovery_request_round_trips_verbatim() {
        let f = HsfzFrame::identification_request();
        let bytes = f.encode();
        assert_eq!(bytes, vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x11]);
        assert_eq!(HsfzFrame::decode(&bytes).unwrap(), f);
    }

    #[test]
    fn length_excludes_the_control_word() {
        // The tiebreaker: a bare control word with no body must encode LENGTH=0.
        let bytes = HsfzFrame::identification_request().encode();
        let length = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        assert_eq!(length, 0, "control word must NOT be counted in LENGTH");
    }

    // DERIVED: HSFZ-wrapped frames built from the report's framing rules. The
    // UDS byte strings inside are verbatim from the report; the wrapping depends
    // on the (capture-unverified) length convention. [verify against capture]
    #[test]
    fn encode_tester_present_derived() {
        // 3E 00 to ZGW: LENGTH = 2 (src+tgt) + 2 (uds) = 4.
        let f = HsfzFrame::diagnostic(0xF4, 0x10, vec![0x3E, 0x00]);
        assert_eq!(
            f.encode(),
            vec![0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0xF4, 0x10, 0x3E, 0x00]
        );
    }

    #[test]
    fn encode_dsc_extended_derived() {
        let f = HsfzFrame::diagnostic(0xF4, 0x10, vec![0x10, 0x03]);
        assert_eq!(
            f.encode(),
            vec![0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0xF4, 0x10, 0x10, 0x03]
        );
    }

    #[test]
    fn decode_positive_response_derived() {
        // Response with SRC/TGT swapped (ZGW 0x10 -> tester 0xF4), UDS 7E 00.
        let bytes = vec![0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0x10, 0xF4, 0x7E, 0x00];
        let f = HsfzFrame::decode(&bytes).unwrap();
        assert_eq!(f.control, control::DIAGNOSTIC);
        assert_eq!(f.addr, Some((0x10, 0xF4)));
        assert_eq!(f.payload, vec![0x7E, 0x00]);
    }

    #[test]
    fn encode_decode_round_trip_derived() {
        let f = HsfzFrame::diagnostic(0xF4, 0x10, vec![0x10, 0x03]);
        assert_eq!(HsfzFrame::decode(&f.encode()).unwrap(), f);
    }

    #[test]
    fn decode_truncated_body_is_error_not_panic() {
        // Header claims LENGTH=4 but only one body byte is present.
        let bytes = vec![0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0xF4];
        assert!(matches!(
            HsfzFrame::decode(&bytes),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn decode_oversized_length_is_error_not_panic() {
        let bytes = vec![0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x01];
        assert!(matches!(
            HsfzFrame::decode(&bytes),
            Err(Error::FrameTooLarge { .. })
        ));
    }

    #[test]
    fn decode_too_short_for_header_is_error() {
        assert!(matches!(
            HsfzFrame::decode(&[0x00, 0x00]),
            Err(Error::Truncated { .. })
        ));
    }

    #[test]
    fn decode_diagnostic_without_addresses_is_error() {
        // control 0x01 but LENGTH=1 — no room for SRC/TGT.
        let bytes = vec![0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0xAA];
        assert!(matches!(
            HsfzFrame::decode(&bytes),
            Err(Error::Truncated { .. })
        ));
    }

    // `read_frame` reassembles a whole frame from a byte source. A `&[u8]` is a
    // tokio `AsyncRead`, standing in for a socket without any I/O.
    #[tokio::test]
    async fn read_frame_round_trips_a_diagnostic_frame() {
        let frame = HsfzFrame::diagnostic(0x10, 0xF4, vec![0x7E, 0x00]);
        let bytes = frame.encode();
        let mut reader: &[u8] = &bytes;
        let read = read_frame(&mut reader, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(read, frame);
        assert!(reader.is_empty(), "the whole frame should be consumed");
    }

    // A reader that ends before the body completes is a clean error, not a hang.
    #[tokio::test]
    async fn read_frame_on_truncated_source_errors() {
        // LENGTH=4 promised, but only the header + one body byte are present.
        let truncated = [0x00, 0x00, 0x00, 0x04, 0x00, 0x01, 0xF4];
        let mut reader: &[u8] = &truncated;
        let result = read_frame(&mut reader, Duration::from_secs(1)).await;
        assert!(matches!(result, Err(Error::Io(_))));
    }
}
