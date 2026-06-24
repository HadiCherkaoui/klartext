//! A managed UDS session over one HSFZ connection.
//!
//! [`Session`] turns the M1 single-shot transport into a held conversation: it
//! splits the connection so a background task can send a TesterPresent keepalive
//! (`3E 80`) every [`KEEPALIVE_INTERVAL`] while the foreground is free to issue
//! requests, so a sequence of reads does not let the session time out.
//!
//! The keepalive's suppress-positive bit means it draws no positive response, but
//! the ECU *may* still answer a keepalive with a negative response on error. With
//! nobody reading between requests, such a stray `7F 3E xx` would sit buffered and
//! be misread as the answer to the next request. [`read_matching`] prevents that:
//! it accepts only a frame whose UDS SID matches the request (the expected
//! positive SID, or `7F <request-sid> <nrc>`), skipping anything else — the same
//! idea as skipping a non-diagnostic HSFZ frame, one layer up. [verify against
//! capture] for the exact keepalive-error behaviour.

use std::sync::Arc;
use std::time::Duration;

use klartext_hsfz::{HsfzConnection, HsfzFrame, control, read_frame, write_frame};
use klartext_uds::{
    NRC_RESPONSE_PENDING, Nrc, positive_response_sid, sid, tester_present_suppressed,
};
use tokio::io::AsyncRead;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::error::ClientError;

/// How often the background keepalive sends `3E 80`.
///
/// Comfortably under the S3 inactivity timeout (~5 s, report §1.4) so a
/// non-default session never lapses between requests. [verify against capture].
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);

/// Max NRC 0x78 "response pending" re-reads for one request before giving up.
const MAX_PENDING_RETRIES: u32 = 10;

/// A held UDS session: a foreground request path plus a background keepalive.
///
/// Only one request may be in flight at a time, enforced by `&mut self` on
/// [`Session::request`]. The keepalive task is aborted when the session is
/// dropped, so closing the session stops the background traffic.
#[derive(Debug)]
pub struct Session {
    /// Owned read half — the response reader; there is exactly one reader.
    read: OwnedReadHalf,
    /// Write half shared with the keepalive task, locked per whole-frame write.
    write: Arc<Mutex<OwnedWriteHalf>>,
    /// The background keepalive task, aborted on drop.
    keepalive: JoinHandle<()>,
    source: u8,
    target: u8,
    read_timeout: Duration,
}

impl Drop for Session {
    fn drop(&mut self) {
        // `abort` only signals; the task stops at its next await. Nothing reads
        // keepalive responses, so a cancelled in-flight write is harmless.
        self.keepalive.abort();
    }
}

impl Session {
    /// Open a managed session over `conn`, addressing `source` -> `target`.
    ///
    /// Spawns the keepalive immediately so even a read-only sequence holds the
    /// session. (A pure default session may not strictly time out — [verify
    /// against capture] — but BMW gateways can drop idle links, and the keepalive
    /// is safe thanks to [`read_matching`]'s SID filtering.)
    pub fn open(conn: HsfzConnection, source: u8, target: u8) -> Self {
        Self::start(conn, source, target, KEEPALIVE_INTERVAL)
    }

    /// Open with an explicit keepalive interval (used by tests to run it fast).
    fn start(conn: HsfzConnection, source: u8, target: u8, interval: Duration) -> Self {
        let (read, write, _peer, read_timeout) = conn.into_parts();
        let write = Arc::new(Mutex::new(write));
        let keepalive = spawn_keepalive(Arc::clone(&write), source, target, interval);
        Self {
            read,
            write,
            keepalive,
            source,
            target,
            read_timeout,
        }
    }

    /// Send a UDS request and return the matching UDS response payload.
    ///
    /// The write lock is released before reading, so the keepalive can still send
    /// while this call is blocked waiting for the response.
    ///
    /// # Errors
    /// Returns [`ClientError::Hsfz`] on a transport failure (including a read
    /// timeout) and [`ClientError::Negative`] if the ECU rejects the request.
    pub async fn request(&mut self, uds: &[u8]) -> Result<Vec<u8>, ClientError> {
        let request_sid = uds.first().copied().unwrap_or_default();
        let frame = HsfzFrame::diagnostic(self.source, self.target, uds);
        {
            let mut writer = self.write.lock().await;
            write_frame(&mut *writer, &frame).await?;
        } // release the write lock before the (potentially long) read
        read_matching(&mut self.read, request_sid, self.read_timeout).await
    }

    /// Move the ECU into `session` (e.g. extended) via DiagnosticSessionControl.
    ///
    /// # Errors
    /// As [`Session::request`]; a rejected session change surfaces as
    /// [`ClientError::Negative`].
    pub async fn enter_session(&mut self, session: u8) -> Result<(), ClientError> {
        // `request` already errors on a negative response and only returns on the
        // matching `50 ..` positive, so reaching here means the change took. The
        // ECU's P2/P2* timing in the payload is left at the configured default.
        self.request(&klartext_uds::diagnostic_session_control(session))
            .await?;
        Ok(())
    }
}

/// Spawn the background keepalive: send `3E 80` every `interval`.
fn spawn_keepalive(
    write: Arc<Mutex<OwnedWriteHalf>>,
    source: u8,
    target: u8,
    interval: Duration,
) -> JoinHandle<()> {
    let frame = HsfzFrame::diagnostic(source, target, tester_present_suppressed());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        // The first tick is immediate; consume it so we don't send at t=0.
        ticker.tick().await;
        loop {
            ticker.tick().await;
            let mut writer = write.lock().await;
            // Best-effort: if the link is gone, stop; the next request surfaces it.
            if write_frame(&mut *writer, &frame).await.is_err() {
                break;
            }
        }
    })
}

/// Read frames until one whose UDS SID matches `request_sid`, returning its payload.
///
/// Skips non-diagnostic HSFZ frames (acks, keepalive echoes) and diagnostic frames
/// whose UDS belongs to a *different* service (a stray keepalive NAK). On
/// `7F <request_sid> 78` (response pending) it keeps waiting, bounded by
/// [`MAX_PENDING_RETRIES`]; any other negative for `request_sid` is returned as a
/// typed [`ClientError::Negative`].
///
/// Free function over `impl AsyncRead` so the matching logic is testable without a
/// socket.
///
/// # Errors
/// Returns [`ClientError::Hsfz`] on a transport/timeout error and
/// [`ClientError::Negative`] on a negative response for `request_sid`.
async fn read_matching(
    reader: &mut (impl AsyncRead + Unpin),
    request_sid: u8,
    read_timeout: Duration,
) -> Result<Vec<u8>, ClientError> {
    let expected_positive = positive_response_sid(request_sid);
    let mut pending_retries = 0u32;
    loop {
        let frame = read_frame(reader, read_timeout).await?;
        if frame.control != control::DIAGNOSTIC {
            continue; // ack / keepalive / other — HSFZ-level skip (as M1 does)
        }
        let payload = frame.payload;
        match payload.first().copied() {
            // The positive response to our request.
            Some(byte) if byte == expected_positive => return Ok(payload),
            // A negative response — only ours counts.
            Some(sid::NEGATIVE_RESPONSE) => {
                if payload.get(1).copied() != Some(request_sid) {
                    continue; // negative for another service (stray keepalive NAK)
                }
                let nrc = payload.get(2).copied().unwrap_or_default();
                if nrc == NRC_RESPONSE_PENDING && pending_retries < MAX_PENDING_RETRIES {
                    pending_retries += 1;
                    continue; // keep waiting for the real response
                }
                return Err(ClientError::Negative {
                    sid: request_sid,
                    nrc: Nrc::from(nrc),
                });
            }
            // A positive for a different SID, or an empty payload — stale, skip.
            _ => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use tokio::net::TcpListener;

    // CORRECTION-#1 PROOF: a stray keepalive NAK (`7F 3E 22`) sits in the stream
    // ahead of the real read response (`62 F1 90 …`). The SID filter must skip the
    // NAK and return the real response, not mis-attribute the NAK to the read.
    #[tokio::test]
    async fn read_matching_skips_stray_keepalive_nack() {
        let stray = HsfzFrame::diagnostic(0x10, 0xF4, vec![0x7F, 0x3E, 0x22]); // NAK to 3E
        let mut real_uds = vec![0x62, 0xF1, 0x90];
        real_uds.extend_from_slice(b"WBA3B5C50EK123456");
        let real = HsfzFrame::diagnostic(0x10, 0xF4, real_uds.clone());

        let mut bytes = stray.encode();
        bytes.extend_from_slice(&real.encode());
        let mut reader: &[u8] = &bytes;

        // We asked for ReadDataByIdentifier (0x22); expect the 0x62 response.
        let got = read_matching(&mut reader, 0x22, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(got, real_uds);
    }

    // A negative response for OUR service is surfaced as a typed NRC.
    #[tokio::test]
    async fn read_matching_returns_our_negative_as_typed_nrc() {
        let nak = HsfzFrame::diagnostic(0x10, 0xF4, vec![0x7F, 0x22, 0x31]); // 0x22 rejected
        let bytes = nak.encode();
        let mut reader: &[u8] = &bytes;
        let err = read_matching(&mut reader, 0x22, Duration::from_secs(1))
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            ClientError::Negative {
                sid: 0x22,
                nrc: Nrc::RequestOutOfRange
            }
        ));
    }

    /// A loopback mock gateway: replies to reads, counts keepalive frames.
    async fn spawn_mock_gateway() -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let keepalives = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&keepalives);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            loop {
                let frame = match read_frame(&mut stream, Duration::from_secs(5)).await {
                    Ok(f) => f,
                    Err(_) => break,
                };
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                match frame.payload.as_slice() {
                    [0x3E, 0x80] => {
                        counter.fetch_add(1, Ordering::SeqCst); // keepalive, no reply
                    }
                    [0x22, 0xF1, 0x90] => {
                        let mut uds = vec![0x62, 0xF1, 0x90];
                        uds.extend_from_slice(b"WBA3B5C50EK123456");
                        let reply = HsfzFrame::diagnostic(0x10, 0xF4, uds);
                        write_frame(&mut stream, &reply).await.unwrap();
                    }
                    [0x19, 0x02, _mask] => {
                        let reply = HsfzFrame::diagnostic(
                            0x10,
                            0xF4,
                            vec![0x59, 0x02, 0xFF, 0x4A, 0x12, 0x34, 0x08],
                        );
                        write_frame(&mut stream, &reply).await.unwrap();
                    }
                    _ => {}
                }
            }
        });
        (addr, keepalives)
    }

    // A multi-read sequence with an idle gap holds the session, and the keepalive
    // fires in the background during the gap. Uses a short interval to stay fast.
    #[tokio::test]
    async fn keepalive_holds_a_multi_read_sequence() {
        let (addr, keepalives) = spawn_mock_gateway().await;
        let conn = HsfzConnection::connect(
            addr.ip(),
            addr.port(),
            Duration::from_secs(2),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        let mut session = Session::start(conn, 0xF4, 0x10, Duration::from_millis(50));

        let first = session.request(&[0x22, 0xF1, 0x90]).await.unwrap();
        assert_eq!(&first[..3], &[0x62, 0xF1, 0x90]);

        // Idle long enough for several keepalives, then read again.
        tokio::time::sleep(Duration::from_millis(220)).await;

        let second = session.request(&[0x19, 0x02, 0xFF]).await.unwrap();
        assert_eq!(second[0], 0x59);

        assert!(
            keepalives.load(Ordering::SeqCst) >= 1,
            "keepalive should fire during the idle gap"
        );
    }
}
