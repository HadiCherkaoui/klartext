# M8 — Service-Function Engine + Guided Layer Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Complete the service-function engine — derive execution frames for all *offline-derivable* DDE service functions (not just CBS), surface the full catalog to Claude via ONE read-only MCP tool, and encode the safe discover→recommend→human-executes workflow as a skill.

**Architecture:** Build on M1–M7 (do not rewrite). `klartext-semantic::service_function` gains a per-function `Derivation` status (`Derived{request,cite}` vs `NotDerivable{reason}`) and a small curated registry of disassembly-derived standalone reset frames; `klartext-client` gains a generic low-risk single-shot executor; the CLI runs LOW+Derived behind `--confirm` and refuses HIGH; the MCP server gains a list-only `list_service_functions` tool. A new `skills/klartext-service/SKILL.md` teaches the workflow. MCP stays exactly read-only.

**Tech Stack:** Rust 2024, tokio, rmcp (MCP), rusqlite (unused here), the existing `klartext-sgbd` `.prg` parser. ediabasx = offline disassembly oracle only (scratch, never committed).

## Global Constraints (hold without exception)

- **MCP READ-ONLY.** The MCP server may LIST/DESCRIBE service functions; it must NEVER execute one. No run/actuate tool is added. Execution stays in the CLI behind explicit human `--confirm`. The exact-tool-surface test must still assert no mutating tool name appears.
- **Risk tiering enforced on execution.** LOW-risk (counter/adaptation/statistic resets, no physical actuation) is executable behind `--confirm`. HIGH-risk PHYSICAL actuation (STELLER, ABGLEICH — pumps/fans/throttle/glow/injectors/calibration) is refused/hard-gated and never surfaced as agent-runnable.
- **Every derived frame marked UNCONFIRMED / [verify against capture]** in code and in ALL output (CLI, MCP, skill). None are hardware-tested. This must be impossible to miss.
- **Never guess a frame.** Derive from disassembly with a citation, or mark `NotDerivable` with an honest reason. No un-cited execution bytes.
- **BYO-data.** Read `.prg`/DB read-only from `data/`; never commit their contents or the ediabasx source. F-series only.
- **Conventions:** thiserror in libs / anyhow at the binary; `cargo fmt` + `clippy --all-targets -D warnings` clean; conventional commits; run `cargo fmt` via Bash (the Edit hook uses an older rustfmt — see memory `rustfmt-hook-mismatch`).

## Derived frames (from `docs/superpowers/…` scratch derivation; d72n47a0, ediabasx oracle)

DERIVED = disassembly-cited, UNCONFIRMED. See the derivation note; citations reproduced in `docs/service-functions-findings.md` §12a.

| Function | Job | Frame | Risk | Status |
|---|---|---|---|---|
| CBS reset (×counter) | `CBS_RESET` | `2E 10 01 01 <id> 64 1F 80 00 0F FF 0F 3F FF 00` | LOW | Derived (M7) |
| MSA2 history reset | `STEUERN_MSA2HISTORIERESET` | `2E 5F 84` | LOW | Derived (M8) |
| PM histogram reset | `STEUERN_PM_HISTOGRAM_RESET` | `2E 5F F5 04` | LOW | Derived (M8) |
| DAROL load-data reset | `STEUERN_DAROL_RESET` | `2E 62 00 01` | LOW | Derived (M8) |
| LLKETA reset | `STEUERN_LLKETA_RESET` | `31 01 F0 65` | LOW | Derived (M8) |
| Learned-value resets (18) | `LERNWERTE_RUECKSETZEN` | — | LOW | **NotDerivable**: read-modify-write (reads `22 5F D3`, write payload computed from the live response), LID-width branching, per-LID special cases → needs a capture or a BEST2 interpreter |
| Actuators (45) | `STEUERN_SELECTIV` | — | HIGH | NotDerivable + refused (0x2F IO-control, value scaling) |
| Calibrations (85) | `ABGLEICH_PROGRAMMIEREN_*` | — | HIGH | NotDerivable + refused (external part codes) |

## File Structure

- `crates/semantic/src/service_function.rs` — add `Derivation`, the derived-frame registry (the 4 statistic resets + CBS), attach status to every discovered function, generalize `ServiceFunction`. Owns discovery + derivation.
- `crates/client/src/client.rs` — add `run_service_reset(request)`: the generic LOW-risk single-shot executor (extended session → send derived request → return positive-response bytes). CBS keeps its write+read-back path.
- `cli/src/main.rs` — `service list` prints derivation status; `service run` runs LOW+Derived (incl. the 4 new resets) behind `--confirm`, reports LOW+NotDerivable honestly, refuses HIGH.
- `mcp/src/dto.rs` — `ListServiceFunctionsRequest` / `…Result` + `ServiceFunctionInfo`.
- `mcp/src/server.rs` — `list_service_functions` tool (list-only, offline via `--sgbd-dir`).
- `mcp/tests/integration.rs` — surface test becomes 6 tools; assert no run tool; add a list test.
- `skills/klartext-service/SKILL.md` — the workflow skill (knowledge only).
- `docs/service-functions-findings.md` — §12a M8 derivation record with citations.

---

### Task A1: `Derivation` status + derived-frame registry in `klartext-semantic`

**Files:** Modify `crates/semantic/src/service_function.rs`, `crates/semantic/src/lib.rs`.

**Interfaces produced:**
- `enum Derivation { Derived { request: Vec<u8>, cite: &'static str }, NotDerivable { reason: &'static str } }`
- `ServiceFunction` gains `derivation: Derivation`; methods `is_derived()`, `request() -> Option<&[u8]>`, `citation() -> Option<&str>`, `status_label() -> &'static str`.
- `Category::StatisticReset` (risk LOW).
- `ServiceFunctions::from_tables` attaches derivation per row; plus injects the curated standalone `DERIVED_RESETS`.

- [ ] **Step 1 (test):** `statistic_reset_is_low_risk_and_derived` — a discovered `STEUERN_MSA2HISTORIERESET` has `Category::StatisticReset`, `Risk::Low`, `is_derived()`, `request()==[0x2E,0x5F,0x84]`, citation contains `128A75`.
- [ ] **Step 2 (test):** `cbs_entry_is_derived_with_frame` — engine-oil CBS row → `is_derived()`, `request()` == the 15-byte frame.
- [ ] **Step 3 (test):** `learned_value_reset_is_low_but_not_derivable` — an `LERNWERTE_RUECK` row → `Risk::Low`, `!is_derived()`, `request()==None`, reason mentions "read-modify-write".
- [ ] **Step 4 (test):** `actuator_is_high_and_not_derivable` — a `STELLER` row → `Risk::High`, `!is_derived()`.
- [ ] **Step 5:** run tests → fail to compile (new API).
- [ ] **Step 6 (impl):** add `Derivation`, `Category::StatisticReset`, the `DERIVED_RESETS` const registry (4 entries, each with fixed frame + cite), wire `parse_cbs`→Derived, `parse_labelled`(LERNWERTE→NotDerivable{rmw}, STELLER/ABGLEICH→NotDerivable{high}), inject `DERIVED_RESETS` in `from_tables`. Keep degrade-quietly.
- [ ] **Step 7:** run tests → pass. `cargo fmt` (Bash), `clippy -D warnings`.
- [ ] **Step 8:** commit `feat(semantic): per-function derivation status + disassembly-derived reset frames (M8)`.

### Task A2: generic LOW-risk executor + CLI risk-tiered wiring

**Files:** Modify `crates/client/src/client.rs`, `cli/src/main.rs`.

**Interfaces produced:** `DiagnosticClient::run_service_reset(&mut self, request: &[u8]) -> Result<Vec<u8>, ClientError>` — enter extended session, send `request`, return the positive-response bytes.

- [ ] **Step 1 (test):** in `client.rs`, loopback mock accepts `10 03` then a derived reset (`2E 5F 84` → `6E 5F 84`, or `31 01 F0 65` → `71 01 F0 65`); assert `run_service_reset` returns the echo.
- [ ] **Step 2:** run → fail.
- [ ] **Step 3 (impl):** add `run_service_reset` (mirrors `reset_cbs` without the read-back; single-shot, no actuation bracket needed — these don't latch a component).
- [ ] **Step 4:** run → pass.
- [ ] **Step 5 (impl, CLI):** `run_service_run` — HIGH → refuse (unchanged); LOW+Derived → `--confirm`-gated, call `run_service_reset` (CBS keeps `reset_cbs`); LOW+NotDerivable → honest "listed, frame not derivable offline" message. Print the UNCONFIRMED/[verify against capture] banner on every run/list.
- [ ] **Step 6 (impl, CLI):** `print_service_functions` shows `[derived — UNCONFIRMED]` / `[frame not derivable offline]` per function.
- [ ] **Step 7:** `cargo fmt`, `clippy -D warnings`, `cargo test`.
- [ ] **Step 8:** commit `feat(client,cli): execute disassembly-derived low-risk resets behind --confirm (M8)`.

### Task B: read-only `list_service_functions` MCP tool

**Files:** Modify `mcp/src/dto.rs`, `mcp/src/server.rs`, `mcp/tests/integration.rs`.

**Interfaces produced:** DTOs `ListServiceFunctionsRequest { variant, ecu?, risk? }`, `ServiceFunctionInfo { label, name, category, risk, derivation_status, confirmed, executable_via }`, `ListServiceFunctionsResult { functions, count, note, source_variant }`.

- [ ] **Step 1 (test):** `advertises_exactly_the_six_read_only_tools` — surface == connect/disconnect/list_ecus/read_data/read_faults/**list_service_functions**; forbidden run/write/actuate/clear names still absent.
- [ ] **Step 2 (test, ignored/BYO):** `list_service_functions_lists_catalog` — with `--sgbd-dir`, `variant="d72n47a0"` returns >100 functions, each with a risk + derivation status; a CBS entry is `derived`+low; a STELLER entry is high; optional `risk="low"` filter drops highs.
- [ ] **Step 3:** run → fail.
- [ ] **Step 4 (impl):** add DTOs + the `#[tool]` (list-only; loads the SGBD via the same guarded `variant`/`--sgbd-dir` path as `read_data`; NO connection, NO execution). Description tells the AI caller: read-only, derived-unconfirmed, LOW runs in the CLI behind --confirm, HIGH is human-only.
- [ ] **Step 5:** run → pass.
- [ ] **Step 6:** `cargo fmt`, `clippy -D warnings`, `cargo test`.
- [ ] **Step 7:** commit `feat(mcp): read-only list_service_functions tool (M8 Part B)`.

### Task C: `skills/klartext-service/SKILL.md`

**Files:** Create `skills/klartext-service/SKILL.md`.

- [ ] **Step 1:** write the skill: discover via `list_service_functions`; interpret risk + derived-unconfirmed; LOW-risk → explain + hand the exact `klartext service run <label> --confirm --sgbd <ecu>.prg` for the USER to run (Claude never executes); HIGH-risk → advisory only, no casual run command, human-only; always surface disassembly-derived/never-hardware-confirmed and "test LOW first, watch behaviour". Frame as guided-advisor; grants no execution.
- [ ] **Step 2:** verify frontmatter (`name`, `description`) + no execution claims.
- [ ] **Step 3:** commit `docs(skill): klartext-service guided service-function workflow (M8 Part C)`.

### Task D: docs, verify checklist, review

**Files:** Modify `docs/service-functions-findings.md`.

- [ ] **Step 1:** add §12a — the M8 derivation record (the derived-frames table + citations + the honest NotDerivable reasons).
- [ ] **Step 2:** full verify: `cargo build`, `clippy --all-targets -D warnings`, `cargo fmt --check`, `cargo test` (+ note the `#[ignore]` BYO tests). Confirm no DB/.prg/ediabasx committed (`git status`, `git diff --stat`).
- [ ] **Step 3:** superpowers:requesting-code-review; address findings.
- [ ] **Step 4:** commit `docs(service-functions): record M8 derivation + status model`.
