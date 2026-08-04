# Car session 2 — 2026-08-02 — parity + write-tier protocol, tcpdump-verified

Execution of `docs/on-car-test-protocol-2026-07-19.md` against the **F25 X3**
(gateway `169.254.71.121`, I-Stufe `F025-23-07-530`, bordnet `F25_1404`, DDE `d72n47a0`).
Every claim below is backed by bytes in the capture, not by tool output alone.

> **Re-verified independently 2026-08-04** by re-parsing all three pcaps with a second HSFZ
> walker. Capture integrity is established: the two overlapping `tcpdump` instances recorded
> **byte-identical frame sets** across their 30 s overlap, so nothing was dropped. The SID
> census and every pass/fail row below reproduce — **except** §3.4, §4's info-memory
> paragraph and A6's severity claim, which were wrong and are corrected in place (each marked
> **CORRECTED 2026-08-04**). One defect the original pass missed is added as §3.6.

Captures (gitignored, contain the VIN — never commit):
`captures/oncar-20260802-1639.pcapng` (Phase A/B + C1),
`captures/oncar-20260802-1650-part2.pcapng` (C2–C5, packet-buffered) and
`captures/oncar-20260802-1710-vm.pcapng` (§7 VM read tests, packet-buffered).
The first split exists only because the first `tcpdump` ran without `-U` and buffered; a
second `-U` capture was started *before* the first was closed, so no frames were lost.
**Always pass `-U`** — buffering hid C1 entirely until the process was stopped.

Decoding: `tshark -d tcp.port==6801,hsfz` (the decode-as name is `hsfz`, **not**
`hsfz_over_tcp`). tshark hands `ctrlword 0x0001` payloads to the UDS sub-dissector, so
`hsfz.data` is empty on real requests — extract `tcp.payload` and walk it instead. An HSFZ
frame is `[u32 length][u16 ctrl][u8 src][u8 tgt][UDS…]` where **`length` counts `src+tgt+uds`
only** (header is 6 bytes, so the next frame is at `off + 6 + length`). One TCP segment can
carry several HSFZ frames; a naive one-frame-per-segment parse silently dropped 107 of 1 883
frames here, including the entire `2C`/`22 F303` sequence. Reusable walker:
`scripts/` has none — the throwaway lived in the session scratchpad.

---

## 1. Pass/fail summary (protocol §5)

| # | Property | Verdict | The bytes |
|---|---|---|---|
| A1 | VIN ladder stops at first rung | **PASS** | one `22 F1 90` → `62 F1 90 …`; no CAS/FRM rungs |
| A2 | SVT read | **PASS** | one `22 3F 07` → `62 3F 07 **0020** …` = 32 ECUs |
| A3 | I-Stufe + VIN | **PASS** | `F025-23-07-530`; VIN matches A1 |
| A4 | **fault mask** | **PASS** | `19 02 0C` ×33, `19 02 FF` ×**0** |
| A4 | **info fold-in, no version byte** | **PASS** | `62 20 00 \| 277F00 20 \| 2C9D00 20 \| 36F800 2F` |
| A5 | **sequential, in-flight depth 1** | **PASS** | 467 requests, **0 violations**; **0/32 ECUs timed out** |
| A6 | **detail order + `F_SEVERITY` gate** | **PASS (order only)** | DDE `19 06`→`19 04`, **no `19 09`**; headunit `19 09`→`19 06`→`19 04`. **Every DDE detail read was REFUSED (`7F 19 31`) and the headunit's `19 09` was refused (`7F 19 12`)** — see §3.6 and §4 |
| A7 | dynamic measurement | **PASS** | `2C 03 F303` / `2C 01 F303 4517 01 02` / `22 F303` → `62 F303 3200` = 28 °C |
| A8 | write-tier discovery | **PASS** | 64 `function_id`s with titles + operator text |
| B1 | sequential depth (re-check) | **PASS** | same capture as A5 |
| B2 | retry discipline | **PASS** | 1 retry (timed-out read); 319 negatives, **none** retried |
| B3 | VIN mismatch abort | **N/A** | only the F25 was reachable (F20 not connected) |
| C1 | **no post-clear reset** | **PASS** | `19 02 0C`/`10 03`/`14 FF FF FF`→`54`, then **nothing**; `11 xx` ×0 |
| C2 | **functional clear** | **PASS** | `14 FF FF FF` → **`0xDF`**, 31 ECUs answered `54` |
| C2 | **gateway ZFS** | **PASS** | `31 01 40 00 00` → `0x10` → `71 01 40 00 00` |
| C2 | **clamp cycle** | **PASS** | `31 01 10 01 06 06 A8` → **15.16 s** → `31 01 10 01 0A 0A 43`, both `71`-acked |
| C2 | **no reset** | **PASS** | `11 xx` ×0 across the whole sequence |
| C2 | supplier info clears absent | **PASS (known gap)** → see §4 | total `0x14` on the wire = 2; but the real gap is larger — klartext sends NO info-memory clear of any kind (ISTA has three) |
| C3 | **confirmed write reaches car** | **PASS** | `2F 60C3 03 0001` → `6F 60C3 03`; `34`–`37` ×0 |
| C4 | held teardown | **N/A** | C3's function is non-holding by catalog (`has_reset:false`, `hold:none`) |
| C5 | socket closes | **PASS** | FIN/ACK from tester, gateway FIN back |
| — | permanent DTCs never read | **PASS** | `19 15` ×0 |
| §7 | 3-`xsend` dedicated job via VM | **PASS** | `2C 03`/`2C 01 F303 4BC3 01 02`/`22 F303` → `0B EA` = 31.86 °C |
| §7 | CP1252 `_INFO` decode | **PASS** | `Drehzahl des E-Luefters`, no mojibake |
| §7 | VM self-report vs wire | **PASS** | every `_REQUEST_n`/`_RESPONSE_n` byte-identical to the pcap |
| §7 | structured multi-value (`RES_`) | **NOT CLOSED** | single-`ARG` job returns one triplet; needs DSC + §8 |
| §7 | `0x7FFF` sentinel handling | ~~FAIL~~ → **PASS** | withdrawn 2026-08-04: `-0.083 rpm` IS this row's encoding of 0 rpm (unsigned 16-bit, MUL 0.152590, ADD −5000 ⇒ ±5000 range centred on `0x7FFF`). No sentinel in the SGBD, the bytecode, or ISTA — see §7.5 |

Rows marked §7 are the follow-on VM read tests run after the protocol proper; §8 records why
the AC and oil-level tests could not be run at all.

**Whole-run SID census** (Phase A/B/C captures, transmitted requests only):

```
3e ×515   22 ×489   19 ×109   31 ×3   2c ×2   14 ×2   2f ×1   10 ×1
11 ×0     2e ×0     34-37 ×0    19 15 ×0
```

Every write SID on the wire is accounted for and was `confirm`-gated. TesterPresent cadence
is **2.0 s**, dead steady (510 of 512 gaps at exactly 2.0 s).

---

## 2. What improved since car session 1

- **Zero ECU dropouts.** Session 1 lost ~10/32 ECUs to timeouts under the rapid sweep; here
  all 32 answered, with exactly **one** retry in 467 requests. P1.2's strict one-outstanding-
  request discipline is confirmed on the wire: no request was ever issued while another was
  open (the only apparent violation was the P1.1 retry, 209 ms after a `22 2000` timeout to
  `0x17`, which then answered).
- **The TCP session survived the ignition drop.** Session 1 recorded "gateway FIN+RST on
  ignition-off drops the session with no auto-reconnect". Here the KL15 cycle via `0x40`
  produced **zero FIN/RST**; the same connection carried the re-identification and the
  verification read straight through.
- **The corrected ZFS byte is confirmed on the car.** `31 01 40 00 **00**` — the `FF`
  placeholder fixed in `62e4abf` was indeed wrong; the gateway accepted the `00` form with
  `71 01 40 00 00`.
- **The `F_SEVERITY` gate discriminates in both directions in one capture** — the DDE
  (`F_SEVERITY = nein`) got no `19 09`, the headunit got one.
  **CORRECTED 2026-08-04:** the gate *branched* both ways, but the headunit then **refused**
  the read — `19 09 B7F805` → **`7F 19 12`** (subFunctionNotSupported). So `F_SEVERITY = ja`
  in the SGBD does **not** imply the ECU implements `19 09`; the gate saves a round trip only
  on the `nein` side, and on the `ja` side it can still spend one and get nothing. The
  original wording ("the strongest possible form of this test") overstated it.

C2's post-cycle timing matches the spec to the millisecond: clamp-ON response at t=104.453,
re-identification `22 3F07` at t=104.954 (**501 ms**), verification read at t=105.171
(**202 ms** after the re-ident response).

---

## 3. Findings to act on

### 3.1 `setflt` (opcode `0x88`) is unimplemented in the BEST/2 executor — blocks actuation

`run_service_function` on the DDE's **Electric fan** (`function_id 20000725015199`, job
`STEUERN_E_LUEFTER`, arg `"90"`) failed with:

```
opcode `setflt` is not implemented by the executor
```

The opcode is *decoded* (`crates/best/src/opcode.rs:226` — `op("setflt", Misc, false)  // 0x88`)
but has no executor arm (`crates/best/src/exec.rs:142` raises the error). **No frame was
transmitted** — the VM aborted before any `xsend`, so nothing actuated and the failure path
behaved correctly (`held:false`, teardown ran).

**SETTLED + FIXED 2026-08-04 — and the guess above was wrong.** `setflt` is "set FLOAT
precision", not "set fault". `OpSetflt` (EdOperations.cs) is one line:

```csharp
ediabas._floatPrecision = arg0.GetValueData();
```

No fault, no flags — it sets the significant-digit precision `flt2a` formats with. klartext
had hardcoded that precision (`FLOAT_PRECISION = 4`) with a comment already naming the gap:
*"only changed by a config op not built in Phase 1"*. That config op is this one. `setflt` now
writes `Machine::float_precision` and `flt2a` reads the live value, pinned by a test that
changes it and asserts the formatting changes with it.

`STEUERN_E_LUEFTER` is now **opcode-complete** — the VM no longer aborts before transmitting.
Whether the actuation itself succeeds is a separate question that needs the car.

The second actuation, **Air duct flap 1** (`20000725001437`, `STEUERN_GLF`, arg `"1"`),
ran clean, so this is opcode-specific, not a broken write path.

### 3.2 `run_service_function` opens no extended session

The protocol expected `10 03` before the phase jobs; none was sent. The DDE accepted
`2F 60C3 03 0001` in the default session and answered positively.

**SETTLED 2026-08-04 — the expectation was wrong, klartext is right.** Neither side sends it:
`STEUERN_E_LUEFTER`, `STEUERN_GLF` and `STATUS_BLOCK_LESEN` contain **zero** `10 03` literals
in their bytecode, and grepping the decompiled `RheingoldDiagnostics` for
`DiagnosticSessionControl`/`ExtendedDiagnostic`/`10 03` around the service-function path
returns nothing. EDIABAS opens no session for these jobs, so neither should klartext. No code
change; the protocol document's expectation is the thing to correct.

**But the same check found a real divergence next door — see §3.8.**

### 3.3 "Ran and returned to safe" overstates what klartext did

C3 reported `note: "Ran and returned to safe."` with `teardown: "not_defined"`, and the wire
shows **one** `2F` and no return-to-safe frame. For a `has_reset:false` / `hold:none`
function that is correct by catalog — the ECU self-reverts after its 20 s activation
(`controlOption 0x03`, shortTermAdjustment) — but the wording claims an action klartext did
not take.

**FIXED 2026-08-04.** The two cases now read differently: an undefined teardown says klartext
sent no return-to-safe frame and the ECU is expected to revert on its own, while a real one
says klartext transmitted it. Pinned by a test that asserts the strings differ and that
neither carries the old ambiguous phrasing.

### 3.8 NEW (found 2026-08-04) — klartext opens an extended session before a clear; ISTA does not

Found while settling §3.2. klartext's per-ECU clear sends `10 03` before `14 FF FF FF`
(`crates/client/src/client.rs:874`, whose comment asserts "BMW requires [it] before a clear" —
unsourced). Three things say otherwise:

- **BMW's own clear job does not.** `FS_LOESCHEN`'s bytecode on `d72n47a0` contains exactly one
  request literal — `14 FF FF FF` — and no `10 03`.
- **klartext's own broadcast path does not.** `clear_all_dtcs_functional` sends the bare
  broadcast.
- **The car proves it is unnecessary.** In `oncar-20260802-1650-part2.pcapng` the whole-vehicle
  clear ran with **no `10 03` anywhere** (the capture's SID census has no `10` at all) and
  **31 ECUs answered `54`**.

So the session control is a klartext invention, inconsistent between its own two clear paths,
and demonstrably not needed on this car. By the parity mandate that is a defect.

**NOT changed here — this is an owner call.** Removing it alters a write path, and the
evidence, while strong, covers the broadcast rather than every physically-addressed ECU: an
ECU that genuinely requires the extended session would start refusing its clear
(`7F 14 22`). That failure would be reported rather than silent, so the risk is annoyance
rather than data loss — but it is still the owner's decision, not the implementer's.
Recommendation: **remove it**, matching `FS_LOESCHEN` and klartext's own broadcast.

### 3.4 Four ECUs time out on the PRE-clear read — the clear proceeds without a discard record

**CORRECTED 2026-08-04. The original text had this backwards** (it claimed the timeouts were
on the post-clamp verification read, and blamed ECUs "still booting ~700 ms after KL15 came
back"). The capture says the opposite, and the corrected version matters more.

`0x06`, `0x08`, `0x19`, `0x2C` timed out on C2's **pre-clear** read — two attempts each
(initial + the P1.1 retry), 1.2 s apart, **zero responses**, all of it before the broadcast
clear at t=87.498:

```
75.160 TX ->0x06 19020c      77.562 TX ->0x08 19020c      80.757 TX ->0x19 19020c
76.361 TX ->0x06 19020c      78.763 TX ->0x08 19020c      81.958 TX ->0x19 19020c
   (no response)                (no response)                (no response)
```

(`0x30` also timed out once and recovered on the retry.) On the **post-clamp verification**
read every one of the 32 ECUs answered first time, in milliseconds — `0x06` at t=105.247,
`0x08` at 105.359, `0x19` at 106.344, `0x2C` at 106.574. And all four had answered in
milliseconds ~8 minutes earlier, in capture 1's A5 sweep (t≈244).

**So they fell asleep between A5 and C2, and the KL15 cycle woke them.** The clamp cycle is
the cure here, not the cause; a longer settle before the verification sweep would fix a phase
that had no problem.

What this actually exposes: the pre-read **is** the tier ladder's "pre-read + record each
ECU's DTCs before erasing" safety step. For those four it produced an error, not a fault
list, and the sequence went on to erase them anyway — they answered the broadcast `54`, so
`crates/client/src/scan.rs:560` (`(had_faults || unreadable) && !answered.contains(…)`)
correctly did not make them stragglers, and nothing else flagged that four ECUs were cleared
with no discard record. No data was provably lost this run (all four read back empty), but
the mechanism is the point. **Open question for the owner:** should `clear_all_faults` refuse,
or at minimum report prominently, when the pre-read failed for an ECU it is about to erase?
Worth checking what ISTA does when its `FS_LESEN` pre-read fails before a vehicle clear.

### 3.5 FA (vehicle order) still decodes to nothing

`identify_vehicle` returned `baureihe`, `typ_schluessel`, `build_date`, `lackcode`,
`polstercode` all null and `options: []` from a 214-byte `62 3F06` payload (`version: 87`).
Unchanged from session 1's "FA decode incomplete". The raw bytes are in the capture.

**Investigated 2026-08-04; still open, but narrowed.** `decode_vehicle_order`
(`crates/semantic/src/identity.rs:69`) is a stub that reads the version byte and returns
`None` for every field — so this is unimplemented, not mis-implemented. The payload is **211
bytes after the `62 3F 06` echo** and is **bit-packed, not ASCII**: the few readable
fragments in a hex dump are coincidental, and the obvious 6-bit-packed-alphanumeric
hypothesis was tested at every byte offset in both bit orders and produces noise (no
recognisable 3-character SA codes, no plausible 4-character type key).

So this is reverse-engineering, not a fix, and it should be done from ISTA's own parser
rather than guessed. Next step and its target: `Fahrzeugauftrag` appears in
`RheingoldISTACoreFramework.dll`, `RheingoldFASTA.dll` and
`RheingoldOperationsReportConverter.dll`; `Salapa` in `RheingoldCoreContracts.dll` and the
PSdZ adapters. Decompile those and read the FA parser before writing any decode. The captured
211-byte vector is the test fixture once the layout is known — it stays in the pcap, not in
the repo, because the vehicle order identifies the car.

### 3.6 NEW (found 2026-08-04) — `read_fault_detail` sends fault-memory services for an info-memory code

Two individually-correct features composing wrongly. On the wire:

```
336.651 TX ->0x12  19 06 36F800 FF   →  7F 19 31   (requestOutOfRange)
336.678 TX ->0x12  19 04 36F800 FF   →  7F 19 31
```

The DDE's **fault** memory is empty — `19 02 0C` → `59 02 FF`, availability mask and zero
records, on every read in both captures. `36F800` exists only in its **info** memory:
`62 2000 277F00 20 | 2C9D00 20 | 36F800 2F`. So A6 pointed `read_fault_detail` at an
info-memory code and klartext asked the fault-memory services about it, which by construction
cannot know it.

The request *form* is fine — the identical `FF` record byte worked on `0x63`
(`19 06 B7F805 FF` → `59 06 …`), where the DTC really was in fault memory. The defect is
routing, not framing.

**Cause.** P2.2 (`8fb5347`) folded info memory into `read_faults` and tagged every fault
`source: "fault_memory" | "info_memory"` — so info-memory codes are now first-class output,
and an operator or agent will naturally feed one straight back into `read_fault_detail`.
But `read_fault_detail` (`crates/client/src/client.rs:512`) takes `target` and `dtc` and no
source, and unconditionally issues `19 09`/`19 06`/`19 04`. Per `CLAUDE.md` the info-memory
detail read is a different service entirely: **`22 20 nn`** (`IS_LESEN_DETAIL`). The fold-in
created the reachable path; the detail read never learned about it.

Fix design must follow the parity mandate — decompile the owning ISTA component and read the
DDE `.prg`'s `IS_LESEN_DETAIL` control flow before implementing, rather than assuming the
request layout from the job name.

**FIXED 2026-08-04.** `read_fault_detail` now reads the stores before addressing one and
reports `source` (`"fault_memory"` / `"info_memory"` / `null`); the `19 xx` services are sent
only for a fault-memory code. That mirrors how ISTA reaches a detail read at all — it iterates
`sg.FEHLER` into `FS_LESEN_DETAIL` and `sg.INFO` into `IS_LESEN_DETAIL`, and its fault-memory
step requires `FS_LESEN` alongside `FS_LESEN_DETAIL` (`DoEcuJob(…, "FS_LESEN_DETAIL,FS_LESEN")`,
`RheingoldDiagnostics` :225398). klartext does **not** yet issue the info-memory equivalent —
see §3.7.

### 3.7 NEW (found 2026-08-04) — klartext hand-builds frames the `.prg` chooses per ECU

Raised by the owner while reviewing §3.6: *why is so much of this hardcoded binary — isn't
fault clearing a `.prg` function? How does ISTA do it?* Measured, the answer is uncomfortable.

**ISTA never builds a diagnostic frame.** It names a job and lets EDIABAS run that ECU's
bytecode: `apiJob(sg.ECU_SGBD, "IS_LESEN_DETAIL", item.F_ORT.ToString(), …)`
(`RheingoldDiagnostics` :226004), `ApiJob(sg.ECU_SGBD, "FS_LESEN_DETAIL", fDTC.F_ORT…)`
(:225405), `DoEcuJob(vecInfo, mECU, "IS_LESEN")` (:225834). The `.prg` decides the service,
the layout and the decode. klartext instead hardcodes `19 02 0C` / `22 20 00` / `14 FF FF FF`
and hand-rolls the detail reads.

Those literals are not invented — each was read out of this DDE's own bytecode. The problem is
that **they are per-ECU choices, not constants.** Extracting the request template every
`IS_LESEN` moves into its request register (the `move S1, <literal>` at op 3, the same slot in
every variant) across the shipped SGBD set:

| `IS_LESEN` request template | SGBDs |
|---|---|
| `22 20 00` — what klartext hardcodes | **334** |
| **`19 17 0C 01`** — ISO 14229 `reportUserDefMemoryDTCByStatusMask`, memory #1 | **255** |
| built dynamically (no literal template) | 294 |

The split is generational: `acsm3`/`acsm4`/`acsm5`/`aag_f15` (F-series) use `22 20 00`;
`acsm6`/`acsm7`/`adcam_*`/`bat48_*` (G-series) use `19 17 0C 01`. Verified by disassembly, not
inferred from names — `acsm6.prg` `IS_LESEN` op 3 is `move S1, [19 17 0C 01]`, structurally
identical to `d72n47a0.prg` `IS_LESEN` op 3 `move S1, [22 20 00]`.

**Consequence:** on any ECU in the 255-SGBD family, klartext sends `22 20 00`, gets a negative,
and reports `info_supported: false` — *"this ECU has no info memory"* — when it has one behind
a different service. This car is unaffected (33 of 34 answered `22 20 00` positively; the F25
is entirely in the F-series family), so it is a latent defect, not an observed one. It is
exactly the failure mode the standing "do what ISTA does / must work on ANY car" principle
exists to prevent.

**Why klartext hardcodes at all, measured.** The BEST/2 VM is the right long-term path and is
closer than expected — the whole fault path is 6 opcodes away:

| Job | ops | runnable in the VM today | missing |
|---|---|---|---|
| `FS_LESEN` | 1 472 | **yes** | — |
| `IS_LESEN` | 1 095 | **yes** | — |
| `IS_LOESCHEN` | 212 | **yes** | — |
| `STATUS_LESEN` | 27 615 | **yes** | — |
| `FS_LOESCHEN` | 285 | **yes** | — |
| `FS_LESEN_DETAIL` | 10 312 | no | `setspc`, `srevrs`, `stoken`, `tabsetex` |
| `IS_LESEN_DETAIL` | 12 658 | no | `setspc`, `srevrs`, `stoken`, `tabsetex` |

**Four** opcodes total — `setspc` (0x52), `srevrs` (0x53), `stoken` (0x54), `tabsetex` (0xAA) —
and the same four for both detail jobs. *(A first pass of this measurement claimed six, adding
`parl`/`parw` and marking `FS_LOESCHEN` blocked. That was a measurement bug, not a finding: the
coverage script matched only single-hex `0x5A =>` arms and missed range arms, so it did not see
`0x55..=0x57 => op_parl` at `exec.rs:369`. `parl`/`parw` have been implemented all along and
`FS_LOESCHEN` was already runnable.)*

So **five of the seven fault-path jobs already run**, and the two detail jobs are four opcodes
away — none of them comm or control ops: three string ops and one table op. The second blocker
is variant resolution (§8): the VM must know *which* `.prg`, and that fails on 30 of 32 ECUs
today.

**DONE 2026-08-04 — all four implemented, and they were the same four the IDENT rung needed.**
`setspc`/`srevrs`/`stoken` are the string-token trio (`setspc` arms a separator + a 1-based index
that `stoken` then splits on; all three miss paths write nothing and set Zero). `tabsetex` is the
one that mattered: it switches the table source to **another SGBD file**, which is how a job
reaches a shared table. With it, `FS_LESEN_DETAIL` and `IS_LESEN_DETAIL` are both
**opcode-complete**, and — unplanned — so is the variant ladder's missing rung (§8.3):

`g_klima3.grp`'s `IDENTIFIKATION` was **one opcode** (`tabsetex`) from running. Disassembled, it
is not a mystery at all:

```
op   1  move S1, [83 FF FF 22 F1 50]     UDS ReadDataByIdentifier F150
op  29  xsend S3
op 551  tabsetex "ZuordnungsTabelleUDS", "t_grtb"
op 565  tabseek  "ADR_INDEX"              key = "<addr> <ident index>"
op 583  tabget   S5, "SGBD"               the variant name
op 593  ergs     "VARIANTE"
```

and `VARIANTE` is exactly what ISTA assigns: `mECU.ECU_SGBD =
identJob.getStringResult("VARIANTE")` (`RheingoldDiagnostics` `DoAfterIdentProcessing` :223322).
Proven end to end on real data in `crates/best/tests/group_ident.rs`: the job runs to `eoj` in
klartext's VM and returns `HKA_02` for ident index `0F11B0`, `HKA_G12` for `0F21C0` — real rows
of `t_grtb`'s `ZuordnungsTabelleUDS`. So §8's chicken-and-egg is broken **by running ISTA's own
job**, with no heuristic and nothing invented.

**Wired into the ladder the same day.** `resolve_variant` gains a fourth rung after the three
offline ones (explicit → learned profile → DB-unique → **group ident on the car**), used by
`read_data`, `read_fault_detail` and `run_job`. The address→group mapping needed no new data:
the semantic `ecu` table's `group_name` already holds EDIABAS group names, and **422 of its 428
distinct values have a matching `.grp` on disk**. A resolved variant is written to the learned
per-VIN profile, so it costs one round trip per ECU per car, not one per read. Every failure
degrades to the old "need a variant" error — the rung can only add resolutions.

Coverage measured, not assumed: **261 of the 427 shipped group SGBDs have a runnable
`IDENTIFIKATION` today**; the other 166 are the K-line-era `d_` groups needing
`xsetpar`/`xawlen`/`shmset`/`shmget`/`sett`/`eerr`, which have no meaning over HSFZ anyway. That
is why the rung tries `g_` groups first. For §8.2's actual blocker — IHKA `0x78`, 28
indistinguishable candidates — the DB lists both `d_klima` and `g_klima`, and **`g_klima` is
runnable**, so that case is now reachable. Confirming it on the car is the next session's job.

**Recommended reordering of §8.4:** the `.grp` IDENT rung (§8.1–8.2) and these four opcodes are
the same project — together they let the fault path become job-driven, which removes this whole
class of defect rather than patching instances of it. §3.6's guard is the right fix for today,
but it re-derives in Rust a distinction the bytecode already encodes as two separate jobs.

---

## 4. `[verify against capture]` markers this session flips

- **Info-memory (`22 2000`) response framing — CONFIRMED.** Every `62 20 00` payload across
  many ECUs is a whole multiple of **4 bytes**, decoding as 3-byte code + 1-byte status, with
  **no version byte** and no count prefix. Witnesses: `62 2000` (empty, 0 entries),
  `62 2000 002031 2F` (4 B), `62 2000 277F00 20 2C9D00 20 36F800 2F` (12 B),
  `62 2000 101008 0F 100600 0F 100104 0F 100001 4C 100203 4C 100204 4C` (24 B).
- **`22 3F08` responding-list framing — CONFIRMED.** `62 3F 08 0020 <32 bytes>` — identical
  u16-BE-count + one-byte-per-ECU shape as `3F07`, and on this car an identical member list
  (32 configured, 32 responding).
- **`22 3F07` SVT framing — CONFIRMED** (`62 3F 07 0020` + 32 address bytes).
- **`19 04` / `19 06` on-car exchange — CAPTURED** (first time). From `0x63`:
  `59 06 B7F805 2F 01 00 02 3F 03 28` and a 72-byte
  `59 04 B7F805 2F 00 05 17 00 02 65 D2 17 01 11 49 E4 …`.
  The request/response **shape** is now on record; the field **decode** is still unvalidated
  because `0x63`'s variant did not resolve (`sgbd_available:false`). Re-run `read_fault_detail`
  on `0x63` with an explicit `variant` to close this.
  **CORRECTED 2026-08-04:** the reason the DDE contributed nothing here was *not* "no stored
  freeze-frame" — both its detail reads were **refused** (`7F 19 31`) because the code passed
  in came from **info memory**, a different store. That is a klartext routing defect, now
  §3.6.
- **Negative responders on a functional clear — PARTIALLY settled.** `session.rs:449` carries
  a `[verify against capture]` on "a responder that answers negatively … is treated as a
  straggler and re-addressed physically". Half of that is now observed and half is not:
  `0x29` answered **`7F 14 21`** (busyRepeatRequest — semantically "resend me", unlike
  `0x78`) and was correctly left out of the responder list, but it was **never re-addressed**,
  because the `had_faults || unreadable` term of `scan.rs:560` excluded it (it had no faults).
  So the degradation path itself remains unexercised. `0x29` is the only one of the 32 that
  never answered `54`.

Also observed: the `14 FF FF FF` clear alters the **DDE's** info-memory status bytes — its
entries went from `277F00 20 / 2C9D00 20 / 36F800 2F` before to `277F00 50 / 2C9D00 10 /
36F800 2F` after (the `testFailedSinceLastClear` bit cleared, `testNotCompletedThisOperation
Cycle` set). The info-memory *entries* survive the clear; on the DDE their status bits do not.

**CORRECTED 2026-08-04 — that generalised from a single ECU, and the whole-car picture is the
opposite.** Comparing every ECU's `62 2000` payload before (t≈75–87) and after (t≈105–107):
of the **14 ECUs holding info-memory entries, only the DDE's status bytes changed at all**.

| ECU | entries | before → after |
|---|---|---|
| `0x12` DDE | 3 | `20 / 20 / 2F` → `50 / 10 / 2F` — **changed** |
| `0x1C` | 4 | `2C 2C 2C 2C` → identical |
| `0x10` | 6 | `0F 0F 0F 4C 4C 4C` → identical |
| `0x18` / `0x40` / `0x60` / `0x61` / `0x63` / `0x3D` / `0x00` | 1–2 each | identical |
| `0x29` | 2 | `2C / 6C` → `6C / 6C` — the `+0x40` new-operation-cycle bit only, which the KL15 cycle sets on every ECU; **not** a clear signature |

All of those ECUs answered `54` to the broadcast. So the measured fact is:
**`14 FF FF FF` does not clear info memory on 13 of the 14 ECUs that hold it.** The
discriminator for a genuine clear is the `0x10` bit (`testNotCompletedSinceLastClear`)
appearing while `0x20` (`testFailedSinceLastClear`) drops — seen on the DDE alone.

**ROOT CAUSE FOUND 2026-08-04, and it is bigger than the known gap.** This was blamed on the
six unsent supplier jobs (`IS_LOESCHEN_TMS` etc.). That is not the explanation — those are
ECU-specific (`FEM_20`, `FRM3`, `D_KBM`, `ALC_60`, `LM_AHL`…) and this F25 selects none of
them. The real cause is that **klartext never clears info memory at all.** ISTA has three
paths here and klartext implements none:

| ISTA | Where | Scope |
|---|---|---|
| `IS_LOESCHEN_FUNKTIONAL` | `DoECUClearIS` :223007 | broadcast, via the group SGBD |
| `IS_LOESCHEN` | `doECUClearIS` :222975 | per-ECU physical, keyed on `ECU_GRUPPE` |
| `IS_LOESCHEN_TMS` / `_SMC_*` | `ClearErrorInfoMemoryVehicle` :228350-228386 | six named supplier stores |

So `14 FF FF FF` is simply the wrong instrument — the fault memory's clear was never going to
touch the info store, and nothing else was sent. The DDE's entries changing is the one
side-effect, not evidence the mechanism works.

**NOT implemented here — deliberately.** This is a fault-ERASING write on a store the owner
has never cleared, and it needs three things this session cannot supply: the BEST/2 VM inside
`crates/client/src/scan.rs`'s clear sequence (which cannot depend on `klartext-best`, so the
job runner has to be threaded in from the composing binary), a decision about erasing more
than klartext erases today, and a car to verify against. `IS_LOESCHEN` itself is ready — 212
ops on `d72n47a0`, **opcode-complete** — so the work is the plumbing and the owner's
go-ahead, not the VM.

---

## 5. Vehicle state

Pre-clear faults: DAB L-band aerial open circuit (`B7F805`, headunit `0x63`, **present**),
AUC sensor internal fault (`C90D60`, JBBF `0x00`), and two TCB mobile-network entries
(`03178A` / `031786`, ECALL `0x61`). After the whole-car clear, 27/32 verified clean, the AUC
and TCB codes are gone, and **`B7F805` re-set immediately** with `testFailedThisOperation
Cycle` — an active hardware fault, exactly as session 1 found. DPF/engine side is quiet;
oil temperature read 28 °C.

---

## 6. Data hygiene

All three pcaps are under `captures/` and matched by `.gitignore:4` (`git check-ignore`
verified). They contain the VIN and MACs. No VIN appears in this document. Nothing was
committed.

---

## 7. Follow-on: two BEST/2 VM read tests (same day, third capture)

Run to close two items CLAUDE.md still lists as *"still pending on-car"*. Both are DDE-scoped
(`d72n47a0`, the one variant that resolves), both pure reads, both through the VM.

### 7.1 `run_job STATUS_MOTORTEMPERATUR` — 3-`xsend` dedicated dynamic job ✅ **CLOSED**

Wire (independently confirmed in the pcap, not just the VM's self-report):

```
2C 03 F3 03              → 6C 03 F3 03      clear stale define
2C 01 F3 03 4B C3 01 02  → 6C 01 F3 03      define DID 0x4BC3, pos 1, len 2
22 F3 03                 → 62 F3 03 0B EA   read
```

`0x0BEA` = 3050 → **31.86 °C**, `JOB_STATUS: OKAY`. Cross-validates against the 28 °C oil
temperature read minutes earlier on the same cold engine — physically consistent, so the
scaling is right, not merely well-formed. **Driving a 3-`xsend` dedicated job through the VM
is now proven on a car.**

### 7.2 `run_job STATUS_BLOCK_LESEN` — and a correction to a standing assumption

Args `3;JA;ARG;FanCtl_nSetPoint`. Wire:

```
2C 03 F3 03              → 6C 03 F3 03
2C 01 F3 03 4A 92 01 02  → 6C 01 F3 03      define DID 0x4A92
22 F3 03                 → 62 F3 03 7F FF
```

**This corrects the session-1 root-cause note.** That note concluded the `2C` define "lives in
44 dedicated `STATUS_<X>` jobs" and in `measurement.rs`, with generic jobs being static-only.
That is true of **`STATUS_LESEN` specifically**, not of generic jobs as a class:
`STATUS_BLOCK_LESEN` is generic and emits the full dynamic define correctly. The P2.1
misrouted-measurement guard is therefore scoped correctly, but the *explanation* attached to
it in `CLAUDE.md` overgeneralises and should be narrowed to `STATUS_LESEN`.

### 7.3 CP1252 `_INFO` decode ✅ **CLOSED**

`STAT_FanCtl_nSetPoint_INFO` returned **`Drehzahl des E-Luefters`** — clean, no mojibake. The
`pub klartext_sgbd::cp1252` fix is confirmed on real ECU data.

### 7.4 The VM's self-reported frames match the wire byte-for-byte

Every `_REQUEST_n` / `_RESPONSE_n` the VM returned is identical to the corresponding pcap
frame in both jobs. The VM's own record is trustworthy as evidence — useful, because it means
future job debugging does not always need a capture.

### 7.5 ~~NEW DEFECT — the `0x7FFF` sentinel is scaled as a real value~~ — WITHDRAWN 2026-08-04

**This was not a defect, and implementing the proposed fix would have been a regression.**
Investigated before changing any code; the evidence says klartext's answer is correct.

The `SG_FUNKTIONEN` row is:

```
FanCtl_nSetPoint | 0x4A92 | unsigned int | MUL 0.152590 | ADD -5000.000000 | rpm
```

An `unsigned int` raw over `0..65535` with that MUL and offset spans **−5000 … +4999.9 rpm** —
a symmetric range whose midpoint is `0x7FFF`. So `32767 × 0.152590 − 5000 = −0.083`, and
**−0.083 rpm is simply how this scaling encodes zero**. A fan setpoint of ~0 rpm with the engine
off is the physically correct reading, not a masked failure. 24 rows in this one DDE share the
same `ADD = -5000` symmetric shape.

Checked for a sentinel and found none at any layer:

- **`SG_FUNKTIONEN` has no sentinel column** (`ARG ID RESULTNAME INFO EINHEIT LABEL L/H DATENTYP
  NAME MUL DIV ADD SG_ADR SERVICE ARG_TABELLE RES_TABELLE`).
- **The reader bytecode does not test for one.** `STATUS_BLOCK_LESEN` is 32,146 ops with **zero**
  comparisons against `32767`/`32768`/`65535` (the 19 hits on `255` are byte masks) and no
  "nicht verfügbar"/"not available" string anywhere.
- **ISTA has no such handling either** — grepping the decompiled `RheingoldDiagnostics` for
  `0x7FFF`/`32767`/"not available" returns only unrelated log lines about services and data files.

The original claim ("`0x7FFF` is the classic EDIABAS not-available sentinel; ISTA shows 'signal
not available'") was asserted without evidence and does not hold for this measurement. Special-
casing `0x7FFF` would have **hidden a legitimate ~0 reading** on every measurement whose range
brackets it — the exact class of silent wrongness the finding was worried about, introduced by
the fix rather than removed by it.

*(Kept rather than deleted: the reasoning is the point. The original text follows.)*

### 7.5-original (superseded) — the `0x7FFF` "signal not available" sentinel is scaled as a real value

`62 F3 03 **7F FF**` (engine off, so the fan setpoint is genuinely unavailable) was scaled and
returned as:

```
STAT_FanCtl_nSetPoint_WERT = -0.08346999999957916
STAT_FanCtl_nSetPoint_EINH = rpm
```

`0x7FFF` is the classic EDIABAS not-available sentinel. ISTA shows "signal not available";
klartext emits a plausible-looking small negative rpm. **This is worse than an error** — a
consumer (including an LLM reading these tools) cannot tell it from a real measurement. The
semantic layer needs sentinel handling before measurement output can be trusted unattended.
Check what the SGBD declares for the sentinel rather than hardcoding `0x7FFF`.

### 7.6 What these tests did NOT close

**Structured multi-value (`RES_`) decode is still unproven on a car.** `STATUS_BLOCK_LESEN`
with a single `ARG` returns a single `WERT`/`EINH`/`INFO` triplet, not the multi-result shape.
The offline oracle's "DSC read decodes 40 results / 15 named stems" remains unconfirmed on
hardware — and the DSC path needs variant resolution (§8) before it can be attempted.

Read-only gate held throughout: `2F`/`2E`/`14`/`11`/`31`/`34`–`37` all **zero** in this capture.

---

## 8. The blocker that gates everything else: variant resolution on non-DDE ECUs

Asked whether to test the AC or the P3 oil-level flow. Both investigations landed on the same
wall, which is the most valuable finding of the day after §3.1.

### 8.1 The oil-level flow has nothing to run

The DDE's **entire** runnable catalog is three jobs:

```
STATUS_BLOCK_LESEN    STEUERN_E_LUEFTER    STEUERN_GLF
```

There is no `LERNWERTE_RUECKSETZEN` and no oil-level/`OELNIVEAU`/`OELSTAND` job for
`d72n47a0` — nor for any variant in `job_param`. The P3 spec's flagship *"guided oil-level
flow + the one gated `LERNWERTE_RUECKSETZEN` write"*
(`docs/superpowers/specs/2026-07-06-item5-guided-service-procedures-design.md` §6) **has no
catalog function on this ECU**. That target needs rescoping before more effort goes at it.

### 8.2 The AC test is blocked, and so is every other non-DDE write

IHKA (`0x78`) has exactly the kind of function that is confirmable by ear and hand — Blower
50%, Blower 100%, Defrost 100%, Ventilation 100%, Footwell 100%, Fresh/Recirculated air, A/C
compressor, water valves. None of it is reachable:

> need a `variant` for ECU 0x78 and none could be resolved (no explicit variant, no learned
> profile, and no single DB candidate with a matching .prg). Candidates: ihka01, ihka1rr,
> ihka20, … *(28 of them)*

All 28 candidates have a `.prg` on disk, so the ladder's third rung cannot discriminate. The
learned-profile rung only fills after a *successful scaled read*, which itself needs the
variant — a genuine chicken-and-egg. **The DDE works only because `d72n47a0` is passed by
hand.** 30 of 32 fitted ECUs on this car can do neither a scaled read nor any write.

### 8.3 The missing rung is the EDIABAS group SGBD — already on disk, unused

`--sgbd-dir` holds **1 832** files, of which **419** are group SGBDs with a `.grp` extension,
including exactly the ones needed here:

```
d_klima.grp  g_klima.grp  g_klima2.grp  g_klima3.grp      (IHKA 0x78)
g_lhm_l      g_lhm_r                                       (light modules 0x43/0x44)
d_mmi        g_mmi                                         (headunit 0x63)
```

These match the `group_name` column already stored in the semantic `ecu` table. Identifying an
ECU's variant by running the group SGBD's IDENT job is the standard EDIABAS mechanism, and
klartext never loads a `.grp` at all — the ladder is explicit → learned profile → single DB
candidate, with no IDENT rung.

**Verify before building:** disassemble `d_klima.grp` with `klartext-sgbd`/`klartext-best` and
read what its IDENT job actually does before implementing, per the parity mandate. Do not
assume the mechanism from the file names.

Adding that rung would unlock, in one change: scaled reads car-wide, the AC actuation test,
the C4 held-teardown test (§1 — still the only untested protocol item, and the side-light
functions at `0x43`/`0x44` are both held *and* visually confirmable), and the DSC structured
multi-value read left open in §7.6.

### 8.4 Recommended order

0. Route `read_fault_detail` by fault source (§3.6) — the one defect that makes a shipped
   read path return a guaranteed refusal. *(offline)*
1. Disassemble `d_klima.grp`; confirm the IDENT mechanism. *(offline, no car)*
2. Add the IDENT rung to the variant ladder. *(offline)*
3. Fix the `0x7FFF` sentinel (§7.5). *(offline)*
4. Settle `setflt` (§3.1) — decompile before implementing; it may be a fault-raise, not arithmetic. *(offline)*
5. One car session: AC blower actuation, C4 held teardown + `stop_service` + teardown-on-disconnect via the side lights, DSC structured multi-value read.
