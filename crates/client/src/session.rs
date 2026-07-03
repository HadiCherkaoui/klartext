//! A managed, demultiplexed UDS session over one HSFZ connection.
//!
//! One TCP/HSFZ connection to the gateway carries requests to *many* ECU
//! addresses. A background reader task owns the read half and routes each
//! response frame to the pending request for that frame's **source address**
//! (HSFZ frames carry SRC/TGT both ways, and a response swaps them — verified
//! 2026-07-03: request `f4 12` draws a response `12 f4`). So requests to
//! different targets can be in flight at once over the single socket; at most one
//! request per target is outstanding at a time. A second background task sends the
//! TesterPresent keepalive (`3E 80`) so the link never lapses.
//!
//! Routing by source address — instead of by SID over a single stream, as the
//! M2 code did — means a late response from a timed-out probe can no longer be
//! mis-attributed to a later request. Within one target's stream the old hazard
//! remains (a stray keepalive NAK `7F 3E xx` arriving before the real response),
//! so the reader still filters a negative response by its echoed request SID.
//!
//! [verify live]: whether the ZGW tolerates *interleaved* requests to different
//! targets; the pcap is lockstep, and a scan concurrency of 1 degrades this to
//! strictly sequential probing.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use klartext_hsfz::{control, read_frame, write_frame, HsfzConnection, HsfzFrame};
use klartext_uds::{
    positive_response_sid, sid, tester_present_suppressed, Nrc, NRC_RESPONSE_PENDING,
};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::ClientError;

/// How often the background keepalive sends `3E 80` to the gateway.
///
/// Comfortably under the S3 inactivity timeout (~5 s, report §1.4); the capture
/// shows ISTA-like tooling sending `3E 80` at this cadence. [verify against capture].
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);

/// Max NRC 0x78 "response pending" ticks for one request before giving up.
const MAX_PENDING_TICKS: u32 = 10;

/// An upper bound on a single reader read, so a wedged socket eventually errors.
///
/// Not a per-request timeout — each request times out itself. This only stops the
/// reader from blocking forever on a half-open connection that never sends EOF.
const READER_IDLE_CAP: Duration = Duration::from_secs(3600);

/// What the reader delivers to a waiting request.
enum Delivery {
    /// The final outcome: a positive payload, or a terminal negative as an error.
    Final(Result<Vec<u8>, ClientError>),
    /// An NRC 0x78 for this target: keep waiting, re-arm the timeout.
    Pending,
}

/// One outstanding request: the SID it expects and where to deliver its outcome.
#[derive(Debug)]
struct PendingReq {
    /// The request's SID, used to recognise its positive/negative responses and
    /// skip a stray keepalive NAK for a different service.
    request_sid: u8,
    /// The channel the reader delivers this request's outcome on.
    tx: mpsc::UnboundedSender<Delivery>,
}

/// Per-target pending table shared between `request` and the reader task.
type Pending = Arc<Mutex<HashMap<u8, PendingReq>>>;

/// A held, demuxed UDS session: concurrent per-target requests plus a keepalive.
#[derive(Debug)]
pub struct Session {
    /// Write half shared with the keepalive task, locked per whole-frame write.
    write: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    /// Outstanding requests keyed by target address.
    pending: Pending,
    /// The background reader task, aborted on drop.
    reader: JoinHandle<()>,
    /// The background keepalive task, aborted on drop.
    keepalive: JoinHandle<()>,
    source: u8,
    read_timeout: Duration,
}

impl Drop for Session {
    fn drop(&mut self) {
        // `abort` only signals; the tasks stop at their next await. Aborting the
        // reader drops its senders, so any waiter still parked wakes with a closed
        // channel rather than hanging.
        self.reader.abort();
        self.keepalive.abort();
    }
}

impl Session {
    /// Open a managed session over `conn`; the keepalive targets `gateway`.
    pub fn open(conn: HsfzConnection, source: u8, gateway: u8) -> Self {
        Self::start(conn, source, gateway, KEEPALIVE_INTERVAL)
    }

    /// Open with an explicit keepalive interval (tests run it fast).
    fn start(conn: HsfzConnection, source: u8, gateway: u8, interval: Duration) -> Self {
        let (mut read, write, _peer, read_timeout) = conn.into_parts();
        let write = Arc::new(tokio::sync::Mutex::new(write));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let reader_pending = Arc::clone(&pending);
        let reader = tokio::spawn(async move {
            // Route frames until the connection closes or a read fatally errors.
            while let Ok(frame) = read_frame(&mut read, READER_IDLE_CAP).await {
                route_frame(&reader_pending, frame);
            }
            // Fail every waiter so no request hangs forever.
            let mut map = reader_pending.lock().expect("pending mutex poisoned");
            for (_target, req) in map.drain() {
                let _ = req
                    .tx
                    .send(Delivery::Final(Err(ClientError::ConnectionClosed)));
            }
        });

        let keepalive = spawn_keepalive(Arc::clone(&write), source, gateway, interval);
        Self {
            write,
            pending,
            reader,
            keepalive,
            source,
            read_timeout,
        }
    }

    /// Send a UDS request to `target` and return its response payload.
    ///
    /// Uses the connection's default read timeout.
    ///
    /// # Errors
    /// [`ClientError::RequestInFlight`] if `target` already has a request pending,
    /// [`ClientError::Hsfz`] on a transport or timeout error,
    /// [`ClientError::ConnectionClosed`] if the reader ended, and
    /// [`ClientError::Negative`] if the ECU rejects the request.
    pub async fn request(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ClientError> {
        self.request_with_timeout(target, uds, self.read_timeout)
            .await
    }

    /// As [`Session::request`], with an explicit per-request read timeout.
    ///
    /// Used by fast presence probes so an absent ECU costs `timeout`, not P2*.
    ///
    /// # Errors
    /// As [`Session::request`].
    pub async fn request_with_timeout(
        &self,
        target: u8,
        uds: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, ClientError> {
        let request_sid = uds.first().copied().unwrap_or_default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Register the pending slot; reject a second in-flight request per target.
        {
            let mut map = self.pending.lock().expect("pending mutex poisoned");
            if map.contains_key(&target) {
                return Err(ClientError::RequestInFlight { target });
            }
            map.insert(target, PendingReq { request_sid, tx });
        }
        // Send the frame. On write failure, clear the slot and surface the error.
        let frame = HsfzFrame::diagnostic(self.source, target, uds.to_vec());
        if let Err(error) = {
            let mut writer = self.write.lock().await;
            write_frame(&mut *writer, &frame).await
        } {
            self.pending
                .lock()
                .expect("pending mutex poisoned")
                .remove(&target);
            return Err(error.into());
        }
        // Await delivery; NRC 0x78 re-arms the timeout, bounded by MAX_PENDING_TICKS.
        let mut ticks = 0u32;
        loop {
            match tokio::time::timeout(timeout, rx.recv()).await {
                Ok(Some(Delivery::Final(result))) => {
                    self.forget(target);
                    return result;
                }
                Ok(Some(Delivery::Pending)) => {
                    ticks += 1;
                    if ticks > MAX_PENDING_TICKS {
                        self.forget(target);
                        return Err(read_timeout(timeout));
                    }
                }
                Ok(None) => {
                    self.forget(target);
                    return Err(ClientError::ConnectionClosed);
                }
                Err(_) => {
                    self.forget(target);
                    return Err(read_timeout(timeout));
                }
            }
        }
    }

    /// Move `target` into `session` (e.g. extended) via DiagnosticSessionControl.
    ///
    /// # Errors
    /// As [`Session::request`]; a rejected change surfaces as [`ClientError::Negative`].
    pub async fn enter_session(&self, target: u8, session: u8) -> Result<(), ClientError> {
        self.request(target, &klartext_uds::diagnostic_session_control(session))
            .await?;
        Ok(())
    }

    /// Drop the pending slot for `target` (idempotent).
    fn forget(&self, target: u8) {
        self.pending
            .lock()
            .expect("pending mutex poisoned")
            .remove(&target);
    }
}

/// The read-timeout error for a per-request deadline.
fn read_timeout(timeout: Duration) -> ClientError {
    ClientError::Hsfz(klartext_hsfz::Error::ReadTimeout { timeout })
}

/// Route one received frame to the pending request for its source address.
///
/// A non-diagnostic frame (a `0x02` ack, which the gateway sends for every
/// diagnostic frame) is skipped. A diagnostic frame is matched against the
/// waiter registered for its source ECU: the request's positive SID delivers the
/// payload; a negative for a *different* SID (a stray keepalive NAK) is skipped;
/// NRC 0x78 re-arms the waiter's timeout; any other negative delivers a typed
/// error. A frame with no waiter (a stray/late reply) is dropped.
fn route_frame(pending: &Pending, frame: HsfzFrame) {
    if frame.control != control::DIAGNOSTIC {
        return; // ack / keepalive echo / other
    }
    let Some((src, _tgt)) = frame.addr else {
        return;
    };
    let payload = frame.payload;

    let mut map = pending.lock().expect("pending mutex poisoned");
    // Copy the expected SID and release the borrow before any `remove`.
    let Some(request_sid) = map.get(&src).map(|req| req.request_sid) else {
        return; // no waiter for this ECU — stray/late reply
    };
    let expected_positive = positive_response_sid(request_sid);

    match payload.first().copied() {
        Some(byte) if byte == expected_positive => {
            if let Some(req) = map.remove(&src) {
                let _ = req.tx.send(Delivery::Final(Ok(payload)));
            }
        }
        Some(sid::NEGATIVE_RESPONSE) => {
            if payload.get(1).copied() != Some(request_sid) {
                return; // negative for another service (stray keepalive NAK) — skip
            }
            let nrc = payload.get(2).copied().unwrap_or_default();
            if nrc == NRC_RESPONSE_PENDING {
                if let Some(req) = map.get(&src) {
                    let _ = req.tx.send(Delivery::Pending); // keep the slot, keep waiting
                }
            } else if let Some(req) = map.remove(&src) {
                let _ = req.tx.send(Delivery::Final(Err(ClientError::Negative {
                    sid: request_sid,
                    nrc: Nrc::from(nrc),
                })));
            }
        }
        // A positive for a different SID, or an empty payload — stale, skip.
        _ => {}
    }
}

/// Spawn the background keepalive: send `3E 80` to `gateway` every `interval`.
fn spawn_keepalive(
    write: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    source: u8,
    gateway: u8,
    interval: Duration,
) -> JoinHandle<()> {
    let frame = HsfzFrame::diagnostic(source, gateway, tester_present_suppressed());
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use klartext_hsfz::{control, read_frame, write_frame, HsfzConnection, HsfzFrame};
    use tokio::net::TcpListener;

    /// A loopback gateway hosting several mock ECUs keyed by target address.
    ///
    /// `present` addresses answer `22 F1 90` (with a per-address VIN byte) and any
    /// `3E 00` with `7E 00`; absent addresses never reply. Keepalives are counted.
    /// Every reply swaps SRC/TGT, as the real gateway does.
    async fn spawn_multi_ecu_gateway(present: &[u8]) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let present: Vec<u8> = present.to_vec();
        let keepalives = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&keepalives);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                let (tester, ecu) = frame.addr.unwrap(); // tester -> ecu
                if frame.payload == [0x3E, 0x80] {
                    counter.fetch_add(1, Ordering::SeqCst); // keepalive
                    continue;
                }
                if !present.contains(&ecu) {
                    continue; // absent ECU: silence
                }
                let uds = match frame.payload.as_slice() {
                    [0x3E, 0x00] => vec![0x7E, 0x00],
                    [0x22, 0xF1, 0x90] => vec![0x62, 0xF1, 0x90, ecu], // 1-byte "VIN"
                    _ => continue,
                };
                let reply = HsfzFrame::diagnostic(ecu, tester, uds); // swap src/tgt
                let _ = write_frame(&mut stream, &reply).await;
            }
        });
        (addr, keepalives)
    }

    async fn open_session(addr: std::net::SocketAddr) -> Session {
        let conn = HsfzConnection::connect(
            addr.ip(),
            addr.port(),
            Duration::from_secs(2),
            Duration::from_secs(5),
        )
        .await
        .unwrap();
        Session::start(conn, 0xF4, 0x10, Duration::from_millis(50))
    }

    #[tokio::test]
    async fn routes_responses_to_the_right_target() {
        let (addr, _) = spawn_multi_ecu_gateway(&[0x12, 0x40]).await;
        let session = open_session(addr).await;
        let a = session.request(0x12, &[0x22, 0xF1, 0x90]).await.unwrap();
        let b = session.request(0x40, &[0x22, 0xF1, 0x90]).await.unwrap();
        assert_eq!(a, vec![0x62, 0xF1, 0x90, 0x12]);
        assert_eq!(b, vec![0x62, 0xF1, 0x90, 0x40]);
    }

    #[tokio::test]
    async fn concurrent_requests_to_distinct_targets_share_one_socket() {
        let (addr, _) = spawn_multi_ecu_gateway(&[0x12, 0x40, 0x60]).await;
        let session = open_session(addr).await;
        let (a, b, c) = tokio::join!(
            session.request(0x12, &[0x22, 0xF1, 0x90]),
            session.request(0x40, &[0x22, 0xF1, 0x90]),
            session.request(0x60, &[0x22, 0xF1, 0x90]),
        );
        assert_eq!(a.unwrap()[3], 0x12);
        assert_eq!(b.unwrap()[3], 0x40);
        assert_eq!(c.unwrap()[3], 0x60);
    }

    #[tokio::test]
    async fn absent_target_times_out_without_blocking_others() {
        let (addr, _) = spawn_multi_ecu_gateway(&[0x12]).await;
        let session = open_session(addr).await;
        // Absent 0x18 times out fast; 0x12 still answers.
        let absent = session
            .request_with_timeout(0x18, &[0x3E, 0x00], Duration::from_millis(150))
            .await;
        assert!(matches!(
            absent,
            Err(ClientError::Hsfz(klartext_hsfz::Error::ReadTimeout { .. }))
        ));
        let present = session.request(0x12, &[0x3E, 0x00]).await.unwrap();
        assert_eq!(present, vec![0x7E, 0x00]);
    }

    #[tokio::test]
    async fn keepalive_targets_the_gateway_during_idle() {
        let (addr, keepalives) = spawn_multi_ecu_gateway(&[0x12]).await;
        let session = open_session(addr).await;
        let _ = session.request(0x12, &[0x22, 0xF1, 0x90]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(180)).await;
        assert!(
            keepalives.load(Ordering::SeqCst) >= 1,
            "keepalive should fire when idle"
        );
    }

    // A stray keepalive NAK (`7F 3E 22`) from the target must NOT be delivered as
    // our read's response: route_frame skips a negative whose echoed SID differs
    // from the pending request's SID. (The single-target M2 hazard, preserved.)
    #[test]
    fn route_frame_skips_a_stray_keepalive_nack_then_delivers_the_real_response() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        pending.lock().unwrap().insert(
            0x12,
            PendingReq {
                request_sid: 0x22,
                tx,
            },
        );

        // Stray NAK to TesterPresent (SID 0x3E) from 0x12 — must be skipped.
        route_frame(
            &pending,
            HsfzFrame::diagnostic(0x12, 0xF4, vec![0x7F, 0x3E, 0x22]),
        );
        assert!(
            rx.try_recv().is_err(),
            "the stray NAK must not be delivered"
        );

        // The real 0x62 response — must be delivered.
        route_frame(
            &pending,
            HsfzFrame::diagnostic(0x12, 0xF4, vec![0x62, 0xF1, 0x90, 0xAB]),
        );
        match rx.try_recv() {
            Ok(Delivery::Final(Ok(payload))) => assert_eq!(payload, vec![0x62, 0xF1, 0x90, 0xAB]),
            other => panic!("expected the real response, got {}", describe(other)),
        }
    }

    // A negative response for OUR service surfaces as a typed NRC.
    #[test]
    fn route_frame_delivers_our_negative_as_a_typed_nrc() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (tx, mut rx) = mpsc::unbounded_channel();
        pending.lock().unwrap().insert(
            0x12,
            PendingReq {
                request_sid: 0x22,
                tx,
            },
        );
        route_frame(
            &pending,
            HsfzFrame::diagnostic(0x12, 0xF4, vec![0x7F, 0x22, 0x31]),
        );
        match rx.try_recv() {
            Ok(Delivery::Final(Err(ClientError::Negative {
                sid: 0x22,
                nrc: Nrc::RequestOutOfRange,
            }))) => {}
            other => panic!("expected a typed NRC, got {}", describe(other)),
        }
    }

    /// Render a `try_recv` outcome for a test panic message.
    fn describe(outcome: Result<Delivery, mpsc::error::TryRecvError>) -> &'static str {
        match outcome {
            Ok(Delivery::Final(Ok(_))) => "Final(Ok)",
            Ok(Delivery::Final(Err(_))) => "Final(Err)",
            Ok(Delivery::Pending) => "Pending",
            Err(_) => "nothing",
        }
    }
}
