# ISTA parity audit — full divergence inventory and fix ordering

**Owner ruling, 2026-07-18:** klartext must be 1:1 with ISTA's behaviour. Anything derived,
approximated or invented is a defect. Fix everything here **before** building new features.

**Method.** Four parallel audits, each decompiling the ISTA component that owns a behaviour
(`ilspycmd` over `data/TesterGUI/bin/Release/`) and diffing it against klartext. Two audits went
further and disassembled the ECU's own BEST/2 `.prg` bytecode with klartext's own crates. One found
the shipped `EDIABAS.INI` — ground-truth runtime config. Every claim below carries a `file:line`.

**What is NOT readable, so cannot be 1:1 by construction** — match observable behaviour, never invent
an algorithm and call it parity:
- **KMM rules engine** (`IKmmService`/`IKmmModule`) — interface only, no managed implementation.
- **PSdZ Sollverbauung matcher** — external service behind a web-API adapter.
- **`XEnet32/64.dll`** — native PE. Owns the real TesterPresent cadence and NRC `0x78` retry.

---

## P0 — corrections to code already shipped on main

### P0.1 Post-clear ECU reset: ISTA does not do this at all
**ISTA:** `VehicleIdent.ClearErrorInfoMemoryVehicle` (`VehicleIdent.cs:9720-9788`) sends **no UDS
ECUReset**. All three `STEUERGERAETE_RESET` call sites (`VehicleIdent.cs:289,1328,2473`) are unrelated
recovery paths (FEM_20 read-failure retry, MOST-gateway wake) and none is reachable from the clear
flow. Completion is instead an **ignition power-cycle**: `ClampSwitchVehicle.DoClampSwitch` —
automated via a BN2020+PAD test module, or an interactive prompt polling `VCI.GetClamp15()`.

**klartext:** `clear_faults_all_with_reset` (`crates/client/src/scan.rs:171-205`) sends `11 01` per
cleared ECU, gateway excluded.

**Assessment.** This almost certainly explains the owner's original observation ("ISTA's clear-all
reset the dash") — ISTA had him cycle the ignition. Our `0x11` is a klartext invention that
approximates the *effect*. **Owner decision required:** match ISTA (drop `0x11`, surface an
ignition-cycle instruction) or keep ours as a documented, agreed divergence.

### P0.2 Fault relevance filter: `RELEVANT_MASK` has no ISTA counterpart
**ISTA:** grepping all 147 DLLs for the ISO-14229 status-bit names our mask encodes returns **zero
hits**. ISTA adds every returned DTC to the displayed list (`VehicleIdent.cs:2944,2595`), then uses
(a) `XEP_FAULTCODES.RELEVANCE` per fault **code** (`PlausibilityCheck.cs:33-56`) affecting only a
count, gated by `TesterGUI.HideBogusFaults` (default **true**) / `HideUnknownFaults` (default false)
(`VehicleIdent.cs:84-86,5440-5441`); (b) a user `FaultFilter` by fault **class** (default 1-5) and
mileage; (c) exactly one status check — `SetDtcRelevanceBasedOnStatusByte`
(`VehicleIdent.cs:3315-3333`) — forcing `Relevance=false` only when `F_STATUSBYTE` is **exactly**
`0x60` or `0x20` **and** the fault has no freeze-frame records.

**klartext:** `RELEVANT_MASK = 0xAF` applied uniformly (`crates/uds/src/dtc.rs:45,76-78`).

**Also:** we extract none of the columns ISTA actually uses. `XEP_FAULTCODES` carries `RELEVANCE`
(0 on 4,362 codes / 1 on 202,285), `SCHEINFEHLER`, `AUSBLENDINDEX`, `WEIGHTING`,
`SICHERHEITSRELEVANT`.

### P0.3 Fault-read status mask on the wire
**ISTA:** `19 02 0C` — pending|confirmed only. Proven by disassembling `FS_LESEN`/`FS_LESEN_EXPERT`
bytecode: the literal `Str([25,2,12])` traced register-for-register to the `xsend`, **byte-identical
in two independent SGBDs** (`d72n47a0`, `cas4_2`).
**klartext:** `19 02 FF` (`crates/uds/src/service.rs:93`) then client-side filtering. A DTC with only
`testFailedSinceLastClear` (0x20) is never returned to ISTA at all, but passes our filter.

### P0.4 Drop the CLI
2,245 lines, leaf binary, nothing depends on it, and it already carries a known duplicate of the
multi-job defect (`format_job_args`). The owner does not use it. Removing it deletes parity surface
rather than requiring parity work.

---

## P1 — resilience: fixes behaviour observed to fail on the real car

### P1.1 Automatic retry (root cause of the 10-of-32 sweep dropouts)
**ISTA:** `EDIABAS.INI:31` — `RetryComm = 1`, documented at lines 403-408 as "Repeat failed
communication automatically (1x)". Above that, `doMissingECUIdent` (`VehicleIdent.cs:869-933`) loops
`retry+1` times and escalates to `DoECUIdentDeepAwake` (bus/gateway reanimation). ISTA also treats
EDIABAS error code 98 and job state `ERROR_ECU_NACK` as **success**
(`VehicleIdent.cs:2827-2830,2968-2971,2981-2985`).
**klartext:** zero retry anywhere (`crates/client/src/session.rs` — no occurrence of "retry").

### P1.2 Sequential ECU communication
**ISTA:** `ECUKom` holds a single blocking EDIABAS handle (`ECUKom.cs:78`); no parallelism found in
the comms/session/diagnostics DLLs. Fault reads are plain synchronous `foreach`.
**klartext:** `scan_faults` fans out `buffer_unordered(concurrency)`, default 8
(`crates/client/src/scan.rs:86-113`).
**Note:** contention — not sleepy ECUs — may be the real cause of the dropouts. Testable in one run
by forcing concurrency to 1.

### P1.3 Reconnect with mandatory VIN re-validation
**ISTA:** detects loss, surfaces a modal reconnect prompt
(`OpenConnectionLossPopupToAskUserForReconnection`), and on reconnect **re-validates the VIN** before
resuming — `CheckVinOverConnectionLossPopup` → `CompareSessionVinToEcuJobVin`, with a dedicated
`ConnectionLossError::VehicleVinNotMatch` (`Logic.decompiled.cs:100,3698-3723`). Ignition is checked
at reconnect time, not monitored proactively.
**klartext:** no reconnect path; a dropped session fails every waiter
(`crates/client/src/session.rs:148-160`) and nothing verifies the same car is on the other end.
**The VIN re-check is the piece worth porting regardless of how reconnect is triggered.**

---

## P2 — scope: operations that are narrower than ISTA's

### P2.1 Clearing is FS-only
**ISTA** clears, in order: functional/broadcast `FS_LOESCHEN_FUNKTIONAL` first, physical `FS_LOESCHEN`
only for stragglers (`VehicleIdent.cs:606,671,687`); **info memory** `IS_LOESCHEN(_FUNKTIONAL)` plus
six component-specific supplier jobs (FEM_20 → `IS_LOESCHEN_TMS`; FRM3, D_KBM, D_0066, ALC_60, D_LM)
(`VehicleIdent.cs:737,769,9733-9780`); and the gateway's combined store
`STEUERN_ZFS_LOESCHEN` on `G_ZGW` (`VehicleIdent.cs:9783`). Then full re-identification, then **one
whole-vehicle** verification read (`VehicleIdent.cs:9790`).
**klartext** clears FS only, per-ECU, verifying per-ECU.

### P2.2 "Read faults" is a bundle
**ISTA** always runs the same sequence at ~13 call sites: `DoECUReadFS` → `DoECUReadIS` (`22 2000`) →
`DoECUReadFSDetails` → conditionally `ZFS` (gated `IsVehicleInNewGeneration`,
`DiagnosticsBusinessData.cs:998-1005` — likely not F20/F25).
**klartext** exposes three separate, independently-invoked operations.

### P2.3 Freeze-frame data arrives inline
**ISTA:** plain `FS_LESEN` already populates `F_UW_KM`/`F_UW_ZEIT`/`F_UW_ANZ`/`F_UW[]` per DTC
(`VehicleIdent.cs:2834-2991`) — no second round trip.
**klartext:** base `Dtc` is code+status only; freeze-frame needs a separate `read_fault_detail`.

### P2.4 Measurement reads bypass the job
**ISTA** never builds a raw UDS frame for a measurement — every read is
`ApiJob(variant, job, ";"-joined args, "")` → native `api64.dll` → the ECU's bytecode
(`EcuFunctionReadStatus.cs:176-178`). There is **no dynamic-vs-static branch anywhere** in ISTA.
**klartext's** `read_data` hand-builds `22` / `2C`-define frames, and
`misrouted_dynamic_measurement` is a klartext invention compensating for not running the job. It is
on-car proven and worth keeping — but it is **not** parity, and must stop being described as such.
`run_job` is the only structurally analogous path.

### P2.5 Catalog post-scaling is extracted but never applied
**ISTA** always applies `value * Multiplikator + Offset`, then `Zahlenformat` formatting, else
`Runden` rounding (`EcuFunctionReadStatus.ConvertResultValue:286-320`), on top of the job's result.
**klartext** parses `mul`/`offset`/`round`/`format` into `MeasurementCatalogEntry` and applies none.
Fleet: 155 non-identity multipliers, 30 non-zero offsets, 706 formats, rounding on **all** 52,207.
**Caution:** ISTA applies this on top of a possibly already-scaled job result. The one affected
variant we could inspect (`MRMA24`, `STAT_TEMPERATUR_WERT`, mul 0.1) has no local `.prg`, so
double-scaling is **unresolved** — do not apply blind. `d72n47a0` has zero non-identity mul/offset.

---

## P3 — ECU presence: wrong mechanism for the owner's own cars

**ISTA branches on `BNType`** (`VehicleIdent.cs:7189,7203-7231`):
- **BN2020 (G-series):** live `STATUS_VCM_GET_ECU_LIST_ALL` (`VehicleIdent.cs:7579`) — this **is**
  klartext's `22 3F07`. Real parity. Plus live per-bus lists
  (`STATUS_VCM_GET_ECU_LIST_MOST/_BODY_CAN/_K_CAN/_FLEXRAY/_FA_CAN`, `VehicleIdent.cs:7620-7644`).
- **BN2000 (F20/F25 — the owner's cars):** `ExecuteEdiabasJobBN2000` (`VehicleIdent.cs:7443-7464`)
  **never** calls `GetAllEcus`. It runs `DoKMMCheckConfig` (`VehicleIdent.cs:6366`), building a key
  from the car's own FA equipment codes (every SA + `E_WORT` + `HO_WORT` + type + both I-levels,
  lines 6419-6469) and asking the KMM rules engine for `GetExpectedEcuAddresses` (line 6488).
  Presence is **computed from the vehicle order**, not read from the gateway. PSdZ Sollverbauung
  reconciliation layers on during re-identification (`VehicleIdent.cs:1024,7276,8557,8605`).

**The `minimal` bordnet flag is not ISTA's presence filter.**
`VehicleLogistics.CalculateECUConfiguration`/`ShapeECUConfiguration` are called from exactly two
sites — motorbikes (`VehicleIdent.cs:7267`) and the no-vehicle simulator (line 7649). On a real car
the bordnet is display topology only (`EcuTreeBuilder.IsEcuRelevant` filters only virtual buses).
Our 32→11 match was **the right number by the wrong route**.

**Also:** the bordnet has five categories — `minimal`/`excluded`/`optional`/`unsure`/`xor`
(`BaseEcuCharacteristics.cs:43-51`); we kept one boolean. ISTA tracks a **3-state** identification
confidence per ECU (`SetECUColor`, `VehicleIdent.cs:5879`), never removing a configured-but-silent
ECU; we have a binary `responding` flag. No ISTA counterpart for `22 3F08` was found in any DLL.

---

## P4 — features absent from klartext entirely

- **Permanent DTCs** — `FS_LESEN_PERMANENT` emits `19 15` (reportDTCWithPermanentStatus, confirmed by
  bytecode). Our `dtc_subfn` has only `02/04/06/09`.
- **Virtual/synthetic fault entries** — ISTA inserts a *fault list entry* for an ECU that does not
  answer or has a programming error (`HandleVirtualErrorCodes`/`AddVirtualErrorCode`,
  `VehicleIdent.cs:3040-3112`, from `XEP_VIRTUALFAULTCODES`). We only log a scan-level error string.
- **Combined faults** — cross-ECU rule synthesis (`TransactionCalculateCombinedFaults`,
  `XEP_COMBINEDFAULTS` + `XEP_RULES`).
- **State-value decoding** — `XEP_STATEVALUES` maps a raw result to localized text ("Active"/"Fault")
  via `FindMatchingValue` (`EcuFunctionReadStatus.cs:395-444`). We show a raw number; the table is not
  even extracted.
- **Central gateway fault memory (ZFS)** — `STATUS_ZFS_LESEN_GESAMT` on `G_ZGW`, rich per-fault record
  (`VehicleIdent.cs:4448-4591`). Gated to newer platforms; likely not F20/F25.
- **Per-result display gate** — `EvaluateXepRulesById` (`EcuFunctionReadStatus.cs:112`), the
  `XEP_RULES` engine, already known as separate future work.

---

## Confirmed correct — do not "fix" these

- **Every wire constant**, validated against the shipped `EDIABAS.INI`: 6-byte header
  (`HeaderFormat=0`, line 129), ports 6801/6811 (134,137), tester `F4` (130), connect timeout 5000
  (142), discovery window 2000 (143), the `00 00 00 00 00 11` datagram and `BMWMAC`/`BMWVIN` anchors.
- **BEST/2 job invocation shape** — job name + `;`-joined positional args, matching
  `XEP_ECUJOBSEX.GetParameterString()` (`XEP_ECUJOBSEX.cs:179-192`).
- **VM failure handling** — mirrors real EDIABAS trap semantics (`IFH_0009`, trap bit 19), not merely
  ISTA's C# wrapper.
- **`reset_subfn::HARD` = `11 01`** as the bytes BMW's own `STEUERGERAETE_RESET` job emits (that job
  is real; it is simply not part of the clear flow — see P0.1).

## OWNER RULINGS 2026-07-18 — all three decided, no open questions

1. **P0.1 — DROP the `0x11` ECU reset. Do NOT add an ignition-cycle instruction.**
   Owner: *"no need to instruct an ignition cycle — when I had ISTA wipe my faults it never told me
   anything, it just wiped and reset everything."*
   Follow-up investigation: `ClampSwitchVehicle.ManualClampSwitch` DOES register an ignition prompt
   for a BN2000 car, but `CheckForAutoSkip` (`ClampSwitchVehicle.cs:235-254`) polls
   `VCI.GetClamp15()` every second and calls `interaction.Continue()` the moment the voltage
   condition is met — the prompt can silently self-dismiss. UNRESOLVED whether that is what happened
   for the owner (a plain ENET cable may report no clamp voltage at all, and he may have used a
   different UI entry point than `ClearAndReadErrorInfoMemory`). **The design does not depend on
   resolving it:** klartext has no VCI clamp control and cannot perform a clamp switch over ENET, so
   we simply do not do one. The dash reset he observed is most plausibly the WIDER WIPE (P2.1) —
   info memory + gateway ZFS + check-control — clearing the cluster's warnings.

2. **P0.2 / P0.3 — ADOPT ISTA's model wholesale.** Send `19 02 0C`; delete `RELEVANT_MASK`; extract
   and use `XEP_FAULTCODES.RELEVANCE`/`SCHEINFEHLER`/`AUSBLENDINDEX` + fault class. Owner accepts
   that this changes which faults are shown.

3. **P1.2 — MATCH ISTA: serialise.** Whole-car reads become sequential, as ISTA is.

## Implementation order agreed
P0.4 (drop CLI — removes surface first) → P0.1 (drop 0x11) → P0.3+P0.2 (mask + relevance) →
P1.1 (retry) → P1.2 (serialise) → P2.1 (wider clear) → P2.2/P2.3 (bundle + inline freeze frames) →
P1.3 (VIN re-check) → P3/P4.
