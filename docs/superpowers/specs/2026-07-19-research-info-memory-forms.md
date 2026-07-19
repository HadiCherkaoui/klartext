# P2 — Info memory (`IS_LESEN`) has three wire forms; klartext implements one

Research only. No production code was written. All claims below are cited to a decompiled
`file:line` or a bytecode offset that can be reproduced with the probe described in §0.

**Headline:** klartext hand-builds `22 20 00` and therefore reads nothing on the ~43% of
info-memory-capable ECUs that use `19 17 0C 01`. **Neither of the owner's two cars is
affected** — every confirmed ECU on both is a `22 20 00` ECU. The defect is real but it is
a defect for *other people's* cars and for future G-series work, and it **cannot be
validated on his hardware**. Recommendation is §D: run the job through the BEST/2 VM.

---

## 0. Reproducing

Probe: `<scratchpad>/p2im`
(cargo bin, path deps on `klartext-best` + `klartext-sgbd`; the repo was not modified).

```
p2im sweep <ecu-dir> <job>   # classify every .prg's wire form
p2im dump  <prg> <job>       # full disassembly
p2im hdr   <ecu-dir> <job>   # fleet-wide (header/stride) from the count arithmetic
p2im jobs  <prg>             # job directory
```

SGBDs: `data/Testmodule(1)/Ecu/*.prg` (1,405 files; 887 define `IS_LESEN`).
Decompiled ISTA: `<scratchpad>/decompiled/VehicleIdent.cs`.

---

## A. Which ECUs use which form — and can klartext know in advance?

### A.1 Fleet distribution (887 SGBDs defining `IS_LESEN`)

| Wire form | SGBDs | Era (from the naming ladder) |
|---|---:|---|
| `22 20 00` (UDS ReadDataByIdentifier) | 334 | F-series |
| `19 17 0C 01` (UDS ReadDTCInformation, user-def memory) | 255 | G / I-series |
| no literal — request assembled byte-by-byte at run time | 267 | E-series |
| DS2-era raw telegrams (`xx 05 14 xx`) | 29 | pre-UDS |
| open/decode error | 2 | — |

The split is **generational, not by bus, supplier, or address.** The cleanest evidence is
the ladder *inside a single ECU family* — same part, successive generations:

```
acsm2  → dynamic       acsm3/4/4i/5 → 22 20 00      acsm6/7 → 19 17 0C 01
bfh_65_2, bfh_plx → dynamic    bfh_01/12/g11 → 22 20 00     bfh_i20 → 19 17 0C 01
dsc_60/85/87/e65 → (no job)    dsc_01/10/25/g11/g20 → 22 20 00   dsc_g30n, ib_g70/i20 → 19 17
zgw_01 → 22 20 00                                        zgw_sp18, bcp_sp21 → 19 17 0C 01
```

Within a form the job is **byte-identical across ECUs** — a shipped framework job, not
per-ECU code. Op counts cluster hard: `22 20 00` → 1095 ops (197 SGBDs) / 1012 (135);
`19 17 0C 01` → 1484 (163) / 1377 (91). This matches the known finding that the generic
`STATUS_LESEN`/`STEUERN_*` framework jobs are byte-identical across F20 ECUs.

### A.2 Address is NOT a usable predictor — and a heuristic would misfire on his gateway

The ISTA DB maps one diagnostic address to many candidate variants fleet-wide. Counting
candidates per address for the F25_1404 bordnet (`ecu_tree` ⋈ `ecu` ⋈ sweep):

| addr | ECU | candidates | `22 20 00` | `19 17` | dynamic |
|---:|---|---:|---:|---:|---:|
| 0x00 | JBBF | 14 | 1 | 0 | 6 |
| 0x01 | AIRBAG | 14 | 4 | 2 | 8 |
| **0x10** | **ZGW** | **5** | **1** | **4** | 0 |
| 0x12 | DME/DDE | 64 | 26 | 6 | 32 |
| 0x1C | ICMQL | 8 | 2 | 3 | 3 |
| 0x29 | DSC | 14 | 8 | 6 | 0 |
| 0x40 | CAS | 11 | 6 | 3 | 2 |
| 0x60 | KOMBI | 22 | 15 | 3 | 4 |
| 0x63 | HEADUNIT | 28 | 13 | 4 | 11 |
| 0x67 | ZBE | 13 | 6 | 1 | 6 |
| 0x78 | IHKA | 12 | 11 | 1 | 0 |

Every address is ambiguous. Note **address 0x10 (the ZGW)**: 4 of its 5 candidates are
`19 17`, but the variant actually fitted to his car — `zgw_01` — is `22 20 00`. **A
majority-vote or "newest wins" heuristic would send the wrong frame to the owner's own
gateway.** Prediction from address is not merely imprecise, it is actively wrong.

### A.3 The right framing: it is a lookup, not a prediction

The form is a **static per-variant fact stored in the `.prg` klartext already loads.**
`d72n47a0.prg` offset `0x000019` is literally `move S1, {22 20 00}`; `acsm7.prg` offset
`0x000019` is `move S1, {19 17 0C 01}`. Nothing needs to be inferred or probed — once the
*variant* is known, the answer is a read.

So the prerequisite is variant resolution, which klartext already has (the M10 ladder:
explicit → learned per-VIN profile → single DB candidate).

**How ISTA gets the variant:** it passes the **group** SGBD, not the variant —
`apiJob(text, "IS_LESEN", …)` where `text` comes from `mECU.ECU_GRUPPE.Split('|')`
(`VehicleIdent.cs:3600-3603`). Group SGBDs are `.grp` files, and I verified they contain
**only** `INITIALISIERUNG` and `IDENTIFIKATION` (`p2im jobs g_motor.grp`, `g_cas.grp`) —
no `IS_LESEN`. EDIABAS resolves group → variant via `IDENTIFIKATION`, then runs `IS_LESEN`
from the variant `.prg`. So the variant `.prg` is the thing that executes, and the
per-variant analysis above is the correct one.

### A.4 The practically important answer — the owner's two cars

**Both cars are `22 20 00` throughout. klartext reads their info memory with the correct
frame today.**

Variants confirmed on the wire in car session 1 / the F20 captures:

| ECU | addr | variant | form | ops |
|---|---:|---|---|---:|
| DDE (N47, both cars) | 0x12 | `d72n47a0` | `22 20 00` | 1095 |
| CAS4 (F25) | 0x40 | `cas4_2` | `22 20 00` | 1095 |
| ZGW (gateway) | 0x10 | `zgw_01` | `22 20 00` | 1095 |

All three are the same 1095-op byte-identical framework job. For the remaining ECUs the
variants are not individually confirmed, but every `19 17` candidate sitting at those
addresses is a G/I-series part that cannot be fitted to a 2017 F25 or an F20 — `bdc_g05`,
`ib_g70`, `ib_i20`, `hu_mgu`, `idcevo25`, `acsm6/7`, `zgw_sp18`, `kombsp18/21`.

**Consequence for prioritisation:** this fix buys the owner nothing on his own cars, and
the `19 17` path **cannot be tested on his hardware**. It is correctness work for arbitrary
cars — which is squarely the standing "must work on ANY car plugged in" principle — but it
should be scheduled honestly as such, and it will ship unvalidated on the wire.

---

## B. The `19 17` response layout — verified from bytecode, fleet-wide

### B.1 Request

Literal, read directly from the bytecode (not inferred): `19 17 0C 01`
= SID `0x19` ReadDTCInformation, sub-function `0x17` reportUserDefMemoryDTCByStatusMask,
status mask `0x0C`, MemorySelection `0x01`. Mask `0x0C` is the same one `FS_LESEN` uses.

The `19 17` job additionally reads job parameter 1 (`pars S1, 0x1` at `acsm7.prg` `0x000125`
and `0x000150`) and branches on its presence; the `22 20 00` job has no `pars` at all.
**ISTA passes no argument** — `apiJob(text, "IS_LESEN", string.Empty, string.Empty, …)`
(`VehicleIdent.cs:3603`). Pass empty args to match.

### B.2 Count arithmetic — the only layout difference

`d72n47a0.prg` `IS_LESEN` (`22 20 00` form):

```
0x000563  slen L0, S3        ; L0 = len(bare UDS payload)
0x000573  move L0, 0x3       ; → L1 = 3        HEADER
0x000583  subb L0, L1        ; L0 = len - 3
0x00058A  move L0, 0x4       ; → L1 = 4        STRIDE
0x00059A  divs L0, L1        ; count = (len - 3) / 4
```

`acsm7.prg` `IS_LESEN` (`19 17 0C 01` form):

```
0x00082C  slen L0, S3
0x00083C  move L0, 0x4       ; → L1 = 4        HEADER   ← the only change
0x00084C  subb L0, L1        ; L0 = len - 4
0x000853  move L0, 0x4       ; → L1 = 4        STRIDE
0x000863  divs L0, L1        ; count = (len - 4) / 4
```

**Verified fleet-wide, not from two samples.** `p2im hdr` walks every `divs` in all 887
`IS_LESEN` jobs and reports the two preceding `L0` immediates:

```
334  22 20 00     => 3/4
255  19 17 0C 01  => 4/4
```

Zero exceptions in either group.

### B.3 Record shape — unchanged

Both forms slice a **4-byte record** and emit it whole as `F_HEX_CODE`:

```
d72n47a0 0x0009CE   move S4, S1[L0 .. len L1]   with L1 = 4   (0x0009C7 move L1, 0x4)
acsm7    0x001383   move S4, S1[L0 .. len L1]   with L1 = 4   (0x00137C move L1, 0x4)
```

Record base = `header + i*4`, computed through the shared signed-multiply helper
(`0x000647` / `0x0009A8` `mult`). Layout is `[3-byte location code][1-byte ISO status]` —
**identical to what `decode_info_memory` already parses.** Only the header length changes.

### B.4 Honest limit — the header *contents* are NOT verified

The brief asked whether ISO 14229's `[subfn][MemorySelection][statusAvailabilityMask]` echo
holds. **The bytecode cannot answer that, and I am not going to assert it.**

After the transport helper strips the BMW-FAST frame (`serase S3[0], B30` at `acsm7`
`0x0004B9`, `B30` ∈ {3,4,6}) and the trailing checksum, the job reads exactly one byte of
the bare UDS payload: `S3[0]`, compared against `request[0] + 0x40` (`0x0005B4-0x0005BE`)
and against `0x7F` for the negative-response path (`0x000521`). **Offsets 1..3 are never
read, never validated, never emitted.** The bytecode only *skips* them.

So: header **length = 4 is verified**; header **semantics are unverified**. A 4-byte header
is consistent with ISO's echo, but the field order is an assumption the data does not
support. This does not block implementation — klartext, like the bytecode, only needs to
skip 4 bytes — but the constant must be documented as "length verified, contents opaque"
and the raw payload must stay surfaced for the eventual capture.

### B.5 The named result set is IDENTICAL across both forms

Extracted from the `erg*` opcodes in both disassemblies:

```
F_VERSION  JOB_STATUS  JOB_MESSAGE  _REQUEST  _RESPONSE
F_ORT_NR  F_ORT_TEXT  F_HEX_CODE  F_STATUSBYTE  F_EREIGNIS_DTC
F_READY_NR  F_READY_TEXT   F_VORHANDEN_NR  F_VORHANDEN_TEXT
F_WARNUNG_NR  F_WARNUNG_TEXT
```

Byte-for-byte the same set, same names, in both generations. **This is the single most
important fact in this report** — it is precisely why ISTA can be blind to the wire form,
and it is the load-bearing argument for §D.

Note klartext's hand-decoder produces only `code + status`. The job produces fourteen named
results including `F_ORT_TEXT` and the three status-flag pairs. Even on `22 20 00` ECUs
where klartext's frame is *correct*, it is surfacing materially less than ISTA does.

### B.6 `IS_LESEN_DETAIL`, for completeness

| Wire form | SGBDs |
|---|---:|
| `22 20 00` (detail via `22 20 nn`) | 334 |
| `19 09 FF FF FF` + `19 18 FF FF FF FF 01` + `19 19 FF FF FF FF 01` | 163 |
| `19 18 FF FF FF FF 01` + `19 19 FF FF FF FF 01` | 92 |

The 163/92 split lines up exactly with the 1484/1377 op-count split in §A.1 — the two
`19 17` sub-generations differ by whether they also read severity (`19 09`).
ISTA calls it per entry with the location as the argument:
`apiJob(sg.ECU_SGBD, "IS_LESEN_DETAIL", item.F_ORT.ToString(), …)` (`VehicleIdent.cs:3766`)
— and note this one is called on `ECU_SGBD` (the **variant**), not the group.
klartext does not implement `IS_LESEN_DETAIL` at all today; it is out of scope here.

---

## C. What ISTA does — it never sees the wire form

**ISTA does not branch on the form. It calls the job and reads named results.** There is no
form-selection logic anywhere in the path, because there is nothing to select — see §B.5.

Two entry points, both in `VehicleIdent.cs`, called as a pair (`:453-454`, `:6177-6180`,
`:7929-7930`, and six more sites):

**1. Functional/broadcast first — `DoECUReadIS` (`VehicleIdent.cs:3443`)**

```csharp
IEcuJob ecuJob = ecuKom.apiJob(groupSgbd, "IS_LESEN_FUNKTIONAL", string.Empty, string.Empty,
                              retries, "", null, "DoECUReadIS");          // :3462
```

One broadcast job on `VecInfo.MainSeriesSgbd`; each result set is one ECU, demultiplexed by
the named result `ID_SG_ADR` (`:3471`). Reads `JOB_STATUS`, `F_ANZ` (entry count, via
`getResultFormat` — short or int, `:3494`), then per entry `F_ORT{i}_NR` / `F_ART{i}_NR`
(`:3523-3524`), gated on `F_ORT.HasValue` (`:3527`), plus `F_HEX_CODE`.

`IS_LESEN_FUNKTIONAL` exists in only **8** SGBDs — the group/main-series ones. Two of them,
`f01.prg` and `x_x001.prg`, contain **both** `22 20 00` and `19 17 0C 01` literals in the
*same* job: the broadcast job itself branches at run time across a mixed-generation
vehicle. No caller could hoist that decision even in principle.

**2. Physical per-ECU fallback — `doECUReadIS` (`VehicleIdent.cs:3570`)**

For every ECU where `IS_SUCCESSFULLY == false` after the broadcast (`:3553`):

```csharp
string[] array = mECU.ECU_GRUPPE.Split(new char[1] { '|' });               // :3600
foreach (string text in array) {
    ecuJob = ecuKom.apiJob(text, "IS_LESEN", string.Empty, string.Empty, …); // :3603
```

Each result set is one entry. Reads `F_ART_NR`, `F_ORT_NR` (`:3623-3624`), and on the
`!F_ART.HasValue` / DS2 branch also `F_ORT_TEXT` (`:3631`), `F_HFK`, `F_LZ`, `F_CLA`,
`F_UW_KM`, `F_KM_LAST`, `F_UW_ZEIT`, `F_UW_ANZ`, then `F_ART_ANZ` (`:3670`) and
`F_ART{k}_TEXT/NR` (`:3680`). The gate for keeping an entry is `dTC.F_ORT.HasValue`
(`:3696`), then `F_HEX_CODE` (`:3698`) + `F_ORT_TEXT` (`:3699`). Entries land in `eCU.INFO`
and are cross-checked into `FEHLER` via
`PlausibilityCheck.HandleRelevanceFlagAndFaultCodeForDTC`.

**Two further parity behaviours worth recording:**

- **Skip-if-ident-failed.** `DoEcuJob(vecInfo, mECU, "IS_LESEN")` (`:3596`) returns false —
  and `doECUReadIS` returns without reading — when the vehicle is `BNType.BN2020` and
  `ecu.IDENT_SUCCESSFULLY` is false (`:5510-5519`). ISTA does **not** read info memory from
  an ECU whose identification failed. Directly relevant to the ~10/32 ECUs that timed out in
  car session 1.
- **Hard-coded skip list.** `MFL2`, `MFLR`, `MFLR50` are marked successful and skipped
  without any read (`:3589-3593`).

**Architecture this implies:** unambiguous. ISTA's contract with the ECU is *the job and its
named results*, never the frame. The wire form is an implementation detail owned by the
ECU's own bytecode, and the result-set stability across the `22 20 00` → `19 17` transition
(§B.5) is what makes that contract hold across a decade of hardware. **A klartext design
that hand-builds frames is re-implementing, by hand and per generation, the exact thing BMW
put in the SGBD so that nobody would have to.**

---

## D. Recommendation — option 2, run the job through the BEST/2 VM

**Pick option 2, with a narrow fallback retained.** Reasoning, then the caveat that matters.

### Why option 2

1. **It is what ISTA does** (§C), which is the standing mandate.
2. **It fixes all three forms at once**, including the 267 dynamic E-series SGBDs that
   options 1 and 3 cannot address at all — those build their request byte-by-byte at run
   time and have no literal to copy or probe for. Option 1 would leave ~30% of the fleet
   permanently unreadable; option 2 covers them for free.
3. **It closes a parity gap options 1 and 3 do not even touch** — the fourteen named
   results (§B.5) vs klartext's `code + status`, on *every* ECU including the `22 20 00`
   ones that already "work".
4. **klartext already has every piece.** Nothing new is invented:
   - variant resolution — the M10 ladder,
   - `.prg` loading — `mcp/src/server.rs:148` `sgbd_path`,
   - execution — `crates/best/src/engine.rs:162` `Ecu::run_job`,
   - the transmit-seam gate — `GatedExchange::read_only`, and **`0x19` already classifies
     as `SidClass::Pass`** (`crates/best/src/gate.rs:17-18`, `:71`). **No gate change is
     needed** for the `19 17` form.
5. **The known constraint does not apply here — verified, not assumed.** The
   `misrouted_dynamic_measurement` finding (`crates/semantic/src/measurement.rs:424`) arose
   because `STATUS_LESEN` is **data-driven**: it reads `SG_FUNKTIONEN.SERVICE` from a table
   and emits whatever DID that row names, which is wrong for a `2C`-define row. `IS_LESEN`
   has **no such indirection** — its request is a hard literal at offset `0x000019` in both
   forms, with no table lookup anywhere in the request path. There is nothing to misroute.
   This is the specific check that made the difference, so it is worth stating plainly:
   *the prior failure mode was table-driven request construction, and `IS_LESEN` does not do
   that.*

### The caveat — do not make this a hard dependency

`read_info_memory` today is pure UDS and works with **no `--sgbd-dir` and no resolved
variant**. Routing it through the VM would make a currently-working read depend on both.
That is a robustness regression on the F-series path — including both of the owner's cars.

So: **VM primary, hand-built frame as fallback.**

- variant resolved **and** `.prg` available → `run_job("IS_LESEN")`, surface named results.
- otherwise → the existing hand-built path, choosing the form from the variant's `.prg`
  literal when the `.prg` is readable, else defaulting to `22 20 00`.

The fallback must therefore still learn `19 17` — this is not "option 2 instead of option
1", it is option 2 *in front of* a corrected option 1. Cost is small: one extra constant and
a header length that is already proven fleet-wide (§B.2).

### Why not the others

- **Option 1 alone** — needs a predictor, and §A.2 shows any address-based heuristic sends
  the wrong frame to his own gateway. Predicting from the *variant* is fine, but at that
  point you are already holding the `.prg`, so option 2 costs no more information and
  delivers strictly more. Leaves the 267 dynamic SGBDs unreachable forever.
- **Option 3 (probe)** — two round trips per ECU on a car where ~10/32 ECUs already time
  out under sweep pressure; ISTA never probes; and the fallback still has to guess for the
  dynamic 267. It also burns a request to learn something already sitting in a file on
  disk. Reject.

### Files touched

**Option 2 (recommended):**
- `mcp/src/server.rs:419` `read_info_memory` — route through the VM when variant + `.prg`
  resolve (mirror the `run_job` composition at `:1066-1069`), fall back otherwise.
- `mcp/src/dto.rs:132` `InfoMemoryResult` — carry named results, not just code+status.
- `crates/client/src/client.rs:249` `read_info_memory` — keep as the fallback path.
- `crates/uds/src/dtc.rs:345` `decode_info_memory` + `:305` `INFO_RECORD_LEN` — add the
  header-length parameter for the fallback's `19 17` support.
- `crates/uds/src/service.rs:42` `did::INFO_MEMORY` — document that `22 2000` is one of
  three forms and is generation-scoped.
- No change to `crates/best/src/gate.rs` (`0x19` already passes).

**Option 1 only (if option 2 is rejected):** the two `crates/uds` files above plus
`crates/client/src/client.rs:249`, and a new form-selection input threaded from the
variant — with §A.2's warning that address-based selection is wrong.

---

## E. Risks

**`19 17` is a pure read — confirmed three ways.** ISO 14229 service `0x19`
ReadDTCInformation has no write semantics in any sub-function; sub-function `0x17`
reportUserDefMemoryDTCByStatusMask reads a DTC list. klartext's own gate already classifies
`0x19` as `SidClass::Pass` alongside `0x22`/`0x2C` (`crates/best/src/gate.rs:17-18`, `:71`).
And the bytecode does exactly one `xsend` (`acsm7` `0x00042B`) with no session change, no
`0x27` security access, no `0x2E`/`0x31`/`0x2F` anywhere in the job.

**Worst case of sending `19 17 0C 01` to an ECU that does not support it:** NRC `0x12`
(subFunctionNotSupported) or `0x31` (requestOutOfRange) — the same class of outcome
klartext already handles today when `22 2000` is rejected (`request_optional` → `None` →
`supported=false`). Benign, and no worse than the current behaviour.

**Cannot be verified without the car — do not claim otherwise:**

1. That a real `19 17` response carries a 4-byte header on the wire. The bytecode says
   *skip 4*; no capture of this form exists anywhere in the repo.
2. The semantic content of those 4 header bytes (§B.4) — unreadable from bytecode by
   construction.
3. **That any ECU on the owner's cars answers `19 17` at all — none should.** Both cars are
   `22 20 00` throughout (§A.4), so **this code path is untestable on his hardware.**
   Validating it needs a G-series or I-series car. Any implementation ships unproven on the
   wire and must be labelled `[verify against capture]`, exactly as `22 2000` is today.
4. CP1252 rendering of `F_ORT_TEXT` / `_INFO` through the VM path (the known `_INFO`
   mojibake class of bug, already fixed once in `klartext_sgbd::cp1252`).

**One behavioural risk in option 2 worth flagging to the implementer:** moving from a
single hand-built request to a full job execution changes the traffic profile per ECU
(the `19 17` job is 1377-1484 ops with retry/`wait` handling). On a car where ~10/32 ECUs
already time out under a rapid sweep, measure the per-ECU wall clock before adopting it for
`read_all_*`-style batch paths.

---

## F. Concrete change list

1. **Do not ship an address-based form predictor.** §A.2 — it sends the wrong frame to the
   owner's own gateway. Form selection keys off the *variant*, or is delegated to the VM.
2. **Route `read_info_memory` through `run_job("IS_LESEN")`** when the variant resolves and
   the `.prg` is present (`mcp/src/server.rs:419`, composing as `:1066-1069` does). Pass
   **empty args** — ISTA does (`VehicleIdent.cs:3603`).
3. **Surface the full named result set** (§B.5) — `F_ORT_NR`, `F_ORT_TEXT`, `F_HEX_CODE`,
   `F_STATUSBYTE`, `F_EREIGNIS_DTC`, `F_READY_NR/TEXT`, `F_VORHANDEN_NR/TEXT`,
   `F_WARNUNG_NR/TEXT`, `JOB_STATUS` — in `mcp/src/dto.rs:132`. This is a parity gain on
   *every* ECU, `22 20 00` ones included.
4. **Keep the hand-built path as fallback** and teach it `19 17`: header 4, stride 4,
   `count = (len-4)/4`, record `[3-byte code][1-byte status]`
   (`crates/uds/src/dtc.rs:345`, `:305`). Header length is fleet-verified; document its
   *contents* as opaque and keep `raw` surfaced.
5. **Do not add a probe/fallback round trip** (option 3) — §D.
6. **Adopt ISTA's skip-if-ident-failed rule** (`VehicleIdent.cs:5510-5519`): do not read
   info memory from an ECU whose identification failed. Cheap, and it directly reduces the
   car-session-1 timeout burn.
7. **Mark the `19 17` path `[verify against capture]`** and state in the tool description
   that it is unvalidated on hardware — neither of the owner's cars can exercise it (§A.4).
8. **Out of scope here:** `IS_LESEN_DETAIL` (§B.6), and the 29 DS2-era SGBDs.

---

## G. What proved unreadable

- **The semantic content of the `19 17` 4-byte response header** (§B.4). Not a tooling
  limit — the bytecode genuinely never reads those bytes, so the information is not present
  in the artefact. Only a wire capture from a G/I-series car can settle it.
- **How EDIABAS resolves group → variant at run time.** The `.grp` files' `IDENTIFIKATION`
  bytecode is readable, but the dispatch that consumes its result lives in
  `XEnet32/64.dll` — native PE, already recorded as unreadable in the parity audit. I
  confirmed only the *structure* (`.grp` holds `INITIALISIERUNG` + `IDENTIFIKATION` and no
  `IS_LESEN`), not the dispatch algorithm. klartext does not need it — its M10 ladder
  resolves the variant independently.
- **The 267 dynamic-request SGBDs' actual emitted bytes.** Determining them statically
  would mean symbolically executing each job; I classified them by absence of a literal and
  by generation only. This does not affect the recommendation (option 2 runs them without
  needing to know), but it does mean I cannot state what an E-series `IS_LESEN` puts on the
  wire.
