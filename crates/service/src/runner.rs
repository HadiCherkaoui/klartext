//! Run a service function's phase cycle, tearing down even when it fails.

use crate::phase::{Invocation, Phase};
use crate::precondition::{MeasurementReader, PreconditionOutcome, blocks, defaults_for, evaluate};
use klartext_semantic::{Category, FixedFunction};
use std::time::Duration;

/// Runs one named EDIABAS job with an argument buffer.
///
/// Abstracted so the cycle is testable without a VM or a car: the production impl
/// wraps [`klartext_best::Ecu::run_job`], and tests substitute a spy. The error is
/// a `String` because a caller only reports it — the concrete `RunError` stays in
/// the binary that owns the VM.
#[async_trait::async_trait]
pub trait JobRunner {
    /// Run `job` on `target` with the `;`-joined `args` buffer.
    async fn run(&self, job: &str, target: u8, args: &str) -> Result<(), String>;
}

/// What happened to the return-to-safe step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Teardown {
    /// The function defines no Reset phase — nothing to tear down.
    NotDefined,
    /// The Reset phase ran successfully.
    Ran,
    /// The Reset phase is DEFERRED — a held ([`Hold::UntilStop`]) actuation is
    /// still forced, awaiting an explicit [`stop_service`] (owner ruling 2). The
    /// component is meant to stay energised; teardown is postponed, not skipped.
    Deferred,
    /// The Reset phase FAILED — the ECU may still be actuating.
    Failed(String),
}

/// The post-Main wait ISTA performs before tearing a function down.
///
/// Derived from the owning function's `XEP_ECUFIXEDFUNCTIONS.ACTIVATION` /
/// `ACTIVATION_DURATION_MS` by [`hold_for`] (research Q2). A [`Hold::UntilStop`]
/// actuation is held past the call: [`run_cycle`] defers its teardown and
/// [`stop_service`] performs it later (owner ruling 2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold {
    /// No wait: teardown runs immediately after Main (a function with no Reset).
    None,
    /// Hold for a bounded duration, then tear down within the one call.
    Timed(Duration),
    /// Hold until an explicit stop (ISTA's `WaitOne(-1)`); teardown is deferred.
    UntilStop,
}

/// Derive the post-Main hold from a function's activation parameters.
///
/// Mirrors ISTA's `DoTriggerComponent` (research Q2, lines 12137-12146):
/// `num3 = Activation > 0 ? ActivationDurationMs : -1`, then a function with no
/// Reset job forces `num3 = 0`. So a reset-bearing function with `Activation > 0`
/// holds for its duration ([`Hold::Timed`]); one with `Activation == 0` holds
/// until stopped ([`Hold::UntilStop`]); and a function with no Reset never holds
/// ([`Hold::None`]). A reset-bearing function with no catalog row — activation
/// unknown — is treated as hold-until-stop: a force we cannot time must be
/// stopped explicitly rather than flicked off the same instant it is applied.
pub fn hold_for(ff: Option<&FixedFunction>, has_reset: bool) -> Hold {
    if !has_reset {
        return Hold::None;
    }
    match ff.and_then(|f| f.activation) {
        Some(a) if a > 0 => Hold::Timed(Duration::from_millis(
            ff.and_then(|f| f.activation_duration_ms)
                .unwrap_or(0)
                .max(0) as u64,
        )),
        _ => Hold::UntilStop,
    }
}

/// One executed phase-run's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseOutcome {
    /// Which phase this run belonged to.
    pub phase: Phase,
    /// The EDIABAS job this run sent — a phase may run several distinct jobs.
    pub job: String,
    /// This run's rank within its phase; `None` (unranked) ran first.
    pub rank: Option<i64>,
    /// The argument buffer sent.
    pub args: String,
    /// The failure, when this run failed.
    pub error: Option<String>,
}

/// The record of one service-function execution.
// `PreconditionOutcome` carries a measured `f64`, so this can only be `PartialEq`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceReport {
    /// The ISTA function title, when known.
    pub title: Option<String>,
    /// Each phase-run that was attempted, in execution order.
    pub phases: Vec<PhaseOutcome>,
    /// What happened to the return-to-safe step.
    pub teardown: Teardown,
    /// True only when a job ran, none failed, AND teardown did not fail.
    pub succeeded: bool,
    /// True when this is a held ([`Hold::UntilStop`]) actuation whose Main
    /// succeeded and whose teardown was DEFERRED: the component is still forced,
    /// so an explicit [`stop_service`] — or a disconnect — is owed before it is
    /// returned to safe (owner ruling 2). False on every other path, including a
    /// FAILED hold, which ruling 1 tears down at once.
    pub held: bool,
    /// Each precondition's outcome, checked before anything was sent.
    pub preconditions: Vec<PreconditionOutcome>,
    /// True when a RESOLVED precondition failed and the cycle was refused.
    pub blocked: bool,
}

/// Run function `function_id`'s phases as `Preset → Main`, hold, then `Reset`.
///
/// A phase is an ordered LIST of job-runs, not one job: ISTA's `GetJobsByPhase`
/// is plural and each phase loop is `OrderBy(Rank)` with `NULL` first (research
/// Q1), so a phase that runs `DIAGNOSE_MODE` then `STEUERN_IO`, or the same job
/// at two ranks, is several invocations here — each with its OWN job name and
/// `;`-joined buffer. [`crate::invocations`] hands them over already rank-ordered.
///
/// `invocations` may describe SEVERAL functions — one EDIABAS job name commonly
/// carries many (see [`crate::function_ids`]) — so only the invocations belonging
/// to `function_id` run. A `function_id` this slice does not carry matches
/// nothing, in which case NOTHING is sent and the report says so rather than
/// claiming success.
///
/// The safety contract:
/// - Only the REQUESTED function's invocations run: on variant `MRBMSC`,
///   `IO_STATUS_VORGEBEN` drives the fan, the fuel pump and the injectors alike,
///   so picking by phase alone would actuate an arbitrary component.
/// - Phases run in lifecycle order, and within a phase in rank order.
/// - ISTA aborts a phase on its FIRST failing job and skips every later phase
///   (research Q1); a failed `Preset` therefore never lets `Main` actuate.
/// - After a successful `Main`, a [`Hold::Timed`] actuation is HELD for its
///   duration before teardown (research Q2); a [`Hold::UntilStop`] actuation is
///   held INDEFINITELY — its teardown is deferred to [`stop_service`] and the
///   report is flagged `held` (owner ruling 2).
/// - `Reset` runs on EVERY path that attempted anything — success OR failure — so
///   an actuation is never left running (a deliberate, owner-approved DIVERGENCE
///   from ISTA, owner ruling 1). The ONE exception is a SUCCESSFUL
///   [`Hold::UntilStop`] actuation, whose teardown is deferred to [`stop_service`]
///   by design (owner ruling 2), not skipped — a FAILED hold still tears down here.
/// - A failed teardown is reported in [`Teardown::Failed`] and forces
///   `succeeded = false`: an actuation that could not be stopped must never look
///   like a success.
pub(crate) async fn run_cycle(
    runner: &(dyn JobRunner + Sync),
    function_id: i64,
    target: u8,
    invocations: &[Invocation],
    hold: Hold,
) -> ServiceReport {
    let select = |phase: Phase| {
        invocations
            .iter()
            .filter(move |i| i.function_id == function_id && i.phase == phase)
    };
    let title = chosen_title(function_id, invocations);
    let mut phases: Vec<PhaseOutcome> = Vec::new();
    let mut failed = false;

    // Preset then Main, each an ordered list of job-runs in rank order. On the
    // FIRST failing job we `break 'phases`: that aborts the rest of the current
    // phase AND skips every later phase in one step, so a failed Preset never lets
    // Main actuate — reproducing ISTA's throw-and-unwind (research Q1).
    'phases: for phase in [Phase::Preset, Phase::Main] {
        for inv in select(phase) {
            let args = inv.arg_buffer();
            let error = runner.run(&inv.job, target, &args).await.err();
            let this_failed = error.is_some();
            phases.push(PhaseOutcome {
                phase,
                job: inv.job.clone(),
                rank: inv.rank,
                args,
                error,
            });
            if this_failed {
                failed = true;
                break 'phases;
            }
        }
    }

    // A HELD actuation: a successful `Main` under `Hold::UntilStop`. Its teardown
    // is deferred (owner ruling 2) rather than run now, and the report flags that
    // a stop is owed. `!phases.is_empty()` excludes the nothing-ran cases (empty
    // slice / unmatched function): there is no force to hold. A FAILED cycle is
    // never held — ruling 1 below tears it down at once.
    let held = !failed && hold == Hold::UntilStop && !phases.is_empty();

    // Post-Main hold: ISTA parks on the termination event for the activation
    // window before tearing down (research Q2). Only `Hold::Timed` waits in-call;
    // `Hold::UntilStop` waits for the explicit stop, so it does not sleep here.
    // Never hold after a failure.
    if !failed && let Hold::Timed(duration) = hold {
        tokio::time::sleep(duration).await;
    }

    // TEARDOWN — owner ruling 1, a DELIBERATE divergence from ISTA. ISTA skips
    // teardown when a phase throws (research Q3); that is only safe because its
    // session then lapses and the UDS S3 timeout reverts the I/O force. klartext
    // keeps the session alive with TesterPresent, so a skipped teardown would
    // leave a latching `2F…03` force ENERGISED until disconnect. We therefore
    // ALWAYS run the Reset invocations on the failure path too, so a failed
    // actuation is never left forced. The ONE exception is a held, successful
    // actuation (owner ruling 2): its teardown is deliberately DEFERRED to
    // `stop_service` — the component is meant to stay forced — not skipped.
    let teardown = if held {
        Teardown::Deferred
    } else {
        let resets: Vec<&Invocation> = select(Phase::Reset).collect();
        run_teardown(runner, target, &resets, &mut phases).await
    };

    ServiceReport {
        title,
        // An empty phase list means nothing ever left the tester: an unknown
        // (variant, function) resolves to no invocations, and a `function_id`
        // this slice does not carry matches nothing. Calling that a success would
        // tell the operator a service function completed when no frame was sent.
        succeeded: !phases.is_empty() && !failed && !matches!(teardown, Teardown::Failed(_)),
        held,
        phases,
        teardown,
        preconditions: Vec::new(),
        blocked: false,
    }
}

/// Run a function's `Reset` invocations best-effort, recording each on `phases`.
///
/// The shared return-to-safe step: [`run_cycle`]'s always-run teardown (owner
/// ruling 1) and the standalone [`stop_service`] stop path (owner ruling 2) both
/// call it, so a stop tears down EXACTLY as the runner does. Every invocation is
/// attempted even after one fails — each may return a DIFFERENT component to safe
/// — and the FIRST failure is the one reported. An empty `resets` slice means the
/// function defines no teardown, reported as [`Teardown::NotDefined`].
async fn run_teardown(
    runner: &(dyn JobRunner + Sync),
    target: u8,
    resets: &[&Invocation],
    phases: &mut Vec<PhaseOutcome>,
) -> Teardown {
    let mut teardown = if resets.is_empty() {
        Teardown::NotDefined
    } else {
        Teardown::Ran
    };
    for inv in resets {
        let args = inv.arg_buffer();
        let error = runner.run(&inv.job, target, &args).await.err();
        // Keep the FIRST teardown failure while still attempting the rest.
        if let Some(e) = &error
            && matches!(teardown, Teardown::Ran)
        {
            teardown = Teardown::Failed(e.clone());
        }
        phases.push(PhaseOutcome {
            phase: Phase::Reset,
            job: inv.job.clone(),
            rank: inv.rank,
            args,
            error,
        });
    }
    teardown
}

/// The chosen function's title, ignoring every other function's.
///
/// Scoped to `function_id` because one job's invocations carry several functions'
/// titles; taking the first non-`None` across the whole slice would label a fuel-pump
/// actuation "Fan".
fn chosen_title(function_id: i64, invocations: &[Invocation]) -> Option<String> {
    invocations
        .iter()
        .filter(|i| i.function_id == function_id)
        .find_map(|i| i.title.clone())
}

/// Check `category`'s preconditions, then run function `function_id`'s cycle if
/// they allow it.
///
/// This is the ONLY way to execute a service function: the unguarded cycle is
/// crate-private, so a caller cannot reach an actuation without its preconditions.
///
/// A RESOLVED precondition failure refuses the whole cycle and NOTHING is sent —
/// not even `Preset`. An unresolvable check is advisory: it is reported and the
/// cycle proceeds (spec §5 — the human already confirmed; klartext must not refuse
/// because a lookup failed).
///
/// `invocations` may carry several functions; only `function_id`'s invocations
/// run. `hold` is the post-Main wait for this function (see [`hold_for`]). Use
/// [`crate::function_ids`] to enumerate what a job offers.
///
/// Both trait objects are `Sync` so this future is `Send` and an MCP tool — whose
/// futures rmcp boxes as `Send` — can await it. Every realistic implementor is
/// already `Sync`, so the bound costs callers nothing (the same reasoning as
/// `klartext_best`'s exchange).
pub async fn run_service(
    runner: &(dyn JobRunner + Sync),
    reader: &(dyn MeasurementReader + Sync),
    target: u8,
    function_id: i64,
    category: Category,
    invocations: &[Invocation],
    hold: Hold,
) -> ServiceReport {
    let preconditions = evaluate(reader, &defaults_for(category)).await;
    if blocks(&preconditions) {
        return ServiceReport {
            title: chosen_title(function_id, invocations),
            phases: Vec::new(),
            teardown: Teardown::NotDefined,
            succeeded: false,
            held: false,
            preconditions,
            blocked: true,
        };
    }
    let mut report = run_cycle(runner, function_id, target, invocations, hold).await;
    report.preconditions = preconditions;
    report
}

/// Run ONLY function `function_id`'s `Reset` invocations — the deferred teardown.
///
/// The stop half of the start/stop split for a held actuation (owner ruling 2):
/// [`run_service`] with a [`Hold::UntilStop`] function actuates and DEFERS its
/// teardown, reporting `held = true`; `stop_service` performs that teardown and
/// nothing else. It runs neither `Preset` nor `Main` — a stop must never
/// re-actuate — and is the path a caller invokes on an explicit stop AND on
/// disconnect, so a held component is always returned to safe.
///
/// Teardown is best-effort, EXACTLY as [`run_service`]'s is — both call the same
/// step: every `Reset` invocation is attempted even if one fails, the first
/// failure surfaces as [`Teardown::Failed`], and a function with no `Reset`
/// invocations reports [`Teardown::NotDefined`]. `invocations` may carry several
/// functions; only `function_id`'s `Reset` invocations run. The report is never
/// `held` — a stop clears the hold.
///
/// The runner is `Sync` for the same reason as [`run_service`]: the returned
/// future must be `Send` for the MCP boundary.
pub async fn stop_service(
    runner: &(dyn JobRunner + Sync),
    function_id: i64,
    target: u8,
    invocations: &[Invocation],
) -> ServiceReport {
    let resets: Vec<&Invocation> = invocations
        .iter()
        .filter(|i| i.function_id == function_id && i.phase == Phase::Reset)
        .collect();
    let mut phases: Vec<PhaseOutcome> = Vec::new();
    let teardown = run_teardown(runner, target, &resets, &mut phases).await;
    ServiceReport {
        title: chosen_title(function_id, invocations),
        // A stop that could not tear the component down is not a success, and an
        // empty teardown (no Reset defined) never sent a frame, so it is not one
        // either — mirroring `run_cycle`'s empty-phase rule.
        succeeded: !phases.is_empty() && !matches!(teardown, Teardown::Failed(_)),
        held: false,
        phases,
        teardown,
        preconditions: Vec::new(),
        blocked: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precondition::{Quantity, Verdict};
    use std::sync::Mutex;

    /// Records every job name run, and fails the named one.
    struct SpyEcu {
        ran: Mutex<Vec<String>>,
        fail_on: Option<&'static str>,
    }

    #[async_trait::async_trait]
    impl JobRunner for SpyEcu {
        async fn run(&self, job: &str, _target: u8, args: &str) -> Result<(), String> {
            self.ran.lock().unwrap().push(format!("{job}({args})"));
            match self.fail_on {
                Some(f) if args.contains(f) => Err("boom".to_string()),
                _ => Ok(()),
            }
        }
    }

    /// Records each job together with how long after start it ran, for asserting
    /// that the post-Main hold is actually awaited before teardown.
    struct TimedSpy {
        ran: Mutex<Vec<(String, Duration)>>,
        start: std::time::Instant,
    }

    #[async_trait::async_trait]
    impl JobRunner for TimedSpy {
        async fn run(&self, job: &str, _target: u8, args: &str) -> Result<(), String> {
            self.ran
                .lock()
                .unwrap()
                .push((format!("{job}({args})"), self.start.elapsed()));
            Ok(())
        }
    }

    /// The function the single-function tests below ask for.
    const FN: i64 = 1;

    /// A single-function invocation on the generic `STEUERN_X` job (unranked).
    fn inv(phase: Phase, arg: &str) -> Invocation {
        inv_named(FN, "EXAMPLE", "STEUERN_X", phase, arg)
    }

    /// A titled invocation on `IO_STATUS_VORGEBEN` — the real multi-function job
    /// the function-selection tests exercise (unranked).
    fn inv_for(function_id: i64, title: &str, phase: Phase, arg: &str) -> Invocation {
        inv_named(function_id, title, "IO_STATUS_VORGEBEN", phase, arg)
    }

    /// A titled invocation carrying an explicit job name (unranked).
    fn inv_named(function_id: i64, title: &str, job: &str, phase: Phase, arg: &str) -> Invocation {
        Invocation {
            function_id,
            title: Some(title.to_string()),
            phase,
            job: job.to_string(),
            rank: None,
            args: vec![arg.to_string()],
        }
    }

    /// A titleless invocation carrying an explicit job and rank — for the
    /// multi-job / multi-rank cycle tests.
    fn inv_job(
        function_id: i64,
        phase: Phase,
        rank: Option<i64>,
        job: &str,
        arg: &str,
    ) -> Invocation {
        Invocation {
            function_id,
            title: None,
            phase,
            job: job.to_string(),
            rank,
            args: vec![arg.to_string()],
        }
    }

    #[test]
    fn hold_for_covers_all_three_arms() {
        // Activation > 0 with a Reset → a bounded timed hold of the duration.
        let timed = FixedFunction {
            function_id: 1,
            activation: Some(1),
            activation_duration_ms: Some(5000),
            preparing_text: None,
            processing_text: None,
            post_text: None,
        };
        assert_eq!(
            hold_for(Some(&timed), true),
            Hold::Timed(Duration::from_millis(5000))
        );
        // Activation == 0 with a Reset → hold until an explicit stop. This is the
        // arm the `a > 0` → `a >= 0` mutation breaks: with `>=`, activation 0
        // would wrongly resolve to `Timed(0)` here.
        let held = FixedFunction {
            function_id: 2,
            activation: Some(0),
            activation_duration_ms: None,
            preparing_text: None,
            processing_text: None,
            post_text: None,
        };
        assert_eq!(hold_for(Some(&held), true), Hold::UntilStop);
        // No Reset job → never hold, regardless of activation.
        assert_eq!(hold_for(Some(&timed), false), Hold::None);
        assert_eq!(hold_for(None, false), Hold::None);
        // A reset-bearing function with no catalog row → the safe hold-until-stop
        // default, never a zero-length flick.
        assert_eq!(hold_for(None, true), Hold::UntilStop);
    }

    #[tokio::test]
    async fn runs_preset_then_main_then_reset_in_order() {
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let report = run_cycle(
            &spy,
            FN,
            0x12,
            &[
                inv(Phase::Main, "GO"),
                inv(Phase::Preset, "PRE"),
                inv(Phase::Reset, "OFF"),
            ],
            Hold::None,
        )
        .await;
        assert_eq!(
            *spy.ran.lock().unwrap(),
            vec!["STEUERN_X(PRE)", "STEUERN_X(GO)", "STEUERN_X(OFF)"]
        );
        assert!(report.succeeded);
        assert_eq!(report.teardown, Teardown::Ran);
    }

    #[tokio::test]
    async fn reset_still_runs_when_main_fails() {
        // The safety property: a failed actuation must still be torn down, never
        // left running. The report must say so and must NOT claim success.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: Some("GO"),
        };
        let report = run_cycle(
            &spy,
            FN,
            0x12,
            &[
                inv(Phase::Preset, "PRE"),
                inv(Phase::Main, "GO"),
                inv(Phase::Reset, "OFF"),
            ],
            Hold::None,
        )
        .await;
        assert_eq!(
            *spy.ran.lock().unwrap(),
            vec!["STEUERN_X(PRE)", "STEUERN_X(GO)", "STEUERN_X(OFF)"],
            "Reset must run after a failed Main"
        );
        assert!(!report.succeeded);
        assert_eq!(report.teardown, Teardown::Ran);
        assert!(
            report
                .phases
                .iter()
                .any(|p| p.phase == Phase::Main && p.error.is_some())
        );
    }

    #[tokio::test]
    async fn a_failed_preset_skips_main_but_still_tears_down() {
        // If preparation failed the ECU is in an unknown state: do NOT actuate,
        // but do run the return-to-safe step.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: Some("PRE"),
        };
        let report = run_cycle(
            &spy,
            FN,
            0x12,
            &[
                inv(Phase::Preset, "PRE"),
                inv(Phase::Main, "GO"),
                inv(Phase::Reset, "OFF"),
            ],
            Hold::None,
        )
        .await;
        let ran = spy.ran.lock().unwrap().clone();
        assert!(
            !ran.iter().any(|r| r.contains("GO")),
            "Main must not run: {ran:?}"
        );
        assert!(
            ran.iter().any(|r| r.contains("OFF")),
            "Reset must run: {ran:?}"
        );
        assert!(!report.succeeded);
        // Preset's own failure must be recorded on Preset's outcome. Without this,
        // an implementation that got every flag right but dropped the Preset error
        // (e.g. hardcoding `error: None` there) would pass every other assertion —
        // the operator would see a failed run with no phase explaining why.
        assert!(
            report
                .phases
                .iter()
                .any(|p| p.phase == Phase::Preset && p.error.is_some()),
            "the Preset outcome must carry its error: {:?}",
            report.phases
        );
    }

    #[tokio::test]
    async fn a_failed_job_aborts_the_phase_but_still_tears_down() {
        // ISTA semantics and the ruling-1 divergence, together: Main runs
        // STEUERN_IO at rank 1 (fails), so its rank-2 sibling must NOT run (ISTA
        // aborts the phase) — yet the Reset invocation MUST still run (klartext's
        // safe-teardown divergence). This is the load-bearing multi-job test.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: Some("IO1"),
        };
        let invs = vec![
            inv_job(5, Phase::Main, Some(1), "STEUERN_IO", "IO1"),
            inv_job(5, Phase::Main, Some(2), "STEUERN_IO", "IO2"),
            inv_job(5, Phase::Reset, Some(1), "STEUERN_IO_AUS", "OFF"),
        ];
        let report = run_cycle(&spy, 5, 0x12, &invs, Hold::None).await;
        let ran = spy.ran.lock().unwrap().clone();
        assert!(ran.iter().any(|r| r.contains("IO1")), "{ran:?}");
        assert!(
            !ran.iter().any(|r| r.contains("IO2")),
            "rank 2 must not run after rank 1 fails: {ran:?}"
        );
        assert!(
            ran.iter().any(|r| r.contains("OFF")),
            "safe teardown must run on failure: {ran:?}"
        );
        assert!(!report.succeeded);
    }

    #[tokio::test]
    async fn a_timed_hold_is_awaited_before_the_teardown() {
        // The hold must be a REAL wait, and it must fall BETWEEN Main and Reset. A
        // spy recording how long after start each job ran proves both: with a 25 ms
        // hold, Main runs immediately and Reset only once the hold elapses. A build
        // that tore down BEFORE the hold (or dropped the wait) would record Reset
        // at ~0 ms and fail the lower bound. `tokio::time::sleep` never fires
        // early, so the bound is not flaky — a loaded machine only pushes it later.
        const HOLD: Duration = Duration::from_millis(25);
        let spy = TimedSpy {
            ran: Mutex::new(Vec::new()),
            start: std::time::Instant::now(),
        };
        let invs = vec![
            inv_job(5, Phase::Main, Some(1), "STEUERN_IO", "ON"),
            inv_job(5, Phase::Reset, Some(1), "STEUERN_IO_AUS", "OFF"),
        ];
        let report = run_cycle(&spy, 5, 0x12, &invs, Hold::Timed(HOLD)).await;
        let log = spy.ran.lock().unwrap().clone();
        assert_eq!(log.len(), 2, "{log:?}");
        assert_eq!(log[0].0, "STEUERN_IO(ON)", "Main runs first");
        assert_eq!(log[1].0, "STEUERN_IO_AUS(OFF)", "teardown runs, and last");
        assert!(
            log[1].1 >= HOLD - Duration::from_millis(5),
            "teardown must run AFTER the hold elapsed, but Reset ran at {:?}",
            log[1].1
        );
        assert!(report.succeeded);
    }

    #[tokio::test]
    async fn an_until_stop_function_holds_and_defers_teardown() {
        // Owner ruling 2: an Activation==0 function actuates and HOLDS. Main
        // succeeds, so the Reset (teardown) is DEFERRED to `stop_service` — the
        // component stays forced — and the report says so with `held`. This tests
        // the DEFER, not a timer: `Hold::UntilStop` never sleeps here, so the
        // suite stays fast.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let invs = vec![
            inv_job(5, Phase::Main, Some(1), "STEUERN_IO", "ON"),
            inv_job(5, Phase::Reset, Some(1), "STEUERN_IO_AUS", "OFF"),
        ];
        let report = run_cycle(&spy, 5, 0x12, &invs, Hold::UntilStop).await;
        assert!(
            report.held,
            "an until-stop function must report it is holding"
        );
        assert!(report.succeeded, "the actuation itself succeeded");
        assert_eq!(
            report.teardown,
            Teardown::Deferred,
            "the teardown is deferred, not run and not skipped"
        );
        assert!(
            !spy.ran.lock().unwrap().iter().any(|r| r.contains("OFF")),
            "teardown must be DEFERRED, not run: {:?}",
            spy.ran.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn a_failed_until_stop_hold_tears_down_at_once_and_is_not_held() {
        // Ruling 1 still wins over ruling 2: if Main FAILS under Hold::UntilStop,
        // the actuation is NOT parked awaiting a stop — teardown runs immediately
        // (safe) and the report is not `held`. The `!failed` term of the `held`
        // computation is what enforces this; dropping it leaves a failed force
        // holding.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: Some("ON"),
        };
        let invs = vec![
            inv_job(5, Phase::Main, Some(1), "STEUERN_IO", "ON"),
            inv_job(5, Phase::Reset, Some(1), "STEUERN_IO_AUS", "OFF"),
        ];
        let report = run_cycle(&spy, 5, 0x12, &invs, Hold::UntilStop).await;
        assert!(!report.held, "a FAILED hold is torn down, never held");
        assert!(!report.succeeded);
        assert!(
            spy.ran.lock().unwrap().iter().any(|r| r.contains("OFF")),
            "ruling 1: teardown must run at once on a failed hold: {:?}",
            spy.ran.lock().unwrap()
        );
        assert_eq!(report.teardown, Teardown::Ran);
    }

    #[tokio::test]
    async fn stop_service_runs_only_the_teardown() {
        // The stop half of ruling 2: `stop_service` runs the function's Reset
        // invocations and NOTHING else — never Main — so a held component is
        // returned to safe without being re-actuated.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let invs = vec![
            inv_job(5, Phase::Main, Some(1), "STEUERN_IO", "ON"),
            inv_job(5, Phase::Reset, Some(1), "STEUERN_IO_AUS", "OFF"),
        ];
        let report = stop_service(&spy, 5, 0x12, &invs).await;
        assert_eq!(*spy.ran.lock().unwrap(), vec!["STEUERN_IO_AUS(OFF)"]);
        assert_eq!(report.teardown, Teardown::Ran);
        assert!(!report.held, "a stop clears the hold");
        assert!(report.succeeded);
    }

    #[tokio::test]
    async fn teardown_failure_is_reported_not_swallowed() {
        // An actuation that could not be stopped is the worst outcome; it must be
        // impossible to mistake for success.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: Some("OFF"),
        };
        let report = run_cycle(
            &spy,
            FN,
            0x12,
            &[inv(Phase::Main, "GO"), inv(Phase::Reset, "OFF")],
            Hold::None,
        )
        .await;
        assert!(matches!(report.teardown, Teardown::Failed(_)));
        assert!(!report.succeeded);
    }

    #[tokio::test]
    async fn a_function_with_no_reset_phase_reports_none_not_ran() {
        // Most read/reset functions define only Main. "No teardown defined" must be
        // distinguishable from "teardown ran".
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let report = run_cycle(&spy, FN, 0x12, &[inv(Phase::Main, "GO")], Hold::None).await;
        assert_eq!(report.teardown, Teardown::NotDefined);
        assert!(report.succeeded);
    }

    #[tokio::test]
    async fn only_the_requested_functions_phases_run() {
        // THE safety property of function selection. On variant MRBMSC the single
        // job IO_STATUS_VORGEBEN carries the fan, the oxygen-sensor heating, the
        // fuel pump, the injectors and the idle actuator — 1,719 of the catalog's
        // 2,792 (variant, job) pairs are multi-function. Picking by phase alone
        // would take whichever sorts first, so asking for the pump would spin the
        // FAN. The fan is deliberately first in the slice and lower-numbered here:
        // an implementation that ignores `function_id` runs FAN and fails this.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let report = run_cycle(
            &spy,
            2,
            0x12,
            &[
                inv_for(1, "Fan", Phase::Main, "FAN_ON"),
                inv_for(1, "Fan", Phase::Reset, "FAN_OFF"),
                inv_for(2, "Electric fuel pump", Phase::Main, "PUMP_ON"),
                inv_for(2, "Electric fuel pump", Phase::Reset, "PUMP_OFF"),
            ],
            Hold::None,
        )
        .await;
        assert_eq!(
            *spy.ran.lock().unwrap(),
            vec![
                "IO_STATUS_VORGEBEN(PUMP_ON)",
                "IO_STATUS_VORGEBEN(PUMP_OFF)"
            ],
            "only function 2's phases may reach the car"
        );
        // The title must name what actually ran, not the first title in the slice.
        assert_eq!(report.title.as_deref(), Some("Electric fuel pump"));
        assert!(report.succeeded);
    }

    #[tokio::test]
    async fn a_function_id_the_job_does_not_define_runs_nothing_and_fails() {
        // A resolution miss must NOT read as "the service function completed".
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let report = run_cycle(
            &spy,
            99,
            0x12,
            &[
                inv_for(1, "Fan", Phase::Main, "FAN_ON"),
                inv_for(2, "Electric fuel pump", Phase::Main, "PUMP_ON"),
            ],
            Hold::None,
        )
        .await;
        assert!(
            spy.ran.lock().unwrap().is_empty(),
            "nothing may be sent: {:?}",
            spy.ran.lock().unwrap()
        );
        assert!(!report.succeeded, "an unmatched function is not a success");
        assert!(report.phases.is_empty());
        assert_eq!(report.title, None, "no function ran, so none may be named");
    }

    #[tokio::test]
    async fn an_empty_invocation_list_is_not_a_success() {
        // `Catalog::job_parameters_for_function` returns an empty Vec for an
        // unknown (variant, function). With `succeeded` computed only from failure
        // flags, that miss surfaces to the human as a completed service function
        // while no frame ever left the tester.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let report = run_cycle(&spy, FN, 0x12, &[], Hold::None).await;
        assert!(!report.succeeded, "nothing ran, so nothing succeeded");
        assert!(spy.ran.lock().unwrap().is_empty());
        assert_eq!(report.teardown, Teardown::NotDefined);
    }

    struct TableReader(Vec<(Quantity, f64)>);

    #[async_trait::async_trait]
    impl crate::precondition::MeasurementReader for TableReader {
        async fn read(&self, quantity: Quantity) -> Result<f64, String> {
            self.0
                .iter()
                .find(|(q, _)| *q == quantity)
                .map(|(_, v)| *v)
                .ok_or_else(|| format!("no reading for {quantity:?}"))
        }
    }

    #[tokio::test]
    async fn a_violated_precondition_blocks_and_sends_nothing() {
        // The crux: NOTHING may reach the car when a precondition fails.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let reader = TableReader(vec![
            (Quantity::TerminalStatus, 0.0),
            (Quantity::BatteryVoltage, 12.6),
            (Quantity::RoadSpeed, 0.0),
        ]);
        let report = run_service(
            &spy,
            &reader,
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO"), inv(Phase::Reset, "OFF")],
            Hold::None,
        )
        .await;
        assert!(report.blocked);
        assert!(!report.succeeded);
        assert!(
            spy.ran.lock().unwrap().is_empty(),
            "no frame may be sent: {:?}",
            spy.ran.lock().unwrap()
        );
        assert_eq!(report.teardown, Teardown::NotDefined);
    }

    #[tokio::test]
    async fn satisfied_preconditions_let_the_cycle_run() {
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let reader = TableReader(vec![
            (Quantity::TerminalStatus, 1.0),
            (Quantity::BatteryVoltage, 12.6),
            (Quantity::RoadSpeed, 0.0),
        ]);
        let report = run_service(
            &spy,
            &reader,
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO"), inv(Phase::Reset, "OFF")],
            Hold::None,
        )
        .await;
        assert!(!report.blocked);
        assert!(report.succeeded);
        assert_eq!(
            *spy.ran.lock().unwrap(),
            vec!["STEUERN_X(GO)", "STEUERN_X(OFF)"]
        );
        // `.all()` is TRUE on an empty Vec, so without the length pin this whole
        // assertion survives dropping `report.preconditions` entirely — and that
        // field is what the surfaces render to tell "checked and fine" apart from
        // "could not check".
        assert_eq!(
            report.preconditions.len(),
            defaults_for(Category::ActuatorControl).len()
        );
        assert!(
            report
                .preconditions
                .iter()
                .all(|p| p.verdict == Verdict::Passed)
        );
    }

    #[tokio::test]
    async fn unverifiable_preconditions_do_not_block_but_are_reported() {
        // Spec §5: degrade to advisory, and SAY SO — the caller must be able to
        // tell "checked and fine" from "could not check".
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let reader = TableReader(Vec::new());
        let report = run_service(
            &spy,
            &reader,
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO")],
            Hold::None,
        )
        .await;
        assert!(!report.blocked);
        assert!(report.succeeded);
        assert!(
            !spy.ran.lock().unwrap().is_empty(),
            "the cycle must still run"
        );
        // Same vacuity guard: the outcomes must actually SURVIVE onto the report,
        // not merely be absent-and-therefore-trivially-all-unverified.
        assert_eq!(
            report.preconditions.len(),
            defaults_for(Category::ActuatorControl).len()
        );
        assert!(
            report
                .preconditions
                .iter()
                .all(|p| p.verdict == Verdict::Unverified),
            "{:?}",
            report.preconditions
        );
    }

    #[tokio::test]
    async fn a_violated_precondition_blocks_even_with_a_preset_phase_defined() {
        // `a_violated_precondition_blocks_and_sends_nothing` only supplies Main and
        // Reset, so it can't tell "refuse before anything runs" apart from "refuse
        // before Main" — a Preset invocation would sail through either way. This is
        // the doc comment's literal claim ("not even Preset"): give the cycle a
        // Preset step too and prove it never fires.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let reader = TableReader(vec![
            (Quantity::TerminalStatus, 0.0),
            (Quantity::BatteryVoltage, 12.6),
            (Quantity::RoadSpeed, 0.0),
        ]);
        let report = run_service(
            &spy,
            &reader,
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[
                inv(Phase::Preset, "PRE"),
                inv(Phase::Main, "GO"),
                inv(Phase::Reset, "OFF"),
            ],
            Hold::None,
        )
        .await;
        assert!(report.blocked);
        assert!(
            spy.ran.lock().unwrap().is_empty(),
            "not even Preset may run: {:?}",
            spy.ran.lock().unwrap()
        );
    }

    #[tokio::test]
    async fn run_service_checks_the_passed_categorys_defaults_not_a_fixed_set() {
        // A plausible wrong implementation hardcodes (or defaults to)
        // ActuatorControl's checks regardless of `category`. Feed values that
        // VIOLATE ActuatorControl's extra checks (battery, stationary) but SATISFY
        // CbsReset's only check (terminal on): only a build that actually looks up
        // `category`'s own defaults lets this cycle through.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let reader = TableReader(vec![
            (Quantity::TerminalStatus, 1.0),
            (Quantity::BatteryVoltage, 10.0),
            (Quantity::RoadSpeed, 50.0),
        ]);
        let report = run_service(
            &spy,
            &reader,
            0x12,
            FN,
            klartext_semantic::Category::CbsReset,
            &[inv(Phase::Main, "RESET")],
            Hold::None,
        )
        .await;
        assert!(
            !report.blocked,
            "CbsReset requires only TerminalOn: {:?}",
            report.preconditions
        );
        assert!(!spy.ran.lock().unwrap().is_empty(), "the cycle must run");
    }

    #[tokio::test]
    async fn run_service_enforces_the_actuator_categorys_stricter_checks() {
        // The test above proves a LOW-risk category is not over-gated. This proves
        // the reverse, which is the dangerous direction: that a HIGH-risk category's
        // extra checks are actually consulted, not silently replaced by a weaker
        // set. TerminalOn alone passes here, so a build that hardcoded any low-risk
        // category's defaults (CbsReset/LearnedValueReset/StatisticReset all reduce
        // to TerminalOn only) would let an actuation run on a 10 V battery.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let reader = TableReader(vec![
            (Quantity::TerminalStatus, 1.0),
            (Quantity::BatteryVoltage, 10.0),
            (Quantity::RoadSpeed, 0.0),
        ]);
        let report = run_service(
            &spy,
            &reader,
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO")],
            Hold::None,
        )
        .await;
        assert!(
            report.blocked,
            "ActuatorControl's BatteryAbove(12.0) must be checked, not dropped: {:?}",
            report.preconditions
        );
        assert!(spy.ran.lock().unwrap().is_empty(), "nothing may be sent");
    }

    #[tokio::test]
    async fn run_service_selects_the_function_too_not_just_run_cycle() {
        // `run_cycle` is crate-private, so the PUBLIC path is what a binary will
        // use. Proving selection on the inner function alone would not stop
        // `run_service` from passing the wrong id (or dropping the filter) on its
        // way through.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let reader = TableReader(vec![(Quantity::TerminalStatus, 1.0)]);
        let report = run_service(
            &spy,
            &reader,
            0x12,
            2,
            klartext_semantic::Category::CbsReset,
            &[
                inv_for(1, "Fan", Phase::Main, "FAN_ON"),
                inv_for(2, "Electric fuel pump", Phase::Main, "PUMP_ON"),
            ],
            Hold::None,
        )
        .await;
        assert_eq!(
            *spy.ran.lock().unwrap(),
            vec!["IO_STATUS_VORGEBEN(PUMP_ON)"]
        );
        assert_eq!(report.title.as_deref(), Some("Electric fuel pump"));
    }

    #[tokio::test]
    async fn a_blocked_report_names_the_chosen_function_not_the_first() {
        // The refusal path builds its own report, so it needs its own proof that
        // the title is scoped to the requested function: an operator told "Fan
        // refused" when they asked for the fuel pump learns the wrong thing about
        // their car.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let reader = TableReader(vec![(Quantity::TerminalStatus, 0.0)]);
        let report = run_service(
            &spy,
            &reader,
            0x12,
            2,
            klartext_semantic::Category::ActuatorControl,
            &[
                inv_for(1, "Fan", Phase::Main, "FAN_ON"),
                inv_for(2, "Electric fuel pump", Phase::Main, "PUMP_ON"),
            ],
            Hold::None,
        )
        .await;
        assert!(report.blocked);
        assert_eq!(report.title.as_deref(), Some("Electric fuel pump"));
    }

    #[tokio::test]
    async fn the_service_future_is_send_for_the_mcp_boundary() {
        // rmcp boxes MCP tool futures as `Send`, and `&dyn Trait` is `Send` only
        // when the trait is `Sync`. Nothing else in this crate would notice the
        // `+ Sync` bounds regressing — the break would surface only once a binary
        // is written against this seam, which is exactly the cost this pins down.
        // Constructing the future (never polling it) is enough to check the bound.
        fn assert_send<T: Send>(_: T) {}
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let reader = TableReader(Vec::new());
        assert_send(run_service(
            &spy,
            &reader,
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO")],
            Hold::None,
        ));
    }

    #[tokio::test]
    async fn a_blocked_report_still_names_the_function() {
        // The operator has to know WHICH function was refused. Untested, a build
        // hardcoding `title: None` on the blocked path would read as a nameless
        // refusal.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let reader = TableReader(vec![(Quantity::TerminalStatus, 0.0)]);
        let report = run_service(
            &spy,
            &reader,
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO")],
            Hold::None,
        )
        .await;
        assert!(report.blocked);
        assert_eq!(report.title.as_deref(), Some("EXAMPLE"));
    }
}
