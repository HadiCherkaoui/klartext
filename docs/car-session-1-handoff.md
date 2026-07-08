# Full acceptance protocol — verify everything built so far (car session 1)

**Goal:** verify **every feature implemented to date**, across M1 → Item 5/P2 — transport,
DTC read/clear, DTC text, measurement scaling, the MCP server, service functions, live
discovery, **ECU auto-resolve**, freeze-frame metadata, the SVT/identity dump, the
**fault/repair-doc catalog**, the BEST/2 engine, and the live `run_job` path. Split into what is
**already verified offline** (Part A — done, no car) and the **on-car acceptance pass** (Part B —
what genuinely needs the car), reads first, the two gated writes last.

Drive the on-car reads through the **MCP server** (agent-driven); the writes are explicit CLI/MCP
steps you run by hand. **pcap + frame-log the whole session.**

---

## Coverage matrix — every feature, and how it's verified

| Feature | Milestone | How verified | Status |
|---|---|---|---|
| HSFZ transport + handshake | M1 | on-car: `connect` | ⏳ Part B |
| Gateway discovery | M1 | on-car: `discover` / `connect` | ⏳ Part B |
| Read DTCs | M1/M2 | on-car: `read_faults` | ⏳ Part B |
| DTC → text decode | M3 | offline unit + on-car: fault titles | ✅ unit / ⏳ B |
| Standard OBD-II PID scaling | M5 | offline unit (a **no-op on this DDE** — it rejects `F4xx`) | ✅ unit |
| Proprietary measurement scaling (`SG_FUNKTIONEN`) | M6 | offline differential oracle + on-car: `read_data` | ✅ oracle / ⏳ B |
| MCP read-only server | M4 | on-car: every MCP tool | ⏳ Part B |
| Service-function catalog | M7/M8 | **offline: `service list` → 160 functions** | ✅ **done here** |
| `list_measurements` | M9 | on-car: `list_measurements` | ⏳ Part B |
| `read_data` by name | M9 | on-car: `read_data` | ⏳ Part B |
| `scan_ecus` (fitted probe) | M10 | on-car: `scan_ecus` (+ `rescan`) | ⏳ Part B |
| ECU names from ISTA DB (no hardcoded aliases) | M10 | on-car: names in `scan_ecus`/`identify_vehicle` | ⏳ Part B |
| **ECU / variant auto-resolve ladder** | M10 | on-car: `read_data`/`run_job` **without** an explicit variant | ⏳ Part B |
| One demuxed connection → all ECUs | M10 | on-car: `read_all_faults` | ⏳ Part B |
| **Freeze-frame / snapshot metadata** | M11 Item 1 | on-car: `read_fault_detail` (`19 04/06/09`) | ⏳ Part B |
| **SVT + full identity dump** | M11 Item 2 | on-car: `identify_vehicle` (`22 3F07`, VIN, FA, I-Stufe) | ⏳ Part B |
| **Fault → repair-doc catalog** | M11 Item 4 | **offline: `fault-docs 000480` → 2 ISTA docs** | ✅ **done here** |
| BEST/2 VM engine | Item 5 P1 | offline differential oracle (VM == `measurement.rs`) | ✅ oracle |
| `job list` | Item 5 P2 | **offline: `job list` → 272 DDE jobs** | ✅ **done here** |
| `run_job` live (CLI + MCP) | Item 5 P2 | on-car: `run_job` STATUS_LESEN (DDE + DSC) | ⏳ Part B |
| Read-only SID gate (no write reaches car) | Item 5 P2 | offline unit + on-car: a write-job refuses | ✅ unit / ⏳ B |
| CP1252 string decode (`_INFO` umlauts) | Item 5 P2 | offline unit + on-car: `_INFO` reads clean | ✅ unit / ⏳ B |
| **CBS oil-service reset (write)** | M7/M8 | on-car **write**: `service run Oel --confirm` (self-confirming) | ⏳ Part B (write) |
| High-risk actuation **refusal** | M8 | on-car: `service run <STELLER/ABGLEICH>` → refused | ⏳ Part B |
| **Clear DTCs (write)** | M2 / M9 | on-car **write**: `clear-faults`/`clear_faults` (confirm) — **last** | ⏳ Part B (write) |

---

## Part A — already verified OFFLINE (no car; done on the build machine)

Run on this machine at `01cfbda` with the real DB + SGBD:

- **Service catalog (M7/M8):** `klartext --sgbd d72n47a0.prg service list` → **160 service
  functions** (CBS resets `Oel`/`Br_v`/…, learned-value resets, actuators, calibrations), each
  risk-tagged.
- **Fault/repair-doc catalog (M11 Item 4 — the "docs linking"):** `klartext fault-docs --target
  12 000480` → fault "DDE: Electric fan" + **2 linked ISTA documents** (`FKB Kühlerlüfter`,
  `FKB Fan relay`) with doc numbers + IDs. This is the offline `fault_help` layer; it needs no car.
- **BEST/2 job list (Item 5):** `klartext --sgbd d72n47a0.prg job list` → **272 DDE jobs**.
- **Unit + oracle layer (M3/M5/M6, Item 5 P1/P2):** full workspace `cargo test` green at P2
  close — the differential oracle (VM `STATUS_LESEN` == `measurement.rs` on real bytes), the M5
  PID formulas, the telegram codec, the read-only gate, and CP1252 round-trip are all pinned.

**These do not need the car.** Part B covers only what transmits.

---

## Part B — on-car acceptance pass

### B0. Transfer, build, setup

Same as before — **only the code moves** (you have the SGBD; the DB probably suffices, see the
DB note at the end):
```bash
cd <laptop repo>
git fetch ~/…/klartext-main.bundle main && git merge --ff-only FETCH_HEAD    # updates code, keeps data/
cargo build --release -p klartext-cli -p klartext-mcp
```
Network per `docs/on-car-verification-protocol.md` Part 0 (ENET, link-local IP, gateway IP).
**pcap the whole session** and **launch the MCP server with frame tracing → stderr log**:
```bash
doas tcpdump -i <enet-iface> -w captures/car-session-1.pcapng 'tcp port 6801 or udp port 6811'
# in another shell:
RUST_LOG=klartext_client=trace ./target/release/klartext-mcp \
  --gateway-ip <IP> --sgbd-dir "data/Testmodule(1)/Ecu" 2> captures/frames-$(date +%Y%m%d).log
```

### B1. Reads — on-car agent prompt (copy-paste; all read-only)

> You are connected to the klartext MCP server for a full read-only acceptance capture on a
> BMW F20. Every step here is a READ — do **NOT** clear faults or call any write; we need the
> fault memory intact for the freeze-frame reads (the writes are a later, manual phase). Run in
> order; after each, paste the full JSON result + a one-line note if anything errored (an error
> is data — record and continue, no retry-loop).
>
> 1. `connect` — HSFZ transport up (M1).
> 2. `identify_vehicle` — SVT + VIN + FA + I-Stufe + per-ECU identity (M11 Item 2). Expect a real
>    VIN, ~15 ECUs **by name**, an I-Stufe, a vehicle order. Empty `ecus`/missing `vin` ⇒ STOP
>    and report (SVT framing is the #1 catch).
> 3. `scan_ecus`, then again with `{ "rescan": true }` (M10 — fitted probe + ECU names, stable
>    across two reads).
> 4. `list_ecus` — the resolved fitted list (M10).
> 5. `read_faults` on the DDE `0x12`, the FEM `0x40`, and one more fitted ECU (M1/M3 — codes +
>    decoded titles).
> 6. `read_fault_detail` for one DDE `0x12` fault code from step 5 (M11 Item 1 — freeze-frame:
>    km, RPM, temperatures at occurrence). Its decode is the one we can fully check.
> 7. `list_measurements` for the DDE `0x12`, then **`read_data`** the oil temperature by name
>    (M6/M9 — proprietary scaling). **This also verifies ECU/variant auto-resolve (M10):** you
>    pass only the ecu/measurement, no explicit variant.
> 8. `run_job { "ecu": "0x12", "job": "STATUS_LESEN", "args": ["ARG", "ITOEL"] }` (Item 5 P2 —
>    the live engine). Expect `STAT_…OEL…_WERT`/`_EINH`/`_INFO` + **`JOB_STATUS = OKAY`**, and
>    the `_INFO` German text with **clean umlauts** (verifies the CP1252 fix). Compare its value
>    to step 7's `read_data` (two independent read paths, same sensor).
> 9. `run_job { "ecu": "0x29", "job": "STATUS_LESEN", "args": ["ARG", "<a DSC status/bitfield
>    measurement's ARG name from list_measurements>"] }` — expect **several** distinct `STAT_*`
>    stems in `sets` (multi-value proof). If variant resolution fails, add `"variant": "dsc_10"`.
> 10. `read_all_faults` — whole-car (M10 demuxed connection); note the wall-clock.
> 11. `disconnect`.

### B2. Negative safety test (verify the gate refuses — nothing executes)

Confirms the blast-radius refusals without changing anything:
- `service run <a high-risk label>` (pick any `STELLER` actuator or `ABGLEICH` from `service
  list` — e.g. an electric-fan/throttle actuator) → **expect "refused, human-only"**, no frame
  sent. (M8 gating.)
- Via MCP, confirm the surface is read-only: `run_job` on a **write** job (any `STEUERN_*` /
  `ABGLEICH_*` name) → **expect the read-only gate to refuse it** ("job emits a write; run it
  from the CLI"), and the pcap shows **no** write frame left the tester. (Item 5 P2 gate.)

### B3. Writes — explicit, last (these change ECU state)

Run these **only after B1 is captured** (B3.2 wipes the fault memory). Each is confirm-gated.

1. **CBS oil-service reset (M7/M8 — low-risk, self-confirming):**
   ```bash
   ./target/release/klartext --gateway-ip <IP> --sgbd "data/Testmodule(1)/Ecu/d72n47a0.prg" \
     --target 12 service run Oel --confirm
   ```
   Expect success + the built-in read-back of the CBS block, and the **dashboard oil-service
   indicator visibly resets**. This is the write-path proof on the safest possible operation.
2. **Clear DTCs (M2 / M9 — the one MCP write) — DO THIS LAST:**
   - CLI: `klartext --gateway-ip <IP> --target 12 clear-faults --confirm`, **or**
   - MCP: `clear_faults { "ecu": "0x12", "confirm": true }` — it pre-reads + echoes the codes it
     discards, clears, then a follow-up `read_faults` shows them gone.
   This wipes the DDE fault memory, so it must come after every B1 fault/freeze-frame read.

---

## Part C — what to send me back

1. **`captures/car-session-1.pcapng`** + **`captures/frames-*.log`** — the whole session (VIN
   redactable: `62 F1 90` + 17 ASCII → `XX…`). The most important artifact.
2. The **JSON** from every B1 step (esp. `identify_vehicle`, a `read_faults`, the
   `read_fault_detail`, both `run_job`, `read_data`).
3. The **B2** refusal outputs (proof the gate/blast-radius holds).
4. The **B3** write outputs (CBS read-back; the clear pre/post fault lists) + whether the
   dashboard indicator reset.
5. Anything that errored, verbatim, + the `read_all_faults` timing.

From that I confirm every ⏳ row in the matrix — flipping the `[verify against capture]` markers
(SVT/FA/I-Stufe/DTC/freeze-frame framing + the P2 telegram/checksum/multi-value markers) — or fix
whatever the wire disagrees with. Then we start **P3** (guided oil-level flow + the confirmed
write ritual).

---

## The one value that matters most

On the `run_job STATUS_LESEN` calls, **`JOB_STATUS = OKAY`** ⇒ the BMW-FAST codec is wire-correct
(a real ECU accepted our additive checksum + framing). **`ERROR_ECU_INCORRECT_LEN`** ⇒ the wire
disagrees with what we derived offline — capture it; that is exactly what this session exists to
catch.

## DB note (unchanged)

P1/P2 changed nothing in the DB. The session reads it only for ECU names (`ecu`), fault text
(`dtc`), and freeze-frame labels (`envcond`) — measurements come from the SGBD. If your laptop DB
already reads faults/scans fine, use it; the Item-4 `fault_doc`/`infoobject` tables feed only the
**offline** `fault-docs`/`fault_help` (already verified in Part A), which the car session doesn't
call. For zero doubt, copy this machine's `data/klartext-semantic.db` (51 M, dated 2026-07-04).
