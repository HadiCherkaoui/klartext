//! Whole-car orchestrations over the demuxed client.
//!
//! Two concrete procedures shared by the CLI and the MCP server: whole-car fault
//! reads (over the gateway SVT addresses — read → partition relevant vs
//! not-tested per ECU) and a verified whole-car clear (per ECU: pre-read →
//! extended session → standard `14 FF FF FF` → post-read verify).
//!
//! These are concrete procedures, not a general guided-procedure engine (that is
//! a named future milestone). Reads are autonomous-safe and fan out concurrently;
//! the clear is a state change, stays strictly sequential, and records each ECU's
//! stored faults before erasing them.

use futures::stream::{self, StreamExt};
use klartext_uds::Dtc;

use crate::client::DiagnosticClient;

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
    /// Read and partition faults for each address in `addrs`, bounded by `concurrency`.
    ///
    /// `addrs` is the fitted list from the gateway SVT ([`DiagnosticClient::read_ecu_list`]).
    /// A per-ECU read failure (e.g. an installed-but-silent ECU) is recorded in
    /// [`EcuFaults::error`], never aborting the scan. The result is sorted by address.
    pub async fn scan_faults(&self, addrs: &[u8], concurrency: usize) -> Vec<EcuFaults> {
        let mut out: Vec<EcuFaults> = stream::iter(addrs.iter().copied())
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
            .buffer_unordered(concurrency.max(1))
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

    #[tokio::test]
    async fn scan_faults_partitions_relevant_from_not_tested() {
        let addr = spawn(&[0x12]).await;
        let client = client(addr).await;
        let faults = client.scan_faults(&[0x12], 4).await;
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
        let faults = client.scan_faults(&[0x12, 0x18], 4).await;
        assert_eq!(faults.len(), 2);
        assert_eq!(faults[0].address, 0x12);
        assert!(faults[0].error.is_none());
        assert_eq!(faults[0].relevant.len(), 1);
        assert_eq!(faults[1].address, 0x18);
        assert!(faults[1].error.is_some());
        assert!(faults[1].relevant.is_empty());
        assert_eq!(faults[1].not_tested, 0);
    }
}
