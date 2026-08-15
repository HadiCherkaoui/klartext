//! Whole-car orchestrations over the demuxed client.
//!
//! Two concrete procedures used by the MCP server (and the future mobile app):
//! whole-car fault reads (over the gateway SVT addresses — read → partition
//! relevant vs not-tested per ECU) and ISTA's whole-vehicle clear sequence
//! ([`DiagnosticClient::clear_faults_all`]: pre-read → functional broadcast →
//! physical stragglers → supplier stores → gateway ZFS → terminal-15 cycle →
//! re-identify → verify).
//!
//! These are concrete procedures, not a general guided-procedure engine (that is
//! a named future milestone). Both walk the car one ECU at a time: reads are
//! autonomous-safe but the gateway will not carry them in parallel (see
//! [`DiagnosticClient::scan_faults`]), and the clear is a state change that
//! records each ECU's stored faults before erasing them.

use std::time::Duration;

use klartext_uds::{Dtc, service::clamp};

use crate::client::DiagnosticClient;
use crate::error::ClientError;

/// ISTA's settle after the clamp cycle, before re-identifying
/// (`SleepUtility.ThreadSleep(500, "VehicleIdent.ClearAndReadErrorInfoMemory")`).
const SETTLE_BEFORE_REIDENT: Duration = Duration::from_millis(500);

/// ISTA's settle between the re-identification and the verification read
/// (`SleepUtility.ThreadSleep(200, …)`).
const SETTLE_AFTER_REIDENT: Duration = Duration::from_millis(200);

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
    /// The ECU's `22 2000` info-memory (Infospeicher) entries, which ISTA reads and
    /// merges alongside faults (`VehicleIdent.cs:3535`, research §A.6/§E.4).
    ///
    /// Empty when the ECU keeps no info memory (`info_supported` is false) or keeps
    /// one but has nothing stored. Each entry reuses [`Dtc`] as the record shape, as
    /// on the wire (`IS_LESEN`'s records are identical to `FS_LESEN`'s).
    pub info: Vec<Dtc>,
    /// Whether the ECU keeps an info memory at all.
    ///
    /// `false` is the NORMAL case, not an error — only 342/1405 ECUs document
    /// `22 2000`, so an info-memory negative response degrades to this rather than
    /// populating [`EcuFaults::error`] (research §F.3).
    pub info_supported: bool,
    /// Set if reading this ECU failed (the scan continues past it).
    pub error: Option<String>,
}

/// What the car is made of, as ISTA's supplier gates read it.
///
/// The gates at `VehicleIdent.cs:9733-9785` test three distinct things, and mixing
/// them up is the documented trap: `ECU_SGBD` (the identified variant, e.g.
/// `FEM_20`), `ECU_GRUPPE` (the group, e.g. `D_0066`) and `HasSA` (a SALAPA option
/// code off the vehicle order). Build this from the ECUs actually identified on the
/// car, not from the per-model catalog — ISTA evaluates it against live `VecInfo`.
#[derive(Debug, Clone, Default)]
pub struct VehicleComposition {
    /// Identified variant (`ECU_SGBD`) names, e.g. `FEM_20`. Matched case-insensitively.
    pub sgbds: Vec<String>,
    /// Identified group (`ECU_GRUPPE`) names, e.g. `D_0066`. Matched case-insensitively.
    pub groups: Vec<String>,
    /// SALAPA option codes off the vehicle order (FA), e.g. `524`.
    ///
    /// `None` means **not known** — the caller could not read the vehicle order —
    /// as distinct from `Some(vec![])`, "read it, this car has no such option". A
    /// gate that needs a SALAPA code cannot be evaluated while this is `None`, and
    /// [`supplier_clear_jobs`] reports that rather than guessing either way.
    pub sa_codes: Option<Vec<String>>,
}

impl VehicleComposition {
    /// Whether an `ECU_SGBD` (variant) of this name was identified on the car.
    fn has_sgbd(&self, name: &str) -> bool {
        self.sgbds.iter().any(|s| s.eq_ignore_ascii_case(name))
    }

    /// Whether an `ECU_GRUPPE` of this name was identified on the car.
    ///
    /// `ECU_GRUPPE` may be a `|`-separated list on one ECU (`VehicleIdent.cs:680`),
    /// so each entry is split before comparing.
    fn has_group(&self, name: &str) -> bool {
        self.groups
            .iter()
            .flat_map(|g| g.split('|'))
            .any(|g| g.trim().eq_ignore_ascii_case(name))
    }

    /// ISTA's `HasSA(code)`: whether the vehicle order carries a SALAPA code.
    ///
    /// `None` when the option list is not decoded — distinct from a decoded list
    /// that simply lacks the code.
    fn has_sa(&self, code: &str) -> Option<bool> {
        let codes = self.sa_codes.as_ref()?;
        Some(codes.iter().any(|c| c.trim() == code))
    }
}

/// One supplier-specific store clear from ISTA's hardcoded list.
///
/// The `ecu` is the EDIABAS target ISTA names in the `apiJob` call — which is
/// frequently **not** the ECU the gate tested (see [`supplier_clear_jobs`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupplierJob {
    /// The EDIABAS SGBD or group the job is addressed to, e.g. `FEM_20`, `D_KBM`.
    pub ecu: &'static str,
    /// The EDIABAS job name, e.g. `IS_LOESCHEN_TMS`.
    pub job: &'static str,
    /// The job argument ISTA passes; `""` for most of them.
    pub arg: &'static str,
}

/// One supplier job's place in a clear sequence: what ISTA would run, and what
/// klartext did with it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SupplierJobReport {
    /// The job whose gate the car satisfied.
    pub job: SupplierJob,
    /// Why klartext did not execute it, or `None` if it ran.
    ///
    /// `None` once a [`SupplierJobRunner`] transmitted it successfully; otherwise the
    /// runner's error, or [`SUPPLIER_NOT_RUN`] when the sequence was given no runner
    /// at all.
    pub not_run: Option<String>,
}

/// Runs one EDIABAS job as a WRITE, for the clear sequence's supplier step.
///
/// ISTA addresses these by SGBD NAME rather than by diagnostic address —
/// `apiJob("FEM_20", "IS_LOESCHEN_TMS", "0x01", …)` — and lets EDIABAS resolve the
/// name to an ECU. klartext's BEST/2 VM lives ABOVE this crate (`klartext-client`
/// depends only on the transport and UDS layers), so the composing binary supplies
/// the runner and owns that resolution.
///
/// The contract is deliberately narrow because ISTA's is: it ignores every one of
/// these jobs' results, so "did it run" is the whole answer.
#[async_trait::async_trait]
pub trait SupplierJobRunner: Sync {
    /// Run `job` with `arg` on the ECU that `sgbd` names.
    ///
    /// # Errors
    /// A human message when the SGBD cannot be resolved, the job is absent, or the
    /// ECU refused — recorded against the job, never aborting the clear sequence.
    async fn run(&self, sgbd: &str, job: &str, arg: &str) -> Result<(), String>;
}

/// Why a supplier job did not run when NO runner was supplied.
///
/// With a [`SupplierJobRunner`] the jobs are transmitted and this is replaced by the
/// runner's own error, or by `None` on success.
const SUPPLIER_NOT_RUN: &str = "no job runner was supplied to the clear sequence, so this job was planned but not \
     transmitted (klartext-client cannot execute EDIABAS jobs itself — the BEST/2 VM \
     lives above it and the composing binary supplies the runner)";

/// ISTA's hardcoded supplier-specific info-memory clears, gated on vehicle composition.
///
/// Verbatim from `VehicleIdent.ClearErrorInfoMemoryVehicle`, `VehicleIdent.cs:9733-9785`,
/// in ISTA's own order. All six ECU names and all eleven job names are C# string
/// literals inside `if` statements testing `VecInfo` membership — there is no catalog
/// lookup, no table and no config file, and the identical block appears in the
/// `[Obsolete]` twin at `VehicleIdent.cs:6110-6162`. Replicating the hardcoding IS
/// parity; the behaviour is still car-dependent because the *conditions* read live
/// vehicle data. On a car with none of these ECUs the list is empty, which is correct.
///
/// **The gate is frequently not the ECU being addressed** — three traps:
/// 1. `D_KBM` is gated on `FRM_70`/`FRM_87` presence **and** SALAPA `524`, never on
///    `D_KBM` itself.
/// 2. `D_LM` is gated on `LM_AHL`/`LM_AHL_2`, never on `D_LM` itself.
/// 3. `D_0066` and `G_ZGW` are looked up by **`ECU_GRUPPE`**; the rest by `ECU_SGBD`.
///    (`VehicleIdent.cs:6158`, the obsolete twin, tests `getECUbyECU_SGBD("G_ZGW")`
///    where the live path at `:9781` tests `getECUbyECU_GRUPPE`. Follow the live path.)
///
/// `G_ZGW`/`STEUERN_ZFS_LOESCHEN` is deliberately **absent** from this list: it is
/// step 1.10 of the same phase but klartext sends it directly as a pinned UDS frame
/// ([`DiagnosticClient::clear_gateway_combined_store`]), not as an EDIABAS job, so it
/// is a real step of the sequence rather than a planned-but-unrun one.
///
/// # Nothing here is transmitted today
///
/// Every entry comes back with [`SupplierJobReport::not_run`] set. Running these
/// needs three things klartext does not have, established by disassembling each job
/// and running it in klartext's own BEST/2 VM:
/// - `D_KBM`, `D_0066` and `D_LM` are EDIABAS **group** files (`d_kbm.grp` and
///   siblings) carrying only `INITIALISIERUNG` and `IDENTIFIKATION` — the
///   `IS_LOESCHEN_SMC_*` jobs are not in them. Reaching the job needs EDIABAS
///   group→variant dispatch, a subsystem klartext has not built.
/// - The jobs that ARE reachable emit services the read-only transmit gate refuses:
///   `FEM_20/IS_LOESCHEN_TMS` emits `31 01 1009 29 00 00 04 14 FF FF FF` and
///   `FRM3/IS_LOESCHEN_TMS_L_LEAR` emits `31 01 1009 01 00 04 14 FF FF FF` (a clear
///   tunnelled through a RoutineControl), while `ALC_60/IS_LOESCHEN_SMC_L_LEAR`
///   emits `A6 89 03 31 06` — a **proprietary `0xA6` service**, not the `0x14`/`0x31`
///   the P2.1 research assumed. All three classify as `SidClass::Gated`, so they need
///   `Policy::ConfirmedWrite` — a P3 service-write decision (preconditions plus a
///   `Reset` phase), not a clear-sequence one.
/// - Executing any of them needs the SGBD `.prg` files at run time (BYO data).
///
/// Reporting the plan is still worth its keep: it is the parity-critical half (ISTA
/// **ignores every one of these jobs' results**, so selection is the entire
/// observable behaviour), it is offline-testable, and it tells the human exactly
/// which supplier stores klartext left untouched.
///
/// Research: `docs/superpowers/specs/2026-07-18-research-p2-clear-sequence.md` §C.
pub fn supplier_clear_jobs(vehicle: &VehicleComposition) -> Vec<SupplierJobReport> {
    let mut jobs: Vec<SupplierJob> = Vec::new();
    let mut push = |ecu, job, arg| jobs.push(SupplierJob { ecu, job, arg });

    // 1.3 — VehicleIdent.cs:9733-9739. Two calls, same job, different argument.
    if vehicle.has_sgbd("FEM_20") {
        push("FEM_20", "IS_LOESCHEN_TMS", "0x01");
        push("FEM_20", "IS_LOESCHEN_TMS", "0x02");
    }
    // 1.4 — VehicleIdent.cs:9740-9746.
    if vehicle.has_sgbd("FRM3") {
        push("FRM3", "IS_LOESCHEN_TMS_L_LEAR", "");
        push("FRM3", "IS_LOESCHEN_TMS_R_LEAR", "");
    }
    // 1.6 — VehicleIdent.cs:9753-9759. Trap 1: the FRM gates the KBM, and the SALAPA
    // code is a second condition. `has_sa` is None when the FA option list is not
    // decoded; an undecodable SALAPA must not be read as "present" — that would fire
    // a supplier write on a car ISTA would have skipped.
    if (vehicle.has_sgbd("FRM_70") || vehicle.has_sgbd("FRM_87"))
        && vehicle.has_sa("524") == Some(true)
    {
        push("D_KBM", "IS_LOESCHEN_SMC_L_LEAR", "");
        push("D_KBM", "IS_LOESCHEN_SMC_R_LEAR", "");
    }
    // 1.7 — VehicleIdent.cs:9760-9766. Trap 3: GRUPPE, not SGBD.
    if vehicle.has_group("D_0066") {
        push("D_0066", "IS_LOESCHEN_SMC_L", "");
        push("D_0066", "IS_LOESCHEN_SMC_R", "");
    }
    // 1.8 — VehicleIdent.cs:9767-9773.
    if vehicle.has_sgbd("ALC_60") {
        push("ALC_60", "IS_LOESCHEN_SMC_L_LEAR", "");
        push("ALC_60", "IS_LOESCHEN_SMC_R_LEAR", "");
    }
    // 1.9 — VehicleIdent.cs:9774-9780. Trap 2: the AHL lamp modules gate D_LM.
    if vehicle.has_sgbd("LM_AHL") || vehicle.has_sgbd("LM_AHL_2") {
        push("D_LM", "IS_LOESCHEN_SMC_L_LEAR", "");
        push("D_LM", "IS_LOESCHEN_SMC_R_LEAR", "");
    }

    jobs.into_iter()
        .map(|job| SupplierJobReport {
            job,
            not_run: Some(SUPPLIER_NOT_RUN.to_string()),
        })
        .collect()
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

/// Why the post-clear terminal-15 cycle failed.
///
/// `restored` is the field that matters and the reason this is not a plain string:
/// `false` means **terminal 15 may still be DOWN** and the car may not start until it
/// is raised. Surface the two cases with different words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClampCycleFailure {
    /// Whether terminal 15 is known to be up again.
    pub restored: bool,
    /// The failure, already carrying the restore warning when `restored` is false.
    pub message: String,
}

/// The record of one whole-vehicle clear sequence, phase by phase.
///
/// Every field is an outcome, never a reason to have stopped — see
/// [`DiagnosticClient::clear_faults_all`], which never aborts.
#[derive(Debug, Clone)]
pub struct ClearSequenceReport {
    /// Every fitted ECU's stored faults, read BEFORE anything was erased.
    pub before: Vec<EcuFaults>,
    /// The addresses that answered the functional broadcast, or why it failed.
    ///
    /// `Ok` with an empty list means the broadcast went out and nobody answered —
    /// not a failure, and the reason every faulted ECU then becomes a straggler.
    pub functional: Result<Vec<u8>, String>,
    /// The per-ECU physical clears for the ECUs that did not answer the broadcast.
    pub stragglers: Vec<ClearReport>,
    /// The supplier-specific stores ISTA clears, and what klartext did with each.
    pub supplier_jobs: Vec<SupplierJobReport>,
    /// The gateway's combined fault store (ZFS) clear.
    pub gateway_zfs: Result<(), String>,
    /// ISTA's post-clear terminal-15 OFF → 15 s → ON cycle.
    pub clamp_cycle: Result<(), ClampCycleFailure>,
    /// The post-clamp re-identification: the fitted list, re-read from the gateway.
    pub reident: Result<Vec<u8>, String>,
    /// The post-clear whole-car verification read.
    pub verification: Vec<EcuFaults>,
}

/// One ECU's before/after verdict across a whole clear sequence.
///
/// See [`ClearSequenceReport::ecu_verdicts`] — this is klartext's addition, not ISTA's.
#[derive(Debug, Clone)]
pub struct EcuClearVerdict {
    /// The diagnostic address.
    pub address: u8,
    /// Every DTC stored before the clear — the record of what was discarded.
    pub before: Vec<Dtc>,
    /// Whether this ECU answered the functional broadcast.
    pub answered_broadcast: bool,
    /// Whether this ECU was additionally cleared by a physical straggler pass.
    pub cleared_physically: bool,
    /// The faults still stored after the whole sequence.
    pub after: Vec<Dtc>,
    /// True if the ECU was read both before and after, and nothing remains.
    pub verified_clean: bool,
    /// The first failure recorded for this ECU, if any.
    pub error: Option<String>,
}

impl ClearSequenceReport {
    /// Roll the phases up into one before/after verdict per ECU.
    ///
    /// **This has no ISTA counterpart** — ISTA never compares before against after
    /// (`VehicleIdent.cs:9790-9812`; residual faults just reappear in the refreshed
    /// list and nothing branches on them). klartext keeps the diff because it answers
    /// the question the human actually asked: did the clear work?
    ///
    /// `verified_clean` requires evidence on both sides. An ECU that could not be
    /// read afterwards is NOT clean — it is unknown, and carries the read error.
    pub fn ecu_verdicts(&self) -> Vec<EcuClearVerdict> {
        let answered = self.functional.as_deref().unwrap_or(&[]);
        self.before
            .iter()
            .map(|before| {
                let straggler = self.stragglers.iter().find(|s| s.address == before.address);
                let after = self
                    .verification
                    .iter()
                    .find(|v| v.address == before.address);
                // The pre-read failure comes first: it is why nothing was cleared.
                let error = before
                    .error
                    .clone()
                    .or_else(|| straggler.and_then(|s| s.error.clone()))
                    .or_else(|| after.and_then(|a| a.error.clone()));
                let read_after = after.is_some_and(|a| a.error.is_none());
                EcuClearVerdict {
                    address: before.address,
                    before: before.faults.clone(),
                    answered_broadcast: answered.contains(&before.address),
                    cleared_physically: straggler.is_some(),
                    after: after.map(|a| a.faults.clone()).unwrap_or_default(),
                    verified_clean: before.error.is_none()
                        && read_after
                        && after.is_some_and(|a| a.faults.is_empty()),
                    error,
                }
            })
            .collect()
    }
}

impl DiagnosticClient {
    /// Read each address in `addrs` the way ISTA does — fault + info memory — one ECU
    /// at a time.
    ///
    /// `addrs` is the fitted list from the gateway SVT ([`DiagnosticClient::read_ecu_list`]).
    /// Each ECU is read with [`DiagnosticClient::read_ecu_faults`], so the sweep now
    /// issues **two** requests per ECU (the `19 02 0C` fault read plus the `22 2000`
    /// info read, research §E.4) rather than one — which is why keeping it strictly
    /// sequential matters more, not less. A per-ECU read failure (e.g. an
    /// installed-but-silent ECU) is recorded in [`EcuFaults::error`], never aborting
    /// the scan. The result is sorted by address.
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
            out.push(match self.read_ecu_faults(address).await {
                Ok(bundle) => EcuFaults {
                    address,
                    faults: bundle.faults,
                    info: bundle.info,
                    info_supported: bundle.info_supported,
                    error: None,
                },
                Err(error) => EcuFaults {
                    address,
                    faults: Vec::new(),
                    info: Vec::new(),
                    info_supported: false,
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

    /// Run ISTA's whole-vehicle clear sequence over the fitted ECUs in `addrs`.
    ///
    /// This is `VehicleIdent.ClearAndReadErrorInfoMemory` (`VehicleIdent.cs:9620`),
    /// non-motorcycle branch, in ISTA's order:
    ///
    /// 1. **pre-read** every fitted ECU — *klartext only*, see below
    /// 2. **functional clear**: one `14 FF FF FF` broadcast to `0xDF`
    ///    ([`DiagnosticClient::clear_all_dtcs_functional`], ISTA's `FS_LOESCHEN_FUNKTIONAL`)
    /// 3. **physical stragglers**: per-ECU `14 FF FF FF` for whatever stayed silent
    /// 4. **supplier jobs**: the six hardcoded supplier stores ([`supplier_clear_jobs`])
    /// 5. **gateway ZFS**: [`DiagnosticClient::clear_gateway_combined_store`]
    /// 6. **clamp cycle**: terminal 15 OFF → 15 s → ON ([`DiagnosticClient::cycle_terminal_15`])
    /// 7. 500 ms settle → **re-identification** → 200 ms settle
    /// 8. **verification read**: the whole car's fault memories again
    ///
    /// **Nothing aborts.** Every step is best-effort and its outcome is recorded, as
    /// in ISTA, whose clear phase wraps everything in one `catch` that only logs
    /// (`VehicleIdent.cs:660-663`) and whose nine supplier `apiJob` calls are
    /// completely unguarded. A failure mid-sequence leaves some ECUs cleared and some
    /// not; the report says which rather than retrying blind.
    ///
    /// # This drops the car's terminal 15 for fifteen seconds
    ///
    /// Step 6 is unconditional in ISTA and unconditional here, so the whole call
    /// blocks for >15 s. It is what resets the instrument cluster after a clear.
    ///
    /// # Deliberate divergences from ISTA, both in klartext's favour
    ///
    /// - **The pre-read (step 1) has no ISTA counterpart.** klartext never clears
    ///   blind: an ECU whose fault memory could not be read first is not cleared, and
    ///   every erased code is recorded in the report. It also supplies the straggler
    ///   rule's "had faults" term.
    /// - **`verified_clean` has no ISTA counterpart.** ISTA's verification read is
    ///   seven steps and it never diffs before against after — residual faults simply
    ///   reappear in the refreshed list, nothing branches on whether the clear worked
    ///   (`VehicleIdent.cs:9790-9812`, and the method returns `void`). klartext diffs
    ///   per ECU ([`ClearSequenceReport::ecu_verdicts`]). That is strictly more
    ///   informative and is kept on purpose.
    ///
    /// Two further gaps are honest under-implementation, not improvements: the
    /// verification read is ISTA's step 8.1 only (its per-fault detail reads, the
    /// functional info-memory read and its details, the gateway ZFS *read* and the
    /// check-control read are not performed), and no supplier job is transmitted.
    ///
    /// # What is deliberately NOT sent
    ///
    /// - **No UDS `0x11` ECU reset**, anywhere. ISTA's clear flow sends none; the
    ///   cluster reset it appears to perform is the terminal-15 cycle at step 6.
    ///   klartext sent a reset until 2026-07-18 (parity audit P0.1).
    /// - **No general info-memory clear.** `IS_LOESCHEN_FUNKTIONAL` and per-ECU
    ///   `IS_LOESCHEN` are dead code in ISTA: the guard at `VehicleIdent.cs:9747` is
    ///   provably always false, because `VehicleIdent`'s constructor
    ///   (`VehicleIdent.cs:581`) has already inserted the key `TryGetService` tests —
    ///   `ServiceLocator.GetService` adds a null entry on a miss. ISTA broadly
    ///   *reads* info memory but only ever clears the six hardcoded supplier stores.
    /// - **No broadcast `10 03`.** The physical clear keeps its on-car-confirmed
    ///   extended-session prefix; the functional job emits none.
    ///
    /// Research: `docs/superpowers/specs/2026-07-18-research-p2-clear-sequence.md`.
    pub async fn clear_faults_all(
        &self,
        addrs: &[u8],
        vehicle: &VehicleComposition,
        supplier_runner: Option<&(dyn SupplierJobRunner + Send)>,
    ) -> ClearSequenceReport {
        self.clear_faults_all_holding(
            addrs,
            vehicle,
            supplier_runner,
            Duration::from_millis(clamp::OFF_DURATION_MS),
        )
        .await
    }

    /// [`clear_faults_all`](Self::clear_faults_all) with the clamp hold made explicit.
    ///
    /// The seam exists so tests can drive the whole sequence — including a real
    /// terminal-15 OFF/ON pair — without waiting fifteen seconds each time. Production
    /// has exactly one legitimate hold, so the public method takes no argument and
    /// nothing outside this crate can shorten it. The two settle sleeps are NOT
    /// parameterised: they are short enough to run for real, so tests exercise
    /// ISTA's actual timings.
    pub(crate) async fn clear_faults_all_holding(
        &self,
        addrs: &[u8],
        vehicle: &VehicleComposition,
        supplier_runner: Option<&(dyn SupplierJobRunner + Send)>,
        clamp_hold: Duration,
    ) -> ClearSequenceReport {
        // 1. Pre-read, before anything is erased: the discard record, and the
        //    "had faults" term of the straggler rule.
        let before = self.scan_faults(addrs).await;

        // 2. One functional broadcast. A transport failure is recorded, not fatal:
        //    with no responder list every fitted ECU that had faults becomes a
        //    straggler, so the sequence degrades to the per-ECU clear it replaced.
        let functional = self
            .clear_all_dtcs_functional()
            .await
            .map_err(|e| e.to_string());
        let answered: &[u8] = match &functional {
            Ok(addrs) => addrs,
            Err(_) => &[],
        };

        // 3. Physical stragglers: fitted AND had faults AND did not answer the
        //    broadcast. ISTA reaches the same set by the opposite route — its
        //    broadcast pass zeroes the fault state of every ECU that answered, so
        //    its `FEHLER.Count > 0` test at VehicleIdent.cs:651 naturally leaves the
        //    silent ones. Reading the responder list directly says the same thing
        //    without keeping the mutation.
        let mut stragglers = Vec::new();
        for ecu in &before {
            let had_faults = !ecu.faults.is_empty();
            let unreadable = ecu.error.is_some();
            if (had_faults || unreadable) && !answered.contains(&ecu.address) {
                stragglers.push(self.clear_faults_verified(ecu.address).await);
            }
        }

        // 4. The supplier-specific stores. ISTA ignores every one of these jobs'
        //    results, so a failure is recorded against the job and the sequence
        //    carries on — as every other step here does.
        let mut supplier_jobs = supplier_clear_jobs(vehicle);
        if let Some(runner) = supplier_runner {
            for report in &mut supplier_jobs {
                report.not_run = runner
                    .run(report.job.ecu, report.job.job, report.job.arg)
                    .await
                    .err();
            }
        }

        // 5. The gateway's own combined fault store.
        let gateway_zfs = self
            .clear_gateway_combined_store()
            .await
            .map_err(|e| e.to_string());

        // 6. The clamp cycle — the cluster reset. Blocks for `clamp_hold`.
        let clamp_cycle = self
            .cycle_terminal_15_holding(clamp_hold)
            .await
            .map_err(|error| ClampCycleFailure {
                restored: !matches!(
                    &error,
                    ClientError::ClampSwitch {
                        restored: false,
                        ..
                    }
                ),
                message: error.to_string(),
            });

        // 7. Settle, re-identify, settle. The re-identification re-establishes the
        //    ECU list after the clamp cycle dropped and restored terminal 15, which
        //    is why it sits between the two sleeps.
        tokio::time::sleep(SETTLE_BEFORE_REIDENT).await;
        let reident = self
            .read_ecu_list()
            .await
            .map(|list| list.addresses)
            .map_err(|e| e.to_string());
        tokio::time::sleep(SETTLE_AFTER_REIDENT).await;

        // 8. The verification read, over whatever the re-identification found (the
        //    pre-read's list if it failed — a failed re-ident must not silently
        //    shrink the set that gets verified).
        let verify_addrs: Vec<u8> = match &reident {
            Ok(found) if !found.is_empty() => found.clone(),
            _ => addrs.to_vec(),
        };
        let verification = self.scan_faults(&verify_addrs).await;

        ClearSequenceReport {
            before,
            functional,
            stragglers,
            supplier_jobs,
            gateway_zfs,
            clamp_cycle,
            reident,
            verification,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::net::Ipv4Addr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    use klartext_hsfz::{HsfzFrame, control, read_frame, write_frame};
    use klartext_uds::service::clamp;
    use tokio::net::TcpListener;

    use super::{SupplierJobRunner, VehicleComposition, supplier_clear_jobs};
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
                    // The fault+info bundle's info read: this mock ECU keeps no info
                    // memory, so 22 2000 answers a clean negative and the bundle
                    // degrades to info_supported=false — never a timeout that would
                    // wrongly mark the ECU errored.
                    [0x22, 0x20, 0x00] => vec![0x7F, 0x22, 0x31],
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
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                let (tester, ecu) = frame.addr.unwrap();
                // The bundle issues a 22 2000 info read after each 19 02 fault read.
                // Answer it immediately with a clean negative so it degrades to
                // info_supported=false without a timeout, and keep it OUT of the depth
                // census below: this probe measures the FAULT-read fan-out, which is
                // what the strictly-sequential guarantee is about.
                if frame.payload.first() == Some(&0x22) {
                    let reply = HsfzFrame::diagnostic(ecu, tester, vec![0x7F, 0x22, 0x31]);
                    let mut writer = write.lock().await;
                    let _ = write_frame(&mut *writer, &reply).await;
                    continue;
                }
                if frame.payload.first() != Some(&0x19) {
                    continue;
                }
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

    /// A loopback car that can carry the WHOLE ISTA clear sequence, recording every
    /// `(target, payload)` transmitted so a test can assert on order and absence.
    ///
    /// `faulted` are the ECUs that hold a DTC before the clear; `broadcast_responders`
    /// are the ECUs that answer the functional `0xDF` clear (each from its own source
    /// address, as a real broadcast does). An ECU that answers the broadcast is
    /// cleared by it; the rest keep their fault until physically cleared. The gateway
    /// (`0x10`) additionally serves the SVT re-identification and the ZFS routine, and
    /// `0x40` serves the two clamp payloads — so a sequence step that is dropped shows
    /// up as a MISSING frame rather than as a timeout.
    async fn spawn_car(
        fitted: &'static [u8],
        faulted: &'static [u8],
        broadcast_responders: &'static [u8],
    ) -> (std::net::SocketAddr, crate::client::tests::FrameLog) {
        spawn_car_with_unreadable(fitted, faulted, broadcast_responders, &[]).await
    }

    /// [`spawn_car`], plus ECUs whose FAULT READ alone fails.
    ///
    /// `unreadable` ECUs stay silent for `19 02 0C` but answer `10 03` and
    /// `14 FF FF FF` normally. That separation is what makes the never-clear-blind
    /// rule testable: an ECU that is simply absent cannot prove the rule, because its
    /// extended-session request fails first and the clear is never reached anyway.
    /// Here the transport would happily carry the clear — only the guard stops it.
    async fn spawn_car_with_unreadable(
        fitted: &'static [u8],
        faulted: &'static [u8],
        broadcast_responders: &'static [u8],
        unreadable: &'static [u8],
    ) -> (std::net::SocketAddr, crate::client::tests::FrameLog) {
        spawn_car_inner(fitted, faulted, broadcast_responders, unreadable, true).await
    }

    /// [`spawn_car`], but the clamp ECU accepts terminal 15 OFF and then never
    /// answers ON — the one failure that leaves the car with its ignition down.
    ///
    /// This is the shape a mock cannot produce by simply going silent: an
    /// unresponsive clamp ECU fails at OFF, which is the SAFE failure (nothing was
    /// dropped). Only a car that takes the OFF and refuses the ON exercises the
    /// `restored == false` path.
    async fn spawn_car_where_clamp_cannot_restore(
        fitted: &'static [u8],
        faulted: &'static [u8],
        broadcast_responders: &'static [u8],
    ) -> (std::net::SocketAddr, crate::client::tests::FrameLog) {
        spawn_car_inner(fitted, faulted, broadcast_responders, &[], false).await
    }

    async fn spawn_car_inner(
        fitted: &'static [u8],
        faulted: &'static [u8],
        broadcast_responders: &'static [u8],
        unreadable: &'static [u8],
        answer_clamp_on: bool,
    ) -> (std::net::SocketAddr, crate::client::tests::FrameLog) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let log: crate::client::tests::FrameLog = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut cleared: std::collections::HashSet<u8> = Default::default();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC || frame.payload == [0x3E, 0x80] {
                    continue;
                }
                let (tester, target) = frame.addr.unwrap();
                sink.lock().unwrap().push((target, frame.payload.clone()));

                // The functional clear: one reply per responder, from its own address.
                if target == klartext_uds::FUNCTIONAL_ADDRESS_F01 {
                    if frame.payload == [0x14, 0xFF, 0xFF, 0xFF] {
                        for &responder in broadcast_responders {
                            cleared.insert(responder);
                            let reply = HsfzFrame::diagnostic(responder, tester, vec![0x54]);
                            let _ = write_frame(&mut stream, &reply).await;
                        }
                    }
                    continue;
                }
                if !fitted.contains(&target) {
                    continue; // not on this car: silence
                }
                let uds = match frame.payload.as_slice() {
                    [0x3E, 0x00] => vec![0x7E, 0x00],
                    [0x10, 0x03] => vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88],
                    [0x14, 0xFF, 0xFF, 0xFF] => {
                        cleared.insert(target);
                        vec![0x54]
                    }
                    // The SVT re-identification (22 3F 07) from the gateway.
                    [0x22, 0x3F, 0x07] if target == 0x10 => {
                        let mut uds = vec![0x62, 0x3F, 0x07, 0x00, fitted.len() as u8];
                        uds.extend_from_slice(fitted);
                        uds
                    }
                    // The gateway's combined-store (ZFS) routine.
                    [0x31, 0x01, 0x40, 0x00, 0x00] if target == 0x10 => {
                        vec![0x71, 0x01, 0x40, 0x00]
                    }
                    // The two terminal-15 payloads, on the clamp ECU. When
                    // `answer_clamp_on` is false the OFF is accepted and the ON is
                    // ignored, leaving terminal 15 down.
                    [0x31, 0x01, 0x10, 0x01, ..]
                        if target == clamp::TARGET
                            && (answer_clamp_on || frame.payload.as_slice() == clamp::KL15_OFF) =>
                    {
                        vec![0x71, 0x01, 0x10, 0x01]
                    }
                    // An ECU reset is answered POSITIVELY, deliberately: klartext must
                    // send none (parity audit P0.1), and serving one means a
                    // reintroduced reset lands in the log as a real frame instead of
                    // hiding as a read timeout.
                    [0x11, 0x01] => vec![0x51, 0x01],
                    // Only ISTA's mask is served: a regression to `19 02 FF` falls
                    // through to silence rather than quietly getting the same answer.
                    [0x19, 0x02, 0x0C] if unreadable.contains(&target) => continue,
                    [0x19, 0x02, 0x0C] => {
                        if faulted.contains(&target) && !cleared.contains(&target) {
                            vec![0x59, 0x02, 0x0C, 0x00, 0x00, 0x02, 0x2F]
                        } else {
                            vec![0x59, 0x02, 0x0C]
                        }
                    }
                    // The bundle's info read (scan_faults now reads fault + info per
                    // ECU). These mock ECUs keep no info memory, so 22 2000 answers a
                    // clean negative and the pre-read/verification scans degrade to
                    // info_supported=false instead of timing out.
                    [0x22, 0x20, 0x00] => vec![0x7F, 0x22, 0x31],
                    _ => continue,
                };
                let _ = write_frame(&mut stream, &HsfzFrame::diagnostic(target, tester, uds)).await;
            }
        });
        (addr, log)
    }

    /// A client whose read timeout — and therefore the broadcast quiet period — is
    /// short enough that a whole sequence runs in well under a second.
    async fn fast_client(addr: std::net::SocketAddr) -> DiagnosticClient {
        let config = ClientConfig {
            port: addr.port(),
            read_timeout: Duration::from_millis(150),
            ..ClientConfig::default()
        };
        DiagnosticClient::connect(addr.ip(), &config).await.unwrap()
    }

    /// The clamp hold used in tests: the real sequence is run, but not for 15 s.
    const TEST_CLAMP_HOLD: Duration = Duration::from_millis(1);

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
        // 0x12 keeps no info memory (the mock negatives 22 2000): the bundle degrades
        // to info_supported=false with no info entries, and this is NOT an error.
        assert!(!faults[0].info_supported);
        assert!(faults[0].info.is_empty());
        assert_eq!(faults[1].address, 0x18);
        assert!(faults[1].error.is_some());
        assert!(faults[1].faults.is_empty());
        // The errored ECU carries the default info fields, never a stale merge.
        assert!(faults[1].info.is_empty());
        assert!(!faults[1].info_supported);
    }

    /// `scan_faults` merges each ECU's info memory alongside its faults, as ISTA reads
    /// both stores per ECU (research §A.6/§E.4): a supported `22 2000` populates
    /// `info` and sets `info_supported`. Dropping the info read from the bundle leaves
    /// `info` empty and fails this.
    #[tokio::test]
    async fn scan_faults_merges_info_memory_entries_when_supported() {
        let addr = crate::client::tests::spawn_gateway_multi(&[
            (
                0x12,
                vec![0x19, 0x02, 0x0C],
                vec![0x59, 0x02, 0x0C, 0x4A, 0x12, 0x34, 0x08],
            ),
            (
                0x12,
                vec![0x22, 0x20, 0x00],
                vec![0x62, 0x20, 0x00, 0xC9, 0x0D, 0x60, 0x2F],
            ),
        ])
        .await;
        let client = client(addr).await;

        let scanned = client.scan_faults(&[0x12]).await;
        assert_eq!(scanned.len(), 1);
        assert!(scanned[0].error.is_none());
        assert_eq!(scanned[0].faults.len(), 1);
        // The info entry rides alongside the fault, marked supported.
        assert_eq!(scanned[0].info.len(), 1);
        assert_eq!(scanned[0].info[0].code, [0xC9, 0x0D, 0x60]);
        assert!(scanned[0].info_supported);
    }

    /// The sequence must follow ISTA's order, and the functional broadcast must come
    /// FIRST — it is what makes the physical pass a straggler pass rather than the
    /// per-ECU loop klartext ran until 2026-07-18. The census pins the whole ordered
    /// shape: pre-read, ONE broadcast, physical only for the silent faulted ECU,
    /// gateway ZFS, both clamp payloads, the SVT re-ident, then the verification read.
    #[tokio::test]
    async fn clear_sequence_broadcasts_first_then_clears_only_the_stragglers() {
        // 0x12 and 0x40 hold faults; only 0x12 answers the broadcast. 0x10 is the
        // gateway, fitted and fault-free.
        let (addr, log) = spawn_car(&[0x10, 0x12, 0x40], &[0x12, 0x40], &[0x12]).await;
        let client = fast_client(addr).await;

        let report = client
            .clear_faults_all_holding(
                &[0x10, 0x12, 0x40],
                &VehicleComposition::default(),
                None,
                TEST_CLAMP_HOLD,
            )
            .await;

        assert_eq!(report.functional.as_deref(), Ok(&[0x12][..]));
        // Only 0x40 is a straggler: faulted AND silent on the broadcast. 0x12 answered
        // (so it is already clear) and 0x10 had nothing to clear.
        let straggler_addrs: Vec<u8> = report.stragglers.iter().map(|s| s.address).collect();
        assert_eq!(straggler_addrs, vec![0x40]);

        let sent = log.lock().unwrap().clone();
        let clears: Vec<(u8, Vec<u8>)> = sent
            .iter()
            .filter(|(_, p)| p.first() == Some(&0x14))
            .cloned()
            .collect();
        assert_eq!(
            clears,
            vec![
                (0xDF, vec![0x14, 0xFF, 0xFF, 0xFF]),
                (0x40, vec![0x14, 0xFF, 0xFF, 0xFF]),
            ],
            "the broadcast must come first, and only the silent faulted ECU may be \
             cleared physically afterwards — got {sent:02X?}"
        );

        // The whole ordered spine, by (target, first byte), with the per-ECU reads
        // collapsed so the assertion is about phase order rather than ECU count. The
        // scan phases' reads — 19 02 fault AND 22 2000 info — are both filtered out;
        // the only 0x22 that belongs to the spine is the SVT re-identification
        // (22 3F 07), so it is matched by its full payload, not by its SID alone.
        let spine: Vec<(u8, u8)> = sent
            .iter()
            .filter(|(_, p)| matches!(p.as_slice(), [0x14, ..] | [0x31, ..] | [0x22, 0x3F, 0x07]))
            .map(|(t, p)| (*t, p[0]))
            .collect();
        assert_eq!(
            spine,
            vec![
                (0xDF, 0x14), // functional clear
                (0x40, 0x14), // straggler
                (0x10, 0x31), // gateway ZFS
                (0x40, 0x31), // clamp OFF
                (0x40, 0x31), // clamp ON
                (0x10, 0x22), // SVT re-identification
            ],
            "ISTA's phase order, from {sent:02X?}"
        );

        assert_eq!(report.gateway_zfs, Ok(()));
        assert_eq!(report.clamp_cycle, Ok(()));
        assert_eq!(report.reident.as_deref(), Ok(&[0x10, 0x12, 0x40][..]));
        assert_eq!(report.verification.len(), 3);
    }

    /// A runner that records every job it was asked to run, and can be told to fail.
    struct RecordingSupplierRunner {
        seen: std::sync::Mutex<Vec<(String, String, String)>>,
        fail: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl SupplierJobRunner for RecordingSupplierRunner {
        async fn run(&self, sgbd: &str, job: &str, arg: &str) -> Result<(), String> {
            self.seen
                .lock()
                .unwrap()
                .push((sgbd.to_string(), job.to_string(), arg.to_string()));
            match self.fail {
                Some(message) => Err(message.to_string()),
                None => Ok(()),
            }
        }
    }

    /// A car whose composition selects the FEM_20 supplier pair.
    fn fem20_car() -> VehicleComposition {
        VehicleComposition {
            sgbds: vec!["FEM_20".to_string()],
            ..VehicleComposition::default()
        }
    }

    /// With a runner the supplier jobs are TRANSMITTED, each with the SGBD, job name
    /// and argument ISTA passes — and a success clears `not_run`.
    #[tokio::test]
    async fn supplier_jobs_are_run_when_a_runner_is_supplied() {
        let (addr, _log) = spawn_car(&[0x10, 0x12, 0x40], &[0x12], &[0x12]).await;
        let client = fast_client(addr).await;
        let runner = RecordingSupplierRunner {
            seen: std::sync::Mutex::new(Vec::new()),
            fail: None,
        };

        let report = client
            .clear_faults_all_holding(
                &[0x10, 0x12, 0x40],
                &fem20_car(),
                Some(&runner),
                TEST_CLAMP_HOLD,
            )
            .await;

        assert_eq!(
            runner.seen.lock().unwrap().clone(),
            vec![
                ("FEM_20".into(), "IS_LOESCHEN_TMS".into(), "0x01".into()),
                ("FEM_20".into(), "IS_LOESCHEN_TMS".into(), "0x02".into()),
            ],
            "ISTA runs the same job twice with different arguments"
        );
        assert!(
            report.supplier_jobs.iter().all(|j| j.not_run.is_none()),
            "a job that ran must not be reported not-run: {:?}",
            report.supplier_jobs
        );
    }

    /// A failing supplier job is RECORDED, not fatal — ISTA ignores these results
    /// entirely, so everything after them must still happen.
    #[tokio::test]
    async fn a_failed_supplier_job_is_recorded_and_the_sequence_continues() {
        let (addr, _log) = spawn_car(&[0x10, 0x12, 0x40], &[0x12], &[0x12]).await;
        let client = fast_client(addr).await;
        let runner = RecordingSupplierRunner {
            seen: std::sync::Mutex::new(Vec::new()),
            fail: Some("ECU refused"),
        };

        let report = client
            .clear_faults_all_holding(
                &[0x10, 0x12, 0x40],
                &fem20_car(),
                Some(&runner),
                TEST_CLAMP_HOLD,
            )
            .await;

        assert!(
            report
                .supplier_jobs
                .iter()
                .all(|j| j.not_run.as_deref() == Some("ECU refused")),
            "the runner's error must be recorded against each job"
        );
        assert_eq!(report.clamp_cycle, Ok(()), "the sequence must carry on");
    }

    /// The clamp cycle is ISTA's cluster reset and is unconditional in its flow, so
    /// it must be unconditional here — both payloads, in order, on every clear.
    #[tokio::test]
    async fn clear_sequence_always_cycles_terminal_15() {
        let (addr, log) = spawn_car(&[0x10, 0x12, 0x40], &[], &[0x12]).await;
        let client = fast_client(addr).await;

        let report = client
            .clear_faults_all_holding(
                &[0x10, 0x12, 0x40],
                &VehicleComposition::default(),
                None,
                TEST_CLAMP_HOLD,
            )
            .await;

        assert_eq!(report.clamp_cycle, Ok(()));
        let clamp_frames: Vec<Vec<u8>> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|(t, p)| *t == clamp::TARGET && p.starts_with(&[0x31, 0x01, 0x10, 0x01]))
            .map(|(_, p)| p.clone())
            .collect();
        assert_eq!(
            clamp_frames,
            vec![clamp::KL15_OFF.to_vec(), clamp::KL15_ON.to_vec()],
            "terminal 15 must go OFF then back ON — this is what resets the cluster, \
             and ISTA performs it after every clear with no condition at the call site"
        );
    }

    /// A car with nothing stored is still fully swept: the sequence must not skip the
    /// broadcast, the gateway store or the clamp cycle just because the pre-read was
    /// clean. It must, however, clear no ECU physically — there is no straggler.
    #[tokio::test]
    async fn clear_sequence_on_a_clean_car_sends_no_physical_clear() {
        let (addr, log) = spawn_car(&[0x10, 0x12, 0x40], &[], &[0x12]).await;
        let client = fast_client(addr).await;

        let report = client
            .clear_faults_all_holding(
                &[0x10, 0x12, 0x40],
                &VehicleComposition::default(),
                None,
                TEST_CLAMP_HOLD,
            )
            .await;

        assert!(report.stragglers.is_empty());
        let clears: Vec<(u8, Vec<u8>)> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p.first() == Some(&0x14))
            .cloned()
            .collect();
        assert_eq!(clears, vec![(0xDF, vec![0x14, 0xFF, 0xFF, 0xFF])]);
        assert_eq!(report.clamp_cycle, Ok(()));
        assert_eq!(report.gateway_zfs, Ok(()));
    }

    /// klartext never clears blind: an ECU whose pre-read failed is not cleared, and
    /// it is not reported clean either — it is reported unknown, with the error.
    #[tokio::test]
    async fn clear_sequence_never_clears_an_ecu_it_could_not_pre_read() {
        // 0x18 is fitted, present and WILLING: it answers `10 03` and would answer
        // `14 FF FF FF`, but its fault read stays silent. That is the only shape that
        // can prove the rule — an ECU that is merely absent fails its extended-session
        // request first, so the clear is never reached and the guard is never tested.
        let (addr, log) =
            spawn_car_with_unreadable(&[0x10, 0x12, 0x18], &[0x12], &[], &[0x18]).await;
        let client = fast_client(addr).await;

        let report = client
            .clear_faults_all_holding(
                &[0x10, 0x12, 0x18],
                &VehicleComposition::default(),
                None,
                TEST_CLAMP_HOLD,
            )
            .await;

        // 0x12 was faulted and silent on the broadcast, so it IS cleared physically —
        // which proves the transport and the straggler pass both work here, so 0x18's
        // absence from the list below is the guard's doing and not a dead mock.
        let cleared_targets: Vec<u8> = log
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, p)| p.first() == Some(&0x14))
            .map(|(t, _)| *t)
            .collect();
        assert!(
            cleared_targets.contains(&0x12),
            "0x12 must have been cleared, else this proves nothing: {cleared_targets:02X?}"
        );
        assert!(
            !cleared_targets.contains(&0x18),
            "an ECU whose fault memory could not be read must never be cleared, got \
             {cleared_targets:02X?}"
        );

        let verdicts = report.ecu_verdicts();
        let unreadable = verdicts.iter().find(|v| v.address == 0x18).unwrap();
        assert!(!unreadable.verified_clean, "unreadable is not clean");
        assert!(unreadable.error.is_some());
    }

    /// `verified_clean` is klartext's addition (ISTA never diffs before against
    /// after). It must need evidence on BOTH sides: an ECU that stored a fault and
    /// reads empty afterwards is clean; one that still holds its fault is not.
    #[tokio::test]
    async fn ecu_verdicts_diff_before_against_after_per_ecu() {
        let (addr, _log) = spawn_car(&[0x10, 0x12, 0x40], &[0x12, 0x40], &[0x12]).await;
        let client = fast_client(addr).await;

        let report = client
            .clear_faults_all_holding(
                &[0x10, 0x12, 0x40],
                &VehicleComposition::default(),
                None,
                TEST_CLAMP_HOLD,
            )
            .await;

        let verdicts = report.ecu_verdicts();
        let by = |a: u8| verdicts.iter().find(|v| v.address == a).unwrap();

        // 0x12 stored a fault, answered the broadcast, and reads clean afterwards.
        assert_eq!(by(0x12).before.len(), 1);
        assert!(by(0x12).answered_broadcast);
        assert!(!by(0x12).cleared_physically);
        assert!(by(0x12).verified_clean);

        // 0x40 stored a fault, stayed silent, and was cleared by the straggler pass.
        assert_eq!(by(0x40).before.len(), 1);
        assert!(!by(0x40).answered_broadcast);
        assert!(by(0x40).cleared_physically);
        assert!(by(0x40).verified_clean);

        // The gateway had nothing stored and still reads clean.
        assert!(by(0x10).before.is_empty());
        assert!(by(0x10).verified_clean);
    }

    /// ISTA's clear phase never aborts: a failing step is logged and the sequence
    /// continues. Here the gateway refuses the ZFS routine — the clamp cycle, the
    /// re-identification and the verification read must all still happen.
    #[tokio::test]
    async fn clear_sequence_continues_past_a_failed_gateway_store() {
        // The gateway is absent from `fitted`, so its ZFS routine goes unanswered
        // while every other ECU behaves. The gateway SVT read fails with it, which
        // also exercises the re-identification fallback.
        let (addr, log) = spawn_car(&[0x12, 0x40], &[0x12], &[]).await;
        let client = fast_client(addr).await;

        let report = client
            .clear_faults_all_holding(
                &[0x12, 0x40],
                &VehicleComposition::default(),
                None,
                TEST_CLAMP_HOLD,
            )
            .await;

        assert!(
            report.gateway_zfs.is_err(),
            "the ZFS clear must have failed"
        );
        assert_eq!(
            report.clamp_cycle,
            Ok(()),
            "a failed gateway store must not abort the sequence"
        );
        assert!(report.reident.is_err());
        // The verification read falls back to the addresses it was given, so a failed
        // re-identification cannot silently shrink what gets verified.
        assert_eq!(report.verification.len(), 2);
        let clamped = log
            .lock()
            .unwrap()
            .iter()
            .any(|(t, p)| *t == clamp::TARGET && p.as_slice() == clamp::KL15_ON);
        assert!(clamped, "terminal 15 must still be restored");
    }

    /// A clamp cycle that leaves terminal 15 DOWN must surface `restored == false` —
    /// the one outcome that needs different words to the human (the car may not
    /// start). It must not be flattened into a generic error, and it must not be
    /// confused with the safe failure where the OFF never got through.
    ///
    /// Both arms are asserted together on purpose: a flag that is hardcoded either
    /// way passes one of them, so only the pair pins the distinction.
    #[tokio::test]
    async fn a_failed_clamp_restore_reports_that_terminal_15_may_be_down() {
        // Arm 1 — the SAFE failure. A car with no clamp ECU: the OFF itself fails, so
        // terminal 15 was never dropped and the report must say it is up.
        let (addr, _log) = spawn_car(&[0x10, 0x12], &[], &[0x12]).await;
        let safe = fast_client(addr)
            .await
            .clear_faults_all_holding(
                &[0x10, 0x12],
                &VehicleComposition::default(),
                None,
                TEST_CLAMP_HOLD,
            )
            .await
            .clamp_cycle
            .expect_err("the OFF command must fail");
        assert!(
            safe.restored,
            "a failure before the drop must report terminal 15 as up: {safe:?}"
        );
        assert!(
            !safe.message.contains("MAY STILL BE DOWN"),
            "and must NOT carry the restore warning: {}",
            safe.message
        );

        // Arm 2 — the DANGEROUS failure. The clamp ECU takes the OFF and never
        // answers the ON, so terminal 15 is down and stays down.
        let (addr, log) =
            spawn_car_where_clamp_cannot_restore(&[0x10, 0x12, 0x40], &[], &[0x12]).await;
        let stranded = fast_client(addr)
            .await
            .clear_faults_all_holding(
                &[0x10, 0x12, 0x40],
                &VehicleComposition::default(),
                None,
                TEST_CLAMP_HOLD,
            )
            .await
            .clamp_cycle
            .expect_err("the ON command must fail");
        assert!(
            !stranded.restored,
            "terminal 15 is DOWN and the report must say so: {stranded:?}"
        );
        assert!(
            stranded.message.contains("MAY STILL BE DOWN"),
            "and the message must warn the human plainly: {}",
            stranded.message
        );
        // Guards the vacuous pass: the OFF really did go out and get accepted, so
        // this is a stranded car and not simply an unreachable ECU.
        let sent = log.lock().unwrap().clone();
        assert!(
            sent.iter()
                .any(|(t, p)| *t == clamp::TARGET && p.as_slice() == clamp::KL15_OFF),
            "the OFF must have been transmitted: {sent:02X?}"
        );
    }

    #[tokio::test]
    async fn clear_faults_all_continues_past_a_failed_ecu_and_never_resets() {
        // 0x18 is in the fitted list but absent from the car, so every request to it
        // runs to its read timeout. The sequence must complete anyway and must still
        // clear 0x40. A short `read_timeout` keeps the silent ECU bounded.
        //
        // The mock serves `11 01` with a POSITIVE response. That is the point: if a
        // regression reintroduced the post-clear ECU reset (parity audit P0.1 — ISTA
        // sends no `0x11` anywhere in its clear flow, and the cluster reset it appears
        // to do is the terminal-15 cycle instead), it would SUCCEED here rather than
        // time out, so this mock cannot hide one.
        let (addr, frames) = spawn_car(&[0x10, 0x40], &[0x40], &[]).await;
        let c = fast_client(addr).await;

        let report = c
            .clear_faults_all_holding(
                &[0x10, 0x40, 0x18],
                &VehicleComposition::default(),
                None,
                TEST_CLAMP_HOLD,
            )
            .await;

        let verdicts = report.ecu_verdicts();
        let by = |a: u8| verdicts.iter().find(|v| v.address == a).unwrap();
        assert!(by(0x18).error.is_some(), "0x18 must have failed");
        assert!(!by(0x18).verified_clean);
        assert!(
            by(0x40).verified_clean,
            "the sequence must continue past 0x18's failure"
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

    // ── the supplier gates (§C.2) ────────────────────────────────────────────────

    /// Build a composition from variant names alone — the common gate shape.
    fn with_sgbds(names: &[&str]) -> VehicleComposition {
        VehicleComposition {
            sgbds: names.iter().map(|s| s.to_string()).collect(),
            ..VehicleComposition::default()
        }
    }

    fn planned(vehicle: &VehicleComposition) -> Vec<(&'static str, &'static str, &'static str)> {
        supplier_clear_jobs(vehicle)
            .into_iter()
            .map(|r| (r.job.ecu, r.job.job, r.job.arg))
            .collect()
    }

    /// A car with none of the six supplier ECUs runs none of the jobs. This is the
    /// F25 X3's case and it must degrade to nothing, not to a default.
    #[test]
    fn supplier_gates_fire_nothing_on_a_car_without_those_ecus() {
        assert!(planned(&with_sgbds(&["d72n47a0", "cas4_2", "zgw_01"])).is_empty());
        assert!(planned(&VehicleComposition::default()).is_empty());
    }

    /// FEM_20 gets the SAME job twice with different arguments — the argument is what
    /// selects the store, so collapsing the two calls into one would silently skip a
    /// store. `apiJob(variant, job, param, …)` passes "0x01"/"0x02" as the PARAMETER,
    /// not as a result filter (`VehicleIdent.cs:9733-9739`).
    #[test]
    fn fem_20_gate_fires_the_same_job_with_both_arguments() {
        assert_eq!(
            planned(&with_sgbds(&["FEM_20"])),
            vec![
                ("FEM_20", "IS_LOESCHEN_TMS", "0x01"),
                ("FEM_20", "IS_LOESCHEN_TMS", "0x02"),
            ]
        );
    }

    /// Trap 1: `D_KBM` is gated on FRM_70/FRM_87 presence AND SALAPA 524 — never on
    /// `D_KBM` itself. Both halves must be load-bearing.
    #[test]
    fn d_kbm_gate_needs_an_frm_and_salapa_524_not_d_kbm_itself() {
        let with_sa = |sgbds: &[&str], sa: Option<&[&str]>| VehicleComposition {
            sgbds: sgbds.iter().map(|s| s.to_string()).collect(),
            sa_codes: sa.map(|c| c.iter().map(|s| s.to_string()).collect()),
            ..VehicleComposition::default()
        };
        let fires = vec![
            ("D_KBM", "IS_LOESCHEN_SMC_L_LEAR", ""),
            ("D_KBM", "IS_LOESCHEN_SMC_R_LEAR", ""),
        ];

        // Both halves present: fires. Either FRM variant satisfies the first half.
        assert_eq!(planned(&with_sa(&["FRM_70"], Some(&["524"]))), fires);
        assert_eq!(planned(&with_sa(&["FRM_87"], Some(&["524", "6NS"]))), fires);

        // The FRM without the option code: nothing.
        assert!(planned(&with_sa(&["FRM_70"], Some(&["6NS"]))).is_empty());
        // The option code without an FRM: nothing.
        assert!(
            planned(&with_sa(&["ALC_60"], Some(&["524"])))
                .iter()
                .all(|j| j.0 != "D_KBM")
        );
        // D_KBM itself is NOT the gate — naming it must fire nothing.
        assert!(planned(&with_sa(&["D_KBM"], Some(&["524"]))).is_empty());
        // An undecoded SALAPA list is "unknown", not "present": klartext cannot read
        // the FA option vector yet, and guessing yes would fire a supplier write on a
        // car ISTA would have skipped.
        assert!(planned(&with_sa(&["FRM_70"], None)).is_empty());
    }

    /// Trap 2: `D_LM` is gated on LM_AHL/LM_AHL_2, never on `D_LM` itself.
    #[test]
    fn d_lm_gate_needs_an_ahl_lamp_module_not_d_lm_itself() {
        let fires = vec![
            ("D_LM", "IS_LOESCHEN_SMC_L_LEAR", ""),
            ("D_LM", "IS_LOESCHEN_SMC_R_LEAR", ""),
        ];
        assert_eq!(planned(&with_sgbds(&["LM_AHL"])), fires);
        assert_eq!(planned(&with_sgbds(&["LM_AHL_2"])), fires);
        assert!(planned(&with_sgbds(&["D_LM"])).is_empty());
    }

    /// Trap 3: `D_0066` is looked up by ECU_GRUPPE, the others by ECU_SGBD. Getting
    /// the field wrong silently skips (or wrongly fires) the store.
    #[test]
    fn d_0066_is_a_group_lookup_and_alc_60_is_a_variant_lookup() {
        let group = |names: &[&str]| VehicleComposition {
            groups: names.iter().map(|s| s.to_string()).collect(),
            ..VehicleComposition::default()
        };
        let fires = vec![
            ("D_0066", "IS_LOESCHEN_SMC_L", ""),
            ("D_0066", "IS_LOESCHEN_SMC_R", ""),
        ];
        assert_eq!(planned(&group(&["D_0066"])), fires);
        // A `|`-separated ECU_GRUPPE cell still matches (VehicleIdent.cs:680).
        assert_eq!(planned(&group(&["D_0044|D_0066"])), fires);
        // ...but the same name as a VARIANT must not fire the group gate.
        assert!(planned(&with_sgbds(&["D_0066"])).is_empty());

        // And the mirror: ALC_60 is a variant gate, so a group of that name is inert.
        assert_eq!(
            planned(&with_sgbds(&["ALC_60"])),
            vec![
                ("ALC_60", "IS_LOESCHEN_SMC_L_LEAR", ""),
                ("ALC_60", "IS_LOESCHEN_SMC_R_LEAR", ""),
            ]
        );
        assert!(planned(&group(&["ALC_60"])).is_empty());
    }

    /// ISTA's order is part of the behaviour, and every planned job must say it was
    /// not transmitted — klartext has no path to run these as writes.
    #[test]
    fn supplier_jobs_keep_istas_order_and_report_that_none_were_transmitted() {
        let vehicle = VehicleComposition {
            sgbds: ["FEM_20", "FRM3", "ALC_60", "LM_AHL"]
                .iter()
                .map(|s| s.to_string())
                .collect(),
            groups: vec!["D_0066".to_string()],
            sa_codes: None,
        };
        let reports = supplier_clear_jobs(&vehicle);
        assert_eq!(
            reports.iter().map(|r| r.job.ecu).collect::<Vec<_>>(),
            // VehicleIdent.cs steps 1.3, 1.4, 1.7, 1.8, 1.9 — D_KBM (1.6) is absent
            // because its SALAPA half cannot be evaluated.
            vec![
                "FEM_20", "FEM_20", "FRM3", "FRM3", "D_0066", "D_0066", "ALC_60", "ALC_60", "D_LM",
                "D_LM",
            ]
        );
        assert!(
            reports.iter().all(|r| r.not_run.is_some()),
            "no supplier job is executable yet — every one must say so"
        );
    }

    /// `IS_LOESCHEN_FUNKTIONAL` and a general per-ECU `IS_LOESCHEN` are DEAD CODE in
    /// ISTA (the guard at `VehicleIdent.cs:9747` is provably always false), so they
    /// must never appear in the plan. ISTA broadly READS info memory but only ever
    /// clears the six hardcoded supplier stores.
    #[test]
    fn no_general_info_memory_clear_is_ever_planned() {
        let everything = VehicleComposition {
            sgbds: [
                "FEM_20", "FRM3", "FRM_70", "FRM_87", "ALC_60", "LM_AHL", "LM_AHL_2",
            ]
            .iter()
            .map(|s| s.to_string())
            .collect(),
            groups: ["D_0066", "G_ZGW"].iter().map(|s| s.to_string()).collect(),
            sa_codes: Some(vec!["524".to_string()]),
        };
        for report in supplier_clear_jobs(&everything) {
            assert_ne!(report.job.job, "IS_LOESCHEN_FUNKTIONAL");
            assert_ne!(report.job.job, "IS_LOESCHEN");
        }
    }
}
