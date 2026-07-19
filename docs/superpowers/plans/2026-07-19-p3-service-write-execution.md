# P3 Service-Write Execution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make klartext execute ECU service functions (resets, adaptations, actuation, calibration) as ISTA's own function-keyed, rank-ordered, held phase cycle, exposed on the MCP server behind `confirm=true` and `Policy::ConfirmedWrite`.

**Architecture:** Reshape the orphaned `klartext-service` crate from job-keyed to function-keyed: an `Invocation` becomes one job-run carrying its own job name, rank, and args; the runner runs every invocation of the chosen function per phase in rank order, holds after Main for the function's activation duration, and — a deliberate owner-approved divergence from ISTA — always runs the safe teardown on failure. The MCP binary composes `Ecu::run_job` under `GatedExchange::confirmed_write` (the transmit seam) into a `JobRunner`, exactly as `run_job` already composes it under `read_only`. `klartext-best` never depends on `klartext-client`; they meet only in the binary.

**Tech Stack:** Rust 2024, tokio, rusqlite (semantic), rmcp (MCP), `async_trait`. Semantic data from ISTA's `XEP_*` catalog via `scripts/build-semantic-db.sh`.

## Global Constraints

- **1:1 ISTA parity mandate.** Replicate ISTA's behaviour; every divergence must be one of the three owner rulings below, cited in code. Cite `file:line` into the research when implementing a behaviour.
- **Owner ruling 1 — safe teardown on failure (DIVERGENCE):** on a failed Main (or a failed job mid-phase), still run the function's Reset invocations (return-to-safe) before returning, and report `Teardown::Failed` if that too fails. ISTA skips teardown on failure; klartext must not. Label it in code as an agreed divergence.
- **Owner ruling 2 — held functions use start/hold + explicit stop:** a function ISTA holds indefinitely (`Activation==0` with Reset jobs) actuates and holds; a separate stop path runs its teardown. Stop MUST also fire on disconnect. Timed holds (`Activation>0`) hold for `activation_duration_ms` then teardown within the one call; no-hold functions keep firing-and-returning.
- **Owner ruling 3 — preconditions advise only:** drop `defaults_for(category)` and all invented threshold gating. `run_service` NEVER blocks on a precondition. Surface ISTA's operator text instead.
- **The gate is the transmit seam.** Every write reaches the car only through `GatedExchange::confirmed_write(...)`; `0x2E/0x2F/0x31/0x14/0x27` are `SidClass::Gated`, admitted under `ConfirmedWrite`, refused under `ReadOnly`. Do NOT change `crates/best/src/gate.rs`.
- **Confirmation:** every MCP write tool refuses unless `confirm=true` is relayed from the human, checked before anything touches the car — the same pattern as `clear_faults`.
- **No CLI.** It was deleted. No CLI features, no text pointing at one.
- **BYO-data.** Never commit ISTA DBs or `.prg` files; no test may depend on them. Use synthetic in-memory SQLite fixtures (see `crates/semantic/src/catalog.rs` `fixture_job_param_rank`) and the `SpyEcu` pattern in `crates/service/src/runner.rs`.
- **Gates, checked directly (never behind a pipe):** `cargo fmt --all --check; echo "FMT_RC=$?"` / `cargo test --workspace -q >/dev/null 2>&1; echo "TEST_RC=$?"` / `cargo clippy --workspace --all-targets -- -D warnings >/dev/null 2>&1; echo "CLIPPY_RC=$?"`. All 0. Run `cargo fmt --all` via Bash (the Edit hook uses an older rustfmt). No "ms-rust" marker comments.
- **Tests must discriminate.** Prove each new test by mutation: break the code, watch that specific test fail, restore, confirm green. A mutation that does not COMPILE proves nothing.
- **Stage only edited paths; never `git add -A`** (concurrent agents share the tree).

## Research source of truth

`docs/superpowers/specs/2026-07-19-research-p3-service-execution.md` — the owner rulings block and change list D0–D6. Key measured facts: 54 `(variant,fn,phase)` groups run >1 distinct job (e.g. `DIAGNOSE_MODE` then `STEUERN_IO`); 519 repeat one job at >1 rank; ISTA aborts a phase on the first failing job; hold source is `XEP_ECUFIXEDFUNCTIONS.ACTIVATION`/`ACTIVATION_DURATION_MS`; `0x27` security is NOT required for service functions; the arg buffer is per-job `;`-joined in numeric position order.

## File Structure

- `scripts/build-semantic-db.sh` — add a per-function `fixed_function(function_id, activation, activation_duration_ms)` table (D2). Needs an owner DB rebuild.
- `crates/semantic/src/catalog.rs` — add `job: String` to `JobParameterEntry`; add `job_parameters_for_function(variant, function_id)`; add `FixedFunction` struct + `fixed_function(function_id)` accessor (D1, D2).
- `crates/service/src/phase.rs` — `Invocation` becomes one job-run with `job`, `rank`, `args`; group by `(function_id, phase, rank)`; a function-scoped builder (D3).
- `crates/service/src/runner.rs` — function-keyed cycle: per phase run every invocation by rank, abort the phase on first failure, hold after Main, safe-teardown divergence; a `Hold` outcome and a `stop`/teardown entry point (D4, D5, rulings 1+2).
- `crates/service/src/precondition.rs` — remove the blocking gate; keep an advisory operator-text carrier (ruling 3).
- `crates/service/src/lib.rs` — re-export surface follows the reshape.
- `mcp/src/server.rs`, `mcp/src/dto.rs` — `run_service_function` + `stop_service` tools; a `ConfirmedWriteBridge` `JobRunner`; stop-on-disconnect.
- `mcp/tests/integration.rs` — end-to-end over the mock gateway.

---

### Task 1: Semantic DB — the hold parameters and the operator text

**Files:**
- Modify: `scripts/build-semantic-db.sh` (the `CREATE TABLE sem.*` block, after `job_param`, before the `CREATE INDEX` block near line 163)

**Interfaces:**
- Produces: a `fixed_function` table `(function_id INTEGER, activation INTEGER, activation_duration_ms INTEGER, preparing_text TEXT, processing_text TEXT, post_text TEXT)` in the built semantic DB, keyed by `function_id`, one row per fixed function that schedules at least one job.

- [ ] **Step 1: Add the `fixed_function` extraction.** After the `sem.job_param` `CREATE TABLE ... WHERE p.NAME GLOB 'P*';` statement, insert:

```sql
CREATE TABLE sem.fixed_function AS
  SELECT DISTINCT ff.ID                              AS function_id,
         CAST(ff.ACTIVATION AS INTEGER)              AS activation,
         CAST(ff.ACTIVATION_DURATION_MS AS INTEGER)  AS activation_duration_ms,
         NULLIF(pre.CONTENT_ENGB, '')                AS preparing_text,
         NULLIF(pro.CONTENT_ENGB, '')                AS processing_text,
         NULLIF(pst.CONTENT_ENGB, '')                AS post_text
  FROM XEP_ECUFIXEDFUNCTIONS ff
  LEFT JOIN XEP_REFCONTENTS rpre ON rpre.ID = ff.PREPARING_OPERATOR_TEXTID
  LEFT JOIN XEP_IOCONTENTS  pre  ON pre.CONTROLID = rpre.CONTENTCONTROLID
  LEFT JOIN XEP_REFCONTENTS rpro ON rpro.ID = ff.PROCESSING_OPERATOR_TEXTID
  LEFT JOIN XEP_IOCONTENTS  pro  ON pro.CONTROLID = rpro.CONTENTCONTROLID
  LEFT JOIN XEP_REFCONTENTS rpst ON rpst.ID = ff.POST_OPERATOR_TEXTID
  LEFT JOIN XEP_IOCONTENTS  pst  ON pst.CONTROLID = rpst.CONTENTCONTROLID
  WHERE ff.ID IN (SELECT ID FROM XEP_REFECUJOBS);
CREATE INDEX sem.idx_fixed_function ON fixed_function(function_id);
```

**Note for the implementer:** the operator-text column/ref names (`PREPARING_OPERATOR_TEXTID` etc.) and the content-join shape follow the existing `bordnet_doc`/`fault_doc` pattern in this script (join `XEP_REFCONTENTS` → `XEP_IOCONTENTS`). **Verify the exact column names against the real schema first:** `sqlite3 <DiagDocDb> '.schema XEP_ECUFIXEDFUNCTIONS'` (decrypt via the script's existing `sqlite3mc` + rc4 key). If the operator text is a direct column rather than a content ref, select it directly; if a ref name differs, correct it. The `activation`/`activation_duration_ms`/`function_id` columns ARE direct on `XEP_ECUFIXEDFUNCTIONS` (research §Q2) and must not change. If the operator-text join cannot be resolved, ship the three text columns as `NULL` and record that in the commit — the hold parameters are the load-bearing part; the text is additive.

- [ ] **Step 2: Verify the script still parses.** This is a shell/SQL change with no unit test (the DB is BYO-data, absent in CI). Run `bash -n scripts/build-semantic-db.sh; echo "SYNTAX_RC=$?"` — expect `0`. If the ISTA DB is present locally, run the script and confirm `sqlite3 <out.db> 'SELECT COUNT(*) FROM fixed_function;'` is non-zero and `'SELECT COUNT(*) FROM fixed_function WHERE activation_duration_ms IS NOT NULL;'` is plausible (research: ~67 timed functions fleet-wide, so any small positive count is fine on a subset).

- [ ] **Step 3: Commit.**

```bash
git add scripts/build-semantic-db.sh
git commit -m "feat(semantic): extract fixed-function hold params + operator text

XEP_ECUFIXEDFUNCTIONS.ACTIVATION/ACTIVATION_DURATION_MS drive ISTA's post-Main
hold (research Q2); PREPARING/PROCESSING/POST operator text is what ISTA shows the
human in place of a machine-checked precondition (ruling 3). Keyed by function_id.
Owner must rebuild the semantic DB (one command) to populate it.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01SeoB5T7GkuBQXFJHEySpgq"
```

---

### Task 2: Semantic layer — job name, function-keyed query, fixed-function accessor

**Files:**
- Modify: `crates/semantic/src/catalog.rs` (`JobParameterEntry` ~170-191; `job_parameters` ~683; the test fixtures ~877 and ~966)

**Interfaces:**
- Consumes: the `job_param.job` column (already extracted — `build-semantic-db.sh:154`, fixture line 877) and the Task 1 `fixed_function` table.
- Produces:
  - `JobParameterEntry` gains `pub job: String`.
  - `Catalog::job_parameters_for_function(&self, variant: &str, function_id: i64) -> Result<Vec<JobParameterEntry>, SemanticError>` — ALL rows for that `(variant, function_id)` across every phase/job/rank, `ORDER BY phase, rank, position`.
  - `pub struct FixedFunction { pub function_id: i64, pub activation: Option<i64>, pub activation_duration_ms: Option<i64>, pub preparing_text: Option<String>, pub processing_text: Option<String>, pub post_text: Option<String> }`.
  - `Catalog::fixed_function(&self, function_id: i64) -> Result<Option<FixedFunction>, SemanticError>`.

- [ ] **Step 1: Add `job` to `JobParameterEntry` and select it.** Add the field (doc: "The EDIABAS job this argument's phase runs — an invocation is scoped by `(function_id, phase, rank)` and carries its own job name, since a phase may run several distinct jobs."). In `job_parameters`, add `job` to both SELECT lists (`... value, label, job FROM job_param ...`) and read it: `job: row.get(8)?`. Add `job TEXT` to the two in-code test fixtures that create `job_param` if missing (line ~877 already has it; confirm both fixtures include it — the `fixture_job_param_rank` at ~966 already does).

- [ ] **Step 2: Update existing `job_parameters` tests for the new field.** Any test constructing `JobParameterEntry` literals (search `JobParameterEntry {`) gets `job: "…".to_string()`. Run `cargo test -p klartext-semantic -q; echo "RC=$?"` — expect PASS.

- [ ] **Step 3: Write the failing test for `job_parameters_for_function`.** In the `catalog.rs` tests, using the `fixture` DB (which has functions 9001/9002 on `dde_a` across Main/Reset):

```rust
#[test]
fn job_parameters_for_function_returns_all_phases_and_jobs_ordered() {
    let (_dir, path) = fixture();
    let cat = Catalog::open(&path).unwrap();
    // Function 9001 on dde_a spans Main and Reset, job STEUERN_EXAMPLE.
    let rows = cat.job_parameters_for_function("dde_a", 9001).unwrap();
    assert!(rows.iter().all(|r| r.function_id == 9001), "{rows:?}");
    assert!(rows.iter().all(|r| r.job == "STEUERN_EXAMPLE"), "{rows:?}");
    // Phase order Main before Reset, and every returned row belongs to 9001 only.
    let phases: Vec<&str> = rows.iter().map(|r| r.phase.as_deref().unwrap_or("")).collect();
    let main_at = phases.iter().position(|p| *p == "Main").unwrap();
    let reset_at = phases.iter().position(|p| *p == "Reset").unwrap();
    assert!(main_at < reset_at, "Main must precede Reset: {phases:?}");
    // A different function on the same variant is NOT included.
    assert!(cat.job_parameters_for_function("dde_a", 9002).unwrap()
        .iter().all(|r| r.function_id == 9002));
}
```

Run: `cargo test -p klartext-semantic job_parameters_for_function -q` → FAIL (method missing).

- [ ] **Step 4: Implement `job_parameters_for_function`.** Mirror `job_parameters` but key on `function_id`, dropping the `job` filter. Guard `has_table("job_param")`; branch on `has_column("job_param","rank")` for the `rank` vs `NULL` SELECT. SQL (rank present):

```sql
SELECT function_id, function_en, function_de, phase, rank, position, value, label, job
FROM job_param WHERE ecu_variant = ?1 AND function_id = ?2
ORDER BY phase, rank, position
```

Params `[variant, &function_id.to_string()]` won't type-check for the i64 — bind with `rusqlite::params![variant, function_id]`. Row mapping identical to `job_parameters` including `job: row.get(8)?`. Run Step 3's test → PASS.

- [ ] **Step 5: Add `FixedFunction` + `fixed_function`, with a fixture row.** Add the struct (above the impl). In the main `fixture` builder, add:

```sql
CREATE TABLE fixed_function (function_id INTEGER, activation INTEGER, activation_duration_ms INTEGER, preparing_text TEXT, processing_text TEXT, post_text TEXT);
INSERT INTO fixed_function VALUES (9001, 1, 5000, 'Ansteuerung 5s', NULL, NULL);
INSERT INTO fixed_function VALUES (9002, 0, NULL, NULL, NULL, NULL);
```

Implement:

```rust
pub fn fixed_function(&self, function_id: i64) -> Result<Option<FixedFunction>, SemanticError> {
    if !self.has_table("fixed_function")? {
        return Ok(None);
    }
    let mut stmt = self.conn.prepare(
        "SELECT function_id, activation, activation_duration_ms, preparing_text, \
         processing_text, post_text FROM fixed_function WHERE function_id = ?1",
    )?;
    let row = stmt
        .query_row([function_id], |r| {
            Ok(FixedFunction {
                function_id: r.get(0)?,
                activation: r.get(1)?,
                activation_duration_ms: r.get(2)?,
                preparing_text: r.get(3)?,
                processing_text: r.get(4)?,
                post_text: r.get(5)?,
            })
        })
        .optional()?;
    Ok(row)
}
```

(`optional()` is `rusqlite::OptionalExtension`, already imported for other accessors — confirm the `use`.)

- [ ] **Step 6: Write and pass the `fixed_function` test.**

```rust
#[test]
fn fixed_function_reads_hold_params_and_missing_table_is_none() {
    let (_dir, path) = fixture();
    let cat = Catalog::open(&path).unwrap();
    let ff = cat.fixed_function(9001).unwrap().expect("9001 present");
    assert_eq!(ff.activation, Some(1));
    assert_eq!(ff.activation_duration_ms, Some(5000));
    assert_eq!(ff.preparing_text.as_deref(), Some("Ansteuerung 5s"));
    // A function absent from the table resolves to None, not an error.
    assert!(cat.fixed_function(4242).unwrap().is_none());
}
```

Run: `cargo test -p klartext-semantic fixed_function -q` → PASS.

- [ ] **Step 7: Export and commit.** Add `FixedFunction` to the `pub use catalog::{...}` re-export in `crates/semantic/src/lib.rs`. Run all three gates.

```bash
git add crates/semantic/src/catalog.rs crates/semantic/src/lib.rs
git commit -m "feat(semantic): function-keyed job query + fixed-function accessor

JobParameterEntry carries its job name; job_parameters_for_function returns a
function's every phase/job/rank invocation ordered for execution; FixedFunction
exposes the hold params + operator text. Feeds the reshaped service runner.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01SeoB5T7GkuBQXFJHEySpgq"
```

---

### Task 3: Phase model — one invocation is one job-run

**Files:**
- Modify: `crates/service/src/phase.rs` (whole file: `Invocation`, `invocations`, `function_ids`, tests)

**Interfaces:**
- Consumes: `JobParameterEntry` with its new `job` field (Task 2).
- Produces:
  - `Invocation` gains `pub job: String` and `pub rank: Option<i64>`; keeps `function_id`, `title`, `phase`, `args`.
  - `invocations(rows: &[JobParameterEntry]) -> Vec<Invocation>` now groups by `(function_id, phase, job, rank)` — a new group whenever ANY of those change — and orders within a function by `phase` then `rank` (NULL rank first, matching ISTA's stable `OrderBy(Rank)` with nulls first) then arrival.
  - `function_ids` unchanged.

- [ ] **Step 1: Write the failing multi-job test.** A phase with two distinct jobs must produce two invocations in rank order, each with its own job and buffer:

```rust
#[test]
fn a_phase_with_two_jobs_yields_two_invocations_in_rank_order() {
    // ISTA's ccu_01 Main = DIAGNOSE_MODE (rank 1) then STEUERN_IO (rank 2):
    // klartext must send diagnose mode FIRST, then actuate — two separate runs.
    let rows = vec![
        JobParameterEntry { function_id: 5, function_en: Some("Fan".into()), function_de: None,
            phase: Some("Main".into()), rank: Some(2), position: 1, value: Some("ON".into()),
            label: None, job: "STEUERN_IO".into() },
        JobParameterEntry { function_id: 5, function_en: Some("Fan".into()), function_de: None,
            phase: Some("Main".into()), rank: Some(1), position: 1, value: Some("DIAG".into()),
            label: None, job: "DIAGNOSE_MODE".into() },
    ];
    let invs = invocations(&rows);
    let main: Vec<(&str, String)> = invs.iter()
        .filter(|i| i.phase == Phase::Main)
        .map(|i| (i.job.as_str(), i.arg_buffer()))
        .collect();
    assert_eq!(main, vec![("DIAGNOSE_MODE", "DIAG".to_string()), ("STEUERN_IO", "ON".to_string())]);
}
```

Run → FAIL (rows coalesce into one invocation / no `job` field).

- [ ] **Step 2: Reshape `Invocation` and `invocations`.** Add `job: String` and `rank: Option<i64>` to the struct. Rewrite the grouping key so a new `Invocation` starts whenever `(function_id, phase, job, rank)` changes; sort by `(function_id, phase, rank.unwrap_or(i64::MIN), arrival_index)` so NULL rank sorts first and equal keys keep catalog order. Keep the "NULL value → empty arg, never dropped" rule and the title-fill rule. Show the full rewritten `invocations`:

```rust
pub fn invocations(rows: &[JobParameterEntry]) -> Vec<Invocation> {
    let mut indexed: Vec<(usize, &JobParameterEntry)> = rows.iter().enumerate().collect();
    indexed.sort_by_key(|(idx, r)| {
        (r.function_id, Phase::from_catalog(r.phase.as_deref()),
         r.rank.unwrap_or(i64::MIN), r.position, *idx)
    });
    let mut out: Vec<Invocation> = Vec::new();
    for (_, row) in indexed {
        let phase = Phase::from_catalog(row.phase.as_deref());
        let title = row.function_en.clone().or_else(|| row.function_de.clone())
            .map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
        let same = out.last().is_some_and(|i| {
            i.function_id == row.function_id && i.phase == phase
                && i.job == row.job && i.rank == row.rank
        });
        if same {
            let cur = out.last_mut().unwrap();
            cur.args.push(row.value.clone().unwrap_or_default());
            if cur.title.is_none() { cur.title = title; }
        } else {
            out.push(Invocation {
                function_id: row.function_id, title, phase,
                job: row.job.clone(), rank: row.rank,
                args: vec![row.value.clone().unwrap_or_default()],
            });
        }
    }
    out
}
```

Update the existing `row(...)` test helper to take/emit a `job` (default `"STEUERN_EXAMPLE"`) and set `rank: None`. Run Step 1's test → PASS.

- [ ] **Step 3: Add the same-job-two-ranks test.** The 519-group case: one job at rank 1 and rank 2 in Main must be TWO sequential runs, not one coalesced buffer.

```rust
#[test]
fn the_same_job_at_two_ranks_is_two_sequential_runs_not_one_buffer() {
    let rows = vec![
        JobParameterEntry { function_id: 7, function_en: None, function_de: None,
            phase: Some("Main".into()), rank: Some(1), position: 1, value: Some("A".into()),
            label: None, job: "STEUERN_LAMPEN".into() },
        JobParameterEntry { function_id: 7, function_en: None, function_de: None,
            phase: Some("Main".into()), rank: Some(2), position: 1, value: Some("B".into()),
            label: None, job: "STEUERN_LAMPEN".into() },
    ];
    let mains: Vec<String> = invocations(&rows).iter()
        .filter(|i| i.phase == Phase::Main).map(|i| i.arg_buffer()).collect();
    assert_eq!(mains, vec!["A".to_string(), "B".to_string()], "two ranks, two runs");
}
```

Run → PASS (the reshape already yields it). Fix any other `phase.rs` test broken by the struct change (add `job`/`rank`). Prove the multi-job test discriminates: temporarily drop `i.job == row.job && i.rank == row.rank` from the `same` check → Step 1 fails → restore.

- [ ] **Step 4: Run gates and commit.**

```bash
git add crates/service/src/phase.rs
git commit -m "feat(service): an invocation is one job-run (function+phase+job+rank)

Reshapes the phase model so a phase running two distinct jobs (DIAGNOSE_MODE then
STEUERN_IO — 54 real groups) produces two ordered runs, and the same job at two
ranks (519 groups) runs twice, instead of coalescing into one malformed buffer.
Rank orders within a phase, nulls first, matching ISTA's OrderBy(Rank).

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01SeoB5T7GkuBQXFJHEySpgq"
```

---

### Task 4: Runner — function-keyed cycle, rank order, multi-job abort, hold, safe teardown

**Files:**
- Modify: `crates/service/src/runner.rs` (`run_cycle`, `run_service`, `ServiceReport`, `PhaseOutcome`, tests)

**Interfaces:**
- Consumes: reshaped `Invocation` (Task 3), `JobRunner` (unchanged trait), `FixedFunction` hold params (Task 2).
- Produces:
  - `run_cycle`/`run_service` DROP the `job: &str` parameter — each invocation carries its own job. Signature becomes `run_service(runner, function_id, target, invocations, hold, clock)` (see below); the `category`/`reader`/preconditions come off in Task 6, so for THIS task keep the precondition params but stop threading a single `job`.
  - `PhaseOutcome` gains `pub job: String` and `pub rank: Option<i64>`.
  - A `Hold` value describing the post-Main wait: `pub enum Hold { None, Timed(Duration), UntilStop }`, derived from `FixedFunction` by a pure `fn hold_for(ff: Option<&FixedFunction>, has_reset: bool) -> Hold` (research Q2: `Activation>0 && has_reset → Timed(dur)`; `Activation==0 && has_reset → UntilStop`; no reset → `None`).

- [ ] **Step 1: Write `hold_for` and its test (pure, table-driven).**

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Hold { None, Timed(std::time::Duration), UntilStop }

/// ISTA's post-Main hold (research Q2, DoTriggerComponent:12137-12146):
/// num3 = Activation>0 ? ActivationDurationMs : -1; then no-Reset forces 0.
pub fn hold_for(ff: Option<&FixedFunction>, has_reset: bool) -> Hold {
    if !has_reset { return Hold::None; }
    match ff.and_then(|f| f.activation) {
        Some(a) if a > 0 => Hold::Timed(std::time::Duration::from_millis(
            ff.and_then(|f| f.activation_duration_ms).unwrap_or(0).max(0) as u64)),
        _ => Hold::UntilStop,
    }
}
```

Test all three arms, incl. `has_reset=false` → `None` regardless of activation, and `Activation=0, has_reset=true` → `UntilStop`. Mutation: change `a > 0` to `a >= 0` → the `Activation=0` arm test fails.

- [ ] **Step 2: Rewrite `run_cycle` to be function-keyed and multi-job, with hold and safe teardown.** The clock (the hold sleep) is injected so tests don't wait — take `hold: Hold` and a `sleep: impl Fn(Duration) -> Fut` seam, OR reuse the crate's tokio and expose a `run_cycle_holding` seam with the sleep as a `Duration` param the tests pass as near-zero (mirror the `cycle_terminal_15_holding` pattern in `crates/client/src/client.rs`). Core logic:

```
select = |phase| invocations.iter().filter(|i| i.function_id == function_id && i.phase == phase)
for phase in [Preset, Main]:
    for inv in select(phase) ordered as given (already rank-ordered by Task 3):
        if failed: break outer
        run inv.job with inv.arg_buffer(); push PhaseOutcome{phase, job, rank, args, error}
        if error: failed = true; break        // ISTA aborts the phase on first bad job
if not failed and hold == Timed(d): sleep(d)   // UntilStop is handled by Task 5, treat as no-wait here
// TEARDOWN — ruling 1 divergence: run Reset invocations on EVERY path, success OR failure.
for inv in select(Reset): run inv.job; record; set Teardown::Ran / Failed
```

`succeeded = attempted_something && !failed && teardown != Failed`. Keep the "empty invocation list / unmatched function id → not a success, nothing sent" guarantees and their tests (adapt to the new signature). Every existing safety test in this file (`reset_still_runs_when_main_fails`, `a_failed_preset_skips_main_but_still_tears_down`, `only_the_requested_functions_phases_run`, etc.) must keep passing — update them to the new `Invocation`/signature, NOT delete them.

- [ ] **Step 3: Write the multi-job-abort test (ISTA semantics + ruling 1 divergence together).** Preset `DIAGNOSE_MODE` ok, Main runs `STEUERN_IO` rank 1 (fails) — rank 2 job must NOT run (ISTA abort), but Reset MUST run (klartext safe-teardown divergence):

```rust
#[tokio::test]
async fn a_failed_job_aborts_the_phase_but_still_tears_down() {
    let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: Some("IO1") };
    let invs = vec![
        inv_job(5, Phase::Main, Some(1), "STEUERN_IO", "IO1"),
        inv_job(5, Phase::Main, Some(2), "STEUERN_IO", "IO2"),
        inv_job(5, Phase::Reset, Some(1), "STEUERN_IO_AUS", "OFF"),
    ];
    let report = run_cycle(&spy, 5, 0x12, &invs, Hold::None).await;
    let ran = spy.ran.lock().unwrap().clone();
    assert!(ran.iter().any(|r| r.contains("IO1")), "{ran:?}");
    assert!(!ran.iter().any(|r| r.contains("IO2")), "rank 2 must not run after rank 1 fails: {ran:?}");
    assert!(ran.iter().any(|r| r.contains("OFF")), "safe teardown must run on failure: {ran:?}");
    assert!(!report.succeeded);
}
```

`inv_job` is a new test helper `fn inv_job(fid, phase, rank, job, arg) -> Invocation`. Run → PASS. Mutations: (a) remove the `break` after a failed job → `IO2` runs → test fails; (b) skip teardown when `failed` → `OFF` absent → test fails. Restore both.

- [ ] **Step 4: Write the timed-hold test.** With `Hold::Timed`, teardown runs AFTER the hold, and the hold is actually awaited (assert ordering via a spy timestamp or that the sleep seam was called with the right duration). Use the injected sleep seam so the test is instant. Mutation: run teardown before the hold → fails.

- [ ] **Step 5: Run gates and commit.** Suite wall time must not balloon (the hold is injected/near-zero in tests).

```bash
git add crates/service/src/runner.rs
git commit -m "feat(service): function-keyed multi-job cycle with hold + safe teardown

Runs each invocation's own job in rank order, aborts the phase on the first
failing job (ISTA), holds after Main for the function's activation duration
(research Q2), and — owner ruling 1, a documented divergence from ISTA — ALWAYS
runs the Reset invocations on failure so a failed actuation is never left forced.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01SeoB5T7GkuBQXFJHEySpgq"
```

---

### Task 5: Runner — start/hold + explicit stop for held functions (ruling 2)

**Files:**
- Modify: `crates/service/src/runner.rs` (add a start/stop split for `Hold::UntilStop`)

**Interfaces:**
- Produces:
  - `run_service` returns, for a `Hold::UntilStop` function, a report whose Main ran and whose Reset did NOT (the component is held), tagged so a caller knows a stop is owed: add `pub held: bool` to `ServiceReport` (true only when Main succeeded and the hold is `UntilStop` and teardown was deferred).
  - `Catalog`-free `stop_service(runner, function_id, target, invocations) -> ServiceReport` running ONLY the Reset invocations (the teardown), for the caller's explicit stop and for stop-on-disconnect.

- [ ] **Step 1: Write the held-start test.** `Hold::UntilStop`, Main succeeds → Reset is NOT run, `report.held == true`, `report.succeeded == true` (the actuation itself worked):

```rust
#[tokio::test]
async fn an_until_stop_function_holds_and_defers_teardown() {
    let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: None };
    let invs = vec![
        inv_job(5, Phase::Main, Some(1), "STEUERN_IO", "ON"),
        inv_job(5, Phase::Reset, Some(1), "STEUERN_IO_AUS", "OFF"),
    ];
    let report = run_cycle(&spy, 5, 0x12, &invs, Hold::UntilStop).await;
    assert!(report.held, "an until-stop function must report it is holding");
    assert!(report.succeeded);
    assert!(!spy.ran.lock().unwrap().iter().any(|r| r.contains("OFF")),
        "teardown must be DEFERRED, not run: {:?}", spy.ran.lock().unwrap());
}
```

**Ruling 1 still applies:** if Main FAILS under `UntilStop`, teardown runs immediately (safe) and `held` is false. Add that as a second test. Run → FAIL, then make `run_cycle` skip teardown only when `hold == UntilStop && !failed`, setting `held` accordingly.

- [ ] **Step 2: Write the `stop_service` test.** Runs ONLY Reset invocations:

```rust
#[tokio::test]
async fn stop_service_runs_only_the_teardown() {
    let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: None };
    let invs = vec![
        inv_job(5, Phase::Main, Some(1), "STEUERN_IO", "ON"),
        inv_job(5, Phase::Reset, Some(1), "STEUERN_IO_AUS", "OFF"),
    ];
    let report = stop_service(&spy, 5, 0x12, &invs).await;
    assert_eq!(*spy.ran.lock().unwrap(), vec!["STEUERN_IO_AUS(OFF)"]);
    assert_eq!(report.teardown, Teardown::Ran);
}
```

Implement `stop_service`. Run → PASS. Mutation: have `stop_service` also run Main → fails.

- [ ] **Step 3: Run gates and commit.**

```bash
git add crates/service/src/runner.rs
git commit -m "feat(service): start/hold + explicit stop for held functions (ruling 2)

An Activation==0 function actuates and HOLDS — its teardown is deferred and the
report says so (held=true) — matching ISTA's hold-until-Stop. stop_service runs
only the teardown, for the caller's explicit stop and for stop-on-disconnect.
Ruling 1 still wins on failure: a failed hold tears down immediately.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01SeoB5T7GkuBQXFJHEySpgq"
```

---

### Task 6: Preconditions — advise only, drop the invented gate (ruling 3)

**Files:**
- Modify: `crates/service/src/runner.rs` (`run_service` stops blocking), `crates/service/src/precondition.rs`, `crates/service/src/lib.rs`

**Interfaces:**
- Produces:
  - `run_service` no longer takes `reader`/`category` and never blocks. `ServiceReport.blocked` and `.preconditions` are REMOVED. `run_service(runner, function_id, target, invocations, hold) -> ServiceReport`.
  - The invented `Precondition`/`Verdict`/`defaults_for`/`evaluate`/`blocks`/`PreconditionOutcome`/`MeasurementReader`/`Quantity` are removed from the execution path. `Quantity` + `resolve_quantity` may already be used by the semantic catalog — do NOT remove those; only remove what lived in `crates/service`.
  - Advisory operator text rides on the report or is fetched at the surface from `FixedFunction` (Task 2) — the runner does not gate on it.

- [ ] **Step 1: Delete the blocking path from `run_service` and its tests.** Remove the `evaluate/blocks/return blocked` prelude. Delete the precondition tests in `runner.rs` (`a_violated_precondition_blocks_*`, `run_service_checks_the_passed_categorys_defaults_*`, `unverifiable_preconditions_*`, etc.) — they assert the invented behaviour the owner dropped. Keep every NON-precondition test (phase order, teardown, function selection). Update `run_service` callers/signature.

- [ ] **Step 2: Reduce `precondition.rs` to the advisory carrier, or remove it.** If nothing in `crates/service` still needs `MeasurementReader`/`Quantity`, delete `precondition.rs` and its `mod`/`pub use` lines (YAGNI — it was the invented gate). If the surface will show operator text, that text comes from `FixedFunction`, not this module. Decide and state which in the commit. Run `cargo test -p klartext-service -q; echo "RC=$?"`.

- [ ] **Step 3: Update `lib.rs` re-exports** to the reshaped surface: `pub use runner::{JobRunner, PhaseOutcome, ServiceReport, Teardown, Hold, hold_for, run_service, stop_service};` and `pub use phase::{Invocation, Phase, function_ids, invocations};`. Remove precondition re-exports if the module is gone.

- [ ] **Step 4: Run gates and commit.**

```bash
git add crates/service/src/runner.rs crates/service/src/precondition.rs crates/service/src/lib.rs
git commit -m "refactor(service): preconditions advise only, drop the invented gate (ruling 3)

ISTA machine-checks no preconditions (research A) — they are prose operator text
it shows the human. Removes klartext's invented defaults_for(category) thresholds
and the blocking path; run_service never refuses on a precondition. Operator text
comes from FixedFunction and is advisory. This is 1:1 with ISTA, which proceeds.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01SeoB5T7GkuBQXFJHEySpgq"
```

---

### Task 7: MCP — `run_service_function` + `stop_service` behind `ConfirmedWrite`

**Files:**
- Modify: `mcp/src/server.rs` (new tools, a `ConfirmedWriteBridge` `JobRunner`, stop-on-disconnect), `mcp/src/dto.rs` (request/result types), `mcp/tests/integration.rs`

**Interfaces:**
- Consumes: `klartext_service::{run_service, stop_service, invocations, Hold, hold_for, ServiceReport}`, `Catalog::{job_parameters_for_function, fixed_function}`, `Ecu::run_job`, `GatedExchange::confirmed_write`, `TelegramExchange`, `SessionBridge`.
- Produces: MCP tools `run_service_function { ecu, variant?, function_id, confirm }` and `stop_service { ecu, variant?, function_id, confirm }`; a `ConfirmedWriteBridge` implementing `klartext_service::JobRunner` over `Ecu` + the confirmed-write gate; `KlartextServer` tracks any outstanding held function so `disconnect` stops it.

- [ ] **Step 1: Add the `ConfirmedWriteBridge` `JobRunner`.** It composes exactly as `run_job` does but with `confirmed_write` instead of `read_only`. It must hold the `Ecu` and the session client; each `run(job, target, args)` runs one job through the gate and maps the VM error to `String`. Sketch:

```rust
struct ConfirmedWriteBridge<'a> { ecu: &'a Ecu, client: &'a DiagnosticClient }

#[async_trait::async_trait]
impl klartext_service::JobRunner for ConfirmedWriteBridge<'_> {
    async fn run(&self, job: &str, target: u8, args: &str) -> Result<(), String> {
        let gate = GatedExchange::confirmed_write(TelegramExchange::new(SessionBridge { client: self.client }));
        self.ecu.run_job(job, target, args.as_bytes(), &gate)
            .await.map(|_| ()).map_err(|e| e.to_string())
    }
}
```

The gate is the transmit seam: a job whose bytecode emits a non-write SID still passes; a flashing SID is refused even here. Do NOT change the gate.

- [ ] **Step 2: Implement `run_service_function`.** Refuse unless `confirm` (before touching the car), resolve address + variant + `.prg` (reuse `resolve_variant`/`sgbd_path` like `run_job`), load the function's invocations via `job_parameters_for_function` → `klartext_service::invocations`, compute `Hold` from `fixed_function` + whether Reset invocations exist, then `run_service(&bridge, function_id, address, &invs, hold)` under the session lock. Return a DTO carrying: the function title, each `PhaseOutcome` (phase/job/rank/args/error), `succeeded`, `teardown`, `held`, and the operator text from `fixed_function`. The tool description MUST state: this actuates a component; requires `confirm=true`; a held function stays actuated until `stop_service` or `disconnect`; and (from `FixedFunction`) surface the operator text so the human sees ISTA's instructions.

- [ ] **Step 3: Implement `stop_service` + stop-on-disconnect.** `stop_service` refuses without `confirm`, then runs `klartext_service::stop_service` (teardown only) over the bridge. Track the outstanding held `(function_id, address, variant)` on `KlartextServer` state when `run_service_function` returns `held=true`; clear it on a successful `stop_service`; and in `disconnect`, if one is outstanding, run its teardown before dropping the session (best-effort, logged to stderr — never stdout). This is ruling 2's "stop MUST also fire on disconnect".

- [ ] **Step 4: Add the confirm-refusal tests.** Both tools refuse before the connection check when `confirm=false` (mirror `clear_faults_refuses_without_confirm`), and the message names the component/function. Run → PASS. Mutation: drop the `confirm` guard → the refusal test fails.

- [ ] **Step 5: Add the end-to-end wire test.** Over `spawn_mock_gateway` with the real fixture SGBD (the `#[ignore]` BYO-data pattern used by `run_job_reads_named_results_over_the_read_only_gate` — or a synthetic in-tree SGBD if one exists), assert `run_service_function` on a known actuation sends the Main job then the Reset job, in that order, and that a write SID actually leaves the tester (proving `confirmed_write` admits it where `read_only` would refuse). If no in-tree SGBD supports a `STEUERN_*` job, assert at minimum via the `ConfirmedWriteBridge` unit that a `0x2E/0x2F/0x31` frame passes the confirmed-write gate and is refused by the read-only gate — reuse the gate tests' shape. State which you did in the report.

- [ ] **Step 6: Update `list_service_functions` note.** It currently says "does not execute service functions yet". Change it to point at `run_service_function` and keep `confirmed_write_eligible` meaningful (or rename if it now misleads). Run the full suite; check wall time did not balloon.

- [ ] **Step 7: Run all three gates and commit.**

```bash
git add mcp/src/server.rs mcp/src/dto.rs mcp/tests/integration.rs
git commit -m "feat(mcp): run_service_function + stop_service behind ConfirmedWrite

klartext can now WRITE: it runs an ECU service function as ISTA's function-keyed
phase cycle through the BEST/2 VM under GatedExchange::confirmed_write — the
transmit seam that admits 0x2E/0x2F/0x31 only for a caller holding the human's
confirm. Held functions stay actuated until stop_service or disconnect (ruling 2);
a failed cycle still tears down (ruling 1). Closes the read-only-only limitation.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>
Claude-Session: https://claude.ai/code/session_01SeoB5T7GkuBQXFJHEySpgq"
```

---

## Out of scope (explicit)

- **The P2.1 supplier-store clear** is not re-plumbed here, but Task 7's `ConfirmedWriteBridge` is exactly the mechanism it needs. A follow-up wires `supplier_clear_jobs` through the same bridge; note it in the P2.1 audit entry, do not build it in this plan.
- **Security access `0x27`** — not required for service functions (research B); not implemented.
- **Coding/flashing** — never.
- **Per-actuation firmware self-termination** — unreadable (research); klartext's safe teardown (ruling 1) is the mitigation.

## Self-Review

- **Spec coverage:** D1 → Task 2; D2 → Task 1; D3 → Task 3; D4 → Tasks 4+5; D5 → Task 6; D6 → Task 7. Rulings 1/2/3 → Tasks 4-5/5/6. Multi-job (54) and multi-rank (519) → Task 3. Hold source → Tasks 1+4. `0x27` not needed → confirmed, no task. ✅
- **Type consistency:** `Invocation` gains `job`/`rank` in Task 3 and every later task uses them; `run_service`/`run_cycle` lose `job:&str` in Task 4 and lose `reader`/`category` in Task 6 — Task 7 calls the final 5-arg `run_service(runner, function_id, target, invocations, hold)`. `Hold`/`hold_for`/`stop_service`/`FixedFunction`/`job_parameters_for_function` names are stable across tasks. ✅
- **Ambiguity:** the operator-text SQL in Task 1 is the one place needing schema verification; Step 1 instructs the implementer to confirm column names and gives a ship-NULL fallback so it never blocks the load-bearing hold params. ✅
