# On-car test protocol — 2026-08-04 work (tcpdump-verified)

**For a Claude session driving the klartext MCP server against the real car, with `tcpdump`
capturing every frame.** Everything below was built or corrected offline since car session 2;
none of it has touched a car. The point of this session is to find out which parts are right.

> **Read the whole file first.** Phases A–C are safe reads. Phase D contains PHYSICAL actions,
> one of which **cuts the ignition for 15 seconds**. Phase E **actuates a component**. Do not
> run D or E without the owner present and each step explicitly OK'd.

Cars: **F20** gateway `169.254.90.33` (ZGW still absent on Ethernet as of 2026-07-29 — expect
this car to be unusable), **F25 X3** gateway `169.254.71.121`. Ports: TCP **6801** diagnostic,
UDP/TCP **6811** ident.

---

## 0. Prerequisites

1. **The semantic DB is already rebuilt — you do NOT need to run the script.** Built
   2026-08-15 against your own `data/Testmodule(1)`, verified row-by-row:

   | table | rows |
   |---|---|
   | `fault_test_plan` | 357,209 |
   | `diag_object` | 93,153 |
   | `diag_info` | 649,145 |
   | `symptom` / `symptom_test_plan` | 5,385 / 24,797 |
   | `repair_doc` | 181,292 |
   | `virtual_fault` | 918 |
   | `ecu_tree` | 4,019 |
   | `fkb_body` / `repair_body` (doc store) | 77,187 / 73,592 |

   `data/klartext-semantic.db` 195 MB, `data/klartext-docs.db` 109 MB. Re-run
   `scripts/build-semantic-db.sh` only if you replace the ISTA data — it takes ~20 min,
   almost all of it fetching document bodies out of the 50 GB store.
2. `--sgbd-dir` points at the real `.prg`/`.grp` set (`data/Testmodule(1)/Ecu`). Several new
   paths need `.grp` files, not just `.prg`.
3. Car awake, ENET cable in, link-local address on the interface.
4. `cargo build --release -p klartext-mcp`.

**Capture** (the `-U` lesson from last time — without it the buffer hides everything until the
process stops):

```bash
ip -o -4 addr show | grep 169.254          # find the ENET interface
doas tcpdump -i <iface> -U -s0 -w /tmp/klartext-oncar-$(date +%H%M).pcapng \
     'tcp port 6801 or port 6811'
```

The pcap holds the VIN. `captures/` is gitignored — never commit it.

---

## Phase A — offline, no car needed

Run these BEFORE plugging in. If they fail, the rebuild did not take and everything downstream
is meaningless.

### A1 — repair documents exist and render ⭐ the headline
- **Call:** `repair_docs { query: "Nockenwelle" }`
- **Expect:** at least one `REP` document, `title` like *"Nockenwelle ausbauen (M40)"*, and a
  `body` containing `## Schritt 1`, at least one torque figure (`= 5,5 Nm`), and an
  `[Abbildung: …]` marker.
- **If `body` is null** the doc store did not build — check the build output for
  "wrote N repair/location/tool bodies".
- **If `docs` is empty** the DB predates `repair_doc`; rebuild.
- Also try `repair_docs { query: "Sicherung", infotype: "EBO" }` (fuse locations) and
  `repair_docs { query: "Ringschlüssel", infotype: "SWZ" }` (tools).
- **Search in GERMAN.** The shipped titles are German; English matches far less.

### A2 — the test plan for a fault ⭐ the newest headline
The bridge from a code the car reports to what ISTA would actually check.
- **Call:** `test_plan { ecu: "0x12", code: "<a code read_faults gave you>", variant: "d72n47a0" }`
- **Expect:** one or more steps with a `name` like `Luftmassensystemtest_sys_DDE`, a `title`,
  ISTA's `priority`, and a `docs` list. Steps flagged `sure_suspicion` come first.
- **`variant` matters and the reply says whether it was used.** 153 ECU variants share address
  `0x12`; unscoped, the plan mixes in other engines' steps. If `variant` in the reply is `null`,
  the ladder could not resolve one — pass it explicitly and compare: the unscoped list should be
  strictly larger and contain obviously-wrong entries (petrol ignition steps on a diesel).
  That difference IS the check.
- **Then read a step's document:** take an `infoobject_id` from a step whose `has_body` is true
  and call `repair_docs { document_id: <id> }`. Expect a rendered `body`.
- **An `ABL` with `has_body: false` is CORRECT, not a bug** — `ABL` names an executable ISTA
  test module, not a document; there is no body for it in these databases.
- **If every plan is empty**, the DB predates the spine tables — rebuild.

### A3 — the symptom side, for when there is no fault code
- **Call:** `symptom_search { query: "Motor" }` (German — the tree is German)
- **Expect:** complaints with `selectable: true` first, each with an `id`.
- **Then:** `symptom_test_plan { symptom_id: <one of them> }` → the same shape as A3.
- Roughly 2/3 of complaints carry a plan, so an empty one on a given entry is normal; an empty
  one on *every* entry is not.

### A4 — the FA decoder, against your own car's bytes
Needs the car (it is a read), but listed here because it is the cheapest high-value check.
- **Call:** `identify_vehicle {}`
- **Expect:** `version: 3` (**not 87** — that was the off-by-3), `baureihe: "F025"`,
  `build_date: "0317"` (MMyy → March 2017, matching the factory I-Stufe `F025-17-03`),
  plus non-empty `options`, and a `standard_fa` string of the form
  `F025#0317*WZ51%0300&LUSW$…`.
- **This is the single most informative check in the protocol.** If the header fields decode
  but `options` is empty, the tagged bit-stream walk is wrong. If `version` is 87, the rebuild
  did not include the new decoder binary.

---

## Phase B — reads (safe, run all)

### B1 — info memory now carries BMW's own text ⭐
- **Call:** `read_faults { ecu: "0x12" }`
- **Wire:** `19 02 0C`, then — new — the DDE's own `IS_LESEN` job rather than a bare
  `22 20 00`. You should see `22 20 00` sent BY THE JOB.
- **Expect:** each `info_entries[]` entry now has a `descriptions[0].variant == "ecu"` whose
  text is BMW's own, e.g. *"DDE-Steuergerät intern (Recovery suppressed): Recovery
  aufgetreten"*. Previously these were code+status only.
- **Check:** if `descriptions` is empty for every info entry, the job did not run — the note
  will say why.

### B2 — the whole-car sweep says the same thing
- **Call:** `read_all_faults {}`
- **Expect:** the same enriched `info_entries` on every ECU, not just `0x12`. Before today the
  two tools disagreed.
- **Watch the timing.** This now runs `IS_LESEN` per ECU. If the sweep gets dramatically
  slower, say so — that is a real cost finding.

### B3 — virtual faults for a silent ECU ⭐ needs an ECU that does not answer
- Car session 2 had four (`0x06`, `0x08`, `0x19`, `0x2C`) time out on the pre-clear read.
- **Expect:** any ECU with `error` set now also carries `virtual_faults[]` — for the DDE that
  is code **`S 0003`**, `reason: "the ECU answered nothing…"`.
- **If no ECU fails**, this cannot be tested; say so rather than inventing one.

### B4 — fault detail routes by store ⭐ the car-session-2 §3.6 defect
- **Call:** `read_fault_detail { ecu: "0x12", code: "<an INFO-memory code from B1>" }`
  (last time: `36F800`)
- **Wire:** **NO `19 06` / `19 04`.** Instead the DDE's own `IS_LESEN_DETAIL`, which sends
  `22 20 00` then `22 20 <position>`.
- **Expect:** `source: "info_memory"`, `snapshot`/`extended` empty, and `info_detail[]`
  populated with named EDIABAS results (`F_HEX_CODE`, `JOB_STATUS`, …).
- **Check:** `tshark … -Y 'hsfz.data[0:2]==19:06'` for this call returns **ZERO**. Last time
  it returned two, both `7F 19 31`.
- **Then a fault-memory code** (e.g. `B7F805` on `0x63`): `source: "fault_memory"` and the
  `19 06`/`19 04` reads DO appear, unchanged.
- **The `22 20 <position>` response layout has never been captured — this call is what
  captures it.** Keep the bytes.

### B5 — variant resolution without passing one ⭐ the §8 blocker
- **Call:** `read_data { ecu: "0x78", name: "<any IHKA measurement>" }` — **no `variant`**.
- **Expect:** it resolves on its own via `g_klima`'s `IDENTIFIKATION` job.
- **Wire:** `22 F1 50` to `0x78`, then the measurement read.
- **This is the rung that unlocks 30 of 32 ECUs.** If it fails, capture the `22 F150`
  response — the ident-index → variant lookup is the thing to check.
- Repeat on two or three other ECUs (`0x40`, `0x63`) to see how general it is.

### B6 — `IDENT_FUNKTIONAL`, the P3 broadcast ⭐⭐ THE most valuable capture
- **Call:** `run_job { ecu: "0x10", variant: "f01", job: "IDENT_FUNKTIONAL" }`
  (if `f01` will not load as a variant, this may need a code change — note that and move on.)
- **Wire:** ONE functional frame — format byte `0xC3` (functional, len 3) to target **`0xDF`**
  carrying `22 F1 50` — followed by MANY responses from different source addresses.
- **Expect:** possibly an error. **That is fine and is the point.**
- **What matters is the CAPTURE.** klartext's concatenation of the responder telegrams is
  marked `[verify against capture]` in `crates/best/src/bridge.rs`: against a mock the job
  parses the first telegram and then loses sync. The real bytes settle how EDIABAS hands
  multiple responses to a job — which is the last unknown blocking ISTA's real BN2000 ECU
  discovery.
- **Record:** every frame between the `C3 DF F1 22 F1 50` request and the job's result, in
  order, with source addresses.

### B7 — everything that must NOT appear
Across the whole session's capture:
- `11 xx` (ECU reset) — **zero**
- `19 15` (permanent DTC) — **zero**
- `34`–`37` (flashing) — **zero**
- `2E`/`2F`/`31` — only where Phase D/E explicitly calls for them

---

## Phase C — regression checks on what already worked

Fast confirmations that the day's changes broke nothing.

| Call | Expect |
|---|---|
| `connect {}` | one `22 F1 90`, VIN populated |
| `scan_ecus {}` | one `22 3F 07` → `62 3F 07 0020` = 32 ECUs |
| `read_faults { ecu: "0x12" }` | `19 02 0C` present, `19 02 FF` **absent** |
| `read_fault_detail` on `0x63` | `19 09` → `19 06` → `19 04`; on `0x12` no `19 09` |
| `read_data { ecu: "0x12", name: "ITOEL", variant: "d72n47a0" }` | `2C 01 F303 …` + `22 F303`, ~oil temp with a unit |

---

## Phase D — the clear (PHYSICAL, drops ignition 15 s)

> **STOP.** `clear_all_faults` switches the ignition off for 15 seconds. Owner present, car in
> park, nothing depending on continuous power.

### D1 — `clear_faults` on ONE ECU ⭐ the `10 03` removal
- **First:** call with `confirm: false` → refusal, no wire traffic.
- **Call:** `clear_faults { ecu: "0x12", confirm: true }`
- **Wire:** `19 02 0C` → **`14 FF FF FF`** → nothing.
- **Check:** **NO `10 03`.** klartext sent one until today; BMW's own `FS_LOESCHEN` does not,
  and the census test now asserts its absence. `tshark … -Y 'hsfz.data[0]==10'` → **zero**.
- **If the ECU refuses with `7F 14 22`** (conditionsNotCorrect), that is the one outcome that
  would justify putting `10 03` back — record it precisely.
- **Still no `11 xx`.**

### D2 — `clear_all_faults` ⚠️ DROPS IGNITION
- **Call:** `clear_all_faults { confirm: true }`
- **Wire, in ISTA's order:** per-ECU pre-read → functional `14 FF FF FF` to `0xDF` → physical
  stragglers → gateway ZFS `31 01 40 00 00` → clamp `31 01 10 01 06 06 A8` → ~15 s →
  `…0A 0A 43` → re-ident → verification read.
- **New this time — the supplier info-memory clears.** `supplier_jobs[]` in the result now
  reports `not_run: null` for any job that actually ran. **On this F25 expect the list to be
  EMPTY** — it has none of `FEM_20`/`FRM3`/`D_KBM`/`ALC_60`/`LM_AHL`. An empty list is the
  correct result, not a failure.
- **Also new — SALAPA.** The FA's option list is now read before the sequence, so a
  SALAPA-gated job can be evaluated. Confirm a `22 3F 06` read precedes the clear.
- **Checks unchanged:** `14 FF FF FF` to `0xDF` exists; ZFS to `0x10`; both clamp frames ~15 s
  apart; **no `11 xx` anywhere**.
- **Watch the pre-read.** Car session 2 had four ECUs time out here and get erased with no
  discard record (§3.4). If that recurs, note which ECUs — it is an open owner question
  whether the clear should refuse.

---

## Phase E — actuation (PHYSICAL, moves a component)

### E1 — the electric fan ⭐ the `setflt` fix
- Car session 2 died with `opcode setflt is not implemented`. It is implemented now
  (it sets float PRECISION, not a fault), and `STEUERN_E_LUEFTER` is opcode-complete.
- **Refusal check first:** `run_service_function { …, confirm: false }` → refuses, no traffic.
- **Call:** `run_service_function { ecu: "0x12", variant: "d72n47a0",
  function_id: 20000725015199, confirm: true }`
- **Expect:** the fan actually spins. A `2F`/`31` on the wire.
- **The note wording changed:** for a `has_reset:false` function it should now say klartext
  sent **no** return-to-safe frame and the ECU reverts on its own — not the old "Ran and
  returned to safe", which claimed an action klartext did not take.
- **No `10 03`** before the phase jobs — settled as correct parity (neither the `.prg` nor
  ISTA sends one).

### E2 — a held function, if E1 works
- Pick a holding function from `list_service_function_ids` (the side lights at `0x43`/`0x44`
  are both held and visually confirmable).
- Verify `stop_service` transmits ONLY the Reset phase, and that `disconnect` with a hold
  outstanding also fires it.

---

## What to bring back

For each item: **PASS** (wire matches), **FAIL** (record the exact bytes), or **N/A**.

The six that matter most, in order:

| # | What | Why it matters |
|---|---|---|
| B6 | the `IDENT_FUNKTIONAL` capture | the last unknown blocking ISTA's real ECU discovery |
| A4 | FA decodes to your car | proves a whole layout read from two independent sources |
| A2 | a fault's test plan, scoped vs unscoped | the newest layer, and variant scoping is the thing that makes it correct rather than plausible |
| B4 | no `19 06` for an info code | the car-session-2 defect, and it captures `22 20 <pos>` |
| B5 | variant resolves unaided | unlocks 30 of 32 ECUs |
| D1 | no `10 03` before a clear | a write-path change made on your ruling |

Anything that FAILs: capture the bytes (`tshark … -x`) and the tool's JSON. A precise failure
is worth more than a vague pass.

---

## Known-absent, so do not go looking

- **Permanent DTCs (`19 15`)** — deliberately never implemented (FASTA-only in ISTA).
- **General info-memory clear** — ISTA's is dead code behind a guard that is never true; the
  info store surviving a clear is correct parity.
- **`ABL` bodies** — an `ABL` in a test plan is title-only *by nature*: it names an executable
  ISTA test module, not a document, and no body for it exists in these databases. `has_body:
  false` on one is the correct answer, not a build failure.
- **Fitment filtering on a test plan** — the steps A2/A3 return are ISTA's CANDIDATE set. ISTA
  additionally gates each one on its `XEP_RULES` offline-fitment engine and a per-vehicle
  validity check; neither is ported (the rules engine is a logic port, still open), so expect
  steps that do not apply to your car. Every reply says so in its `note`.
- **The other 14 ISTA document families** — SSP schematics, STA connector data and the rest are
  still unextracted. `FUB` and the fault/symptom→procedure spine are now IN (this round); `ABL`
  is in as titles only, per above.
- **P2.4 / P2.5** — measurements still hand-build frames rather than running the job, and the
  catalog's `mul`/`offset`/`round`/`format` are still applied to nothing. Both deliberate:
  P2.5's extract has no `StateValues`, so applying it would corrupt enumerated results.
