# P3 — the service write tier: generalized ECU service functions

**Status:** approved design, 2026-07-18. Supersedes the narrower "guided oil-level flow"
framing of Item 5 P3 in `2026-07-06-item5-guided-service-procedures-design.md` §6.

## 1. Goal

Make klartext able to *do things* to the car, not just read it — every ECU's service
functions (resets, adaptations, **and physical actuation**), executed the way ISTA executes
them, with preconditions as the safety mechanism and MCP as the primary surface.

The oil-level reset is one instance of this engine, not the feature. The owner has performed
that procedure manually and understands it; it is a validation case, not the deliverable.

## 2. Decisions (and what they reverse)

| # | Decision | Rationale |
|---|---|---|
| D1 | **Actuation becomes MCP-executable**, behind per-call `confirm: true` | Reverses the previous "NO physical actuation is ever MCP-executable" invariant. The owner works on his own cars; ISTA actuates on a button press without further prompting. A blanket refusal was overcautious and blocked the actual use case. |
| D2 | **MCP + core are the primary surfaces; the CLI merely inherits** | The CLI cannot show live measurements while a component is actuating — precisely when they matter. Future clients are the mobile app and an agent, not a terminal. |
| D3 | **Consent = per-call `confirm: true`** | Uniform with the existing `clear_faults` / `clear_all_faults` pattern. Session-arming was considered and rejected as a second, inconsistent consent model. |
| D4 | **Execution runs ISTA's own EDIABAS job through the BEST/2 VM**, with arguments from the `job_param` catalog | Generalizes to every ECU with an SGBD at zero per-function cost (488 jobs / 941 variants already extracted). Hand-derived frames do not generalize and stay `[verify against capture]` forever. |
| D5 | **Hand-built frames survive only where no job exists** | e.g. `measurement.rs`'s `2C` dynamic-read sequence — ITOEL has no dedicated `STATUS_` job. This is a gap-filler, not a parallel architecture. |
| D6 | **Calibration is NOT specially banned** | It is a `Category` like any other, subject to the same confirm + preconditions. The previous categorical ban is dropped along with D1. |
| D7 | **Flashing (`0x34`–`0x37`) remains unsupported, permanently** | Unchanged. Out of project scope. |

### 2.1 The tier ladder (replaces the old invariant)

| Tier | Operations | Surface | Gate |
|---|---|---|---|
| Read | faults, freeze-frames, measurements, identity, info memory, `run_job` | MCP + CLI | none |
| Standard clear | `14 FF FF FF`, plus the new post-clear ECU reset | MCP + CLI | `confirm: true` |
| Service write | resets, adaptations, actuation, calibration | MCP + CLI | `confirm: true` + preconditions + Reset-on-failure |
| Flashing | `0x34`–`0x37` | — | never implemented |

`CLAUDE.md` is updated in the same commit to state this ladder.

## 3. Architecture

New library crate **`crates/service`** (`klartext-service`), orchestration only:

```
resolve      (klartext-semantic: job name + per-phase args from job_param)
  ↓
preconditions (reads via the injected exchange)
  ↓
Preset ─┐
Main    ├─ klartext-best VM  →  GatedExchange(Policy::ConfirmedWrite)
Reset  ─┘     (Reset also runs on error or abort)
  ↓
ServiceReport
```

**Dependency rule (preserved):** `klartext-service` depends on `klartext-semantic` and
`klartext-best`, and takes the exchange **injected**. It never references `klartext-client`
or HSFZ, so the existing rule — that `klartext-best` and `klartext-client` meet only in a
binary's `SessionBridge` — is unchanged. Binaries compose:

```rust
GatedExchange::confirmed_write(TelegramExchange::new(SessionBridge { client: &client }))
```

**Gate.** `klartext-best` gains `Policy::ConfirmedWrite` beside today's `ReadOnly`. Same
`peek_sid` transmit seam, already wire-proven; only the verdict changes, and only for a
confirmed call. `ReadOnly` remains the default for `run_job` and every read path — those
guarantees are untouched and their tests must keep passing unmodified.

## 4. Execution model — the phase cycle

`job_param` stores arguments **per phase**: `Main` (55,099 rows), `Preset` (1,230),
`Reset` (9,039). This is ISTA's own setup → act → teardown sequence.

1. **Preset** — if the catalog defines it, run first (prepares the ECU).
2. **Main** — the actuation/reset itself. `ACTIVATION_DURATION_MS` (from
   `XEP_ECUFIXEDFUNCTIONS`) gives its intended duration.
3. **Reset** — run on normal completion **and on any failure or abort**. This is what stops
   an actuation rather than leaving it running.

Each phase's argument buffer is the `;`-join of its `job_param` rows in `position` order —
byte-identical to what the `job args` CLI already prints.

## 4.1 Function identity and enumeration

Two catalogs already exist and must not be conflated:

- **`job_param` (ISTA catalog)** — the authority on *how to invoke*: job name + per-phase
  positional arguments, keyed by `(ecu_variant, job)`. This is what generalizes to 941
  variants and drives execution.
- **SGBD `ServiceFunctions`** (existing `crates/semantic/src/service_function.rs`) — the
  authority on *classification*: `Category` (CbsReset / LearnedValueReset / StatisticReset /
  ActuatorControl / Calibration) and `Risk`, derived from EDIABAS job-name convention.

The runner enumerates functions from the ISTA catalog (with its human titles) and asks
`ServiceFunctions` to classify each by job name, so a function has both an invocation and a
category. A function present in one catalog but not the other is still listed: an unclassified
function defaults to the strictest category defaults for preconditions, and an unlisted-in-ISTA
function falls back to its existing `Derivation::Derived` frame where one exists (D5).

## 4.2 Surface (new and changed)

**MCP** (additions to the current 16 tools):
- `run_service_function` — `{ecu, function, variant?, confirm, watch?}`. Executes the phase
  cycle. Refuses without `confirm: true`. Returns a `ServiceReport`: per-phase outcome,
  precondition results (passed / failed / unverified), any `watch` samples, and teardown status.
- `list_service_functions` (existing) — extended to return the ISTA title, category, risk,
  the precondition list that *would* be enforced, and the ISTA operator prose where present.
- `clear_faults` / `clear_all_faults` (existing) — gain `reset: bool` (default `true`).

**CLI** inherits the same runner; `service run` is rewired onto it, and `clear-faults` gains
`--no-reset`. No new CLI-only capability is introduced (D2).

`run_job` is **unchanged and stays `Policy::ReadOnly`** — it is not the write path.

## 5. Preconditions

ISTA's preconditions are **not** machine-readable: of the DDE's 87 fixed functions only 3
carry any preparing text, and it is prose ("Activation for 20 seconds (up to maximum 90
degrees Celsius engine temperature)"); `ACTIVATION` is a bare boolean (307 of 35,555 rows
DB-wide). klartext therefore defines its own.

```rust
enum Precondition {
    EngineRunning,
    EngineOff,
    TerminalOn,
    BatteryAbove(f64),   // volts
    CoolantBelow(f64),   // °C
    VehicleStationary,
}
```

**Attachment, two layers:**
- **Category defaults** — e.g. every `ActuatorControl` requires `TerminalOn` +
  `BatteryAbove(12.0)`. Covers all 941 variants with no curation.
- **Override table** — a small per-function map for the cases ISTA documents in prose.

**Resolution source.** Preconditions read the **vehicle**, not the target ECU: engine state
comes from the engine ECU (DDE) whatever is being actuated. The runner resolves the engine
address from the ECU tree / catalog.

**Unresolvable checks degrade to advisory.** If a precondition's measurement cannot be read
(no SGBD for the engine, unknown variant, ECU asleep), it is reported as unverified and does
**not** block. The human already confirmed; klartext must not refuse merely because it could
not look something up. Only a check that resolves *and fails* blocks.

**ISTA prose is surfaced verbatim.** The `PREPARING/PROCESSING/POST` operator texts are
extracted into the catalog (an additive column set on the existing `job_param` extraction in
`scripts/build-semantic-db.sh`) and returned with the function, unparsed, as human guidance.

## 6. Clear + ECU reset (folded in)

klartext has **no `0x11` ECUReset today** — the entire service is absent from `crates/uds`.
This is the root cause of the observed difference: ISTA's clear-all reboots the instrument
cluster; klartext's `10 03` + `14 FF FF FF` does not.

- Add `0x11` ECUReset to `crates/uds`: `hardReset 0x01`, `keyOffOnReset 0x02`, `softReset 0x03`.
- A confirmed clear then resets, **default on** (ISTA parity), switchable off per call.
- **Order: clear everything first, then reset.** Resetting mid-sweep would drop ECUs still
  being cleared.
- **The gateway (`0x10`) is excluded — always.** Resetting it kills our own session. The
  "reset it last on explicit request, then reconnect" path in the original draft was
  deliberately NOT built: categorically unresettable is the safer posture, and no need for
  it has appeared. Recorded as an accepted deviation, not an oversight.
- **Sub-function RESOLVED 2026-07-18 — hardReset (`0x01`), SGBD-confirmed offline.** Decoding
  BMW's own `STEUERGERAETE_RESET` job shows exactly one UDS request literal, `11 01`, in both
  the DDE (`d72n47a0`) and the gateway (`zgw_01`) bytecode, with zero `11 02`/`11 03` in
  either. This supersedes the original `[verify against capture]` marker and matters because
  the owner no longer has a working ISTA VM: the question was settled from the `.prg` files
  on disk rather than by capturing a live ISTA session. Still not wire-observed.

## 7. Live data during actuation

The run call accepts `watch: [measurement names]`. The runner samples those measurements
across the `Main` phase — window sized from `ACTIVATION_DURATION_MS`, capped at 20 samples —
and returns the series in the report. MCP is request/response, so a bounded sampled series is
the honest shape for "watch it while it runs"; no streaming channel is invented.

## 8. Error handling

| Situation | Behaviour |
|---|---|
| Blocking precondition fails | Refuse. **Zero frames on the wire.** Report the failing check *and its measured value*. |
| Precondition unresolvable | Proceed; report as unverified. |
| ECU negative response (NRC) | Decode and surface; run the `Reset` phase. |
| Transport error / timeout during `Main` | Run `Reset` best-effort; report **both** the original error and whether teardown succeeded. |
| Any outcome | The report always states teardown status — an actuation is never left running silently. |

## 9. Testing

**Unit**
- Phase sequencing: `Preset → Main → Reset` in order; `Reset` runs on failure paths.
- Precondition evaluation, including unresolvable → advisory (not blocking).
- Gate matrix: `ReadOnly` still refuses every write SID; `ConfirmedWrite` admits a write only
  with `confirm`, and still refuses flashing SIDs.
- Argument-buffer construction matches `job args` output byte-for-byte.

**Behavioural (mock exchange)**
- Assert the exact telegrams emitted for a full `Preset→Main→Reset` cycle.
- Assert a blocked precondition emits **zero** frames — mirroring the existing
  `run_job_gate_refuses_a_write_before_the_wire` test.
- Assert the post-clear reset ordering, and that `0x10` is excluded by default.

**Differential oracle**
- Extended to assert the **emitted request**, not only the decoded response — the defect P2.1
  exposed (the VM emitted `22 4517` where the correct frame was the `2C` define sequence).

**On-car (manual, owner-run)**
- Start with a harmless actuation (e.g. the electric fan, `STEUERN_E_LUEFTER`, one documented
  `Main` argument `90`), with a pcap.
- Then the clear + ECU reset, confirming the cluster reboots as ISTA's does.
- Never claimed working from unit tests alone.

## 9.1 Staging note

This spec is deliberately one milestone but is large enough to stage. A sensible split for
the implementation plan, each independently testable:

1. `Policy::ConfirmedWrite` + the gate matrix tests (no behaviour change yet).
2. `0x11` ECUReset in `crates/uds` + clear-then-reset ordering, gateway excluded (delivers
   the visible ISTA-parity win on its own).
3. `crates/service`: phase runner + `ServiceReport`, mock-exchange behavioural tests.
4. Preconditions (category defaults, overrides, advisory degradation) + operator-text
   extraction.
5. Surface wiring: MCP `run_service_function`, extended `list_service_functions`, `watch`
   sampling; CLI rewired.

## 10. Out of scope (explicit)

- **Service-book / service-history writing** — sub-project C, blocked on a research spike.
  The job catalog has no `%SERVICE%`/`%HISTOR%`/`%WARTUNG%` write job; the owner supplied
  `data/BMW-HU-ServiceManager.zip` (gitignored) as the lead. Separate spec.
- **Guided multi-step procedures (ABL interpreter)** — sub-project D, needs this write tier
  plus the diagnosis/symptom catalog. Separate spec.
- **Replay-coding**, **DoIP**, **flashing** — unchanged scope decisions.
