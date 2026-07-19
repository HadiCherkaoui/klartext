# P0.2 + P0.3 — ISTA's fault-read and fault-relevance model

> **Research record, 2026-07-18.** Produced under the 1:1 ISTA parity mandate
> (see `CLAUDE.md`): every claim is cited to decompiled ISTA source, ECU `.prg`
> bytecode disassembled with klartext's own `klartext-best`, or a query run
> against the real ISTA database. Citations reference a local `<scratchpad>`
> of `ilspycmd` output over `data/TesterGUI/bin/Release/` — BYO data, gitignored,
> never committed. Regenerate locally to follow a citation.


**Status:** research complete, implementation-ready.
**Date:** 2026-07-18.
**Scope:** the wire mask for fault reads, and everything ISTA does to decide whether a fault is
shown, counted, or coloured. Produces a concrete change list for `crates/uds`, `crates/client`,
`crates/semantic`, `mcp/`, and `scripts/build-semantic-db.sh`.

Every claim below carries a citation into decompiled ISTA source, disassembled SGBD bytecode, a
query actually run against the owner's ISTA database, or the on-car pcap. Where ISTA is unreadable
it is called out explicitly in §F.

**Citation roots** (paths are absolute; `SP` = the session scratchpad
`<scratchpad>`):

| Short form | Full path |
|---|---|
| `VehicleIdent.cs` | `SP/VehicleIdent.cs` (from `RheingoldDiagnostics.dll`) |
| `PlausibilityCheck.cs` | `SP/DTCPlausibilityCheck/BMW.ISPI.TRIC.ISTA.DTCPlausibilityCheck/PlausibilityCheck.cs` |
| `FaultFilter.cs` | `SP/FaultFilter.cs` (from `RheingoldCoreFramework.dll`) |
| `DTC.cs` | `SP/DTC.cs` (from `RheingoldCoreFramework.dll`) |
| `Logic.cs` | `SP/parity-research/logic/Logic.cs` (from `RheingoldSessionController.dll`) |
| `conwoy` | `SP/parity-research/relfc/conwoy/RheingoldConWoyDataConnector.decompiled.cs` |
| `coreframework` | `SP/parity-research/relfc/coreframework/RheingoldCoreFramework.decompiled.cs` |
| `ConWoyDataProviderSQLite.cs` | `SP/parity-research/dbprov/ConWoy/BMW.Rheingold.Data.ConWoyConnector/ConWoyDataProviderSQLite.cs` |
| `TestplanCalculator.cs` | `SP/parity-research/dbprov/RheingoldSessionController/BMW.Rheingold.CoreFramework.TestplanCalculation/TestplanCalculator.cs` |

---

## A. The wire read

### A.1 `FS_LESEN` transmits `19 02 0C` — hard literal, verified

Verified independently in this research by disassembling the bytecode with klartext's own
`klartext_best::decode_job`. In `data/Testmodule(1)/Ecu/d72n47a0.prg`, job `FS_LESEN`:

```
[  2] 000016: clear    S1
[  3] 000019: move     S1, [19 02 0C]     <- literal at bytecode offset 0x19
[  4] 000021: clear    S2
[  5] 000024: move     S2, S1             <- S2 = payload (never written again)
[130] 0002C2: clear    S1
[131] 0002C5: move     S1, S2             <- payload back into S1
[164] 000360: spaste   S1[#0], [80 00 00] <- BMW-FAST short header prepended
[165] 00036A: adds     S1[#0], B6         <-   0x80|len
[174] 0003AC: move     L0, #0x12          <- target = DDE 0x12
[176] 0003B9: move     B0, #0xF1          <- source = tester
[182] 0003D7: xsend    S3, S1             <- TRANSMIT
```

`cas4_2.prg / FS_LESEN` is byte-identical at the same op indices.

**Proof it is not computed:** an exhaustive enumeration of every write to `S2` in both jobs returns
exactly two instructions — `clear S2` and `move S2, S1`. There are **no indexed writes** (`move
S2[...]`) anywhere in either job. The same zero-indexed-write result was confirmed on `fem_20`,
`bdc`, `bdc_g05`, `komb01`, `dsc_10`, `gs100a`. The job's one argument (`IGNORIERE_EREIGNIS_DTC`,
read at op 53 via `pars`) is a string flag that post-filters decoded results and never touches the
request.

**`FS_LESEN` sends a hard-coded `19 02 0C`. Not parameterised, not computed.**

### A.2 Two corrections to the parity audit

The audit stated the literal was traced "byte-identical in two independent SGBDs
(`d72n47a0`, `cas4_2`)" for **both** `FS_LESEN` and `FS_LESEN_EXPERT`. Both halves of that are wrong
about `FS_LESEN_EXPERT`:

1. **`d72n47a0` has no `FS_LESEN_EXPERT` job at all.** It ships `FS_LESEN_PERMANENT` instead;
   `cas4_2` is the reverse. So "byte-identical in both" cannot be true of that job.
2. **`FS_LESEN_EXPERT`'s mask is parameterised — `0x0C` is only its default.** At ops 310–332 in
   `cas4_2` (byte-identical in `fem_20`):

```
[310] parl  L0, #0x2        <- is job ARGUMENT #2 present?
[316] jz    #0x43           <- if absent, skip the patch (default 0x0C stands)
[319] parb  B0, #0x2        <- B0 = argument #2 as a byte
[324] move  L0, #0x2        <- index 2 == the mask byte
[332] move  S2[L1], B0      <- S2[2] = caller-supplied mask
```

This matters for klartext's API design: a **parameterised** mask builder is ISTA-faithful, not a
klartext invention (see §D.1). What is unfaithful is *defaulting* it to `0xFF`.

### A.3 Every fault-memory job, and the mask each emits

Enumerated across `d72n47a0.prg` and `cas4_2.prg`:

| Job | Request emitted | Mask origin |
|---|---|---|
| `FS_LESEN` | `19 02 0C` | **hard literal** (op 3) |
| `FS_LESEN_EXPERT` | `19 02 0C` default | **job argument #2** patches `S2[2]` |
| `FS_LESEN_FUNKTIONAL` | `19 02 0C` | literal |
| `FS_LESEN_DETAIL` | `19 09 FF FF FF`, then `19 06 FF FF FF FF`, then `19 04 FF FF FF FF` | no mask exists; the `FF`s are patched with the DTC number from the job arg |
| `FS_LESEN_PERMANENT` | `19 15` | ISO sub-function takes no mask |
| `FS_LOESCHEN` | `14 FF FF FF` | literal |
| `FS_SPERREN` | `85 02` | literal (ControlDTCSetting off) |
| `IS_LESEN` | `22 20 00` **or** `19 17 0C 01` | literal either way |
| `IS_LESEN_DETAIL` | `22 20 00` / `19 09 FF FF FF` + `19 18`/`19 19 … 01` | literal |
| `IS_LOESCHEN` | `31 01 0F 06` | literal |

Note `IS_LESEN` is **bimodal across the fleet**: 255 ECUs implement it as
`19 17 0C 01` (`reportUserDefMemoryDTCByStatusMask`, mask `0x0C`, memory `0x01`), while
`cas4_2`/`d72n47a0` use `22 20 00`. klartext's existing `22 2000` info-memory read is correct for
both target cars but is **not** universal — worth a note in `crates/uds/src/service.rs`'s `did::INFO_MEMORY`
docs. Also note the same mask byte `0x0C` recurs there, reinforcing that `0x0C` is BMW's fault-scan
constant, not a `FS_LESEN` quirk.

### A.4 Fleet-wide mask distribution (1,405 `.prg`; 1,328 contain `FS_LESEN`)

| `FS_LESEN` request template | Count | Era |
|---|---|---|
| `19 02 0C` | **614** | UDS (F/G-series) |
| `84 FF F1 18 02 FF FF` | 418 | KWP2000 service `0x18`, pre-framed |
| `32 05 04 01` / `5B 05 04 01` / … | ~296 | DS2 proprietary |

**Among every UDS-era ECU, `19 02 0C` is universal — 614/614, zero exceptions.** Spot-checked
families `fem_20`, `bdc`, `bdc_g05`, `komb01`, `dsc_10`, `cas4_2`, `d72n47a0`, `gs100a`, `gws2` — all
`19 02 0C`. The KWP/DS2 rows are pre-UDS ECUs klartext does not target (klartext speaks UDS over
HSFZ only); they are listed so nobody mistakes the 614 for partial coverage.

**The single fleet-wide outlier:** `d94bx7a0.prg` job `_FS_LESEN_MIT_BIT5` → `19 02 2C`
(`0x0C | 0x20`, testFailedSinceLastClear — the job name means literally "with bit 5"). It is an
**extra underscore-prefixed job alongside** a normal `FS_LESEN` that still emits `19 02 0C` in the
same file, not a replacement. `_IS_LESEN_MIT_BIT5` and `x_bms.prg/IS_LESEN_MOTORRAD` are the
analogous `19 17 2C 01`.

Other `0x19` sub-functions across the fleet, for reference: `19 09 FF FF FF` (681), `19 06 FF FF FF FF`
(616), `19 04 FF FF FF FF` (611), `19 17 0C 01` (257), `19 18`/`19 19 … 01` (255 each), `19 15` (95),
`19 13 0C` (31), `19 12 0C` (29).

**Conclusion for A: klartext must send `19 02 0C`.** No call path in ISTA sends a different mask for
an ordinary fault scan; the only way a different mask reaches the wire is a caller explicitly passing
argument #2 to `FS_LESEN_EXPERT`.

### A.5 The `0x20`/`0x60` puzzle — resolved

A natural objection: if the scan is `19 02 0C` (pending|confirmed), a DTC whose status is exactly
`0x20` or `0x60` can never be returned — so how can `SetDtcRelevanceBasedOnStatusByte`
(§B.5) ever observe those values?

**Resolved from bytecode.** The two results come from *different requests*:

- **`FS_LESEN`** → `F_STATUSBYTE = S3[cursor+3]`, the per-record status of the `19 02 0C` response
  (record layout `[DTC hi][DTC mid][DTC lo][status]`). Mask-constrained.
- **`FS_LESEN_DETAIL`** → `F_STATUSBYTE = S2[5]`, where `S2` holds the header-stripped response of
  the **second `xsend`, the `19 06` request**. Buffer identity is proved inside the job itself:
  `ergy [_RESPONSE_EXTENDED_DATA], S2` at op 1608, immediately before the index constant is set
  (`move L0,#0x5` → `S0[#12]=5`) and read back at the store (ops 1957–1969). `cas4_2` is identical
  with the constant in `S0[#20]` (ops 2064–2067, store at op 2405).

In `59 06` the layout is `[59][06][DTC hi][DTC mid][DTC lo][statusOfDTC][recordNum][data…]`, so
payload index 5 is the status byte. **`19 06` is a by-DTC-number query with no status-mask
filtering**, so the ECU reports the DTC's full current status regardless of the `0x0C` used to
*discover* it.

That is exactly how `F_STATUSBYTE` can legitimately be `0x20` or `0x60` while discovery used
`19 02 0C` — no contradiction, and no need for a klartext-invented mask.

*(Verified: bytecode reads payload index 5 of the `19 06` response. Inferred: index 5 ↔ `statusOfDTC`
via the ISO `59 06` layout — corroborated by the result's name and by `FS_LESEN` reading the same
semantic byte at record `+3`.)*

`F_UW_ANZ` is **not** a wire byte: it is job-local counter `S0[#47]`, initialised to 0 (op 3349),
incremented per parsed record (op 5549), emitted via `ergi` (op 10162). It counts
**environmental-condition (Umweltbedingungen) records parsed from the `19 04` snapshot response**
— i.e. "how many freeze-frames does this DTC have".

---

## B. The relevance pipeline, in execution order

The single most important correction to klartext's mental model:

> **ISTA never removes a fault from the ECU's fault list because of its status bits.** Every DTC the
> ECU returns is added. Relevance is a tri-state *annotation* driven by a database lookup; it changes
> counts, colours, and test-plan expansion. Exactly one display-layer filter can hide a row, it is
> user-configurable, and its status-related term is data-driven and inactive on both of the owner's
> cars.

### B.1 Every returned DTC is added to the list — unconditionally

`VehicleIdent.cs:2943-2944` (the physical `FS_LESEN` path):

```csharp
DTC dTC2 = (DTC)new PlausibilityCheck(...).HandleRelevanceFlagAndFaultCodeForDTC(vecInfo, dTC, mECU);
mECU.FEHLER.Add(dTC2);          // <- unconditional Add
```

The functional-addressing path at `VehicleIdent.cs:2594-2595` is the same shape. There is no status
test guarding either `Add`.

| | |
|---|---|
| **Input** | every 4-byte record from the `59 02` response |
| **Output** | `ECU.FEHLER` — the fault list |
| **Filter or annotate?** | **Neither — it is the source.** Nothing is dropped. |

Two *later* add-sites do gate on relevance, but they are a different list — the aggregated
vehicle-wide fault list, not the per-ECU one: `VehicleIdent.cs:3536` and `:3704` both read
`if (dTC2.Relevance == true && !eCU.FEHLER.Contains(dTC2))`. These build the cross-ECU rollup.

### B.2 `PlausibilityCheck` — the DB lookup that sets `Relevance` (ANNOTATE)

`PlausibilityCheck.cs:33-56`:

```csharp
IXepFaultCode fc = dbProcessor.GetFaultCodeByCodeAndVariantName(
        (long)fDtc.fOrt.Value, identEcu.ECU_SGBD, fDtc.EcuDTCType, vecInfo);
if (fc != null) {
    fDtc.Relevance = Convert.ToBoolean(fc.RELEVANCE);   // :41
    fDtc.dtcId     = fc.ID.ToString();                  // :42
} else {
    fDtc.Relevance = null;                              // :48  <- tri-state
}
```

The concrete implementation is `ConWoyDataProviderSQLite.GetFaultCodeByCodeAndVariantName`
(`ConWoyDataProviderSQLite.cs:2384`). Verbatim SQL (`:2392`):

```sql
SELECT ID, CODE, DATATYPE, WEIGHTING, SCHEINFEHLER, AUSBLENDINDEX, RELEVANCE, SICHERHEITSRELEVANT,
       VALIDTO, VALIDFROM, DIAGNOSEINDEX, ECUVARIANTID
FROM XEP_FAULTCODES
WHERE (code = '{0}' COLLATE UTF8CI)
  AND (ecuvariantid = (SELECT ID FROM XEP_ECUVARIANTS where (name = '{1}' COLLATE UTF8CI)))
```

…plus ` and (datatype = '{2}')` when `dataType` is non-empty.

Three facts that decide the klartext implementation:

1. **`CODE` is matched as the DECIMAL string of `F_ORT`, not hex.** `PlausibilityCheck.cs:38` passes
   `(long)fDtc.fOrt.Value`, formatted with `CultureInfo.InvariantCulture` — so DTC `0x4B15` queries as
   `'19221'`. Independently corroborated by the CoreFramework cache path
   (`FaultCode.cs:430`: `fc.CODE == f_ort.ToString()`). klartext's semantic DB already stores
   `CAST(fc.CODE AS INTEGER)`, so it is already on the right footing.
2. **The variant key is `XEP_ECUVARIANTS.NAME`**, matched case-insensitively, and the caller passes
   `identEcu.ECU_SGBD` — the SGBD name *is* the variant name. This is exactly klartext's
   `ecu_variant` / M10 `resolve_variant` ladder.
3. **`dataType` is `XEP_FAULTCODES.DATATYPE` and takes exactly two values, `"F"` and `"I"`.**
   `DTC.EcuDTCType` carries `[DefaultValue("F")]` (`DTC.cs:1212`) = Fehlerspeicher. It is set to
   `"I"` in exactly two places, both inside `doECUReadIS()` — the `IS_LESEN` **Infospeicher** read
   (`VehicleIdent.cs:3521`, `:3621`). **This directly concerns klartext's existing
   `read_info_memory`: info-memory entries resolve against the same table with `DATATYPE='I'`.**

**Multi-row resolution** (`ConWoyDataProviderSQLite.cs:2416-2430`): exactly 1 → return it; >1 → log
`"{0} FaultCodes ... found. Only the first hit will be returned!"` and `return list.First()` — **no
`ORDER BY` anywhere, so "first" is whatever SQLite's scan order yields**; 0 → warn, return `null`.

**Per-row rule gate.** Each candidate row is filtered *inside the reader loop*
(`ConWoyDataProviderSQLite.cs:2399`):

```csharp
faultCode.ID = sQLiteSelectCmd.ReadDb<decimal>("ID");
if (vec == null || EvaluateXepRulesById(faultCode.ID, vec, ffmResolver))
{ /* hydrate + list.Add(faultCode) */ }
```

Rows whose rule fails are never added, so they cannot become the "first hit". `EvaluateXepRulesById`
(`:399`) is **inverted** — it returns `false` when the id IS in the not-valid set. The rule is keyed
by the fault-code row's own ID (`XEP_RULES.ID == XEP_FAULTCODES.ID`, 1:1). See §F for readability.

| | |
|---|---|
| **Input** | `(fault code as decimal, ECU SGBD/variant name, datatype F\|I)` + vehicle (for rules) |
| **Output** | `DTC.Relevance : bool?` — **tri-state** `true` / `false` / `null` (no catalog row) |
| **Filter or annotate?** | **ANNOTATE.** The DTC is already in `ECU.FEHLER` (§B.1) and stays there. |

### B.3 `hideBogusFaults` / `hideUnknownFaults` — the count and colour gates (ANNOTATE)

Defaults, read once in the `VehicleIdent` constructor (`VehicleIdent.cs:5440-5441`; fields declared
`:84-86`):

```csharp
hideBogusFaults   = ConfigSettings.getConfigStringAsBoolean("TesterGUI.HideBogusFaults",   defaultValue: true);
hideUnknownFaults = ConfigSettings.getConfigStringAsBoolean("TesterGUI.HideUnknownFaults", defaultValue: false);
```

So by default: **relevance-false faults are hidden from the count; relevance-unknown faults are
counted.**

The count logic, identical at both sites (`VehicleIdent.cs:2945-2960` for `F_ANZ`, and
`:5915-5930` inside `SetECUColor` for the ECU-tree colour):

```csharp
bool? relevance = dTC2.Relevance;
if (relevance.HasValue) {
    if (relevance == true)      mECU.F_ANZ++;
    else if (!hideBogusFaults)  mECU.F_ANZ++;
} else if (!hideUnknownFaults)  mECU.F_ANZ++;
```

| | |
|---|---|
| **Input** | `DTC.Relevance` (tri-state) + the two booleans |
| **Output** | `ECU.F_ANZ` (fault count) and `ECU.ECUTreeColor` |
| **Filter or annotate?** | **ANNOTATE.** Affects a *count* and a *colour*. The fault row is untouched. |

### B.4 `FaultFilter` + `Logic.FilterDTCRelevance` — the one real row filter

This is the only mechanism in ISTA that hides a fault row, and it is the display layer.

`FaultFilter` (`FaultFilter.cs`) is user-editable state: `FaultClassHidden` (default `null`),
`FaultGroupNumbers` (default `{1,2,3,4,5}`, `:17`, assigned in the ctor `:102`), `LowerKMBound` /
`UpperKMBound` (default `null`). `FaultFilterExpertMode` overrides the default groups to **all**
`FaultGroup` enum values (`FaultFilterExpertMode.cs:13`).

`Logic.FilterDTCRelevance(DTC, ICollection<ZFSResult>, FaultFilter)` (`Logic.cs:5336-5396`) returns
`true` = show. In order:

```csharp
if (dtc == null) return false;                                             // :5340
bool? relevance = dtc.Relevance;
if (relevance.HasValue) { if (relevance != true && hideBogusFaults) return false; }  // :5344-5351
else if (hideUnknownFaults) return false;                                  // :5352-5355
// mileage window, only when the DTC has freeze-frame mileage:
if (dtc.Current != null) { ...LowerKMBound / UpperKMBound... return false; }// :5356-5368
else if (faultFilter.UpperKMBound.HasValue || faultFilter.LowerKMBound.HasValue) return false; // :5369-5372
if (faultFilter.FaultClassHidden != null
    && faultFilter.FaultClassHidden.Contains(FaultCodeConverters.GetFaultClass(dtc, zfs))) return false; // :5373
if (faultFilter.FaultGroupNumbers != null) {
    Fault fault = vecInfo.FaultList.FirstOrDefault(p => p.DTC.FortAsHexString == dtc.FortAsHexString);
    if (fault == null || (fault.FaultGroupNumber != 0
        && !faultFilter.FaultGroupNumbers.Contains(fault.FaultGroupNumber))) return false; // :5377-5383
}
if (ConfigSettings.getConfigStringAsBoolean("EnableRelevanceFaultCode", defaultValue: true)
    && dtc.RelevanceFaultCode != null && dtc.F_VORHANDEN_NR.HasValue
    && !dtc.RelevanceFaultCode.Contains(dtc.F_VORHANDEN_NR.Value)) return false;          // :5384-5387
return true;
```

Note the escape hatch at `:5380`: **fault group `0` (unclassifiable) is never filtered out.**

| | |
|---|---|
| **Input** | `DTC.Relevance`, freeze-frame mileage, fault class, fault group, `F_VORHANDEN_NR` |
| **Output** | `bool` — show / hide |
| **Filter or annotate?** | **FILTER** — the only one. Note it re-applies the §B.3 relevance gate. |

**Fault class → fault group** is a pure function of an EDIABAS job result — **no database involved**.
`F_FEHLERKLASSE_NR` is populated exclusively from job results (`VehicleIdent.cs:3280` from
`F_FEHLERKLASSE_NR`, and `:2755`/`:2874`/`:3634` from `F_CLA`). It is then mapped:

`DTC.ConvertToFaultClass(int?)` (`DTC.cs:1391-1468`) — formats the value `"X8"` and inspects three
slices (`Substring(2,2)`, `Substring(4,2)`, `Substring(6,1)`), first non-`"00"` wins:

| Slice | Token → FaultClass |
|---|---|
| `[2..4]` | `08`→14, `04`→13, `02`→12, `01`→16 |
| `[4..6]` | `80`→15, `40`→11, `20`→10, `10`→9, `08`→1, `04`→6, `02`→5, `01`→8 |
| `[6..7]` | `8`→7, `4`→4, `2`→3, `1`→2 |

`DTC.MapFaultClassToFaultGroup()` (`DTC.cs:1471-1505`), fired from the `FaultClass` setter (`:343`):

| FaultClass | FaultGroup |
|---|---|
| 1 | 1 |
| 2,3,4,5,6 | 2 |
| 7,8 | 3 |
| 9,10 | 4 |
| 12,13,14 | 5 |
| 11,15,16 | 6 |
| *(anything else)* | **0** |

`FaultGroup` enum (`CoreFramework/BMW.Rheingold.CoreFramework/FaultGroup.cs:3`, implicit numbering):
`OperatingState=1, ECUFault=2, ElectricalFault=3, BusFault=4, InformationEntry=5, MessageFault=6,
CheckControlMessage=7`.

So the default filter `{1,2,3,4,5}` **hides `MessageFault` (6) and `CheckControlMessage` (7)**.
Note `CheckControlMessage` (7) is never produced by `MapFaultClassToFaultGroup`; it is set separately
at `CheckControlMessage.cs:128`, and `Fault.IsCheckControlMessage => FaultGroupNumber == 7`
(`Fault.cs:50`).

### B.5 `SetDtcRelevanceBasedOnStatusByte` — the one status check (ANNOTATE)

`VehicleIdent.cs:3315-3333`, verified verbatim:

```csharp
private static void SetDtcRelevanceBasedOnStatusByte(DTC dtc, IEcuJob fsDetailJob, ushort set)
{
    try {
        short? num = fsDetailJob.getshortResult(set, "F_UW_ANZ");
        if (!num.HasValue || num <= 0) {
            short? num2 = fsDetailJob.getshortResult(set, "F_STATUSBYTE");
            if (num2 == 96 || num2 == 32) { dtc.Relevance = false; }
        }
    } catch (Exception ex) { Log.Warning(...); }
}
```

Called only from the **detail** path `doECUReadFSDetails`, at `:3237` (per result set) and `:3251`
(once more for the final set). The `FsLesenExpertOldCode` feature-flag branch at `:3193-3211` inlines
the identical logic.

Semantics: **if the DTC has no freeze-frame records (`F_UW_ANZ` absent or ≤ 0) AND its
`19 06` status byte is exactly `0x60` or `0x20`, force `Relevance = false`.**

`0x20` = testFailedSinceLastClear alone; `0x60` = testFailedSinceLastClear + testNotCompletedThisOperationCycle.
Both mean "it failed at some point since the last clear, it is not failing now, and there is no
freeze-frame to explain when" — a stale echo.

| | |
|---|---|
| **Input** | `F_UW_ANZ` (freeze-frame count, from `19 04`) + `F_STATUSBYTE` (from `19 06`) |
| **Output** | forces `DTC.Relevance = false` |
| **Filter or annotate?** | **ANNOTATE** — but it feeds §B.3's count and §B.4's filter, so with the default `hideBogusFaults=true` it does ultimately hide the row in the GUI. |
| **Reachability** | **Only after a detail read.** A plain `19 02` scan never runs it. |

### B.6 `RelevanceFaultCode` — a data-driven status allowlist (FILTER, inactive on both cars)

The audit missed this entirely, and it is the closest thing in all of ISTA to klartext's
`RELEVANT_MASK`. It is worth understanding precisely, and then *not* implementing (§D.7).

`XEP_FAULTCODES.RELEVANCE_F_VORH_NR` (VARCHAR) is loaded by a whole-table preload cache,
`ConWoyDataProviderSQLite.InitializeFaultCodeRelevanceCache()` (`conwoy:20678`), which is called
unconditionally and is *not* gated by the feature flag. Verbatim SQL (`conwoy:20680`):

```sql
SELECT faults.id ECUFAULT_ID, faults.RELEVANCE_F_VORH_NR RELEVANCE, faults.code FAULTCODE, NAME ECUVARIANT_NAME
FROM XEP_ECUVARIANTS ecus, xep_faultcodes faults
WHERE ecus.id = faults.ecuvariantid AND faults.RELEVANCE_F_VORH_NR IS NOT NULL
```

Read by alias (`ReadDb<string>("RELEVANCE")`, `conwoy:20685`). Parsed at `conwoy:19732`:

```csharp
char[] separator = new char[3] { ' ', ',', ';' };
string[] array = relevance.Split(separator);
for (int j = 0; j < array.Length; j++) {
    int item = Convert.ToInt32(array[j], 16);      // <- BASE 16
    observableCollection.Add(item);
}
```

`Convert.ToInt32(s, 16)` — **hex**. The collection is `ObservableCollection<int>` (`coreframework:98024`)
and `.Contains` is exact whole-value membership, **not a bitmask test**.

**`F_VORHANDEN_NR` is the statusOfDTC byte, masked in the SGBD.** Verified in `d72n47a0.prg`
`FS_LESEN_DETAIL` bytecode — same source byte as `F_STATUSBYTE` (payload index 5) but masked:

| Result | Ops | Computation |
|---|---|---|
| `F_STATUSBYTE` | `[1956]–[1969]` @`0x002B9D` | `response[5]`, no mask |
| `F_VORHANDEN_NR` | `[1662]–[1680]` @`0x002561` | `move L0,#0x4D` @`0x00258C` → `and L0,L1` @`0x00259C` |
| `F_READY_NR` | `[1971]–[2010]` | `(status>>4)&1 + 0x10` |
| `F_WARNUNG_NR` | `[2104]–[2134]` | `(status>>7)&1 + 0x80` |

Bits 4 and 7 are not discarded — they are split into sibling results. Bit 1 is genuinely dropped.
A 256-value VM sweep matched `status & 0x4D` on 256/256.

**The mask is per-SGBD.** Each `.prg` carries both a `0x4D` and a `0x6D` block, selected by a
constant-folded branch. Fleet survey of the 400 largest `.prg`: 102 × `0x4D`, 33 × `0x6D` (G-series).
`d72n47a0`, `cas4_2`, `fem_20`, `dsc_10` all use **`0x4D`**.

**DB cross-check:** 622 non-empty rows across 107 variants; 28 distinct tokens
(`00 04 05 08 09 0C 0D 21 25 28 29 2C 2D 40 44 45 48 49 4C 4D 60 61 64 65 68 69 6C 6D`). Under a hex
reading every token is a byte, all ⊆ `0x6D`, with bits 1/4/7 never set — matching the bytecode
exactly. Under a decimal reading, 8 of 28 tokens (`0C 0D 2C 2D 4C 4D 6C 6D`) are not valid decimal at
all. Hex confirmed. Cross-referencing each variant's DB token union against the SGBD mask derived
independently from bytecode: **14/14 agree, 0 mismatches** — two independent evidence chains
(BMW's authoring data vs compiled bytecode) landing on the same per-ECU mask.

**Neither `d72n47a0` (1,105 fault codes) nor `cas4_2` (124) has a single non-empty row.** The 107
variants are G-series ADAS/radar/ultrasonic and EV/HV parts (`srr50_*`, `mrr_40`, `hvs_*`, `libb_*`,
`idcevo25`, `cce_*`, `ccu_*`). Also, 19 of the 622 rows reference an `ECUVARIANTID` with no
`XEP_ECUVARIANTS` row; ISTA's inner join silently drops them.

| | |
|---|---|
| **Input** | per-(code, variant) hex allowlist + `F_VORHANDEN_NR` (= status & 0x4D or 0x6D) |
| **Output** | hide the row (`Logic.cs:5384`) / set `Relevance=false` (`Fault.ResolveRelevanceFaultCode`, `coreframework:103595`) |
| **Filter or annotate?** | **FILTER** at `Logic.cs:5384`, **ANNOTATE** at `coreframework:103595` — both. |
| **Applicability to klartext** | **Zero rows on either of the owner's cars.** |

### B.7 The real "is this fault present right now" decision — and it is trivially portable

Separate from relevance, ISTA has an explicit presence decision, and it is the most directly useful
thing found in this research.

`Fault.SetExisting()` (`coreframework:103505`), UDS branch, switching on `F_VORHANDEN_NR`:

| Verdict | `F_VORHANDEN_NR` values |
|---|---|
| `#YesSmall` (present) | 5, 9, 13, 33, 37, 41, 45 = `0x05 0x09 0x0D 0x21 0x25 0x29 0x2D` |
| `#NoSmall` (absent) | 4, 8, 12, 32, 36, 40, 44 = `0x04 0x08 0x0C 0x20 0x24 0x28 0x2C` |
| *(no case — stays `#UnknownSmall`)* | everything else, i.e. every value with bit 6 (`0x40`) set |

Present values are exactly the odd ones (bit 0 = `testFailed`); absent values exactly the even ones;
anything with `testNotCompletedThisOperationCycle` falls through to "unknown".

`TestplanCalculator.CalculateProryItem` (`TestplanCalculator.cs:189-196`) states the same rule
arithmetically for UDS (`F_VERSION == 3`):

```csharp
int num  = fault.DTC.F_VORHANDEN_NR.Value & 1;
int num2 = fault.DTC.F_VORHANDEN_NR.Value & 0x40;
if (num == 1 && num2 == 0) { item.IsFaultPresent = true; }
```

**Critical simplification for klartext:** bits 0 and 6 are members of *both* SGBD masks (`0x4D` and
`0x6D`), so `F_VORHANDEN_NR & 0x01 == status & 0x01` and `F_VORHANDEN_NR & 0x40 == status & 0x40`.
**klartext can compute ISTA's exact presence verdict from the raw `19 02` status byte alone, with no
per-SGBD mask, no `.prg` read, and no detail read:**

```
present = (status & 0x01) != 0 && (status & 0x40) == 0
absent  = (status & 0x01) == 0 && (status & 0x40) == 0
unknown = (status & 0x40) != 0
```

Checked against the owner's car: DAB fault status `0x2F` → `0x2F & 0x01 = 1`, `0x2F & 0x40 = 0` →
**present**. Correct — the antenna is genuinely disconnected. Status `0x50` and `0x40` → unknown
(bit 6 set), which is exactly right for "the test has not run since the memory was cleared".

---

## C. The data columns

All figures from `SELECT`s run against the owner's own `data/Testmodule(1)/SQLiteDBs/DiagDocDb.sqlite`
(RC4, key `6505EFBDC3E5F324`) on 2026-07-18. **Total `XEP_FAULTCODES` rows: 206,647.**

Schema:

```sql
CREATE TABLE "XEP_FAULTCODES" ("ID" INTEGER PRIMARY KEY NOT NULL, "CODE" VARCHAR, "DATATYPE" VARCHAR,
  "WEIGHTING" INTEGER, "SCHEINFEHLER" VARCHAR, "AUSBLENDINDEX" VARCHAR, "RELEVANCE" INTEGER,
  "SICHERHEITSRELEVANT" INTEGER, "VALIDTO" DATETIME, "VALIDFROM" DATETIME, "DIAGNOSEINDEX" VARCHAR,
  "ECUVARIANTID" INTEGER, "RELEVANCE_F_VORH_NR" VARCHAR)
```

| Column | Distribution (real data) | Read by ISTA? | Verdict |
|---|---|---|---|
| **`RELEVANCE`** | `1` → 202,285 · `0` → **4,362** | **Yes.** `PlausibilityCheck.cs:41` → `DTC.Relevance`. Also `FaultCode.bRelevance` (`FaultCode.cs:310-320`) consumed at `TestplanCalculator.cs:137` — **a false `bRelevance` skips test-plan/diag-object expansion for that fault entirely.** | **USE IT.** The one column that matters. |
| **`WEIGHTING`** | `50` → 147,330 · `25` → 43,400 · `100` → 14,924 · `75` → 993 | **Yes, test-plan priority only.** `TestplanCalculator.cs:211-231`: `100`→`PrioSureSuspicionCount`, `75`→`PrioWeightStrong`, `50`→`PrioWeightMiddle`, `25`→`PrioWeightLow`; anything else logs "unknown" and is ignored. Not a filter, not displayed. | **Extract as an advisory severity hint.** Populated and meaningful. |
| **`SCHEINFEHLER`** | **empty string on all 206,647 rows** | **No.** Exhaustively: SELECT lists + `ReadDb` hydration in all three providers; property + `Equals` in `XEP_FAULTCODE.cs:122,347`; the `DataValueNames` array and `GetDataValue` switch in `FaultCode.cs:62,627,665`; interface decl `IXepFaultCode.cs:21`. **Zero conditionals, zero display bindings, zero filters.** | **DEAD twice over** — no data, no reader. Do not extract. |
| **`AUSBLENDINDEX`** | **empty string on all 206,647 rows** | **No.** Same profile (`XEP_FAULTCODE.cs:146,351`; `FaultCode.cs:62,629,668`; `IXepFaultCode.cs:9`). Appears in one further SELECT list (`ConWoyDataProviderSQLite.cs:1463`) but is not read out of that result either. Despite the name ("blank-out index") nothing blanks anything out. | **DEAD twice over.** Do not extract. |
| **`SICHERHEITSRELEVANT`** | **`0` on all 206,647 rows** | **Never read for `XEP_FAULTCODES` rows.** On `FaultCode` it appears only as a *write* target when synthesising combined/virtual fault codes from *other* tables (`FaultCode.cs:350` from `XEP_COMBINEDFAULTS`; `:507`/`:567` from `XEP_VIRTUALFAULTCODES`). | **DEAD.** Independently corroborates the existing memory `fault-doc-safety-flag-all-zero`. Do not extract. |
| **`RELEVANCE_F_VORH_NR`** | empty on 206,025 · **622 non-empty**, 107 variants, none on `d72n47a0`/`cas4_2` | **Yes** — §B.6. | **Understood, deliberately not implemented** (§D.7). |
| `VALIDFROM` / `VALIDTO` | **`0001-01-01 00:00:00` on all rows** | hydrated, never compared | DEAD. |
| `DIAGNOSEINDEX` | **empty on all rows** | hydrated, never compared | DEAD. |

**Keying.** `RELEVANCE` is keyed by **(CODE, ECUVARIANTID, DATATYPE)** — *not* by code alone. Concretely:

- `d72n47a0` (the shared N47 DDE): 916 codes relevant, **189 codes with `RELEVANCE=0`** (17% of 1,105).
  All 189 have labels, so all 189 are real, displayable faults that ISTA hides by default.
- `cas4_2`: 124 codes, **all relevant** — zero bogus.
- 353 distinct variants fleet-wide carry at least one `RELEVANCE=0` code.

A sample of `d72n47a0`'s hidden codes makes the intent obvious — they are actuators this engine
variant does not have, in an SGBD shared across variants:

| CODE | hex | title |
|---|---|---|
| 2384128 | `246100` | Exhaust counter-pressure after turbine, plausibility |
| 2415104 | `24DA00` | Bypass plate, activation: Open circuit |
| 2415360 | `24DB00` | Bypass plate, activation: Output stage, excess temperature |
| 2433280 | `252100` | Wastegate valve, activation: Open circuit |
| 2433536 | `252200` | Wastegate valve, activation: Output stage, overtemperature |

**Duplicate keys.** 1,489 `(CODE, ECUVARIANTID, DATATYPE)` groups have more than one row — this is
what ISTA's "Only the first hit will be returned!" warning is about, and ISTA's choice is
**nondeterministic** (no `ORDER BY`). Of those, only **3 groups disagree on `RELEVANCE`** and **15
disagree on `WEIGHTING`**. After the `SELECT DISTINCT` projection in §D.6 the table is 203,735 rows.
Given 3 disagreements out of 206,647, klartext should pick deterministically (see §D.6) and accept a
documented, negligible divergence from ISTA's coin-flip.

---

## D. What klartext should do

klartext's surfaces are the MCP server (an AI agent reads faults and reasons about them) and a future
mobile app. There is no GUI, no settings dialog, no colour column, no test plan. The mapping:

| ISTA mechanism | klartext analogue |
|---|---|
| `19 02 0C` on the wire (§A) | **Adopt exactly.** Change the default mask. |
| Every returned DTC added (§B.1) | **Adopt.** Delete the client-side status filter. |
| `RELEVANCE` DB lookup (§B.2) | **Adopt** as a tri-state field + a default-on filter. New DB extraction. |
| `hideBogusFaults=true` / `hideUnknownFaults=false` (§B.3) | **Adopt as tool defaults**, overridable per call. |
| ECU count / tree colour (§B.3) | Count: yes. **Colour: no analogue** — no GUI. |
| `FaultFilter` mileage window (§B.4) | **No analogue.** User-interactive workshop triage; an agent has no such session state, and the inputs need a detail read. Omit. |
| `FaultFilter` fault-class/group (§B.4) | **Defer** — needs `F_FEHLERKLASSE_NR`, only available from `FS_LESEN_DETAIL` (§D.5). |
| `SetDtcRelevanceBasedOnStatusByte` (§B.5) | **Adopt, correctly scoped** — only on the detail path. |
| `RelevanceFaultCode` allowlist (§B.6) | **Do not implement** (§D.7). |
| `SetExisting` / `IsFaultPresent` (§B.7) | **Adopt** — the single best replacement for `RELEVANT_MASK`. |
| `WEIGHTING` (§C) | **Extract**, surface as an advisory hint. No test plan to feed. |
| `SCHEINFEHLER` / `AUSBLENDINDEX` / `SICHERHEITSRELEVANT` | **Do not extract.** Dead in code *and* data. |

### D.1 `crates/uds/src/service.rs`

Replace the default mask constant. Keep the builder parameterised — that is ISTA-faithful (§A.2),
and the `19 02 FF` path stays reachable for diagnostics.

```rust
/// The DTCStatusMask every BMW `FS_LESEN` transmits: pending | confirmed.
///
/// ISTA's fault scan is `19 02 0C`, a hard literal in the SGBD bytecode — verified
/// register-for-register to the `xsend` in `d72n47a0` and `cas4_2`, and identical in
/// all 614 UDS-era `.prg` files in the fleet. The ECU therefore filters; klartext
/// applies no status filter of its own.
pub const ISTA_DTC_STATUS_MASK: u8 = dtc::status::PENDING | dtc::status::CONFIRMED; // 0x0C
```

- Keep `ALL_DTC_STATUS_MASK = 0xFF`, but re-document it as the **expert/diagnostic** mask, the
  counterpart of `FS_LESEN_EXPERT`'s argument #2 — not the default.
- Fix the stale doc comment at `service.rs:90-92` ("The report's workshop scan instead uses
  `CONFIRMED` (0x08)") — that is neither ISTA's behaviour nor klartext's.
- Add a test asserting `read_dtc_by_status_mask(ISTA_DTC_STATUS_MASK) == [0x19, 0x02, 0x0C]`.
- Update the `did::INFO_MEMORY` doc to note `IS_LESEN` is bimodal fleet-wide (`22 20 00` on both
  target cars, `19 17 0C 01` on 255 other ECUs) — §A.3.

### D.2 `crates/uds/src/dtc.rs` — delete `RELEVANT_MASK`, add presence

**Delete** `status::RELEVANT_MASK` (`:45`), `Dtc::is_relevant` (`:76-78`), and the test
`relevant_mask_partitions_stored_faults_from_not_tested_noise` (`:614-651`). Keep the individual bit
constants — they are ISO 14229 facts and are used by `status_flags`.

**Add** ISTA's presence verdict (§B.7), which is the honest replacement:

```rust
/// Whether a fault is failing right now, per ISTA's own rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Presence {
    /// Failing now: testFailed set, and the test HAS run this cycle.
    Present,
    /// Stored but not currently failing: testFailed clear, test has run this cycle.
    Absent,
    /// The test has not completed this operation cycle — nothing can be concluded.
    Unknown,
}

impl Dtc {
    /// ISTA's present/absent/unknown verdict for this fault.
    ///
    /// Mirrors `Fault.SetExisting` (UDS branch) and `TestplanCalculator`'s
    /// `IsFaultPresent`: ISTA tests `F_VORHANDEN_NR & 1` and `& 0x40`, where
    /// `F_VORHANDEN_NR` is the statusOfDTC masked in the SGBD by 0x4D or 0x6D.
    /// Bits 0 and 6 are in BOTH masks, so this is computable from the raw status
    /// byte with no per-SGBD mask and no detail read.
    pub fn presence(self) -> Presence {
        if self.status & status::TEST_NOT_COMPLETED_THIS_OPERATION_CYCLE != 0 {
            Presence::Unknown
        } else if self.status & status::TEST_FAILED != 0 {
            Presence::Present
        } else {
            Presence::Absent
        }
    }
}
```

Test vectors (each traceable to §B.7's enumeration or the on-car capture):

| status | presence | source |
|---|---|---|
| `0x2F` | `Present` | the owner's DAB fault, car session 1 |
| `0x09`, `0x0D`, `0x05`, `0x21`, `0x25`, `0x29`, `0x2D` | `Present` | `SetExisting` Yes-list |
| `0x04`, `0x08`, `0x0C`, `0x20`, `0x24`, `0x28`, `0x2C` | `Absent` | `SetExisting` No-list |
| `0x40`, `0x50`, `0x4D`, `0x60`, `0x6D` | `Unknown` | bit-6 fall-through |

### D.3 `crates/client/src/client.rs`

- `read_all_dtcs` (`:231`) currently passes `ALL_DTC_STATUS_MASK`. Point it at
  `ISTA_DTC_STATUS_MASK`. **Rename it** — `read_all_dtcs` becomes actively misleading once the ECU
  filters. Suggest `read_faults(target)`, keeping `read_dtcs(target, mask)` (`:221`) as the explicit-mask
  escape hatch.
- Update every call site (`scan.rs`, `mcp/src/server.rs`, CLI if still present at implementation time).

### D.4 `crates/client/src/scan.rs` — the partition disappears

With `19 02 0C` the ECU returns only pending/confirmed DTCs, so klartext has nothing left to
partition. `EcuFaults.relevant` / `EcuFaults.not_tested` (`:23`, `:26`) and the
`partition(|d| d.is_relevant())` at `:59` must go.

```rust
pub struct EcuFaults {
    pub address: u8,
    /// Every DTC the ECU returned to `19 02 0C` — ISTA adds them all (VehicleIdent.cs:2944).
    pub faults: Vec<Dtc>,
    pub error: Option<String>,
}
```

Likewise `ClearReport.after_relevant` (`:26`) → `after`, and drop the
`.filter(|d| d.is_relevant())` at `:109`. `verified_clean` should become `after.is_empty()`.

**Watch out:** `verified_clean` gets *stricter* under this change. Previously a post-clear DTC at
status `0x50` was filtered out and the clear counted as verified. Now `19 02 0C` will not return
`0x50` at all, so behaviour is in fact unchanged — but state this explicitly in the doc comment so a
future reader does not "restore" the filter.

### D.5 `crates/semantic/` — the relevance lookup

Add alongside `describe_dtc` (`catalog.rs:315`), keyed the way ISTA keys it (§B.2):

```rust
/// ISTA's catalog verdict for one fault on one ECU variant.
pub struct FaultRelevance {
    /// `XEP_FAULTCODES.RELEVANCE`: Some(false) = ISTA's "bogus fault"
    /// (hidden by default), Some(true) = real, None = no catalog row.
    pub relevance: Option<bool>,
    /// `XEP_FAULTCODES.WEIGHTING` — 100/75/50/25, ISTA's test-plan priority. Advisory.
    pub weighting: Option<i64>,
}

impl Catalog {
    /// Look up ISTA's relevance for `code` on `ecu_variant`.
    ///
    /// `datatype` is `"F"` (fault memory) or `"I"` (Infospeicher) — ISTA's own
    /// discriminator (VehicleIdent.cs:3521,3621). Returns `relevance: None` when
    /// no row matches, which is ISTA's tri-state "unknown" (PlausibilityCheck.cs:48),
    /// NOT a failure.
    pub fn fault_relevance(&self, ecu_variant: &str, code: [u8; 3], datatype: &str)
        -> Result<FaultRelevance, SemanticError>;
}
```

Notes for the implementer:

- Match `ecu_variant` **case-insensitively** — ISTA uses `COLLATE UTF8CI` (`ConWoyDataProviderSQLite.cs:2392`).
- The variant comes from the existing M10 `resolve_variant` ladder. **When the variant is unknown,
  return `relevance: None`** (unknown), *not* a guess across variants — ISTA's key includes the
  variant and 353 variants disagree about which codes are bogus.
- klartext's `read_info_memory` path should pass `datatype = "I"`.
- Do **not** implement the `XEP_RULES` per-row gate (§F.1).

### D.6 `scripts/build-semantic-db.sh` — the new extraction

Add a table beside the existing `sem.dtc`. It must be **separate**, not merged into `dtc`: `dtc` is
label-joined and drops codes with no label (`WHERE COALESCE(l.TITLE_ENGB, l.TITLE_DEDE) IS NOT NULL`),
whereas relevance must resolve for *every* code, labelled or not.

```sql
-- ISTA's per-(variant, code, datatype) fault verdict. Key and columns mirror
-- ConWoyDataProviderSQLite.GetFaultCodeByCodeAndVariantName (the query ISTA runs
-- to set DTC.Relevance). RELEVANCE=0 is ISTA's "bogus fault", hidden by default
-- under TesterGUI.HideBogusFaults. WEIGHTING (100/75/50/25) is ISTA's test-plan
-- priority, kept as an advisory severity hint. SCHEINFEHLER, AUSBLENDINDEX,
-- SICHERHEITSRELEVANT, VALIDFROM/VALIDTO and DIAGNOSEINDEX are deliberately NOT
-- extracted: each is constant-valued across all 206,647 rows AND never read by
-- any shipped ISTA code path.
CREATE TABLE sem.fault_relevance AS
  SELECT g.DIAGNOSTIC_ADDRESS      AS address,
         v.NAME                    AS ecu_variant,
         CAST(fc.CODE AS INTEGER)  AS code,
         fc.DATATYPE               AS datatype,
         MIN(fc.RELEVANCE)         AS relevance,
         MIN(fc.WEIGHTING)         AS weighting
  FROM XEP_FAULTCODES fc
  JOIN XEP_ECUVARIANTS v ON v.ID = fc.ECUVARIANTID
  JOIN XEP_ECUGROUPS   g ON g.ID = v.ECUGROUPID
  GROUP BY g.DIAGNOSTIC_ADDRESS, v.NAME, CAST(fc.CODE AS INTEGER), fc.DATATYPE;
CREATE INDEX sem.idx_fault_relevance ON fault_relevance(ecu_variant, code, datatype);
```

**Why `MIN` + `GROUP BY` rather than `SELECT DISTINCT`:** 1,489 key groups are duplicated and ISTA
resolves them by taking whichever row SQLite happens to scan first — nondeterministic and
unreproducible. Only 3 groups actually disagree on `RELEVANCE` and 15 on `WEIGHTING`. `MIN` makes
klartext deterministic and biases the 3 disagreements toward `0` (hide) — the conservative direction,
consistent with `hideBogusFaults=true`. **This is a documented, deliberate divergence from ISTA's
coin-flip; record it in `docs/superpowers/specs/2026-07-18-ista-parity-audit.md`.**

Expected result: **203,735 rows** (from 206,647 source rows).

The owner must re-run `scripts/build-semantic-db.sh` for this to take effect — same manual step as
every prior extraction.

### D.7 What NOT to build

- **`RELEVANCE_F_VORH_NR` / `RelevanceFaultCode` (§B.6).** Fully understood, but zero applicable rows
  on either of the owner's cars; it is a G-series ADAS/EV feature. Add a line to the parity audit:
  *"understood, deliberately not implemented, would matter only for G-series ADAS/EV ECUs."* If it is
  ever implemented: **do not hardcode `0x6D`** — the mask is per-SGBD and both target cars use
  `0x4D`; hardcoding `0x6D` would compare `0x2D` where ISTA compares `0x0D`.
- **`SCHEINFEHLER`, `AUSBLENDINDEX`, `SICHERHEITSRELEVANT`, `VALIDFROM`/`VALIDTO`, `DIAGNOSEINDEX`.**
  Constant-valued in the data *and* unread in the code.
- **The mileage window.** No agent analogue, and its inputs need a detail read.
- **Fault class / fault group filtering.** Genuinely part of ISTA's default behaviour (groups 6 and 7
  hidden), but `F_FEHLERKLASSE_NR` arrives only as an `FS_LESEN_DETAIL` job result. klartext does raw
  `19 04`/`19 06` reads, not the detail *job*, so the value is not currently available. **Defer to the
  milestone that runs `FS_LESEN_DETAIL` through the BEST/2 VM**; the two mapping functions are pure
  and fully specified in §B.4 when that day comes. Flag it in the audit as a known, scoped gap rather
  than silently skipping it.

### D.8 `mcp/src/` — the surface

`dto.rs:96` `FaultInfo` — add three fields:

```rust
pub struct FaultInfo {
    pub code_hex: String,
    pub status_hex: String,
    pub status_flags: Vec<String>,
    /// ISTA's presence verdict: "present" | "absent" | "unknown" (Dtc::presence).
    pub presence: String,
    /// ISTA's XEP_FAULTCODES.RELEVANCE for this (variant, code):
    /// Some(false) = ISTA's "bogus fault", None = not in the catalog.
    pub relevance: Option<bool>,
    /// ISTA's XEP_FAULTCODES.WEIGHTING (100/75/50/25) — advisory priority hint.
    pub weighting: Option<i64>,
    pub descriptions: Vec<FaultDescription>,
}
```

`dto.rs:107` `ReadFaultsResult` — **remove `not_tested_count`** (meaningless once the ECU filters);
**add `hidden_bogus_count`** (how many rows the relevance filter suppressed, so the agent can tell
"clean" from "filtered").

`server.rs:383-390` — replace the `is_relevant` partition with ISTA's relevance gate, defaults
matching ISTA's own:

```rust
// ISTA: TesterGUI.HideBogusFaults defaults true, HideUnknownFaults defaults false
// (VehicleIdent.cs:5440-5441). Same defaults here; both overridable per call.
let hide_bogus   = req.hide_bogus.unwrap_or(true);
let hide_unknown = req.hide_unknown.unwrap_or(false);
```

with the retention predicate transcribing `Logic.cs:5344-5355` exactly:

```rust
match relevance {
    Some(true)  => true,
    Some(false) => !hide_bogus,
    None        => !hide_unknown,
}
```

Replace the `include_not_tested` request field with `hide_bogus` / `hide_unknown`
(both `Option<bool>`). Document in the tool description that the ECU itself now filters by
`19 02 0C`, so a fault absent from the result is absent from the ECU's pending/confirmed memory —
**not** hidden by klartext.

`server.rs:1872` `fault_info` needs the resolved variant to look up relevance; thread it from the
same M10 `resolve_variant` ladder `describe_faults` already relies on.

**Also apply `SetDtcRelevanceBasedOnStatusByte` (§B.5) on the detail path only** — `read_fault_detail`,
where klartext has both the `19 06` status and the `19 04` snapshot count:

```rust
// VehicleIdent.cs:3315-3333 — no freeze-frames AND status exactly 0x60 or 0x20
// forces relevance false. Detail path ONLY; a plain 19 02 scan never runs this.
if snapshot_records == 0 && (ext_status == 0x60 || ext_status == 0x20) {
    relevance = Some(false);
}
```

Note it is an **exact equality** on the whole byte, not a mask test — do not "improve" it into
`status & 0x60`.

### D.9 Suggested implementation order

1. §D.1 + §D.2 (`crates/uds`) — pure, offline, unit-testable. Deleting `is_relevant` will break
   compiles at exactly the call sites that need review.
2. §D.3 + §D.4 (`crates/client`) — mechanical once (1) lands.
3. §D.6 (extraction) — then have the owner rebuild the semantic DB.
4. §D.5 (`crates/semantic`) — needs the rebuilt DB for its tests.
5. §D.8 (`mcp`) — the surface, last.

Gate as usual: `cargo fmt`, `cargo test`, `cargo clippy -- -D warnings` (run `cargo fmt` via Bash —
the Edit hook uses an older rustfmt).

---

## E. Risks

### E.1 The wire change is strictly narrowing — and measured at zero on the real car

`19 02 0C` returns a subset of `19 02 FF`. Statuses that pass klartext's old `0xAF` filter but fail
`0x0C` are those with bits from `{0x01, 0x02, 0x20, 0x80}` and neither `0x04` nor `0x08` — e.g.
`0x20`, `0x60`, `0x01`, `0x02`, `0x80`. The reverse set is empty (`0x0C ⊂ 0xAF`).

**Measured against the real capture** (`captures/captures/car-session-1.pcapng`, the F25 X3 session;
HSFZ frames parsed, all `59 02` responses walked as 4-byte records):

```
TOTAL 59 02 DTC records: 1498      ECUs answering 19 02: 20

  status  count   &0xAF (klartext)   &0x0C (ISTA)
   0x2F       3       PASS               PASS
   0x40     305       drop               drop
   0x50    1190       drop               drop

klartext (19 02 FF + 0xAF filter) surfaces: 3
ISTA     (19 02 0C on the wire)   surfaces: 3
DELTA: 0
```

**Only three distinct status values occur on the entire car**, and the two models agree on all
1,498 records. The three surfaced records are the same DAB antenna fault (`B7F805`, status `0x2F`,
ECU `0x63`) read three times across the session.

**Is there any fault the owner SAW that would vanish? No.** The one genuine fault (`B7F805` at
`0x2F`) passes `0x0C` cleanly (`0x2F & 0x0C = 0x0C`). The 1,495 dropped records are all `0x40`/`0x50`
— "not tested this cycle" / "not tested since clear" — which klartext *already* suppressed via
`RELEVANT_MASK`. Nothing the owner has ever been shown changes.

The second capture (`captures/klartext-session-2026-07-03.pcap`, the F20) contains no `59 02`
responses at all (it was a measurement-focused session), so it neither confirms nor contradicts.

### E.2 Secondary benefit: a large wire-volume reduction

With `19 02 0C` the ECUs return **3 records instead of 1,498**. On a car where ~10 of 32 ECUs
already time out under the whole-car sweep (a known open issue), cutting each fault response by
~99.8% is a meaningful reduction in per-ECU response size and parse time. It may partially mitigate
the sweep dropouts, though P1.1/P1.2 (retry + sequential comms) remain the actual fixes.

### E.3 Genuine behavioural risks

1. **The relevance filter is new suppression.** Switching the mask hides nothing extra, but turning on
   `hide_bogus` by default *does*: on `d72n47a0`, **189 of 1,105 catalog codes** are `RELEVANCE=0`.
   If one of those ever sets on the owner's car it will be suppressed by default where klartext used
   to show it. This is ISTA parity working as intended (they are actuators the engine variant does not
   have), and `hidden_bogus_count` plus `hide_bogus=false` make it visible and recoverable. **Call it
   out in the changelog** — it is the one user-visible regression risk in this work.
2. **Variant resolution becomes load-bearing for relevance.** With an unresolved variant, relevance is
   `None` → shown (since `hideUnknownFaults=false`). Fail-open, and correct, but it means relevance
   silently does nothing until the M10 ladder resolves. Surface the variant in the result so the agent
   can tell.
3. **Three nondeterministic duplicate keys** (§C, §D.6) — accepted, documented, negligible.
4. **`FS_LESEN_EXPERT` reachability.** Nothing in klartext will emit a non-`0x0C` mask after this
   change unless a caller explicitly asks. Keep `read_dtcs(target, mask)` public so the diagnostic
   `0xFF` scan remains available for future capture work — losing it would make a finding like §E.1
   impossible to reproduce.
5. **A concurrent branch is already touching these files.** At research time `git status` on
   `feat/ista-parity-p0` showed `crates/uds/src/dtc.rs`, `lib.rs`, `service.rs`,
   `crates/client/src/session.rs`, `crates/hsfz/src/lib.rs` modified by other agents in this session.
   The implementer must re-read current state before applying §D — the line numbers cited here are
   from the committed tree.

---

## F. Where ISTA proved unreadable

Three honest gaps. None blocks the change list in §D.

1. **`XEP_RULES` / `EvaluateXepRulesById` — readable in structure, not evaluated here.** The rule
   engine that gates each candidate `XEP_FAULTCODES` row (`ConWoyDataProviderSQLite.cs:2399`, via
   `RuleEvaluationUtill.RetrieveNotValidRulesIds`,
   `RuleEval/BMW.ISPI.TRIC.ISTA.RuleEvaluation.RuleVariantHandling/RuleEvaluationUtill.cs:45`) is
   *not* interface-only — the expression node types are all present
   (`SaLaPa`, `IStufe`, `Country`, `EcuVariant`, `Equipment`, `And`/`Or`/`Not`/`Compare`, …) and
   `RULE` is a serialized `byte[]` deserialized by `RuleExpression.Deserialize`. But porting it is a
   substantial logic project (it is the same `XEP_RULES` offline-fitment engine CLAUDE.md already
   lists as a separate milestone), and I did not decode the serialized format. **Impact:** only
   5,766 of 206,647 fault-code rows (2.8%) have a rule at all, and the gate can only *remove*
   candidate rows. Skipping it means klartext may return a relevance value where ISTA would have
   found none (→ `null`). Fail-toward-showing. Acceptable; note it in the audit.
2. **`IFFMDynamicResolverRuleEvaluation.Resolve`** is consulted in exactly one place in the whole
   rule engine (`RuleEval/.../RuleExpressions/EquipmentExpression.cs:59`); everywhere else it is
   threaded through unused. The implementation behind the interface was not chased. Moot given (1).
3. **`GetDataValue`-by-name reachability from ABL/service-program scripts.** The five
   `XEP_FAULTCODES` columns are reachable *by string name* through `FaultCode.GetDataValue`
   (`FaultCode.cs:601,647`, `ISPELocator.cs:28`, `FaultCodeLocator.cs:98,103`). I grepped every
   literal call site in the shipped C# — only `"F_SELEKT_CODE"` and `"Code"` are ever requested
   (`FKB_AnzeigeServiceDlgImpl.cs:189,1052`). So **"dead in the shipped C#" is verified; "dead
   everywhere" is not** — ABL/service-program data files were not scanned. Given `SCHEINFEHLER` and
   `AUSBLENDINDEX` are *also* empty on all 206,647 rows, this gap cannot change the §D.7 conclusion.

**A methodology note worth keeping.** The initial premise that `RELEVANCE_F_VORH_NR` appears in zero
DLLs was a **`strings` artifact, not a fact**: .NET string literals live in the `#US` metadata heap as
UTF-16LE, and plain ASCII `strings`/`rg` misses them. Sweeping the 147 DLLs with `strings -a -e l`
returns 6 hits, all in `RheingoldConWoyDataConnector.dll`. **Use `strings -e l` (or `rg -a` with a
UTF-16 encoding) when hunting ISTA SQL — several "not used anywhere" conclusions in past klartext
research may deserve a re-check under this lens.**
