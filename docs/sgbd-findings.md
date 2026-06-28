# SGBD / BEST2 path — Milestone 6, Phase 1 findings

Read-only exploration of the EDIABAS SGBD (`.prg`) format and the BEST/2 bytecode, to
scope **native proprietary-measurement scaling** for klartext and recommend an
architecture. **This is a findings doc. Nothing was built. Halt for a decision.**

The proprietary scaling that M3 (`docs/sqlite-findings.md`) and M5 (`docs/standard-pids.md`)
both deferred — turning a raw BMW measurement response into a physical value — lives in the
compiled SGBD, not in the ISTA SQLiteDB. This doc establishes exactly *how*, with a real
worked example off the user's own car data, and sizes each way of getting it into Rust.

## TL;DR — recommendation

The three architectures in the brief are **(1)** port a BEST2 interpreter, **(2)** hand-port
chosen scalings, **(3)** delegate to ediabasx/ediabaslib as a subprocess. The evidence points
to a **fourth, better option the brief didn't name:**

> **(4) Extract the SGBD measurement *tables* and apply a generic `raw × MUL / DIV + ADD`
> scaler in native Rust.** BMW's modern SGBDs don't bury scaling in per-measurement bytecode —
> they put it in a structured data table (`SG_FUNKTIONEN`) that a *generic* bytecode job reads.
> That table is plain data we can parse without running any VM. This gives **broad, whole-fleet
> coverage for a fraction of a full interpreter's effort, stays native Rust, stays ISTA-free**,
> and drops straight into the existing degrade-to-raw scaling pattern in `crates/semantic`.

Reserve **(1)** the full interpreter as a later milestone for the **inline-scaling tail**
(older / some supplier ECUs like the TRW DSC, which compute scaling in bytecode and have no
table). Use **(3)** ediabasx only as an **offline oracle** (one-time table extraction +
test-vector generation), never as a shipped runtime dependency. **(2)** is not recommended —
it doesn't generalise to "all supported BMWs," which is the actual goal.

Coverage/effort/fit at a glance (detail in [§7](#7-architecture-options-evaluated)):

| Approach | Native Rust | ISTA-free | Fleet coverage | Effort |
|---|---|---|---|---|
| 1 Full BEST2 interpreter | ✅ | ✅ | 100% (table + inline + actuation) | **Largest** (~98 live opcodes + live-I/O VM) |
| 2 Hand-port chosen scalings | ✅ | ✅ | only the hand-picked set | low/item, **unbounded** for the fleet |
| 3 Subprocess ediabasx/ediabaslib | ❌ Node/.NET dep | ✅ | 100% | lowest Rust work |
| **4 Table extract + Rust scaler** | ✅ | ✅ | **broad** (every `SG_FUNKTIONEN` ECU); raw fallback elsewhere | **moderate** |

## 1. Scope, method, and what is committed

- **Data (gitignored, BYO):** `data/Testmodule(1)/Ecu/*.prg` — a full EDIABAS SGBD catalogue
  (~1405 `.prg`) the user supplied. Plus the decrypted M3 extract `data/klartext-semantic.db`
  (`ecu(address,variant,group_name)`), used here only to map the engine ECU.
- **Reference tools (cloned to scratch, NOT committed):** `emdzej/ediabasx` (TypeScript port of
  EdiabasLib, with a `.prg` parser + disassembler) and `uholeschak/ediabaslib` (canonical C#).
  Studied for the format and instruction set; **no reference source is copied into this repo** —
  only `file:line` citations. ediabasx was built (`pnpm install && turbo build`) and run as a
  disassembler over the user's real `.prg` files.
- **Committed by this milestone:** *only this document.* It quotes the file format, a handful of
  BEST2 instructions, the measurement-table column names, and **one** scaling row — the same kind
  and amount of factual detail the M3/M5 docs already carry. No `.prg`/`.grp` blobs, no bulk table
  dumps, no VIN/PII.
- **Hardware-in-the-loop:** Claude cannot reach the car. Every byte/opcode/scaling fact below is
  from **offline static disassembly** of the SGBD plus the two references. The scaling *math* is
  verifiable offline (shown). Confirming a live raw value → physical value on the car is a
  **manual step for later**, not claimed here.

## 2. The `.prg` / BEST2 container format

Cross-validated against both references (they agree). Citations: ediabaslib `EdiabasNet.cs`,
ediabasx `packages/best-parser/src/parser.ts` + `disassembler.ts`.

- **Signature:** 16-byte `@EDIABAS OBJECT\0` at `0x00`; `u32` file-type at `0x10` (0 = `.grp`
  group, 1 = `.prg` variant).
- **Obfuscation = single-byte XOR `0xF7`.** Bytes `[0x00, 0xA0)` are **plaintext** (signature +
  the little-endian header pointer table + the `.B2V` source-file name); bytes `[0xA0, EOF)` are
  XOR-`0xF7` (every job name, result name, text string, table cell, and instruction). No
  compression, no rolling key, **no encryption/license layer.** *(ediabaslib `EdiabasNet.cs:7165`
  `buffer[i] ^= 0xF7`; ediabasx `parser.ts:8` `EDIABAS_XOR_KEY=0xf7`.)* This is exactly why
  `strings file.prg` shows only `@EDIABAS OBJECT` and e.g. `D72N47A0.B2V` and nothing else — the
  rest is past `0xA0` and scrambled.
- **Header pointers (LE u32):** `0x84` → table-section offset, `0x88` → job-list offset
  (read raw, before de-XOR). *(ediabaslib `:4916/:4995`; ediabasx `parser.ts:138/172`.)*
- **Job table:** `u32` count, then **0x44 (68)-byte** records = 64-byte XOR'd name + `u32`
  bytecode offset. Bytecode is decoded on demand per job, not pre-extracted.
- **Table section:** `u32` count, then **0x50 (80)-byte** records = 64-byte name + column/row
  offsets+counts; **cells are null-terminated XOR'd strings**, header row first. *This is the part
  that matters most for us (see [§5](#5-the-pivot-scaling-is-table-driven-not-per-job-bytecode)).*
- **Text/comment block** (XOR'd, CP1252): `ECU:/ORIGIN:/REVISION:/AUTHOR:/ECUCOMMENT:` and
  per-job `JOBCOMMENT:/RESULT:/RESULTTYPE:/ARG:/…`.

A native Rust loader for this is **trivial** (XOR a buffer, follow ~4 LE pointers, walk fixed
records). The cost of a full port is the *executor*, not the loader (next section).

## 3. The BEST2 instruction set (sizes a port)

- **One flat opcode table, 184 entries, contiguous `0x00`–`0xB7`; the array index *is* the opcode
  byte.** *(ediabaslib `OcList` `EdiabasNet.cs:1949`; ediabasx `OPCODES` `disassembler.ts:174`.)*
- **Encoding:** `[opcode:1][addrModeByte:1][arg0…][arg1…]`. Exactly two operand slots; the
  addr-mode byte packs both (hi nibble = arg0 mode, lo = arg1 mode), selecting one of **16
  addressing modes** (immediate 8/16/32 LE, register, indexed/indirect with length, string).
  Jumps (21 ops) take a PC-relative `Imm32`.
- **Machine model:** byte regs `B0..BF`, 16-bit `I0..IF`, 32-bit `L0..L7`, string/byte-buffer
  `S0..SF`, IEEE-754 double `F0..F7`; flags Z/S/C/V; separate call + data stacks. `S` registers
  double as byte buffers, and indexed modes slice the response telegram out of an `S` reg.
- **Result types** — `ResultType : byte`, values 0–10 *(ediabaslib `EdiabasNet.cs:1473`)*:
  `0 B`(u8) `1 W`(u16) `2 D`(u32) `3 Q`(u64) `4 C`(char) `5 I`(s16) `6 L`(s32) `7 LL`(s64)
  `8 R`(real/double) `9 S`(string) `10 Y`(binary). Store ops `ergb/ergw/ergd/ergi/ergl/ergc`
  (ints), `ergr` (real), `ergs` (string), `ergy` (binary); `enewset` commits a result set,
  `etag` filters a set by requested name.

**Opcode inventory by purpose** (classification sums to 184; from ediabasx
`operations/*` + the canonical table):

| Class | Count | What |
|---|---|---|
| **Core** (arith/logic/move 18, control-flow 22, flags 3, stack 8, result 12) | **63** | the register/stack VM + result emit |
| **Communication** (`x*`) | **35** | ECU request/response, transport params, timing |
| Secondary (float 18, string 11, params 7, byte-conv 4, config 3, reg-iter 3, misc 2) | 48 | scaling math, text, job args |
| Long tail (file 8, time 10, table 8, shared-mem 2, procedures 10) | 38 | incl. **~13 deliberate no-ops** (plugins/extended-comm) |

So a faithful interpreter is **~98 opcodes that carry real logic** (63 core + 35 comm) + ~48
secondary; ~13 are no-ops and ~15 are unimplemented even in ediabaslib. **Empirically, the F20
diesel DDE SGBD uses 102 distinct opcodes** across 400k instructions, and **a single measurement
job uses 43** (it spans arithmetic + control-flow + table + comm + float + string + result). The
unavoidable hard part of a port is that the comm (`x*`) ops **interleave live ECU I/O with
compute** — you cannot statically pre-evaluate a job; you must run the VM against the transport.

## 4. The DDE measurement jobs (identified)

Engine ECU = diagnostic address **`0x12`**. The F-series N47 diesel DDE is the `d_motor`-group
variant **`d72n47a0.prg`** — ECU comment: *"SGBD für N47TÜ/N57TÜ (DDE7.21/7.01/7.41) verwendet in
**F0x, F1x, F2x, F3x** (UDS, MV, FlexRay)"*. It is the **F2x** variant **and** speaks **UDS**, so
it matches both an F20 and klartext's existing `read-did` (0x22) path. (The first file I opened,
`d73n47a0`, is the **E84/X1**, KWP2000 — a good reminder that the same engine spans many chassis
and *two* protocols.) `d72n47a0`: **272 jobs, 89 tables, revision 24.000**.

> ⚠️ **Exact-variant caveat:** several N47 DDE variants exist (`d70n47a0/b0`, `d71n47a0…d0`,
> `d72n47a0/b0`, `d73n47a0`, `dde50k47`). The precise one for a given car is resolved **on-car**
> by the `d_motor` group's IDENT job from the car's SVT — which I can't read offline. `d72n47a0`
> is the correct F2x/UDS family member; confirm the exact revision with a `klartext discover`
> against the car when convenient. The format/scaling findings hold across the family.

Measurement jobs in `d72n47a0` are self-describing, e.g. `STATUS_MOTORTEMPERATUR`,
`STATUS_KUEHLMITTELTEMPERATUR`, `STATUS_RAILDRUCK_IST/SOLL`, `STATUS_LADEDRUCK_IST/SOLL`,
`STATUS_ABGASTEMPERATUR_KAT`, `STATUS_OELNIVEAU`, and the generic block reader the brief named,
`(FASTA_)MESSWERTBLOCK_LESEN`. Each measurement job's comment is
*"Messwert selectiv lesen UDS: $2C DynamicallyDefineDataIdentifier"* and each emits, alongside the
raw telegrams `_REQUEST_n`/`_RESPONSE_n`, a **`STAT_…_WERT`** (`real`, the scaled value) and a
**`STAT_…_EINH`** (`string`, the unit).

## 4a. One job, end to end — `STATUS_MOTORTEMPERATUR`

Disassembled from `d72n47a0.prg` with ediabasx (`decompile … STATUS_MOTORTEMPERATUR`). Three
stages: build+send the UDS exchange, parse the typed raw value, scale it.

**(a) Build the request.** The job constructs a UDS DynamicallyDefineDataIdentifier telegram in an
`S` register and transmits it:

```
move   S1,{$2C.B,$03.B,$F3.B,$03.B}   ; UDS 2C 03 F3 03  (DDDI, dynamic DID 0xF303)
…                                      ; set target $12 (engine), source $F1 (tester) in header
xsend  S4,S1                           ; transmit; response captured into S4
```

The full pattern (three `_REQUEST_/_RESPONSE_` pairs + the `SERVICE "22;2C"` table column, below)
is the standard EDIABAS "selektiv lesen": **clear** dynamic DID `0xF303` (`2C 03 F3 03`, traced
above), **define** `0xF303` ↔ the measurement's internal id `0x4BC3`, then **read** it with
`22 F3 03`. *(I byte-traced the clear + the `xsend`; the define+read pair is what the `22;2C`
services and the three telegram pairs imply — not every byte of REQ2/REQ3 was traced.)*

**(b) Parse + (c) scale.** The response's raw value (an `S`/byte reg, sized by `DATENTYP`) is
converted to float and run through a linear transform, then emitted:

```
atsp     L2,#$C.L      ; load table column 12 (the multiplier) -> L2
fix2flt  F1,L2         ; F1 = (float) multiplier
a2flt    F0,S6         ; F0 = (float) raw measurement value
fmul     F1,F0         ; F1 = multiplier * raw
a2flt    F0,S4         ; F0 = (float) offset
fadd     F1,F0         ; F1 = multiplier*raw + offset
…
ergr     "STAT_MOTORTEMPERATUR_WERT",F0   ; emit real result
ergs     "STAT_MOTORTEMPERATUR_EINH",S1   ; emit unit string (e.g. "degC")
```

The bytecode is **generic** — `fmul`/`fadd` over values it just looked up. The per-measurement
constants come from a table (next section): for engine temperature, the row gives multiplier
`0.1`, offset `−273.14`, unit `degC`, raw type `unsigned int` (u16), internal id `0x4BC3`.

**Worked example.** `physical = raw × MUL / DIV + ADD`, with `DIV` absent ⇒ 1:

```
T[°C] = raw_u16 × 0.1 / 1 + (−273.14)        (raw is deci-Kelvin)
raw = 0x0E2F = 3631  →  3631 × 0.1 − 273.14 = 363.10 − 273.14 = 89.96 °C   (warm idle ✓)
raw = 0x0AAB = 2731  →  273.1 − 273.14 ≈ 0.0 °C                              (0 °C sanity ✓)
```

This is the same shape as the public J1979 coolant PID in `pid.rs` (`A − 40`), but the constants
are **BMW-proprietary and ECU-specific**, sourced from the SGBD — which is exactly the gap M5 left
open.

## 5. The pivot: scaling is *table-driven*, not per-job bytecode

The decisive finding. The constants above are not literals in the temperature job's bytecode —
they are a **row in a structured table the SGBD ships**, named **`SG_FUNKTIONEN`** (1787 rows in
`d72n47a0`). Columns (the table's own header row):

```
ARG, ID, RESULTNAME, INFO, EINHEIT, LABEL, L/H, DATENTYP, NAME, MUL, DIV, ADD, SG_ADR, SERVICE, ARG_TABELLE, RES_TABELLE
```

The engine-temperature row, read straight out of the `.prg` (no VM, just the table parser):

```
ITMOT | 0x4BC3 | STAT_MOTORTEMPERATUR_WERT | Motortemperatur | degC | EngDa_tEng | - |
unsigned int | - | 0.100000 | - | -273.140000 | 12 | 22;2C | - | -
```

Read as a scaling spec: **ID** `0x4BC3` (the identifier used in the `2C`/`22` read) · **EINHEIT**
`degC` · **DATENTYP** `unsigned int` (⇒ 2 raw bytes, unsigned) · **MUL** `0.1` · **DIV** 1 ·
**ADD** `−273.14` · **SG_ADR** `0x12` · **SERVICE** `22;2C` · **RES_TABELLE** (for enum/status
measurements) → a status-text table. A generic job does `tabset SG_FUNKTIONEN` → `tabseek` the
requested ARG → `tabget` these cells → builds the request from SERVICE/ID/SG_ADR → reads
`DATENTYP` bytes → `raw × MUL / DIV + ADD` → emits `…_WERT` and a separate `…_EINH` string.

**This reconciles the two references.** EdiabasLib is right that the VM has *no built-in*
DID→scaling table — but modern BMW SGBDs *author* the scaling into `SG_FUNKTIONEN` and let a
generic bytecode job consume it (ediabasx documents the same `tabset→tabseek→tabget→ergs` idiom,
`docs/vm-reference.md:421`). So for these SGBDs the scaling **is** declarative data we can read
without executing bytecode. Confirmed by `ediabasx table d72n47a0.prg SG_FUNKTIONEN --json`
returning all 1787 rows as structured cells.

**Coverage of the table pattern (sampled, diverse ECU types):**

| SGBD | ECU type | `SG_FUNKTIONEN`? |
|---|---|---|
| `d72n47a0` | N47 diesel DDE (F0x–F3x, UDS) | ✅ 1787 rows, **all** with numeric MUL |
| `mevd172` | MEVD17.2 petrol DME | ✅ identical 16-col schema |
| `acsm4` | airbag / crash safety | ✅ identical schema |
| `cic` | CIC infotainment | ✅ identical schema |
| `dsc_56` | **TRW** DSC (R56 brakes) | ❌ **none** — scaling done **inline** in bytecode |

The first four span engine, body, safety, and infotainment — strong evidence `SG_FUNKTIONEN` is the
*modern BMW* convention, so a table extractor covers the bulk of the fleet's measurements. `dsc_56`
is the honest counter-example: its `STATUS_RADGESCHWINDIGKEIT` masks/shifts response bytes with
**literal** constants in bytecode (`and B0,#$C0` / `and I2,#$3F` …) and has no measurement table.
That **inline-scaling tail** (older and some supplier ECUs) is what only a real interpreter, or a
raw fallback, can serve.

Units across the DDE's table are real engineering units (`%`, `degC`, `hPa`, `mV`, `kg/h`, `Nm`,
`rpm`, `bar`, `mg/hub`, …); ~45% of rows are dimensionless (status/enum values resolved via
`RES_TABELLE`, also a table — so the table approach covers enums too, not just linear values).

## 6. How this slots into klartext

The existing scaler (`crates/semantic/src/pid.rs`, `did.rs`) is pure, table-driven, and
`Option`-returning — it scales a recognised signal and **degrades to raw** otherwise, never
erroring. A `SG_FUNKTIONEN`-backed scaler is the same shape, one level richer:

- Input key is the **ARG/ID**, not a static DID. Reads here use UDS **`0x2C` define + `0x22` read**
  of a dynamic DID (`0xF303`), per `SERVICE`. Some ECUs/measurements use a plain static `0x22`;
  `SERVICE` says which. This is new request-sequencing work versus M5's single static `0x22`.
- Output is `value = raw·MUL/DIV+ADD` (per `DATENTYP`) **plus a unit string** — note the unit is a
  *separate* result in EDIABAS, not a field on the value, matching `ScaledPid { value, unit }`.
- Unknown ARG / inline-only ECU / unsupported `DATENTYP` ⇒ **degrade to raw**, exactly as today.

It also dovetails with M3: the ISTA DB's `XEP_ECURESULTS.MULTIPLIKATOR/OFFSET` is a *display*
re-scaling keyed by the very `RESULTNAME` (`STAT_MOTORTEMPERATUR_WERT`) that `SG_FUNKTIONEN`
produces — the two layers compose.

## 7. Architecture options evaluated

Goals to weigh against: **native Rust**, **fully ISTA-free**, and (user-stated) **works across all
supported BMWs**, with klartext's blast-radius rule (reads autonomous; this is all reads).

**(1) Port a BEST2 interpreter to Rust.** *General, native, runs any job.*
- Scope: loader trivial; decoder small/fully-specified (184-entry table + 16 addressing modes);
  the **executor is the bulk** — ~98 live opcodes, plus the comm `x*` ops that interleave live
  transport I/O, plus `.grp` group→variant two-level dispatch, a soft-error/trap model, CP1252,
  and C#-quirk parity. ediabasx's per-category `*.spec.ts` vectors are reusable as Rust tests.
- Coverage: **100%** — table *and* inline measurements, every ECU, and (if ever wanted) actuation/
  routine jobs. Only this option serves the inline-scaling tail.
- Cost: **largest** — a multi-week milestone to reach parity; the comm ops can't be unit-tested
  without a transport/sim.
- Fit: native ✅, ISTA-free ✅. The right *eventual* answer, wrong *first* step.

**(2) Hand-port chosen measurement scalings from disassembly.** *Least effort, not general.*
- Reality check from the disasm: a measurement job touches 43 opcodes and the "formula" isn't an
  isolated literal — for modern SGBDs it's produced by the generic table-reader (so hand-porting
  one means re-implementing that reader anyway = option 4), and for inline ECUs it's tangled
  bit/arith bytecode. Re-deriving per measurement is brittle and **O(measurements × ECUs)**.
- Coverage: only the hand-picked set; **explicitly fails the "all BMWs" goal.**
- Fit: native ✅, ISTA-free ✅, but doesn't scale. Only defensible as a tiny throwaway stopgap.

**(3) Delegate to ediabasx (Node) / ediabaslib (.NET) as a subprocess engine.** *ISTA-free, adds a
runtime.*
- Coverage: **100%** (it's the real engine, table + inline).
- Cost: **lowest Rust work** — shell out, parse output. But it would still need to drive the live
  transport (or be fed captured responses), and you inherit its process model.
- Fit: ISTA-free ✅, but **breaks native-Rust** — bolting a Node or .NET runtime onto a project
  whose thesis is a ~20 MB native binary. **Best use: an offline *oracle*** — run ediabasx once to
  extract tables and to generate golden test vectors for a Rust implementation; don't ship it.

**(4) Extract `SG_FUNKTIONEN` + generic Rust scaler (recommended primary).**
- What: get the measurement table out of each SGBD — either a **small native Rust parser** of the
  `.prg` table section (XOR-`0xF7`, header ptr `0x84`, 0x50-byte records, string cells — all
  specified in §2, far smaller than the opcode VM) **or** a **one-time offline extraction** with
  ediabasx into a bundled data file (mirroring M3's `klartext-semantic.db` extract precedent).
  Then implement in Rust: build the `2C`/`22` (or static `22`) request from `SERVICE/ID/SG_ADR`,
  read `DATENTYP` bytes, apply `raw·MUL/DIV+ADD`, attach `EINHEIT`, resolve enums via `RES_TABELLE`.
- Coverage: **broad** — every `SG_FUNKTIONEN` ECU (sampled: engine/body/safety/infotainment;
  1787 measurements in the DDE alone, *all* numerically scalable). Inline-only ECUs ⇒ raw fallback.
- Cost: **moderate** — a bounded table parser + a generic scaler + the `0x2C/0x22` sequencing. No
  per-measurement hand-work; no VM; no non-Rust runtime at ship time.
- Fit: native ✅✅, ISTA-free ✅✅ (the `.prg` is BYO-data the user already has; we parse data, we
  don't run BMW code), and it **generalises across the fleet** — the user's actual requirement.

### Recommended path

1. **Now:** option **4**. Sub-decision — start with **offline extraction via ediabasx (option 3 as
   a build-time oracle)** to ship a measurement catalogue fast and validate the generic Rust scaler
   against ediabasx's output as golden vectors; promote to a **native Rust `.prg` table parser**
   later for a zero-non-Rust-deps build. Keep the existing **degrade-to-raw** contract for
   unknown/inline cases.
2. **Later milestone:** option **1**, the BEST2 interpreter, to absorb the inline-scaling tail
   (TRW/older ECUs) and reach full generality. Size it from §3 (~98 live opcodes); reuse ediabasx
   spec vectors.
3. **Never ship** option **3** as a runtime; **skip** option **2** beyond a throwaway probe.

## 7a. Phase 2 status — Part A built, Part B derived from disassembly

Option 4 is implemented (M6 Phase 2):

- New crate **`klartext-sgbd`**: a sans-IO `.prg` loader (`Prg::parse(&[u8])` / `Prg::open`) —
  magic check, XOR-`0xF7` body from `0xA0`, the `0x84` table directory of `0x50`-byte entries,
  null-terminated CP1252 cells — exposing each [`Table`] as `(name, columns, rows)`. Hermetic
  tests build synthetic `.prg`s; an `#[ignore]`d real-data test extracts `SG_FUNKTIONEN` and was
  diffed **byte-for-byte against the ediabasx oracle** across all 1787 rows × 16 columns (0 diff).
- **`klartext-semantic::measurement`**: `DataType` (`unsigned char/int/long`, `motorola float`),
  the pure `scale(raw, dt, mul, div, add) = raw·MUL/DIV+ADD`, and `Measurement`/`Measurements`
  (parsed from `SG_FUNKTIONEN`, indexed by id) with degrade-to-raw on every unhandled case. The
  worked example `STATUS_MOTORTEMPERATUR` raw `0E 2F` → **89.96 °C** is a unit test and also passes
  on the real DDE `.prg`.
- **Wiring**: CLI `read-did --sgbd <ecu>.prg` and MCP `read_data` (`--sgbd-dir` + a `variant`
  arg) overlay a proprietary value + unit when a DID is a `SG_FUNKTIONEN` id; standard PIDs (M5),
  ISO-named DIDs, and unknowns are unchanged, and raw bytes are always present. ediabasx is used
  only as an offline extraction/verification oracle — never committed, never a runtime dependency.

**Part B (the request builder) is implemented — DERIVED FROM DISASSEMBLY, not a capture.** No
pcap was available (the ISTA VM is gone; the `.prg`s survived), so the `2C`/`22` sequence was read
off the `STATUS_MOTORTEMPERATUR` BEST2 in `d72n47a0.prg`, disassembled with `ediabasx` (the
Phase-1 offline oracle — never committed) and cross-checked against `ediabaslib`.
`measurement::build_read_request` returns the ordered UDS payloads, which
`DiagnosticClient::read_dynamic_measurement` sends in turn:

1. **clear** dynamic DID `0xF303` — `2C 03 F3 03` (inline literal, job `@0x1F2DAF`);
2. **define** `0xF303` by identifier from the measurement's internal id as a *source DID*,
   position `01`, size = the data type's byte width — `2C 01 F3 03 <id> 01 <width>`, e.g.
   `2C 01 F3 03 4B C3 01 02` for engine temp (u16). In the job the id is the `SG_FUNKTIONEN.ID`
   resolved by `LABEL`, then `hex2y`-decoded; size is DATENTYP-driven (u8→`01`, u16→`02`,
   u32/float→`04`);
3. **read** `0xF303` — `22 F3 03`; the `62 F3 03 <hi> <lo>` response carries the raw value at
   offset 0 after the 3-byte echo, width = the data type (the job reads payload bytes `3..3+width`
   big-endian, then scales `raw·MUL/DIV+ADD`).

Subfunction is unambiguously `01` (defineByIdentifier), **not** `02`/memory-address; there is **no**
session/security precondition and **no** trailing clear — exactly three telegrams in this order,
target `$12` (DDE), source `$F1`. The derived frames are committed as an offline replay fixture —
the loopback mock in `mcp/tests/integration.rs` plus `klartext-semantic`/`klartext-client` unit
tests — so the full **define → read → scale** path runs without a car: engine temp reads `0E 2F`
→ **89.96 °C**.

> ⚠️ **DERIVED, not captured — pending on-car confirmation.** Every byte above is *observed* in the
> disassembly, but the real ECU's response can only be confirmed on the F20. Manual HIL step: read a
> dynamic proprietary measurement through MCP on the car and confirm a plausible value. Until then
> the `2C` framing is `[verify against capture]`.

## 8. Open questions / to verify before/while building (not blockers to deciding)

- **Exact F-series DDE revision** for the user's car — resolve on-car via the `d_motor` IDENT
  (`klartext discover`); doesn't change the architecture.
- **`2C`/`22` request sequencing** — ✅ resolved: derived from the `STATUS_MOTORTEMPERATUR`
  disassembly and implemented (§7a). The real on-car response bytes remain the manual confirmation.
- **Table-format stability** across SGBD generations/suppliers — sampled 5 ECUs; a broader sweep
  (and noting which ECUs are inline-only) sizes the raw-fallback gap.
- **On-car confirmation** of one scaled value (e.g. coolant temp plausibility) — the manual HIL
  step, deferred per project rules.

## 9. References

- ediabaslib (`uholeschak/ediabaslib`) — `EdiabasNet.cs` (opcode table `:1949`, XOR `:7165`,
  `ResultType` `:1473`, header offsets `:4900–4995`).
- ediabasx (`emdzej/ediabasx`) — `packages/best-parser/src/parser.ts` (format), `disassembler.ts`
  (opcodes), `packages/interpreter/src/operations/*` (semantics), `docs/vm-reference.md` (the
  table-driven measurement idiom). Plus `emdzej/ediabasx-docs-sgbd` (generated per-SGBD disasm).
- ISO 14229-1 (UDS) — `0x22` ReadDataByIdentifier, `0x2C` DynamicallyDefineDataIdentifier.
- `docs/sqlite-findings.md` (M3, the deferred SGBD scaling), `docs/standard-pids.md` (M5, the
  public-PID scaler this extends).
