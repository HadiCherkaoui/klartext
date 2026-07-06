# BEST/2 VM + EDIABAS job engine — design

**Status:** design, approved-in-shape (2026-07-05). Supersedes the "later milestone"
placeholder in `docs/sgbd-findings.md` §7 option (1). First implementation plan is
**Phase 1** (offline VM core); see §9.

## 0. Decision record (why this, with eyes open)

Before committing, we verified against the real `.prg` catalog (1405 files) what
actually needs a bytecode interpreter on the owner's F20. Findings (real
`klartext-sgbd` parser, not a token proxy):

- **Single-value numeric scaling is already served.** The engine DDE `d72n47a0`
  (1787 rows, all numeric) is fully handled by `klartext-semantic::measurement`
  today. It is the one fitted ECU that scales *directly* in `SG_FUNKTIONEN`.
- **The rest of the car decodes through a table layer we do not read yet — and it is
  tables, not bytecode.** Every other F20 ECU (brakes `dsc_10`, steering `eps_20`,
  cluster `komb01`, climate `ihka20`, body `frm3`, access `cas4_2`, infotainment
  `cic`) routes most measurements through a per-DID `RES_*` table: multi-result
  decomposition, `MASKE` bitfields, per-field scaling, units, descriptions. These
  tables resolve 100% within the `.prg` under a **case-insensitive** lookup
  (`RES_0x5001` referenced, `RES_0X5001` stored — see the `sgbd-table-name-casing`
  finding).
- **The BEST VM's *irreducible* scope is narrow for this car.** After the tables cover
  the above, what strictly requires the interpreter is: the **inline-scaling tail**
  (ECUs with *no* `SG_FUNKTIONEN` — `dsc_56`, `eps_56`, E-series DDEs), **none of
  which are on the F20**; any **residual job logic** the tables cannot express
  (bounded, unknown-small on the F20); and **live job *execution*** (Item 5).

The lower-risk alternative — a native-Rust "table response-decoder" extending
`measurement.rs` (`RES_` walk + `MASKE` + multi-result) — was offered and **not**
chosen. The owner chose the **full BEST VM** as the strategic foundation: it is the
real Item-5 execution engine, the only route to the project's stated "works across all
supported BMWs" goal (the inline tail), and it subsumes the table decoder (the VM
performs the `RES_` walk by *running the generic job's bytecode* rather than by
re-implementing that walk in Rust). This spec records that the near-term F20 read
payoff overlaps the table path; the VM is justified by execution + generality, not by
beating the table decoder on F20 reads.

## 1. What this is

A new library crate **`klartext-best`** that decodes and interprets BMW BEST/2
bytecode to execute a single named EDIABAS *job* end-to-end: build the UDS request(s),
exchange them with an ECU, parse the response into named, typed, scaled results — the
`ecuKom.apiJob(ecu, JOB, …)` primitive that ISTA's own diagnostics run on. It reuses
the existing `klartext-sgbd` container reader for the job bytecode and tables, and the
existing `klartext-client` session for the wire exchange.

## 2. Scope

**In:**
- Decode the BEST/2 instruction stream of a job (opcodes, addressing modes).
- Execute one named job: request build (comm opcodes) → UDS exchange → response parse
  → result set, including the generic measurement path
  (`SG_FUNKTIONEN` + `RES_*` tables, `MASKE` bitfields, multi-result).
- Two exchange backends: a **mock** (offline, canned responses) and a
  **`Session`-backed** live one over HSFZ.
- Reads run freely; **writes/actuation are gated** at the exchange (confirm + CLI-only
  + read-back), per the M8/M10 rule.

**Out (this milestone):**
- Guided multi-step **procedures / ABL flow orchestration** (ISTA FLOWXML/diagnosis
  tree). That is a *separate later milestone* built on this engine — different input
  format, different design.
- ECU **flashing / transfer** services `0x34–0x37` — out of the project entirely; the
  gate refuses them regardless.
- Full `.grp` **group→variant flow dispatch** beyond resolving to a known variant
  `.prg` (add IDENT-driven dispatch in Phase 2 only if a target job needs it).

## 3. Background: format, instruction set, references

The container format is already handled by `klartext-sgbd` (magic, XOR-`0xF7` body
from `0xA0`, `0x84` table directory, `0x88` job directory). Two facts this milestone
adds on top, both established in `docs/sgbd-findings.md` §2–3 and cross-checked against
two open references:

- **Job bytecode** lives at the `u32` offset stored in each `0x44`-byte job-directory
  record (currently *discarded* by `sgbd::parse_jobs`). Encoding:
  `[opcode:1][addrModeByte:1][arg0…][arg1…]`, one flat **184-entry opcode table**
  (index = opcode byte, `0x00–0xB7`), the addr-mode byte packing two of **16
  addressing modes** (immediate 8/16/32 LE, register, indexed/indirect with length,
  string). Jumps take a PC-relative `Imm32`.
- **Machine model:** byte regs `B0–BF`, 16-bit `I0–IF`, 32-bit `L0–L7`, string/
  byte-buffer `S0–SF`, IEEE-754 double `F0–F7`; flags Z/S/C/V; separate call + data
  stacks. `S` registers double as byte buffers; indexed modes slice the response
  telegram out of an `S` reg. Result types are `ResultType` 0–10
  (`B/W/D/Q/C/I/L/LL/R/S/Y`); store ops `ergb/…/ergr/ergs/ergy`, set commit `enewset`,
  name filter `etag`.
- **Opcode inventory (sizing):** ~98 opcodes carry real logic (63 core: arith/logic/
  move/control-flow/flags/stack/result; 35 comm `x*`); ~48 secondary (float/string/
  param/byte-conv/**table**/config); ~13 no-ops. The F20 DDE uses 102 distinct
  opcodes; one measurement job uses 43.

**References (read as spec, never copied — AGPL rule):** `uholeschak/ediabaslib` (C#,
canonical `EdiabasNet.cs`) and `emdzej/ediabasx` (TypeScript, with per-opcode
`*.spec.ts` vectors). They are re-cloned to scratch during implementation, cited by
`file:line`, and reimplemented in Rust. Their test vectors inform our own — we
**generate/derive our own vectors** rather than copy theirs (license hygiene). Reading
for facts (opcode semantics, addressing modes) is unconstrained; copying code is not.

## 4. Architecture

New crate `crates/best` (pkg `klartext-best`), four internal layers:

1. **`decode`** — job bytecode → instruction stream (`Op { code, mode, arg0, arg1 }`).
   Pure, offline, exhaustively unit-tested. This is the disassembler equivalent.
2. **`machine`** — the register file, flags, and stacks (§3 model). Pure state + the
   16 addressing-mode resolvers (read/write an operand given a mode + the machine).
3. **`exec`** — the opcode dispatch loop: core, float, string, byte-conv, **table**
   (`tabset/tabseek/tabget` — the measurement path, §5), param (job args), and comm.
4. **`comm`** — the `x*` opcodes call a `UdsExchange` seam (§6).

**Public API (the whole point):**

```rust
let ecu = Ecu::load(&prg)?;                 // wraps a klartext_sgbd::Prg
let results = ecu.run_job("STATUS_MOTORTEMPERATUR", &args, &exchange).await?;
// results: a ResultSet — name -> typed value (R/S/Y/…), already scaled.
```

`run_job` is async because the comm opcodes await the exchange. `ResultSet` is the
EDIABAS named-result set; the semantic layer maps it to display values/units (it mostly
already carries them — scaling is done *by the bytecode*).

**Two seams into existing crates, both minimal:**

- **`klartext-sgbd`** gains a way to hand out a job's *raw bytecode bytes*
  (`Prg::job_bytecode(name) -> Option<&[u8]>`), plus the case-insensitive table lookup
  the `RES_` walk needs. sgbd stays the container reader; *decoding* stays in
  `klartext-best`.
- **`klartext-client`**: the VM talks through
  `trait UdsExchange { async fn request(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError>; }`
  — three participants day one (mock, `Session`-backed, gating decorator), so a trait
  is justified. This is **not** the speculative HSFZ/DoIP transport trait CLAUDE.md
  forbids; it is a UDS-exchange seam one level above transport, and it maps directly
  onto the existing `Session::request(target, uds) -> Vec<u8>`.

## 5. The measurement path (what the verification pinned down)

A `STATUS_*` measurement job, run by the VM, reproduces the generic EDIABAS idiom:

1. `tabset SG_FUNKTIONEN` → `tabseek` the requested ARG/ID → `tabget` its cells
   (id, service, data type, and — for structured measurements — a `RES_TABELLE`
   pointer).
2. Build the request from `SERVICE`/`ID` (static `22 <id>`, or the `2C` define + `22`
   read dynamic sequence already derived in `measurement.rs`).
3. `xsend`/exchange; capture the response in an `S` reg.
4. Decode the response: for a direct-scaled row, `raw·MUL/DIV+ADD`; for a structured
   one, walk the `RES_<did>` table — each row a sub-result with its own `DATENTYP`,
   `MASKE` (bitfield extraction), `MUL/DIV/ADD`, unit, and description — emitting one
   named result per row.

Because the VM *runs* this, it handles both the direct case (matching `measurement.rs`)
and the structured `RES_`/`MASKE`/multi-result case (which `measurement.rs` does not).

**Relationship to `measurement.rs`:** the VM becomes the general measurement path.
`measurement.rs`'s direct-scale (`scale`, `Measurements`) is retained through Phase 1
as an **independent oracle** (the engine's 1787 values must agree between the two
paths); its role after the VM ships — fast-path vs retire — is decided in Phase 2, not
now (no premature deletion).

## 6. Safety model — the gate *is* the exchange seam

Every byte the VM transmits funnels through `UdsExchange::request`, so the write-gate
lives there, not in trusting job names:

- Classify the outgoing frame by **SID**: reads (`0x22/0x19/0x2C-read/0x3E/0x10/0x1A`)
  pass; state-changers (`0x2E/0x31/0x14/0x2F`, and out-of-scope `0x34–0x37`) are writes.
- A `GatedExchange` decorator refuses to transmit a write unless it holds a
  write-authorization (`confirm = true`) and reads-back + records the prior state first
  (M8/M10). **MCP is always handed a read-only exchange; only the CLI can construct an
  authorizing one** — so "MCP read-first" holds by construction.
- Enforcement at the transmit boundary means a mislabeled `STATUS_*` job that actually
  writes is still caught — the VM-level guarantee. The `STATUS_`/`STEUERN_` name
  convention (`ediabas-job-class-convention`) survives only as a listing hint.

## 7. Non-negotiables

- **No degrade-to-raw inside the VM.** An unimplemented opcode or malformed decode is a
  *hard, loud error* — never a guess. A wrong scaling is worse than none. (The
  degrade-to-raw contract stays at the *semantic* boundary that *calls* the VM.)
- **Native Rust, ISTA-free at runtime.** References are an offline spec/oracle only.
- **BYO-data.** No `.prg`/table content committed; real-data tests are `#[ignore]`d.
- `cargo fmt` + `cargo clippy -D warnings` clean; conventional commits; thiserror in
  the lib, anyhow at binaries.

## 8. Testing

- **Per-opcode** unit tests with our own input→output vectors (decode + execute),
  organized by opcode class, mirroring the references' coverage without copying.
- **Whole-job offline** tests: feed known response bytes via the mock exchange, assert
  the `ResultSet`. `STATUS_MOTORTEMPERATUR` raw `0E 2F` → `89.96 °C` is the anchor.
- **Engine oracle cross-check:** for direct-scaled measurements, the VM's result must
  equal `measurement.rs` across the DDE's rows (two independent paths agree).
- **Structured-decode proof:** a `RES_`/`MASKE` multi-result job (e.g. an `frm3`/
  `eps_20` measurement) decodes to the expected named sub-results — the case
  `measurement.rs` cannot do.
- **Inline-tail proof:** a `dsc_56`-style job (no `SG_FUNKTIONEN`, inline mask/shift)
  scales correctly — the case only the VM can do.
- **Real-`.prg`** `#[ignore]`d tests (BYO). **Live** execution is the manual
  hardware-in-the-loop step the human runs; never claimed here.

## 9. Phasing (each a shippable increment; one plan each)

- **Phase 1 — offline VM core.** `decode` + `machine` + non-comm opcodes (core, float,
  string, byte-conv, table, param) + the **mock** exchange + `ResultSet`. Delivers
  offline execution of read jobs, gated by the engine oracle + a structured-decode
  proof. No live I/O, no write-gate yet. **This is the first implementation plan.**
- **Phase 2 — live reads.** Comm `x*` opcodes + the `Session`-backed exchange + (if
  needed) `.grp`→variant IDENT dispatch. Wire into CLI (`run-job`) and MCP
  (read-only). Manual HIL confirmation.
- **Phase 3 — gated writes.** `GatedExchange` + SID classifier + read-back; actuation
  (`STEUERN_*`) jobs runnable from the CLI with `confirm`, never from MCP.

## 10. Open items / spikes (resolve in the spec-to-plan transition)

- Re-clone + **license-check** `ediabaslib` / `ediabasx`; confirm read-as-spec is
  clean (it is — facts aren't copyrightable — but record the licenses).
- Confirm crate name `klartext-best` (vs `klartext-ediabas`).
- **Opcode-coverage order:** implement the ~43 a measurement job needs first, grow
  toward the ~98 live via the target jobs; fail-loud on the rest.
- The `RES_` **byte-offset mechanism**: within the VM this is just the generic job's
  sequential `S`-reg walk, but confirm no job relies on offset logic the decoder must
  special-case.
- `measurement.rs` fate (fast-path vs retire) — decide in Phase 2.

## 11. Phase 1 outcome + the oracle finding (2026-07-06)

Phase 1 shipped: `klartext-best` is a complete, faithfully-verified BEST/2 VM core —
decoder, machine (registers/flags/stacks), ~110 opcodes across arith/control/float/
string/byte-conv/table/result/param, the `atsp` stack op, the `UdsExchange`/`MockExchange`
abstraction, and `Ecu::run_job`. Every layer was reviewed byte-exact against `ediabaslib`
(GPLv3) / `ediabasx` (PolyForm-NC), read-as-facts, no code copied. 144 unit tests + 2
integration tests green; fmt/clippy/doc clean. A **non-ignored** end-to-end test drives a
hand-assembled job through `run_job` (`move`→`xsend`→`fix2flt`/`a2flt`/`fmul`/`fadd`→`ergr`)
and reproduces `raw·0.1−273.14 → 89.96` — proving the VM *can* run and scale a real job.

**But the engine oracle (§8) disproved its own premise, and this is the load-bearing
finding of the milestone.** Running the *real* F20 DDE `STATUS_MOTORTEMPERATUR` bytecode
(and surveying all 272 jobs in `d72n47a0.prg`, independently reproduced by review) shows:

- The measurement job is **raw-only**: 207 ops, it emits the raw response telegram
  (`ergy → _RESPONSE_1`) plus a status text, and returns. It contains **no**
  `fmul`/`fadd`/`a2flt`/`ergr` and never emits `STAT_MOTORTEMPERATUR_WERT`.
- Across all 272 DDE jobs: only 8 use `ergr`, exactly 1 uses `fmul`, **none** is a
  temperature job. No job hardcodes a `*MOTORTEMPERATUR*` result name.
- The `89.96` comes from **`klartext-semantic::measurement`** (the M6 `SG_FUNKTIONEN`
  table scaler, in Rust), whose `real_dde_scales_motor_temperature` test passes.

So **§4a of `docs/sgbd-findings.md` was wrong** — its "the job scales via `fmul`/`fadd`/
`ergr`" sketch mislabelled `atsp` (a data-stack peek) as "load table column 12". The DDE
bytecode does **not** scale; the Rust table path does. This corroborates the brainstorm's
own conclusion (§0) and the Task-11 finding that `atsp` is a stack op — the VM's value was
never F20 passive-read scaling.

**Blocker to running real *read* jobs to `eoj`:** the deferred `Operand::Indexed`
addressing mode (index a slice out of an `S` register, width from the counterpart operand,
big-endian) plus a few opcodes (`gettmr`/`settmr`/`clrt`/`wait`/`fix2hex`/`jt`). Even with
those, a real read job yields the **raw** `_RESPONSE_n` that `measurement.rs` already scales
— low near-term payoff. The `#[ignore]`d oracle now stands as a truthful tripwire asserting
this finding (not a fabricated `89.96`), and will flag if indexed addressing later lands.

## 12. Revised direction (supersedes §9's Phase 2/3 as written)

The finding re-points the roadmap, without invalidating the choice to build the VM (§0):
the VM was always for **job execution**, not passive read-scaling.

- **Passive live-data scaling stays in `klartext-semantic` (Rust, `SG_FUNKTIONEN`).** The VM
  does not duplicate it. `measurement.rs` is *not* retired (reverses the §10 open item).
- **The VM's real value is the step-by-step *service functions*** — guided actuation +
  measurement procedures (oil-level, DPF regen, EMF/steering calibration, bleeding): jobs
  that *command the car* and orchestrate multiple steps, which only a job engine can run.
  That is **Item 5** of the M11 roadmap, and it is the natural next milestone (its own
  brainstorm), rather than "Phase 2 live reads" as originally sequenced.
- **Multi-value results are a first-class future requirement (owner, 2026-07-06):** a read
  must be able to return **many named values, not a single scalar** — a structured
  `RES_`-table DID decodes into several sub-results, each with its own mask/type/scaling,
  and a service function returns a measurement set. `ResultSet` already models this (ordered
  named entries, multiple sets); the consumers (CLI/MCP/semantic overlay) must surface it as
  a multi-field result, and the structured-`RES_` decode itself (whether VM-bytecode or a
  Rust extension of `measurement.rs`) remains to be settled for the non-DDE F20 ECUs.
- **If offline `.prg` analysis is insufficient** to pin response layouts or the live
  exchange path, the fallback is a real capture — see the pcap step added to
  `docs/on-car-verification-protocol.md` (2026-07-06): run the MCP read protocol with
  `tcpdump` on, capturing a structured multi-result read (and a pure-read service function
  if one is exposed) to validate the VM's exchange + multi-value decode byte-for-byte.
- `Operand::Indexed` + the residual opcodes become a scoped task **only if/when** a target
  job (a service function) actually needs them — not built speculatively for raw reads.
