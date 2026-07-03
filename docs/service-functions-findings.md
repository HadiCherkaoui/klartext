# Service functions (write/actuation) — Milestone 7, Phase 1 findings

Read-only exploration of where BMW **service functions** (resets, adaptations, actuations,
calibrations) live in the surviving ISTA data, how extractable each layer is, and a
risk-ranked recommendation for the first write-side vertical slice. **This is a findings
doc. Nothing was built. Halt for a decision** (which first function + architecture sign-off).

This is the **write side**. M1–M6 built the read path (HSFZ → UDS → managed session →
DTC/DID/measurement decode) and are not touched here. The blast-radius rule now bites for
real: every function below either changes ECU state or moves a physical component.

## TL;DR — recommendation

- **Where they live (the split).** A service function = a named entry in the ISTA DB
  (`XEP_ECUFIXEDFUNCTIONS`) bound to an **ordered sequence of symbolic EDIABAS jobs**
  (`XEP_REFECUJOBS` → `XEP_ECUJOBS`). The **UDS bytes are not in the DB** — they are in the
  SGBD `.prg` (the layer M6 already cracked). So execution = **DB (which jobs, in what order,
  with what params) + SGBD (the bytes each job emits)**. Two tiers:
  - **Tier 1 — simple ECU functions: derivable from data.** Reset / adaptation / single
    actuation. The DB gives a *linear* `Preset → Main → Reset` job list; the SGBD gives the
    frame. For the **DDE specifically**, the SGBD even ships the control parameters as **plain
    tables** (`STELLER` actuators, `LERNWERTE_RUECK` resets, `ABGLEICH` calibrations) — the
    same kind of structured data `SG_FUNKTIONEN` was for measurements. **This is the buildable,
    in-scope layer.**
  - **Tier 2 — guided procedures: compiled, not portable.** Multi-step diagnostics / maintenance
    plans with preconditions and branching are **compiled .NET** (`22,848 ABL_*.dll`,
    `IstaModuleBase`). They *decompile* fine (ilspycmd → readable C#; not encrypted), but they are
    not **portable** the way the SGBD tables are — for four reasons that compound: **(1) scale** —
    a median module is ~1,915 lines / 140 branches, 22,848 of them, 1.4 GB, and BMW reships them
    every ISTA update; **(2) coupling** — each derives from `IstaModuleBase` and calls the
    Rheingold runtime (`InteropHelper`, `ServiceProgram`), executes EDIABAS jobs by name, reads
    `XEP_RULES` applicability blobs, and drives interactive technician dialogs — porting one drags
    a slice of the ISTA app with it; **(3) license** — decompiling *to understand* is fine (M3 did
    it to recover the DB password), but hand-porting BMW's procedure *logic* into this AGPL repo
    copies copyrighted expression (the Scapy trap CLAUDE.md warns of) — the SGBD tables were safe
    only because they are *data/facts* (a multiplier, a LID), which are not copyrightable;
    **(4) safety** — those branches often *are* the preconditions ("car stationary before opening
    the caliper"); a hand-port that drops one is dangerous on the write side. The DB only *names*
    them. **Out of scope** — and largely unneeded: the useful resets/actuations are Tier 1.
- **First slice (recommended): the DDE engine-oil CBS reset (`CBS_RESET`)** — a maintenance
  counter reset. No physical actuation, derivable from surviving data, and **self-confirming
  on-car** by reading the counter back (`CBS_DATEN_LESEN`) before/after. Purest-derivable
  alternative: `LERNWERTE_RUECKSETZEN` (a learned-value reset). Both exercise the *entire*
  write-side machinery (session → write → read-back) on the safest possible operation.
- **Architecture.** Add an actuation path that opens an **extended session (`10 03`)**, emits
  **`0x31` RoutineControl** (resets/routines) or **`0x2F` IO-control** (actuators) or **`0x2E`**
  (adaptation writes), holds **TesterPresent (`3E 80`)**, and **always runs the function's
  `Reset`/return-control phase** (the built-in undo) even on error. **`0x27` SecurityAccess**
  is ECU-specific and deferred until an ECU demands it (NRC 0x33/0x35). Three gating tiers by
  blast radius — and the **MCP server stays read-only** (no service functions as agent tools).

| Layer | Where | Encoding | Extractable now? | In scope |
|---|---|---|---|---|
| Function catalog (name, operator text, safety flags) | DB `XEP_ECUFIXEDFUNCTIONS` | structured rows | ✅ yes | yes (labels) |
| Tier-1 job sequence (which jobs, order, params) | DB `XEP_REFECUJOBS`/`XEP_ECUJOBS` | structured rows (PHASE+RANK) | ✅ yes | yes |
| Job → UDS bytes | SGBD `.prg` | DDE: **tables**; others: BEST2 bytecode | ✅ DDE tables / ⚠ else needs interpreter | yes (DDE) |
| Tier-2 procedure logic (steps, preconditions, branching) | `ABL_*.dll` on disk | **compiled .NET (PE32+)** | ⚠ decompilable, not portable (scale/coupling/license/safety) | **no** |

## 1. Scope, method, what is committed

- **Data (gitignored, BYO):** `data/Testmodule(1)/` — the same ISTA export used by M3/M6:
  `Ecu/*.prg` (1405 SGBD), `SQLiteDBs/DiagDocDb.sqlite` (encrypted, decryptable per M3),
  `EcuFunctions/*.xml` (26 plaintext function-catalog files), and
  `Testmodule/*.dll` (22,848 compiled `ABL_*` procedure modules). The ISTA VM is gone; this
  data survived.
- **Method:** SGBD `.prg` read with a throwaway parser mirroring `crates/sgbd/src/prg.rs`
  (validated against M6's facts: DDE = 272 jobs / 89 tables / `SG_FUNKTIONEN` 1787 rows) plus
  a job-list walk (offset `0x88`) the shipped crate skips. `DiagDocDb` queried read-only via
  the cached SQLite3MC (rc4, password per M3). `ABL_*.dll` typed with `file` + `strings`.
  EcuFunctions XML parsed directly. **No car, no capture** — offline static analysis only.
- **Committed by this milestone:** *only this document.* It quotes job names, table column
  headers, a handful of example rows, and DB table/column names + row counts — the same kind
  and amount of factual detail the M3/M6 docs carry. No `.prg`/`.dll`/DB blobs, no bulk dumps,
  no VIN/PII (none encountered).
- **Hardware-in-the-loop:** Claude cannot reach the car. Every UDS framing below is **derived**
  from the data + ISO 14229 + `docs/protocol-reference.md`, and is flagged where on-car
  confirmation is still required — exactly the posture M6 Part B took for the `2C`/`22` frames.

## 2. The anatomy of a BMW service function

ISTA layers a service function across three artefacts. Cross-validated from both the SGBD side
and the DB side; they agree.

```
  ISTA DB (DiagDocDb)                         SGBD (.prg)                 transport
  ┌───────────────────────────┐    symbolic   ┌───────────────────┐   UDS  ┌─────┐
  │ XEP_ECUFIXEDFUNCTIONS      │    job name   │ STEUERN_* / *_RESET│  bytes │ ECU │
  │  "reset oil service"       │──────────────▶│  job  → emits a    │───────▶│     │
  │  + operator text + flags   │               │  UDS request frame │        └─────┘
  │ XEP_REFECUJOBS (PHASE,RANK)│               └───────────────────┘
  │  ordered job sequence      │     the bytes live HERE, not in the DB
  └───────────────────────────┘
```

- The **DB** answers *what function, which jobs, in what order, with which symbolic params,
  and what to tell the technician* — but never the wire bytes.
- The **SGBD** answers *what UDS frame a job emits*. M6 proved this is `2C`/`22` for the
  `STATUS_*` measurement jobs; the `STEUERN_*` / `*_RESET` jobs are the write analog.
- The **ABL .dll** only matters for tier-2 (guided, branching procedures); tier-1 functions
  do not need it.

## 3. Where service functions live (the split)

### 3a. SGBD: the DDE exposes control as plain tables (derivable)

The DDE diesel SGBD `d72n47a0.prg` has **272 jobs**: 107 `STATUS_*` (reads), **61 `STEUERN_*`**
(control), **28 `ABGLEICH*`** (calibration), plus `START`/`STOP`/`RESET` families. Crucially,
`SG_FUNKTIONEN` (the 1787-row measurement table M6 used) is **100% `SERVICE = 22;2C`** — pure
reads. Control does **not** live there; it lives in three *separate* tables whose schema is the
control analog of `SG_FUNKTIONEN`:

| Table | Rows | Models | Key columns | Invoking job(s) |
|---|---|---|---|---|
| `STELLER` ("actuators") | 45 | physical actuator on/off | `LABEL, TEXT, LID, BYTES, FACT_A/B, VGR_TU/TO, JOB_LESEN, JOB_EIN, JOB_AUS, …` | `STEUERN_SELECTIV` / `STEUERN_ENDE_SELECTIV` |
| `LERNWERTE_RUECK` ("reset learned values") | 18 | adaptation/counter reset | `LABEL, TEXT, LID, …, JOB_LESEN, JOB_VERST, JOB_PROG, VALUE` | `LERNWERTE_RUECKSETZEN` |
| `ABGLEICH` ("calibration") | 85 | sensor/injector calibration write | `LABEL, TEXT, LID, BYTES, …, JOB_LESEN, JOB_PROG, TEXT_ACC` | `ABGLEICH_PROGRAMMIEREN_*` |

Read as a control spec, a row gives the **LID** (the identifier, e.g. throttle `DRO` = `0x602A`),
the value encoding (`BYTES`, `FACT_A/B`, valid range `VGR_TU..VGR_TO`), and **which generic job**
turns it on / off / programs it. Two real rows (one actuator, one reset):

```
STELLER         : DRO   "Drosselklappe" (throttle)  LID 0x602A  EIN=STEUERN_SELECTIV  AUS=STEUERN_ENDE_SELECTIV
LERNWERTE_RUECK : IBSRE "Rücksetzen IBS-Erkennung"  LID 0xA0F7  PROG=LERNWERTE_RUECKSETZEN  VALUE 0x00
```

This is exactly the M6 measurement win, one notch further: the table makes the **parameters**
of a control function declarative data. The actuation idiom is visible in the column pairs —
`JOB_EIN`/`JOB_AUS` (on / off) and the `STEUERN_*` / `STEUERN_ENDE_*` start/stop pairing — i.e.
**every actuation has a paired "return control" job**. That pairing is the safety primitive (see §7).

> ⚠ **What the table does NOT give: the SID.** The cells give LID + value + *which* job, but
> not the UDS service byte the generic job emits. As in M6 Part B, the exact `0x2F`/`0x31`/`0x2E`
> framing and subfunction must be read from the generic job's BEST2 bytecode (one disassembly
> with the ediabasx oracle M6 used) or confirmed by an on-car capture. The *class* is certain
> from `protocol-reference.md:311` ("`STEUERN`-type jobs → `0x2F` or `0x31`"); the *exact bytes*
> are `[verify]`.

### 3b. The DDE is special — most ECUs are bytecode-bound

The table-driven control layer does **not** generalise the way `SG_FUNKTIONEN` did. Sweeping
the control tables across diverse ECUs:

| ECU | jobs | STEUERN | control tables present |
|---|---|---|---|
| **DDE diesel** `d72n47a0` | 272 | 61 | **`STELLER`, `LERNWERTE_RUECK`, `ABGLEICH`** (+ `DIGITALARGUMENT`) |
| MEVD petrol `mevd172` | 290 | 104 | none (only `DIGITALARGUMENT` enum) |
| EMF park-brake `emf_01` | 35 | 6 | none |
| DSC brakes `dsc_56` | 92 | 6 | none |
| IHKA climate `ihka87` | 142 | 49 | none |
| BDC body `bdc` | 190 | 60 | none |
| ACSM airbag `acsm4` | 87 | 13 | none |

Only the DDE ships `STELLER`/`LERNWERTE_RUECK`/`ABGLEICH`. Every other ECU has many `STEUERN_*`
jobs but encodes their control logic in **job bytecode** — to derive their frames you must run a
BEST2 interpreter on each job (M6's deferred "inline tail"), or capture on-car. `DIGITALARGUMENT`
is just a shared `ein=1/aus=0` vocabulary, not a control catalog. **Implication:** the DDE — the
project's priority ECU — is the cheapest place to land the first write functions from data alone.

### 3c. DB: the function catalog is structured, but binds to jobs, not bytes

`XEP_ECUFIXEDFUNCTIONS` (35,555 rows) is the named, localized, hierarchical catalog. Per-row it
carries `TITLE_*` (23 languages), the operator prose `PREPARINGOPERATORTEXT_*` /
`PROCESSINGOPERATORTEXT_*` / `POSTOPERATORTEXT_*` (preconditions and steps **as text for a
human**), flags `SICHERHEITSRELEVANT` / `STEUERGERAETEFUNKTIONENRELEVAN`, and
`ACTIVATION` / `ACTIVATION_DURATION_MS`. It has **no** column for a job, a DLL, or a procedure —
the binding is external, via `XEP_REFECUJOBS`. By `NODECLASS` (decoded via `XEP_NODECLASSES`):

| Function class | count | UDS analog |
|---|---|---|
| `ECUFixedFunctionReadingState` | 24,589 | read (`0x22`) |
| `ECUFixedFunctionControlingActuator` | **9,660** | **actuation (`0x2F`)** |
| `ECUFixedFunctionReadingIdentification` | 1,306 | read (`0x22`) |
| `…DeletingFaultCode` / `…CodingECU` / `…InitECU` | smaller | `0x14` / `0x2E` / routine |

The **tier-1 orchestration is structured data**: `XEP_REFECUJOBS` (44,109 rows referencing a
fixed-function) orders jobs by `PHASE` (**Preset 1,592 / Main 38,512 / Reset 4,128**) and `RANK`.
That is a *linear* `Preset → Main → Reset` sequence — **no conditional branching** — directly
parseable. End-to-end example (fleet sample, washer pump):

```
XEP_ECUFIXEDFUNCTIONS "Windscreen washer pump"  (NODECLASS = ControlingActuator)
 └─ XEP_REFECUJOBS  PHASE=Main RANK=1
     └─ XEP_ECUJOBS.NAME = STEUERN_DIGITAL
         └─ XEP_ECUPARAMETERS  P1=MFBHA (component-code symbol)  P2=1 (state on)
```

`XEP_ECUJOBS` columns are `ID, FUNCTIONNAMEJOB, ADAPTERCONFIGURATION, NAME` — **symbolic only,
no service byte**. This is the authority that the DB never holds wire bytes; the SGBD does.
(`SICHERHEITSRELEVANT` is sparse — 0 for all 132 engine fixed-functions sampled — so it is *not*
a reliable engine-domain risk discriminator; physical actuation, i.e. the `ControlingActuator`
nodeclass / `STELLER` membership, is the real signal.)

### 3d. ABL: the tier-2 procedure layer is compiled code (decompilable, not portable)

`Testmodule/` holds **22,848 `ABL_*.dll`**, every one a `PE32+ … x86-64 Mono/.Net assembly`
exporting `IstaModuleBase` / `BMW.Rheingold.Module.ISTA` / `RheingoldServiceProgramCompiler`.
Sizes: median **37 KB**, p90 **137 KB**, max **26 MB**, **1.4 GB** total — substantial compiled
logic, not thin wrappers. The DB catalogs them in `XEP_INFOOBJECTS` (22,834 rows with
`INFOTYPE='ABL'`, `IDENTIFIKATOR='ABL-DIT-…'` mapping dash↔underscore to the on-disk
`ABL_DIT_….dll`; `NAME`/`ASSEMBLY` null — a stub pointing at the external module) and types them
`PROGRAMTYPE` ∈ {`Diagnosetest` 15,702, `Wartungsablauf` 359, `Bibliotheksfunktion`,
`Eskalationsvoraussetzungsprüfung` (escalation **precondition check**), …}. The whole DB has
**only three BLOB columns** (`XEP_RULES`, `XEP_RULES_INCLUDE`, `RG_NEWS`); none stores procedure
steps. `XEP_RULES` (343k rows) is a serialized *applicability* expression (does this function
apply to this variant) — not runtime branching. **So the steps/preconditions/branching of a
guided procedure are inside the .NET IL.** Decompilable (ilspycmd → readable C#; a median module
is ~1,915 lines / 140 branches), but **not portable** — scale (22,848 modules), runtime coupling
(`IstaModuleBase`/`InteropHelper`/`ServiceProgram`), license (porting BMW's logic taints the AGPL
repo; cf. the TL;DR's four reasons), and safety (the branches are the preconditions). Firmly out
of scope — and unneeded, since the in-scope resets/actuations are Tier 1.

## 4. Extractability assessment (per layer)

| Layer | Encoding | Verdict |
|---|---|---|
| Function name + operator text + flags | DB rows (`XEP_ECUFIXEDFUNCTIONS`) | **Trivial** — like M3's DTC text |
| Tier-1 job sequence (Preset/Main/Reset + RANK + params) | DB rows (`XEP_REFECUJOBS`/`ECUJOBS`/`ECUPARAMETERS`) | **Easy** — linear, no branching |
| Job → UDS bytes, **DDE** | SGBD `STELLER`/`LERNWERTE_RUECK`/`ABGLEICH` tables | **Easy** — same parser as `SG_FUNKTIONEN`; SID needs 1 disasm/capture |
| Job → UDS bytes, **other ECUs** | SGBD BEST2 job bytecode | **Hard** — needs the interpreter (M6's deferred tail) |
| Tier-2 procedure logic | compiled `ABL_*.dll` (.NET IL) | **Decompilable but not portable** — scale + runtime coupling + license + safety; out of scope |

The procedure layer the brief asked about is therefore **two things**: a *thin structured part*
(the tier-1 Preset/Main/Reset job list — extractable, and all we need for in-scope functions) and
a *thick compiled part* (tier-2 guided logic — decompilable but not portable, and not needed for
resets/single actuations). The honest line: **simple service functions are data; guided
procedures are code.**

## 5. Risk-ranked inventory

Risk axis = **physical blast radius**, not "does it write." LOW = changes a stored value the ECU
re-derives, no component moves (counter/adaptation/learned-value resets). HIGH = actuates a
physical component, or writes a value that changes combustion/safety behavior.

### 5a. DDE (`d72n47a0`, engine — priority ECU)

| Function (job) | Location | Risk | Why |
|---|---|---|---|
| `CBS_RESET` (oil/CBS service counter) | SGBD job + `CBSKENNUNG` table | **LOW** | resets a maintenance counter; `CBSKENNUNG`: `0x01=Oel`(oil), `0x02=Br_v`, `0x03=Brfl` |
| `LERNWERTE_RUECKSETZEN` (learned-value reset) | SGBD `LERNWERTE_RUECK` table (18) | **LOW** | zeroes an adaptation (IBS/ARS detection…); ECU re-learns |
| `STEUERN_PM_HISTOGRAM_RESET`, `STEUERN_MSA2HISTORIERESET`, `STEUERN_DAROL_RESET`, `STEUERN_LLKETA_RESET` | SGBD jobs | **LOW** | reset diagnostic statistics only; no behavior change |
| `STEUERN_BATTERIETAUSCH_REGISTRIEREN` (battery registration) | SGBD job + battery tables | **LOW–MED** | adaptation write; changes charging strategy (its purpose), reversible |
| `STEUERN_E_LUEFTER` / `_AUS` (electric fan) | `STELLER` (fan) | **HIGH** | spins the radiator fan |
| `STEUERN_SELECTIV` on throttle/intake-flap/glow relay | `STELLER` (`DRO`,`ZLK`,`GLU`,…) | **HIGH** | moves throttle / flaps, energizes glow relay |
| `STEUERN_HYDRAULIKTEST_DDE` / `_ENDE` | SGBD job (start/stop) | **HIGH** | pressurizes the fuel rail (hydraulic test) |
| `STEUERN_ROE_START/STOP`, `STEUERN_DOSIERSTRATEGIE`, `STEUERN_ENTLUEFTUNG_KUEHLKREIS` | SGBD jobs | **HIGH** | regen/oil-dilution routine, SCR dosing, coolant-pump bleed |
| `ABGLEICH_PROGRAMMIEREN_IMA*` (injector calibration) | SGBD `ABGLEICH` table (85) | **HIGH** | writes injector quantity codes; wrong values → rough running/damage; needs correct data |

The split inside one ECU is clear: **resets/adaptations (low)** vs **`STELLER` actuation +
test/regen routines + calibration writes (high)**.

### 5b. EMF (`emf_01`, electromechanical parking brake — the contrast ECU)

EMF has **no control tables** — just a generic `STEUERN_ROUTINE` / `STEUERN_ROE_*` (i.e. UDS
`0x31` RoutineControl with a routine-id argument). Its meaningful service functions (release
calipers to the **service/pad-change position**, re-tension, mount/dismount) are therefore
**tier-2**: the routine-id + the *mandatory preconditions* (vehicle stationary, ignition on,
caliper sequencing) live in the catalog's operator text and the compiled `ABL`. **All HIGH** —
moving the brake actuators with a person near the wheels is the textbook "do not actuate blind"
case. EMF is precisely the class that is **not derivable from the SGBD alone** and **must not**
be a first slice.

> The fleet shape generalises this: the `ECUFixedFunctionControlingActuator` nodeclass = 9,660
> functions fleet-wide.
> Most non-DDE actuation resolves to a generic `STEUERN_ROUTINE`/`STEUERN_DIGITAL` whose
> routine-id/component-code is a symbolic param in the DB but whose *bytes* and *preconditions*
> are bytecode/compiled — high-risk and procedure-bound.

## 6. Recommended first vertical slice

**Primary: the DDE engine-oil CBS reset (`CBS_RESET`, counter `0x01 = Oel`).**

- **Low-risk.** Resets a maintenance counter. No component moves; nothing about how the engine
  runs changes. The canonical "safe service function."
- **Derivable from surviving data.** The job exists in `d72n47a0.prg`; `CBSKENNUNG` gives the
  counter id (`0x01` oil); `CBS_DATEN_LESEN` reads the counters back. The DB names it and (via
  `XEP_REFECUJOBS`) gives its Preset/Main/Reset job order. Only the exact SID/subfunction needs
  one BEST2 disasm of the reset job (re-clone the ediabasx oracle as M6 did) or an on-car
  capture — flagged `[verify]`, same posture as M6 Part B.
- **Self-confirming on-car without ISTA.** Read the oil-service counter (`CBS_DATEN_LESEN`,
  the existing M5/M6 read path) → reset → read again → it returns to full/zero; the dashboard
  service indicator visibly resets. The read path *is* the oracle.
- **Exercises the whole write architecture** on the safest possible op: extended session →
  TesterPresent keepalive → a single write (`0x31` or `0x2E`, `[verify]`) → read-back verify →
  session close. Everything §7 needs, with near-zero blast radius.
- *Caveat:* on F-series some CBS state is mirrored centrally (KOMBI/BDC); resetting the DDE's
  own counter and reading **the same ECU's** counter back is internally consistent regardless,
  which is all a vertical slice needs to prove.

**Alternative (purest table-derivable): `LERNWERTE_RUECKSETZEN`** on a benign `LERNWERTE_RUECK`
row (e.g. `IBSRE`, reset IBS detection). Lowest blast radius of all (ECU re-learns), and the
`LERNWERTE_RUECK` table gives LID + VALUE + job directly — the closest parallel to M6's
measurement-table win. Slightly less legible to a human and the read-back is fuzzier (the value
re-adapts), which is why CBS oil reset is the primary pick.

**Explicitly not first:** anything in `STELLER` (fan/throttle/glow), any `*HYDRAULIKTEST*` /
`*ROE*` / `*DOSIER*` routine, any `ABGLEICH_PROGRAMMIEREN*`, and **all** of EMF.

## 7. Write-side architecture recommendation

### 7a. UDS services needed (and where each maps)

From `docs/protocol-reference.md` (the source of truth) + the function classes above:

| Service | Use | Function class | Notes |
|---|---|---|---|
| `0x10` DiagnosticSessionControl | enter **`10 03` extended session** | **all writes** | `protocol-reference.md:106` — "the session you enter for service functions"; benign itself |
| `0x3E` TesterPresent (`3E 80`) | keepalive (~every 2 s, S3 = 5 s) | any held session | required so the ECU doesn't drop to default mid-function |
| `0x31` RoutineControl | start/stop/result a routine | **resets, tests, regen, EMF caliper** | subfn `01` start / `02` stop / `03` result; the `*_RESET` and `STEUERN_ROUTINE`/`ROE` jobs |
| `0x2F` InputOutputControlByIdentifier | actuate a component | **`STELLER` actuators** | ctrl option `03` shortTermAdjust to set, `00` returnControl to release |
| `0x2E` WriteDataByIdentifier | write an adaptation/value | **`LERNWERTE_RUECK`, `ABGLEICH`, battery reg** | the `VALUE`/calibration write |
| `0x27` SecurityAccess | unlock protected writes | **deferred** | ECU-specific seed→key; implement only when an ECU answers NRC `0x33`/`0x35` (`protocol-reference.md:330,340`) |

For the **first slice** only `0x10` + `0x3E` + (`0x31` **or** `0x2E`) + the existing `0x22`
read-back are needed. `0x2F`, `0x27`, and the full `STELLER`/`ABGLEICH` paths come later, gated
harder.

### 7b. The Preset → Main → Reset invariant (safety primitive)

Both data sources independently encode the same shape: SGBD pairs `JOB_EIN`/`JOB_AUS` and
`STEUERN_*`/`STEUERN_ENDE_*`; the DB orders jobs `Preset → Main → Reset`. **An actuation is never
a single shot — it is a bracket.** The architecture must therefore run the function as a
guaranteed bracket: execute Preset/Main, and **always** execute the Reset / return-control phase
(`0x2F` returnControlToECU `00`, or `0x31` stopRoutine `02`, or `STEUERN_ENDE_*`) in a `finally`
— on success, on error, on timeout, on Ctrl-C, and on transport drop. Bound every actuation by
`ACTIVATION_DURATION_MS` (the DB even ships the duration). This is the difference between
"pulse the fan for 5 s" and "leave the fan latched on."

### 7c. Blast-radius gating — three tiers, where the line sits

```
 Tier R  READS (DTC, DID, measurement, CBS_DATEN_LESEN)         → autonomous; MCP + CLI
 ────────────────────────────────────────────────────────────────────────────────────
 Tier W1 LOW writes: counter/adaptation/learned-value resets    → CLI, single explicit confirm,
         (CBS_RESET, LERNWERTE_RUECKSETZEN, battery reg)            read-back-verify, backup-before
 ────────────────────────────────────────────────────────────────────────────────────
 Tier W2 HIGH actuation/calibration: STELLER, tests, regen,     → CLI, human-in-the-loop ONLY:
         dosing, ABGLEICH writes, ALL EMF/brake/airbag             typed confirmation + explicit
                                                                    safety preconditions acknowledged;
                                                                    never scripted, never autonomous
```

- **MCP stays read-only — no service functions at all.** This is the M4 decision and the
  CLAUDE.md blast-radius rule for the autonomous surface: the agent reads and reasons; the human
  writes. **None** of Tier W1/W2 is exposed as an MCP tool — not even the low-risk resets. (This
  supersedes the older `protocol-reference.md:332` framing that imagined confirm-gated actuation
  tools over MCP; M4 narrowed MCP to reads, and M7 keeps it there.) An agent may *recommend* "the
  oil counter is due — run `klartext service oil-reset`," but a human runs it.
- **The W1/W2 line is the physical-actuation boundary.** W1 = state-only, reversible, self-
  confirming, single confirm. **W2 = anything that moves a component or alters combustion/safety
  behavior, and is human-driven only** — typed confirmation plus an explicit acknowledgement of
  the function's preconditions (from the catalog's operator text), never wrapped in a script or
  batch. EMF and all brake/airbag/steering actuation are W2 with the additional rule that they
  are **out of scope for the first build** entirely.
- **Universal write guards** (all of W1/W2, per `protocol-reference.md:330`): enter `10 03`
  explicitly; **read + back up the original bytes before writing**; **read back + verify after**;
  always run the Reset/return-control phase (§7b); abort cleanly on any NRC and surface it.

### 7d. Crate shape (recommendation, not a build)

Keep M1–M6 intact. The write path is a new capability on `klartext-client` (the managed session
already exists) + a small `klartext-semantic` addition that parses the DDE control tables
(`STELLER`/`LERNWERTE_RUECK`/`ABGLEICH`) the way `measurement.rs` parses `SG_FUNKTIONEN`,
exposing a `ServiceFunction { name, jobs: [Phase, Rank, …], read_back }` plus a
`build_*_request()` (mirroring `measurement::build_read_request`). Confirmation/back-up/verify
live in the **CLI** (`klartext service …`), never in `klartext-mcp`. No new crate is required
for the first slice; revisit if a BEST2 interpreter is later added for non-DDE control.

## 8. Verification without an ISTA oracle (per risk category)

The ISTA VM is gone, so ISTA cannot be the oracle. How each category is validated:

- **Tier R / read-back (the oracle we DO have).** The existing M5/M6 read path is the
  verification instrument: any low-risk write is confirmed by reading the same value back. This
  is why the first slice is a reset — it is **self-verifying**.
- **W1 low-risk resets — safely on-car testable.** Offline: the request frame is unit-tested
  against a derived byte vector (as M6 did) and the table parse is hermetic. On-car: read →
  reset → read; expect the documented reset value / a visibly reset service indicator. Safe to
  run because the only effect is a counter the ECU re-derives. **In scope to validate now.**
- **W2 physical actuation — NOT safely testable without care; mostly out of scope.** There is no
  software oracle for "did the fan spin / the caliper open correctly," and the failure mode is
  physical (pinch, unintended movement, fuel pressure). These require: the car in a known safe
  state, the catalog's preconditions met by a human, eyes/hands on the component, and the
  return-control bracket proven first. **Do not** validate these in the first slice; when they
  do come, each is a deliberate, supervised, single-function bring-up — never a sweep.
- **W2 calibration writes (`ABGLEICH`) — out of scope.** Correctness depends on *external* data
  (the injector codes printed on the parts); a wrong value is silently damaging and has no benign
  read-back. Needs the real part data and is not a diagnostic-tool concern for now.
- **Anything tier-2 / ABL-bound — cannot be derived, so cannot be safely reproduced.** Without
  the compiled procedure we do not know the preconditions; reproducing the bytes blind would
  strip exactly the safety logic ISTA wraps around them. Stays out until (if ever) the BEST2
  interpreter + a captured reference make a specific function auditable.

The honest boundary: **W1 resets are verifiable now with what survived; W2 actuation is not, and
tier-2 is unreachable.** The first slice sits entirely inside the verifiable region.

## 9. Open questions / to confirm in Phase 2 (not blockers to deciding)

- **Exact SID/subfunction of the chosen first job** (`CBS_RESET` or `LERNWERTE_RUECKSETZEN`):
  `0x31` routine vs `0x2E` write — read from one BEST2 disassembly (ediabasx oracle, offline,
  never committed) or an on-car capture. The class (`0x2F`/`0x31`/`0x2E`) is settled; the bytes
  are `[verify]`.
- **Does the chosen function require `10 03` only, or also `0x27`?** Resets usually need only the
  extended session; confirm by the ECU's response (NRC `0x33`/`0x35` ⇒ security needed). If `0x27`
  is required, its seed→key is ECU-specific and a separate effort.
- **CBS central-sync** on the F20 (does the cluster reflect a DDE-local reset?) — a real-car
  observation, not an architecture question.
- **The on-car confirmation itself** (read → reset → read) — the manual HIL step, run by the
  human, deferred per project rules.

## 10. References

- `docs/sqlite-findings.md` (M3 — DB decryption + schema), `docs/sgbd-findings.md` (M6 — `.prg`
  format, BEST2, the `SG_FUNKTIONEN`/measurement win this extends), `docs/standard-pids.md` (M5).
- `docs/protocol-reference.md` — UDS service catalog (`:78–92`), extended session (`:106`),
  `STEUERN → 0x2F/0x31` (`:311`), write gating + SecurityAccess (`:330,332,340`).
- ISO 14229-1 (UDS) — `0x10`, `0x27`, `0x2E`, `0x2F`, `0x31`, `0x3E`.
- Surviving data (BYO, gitignored): `data/Testmodule(1)/{Ecu/*.prg, SQLiteDBs/DiagDocDb.sqlite,
  EcuFunctions/*.xml, Testmodule/ABL_*.dll}`. Tooling (cached SQLite3MC; an ediabasx oracle for
  Phase-2 disasm) stays offline and uncommitted, exactly as in M3/M6.
## 11. Decision (resolved)

Phase 1 ended with a decision (recorded here so Phase 2 has a clean entry point):

- **First vertical slice: the DDE engine-oil CBS reset (`CBS_RESET`, counter `0x01 = Oel`)** —
  §6 primary. Low-risk, derivable, self-confirming via `CBS_DATEN_LESEN`.
- **Write-side architecture (§7): approved as written** — `10 03` extended session + TesterPresent,
  `0x31`/`0x2E` for this slice (`0x2F`/`0x27`/`STELLER`/`ABGLEICH` deferred), the always-run
  Preset→Main→Reset return-control bracket, W1/W2 blast-radius gating, and **MCP stays read-only**
  (no service functions as agent tools).

## 12. Phase 2 status — built (CBS-reset slice + generalized catalog)

Phase 2 is implemented. The exact frames were pinned by disassembling `CBS_RESET` /
`CBS_DATEN_LESEN` / `LERNWERTE_RUECKSETZEN` with the ediabasx offline oracle (rebuilt in scratch,
never committed) — **`CBS_RESET` is `0x2E` WriteDataByIdentifier, DID `0x1001`**, payload
`2E 10 01 01 <CBS_id> 64 1F 80 00 0F FF 0F 3F FF 00` (record-count `01`, the CBSKENNUNG id, then
availability 100 % / service +1 / "no change" defaults); read-back is `22 10 01` (same DID). The
job issues **no** inline session/security/TesterPresent — ISTA opens the extended session at a
higher level, so klartext does the same. Every frame is **DERIVED, not captured — `[verify against
capture]`**, exactly as M6 Part B.

Built (test-first; `cargo fmt` + `clippy -D warnings` clean; on-car confirmation is the deferred
manual HIL step):

- **`klartext-uds`**: `routine_control` (`0x31`) and `write_data_by_identifier` (`0x2E`) request
  builders + their sub-function constants — pure ISO 14229.
- **`klartext-semantic::service_function`**: the **generalized control catalog** — parses all four
  DDE control tables (`CBSKENNUNG`, `LERNWERTE_RUECK`, `STELLER`, `ABGLEICH`) into one risk-tagged
  [`ServiceFunction`] set (the real DDE yields **156** functions), plus `build_cbs_reset_request` /
  `build_cbs_read_request`. The same shape as `measurement.rs`; degrade-quietly throughout.
- **`klartext-client`**: `reset_cbs` — enters the extended session, writes the CBS reset, reads the
  block back; validated by a loopback mock (no car).
- **`klartext-cli`**: `service list` (offline discovery, grouped by category + risk) and
  `service run <label> --confirm` — **blast-radius gated**: low-risk CBS reset runs behind
  `--confirm`; high-risk `STELLER`/`ABGLEICH` is **refused (human-only)**; `LERNWERTE` resets are
  listed but report their frame as pending capture (the oracle could not deterministically pin the
  per-width branches — honest gap).
- **`klartext-mcp`**: unchanged — still exactly the five read-only tools (asserted by test). **No
  service functions over MCP.**

**Honest scope.** The CBS-reset write path is complete and unit-tested on derived frames; what is
*not* done: on-car confirmation (the manual HIL step — read → reset → read, dashboard check),
`LERNWERTE` execution (frame unconfirmed), all high-risk actuation/calibration execution
(deliberately human-only), and deep parsing of the CBS read-back block (returned raw for now).

## 12a. Phase 3 status — engine completion + guided layer (M8)

M8 completed the engine (derive every offline-derivable frame, honestly flag the rest), added a
read-only MCP list tool, and wrote the guided-service skill. The DDE (`d72n47a0`) was disassembled
again with the ediabasx offline oracle (rebuilt in scratch, never committed). **Every frame below is
DERIVED, not captured — `[verify against capture]`.**

### Derived (single fixed telegram, cited, LOW risk → executable behind `--confirm`)

Each job builds ONE `move S1,{…}` telegram literal with exactly one `xsend` (no table-driven byte
splicing before the send), so the frame is a literal cited to one disassembly line:

| Function | Job | Frame | Service | Citation (d72n47a0) |
|---|---|---|---|---|
| CBS reset (×counter) | `CBS_RESET` | `2E 10 01 01 <id> 64 1F 80 00 0F FF 0F 3F FF 00` | 0x2E WDBI 0x1001 | tmpl `@0x969BD`; id splice from `CbsKennung.NR` `@0x96AE1` |
| MSA2 history reset | `STEUERN_MSA2HISTORIERESET` | `2E 5F 84` | 0x2E WDBI 0x5F84 | tmpl `@0x128A75`; single xsend `@0x128C0F` |
| PM histogram reset | `STEUERN_PM_HISTOGRAM_RESET` | `2E 5F F5 04` | 0x2E WDBI 0x5FF5 | tmpl `@0x16DD4E`; single xsend `@0x16DEE6` |
| DAROL load-data reset | `STEUERN_DAROL_RESET` | `2E 62 00 01` | 0x2E WDBI 0x6200 | tmpl `@0x1BBF27`; single xsend `@0x1BC0BF` |
| LLKETA reset | `STEUERN_LLKETA_RESET` | `31 01 F0 65` | 0x31 RC start 0xF065 | tmpl `@0x12D0CC`; single xsend `@0x12D264` |

The four statistic resets are standalone jobs (not in the four control tables), so they are gated on
the ECU actually defining the job — `klartext-sgbd` now parses the job directory (header `0x88`,
`0x44`-byte records) so `ServiceFunctions::from_prg` surfaces a derived reset only when
`Prg::has_job(...)` is true. CBS keeps its write + `22 10 01` read-back; the statistic resets are
one-shot (no read-back, no byte backup — they latch nothing).

### Not derivable offline (honest discovery-only status — never guessed)

| Set | Job | Why not derivable |
|---|---|---|
| Learned-value resets (18) | `LERNWERTE_RUECKSETZEN` | **Read-modify-write:** reads `22 5F D3`, then computes the `2E 5F 8A` write from the ECU's *live* response (`move S1,S3` `@0xB4938`) — a value absent offline. Also LID-width branching (`& 0xFFFF00` `@0xB39B8`), per-LID special cases (0xFA `@0xB3AD3`, 0xD9 `@0xB3B08`), two xsends. Needs an on-car capture or a BEST/2 interpreter. |
| Actuators (`STELLER`, 45) | `STEUERN_SELECTIV` | `2F FF FF 03 FF FF` template + LID/FACT_A/FACT_B value-scaling spliced from the table at run time (`@0xC238A`; `tabget FACT_A/B` `@0xC2954/@0xC2997`). HIGH risk; refused regardless. |
| Calibrations (`ABGLEICH`, 85) | `ABGLEICH_PROGRAMMIEREN_*` | Writes externally-sourced injector/part codes. HIGH risk; refused regardless. |

This resolves the M7 "`LERNWERTE` frame pending capture" gap honestly: it is not a missing capture
but a genuinely un-derivable (read-modify-write) frame — recorded, not guessed.

### Surfaced everywhere

`Derivation` (`Derived{request,cite}` | `NotDerivable{reason}`) is now a field on every
`ServiceFunction`. The CLI `service list` tags each function `[derived*]`/`[not-derivable]` and
prints the `* = UNCONFIRMED [verify against capture]` legend; `service run` executes LOW+Derived
behind `--confirm` (with the unconfirmed banner), refuses LOW+NotDerivable with the reason, and
refuses all HIGH (human-only). The MCP `list_service_functions` tool returns the full catalog
(label, description, category, risk, derivation status + citation, `runnable_in_cli`) but exposes no
frame bytes and cannot execute — the MCP surface is asserted read-only (six tools; no run/reset/
execute verb). The `skills/klartext-service` skill encodes discover→recommend→human-executes.

**Still deferred (by the user's explicit choice):** on-car confirmation of every frame (the manual
HIL step — test a LOW-risk reset first, watch the car, before trusting any frame), `LERNWERTE`
execution, and all high-risk actuation/calibration execution.

**On-car test order (when the HIL step comes).** Start with `Oel` (CBS oil reset): it self-confirms
via the `22 10 01` read-back and a visible dashboard reset. Then the statistic resets — scrutinize
`MSA2Hist` (`2E 5F 84`) first: it is a `0x2E` WriteDataByIdentifier with an **empty data record**,
which is atypical for `0x2E`, so confirm the ECU's positive `6E 5F 84` (and that it does not answer
an NRC) before trusting the other statistic frames. None of these are confirmed until run on the F20.


## 12b. M9 status — live-data discovery + the one MCP write (refined invariant)

*(2026-07-03, M9.)* Two additions and one invariant refinement:

- **`list_measurements` (MCP, read-only).** The read-side parallel to `list_service_functions`:
  the `SG_FUNKTIONEN` catalog becomes discoverable by name. The DDE defines 1787 rows (all of
  them parse/scale); a call returns at most 200 with an explicit `total` and a narrow-with-
  `search` note — truncation is never silent. `read_data` now also resolves a measurement **by
  name** (`name` + `variant`): exact, ASCII-case-insensitive, tiered ARG > RESULTNAME > INFO;
  an ambiguous name (descriptions repeat in real data, e.g. "Statuswort" ×4) errors with the
  candidate ids instead of guessing. Discover→read verified against the real `d72n47a0`: oil
  temp `ITOEL`/4517, coolant `ITKUM`/461B, DPF soot `IMRUP`/44BE, ash `IMASOEL`/44BD, regen
  status `PFltRgn_numRgn`/44BB, engine RPM `Nkw`/427F.
- **`clear_faults` (MCP, the ONLY write).** The M4 "MCP stays read-only" rule is REFINED, not
  abandoned. The real line, unchanged and absolute: no autonomous physical actuation, and no
  agent execution of derived-UNCONFIRMED frames. A **standard, well-defined, non-physical,
  reversible** diagnostic operation may be agent-invokable behind explicit confirmation —
  clearing DTCs (UDS 0x14 via the M2 `clear_all_dtcs` path, `14 FF FF FF`; no new frame)
  qualifies and is the only member today. The tool refuses without `confirm=true` (before even
  the connection check), pre-reads and echoes the codes it discards, and its description warns
  that freeze-frame/snapshot data dies and readiness monitors may reset. Service-function
  execution and actuation remain permanently out of MCP.
- **The surface test asserts the refined invariant**: exactly eight tools; "clear" appears only
  as `clear_faults`; no actuate/execute/run/reset/write/code/flash verb in any tool name; and —
  behaviorally, against a frame-recording mock — the confirmed clear path sends only ISO-standard
  UDS (`19 02 FF`, `10 03`, `14 FF FF FF`), never a derived frame.
