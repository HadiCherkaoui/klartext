# P3 service-function execution — ISTA parity research

> **Research record, 2026-07-19.** Produced under the 1:1 ISTA parity mandate.
> Every claim is cited to decompiled ISTA source or ECU `.prg` bytecode
> disassembled with klartext's own `klartext-best`. Citations reference a local
> `<scratchpad>` of `ilspycmd` output over `data/TesterGUI/bin/Release/` — BYO
> data, gitignored, never committed. Regenerate locally to follow a citation.


---

## OWNER RULINGS 2026-07-19 — the three design forks, decided

1. **Teardown on failure: KEEP THE SAFE TEARDOWN (deliberate divergence from ISTA).**
   On a failed Main, klartext commands the component back to safe (`returnControlToECU`) before
   returning, rather than skipping teardown as ISTA does. The research proved ISTA's skip is only
   safe because its session eventually drops (S3 ~5s), and klartext keeps the session alive with
   TesterPresent — so matching ISTA would leave a latching force ENERGISED until disconnect. The
   owner ruled to fail safe. Record it in code as an agreed divergence; do not "fix" it toward ISTA.

2. **Infinite hold: HOLD, REQUIRE AN EXPLICIT STOP.** For the 3,989 functions ISTA holds until a
   human presses Stop, klartext exposes a start/stop pair rather than a bounded cap: `start`
   actuates and holds (component forced), a separate `stop` commands teardown. Closer to ISTA than
   a timed cap. **Consequence the owner accepted:** an agent that starts a hold and never calls stop
   leaves the component forced until the session drops — so ruling 1's safe-teardown is the backstop,
   and stop MUST also fire on disconnect. Timed holds (67) and no-hold functions keep their behaviour.

3. **Preconditions: SURFACE ISTA'S TEXT, ADVISE ONLY.** Drop `defaults_for(category)` — a klartext
   invention with no ISTA counterpart. ISTA machine-checks NONE of its preconditions; they are prose
   operator-text it shows the human. klartext surfaces that text and does not programmatically block;
   an unresolvable check degrades to advisory, never to a refusal.

Scope: how ISTA runs a component-trigger / service function, verified 1:1 against the
binary and the ECU bytecode, and the concrete change list for `crates/service`.

Primary sources (all re-verified for this doc, not taken from prior hand-offs):
- `RheingoldSessionController.dll` decompiled → `<scratch>/p3dec/RheingoldSessionController.decompiled.cs`
- `RheingoldCoreFramework.dll` decompiled → `<scratch>/p3dec/RheingoldCoreFramework.decompiled.cs`
- DDE SGBD bytecode `data/Testmodule(1)/Ecu/d72n47a0.prg` disassembled with klartext's own
  `klartext_sgbd` + `klartext_best` (probe `<scratch>/stprobe`)
- Built semantic DB `data/klartext-semantic.db` and encrypted source `DiagDocDb.sqlite`
  (rc4, key `6505EFBDC3E5F324`, via the repo's cached `sqlite3mc`)

`<scratch>` = `<scratchpad>`

---

## The executor, in full (the ground truth)

`EcuFunctionComponentTrigger.DoTriggerComponent` — `RheingoldSessionController.decompiled.cs:12123`.
Body, lines 12129-12183:

```
try {
    terminationEvent.Reset();
    ...= PreparingOperatorText;   PhasePreset(ecuKom, ecu, fn);      // 12134
    ...= ProcessingOperatorText;  PhaseMain(ecuKom, ecu, fn);        // 12136
    int num  = (int)fn.Activation;                                  // 12137
    int num2 = (int)fn.ActivationDurationMs;                        // 12138
    int num3 = (num > 0) ? num2 : -1;                               // 12139
    if (!fn.GetJobsByPhase("Reset").Any()) { num3 = 0; ... }        // 12140-12144
    bool flag = terminationEvent.WaitOne(num3);                     // 12146
    ...= PostOperatorText;        PhaseReset(ecuKom, ecu, fn);       // 12148-12149
    WriteFasta(ecu, error:false, fn);  return true;                 // 12150-12151
}
catch (EDIABASEcuKomException ex) { ...show error...; WriteFasta(ecu, error:true, fn); } // 12153
catch (Exception exception)       { Log.WarningException(...); }                         // 12179
return false;
```

Each `Phase*` method (`PhaseReset` :12222, `PhasePreset` :12254, `PhaseMain` :12287) is:

```
foreach (XEP_ECUJOBSEX item in from x in fn.GetJobsByPhase("<phase>") orderby x.Rank select x) {
    string p = item.GetParameterString();
    IEcuJob job = ecuKom.ApiJob(ecu.VARIANTE, item.Name, p, "");
    if (job.IsOkay()) { ...copy results...; continue; }
    throw new EDIABASEcuKomException(job);      // MAIN :12316, PRESET :12283, RESET :12250
}
```

---

## Q1 — `GetJobsByPhase` is plural, ordered by Rank  ✅ divergence CONFIRMED

- **Plural**: `GetJobsByPhase` returns `ObservableCollection<XEP_ECUJOBSEX>` and adds
  every job whose `Phase` matches — `RheingoldCoreFramework.decompiled.cs:55335-55349`.
  The phase compare is **case-insensitive** (`StringComparison.OrdinalIgnoreCase`,
  :55342), which is why `PhaseMain` can pass the literal `"MAIN"` (:12290) and still
  match rows stored as `"Main"`.
- **Ordering key = `Rank`**: each phase loop is `... orderby x.Rank select x`
  (:12224-12226 / :12257-12259 / :12290-12292). `XEP_ECUJOBSEX.Rank` is `decimal?`
  (:57079). LINQ `OrderBy` is a stable sort and puts `null` first — so rank-less rows
  keep catalog order ahead of ranked ones.
- **Rank IS in klartext's semantic DB**: `job_param.rank`, extracted from
  `XEP_REFECUJOBS.RANK` (`scripts/build-semantic-db.sh:150`; the phase comes from the
  same JOB record `rj.PHASE`, :149 — authoritative over the parameter-ref phase, per the
  script's own comment :74-80). `Catalog::job_parameters` already emits
  `ORDER BY function_id, phase, rank, position` (`crates/semantic/src/catalog.rs:695`).
  Extraction query is exactly:
  ```sql
  SELECT function_id, function_en, function_de, phase, rank, position, value, label
  FROM job_param WHERE ecu_variant = ?1 AND job = ?2
  ORDER BY function_id, phase, rank, position
  ```
- **Failure inside a multi-job phase → ISTA ABORTS, does NOT continue.** The phase
  method `throw`s `EDIABASEcuKomException` on the first job whose `IsOkay()` is false
  (`:12316`/`:12283`/`:12250`). That propagates out of `DoTriggerComponent` into the
  catch — remaining jobs in the phase, all later phases, and the reset are ALL skipped.

**But klartext cannot represent a multi-job phase at all.** `Catalog::job_parameters` is
**job-keyed** (`WHERE ... AND job = ?2`, catalog.rs:694) and `JobParameterEntry` carries
**no job name** (catalog.rs:170-191). `phase::invocations` then groups rows by
`(function_id, phase)` only — rank and job name are dropped from the grouping key
(`crates/service/src/phase.rs:79-96`) — and `run_cycle` runs **one** externally-supplied
`job: &str` for every phase (`runner.rs:104`, `:113`). Consequences, measured on
`data/klartext-semantic.db`:

| case | groups | what klartext does wrong |
|---|---|---|
| `(variant,fn,phase)` with >1 **distinct job name** | **54** | queries one job, silently DROPS the other — e.g. `ccu_01` Main = `DIAGNOSE_MODE` then `STEUERN_IO`: klartext actuates **without entering diagnose mode** |
| `(variant,fn,phase)` with the **same job at >1 rank** | **519** | coalesces two separate invocations into ONE `;`-buffer → sends a malformed single command instead of two sequential ones |
| total multi-invocation groups (>1 rank) | 573 | |
| total `(variant,fn,phase)` groups | 23,339 | |

Real examples (`GROUP_CONCAT(DISTINCT job)` per group): `ccu_01/ccu_06/ccu_22/ccu_p1`
Main = `DIAGNOSE_MODE,STEUERN_IO`; `cdm02*` Main = `STATUS_LESEN,STEUERN_ROUTINE`.
Rank histogram: rank1 = 62,236 rows, rank2 = 1,334, tail out to rank 13.

Note: **the DDE `d72n47a0` (the owner's own engine ECU on both cars) has ZERO multi-job
and ZERO multi-rank phases** — every DDE service function is single-job/single-rank. The
divergence is real and must be fixed for 1:1 on *any* car, but it does not bite the DDE.

---

## Q2 — ISTA HOLDS after Main  ✅ divergence CONFIRMED

Semantics (`:12137-12146`), units **milliseconds**:

- `num3 = (Activation > 0) ? ActivationDurationMs : -1`, then **`if no Reset jobs → num3 = 0`**.
- `terminationEvent.WaitOne(num3)`:
  - `num3 > 0` → hold that many ms (or until the user stops earlier).
  - `num3 == -1` → **infinite** wait (`Timeout.Infinite`) — blocks until the user clicks
    stop, which calls `StopEcuTriggerTask` → `terminationEvent.Set()` (`:12082-12092`).
  - `num3 == 0` → returns immediately, **no hold**.
- Only AFTER the wait does `PhaseReset` run (:12149). So the actuator is energised by Main,
  held for the window, then switched off by Reset.

**Source of the numbers:** `XEP_ECUFIXEDFUNCTIONS.ACTIVATION` and `.ACTIVATION_DURATION_MS`
(class `RheingoldCoreFramework.decompiled.cs:52847`, properties `:54603` and `:54619`).
**NOT extracted into klartext's semantic DB today** — `job_param` has no such columns and
the build script never selects them. klartext literally cannot know the hold time. This is
a required new extraction (they are function-keyed, not job-keyed — see change list D2).

**Real distribution over trigger functions** (functions with ≥1 scheduled `XEP_REFECUJOBS`),
from the source DB:

| ISTA hold behaviour | Activation | has Reset job | # functions |
|---|---|---|---|
| **no hold** (`num3=0`, fire & return) | 0 | no | 31,244 |
| **no hold** (Activation=1 but no-Reset rule forces `num3=0`) | 1 | no | 240 |
| **INFINITE, until user presses Stop** | 0 | yes | **3,989** |
| **timed hold then Reset** | 1 | yes | 67 |

Query used:
```sql
WITH ff_jobs AS (
  SELECT ff.ID fid, ff.ACTIVATION act, ff.ACTIVATION_DURATION_MS dur,
         MAX(CASE WHEN UPPER(rj.PHASE)='RESET' THEN 1 ELSE 0 END) has_reset
  FROM XEP_ECUFIXEDFUNCTIONS ff JOIN XEP_REFECUJOBS rj ON rj.ID = ff.ID
  GROUP BY ff.ID)
SELECT act>0, has_reset, COUNT(*) FROM ff_jobs GROUP BY act>0, has_reset;
```
Raw `ACTIVATION_DURATION_MS` values when Activation=1: 5000 (130), 10000 (56), 2000 (41),
30000 (20), 20000 (15), 4000 (11), 4500 (8), 8000, 3000, 1000, 15000, 11000, 12000, 31000…

**"Indefinitely" operationally (the design problem):** for the 3,989 infinite functions
ISTA parks the trigger thread on `WaitOne(-1)` and the *technician* ends it by toggling the
component-trigger switch off (`HandleResponse`/`ToggleComponentTrigger` → `StopEcuTriggerTask`).
klartext's surfaces are the MCP server and a mobile app — **there is no live human holding a
button inside a single tool call.** So "match ISTA" cannot mean "block forever". This needs
an owner decision (change list D3): a bounded default hold, and/or a `start`/`stop` tool pair
so the agent (or app UI) issues the explicit stop the way ISTA's switch does.

**klartext today does the opposite of holding:** `run_cycle` runs Reset *immediately* after
Main with no wait (runner.rs:97-132). For the 67+3,989 = **4,056** reset-bearing functions
that flicks the actuator on and straight off. (The DDE has none of these — see Q3.)

---

## Q3 — ISTA does NOT tear down on failure  ✅ CONFIRMED — and the safety answer

**Control flow (confirmed):** `DoTriggerComponent` is a single `try` (`:12129`) whose
`PhaseReset` call sits at `:12149`, INSIDE the try, AFTER the `WaitOne`. There are two
`catch` blocks (`:12153`, `:12179`) and **no `finally`**. Neither catch calls `PhaseReset`.
Any phase that throws (its first non-OK job) therefore **skips teardown entirely** — the
`2F…03` force that Main applied is never followed by the `2F…00` return-to-ECU.

### The self-termination mechanism — established, at the protocol level

I disassembled the DDE fan pair myself (`<scratch>/stprobe`, request literal is
`move S1, {…}` at offset 0x10; each job has exactly **one** `xsend`):

| job | phase | transmitted UDS | control option |
|---|---|---|---|
| `STEUERN_E_LUEFTER` | on (Main) | `2F FF FF 03 FF FF` | **03 = shortTermAdjustment** |
| `STEUERN_E_LUEFTER_AUS` | off | `2F FF FF 00` | **00 = returnControlToECU** |
| `STEUERN_GLUEHSTEUERGERAET` | — | `31 01 F0 64 FF FF` | RoutineControl startRoutine |

(The `FF FF` DID is a placeholder the bytecode overwrites from the `Steller`/`ELU` table via
the `_REQUEST` tag; `03` vs `00` is the load-bearing, fixed difference. The `settmr 0x400` /
`wait` / `gettmr` block is IDENTICAL in the on and off jobs, so it is the **comms
response-wait**, not an actuation-hold timer.)

So `2F…03` is a **latching I/O force**. Per ISO 14229 it holds until either an explicit
`returnControlToECU` (`2F…00`, the Reset job) OR the **diagnostic session reverts** — i.e. the
S3 timeout (~5 s with no TesterPresent) drops the ECU to the default session and returns I/O
control automatically. That session-layer revert is the ONLY "self-termination" I can
establish; **it is NOT a per-actuation duration timer in the SGBD.** RoutineControl
(`31 01…`) actuations are a different shape — some self-complete, some need an explicit
`31 02` stopRoutine (cf. the `STEUERN_ROE_START`/`STEUERN_ROE_STOP` pair) — I cannot assert
all of them self-terminate.

### The honest safety answer

**YES — if klartext matches ISTA and skips teardown on failure, a component CAN be left
actuated.** The mitigating "self-termination" is the UDS session S3 revert, and it fires only
once the tester **stops** sending TesterPresent. klartext keeps the session alive with
TesterPresent (the keep-alive `XEnet32/64.dll` owns on ISTA's side; klartext's own keep-alive
on ours). So a latching `2F…03` actuation whose teardown was skipped stays forced **for as
long as klartext holds the session open** — it reverts only when klartext disconnects or lets
the session lapse. There is no documented SGBD timeout that saves it in the meantime.

Therefore matching ISTA's skip-on-failure is **not categorically safe**; its safety is
entirely contingent on the session then dropping. klartext's *current* always-run-Reset
behaviour (`runner.rs:109-132`) is **safer than ISTA**. My recommendation for the owner
(change list D4): on a failed cycle, **keep an explicit return-to-safe** — send the Reset
jobs' `2F…00` (or, if a phase job itself failed mid-way, at minimum force a session teardown
so S3 fires). This is a deliberate, owner-approvable divergence FROM ISTA **in the safe
direction**, and the owner should choose it knowing precisely what ISTA does and does not do.

---

## A — Preconditions

**ISTA machine-checks NOTHING before a service function.** `DoTriggerComponent` reads no
gauge; the caller chain (`ToggleComponentTrigger` → `StartEcuTriggerTask` :12095) adds no
gate. `XEP_ECUFIXEDFUNCTIONS` has no precondition column — its non-title columns are
`SICHERHEITSRELEVANT` (=0 for all 35,555 rows), `…RELEVANT` flags, `ACTIVATION`,
`ACTIVATION_DURATION_MS`, `PARENTID`, `SORT_ORDER`, and the `PREPARING/PROCESSING/POST`
operator-text trio. Those texts are **prose shown to the technician** (`= PreparingOperatorText`
is assigned to a UI message at `:12133`), e.g. *"Ansteuerung für 20 Sekunden"*, *"Klemme 15
und Klemme R ausschalten!"* — imperatives, not structured conditions. The only ISTA voltage
check that ever *refuses* is on the **programming/flashing** path (`RheingoldProgramming` /
`CheckClamp30`), which is out of klartext's scope; the diagnosis-side voltage watchdog
(`CheckVoltageOfVciDeviceForDiagnosis` :3860) only WARNS and is not on the actuation path.

**klartext's `defaults_for(category)` is an INVENTION with no ISTA counterpart.** Its own
header says so: *"ISTA's preconditions are NOT machine-readable … So klartext defines its
own"* (`crates/service/src/precondition.rs:1-5`). The thresholds (12.0 V, stationary,
terminal-on) and the category→checks mapping exist nowhere in ISTA. `run_service` then
*blocks* on a resolved failure (`runner.rs:188-199`) — klartext is **stricter** than ISTA,
which would proceed. This is not a hidden defect (it's documented), but under the 1:1 mandate
it cannot be cited to ISTA. **Owner decision (D5):** keep the invented gate (defensible for an
autonomous agent surface with no human watching a live gauge) or drop it to 1:1 (show operator
text, actuate on confirm, gate on nothing). The `defaults_for` seam itself is fine as a seam;
the question is whether its content is policy klartext is allowed to add.

## B — Security access (0x27)

**NO.** Service functions do not require UDS SecurityAccess. Two independent lines:
(1) the C# trigger path contains no security/unlock call (grep of the whole
`RheingoldSessionController.decompiled.cs` for `securityaccess|freischalt|seed|0x27` = 0 hits
in the trigger path; the DLL has 0 such strings in ASCII or UTF-16); (2) DDE `STEUERN_*`
bytecode emits only `2E/2F/31/2D/30` — never `27` — and a whole-DB scan of scheduled phase
jobs (`XEP_REFECUJOBS`, 44,559 rows) finds no `FREISCHALTEN`/`AUTHENTISIERUNG`/`SECURITY_ACCESS`
control job in any Preset/Main/Reset phase; the six `…SCHLUESSEL…`/`FREISCHALTUNG` hits are all
`STATUS_*` reads of immobiliser state. 0x27 lives only in the coding/programming/flashing
subsystem, which klartext does not implement. **Not a blocker for the service-write tier.**
(Seed/key jobs DO exist standalone on some ECUs — ABS/DSC/airbag `AUTHENTISIERUNG_START` — but
are never invoked from a trigger phase. `XEnet32/64.dll` is native PE and unread, but it owns
keep-alive timing, not per-job security gating, and the ECU bytecode is the demand side.)

## C — The argument buffer

**Per-JOB, and klartext's format matches — but its grouping does not.** `GetParameterString`
(`RheingoldCoreFramework.decompiled.cs:57105-57118`) sorts *that job's own* `Parameters` with
`XEP_ECUJOBSEX_ParameterSequenceComparer` and `;`-joins with a trailing-`;` trim. The comparer
is `SortUtils.CompareString(x.Name, y.Name, ignoreLength:false)` (`:56921`, `:16166`), which
compares **length first, then ordinal** (`:16180-16188`) — so `"P1".."P9"` (len 2) sort ahead
of `"P10"` (len 3): exactly **numeric** order. klartext's `CAST(SUBSTR(name,2) AS INTEGER)`
numeric position sort (build-semantic-db.sh:151) therefore **reproduces ISTA's order** and the
`;`-join is right. The defect is only that klartext builds **one buffer per `(function, phase)`**
whereas ISTA builds **one per JOB** (and re-runs `ApiJob` per job/rank). Correct whenever a
phase has exactly one job at one rank; wrong for the 573 multi-invocation groups (Q1).

---

## D — Concrete change list

`crates/service` has **no consumer** (nothing in the workspace depends on `klartext-service`;
verified). So this is the moment to reshape it, at zero call-site cost.

**D0 — the shape is wrong: the cycle must be FUNCTION-keyed, not JOB-keyed.** ISTA runs a
`XEP_ECUFIXEDFUNCTIONS` (one UI action) and, per phase, runs *whatever jobs that function
schedules*, by name, in rank order. klartext is built around a single external `job: &str`.
Re-key the whole path on `(variant, function_id)`.

**D1 — semantic layer (`crates/semantic/src/catalog.rs`):**
- Add `job: String` to `JobParameterEntry` and SELECT it (the `job_param.job` column already
  exists in the extract — no rebuild needed for this part).
- Add a **function-keyed** query
  `job_parameters_for_function(variant, function_id) -> Vec<JobParameterEntry>` returning ALL
  rows for that function across every phase/job/rank, `ORDER BY phase, rank, position`. Keep
  the existing job-keyed one for read jobs / callers that want it, or retire it — its only use
  was this crate.

**D2 — new extraction: the hold parameters (needs a DB rebuild).** `ACTIVATION` /
`ACTIVATION_DURATION_MS` are on `XEP_ECUFIXEDFUNCTIONS`, keyed by function id. Add to
`scripts/build-semantic-db.sh` either two columns on a per-function table or a small
`fixed_function(function_id, activation, activation_duration_ms)` table:
```sql
CREATE TABLE sem.fixed_function AS
  SELECT DISTINCT ff.ID AS function_id,
         CAST(ff.ACTIVATION AS INTEGER)            AS activation,
         CAST(ff.ACTIVATION_DURATION_MS AS INTEGER) AS activation_duration_ms
  FROM XEP_ECUFIXEDFUNCTIONS ff
  WHERE ff.ID IN (SELECT ID FROM XEP_REFECUJOBS);
```
Surface via a `Catalog` accessor keyed by `function_id`. (Owner rebuild step, one command.)

**D3 — phase model (`crates/service/src/phase.rs`):** an `Invocation` must become
**one job run**, carrying its own `job: String`, `rank`, and `args`. Group rows by
`(function_id, phase, rank)` — NOT `(function_id, phase)` — and within a phase order
invocations by `rank`. `invocations()` for one function then yields the ordered list ISTA's
three `GetJobsByPhase(...).OrderBy(Rank)` loops would produce.

**D4 — runner (`crates/service/src/runner.rs`):**
- Run `Preset*` (all, by rank) → `Main*` (all, by rank), each invocation with its **own** job
  name and buffer via `runner.run(&inv.job, target, &inv.args)`. Drop the single `job: &str`
  parameter.
- **Abort semantics = ISTA**: on the first failing job in a phase, stop the phase and skip
  later phases (ISTA throws and unwinds). Keep this.
- **Hold**: after Main, hold for `activation_duration_ms` when `activation>0`, `0` when the
  function has no Reset invocations, else the infinite case → **D3 policy decision** (bounded
  default, e.g. cap infinite at a configured max, and/or expose a `stop` path). Do NOT
  `WaitOne(-1)` on an agent/app surface.
- **Teardown (the safety divergence)**: unlike ISTA, run the Reset invocations on the failure
  path too (return-to-safe), and keep reporting `Teardown::Failed`. Document this as an
  explicit, owner-approved divergence from ISTA per Q3. At minimum, guarantee session teardown
  on failure so the S3 revert fires. This is the one place we deliberately do NOT match ISTA.
- `Teardown` / `PhaseOutcome` extend cleanly to per-invocation (job name + rank + error).

**D5 — preconditions:** owner decision (A). If kept, keep `defaults_for` as klartext policy
and label it as such in the surface (not "ISTA requires"); if dropped for 1:1, replace the
gate with surfacing `PreparingOperatorText` to the human and gating only on the relayed
`confirm=true`. Either way, stop implying ISTA machine-checks these.

**D6 — surfaces:** the MCP tool and app flow become: list functions for an ECU (function id +
title + phase/hold summary + operator text) → human/agent picks a `function_id` → run the
function-keyed cycle behind `Policy::ConfirmedWrite`. No single "job name" is ever the unit of
a service write.

---

## Unreadable / not established (named, not guessed)
- **Per-actuation firmware self-termination** — the ECU's compiled firmware is not on disk;
  the SGBD sends one UDS request and the hold/latch lives in the ECU. The only self-terminate
  mechanism I can establish is the UDS **session** S3 revert (protocol-level), not a per-job
  timer. RoutineControl (`31`) self-completion could not be asserted for all jobs.
- **`XEnet32/64.dll`** — native PE (TesterPresent cadence, NRC 0x78 retry). Not decompiled; it
  governs keep-alive, and keep-alive is exactly what prevents the S3 revert during a hold.
- **KMM rules engine / PSdZ Sollverbauung** — interface-only in managed code; not relevant to
  the trigger path (they govern fitment/coding).
