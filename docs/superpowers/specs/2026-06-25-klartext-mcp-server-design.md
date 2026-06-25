# klartext-mcp — M4 design spec

**Date:** 2026-06-25
**Milestone:** M4 — stdio MCP server exposing klartext's diagnostic *reads* as tools.
**Status:** approved (design), pre-implementation.

## Goal

A long-lived stdio MCP server, `klartext-mcp`, that lets an AI client (Claude
Desktop / Claude Code) connect to a BMW F-series car over HSFZ and read + reason
about its faults and live data in natural language. It is a thin MCP surface over
the existing layers: it reuses `klartext-client` (connection/session/keepalive) and
`klartext-semantic` (decoding). It adds **no** new protocol or car logic.

Success: the user adds the binary to their MCP client (absolute path, ISTA
SQLiteDB path in env), plugs into the F20, asks "what faults does my car have,"
and Claude calls the tools, reads real decoded faults, and reasons about them.

## Scope

**In:** five read-only tools — `connect`, `read_faults`, `read_data`, `list_ecus`,
`disconnect`. Structured (`Json<T>`) output written for an AI caller. Session held
in server state with the client's background keepalive. Config via env/args.
stderr-only logging.

**Out (do not build):** any mutating tool — clear-DTC, actuation (IO control,
state-changing routine control), coding/NCD writes, flashing. These stay CLI-only,
where a human is explicitly in the loop (the blast-radius rule applied to the
autonomous-agent surface). The server simply never defines them; a test asserts the
advertised tool set is exactly the five reads.

## Latitude note

The user granted permission to improve M1–M3 and adjust CLAUDE.md where it makes
sense. Applied conservatively:

- **`klartext-semantic`: add `Catalog::ecus()`** — a read-only query returning the
  DB's ECU address↔name map. Needed for DB-driven `list_ecus`; additive, no change
  to existing behavior.
- **Session model stays reconnect-per-target (option A below)**, *not* the
  now-unlocked single-connection retarget (option C). A is more robust: each read is
  a clean single-ECU session and it does not depend on the keepalive/S3 timing the
  report flags `[verify against capture]`. Reconnects to a local gateway are
  sub-millisecond.
- **CLAUDE.md**: after implementation, confirm the `mcp/` crate in the layout notes
  and that `klartext-semantic` now exposes the ECU map. No structural rewrite.

## Architecture

### Crate & placement

New **binary** crate `mcp/` (top-level, per CLAUDE.md "MCP server → `mcp/`"),
package `klartext-mcp`, binary `klartext-mcp`. Added to workspace `members`.

Dependencies: `klartext-client`, `klartext-semantic`, `klartext-hsfz`
(addresses/ports/timeout defaults), `klartext-uds` (`ALL_DTC_STATUS_MASK`, `Dtc`),
`rmcp = { version = "1", features = ["server", "macros", "transport-io"] }`,
`tokio`, `serde`, `serde_json`, `schemars` (via `rmcp::schemars`), `tracing`,
`tracing-subscriber`, `clap`, `anyhow`. All shared versions via
`[workspace.dependencies]`.

### Modules (focused files)

- `main.rs` — parse args/env into `ServerConfig`; init stderr tracing; build
  `KlartextServer`; `server.serve(stdio()).await?.waiting().await?`.
- `config.rs` — `ServerConfig` (gateway_ip, bind, broadcast, port, tester,
  connect_timeout, read_timeout, discovery_wait, semantic_db). Helper to build a
  `klartext_client::ClientConfig` for a given target ECU.
- `ecu.rs` — built-in BMW-wide alias table; `resolve(spec, Option<&Catalog>)`;
  `list(Option<&Catalog>)`; raw-address parsing.
- `session.rs` — `Connection { gateway_ip, vin, target, client }`; the
  `Arc<Mutex<Option<Connection>>>` state; establish (discover/direct) + ensure-target
  (reuse or drop+reconnect) logic, kept out of the tool bodies.
- `server.rs` — `KlartextServer { config, state, tool_router }`; the five `#[tool]`
  methods; `#[tool_handler] impl ServerHandler` with `get_info` carrying AI-facing
  instructions.
- `dto.rs` — request/response types deriving `Serialize`/`Deserialize`/`JsonSchema`.

### Session model (chosen: A — reconnect per target)

`DiagnosticClient`/`Session` fix the target ECU at session-open, but tools name an
ECU per call. The HSFZ transport (report §2.4/§4.1) addresses ECUs by the TARGET
byte on one connection to the gateway; three ways to bridge that to per-call ECUs:

- **A. One held connection, reconnect on target change — CHOSEN.** State holds one
  `Connection`. A read resolves `ecu`→address; if it equals the held `target`, reuse
  the warm session; else drop it and `DiagnosticClient::connect(gateway_ip,
  config_for(target))`. One diagnostic connection at a time (does not assume the
  gateway allows concurrent sessions). Keepalive holds the connection across the AI's
  idle gaps between tool calls. Cost: a sub-millisecond TCP reconnect when switching
  ECUs.
- B. One cached connection per ECU — warmer for rapid multi-ECU, but assumes
  concurrent gateway sessions (unverified) and runs N keepalives. Rejected.
- C. Add retargeting to the client (one connection, vary TARGET per frame) —
  cleanest on the wire, now permitted, but relies on the unverified keepalive/S3
  timing for non-default sessions. Rejected for robustness; revisit if a capture
  confirms the timing.

`connect` establishes the held session to the ZGW (0x10). `disconnect` drops it
(`Session`'s `Drop` aborts the keepalive). The server does **not** connect on
startup.

### ECU model (DB-driven, general — not single-car)

The semantic DB's `ecu` table is built from ISTA's `XEP_ECUVARIANTS ⋈
XEP_ECUGROUPS` — the universal BMW ECU model (every variant/address across models),
not filtered to one car. So a DB-sourced map generalizes.

- **Built-in alias table** (small, BMW-wide documented conventions from report
  §2.4, each `[verify against capture]`): `ZGW`→0x10, `DME`→0x12, `CAS`→0x40. This is
  the always-available fallback and gives friendly names without the DB. It is *not*
  F20-specific.
- **`Catalog::ecus()`** (new) returns the DB's distinct `(address, group_name)`
  rows — the general map — when a DB is present.
- **`ecu` parameter resolution**, in order: (1) raw address `0x12`/`18` (always
  works, no DB); (2) built-in alias, case-insensitive; (3) DB `group_name` `d_0012`;
  else a clear error naming `list_ecus`.
- **`list_ecus`** merges the built-in aliases with the DB map (deduped by address),
  marking each entry's `source` (`builtin`/`db`) and whether the DB was available.

This is general, works without the DB, friendly, and flexible.

### Semantic DB (BYO-data, optional)

Opened **read-only per call** from `config.semantic_db` (mirrors the CLI; sidesteps
`rusqlite::Connection: !Sync` with no shared mutable state — open is cheap). Absent
or unreadable → degrade to status flags + raw codes, log one stderr warning, never
fail the read. Path is env/arg only; contents are never embedded or committed.

### Logging (stdout discipline)

`tracing_subscriber::fmt().with_writer(std::io::stderr).with_ansi(false)` with an
env filter (default `info`). **Zero** `println!`/`print!`/`dbg!`/`eprint!`-to-stdout
anywhere in the crate. stdout carries only the rmcp JSON-RPC stream. rmcp logs via
`tracing` (→ our stderr subscriber); tokio/rusqlite do not touch stdout. Verified by
grep in the checklist.

## Tool contracts

All return `Json<T>` (structured output, auto output-schema). Errors are returned as
a clear tool error whose message reaches the caller (e.g. `not connected — call
connect first`), never a panic.

### `connect`
- **When:** before any read; establishes the car connection.
- **Params:** `gateway_ip?: string` — optional IP override; else use the configured
  `--gateway-ip`/env; else auto-discover on the link-local broadcast.
- **Does:** direct-connect or discover to the ZGW (0x10); read DID `F190` for an
  authoritative ASCII VIN, falling back to discovery's best-effort VIN; store the
  `Connection`. Replaces any existing connection (re-plug friendly).
- **Returns:** `{ connected: bool, gateway_ip: string, vin?: string, vin_source:
  "did_f190" | "discovery" | "unknown", target_ecu: "ZGW (0x10)", note: string }`.
- **Errors:** no gateway found / ambiguous / connect failure → clear message.

### `read_faults`
- **When:** to see stored fault codes (DTCs) for a module.
- **Params:** `ecu: string` (name or address per resolution rules).
- **Does:** ensure-target; `read_dtcs(0xFF)` (all stored); decode each DTC's status
  byte to ISO 14229 flags and look up fault text via `Catalog::describe_dtc`.
- **Returns:** `{ ecu: string, address: "0x12", count: u32, faults: [{ code_hex:
  "4A1234", status_hex: "08", status_flags: ["confirmedDTC", …], descriptions: [{
  variant: string, saecode?: string, text?: string }] }], db_available: bool }`.
  Raw code + status are always present as fields.
- **Errors:** not connected; unknown ECU; transport/negative-response → clear
  message.

### `read_data`
- **When:** to read one data identifier (DID), e.g. VIN, part/software numbers.
- **Params:** `ecu: string`; `did: string` (hex, e.g. `F190`, `0xF190`).
- **Does:** ensure-target; `read_did(did)`; decode via `did::decode` (ISO-standard
  0xF1xx name + text rendering when printable). BMW-specific DID scaling is out of
  scope (SGBD), so raw is always returned.
- **Returns:** `{ ecu: string, address: "0x12", did_hex: "F190", name?: "VIN",
  value_text?: "WBA…", raw_hex: "57 42 41 …", note: string }`.
- **Errors:** not connected; unknown ECU; bad DID hex; transport/negative-response.

### `list_ecus`
- **When:** to discover which ECUs the read tools can target.
- **Params:** none. Does **not** require a connection.
- **Returns:** `{ ecus: [{ address_hex: "0x12", names: ["DME"], group_name?:
  "d_0012", source: "builtin" | "db" | "builtin+db" }], db_available: bool, note:
  string }`.

### `disconnect`
- **When:** to tear down the car connection (or before re-plugging).
- **Params:** none.
- **Returns:** `{ was_connected: bool }`. Drops the `Connection` (aborts keepalive).

### ServerHandler `get_info`

Capabilities: tools only (`ServerCapabilities::builder().enable_tools().build()`).
Instructions (for the AI caller) state: this is a **read-only** BMW diagnostic
server; call `connect` first; targets ECUs by name or address (`list_ecus` to
discover); it cannot clear faults, actuate, or code — those are intentionally absent;
fault text/ECU names need the ISTA SQLiteDB, and reads still work (raw) without it.

## M1–M3 changes

1. **`klartext-semantic::Catalog::ecus()`** → `Result<Vec<EcuEntry>, SemanticError>`,
   where `EcuEntry { address: u8, group_name: String }`, `SELECT DISTINCT address,
   group_name FROM ecu ORDER BY address`. Unit test against the existing synthetic
   fixture (extend it with `ecu` rows — no BMW data). Export `EcuEntry`.
2. No `klartext-client`/`klartext-hsfz`/`klartext-uds` changes are required for the
   chosen design. Any further M1–M3 touch is opportunistic quality only and must not
   change existing behavior or signatures.

## Testing (no real car — HIL is manual)

Reuse the M2/M3 loopback mock-gateway pattern (a `TcpListener` on `127.0.0.1:0`
answering HSFZ frames), extended to: accept reconnections in a loop, and answer
`22 F1 90` (VIN), `19 02 <mask>` (a couple of synthetic DTC records), and a sample
`22 <did>`. Drive the tool methods **directly** on a `KlartextServer` built with
`config.gateway_ip = 127.0.0.1`, `config.port = mock_port`, and a **synthetic**
fixture semantic DB (no BMW data, like `catalog.rs`'s fixture):

- `connect` → `connected`, returns the VIN.
- `read_faults("ZGW")` / a DB-mapped address → decoded `status_flags` + description
  text from the fixture; raw fields present.
- `read_data("ZGW", "F190")` → decoded VIN value.
- `read_faults` / `read_data` **without** a prior `connect` → the clear
  "not connected" error (asserted as `Err`, no panic).
- `tool_router.list_all()` advertises **exactly** `{connect, read_faults, read_data,
  list_ecus, disconnect}` and no mutating tool (asserted by name set).

Discovery is already unit-tested in `klartext-hsfz`; MCP tests use the direct-IP
path for determinism. MCP Inspector and the F20 round-trip are manual steps.

## Verify checklist (maps to the milestone)

- [ ] `cargo build` clean; `cargo clippy --all-targets -- -D warnings` clean;
  `cargo fmt --check` clean.
- [ ] Server starts over stdio; initialize + list-tools advertises exactly the five
  read-only tools with correct schemas (integration test + MCP Inspector); no
  mutating tool.
- [ ] Against the mock: `connect`→VIN; `read_faults`→decoded descriptions + status
  flags; `read_data`→decoded value; reads without `connect`→clear "not connected"
  error, not a panic.
- [ ] No stdout pollution: grep clean for `println!`/`dbg!`; logging path is stderr.
- [ ] Server does not connect on startup; only via `connect`.
- [ ] grep confirms no BMW DB contents committed/embedded; SQLiteDB path is env/arg,
  read-only.
- [ ] Ready for manual test (absolute binary path + SQLiteDB env; plug into F20; ask
  for faults).

## Risks / open `[verify against capture]`

- Keepalive/S3 timing for non-default sessions is unverified — the reconnect model
  avoids depending on it. Reads use the default session.
- ECU address conventions (ZGW/DME/CAS) and the `d_00XX`↔address mapping are
  documented but `[verify against capture]`; raw-address targeting is the escape
  hatch.
- VIN from discovery's 0x11 body is best-effort; `read_data F190` from the ZGW is the
  authoritative path and is preferred on connect.
