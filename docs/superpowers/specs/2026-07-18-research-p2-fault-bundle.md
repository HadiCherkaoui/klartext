# P2.2 + P2.3 — ISTA's fault-read bundle and freeze-frame sourcing

> **Research record, 2026-07-18.** Produced under the 1:1 ISTA parity mandate
> (see `CLAUDE.md`): every claim is cited to decompiled ISTA source, ECU `.prg`
> bytecode disassembled with klartext's own `klartext-best`, the shipped
> `EDIABAS.INI`, or a query against real data. Citations reference files under a
> local `<scratchpad>` produced by `ilspycmd` over `data/TesterGUI/bin/Release/`
> and by `klartext_best::decode_job` over `data/Testmodule(1)/Ecu/*.prg` — BYO
> data, gitignored, never committed. Regenerate them locally to follow a citation.
>
> This is a research record, not a plan of record. Where it corrects the parity
> audit, the audit has been updated; where it recommends work, that work is
> tracked separately.


Research spec. Every claim carries a `file:line` citation into decompiled ISTA source, a
disassembly of real ECU bytecode, or a query run against real data. Implementer works from
this file alone.

## Citation key

| Short form | Absolute path |
|---|---|
| `VehicleIdent.cs` | `<scratchpad>/decompiled/VehicleIdent.cs` |
| `DiagnosticsBusinessData.cs` | `<scratchpad>/decompiled/DiagnosticsBusinessData.cs` (decompiled during this task from `data/TesterGUI/bin/Release/DiagnosticsBusinessData.dll`) |
| `is_lesen.txt`, `fs_lesen.txt`, `fs_lesen_detail.txt`, `is_lesen_detail.txt`, `zfs.txt` | `<scratchpad>/bytecode/` — `klartext_best::decode_job` disassembly, full-range |
| `.prg` sources | `data/Testmodule(1)/Ecu/{d72n47a0,cas4_2,zgw_01}.prg` |

Bytecode offsets below are the hex offsets printed in the dumps, not line numbers.

---

## HEADLINE — the starting hypothesis is REFUTED

> "plain `FS_LESEN` already populates `F_UW_KM` / `F_UW_ZEIT` / `F_UW_ANZ` / `F_UW[]` per DTC
> (`:2834-2991`) — no second round trip"

**This is false for F20/F25.** Three independent proofs:

1. **`FS_LESEN` has exactly ONE `xsend`, and it emits no `F_UW*` result at all.**
   `fs_lesen.txt:1` — `### JOB FS_LESEN bytes=7782 ops=1472 xsend=1`. A grep for `F_UW`
   across the full 1,472-op dump returns **zero** hits, on both `d72n47a0` and `cas4_2`.
   The single request is the literal `19 02 0C` (`fs_lesen.txt` @ `000019`).

2. **The C# block at `VehicleIdent.cs:2874-2905` is real but is dead code on this car.**
   It sits inside `ReadFSLesenDetailInfo` (`VehicleIdent.cs:2834`) behind:
   ```csharp
   // VehicleIdent.cs:2865
   if (mECU.DiagProtocoll == typeDiagProtocoll.DS2 || mECU.DiagProtocoll == typeDiagProtocoll.UNKNOWN || !dTC.F_ART.HasValue)
   ```
   F20/F25 ECUs are `typeDiagProtocoll.UDS` and `F_ART` is populated from the `19 02` status
   byte, so the branch never runs. The identical guard appears in the sibling handler
   `ReadFSLesenBasicInfo` at `VehicleIdent.cs:2749`. This is the **legacy DS2/pre-UDS path**.

3. **Freeze-frames come from `FS_LESEN_DETAIL`, which is a separate job issued once per fault.**
   `fs_lesen_detail.txt:1` — `xsend=3`. Called per-DTC at `VehicleIdent.cs:3167`.

**Consequence for the implementer: klartext does NOT get freeze-frames for free.** klartext's
existing `read_fault_detail` is the architecturally correct mechanism and must stay. The work
in P2.3 is *fixing* it (order + gating, see §C), not replacing it with an inline decode.

Cost model, exact: **1 request per ECU** for the fault list, **+2 requests per fault** for
freeze-frame detail on the F25's DDE (3 on ECUs where `F_SEVERITY=ja`).

---

## A. The exact bundle

### A.1 The sequence

Four jobs, plus a conditional fifth. The lead's brief omitted `DoECUReadISDetails`.

| # | Operation | Job(s) | Wire | Per | Condition |
|---|---|---|---|---|---|
| 1 | `DoECUReadFS` | `FS_LESEN` | `19 02 0C` | ECU | `ECU_GRUPPE` non-empty; `DoEcuJob` passes |
| 2 | `DoECUReadFSDetails` | `FS_LESEN_DETAIL` | `19 09`/`19 06`/`19 04` | **fault** | `ECU_ASSEMBLY_CONFIRMED` && `!IsVirtual` |
| 3 | `DoECUReadIS` | `IS_LESEN_FUNKTIONAL` then `IS_LESEN` | `22 20 00` | ECU | functional first, physical fallback |
| 4 | `DoECUReadISDetails` | `IS_LESEN_DETAIL` | `22 20 00` + `22 20 nn` | **info entry** | `Relevance==true \|\| readInfoFaultDetails` |
| 5 | `DoECUReadZFS` | `STATUS_ZFS_LESEN_GESAMT` | 8 xsends (see §A.4) | vehicle | `G_ZGW` present && **not** new-generation |

### A.2 Is it fixed? — the ORDER is invariant, the SURROUNDINGS are not

Across all 13 call sites, the relative order `FS → FSDetails → IS → ISDetails → (ZFS)` never
varies. What varies is which identification/serial reads are interleaved, and whether ZFS runs.

Call-site inventory:

| `VehicleIdent.cs` | Method / branch | Deviation from the bare sequence |
|---|---|---|
| 438-455 | `PerformVehicleTestOem` | inserts `DoECUReadPhysicalHWNR` + `AIF` + `Serial` between FSDetails and IS; **no ZFS** |
| 6171-6186 | `ReadErrorInfoMemoryVehicle(IProgressMonitor)` `[Obsolete]` | bare sequence + ZFS + `ReadCheckControlMessages` |
| 6197-6208 | `ReadErrorInfoMemoryMotorcycle` `[Obsolete]` | bare sequence, **no ZFS** |
| 7927-7941 | quick test | `IStufeLesenShortTest` + `ECUIdentShortTest` first; ZFS double-gated (see A.3) |
| 8022-8038 | `BNType.BEV2010` | SVK first; HWNR/AIF/Serial/HWReferenz between |
| 8041-8058 | `BNType.BN2000` | `PerformJobFlashProgrammierstatusLesenFunktional` first |
| **8060-8090** | **`BNType.BN2020` — F20/F25 land here** | `ReadSvkDuringVehicleTest` first; `SaeContextBuilder` + `CheckEnergyModeActive` + `HandleMainSeriesSgbdAdditional` + `DoECUReadSerial` between FSDetails and IS; **ZFS + `ReadCheckControlMessages` at the end** |
| 8100-8110 | `BNType.BNK01X_MOTORBIKE` | empty group SGBD, `tryFunctional: false` |
| 8117-8124 | `BNType.IBUS` | empty group SGBD, `tryFunctional: false` |
| 8127-8140 | `BNType.BN2000_MOTORBIKE` | `DoECUReadISDetails(2u)` — bus id **2**, not 0 |
| 9794-9807 | `ReadErrorInfoMemoryVehicle(IInteractionJobService)` — the live one | bare sequence + ZFS + `ReadCheckControlMessages` |
| 9818-9830 | `ReadErrorInfoMemoryMotorcycle(IInteractionJobService)` | bare sequence, no ZFS |

**The F20/F25 reference path is `VehicleIdent.cs:8060` (`case BNType.BN2020`).**

### A.3 The ZFS gate — the brief's guess is INVERTED; ZFS DOES run on F20/F25

`DoECUReadZFS` (`VehicleIdent.cs:4448`) self-gates at `:4456`:
```csharp
if (diagnosticsBusinessData.IsVehicleInNewGeneration(VecInfo)) { return null; }
```
i.e. it is skipped for **new**-generation vehicles. And:
```csharp
// DiagnosticsBusinessData.cs:998
public bool IsVehicleInNewGeneration(IVehicle vecInfo) {
    if (!vecInfo.Classification.IsSp2021 && !vecInfo.Classification.IsSp2025)
        return newFaultMemoryEnabledESeriesLifeCycles.Any(eslc => eslc.Equals(vecInfo.ESeriesLifeCycle, ...));
    return true;
}
// DiagnosticsBusinessData.cs:294
private readonly string[] newFaultMemoryEnabledESeriesLifeCycles =
    { "F95-1", "F96-1", "G05-1", "G06-1", "G07-1", "G09-0", "G18-1", "RR25-0" };
```
F20 and F25 are in neither the SP2021/SP2025 classification nor that 8-entry list →
`IsVehicleInNewGeneration == false` → **ZFS is NOT skipped**.

The second gate is ZGW presence, which the F25 satisfies. Verified against the real semantic DB:
```
$ sqlite3 data/klartext-semantic.db \
    "select * from ecu_tree where series='F25_1404' and name like '%ZGW%';"
F25_1404|16|ZGW|G_ZGW|ROOT||6|0|1
```
The quick-test site adds a third gate, `DoEcuJob` (`VehicleIdent.cs:7937`):
```csharp
if (VecInfo.getECUbyECU_GRUPPE("G_ZGW") != null && DoEcuJob(VecInfo, "G_ZGW", "STATUS_ZFS_LESEN_GESAMT"))
```
The BN2020 vehicle-test site (`:8085`) calls `DoECUReadZFS(RetryCount)` with no outer gate,
relying on the internal one.

### A.4 ZFS is expensive and its wire protocol is NOT statically resolvable

`zfs.txt:1` — `### JOB STATUS_ZFS_LESEN_GESAMT bytes=69093 ops=13615 xsend=8`, disassembled
from `zgw_01.prg`. The eight telegrams are assembled from `S2`/`S3`/`S4` registers populated
by table lookups, not from byte literals; a census of every `S<n>, {…}` literal in the job
yields only ASCII (`"DM01;"`, `"DM02;"`, `"DM03;"`, `"DM??;"`, `"SGBD"`, `"ORTTEXT"`,
`"CC_MSG"`, scaling strings) — **no UDS request literal appears**. I could not statically
resolve which services it sends. See §Unreadable.

`DM` = *Diagnose-Meldung*; the results are `STAT_DM_ADRESSE_SG`, `STAT_DM_MELDUNG_NR`,
`STAT_DM_MELDUNG_TEXT`, `STAT_DM_MELDUNG_TYP`, `STAT_DM_SGBD_INDEX`
(`VehicleIdent.cs:4472-4480`) — a gateway-held *central* message memory, a different data
model from per-ECU DTCs.

**Recommendation: ZFS is out of scope for P2.2/P2.3.** It is one ECU, 8 round trips, an
unresolved wire protocol, and a new result type. File it as its own parity item.

### A.5 `FS_LESEN_EXPERT` does not run on F20/F25

`DoECUReadFS` conditionally follows `FS_LESEN` with `ExecuteFSLesenExpert`
(`VehicleIdent.cs:2698-2706`). That is variant-whitelisted:
```csharp
// DiagnosticsBusinessData.cs:1160
if (fsLesenExpertVariants.Any(v => v.Equals(variant, ...)))
    return ecuKom.ApiJobWithRetries(variant, "FS_LESEN_EXPERT", ";0x2C;0x20", string.Empty, retries);
```
The list (`DiagnosticsBusinessData.cs:225-231`) is 28 modern variants — `PCU48`, `DME9FF_R`,
`IB_G70`, `HVS_02`, … — and contains **neither `d72n47a0` nor `cas4_2`**. Correspondingly
`FS_LESEN_EXPERT` does not even exist in `d72n47a0.prg` (it is `cas4_2`-only, and unreachable
there for the same reason). **Ignore it.**

### A.6 What is done with each result — MERGED, not kept separate

- `FS_LESEN` → `mECU.FEHLER` (`VehicleIdent.cs:2944`), each DTC passed through
  `PlausibilityCheck.HandleRelevanceFlagAndFaultCodeForDTC` (`:2943`).
- `FS_LESEN_DETAIL` → mutates the **same** `DTC` object in place, filling `DTCContext`
  (`VehicleIdent.cs:3186-3262`).
- `IS_LESEN` → `eCU.INFO`, a parallel collection (`VehicleIdent.cs:3479`), **and** relevant
  info entries are additionally pushed into the fault list:
  ```csharp
  // VehicleIdent.cs:3535-3538
  DTC dTC2 = (DTC)new PlausibilityCheck(...).HandleRelevanceFlagAndFaultCodeForDTC(vecInfo, dTC, eCU);
  if (dTC2.Relevance == true && !eCU.FEHLER.Contains(dTC2)) eCU.FEHLER.Add(dTC2);
  ```
  Info entries are tagged `dTC.EcuDTCType = "I"` (`:3521` functional path, `:3621` physical path).
- `IS_LESEN_DETAIL` → mutates the `INFO` entry in place (`VehicleIdent.cs:3776-3800`).

So the human-visible object is **one merged fault list per ECU** where relevant info entries
appear alongside real faults, with a type discriminator. This is the model klartext should copy.

---

## B. Freeze-frame sourcing — proven from bytecode

### B.1 `FS_LESEN` — one request, one response, no freeze-frame

```
### JOB FS_LESEN  bytes=7782  ops=1472  xsend=1  eoj=4          # fs_lesen.txt:1
000000  move     L0, 0x3
000007  ergi     {…} "F_VERSION.", I0        # ← constant 3, emitted BEFORE any send
000019  move     S1, {19 02 0C} "..."        # ← the entire UDS payload, never patched
0003AC  move     L0, 0x12                    # target 0x12 (DDE)
0003B9  move     B0, 0xF1                    # source 0xF1 (tester)
0003D7  xsend    S3, S1                      # ← the ONE xsend
```
The status mask `0x0C` is a hardcoded literal. The job's only argument (`pars S1, 0x1`) is the
string `IGNORIERE_EREIGNIS_DTC` — **the mask is not parameterisable**.

Response walk (`fs_lesen.txt` @ `0007BD`–`00080E`): `len(S3) − 3`, `÷ 4` → fault count; cursor
starts at 3; stride 4. Record = **3-byte DTC big-endian + 1 status byte**, matching klartext's
`59 02` decode. Length mismatch → `ERROR_ECU_INCORRECT_LEN`.

There is a loop around the xsend, but it is a **retry**, not an iteration: it fires only on
NRC `0x21` busyRepeatRequest and is bounded by the `IE` retry register
(`fs_lesen.txt` @ `0004B2`–`0004FF`). NRC `0x78` is not handled here — consistent with
`XEnet32/64.dll` owning it natively.

### B.2 `FS_LESEN_DETAIL` — three requests, verified literals and order

```
### JOB FS_LESEN_DETAIL  bytes=59287  ops=10312  xsend=3        # fs_lesen_detail.txt:1
000179  move     S1, {19 09 FF FF FF}        →  xsend @ 00105E   (severity)
001ABD  move     S1, {19 06 FF FF FF FF}     →  xsend @ 001CE4   (extended data)
003FB4  move     S1, {19 04 FF FF FF FF}     →  xsend @ 0041DB   (snapshot)
```
- The DTC comes from job arg 1 (`parl L0, 0x1`) and patches bytes 2-4 of each literal.
- Byte 5 stays `0xFF` = **all records** — same as klartext's `ALL_DTC_RECORDS`.
- `19 09` is 5 bytes (no record byte); `19 06`/`19 04` are 6.
- **ISTA's order is `09 → 06 → 04`.** klartext's is `04 → 06 → 09` (see §E.2).
- Each site has its own NRC-`0x21` retry loop; **no outer loop wraps any of them** — all
  backward jumps beyond the third site target response-walking code only.

### B.3 Only TWO of the three run on the F25's DDE — the `F_SEVERITY` gate

The severity read is gated on an SGBD table, not on the response:
```
000ADC  tabseek  {…} "NAME.", {…} "F_SEVERITY."     ; in FDetailStruktur
000B34  tabget   S3, {…} "TYP."
000B98  or       L0, 0x20                            ; tolower
000BEC  comp     L0, 0x6E                            ; == 'n'  → "nein"
000F0C  move     I0, S0[34]
000F23  jz       0x8F4  -> 00181D                    ; skips PAST the xsend at 00105E
```
Dumped from the real `.prg`s:

| SGBD | `F_UWB_ERW` | `SAE_CODE` | `F_HLZ` | **`F_SEVERITY`** | `F_UWB_SATZ` | ⇒ requests |
|---|---|---|---|---|---|---|
| `d72n47a0` (F20+F25 DDE) | ja | ja | ja | **nein** | 2 | **2** (`19 06`, `19 04`) |
| `cas4_2` (F25 CAS4) | ja | nein | ja | **ja** | 2 | **3** |

`F_UWB_ERW=ja` gates `19 06`; `F_UWB_SATZ=2` gates `19 04`. Per-fault cost is therefore
**variant-dependent** — read `FDetailStruktur` from the ECU's `.prg` to know it.

### B.4 What `F_UW_KM` / `F_UW_ZEIT` / `F_UW_ANZ` / `F_UW[]` actually are

All are `FS_LESEN_DETAIL`-only (and `IS_LESEN_DETAIL`), decoded from the `19 04` snapshot
response. The "obligatory" identifiers are recognised by their `UWNR` with hardcoded widths:

| Result | UWNR | Width | Absent sentinel | Snapshot table row |
|---|---|---|---|---|
| `F_UW_KM` | `0x1700` | **3 bytes BE** | `0xFFFFFF` | `KM_STAND` |
| `F_UW_ZEIT` | `0x1701` | **4 bytes BE** | `0xFFFFFFFF` | `ABS_ZEIT` |
| `F_SAE_CODE` | `0x1702` | 3 bytes | `0xFFFFFF` | `SAE_CODE` |
| `F_FEHLERKLASSE_NR` | `0x1731` | 1 byte | — | `Fehlerklasse_DTC` |
| `F_UW_BN` | `0x1750` | 1 byte | `0xFF` | `PWF_Basisnetz` |
| `F_UW_TN` | `0x1751` | 3 bytes | `0xFFFFFF` | `PWF_Teilnetz` |
| `F_UW_KM_SUPREME` | `0x1768` | 4 bytes | — | `KM_STAND_SUP` (`cas4_2` only) |
| `F_UW_ZEIT_SUPREME` | `0x1769` | 6 bytes | — | `ABS_ZEIT_SUP` (`cas4_2` only) |

**Units — do not over-claim.** These nine `DTCSNAPSHOTIDENTIFIER` rows are byte-identical
across all 1,403 `.prg` files (531 SGBDs carry `0x1700`-`0x1702`), and all carry
`MUL=1, DIV=1, ADD=0` with `UW_EINH = "0-n"` — which ISTA maps to `UwType.Discrete`, i.e. *not
a unit*. So `F_UW_KM` is kilometres **by table name only** (`KM_STAND`), and `F_UW_ZEIT` is a
raw 4-byte absolute-time counter with **no derivable unit**. ISTA itself only ever uses it as
an ordering key and tests `!= -1` for absence. Surface both as raw integers with the table
name; do not invent seconds/minutes.

**`F_UW_ANZ` is a runtime count, and `F_UW<n>` names are built by string concatenation** — not
a fixed-width array:
```
00806F  move     S1, {46 5F 55 57 00} "F_UW."
0080AD  fix2dez  S5, L0                     ; counter → decimal ASCII
008106  spaste   S5[0], S1                  ; → "F_UW1", "F_UW2", …
008127  tabset   {…} "DTCSnapshotIdentifier."
008177  tabseeku {…} "UWNR.", L2            ; unknown → fall back to FUmweltTexte
```
Per entry the job emits `_NR`, `_EINH`, `_TEXT`, `_WERT`, `_TYP`, `_NAME`, `_DATA` plus the
bare `F_UW<n>`. Per-field width/endianness/scaling come from `FUmweltTexte` columns
`UWTYP` / `L` / `H` / `MUL` / `DIV` / `ADD` (484 rows on `d72n47a0`). ISTA consumes exactly
this set in `ProcessEnvironmentConditions` (`VehicleIdent.cs:2993`), looping
`for (int i = 1; i <= typeDTCContext2.F_UW_ANZ; i++)` — at `VehicleIdent.cs:3214` on the
`FS_LESEN_DETAIL` path, `:3805` on the `IS_LESEN_DETAIL` path (and `:2781` / `:2901` on the
dead DS2 branches of §HEADLINE.2).

### B.5 Relation to klartext's existing `read_fault_detail`

**ISTA's inline data is a strict SUBSET of what klartext already fetches, plus decoding.**
klartext issues the same three `0x19` sub-services and keeps the record regions raw
(`FaultDetailRaw`); ISTA issues the same requests and additionally decodes the snapshot
against the SGBD's `DTCSnapshotIdentifier` / `FUmweltTexte` tables. klartext's mechanism is
right; what it lacks is the table-driven decode — and that is a *separate* (larger) work item,
since it needs `.prg` table access at read time.

---

## C. `DoECUReadFSDetails` — what it adds

`VehicleIdent.cs:3123` (instance) → `:3140` (static) → `:3158` (`SetDTCDetailValues`).

**Per fault, not per ECU.** `doECUReadFSDetails` iterates `sg.FEHLER` and calls
`SetDTCDetailValues` for each (`VehicleIdent.cs:3150-3153`).

Three gates:
1. `sg.FEHLER != null && sg.ECU_ASSEMBLY_CONFIRMED` (`:3148`) — the ECU must be confirmed fitted.
2. `if (!DoEcuJob(vecInfo, sg, "FS_LESEN_DETAIL,FS_LESEN")) return false;` (`:3160`).
3. `if (!fDTC.IsVirtual)` (`:3164`) — virtual/synthesised faults are skipped.

`DoEcuJob` (`VehicleIdent.cs:5502`) is **not** a catalog lookup — it is a liveness check:
```csharp
if (ConfigSettings.getConfigStringAsBoolean("...SkipECUJobNotResp", defaultValue: true) && vecInfo.BNType == BNType.BN2020) {
    if (ecu == null || ecu.IDENT_SUCCESSFULLY) return true;
    // ... else log + SetECUColor + return false
}
```
On BN2020 it skips jobs for ECUs whose **indexing/identification failed**. Unknown ECU name →
`return true` (fail-open, `VehicleIdent.cs:5490-5491`).

The call: `ecuKom.ApiJob(sg.ECU_SGBD, "FS_LESEN_DETAIL", fDTC.F_ORT.ToString(), string.Empty, retries)`
(`VehicleIdent.cs:3167`) — argument is `F_ORT`, the fault location number, as a **decimal
string**.

Beyond `DTCContext`, the detail read fills: `F_VERSION`, `F_HEX_CODE`, `F_CODE`, `F_PCODE`,
`F_PCODE_STRING`, `F_PCODE_TEXT`, `F_SAE_CODE`, `F_SAE_CODE_STRING`, `F_SAE_CODE_TEXT`, `F_LZ`,
and — for UDS — `F_EREIGNIS_DTC` (`VehicleIdent.cs:3176-3191`; the UDS condition is at `:3189`).

**Implementation detail worth copying:** the result-set loop is
`for (ushort num3 = 1; num3 < ecuJob.JobResultSets; num3++)` (`VehicleIdent.cs:3228`) — a
strict `<`, so the **last** result set is deliberately skipped (it is the job-status set).

---

## D. Info memory in the bundle

### D.1 How ISTA decides an ECU has info memory — TRY AND TOLERATE, no catalog check

`DoECUReadIS` (`VehicleIdent.cs:3443`) is two-phase:

1. **Functional broadcast first** (`tryFunctional`, true on every F-series call site):
   `ecuKom.apiJob(groupSgbd, "IS_LESEN_FUNKTIONAL", …)` (`:3462`). One broadcast; each
   responding ECU yields a result set carrying `ID_SG_ADR`, `JOB_STATUS`, `F_ANZ`, and
   `F_ORT<i>_NR` / `F_ART<i>_NR` pairs. `eCU.IS_SUCCESSFULLY` is set only when
   `JOB_STATUS == "OKAY"` (`:3486-3494`).
2. **Physical fallback** for anything that did not answer (`:3549-3558`):
   ```csharp
   foreach (ECU item3 in VecInfo.ECU)
     if (!item3.IS_SUCCESSFULLY && item3.BUSID == busid && !item3.IsVirtual() && item3.ECU_ASSEMBLY_CONFIRMED)
       doECUReadIS(item3, ecuKom, VecInfo, ffmResolver, retries, dbConnector, shouldCalculateFaultProperties: false);
   ```
   which runs `IS_LESEN` physically (`:3603`) behind `DoEcuJob(vecInfo, mECU, "IS_LESEN")` (`:3596`).

**There is no capability probe and no catalog gate.** An ECU without info memory simply fails
the job and is tolerated. klartext's `Ok(None)`-on-negative-response is already the right shape.

`DoECUReadISDetails` (`VehicleIdent.cs:3732` → `:3750`) then runs per entry, gated at `:3762`:
```csharp
if (item == null || (item.Relevance != true && !readInfoFaultDetails)) continue;
```
`readInfoFaultDetails` comes from `BMW.Rheingold.Diagnostics.VehicleIdent.INFO.GetDTCDetails`,
**default `true`** (`VehicleIdent.cs:5444`) — so in stock configuration ISTA reads detail for
**every** info entry, not just relevant ones.

### D.2 RECORD WIDTH — settled by bytecode, and klartext is OFF BY ONE

`IS_LESEN` is byte-identical between `d72n47a0` and `cas4_2` (5,911 B / 1,095 ops) — a generic
framework job. One xsend.

```
### JOB IS_LESEN  bytes=5911  ops=1095  xsend=1                 # is_lesen.txt:1
000000  move     L0, 0x3
000007  ergi     {…} "F_VERSION.", I0        # ← F_VERSION = the LITERAL 3, before any send
000019  move     S1, {22 20 00}
00017D  xsend    S3, S1
000536  move     L0, 0x3   → S0[6] = 3       # ← cursor starts at 3
000563  slen     L0, S3                      #   len(full response)
000583  subb     L0, L1                      #   − 3
00058A  move     L0, 0x4
00059A  divs     L0, L1                      #   ÷ 4  → entry count S0[2]
```
Field extraction confirms the record shape (`is_lesen.txt` @ `000D90`-`0010E0`):
`S0[6]+0 << 16`, `S0[6]+1 << 8`, `S0[6]+2` summed → `F_ORT_NR`; `S0[6]+3` → `F_STATUSBYTE`.
Stride and loop:
```
0015FD  move     L0, 0x4                     # ← STRIDE
001630  adds     L0, L1  → S0[6]             #   off += 4
00164D  jump     0xFFFFF142 -> 000795        # ← BACKWARD, top of the entry loop
```

> **`[verify against capture]` can be flipped to CONFIRMED:**
> the response is `62 20 00` followed by records starting at **offset 3**, each **4 bytes** =
> `[code: 3 BE][status: 1]`, count `(len − 3) / 4`. **There is NO version byte in the payload.**

**This exposes a real bug.** `crates/uds/src/dtc.rs::decode_info_memory` does:
```rust
let rest = body.get(2..)…;          // offset 3 of the response — correct so far
let version = rest.first().copied(); // ← consumes a byte that does not exist
let entries = rest.get(1..)….chunks_exact(INFO_RECORD_LEN)  // ← records start at offset 4
```
Every info-memory record is shifted one byte left of where it should be. The bytecode proves
`F_VERSION` is the compile-time constant `3` emitted at op offset `000007`, *before the request
is even built* — it is a format-generation marker (matching ISTA's
`if (fDTC.F_VERSION == 3 || sg.DiagProtocoll == typeDiagProtocoll.UDS)` at
`VehicleIdent.cs:3189`), never a wire byte. The same constant-3 prologue appears in `FS_LESEN`
(`fs_lesen.txt` @ `000000`-`000007`), where klartext correctly does *not* consume a version byte.

The classification is identical to the fault path (`& 0x4D` → `F_VORHANDEN_NR`,
`>>4 & 1 + 0x10` → `F_READY_NR`, `>>7 & 1 + 0x80` → `F_WARNUNG_NR`), but resolved through the
**parallel `I*` table family** — `IOrtTexte` (`is_lesen.txt` @ `000EF7`), `IArtTexte`
(@ `0011D2`), `IUmweltTexte`, `IDetailStruktur` — which exists alongside the `F*` tables in
every SGBD. Info-entry text must NOT be looked up in the fault tables.

### D.3 `IS_LESEN_DETAIL` — the asymmetry: info freeze-frames ARE inline

`is_lesen_detail.txt` — **2 xsends, both `0x22`, no `19` anywhere**: `22 20 00` (re-read the
list) then `22 20 <pos>`. Position is a **1-based index into the list**, found by a scan loop
(@ `000E4E`-`0010B2`) matching job arg 1; job arg 2 (`parw I0, 0x2`, range-checked `1..=0x1FF`,
else `ERROR_F_POS`) overrides it. Not found → `ERROR-CODE NOT STORED IN SHADOW-MEMORY`.
```
0013CE  parw     I0, 0x2
0014ED  move     L0, 0x2
00150C  move     S7[L1], B0      ; S7 = {22 20 00} → S7[2] = position
001647  xsend    S2, S1
```
That single `22 20 nn` response yields the whole `F_UW*` family plus `F_HFK`/`F_HLZ` — **no
`19 04`/`19 06` at all**. So for *info memory* freeze-frame data is genuinely inline; for
*fault memory* it is not. `d72n47a0`'s `IDetailStruktur` mirrors `FDetailStruktur`
(`F_UWB_ERW=ja`, `SAE_CODE=ja`, `F_HLZ=ja`, `F_SEVERITY=nein`, `F_UWB_SATZ=2`).

ISTA calls it per entry: `ecuKom.apiJob(sg.ECU_SGBD, "IS_LESEN_DETAIL", item.F_ORT.ToString(), …)`
(`VehicleIdent.cs:3766`), then fills `F_ORT_TEXT`, `F_READY_*`, `F_VORHANDEN_*`, `F_WARNUNG_*`,
`F_FEHLERKLASSE_*`, `F_SYMPTOM_*`, `F_HFK`, `F_HLZ`, `F_UEBERLAUF` and the `DTCContext`
(`:3777-3812`).

---

## E. Mapping to klartext — concrete change list

Surfaces are the MCP server and the future mobile app; the CLI is gone. All line numbers below
were current at time of writing — **other agents have `crates/client/src/*` and `mcp/src/*`
modified in the working tree**, so re-locate by symbol name, not by line.

### E.1 RECOMMENDATION — bundle the two BASE reads; make detail an opt-in depth

**Do this:**

1. **Merge `FS_LESEN` + `IS_LESEN` into one operation.** These are exactly **1 request each**,
   both cheap, both bounded, and ISTA never performs one without the other. Merge their output
   into a single fault list with a type discriminator, exactly as ISTA does
   (`VehicleIdent.cs:3535`).
2. **Do NOT bundle the detail reads by default.** They are `2 × (number of faults)` requests,
   unbounded, and are the entire cost risk. Expose them behind a `detail` parameter.
3. **Keep `read_fault_detail` as a separate drill-down tool** (fixed per §E.2).
4. **Remove the standalone `read_info_memory` tool.** ISTA has no "read info memory alone"
   operation; the parity mandate says match ISTA. An extra tool costs schema/context budget for
   zero capability once it is folded in.

**Why this split rather than one fat bundle.** The cost asymmetry is the whole argument:
per-ECU base cost is fixed at 2 requests, per-fault detail cost is unbounded. On the F25
(≈11 fitted ECUs) a whole-car base bundle is **22 requests** — comparable to today's
`read_all_faults`. Adding detail for a car with 20 stored faults adds **40-60 more**, and the
parallel work item making whole-car reads sequential removes the concurrency that hides that
latency today. Car session 1 already saw ~10/32 ECUs time out under a rapid sweep. A tool call
that reliably returns in seconds beats one that sometimes returns in minutes, and an agent can
always follow up on the two ECUs that actually had faults. Bundling the *cheap* half gives the
agent ISTA's merged picture in one call; bundling the *expensive* half would make the common
case pay for the rare one.

### E.2 `crates/uds/src/dtc.rs`

- **FIX `decode_info_memory` (bug).** Drop the phantom version byte: records start at the
  first byte after the `20 00` DID echo, stride 4, count `(len − 3) / 4` where `len` is the
  full response length. Keep the lenient trailing-partial behaviour and `raw`.
- **Change `InfoMemory`**: remove `pub version: Option<u8>` (it is a compile-time constant, not
  wire data). If a `version` is still wanted for display, hardcode `3` with a comment citing
  `is_lesen.txt` @ `000000`-`000007` — do not read it from the payload.
- Update the two lenient-layout unit tests, which currently encode the off-by-one:
  `decode_info_memory_parses_version_and_entries`, `decode_info_memory_is_lenient_on_layout`.
- Drop the `[verify against capture]` markers on `InfoMemory` and `decode_info_memory`;
  replace with a bytecode citation.
- Add a discriminator for merged lists, e.g. `pub enum FaultSource { FaultMemory, InfoMemory }`
  (ISTA's `EcuDTCType` `"F"`/`"I"`, `VehicleIdent.cs:3518`).

### E.3 `crates/client/src/client.rs`

- **FIX `read_fault_detail` request order** to ISTA's `19 09` → `19 06` → `19 04`
  (`fs_lesen_detail.txt` @ `000179`/`001ABD`/`003FB4`). Today it is `04`, `06`, `09`.
- **Gate the `19 09` severity read.** ISTA skips it entirely when the SGBD's `FDetailStruktur`
  has `F_SEVERITY = nein` — which is the case for `d72n47a0`, i.e. **klartext currently sends a
  request ISTA never sends on the F25's DDE**. Where the `.prg` is reachable, read
  `FDetailStruktur` via `klartext_sgbd::Prg::table_ci("FDetailStruktur")` and skip on `nein`;
  where it is not (BYO-data absent), fall back to sending it — `request_optional` already maps
  the negative response to `None`, so the cost is one wasted round trip, not a failure.
  Same for `F_UWB_ERW` (`19 06`) and `F_UWB_SATZ` (`19 04`).
- **Add the bundle method**, e.g.
  `read_ecu_faults(&self, target: u8) -> Result<EcuFaultBundle, ClientError>`: one
  `read_all_dtcs` + one `read_info_memory`, merged. An info-memory negative response must
  degrade to `info_supported: false`, never an error.

### E.4 `crates/client/src/scan.rs`

- Extend `EcuFaults` with `info: Vec<Dtc>` and `info_supported: bool`.
- `scan_faults` calls the new bundle instead of bare `read_all_dtcs`. Note this doubles the
  request count (1 → 2 per ECU); coordinate with the sequential-read work item.
- Keep the existing "record the error, never abort the scan" contract (`EcuFaults::error`).

### E.5 `mcp/src/dto.rs`

- `ReadFaultsResult`: add `info_entries: Vec<FaultInfo>`, `info_supported: bool`; give
  `FaultInfo` a `source` field (`"fault_memory"` / `"info_memory"`).
- `ReadFaultsRequest`: add `detail: DetailDepth` — `None` (default) | `Relevant` | `All`.
  `Relevant` matches ISTA's effective default for info entries (`readInfoFaultDetails = true`,
  `VehicleIdent.cs:5444`) while staying cheap for faults.
- `EcuFaultsInfo`: add the same info fields.
- **Delete `InfoMemoryRequest` and `InfoMemoryResult`.**
- `InfoMemoryResult::note` (the provisional-layout caveat) can go — the layout is now settled.

### E.6 `mcp/src/server.rs`

- `read_faults` becomes the bundle; update the tool description to say it returns fault memory
  **and** info memory, and that info entries are marked.
- **Remove the `read_info_memory` tool** (and its `#[tool]` attribute).
- `read_all_faults`: add `detail` (default `None`); keep the existing not-tested partitioning.
- `read_fault_detail` stays, with §E.3's fixes. Its description should note that `19 09` is
  skipped on ECUs whose SGBD declares `F_SEVERITY = nein`.
- `fault_help`, `clear_faults`, `clear_all_faults` unchanged.

### E.7 Out of scope for this item

- ZFS / `STATUS_ZFS_LESEN_GESAMT` (§A.4) — separate parity item.
- `FS_LESEN_EXPERT` (§A.5) — does not apply to these variants.
- Table-driven snapshot decoding via `DTCSnapshotIdentifier` / `FUmweltTexte` (§B.4) — larger,
  needs `.prg` access at read time.
- The `19 02 FF` + `RELEVANT_MASK = 0xAF` divergence vs ISTA's `19 02 0C` + classification
  (`crates/uds/src/service.rs:93`, `crates/uds/src/dtc.rs:45`) — already tracked in the parity
  audit; noted here only because the bytecode independently confirms it
  (`fs_lesen.txt` @ `000019`).

---

## F. Risks

1. **Request-count doubling.** The base bundle takes whole-car reads from N to 2N requests. On
   the F25 that is 22 instead of 11 — acceptable, but it compounds with the sequential-read
   change. Measure before adding detail-by-default.
2. **Detail-read blowup.** `2 × faults` requests, unbounded. A car with a dead bus can present
   dozens of faults per ECU. Mitigation: `detail: None` default, plus a hard cap with a
   truncation flag in the result.
3. **ECUs without info memory.** Only 342/1405 ECUs document `22 2000`. A negative response is
   the *normal* case and must never surface as an error — degrade to
   `info_supported: false`. This is already how `read_info_memory` behaves; preserve it exactly
   when folding it into the bundle.
4. **Timeouts on silent ECUs.** Car session 1 saw ~10/32 ECUs time out under a rapid sweep
   though they answered individually. Adding a second per-ECU request widens that window. Keep
   the per-ECU error capture and consider ISTA's own precedent — `DoEcuJob`
   (`VehicleIdent.cs:5502`) skips jobs entirely for ECUs whose identification failed, rather
   than retrying them.
5. **The off-by-one fix changes existing output.** Any recorded info-memory results from before
   the fix are wrong and should be discarded, not migrated. There is no on-car info-memory
   capture yet, so nothing downstream depends on the broken layout.
6. **Response size.** A `19 04` snapshot with many `F_UW` entries can be large; HSFZ framing
   handles it, but the MCP JSON payload for a whole-car detail read could be substantial.
   Another argument for opt-in detail.
7. **`19 09` gating depends on BYO-data.** If the `.prg` set is absent, klartext cannot know
   `F_SEVERITY` and will send a request ISTA would skip. Harmless (one negative response) but
   it is a real, documented divergence in the no-data configuration — state it rather than
   hiding it.

---

## Where ISTA's logic proved unreadable

1. **`STATUS_ZFS_LESEN_GESAMT`'s wire protocol.** 13,615 ops, 8 xsends, telegrams assembled
   from registers populated by table lookups. No UDS request literal appears anywhere in the
   job; a full literal census yields only ASCII (`"DM01;"`…`"DM??;"`, `"SGBD"`, `"ORTTEXT"`,
   `"CC_MSG"`). I could not statically determine which services it sends. Resolving it needs
   either symbolic execution of the job or an on-car capture of an ISTA ZFS read.
2. **NRC `0x78` handling.** Absent from every job's retry loop (only `0x21` is handled). Owned
   natively by `XEnet32/64.dll`, a native PE — consistent with the existing project note.
3. **`PlausibilityCheck.HandleRelevanceFlagAndFaultCodeForDTC`** (`VehicleIdent.cs:2943`,
   `:3535`, `:3703`) — decides each DTC's `Relevance`, which in turn gates whether an info entry is
   merged into the fault list and whether `F_ANZ` counts it. It delegates into the KMM rules
   engine, which the project already documents as interface-only. Not resolved here; klartext's
   `is_relevant()` remains its own approximation, tracked separately in the parity audit.
