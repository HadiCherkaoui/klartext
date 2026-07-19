# P2 — `STEUERN_KLEMMEN` clamp-switch payload

> **Research record, 2026-07-19. RESEARCH ONLY — no production code written, no commits.**
> Produced under the 1:1 ISTA parity mandate. Every claim carries a `file:line` or a bytecode
> offset. Resolves §G.3.1 of `docs/superpowers/specs/2026-07-18-research-p2-clear-sequence.md`.

**Citation shorthand**
- `KLEM:NNN` = `<scratchpad>/klemme/BMW.Rheingold.Module.ISTA/ABL_LIF_KLEMMENSTEUERUNG.cs:NNN`
- `CSV:NNN` = `<scratchpad>/decompiled/ClampSwitchVehicle.cs:NNN`
- `VI:NNNN` = `<scratchpad>/decompiled/VehicleIdent.cs:NNNN`
- `CAS@XXXXXX` = byte offset in `data/Testmodule(1)/Ecu/cas4_2.prg`, job `STEUERN_KLEMMEN`
- `FEM@XXXXXX` = same, `data/Testmodule(1)/Ecu/fem_20.prg`

Disassembly and execution used klartext's own `klartext-sgbd` + `klartext-best`
(`<scratchpad>/klemmprobe`, path deps on the repo crates; the repo was not modified).

---

## VERDICT: **PINNED**

Both payloads are pinned by three independent routes that agree exactly: static disassembly,
a precomputed table in `cas4_2.prg`, and a runtime CRC-8 algorithm in `fem_20.prg`/`bdc*.prg`.

| State | ISTA argument | UDS payload (7 bytes) |
|---|---|---|
| **KL15 OFF** | `KL30B_EIN` | **`31 01 10 01 06 06 A8`** |
| **KL15 ON** | `KL15_EIN` | **`31 01 10 01 0A 0A 43`** |

Target ECU address **`0x40`**, tester source `0xF1` — both literals in the bytecode
(`CAS@00054F`, `CAS@00055C`). On BMW-FAST the full telegram is `87 40 F1 31 01 10 01 06 06 A8`
(+ interface-appended checksum); over HSFZ the payload is the bare 7 UDS bytes with `TARGET=0x40`
in the HSFZ header.

**Identical on all four ECUs.** `cas4_2`, `fem_20`, `bdc`, `bdc_g11` each emit byte-for-byte the
same payload — verified by executing each SGBD's job in klartext's VM (§A.4). `bdc_g05.prg` has no
`STEUERN_KLEMMEN` job at all.

The three `FF` bytes are **binary, not decimal ASCII** (§A.3), and are patched by **indexed writes**
(`move S2[L1], B0`), not by rebuilding the string — both of the cautions in the assignment were the
live case.

The third byte is **a CRC-8 over the six preceding bytes**, not a third data value (§A.2).
**Consequence for any future implementation: the value byte cannot be swapped without recomputing
the CRC.** A hardcoded pair of literals is safe; a parameterised value is not.

---

## A. How the payload is built

### A.1 The template and the two table lookups

`CAS@000003  move S1, [31 01 10 01 FF FF FF]` → copied to the request buffer `S2` at `CAS@000012`.

Job argument 1 is mandatory (`CAS@000016 pars S1,#0x1`; `CAS@000036 jz → 000415` = `ERROR_ARGUMENT`,
`eoj`). It is looked up **by name** in two tables:

1. `CAS@0000AA tabset "TAB_CAS_KLEMMENSTATUS_ARG"` → `CAS@0000FE tabseek "TEXT", S1`
   → `CAS@0001AD tabget S4, "WERT"`. Miss → `ERROR_MODE`, `eoj` (`CAS@00015F`).
2. `CAS@000299 tabset "TAB_CAS_KLEMMENSTATUS_CRC"` → `CAS@0002ED tabseek "TEXT", S1`
   → `CAS@00039C tabget S4, "WERT"`. Miss → `ERROR_MODE`, `eoj` (`CAS@00034E`).

```
TAB_CAS_KLEMMENSTATUS_ARG          TAB_CAS_KLEMMENSTATUS_CRC
["4",   "KL30F_EIN"]               ["10",  "KL30F_EIN"]
["6",   "KL30B_EIN"]               ["168", "KL30B_EIN"]      <- 0xA8
["8",   "KLR_EIN"]                 ["225", "KLR_EIN"]
["10",  "KL15_EIN"]                ["67",  "KL15_EIN"]       <- 0x43
["16",  "KL30B_EIN_VERK"]          ["42",  "KL30B_EIN_VERK"]
["255", "ungültig"]                ["255", "ungültig"]
```

### A.2 The three placeholder bytes — indexed writes, not a rebuild

Each patch is the same five-op idiom: push the index, load the value's low byte, `move S2[L1], B0`.

| Byte | Ops | Value |
|---|---|---|
| `S2[4]` | `CAS@0001F9` (`L0=#0x4`) … `CAS@000218 move S2[L1], B0` | ARG `WERT` |
| `S2[5]` | `CAS@000220` (`L0=#0x5`) … `CAS@00023F move S2[L1], B0` | ARG `WERT` (**same value again**) |
| `S2[6]` | `CAS@0003E8` (`L0=#0x6`) … `CAS@000407 move S2[L1], B0` | CRC `WERT` |

The duplicate at index 5 is not a transcription error — it is two separate patch blocks writing the
same `S0[2]` scratch byte, and the executed job confirms it (§A.4).

**The third byte is a checksum.** `fem_20`/`bdc`/`bdc_g11` carry no `_CRC` table; they compute it:

- `FEM@000003 move S1, [00 1D 3A 27 74 69 4E 53 …]` — a **256-byte CRC-8 lookup table**
  (`table[1] = 0x1D` ⇒ polynomial `0x1D`), parked in `S2`.
- `FEM@00010F move L0, #0x8D` → `S0[1]` — the **accumulator seed `0x8D`**.
- `FEM@000417`–`FEM@0004F7` — loop `i = 0..5` (bound `#0x6` at `FEM@000417`):
  `FEM@00048E xor L0, L1` then `FEM@0004A6 move B0, S1[L1]` (indexing the table) ⇒
  `acc = table[acc ^ request[i]]`.
- `FEM@00051C move S3[L1], B0` with `L1 = 6` ⇒ `request[6] = acc`.

Running that algorithm against `cas4_2`'s precomputed table reproduces **all five real clamp states
exactly**:

| ARG text | value | CAS4_2 `_CRC` table | FEM runtime CRC | |
|---|---|---|---|---|
| `KL30F_EIN` | 4 | 10 | 10 | match |
| `KL30B_EIN` | 6 | **168** | **168** | match |
| `KL30B_EIN_VERK` | 16 | 42 | 42 | match |
| `KLR_EIN` | 8 | 225 | 225 | match |
| `KL15_EIN` | 10 | **67** | **67** | match |
| `ungültig` | 255 | 255 | 214 | sentinel row, not a command |

Two independently authored SGBDs agreeing on all five commandable states is the strongest offline
confirmation available without a capture.

### A.3 Binary, not ASCII — how we know

Three independent signals, any one of which settles it:

1. `CAS@0001D9 a2fix L0, S4` — `a2fix` parses the cell's ASCII decimal text into an **integer**
   (`crates/best/src/exec.rs:1439`). The ASCII text never reaches the buffer.
2. The write is `move S2[L1], B0` — `B0` is a **single byte** of the overlapping register file.
   Decimal ASCII `"168"` would need three bytes and a different opcode.
3. The transmitted length is fixed: `CAS@0004D4 slen I3, S2` yields **7**, taking the
   `comp I3,#0x3F / jle` short-form branch (`CAS@0004EC → 000503`) and producing format byte
   `0x80 + 7 = 0x87`. An ASCII payload would vary in length per state; it does not.

### A.4 Executed in klartext's own VM (the definitive proof)

`klartext_best::Ecu::run_job` with a recording `UdsExchange`:

```
cas4_2  KL30B_EIN -> telegram 87 40 F1 31 01 10 01 06 06 A8   UDS = 31 01 10 01 06 06 A8
cas4_2  KL15_EIN  -> telegram 87 40 F1 31 01 10 01 0A 0A 43   UDS = 31 01 10 01 0A 0A 43
fem_20  KL30B_EIN -> 31 01 10 01 06 06 A8      bdc      KL30B_EIN -> 31 01 10 01 06 06 A8
fem_20  KL15_EIN  -> 31 01 10 01 0A 0A 43      bdc      KL15_EIN  -> 31 01 10 01 0A 0A 43
                                               bdc_g11  both      -> identical
```

`cas4_2` runs to `eoj` with `JOB_STATUS = OKAY`. `fem_20`/`bdc`/`bdc_g11` hit the unimplemented
`waitex` opcode (`FEM@000A39`) **after** the `xsend`, in the post-response tail — the request is
fully emitted and unaffected. (`waitex` is a genuine gap in klartext's VM, listed in §E.)

---

## B. Does it need a session change or security access? — **NO**

**`STEUERN_KLEMMEN` is a plain UDS `0x31` RoutineControl, sent in the default session, with no
security access.**

- The job contains **exactly one `xsend`** (`CAS@00057A`). There is no other transmit in its 501 ops
  / 3 083 bytes; the job decodes end-to-end to a terminating `eoj`, so this is the whole job, not a
  truncation.
- No `10 xx` and no `27 xx` literal appears anywhere in the job. The only telegram literal is the
  `31 01 10 01 FF FF FF` template.
- The functional/broadcast flag is compiled to **off**: `CAS@000566 move B0,#0x0` →
  `CAS@00056E jz` skips `CAS@000574 adds S1[0],#0x40`. Physically addressed to `0x40`.
- EDIABAS's connect job `INITIALISIERUNG` in the same SGBD has **zero `xsend`** — it is
  `xconnect` + `xsetpar` (interface parameters only, `CAS-INIT@000000`, `CAS-INIT@0000CA`). So no
  session change happens at connect either.

**What else the test module does** (`ABL_LIF_KLEMMENSTEUERUNG`, complete inventory of
`EcuKomStatement.Execute` — 6× `STATUS_LESEN`, 2× `STEUERN_KLEMMEN`, 1× `STEUERN_ROUTINE`,
1× `STATUS_ELV` per ECU branch). On the F25's `CAS4_2` the full wire set is:

| Job / argument | UDS | Where | Class |
|---|---|---|---|
| `STATUS_LESEN` `ARG;STATUS_KLEMMEN` | `22 DC 56` | both directions, before + after | read |
| `STATUS_ELV` | `22 DA C3` | `Klemme_15_ein` only (`KLEM:663`) | read |
| `STEUERN_ROUTINE` `ARG;ELV_AKTION;STR;0` | `31 01 AA 7C 00` | `Klemme_15_ein` retry loop only (`KLEM:727`) | **write** — releases the electric steering lock |
| `STEUERN_KLEMMEN` `KL30B_EIN` | `31 01 10 01 06 06 A8` | `Klemme_15_aus` (`KLEM:1245`) | **write / actuation** |
| `STEUERN_KLEMMEN` `KL15_EIN` | `31 01 10 01 0A 0A 43` | `Klemme_15_ein` (`KLEM:788`) | **write / actuation** |

All frames pinned by execution in klartext's VM. Note the ON path is **not** a mirror of the OFF
path — it additionally reads and may command the ELV (steering lock). The `STR;8` ELV query is a
`BDC`/`FEM_20`-only branch; `CAS4_2` uses the read-only `STATUS_ELV` instead.

---

## C. How ISTA verifies the clamp actually switched

**Job:** `STATUS_LESEN` with argument `ARG;STATUS_KLEMMEN`, result `STAT_KLEMMENSTATUS`.
**Wire:** `22 DC 56` → `62 DC 56 <status>`.

Provenance: `cas4_2.prg` table `SG_FUNKTIONEN` row 23 —
`["STATUS_KLEMMEN", "0xDC56", "STAT_KLEMMENSTATUS", …, "unsigned char", "TAB_CAS_KLEMMENSTATUS",
… , "22", …]`. Confirmed by execution: `STATUS_LESEN ARG;STATUS_KLEMMEN` emits `22 DC 56`.

The status byte decodes through `TAB_CAS_KLEMMENSTATUS`: `0 INIT`, `2 KL30 alle Klemmen aus`,
`4 KL30F`, **`6 KL30B`**, `8 KLR`, **`10 KL15`**, `13 KL50`, `14 Fehler`, `15 Ungültig`.
So the guards `!= 6` and `!= 10` in the module are literally "is the clamp state KL30B / KL15 yet".

**The loop, per direction** (`Klemme_15_aus` shown; `Klemme_15_ein` is the mirror at `KLEM:692-869`):

1. `KLEM:1188` — read status (`22 DC 56`).
2. `KLEM:1215` — `if (status_klemme_l != 6)` — **already-there is a no-op**; if the car is already in
   KL30B nothing is transmitted at all.
3. `KLEM:1219` — `while (job_status_s != "OKAY" && count < 3)`: send `STEUERN_KLEMMEN`
   (`KLEM:1245`), `count++`, `Sleep(1000)` (`KLEM:1272`). **The retry is on the EDIABAS job status,
   not on the clamp status** — a job that answers `OKAY` is never retried even if the clamp did not
   move.
4. `KLEM:1301` — re-read status (`22 DC 56`).
5. `KLEM:1329` — `num = ((status_klemme_l == 6) ? 2 : 3)`.

**On failure ISTA does not force it — it asks the human.** `num == 3` dispatches to
`Manuelles_Klemmenschalten` (`KLEM:1398`, method at `KLEM:298`), which sets `manuell = true` and
opens the dialogs. That is
the only escalation; there is no harder command and no reset.

Additionally, `STEUERN_KLEMMEN` **returns the resulting clamp status itself**: the job reads response
byte 4 (`CAS@000A56`–`CAS@000A89`) into `STAT_CAS_KLEMMEN_STATUS` and resolves the text through the
same `TAB_CAS_KLEMMENSTATUS`. So the response to the actuation already carries the new state; the
follow-up `22 DC 56` is a second, independent confirmation.

---

## D. What happens if the sequence is interrupted between OFF and ON

**This splits into two questions. The first is proven. The second is NOT determinable from the
shipped data, and I am not going to reassure on it.**

### D.1 ISTA side — **PROVEN: there is no safety net**

**There is no `try`, no `catch`, no `finally`, and no `Dispose` anywhere in
`ABL_LIF_KLEMMENSTEUERUNG.cs`** — a grep for all of them across the 1 552-line file returns **zero
hits**. The ON step is reached purely by ordinary control flow:

```
Klemme_15_aus()            KLEM:954    commands KL30B_EIN
  └─ num = (status==6) ? 2 : 3         KLEM:1329
       └─ Rückgabe()                   KLEM:1395 (method at KLEM:1451)
            └─ count_kl = 2            KLEM:1462
                 └─ Klemmenwechsel()   KLEM:1403
                      └─ Sleep(15000)  KLEM:1427   <-- the 15-second window
                           └─ Klemme_15_ein()      KLEM:1446 -> commands KL15_EIN
```

Anything that stops that chain — an exception, a killed process, a pulled cable, a gateway
FIN+RST — stops it **with terminal 15 down and nothing scheduled to raise it.** The absence of
cleanup is confirmed at all three levels above the module too:

- `CallTestModuleForClampSwitch` (`CSV:197-233`) — no try/catch; an exception from
  `executionOfTestModule()` propagates.
- `DoClampSwitch` (`CSV:65-100`) — no try/catch around the automatic branch. (The `try/catch` at
  `CSV:104` is inside `ManualClampSwitch`, the *manual* path, which is only reached on failure and
  where the human is physically operating the ignition.)
- `ClearAndReadErrorInfoMemory` (`VI:9646-9666`) — no try/catch/finally around the clamp switch. The
  one handled case, `if (clampSwitchVehicle.Canceled) return;` (`VI:9656`), **also returns without
  restoring KL15**.

So: on the tester side, an interrupted sequence leaves terminal 15 down. That is not an inference.

### D.2 Car side — **NOT DETERMINABLE from the shipped data**

Whether the CAS re-raises KL15 on its own is ECU firmware behaviour. The `.prg` documents what to
send and how to read the answer; it does not describe the ECU's state-machine recovery policy. **No
shipped artifact states it.** I could not find one, and I will not infer one.

What the data *does* establish, as context and nothing more:

- **The commanded state is a normal resting state, not a fault state.** `TAB_CAS_KLEMMENSTATUS`
  entry 6 is plain `KL30B` — the same clamp state a parked, awake car sits in. It is not an
  "override" or "diagnostic" state; entry 14 (`Fehler`) and 15 (`Ungültig`) are separate.
- **The diagnostic request is one peer trigger among ten**, per `TAB_CAS_KLEMMEN_TRIGGER`:
  `0 Start-Stop-Taster (SST)`, `1 Telestart-Handsender`, `2 Motor-Start-Automatik`,
  `3 Obere Startfähigkeitsgrenze`, `4 ZV sichern`, `5 Fahrertür auf/zu (KL15-Abschaltung)`,
  `6 Timeout nach 16 min`, **`7 Diagnoseanforderung`**, `8 Relais-Kleber KL50`,
  `9 Waschstrassen-Modus Timeout 30min/15min`. The start/stop button is trigger 0 in the *same*
  state machine that trigger 7 drives.
- **There is no "release" or "hand back control" argument.** `TAB_CAS_KLEMMENSTATUS_ARG` holds only
  the five state requests plus the `ungültig` sentinel — consistent with a state *request* rather
  than a latched seizure, but not proof of one.
- **The CAS enforces its own preconditions.** `TAB_CAS_KLEMMEN_VERHINDERER` lists 16 documented
  refusal reasons including `3 Geschwindigkeit Fahren erkannt`, `9 Einschaltverhinderung durch
  Motorsteuerung: Motorlauf erkannt`, `10 Kraftschluss erkannt (P oder N nicht eingelegt)`,
  `14 ELV ist nicht entriegelt`. The ECU is not a dumb actuator.
- **An automatic KL15 shutdown exists and is separately suppressible.**
  `TAB_CAS_UW_ABSCHALTVERHINDERER_KL15` entry 103 is `Autom. KL15-Abschaltung per Diagnose
  deaktiviert`, entry 106 `OBD-Kommunikation aktiv`; `SG_FUNKTIONEN` `CAS_MONTAGEMODUS` (`0xDAB9`)
  documents a Präsentations-Modus that "deaktiviert einige Funktionen in der Klemmen-Steuerung
  (z.B. KL15-Abschaltung)".

**What that does and does not add up to.** It is consistent with the car recovering through ordinary
means (start button, door, timeout). It is **not** evidence that it does. None of these entries
describes what the CAS does with a *commanded* KL30B state when the tester vanishes, and I cannot
rule out that the state persists until an explicit trigger — i.e. that the owner would have to press
start, open a door, or wait, rather than the car returning on its own. That may be a minor
inconvenience or it may not; the data does not say, so **do not tell the owner the car "just
recovers".**

**What would settle it**, in increasing order of cost:

1. A capture of ISTA performing a clamp switch that is then aborted (the direct evidence; also the
   only route that shows the real timing).
2. The CAS4 functional specification or firmware — not present in `data/`, and not something the
   shipped ISTA stack contains.
3. An on-car experiment. This is the owner's call, not ours, and it should be framed honestly: it
   means deliberately putting his own car into the interrupted state to find out. Preconditions
   would matter (stationary, engine off, P engaged, key present, battery healthy) and the failure
   mode to plan for is "the car needs a key/start-button cycle", not "the car is bricked".

### D.3 Bearing on the P2 recommendation

This does not change §F.6 of the clear-sequence spec: **the clamp cycle should still not ship in
P2.1.** The payload being pinned removes the "we don't know the bytes" blocker, but D.2 leaves the
more important blocker standing — we cannot yet state the interrupted-state consequence to the owner
in his own words. If it is ever built, it wants: `Policy::ConfirmedWrite`, the `22 DC 56` pre-read
(so an already-KL30B car is a no-op exactly as ISTA does), the ISTA retry shape (3× / 1 s), the
15-second window surfaced as a live countdown, and an explicit statement of what an abort leaves
behind.

---

## E. Where the data was unreadable, and known gaps

1. **CAS4 firmware behaviour after an interrupted diagnostic clamp request** (§D.2) — not present in
   any shipped artifact. Named, not reconstructed.
2. **`waitex` (`0xAE`) is unimplemented in klartext's BEST/2 VM.** It sits in the post-response tail
   of `fem_20`/`bdc`/`bdc_g11` `STEUERN_KLEMMEN` (`FEM@000A39`) and aborts the run *after* the
   `xsend`. It did not affect the payload — but it is a real gap in the VM and `cas4_2` only avoids
   it by not using the opcode. Worth a separate ticket; it is unrelated to this question.
3. **`XEnet32/64.dll` remain native PE** and still own the interface-appended checksum, TesterPresent
   cadence, and NRC `0x78` retry timing. Unchanged from the prior record; nothing here depended on
   them.
4. **The ELV (steering-lock) sub-flow of `Klemme_15_ein` was mapped but not fully traced.** The
   frames are pinned (`22 DA C3`, `31 01 AA 7C 00`) and the guard structure is read
   (`KLEM:692-757`), but the semantics of `STAT_ELV_VORHANDEN` / `STAT_ELV_ZUSTAND` and the exact
   condition under which the unlock is commanded were out of scope for this assignment. **If the
   clamp cycle is ever implemented, that sub-flow needs its own pass** — it commands the steering
   lock, which is a heavier actuation than the clamp itself.
