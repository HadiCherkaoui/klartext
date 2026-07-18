# P2.1 — ISTA's complete clear-faults sequence

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


Implementation spec. Research complete 2026-07-18. Every claim carries a citation; inferences are
labelled INFERENCE; unreadable components are named in §G.3.

**Citation shorthand**
- `VI:NNNN` = `<scratchpad>/decompiled/VehicleIdent.cs:NNNN`
  (from `RheingoldDiagnostics.dll`, type `BMW.Rheingold.Diagnostics.VehicleIdent`)
- `CSV:NNN` = `.../scratchpad/decompiled/ClampSwitchVehicle.cs:NNN`
- `KLEM:NNN` = `.../scratchpad/klemme/BMW.Rheingold.Module.ISTA/ABL_LIF_KLEMMENSTEUERUNG.cs:NNN`
- `LOGIC:NNN` = `.../scratchpad/ista-decompile/out/RheingoldSessionController/BMW.Rheingold.RheingoldSessionController/Logic.cs:NNN`
- `DBD:NNN` = `.../scratchpad/dbd/BMW.Rheingold.DiagnosticsBusinessData/DiagnosticsBusinessData.cs:NNN`
- `SL` = `.../scratchpad/parity-research/dbprov/CoreFramework/BMW.Rheingold.CoreFramework.Contracts/ServiceLocator.cs`

---

## 0. Headline findings (read this first)

1. **The cluster reset the owner saw is a software terminal-15 OFF → 15 s → ON cycle.** It is not part
   of the erase at all — it is a separate, unconditional, *silent* post-clear step
   (`ClampSwitchVehicle.DoClampSwitch`, `VI:9649`) that runs the test module `ABL-LIF-KLEMMENSTEUERUNG`
   in automatic mode and sends `STEUERN_KLEMMEN` to the CAS/FEM/BDC. See §E. This is the single most
   valuable finding in the assignment.
2. **`IS_LOESCHEN_FUNKTIONAL` is dead code — production ISTA never sends it.** The guard at `VI:9747` is
   provably always false (§C.3). The audit's premise that ISTA broadly clears info memory is **wrong**:
   it broadly *reads* info memory (`IS_LESEN_FUNKTIONAL`, `VI:3461`) but only ever clears the six
   hardcoded supplier-specific stores. Do not implement a general info-memory clear.
3. **The functional clear is `14 FF FF FF` to target `0xDF`** — the same UDS payload klartext already
   sends physically, only the address differs (§B).
4. **The supplier list is hardcoded in C#**, and the gating conditions are *not* the ECUs being named —
   D_KBM is gated on FRM_70/FRM_87 + SALAPA `524`, D_LM on LM_AHL/LM_AHL_2 (§C).
5. **Entry point correction:** the audit named `ClearErrorInfoMemoryVehicle` (`VI:9720`). That is only
   the clear *phase*. The real entry point is `ClearAndReadErrorInfoMemory` (`VI:9620`), which wraps
   clear → clamp switch → re-ident → verification read.
6. Confirmed: **no UDS `0x11` ECUReset anywhere in this flow.** The complete set of `STEUERN_` call
   sites in `VehicleIdent.cs` is five: `STEUERN_KLEMMEN` (`VI:4956`, `VI:5088`),
   `STEUERN_ZUSTAND_FAHRZEUG` (`VI:5189`, `VI:5192`), `STEUERN_ZFS_LOESCHEN` (`VI:6160`, `VI:9783`).

---

## A. The exact ordered sequence

Entry point: `ClearAndReadErrorInfoMemory(IJobServices services, Func<IDictionary<string,object>> executionOfTestModule)`
— `VI:9620`. Non-motorcycle branch (`VI:9646-9666`); `VecInfo.Classification.IsMotorcycle()` is false for
F20/F25, so the motorcycle path at `VI:9627-9645` is out of scope.

`RetryCount` = `ConfigSettings.getConfigint("BMW.Rheingold.Diagnostics.VehicleIdent.RetryCount", 1)`
— `VI:158`. **Default 1.** `DefaultRetryCount = 1` at `VI:76`. This is the `retries` argument threaded
into every `apiJob` call below.

`apiJob` signature: `apiJob(variant, job, param, resultFilter, retries, …)`
— `RheingoldCoreFramework` decompile line 125005. So `apiJob("FEM_20", "IS_LOESCHEN_TMS", "0x01", …)`
passes **`"0x01"` as the job parameter**, not a result filter.

### Phase 0 — FASTA preprocessing (no wire traffic)
`DoFastaPreprocessingErrorInfoMemory(out fastaAblaufschritt, out fastaTestModule)` — `VI:9623`.
Telemetry bookkeeping only. **klartext: skip.**

### Phase 1 — CLEAR (`ClearErrorInfoMemoryVehicle`, `VI:9720-9788`)

| # | Condition | Target | Job | Arg | Cite |
|---|---|---|---|---|---|
| 1.1 | `BNType == BN2020` **&&** (`bdc_g11` \|\| `bdc_g05` \|\| `bcp_sp21` present) **&&** `CanExecute` | — | `TransactionPWFManagement.Execute()` | — | `VI:9723-9727` |
| 1.2 | always | group SGBD + physical stragglers | `DoECUClearFS(MainSeriesSgbd, forcePhysicalOnUnindentified: true, tryFunctional, 0u, RetryCount)` | — | `VI:9730` |
| 1.3 | `getECUbyECU_SGBD("FEM_20") != null` | `FEM_20` | `IS_LOESCHEN_TMS` | `"0x01"` then `"0x02"` (two calls) | `VI:9733-9739` |
| 1.4 | `getECUbyECU_SGBD("FRM3") != null` | `FRM3` | `IS_LOESCHEN_TMS_L_LEAR`, `IS_LOESCHEN_TMS_R_LEAR` | `string.Empty` | `VI:9740-9746` |
| 1.5 | **never true** (§C.3) | — | ~~`DoECUClearIS` / `IS_LOESCHEN_FUNKTIONAL`~~ | — | `VI:9747-9750` |
| 1.6 | (`FRM_70` \|\| `FRM_87` present) **&&** `HasSA("524")` | `D_KBM` | `IS_LOESCHEN_SMC_L_LEAR`, `IS_LOESCHEN_SMC_R_LEAR` | `string.Empty` | `VI:9753-9759` |
| 1.7 | `getECUbyECU_GRUPPE("D_0066") != null` | `D_0066` | `IS_LOESCHEN_SMC_L`, `IS_LOESCHEN_SMC_R` | `string.Empty` | `VI:9760-9766` |
| 1.8 | `getECUbyECU_SGBD("ALC_60") != null` | `ALC_60` | `IS_LOESCHEN_SMC_L_LEAR`, `IS_LOESCHEN_SMC_R_LEAR` | `string.Empty` | `VI:9767-9773` |
| 1.9 | `getECUbyECU_SGBD("LM_AHL") != null` \|\| `getECUbyECU_SGBD("LM_AHL_2") != null` | `D_LM` | `IS_LOESCHEN_SMC_L_LEAR`, `IS_LOESCHEN_SMC_R_LEAR` | `string.Empty` | `VI:9774-9780` |
| 1.10 | `getECUbyECU_GRUPPE("G_ZGW") != null` | `G_ZGW` | `STEUERN_ZFS_LOESCHEN` | `string.Empty` | `VI:9781-9785` |
| 1.11 | always | — | `ClearPruefplan()` → `VecInfo?.Testplan.Clear()` — **local UI state, no wire traffic** | — | `VI:9786`, `VI:6248-6258` |

Progress ticks interleaved: 0.1 at `VI:9722`, 0.2 at `VI:9732`, 0.4 at `VI:9752`.

**Step 1.2 expanded — `DoECUClearFS`, `VI:606-669`:**

- `tryFunctional = VecInfo.BNType != BNType.IBUS` — `VI:9729`. True for F20/F25.
- **1.2a functional broadcast** (`VI:612-646`): `apiJob(groupSgbd, "FS_LOESCHEN_FUNKTIONAL", "", "", retries, …)`.
  `groupSgbd = VecInfo.MainSeriesSgbd` = **`F01`** for both F20 and F25 (§B.1).
  If `ecuJob.IsDone()`, loop `for (ushort num = 1; num <= ecuJob.JobResultSets; num++)`:
  read `getintResult(num, "ID_SG_ADR")`; if `IsOkay(num)` → find the ECU, set `FS_SUCCESSFULLY = false`,
  `F_ANZ = 0`, `FEHLER = new ObservableCollection<DTC>()` (`VI:631-633`); else log the failure and
  **continue** (`VI:641`).
- **1.2b physical stragglers** (`VI:647-657`): for each `ECU` where
  `FEHLER != null && ECU_ASSEMBLY_CONFIRMED && (FEHLER.Count > 0 || (F_ANZ > 0 && busid == BUSID))`
  → `doECUClearFS(ecu, ecuKom, retries)` (`VI:671-721`): split `ECU_GRUPPE` on `'|'`, and for each
  candidate group send `FS_LOESCHEN`; **break on the first `IsDone()`** (`VI:706`) regardless of
  whether it was `IsOkay()`. DS2-protocol ECUs get a preceding `IDENT` job (`VI:683-686`) — not
  applicable to F-series UDS ECUs.
- Note the loop reads the *stale* `FEHLER` list: 1.2a zeroes `FEHLER` for every ECU that answered the
  broadcast, so 1.2b naturally targets exactly the ECUs that did **not** answer. That is the
  straggler mechanism.

**Error handling (this is the whole story):**
- `DoECUClearFS` wraps everything in `try { … } catch (Exception) { Log.WarningException(…) }`
  (`VI:660-663`) with `finally { VecInfo.CalculateFaultProperties(ffmResolver); }` (`VI:664-667`).
  **Nothing aborts.** A thrown exception is logged and the method returns whatever it accumulated.
- `doECUClearFS` (per-ECU) has its own `try/catch` → `Log.WarningException` (`VI:716-719`).
- The nine supplier `apiJob` calls at 1.3–1.10 are **completely unguarded** — no `IsOkay()` check, no
  branch on the result. The `IEcuJob` is appended to `list` and never inspected. A failing supplier job
  is silently tolerated.
- Consequence: **every step is best-effort; no failure aborts the sequence; no failure triggers a retry
  beyond the `retries`/`RetryCount` count inside EDIABAS.**

### Phase 2 — CLAMP SWITCH (the cluster reset) — `VI:9649`
```csharp
ClampSwitchVehicle clampSwitchVehicle = ClampSwitchVehicle.DoClampSwitch(
    ecuKom, vecInfo, interactionService, this, executionOfTestModule);
list.AddRange(clampSwitchVehicle.JobList);
```
Full detail in §E.

### Phase 3 — MPAD special variants — `VI:9651-9655`
`if (VecInfo.IsVehicleTestDone) { HandleEcuVarianteMPAD_PP(); HandleEcuVarianteMPADH_PP(); }` — each is a
20 000 ms `ThreadSleep` when the variant is present (`VI:9697`). Not applicable to F20/F25 unless a
`MPAD_PP`/`MPADH_PP` ECU exists. **klartext: skip.**

### Phase 4 — cancellation check — `VI:9656-9660`
Progress 0.45, then `if (clampSwitchVehicle.Canceled) return;` — **the only early-exit in the flow.**
If the clamp switch was cancelled, the re-ident and verification read are skipped entirely.

### Phase 5 — settle — `VI:9661`
`SleepUtility.ThreadSleep(500, "VehicleIdent.ClearAndReadErrorInfoMemory")` — **500 ms.**

### Phase 6 — full re-identification — `VI:9662`
`DoECUIdent(VecInfo.MainSeriesSgbd, forcePhysicalOnUnindentified: true, tryFunctional: true, RetryCount, 0u, null)`
See §D.1.

### Phase 7 — settle — `VI:9663`
`SleepUtility.ThreadSleep(200, …)` — **200 ms.**

### Phase 8 — verification read (`ReadErrorInfoMemoryVehicle`, `VI:9790-9812`)

| # | Step | Job / mechanism | Cite |
|---|---|---|---|
| 8.1 | `DoECUReadFS(MainSeriesSgbd, forcePhysical: true, tryFunctional: true, 0u, RetryCount)` | `FS_LESEN_FUNKTIONAL` on group SGBD, then per-ECU expert detail, then physical fallback | `VI:9794`, `VI:2419`, `VI:2502-2509` |
| 8.2 | `DoECUReadFSDetails()` | per-fault detail reads | `VI:9797`, `VI:3114` |
| 8.3 | `DoECUReadIS(MainSeriesSgbd, forcePhysical: true, tryFunctional: true, 0u, RetryCount)` | `IS_LESEN_FUNKTIONAL` on group SGBD | `VI:9800`, `VI:3461` |
| 8.4 | `DoECUReadISDetails(0u)` | per-entry info detail | `VI:9803`, `VI:3732` |
| 8.5 | if `getECUbyECU_GRUPPE("G_ZGW") != null` → `DoECUReadZFS(RetryCount)` | `G_ZGW` / `STATUS_ZFS_LESEN_GESAMT` | `VI:9805-9808`, `VI:4468` |
| 8.6 | `ReadCheckControlMessages()` | `STATUS_LESEN` `ARG;BMW_CC_DATENSAETZE` on `G_KOMBI` (fallback `G_MMI`) — **a pure read** | `VI:9809`, `VI:4775`, `DBD:1524-1539` |
| 8.7 | `new TransactionCalculateCombinedFaults(VecInfo, ffmResolver).Execute()` | offline fault correlation, no wire traffic | `VI:9810` |

Progress ticks: 0.5, 0.6, 0.8, 0.9 at `VI:9793/9796/9799/9802`.

`DoECUReadZFS` returns `null` early if `IsVehicleInNewGeneration(VecInfo)` (`VI:4456-4459`) or if
`BMW.Rheingold.Programming.SkipVehicleTestSteps.Enabled` (`VI:4451`). Both `DoECUReadFS` (`VI:2422`) and
`DoECUReadIS` (`VI:3445`) short-circuit on that same config flag.

### Phase 9 — post-checks — `VI:9665`
`AddServiceCodeFor_MPAD_And_MPADH_AfterNoReponse()` — FASTA service code only, no wire traffic.

### Phase 10 — FASTA postprocessing + progress 1.0 — `VI:9668-9669`

---

## B. Functional / broadcast addressing on the wire

### B.1 Which group SGBD

`DoECUClearFS` is called with `VecInfo.MainSeriesSgbd` (`VI:9730`). For both the F20 and the F25 this
resolves to **`F01`**:
`BMW.ISPI.TRIC.ISTA.DiagnosticsBusinessDataCore.dll` → `GetMainSeriesSgbdPkw` (decompile 198–222) returns
`"F01"` for every PKW product line except six legacy keys in `ProduktlinieToSgbd`
(`PL2→E89X, PL3→R56, PL3-ALT→ZCS_ALL, PL4→E70, PL5-ALT→RR1, PL6-ALT→E60`, decompile 62–70) and `PL0`.
`F020` and `F025` are both listed in `BN2020Pkw` (decompile 36–39) → they take the `default:` branch.
*(INFERENCE on the final hop only: the cars' literal `Produktlinie` was not read from the encrypted DB,
but every non-legacy PKW line falls through to `F01`.)*

This matters — **the functional address is platform-specific**:

| group SGBD | `FUNKTIONALEADRESSE` table |
|---|---|
| `f01.prg`, `rr1_2020.prg` | **one row: `0xDF` / `ALL` / "alle Steuergeräte"** |
| `e70.prg`, `f01bn2k.prg` | `0xE6` VD-FLEXRAY … `0xEF` ALL (11 rows, per-bus) |
| `e90.prg` | `0xE9` K-CAN … `0xEF` ALL (8 rows) |

`FS_LOESCHEN_FUNKTIONAL` exists in 50 of 1 405 `.prg` files — all group/series SGBDs, never an ECU SGBD.
(Job names are XOR-`0xF7` obfuscated in the `.prg` body — `crates/sgbd/src/prg.rs:22` — so a naive `rg`
returns zero hits.)

### B.2 The emitted bytes — proven by execution

The job's bytecode was run through klartext's own VM (`klartext_best::Ecu::run_job`) with a recording
`UdsExchange`:

```
f01.prg      / FS_LOESCHEN_FUNKTIONAL   target=0xDF   frame = C4 DF F1 14 FF FF FF
d72n47a0.prg / FS_LOESCHEN              target=0x12   frame = 84 12 F1 14 FF FF FF
```

**The UDS payload is byte-identical: `14 FF FF FF`** (ClearDiagnosticInformation, groupOfDTC = all
— exactly what klartext already sends). Only two things differ: the target byte, and bit `0x40` of the
BMW-FAST format byte.

Byte provenance (offsets within `f01.prg / FS_LOESCHEN_FUNKTIONAL`):
- `0x000000 move L0, #0xDF` → `S0[0]` — the hardcoded default functional address. ISTA passes **no** job
  argument (`apiJob(groupSgbd, "FS_LOESCHEN_FUNKTIONAL", string.Empty, …)`, `VI:614`), so the optional
  `FunktionaleAdresse`/`F_ADR`→`NR` table lookup at `0x8C`–`0x17A` is skipped and `0xDF` stands.
- `0x00001D move S1, [14 FF FF FF]` — the payload literal.
- `0x0003A2 spaste S1[0], [80 00 00]` then `0x0003AC adds S1[0], B6` (B6 = payload length 4) → `0x84`.
- `0x0003EE–0x0003FC` → `S1[1] = target`, `S1[2] = 0xF1` (tester source).
- `0x000402 move B0, #0x1` → `0x000410 adds S1[0], #0x40` — **the functional flag**. `0x84 + 0x40 = 0xC4`.
- `0x000416 xsend S3, S1`.

The physical twin is the same inlined send/receive routine compiled with the flag = 0: at
`0x000267 move B0, #0x0` the `jz` skips `adds S1[0], #0x40` (`0x000275`). `0xC0|len` vs `0x80|len` is the
ISO 14230-2 format-byte address-mode field (`10` = physical, `11` = functional) — read from the bytecode
contrast, not assumed.

Incidental: physical `FS_LOESCHEN` in the DDE accepts an optional 32-bit arg
(`0x000013 parl L0, #0x1`) overwriting `S2[1..3]`, i.e. it can clear a single DTC. ISTA passes
`string.Empty`, so `14 FF FF FF` — matching klartext's on-car-confirmed frame.

### B.3 How many responses are collected

One `xsend` **per response**, in an outer loop only the functional branch enters:
- `0x00117C eoj` — the physical path ends here (`S0[3] == 0`).
- Functional continues: `0x00117E settmr #0x180000` (trap mask) → `0x0011C6 enewset` (**start a new
  result set**) → `0x0011EA` increments responder counter `S0[4]` → `0x0012B1 comp S0[4], S0[7]` + `jc`
  (`S0[7] = 0x64` = **100 max responders**, set at `0x000047`) → `0x0012D6 jump -0x104B` back to `0x291`,
  the functional/physical dispatch, which rebuilds and `xsend`s again.
- **Exit is by trap, not by counter**: after a send/receive reporting no response, `0x000CC1 jt` →
  `0x000CE7 jump` → `0x0012DC`, which sets `JOB_STATUS = OKAY` and `eoj`. i.e. the loop terminates on
  the first silent slot — a quiet-period timeout.
- Per iteration, a length check (`0x00047A comp I11, I10`) requires the buffer hold exactly one frame,
  and NRC `0x21` triggers a `wait #0x1` retry loop (`0x00053D`–`0x000559`, backward branch to `0x36D`)
  bounded by the `retries` register (= `RetryCount`, default 1).

**Each result set carries** — exactly what ISTA reads back:
- `_RESPONSE` (`0x0004DF ergy`), tagged `0x0004CD etag`
- `ECU_ADR` — responder address, 2 uppercase hex chars (`0x000CED etag` … `0x000E8D ergs`)
- `ID_SG_ADR` — responder address as integer (`0x000E9D etag`, `0x000EDF ergl`) ← read at `VI:620`
- `ECU_GROBNAME` — resolved offline from the group SGBD's own `GrobName` table
  (`tabset "GrobName"` `0x000F3A` → `tabseek "ADR"` `0x000F86` → `tabget "GROBNAME"` `0x000FE4`)
- `JOB_STATUS` — `OKAY` or `ERROR_ECU_*`

### B.4 How responders are told apart

By each response's own **source-address byte**. `0x00048A move L3, S3[2]` (source) and
`0x000497 move L4, S3[1]` (target); the epilogue writes the source back to `S0[1]`
(`0x000C5A`/`0x000C5E`), which becomes `ID_SG_ADR`.

The validation differs in exactly the way you would expect:

| check | functional (`f01`) | physical (`d72n47a0`) |
|---|---|---|
| response SID == request SID + 0x40 (`0x54`) | enforced `0x0005A9` | enforced `0x0003F3` |
| response addressed to tester `0xF1` | enforced `0x0005BB` | enforced `0x000405` |
| response **source == request target** | **SKIPPED** — `0x0005CE move B0,#0x1` makes `jnz` bypass `comp L3, L2` | **enforced** — `0x000418 move B0,#0x0` falls through to `0x000426 comp L2, #0x12` |

The skip is the point: with a broadcast, no responder's source equals `0xDF`.

### B.5 HSFZ carries no functional mechanism of its own

`docs/protocol-reference.md` §2.1 documents the HSFZ frame as
`LENGTH | CONTROL | SOURCE | TARGET | PAYLOAD`, where **payload is bare UDS**. There is no format byte on
the wire, so the BMW-FAST `0x40` bit has nowhere to live. Confirmed empirically by klartext's own
capture: `docs/car-session-1-results.md:57` shows `TX F4→12: 22 45 17` — bare UDS, no `83 12 F1 …`
framing. Neither the protocol reference nor §2.4 describes functional/broadcast addressing at all.

**So on ENET, functional addressing can only be `TARGET = 0xDF` in the HSFZ header.**
This is INFERENCE from the frame layout, not something read out of a capture or a binary.

`data/Testmodule(1)/Ediabas/BIN/XEnet64.dll` (`PE32+ x86-64`, not decompilable with `ilspycmd`) proves
the native layer *does* implement functional sending on the HSFZ path — its strings include:
```
CHsfzGateway::SendDiagTel: Funktional sending with suppressed responses -> handle as physical sending
CHsfzGateway::SendDiagTel: Functional sending not allowed in proxy mode - positive responses not suppressed (Error: TELEGRAM_FORMAT_ERROR)
CHsfzGateway::SendDiagTel: Functional sending not possible (Error: %d)
CHsfzProtocol::Receive: t=%d (ECU=%02X) (%s)   /   SM[%02X].Error=%d
```
plus an RTTI type `CHsfzRoutingTable` (mirroring `CDoipRoutingTable`'s `SetPhysEntry`/`SetFunctEntry`)
and a per-ECU state-machine array — which is how one broadcast fans out into N tracked responses.

One derivable detail that *is* solid: the driver inspects the payload for the
suppressPosRspMsgIndication bit ("Funktional sending with suppressed responses -> handle as physical
sending"). `14 FF FF FF` has no sub-function byte, hence no suppress bit → it stays functional and every
ECU answers.

---

## C. The supplier-job list

### C.1 Verdict: HARDCODED in C#

All six ECU names and all eleven job names are C# string literals at `VI:9733-9785`, inside
`if` statements testing `VecInfo` membership. There is no catalog lookup, no table, no config file.
The identical block appears in the `[Obsolete]` twin at `VI:6110-6162` — same literals, same order —
which is corroboration that this is maintained by hand, not generated.

**Per the owner's standing rule ("do what ISTA does"), replicating the hardcoding IS parity.** But note
what is actually hardcoded: not "these ECUs get cleared" but "these *conditions* trigger these
*supplier-specific* jobs". The conditions are evaluated against live vehicle data (`VecInfo.ECU`,
`VecInfo.HasSA`), so the behaviour is still car-dependent — on a car with none of these ECUs, none of
these jobs run, and the sequence degrades cleanly to steps 1.2 + 1.10.

### C.2 The complete list, verbatim

The gating condition is frequently **not** the ECU being addressed — read this table carefully.

| Gate (verbatim) | Lookup kind | Addressed ECU | Jobs (in order) | Arg |
|---|---|---|---|---|
| `getECUbyECU_SGBD("FEM_20") != null` | SGBD | `FEM_20` | `IS_LOESCHEN_TMS`, `IS_LOESCHEN_TMS` | `"0x01"`, `"0x02"` |
| `getECUbyECU_SGBD("FRM3") != null` | SGBD | `FRM3` | `IS_LOESCHEN_TMS_L_LEAR`, `IS_LOESCHEN_TMS_R_LEAR` | `""` |
| `(getECUbyECU_SGBD("FRM_70") != null \|\| getECUbyECU_SGBD("FRM_87") != null) && HasSA("524")` | SGBD + SALAPA | **`D_KBM`** | `IS_LOESCHEN_SMC_L_LEAR`, `IS_LOESCHEN_SMC_R_LEAR` | `""` |
| `getECUbyECU_GRUPPE("D_0066") != null` | **GRUPPE** | `D_0066` | `IS_LOESCHEN_SMC_L`, `IS_LOESCHEN_SMC_R` | `""` |
| `getECUbyECU_SGBD("ALC_60") != null` | SGBD | `ALC_60` | `IS_LOESCHEN_SMC_L_LEAR`, `IS_LOESCHEN_SMC_R_LEAR` | `""` |
| `getECUbyECU_SGBD("LM_AHL") != null \|\| getECUbyECU_SGBD("LM_AHL_2") != null` | SGBD | **`D_LM`** | `IS_LOESCHEN_SMC_L_LEAR`, `IS_LOESCHEN_SMC_R_LEAR` | `""` |
| `getECUbyECU_GRUPPE("G_ZGW") != null` | **GRUPPE** | `G_ZGW` | `STEUERN_ZFS_LOESCHEN` | `""` |

Three traps for the implementer:
1. **`D_KBM` is gated on `FRM_70`/`FRM_87` presence *and* SALAPA `524`** — never on `D_KBM` itself.
   `HasSA("524")` reads the vehicle order (FA/SALAPA), which klartext already captures
   (`62 3F06` 214-byte vector, per CLAUDE.md).
2. **`D_LM` is gated on `LM_AHL`/`LM_AHL_2`** — never on `D_LM` itself.
3. **`D_0066` and `G_ZGW` use `getECUbyECU_GRUPPE`, the others use `getECUbyECU_SGBD`.** These are
   distinct fields on `ECU` (`ECU_GRUPPE` vs `ECU_SGBD`); `ECU_GRUPPE` may be a `|`-separated list
   (`VI:680`, `VI:732`). One-line note in `VehicleIdent`'s `[Obsolete]` twin diverges here: `VI:6158`
   tests `getECUbyECU_SGBD("G_ZGW")` where the live path `VI:9781` tests `getECUbyECU_GRUPPE("G_ZGW")`.
   **Follow the live path (`GRUPPE`).**

### C.3 How ISTA decides an ECU "supports IS_LOESCHEN" — it does not probe, and the general path is dead

`VI:9747-9751`:
```csharp
if (!ServiceLocator.Current.TryGetService<IDiagnosticsBusinessData>(out var _))
{
    items = DoECUClearIS(VecInfo.MainSeriesSgbd, forcePhysicalOnUnindentified: true, tryFunctional: true, 0u, RetryCount);
}
list.AddRange(items);
```

**This guard is provably always false.** `ServiceLocator` (`SL`) is a plain dictionary:
```csharp
public bool TryGetService<T>(out T service) where T : class {
    Type typeFromHandle = typeof(T);
    if (!services.ContainsKey(typeFromHandle)) { service = null; return false; }
    service = (T)services[typeFromHandle]; return true;
}
```
and `GetService<T>` **inserts a null entry on a miss**:
```csharp
if (!services.ContainsKey(typeFromHandle)) {
    Log.Error("ServiceLocator.GetService<T>()", "No service registered for type \"{0}\". Using default ({1}) instead.", …);
    services.Add(typeFromHandle, null);
}
return (T)services[typeFromHandle];
```
`VehicleIdent`'s **constructor** runs `diagnosticsBusinessData = ServiceLocator.Current.GetService<IDiagnosticsBusinessData>();`
(`VI:581`) — unconditionally, before any of this. So by the time `VI:9747` executes, the key is present
in `services` either way:
- service registered (the normal case) → `ContainsKey` true → `TryGetService` returns **true**;
- service *not* registered → the ctor already inserted `null` under that key → `ContainsKey` still true
  → `TryGetService` returns **true**.

Either way `!TryGetService(…)` is **false**, so `DoECUClearIS` never runs. Independent corroboration
from the same flow: `ReadCheckControlMessages` (`VI:9809`, reached in Phase 8 of the same method)
dereferences `diagnosticsBusinessData` unconditionally at `VI:4775`, so any run that completes proves
the service was non-null.

**Conclusions:**
1. **`IS_LOESCHEN_FUNKTIONAL` (`VI:769`) and per-ECU `IS_LOESCHEN` (`VI:737`) are never sent by
   production ISTA.** ISTA does *not* perform a general info-memory clear. Do not implement one.
2. The `list.AddRange(items)` at `VI:9751` re-adds the **FS**-clear job list a second time (`items`
   still holds `DoECUClearFS`'s return from `VI:9730`) — a genuine ISTA bug producing duplicate entries
   in the FASTA job log. Cosmetic; no wire effect. Do not replicate.
3. So the answer to "how does ISTA decide an ECU supports `IS_LOESCHEN`" is: **it never asks.** It runs
   six hardcoded supplier jobs gated on vehicle composition, and ignores the result of every one of
   them. There is no probe, no catalog job-existence check, and no try-and-ignore fallback for a
   *general* info-memory clear — because no general clear is attempted.
4. Asymmetry worth internalising: ISTA broadly **reads** info memory (`IS_LESEN_FUNKTIONAL`, `VI:3461`,
   Phase 8.3) but only narrowly **clears** it. klartext's existing `read_info_memory` (`22 2000`) is the
   read side and is unaffected by this finding.

---

## D. Re-identification and verification

### D.1 The re-identification (Phase 6)

`DoECUIdent(VecInfo.MainSeriesSgbd, forcePhysicalOnUnindentified: true, tryFunctional: true, RetryCount, 0u, null)`
— `VI:9662`, method at `VI:812`.

**It is the same method used at session start**, but *not* with identical arguments. The session-start
identification (`VI:4983-4992`) calls it up to three times with different group SGBDs and flags:
```csharp
DoECUIdent("f01", forcePhysicalOnUnindentified: true,  tryFunctional: false, RetryCount, 0u, monitor);   // VI:4983
DoECUIdent(VecInfo.MainSeriesSgbdAdditional, forcePhysicalOnUnindentified: false, tryFunctional: true, …); // VI:4989
DoECUIdent(VecInfo.MainSeriesSgbd, forcePhysicalOnUnindentified: true,  tryFunctional: true, RetryCount, 0u, monitor); // VI:4992
```
The post-clear call matches only the **last** of those three (`VI:4992` vs `VI:9662` — identical
arguments except `monitor`/`null`). Session start additionally interleaves `DoECUReadFA` (`VI:4970`) and
`DoECUReadSupplierInfo` (`VI:4984`, `VI:4990`), which the post-clear path does **not** repeat.

So: **post-clear re-identification is the main identification pass only — not the full session-start
ident.** Its purpose is to re-establish the ECU list after the clamp cycle dropped and restored
terminal 15 (Phase 2), which is exactly why it sits between the 500 ms and 200 ms settle sleeps.

### D.2 The verification read (Phase 8)

**It is not "one whole-vehicle fault read" — it is seven steps** (table in §A Phase 8): functional FS
read + per-fault details + functional IS read + per-entry IS details + gateway ZFS + check-control
messages + offline combined-fault correlation.

**What is done with the result:** displayed and logged. Specifically:
- Results populate `VecInfo` model state (`ECU.FEHLER`, `ECU.INFO`, `VecInfo.ZFS`,
  `VecInfo.CheckControlMessages`) which the UI binds to.
- `TransactionCalculateCombinedFaults` (`VI:9810`) correlates faults across ECUs offline.
- The accumulated `IList<IEcuJob>` goes to `DoFastaPostprocessingErrorInfoMemory` (`VI:9668`) for
  telemetry.
- **There is no retry of the clear, and no comparison of before-vs-after.** Residual faults simply
  appear in the refreshed fault list. The method returns `void`; nothing branches on whether the clear
  succeeded.

This is a meaningful divergence from klartext, which today *does* compute `verified_clean` per ECU by
diffing pre- and post-read (`crates/client/src/scan.rs:88`). klartext's behaviour is strictly more
informative; §F.5 recommends keeping it while adding ISTA's wider read.

---

## E. The cluster reset — mechanism found

**Hypothesis (a) CONFIRMED. (b), (c), (d) disproven.** ISTA commands a software terminal-15
OFF → 15 s → ON cycle after every fault clear.

### E.1 The chain

1. **The clear entry point always calls it** — `VI:9648-9649`, immediately after the clear phase, for
   any non-motorcycle. No condition at the call site.
2. **`DoClampSwitch` delegates to a test module and only prompts if that FAILS** — `CSV:79-90`: when
   `BNType == BN2020` && config enabled (`CSV:67`, `defaultValue: true`) && VCI != PTT, it calls
   `CallTestModuleForClampSwitch(executionOfTestModule)`. The `InteractionVehicleIgnitionModel` dialog
   is registered **only inside `if (!CallTestModuleForClampSwitch(...))`** — i.e. only on failure.
   Success = zero prompts. `Canceled` is set only in `ManualClampSwitch`, which the success path never
   reaches.
3. **The module is `ABL-LIF-KLEMMENSTEUERUNG` in automatic mode** — `LOGIC:1753-1765`:
   ```csharp
   configuration.ModuleName = TestModuleName.ClampSwitch;
   parameters.setParameter(ModuleParameter.ParameterName.IN_konfig, "KLwechsel");
   parameters.setParameter(ModuleParameter.ParameterName.IN_pause, 15000);
   parameters.setParameter(ModuleParameter.ParameterName.IN_automode, true);
   parameters.setParameter(ModuleParameter.ParameterName.IN_automaticRun, true);
   ```
   Name resolution `TestModuleName.ClampSwitch => "ABL-LIF-KLEMMENSTEUERUNG"` at `DBD:1367`; on disk at
   `data/Testmodule(1)/Testmodule/ABL_LIF_KLEMMENSTEUERUNG.dll`.
4. **`KLwechsel` = KL15 off, 15 s, KL15 on** — `KLEM`:
   - `Start:168-194` → `Clusterung_Bordnetz:216-224` (BN2020 → `IL_056a`) → `:283`
     `num = IN_automode ? 2 : 1` = 2 → `Konfiguration_Ansteuerung:351-369` → `Klemmenwechsel()`.
   - `Klemmenwechsel:1410-1413`: pass 1, `konfig_kl = "KL30B_EIN"` → `Klemme_15_aus()`.
   - `Rückgabe:1460-1462`: `count_kl = 2`, loops back.
   - `Klemmenwechsel:1425-1432`: `IN_automaticRun` true → **`Sleep(IN_pause)` = 15 000 ms silently**
     (the `!IN_automaticRun` branch would have shown a wait dialog) → `konfig_kl = "KL15_EIN"` →
     `Klemme_15_ein()`.
5. **The wire command** — `Klemme_15_aus:1215-1260` (guard `if (status_klemme_l != 6)`) and
   `Klemme_15_ein:692-803` (guard `if (status_klemme_l != 10)`), each retried up to 3×:
   ```csharp
   EcuKomStatement.Execute("CAS4_2", "STEUERN_KLEMMEN", konfig_kl, 0, FastaProtocoler);   // :1245 / :788
   ```
   with `BDC` (`:1230`/`:773`) and `FEM_20` (`:1260`/`:803`) as sibling branches, selected by
   `HasVehicleVariant("G_CAS", …)`.

   Argument decoding, from `cas4_2.prg` table `TAB_CAS_KLEMMENSTATUS_ARG` (read via `klartext-best`):
   ```
   ["6",  "KL30B_EIN"]   <- KL15 OFF
   ["10", "KL15_EIN"]    <- KL15 ON
   ```
   Bytecode template of `STEUERN_KLEMMEN` in `cas4_2.prg`, offset `000003`:
   ```
   move S1, [31 01 10 01 FF FF FF]   ; RoutineControl startRoutine, RID 0x1001, value patched in
   ```
   → **UDS SID `0x31`, sub-function `0x01`, RID `0x1001`.**
   ⚠️ The exact patched payload tail is **not yet pinned** — the template carries three `FF` placeholder
   bytes and the table supplies decimal `6`/`10`. See §G.3.1.
6. **Why the owner saw no prompt** — `manuell` is initialised false (`KLEM:65`) and set true only in
   `Manuelles_Klemmenschalten` (`KLEM:305`). Every `CreateServiceDialog` in `Klemme_15_ein`/
   `Klemme_15_aus` sits behind `if (manuell)` (`:389`, `:967`) or `if (!IN_automaticRun)` (`:395`,
   `:453`, `:569`, `:973`, `:1031`, `:1147`, `:1335`); the final `Rückgabe` dialogs behind
   `if (IN_automode && manuell)` (`:1484`). In the clear flow `manuell == false` and
   `IN_automaticRun == true` → **every dialog is skipped.**
7. **Both the owner's cars take this path** — `DiagnosticsBusinessDataCore.GetBordnetType:82-102`;
   `BN2020Pkw` (lines 21-24) contains `"F020"` and `"F025"` explicitly. Non-PTT (ENET) VCI → automatic
   branch applies.

**This fully explains the owner's observation:** a 15-second terminal-15 drop resets the instrument
cluster exactly as he described, and step 6 explains the absence of any prompt.

### E.2 The disproven hypotheses

- **(b) Check-control clear — NO. ISTA only ever READS CCMs.** An exhaustive UTF-16LE `strings -el`
  sweep over all 147 DLLs for `STEUERN|STATUS_*CC|CCM|CHECK|CONTROL*`, `*CC*LOESCH*`, `CHECK_?CONTROL*`
  returns only reads: `STATUS_BMW_CC_DATENSAETZE_LESEN`, `STATUS_CHECK_CONTROL_HISTORY`,
  `STATUS_CHECKCONTROL_HISTORY`. **No `CC_LOESCHEN`, no `STEUERN_CC`, no `CCM_LOESCHEN` exists anywhere
  in the stack.** (A first sweep found nothing at all because .NET stores strings UTF-16LE in the `#US`
  heap — plain `rg -a` misses them; the `BMW_CC_DATENSAETZE` control probe confirms the corrected method
  works.) `ReadCheckControlMessages` (`VI:4768`) → `SendStatusLesenCcmJobToKombiOrMmi` (`DBD:1524-1539`)
  is a pure read: `STATUS_LESEN` / `ARG;BMW_CC_DATENSAETZE` on `G_KOMBI` (fallback `G_MMI`). The one CCM
  test module, `ABL-LIF-CCM_ANALYZER` (`DBD:1368`), maps CCM ids to DTC ids — analysis, not clearing.
- **(c) KOMBI reset — NO.** `rg` for `apiJob("*KOMBI*"` / `"G_KOMBI"` across the whole 9 915-line
  `VehicleIdent.cs` returns **zero hits**. Nothing is ever sent to the cluster except the CCM read.
- **(d) `STEUERN_ZFS_LOESCHEN` cascade — NO.** From `zgw_01.prg` bytecode, offset `000003`:
  `move S1, [31 01 40 00 FF]` — RoutineControl startRoutine, RID `0x4000`, one value byte `0xFF`. It
  clears the gateway's own central fault-memory (ZFS) copy; the per-ECU erase is done separately by
  `DoECUClearFS`. INFERENCE: gateway-local, no cross-ECU cascade — the routine emits a single telegram
  per protocol branch with no ECU-address iteration.

### E.3 Two incidental findings

- **`TransactionPWFManagement` never runs on the owner's cars.** `VI:9723-9727` gates it on
  `bdc_g11`/`bdc_g05`/`bcp_sp21` — G-series BDCs only; the F25 has CAS4. Even when it does run it is
  *not* a reset: `TransactionPWFManagement.DoExecute` → `ClampShutdownManagement` (`DBD:1421-1472`),
  which for `CAS4_2` sends `G_CAS / STEUERN_ROUTINE / ID;0xAC51;STR;3` — that *suppresses* KL15
  auto-shutdown during the session, the opposite of a cycle.
- **The two other `STEUERN_KLEMMEN` sites in `VehicleIdent` are ON-only, not cycles.** `SwitchClamp15`
  (`VI:4952-4956`) is behind config `BMW.Rheingold.Diagnostics.DoClamp15Swithing`,
  **`defaultValue: false`**; `PerformNewClampCheck` (`VI:5088`) fires only for `VCIDeviceType.ICOM`/`PTT`
  when KL15 < 1000 mV — a wake-up. INFERENCE: with an ENET cable neither applies, so the OFF→ON cycle
  comes solely from the test module.

---

## F. Mapping to klartext

Current state (working tree, branch `feat/ista-parity-p0`; the diff vs HEAD around every clear path is
doc/comment-only, so these citations hold for both).

### F.0 Five facts that shape the work

1. **No UDS functional addressing exists anywhere in the workspace.** `DEFAULT_BROADCAST`
   (`crates/client/src/client.rs:36`) is an IP-level UDP discovery broadcast (`169.254.255.255`), not
   UDS.
2. **The transport is already fine.** `HsfzFrame::diagnostic(source, target, uds)`
   (`crates/hsfz/src/frame.rs:59-73`) takes an arbitrary per-frame target; `HsfzConnection`
   (`crates/hsfz/src/conn.rs:21`) holds no target field. **A caller can send to `0xDF` today with zero
   transport changes.**
3. **The response demux is the real blocker.** `type Pending = Arc<Mutex<HashMap<u8, PendingReq>>>`
   (`crates/client/src/session.rs:106`) keyed by address, one waiter per address; `route_frame`
   (`crates/client/src/session.rs:295`) does `map.get(&src)` and **returns early when there is no
   waiter**, then `map.remove(&src)` on the first final delivery (`session.rs:314`). A functional
   request registers a waiter under `0xDF` while replies arrive with `src` = `0x12`, `0x40`, … → every
   reply is silently dropped and the request times out.
4. **The `best` gate is not on the clear path at all.** `GatedExchange` only wraps the BEST/2 VM
   exchange used by `run_job`; `clear_faults`/`clear_all_faults` call `DiagnosticClient` →
   `Session::request` directly, **ungated**. The only gate on a clear today is the MCP `confirm` bool.
5. **`GatedExchange::confirmed_write` (`crates/best/src/gate.rs:138`) has zero production call sites.**
   The only construction in the repo is `GatedExchange::read_only(...)` at `mcp/src/server.rs:1056`.

### F.1 `crates/client/src/session.rs` — add a many-response collector (the core change)

Today: `pub async fn request(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ClientError>`
(`session.rs:183`), `request_with_timeout` (`session.rs:195`), `enter_session` (`session.rs:262`).

Add a sibling that collects until quiet — mirroring the bytecode's own trap-based exit (§B.3):

```rust
/// One functional (broadcast) request; collect every ECU that answers.
/// Returns (source_address, payload) per responder, in arrival order.
pub async fn request_functional(
    &self,
    target: u8,                 // 0xDF for F01-platform "all ECUs"
    uds: &[u8],
    quiet_period: Duration,     // stop after this long with no new response
    max_responders: usize,      // ISTA's bytecode caps at 100 (S0[7] = 0x64)
) -> Result<Vec<(u8, Vec<u8>)>, ClientError>
```

Implementation notes:
- The `Pending` map is keyed by **source** address, which is already the right shape — it needs a
  *broadcast waiter* variant, not a rewrite. Suggested: add a second field to the session state, e.g.
  `broadcast: Arc<Mutex<Option<BroadcastWaiter>>>`, and in `route_frame` fall through to it when
  `map.get(&src)` misses **and** the frame's SID matches the in-flight broadcast's expected positive
  response (`0x54`). Preserving the existing early-return for genuinely stray frames matters — do not
  turn the miss branch into a catch-all.
- Validate each response the way the bytecode does (§B.4): positive SID == request SID + `0x40`, and
  target == our tester address. **Do not** check `src == 0xDF` — that check is deliberately skipped.
- Keep `MAX_PENDING_TICKS` NRC-`0x78` handling per responder if it appears; the bytecode retries NRC
  `0x21` (`busyRepeatRequest`) up to `retries`.
- `max_responders`: use `100` to match `S0[7] = 0x64`.
- `quiet_period`: **not readable from ISTA** — the trap timeout `settmr #0x180000` is a trap-mask value,
  not milliseconds (§G.3.2). Start from the existing per-request default read timeout and make it a
  named constant with a `[verify against capture]` marker, per the repo convention.

### F.2 `crates/uds/` — mostly already there

- `clear_all_dtcs()` → `[0x14, 0xFF, 0xFF, 0xFF]` (`crates/uds/src/service.rs:203-210`) — **unchanged**;
  the functional clear uses the identical payload.
- `routine_control(subfn, rid, params)` (`crates/uds/src/service.rs:278`) already exists with
  `routine_subfn::START_ROUTINE = 0x01`. ISTA's two routines map directly:
  - `STEUERN_ZFS_LOESCHEN` = `routine_control(0x01, 0x4000, &[0xFF])`
  - `STEUERN_KLEMMEN` = `routine_control(0x01, 0x1001, &[<val>])` — value pending §G.3.1
  It currently has **no caller** in `client` or `mcp` (only `crates/semantic/src/service_function.rs`
  builds `0x31` frames, and nothing executes them).
- Addressing correctly lives in the transport, not the message layer (`service.rs:7-8`) — keep it that
  way; `request_functional`'s `target` parameter is the right seam.
- Add a named constant for the functional address. **It is platform-specific** (§B.1), so it must not be
  a bare `const FUNCTIONAL_ADDR: u8 = 0xDF`. Model it as data resolved from the platform, defaulting to
  `0xDF` for the F01 group with the E-series per-bus tables (`0xE6`…`0xEF`) as a documented future case.

### F.3 `crates/client/src/client.rs` — new per-ECU and broadcast entry points

Existing, for reference:
```rust
pub async fn clear_dtcs(&self, target: u8, dtc: [u8;3]) -> Result<(), ClientError>   // client.rs:517
pub async fn clear_all_dtcs(&self, target: u8) -> Result<(), ClientError>            // client.rs:531
pub async fn read_all_dtcs(&self, target: u8) -> Result<Vec<Dtc>, ClientError>       // client.rs:231
pub async fn read_info_memory(&self, target: u8) -> Result<Option<InfoMemory>, …>    // client.rs:247
pub async fn run_service_reset(&self, …)                                             // client.rs:633
```
`clear_dtcs` sends `10 03` then `14 FF FF FF` and **discards the response** (`.await?;` → `Ok(())`);
the only check is `route_frame`'s positive-SID match. Confirmed: `klartext_uds::ecu_reset`
(`crates/uds/src/service.rs:219`) has **zero callers** — the `0x11` removal from `9dc80db` is complete.

Add:
```rust
/// ISTA's `FS_LOESCHEN_FUNKTIONAL`: one broadcast clear, many responders.
pub async fn clear_all_dtcs_functional(&self) -> Result<Vec<u8>, ClientError>  // -> addresses that ACKed
/// ISTA's `STEUERN_ZFS_LOESCHEN` on the gateway.
pub async fn clear_gateway_combined_store(&self) -> Result<(), ClientError>    // 31 01 4000 FF to G_ZGW
```
⚠️ `clear_dtcs` currently prefixes `10 03` (extended session). **The bytecode's functional job emits no
session control** — the `f01.prg` disassembly shows a single `xsend` of `C4 DF F1 14 FF FF FF` with no
preceding `10 03`. Do not send a functional `10 03`. (klartext's physical `10 03` is separately
on-car-confirmed, `docs/car-session-1-results.md`; keep it on the physical straggler path.)

### F.4 `crates/client/src/scan.rs` — restructure the batch clear

Today (`scan.rs:128`):
```rust
pub async fn clear_faults_all(&self, addrs: &[u8]) -> Vec<ClearReport> {
    let mut reports = Vec::with_capacity(addrs.len());
    for &address in addrs { reports.push(self.clear_faults_verified(address).await); }
    reports
}
```
Sequential, per-ECU, physical, never aborts (each ECU yields a `ClearReport`, not a `Result`).
`clear_faults_verified` (`scan.rs:88`) pre-reads, **returns early without clearing if the pre-read
fails** (`scan.rs:99-101`, "never clear blind"), clears, re-reads, sets `verified_clean`.

Restructure to ISTA's shape — broadcast first, physical only for stragglers:

```rust
pub struct ClearSequenceReport {
    pub functional: FunctionalClearReport,   // who answered the broadcast
    pub stragglers: Vec<ClearReport>,        // per-ECU physical fallback (existing type)
    pub gateway_zfs: Option<Result<(), String>>,
    pub supplier_jobs: Vec<SupplierJobReport>,
    pub clamp_cycle: Option<ClampCycleReport>,   // see F.6 — omitted unless explicitly requested
    pub verification: Vec<EcuFaults>,            // the post-clear whole-vehicle read
}
```
Ordering must follow §A: functional clear → stragglers → supplier jobs → gateway ZFS → [clamp cycle] →
settle → re-ident → settle → verification read.

**Straggler selection.** ISTA's rule (`VI:651`) is
`FEHLER != null && ECU_ASSEMBLY_CONFIRMED && (FEHLER.Count > 0 || (F_ANZ > 0 && busid == BUSID))`,
evaluated against state the broadcast has just zeroed for every responder. klartext's equivalent:
**every fitted ECU that did not appear in the broadcast's responder list**, intersected with ECUs that
had faults in the pre-read. Keep the existing "never clear blind" pre-read — it is a klartext safety
property with no ISTA counterpart, and it is strictly better.

**Preserve `verified_clean`.** ISTA does not diff before/after (§D.2); klartext does. Keep klartext's
behaviour and note the divergence in the doc comment as a deliberate, owner-visible improvement rather
than a defect.

### F.5 `mcp/src/server.rs` and `mcp/src/dto.rs`

Existing idiom — `#[tool_router] impl KlartextServer` opens at `server.rs:314`; `#[tool_handler] impl
ServerHandler` at `server.rs:1515`; every tool is `#[tool(description = "…")]` + `Parameters<Req>` in +
`Json<Result>` out; registration is automatic via `Self::tool_router()` (`server.rs:109`).

- `clear_faults` — `server.rs:759`, attribute `server.rs:749-758`, request `dto.rs:229`, result
  `dto.rs:243`.
- `clear_all_faults` — `server.rs:1463`, attribute `server.rs:1455-1462`, request `dto.rs:517`,
  per-ECU `EcuClearInfo` `dto.rs:529`, result `dto.rs:543`.
- Confirm gating is a plain `bool` checked as the **first statement, before the connection check**,
  returning `McpError::invalid_params` (`server.rs:765-776`, `server.rs:1468-1477`). Keep that ordering
  — `mcp/tests/integration.rs:813` asserts on it.
- `clear_all_faults` resolves addresses via `fitted_addrs(conn, req.rescan)` (`server.rs:1564`).

Changes:
1. `clear_all_faults` becomes the ISTA-parity sequence. Extend `ClearAllFaultsResult` with the
   broadcast responder list, supplier-job outcomes, the gateway-ZFS outcome, and the verification read.
2. Keep `clear_faults` (single-ECU, physical) unchanged — it has no ISTA counterpart but is a
   legitimate narrower tool, and its frame census test (`integration.rs:859`) is a useful invariant.
3. **Do not add a `reset` knob.** `dto.rs:746`
   (`clear_requests_have_no_reset_knob_and_default_to_refusing`) pins this; a stale client sending
   `{"confirm":true,"reset":true}` must not re-enable anything.
4. Fix the orphaned doc comment at `server.rs:1847-1851` — left by `9dc80db`, it documents a `reset`
   knob that no longer exists and has silently merged into `describe_faults`'s doc block.

### F.6 Blast radius and the gate — where the clamp cycle lands

Per-step classification against the CLAUDE.md tier ladder:

| Step | UDS | Tier | Note |
|---|---|---|---|
| Functional clear `14 FF FF FF` → `0xDF` | `0x14` | **Standard clear** | Same frame klartext already sends, broader target. `confirm=true`. |
| Physical straggler `14 FF FF FF` | `0x14` | **Standard clear** | Unchanged from today. |
| Supplier `IS_LOESCHEN_*` jobs | via BEST/2 VM | **Standard clear** | Executed as EDIABAS jobs; the emitted SID decides — verify each is `0x14`/`0x31` before shipping. |
| Gateway `STEUERN_ZFS_LOESCHEN` | `0x31 01 4000 FF` | **Service write** | RoutineControl. `Policy::ConfirmedWrite`. |
| **Clamp cycle `STEUERN_KLEMMEN`** | `0x31 01 1001 <val>` | **Service write + actuation** | See below. |
| Re-ident + verification read | `0x22`/`0x19`/`0x2C` | **Read** | Autonomous-safe. |

Gate behaviour today (`crates/best/src/gate.rs`):
```rust
pub enum Policy { ReadOnly, ConfirmedWrite }          // gate.rs:46
pub fn classify(sid: u8) -> SidClass {                // gate.rs:87
    match sid {
        0x10 | 0x3E | 0x22 | 0x2C | 0x19 => SidClass::Pass,
        0x34..=0x37                      => SidClass::RefuseAlways,
        _                                => SidClass::Gated,   // fail closed
    }
}
```
Both `0x14` and `0x31` land in the fail-closed `Gated` arm. Dispatch (`gate.rs:169-191`): `RefuseAlways`
matches first regardless of policy; `(ConfirmedWrite, Gated)` delegates; `(ReadOnly, Gated)` →
`ExchangeError::Refused`. So:
- **A `0x31` through the VM** is refused today, because `ReadOnly` is the only policy ever constructed.
- **A functional `0x14`** would be classified `Gated`, but never reaches the gate — the clear path
  bypasses `GatedExchange` entirely.

**Recommendation on the clamp cycle — do not ship it in P2.1.** Reasons:
1. It is genuinely physical actuation: it drops terminal 15 on the car for 15 seconds. That is a
   different risk class from erasing a fault store, and the ladder puts it in **Service write**
   (`Policy::ConfirmedWrite` + preconditions + `Reset` phase on failure).
2. Its exact payload byte is not yet pinned (§G.3.1).
3. It is separable — the erase is complete and correct without it; the clamp cycle only changes what the
   *cluster displays*.

Ship P2.1 as: functional clear + stragglers + supplier jobs + gateway ZFS + re-ident + verification
read, all under the existing `confirm=true`. Then propose the clamp cycle as its own gated tool
(`cycle_terminal_15`) with its own confirmation, its own preconditions (vehicle stationary, engine off,
no other operation in flight), and a documented 15-second window — and route it through
`GatedExchange::confirmed_write`, which would give that currently-unused constructor its first
production call site.

### F.7 Tests to extend (same idiom)

- `crates/client/src/scan.rs` unit tests with the `spawn` mock (`scan.rs:152`):
  `clear_faults_verified_reads_clears_and_confirms_clean` (`:209`),
  `clear_faults_all_continues_past_a_failed_ecu_and_never_resets` (`:243` — the mock deliberately serves
  `11 01` → `51 01` so a reintroduced reset cannot hide as a timeout; **keep this**).
- `crates/uds/src/service.rs`: `clear_all_encodes_14ffffff` (`:382`),
  `routine_control_start_with_params_appends_them` (`:429`).
- `mcp/tests/integration.rs`: the ordered frame census
  `clear_faults_sends_only_the_standard_frames_and_no_ecu_reset` (`:859`) is the strongest existing
  idiom — copy it for the new sequence:
  ```rust
  assert_eq!(frames, vec![
      vec![0x22, 0xF1, 0x90],       // connect: VIN read
      vec![0x19, 0x02, 0xFF],       // pre-read
      vec![0x10, 0x03],             // extended session
      vec![0x14, 0xFF, 0xFF, 0xFF], // standard clear-all
  ]);
  ```
  and `clear_all_faults_confirmed_clears_every_fitted_ecu_and_verifies` (`:1411`), whose wire-level
  filter asserts `reset_targets.is_empty()` (`:1457-1466`).
- New tests needed: a mock that answers one broadcast from **three different source addresses** (proves
  the many-response collector), a test that a stray frame with no waiter is still dropped (proves the
  fall-through did not become a catch-all), and a supplier-gating table test (proves `D_KBM` fires on
  `FRM_70` + SA `524` and **not** on `D_KBM` presence).

---

## G. Risks and unknowns

### G.1 Cannot verify without the car
- Whether the ZGW actually accepts `TARGET = 0xDF` in the HSFZ header and fans the request out
  (§B.5 is inference from the frame layout).
- The real quiet-period for collecting broadcast responses.
- Whether the 2nd..Nth `xsend` in the bytecode loop causes an actual retransmission on the wire or
  drains a buffered queue inside `XEnet64.dll` (§G.3.2). If klartext retransmits where ISTA drains, the
  car sees N broadcasts instead of one — harmless for an idempotent `14 FF FF FF`, but it is a real
  behavioural divergence and should be captured.
- Whether the supplier ECUs on the owner's cars respond to the `IS_LOESCHEN_*` jobs at all.
- CLAUDE.md already records that ~10/32 ECUs time out under a rapid whole-car sweep though they answer
  individually. A broadcast may expose that same wake/retry weakness differently — expect an incomplete
  responder list on the first attempt.

### G.2 What could go wrong on a real car
- **The clamp cycle is the sharp edge.** `STEUERN_KLEMMEN` with the wrong value byte, or issued while
  the car is not stationary, is the one step here that could do something genuinely unwanted. §F.6
  recommends deferring it; if it ships, it needs preconditions and its own confirmation.
- Broadcast clear hits **every** ECU including ones klartext has never talked to. A `14 FF FF FF` is
  idempotent and non-destructive, but the blast radius is by definition wider than the fitted list.
- Clearing erases freeze-frame/diagnostic evidence. klartext's pre-read mitigates this and must stay.
- `STEUERN_ZFS_LOESCHEN` is a `0x31` RoutineControl on the **gateway**. A wrong RID on the gateway is
  the highest-consequence single frame in the sequence — pin it with a unit test against
  `31 01 40 00 FF`.
- Ignition-off during the sequence drops the session (gateway FIN+RST, no auto-reconnect, per
  CLAUDE.md). The sequence is long (two settle sleeps + a full re-ident + a seven-step read); a mid-way
  drop leaves some ECUs cleared and some not. Report partial completion honestly rather than retrying
  blind.

### G.3 Where ISTA proved unreadable

1. **The exact `STEUERN_KLEMMEN` payload tail.** The `cas4_2.prg` template is
   `31 01 10 01 FF FF FF` (three placeholder bytes) and `TAB_CAS_KLEMMENSTATUS_ARG` supplies decimal
   `6`/`10`. How the table value is patched into those three bytes was not disassembled to completion.
   Do not guess — disassemble `STEUERN_KLEMMEN` to its `xsend` before implementing, or capture ISTA
   doing a clamp switch.
2. **`XEnet32.dll` / `XEnet64.dll` are native PE binaries** (`PE32+ x86-64`), not decompilable with
   `ilspycmd`. They own: the literal HSFZ header bytes for a functional send, whether anything encodes
   the address mode on the wire, the TesterPresent cadence, and NRC `0x78` retry timing. Their strings
   prove functional HSFZ sending exists (`CHsfzGateway::SendDiagTel`, `CHsfzRoutingTable`, a per-ECU
   state-machine array) but the byte-level behaviour is not readable. Match observable behaviour; do not
   reconstruct an algorithm.
3. **The trap timeout `settmr #0x180000`** in the functional collection loop is a trap-mask value, not a
   millisecond figure. The real inter-response timeout lives in the native driver.
4. **`VecInfo.ECU` population** — the straggler rule depends on `ECU_ASSEMBLY_CONFIRMED` and `BUSID`,
   which are set during identification by logic spread across `VehicleIdent.DoECUIdent` and the KMM
   rules engine (interface-only, per CLAUDE.md). klartext's fitted-ECU list from `scan_ecus` is the
   practical equivalent; it is not a byte-for-byte reproduction of ISTA's set.

### G.4 Repo hygiene — needs a commit from someone with write authority

`crates/best/tests/zz_scratch_disasm.rs` — a throwaway disassembler harness written during this research
— was swept into commit **`9dc80db`** ("fix!: drop the post-clear ECU reset") by another agent's broad
`git add`. Verified present in HEAD via `git ls-tree -r HEAD --name-only`. It is deleted in the working
tree (` D` in `git status`) and `cargo test -p klartext-best --no-run` is green without it, but **it is
still in HEAD and needs a commit to remove.** No commit was made from this research task.

---

## Ordered change list

1. `crates/client/src/session.rs` — add `request_functional(target, uds, quiet_period, max_responders)`
   and a broadcast-waiter fall-through in `route_frame` (§F.1). Validate positive SID + tester target;
   do **not** validate source == target. Keep the stray-frame drop intact.
2. `crates/uds/` — add the platform-resolved functional-address constant (`0xDF` for F01), with the
   E-series per-bus tables documented as a future case (§F.2). No change to `clear_all_dtcs` /
   `routine_control`.
3. `crates/client/src/client.rs` — add `clear_all_dtcs_functional()` (no `10 03` prefix) and
   `clear_gateway_combined_store()` = `routine_control(0x01, 0x4000, &[0xFF])` to `G_ZGW` (§F.3).
4. `crates/client/src/scan.rs` — restructure `clear_faults_all` into the ISTA-ordered sequence with
   `ClearSequenceReport`; straggler = fitted ∧ had-faults ∧ did-not-answer-broadcast; keep the
   never-clear-blind pre-read and `verified_clean` (§F.4).
5. Supplier jobs — implement the seven hardcoded gates verbatim from the §C.2 table, watching the three
   traps (SGBD vs GRUPPE, `D_KBM` via FRM_70/87 + SA `524`, `D_LM` via LM_AHL/LM_AHL_2). Ignore every
   result, as ISTA does. **Do not implement `IS_LOESCHEN_FUNKTIONAL` or a general `IS_LOESCHEN`** — dead
   code in ISTA (§C.3).
6. Post-clear sequence — 500 ms settle → re-ident (main pass only, §D.1) → 200 ms settle → verification
   read: functional FS read, FS details, functional IS read, IS details, gateway ZFS, check-control read
   (§A Phase 8).
7. `mcp/src/server.rs` + `dto.rs` — extend `clear_all_faults` and its result DTO; keep `confirm` first;
   no `reset` knob; fix the orphaned doc comment at `server.rs:1847-1851` (§F.5).
8. Tests — multi-source broadcast mock, stray-frame-still-dropped, supplier-gating table, gateway-ZFS
   frame pin `31 01 40 00 FF`, and an extended ordered frame census (§F.7).
9. **Deferred, separate proposal:** the clamp cycle (`cycle_terminal_15`) under
   `Policy::ConfirmedWrite` with preconditions — after §G.3.1 is resolved (§F.6).
10. **Needs a commit:** remove `crates/best/tests/zz_scratch_disasm.rs` from HEAD (§G.4).
