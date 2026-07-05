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

**Optional raw-Ethernet fallback** (if you'd rather have a pcap too): in parallel,
`doas tcpdump -i <enet-iface> -w captures/on-car-$(date +%Y%m%d).pcapng 'tcp port 6801 or udp port 6811'`.
Not required — the `frames.log` already has the decoded UDS bytes.

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
