# M11 Item 5 — guided service procedures on the BEST/2 engine — design

**Date:** 2026-07-06 · **Status:** approved by owner (scope, MCP line, HIL cadence, approach,
and all three design sections) · **Builds on:** `2026-07-05-best2-vm-job-engine-design.md`
(Phase 1 VM) · **Supersedes in part:** that spec's §11 mechanism claim (see §1 here).

## 0. Decision record

Owner decisions taken during this brainstorm (2026-07-06):

- **Scope:** oil level + one gated write. Read side: the VM runs `STATUS_OELNIVEAU` and the
  generic `STATUS_LESEN` live, plus a guided oil-level flow. Write side: the SID-classified
  gate proven on **one** low-risk write — `LERNWERTE_RUECKSETZEN` (M8's honestly-not-derivable
  read-modify-write; W1). Standing principle, owner's words: **never hardcode, always derive
  dynamically** — the engine must be generic (any job, any ECU, driven by the SGBD data);
  writes are in-bounds when they replay **exactly what ISTA does** (run the real bytecode,
  preconditions and bracket included, behind confirmation).
- **MCP line:** VM-run reads surface on **MCP and CLI**. The M4 "no derived write is ever
  MCP-executable" absolute is **retired at the owner's direction** and replaced by tiered
  confirmation (§6): W1 writes agent-invokable behind explicit user confirmation; the gate is
  the human confirmation, not the terminal it arrives through. CLAUDE.md is updated in P3.
- **HIL cadence:** mid-milestone capture gate. Build offline → owner runs one on-car capture
  session (MCP protocol + pcap) validating the live exchange before the guided flow and the
  gated write are finalized → second short session for the flow + write.
- **Approach:** full-fidelity engine — one derive-by-execution path. Alternatives (a Rust
  `RES_` decoder for reads / a targets-only minimal VM) were rejected because the write jobs
  use the same 45k-op table-walking framework, so they collapse into full completion anyway
  while adding a duplicate decode path.

## 1. The probe findings (2026-07-06) — and the Phase-1 correction

A throwaway full-range decoder probe (`crates/best/examples/probe_res.rs`, deleted after this
spec) over the real F20 `.prg` files established, against the previous spec's §11:

- **The Phase-1 "raw-only" finding was a measurement artifact.** `decode_job` stops at the
  first `eoj`, so the Phase-1 oracle and its 272-job survey measured only each job's
  arg-validation stub. Full-range decode (bounded by the next job's directory offset):
  `STATUS_MOTORTEMPERATUR` is 1,253 ops / 37 table ops / 43 result-emits / 4 float ops /
  3 exchanges and references `SG_FUNKTIONEN` — it does scale in bytecode.
- **The generic framework jobs do the structured decode in bytecode.** `STATUS_LESEN`
  (27,615 ops), `STEUERN_IO` (47,191), `STEUERN_ROUTINE` (45,656) are **byte-identical across
  DDE / DSC / KOMBI / IHKA**; their literals name the mechanism outright ("SERVICE '22' NOT
  FOUND IN TABLE 'SG_FUNKTIONEN' COLUMN 'SERVICE'", column ref `RES_TABELLE`). Running them
  yields ISTA-parity structured, scaled, multi-named results on every F20 ECU. This resolves
  the open question from the previous spec's §12: the non-DDE `RES_` multi-result decode is
  the **VM path**, not a `measurement.rs` extension.
- **The owner's targets are concrete jobs:** `STATUS_OELNIVEAU` = 1,976 ops / 6 exchanges;
  `LERNWERTE_RUECKSETZEN` full-range = 1,289 ops / 2 exchanges (matches M8's read-modify-write
  disassembly — that "not derivable offline" class is exactly what executing bytecode against
  live responses solves); DSC bleeding (`DSC_ENTLUEFTUNG`) is a `SERVICE=31` `SG_FUNKTIONEN`
  row invoked through `STEUERN_ROUTINE` (out of scope to *run* this milestone; in scope as
  the shape the engine must generalize to).
- **The VM gap, sized:** full-range decode; `Operand::Indexed` execution (1,653–2,317 uses
  per generic job); an opcode tail of ~15 (`jt`/`jnt` + trap state, `gettmr`/`settmr`/`clrt`,
  `wait`, `fix2hex`/`fix2dez`, `enewset`/`etag`/`ergl`, `y42flt`/`y82flt`, `jpl`, `swap`);
  engine loop-bound and `wait`-as-async rework. `jtsr`/`ret` are **not** needed — the
  framework is inlined per job (no subroutine calls in any target job's histogram).
- **What survives:** `measurement.rs` is on-car-verified and stays — the DDE fast path and
  the differential oracle's reference (§8). The Phase-1 pivot ("the VM's value is job
  execution") was right; only the "bytecode never scales" mechanism claim was wrong.

**Correction propagation (P3 doc tasks):** add a correction note (§13) to
`2026-07-05-best2-vm-job-engine-design.md`; correct `docs/sgbd-findings.md` §4a/§5 (the
first-eoj artifact and what the generic jobs actually do). Memory already corrected.

## 2. What this is

Make `klartext-best` a real engine: run the actual, unmodified BEST/2 jobs — generic and
named — against the real car, gated at the transmit boundary, surfacing multi-named-value
results everywhere, and put the first guided procedure (oil level) plus the first gated
write (`LERNWERTE_RUECKSETZEN`) on top. This is M11 Item 5 (roadmap §4, "multi-step guided
procedures"), scoped to what is derivable and safe: **the orchestration is ours (authored,
auditable data); every wire byte is ISTA's own bytecode executing.** ABL (.NET) procedure
logic stays out entirely — scale, coupling, license, safety (services-findings §3d).

## 3. Scope

In:
- Engine completion (§5): full-range decode, `Indexed`, the ~15-opcode tail, async
  `wait`/timers, instruction-budget rework, job args, per-call ECU target.
- Live exchange (§6): `FnExchange` glue over `Session::request`, `GatedExchange` policies.
- Multi-value results end to end (§7): CLI tables, MCP JSON array-of-sets, `JOB_STATUS`.
- Guided flow runner + the oil-level flow (§7); operator text + `ACTIVATION_DURATION_MS`
  extraction into the semantic DB build.
- The one W1 write behind confirmation on both surfaces; CLAUDE.md invariant rewording (§6).
- Two owner car sessions (capture gate after P2; flow + write validation after P3).

Out (unchanged project law or explicitly deferred):
- Physical actuation (W2) **execution** — fan/throttle/rail tests, DPF regen, DSC bleed, all
  EMF. The gate design anticipates them; no W2 job is executable this milestone.
- ABL porting; ECU flashing (`0x34–0x37` refused under every policy, forever).
- Re-routing M8's derived-frame `service run` catalog through the VM (later milestone; the
  legacy path stays as-is).
- `.grp` group-file dispatch, `xrequf` streaming, `jtsr`/`ret` — until a target job needs them.

## 4. Architecture

No new crates. One new principle made structural: **derivation = execution of the SGBD's own
bytecode**; Rust never re-derives what a job already encodes.

```
 cli/  klartext job run|list, klartext flow run oil-level      mcp/  run_job tool
   └── FnExchange(Session::request) ──► GatedExchange(policy) ──► klartext-best Ecu::run_job
         (glue in each binary)             (SID gate, §6)             │
                                                                      ▼
   klartext-semantic ◄── differential oracle (§8) ──── ResultSet (multi-set named values)
   (measurement.rs unchanged; DB build adds operator texts)
```

- `klartext-best` grows: decoder fix, `Indexed`, opcode tail, engine rework, `FnExchange`,
  `GatedExchange`, the flow runner module (§7). It keeps depending only on `klartext-sgbd`.
- `klartext-client` is untouched (its `Session::request(target, uds)` already mirrors the
  `UdsExchange` seam; the binaries wrap it in `FnExchange` — no `client`↔`best` dependency).
- `klartext-semantic`: no decode-path changes; `build-semantic-db.sh` additionally extracts
  `XEP_ECUFIXEDFUNCTIONS` operator texts (`PREPARING/PROCESSING/POSTOPERATORTEXT`) and
  `ACTIVATION`/`ACTIVATION_DURATION_MS` for the functions flows reference.
- Binaries: CLI `job run <ecu> <job> [args…]` (`--confirm` unlocks W1), `job list <ecu>`,
  `flow run oil-level`; MCP `run_job(ecu, job, args?, confirm?)`.

## 5. Engine completion (the `klartext-best` work)

- **Full-range decode.** `Prg` gains each job's end offset (next directory entry's start;
  last job runs to the code region's end) so `job_bytecode` returns exactly one job.
  `decode_job` decodes the whole range; `eoj` becomes a normal instruction that ends
  *execution* — jump targets past an early-exit `eoj` then resolve. `BadPc` remains for
  genuinely wild jumps.
- **`Operand::Indexed` execution** — read and write: index a slice out of an `S`/string
  register, width from the counterpart operand, big-endian, with the `…Len…`/increment
  variants the decoder already models. Byte-verified against ediabaslib semantics (re-clone +
  license-record in the plan; facts only, never code).
- **Opcode tail (~15, fail-loud on the rest, unchanged):** `jt`/`jnt` + the trap state the
  comm/error ops set; timers `gettmr`/`settmr`/`clrt`; `wait`; `fix2hex`/`fix2dez`;
  result-family `enewset`/`etag`/`ergl`; float conversions `y42flt`/`y82flt`; `jpl`; `swap`.
  Exact semantics from ediabaslib (`EdOperations.cs`), own vectors per op.
- **Engine rework:**
  - `run_job` gains the ECU `target` (from the caller's SVT/scan resolution; the Phase-1
    `DEFAULT_TARGET = 0x12` constant is retired — never hardcode).
  - Job args: the raw ISTA semicolon-separated ASCII convention, pinned against ediabaslib
    (`pars`/`parw`/`parl` consume it); `run_job` keeps taking raw bytes, CLI/MCP join
    user-supplied args with `;`.
  - `wait`/timer semantics surface as an async `Flow` variant (tokio sleep at the run loop —
    never a busy spin inside `step`).
  - Instruction budget: replace the flat 100k bound with a budget sized for 27–47k-op jobs
    with table loops (order 1–10M, exact value justified in the plan) plus a wall-clock bound
    around `wait`-heavy jobs; both still kill runaways loudly.
  - `ExchangeError` gains a transport variant so live `Session` errors propagate with context.
- **No degrade-to-raw inside the VM** (unchanged): unknown opcode, decode fault, gate
  refusal, budget exhaustion = hard error with job + pc context.

## 6. Exchange & safety — the gate is the seam

`GatedExchange<E: UdsExchange>` wraps any exchange and classifies every outgoing frame by SID
**at the transmit boundary** (job-name conventions stay hints only):

- **Classes:** session plumbing `0x10`, `0x3E` and reads `0x22`, `0x2C`, `0x19` pass. Writes
  `0x2E`, `0x31`, `0x2F`, `0x14`, `0x27` are gated. `0x34–0x37` refuse under every policy.
- **Policies:**
  - `ReadOnly` (default on both surfaces): any gated SID → hard refusal carrying the exact
    bytes and job context.
  - `ConfirmedWrite` (CLI `--confirm` / MCP `confirm=true`): passes a gated frame **only**
    when the invoked function's risk tier is **W1** (non-physical, reversible), resolved from
    the M7/M8 semantic catalog. **Unclassified writes are never W1 — unknown → W2 → refuse.**
- **W2 (physical actuation):** not executable this milestone from either surface. Future
  ritual (sketch, so the invariant wording is already tier-accurate): per-step typed
  confirmation + operator-text precondition acknowledgment; MCP W2 needs its own designed
  confirmation flow before it ever exists.
- **Write ritual (project law):** pre-read/backup (the LERNWERTE job's own `22 5FD3` read is
  captured in the frame log; flows may add explicit read steps) → write → read-back verify →
  the Reset/return-control bracket in a `finally`. Every gated run persists the full TX/RX
  frame log; confirm surfaces echo exactly what was sent.
- **CLAUDE.md rewording (P3, owner-directed):** replace the M4 absolute ("…is ever
  MCP-executable…") with the tier ladder: reads free; W1 writes agent-invokable behind
  explicit user confirmation relayed from the human (the `clear_faults` posture, extended to
  VM-run W1 jobs, with read-back); W2 never autonomous anywhere — per-step human confirmation
  with acknowledged preconditions, CLI-only until a dedicated MCP confirmation flow is
  designed; flashing unsupported forever. Principle: **the gate is the human confirmation,
  not the terminal it arrives through.**

## 7. Flow runner, the oil-level flow, and result surfacing

- **Runner (in `klartext-best`, generic, knows nothing about oil):** a flow is data — ordered
  steps of `RunJob { ecu, job, args }` · `Prompt { text }` (operator text from the semantic
  DB where present) · `PollUntil { job, args, predicate over ResultSet, interval, timeout }` ·
  `Verify { job, predicate }` — plus an always-run `finally` step list (the Preset→Main→Reset
  bracket; runs on success, error, timeout, Ctrl-C), bounded by `ACTIVATION_DURATION_MS`
  where the catalog ships it. The runner executes steps through the same gated exchange as
  everything else; a flow declares its write steps up front so the human knows before
  starting.
- **Oil-level flow (first flow, read-only):** precondition reads (engine state per the
  catalog's operator text) → operator prompt (the ISTA text: engine running, level surface,
  wait) → `PollUntil` on `STATUS_OELNIVEAU` until the measurement reports stable →
  structured multi-value report. No write frames anywhere in the flow.
- **The gated write (not a flow):** `LERNWERTE_RUECKSETZEN` on a benign row (e.g. `IBSRE`)
  via `job run … --confirm` / MCP `confirm=true` — the bytecode does its own live
  read-modify-write; klartext adds the frame log + read-back.
- **Result surfacing (owner requirement (a)):** `ResultSet` is already multi-set/multi-entry.
  CLI prints every set as a table (name / typed value / unit when known). MCP returns JSON
  `[[{name, value, unit?}, …], …]` plus `JOB_STATUS` explicitly. Units come from the job when
  the framework emits them; else joined from `SG_FUNKTIONEN`/`RES_` rows only when the match
  is unambiguous; otherwise omitted — never guessed.

## 8. Testing

1. **Per-opcode vectors** (Phase-1 discipline): semantics read from ediabaslib, vectors our
   own, byte-faithful review per task.
2. **Decode sweep:** full-range decode of all 272 DDE jobs and the generic jobs on `dsc_10`/
   `komb01`/`ihka20` — zero decode errors tolerated.
3. **Differential oracle:** synthesized response bytes → VM-run `STATUS_LESEN` vs
   `measurement.rs` on the same bytes must emit identical name/value pairs (sampled across
   the engine's direct-scale rows). Disagreement stops the line.
4. **Structured multi-result proof:** `STATUS_LESEN` against a mocked `dsc_10` multi-row
   `RES_` DID asserts several named sub-results with masks/scaling.
5. **Gate tests:** `ReadOnly` kills a `STEUERN_ROUTINE` start at the seam; unknown-tier
   writes refuse; `0x34–0x37` refuse everywhere; the MCP surface test learns `run_job` +
   confirm semantics (tool inventory + behavioral, against a frame-recording mock).
6. **Run-to-eoj:** the `#[ignore]`'d oracle tripwire un-ignores; real DDE jobs (incl.
   `STATUS_MOTORTEMPERATUR`, `STATUS_OELNIVEAU`) run to `eoj` offline against mocks.
7. **LERNWERTE rehearsal:** mock the `22 5FD3` read → assert the job computes exactly the
   `2E 5F 8A` write M8's disassembly derived.

On-car remains the owner's manual step; unit tests never claim a hardware round-trip.

## 9. Phasing (each shippable) and HIL gates

- **P1 — engine completion, offline.** §5 complete; tests §8.1–4, 6–7 green. `job run`
  works against mocks.
- **P2 — live read path.** `FnExchange`, `GatedExchange` (`ReadOnly`), CLI `job run/list`,
  MCP `run_job` read-only, multi-value surfacing. → **Car session 1 (owner, ~20 min):**
  on-car protocol + pcap — VM reads on 2–3 ECUs + `STATUS_OELNIVEAU`; byte-for-byte
  verification flips the `[verify against capture]` markers before P3 builds on top.
- **P3 — guided + gated.** Flow runner + oil-level flow; `ConfirmedWrite` + LERNWERTE on
  both surfaces; CLAUDE.md rewording; §1 correction propagation; probe deleted. →
  **Car session 2:** guided oil flow on the car; LERNWERTE reset with read-back.

## 10. Open items (resolve in the plan phase, not blockers)

- Re-clone `ediabaslib`/`ediabasx` into session scratch; record licenses (GPLv3 /
  PolyForm-NC — read-as-facts, reimplement, never copy; AGPL rule).
- Pin the exact job-args buffer encoding (semicolon ASCII; how `pars`/`parw`/`parl` index)
  against ediabaslib before implementing.
- Pin `wait`/timer tick semantics (ms? tick source?) against ediabaslib.
- Pick and justify the instruction/wall-clock budget values (§5).
- Exact oil-level stability predicate (which `STATUS_OELNIVEAU` results indicate "stable")
  — read from the job's result names + RES_ rows during P1; flag `[verify against capture]`
  until session 1.
- Whether the semantic DB build already carries any operator text (extend vs add table).
- MCP `run_job` result-size guardrails (a job can emit hundreds of results; decide truncation
  that is never silent, mirroring `list_measurements`).
