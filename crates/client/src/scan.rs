//! Whole-car orchestrations over the demuxed client.
//!
//! Three concrete procedures shared by the CLI and the MCP server: fitted-ECU
//! discovery (bounded-concurrent presence probes), whole-car fault reads (present
//! → read → partition relevant vs not-tested), and a verified whole-car clear
//! (per ECU: pre-read → extended session → standard `14 FF FF FF` → post-read
//! verify).
//!
//! These are concrete procedures, not a general guided-procedure engine (that is
//! a named future milestone). Reads are autonomous-safe and fan out concurrently;
//! the clear is a state change, stays strictly sequential, and records each ECU's
//! stored faults before erasing them.

use std::time::Duration;

use futures::stream::{self, StreamExt};
use klartext_uds::Dtc;

use crate::client::{DiagnosticClient, ProbeOutcome};

/// Tuning for a whole-car scan.
#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    /// Per-ECU presence-probe timeout. An absent ECU costs at most this, not the
    /// full read timeout — so a scan never hangs on a missing module.
    pub probe_timeout: Duration,
    /// How many ECUs to probe/read at once over the single connection.
    ///
    /// `1` is strictly sequential — the safe fallback if the gateway dislikes
    /// overlapping requests to different targets ([verify live]).
    pub concurrency: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self {
            probe_timeout: Duration::from_millis(300),
            concurrency: 8,
        }
    }
}

/// A fitted ECU found by [`DiagnosticClient::scan_present`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FittedEcu {
    /// The diagnostic address that answered.
    pub address: u8,
    /// How quickly it answered the probe.
    pub latency: Duration,
}

/// One ECU's faults after partitioning relevant faults from not-tested noise.
#[derive(Debug, Clone)]
pub struct EcuFaults {
    /// The diagnostic address.
    pub address: u8,
    /// Real faults worth surfacing (see [`Dtc::is_relevant`]).
    pub relevant: Vec<Dtc>,
    /// Count of "not tested this cycle" catalog entries suppressed.
    pub not_tested: usize,
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
    /// Probe `addrs` and return those that answer, bounded by `opts.concurrency`.
    ///
    /// The result is sorted by address. An absent ECU yields no entry; a fatal
    /// transport error also drops that address rather than aborting the scan.
    pub async fn scan_present(&self, addrs: &[u8], opts: ScanOptions) -> Vec<FittedEcu> {
        let mut fitted: Vec<FittedEcu> = stream::iter(addrs.iter().copied())
            .map(|address| async move {
                match self.probe(address, opts.probe_timeout).await {
                    Ok(ProbeOutcome::Present { latency, .. }) => {
                        Some(FittedEcu { address, latency })
                    }
                    _ => None,
                }
            })
            .buffer_unordered(opts.concurrency.max(1))
            .filter_map(|found| async move { found })
            .collect()
            .await;
        fitted.sort_unstable_by_key(|f| f.address);
        fitted
    }

    /// Scan `addrs`, then read and partition faults for each fitted ECU.
    ///
    /// A per-ECU read failure is recorded in [`EcuFaults::error`], never aborting
    /// the whole scan. The result is sorted by address.
    pub async fn scan_faults(&self, addrs: &[u8], opts: ScanOptions) -> Vec<EcuFaults> {
        let fitted = self.scan_present(addrs, opts).await;
        let mut out: Vec<EcuFaults> = stream::iter(fitted.into_iter().map(|f| f.address))
            .map(|address| async move {
                match self.read_all_dtcs(address).await {
                    Ok(dtcs) => {
                        let (relevant, noise): (Vec<Dtc>, Vec<Dtc>) =
                            dtcs.into_iter().partition(|d| d.is_relevant());
                        EcuFaults {
                            address,
                            relevant,
                            not_tested: noise.len(),
                            error: None,
                        }
                    }
                    Err(error) => EcuFaults {
                        address,
                        relevant: Vec::new(),
                        not_tested: 0,
                        error: Some(error.to_string()),
                    },
                }
            })
            .buffer_unordered(opts.concurrency.max(1))
            .collect()
            .await;
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
                report.after_relevant = after.into_iter().filter(|d| d.is_relevant()).collect();
                report.verified_clean = report.after_relevant.is_empty();
            }
            Err(error) => report.error = Some(format!("post-read verify failed: {error}")),
        }
        report
    }

    /// Clear every ECU in `addrs`, sequentially, returning a per-ECU report.
    ///
    /// Sequential by design — writes stay lockstep even though reads fan out.
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
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use klartext_hsfz::{HsfzFrame, control, read_frame, write_frame};
    use tokio::net::TcpListener;

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
                    [0x19, 0x02, _] if cleared.contains(&ecu) => vec![0x59, 0x02, 0xFF],
                    [0x19, 0x02, _] => vec![
                        0x59, 0x02, 0xFF, //
                        0x00, 0x00, 0x01, 0x08, // confirmed (relevant)
                        0x00, 0x00, 0x02, 0x40, // not tested this cycle (noise)
                    ],
                    _ => continue,
                };
                let _ = write_frame(&mut stream, &HsfzFrame::diagnostic(ecu, tester, uds)).await;
            }
        });
        addr
    }

    async fn client(addr: std::net::SocketAddr) -> DiagnosticClient {
        let config = ClientConfig {
            port: addr.port(),
            ..ClientConfig::default()
        };
        DiagnosticClient::connect(addr.ip(), &config).await.unwrap()
    }

    fn opts() -> ScanOptions {
        ScanOptions {
            probe_timeout: Duration::from_millis(200),
            concurrency: 4,
        }
    }

    #[tokio::test]
    async fn scan_present_finds_only_fitted_ecus() {
        let addr = spawn(&[0x10, 0x12, 0x40]).await;
        let client = client(addr).await;
        let fitted = client
            .scan_present(&[0x10, 0x12, 0x18, 0x40, 0x60], opts())
            .await;
        let addrs: Vec<u8> = fitted.iter().map(|f| f.address).collect();
        assert_eq!(addrs, [0x10, 0x12, 0x40]);
    }

    #[tokio::test]
    async fn scan_faults_partitions_relevant_from_not_tested() {
        let addr = spawn(&[0x12]).await;
        let client = client(addr).await;
        let faults = client.scan_faults(&[0x12, 0x18], opts()).await;
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].address, 0x12);
        assert_eq!(faults[0].relevant.len(), 1);
        assert_eq!(faults[0].not_tested, 1);
        assert!(faults[0].error.is_none());
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
    async fn scan_concurrency_overlaps_absent_probes() {
        // Nothing present: five 200 ms probes must overlap, finishing well under
        // the ~1 s a serial sweep would take.
        let addr = spawn(&[]).await;
        let client = client(addr).await;
        let started = tokio::time::Instant::now();
        let fitted = client.scan_present(&[1, 2, 3, 4, 5], opts()).await;
        assert!(fitted.is_empty());
        assert!(
            started.elapsed() < Duration::from_millis(600),
            "a 4-wide scan should overlap the probes"
        );
    }
}
