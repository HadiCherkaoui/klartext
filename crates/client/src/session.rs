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
//! One request breaks the per-target model: a FUNCTIONAL (broadcast) request goes
//! to one address and is answered by many ECUs, each from its own source, so no
//! reply can match a per-target waiter. [`Session::request_functional`] collects
//! those through a separate broadcast slot the router falls through to, and only
//! for a frame that genuinely answers the outstanding broadcast — a frame with no
//! waiter is otherwise still dropped.
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

/// The most responders one functional (broadcast) request collects.
///
/// ISTA parity: the group SGBD's own bytecode caps it. `f01.prg`'s
/// `FS_LOESCHEN_FUNKTIONAL` sets `S0[7] = 0x64` at offset `0x000047` and compares the
/// responder counter against it at `0x0012B1` before looping for the next answer.
/// The cap is a backstop, not the exit condition — collection normally ends on a
/// quiet period (see [`Session::request_functional`]).
pub const MAX_BROADCAST_RESPONDERS: usize = 100;

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

/// One outstanding functional (broadcast) request, which MANY ECUs answer.
///
/// A broadcast cannot use the per-target [`Pending`] table: the request goes to one
/// address (`0xDF`) and the answers arrive from every ECU's own address, so no
/// reply's source matches the waiter's key. This is the fall-through the reader
/// tries when [`Pending`] has no waiter for a frame's source.
#[derive(Debug)]
struct BroadcastWaiter {
    /// The broadcast request's SID; only its positive response is collected.
    request_sid: u8,
    /// Our tester address — a collected response must be addressed to us.
    tester: u8,
    /// The channel each responder's `(source, payload)` is delivered on.
    tx: mpsc::UnboundedSender<(u8, Vec<u8>)>,
}

/// The single in-flight broadcast slot, shared with the reader task.
type Broadcast = Arc<Mutex<Option<BroadcastWaiter>>>;

/// Clears the broadcast slot on drop, so a cancelled or failed collector can never
/// leave it occupied and wedge every later broadcast with `RequestInFlight`.
///
/// Simpler than [`SlotGuard`] because there is only ever one broadcast in flight:
/// the slot this guard clears can only be the one its own request installed.
struct BroadcastGuard<'a> {
    broadcast: &'a Broadcast,
}

impl Drop for BroadcastGuard<'_> {
    fn drop(&mut self) {
        *self.broadcast.lock().expect("broadcast mutex poisoned") = None;
    }
}

/// A held, demuxed UDS session: concurrent per-target requests plus a keepalive.
#[derive(Debug)]
pub struct Session {
    /// Write half shared with the keepalive task, locked per whole-frame write.
    write: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    /// Outstanding requests keyed by target address.
    pending: Pending,
    /// The one outstanding functional (broadcast) request, if any.
    broadcast: Broadcast,
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
        let broadcast: Broadcast = Arc::new(Mutex::new(None));

        let reader_pending = Arc::clone(&pending);
        let reader_broadcast = Arc::clone(&broadcast);
        let reader = tokio::spawn(async move {
            // Route frames until the connection closes or a read fatally errors.
            while let Ok(frame) = read_frame(&mut read, READER_IDLE_CAP).await {
                route_frame(&reader_pending, &reader_broadcast, frame);
            }
            // Fail every waiter so no request hangs forever.
            let mut map = reader_pending.lock().expect("pending mutex poisoned");
            for (_target, req) in map.drain() {
                let _ = req
                    .tx
                    .send(Delivery::Final(Err(ClientError::ConnectionClosed)));
            }
            // Dropping the broadcast waiter closes its channel, so a parked
            // collector wakes and returns whatever it had rather than waiting out
            // its quiet period on a dead connection.
            *reader_broadcast.lock().expect("broadcast mutex poisoned") = None;
        });

        let keepalive = spawn_keepalive(Arc::clone(&write), source, gateway, interval);
        Self {
            write,
            pending,
            broadcast,
            reader,
            keepalive,
            next_generation: AtomicU64::new(0),
            source,
            read_timeout,
        }
    }

    /// The connection's default per-request read timeout.
    pub(crate) fn read_timeout(&self) -> Duration {
        self.read_timeout
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

    /// Send ONE functional (broadcast) request and collect every ECU that answers.
    ///
    /// Where [`Session::request`] is one request to one ECU, this is one request to
    /// `target` — a functional address such as
    /// [`klartext_uds::FUNCTIONAL_ADDRESS_F01`] — which many ECUs answer at once.
    /// Returns each responder as `(source address, payload)` in arrival order;
    /// responders are told apart by nothing but their own source byte, exactly as
    /// ISTA's group SGBD does (it writes the response's source into `ID_SG_ADR`,
    /// `f01.prg` offsets `0x00048A`/`0x000C5A`).
    ///
    /// Collection ends on whichever comes first: `quiet_period` elapsing with no new
    /// response (the normal exit — the bytecode's own loop terminates on the first
    /// silent slot via its trap, `f01.prg` `0x000CC1`), `max_responders` answers
    /// (ISTA's cap, [`MAX_BROADCAST_RESPONDERS`]), or the reader ending. A broadcast
    /// therefore always costs `quiet_period` and never fails for want of an answer:
    /// **an empty result means nobody answered, not that the request failed.**
    ///
    /// Each response is validated the way the SGBD bytecode validates it: its SID
    /// must be the request's positive response, and it must be addressed to this
    /// tester. It is deliberately NOT checked that the source equals `target` — with
    /// a broadcast no responder's source can equal `0xDF`, and the bytecode skips
    /// that check for exactly that reason (`0x0005CE`, where the physical twin at
    /// `0x000418` enforces it). Enforcing it here would drop every response.
    ///
    /// A responder that answers negatively — including NRC 0x78 "still working" — is
    /// not collected, matching the bytecode's positive-SID acceptance check. It
    /// degrades safely: an ECU missing from the result is treated as a straggler and
    /// re-addressed physically, and a 0x78 responder's real answer is still collected
    /// if it lands inside the quiet period. [verify against capture]
    ///
    /// **Blast radius.** The service decides this, not the addressing: with `0x14`
    /// this reaches every ECU on the car at once, so the caller must hold the human's
    /// explicit confirmation before calling — see
    /// [`crate::DiagnosticClient::clear_all_dtcs_functional`].
    ///
    /// Research: `docs/superpowers/specs/2026-07-18-research-p2-clear-sequence.md` §B.
    ///
    /// # Errors
    /// [`ClientError::RequestInFlight`] if a broadcast is already outstanding (only
    /// one at a time — responses carry no request id to tell two apart), and
    /// [`ClientError::Hsfz`] if the request cannot be written.
    pub async fn request_functional(
        &self,
        target: u8,
        uds: &[u8],
        quiet_period: Duration,
        max_responders: usize,
    ) -> Result<Vec<(u8, Vec<u8>)>, ClientError> {
        let request_sid = uds.first().copied().unwrap_or_default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        {
            let mut slot = self.broadcast.lock().expect("broadcast mutex poisoned");
            if slot.is_some() {
                return Err(ClientError::RequestInFlight { target });
            }
            *slot = Some(BroadcastWaiter {
                request_sid,
                tester: self.source,
                tx,
            });
        }
        // Clears the slot on every exit path, including this future being cancelled
        // mid-collection.
        let _guard = BroadcastGuard {
            broadcast: &self.broadcast,
        };
        tracing::trace!(
            "HSFZ TX (functional) src={:#04X} tgt={:#04X} {}",
            self.source,
            target,
            hex_frame(uds)
        );
        let frame = HsfzFrame::diagnostic(self.source, target, uds.to_vec());
        {
            let mut writer = self.write.lock().await;
            write_frame(&mut *writer, &frame).await?;
        }
        // No retry: a broadcast is only ever used for a write here, and repeating it
        // would re-broadcast to the whole car. A missed responder is recovered by the
        // physical straggler pass instead, as it is in ISTA.
        let mut responders = Vec::new();
        while responders.len() < max_responders {
            match tokio::time::timeout(quiet_period, rx.recv()).await {
                Ok(Some(responder)) => responders.push(responder),
                // The reader ended (connection closed): no further answer can arrive,
                // so report what did answer rather than discarding it.
                Ok(None) => break,
                // The quiet period elapsed — the normal end of a broadcast.
                Err(_) => break,
            }
        }
        Ok(responders)
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
/// negative delivers a typed error.
///
/// A frame with no per-target waiter falls through to the in-flight broadcast
/// ([`route_to_broadcast`]), which is where a functional request's many answers land
/// — they arrive from every ECU's own address, never from the address the request
/// was sent to. That fall-through is NOT a catch-all: a frame the broadcast does not
/// accept, or any frame at all when no broadcast is outstanding, is still dropped as
/// the stray/late reply it is.
fn route_frame(pending: &Pending, broadcast: &Broadcast, frame: HsfzFrame) {
    if frame.control == control::ACK {
        note_ack(pending, &frame);
        return;
    }
    if frame.control != control::DIAGNOSTIC {
        return; // keepalive echo / other
    }
    let Some((src, tgt)) = frame.addr else {
        return;
    };
    let payload = frame.payload;
    tracing::trace!("HSFZ RX src={:#04X} {}", src, hex_frame(&payload));

    let mut map = pending.lock().expect("pending mutex poisoned");
    // Copy the expected SID and release the borrow before any `remove`.
    let Some(request_sid) = map.get(&src).map(|req| req.request_sid) else {
        // No per-ECU waiter. Offer the frame to an in-flight broadcast, which
        // accepts it only if it answers that broadcast; otherwise it is dropped as a
        // stray/late reply. Release the pending lock first so the two are never held
        // together.
        drop(map);
        route_to_broadcast(broadcast, src, tgt, payload);
        return;
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

/// Offer a frame with no per-target waiter to the in-flight broadcast, if any.
///
/// Applies the two checks the group SGBD's bytecode applies to a functional response
/// (`f01.prg` `0x0005A9`, `0x0005BB`): the payload's SID must be the broadcast
/// request's positive response, and the frame must be addressed to this tester. The
/// third check a *physical* response gets — source == the address we sent to — is
/// deliberately absent; see [`Session::request_functional`].
///
/// Anything else returns without delivering, which is what keeps the miss branch of
/// [`route_frame`] a drop rather than a catch-all.
fn route_to_broadcast(broadcast: &Broadcast, src: u8, tgt: u8, payload: Vec<u8>) {
    let slot = broadcast.lock().expect("broadcast mutex poisoned");
    let Some(waiter) = slot.as_ref() else {
        return; // no broadcast outstanding — stray/late reply
    };
    if tgt != waiter.tester {
        return; // not addressed to us
    }
    if payload.first().copied() != Some(positive_response_sid(waiter.request_sid)) {
        return; // a different service, or a negative — not an answer to our broadcast
    }
    let _ = waiter.tx.send((src, payload));
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
    use klartext_uds::FUNCTIONAL_ADDRESS_F01;
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

    /// A loopback gateway that fans ONE functional request out to several ECUs.
    ///
    /// A frame addressed to [`FUNCTIONAL_ADDRESS_F01`] draws one reply per address in
    /// `responders`, each from that ECU's own source address — which is the shape
    /// that matters: no reply's source is the address the request was sent to.
    /// Anything else (a keepalive, a physically addressed frame) is ignored.
    async fn spawn_broadcast_gateway(responders: &[u8]) -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let responders: Vec<u8> = responders.to_vec();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                let (tester, target) = frame.addr.unwrap();
                if target != FUNCTIONAL_ADDRESS_F01 || frame.payload != [0x14, 0xFF, 0xFF, 0xFF] {
                    continue;
                }
                for &responder in &responders {
                    let reply = HsfzFrame::diagnostic(responder, tester, vec![0x54]);
                    let _ = write_frame(&mut stream, &reply).await;
                }
            }
        });
        addr
    }

    /// An empty broadcast slot, for the routing tests with none in flight.
    fn no_broadcast() -> Broadcast {
        Arc::new(Mutex::new(None))
    }

    /// Install a broadcast waiter for `request_sid`, returning its delivery channel.
    fn register_broadcast(request_sid: u8) -> (Broadcast, mpsc::UnboundedReceiver<(u8, Vec<u8>)>) {
        let (tx, rx) = mpsc::unbounded_channel();
        let broadcast = Arc::new(Mutex::new(Some(BroadcastWaiter {
            request_sid,
            tester: 0xF4,
            tx,
        })));
        (broadcast, rx)
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
        route_frame(&pending, &no_broadcast(), ack);
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
        route_frame(&pending, &no_broadcast(), keepalive_ack);
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
            &no_broadcast(),
            HsfzFrame::diagnostic(0x12, 0xF4, vec![0x7F, 0x3E, 0x22]),
        );
        assert!(
            rx.try_recv().is_err(),
            "the stray NAK must not be delivered"
        );

        // The real 0x62 response — must be delivered.
        route_frame(
            &pending,
            &no_broadcast(),
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
            &no_broadcast(),
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

    // THE broadcast case. A functional request goes to one address and is answered
    // by many ECUs, each from its OWN source — so the per-target waiter table cannot
    // match a single one of them, and validating source == the address we sent to
    // (which the physical path does, and the group SGBD's bytecode deliberately does
    // not) would drop every response.
    #[tokio::test]
    async fn request_functional_collects_every_ecu_that_answers_one_broadcast() {
        let addr = spawn_broadcast_gateway(&[0x12, 0x40, 0x60]).await;
        let session = open_session(addr).await;
        let responders = session
            .request_functional(
                FUNCTIONAL_ADDRESS_F01,
                &[0x14, 0xFF, 0xFF, 0xFF],
                Duration::from_millis(150),
                MAX_BROADCAST_RESPONDERS,
            )
            .await
            .expect("a broadcast that nobody refuses must succeed");
        assert_eq!(
            responders,
            vec![(0x12, vec![0x54]), (0x40, vec![0x54]), (0x60, vec![0x54])],
            "every responder, told apart by its own source address, in arrival order"
        );
    }

    // The cap is ISTA's own (`S0[7] = 0x64`): collection stops at it even while more
    // ECUs are still answering.
    #[tokio::test]
    async fn collection_stops_at_the_responder_cap() {
        let addr = spawn_broadcast_gateway(&[0x12, 0x40, 0x60]).await;
        let session = open_session(addr).await;
        let responders = session
            .request_functional(
                FUNCTIONAL_ADDRESS_F01,
                &[0x14, 0xFF, 0xFF, 0xFF],
                Duration::from_millis(150),
                2,
            )
            .await
            .unwrap();
        assert_eq!(responders.len(), 2, "the cap bounds the collection");
    }

    // The slot must be released when a broadcast ends, or the first whole-car clear
    // would wedge every one after it with `RequestInFlight`.
    #[tokio::test]
    async fn a_finished_broadcast_releases_the_slot_for_the_next_one() {
        let addr = spawn_broadcast_gateway(&[0x12]).await;
        let session = open_session(addr).await;
        let clear = [0x14, 0xFF, 0xFF, 0xFF];
        let quiet = Duration::from_millis(120);
        let first = session
            .request_functional(FUNCTIONAL_ADDRESS_F01, &clear, quiet, 1)
            .await
            .unwrap();
        let second = session
            .request_functional(FUNCTIONAL_ADDRESS_F01, &clear, quiet, 1)
            .await
            .expect("the slot must be free again");
        assert_eq!(first, second);
    }

    // The fall-through to the broadcast is NOT a catch-all: it accepts only a frame
    // that answers the outstanding broadcast — the request's positive SID, addressed
    // to us. Everything else is dropped, exactly as before the fall-through existed.
    #[test]
    fn the_broadcast_fall_through_is_not_a_catch_all() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (broadcast, mut rx) = register_broadcast(0x14);

        // A positive response for a DIFFERENT service, from an ECU with no waiter.
        route_frame(
            &pending,
            &broadcast,
            HsfzFrame::diagnostic(0x12, 0xF4, vec![0x62, 0xF1, 0x90, 0xAB]),
        );
        assert!(
            rx.try_recv().is_err(),
            "another service's reply is not our broadcast's answer"
        );

        // A refusal of OUR service is not a collected responder either — the group
        // SGBD's acceptance check enforces the positive SID.
        route_frame(
            &pending,
            &broadcast,
            HsfzFrame::diagnostic(0x12, 0xF4, vec![0x7F, 0x14, 0x22]),
        );
        assert!(rx.try_recv().is_err(), "a refusal is not an answer");

        // Addressed to a different tester: not ours to collect.
        route_frame(
            &pending,
            &broadcast,
            HsfzFrame::diagnostic(0x12, 0xF1, vec![0x54]),
        );
        assert!(
            rx.try_recv().is_err(),
            "a frame addressed to someone else is not ours"
        );

        // ...and the real answer still lands.
        route_frame(
            &pending,
            &broadcast,
            HsfzFrame::diagnostic(0x40, 0xF4, vec![0x54]),
        );
        assert_eq!(rx.try_recv().unwrap(), (0x40, vec![0x54]));
    }

    // With no broadcast in flight, a reply for an ECU nobody is waiting on is still
    // dropped — above all it must not be handed to an unrelated pending request.
    #[test]
    fn a_reply_with_no_waiter_and_no_broadcast_is_dropped() {
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let (mut rx, _acked) = register(&pending, 0x40, 0x22);
        route_frame(
            &pending,
            &no_broadcast(),
            HsfzFrame::diagnostic(0x12, 0xF4, vec![0x62, 0xF1, 0x90, 0xAB]),
        );
        assert!(
            rx.try_recv().is_err(),
            "0x12's stray reply must not be delivered to 0x40's request"
        );
        assert!(
            pending.lock().unwrap().contains_key(&0x40),
            "the unrelated request must still be waiting"
        );
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
