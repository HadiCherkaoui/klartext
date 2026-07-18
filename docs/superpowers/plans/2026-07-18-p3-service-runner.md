# P3 Service Runner + Preconditions Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the offline-testable engine that executes ANY ECU's service function as ISTA's own `Preset → Main → Reset` phase cycle, guarded by preconditions — with no user-facing surface yet, so actuation is never exposed before its guard exists.

**Architecture:** A new library crate `klartext-service` orchestrates only. It resolves a function's per-phase argument buffers from the `job_param` catalog (`klartext-semantic`), evaluates preconditions through an injected `MeasurementReader`, then runs each phase through `Ecu::run_job` (`klartext-best`) against an injected exchange. It never references `klartext-client` or HSFZ — binaries compose those, preserving the existing rule that `klartext-best` and `klartext-client` meet only in a binary's `SessionBridge`.

**Tech Stack:** Rust edition 2024, tokio, async-trait, thiserror.

## Global Constraints

- Latest stable Rust, edition 2024. `thiserror` in libraries, `anyhow` at binary boundaries.
- `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` clean before a task is done. **Run `cargo fmt` via Bash, not the editor hook** (the hook uses an older rustfmt and reorders imports).
- Verify gate exit codes DIRECTLY (`cmd ; echo "rc=$?"`) — never mask behind `| tail`.
- Conventional commits.
- **Never claim a hardware round-trip works.** Everything here is verified against mocks.
- **No surface in this plan.** Do NOT add an MCP tool or a CLI subcommand — that is the next plan, deliberately sequenced after preconditions exist.
- `klartext-service` must NOT depend on `klartext-client` or `klartext-hsfz`.
- Never commit BMW data (ISTA DBs, SGBDs, captures, VINs).
- No ms-rust guideline-marker comments.

## Interfaces this plan builds on (already on main)

- `klartext_best::Ecu::run_job(&self, name: &str, target: u8, args: &[u8], exchange: &(dyn UdsExchange + Sync)) -> Result<ResultSet, RunError>`
- `klartext_best::{UdsExchange, ExchangeError, GatedExchange, Policy}` — `GatedExchange::confirmed_write(inner)` admits writes; flashing always refused.
- `klartext_semantic::Catalog::job_parameters(variant, job) -> Result<Vec<JobParameterEntry>, SemanticError>`, ordered by `(function_id, phase, position)`.
- `JobParameterEntry { function_id: i64, function_en: Option<String>, function_de: Option<String>, phase: Option<String>, position: i64, value: Option<String>, label: Option<String> }`
- `klartext_semantic::{ServiceFunctions, ServiceFunction, Category, Risk}` — `Category::{CbsReset, LearnedValueReset, StatisticReset, ActuatorControl, Calibration}`; `Category::risk() -> Risk`.

## File Structure

| File | Responsibility |
|---|---|
| `crates/service/Cargo.toml` | New crate manifest (created via `cargo new`, deps via `cargo add`) |
| `crates/service/src/lib.rs` | Crate docs + re-exports |
| `crates/service/src/phase.rs` | Group catalog rows into ordered `Invocation`s (pure) |
| `crates/service/src/precondition.rs` | `Precondition`, `MeasurementReader`, evaluation (pure + injected reads) |
| `crates/service/src/runner.rs` | `ServiceRunner`, `ServiceReport`, the phase cycle with teardown |

---

### Task 1: Scaffold the crate and group phases

**Files:**
- Create: `crates/service/Cargo.toml`, `crates/service/src/lib.rs`, `crates/service/src/phase.rs`
- Modify: root `Cargo.toml` (workspace members)

**Interfaces:**
- Consumes: `klartext_semantic::JobParameterEntry`
- Produces: `Invocation { function_id: i64, title: Option<String>, phase: Phase, args: Vec<String> }`; `Phase::{Preset, Main, Reset}`; `fn invocations(rows: &[JobParameterEntry]) -> Vec<Invocation>`; `Invocation::arg_buffer(&self) -> String`

- [ ] **Step 1: Scaffold the crate with the CLI (never hand-author a manifest)**

```bash
cd /home/hadi/gitlab/klartext
cargo new --lib crates/service --name klartext-service
cargo add --package klartext-service --path crates/semantic
cargo add --package klartext-service --path crates/best
cargo add --package klartext-service async-trait thiserror
cargo add --package klartext-service --dev tokio --features macros,rt
```

Then add `"crates/service"` to the root `Cargo.toml`'s `workspace.members` list, and align the new crate's `[package]` with the workspace convention used by the sibling crates (inspect `crates/semantic/Cargo.toml` and mirror how it inherits `version`/`edition`/`license` from `[workspace.package]`).

Verify: `cargo build -p klartext-service ; echo "rc=$?"` → 0.

- [ ] **Step 2: Write the failing test**

Create `crates/service/src/phase.rs` with only this test module for now:

```rust
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
        let main = invs.iter().find(|i| i.function_id == 9001 && i.phase == Phase::Main).unwrap();
        assert_eq!(main.arg_buffer(), "3;JA;FanArg");
        assert_eq!(main.title.as_deref(), Some("EXAMPLE fan: activation"));

        let reset = invs.iter().find(|i| i.function_id == 9001 && i.phase == Phase::Reset).unwrap();
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
```

- [ ] **Step 3: Run it to see it fail**

Run: `cargo test -p klartext-service phase:: 2>&1 | tail -15`
Expected: FAIL — `cannot find function 'invocations' in this scope`

- [ ] **Step 4: Implement**

Prepend to `crates/service/src/phase.rs`:

```rust
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
```

Set `crates/service/src/lib.rs` to:

```rust
//! Execute an ECU service function as ISTA's own phase cycle, behind preconditions.
//!
//! Orchestration only: this crate resolves a function's per-phase arguments from
//! the ISTA catalog, checks preconditions, and drives each phase through the
//! BEST/2 VM against an INJECTED exchange. It never opens a connection itself and
//! deliberately does not depend on `klartext-client` or `klartext-hsfz` — binaries
//! compose those, keeping the VM and the client apart as elsewhere in the workspace.

pub mod phase;

pub use phase::{Invocation, Phase, invocations};
```

- [ ] **Step 5: Run the tests**

Run: `cargo test -p klartext-service 2>&1 | tail -12`
Expected: PASS, 3 tests.

- [ ] **Step 6: Gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings ; echo "clippy rc=$?"
git add crates/service Cargo.toml Cargo.lock
git commit -m "feat(service): scaffold klartext-service and group catalog rows into phases"
```

---

### Task 2: The phase cycle with teardown-on-failure

**Files:**
- Create: `crates/service/src/runner.rs`
- Modify: `crates/service/src/lib.rs`

**Interfaces:**
- Consumes: Task 1's `Invocation`/`Phase`; `klartext_best::{Ecu, UdsExchange, RunError}`
- Produces: `ServiceRunner`, `ServiceReport`, `PhaseOutcome`, `Teardown`, `ServiceError`

- [ ] **Step 1: Write the failing tests**

Create `crates/service/src/runner.rs` with this test module:

```rust
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
            self.ran
                .lock()
                .unwrap()
                .push(format!("{job}({args})"));
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
        let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: None };
        let report = run_cycle(
            &spy,
            "STEUERN_X",
            0x12,
            &[inv(Phase::Main, "GO"), inv(Phase::Preset, "PRE"), inv(Phase::Reset, "OFF")],
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
        let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: Some("GO") };
        let report = run_cycle(
            &spy,
            "STEUERN_X",
            0x12,
            &[inv(Phase::Preset, "PRE"), inv(Phase::Main, "GO"), inv(Phase::Reset, "OFF")],
        )
        .await;
        assert_eq!(
            *spy.ran.lock().unwrap(),
            vec!["STEUERN_X(PRE)", "STEUERN_X(GO)", "STEUERN_X(OFF)"],
            "Reset must run after a failed Main"
        );
        assert!(!report.succeeded);
        assert_eq!(report.teardown, Teardown::Ran);
        assert!(report.phases.iter().any(|p| p.phase == Phase::Main && p.error.is_some()));
    }

    #[tokio::test]
    async fn a_failed_preset_skips_main_but_still_tears_down() {
        // If preparation failed the ECU is in an unknown state: do NOT actuate,
        // but do run the return-to-safe step.
        let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: Some("PRE") };
        let report = run_cycle(
            &spy,
            "STEUERN_X",
            0x12,
            &[inv(Phase::Preset, "PRE"), inv(Phase::Main, "GO"), inv(Phase::Reset, "OFF")],
        )
        .await;
        let ran = spy.ran.lock().unwrap().clone();
        assert!(!ran.iter().any(|r| r.contains("GO")), "Main must not run: {ran:?}");
        assert!(ran.iter().any(|r| r.contains("OFF")), "Reset must run: {ran:?}");
        assert!(!report.succeeded);
    }

    #[tokio::test]
    async fn teardown_failure_is_reported_not_swallowed() {
        // An actuation that could not be stopped is the worst outcome; it must be
        // impossible to mistake for success.
        let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: Some("OFF") };
        let report = run_cycle(&spy, "STEUERN_X", 0x12, &[inv(Phase::Main, "GO"), inv(Phase::Reset, "OFF")]).await;
        assert!(matches!(report.teardown, Teardown::Failed(_)));
        assert!(!report.succeeded);
    }

    #[tokio::test]
    async fn a_function_with_no_reset_phase_reports_none_not_ran() {
        // Most read/reset functions define only Main. "No teardown defined" must be
        // distinguishable from "teardown ran".
        let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: None };
        let report = run_cycle(&spy, "STATUS_X", 0x12, &[inv(Phase::Main, "GO")]).await;
        assert_eq!(report.teardown, Teardown::NotDefined);
        assert!(report.succeeded);
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p klartext-service runner:: 2>&1 | tail -15`
Expected: FAIL — `cannot find function 'run_cycle'`

- [ ] **Step 3: Implement**

Prepend to `crates/service/src/runner.rs`:

```rust
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
                    phases.push(PhaseOutcome { phase: Phase::Reset, args, error: None });
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
```

Add to `crates/service/src/lib.rs`:

```rust
pub mod runner;

pub use runner::{JobRunner, PhaseOutcome, ServiceReport, Teardown, run_cycle};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p klartext-service 2>&1 | tail -12`
Expected: PASS, 8 tests total.

- [ ] **Step 5: Gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings ; echo "clippy rc=$?"
git add crates/service
git commit -m "feat(service): run the Preset/Main/Reset cycle, tearing down on failure"
```

---

### Task 3: Preconditions

**Files:**
- Create: `crates/service/src/precondition.rs`
- Modify: `crates/service/src/lib.rs`

**Interfaces:**
- Consumes: `klartext_semantic::Category`
- Produces: `Precondition`, `MeasurementReader`, `PreconditionOutcome`, `Verdict`, `fn defaults_for(category: Category) -> Vec<Precondition>`, `async fn evaluate(...) -> Vec<PreconditionOutcome>`

- [ ] **Step 1: Write the failing tests**

Create `crates/service/src/precondition.rs` with this test module:

```rust
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
        assert!(out[0].detail.is_some(), "must say why it could not be checked");
        assert!(!blocks(&out), "unverifiable must NOT block");
    }

    #[tokio::test]
    async fn engine_off_and_running_are_genuinely_opposite() {
        let running = reader(&[("RPM", 800.0)]);
        assert_eq!(evaluate(&running, &[Precondition::EngineOff]).await[0].verdict, Verdict::Failed);
        let stopped = reader(&[("RPM", 0.0)]);
        assert_eq!(evaluate(&stopped, &[Precondition::EngineOff]).await[0].verdict, Verdict::Passed);
    }

    #[tokio::test]
    async fn threshold_preconditions_compare_in_the_right_direction() {
        let r = reader(&[("UBATT", 11.4), ("TCO", 105.0)]);
        let out = evaluate(
            &r,
            &[Precondition::BatteryAbove(12.0), Precondition::CoolantBelow(90.0)],
        )
        .await;
        assert!(out.iter().all(|o| o.verdict == Verdict::Failed), "{out:?}");

        let ok = reader(&[("UBATT", 12.6), ("TCO", 80.0)]);
        let out = evaluate(
            &ok,
            &[Precondition::BatteryAbove(12.0), Precondition::CoolantBelow(90.0)],
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
        assert!(actuation.iter().any(|p| matches!(p, Precondition::BatteryAbove(_))));
    }
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p klartext-service precondition:: 2>&1 | tail -15`
Expected: FAIL — `cannot find type 'Precondition'`

- [ ] **Step 3: Implement**

Prepend to `crates/service/src/precondition.rs`:

```rust
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
                detail: Some(format!(
                    "{} (measured {value})",
                    precondition.requirement()
                )),
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
```

Add to `crates/service/src/lib.rs`:

```rust
pub mod precondition;

pub use precondition::{
    MeasurementReader, Precondition, PreconditionOutcome, Verdict, blocks, defaults_for, evaluate,
};
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p klartext-service 2>&1 | tail -12`
Expected: PASS, 14 tests total.

- [ ] **Step 5: Gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings ; echo "clippy rc=$?"
git add crates/service
git commit -m "feat(service): add preconditions with advisory degradation"
```

---

### Task 4: Gate the cycle behind preconditions

**Files:**
- Modify: `crates/service/src/runner.rs`, `crates/service/src/lib.rs`

**Interfaces:**
- Consumes: Tasks 1–3
- Produces: `async fn run_service(runner, reader, job, target, category, invocations) -> ServiceReport`; `ServiceReport.preconditions: Vec<PreconditionOutcome>`; `ServiceReport.blocked: bool`

- [ ] **Step 1: Write the failing tests**

Add to `crates/service/src/runner.rs`'s test module (reuse `SpyEcu` and `inv`; add a reader double):

```rust
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
        let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: None };
        let reader = TableReader(vec![("KL15", 0.0), ("UBATT", 12.6), ("SPEED", 0.0)]);
        let report = run_service(
            &spy,
            &reader,
            "STEUERN_X",
            0x12,
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
        let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: None };
        let reader = TableReader(vec![("KL15", 1.0), ("UBATT", 12.6), ("SPEED", 0.0)]);
        let report = run_service(
            &spy,
            &reader,
            "STEUERN_X",
            0x12,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO"), inv(Phase::Reset, "OFF")],
        )
        .await;
        assert!(!report.blocked);
        assert!(report.succeeded);
        assert_eq!(*spy.ran.lock().unwrap(), vec!["STEUERN_X(GO)", "STEUERN_X(OFF)"]);
        assert!(report.preconditions.iter().all(|p| p.verdict == Verdict::Passed));
    }

    #[tokio::test]
    async fn unverifiable_preconditions_do_not_block_but_are_reported() {
        // Spec §5: degrade to advisory, and SAY SO — the caller must be able to
        // tell "checked and fine" from "could not check".
        let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: None };
        let reader = TableReader(Vec::new());
        let report = run_service(
            &spy,
            &reader,
            "STEUERN_X",
            0x12,
            klartext_semantic::Category::ActuatorControl,
            &[inv(Phase::Main, "GO")],
        )
        .await;
        assert!(!report.blocked);
        assert!(report.succeeded);
        assert!(!spy.ran.lock().unwrap().is_empty(), "the cycle must still run");
        assert!(
            report.preconditions.iter().all(|p| p.verdict == Verdict::Unverified),
            "{:?}",
            report.preconditions
        );
    }
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p klartext-service runner:: 2>&1 | tail -15`
Expected: FAIL — `cannot find function 'run_service'`

- [ ] **Step 3: Implement**

Add the two fields to `ServiceReport` (and set them in `run_cycle` to `Vec::new()` / `false`):

```rust
    /// Each precondition's outcome, checked before anything was sent.
    pub preconditions: Vec<PreconditionOutcome>,
    /// True when a RESOLVED precondition failed and the cycle was refused.
    pub blocked: bool,
```

Add to `crates/service/src/runner.rs`:

```rust
use crate::precondition::{
    MeasurementReader, PreconditionOutcome, blocks, defaults_for, evaluate,
};
use klartext_semantic::Category;

/// Check `category`'s preconditions, then run the cycle if they allow it.
///
/// A RESOLVED precondition failure refuses the whole cycle and NOTHING is sent —
/// not even `Preset`. An unresolvable check is advisory: it is reported and the
/// cycle proceeds (spec §5 — the human already confirmed; klartext must not refuse
/// because a lookup failed).
pub async fn run_service(
    runner: &dyn JobRunner,
    reader: &dyn MeasurementReader,
    job: &str,
    target: u8,
    category: Category,
    invocations: &[Invocation],
) -> ServiceReport {
    let preconditions = evaluate(reader, &defaults_for(category)).await;
    if blocks(&preconditions) {
        return ServiceReport {
            job: job.to_string(),
            title: invocations.iter().find_map(|i| i.title.clone()),
            phases: Vec::new(),
            teardown: Teardown::NotDefined,
            succeeded: false,
            preconditions,
            blocked: true,
        };
    }
    let mut report = run_cycle(runner, job, target, invocations).await;
    report.preconditions = preconditions;
    report
}
```

Export `run_service` from `lib.rs`.

- [ ] **Step 4: Run the full suite**

Run: `cargo test -p klartext-service 2>&1 | tail -12`
Expected: PASS, 17 tests total.

- [ ] **Step 5: Whole-workspace gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings ; echo "clippy rc=$?"
cargo test --workspace ; echo "test rc=$?"
git add crates/service
git commit -m "feat(service): refuse the cycle when a resolved precondition fails"
```

---

## Explicitly out of scope (next plan)

The MCP `run_service_function` tool, the extended `list_service_functions`, `watch` sampling during `Main`, the CLI rewire, the production `JobRunner`/`MeasurementReader` impls in the binaries, and extracting ISTA's `PREPARING/PROCESSING/POST` operator prose. Sequenced deliberately: **no surface exposes actuation until the guard built here exists.**
