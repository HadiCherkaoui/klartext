//! Run a service function's phase cycle, tearing down even when it fails.

use crate::phase::{Invocation, Phase};
use crate::precondition::{MeasurementReader, PreconditionOutcome, blocks, defaults_for, evaluate};
use klartext_semantic::Category;

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
    /// The Reset phase FAILED — the ECU may still be actuating.
    Failed(String),
}

/// One executed phase's outcome.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhaseOutcome {
    /// Which phase this was.
    pub phase: Phase,
    /// The argument buffer sent.
    pub args: String,
    /// The failure, when the phase failed.
    pub error: Option<String>,
}

/// The record of one service-function execution.
// `PreconditionOutcome` carries a measured `f64`, so this can only be `PartialEq`.
#[derive(Debug, Clone, PartialEq)]
pub struct ServiceReport {
    /// The EDIABAS job that was run.
    pub job: String,
    /// The ISTA function title, when known.
    pub title: Option<String>,
    /// Each phase that was attempted, in execution order.
    pub phases: Vec<PhaseOutcome>,
    /// What happened to the return-to-safe step.
    pub teardown: Teardown,
    /// True only when every attempted phase succeeded AND teardown did not fail.
    pub succeeded: bool,
    /// Each precondition's outcome, checked before anything was sent.
    pub preconditions: Vec<PreconditionOutcome>,
    /// True when a RESOLVED precondition failed and the cycle was refused.
    pub blocked: bool,
}

/// Run function `function_id`'s phases as `Preset → Main → Reset`.
///
/// `invocations` may describe SEVERAL functions — one EDIABAS job name commonly
/// carries many (see [`crate::function_ids`]) — so only the phases belonging to
/// `function_id` are run. Every other function's phases are ignored, including
/// when `function_id` matches nothing at all, in which case NOTHING is sent and
/// the report says so rather than claiming success.
///
/// The safety contract:
/// - Only the REQUESTED function's phases run: on variant `MRBMSC`,
///   `IO_STATUS_VORGEBEN` drives the fan, the fuel pump and the injectors alike,
///   so picking by job name alone would actuate an arbitrary component.
/// - Phases run in lifecycle order regardless of the order supplied.
/// - A failed `Preset` SKIPS `Main` — preparation failed, so the ECU's state is
///   unknown and actuating would be reckless.
/// - `Reset` runs on EVERY path that attempted anything, success or failure, so an
///   actuation is never left running.
/// - A failed teardown is reported in [`Teardown::Failed`] and forces
///   `succeeded = false`: an actuation that could not be stopped must never look
///   like a success.
pub(crate) async fn run_cycle(
    runner: &dyn JobRunner,
    job: &str,
    target: u8,
    function_id: i64,
    invocations: &[Invocation],
) -> ServiceReport {
    let pick = |phase: Phase| {
        invocations
            .iter()
            .find(|i| i.function_id == function_id && i.phase == phase)
    };
    let title = chosen_title(function_id, invocations);
    let mut phases: Vec<PhaseOutcome> = Vec::new();
    let mut failed = false;

    for phase in [Phase::Preset, Phase::Main] {
        let Some(inv) = pick(phase) else { continue };
        // A failed Preset leaves the ECU unprepared: do not actuate.
        if failed {
            break;
        }
        let args = inv.arg_buffer();
        let error = runner.run(job, target, &args).await.err();
        failed |= error.is_some();
        phases.push(PhaseOutcome { phase, args, error });
    }

    let teardown = match pick(Phase::Reset) {
        None => Teardown::NotDefined,
        Some(inv) => {
            let args = inv.arg_buffer();
            match runner.run(job, target, &args).await {
                Ok(()) => {
                    phases.push(PhaseOutcome {
                        phase: Phase::Reset,
                        args,
                        error: None,
                    });
                    Teardown::Ran
                }
                Err(e) => {
                    phases.push(PhaseOutcome {
                        phase: Phase::Reset,
                        args,
                        error: Some(e.clone()),
                    });
                    Teardown::Failed(e)
                }
            }
        }
    };

    ServiceReport {
        job: job.to_string(),
        title,
        // An empty phase list means nothing was ever sent: `Catalog::job_parameters`
        // returns no rows for an unknown (variant, job), and a `function_id` this
        // job does not define matches nothing. Calling that a success would tell
        // the operator a service function completed when no frame left the tester.
        succeeded: !phases.is_empty() && !failed && !matches!(teardown, Teardown::Failed(_)),
        phases,
        teardown,
        preconditions: Vec::new(),
        blocked: false,
    }
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
/// `invocations` may carry several functions; only `function_id`'s phases run. Use
/// [`crate::function_ids`] to enumerate what a job offers.
pub async fn run_service(
    runner: &dyn JobRunner,
    reader: &dyn MeasurementReader,
    job: &str,
    target: u8,
    function_id: i64,
    category: Category,
    invocations: &[Invocation],
) -> ServiceReport {
    let preconditions = evaluate(reader, &defaults_for(category)).await;
    if blocks(&preconditions) {
        return ServiceReport {
            job: job.to_string(),
            title: chosen_title(function_id, invocations),
            phases: Vec::new(),
            teardown: Teardown::NotDefined,
            succeeded: false,
            preconditions,
            blocked: true,
        };
    }
    let mut report = run_cycle(runner, job, target, function_id, invocations).await;
    report.preconditions = preconditions;
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::precondition::Verdict;
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

    /// The function the single-function tests below ask for.
    const FN: i64 = 1;

    fn inv(phase: Phase, arg: &str) -> Invocation {
        inv_for(FN, "EXAMPLE", phase, arg)
    }

    fn inv_for(function_id: i64, title: &str, phase: Phase, arg: &str) -> Invocation {
        Invocation {
            function_id,
            title: Some(title.to_string()),
            phase,
            args: vec![arg.to_string()],
        }
    }

    #[tokio::test]
    async fn runs_preset_then_main_then_reset_in_order() {
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let report = run_cycle(
            &spy,
            "STEUERN_X",
            0x12,
            FN,
            &[
                inv(Phase::Main, "GO"),
                inv(Phase::Preset, "PRE"),
                inv(Phase::Reset, "OFF"),
            ],
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
            "STEUERN_X",
            0x12,
            FN,
            &[
                inv(Phase::Preset, "PRE"),
                inv(Phase::Main, "GO"),
                inv(Phase::Reset, "OFF"),
            ],
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
            "STEUERN_X",
            0x12,
            FN,
            &[
                inv(Phase::Preset, "PRE"),
                inv(Phase::Main, "GO"),
                inv(Phase::Reset, "OFF"),
            ],
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
    async fn teardown_failure_is_reported_not_swallowed() {
        // An actuation that could not be stopped is the worst outcome; it must be
        // impossible to mistake for success.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: Some("OFF"),
        };
        let report = run_cycle(
            &spy,
            "STEUERN_X",
            0x12,
            FN,
            &[inv(Phase::Main, "GO"), inv(Phase::Reset, "OFF")],
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
        let report = run_cycle(&spy, "STATUS_X", 0x12, FN, &[inv(Phase::Main, "GO")]).await;
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
            "IO_STATUS_VORGEBEN",
            0x12,
            2,
            &[
                inv_for(1, "Fan", Phase::Main, "FAN_ON"),
                inv_for(1, "Fan", Phase::Reset, "FAN_OFF"),
                inv_for(2, "Electric fuel pump", Phase::Main, "PUMP_ON"),
                inv_for(2, "Electric fuel pump", Phase::Reset, "PUMP_OFF"),
            ],
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
            "IO_STATUS_VORGEBEN",
            0x12,
            99,
            &[
                inv_for(1, "Fan", Phase::Main, "FAN_ON"),
                inv_for(2, "Electric fuel pump", Phase::Main, "PUMP_ON"),
            ],
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
        // `Catalog::job_parameters` returns an empty Vec for an unknown
        // (variant, job). With `succeeded` computed only from failure flags, that
        // miss surfaces to the human as a completed service function while no
        // frame ever left the tester.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let report = run_cycle(&spy, "STEUERN_X", 0x12, FN, &[]).await;
        assert!(!report.succeeded, "nothing ran, so nothing succeeded");
        assert!(spy.ran.lock().unwrap().is_empty());
        assert_eq!(report.teardown, Teardown::NotDefined);
    }

    struct TableReader(Vec<(&'static str, f64)>);

    #[async_trait::async_trait]
    impl crate::precondition::MeasurementReader for TableReader {
        async fn read(&self, name: &str) -> Result<f64, String> {
            self.0
                .iter()
                .find(|(n, _)| *n == name)
                .map(|(_, v)| *v)
                .ok_or_else(|| format!("no measurement '{name}'"))
        }
    }

    #[tokio::test]
    async fn a_violated_precondition_blocks_and_sends_nothing() {
        // The crux: NOTHING may reach the car when a precondition fails.
        let spy = SpyEcu {
            ran: Mutex::new(Vec::new()),
            fail_on: None,
        };
        let reader = TableReader(vec![("KL15", 0.0), ("UBATT", 12.6), ("SPEED", 0.0)]);
        let report = run_service(
            &spy,
            &reader,
            "STEUERN_X",
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO"), inv(Phase::Reset, "OFF")],
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
        let reader = TableReader(vec![("KL15", 1.0), ("UBATT", 12.6), ("SPEED", 0.0)]);
        let report = run_service(
            &spy,
            &reader,
            "STEUERN_X",
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO"), inv(Phase::Reset, "OFF")],
        )
        .await;
        assert!(!report.blocked);
        assert!(report.succeeded);
        assert_eq!(
            *spy.ran.lock().unwrap(),
            vec!["STEUERN_X(GO)", "STEUERN_X(OFF)"]
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
            "STEUERN_X",
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO")],
        )
        .await;
        assert!(!report.blocked);
        assert!(report.succeeded);
        assert!(
            !spy.ran.lock().unwrap().is_empty(),
            "the cycle must still run"
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
        let reader = TableReader(vec![("KL15", 0.0), ("UBATT", 12.6), ("SPEED", 0.0)]);
        let report = run_service(
            &spy,
            &reader,
            "STEUERN_X",
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[
                inv(Phase::Preset, "PRE"),
                inv(Phase::Main, "GO"),
                inv(Phase::Reset, "OFF"),
            ],
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
        let reader = TableReader(vec![("KL15", 1.0), ("UBATT", 10.0), ("SPEED", 50.0)]);
        let report = run_service(
            &spy,
            &reader,
            "IS_LERNWERT",
            0x12,
            FN,
            klartext_semantic::Category::CbsReset,
            &[inv(Phase::Main, "RESET")],
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
        let reader = TableReader(vec![("KL15", 1.0), ("UBATT", 10.0), ("SPEED", 0.0)]);
        let report = run_service(
            &spy,
            &reader,
            "STEUERN_X",
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO")],
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
        let reader = TableReader(vec![("KL15", 1.0)]);
        let report = run_service(
            &spy,
            &reader,
            "IO_STATUS_VORGEBEN",
            0x12,
            2,
            klartext_semantic::Category::CbsReset,
            &[
                inv_for(1, "Fan", Phase::Main, "FAN_ON"),
                inv_for(2, "Electric fuel pump", Phase::Main, "PUMP_ON"),
            ],
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
        let reader = TableReader(vec![("KL15", 0.0)]);
        let report = run_service(
            &spy,
            &reader,
            "IO_STATUS_VORGEBEN",
            0x12,
            2,
            klartext_semantic::Category::ActuatorControl,
            &[
                inv_for(1, "Fan", Phase::Main, "FAN_ON"),
                inv_for(2, "Electric fuel pump", Phase::Main, "PUMP_ON"),
            ],
        )
        .await;
        assert!(report.blocked);
        assert_eq!(report.title.as_deref(), Some("Electric fuel pump"));
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
        let reader = TableReader(vec![("KL15", 0.0)]);
        let report = run_service(
            &spy,
            &reader,
            "STEUERN_X",
            0x12,
            FN,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO")],
        )
        .await;
        assert!(report.blocked);
        assert_eq!(report.title.as_deref(), Some("EXAMPLE"));
    }
}
