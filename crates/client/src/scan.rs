//! Whole-car orchestrations over the demuxed client.
//!
//! Two concrete procedures used by the MCP server (and the future mobile app):
//! whole-car fault reads (over the gateway SVT addresses — read → partition
//! relevant vs not-tested per ECU) and a verified whole-car clear (per ECU:
//! pre-read → extended session → standard `14 FF FF FF` → post-read verify).
//!
//! These are concrete procedures, not a general guided-procedure engine (that is
//! a named future milestone). Both walk the car one ECU at a time: reads are
//! autonomous-safe but the gateway will not carry them in parallel (see
//! [`DiagnosticClient::scan_faults`]), and the clear is a state change that
//! records each ECU's stored faults before erasing them.

use klartext_uds::Dtc;

use crate::client::DiagnosticClient;

/// One ECU's faults after partitioning relevant faults from not-tested noise.
#[derive(Debug, Clone)]
pub struct EcuFaults {
    /// The diagnostic address.
    pub address: u8,
    /// Every fault the ECU returned, unfiltered.
    ///
    /// klartext applies NO status filter: the request is `19 02 0C`, so the ECU has
    /// already filtered to pending|confirmed, and ISTA surfaces every DTC it gets
    /// back. Judge an individual fault with [`Dtc::presence`].
    pub faults: Vec<Dtc>,
    /// Set if reading this ECU failed (the scan continues past it).
    pub error: Option<String>,
}

/// The record of a verified per-ECU clear.
#[derive(Debug, Clone)]
pub struct ClearReport {
    /// The diagnostic address.
    pub address: u8,
    /// Every DTC stored immediately before the clear (all statuses) — the record
    /// of what was discarded (together with its freeze-frame/snapshot data).
    pub before: Vec<Dtc>,
    /// Relevant faults still present after the clear (empty means clean).
    pub after_relevant: Vec<Dtc>,
    /// True if the post-clear re-read showed no relevant faults.
    pub verified_clean: bool,
    /// Set if any step failed for this ECU (others are still processed).
    pub error: Option<String>,
}

impl DiagnosticClient {
    /// Read and partition faults for each address in `addrs`, one ECU at a time.
    ///
    /// `addrs` is the fitted list from the gateway SVT ([`DiagnosticClient::read_ecu_list`]).
    /// A per-ECU read failure (e.g. an installed-but-silent ECU) is recorded in
    /// [`EcuFaults::error`], never aborting the scan. The result is sorted by address.
    ///
    /// **Strictly sequential, and not tunable.** klartext fanned this out over
    /// eight concurrent reads until 2026-07-18. The owner's `car-session-1` capture
    /// measures what that cost, on one car in one session: the sequential ident
    /// sweep lost **0 of 388** requests, while this sweep at an in-flight depth of
    /// 3–7 lost **10 of 32 (31 %)** — and five of those ten were never acknowledged
    /// by the gateway at the HSFZ layer at all, i.e. refused by its admission
    /// control rather than lost on the bus. Loss was zero at depth 0–1 and 27–100 %
    /// at depth ≥ 3. Sequential costs about three seconds more on a 32-ECU car.
    ///
    /// The concurrency knob is gone rather than defaulted to 1: a tunable whose
    /// only safe value is 1 is not a tunable, and leaving it exposed invites the
    /// same 31 % loss to be switched back on. Note this is *observable* parity, not
    /// a protocol constraint — ISTA is sequential because `ECUKom` holds one
    /// blocking EDIABAS handle (`ECUKom.decompiled.cs:78`) and never tries to
    /// interleave, not because anything stops it; there is no lock in its code.
    /// The per-target demux in [`crate::Session`] stays, and other callers (the
    /// BEST/2 VM's jobs) may still interleave.
    pub async fn scan_faults(&self, addrs: &[u8]) -> Vec<EcuFaults> {
        let mut out = Vec::with_capacity(addrs.len());
        for &address in addrs {
            out.push(match self.read_all_dtcs(address).await {
                Ok(faults) => EcuFaults {
                    address,
                    faults,
                    error: None,
                },
                Err(error) => EcuFaults {
                    address,
                    faults: Vec::new(),
                    error: Some(error.to_string()),
                },
            });
        }
        out.sort_unstable_by_key(|e| e.address);
        out
    }

    /// Clear one ECU with a pre-read record and a post-clear verification.
    ///
    /// A state change (UDS 0x14): reads and records the stored DTCs, enters the
    /// extended session, clears all, then re-reads to confirm no relevant fault
    /// remains. Never aborts a batch — a failure is captured in the report, and a
    /// failed pre-read means the ECU is *not* cleared (never clear blind).
    pub async fn clear_faults_verified(&self, target: u8) -> ClearReport {
        let mut report = ClearReport {
            address: target,
            before: Vec::new(),
            after_relevant: Vec::new(),
            verified_clean: false,
            error: None,
        };
        match self.read_all_dtcs(target).await {
            Ok(before) => report.before = before,
            Err(error) => {
                report.error = Some(format!("pre-read failed: {error}"));
                return report; // never clear blind
            }
        }
        if let Err(error) = self.clear_all_dtcs(target).await {
            report.error = Some(format!("clear failed: {error}"));
            return report;
        }
        match self.read_all_dtcs(target).await {
            Ok(after) => {
                // The ECU already filtered to pending|confirmed (`19 02 0C`), so
                // anything still returned after a clear is a genuine residual fault.
                report.after_relevant = after;
                report.verified_clean = report.after_relevant.is_empty();
            }
            Err(error) => report.error = Some(format!("post-read verify failed: {error}")),
        }
        report
    }

    /// Clear every ECU in `addrs`, sequentially, returning a per-ECU report.
    ///
    /// Sequential by design — writes stay lockstep even though reads fan out.
    ///
    /// NO ECU reset follows the clear. ISTA's own whole-vehicle clear
    /// (`VehicleIdent.ClearErrorInfoMemoryVehicle`, `VehicleIdent.cs:9720-9788`)
    /// sends no UDS `0x11` anywhere; all three `STEUERGERAETE_RESET` call sites
    /// (`VehicleIdent.cs:289,1328,2473`) are unrelated recovery paths. klartext
    /// sent one until 2026-07-18 — an invention approximating the *effect* of the
    /// ignition cycle ISTA performs instead, which klartext cannot do over ENET
    /// (it has no VCI clamp control). See the parity audit, P0.1.
    pub async fn clear_faults_all(&self, addrs: &[u8]) -> Vec<ClearReport> {
        let mut reports = Vec::with_capacity(addrs.len());
        for &address in addrs {
            reports.push(self.clear_faults_verified(address).await);
        }
        reports
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use klartext_hsfz::{HsfzFrame, control, read_frame, write_frame};
    use tokio::net::TcpListener;

    use crate::client::tests::spawn_gateway_recording;
    use crate::{ClientConfig, DiagnosticClient};

    /// A loopback gateway where `present` ECUs answer `3E 00`, `19 02` (one
    /// confirmed + one not-tested DTC), `14 FF FF FF` (then read clean), and the
    /// extended-session `10 03`. Absent addresses never reply. Every reply swaps
    /// SRC/TGT, and per-ECU "cleared" state makes the post-clear read return clean.
    async fn spawn(present: &[u8]) -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let present: Vec<u8> = present.to_vec();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut cleared: std::collections::HashSet<u8> = Default::default();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                let (tester, ecu) = frame.addr.unwrap();
                if frame.payload == [0x3E, 0x80] || !present.contains(&ecu) {
                    continue;
                }
                let uds = match frame.payload.as_slice() {
                    [0x3E, 0x00] => vec![0x7E, 0x00],
                    [0x10, 0x03] => vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88],
                    [0x14, 0xFF, 0xFF, 0xFF] => {
                        cleared.insert(ecu);
                        vec![0x54]
                    }
                    // Only the ISTA mask is served. A regression that went back to
                    // `19 02 FF` would fall through to `_ => continue` and time out,
                    // rather than quietly getting the same answer.
                    [0x19, 0x02, 0x0C] if cleared.contains(&ecu) => vec![0x59, 0x02, 0x0C],
                    [0x19, 0x02, 0x0C] => vec![
                        0x59, 0x02, 0x0C, //
                        0x00, 0x00, 0x01,
                        0x08, // confirmed, test has run  -> Present? no: bit0 clear
                        0x00, 0x00, 0x02, 0x2F, // testFailed + confirmed   -> Present
                    ],
                    _ => continue,
                };
                let _ = write_frame(&mut stream, &HsfzFrame::diagnostic(ecu, tester, uds)).await;
            }
        });
        addr
    }

    /// A loopback gateway that answers every `19 02` after `delay`, recording the
    /// high-water mark of requests outstanding at once.
    ///
    /// Each request is parked in its own task so the read loop keeps accepting
    /// frames while earlier ones are still unanswered — that is what makes the
    /// counter measure klartext's fan-out rather than the mock's own pacing.
    async fn spawn_depth_probe(delay: Duration) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let peak = Arc::new(AtomicUsize::new(0));
        let high_water = Arc::clone(&peak);
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let (mut read, write) = stream.into_split();
            let write = Arc::new(tokio::sync::Mutex::new(write));
            let in_flight = Arc::new(AtomicUsize::new(0));
            while let Ok(frame) = read_frame(&mut read, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC || frame.payload.first() != Some(&0x19) {
                    continue;
                }
                let (tester, ecu) = frame.addr.unwrap();
                let depth = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                high_water.fetch_max(depth, Ordering::SeqCst);
                let write = Arc::clone(&write);
                let in_flight = Arc::clone(&in_flight);
                tokio::spawn(async move {
                    tokio::time::sleep(delay).await;
                    // Release the slot before writing, so the reply racing the next
                    // request can never inflate the mark on a sequential client.
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    let reply = HsfzFrame::diagnostic(ecu, tester, vec![0x59, 0x02, 0xFF]);
                    let mut writer = write.lock().await;
                    let _ = write_frame(&mut *writer, &reply).await;
                });
            }
        });
        (addr, peak)
    }

    async fn client(addr: std::net::SocketAddr) -> DiagnosticClient {
        let config = ClientConfig {
            port: addr.port(),
            ..ClientConfig::default()
        };
        DiagnosticClient::connect(addr.ip(), &config).await.unwrap()
    }

    /// P0.2/P0.3: the sweep surfaces EVERY fault the ECU returned, unfiltered, and
    /// annotates each with ISTA's presence verdict instead of dropping any.
    #[tokio::test]
    async fn scan_faults_surfaces_every_returned_fault_with_a_presence_verdict() {
        let addr = spawn(&[0x12]).await;
        let client = client(addr).await;
        let faults = client.scan_faults(&[0x12]).await;
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].address, 0x12);
        // Both records survive: klartext no longer filters by status at all.
        assert_eq!(faults[0].faults.len(), 2);
        assert!(faults[0].error.is_none());
        // ...and they are distinguishable by ISTA's rule, not by a klartext mask.
        assert_eq!(
            faults[0].faults[0].presence(),
            klartext_uds::Presence::Absent
        );
        assert_eq!(
            faults[0].faults[1].presence(),
            klartext_uds::Presence::Present
        );
    }

    // P1.2 — the whole-car sweep must never hold more than one request open. On
    // `car-session-1` loss was 0 % at in-flight depth 0–1 and 27–100 % at depth
    // ≥ 3, and the eight-wide fan-out this replaces lost 10 of the car's 32 ECUs.
    #[tokio::test]
    async fn scan_faults_keeps_only_one_request_in_flight() {
        let (addr, peak) = spawn_depth_probe(Duration::from_millis(40)).await;
        let client = client(addr).await;
        let faults = client.scan_faults(&[0x12, 0x18, 0x40, 0x60]).await;
        assert_eq!(faults.len(), 4);
        // Guards the vacuous pass: a depth of 1 proves nothing if nothing was read.
        for ecu in &faults {
            assert_eq!(
                ecu.error, None,
                "0x{:02X} must actually have been read",
                ecu.address
            );
        }
        assert_eq!(
            peak.load(Ordering::SeqCst),
            1,
            "the sweep must be strictly sequential — the capture lost 31 % of \
             requests at an in-flight depth of 3–7"
        );
    }

    #[tokio::test]
    async fn clear_faults_verified_reads_clears_and_confirms_clean() {
        let addr = spawn(&[0x12]).await;
        let client = client(addr).await;
        let report = client.clear_faults_verified(0x12).await;
        assert_eq!(report.before.len(), 2); // both DTCs stored before the clear
        assert!(report.after_relevant.is_empty());
        assert!(report.verified_clean);
        assert!(report.error.is_none());
    }

    #[tokio::test]
    async fn scan_faults_records_a_silent_listed_ecu_as_error() {
        // 0x12 answers; 0x18 is listed by the SVT but never replies. The silent ECU
        // must surface as an `error` entry (not be dropped), and the scan must still
        // read 0x12. A short read timeout keeps the silent read from costing P2*.
        let addr = spawn(&[0x12]).await;
        let config = ClientConfig {
            port: addr.port(),
            read_timeout: Duration::from_millis(200),
            ..ClientConfig::default()
        };
        let client = DiagnosticClient::connect(addr.ip(), &config).await.unwrap();
        let faults = client.scan_faults(&[0x12, 0x18]).await;
        assert_eq!(faults.len(), 2);
        assert_eq!(faults[0].address, 0x12);
        assert!(faults[0].error.is_none());
        assert_eq!(faults[0].faults.len(), 2);
        assert_eq!(faults[1].address, 0x18);
        assert!(faults[1].error.is_some());
        assert!(faults[1].faults.is_empty());
    }

    #[tokio::test]
    async fn clear_faults_all_continues_past_a_failed_ecu_and_never_resets() {
        // 0x12 has no `14 FF FF FF` arm, so its clear is rejected: the mock's
        // fallback reply for a known-but-unserved request is hardcoded to echo SID
        // 0x22 (`7F 22 31`), which the client's frame router treats as a stray reply
        // to a DIFFERENT service and drops silently — so the clear request runs to
        // its read timeout rather than failing on a clean negative response. A short
        // `read_timeout` keeps that bounded. 0x40 must still be cleared afterwards.
        //
        // Both ECUs DO serve `11 01` with a positive response. That is the point:
        // if a regression reintroduced the post-clear ECU reset (parity audit P0.1
        // — ISTA sends no `0x11` in its clear flow), it would SUCCEED here rather
        // than time out, so this table cannot hide one.
        let (addr, frames) = spawn_gateway_recording(&[
            (0x12, vec![0x19, 0x02, 0x0C], vec![0x59, 0x02, 0x0C]),
            (
                0x12,
                vec![0x10, 0x03],
                vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88],
            ),
            (0x12, vec![0x11, 0x01], vec![0x51, 0x01]),
            (0x40, vec![0x19, 0x02, 0x0C], vec![0x59, 0x02, 0x0C]),
            (
                0x40,
                vec![0x10, 0x03],
                vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88],
            ),
            (0x40, vec![0x14, 0xFF, 0xFF, 0xFF], vec![0x54]),
            (0x40, vec![0x11, 0x01], vec![0x51, 0x01]),
        ])
        .await;
        let config = ClientConfig {
            port: addr.port(),
            read_timeout: Duration::from_millis(150),
            ..ClientConfig::default()
        };
        let c = DiagnosticClient::connect(addr.ip(), &config).await.unwrap();
        let reports = c.clear_faults_all(&[0x12, 0x40]).await;
        assert_eq!(reports.len(), 2);

        assert_eq!(reports[0].address, 0x12);
        assert!(reports[0].error.is_some(), "0x12's clear must have failed");
        assert!(!reports[0].verified_clean);

        assert_eq!(reports[1].address, 0x40);
        assert_eq!(reports[1].error, None, "0x40's clear must have succeeded");
        assert!(
            reports[1].verified_clean,
            "the batch must continue past 0x12's failure"
        );

        // Parity audit P0.1 at the LIBRARY layer, not just on the MCP surface.
        // `crates/client` is what the planned mobile core consumes over UniFFI, so
        // its no-reset guarantee has to be pinned in this crate — a review found
        // this test previously passed under a mutation that reset every ECU,
        // because its name claimed a property nothing here checked.
        let sent = frames.lock().unwrap().clone();
        let resets: Vec<(u8, Vec<u8>)> = sent
            .iter()
            .filter(|(_, payload)| payload.first() == Some(&klartext_uds::sid::ECU_RESET))
            .cloned()
            .collect();
        assert!(
            resets.is_empty(),
            "clear_faults_all must send no ECUReset to any address, got {resets:02X?} \
             in {sent:02X?}"
        );
    }
}
