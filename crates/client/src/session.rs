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
//! M2 code did — means a response for one ECU can never be mis-attributed to a
//! request for a *different* ECU. Two same-target hazards remain and are handled:
//! a stray keepalive NAK (`7F 3E xx`) is skipped by filtering a negative response
//! on its echoed request SID; and a *late* response to a timed-out request (which
//! the wire cannot distinguish from the next same-target request's response, as
//! HSFZ carries no per-request id) is caught one layer up — [`DiagnosticClient`]
//! validates the echoed DID against the requested one and rejects a mismatch as
//! [`ClientError::UnexpectedDid`]. The residual case a caller must know: a *same
//! DID* read repeated on one ECU after the first timed out could return the first
//! read's (stale) value; treat a read that follows a timeout on that ECU as
//! suspect. Reads normally answer within P2 (~50 ms), so a timeout means the ECU
//! is not answering — the window is narrow.
//!
//! ANSWERED 2026-07-18 — the ZGW does **not** tolerate interleaving well. The
//! `car-session-1` capture settles what was flagged here as `[verify live]`: on one
//! car in one session, a strictly sequential ident sweep lost 0 of 388 requests
//! while a whole-car fault sweep at an in-flight depth of 3–7 lost 10 of 32 (31 %),
//! five of them refused at the HSFZ admission layer (never acknowledged). The
//! per-target demux below is still correct and still useful — the BEST/2 VM's jobs
//! legitimately interleave — but the whole-car sweep no longer fans out
//! ([`crate::DiagnosticClient::scan_faults`]). See the P1 resilience research, §0.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use klartext_hsfz::{HsfzConnection, HsfzFrame, control, read_frame, write_frame};
use klartext_uds::{
    NRC_RESPONSE_PENDING, Nrc, P2_STAR_SERVER_MAX_DEFAULT_MS, is_retry_safe, positive_response_sid,
    sid, tester_present_suppressed,
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

/// How many times a failed exchange is automatically repeated.
///
/// ISTA parity: `EDIABAS.INI:31` `RetryComm = 1`, documented at `:403-408` as
/// "Repeat failed communication automatically (1x)". ISTA's *other* retry layer —
/// the C# `ECUKom.apiJob` loop at
/// `BMW.Rheingold.VehicleCommunication.ECUKom.decompiled.cs:1440` — never runs in a
/// stock install: its counter starts at 1 and the shipped `RetryCount` is 1, so
/// `1 < 1` is false. All of ISTA's real retry behaviour is this one native repeat.
///
/// What the native core repeats, and at which layer, is NOT DETERMINABLE — the
/// mechanism lives in `XEnet32/64.dll`, native PE that `ilspycmd` cannot read. So
/// klartext matches the observable count and nothing more.
const RETRY_COMM: u32 = 1;

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
    /// A per-request id, so cleanup only ever evicts *this* request's slot — never
    /// a newer same-target request that reused the address after this one finished.
    generation: u64,
    /// The channel the reader delivers this request's outcome on.
    tx: mpsc::UnboundedSender<Delivery>,
    /// Set once the gateway's HSFZ `0x02` acknowledge for this request is seen.
    /// Shared with the waiting request, which outlives the slot. Observability
    /// only — see [`note_ack`].
    acked: Arc<AtomicBool>,
}

/// Removes a request's pending slot on drop, so a cancelled request future (or any
/// early return) can never leak the slot and wedge the target with `RequestInFlight`.
///
/// The generation check makes cleanup precise: if the reader already delivered and
/// a *newer* request reused the target address, this guard leaves that newer slot
/// alone.
struct SlotGuard<'a> {
    pending: &'a Pending,
    target: u8,
    generation: u64,
}

impl Drop for SlotGuard<'_> {
    fn drop(&mut self) {
        let mut map = self.pending.lock().expect("pending mutex poisoned");
        if map
            .get(&self.target)
            .is_some_and(|req| req.generation == self.generation)
        {
            map.remove(&self.target);
        }
    }
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
    /// Mints a unique generation per request for precise slot cleanup.
    next_generation: AtomicU64,
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
        let (mut read, write, read_timeout) = conn.into_parts();
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
            next_generation: AtomicU64::new(0),
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
    /// Lets a caller override the connection's default read timeout for a single
    /// request (e.g. a shorter deadline for an ECU that may not answer). A
    /// retry-safe request that times out is repeated once; see [`RETRY_COMM`].
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
        // A retry re-sends the identical bytes, so it is only offered for a service
        // that can absorb being sent twice. Every write — a clear, an actuation —
        // gets exactly one attempt, and an ambiguous timeout on one is surfaced to
        // the human rather than silently repeated.
        let budget = if is_retry_safe(request_sid) {
            RETRY_COMM
        } else {
            0
        };
        let mut retries = 0;
        loop {
            let outcome = self.request_once(target, uds, timeout).await;
            // ONLY a timeout is retried. A negative response is not a failed
            // exchange: the ECU received the request, decided, and answered. ISTA
            // agrees — its retry condition is `!IsDone()` (`ECUJob.cs:60-75`), and a
            // job that returns an `ERROR_ECU_NACK` result *is* done, however
            // unwelcome the result. Nor is a dead socket retried: ISTA splits
            // `IFH-0009 NO RESPONSE FROM CONTROLUNIT` (an ordinary job failure,
            // `ECUKom.decompiled.cs:1557`) from `NET-0014 CONNECTION ABORTED`, which
            // routes to its connection-loss flow instead of another attempt
            // (`EcuKomServiceDlgImpl.cs:457`).
            let silent = matches!(
                outcome,
                Err(ClientError::Hsfz(klartext_hsfz::Error::ReadTimeout { .. }))
            );
            if !silent || retries >= budget {
                return outcome;
            }
            retries += 1;
            tracing::debug!(
                "HSFZ retry {}/{} tgt={:#04X} sid={:#04X} after {:?} of silence",
                retries,
                budget,
                target,
                request_sid,
                timeout
            );
        }
    }

    /// One request/response exchange: no retry, one write, one awaited outcome.
    async fn request_once(
        &self,
        target: u8,
        uds: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, ClientError> {
        let request_sid = uds.first().copied().unwrap_or_default();
        let generation = self.next_generation.fetch_add(1, Ordering::Relaxed);
        let acked = Arc::new(AtomicBool::new(false));
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Register the pending slot; reject a second in-flight request per target.
        {
            let mut map = self.pending.lock().expect("pending mutex poisoned");
            if map.contains_key(&target) {
                return Err(ClientError::RequestInFlight { target });
            }
            map.insert(
                target,
                PendingReq {
                    request_sid,
                    generation,
                    tx,
                    acked: Arc::clone(&acked),
                },
            );
        }
        // The guard removes this slot on every exit path — normal return, an early
        // error, or the future being cancelled mid-await — so a target can never
        // stay wedged with a leaked slot.
        let _guard = SlotGuard {
            pending: &self.pending,
            target,
            generation,
        };
        // Send the frame; on write failure the guard cleans up as we return.
        // Trace the raw UDS bytes (off unless RUST_LOG enables trace) — the on-car
        // capture path for confirming derived frame layouts. See docs/on-car-verification-protocol.md.
        tracing::trace!(
            "HSFZ TX src={:#04X} tgt={:#04X} {}",
            self.source,
            target,
            hex_frame(uds)
        );
        let frame = HsfzFrame::diagnostic(self.source, target, uds.to_vec());
        {
            let mut writer = self.write.lock().await;
            write_frame(&mut *writer, &frame).await?;
        }
        // Await delivery. `timeout` bounds only the wait for the ECU to say
        // *anything*; once it answers NRC 0x78 ("received, still working") the
        // tester owes it the longer ISO 14229-2 P2* budget instead, re-armed per
        // tick and bounded by MAX_PENDING_TICKS. Conflating the two would let the
        // ENET-parity initial timeout (1200 ms) silently shorten P2* to match.
        let mut ticks = 0u32;
        let mut deadline = timeout;
        let outcome = loop {
            match tokio::time::timeout(deadline, rx.recv()).await {
                Ok(Some(Delivery::Final(result))) => break result,
                Ok(Some(Delivery::Pending)) => {
                    ticks += 1;
                    if ticks > MAX_PENDING_TICKS {
                        break Err(read_timeout(deadline));
                    }
                    deadline = Duration::from_millis(P2_STAR_SERVER_MAX_DEFAULT_MS);
                }
                Ok(None) => break Err(ClientError::ConnectionClosed),
                Err(_) => break Err(read_timeout(deadline)),
            }
        };
        // The gateway acknowledged 1 507 of 1 517 diagnostic frames in the
        // `car-session-1` capture (99.34 %, median 0.59 ms) — and five of the ten
        // un-acked frames were exactly the whole-car sweep's lost requests. A
        // missing ack is therefore a sharp hint that the gateway's admission
        // control refused the frame, and worth surfacing without a pcap.
        if !acked.load(Ordering::Relaxed) {
            tracing::debug!(
                "HSFZ no 0x02 ack for tgt={:#04X} sid={:#04X} ({})",
                target,
                request_sid,
                if outcome.is_ok() {
                    "answered anyway"
                } else {
                    "and it failed"
                }
            );
        }
        outcome
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
}

/// The read-timeout error for a per-request deadline.
fn read_timeout(timeout: Duration) -> ClientError {
    ClientError::Hsfz(klartext_hsfz::Error::ReadTimeout { timeout })
}

/// Space-separated uppercase hex of a frame's UDS bytes, for trace logging.
///
/// Only called inside `tracing::trace!` — allocation happens only when trace
/// logging is enabled (e.g. `RUST_LOG=klartext_client=trace`), so the normal
/// path pays nothing.
fn hex_frame(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|b| format!("{b:02X}"))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Route one received frame to the pending request for its source address.
///
/// A `0x02` ack is recorded against its request ([`note_ack`]) and delivered to
/// nobody; any other non-diagnostic frame is skipped. A diagnostic frame is
/// matched against the waiter registered for its source ECU: the request's
/// positive SID delivers the payload; a negative for a *different* SID (a stray
/// keepalive NAK) is skipped; NRC 0x78 re-arms the waiter's timeout; any other
/// negative delivers a typed error. A frame with no waiter (a stray/late reply)
/// is dropped.
fn route_frame(pending: &Pending, frame: HsfzFrame) {
    if frame.control == control::ACK {
        note_ack(pending, &frame);
        return;
    }
    if frame.control != control::DIAGNOSTIC {
        return; // keepalive echo / other
    }
    let Some((src, _tgt)) = frame.addr else {
        return;
    };
    let payload = frame.payload;
    tracing::trace!("HSFZ RX src={:#04X} {}", src, hex_frame(&payload));

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

/// Record the gateway's HSFZ `0x02` acknowledge against the request it accepts.
///
/// The ack echoes the accepted frame verbatim — same SRC/TGT, same UDS bytes — so
/// unlike a diagnostic *response* (keyed by its SOURCE, the answering ECU) an ack
/// is keyed by its TARGET. The echoed SID must also match, because the keepalive
/// and the gateway's own SVT read share target `0x10`: without that check, the ack
/// of a `3E 80` keepalive would mark a pending `22 3F07` as acknowledged.
///
/// Observability only. A missing ack never fails a request: HSFZ calls the ack
/// optional (`docs/protocol-reference.md:309`), and five of the ten un-acked
/// frames in the `car-session-1` capture were answered regardless.
fn note_ack(pending: &Pending, frame: &HsfzFrame) {
    let Some((_src, target)) = frame.addr else {
        return;
    };
    let map = pending.lock().expect("pending mutex poisoned");
    if let Some(req) = map.get(&target)
        && frame.payload.first() == Some(&req.request_sid)
    {
        req.acked.store(true, Ordering::Relaxed);
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

    use klartext_hsfz::{HsfzConnection, HsfzFrame, control, read_frame, write_frame};
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

    /// A loopback gateway that counts requests to `target`, swallows the first
    /// `drop_first` of them, and answers the rest with `reply` (silence if `None`).
    ///
    /// Keepalives and frames for any other target are ignored and never counted,
    /// so the count is exactly "how many times klartext asked this ECU".
    async fn spawn_counting_gateway(
        target: u8,
        drop_first: usize,
        reply: Option<Vec<u8>>,
    ) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC || frame.payload == [0x3E, 0x80] {
                    continue;
                }
                let (tester, ecu) = frame.addr.unwrap();
                if ecu != target {
                    continue;
                }
                if counter.fetch_add(1, Ordering::SeqCst) < drop_first {
                    continue; // swallowed — the ECU stays silent for this attempt
                }
                let Some(uds) = reply.clone() else {
                    continue;
                };
                let reply = HsfzFrame::diagnostic(ecu, tester, uds);
                let _ = write_frame(&mut stream, &reply).await;
            }
        });
        (addr, seen)
    }

    /// A gateway whose 0x12 answers `22 F1 90` with NRC 0x78 ("received, still
    /// working"), then delivers the real response `work` later. Requests counted.
    async fn spawn_working_gateway(work: Duration) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let seen = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&seen);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC || frame.payload != [0x22, 0xF1, 0x90] {
                    continue;
                }
                let (tester, ecu) = frame.addr.unwrap();
                counter.fetch_add(1, Ordering::SeqCst);
                let pending = HsfzFrame::diagnostic(ecu, tester, vec![0x7F, 0x22, 0x78]);
                let _ = write_frame(&mut stream, &pending).await;
                tokio::time::sleep(work).await;
                let done = HsfzFrame::diagnostic(ecu, tester, vec![0x62, 0xF1, 0x90, ecu]);
                let _ = write_frame(&mut stream, &done).await;
            }
        });
        (addr, seen)
    }

    /// Register a pending request for `target`, returning its delivery channel and
    /// the ack flag [`route_frame`] writes.
    fn register(
        pending: &Pending,
        target: u8,
        request_sid: u8,
    ) -> (mpsc::UnboundedReceiver<Delivery>, Arc<AtomicBool>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let acked = Arc::new(AtomicBool::new(false));
        pending.lock().unwrap().insert(
            target,
            PendingReq {
                request_sid,
                generation: 0,
                tx,
                acked: Arc::clone(&acked),
            },
        );
        (rx, acked)
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

    // P1.1, ISTA parity `RetryComm = 1` (EDIABAS.INI:31): an ECU that misses one
    // request is asked again, and the repeat is what recovers the read. The car
    // session lost 10 of 32 ECUs to single missed requests.
    #[tokio::test]
    async fn a_timed_out_read_is_repeated_once_and_then_succeeds() {
        let (addr, seen) =
            spawn_counting_gateway(0x12, 1, Some(vec![0x62, 0xF1, 0x90, 0x12])).await;
        let session = open_session(addr).await;
        let response = session
            .request_with_timeout(0x12, &[0x22, 0xF1, 0x90], Duration::from_millis(120))
            .await
            .expect("the automatic repeat must recover the read");
        assert_eq!(response, vec![0x62, 0xF1, 0x90, 0x12]);
        assert_eq!(
            seen.load(Ordering::SeqCst),
            2,
            "one attempt plus exactly one retry"
        );
    }

    // ...and the repeat is bounded. `RetryComm = 1` is one repeat, not a loop: a
    // permanently silent ECU must cost two attempts and then fail.
    #[tokio::test]
    async fn the_retry_is_bounded_to_a_single_repeat() {
        let (addr, seen) = spawn_counting_gateway(0x12, usize::MAX, None).await;
        let session = open_session(addr).await;
        let error = session
            .request_with_timeout(0x12, &[0x22, 0xF1, 0x90], Duration::from_millis(80))
            .await
            .expect_err("a permanently silent ECU must still fail");
        assert!(matches!(
            error,
            ClientError::Hsfz(klartext_hsfz::Error::ReadTimeout { .. })
        ));
        assert_eq!(
            seen.load(Ordering::SeqCst),
            2,
            "RETRY_COMM = 1, not unbounded"
        );
    }

    // THE load-bearing case. A negative response is a COMPLETED exchange, not a
    // failed one — the ECU received the request, decided, and answered. ISTA's
    // retry condition is `!IsDone()`, and a job carrying an `ERROR_ECU_NACK`
    // result *is* done (`ECUJob.cs:60-75`), so it does not repeat one either.
    // Retrying here would both diverge from ISTA and double a rejected request
    // on the wire.
    #[tokio::test]
    async fn a_negative_response_is_never_retried() {
        let (addr, seen) = spawn_counting_gateway(0x12, 0, Some(vec![0x7F, 0x22, 0x31])).await;
        let session = open_session(addr).await;
        let error = session
            .request_with_timeout(0x12, &[0x22, 0xF1, 0x90], Duration::from_millis(500))
            .await
            .expect_err("the NRC must surface as an error");
        assert!(matches!(
            error,
            ClientError::Negative {
                sid: 0x22,
                nrc: Nrc::RequestOutOfRange
            }
        ));
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "an NRC is an answer, not a failed exchange"
        );
    }

    // A write gets exactly one attempt however silent the ECU. klartext's retry
    // deliberately excludes every write where ISTA's job-level retry would repeat
    // one: an ambiguous timeout on a clear must reach the human, not be re-sent
    // silently — it could otherwise erase a fault nobody ever saw.
    #[tokio::test]
    async fn a_write_is_never_retried_even_when_the_ecu_is_silent() {
        let (addr, seen) = spawn_counting_gateway(0x12, usize::MAX, None).await;
        let session = open_session(addr).await;
        let error = session
            .request_with_timeout(0x12, &[0x14, 0xFF, 0xFF, 0xFF], Duration::from_millis(80))
            .await
            .expect_err("a silent clear must fail");
        assert!(matches!(
            error,
            ClientError::Hsfz(klartext_hsfz::Error::ReadTimeout { .. })
        ));
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "0x14 is not retry-safe — one attempt only"
        );
    }

    // The initial timeout bounds only the wait for the ECU to say ANYTHING. Once
    // it answers NRC 0x78 the tester owes it the ISO 14229-2 P2* budget, so a
    // response 250 ms behind a 0x78 must land despite a 60 ms initial deadline.
    // Without that split, dropping the initial timeout to ISTA's ENET 1200 ms
    // would have quietly cut P2* from 5 s to 1.2 s.
    #[tokio::test]
    async fn nrc_0x78_rearms_the_wait_with_p2_star_not_the_initial_timeout() {
        let (addr, seen) = spawn_working_gateway(Duration::from_millis(250)).await;
        let session = open_session(addr).await;
        let response = session
            .request_with_timeout(0x12, &[0x22, 0xF1, 0x90], Duration::from_millis(60))
            .await
            .expect("a 0x78 must buy the ECU the P2* budget");
        assert_eq!(response, vec![0x62, 0xF1, 0x90, 0x12]);
        assert_eq!(
            seen.load(Ordering::SeqCst),
            1,
            "the 0x78 path must not have needed a retry"
        );
    }

    // P1.2 support: the gateway's HSFZ `0x02` ack is recorded against its request
    // and delivered to nobody. The ack is the sharpest available signal that a
    // frame was refused by the gateway's admission control rather than lost on
    // the bus — five of the whole-car sweep's ten losses were un-acked.
    #[test]
    fn route_frame_records_the_gateway_ack_without_delivering_it() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (mut rx, acked) = register(&pending, 0x12, 0x22);
        // An ack echoes the frame it accepts verbatim — same SRC/TGT, same UDS.
        let mut ack = HsfzFrame::diagnostic(0xF4, 0x12, vec![0x22, 0xF1, 0x90]);
        ack.control = control::ACK;
        route_frame(&pending, ack);
        assert!(acked.load(Ordering::Relaxed), "the ack must be recorded");
        assert!(rx.try_recv().is_err(), "an ack is not a response");
        assert!(
            pending.lock().unwrap().contains_key(&0x12),
            "the request must still be waiting for its real answer"
        );
    }

    // The keepalive and the gateway's own SVT read both target 0x10, so an ack
    // must echo the pending request's SID or it credits the wrong request.
    #[test]
    fn an_ack_for_a_different_service_is_not_credited() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (_rx, acked) = register(&pending, 0x10, 0x22); // a pending `22 3F07`
        let mut keepalive_ack = HsfzFrame::diagnostic(0xF4, 0x10, vec![0x3E, 0x80]);
        keepalive_ack.control = control::ACK;
        route_frame(&pending, keepalive_ack);
        assert!(
            !acked.load(Ordering::Relaxed),
            "the keepalive's ack is not our read's ack"
        );
    }

    // A stray keepalive NAK (`7F 3E 22`) from the target must NOT be delivered as
    // our read's response: route_frame skips a negative whose echoed SID differs
    // from the pending request's SID. (The single-target M2 hazard, preserved.)
    #[test]
    fn route_frame_skips_a_stray_keepalive_nack_then_delivers_the_real_response() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (mut rx, _acked) = register(&pending, 0x12, 0x22);

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
        let (mut rx, _acked) = register(&pending, 0x12, 0x22);
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
