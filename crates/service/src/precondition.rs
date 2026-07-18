//! Preconditions: klartext's own safety checks before a service write.
//!
//! ISTA's preconditions are NOT machine-readable — of the DDE's 87 fixed functions
//! only 3 carry any preparing text, and it is prose. So klartext defines its own,
//! expressed over measurements it can actually read, and attaches them by category.
//!
//! Deliberate design point (spec §5): a check that cannot be RESOLVED degrades to
//! advisory and does NOT block. The human already confirmed; refusing because a
//! lookup failed would be obstruction, not safety. Only a check that resolves and
//! FAILS blocks.

use klartext_semantic::Category;

/// Reads one named measurement from the vehicle.
///
/// Injected so this crate needs no connection: the binary backs it with the
/// client's scaled read path, tests substitute a table. Preconditions read the
/// VEHICLE (engine state comes from the engine ECU whatever is being actuated),
/// not the target ECU.
#[async_trait::async_trait]
pub trait MeasurementReader {
    /// Read `name`, or explain why it could not be read.
    async fn read(&self, name: &str) -> Result<f64, String>;
}

/// A condition that must hold before a service write runs.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Precondition {
    /// The engine must be turning.
    EngineRunning,
    /// The engine must be stopped.
    EngineOff,
    /// Terminal 15 (ignition) must be on.
    TerminalOn,
    /// Battery voltage must exceed this many volts.
    BatteryAbove(f64),
    /// Coolant temperature must be below this many °C.
    CoolantBelow(f64),
    /// The vehicle must not be moving.
    VehicleStationary,
}

/// The result of checking one precondition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// Resolved and satisfied.
    Passed,
    /// Resolved and violated — this blocks.
    Failed,
    /// Could not be resolved; advisory only.
    Unverified,
}

/// One precondition's outcome, with the number behind it.
#[derive(Debug, Clone, PartialEq)]
pub struct PreconditionOutcome {
    /// Which condition was checked.
    pub precondition: Precondition,
    /// The verdict.
    pub verdict: Verdict,
    /// The measured value, when one was read.
    pub measured: Option<f64>,
    /// Why it failed or could not be checked.
    pub detail: Option<String>,
}

impl Precondition {
    /// The measurement this condition reads.
    fn measurement(self) -> &'static str {
        match self {
            Precondition::EngineRunning | Precondition::EngineOff => "RPM",
            Precondition::TerminalOn => "KL15",
            Precondition::BatteryAbove(_) => "UBATT",
            Precondition::CoolantBelow(_) => "TCO",
            Precondition::VehicleStationary => "SPEED",
        }
    }

    /// Whether `value` satisfies the condition.
    fn satisfied_by(self, value: f64) -> bool {
        match self {
            // A cranking-speed floor, not > 0: a coasting-down engine still
            // reports a few rpm and is not "running".
            Precondition::EngineRunning => value >= 300.0,
            Precondition::EngineOff => value < 300.0,
            Precondition::TerminalOn => value != 0.0,
            Precondition::BatteryAbove(v) => value > v,
            Precondition::CoolantBelow(v) => value < v,
            Precondition::VehicleStationary => value == 0.0,
        }
    }

    /// A human explanation of what was required.
    fn requirement(self) -> String {
        match self {
            Precondition::EngineRunning => "the engine must be running".to_string(),
            Precondition::EngineOff => "the engine must be off".to_string(),
            Precondition::TerminalOn => "the ignition (terminal 15) must be on".to_string(),
            Precondition::BatteryAbove(v) => format!("battery voltage must exceed {v} V"),
            Precondition::CoolantBelow(v) => format!("coolant must be below {v} °C"),
            Precondition::VehicleStationary => "the vehicle must be stationary".to_string(),
        }
    }
}

/// The default preconditions for a category, applied when nothing more specific is known.
///
/// Category defaults cover every variant with no per-function curation. Actuation
/// is strictest: it moves a component, so it demands ignition and healthy voltage
/// (a brown-out mid-actuation is how a component gets left in a bad state).
pub fn defaults_for(category: Category) -> Vec<Precondition> {
    match category {
        Category::ActuatorControl => vec![
            Precondition::TerminalOn,
            Precondition::BatteryAbove(12.0),
            Precondition::VehicleStationary,
        ],
        Category::Calibration => vec![Precondition::TerminalOn, Precondition::BatteryAbove(12.0)],
        Category::CbsReset | Category::LearnedValueReset | Category::StatisticReset => {
            vec![Precondition::TerminalOn]
        }
    }
}

/// Check each precondition, reading what it needs.
pub async fn evaluate(
    reader: &dyn MeasurementReader,
    preconditions: &[Precondition],
) -> Vec<PreconditionOutcome> {
    let mut out = Vec::with_capacity(preconditions.len());
    for &precondition in preconditions {
        let outcome = match reader.read(precondition.measurement()).await {
            Ok(value) if precondition.satisfied_by(value) => PreconditionOutcome {
                precondition,
                verdict: Verdict::Passed,
                measured: Some(value),
                detail: None,
            },
            Ok(value) => PreconditionOutcome {
                precondition,
                verdict: Verdict::Failed,
                measured: Some(value),
                detail: Some(format!("{} (measured {value})", precondition.requirement())),
            },
            Err(why) => PreconditionOutcome {
                precondition,
                verdict: Verdict::Unverified,
                measured: None,
                detail: Some(format!(
                    "could not check that {}: {why}",
                    precondition.requirement()
                )),
            },
        };
        out.push(outcome);
    }
    out
}

/// Whether any outcome blocks execution — only a RESOLVED failure does.
pub fn blocks(outcomes: &[PreconditionOutcome]) -> bool {
    outcomes.iter().any(|o| o.verdict == Verdict::Failed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use klartext_semantic::Category;
    use std::collections::HashMap;

    /// Answers from a fixed table; anything absent is unreadable.
    struct FakeReader(HashMap<&'static str, f64>);

    #[async_trait::async_trait]
    impl MeasurementReader for FakeReader {
        async fn read(&self, name: &str) -> Result<f64, String> {
            self.0
                .get(name)
                .copied()
                .ok_or_else(|| format!("no measurement '{name}'"))
        }
    }

    fn reader(pairs: &[(&'static str, f64)]) -> FakeReader {
        FakeReader(pairs.iter().copied().collect())
    }

    #[tokio::test]
    async fn a_satisfied_precondition_passes() {
        let r = reader(&[("RPM", 800.0)]);
        let out = evaluate(&r, &[Precondition::EngineRunning]).await;
        assert_eq!(out[0].verdict, Verdict::Passed);
        assert!(!blocks(&out));
    }

    #[tokio::test]
    async fn a_violated_precondition_blocks_and_reports_the_measured_value() {
        // The operator must be told WHY, with the number that failed — "engine not
        // running" alone is not actionable.
        let r = reader(&[("RPM", 0.0)]);
        let out = evaluate(&r, &[Precondition::EngineRunning]).await;
        assert_eq!(out[0].verdict, Verdict::Failed);
        assert_eq!(out[0].measured, Some(0.0));
        assert!(blocks(&out));
    }

    #[tokio::test]
    async fn an_unreadable_precondition_is_advisory_not_blocking() {
        // Spec §5: klartext must not refuse merely because it could not look
        // something up — the human already confirmed.
        let r = reader(&[]);
        let out = evaluate(&r, &[Precondition::EngineRunning]).await;
        assert_eq!(out[0].verdict, Verdict::Unverified);
        assert!(
            out[0].detail.is_some(),
            "must say why it could not be checked"
        );
        assert!(!blocks(&out), "unverifiable must NOT block");
    }

    #[tokio::test]
    async fn engine_off_and_running_are_genuinely_opposite() {
        let running = reader(&[("RPM", 800.0)]);
        assert_eq!(
            evaluate(&running, &[Precondition::EngineOff]).await[0].verdict,
            Verdict::Failed
        );
        let stopped = reader(&[("RPM", 0.0)]);
        assert_eq!(
            evaluate(&stopped, &[Precondition::EngineOff]).await[0].verdict,
            Verdict::Passed
        );
    }

    #[tokio::test]
    async fn threshold_preconditions_compare_in_the_right_direction() {
        let r = reader(&[("UBATT", 11.4), ("TCO", 105.0)]);
        let out = evaluate(
            &r,
            &[
                Precondition::BatteryAbove(12.0),
                Precondition::CoolantBelow(90.0),
            ],
        )
        .await;
        assert!(out.iter().all(|o| o.verdict == Verdict::Failed), "{out:?}");

        let ok = reader(&[("UBATT", 12.6), ("TCO", 80.0)]);
        let out = evaluate(
            &ok,
            &[
                Precondition::BatteryAbove(12.0),
                Precondition::CoolantBelow(90.0),
            ],
        )
        .await;
        assert!(out.iter().all(|o| o.verdict == Verdict::Passed), "{out:?}");
    }

    #[test]
    fn actuation_defaults_are_stricter_than_a_counter_reset() {
        // A category that moves a component must demand more than one that only
        // rewrites a counter.
        let actuation = defaults_for(Category::ActuatorControl);
        let reset = defaults_for(Category::CbsReset);
        assert!(
            actuation.len() > reset.len(),
            "actuation {actuation:?} vs reset {reset:?}"
        );
        assert!(actuation.contains(&Precondition::TerminalOn));
        assert!(
            actuation
                .iter()
                .any(|p| matches!(p, Precondition::BatteryAbove(_)))
        );
    }
}
