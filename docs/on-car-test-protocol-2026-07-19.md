# On-car test protocol — 2026-07-19 parity + write-tier work (tcpdump-verified)

**For a Claude session operating the klartext MCP server against the real car, with `tcpdump`
capturing every frame.** The goal: verify byte-for-byte that this session's behaviour matches what
the code intends and what ISTA does. You (Claude) drive the MCP tools; `tcpdump` records the wire;
you check the capture against the expected frames below.

> **Read this whole file before touching the car.** Phases A/B are safe reads. Phase C contains
> PHYSICAL actions — one of them **cuts the car's ignition (terminal 15) for 15 seconds**. Do not
> run Phase C without the owner physically present and each step explicitly authorised.

Cars (from `CLAUDE.md`): **F20** gateway `169.254.90.33`, **F25 X3** gateway `169.254.71.121`.
Both share the N47 DDE `d72n47a0`. Ports: **TCP 6801** diagnostic, **UDP/TCP 6811** ident/control.

---

## 0. Prerequisites

1. **Semantic DB rebuilt** — `scripts/build-semantic-db.sh` has been run since this session (it adds
   the `fixed_function` table and the freeze-frame gate data). Without it, `read_faults`' source
   tags and service-function discovery still work, but the `19 09` severity gate falls back to
   "send anyway" (a wasted round trip, not a failure) and `list_service_function_ids` is empty.
2. **`--sgbd-dir` points at the real `.prg` set** (`data/Testmodule(1)/Ecu/`) — needed for
   `run_service_function`, `run_job`, and the `F_SEVERITY` skip in `read_fault_detail`.
3. Car **awake** (terminal 15 / ignition on for the read phases), ENET cable in, a **link-local**
   address on the ENET interface (`169.254.x.x`).
4. The MCP server binary built: `cargo build --release -p klartext-mcp`.

---

## 1. Start the capture

**Identify the ENET interface** (the one holding a link-local address):

```bash
ip -o -4 addr show | grep 169.254     # → e.g. enp3s0 or the USB-Ethernet adapter
```

**Start `tcpdump`** (the user runs privilege escalation with `doas`, not `sudo`). Capture both the
diagnostic and control ports, full frames:

```bash
doas tcpdump -i <iface> -s0 -w /tmp/klartext-oncar-$(date +%H%M).pcapng 'tcp port 6801 or port 6811'
```

Leave it running for the whole session in one terminal. **The capture contains the VIN and is BYO
data — it is gitignored; never commit it, and redact the VIN before sharing** (see §6).

**Decoding the capture.** Wireshark/tshark has a native HSFZ dissector (BMW/Technica, ~2023+). Map
port 6801 to it and read the embedded UDS:

```bash
# every diagnostic frame as: time  src→tgt  UDS-bytes
tshark -r <capture>.pcapng -d tcp.port==6801,hsfz -Y hsfz \
  -T fields -e frame.time_relative -e hsfz.source -e hsfz.target -e hsfz.data
```

If your tshark lacks the HSFZ dissector: an HSFZ diagnostic frame is `[u32 length][u16 control]
[u8 source][u8 target][UDS…]` — the UDS payload starts at byte **8** of the TCP segment; control
word `0x0001` = request/response, `0x0002` = the gateway's ack (echoes the frame). A UDS request's
first byte is the service ID; a positive response is `SID+0x40`; a negative is `7F <sid> <nrc>`.

---

## 2. Launch the MCP server and connect

Launch pointing at the car under test (example: F25):

```bash
./target/release/klartext-mcp \
  --gateway-ip 169.254.71.121 --port 6801 \
  --semantic-db data/klartext-semantic.db \
  --sgbd-dir data/Testmodule(1)/Ecu
```

All logging goes to **stderr** — stdout is the JSON-RPC stream. Drive the tools as an MCP client.

---

## Phase A — READS (safe; run all of these)

Each test lists: the tool call, the **UDS you should see on the wire**, and the **check**. A read
never needs `confirm`.

### A1 — `connect` (VIN ladder, P1.3)
- **Call:** `connect {}`
- **Wire:** `22 F1 90` to the gateway (`0x10`). If it answers a VIN, the ladder stops there; if it
  returns the all-zeros sentinel or is empty, you should see `22 F1 90` again to the next rung
  (CAS `0x40`, then FRM). **Confirm the ladder stops at the FIRST non-empty rung** — you should NOT
  see all three if the gateway answered.
- **Check:** `tshark … -Y 'hsfz.data[0:3]==22:f1:90'` → count the `22 F1 90` requests; expect 1 on a
  healthy gateway. The result's `vin` is populated.

### A2 — `scan_ecus` (P/M11)
- **Call:** `scan_ecus {}`
- **Wire:** `22 3F 07` (configured superset) to `0x10`, then a best-effort `22 3F 08` (responding
  subset).
- **Check:** exactly one `22 3F 07`; the response is `62 3F 07` + a u16-BE count + one byte per ECU.

### A3 — `identify_vehicle`
- **Call:** `identify_vehicle {}`
- **Wire:** `22 3F07`, `22 3F06` (FA/SALAPA), `22 100B` (I-Stufe), `22 F190` (VIN).
- **Check:** the I-Stufe decodes to `Fxxx-yy-mm-ppp`; VIN matches A1.

### A4 — `read_faults` on ONE ECU — the P2.2 bundle + P0.3 mask
- **Call:** `read_faults { ecu: "0x12" }`  (the DDE; pick any fitted ECU)
- **Wire (the load-bearing one):** **`19 02 0C`** (NOT `19 02 FF`) — the ISTA mask — followed by
  **`22 20 00`** (info memory). Both, in that order, one of each.
- **Checks:**
  - `19 02 0C` present, `19 02 FF` ABSENT (P0.3). `tshark … -Y 'hsfz.data[0:2]==19:02'` then read
    byte 3 — it must be `0c`.
  - `22 20 00` present (info memory folded in, P2.2). Its response `62 20 00` is followed directly by
    4-byte records — **no version byte** (P0.2/cddf839): the first byte after `62 20 00` is a DTC
    code byte, not `03`.
  - In the result JSON, each fault carries `source: "fault_memory"` or `"info_memory"`, and a
    `presence` of `present`/`absent`/`unknown`. If the ECU has no info memory it answers `7F 22 31`
    → `info_supported: false`, **no error**.

### A5 — `read_all_faults` — the P1.2 sequential sweep
- **Call:** `read_all_faults {}`
- **Wire:** `19 02 0C` + `22 20 00` for EACH fitted ECU, **strictly one request outstanding at a
  time**. This is the P1.2 property and the fix for the car-session-1 dropouts.
- **Check (the important pcap analysis):** sort the diagnostic requests by time and confirm every
  request's response (or timeout) precedes the next request — **in-flight depth is always 1**. A
  regression to concurrency would interleave requests. Also: an ECU that answers `19 02` but times
  out on `22 2000` must still appear WITH its faults and `info_supported:false` (7572ddf), not as a
  bare error.

### A6 — `read_fault_detail` — the P2.3 order + F_SEVERITY gate
- **Precondition:** a stored fault to point at (from A4/A5). Call:
  `read_fault_detail { ecu: "0x12", code: "<a code from A4>" }`
- **Wire:** ISTA's order **`19 09` → `19 06` → `19 04`** — EXCEPT on `d72n47a0`, whose SGBD declares
  `F_SEVERITY = nein`, so **`19 09` is SKIPPED**: you should see only **`19 06` then `19 04`** on the
  DDE. (On an ECU whose `.prg` has `F_SEVERITY = ja`, e.g. the F25 CAS `cas4_2`, all three appear.)
- **Check:** on the DDE, `tshark … -Y 'hsfz.data[0:2]==19:09'` returns ZERO for this call. The two
  that appear are in the order `06` then `04`. Freeze-frame fields (mileage etc.) decode with a unit
  only where the SGBD gives a real one — mileage (`F_UW_KM`) has NO unit (it is a raw counter).

### A7 — `read_data` (a live measurement)
- **Call:** `read_data { ecu: "0x12", name: "ITOEL", variant: "d72n47a0" }` (oil temp; on-car proven)
- **Wire:** the dynamic-define sequence `2C 01 F303 …` then `22 F303` (NOT a static `22 4517`, which
  the DDE rejects — the misrouted-measurement guard prevents that).
- **Check:** a scaled value with a unit (e.g. ~46 °C). No `7F 22 31` on a `22 4517`.

### A8 — `list_service_function_ids` (the write-tier discovery surface)
- **Call:** `list_service_function_ids { ecu: "0x12", variant: "d72n47a0" }`
- **Wire:** NONE (pure DB read). **Check:** returns catalog `function_id`s with titles + hold summary
  + operator text — these are the ids Phase C's `run_service_function` takes. If empty, the semantic
  DB was not rebuilt (§0.1).

---

## Phase B — RESILIENCE (mostly analysis; one physical VIN test)

### B1 — Sequential depth (P1.2) — analysis only
Re-use the A5 capture. Confirm in-flight depth 1 as in A5's check. No new car action.

### B2 — Retry (P1.1) — observational
klartext repeats a **timed-out READ once** (`RETRY_COMM=1`), never a negative, never a write. You
cannot reliably force a timeout on demand, but IF an ECU times out during A5, confirm in the pcap
that its request appears **twice** (the retry), then either an answer or a final failure. A negative
response (`7F …`) must appear only ONCE (never retried).

### B3 — VIN mismatch abort (P1.3) — needs both cars OR a cable move
- **Only if practical:** connect to car 1, then move the ENET cable to car 2 (different VIN) and call
  `connect {}` again.
- **Expected:** the second `connect` **FAILS** with a message naming both VINs, and the old session
  is closed (a subsequent read reports "not connected"). This is ISTA's hard-abort parity. On the
  wire you will see the new `22 F1 90` read (that is what detects the different VIN).
- **`unreadable` does NOT abort** — if a VIN read simply fails, connect proceeds.

---

## Phase C — WRITES (PHYSICAL; explicit per-step authorisation)

> **STOP.** Everything below changes the car. `clear_all_faults` **drops terminal 15 for 15
> seconds** (the dash goes dark and returns). `run_service_function` **actuates a component**. Do
> NOT proceed without the owner present and each step OK'd out loud. The safety model is: the tool
> refuses unless you pass `confirm: true`, the transmit gate physically cannot send an unconfirmed
> write, and a failed write is always torn down. But the wire effects are REAL.

### C1 — `clear_faults` on ONE ECU (the smaller write) — P0.1 no-reset
- **Call:** `clear_faults { ecu: "0x12", confirm: true }`  (without `confirm` it refuses — verify
  that first: call with `confirm:false`, expect a refusal, no wire traffic to the clear).
- **Wire:** pre-read `19 02 0C` → extended session `10 03` → **`14 FF FF FF`** → and **NOTHING after
  it**. Specifically **NO `11 xx`** (P0.1: klartext sends no ECUReset; ISTA doesn't either).
- **Check:** `tshark … -Y 'hsfz.data[0]==11'` for this call returns **ZERO**. The census is exactly
  `22 F190`(if reconnected)/`19 02 0C`/`10 03`/`14 FF FF FF`.

### C2 — `clear_all_faults` — the full P2.1 sequence ⚠️ DROPS IGNITION 15 s
- **This is the big one. The ignition will switch off for 15 seconds and back on.** Confirm the owner
  is ready (car in park, nothing depending on continuous power).
- **Call:** `clear_all_faults { confirm: true }`
- **Wire, in ISTA's order:**
  1. pre-read `19 02 0C` (+`22 20 00`) per ECU,
  2. **functional clear `14 FF FF FF` to target `0xDF`** (one broadcast — many ECUs answer),
  3. **physical `14 FF FF FF`** only to stragglers (fitted ∧ had faults ∧ silent on the broadcast),
  4. **gateway ZFS `31 01 40 00 00`** to `0x10` (`G_ZGW`),
  5. **the clamp cycle to `0x40`: `31 01 10 01 06 06 A8`** (KL15 OFF) → **~15 s gap** →
     **`31 01 10 01 0A 0A 43`** (KL15 ON),
  6. 500 ms → re-identification → 200 ms → a whole-vehicle verification read.
- **Checks (the parity proof):**
  - a `14 FF FF FF` addressed to **`0xDF`** exists (the functional broadcast — new this session),
  - `31 01 40 00 00` to `0x10` exists (gateway ZFS),
  - the two clamp frames `31 01 10 01 06 06 A8` and `…0A 0A 43` to `0x40`, ~15 s apart,
  - **NO `11 xx` anywhere** in the whole sequence,
  - **the six supplier info-memory clears (`IS_LOESCHEN_TMS` etc.) are NOT transmitted** — this is
    the known, documented gap; the result note says so. Confirm their absence matches the note.
- **Result note:** if the clamp ON failed, the note leads with `URGENT: terminal 15 may still be
  DOWN` — if you see that, tell the owner to press start/stop immediately.

### C3 — `run_service_function` — actuation via the VM (P3) ⚠️ MOVES A COMPONENT
- **Pick a low-consequence function** from A8's list (e.g. an electric-fan actuation). Confirm with
  the owner what it moves.
- **Refusal check first:** `run_service_function { ecu, function_id, confirm: false }` → refuses,
  names the function, no wire traffic.
- **Call:** `run_service_function { ecu: "0x12", variant: "d72n47a0", function_id: <id>, confirm: true }`
- **Wire:** `10 03` (session) then the function's phase jobs by rank — a `STEUERN_*` transmits
  `2F …` (inputOutputControl) or `31 01 …` (routineControl) via the BEST/2 VM under the
  **confirmed-write** gate. A read-only path would have REFUSED this SID at the seam — its appearance
  on the wire is the proof the confirmed-write bridge works.
- **Checks:**
  - the actuation SID (`2F`/`31`/`2E`) reaches the car (only possible under confirmed_write),
  - if the function is non-holding, a Reset/return-to-safe frame follows (e.g. fan release
    `2F FF FF 00`),
  - **no flashing SID ever** (`34`–`37`) — refused under every policy.

### C4 — `stop_service` and held-teardown-on-disconnect (P3 ruling 2)
- **If** you ran a *holding* function in C3 (`held: true` in the result), verify BOTH stop paths:
  - `stop_service { ecu, function_id, confirm: true }` → transmits ONLY the Reset phase (the
    return-to-safe), never re-actuates.
  - Alternatively, `disconnect {}` while a hold is outstanding → the Reset frame fires on the wire as
    part of the disconnect (the component is returned to safe even if you forget to stop it).
- **Check:** the Reset SID appears on `stop_service`/`disconnect`; Main is NOT re-sent.

### C5 — end
`disconnect {}` (also stops any outstanding hold). Confirm the socket closes (FIN).

---

## 5. Pass/fail summary to record

For each test, note: **PASS** (wire matches), **FAIL** (mismatch — record the actual bytes), or
**N/A** (not run). The load-bearing ones, in priority order:

| # | Property | The one check |
|---|---|---|
| A4 | fault mask | `19 02 0C`, never `19 02 FF` |
| A4 | info fold-in | `22 20 00` present; response has NO version byte |
| A5 | sequential | in-flight depth always 1 |
| A6 | detail order + gate | DDE: `19 06`→`19 04`, NO `19 09` |
| C1 | no post-clear reset | NO `11 xx` after `14 FF FF FF` |
| C2 | functional clear | `14 FF FF FF` to `0xDF` |
| C2 | gateway ZFS | `31 01 40 00 00` to `0x10` |
| C2 | clamp cycle | `…06 06 A8` then `…0A 0A 43` to `0x40`, ~15 s apart |
| C2 | no reset | NO `11 xx` in the whole sequence |
| C3 | confirmed write | a `2F`/`31` actuation reaches the car; no `34`–`37` |
| B3 | VIN mismatch | second `connect` to a different car FAILS |

Anything that FAILs is a real divergence to bring back — capture the exact bytes (`tshark … -x` for
the hex) and the tool's JSON result.

---

## 6. After — data hygiene

- The pcap holds the **VIN** (and MAC). It is BYO data: `captures/`, `*.pcap`, `*.pcapng` are
  gitignored. **Never commit it.**
- Before sharing any excerpt, redact the VIN (the 17 chars after `62 F1 90` in a VIN response) and
  the partial-VIN in HSFZ ident announcements on 6811.
- Keep the pcap locally for flipping any remaining `[verify against capture]` markers to confirmed.

---

## What this protocol does NOT cover (out of scope this session)

- **Permanent DTCs (`19 15`)** — deliberately NOT implemented; ISTA reads them only inside FASTA
  field-data export, never in the fault path (see `2026-07-19-research-permanent-dtc.md`). You should
  see NO `19 15` from klartext.
- **Supplier info-memory clears** — selected but not transmitted (C2's known gap).
- **KMM-based ECU presence, `XEP_RULES` virtual/combined faults** — unbuilt (source logic is
  unreadable; awaiting an owner decision).
