# On-car verification protocol — M11 framings (run over the MCP server)

**Date:** 2026-07-04 · **Car:** F20 (N47 diesel, ZGW gateway) · **Status:** ready to run.

## Why this exists

Several frame layouts in klartext are **derived from disassembly + ISO, not yet observed on
the wire** — they carry a `[verify against capture]` marker in the code. The daily read path
works, but the exact byte offsets are unconfirmed. This protocol drives the real car through the
MCP server with **raw-frame tracing on**, so the exact request/response bytes land in a log I can
read and use to flip each marker to confirmed (or fix it).

**Update (2026-07-05) — the REQUEST framings are now confirmed offline.** The M11 read *requests*
(`22 3F07` SVT, `22 3F06` FA, `22 100B` I-Stufe, and the `19 04`/`19 06` freeze-frame
subfunctions) were byte-confirmed against the F20's own gateway SGBD
(`data/Testmodule(1)/Ecu/zgw_01.prg`, deobfuscated XOR 0xF7 from offset 0xA0). So this on-car
protocol now targets the **response byte LAYOUTS only** — the "What's unconfirmed" column below is
already response-side (count/stride, header offsets, record framing); the request bytes it sends
are settled.

**The old pcap (`captures/…2026-07-03`) had none of this traffic** — no `0x19`, no `0x22 3F07/
3F06/100B`. So this is the first capture of it.

**Update (2026-07-06) — now also the BEST/2 VM's fallback capture.** Phase 1 of the BEST/2 job
engine (`klartext-best`) is built and validated offline, and the oracle proved the F20's *passive*
read jobs are raw-only (the SGBD bytecode emits the raw response; `klartext-semantic` scales it from
the `SG_FUNKTIONEN` table in Rust — see the BEST/2 spec §0/§12). So the VM's value is **job
execution** — the step-by-step *service functions* (e.g. oil-level: the procedure that revs the
engine to a stable ~1000 rpm, holds ~3 min, then measures), not passive live-data reads. Verifying
that class of job, and the exact **response byte layouts** they consume, is the one thing offline
`.prg` work cannot fully settle. **So this protocol now doubles as the capture that unblocks the VM
if offline-only proves insufficient:** run it with the **pcap ON (now recommended, not optional —
Part 0 step 6)** so a byte-exact request/response record exists to (a) confirm the live exchange
path the VM's `xsend`/comm bridge drives, and (b) pin the multi-result response layouts. **The VM
must return multiple values per read, not a single value** (a structured `RES_`-table DID decodes
into many named sub-results with per-field masks/scaling) — so the capture must include at least one
*structured/multi-result* read and, if safely possible, one *service function*, both listed in
Part 3.5.

### What this run confirms (each maps to a `[verify against capture]` marker)

| Read | Frame | What's unconfirmed | Source |
|---|---|---|---|
| SVT / fitted list | `62 3F 07` | count offset (u16 BE?) + **1 byte per ECU** stride (no trailing status/variant byte?) | design §3.2, §9.1 |
| Vehicle order (FA) | `62 3F 06` | fixed-header field offsets, `STAT_VERSION` value, SALAPA packing | design §3.3, §9.2 |
| I-Stufe | `62 10 0B` | factory-vs-current layout | design §3.4, §9.3 |
| Per-ECU identification | `62 F1 xx` | which `F1xx` DIDs each ECU actually answers | design §1 |
| DTC list | `59 02` | record framing (3-byte DTC + 1 status) — never captured | protocol-ref §1.5 |
| Freeze-frame detail | `59 04 / 06 / 09` | snapshot/extended/severity record layout (M11 Item 1) | freeze-frames §3, §9 |
| Live measurement | `2C … / 22 …` | already byte-confirmed 2026-07-03 — re-confirm opportunistically | sgbd §7a |
| Multi-target scan | interleaving | does the ZGW tolerate concurrent requests to different ECUs? | design §2.1 [verify live] |

## Two roles

- **Human (you):** hardware setup, launch the MCP server with frame tracing, capture its stderr,
  and send me the results. You never touch the car software beyond launching the server.
- **On-car Claude (an MCP client — Claude Desktop or Claude Code with the klartext-mcp server
  connected):** runs the tool sequence in Part 3. It only calls **read** tools. It never clears.

---

## Part 0 — Setup (human)

1. **Physical + network** (see `skills/klartext-service` for the full ritual):
   - ENET cable in the OBD port; the car awake (terminal 15 / ignition on, engine off is fine).
   - Host link-local IP on the ENET interface (`169.254.x.x/16`); firewall allows TCP 6801 / UDP 6811.
   - Confirm reachability: the gateway answers discovery (or you know its IP, e.g. `169.254.90.33`).
2. **Data (BYO, gitignored):** the ISTA semantic DB (`data/klartext-semantic.db`) and the SGBD
   `.prg` dir (`data/Testmodule(1)/Ecu/`) — for names + the DDE freeze-frame decode.
3. **Build:** `cargo build -p klartext-mcp --release` (workspace is green as of this branch).
4. **Launch the MCP server WITH FRAME TRACING, stderr → a log file:**
   ```bash
   RUST_LOG=klartext_client=trace \
   ./target/release/klartext-mcp \
     --gateway 169.254.90.33 \
     --sgbd-dir "data/Testmodule(1)/Ecu" \
     2> captures/frames-$(date +%Y%m%d).log
   ```
   - `KLARTEXT_SEMANTIC_DB` defaults to `data/klartext-semantic.db`; pass `--gateway` if discovery
     is flaky (skips the UDP broadcast). stdout stays the JSON-RPC transport — **only** stderr is
     the log.
   - Every UDS request/response now appears in the log as
     `HSFZ TX src=0xF4 tgt=0xNN <hex>` / `HSFZ RX src=0xNN <hex>` — that is the capture.
5. **Connect this server to your MCP client** (Claude Desktop config, or `claude mcp add`), so the
   on-car Claude can call its tools. Hand the on-car Claude **Part 3** as its checklist.

6. **Raw-Ethernet pcap — RECOMMENDED (was optional; now the BEST/2 VM's fallback record).** In
   parallel with the server, capture the wire:
   ```bash
   doas tcpdump -i <enet-iface> -w captures/on-car-$(date +%Y%m%d).pcapng 'tcp port 6801 or udp port 6811'
   ```
   The `frames.log` has the decoded UDS bytes and is enough for the M11 layout checks; the **pcap is
   what unblocks the BEST/2 VM** if offline `.prg` work isn't sufficient — it preserves the full
   HSFZ framing + timing of every request/response, so the VM's live exchange path and the
   multi-result response layouts can be validated byte-for-byte against a real session. Keep it (it
   is BYO-data — VIN inside; `captures/` stays gitignored, never committed).

---

## Part 1 — Capture hygiene (human, ongoing)

- `captures/` is gitignored — keep it that way. **`frames.log` contains the real VIN** (in the
  `62 F1 90 …` response) and part numbers. It's BYO-data: never commit it.
- When you send me results, you may **redact the VIN** — replace the 17 ASCII bytes after
  `62 F1 90` with `XX …`. I need the *framing* (offsets, lengths, which DIDs answer), not the VIN
  value. Same for any obviously-personal ASCII.

---

## Part 2 — Safety (both)

- Every tool in Part 3 is a **read** (`0x22`/`0x19`/`0x2C`) — autonomous-safe, no confirmation.
- **Do NOT run `clear_faults` or `clear_all_faults` during this protocol.** We want the stored
  faults present so `read_fault_detail` has freeze-frames to read. Clearing destroys exactly the
  data we're trying to capture.
- If a tool errors, that's useful data — record it and continue; don't retry-loop.

---

## Part 3 — The tool sequence (on-car Claude runs these, in order)

For each step: call the tool, and paste back **(a)** the tool's JSON result and **(b)** a one-line
note if anything looked wrong. The matching `HSFZ TX/RX` lines are captured in `frames.log`
automatically — you don't need to read them, the human sends the whole log.

1. **`connect`** — establish the session.
   - Expect: success, and a `HSFZ TX … 22 F1 90` (VIN read) in the log soon after.

2. **`identify_vehicle`** — the headline read. Exercises the SVT, VIN, I-Stufe, FA, and per-ECU
   identification in one call.
   - Eyeball: `vin` is a real 17-char VIN; `ecus` lists a plausible fitted set (~15 modules) with
     names (e.g. DDE, FEM, DSC, EKPS, EGS…); `i_stufe` looks like an integration level string;
     `vehicle_order.version` is set and `raw_hex` is non-empty; each ECU's `identification` has a
     few `F1xx` fields.
   - Log will contain: `22 3F 07` (+ `62 3F 07 …` ← **the SVT bytes**), `22 F1 90`, `22 10 0B`
     (`62 10 0B …` ← **I-Stufe**), `22 3F 06` (`62 3F 06 …` ← **the FA bytes**), and a burst of
     `22 F1 xx` per ECU.
   - ⚠ If `ecus` is empty or wrong, or `vin` is missing, STOP and report — the SVT framing may be
     off (that's the #1 thing to catch).

3. **`scan_ecus`** — the fitted list on its own (should match `identify_vehicle`'s `ecus`).
   - Then **`scan_ecus` with `rescan: true`** — forces a fresh SVT read (a second `62 3F 07`
     sample confirms the framing is stable, not a fluke).

4. **`read_faults`** on **two or three ECUs** that `identify_vehicle` showed as fitted — pick ones
   likely to have stored faults (e.g. the DDE `0x12`, the FEM `0x40`). Use the ECU name or hex.
   - Eyeball: fault codes + status flags decode to something plausible.
   - Log will contain `19 02 FF` → `59 02 …` ← **the DTC record framing** (never captured before).
   - Note the `not_tested_count` and whether any relevant faults came back.

5. **`read_fault_detail`** for **one fault code** from step 4 (pass its `code_hex` + the same ECU).
   - This is the freeze-frame read: `19 04` / `19 06` / `19 09` → `59 04/06/09 …`.
   - Eyeball: snapshot fields (mileage, RPM, temperatures) — do the values look sane, or garbled?
   - ⚠ If the DDE (`0x12`, variant `d72n47a0`) has a fault, prefer it — its SGBD decode is the one
     we can fully check. Report whether fields decoded or fell back to raw.

6. **`read_data`** — one live measurement on the DDE. First **`list_measurements`** for `0x12`
   (needs the SGBD), pick one (e.g. oil temperature or RPM), then `read_data` it.
   - Log will contain the `2C 03 … / 2C 01 … / 22 F3 03 …` dynamic-read sequence → confirms the
     M6 path still matches on this session.

7. **`read_all_faults`** — the whole-car read (scans the SVT list, reads each ECU's faults).
   - This exercises **multi-target interleaving** (concurrent requests to different ECUs). Note the
     wall-clock; if it errors or stalls, that tells us the ZGW's concurrency tolerance.

8. **`disconnect`** — clean shutdown.

---

## Part 3.5 — BEST/2 VM captures (on-car Claude, if the pcap is running)

These add the traffic the VM needs; they are all **reads** (autonomous-safe). Skip only if a tool
isn't exposed — never substitute a write.

1. **A structured / multi-result read.** On a non-DDE ECU that uses `RES_`-table decoding (e.g. the
   DSC `0x29`, EPS, or the instrument cluster), `list_measurements` then `read_data` **one**
   measurement whose response carries several sub-values (a status/bitfield block, not a single
   scalar). This is the case the VM must decode into **multiple named results** — the pcap gives the
   real multi-field response bytes to pin the layout.
   - Note in your paste: the ECU, the measurement name, and how many distinct values you'd expect.

2. **One service-function read-back, IF one is exposed as a pure read** (`list_service_functions` /
   the read side of a routine). Do **NOT** run any function that moves a component, revs the engine,
   or changes state — those stay CLI + human-in-loop, never in this protocol. If nothing qualifies
   as a pure read, **skip this step and say so** — it just means the service-function capture waits
   for a supervised CLI session.

The pcap (Part 0 step 6) records the frames automatically; you don't paste bytes, just the JSON
results + the notes above.

---

## Part 4 — What to send me

1. **`captures/frames.log`** (VIN redacted if you prefer) — the raw HSFZ TX/RX lines. This is the
   most important artifact.
2. The **JSON results** of `identify_vehicle`, one `read_faults`, the `read_fault_detail`, and
   `scan_ecus` (the on-car Claude pastes these).
3. Any **anomalies**: a tool that errored, a value that looked garbled, an ECU that didn't answer,
   the `read_all_faults` timing.

That's everything I need. From `frames.log` I'll confirm — byte by byte — the SVT count/stride,
the FA field offsets, the I-Stufe layout, the DTC and freeze-frame record framing, and the
identification DID set, then flip the `[verify against capture]` markers to confirmed (or file the
corrections) and fill in the capture-gated FA field decode against the real bytes.

## Part 5 — The checks I'll run (reference)

- **`62 3F 07`**: strip the `62 3F 07` echo → is the next 1 byte / 2 bytes the ECU count? Does
  `5 + count` (or `3 + count`) equal the frame length, i.e. exactly one address byte per ECU?
- **`62 3F 06`**: locate `STAT_VERSION`, then the fixed header (Baureihe / Typ / Lack / Polster /
  Zeitkriterium) offsets, then the SALAPA block — decode against the `TABKOMPRIMIERUNG` 2-bit map.
- **`62 10 0B`**: ASCII? one I-Stufe or two (factory + current)?
- **`59 02`**: confirm 4-byte records (3-byte DTC + 1 status), matching `decode_dtcs`.
- **`59 04/06/09`**: confirm the snapshot/extended/severity preamble offsets from the freeze-frame
  spec §3.
- **Identification**: tabulate which `F1 xx` each ECU answered positively vs `7F 22 31`.

---

## Part 6 — BEST/2 live read path (car session 1)

**Added 2026-07-07 (Item 5 P2).** The BEST/2 job engine (`klartext-best`) now runs a real EDIABAS
job end to end: `job run STATUS_LESEN ARG ITOEL` decodes the DDE's own bytecode, builds the
BMW-FAST request telegram, exchanges it through a **read-only gate**, and scales the response into
named results. All of it is unit-tested and **oracle-green offline** (the VM's `STATUS_LESEN` == 
`measurement.rs` ±1e-6; a DSC read decodes 15 named stems). What offline `.prg` work **cannot** 
settle is the one thing this session captures: that a **real ECU accepts the telegram we transmit** —
specifically our **ADDITIVE checksum** (`CalcChecksumBmwFast`, the wrapping `u8` SUM of the frame,
`crates/best/src/telegram.rs` §"The checksum is ADDITIVE") — and answers with the framing our
decoder expects. The scaled values themselves stay `[verify against capture]` until this run
(`crates/best/src/engine.rs` line 22; the MCP `run_job` note, `mcp/src/server.rs:923`).

**The frozen facts this session confirms on the wire** (all four were frozen offline in
`crates/best/tests/differential.rs:6-22` and are unverified against real hardware until now):

| # | What's unconfirmed | How this run confirms it | Marker it flips |
|---|---|---|---|
| 1 | The **request telegram shape** — `[0x80\|len][target][source][uds…][cksum]`, e.g. `83 12 F1 22 45 17` (a static `0x22` read of DID `0x4517`), not the bare `[0x22,hi,lo]` | the pcap/`frames.log` shows the exact TX bytes for `job run … ITOEL` | `differential.rs:13-15`; `telegram.rs` doc |
| 2 | The **additive checksum a real ECU accepts** — a live ECU verifies the checksum of what we send; a bad one gets no/garbled answer | the ECU **answers** (`62 45 17 …`) at all ⇒ our TX checksum was wire-correct; `JOB_STATUS=OKAY` (not `ERROR_ECU_INCORRECT_LEN`) is the litmus the codec is right | `telegram.rs` §checksum; `engine.rs:22` |
| 3 | The **response telegram layout** — `[0x80\|len][0xF1][ecu][62 …][cksum]`, which the job length-checks (`total == 1 + headerSize + dataLen`, `resp[1]==0xF1`, `resp[2]==ecu`) | the RX bytes in the pcap match, and the job reaches its own scaling path rather than the error path | `differential.rs:16-20` |
| 4 | **Multi-value surfacing** — one structured `RES_`-table response must decode into several named sub-results, not one scalar | the DSC read (6.2 step 2) returns several distinct `STAT_*` stems in its `sets` | `differential.rs:8.4`; §"The VM must return multiple values per read" (Part 0 update) |

Run the CLI capture (6.1) **and** the MCP capture (6.2); they exercise the same engine through the
two faces (CLI: `SessionBridge` over the client; MCP: `run_job` over the same bridge behind the
read-only gate). Keep `tcpdump` (Part 0 step 6) running for both — the pcap is the byte-exact record.

### Part 6.1 — CLI capture (human, with `tcpdump` on)

The CLI drives the car directly (no MCP server needed). With the ENET link up (Part 0 step 1) and
the pcap running (Part 0 step 6), route the CLI's frame trace to a log the same way:

```bash
# 1. Offline sanity: list the DDE's BEST/2 jobs from its SGBD (no car needed).
klartext --sgbd "data/Testmodule(1)/Ecu/d72n47a0.prg" --target 12 job list \
  2>> captures/cli-frames-$(date +%Y%m%d).log
#    Expect: a sorted list of the DDE's job names incl. STATUS_LESEN, STATUS_MESSWERTE_BLOCK, …

# 2. Live read: run the oil-temperature status job against the car.
RUST_LOG=klartext_client=trace \
klartext --gateway-ip 169.254.90.33 \
  --sgbd "data/Testmodule(1)/Ecu/d72n47a0.prg" --target 12 \
  job run STATUS_LESEN ARG ITOEL \
  2>> captures/cli-frames-$(date +%Y%m%d).log
```

- `job run STATUS_LESEN ARG ITOEL` joins the args into the EDIABAS buffer `ARG;ITOEL` (the
  discovered grammar: `SPALTE;value`, **not** bare `ITOEL`, which the job rejects as
  `ARGUMENT_SPALTE='ITOEL' not valid`).
- **Expect** result sets containing `STAT_MOTOROEL_TEMPERATUR_WERT` (the scaled °C value),
  `…_EINH` (unit, `degC`), `…_INFO` (`gefilterte Öltemperatur` — note the correct `Ö`, see §6.4),
  and `JOB_STATUS = OKAY`.
- ⚠ If `JOB_STATUS = ERROR_ECU_INCORRECT_LEN` (or the CLI reports a transport/checksum error), STOP
  and send the log — that means the codec's framing or checksum is off, which is exactly what this
  session exists to catch (marker #2/#3 above).
- The `frames.log` / pcap will show `HSFZ TX … 83 12 F1 22 45 17`-shape TX and a `62 45 17 …` RX.

### Part 6.2 — MCP capture (on-car Claude, via the Part 0 server)

With the MCP server from Part 0 running (frame trace + pcap on), the on-car Claude calls `run_job`.
Paste back the **full JSON** each returns (VIN redaction doesn't apply — these carry no VIN).

1. **DDE single-value read** — the CLI job's twin through the MCP face:
   ```json
   run_job { "ecu": "DDE", "job": "STATUS_LESEN", "args": ["ARG", "ITOEL"] }
   ```
   - Expect one `sets` entry whose `NamedValue`s include `STAT_MOTOROEL_TEMPERATUR_WERT` (`kind:"R"`),
     `…_EINH`/`…_INFO` (`kind:"S"`), and `JOB_STATUS = OKAY`; `note` unset (nothing truncated).

2. **A structured / multi-result read on a NON-DDE ECU** — the case the VM must decode into several
   named sub-results (marker #4). First `list_measurements` for the **DSC** (`0x29`), pick a
   status/bitfield measurement (the wheel-speed / deflation-detection block, `RES_0x4005`), then:
   ```json
   run_job { "ecu": "DSC", "job": "STATUS_LESEN", "args": ["ARG", "<the RES_ measurement name>"] }
   ```
   - Expect **several distinct `STAT_*` stems** in the returned `sets` — e.g. bitfield bits like
     `STAT_WARNUNG_AKTIV`, scalars like `STAT_DSC_SIGNAL_VR`, and a table-mapped
     `STAT_DEFLATION_POSITON_TEXT` — with `JOB_STATUS = OKAY`. A single scalar back means the
     `RES_`-table walk didn't fire; note that and send the log.
   - In your paste, record: the ECU, the measurement name, and **how many distinct values you got**
     vs expected (this is the multi-value confirmation).

### Part 6.3 — Safety (both)

- Every step here is a **`0x22` / `0x2C` / `0x19` READ**. `run_job` and `job run` execute the ECU's
  bytecode behind a **read-only gate** that refuses any write/actuation service **at the transmit
  boundary** — a write-emitting job dies at the seam with no frame sent. The gate is belt-and-braces;
  the human still runs the session.
- **No `clear`, no write, no service function that moves a component.** Do not run `clear_faults`,
  `clear_all_faults`, or any `service run`/actuation during this protocol. `STATUS_LESEN` is a pure
  read; keep it that way.
- If a job errors, that is useful data (see the `ERROR_ECU_INCORRECT_LEN` note) — record it and
  continue; don't retry-loop.

### Part 6.4 — Note on `_INFO` text (resolved offline 2026-07-07)

The DDE `…_INFO` field carries German text (`gefilterte Öltemperatur`). Its decode was **fixed
offline** in P2: EDIABAS holds string-register text as **CP1252** (`Encoding.GetEncoding(1252)`),
one byte per char, and the VM now encodes/decodes that boundary with `klartext_sgbd::cp1252`
(previously a UTF-8 write split `Ö` into two bytes that read back as mojibake). So `_INFO` should
already read as clean German in this capture — **no on-car action needed**; just eyeball that the
umlauts render correctly and flag it only if they don't.

### Part 6.5 — What to send me (this session)

1. **`captures/on-car-<date>.pcapng`** and the CLI/MCP `frames.log`s — the byte-exact TX/RX record
   (the single most important artifact for flipping markers #1–#3).
2. The **JSON** of the two `run_job` calls (6.2) and the CLI `job run` stdout (6.1).
3. Any anomaly: a non-`OKAY` `JOB_STATUS`, a checksum/transport error, or the DSC read returning a
   single value instead of several. From these I confirm the live telegram exchange byte-for-byte,
   flip the four `[verify against capture]` markers to confirmed (or file the correction), and close
   the last open item on the BEST/2 live read path.
