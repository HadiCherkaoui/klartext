# Car session 3 — results (2026-08-15, F25 X3)

Ran `docs/on-car-test-protocol-2026-08-04.md` end to end against the F25 (gateway
`169.254.71.121`), tcpdump capturing every frame: **4,096 frames / 971 s**, pcap
`/tmp/klartext-oncar-1844.pcapng` (holds the VIN — never commit it).

Everything in Phases A–C was verified byte-for-byte against the capture. Phases D and E
were run with the owner present and each step explicitly authorised.

## Verdicts

| # | Check | Verdict |
|---|---|---|
| A1 | Repair documents render | **PASS** for REP/SWZ/FUB; see D-4 for EBO |
| A2 | Fault → test plan, scoped vs unscoped | **PASS** |
| A3 | Symptom side | **PASS** |
| A4 | FA decoder against this car | **PASS** |
| B1 | Info memory carries BMW's own text | **PASS** |
| B2 | Whole-car sweep says the same thing | **PARTIAL** — see D-1 |
| B3 | Virtual faults for a silent ECU | **N/A** — no ECU failed |
| B4 | Fault detail routes by store | **PASS** |
| B5 | Variant resolution without passing one | **FAIL** — see D-1 |
| B6 | `IDENT_FUNKTIONAL` capture | **PASS** — question settled, see below |
| B7 | Nothing forbidden on the wire | **PASS** |
| C | Regressions | **PASS** |
| D1 | Single-ECU clear, no `10 03` | **PASS** |
| D2 | Whole-vehicle clear + clamp cycle | **PASS** (one open issue, D-2) |
| E1 | Electric fan actuation (`setflt` fix) | **PASS** — wire + fan physically spun (owner-confirmed) |
| E2 | A held function + `stop_service` | **BLOCKED** by D-1 |
| — | `fault_help` (not in the protocol) | **PASS** — full FKB prose for this car's live fault |

## Defects found

### D-1 — SGBD file lookup is case-sensitive (root cause of B5, B2-partial, E2-blocked)

The M10 ident rung is **not** broken. It works exactly as designed: `22 F1 50` → ident
index → group table → variant name. On `0x63` it returned ident `0F 22 30` and resolved
the variant **`NBTEVO`**.

The failure is one line later. `mcp/src/server.rs:311`:

```rust
Some(dir.join(format!("{variant}.prg")))
```

The name is used verbatim. The group table stores it **uppercase**; every `.prg`/`.grp`
on disk is **lowercase** (`nbtevo.prg`). On Windows — where ISTA runs — this works. On
Linux it cannot. `grp_path` (`server.rs:409`) has the same shape.

Proven by passing the same variant in both cases:

| variant passed | result |
|---|---|
| `nbtevo` | loads, `variant: "nbtevo"` |
| `NBTEVO` | `no SGBD for variant 'NBTEVO'` |

Cascade: 30 of 32 ECUs cannot resolve a variant → `read_faults` cannot run `IS_LESEN`, so
the BMW-text enrichment lands on `0x12` only (**B2 partial**); `read_fault_detail` on
`0x63` read a 72-byte freeze frame it could not decode; no held service function is
reachable (**E2 blocked**).

A second, independent mode also exists: on `0x78`/`0x40` the group ident job never
transmits at all (zero `22 F150`), consistent with the documented K-line-era opcode gap.
Fixing the case bug will not clear those two.

### D-2 — ECUs that time out on the pre-read are still erased

Car session 2 §3.4 **recurred, and worse**: six ECUs (`0x06`, `0x08`, `0x19`, `0x2C`,
`0x30`, `0x5D`) timed out at 1.2 s during `clear_all_faults`' pre-read, were recorded as
`codes_before: []`, and were then **cleared physically anyway** — erased with no record of
what they held. The same six answered fine in the standalone `read_all_faults` sweep
moments earlier, so this is load/timing under the clear sequence, not dead ECUs.

Still the open owner question: should the clear refuse an ECU it could not pre-read?

### D-3 — misleading empty-result note on `test_plan`

An empty plan reports "usually means the DB predates the test-plan extract — rebuild it".
The DB was fine; `36F800` simply has **zero** rows table-wide while `d72n47a0` has 1,343.
The note sends you to a 20-minute rebuild for a correct answer.

### D-4 — the document store is 41% complete, and thinnest where hands-on work needs it

Every `repair_doc` row carries its own distinct `content_dede` id (zero nulls, zero
duplicates), but only 73,592 of 181,292 have a body:

| family | docs | with body | coverage |
|---|---|---|---|
| REP repair instructions | 123,778 | 49,484 | 40.0 % |
| EBO component/fuse locations | 38,021 | 4,747 | **12.5 %** |
| FUB function-test instructions | 11,673 | 11,673 | **100 %** |
| SWZ special tools | 7,820 | 7,688 | 98.3 % |

So the DB answers "what is this document called" for all 181k and "what does it say" for
41%. **The diagnostic path is unaffected** — FUB is the family the fault/symptom → test-plan
spine links to, and it is complete; the A2 step's FUB body rendered in full. The gap is in
the REP repair procedures and, worst, EBO fuse/component locations.

Not diagnosed: `xmlvalueprimitive_DEDE.sqlite` is no longer on this machine, so whether the
missing 59% lack German content at source or the build skipped them could not be checked
here. Worth settling before trusting a "no document" answer.

Protocol A1's EBO sub-check would have failed even with a correct query — not because of
the query, but because EBO bodies are largely absent.

## B6 — the `IDENT_FUNKTIONAL` capture (the headline)

`crates/best/src/bridge.rs`'s `[verify against capture]` is now **settled by real bytes**.

Request — exactly one functional frame, as predicted:

```
ctrl=0x0001 F4->DF  22 F1 50      (+ a ctrl=0x0002 echo)
```

**All 32 ECUs answered**, each `62 F1 50 0F <2-byte ident index>`:

```
00 0F1050   01 0F16E5   06 0F1D00   08 0F1390   10 0F1310   12 0F1F90
17 0F1180   18 0F14B0   19 0F1480   1C 0F1650   20 0F1760   29 0F1620
2A 0F14C0   2C 0F1B60   30 0F1E80   35 0F1C00   3D 0F1260   40 0F1001
41 0F1B00   42 0F1B10   43 0F1B20   44 0F1B30   5D 0F1840   5E 0F1900
60 0F1160   61 0F20C0   63 0F2230   67 0F2290   6B 0F1930   6D 0F1960
72 0F10D0   78 0F1618
```

**The answer:** 89 TCP segments carried **94 HSFZ frames**. Four segments held two frames,
one held three (`2C`, `6D`, `61`). Each responder is a **complete, independent HSFZ frame**
(`ctrl=0x0001`, its own source address) — the ZGW does not merge them; **TCP coalesces
them**. The reader must frame by the HSFZ length field and keep consuming within a segment,
never assume one frame per read.

That is precisely why the job returned `ERROR_ECU_INCORRECT_LEN` and then a long run of
`ERROR_ECU_NO_RESPONSE`, all misattributed to `ECU_ADR 00`/`JBBF`.

## Confirmations worth keeping

- **A4 / FA decode** — `version: 3` (not 87), `baureihe F025`, `build_date 0317` (matching
  factory I-Stufe `F025-17-03`), 74 SA options, `typ_schluessel WZ51`,
  `standard_fa F025#0317*WZ51%0300&LUSW$1CA$…-A090-LEDE`. A whole layout, read from two
  independent sources, correct on the first car it met.
- **B4** — info-memory detail sends **no** `19 06`/`19 04`. Captured the previously-unseen
  `22 20 <pos>` layout: index `62 20 00` is 4 × (3-byte code + 1-byte status); detail
  `62 20 03 00 36 F8 00 2F 01 17 …`.
- **D1** — `19 02 0C` → `59 02 FF` → `14 FF FF FF` → `54`. **No `10 03`**, and no
  `7F 14 22`: the ECU accepted the clear with no diagnostic session, so removing `10 03`
  was right.
- **D2 order on the wire** — `22 3F 06` (SALAPA, new) → functional `14 FF FF FF` → `0xDF`
  → ZFS `31 01 40 00 00` → clamp `31 01 10 01 06 06 A8` → **15.16 s** →
  `31 01 10 01 0A 0A 43` → re-ident `22 3F 07`. `terminal_15_restored: true`.
  `supplier_jobs: []` — correct for this F25, which has none of those modules.
- **E1** — `2F 60 DA 03 23 28` → `6F 60 DA 03`. No `setflt` crash; no `10 03`. The note now
  correctly says klartext sent **no** return-to-safe frame rather than claiming it did.
- **B7** — whole session: `22`, `3E`, `19`, plus exactly `31`×3, `2C`×2, `14`×2, `2F`×1.
  Zero `11`, zero `19 15`, zero `34`–`37`, zero `10`.
- **C / P2.1** — `read_data ITOEL` emitted `2C 01 F3 03 45 17 01 02` then `22 F3 03` →
  `62 F3 03 34 58` = 34 °C. No bare `22 4517`. The fix holds on the car.
- **All 32 ECUs responding** (`62 3F 08 00 20 …`); session-2's four timeouts did not recur
  on the standalone sweep, which is why B3 could not be exercised.

## Vehicle state

Before: DAB aerial `B7F805` on `0x63` (present), AUC sensor `C90D60` on `0x00` (absent),
TCB `03178A` on `0x61` (unknown). After the whole-car clear: 25 of 32 verified clean;
`B7F805` re-set immediately, as it did in session 1 — a genuinely active fault.

## Protocol-document corrections

- `repair_docs` titles are **English** in this data; the doc's "search in GERMAN" advice is
  inverted for titles (bodies are German).
- EBO titles are connector/fuse **designators** (`FL8, FL9`, `R10, X73 n. NG`), not prose,
  so the doc's suggested `Sicherung` / `Fuse` queries correctly match nothing.
- Prerequisite 2's path is `data/Testmodule(1)/Ecu`, but the shipped directory is
  `TestModule(1)` — a case mismatch that silently disables every SGBD path on Linux.

## `fault_help` — the most useful output of the session

Not in the protocol, but exercised because it is the other half of the doc layer. For this
car's one live fault (`B7F805` on `0x63`) it returned five linked FKB documents, each with
full German prose under its own headings: set condition, voltage and terminal condition,
timing condition, service measure, visible effect, warning lamp. The service measure is a
three-step escalation (wiring/connectors → antenna → head unit), and it records that this
fault raises no CC message and no warning lamp — matching the car, where the fault has been
stored since session 1 with nothing shown on the dash.

Read it with `fault_help { ecu: "0x63", code: "B7F805" }`. The prose itself stays out of
this repo, per the BYO-data rule — this note records only that the path works and what
shape the answer takes.

FKB prose coverage is what makes this work; the linked procedure docs stay titles by design.

## Still pending

- **E2** — needs D-1 fixed first.
- **B3** — needs an ECU that does not answer.
- **D-4** — confirm against the source store whether the missing REP/EBO bodies exist.

## Owner-confirmed physical outcomes

- The electric fan **did spin** on E1.
- The dash **did go dark and come back** on D2's terminal-15 cycle, matching
  `terminal_15_restored: true` and the measured 15.16 s gap.
