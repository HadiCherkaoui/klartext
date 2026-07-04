# klartext

A native-Rust BMW diagnostic & coding tool for F-series cars (target: a 2014 F20), speaking
UDS over the BMW-proprietary **HSFZ** transport across an ENET (Ethernet) cable.

> **Milestones 1–3 (done):** HSFZ transport bring-up, diagnostics (read DTCs / DIDs, clear
> faults, gateway discovery), and the **semantic layer** that turns raw results into meaning
> from the user's own ISTA data. Replay-coding is a later milestone.

## Status

- ✅ HSFZ framing (encode/decode) — implemented from `docs/protocol-reference.md`
- ✅ Async TCP connection — connect + `TCP_NODELAY`, segment reassembly, ack-skip, bounded NRC-0x78 retry
- ✅ UDS reads/clears — TesterPresent, DiagnosticSessionControl, ReadDTCInformation, ReadDataByIdentifier, ClearDiagnosticInformation
- ✅ CLI — gateway discovery, whole-car `scan`, `identify` (VIN / FA / I-Stufe / fitted ECUs), `read-faults`, `fault-docs` (offline ISTA repair-doc lookup), `read-did`, `clear-faults` (per-ECU or `--all-ecus`, gated by `--confirm`), `tester-present`
- ✅ Semantic layer (M3) — DB-backed fault text + ISO 14229 status flags; ISO-standard DID names; sourced from the user's ISTA SQLiteDB
- ✅ Live discovery (M10) — one demultiplexed HSFZ connection reaches every ECU; `scan_ecus` finds the FITTED set; ECU names + SGBD variants resolve from the DB (no hardcoded aliases)
- ✅ Connected to a real F20 (2026-07-03) — the pcap confirmed the HSFZ framing, response SRC/TGT swap, and the M6 `2C`/`22` measurement sequence (see `docs/field-findings-2026-07-03.md`)
- ⏳ Remaining on-car checks — the `59 02` DTC record framing (no fault traffic in the capture) and multi-target request interleaving

HSFZ is reverse-engineered from the report; **no packet capture and no ISTA data are committed**
(BYO-data — they contain the VIN / are BMW-proprietary). A few wire values remain unverified, so
the CLI prints them as a checklist to confirm against your car (see *Verify against a capture*).

## Layout (Cargo workspace)

| Dir | Package | Role |
|---|---|---|
| `crates/uds` | `klartext-uds` | Pure UDS (ISO 14229) message encode/decode. No transport, no async. |
| `crates/hsfz` | `klartext-hsfz` | The concrete HSFZ transport: frame codec + async connection. |
| `crates/client` | `klartext-client` | Managed UDS session + typed read/clear services over HSFZ. |
| `crates/semantic` | `klartext-semantic` | Meaning: raw DTC/DID → human text + scaled values, via the ISTA SQLiteDB and the SGBD `SG_FUNKTIONEN` table (read-only). |
| `crates/sgbd` | `klartext-sgbd` | EDIABAS SGBD (`.prg`) container parser: XOR-`0xF7` body + tables; feeds proprietary measurement scaling. |
| `cli` | `klartext-cli` | The `klartext` binary; composes the crates above. |
| `mcp` | `klartext-mcp` | MCP server over stdio: reads (incl. whole-car scan) + the confirmation-gated clear (`clear_faults`/`clear_all_faults`); no actuation, ever (reuses client + semantic). |

Future sibling: `klartext-doip`. There is deliberately **no `Transport` trait**
yet — one transport exists today; a trait gets extracted when DoIP is added.

## Build & test

```sh
cargo build --workspace
cargo test --workspace                               # unit tests (report byte vectors, DB lookups on synthetic fixtures)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## Semantic database (BYO-data, one-time)

The semantic layer reads a compact SQLite extracted from **your own** ISTA `DiagDocDb`. ISTA ships
that database encrypted; `scripts/build-semantic-db.sh` decrypts it locally and extracts only the
tables klartext needs into `data/klartext-semantic.db` (gitignored — never committed or embedded):

```sh
scripts/build-semantic-db.sh /path/to/ISTA/SQLiteDBs/DiagDocDb.sqlite
# or, with the default path data/Testmodule(1)/SQLiteDBs/DiagDocDb.sqlite:
scripts/build-semantic-db.sh
```

It builds [SQLite3 Multiple Ciphers](https://github.com/utelle/SQLite3MultipleCiphers) from a
pinned, checksum-verified amalgamation (only a C compiler is needed). The cipher/password were
recovered from your ISTA install — see `docs/sqlite-findings.md`. Without this DB, reads still work
and fall back to raw codes.

## Usage

Find the gateway (BMW gateways usually sit on an unconfigured link-local `169.254.x.x` address):

```sh
klartext discover                 # broadcasts 00 00 00 00 00 11 on UDP 6811, dumps replies + source IP
```

Reads auto-discover and connect (or pass `--gateway-ip`); `--target <hex>` selects the ECU:

```sh
klartext scan                             # read the gateway SVT → the FITTED ECUs + each one's faults
klartext scan --ecus-only                 # just the fitted-ECU list (address + name)
klartext identify                         # full vehicle identity: VIN, FA, I-Stufe, fitted ECUs, per-ECU IDs
klartext --target 12 read-faults          # relevant fault text + ISO status flags for the engine (0x12)
klartext --target 12 read-faults --all    # ...also the "not tested this cycle" catalog entries
klartext --target 12 fault-docs 4B1234    # OFFLINE (no car): fault text + linked ISTA doc titles
klartext read-did F190                     # ReadDataByIdentifier 0xF190 → "VIN": decoded value
klartext read-did 172A --raw               # ...also the underlying bytes
klartext --target 12 clear-faults --confirm   # state change — refuses without --confirm
klartext clear-faults --all-ecus --confirm    # whole-car clear (per-ECU pre-read + verify)
```

Key flags: `--target <hex>` (default `10` = gateway), `--semantic-db <path>` (default
`data/klartext-semantic.db`, env `KLARTEXT_SEMANTIC_DB`), `--gateway-ip`, `--raw`, `--all`,
`--scan-concurrency` (scan concurrency), `--port`, `--timeout`,
`--connect-timeout`. See `klartext --help`.

### What the semantic layer decodes

- **`read-faults`** — the raw 3-byte DTC is mapped to ISTA's fault text per ECU variant, and the
  status byte is decoded into ISO 14229 flags (`testFailed`, `confirmedDTC`, `pendingDTC`, …).
- **`read-did`** — the ISO-standard identification DIDs (0xF1xx, e.g. VIN) are named from the
  report; values render as text when printable. **BMW-specific DID scaling is _not_ in the
  SQLiteDB** — it lives in the EDIABAS SGBD — so arbitrary live-data DIDs show name (if standard)
  plus the raw value. Full physical scaling is deferred to the SGBD path. See `docs/sqlite-findings.md`.
- **`fault-docs`** — **offline, no car connection**: resolves a fault (`--target <ecu>` + DTC code)
  to its meaning plus the ISTA documents linked to it — each one's title, type (`FKB` = fault
  description; others are procedures), doc number, and safety flag — straight from the semantic DB.
  The document prose is a deferred layer (titles/pointers only). See `docs/sqlite-findings.md`.

## MCP server (Claude as diagnostic client)

`klartext-mcp` serves the diagnostics as MCP tools over stdio, so an AI client (Claude Code /
Claude Desktop) can read the car and reason about it. Fourteen tools: `connect`, `scan_ecus`
(the FITTED ECUs, from the gateway SVT), `identify_vehicle` (VIN, FA, I-Stufe, fitted ECUs +
per-ECU identification), `read_faults`, `read_fault_detail` (freeze-frame / snapshot),
`fault_help` (offline ISTA repair-doc lookup — no car), `read_all_faults` (whole car),
`read_data`, `list_ecus`, `list_measurements`,
`list_service_functions`, `disconnect`, and the one confirmation-gated write — standard UDS
0x14 — as `clear_faults` (one ECU) and `clear_all_faults` (whole car). No actuation, no coding, no service-function execution — ever (asserted by test,
down to the frames on the wire). The server disconnects the car on exit. ECU names come from the
DB (no hardcoded aliases); an SGBD `variant` can be passed, learned per-VIN, or resolved from a
single DB candidate.

The server starts with **no data at all** and degrades gracefully; each BYO input unlocks a layer:

| BYO input | Flag / env | Unlocks | Without it |
|---|---|---|---|
| *(none)* | — | `connect`, `scan_ecus`, `read_faults`, `read_all_faults`, `read_data`, `identify_vehicle`, `clear_faults` — raw codes + ISO status flags, standard PIDs/ISO DIDs (without the DB, `identify_vehicle` still reads the SVT/FA/identification but names degrade to raw hex) | — |
| ISTA semantic DB (SQLite) | `--semantic-db` / `KLARTEXT_SEMANTIC_DB` (default `data/klartext-semantic.db`) | human fault text, the fault→ISTA repair-doc catalog (`fault_help`, offline), ECU names/titles + SGBD variant candidates, the per-model ECU map | raw codes, no `fault_help`; target ECUs by hex address only |
| SGBD `.prg` dir | `--sgbd-dir` / `KLARTEXT_SGBD_DIR` | `list_measurements`, `read_data` by *name*, proprietary scaling to value + unit | proprietary DIDs stay raw |
| learned profile | `--profile-dir` (default XDG state) | remembers each ECU's SGBD variant per VIN after a scaled read | pass `variant` each time |

`--gateway-ip` / `KLARTEXT_GATEWAY` pins the gateway; omit it to auto-discover on the ENET link
at `connect` time.

**Claude Code**: the repo ships `.mcp.json` (project-scoped, relative paths — build once with
`cargo build --release -p klartext-mcp`). **Claude Desktop** launches servers with an unspecified
working directory, so use absolute paths in `claude_desktop_config.json`:

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

A session then looks like: *"connect to the car"* → `connect` (discovers the gateway, reads the
VIN) → *"what's actually on this car?"* → `scan_ecus` → *"any faults anywhere?"* →
`read_all_faults` → *"what's the oil temp and DPF soot load?"* →
`list_measurements {variant:"d72n47a0", search:"Öltemperatur"}` →
`read_data {ecu:"0x12", name:"ITOEL", variant:"d72n47a0"}` → scaled `degC` value (after which the
variant is remembered for `0x12` on this VIN). Clearing codes requires the human's explicit
go-ahead: `clear_faults` / `clear_all_faults` refuse without `confirm:true` and warn that
freeze-frames are discarded and readiness monitors may reset. The
`skills/klartext-service` skill teaches Claude this exact workflow.

## Manual hardware test (your step)

We can't reach the car — the unit tests only cover framing/decoding against known vectors and
synthetic DB fixtures, and we never claim a hardware round-trip works. To validate end-to-end:

1. Connect the ENET cable; give your NIC a link-local `169.254.x.x` address; wake the car (terminal 15).
2. `klartext discover` → note the responder's IP (that's your gateway).
3. `klartext --target 12 read-faults` → expect decoded fault descriptions for the DME.
4. Capture the session in Wireshark and confirm the values in *Verify against a capture* — in
   particular the raw-3-byte-DTC → ISTA code mapping the semantic layer relies on.

## Verify against a capture

Most of these were **confirmed against a real F20 capture (2026-07-03)** — items 1, 2, 3, 6, 7,
and 8 below, plus the response SRC/TGT swap and the M6 `2C`/`22` measurement sequence (see
`docs/field-findings-2026-07-03.md`). Still open on-car: **item 9** (the `59 02` DTC record
framing — the capture had no fault-read traffic) and multi-target request interleaving. The CLI
still prints the list after every real run; confirm any remaining item with Wireshark (it has
HSFZ/DoIP dissectors) on the ENET link (report Part 6):

1. **HSFZ LENGTH semantics** — counts `SRC+TGT+UDS` (= `2 + len(UDS)`), excluding the 6-byte
   length+control header. *Highest priority:* the report self-contradicts; resolved via Scapy and
   the `00 00 00 00 00 11` discovery datagram (LENGTH=0 with a control word present).
2. Diagnostic port **TCP 6801**, control/ident port **UDP 6811** — ICOM setups reassign these.
3. Tester address **0xF4**, ZGW/gateway **0x10** — scan targets to see which answer.
4. Connect timeout **5000 ms** (ediabaslib) vs **20000 ms** (EDIABAS.INI) — set via `--connect-timeout`.
5. **P2 = 50 ms / P2\* = 5000 ms** — ISO defaults; the F20 reports its own in the `10 03` response.
6. Control words **0x01/0x02/0x11/0x12** — corroborated but proprietary.
7. **0x11 identification-string layout** — ✅ confirmed `DIAGADR<addr>BMWMAC<mac>BMWVIN<vin>`; the
   VIN is now parsed by anchoring on the `BMWVIN` marker.
8. **Alive-check** — ✅ the session is held by UDS `3E 80` alone (~2 s cadence, gateway-ACKed);
   the capture shows **no** HSFZ `0x12` alive-check frame, so it is not implemented.
9. **DTC numbering** — that the raw 3-byte DTC equals ISTA's `XEP_FAULTCODES.CODE` (24-bit) the
   semantic layer keys on, and the `59 02` record framing. **Still open** — no fault-read traffic
   in the 2026-07-03 capture; needs an on-car scan with faults present.

## Safety

Reads (DTCs, DIDs, identifiers, measurements) are safe and run autonomously. State changes —
`clear-faults` in the CLI, `clear_faults` over MCP — require explicit confirmation. Over MCP that
clear is the *only* write; actuation, service-function execution, and coding are never
agent-invokable and stay in the CLI with a human in the loop. No flashing, period. The semantic
layer is strictly read-only: it opens the SQLiteDB read-only and never reaches the network or
the car.

## License

AGPL-3.0-only. Protocols are implemented from the report and ISO standards (frame layouts and
handshakes are facts, not copyrightable); no code is copied from GPL reference libraries such as Scapy.
