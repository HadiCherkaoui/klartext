//! Run a service function's phase cycle, tearing down even when it fails.

use crate::phase::{Invocation, Phase};

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
#[derive(Debug, Clone, PartialEq, Eq)]
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
}

/// Run `invocations` as `Preset → Main → Reset`.
///
/// The safety contract:
/// - Phases run in lifecycle order regardless of the order supplied.
/// - A failed `Preset` SKIPS `Main` — preparation failed, so the ECU's state is
///   unknown and actuating would be reckless.
/// - `Reset` runs on EVERY path that attempted anything, success or failure, so an
///   actuation is never left running.
/// - A failed teardown is reported in [`Teardown::Failed`] and forces
///   `succeeded = false`: an actuation that could not be stopped must never look
///   like a success.
pub async fn run_cycle(
    runner: &dyn JobRunner,
    job: &str,
    target: u8,
    invocations: &[Invocation],
) -> ServiceReport {
    let pick = |phase: Phase| invocations.iter().find(|i| i.phase == phase);
    let title = invocations.iter().find_map(|i| i.title.clone());
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
        phases,
        succeeded: !failed && !matches!(teardown, Teardown::Failed(_)),
        teardown,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
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

    fn inv(phase: Phase, arg: &str) -> Invocation {
        Invocation {
            function_id: 1,
            title: Some("EXAMPLE".to_string()),
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
        let report = run_cycle(&spy, "STATUS_X", 0x12, &[inv(Phase::Main, "GO")]).await;
        assert_eq!(report.teardown, Teardown::NotDefined);
        assert!(report.succeeded);
    }
}
