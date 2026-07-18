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

/// A physical quantity a precondition needs, in a FIXED unit.
///
/// The implementor resolves the per-variant measurement name AND converts to the
/// documented unit. A name cannot carry a unit: `STAT_UBATT_WERT` is volts on 33
/// variants and millivolts on 28, so a string-keyed read would let a flat battery
/// satisfy a 12.0 V floor at 12 mV — inverting the guard into a rubber stamp.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantity {
    /// Engine speed, 1/min.
    EngineSpeed,
    /// Battery voltage, V.
    BatteryVoltage,
    /// Coolant temperature, °C.
    CoolantTemp,
    /// Road speed, km/h.
    RoadSpeed,
    /// Terminal 15 status: 0 = off, non-zero = on.
    TerminalStatus,
}

/// Reads one physical quantity from the vehicle, in that quantity's unit.
///
/// Injected so this crate needs no connection: the binary backs it with the
/// client's scaled read path, tests substitute a table. Preconditions read the
/// VEHICLE (engine state comes from the engine ECU whatever is being actuated),
/// not the target ECU.
///
/// Resolving which measurement NAME carries a [`Quantity`] on a given variant is
/// deliberately the implementor's job: the variant ladder and the catalog's `unit`
/// column live in the binary, not here. This crate states what it needs and in
/// what unit; the binary satisfies it.
#[async_trait::async_trait]
pub trait MeasurementReader {
    /// Read `quantity`, in the unit [`Quantity`] documents, or explain why it
    /// could not be read.
    async fn read(&self, quantity: Quantity) -> Result<f64, String>;
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
    /// The quantity this condition reads.
    pub fn quantity(self) -> Quantity {
        match self {
            Precondition::EngineRunning | Precondition::EngineOff => Quantity::EngineSpeed,
            Precondition::TerminalOn => Quantity::TerminalStatus,
            Precondition::BatteryAbove(_) => Quantity::BatteryVoltage,
            Precondition::CoolantBelow(_) => Quantity::CoolantTemp,
            Precondition::VehicleStationary => Quantity::RoadSpeed,
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
}

/// Renders what the condition requires, e.g. "battery voltage must exceed 12 V".
///
/// Public so a binary can show the operator which preconditions WOULD be enforced
/// before they confirm a write; `Debug` would leak Rust syntax into a human-facing
/// prompt.
impl std::fmt::Display for Precondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Precondition::EngineRunning => write!(f, "the engine must be running"),
            Precondition::EngineOff => write!(f, "the engine must be off"),
            Precondition::TerminalOn => write!(f, "the ignition (terminal 15) must be on"),
            Precondition::BatteryAbove(v) => write!(f, "battery voltage must exceed {v} V"),
            Precondition::CoolantBelow(v) => write!(f, "coolant must be below {v} °C"),
            Precondition::VehicleStationary => write!(f, "the vehicle must be stationary"),
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

/// Check each precondition, reading the quantity it needs.
pub async fn evaluate(
    reader: &dyn MeasurementReader,
    preconditions: &[Precondition],
) -> Vec<PreconditionOutcome> {
    let mut out = Vec::with_capacity(preconditions.len());
    for &precondition in preconditions {
        let outcome = match reader.read(precondition.quantity()).await {
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
                detail: Some(format!("{precondition} (measured {value})")),
            },
            Err(why) => PreconditionOutcome {
                precondition,
                verdict: Verdict::Unverified,
                measured: None,
                detail: Some(format!("could not check that {precondition}: {why}")),
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

    /// Answers from a fixed table; any quantity absent from it is unreadable.
    struct FakeReader(Vec<(Quantity, f64)>);

    #[async_trait::async_trait]
    impl MeasurementReader for FakeReader {
        async fn read(&self, quantity: Quantity) -> Result<f64, String> {
            self.0
                .iter()
                .find(|(q, _)| *q == quantity)
                .map(|(_, v)| *v)
                .ok_or_else(|| format!("no reading for {quantity:?}"))
        }
    }

    fn reader(pairs: &[(Quantity, f64)]) -> FakeReader {
        FakeReader(pairs.to_vec())
    }

    #[tokio::test]
    async fn a_satisfied_precondition_passes() {
        let r = reader(&[(Quantity::EngineSpeed, 800.0)]);
        let out = evaluate(&r, &[Precondition::EngineRunning]).await;
        assert_eq!(out[0].verdict, Verdict::Passed);
        assert!(!blocks(&out));
    }

    #[tokio::test]
    async fn a_violated_precondition_blocks_and_reports_the_measured_value() {
        // The operator must be told WHY, with the number that failed — "engine not
        // running" alone is not actionable.
        let r = reader(&[(Quantity::EngineSpeed, 0.0)]);
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
        let running = reader(&[(Quantity::EngineSpeed, 800.0)]);
        assert_eq!(
            evaluate(&running, &[Precondition::EngineOff]).await[0].verdict,
            Verdict::Failed
        );
        let stopped = reader(&[(Quantity::EngineSpeed, 0.0)]);
        assert_eq!(
            evaluate(&stopped, &[Precondition::EngineOff]).await[0].verdict,
            Verdict::Passed
        );
    }

    #[tokio::test]
    async fn threshold_preconditions_compare_in_the_right_direction() {
        let r = reader(&[
            (Quantity::BatteryVoltage, 11.4),
            (Quantity::CoolantTemp, 105.0),
        ]);
        let out = evaluate(
            &r,
            &[
                Precondition::BatteryAbove(12.0),
                Precondition::CoolantBelow(90.0),
            ],
        )
        .await;
        assert!(out.iter().all(|o| o.verdict == Verdict::Failed), "{out:?}");

        let ok = reader(&[
            (Quantity::BatteryVoltage, 12.6),
            (Quantity::CoolantTemp, 80.0),
        ]);
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

    #[tokio::test]
    async fn terminal_on_reads_terminal_status_in_the_right_direction() {
        // TerminalOn appears in EVERY category's defaults, so it is the single most
        // load-bearing check here — and nothing else exercises it. Inverted, it
        // would permit actuation precisely when the ignition is OFF.
        let off = reader(&[(Quantity::TerminalStatus, 0.0)]);
        assert_eq!(
            evaluate(&off, &[Precondition::TerminalOn]).await[0].verdict,
            Verdict::Failed
        );
        let on = reader(&[(Quantity::TerminalStatus, 1.0)]);
        assert_eq!(
            evaluate(&on, &[Precondition::TerminalOn]).await[0].verdict,
            Verdict::Passed
        );
    }

    #[tokio::test]
    async fn vehicle_stationary_reads_speed_in_the_right_direction() {
        // Guards the highest-risk category. Reading the wrong measurement (or the
        // wrong direction) would allow actuating a MOVING car.
        let moving = reader(&[(Quantity::RoadSpeed, 12.0)]);
        assert_eq!(
            evaluate(&moving, &[Precondition::VehicleStationary]).await[0].verdict,
            Verdict::Failed
        );
        let stopped = reader(&[(Quantity::RoadSpeed, 0.0)]);
        assert_eq!(
            evaluate(&stopped, &[Precondition::VehicleStationary]).await[0].verdict,
            Verdict::Passed
        );
    }

    #[test]
    fn each_precondition_renders_its_requirement_for_a_human() {
        // A binary must be able to show WHICH checks it would enforce before the
        // operator confirms. `Debug` would print `BatteryAbove(12.0)`; this is the
        // sentence the human reads, and the threshold has to survive into it.
        assert_eq!(
            Precondition::BatteryAbove(12.0).to_string(),
            "battery voltage must exceed 12 V"
        );
        assert_eq!(
            Precondition::VehicleStationary.to_string(),
            "the vehicle must be stationary"
        );
        assert_eq!(
            Precondition::TerminalOn.to_string(),
            "the ignition (terminal 15) must be on"
        );
    }

    #[test]
    fn each_precondition_reads_the_quantity_it_reasons_about() {
        // The seam's whole point: the pairing of condition to quantity is what a
        // binary implements against. A condition wired to the wrong quantity would
        // compare a voltage to an rpm floor and never be noticed by a type check.
        assert_eq!(
            Precondition::EngineRunning.quantity(),
            Quantity::EngineSpeed
        );
        assert_eq!(Precondition::EngineOff.quantity(), Quantity::EngineSpeed);
        assert_eq!(
            Precondition::TerminalOn.quantity(),
            Quantity::TerminalStatus
        );
        assert_eq!(
            Precondition::BatteryAbove(12.0).quantity(),
            Quantity::BatteryVoltage
        );
        assert_eq!(
            Precondition::CoolantBelow(90.0).quantity(),
            Quantity::CoolantTemp
        );
        assert_eq!(
            Precondition::VehicleStationary.quantity(),
            Quantity::RoadSpeed
        );
    }

    #[test]
    fn every_category_gets_its_intended_default_set() {
        // The one comparison below covers ActuatorControl vs CbsReset only, so a
        // category could silently lose a check. Pin each one.
        assert!(
            defaults_for(Category::Calibration)
                .iter()
                .any(|p| matches!(p, Precondition::BatteryAbove(_))),
            "a calibration write must demand healthy voltage"
        );
        for category in [
            Category::CbsReset,
            Category::LearnedValueReset,
            Category::StatisticReset,
        ] {
            assert_eq!(
                defaults_for(category),
                vec![Precondition::TerminalOn],
                "{category:?} should require ignition only"
            );
        }
        // The highest-risk category must demand the car is not moving.
        assert!(
            defaults_for(Category::ActuatorControl).contains(&Precondition::VehicleStationary),
            "actuation must require a stationary vehicle"
        );
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
