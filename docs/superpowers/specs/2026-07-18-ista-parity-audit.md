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

### P0.1 Post-clear ECU reset: ISTA does not do this at all — ✅ FIXED (`9dc80db`)
**ISTA:** `VehicleIdent.ClearErrorInfoMemoryVehicle` (`VehicleIdent.cs:9720-9788`) sends **no UDS
ECUReset**. All three `STEUERGERAETE_RESET` call sites (`VehicleIdent.cs:289,1328,2473`) are unrelated
recovery paths (FEM_20 read-failure retry, MOST-gateway wake) and none is reachable from the clear
flow. Completion is instead an **ignition power-cycle**: `ClampSwitchVehicle.DoClampSwitch` —
automated via a BN2020+PAD test module, or an interactive prompt polling `VCI.GetClamp15()`.

**klartext (before the fix):** `clear_faults_all_with_reset` (`crates/client/src/scan.rs:171-205`)
sent `11 01` per cleared ECU, gateway excluded.

**Assessment.** Our `0x11` was a klartext invention approximating the *effect* of the ignition cycle.
**RESOLVED — the owner ruled: drop it, and add NO ignition-cycle instruction** (see Owner Rulings
below). Note the earlier guess that ISTA "had him cycle the ignition" is NOT established — see
ruling 1 for what is and is not known. The dash reset he saw is more plausibly the wider wipe (P2.1).

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

### P0.4 Drop the CLI — ✅ DONE (`a6fab70`)
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

## CORRECTIONS TO THIS AUDIT — from the 2026-07-18 research records

Three of this audit's claims were wrong. They were derived from decompiled C# without
cross-checking the ECU bytecode, which is precisely the failure the parity mandate warns about.

### C1. The cluster reset is a SOFTWARE TERMINAL-15 CYCLE — mechanism found
`docs/superpowers/specs/2026-07-18-research-p2-clear-sequence.md` §E.

After **every** fault clear, unconditionally and silently, ISTA commands terminal 15 OFF → wait
15 s → ON. Entry `ClearAndReadErrorInfoMemory` (`VehicleIdent.cs:9620`) calls `DoClampSwitch`
(`:9648`), which runs the test module `ABL-LIF-KLEMMENSTEUERUNG` in automatic mode
(`IN_konfig="KLwechsel"`, `IN_pause=15000`, `IN_automode`/`IN_automaticRun` true) and issues
`STEUERN_KLEMMEN` to `CAS4_2`/`BDC`/`FEM_20` — UDS `0x31` startRoutine, RID `0x1001`.

**The owner saw no prompt because there is none on the success path:** the ignition dialog is
registered only inside `if (!CallTestModuleForClampSwitch(...))`, and every `CreateServiceDialog`
in the module sits behind `if (manuell)` or `if (!IN_automaticRun)` — both false here. The
earlier `CheckForAutoSkip` theory is superseded; nothing self-dismisses because nothing is shown.

This **fully explains the owner's original observation** and retroactively confirms the P0.1
ruling was right for the wrong reason: our `0x11` was indeed an invention, but what it was
imitating is a clamp cycle, not an ECU reset.

**NOT IMPLEMENTED, and blocked on two things — owner decision required.**
(a) The exact wire payload is NOT pinned: the `cas4_2.prg` template is `31 01 10 01 FF FF FF`
with three placeholder bytes, and `TAB_CAS_KLEMMENSTATUS_ARG` supplies decimal `6` (KL30B_EIN =
KL15 off) / `10` (KL15_EIN). How the value patches into those bytes was not disassembled to
completion. Guessing here would cut terminal 15 on a real car with invented bytes — exactly what
the mandate forbids. Resolve by disassembling `STEUERN_KLEMMEN` to its `xsend`, or by capturing
ISTA performing a clamp switch.
(b) It is a materially new PHYSICAL capability (a `0x31` write that drops the car's electrical
system for 15 s), well beyond anything klartext does today. It needs the owner's explicit
go-ahead, not inference from the general "match ISTA" directive.

### C2. ISTA does NOT broadly clear info memory — `IS_LOESCHEN_FUNKTIONAL` is dead code
Same record, §C.3. The guard at `VehicleIdent.cs:9747` is provably always false. ISTA broadly
**reads** info memory (`IS_LESEN_FUNKTIONAL`, `:3461`) but only ever **clears** the six hardcoded
supplier-specific stores. **Do not implement a general info-memory clear.** P2.1 below overstates
this. The check-control hypothesis is also disproven: an exhaustive UTF-16LE sweep of all 147 DLLs
found only CCM *reads* — no `CC_LOESCHEN`, no `STEUERN_CC`, no `CCM_LOESCHEN` exists anywhere.

### C3. Freeze-frames do NOT arrive inline — P2.3 below is REFUTED
`docs/superpowers/specs/2026-07-18-research-p2-fault-bundle.md` §HEADLINE. `FS_LESEN` has exactly
one `xsend`, emits `19 02 0C`, and produces **no `F_UW*` result at all`. Freeze-frames come from
`FS_LESEN_DETAIL`'s three `xsend`s (`19 09`, `19 06`, `19 04`), issued **per fault**. The C# that
appeared to populate them inline (`:2874-2905`) is inside the legacy DS2/pre-UDS branch, dead on
these cars. klartext's existing separate `read_fault_detail` is therefore the architecturally
correct mechanism and must stay — P2.3 is a cost (+2-3 requests per fault), not a free win.

### C4. The "error 98 = success" premise is WRONG — it is a CATALOG condition
`…-research-p1-resilience.md` §A. This audit's P1.1 states ISTA treats EDIABAS error code 98 as
success, implying klartext should map it to some wire outcome. **Error 98 is `SYS-0008: JOB NOT
FOUND`** (pinned via `apiNET464.cs:516` plus the native core's string table) — the requested job
does not exist in the ECU's SGBD. It is not a timeout, not a transport error, and not an NRC.
**klartext can never observe it**, so there is nothing to map. Do not build a retry rule around it.

Two further P1.1 premises collapse:
- **`ERROR_ECU_NACK`'s specific NRC is NOT DETERMINABLE** — absent from the native string table and
  from all 1,472 decoded ops of `FS_LESEN`. Only the structural inference survives: the ECU
  *answered*, so it is a negative response rather than a timeout.
- **`DoECUIdentDeepAwake` is a dead end.** It is four `STEUERN_*` actuation jobs — gated writes
  under the tier ladder — and it would address a sleepy-ECU problem the capture shows does not
  exist.

### C5. Two facts now pinned that the audit left open
- **Functional clear = `14 FF FF FF` to target `0xDF`** — the identical UDS payload klartext already
  sends physically; only the address differs. Proven by running `f01.prg` through klartext's own VM:
  functional `C4 DF F1 14 FF FF FF` vs physical `84 12 F1 14 FF FF FF`. The group SGBD is `F01` for
  both F20 and F25. Responders are distinguished by source address; the bytecode deliberately skips
  the source==target check, caps at 100 responders, and exits on a quiet trap. (The HSFZ target byte
  itself is INFERENCE from the frame layout, not a capture — `XEnet32/64.dll` is native PE.)
- **`STEUERN_ZFS_LOESCHEN` = `31 01 40 00 FF`**, gateway-local, no cascade.

### C6. A divergence needing the owner's sign-off
klartext's planned automatic retry (P1.1) applies to **reads only**. ISTA's job-level retry would
repeat a `0x14` clear. Excluding writes is almost certainly right — silently re-sending an actuation
contradicts the tier ladder — but under a 1:1 mandate it is a deliberate divergence, recorded here
rather than baked in silently.

### OWNER RULING 2026-07-19: "Continue and follow ISTA's behavior."
Both open decisions below are resolved in favour of ISTA:

1. **The clamp cycle IS implemented.** After a confirmed clear, klartext performs ISTA's
   terminal-15 OFF → 15 s → ON cycle (`31 01 10 01 06 06 A8` / `31 01 10 01 0A 0A 43` to
   `0x40`). The owner was shown the pinned bytes, the CRC-8 constraint, and the fact that
   ISTA has NO safety net — an interruption in the 15-second window leaves terminal 15 down,
   and whether the CAS re-raises it is ECU firmware behaviour no shipped artifact states —
   and ruled to follow ISTA anyway. It runs behind the clear's existing `confirm=true`,
   which is where ISTA also places it (unconditional, once the user has asked to clear).

   **ONE DELIBERATE SAFETY DIVERGENCE, recorded rather than assumed:** klartext makes a
   best-effort attempt to command KL15 ON if the sequence fails after the OFF. ISTA does
   not — its module has zero `try`/`catch`/`finally`/`Dispose` in 1,552 lines, and its own
   cancel path returns without restoring. This changes nothing on the happy path; it only
   avoids knowingly shipping a path that can leave a car unable to start. If the owner wants
   strict parity here too, delete the restore.

2. **VIN mismatch aborts.** `connect` now fails on a mismatch, as ISTA does, instead of
   completing with a warning. Supersedes C6b below.

### C6b. (SUPERSEDED by the 2026-07-19 ruling above) VIN mismatch on reconnect
Implemented in `9e3575a`. **On a VIN mismatch ISTA hard-aborts and fully disconnects**
(`VciConnLossVM`). klartext's `connect` instead COMPLETES and reports loudly — `vin_check:
"mismatch"` plus a note telling the agent that any findings from the previous car are void.

Rationale for the divergence: ISTA's user pressed *Reconnect* to resume an existing session, so
aborting is right there. klartext's `connect` is an explicit request for a NEW session, and
refusing it would strand a human who simply moved the cable to a different car. The information
an agent needs — "this is not the car you were just reasoning about" — is delivered either way.

**Not yet ruled on by the owner.** If he prefers strict parity, `connect` should fail on
mismatch instead.

### C7. Also corrected
- The clear's entry point is `ClearAndReadErrorInfoMemory` (`:9620`), which wraps clear → clamp
  switch → re-ident → verify. `ClearErrorInfoMemoryVehicle` (`:9720`) is only the clear phase.
- The ZFS gate is INVERTED from the guess below: `IsVehicleInNewGeneration` is **false** for
  F20/F25, so ZFS is **not** skipped — it DOES run. (It remains out of scope: one ECU, 8 round
  trips, and a wire protocol that is not statically resolvable.)
- A separate real bug the research surfaced, now fixed in `cddf839`: `decode_info_memory`
  consumed a version byte that does not exist on the wire.

## Implementation order agreed
P0.4 (drop CLI — removes surface first) → P0.1 (drop 0x11) → P0.3+P0.2 (mask + relevance) →
P1.1 (retry) → P1.2 (serialise) → P2.1 (wider clear) → P2.2/P2.3 (bundle + inline freeze frames) →
P1.3 (VIN re-check) → P3/P4.

## NEW FINDING — info memory has THREE wire forms; klartext implements one
Established 2026-07-18 by sweeping all 1,403 shipped SGBDs with klartext's own
`klartext_best::decode_job` (not in any research spec — found while verifying P0.3):

| Job | Wire form | SGBDs |
|---|---|---|
| `IS_LESEN` | `22 20 00` | 334 |
| `IS_LESEN` | **`19 17 0C 01`** (reportUserDefMemoryDTCByStatusMask, memory `01`) | **255** |
| `IS_LESEN_DETAIL` | `22 20 00` | 334 |
| `IS_LESEN_DETAIL` | `19 09` + `19 18` + `19 19` | 163 |
| `IS_LESEN_DETAIL` | `19 18` + `19 19` | 92 |

klartext's `read_info_memory` implements **only `22 20 00`**, so on roughly 43 % of the ECUs
that have an info memory it will read nothing and report the store as unsupported. Note the same
`0x0C` status mask appears in the `19 17` form — consistent with P0.3's finding. Worth folding
into the P2.2 bundle work rather than fixing standalone.

## Progress
- ✅ **P0.4** (`a6fab70`) — `cli/` deleted, 2,362 lines. README purged of it, plus two
  claims this audit disproved (the bordnet "explains ISTA's ~11" line and the safety
  section describing writes as living in the CLI).
- ✅ **P0.1** (`9dc80db`) — the `0x11` reset is gone from both clear paths, along with
  `clear_faults_all_with_reset`, `is_reset_target`, `reset_targets`,
  `DiagnosticClient::ecu_reset`, the `reset` request flag and reset fields on both MCP
  clear DTOs, and every reset clause in the tool descriptions and refusal messages.
  No ignition-cycle instruction was added, per the owner's ruling.
  **Kept deliberately, with the reasoning corrected (`4c1ac0a`):** `klartext_uds::{sid::ECU_RESET,
  reset_subfn, ecu_reset}` stay purely as protocol vocabulary in a crate whose job is pure UDS
  message construction — they have **no caller anywhere in the workspace**, and the first draft of
  this bullet wrongly claimed the BEST/2 VM consumed them (it does not: the VM emits bytes from
  `.prg` literals, and `crates/best` never references `klartext-uds` at all). The gate keeps
  refusing `0x11` regardless, via its fail-closed `_` arm rather than an enumerated one — which is
  what a VM-run `STEUERGERAETE_RESET` actually needs, since nobody has to have listed the SID.
  Both MCP no-reset frame censuses are mutation-proven; the client-crate test is NOT a no-reset
  guard (see below).
- ✅ **P1.1 + P1.2** (`f5050b0`) — the whole-car sweep is sequential (the `concurrency`
  knob is REMOVED, not defaulted to 1) and a timed-out **read** is repeated once
  (`RETRY_COMM = 1`). `is_retry_safe` lives in `klartext-uds` as a predicate deliberately
  separate from the blast-radius gate. Writes are never retried — a documented divergence
  (C6). Mutation-verified on all three behaviours after the implementing agent died mid-task
  without reporting: making the clear retry-safe, retrying on any error, and overlapping
  address pairs each fail their specific test.
- ✅ **P0.3 evidence** — `19 02 0C` confirmed across the fleet (614 `FS_LESEN`, 388/388
  `FS_LESEN_EXPERT`); `19 15` for `FS_LESEN_PERMANENT` (95). Implementation still pending,
  paired with P0.2.
- ✅ **P2.1** (`a222b7e` + `2b1a940` + `f5caa43`) — ISTA's whole-vehicle clear sequence:
  functional `14 FF FF FF` broadcast to `0xDF` first, physical only for stragglers
  (fitted ∧ had-faults ∧ silent), the six hardcoded supplier gates, the gateway ZFS
  (`31 01 40 00 FF`), the terminal-15 cycle, 500 ms → re-ident → 200 ms → verification
  read. Nothing aborts; every step is best-effort and reported, as in ISTA.
  Mutation-verified (5): skipping the broadcast, dropping the clamp cycle, and all three
  supplier-gate traps (undecodable SALAPA read as present, `D_KBM` gated on itself instead
  of the FRM, `D_0066` looked up as SGBD instead of GRUPPE) each fail their own tests.
  **Two divergences, both in klartext's favour and both documented in code:** the pre-read
  (klartext never clears blind; ISTA has no counterpart) and `verified_clean` (ISTA's
  verification read is seven steps and never diffs before against after).

  **KNOWN GAP — the six supplier stores are NOT cleared.** They are selected correctly and
  reported, but not transmitted: klartext has no path to execute an EDIABAS job as a *write*
  (the read-only transmit gate refuses the services they emit, and three of the six targets
  are group SGBDs needing EDIABAS group→variant dispatch that klartext has not built). The
  MCP note says so explicitly to the human rather than implying a complete clear. Closing it
  needs `Policy::ConfirmedWrite` wired to the BEST/2 VM — the P3 service-write tier.

- ⏳ Remaining: P2.2/P2.3 (fault-read bundle + per-fault freeze-frames — researched, not
  started), P3, P4, the info-memory wire-form gap above, and the supplier-store gap noted
  under P2.1.
