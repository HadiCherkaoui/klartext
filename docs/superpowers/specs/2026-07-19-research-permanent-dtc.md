# Parity research: permanent DTCs (UDS `19 15` / job `FS_LESEN_PERMANENT`)

> **Research record, 2026-07-19.** Produced under the 1:1 ISTA parity mandate.
> Verdict: klartext should NOT read permanent DTCs — ISTA only does so inside FASTA
> field-data export, never in the fault-read path. NO code change. Cited to
> decompiled ISTA + DDE bytecode. BYO data, gitignored.


## VERDICT (lead)

**NO — klartext should NOT add a permanent-DTC read.** ISTA never reads `19 15` in its
fault-read/display path (that path is `FS_LESEN` → `19 02`, which klartext already replicates).
The **only** place ISTA emits `19 15` is inside **FASTA** data collection — BMW's warranty /
field-data readout (`fasta6_pkw.run()`), gated by config, run per emissions-relevant ECU variant,
whose results are masked into the **FASTA export job list** and uploaded to BMW. They are **never
merged into the fault memory** and never shown as faults. klartext has **no FASTA equivalent**
(no warranty upload, no FASTA report) — it is not on the roadmap. Adding a standalone `19 15`
read/tool would *invent* a capability in a placement ISTA never uses, which is itself a parity
defect. The only "permanent" concept ISTA shows a mechanic — the S0751/S0756 pre-clear warning —
is a **filter over the already-read `19 02` fault list**, not a `19 15` read.

---

## 1. Call site(s)

`FS_LESEN_PERMANENT` is a single interned UTF-16 literal in exactly one DLL — `RheingoldDiagnostics.dll`
— referenced from **23 call sites**, and **all 23 are inside the FASTA provider classes**:

- `fasta6_pkw : FASTABase, IFASTAProvider` — the **car** FASTA provider
  (`RheingoldDiagnostics.dll`, decompiled `relfc/RheingoldDiagnostics.full.cs:372`), whose one giant
  method `run(IProgressMonitor monitor)` (`:2194`) contains 22 of the calls. First one at
  `:36500` inside a `G_DSC` variant block:
  ```csharp
  if (string.Equals(G_DSC_SGBD, "DSC_10")) {
      ...CALID_CVN_LESEN...
      ecuJob = ecuKom.apiJob(G_DSC_SGBD, "FS_LESEN_PERMANENT", "", "", retries, "", null, "run"); // :36500
      MaskResult(1,-1,"F_EREIGNIS_DTC"); MaskResult(1,-1,"F_HEX_CODE");
      MaskResult(1,-1,"F_READY_NR"); MaskResult(1,-1,"F_VORHANDEN_NR"); MaskResult(1,-1,"F_WARNUNG_NR"); ...
  }
  ```
  Groups covered: `G_DSC` (DSC_10/DSC_25 …), `G_EGS` (transmission), `G_MOTOR`, `G_MOTOR2`, `G_MRMOT`
  — i.e. powertrain + brakes, the OBD/emissions-relevant ECUs. Every call is gated on a specific ECU
  **variant** (`if (G_x_SGBD == "DSC_10")` …).
- `fasta6_ux` — the **motorcycle** FASTA provider (`:207200`), 1 call at `:213776`.

**It is NOT in the fault-read flow.** `BMW.Rheingold.Diagnostics.VehicleIdent`'s fault reading uses
`FS_LESEN` (`19 02`); grep of `VehicleIdent.cs` for `PERMANENT`/`FS_LESEN_PERMANENT` = 0 hits (verified).
This is not dead code — it is live, but its consumer is FASTA, not diagnostics display.

## 2. Trigger + condition

FASTA `run()` is reached only through `VehicleIdent.DoFASTAData(IProgressMonitor)`
(`RheingoldDiagnostics.dll`, `decompiled/VehicleIdent.cs:6265`):

- Provider chosen by `FASTAProviderFactory.GetInstance` (`relfc/RheingoldDiagnostics.full.cs:221255`)
  on `vecInfo.Prodart`: `"P"`→`fasta6_pkw`, `"M"`→`fasta6_ux`.
- `GRPLISTE` built from ECUs that **identified successfully**
  (`ECU_ASSEMBLY_CONFIRMED && IDENT_SUCCESSFULLY`, `VehicleIdent.cs:6289-6310`); for `BN2020`, `G_OBD`
  is appended only if `…doFASTAData().DoOBDReadout` is on (`:6305`).
- The actual `instance.run(monitor)` fires **only if `ConfigSettings.IsVehicleIdentReadFastaDataActive()`**
  (`VehicleIdent.cs:6321`→`ConfigSettings.cs:1212`, config key
  `BMW.Rheingold.Diagnostics.VehicleIdent.ReadFASTAData`).
- Driven from the session controller during the **vehicle test**:
  `Logic.ReadFastaFromVehicle(monitor)` (`RheingoldSessionController.dll`, `Logic.cs:2621`) →
  `if (ConfigSettings.IsVehicleTestReadFastaDataActive()) list = vecIdent.DoFASTAData(monitor);`
  (`:2654-2656`), after an ignition check (`:2628`), status `#CollectingFASTA`, recording
  `MetaData.DistanceOfFastaRead`/`DateOfFastaRead` (`:2659-2660`).

So: config-gated, vehicle-test-phase, per-emissions-ECU-variant, **warranty/field-data collection** —
not a user "read faults" action and not per-ECU fault reading.

## 3. The wire read (DDE bytecode — `data/Testmodule(1)/Ecu/d72n47a0.prg`)

Disassembled with klartext's own `klartext_sgbd::Prg` + `klartext_best::decode_job`
(probe `<scratchpad>/fsprobe`):

| job | bytecode | ops | xsends | request literal (built via `move`→S, then `xsend`) |
|-----|---------:|----:|-------:|-----------------------------------------------------|
| **FS_LESEN_PERMANENT** | 5520 B | 1012 | **1** (`xsend S3,S1` @`0x017c`) | `[19, 15]` @`0x0019` → **`19 15`** |
| FS_LESEN (contrast) | 7782 B | 1472 | 1 | `[19, 02, 0C]` → `19 02` (mask 0x0C) |
| FS_LESEN_DETAIL (contrast) | 59287 B | 10312 | 3 | `19 09`, `19 06 FF..`, `19 04 FF..` |

`19 15` = ISO 14229 **reportDTCWithPermanentStatus** — confirmed byte-for-byte, exactly one xsend.
The bytecode writes these result columns from the response (matching the FASTA `MaskResult` names in §1):
`F_HEX_CODE`, `F_ORT_NR`, `F_ORT_TEXT`, `F_EREIGNIS_DTC`, `F_READY_NR/_TEXT`, `F_VORHANDEN_NR/_TEXT`,
`F_WARNUNG_NR/_TEXT`.

**Response shape:** ISO 14229 `59 15 [DTCStatusAvailabilityMask:1] { DTC[3] statusOfDTC[1] }*` —
**identical 4-byte `[code:3][status:1]` record shape as `19 02`**. klartext's existing `59 02`
record decoder would apply directly; the SGBD merely names the status bits (ready/present/warning).
[Record shape is ISO-standard; not separately wire-captured on-car for `19 15`.]

## 4. How ISTA surfaces it

- **`19 15` results → FASTA export, not the fault list.** `MaskResult` →
  `FASTABase.MaskResult`→`ecuJob.maskResultFASTARelevant(...)` and into `ecuJobList`
  (`relfc/RheingoldDiagnostics.full.cs:221190-221213`); `DoFASTAData` returns that list
  (`instance.EcuJobList`) which becomes FASTA data (`Logic.cs:2656-2660`). It is **not** merged into
  `Vehicle.FaultList` and is **not** rendered as a fault. Contrast: info memory (`22 2000`) is a read
  ISTA shows *alongside* faults; permanent DTCs are not — they go into the warranty dump.
- **The mechanic-facing "permanent" ISTA does show** is unrelated to `19 15`: in the **clear** path,
  `IstaOperationActionImpl.PerformClearErrorMemory` (`IstaOperationImpl.dll:718`) calls
  `logic.VecInfo.PermanentSAEFehlercodesInFaultList()` (`RheingoldCoreFramework.dll`,
  `Vehicle.cs:1174-1192`), which **iterates the already-read `FaultList`** for
  `fault.DTC.FortAsHexString == "S 0751" | "S 0756"` and warns before erasing (texts `#00a1/2/3`,
  resource "Note: Permanent SAE fault codes are saved", `Resources.resx:4321`; status label
  "Status: Permanent" `#SaeStatusText4` `:1213`). **No `19 15` is emitted** — it is a filter over the
  `19 02` faults. (This pre-clear parity is owned by the clear-sequence work, not here — see
  `p2-clear-sequence.md` / `p0-fault-relevance.md`.)

## 5. Verdict for klartext + change list

**Do not add a permanent-DTC read now.** Rationale, against the 1:1 mandate:

1. ISTA's fault read = `FS_LESEN` = `19 02` (+ `19 04/06` freeze frames via `FS_LESEN_DETAIL`).
   klartext already matches this. `19 15` is **absent** from that path.
2. ISTA's only `19 15` use is **FASTA warranty data collection**, a flow klartext does not have and
   is not on the roadmap (`docs/superpowers/specs/2026-07-03-m11-ista-parity-roadmap.md` has no FASTA
   item). A standalone `read_permanent_faults` MCP tool / mobile field would place `19 15` where ISTA
   never places it → a parity defect in the opposite direction.
3. The "permanent" a mechanic sees is the S0751/S0756 pre-clear filter over the `19 02` list — a
   fault-list concern, not a new wire read.

**Concrete outcome: no code change.** No new client method, no new `uds` decoder, no MCP tool.
The honest placement is: **permanent DTCs belong to an unbuilt FASTA/field-data export.** If the owner
ever decides to build that (out of current scope), the faithful shape is:
`19 15` as one job among the FASTA dump, gated **per emissions-relevant ECU variant** exactly as
`fasta6_pkw.run()` does (DSC/EGS/MOTOR/MOTOR2/MRMOT variant guards), results kept in that export
structure and **never merged into `read_faults`**. Reusing klartext's `59 02` 4-byte record decoder is
then correct (§3). Until that flow exists, adding `19 15` anywhere is not ISTA parity.

### Unreadable / not decompiled
None for this question. All call sites, the trigger chain, the gate, the clear-path filter, and the
DDE wire literal were readable and are cited above. (The `59 15` *response* record shape is stated
from ISO 14229 — same as the on-car-confirmed `59 02` shape — and was not separately captured from a
car, since ISTA only reads it during a FASTA session.)
