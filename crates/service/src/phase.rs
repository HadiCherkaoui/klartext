//! Group ISTA's catalog rows into the ordered invocations of a service function.
//!
//! `job_param` stores one row per positional argument, tagged with the actuation
//! phase. This module turns those rows into the `Preset → Main → Reset` sequence
//! ISTA itself performs, with each phase's `;`-joined EDIABAS argument buffer.

use klartext_semantic::JobParameterEntry;

/// One step of an actuation's lifecycle, in execution order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Phase {
    /// Prepares the ECU before the action.
    Preset,
    /// The action itself.
    Main,
    /// Teardown — the return-to-safe step, also run on failure.
    Reset,
}

impl Phase {
    /// Classify the catalog's nullable `phase` text. Anything unrecognised —
    /// including NULL — is [`Phase::Main`]: an unclassifiable step must still run,
    /// never silently disappear.
    fn from_catalog(text: Option<&str>) -> Self {
        match text.unwrap_or("Main") {
            "Preset" => Phase::Preset,
            "Reset" => Phase::Reset,
            _ => Phase::Main,
        }
    }
}

/// One phase of one ISTA function: the arguments to send in that phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The owning ISTA fixed function's catalog id.
    pub function_id: i64,
    /// The function's human title (English preferred), when the catalog has one.
    pub title: Option<String>,
    /// Which lifecycle step this is.
    pub phase: Phase,
    /// The positional argument values, already in `position` order.
    pub args: Vec<String>,
}

impl Invocation {
    /// The EDIABAS argument buffer: the positional values joined with `;`.
    pub fn arg_buffer(&self) -> String {
        self.args.join(";")
    }
}

/// Group catalog rows into invocations, one per (function, phase).
///
/// Rows are expected in `Catalog::job_parameters` order — grouped by
/// `(function_id, phase)` with `position` ascending — and are sorted defensively
/// so a caller passing them in another order still gets the right buffer. A row
/// with a NULL value contributes an EMPTY argument rather than being dropped:
/// EDIABAS arguments are positional, so dropping one would shift every later
/// argument left and send a different command.
pub fn invocations(rows: &[JobParameterEntry]) -> Vec<Invocation> {
    let mut sorted: Vec<&JobParameterEntry> = rows.iter().collect();
    sorted.sort_by_key(|r| {
        (
            r.function_id,
            Phase::from_catalog(r.phase.as_deref()),
            r.position,
        )
    });
    let mut out: Vec<Invocation> = Vec::new();
    for row in sorted {
        let phase = Phase::from_catalog(row.phase.as_deref());
        let title = row
            .function_en
            .clone()
            .or_else(|| row.function_de.clone())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        match out
            .last_mut()
            .filter(|i| i.function_id == row.function_id && i.phase == phase)
        {
            Some(current) => {
                current.args.push(row.value.clone().unwrap_or_default());
                if current.title.is_none() {
                    current.title = title;
                }
            }
            None => out.push(Invocation {
                function_id: row.function_id,
                title,
                phase,
                args: vec![row.value.clone().unwrap_or_default()],
            }),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use klartext_semantic::JobParameterEntry;

    fn row(function_id: i64, phase: &str, position: i64, value: &str) -> JobParameterEntry {
        JobParameterEntry {
            function_id,
            function_en: Some("EXAMPLE fan: activation".to_string()),
            function_de: None,
            phase: Some(phase.to_string()),
            position,
            value: Some(value.to_string()),
            label: None,
        }
    }

    #[test]
    fn shuffled_input_still_groups_and_orders_correctly() {
        // The defensive re-sort is load-bearing but every other test happens to
        // supply rows already in final order, so none of them would notice if it
        // were removed. This one hands them over deliberately scrambled: Reset
        // before Main, and position 10 ahead of 1 and 2 within the same phase.
        // It fails if `invocations` ever degrades to encounter-order grouping.
        let rows = vec![
            row(9001, "Reset", 1, "0"),
            row(9001, "Main", 10, "FanArg"),
            row(9002, "Preset", 1, "PRE"),
            row(9001, "Main", 1, "3"),
            row(9001, "Main", 2, "JA"),
        ];
        let invs = invocations(&rows);
        assert_eq!(invs.len(), 3, "{invs:?}");
        let main = invs
            .iter()
            .find(|i| i.function_id == 9001 && i.phase == Phase::Main)
            .expect("a Main invocation for 9001");
        // Scrambled input, correct buffer: position order, not arrival order.
        assert_eq!(main.arg_buffer(), "3;JA;FanArg");
        // The three Main rows must have coalesced into ONE invocation, not been
        // split by the interleaved Reset row.
        assert_eq!(
            invs.iter()
                .filter(|i| i.function_id == 9001 && i.phase == Phase::Main)
                .count(),
            1
        );
    }

    #[test]
    fn groups_rows_into_ordered_invocations_per_function_and_phase() {
        // Catalog order: grouped by (function_id, phase), positions ascending.
        let rows = vec![
            row(9001, "Main", 1, "3"),
            row(9001, "Main", 2, "JA"),
            row(9001, "Main", 10, "FanArg"),
            row(9001, "Reset", 1, "0"),
            row(9002, "Preset", 1, "PRE"),
        ];
        let invs = invocations(&rows);
        assert_eq!(invs.len(), 3);

        // The `;`-join is the EDIABAS argument buffer; P10 must follow P2.
        let main = invs
            .iter()
            .find(|i| i.function_id == 9001 && i.phase == Phase::Main)
            .unwrap();
        assert_eq!(main.arg_buffer(), "3;JA;FanArg");
        assert_eq!(main.title.as_deref(), Some("EXAMPLE fan: activation"));

        let reset = invs
            .iter()
            .find(|i| i.function_id == 9001 && i.phase == Phase::Reset)
            .unwrap();
        assert_eq!(reset.arg_buffer(), "0");
        assert!(invs.iter().any(|i| i.phase == Phase::Preset));
    }

    #[test]
    fn unknown_or_missing_phase_is_treated_as_main() {
        // The catalog's phase column is nullable; a row we cannot classify must
        // still execute rather than vanish silently.
        let mut r = row(9003, "Main", 1, "X");
        r.phase = None;
        assert_eq!(invocations(&[r])[0].phase, Phase::Main);
    }

    #[test]
    fn a_row_with_no_value_contributes_an_empty_argument() {
        // EDIABAS positional args are positional: dropping a null would SHIFT
        // every later argument left and send a different command.
        let mut r = row(9004, "Main", 1, "X");
        r.value = None;
        let mut r2 = row(9004, "Main", 2, "Y");
        r2.function_en = None;
        assert_eq!(invocations(&[r, r2])[0].arg_buffer(), ";Y");
    }
}
