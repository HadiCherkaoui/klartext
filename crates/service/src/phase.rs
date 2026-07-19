//! Group ISTA's catalog rows into the ordered invocations of a service function.
//!
//! `job_param` stores one row per positional argument, tagged with its actuation
//! phase, EDIABAS job, and rank. This module turns those rows into the ordered
//! `Preset → Main → Reset` job-runs ISTA itself performs, each carrying its own
//! `;`-joined EDIABAS argument buffer.

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

/// One job-run within an ISTA function's phase cycle.
///
/// A phase can run several distinct jobs, and one job can repeat at different
/// ranks, so an invocation is scoped by `(function_id, phase, job, rank)` and
/// carries the single `;`-joined argument buffer for that one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Invocation {
    /// The owning ISTA fixed function's catalog id.
    pub function_id: i64,
    /// The function's human title (English preferred), when the catalog has one.
    pub title: Option<String>,
    /// Which lifecycle step this is.
    pub phase: Phase,
    /// The EDIABAS job this run sends; a phase may run several distinct jobs.
    pub job: String,
    /// This run's rank within its phase; `None` (unranked) sorts first.
    pub rank: Option<i64>,
    /// The positional argument values, already in `position` order.
    pub args: Vec<String>,
}

impl Invocation {
    /// The EDIABAS argument buffer: the positional values joined with `;`.
    pub fn arg_buffer(&self) -> String {
        self.args.join(";")
    }
}

/// Group catalog rows into invocations, one per `(function, phase, job, rank)`.
///
/// ISTA runs a phase as an ordered list of jobs — `GetJobsByPhase` is plural and
/// each phase loop is `OrderBy(Rank)` with `NULL` first (research Q1) — so a
/// phase that runs `DIAGNOSE_MODE` then `STEUERN_IO`, or the same job at two
/// ranks, becomes two ordered invocations here rather than one coalesced buffer.
/// Within a function the order is `phase`, then `rank` (unranked first), then
/// catalog arrival.
///
/// Rows are sorted defensively so a caller passing them out of order still gets
/// the right buffers. A row with a NULL value contributes an EMPTY argument
/// rather than being dropped: EDIABAS arguments are positional, so dropping one
/// would shift every later argument left and send a different command.
pub fn invocations(rows: &[JobParameterEntry]) -> Vec<Invocation> {
    let mut indexed: Vec<(usize, &JobParameterEntry)> = rows.iter().enumerate().collect();
    indexed.sort_by_key(|(idx, r)| {
        (
            r.function_id,
            Phase::from_catalog(r.phase.as_deref()),
            r.rank.unwrap_or(i64::MIN),
            r.position,
            *idx,
        )
    });
    let mut out: Vec<Invocation> = Vec::new();
    for (_, row) in indexed {
        let phase = Phase::from_catalog(row.phase.as_deref());
        let title = row
            .function_en
            .clone()
            .or_else(|| row.function_de.clone())
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty());
        let same = out.last().is_some_and(|i| {
            i.function_id == row.function_id
                && i.phase == phase
                && i.job == row.job
                && i.rank == row.rank
        });
        if same {
            let current = out.last_mut().expect("last exists when same is true");
            current.args.push(row.value.clone().unwrap_or_default());
            if current.title.is_none() {
                current.title = title;
            }
        } else {
            out.push(Invocation {
                function_id: row.function_id,
                title,
                phase,
                job: row.job.clone(),
                rank: row.rank,
                args: vec![row.value.clone().unwrap_or_default()],
            });
        }
    }
    out
}

/// The distinct function ids present in `invocations`, ascending.
///
/// One EDIABAS job name commonly carries SEVERAL functions: on variant `MRBMSC`,
/// `IO_STATUS_VORGEBEN` drives the fan, the oxygen-sensor heating, the fuel pump,
/// the injectors and the idle actuator — 1,719 of the catalog's 2,792
/// (variant, job) pairs map to more than one function. A caller must therefore
/// choose WHICH function to run rather than take whatever comes first; this
/// enumerates the choices on offer.
pub fn function_ids(invocations: &[Invocation]) -> Vec<i64> {
    let mut ids: Vec<i64> = invocations.iter().map(|i| i.function_id).collect();
    ids.sort_unstable();
    ids.dedup();
    ids
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
            rank: None,
            position,
            value: Some(value.to_string()),
            label: None,
            job: "STEUERN_EXAMPLE".to_string(),
        }
    }

    #[test]
    fn an_unranked_row_sorts_before_a_ranked_one_in_the_same_phase() {
        // ISTA orders each phase by `Rank` with a STABLE sort, and LINQ's OrderBy
        // puts null first — so a rank-less row (a legacy extract, or a row the
        // catalog left unranked) runs BEFORE a ranked sibling of the same phase.
        // The grouping key already splits them into separate invocations; this pins
        // their ORDER. Nothing else distinguishes nulls-first from nulls-last: a
        // regression to `unwrap_or(i64::MAX)` reverses these two and only this
        // asserts against it. Two DISTINCT jobs so they cannot coalesce.
        let mut unranked = row(11, "Main", 1, "FIRST");
        unranked.job = "DIAGNOSE_MODE".to_string();
        unranked.rank = None;
        let mut ranked = row(11, "Main", 1, "SECOND");
        ranked.job = "STEUERN_IO".to_string();
        ranked.rank = Some(1);
        // Hand them over ranked-first, so encounter order alone would get it wrong.
        let invs = invocations(&[ranked, unranked]);
        let order: Vec<&str> = invs
            .iter()
            .filter(|i| i.phase == Phase::Main)
            .map(|i| i.job.as_str())
            .collect();
        assert_eq!(
            order,
            vec!["DIAGNOSE_MODE", "STEUERN_IO"],
            "unranked must sort first"
        );
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
    fn function_ids_are_deduped_and_ascending() {
        // A caller uses this to OFFER the choice of function, so it must be a
        // clean menu: one entry per function, in a stable order. Fed descending
        // ids with each appearing in two phases, an implementation that skipped
        // the dedup would list five entries, and one that skipped the sort would
        // lead with 9003. Building the slice by hand (rather than via
        // `invocations`, which already sorts) is what makes both mutations fail.
        let invs = vec![
            Invocation {
                function_id: 9003,
                title: None,
                phase: Phase::Main,
                job: "STEUERN_EXAMPLE".to_string(),
                rank: None,
                args: vec!["A".to_string()],
            },
            Invocation {
                function_id: 9003,
                title: None,
                phase: Phase::Reset,
                job: "STEUERN_EXAMPLE".to_string(),
                rank: None,
                args: vec!["B".to_string()],
            },
            Invocation {
                function_id: 9001,
                title: None,
                phase: Phase::Main,
                job: "STEUERN_EXAMPLE".to_string(),
                rank: None,
                args: vec!["C".to_string()],
            },
            Invocation {
                function_id: 9002,
                title: None,
                phase: Phase::Main,
                job: "STEUERN_EXAMPLE".to_string(),
                rank: None,
                args: vec!["D".to_string()],
            },
            Invocation {
                function_id: 9001,
                title: None,
                phase: Phase::Reset,
                job: "STEUERN_EXAMPLE".to_string(),
                rank: None,
                args: vec!["E".to_string()],
            },
        ];
        assert_eq!(function_ids(&invs), vec![9001, 9002, 9003]);
        assert!(function_ids(&[]).is_empty());
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

    #[test]
    fn a_phase_with_two_jobs_yields_two_invocations_in_rank_order() {
        // ISTA's ccu_01 Main = DIAGNOSE_MODE (rank 1) then STEUERN_IO (rank 2):
        // klartext must send diagnose mode FIRST, then actuate — two separate runs.
        let rows = vec![
            JobParameterEntry {
                function_id: 5,
                function_en: Some("Fan".into()),
                function_de: None,
                phase: Some("Main".into()),
                rank: Some(2),
                position: 1,
                value: Some("ON".into()),
                label: None,
                job: "STEUERN_IO".into(),
            },
            JobParameterEntry {
                function_id: 5,
                function_en: Some("Fan".into()),
                function_de: None,
                phase: Some("Main".into()),
                rank: Some(1),
                position: 1,
                value: Some("DIAG".into()),
                label: None,
                job: "DIAGNOSE_MODE".into(),
            },
        ];
        let invs = invocations(&rows);
        let main: Vec<(&str, String)> = invs
            .iter()
            .filter(|i| i.phase == Phase::Main)
            .map(|i| (i.job.as_str(), i.arg_buffer()))
            .collect();
        assert_eq!(
            main,
            vec![
                ("DIAGNOSE_MODE", "DIAG".to_string()),
                ("STEUERN_IO", "ON".to_string()),
            ]
        );
    }

    #[test]
    fn the_same_job_at_two_ranks_is_two_sequential_runs_not_one_buffer() {
        // The 519-group case: one job at rank 1 and rank 2 in Main must be TWO
        // sequential runs, not one coalesced `;`-buffer.
        let rows = vec![
            JobParameterEntry {
                function_id: 7,
                function_en: None,
                function_de: None,
                phase: Some("Main".into()),
                rank: Some(1),
                position: 1,
                value: Some("A".into()),
                label: None,
                job: "STEUERN_LAMPEN".into(),
            },
            JobParameterEntry {
                function_id: 7,
                function_en: None,
                function_de: None,
                phase: Some("Main".into()),
                rank: Some(2),
                position: 1,
                value: Some("B".into()),
                label: None,
                job: "STEUERN_LAMPEN".into(),
            },
        ];
        let mains: Vec<String> = invocations(&rows)
            .iter()
            .filter(|i| i.phase == Phase::Main)
            .map(|i| i.arg_buffer())
            .collect();
        assert_eq!(
            mains,
            vec!["A".to_string(), "B".to_string()],
            "two ranks, two runs"
        );
    }
}
