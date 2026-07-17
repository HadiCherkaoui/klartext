# klartext

A native-Rust BMW diagnostic tool for F-series cars, speaking UDS (ISO 14229) over the
BMW-proprietary **HSFZ** transport across an ENET (Ethernet) cable. Developed against an
F20 1-series; the read stack is also wire-confirmed on an F25 X3.

Two faces: a **library + CLI** today, and a **read-only MCP server** so an AI client can read
the car and reason about it. The long-term value is the **semantic layer** — turning raw
protocol exchanges into "here's what's wrong and why" from your own ISTA data — not the
protocol plumbing.

> **Personal-use interoperability project — ships no BMW data.** Not affiliated with BMW.
> The repository contains no ISTA databases, no SGBD files, no captures, and no VINs (its
> full history was audited to confirm this); the semantic layer runs against data **you
> supply** from an ISTA install you're licensed to use. See **[DISCLAIMER.md](DISCLAIMER.md)**
> and **[LICENSE](LICENSE)** (AGPL-3.0). Talking to a car changes a car — reads are safe,
> writes are your responsibility.

## What works

Bottom to top, all implemented and unit-tested against known byte vectors and synthetic DB
fixtures:

- **HSFZ transport** — frame codec + async TCP: connect with `TCP_NODELAY`, segment
  reassembly, ack-skip, bounded NRC-`0x78` retry; link-local gateway discovery over UDP.
- **UDS services** — TesterPresent, DiagnosticSessionControl, ReadDTCInformation
  (`19 02`/`04`/`06`/`09`), ReadDataByIdentifier (`22`, incl. the `2C` dynamic-define read
  sequence), ClearDiagnosticInformation (`14`).
- **CLI** (`klartext`) — gateway discovery, whole-car `scan`, `identify`
  (VIN / FA / I-Stufe / configured ECUs), `read-faults`, `fault-detail` (freeze-frame),
  `fault-docs` and `ecu-tree` and `measurements` and `job`/`service` listing (all **offline**,
  no car), `read-did`, `info-memory`, live `job run` (BEST/2 read jobs), and the
  confirmation-gated `clear-faults` / `service run`.
- **Semantic layer** — a compact SQLite extracted from *your* ISTA `DiagDocDb`: fault text +
  ISO-14229 status flags, freeze-frame labels, the fault→ISTA-doc catalog, the
  **measurement catalog** ("the index": result name + unit + scaling + owning job), the
  **job-parameter catalog** (how ISTA invokes each job), and the **per-platform ECU tree**
  (the graph view — short names, buses, and the minimal-configuration core that explains
  ISTA's ~11 boxes vs. the gateway's 30+ configured addresses).
- **BEST/2 VM** (`crates/best`) — an offline EDIABAS `.prg` bytecode engine, plus a live
  **read-only** job runner gated at the transmit seam (a job whose bytecode emits any
  write/actuation service is refused before a frame reaches the car).
- **MCP server** (`klartext-mcp`) — 16 stdio tools: the reads above plus the one
  confirmation-gated clear. No actuation, coding, or service-function execution is ever
  agent-invokable (asserted by tests, down to the frames on the wire).

**On-car confirmation.** The read stack, the freeze-frame reads, the `2C`/`22` measurement
scaling, the SVT identity read, and the gated clear are wire-confirmed against a real F20
(2026-07-03) and again on an F25 X3 (2026-07-10) — two different chassis, proving
portability. See `docs/field-findings-2026-07-03.md` and `docs/car-session-1-results.md`.
A few wire values remain capture-gated; the CLI prints them as a checklist after every real
run (see *Verify against a capture*).

Replay-coding (patch a known NCD byte change and write it back) and a general FDL/CAFD
coding editor are **not** implemented — deferred by design. ECU flashing is out of scope
entirely.

## Layout (Cargo workspace)

| Dir | Package | Role |
|---|---|---|
| `crates/uds` | `klartext-uds` | Pure UDS (ISO 14229) message encode/decode. No transport, no async. |
| `crates/hsfz` | `klartext-hsfz` | The concrete HSFZ transport: frame codec + async connection. |
| `crates/client` | `klartext-client` | Managed UDS session + typed read/clear services over HSFZ. |
| `crates/semantic` | `klartext-semantic` | Meaning: raw DTC/DID → human text + scaled values + ISTA catalogs, via the semantic SQLiteDB and the SGBD `SG_FUNKTIONEN` table (read-only). |
| `crates/sgbd` | `klartext-sgbd` | EDIABAS SGBD (`.prg`) container parser: XOR-`0xF7` body + tables; feeds proprietary measurement scaling. |
| `crates/best` | `klartext-best` | Offline BEST/2 (EDIABAS bytecode) VM + a read-only live job runner. |
| `cli` | `klartext-cli` | The `klartext` binary; composes the crates above. |
| `mcp` | `klartext-mcp` | Read-only MCP server over stdio: reads + the confirmation-gated clear; no actuation, ever. |
| `docbuild` | `klartext-docbuild` | Build-only: renders ISTA repair-doc bodies and the ECU-tree topology into sibling DBs (BYO-data). |

Future sibling: `klartext-doip` (the G-series transport). There is deliberately **no
`Transport` trait** yet — one transport exists today; a trait gets extracted when DoIP is
actually added.

## Build & test

```sh
cargo build --workspace
cargo test --workspace                               # unit tests: report byte vectors, DB lookups on synthetic fixtures
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

We can't reach the car from CI — unit tests only cover framing/decoding and synthetic DB
fixtures, and we never claim a hardware round-trip works. The end-to-end car test is a manual
step (below).

## Semantic database (BYO-data, one-time)

The semantic layer reads a compact SQLite extracted from **your own** ISTA `DiagDocDb`. ISTA
ships that database encrypted; `scripts/build-semantic-db.sh` decrypts it **locally** and
extracts only the tables klartext needs into `data/klartext-semantic.db` (gitignored — never
committed or embedded):

```sh
scripts/build-semantic-db.sh /path/to/ISTA/SQLiteDBs/DiagDocDb.sqlite
# or, with the default path data/Testmodule(1)/SQLiteDBs/DiagDocDb.sqlite:
scripts/build-semantic-db.sh
```

It builds [SQLite3 Multiple Ciphers](https://github.com/utelle/SQLite3MultipleCiphers) from a
pinned, checksum-verified amalgamation (only a C compiler is needed), then also emits — when
the sibling `xmlvalueprimitive_*.sqlite` stores are present — the repair-doc bodies and the
ECU-tree topology via `klartext-docbuild`. The cipher/password are the public ISTA constants
documented in `docs/sqlite-findings.md` (see also [DISCLAIMER.md](DISCLAIMER.md)). Without
this DB, reads still work and fall back to raw codes.

## Usage (CLI)

Find the gateway (BMW gateways usually sit on an unconfigured link-local `169.254.x.x`):

```sh
klartext discover                 # broadcasts the ident datagram on UDP 6811, dumps replies + source IP
```

Reads auto-discover and connect (or pass `--gateway-ip`); `--target <hex>` selects the ECU:

```sh
klartext scan                             # gateway SVT → configured ECUs (+ ISTA names/bus/core) + each one's faults
klartext identify                         # full identity: VIN, FA, I-Stufe, configured ECUs, per-ECU IDs
klartext --target 12 read-faults          # fault text + ISO status flags for the engine (0x12)
klartext --target 12 fault-detail 4B1234  # freeze-frame / snapshot metadata for one DTC
klartext --target 12 fault-docs 4B1234    # OFFLINE: fault text + linked ISTA doc titles
klartext --target 12 info-memory          # the ECU's secondary/info memory (Infospeicher, 22 2000)
klartext read-did F190                     # ReadDataByIdentifier 0xF190 ("VIN") → decoded value
klartext --sgbd d72n47a0.prg read-did 4517 # a proprietary measurement, scaled to value + unit
klartext --target 12 clear-faults --confirm   # state change — refuses without --confirm
klartext clear-faults --all-ecus --confirm    # whole-car clear (per-ECU pre-read + verify)
```

Offline catalog queries (no car — just the semantic DB and/or an SGBD `.prg`):

```sh
klartext ecu-tree F25_1404                     # ISTA's ECU graph for a platform, grouped by bus, * = core
klartext measurements --variant d72n47a0       # readable signals (ISTA catalog: name + unit + reading job)
klartext --sgbd d72n47a0.prg measurements oel  # ...from the SGBD, filtered, each readable live via read-did
klartext --semantic-db ... job args STATUS_BLOCK_LESEN --variant d72n47a0  # how ISTA invokes a job
klartext --sgbd d72n47a0.prg job list          # the BEST/2 jobs the ECU's SGBD defines
klartext --sgbd d72n47a0.prg service list      # service functions, tagged by blast-radius risk
```

Live BEST/2 job execution (read-only — the gate refuses any write the bytecode emits):

```sh
klartext --sgbd d72n47a0.prg --target 12 job run STATUS_MOTORTEMPERATUR
```

Key flags: `--target <hex>` (default `10` = gateway), `--semantic-db <path>` (default
`data/klartext-semantic.db`, env `KLARTEXT_SEMANTIC_DB`), `--sgbd <ecu>.prg` (proprietary
scaling), `--gateway-ip`, `--raw`, `--all`, `--scan-concurrency`, `--port`, `--timeout`,
`--connect-timeout`. See `klartext --help`.

### What the semantic layer decodes

- **`read-faults`** — the raw 3-byte DTC maps to ISTA's fault text per ECU variant, and the
  status byte decodes into ISO 14229 flags (`testFailed`, `confirmedDTC`, `pendingDTC`, …).
- **`read-did`** — ISO-standard identification DIDs (0xF1xx, e.g. VIN) are named from the
  report. Proprietary BMW measurement scaling lives in the EDIABAS SGBD, not the SQLiteDB:
  pass `--sgbd <ecu>.prg` to scale a live measurement to value + unit (dynamic `2C`-define
  measurements are read via the selektiv-lesen sequence); without it, unknown DIDs degrade to
  the raw value.
- **`fault-docs`** / **`ecu-tree`** / **`measurements`** / **`job args`** — **offline, no
  car**: the fault→ISTA-document catalog, the per-platform ECU graph, the readable-signal
  index, and how ISTA invokes each job, straight from the semantic DB. Repair-document prose
  is a deferred layer (titles/pointers today).

## MCP server (Claude as diagnostic client)

`klartext-mcp` serves the diagnostics as MCP tools over stdio, so an AI client (Claude Code /
Claude Desktop) can read the car and reason about it. **Sixteen tools:** `connect`,
`disconnect`, `scan_ecus` (configured ECUs from the gateway SVT, annotated with ISTA
names/bus/core), `identify_vehicle`, `read_faults`, `read_fault_detail` (freeze-frame),
`read_all_faults` (whole car), `read_info_memory` (Infospeicher), `read_data`, `list_ecus`,
`list_measurements`, `list_service_functions`, `fault_help` (offline ISTA repair-doc lookup),
`run_job` (a **read-only** BEST/2 job), and the one confirmation-gated write — standard UDS
`0x14` — as `clear_faults` (one ECU) and `clear_all_faults` (whole car). No actuation, no
coding, no service-function execution — **ever** (asserted by test, down to the frames on the
wire). The server disconnects the car on exit.

The server starts with **no data at all** and degrades gracefully; each BYO input unlocks a
layer:

| BYO input | Flag / env | Unlocks | Without it |
|---|---|---|---|
| *(none)* | — | `connect`, `scan_ecus`, `read_faults`, `read_all_faults`, `read_data`, `identify_vehicle`, `read_info_memory`, `clear_faults` — raw codes + ISO status flags, standard PIDs/ISO DIDs | names degrade to raw hex |
| ISTA semantic DB | `--semantic-db` / `KLARTEXT_SEMANTIC_DB` | human fault text, the fault→doc catalog (`fault_help`), ECU names/titles + variant candidates, the measurement/ECU-tree catalogs | raw codes, no `fault_help`; target ECUs by hex only |
| SGBD `.prg` dir | `--sgbd-dir` / `KLARTEXT_SGBD_DIR` | `list_measurements`, `read_data` by *name*, `run_job`, proprietary scaling to value + unit | proprietary DIDs stay raw |
| learned profile | `--profile-dir` (default XDG state) | remembers each ECU's SGBD variant per VIN after a scaled read | pass `variant` each time |

`--gateway-ip` / `KLARTEXT_GATEWAY` pins the gateway; omit it to auto-discover on the ENET
link at `connect` time.

**Claude Code**: the repo ships `.mcp.json` (project-scoped, relative paths — build once with
`cargo build --release -p klartext-mcp`). **Claude Desktop** launches servers with an
unspecified working directory, so use absolute paths in `claude_desktop_config.json`:

```json
{
  "mcpServers": {
    "klartext": {
      "command": "/home/you/klartext/target/release/klartext-mcp",
      "args": [
        "--semantic-db", "/home/you/klartext/data/klartext-semantic.db",
        "--sgbd-dir", "/home/you/klartext/data/Testmodule(1)/Ecu"
      ]
    }
  }
}
```

A session then looks like: *"connect to the car"* → `connect` → *"what's on this car?"* →
`scan_ecus` → *"any faults anywhere?"* → `read_all_faults` → *"what's the oil temp?"* →
`list_measurements {variant:"d72n47a0", search:"Öltemperatur"}` →
`read_data {ecu:"0x12", name:"ITOEL", variant:"d72n47a0"}` → a scaled `degC` value (after
which the variant is remembered for `0x12` on this VIN). Clearing codes requires the human's
explicit go-ahead: `clear_faults` / `clear_all_faults` refuse without `confirm:true` and warn
that freeze-frames are discarded and readiness monitors may reset.

## Manual hardware test (your step)

We can't reach the car — the unit tests only cover framing/decoding against known vectors and
synthetic DB fixtures, and we never claim a hardware round-trip works. To validate end-to-end:

1. Connect the ENET cable; give your NIC a link-local `169.254.x.x` address; wake the car (terminal 15).
2. `klartext discover` → note the responder's IP (that's your gateway).
3. `klartext --target 12 read-faults` → expect decoded fault descriptions for the DME.
4. Capture the session in Wireshark and confirm the values in *Verify against a capture*.

## Verify against a capture

Most of these were **confirmed against a real F20 (2026-07-03) and an F25 X3 (2026-07-10)** —
items 1–3, 6–9 below, plus the response SRC/TGT swap and the `2C`/`22` measurement sequence
(see `docs/field-findings-2026-07-03.md` and `docs/car-session-1-results.md`). Still open
on-car: DSC structured multi-result decoding and CP1252 `_INFO` text. Wireshark has HSFZ/DoIP
dissectors for the ENET link (report Part 6):

1. **HSFZ LENGTH semantics** — counts `SRC+TGT+UDS` (= `2 + len(UDS)`), excluding the 6-byte
   length+control header. Resolved via the discovery datagram (LENGTH=0 with a control word).
2. Diagnostic port **TCP 6801**, control/ident port **UDP 6811** — ICOM setups reassign these.
3. Tester address **0xF4**, ZGW/gateway **0x10**.
4. Connect timeout **5000 ms** (ediabaslib) vs **20000 ms** (EDIABAS.INI) — `--connect-timeout`.
5. **P2 = 50 ms / P2\* = 5000 ms** — ISO defaults; the car reports its own in the `10 03` response.
6. Control words **0x01/0x02/0x11/0x12** — corroborated but proprietary.
7. **0x11 identification-string layout** — ✅ `DIAGADR<addr>BMWMAC<mac>BMWVIN<vin>`; the VIN is
   parsed by anchoring on the `BMWVIN` marker.
8. **Alive-check** — ✅ the session is held by UDS `3E 80` alone (~2 s cadence, gateway-ACKed);
   the captures show **no** HSFZ `0x12` alive-check frame, so it is not implemented.
9. **DTC numbering** — ✅ the raw 3-byte DTC equals ISTA's `XEP_FAULTCODES.CODE` (24-bit) the
   semantic layer keys on, and the `59 02` records are 4 bytes each (confirmed on the F25).

## Safety

Reads (DTCs, DIDs, identifiers, measurements, info memory) are safe and run autonomously.
State changes — `clear-faults` and the gated low-risk `service run` in the CLI, `clear_faults`
over MCP — require explicit confirmation and back up the original bytes first. Over MCP that
clear is the **only** write; actuation, service-function execution, and coding are never
agent-invokable and stay in the CLI with a human in the loop. Live `job run` is read-only —
the gate refuses any write/actuation service the bytecode emits, before it reaches the car.
No flashing, period. The semantic layer is strictly read-only: it opens the SQLiteDB
read-only and never touches the network.

## License

**AGPL-3.0-only** — see [LICENSE](LICENSE) and **[DISCLAIMER.md](DISCLAIMER.md)**. Protocols
are implemented from public write-ups and ISO standards (frame layouts and handshakes are
facts, not copyrightable); no code is copied from GPL reference libraries such as Scapy or
ediabaslib.
