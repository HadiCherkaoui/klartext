# CLAUDE.md

## What this is
A native-Rust BMW diagnostic and coding tool for a 2014 F-series BMW (F20), talking over Ethernet (ENET cable) directly to the car. Two faces: a library/CLI now, and later an MCP server so an agent can read faults and live data and reason about them. The long-term value is the semantic layer — turning raw protocol exchanges into "here's what's wrong and why" — not the protocol itself.

The protocol spec lives in `docs/protocol-reference.md`. Treat it as the source of truth for frame layouts, the UDS service catalog, the HSFZ handshake, ports, and gateway addressing. Don't re-derive protocol details from memory; read the report.

## Scope (decided — don't expand without being asked)
In:
- Diagnostics: read DTCs, read live data/identifiers, clear DTCs, run service functions (routine control). This is the recurring use and the priority.
- Replay-coding: read a module's NCD, patch known byte changes, write it back. No coding-definition decoding.
Out (do not build unless explicitly asked):
- A general FDL/CAFD editor (editing any coding parameter by name). Deferred; needs a full PSdZData CAFD parser.
- ECU flashing/programming (UDS transfer services 0x34–0x37). Out entirely beyond awareness.

## Architecture
Bottom to top:
1. Transport — HSFZ (BMW-proprietary, F-series) over TCP. Implement concretely now.
2. UDS (ISO 14229) — request/response service layer on top of transport.
3. Semantic — meaning of the bytes (DID scaling, DTC meaning, service-function recipes), built from ISTA's data and captures. A later milestone.

DoIP (ISO 13400) is the G-series transport and a FUTURE addition. Do NOT build a transport trait/abstraction for it yet — there is one transport today. Implement HSFZ as a concrete type behind a clean module boundary; extract a trait when DoIP is actually added. No speculative abstraction.

## Hard rules
- BYO-data. Never commit BMW's proprietary data: ISTA SQLiteDBs, PSdZData, or packet captures (they contain the VIN). Gitignore `captures/`, `*.pcap`, `*.pcapng`, and any data dirs. The repo ships empty of BMW data; the user supplies their own.
- Safety by blast radius (encode as layers are built): reads (DTCs, live data, identifiers) are safe and may run autonomously. Writes — NCD coding writes and actuation (IO control, state-changing routine control) — must require explicit confirmation and must read+back-up the original bytes before writing. Flashing: unsupported.
- License: AGPL-3.0. Implement protocols from the report and ISO standards (frame layouts and handshakes are facts, not copyrightable). Do NOT copy code from reference libraries — especially Scapy (GPLv2), which would force its license. Read them to understand; reimplement in your own code.

## Stack
- Latest stable Rust, edition 2024. Async via tokio.
- Before hand-writing standardized layers, check crates.io: there may be usable UDS crates, and a Rust MCP SDK (check the current crate, e.g. rmcp) for the later MCP milestone. HSFZ is proprietary and niche — write it yourself regardless.
- SQLite parsing for the semantic layer (later) via rusqlite or sqlx — defer until that milestone.
- Cargo workspace (chosen up front for a reusable core that future binaries share). Layout convention: **library crates live under `crates/`; each binary lives in its own top-level directory** — no `bin/` grouping dir. Today: libraries `crates/uds` (pure UDS messages), `crates/hsfz` (concrete HSFZ transport), `crates/client` (managed UDS session + typed read/clear services over HSFZ), `crates/semantic` (ISTA-DB-backed DTC/DID decoding; exposes the general ECU map via `Catalog::ecus()`); binaries `cli/` (`klartext-cli`, builds the `klartext` binary) and `mcp/` (`klartext-mcp`, the read-only stdio MCP server over `klartext-client` + `klartext-semantic`). Dirs are short; package names keep the `klartext-` prefix. Shared versions/metadata via `[workspace.dependencies]` and `[workspace.package]`. Still do NOT pre-create empty crates for layers that don't exist yet — when a milestone needs it, add a new **library** under `crates/` (`klartext-doip`, or a `klartext` facade) and a new **binary** as its own top-level dir.

## Conventions
- Errors: thiserror for library types, anyhow at the binary boundary.
- cargo fmt and cargo clippy -- -D warnings clean before a milestone is done.
- Conventional commits.

<anti-overengineering>
Do NOT:
  - Add traits/interfaces for things with one implementation today (no Transport trait until DoIP exists).
  - Create config systems or plugin layers for values with one setting.
  - Pre-create empty crates for the MCP or semantic layer before their milestone (the workspace exists; its future sibling crates do not — add each when its milestone needs it).
  - Generate "future-proof" abstractions beyond what the current milestone asks.
  - Leave throwaway test/scratch files uncleaned.
Do:
  - YAGNI ruthlessly. One concrete implementation, inlined where single-caller.
  - Hardcode values with one legitimate setting; mark protocol values the report flagged "verify against capture" as clearly-named constants.
  - Write the minimum that passes the milestone's verify checklist.
</anti-overengineering>

## Hardware-in-the-loop
You (Claude) cannot reach the car. Unit-test frame encode/decode against known byte vectors from the report (and from a capture if one is in the repo). The end-to-end test against the real gateway is a MANUAL step the human runs. Keep the two separate — never claim a hardware round-trip works; only that unit tests pass and the manual test is ready.


## MCP server (M4)
- New crate klartext-mcp: a stdio MCP server exposing klartext's diagnostic READS as tools, so an AI client (Claude Desktop / Claude Code) can read and reason about the car. Reuses klartext-client and klartext-semantic; adds no new car/protocol logic.
- Read-first by design; invariant REFINED in M9 (not abandoned) and again in M10 (the one write, batched). Non-mutating tools run freely (connect/discover, read faults, read data, scan_ecus, read_all_faults, list ECUs/measurements/service functions, disconnect). The ONE write exposed is standard UDS 0x14 (the M2 path), non-physical, reversible in that active faults return — as clear_faults (one ECU) and clear_all_faults (iterated over the fitted ECUs); both refuse without confirm=true relayed from the human, both pre-read+record each ECU's DTCs before erasing and post-read to verify. `clear_all_faults` is the SAME frame, batched — not a new capability. The absolute line, unchanged: NO physical actuation (service functions that move components — regen, pumps, EMF, calibration) and NO derived-unconfirmed WRITE frame (service function, actuation, coding) is ever MCP-executable; those writes stay in the CLI where a human is explicitly in the loop. (The M6 dynamic-READ define — 0x2C, session-transient read plumbing, itself disassembly-derived — is the deliberate read-side exception, per the M6 decision.) Blast-radius rule on the autonomous-agent surface: the agent reads and reasons; state changes need the human — via confirm for the standard clear (per-ECU or whole-car), via the CLI for everything else.
- M10 made discovery/naming fully live: `scan_ecus` probes for the FITTED ECUs (not the 170-entry model map); ECU names come from the ISTA DB (no hardcoded aliases — the old "DME"/"CAS" mislabelled this diesel/FEM car); a `variant` resolves via a ladder (explicit → learned per-VIN profile → single DB candidate) so it need not be hand-supplied; one demultiplexed HSFZ connection reaches every ECU; and the server disconnects on exit.
- Uses the official rmcp crate (CURRENT version — verify on crates.io/docs.rs; its macro API has changed across versions, so follow current rmcp examples, not older tutorials).
- stdio transport. CRITICAL: nothing may write to stdout except the JSON-RPC stream — any stdout logging corrupts the transport and the client silently disconnects. Route ALL logging to stderr.
- Same BYO-data boundary: ISTA SQLiteDB path via env/arg, read-only; never embed or commit DB contents.
- Milestone order: M4 MCP server (reads), then later gated service-function recipes / replay-coding (writes) and the SGBD-based DID scaler.

## Standard-PID scaling (M5)
- Extend klartext-semantic with engineering-unit scaling for STANDARD OBD-II / SAE J1979 PIDs only — public, documented formulas (e.g. RPM = ((A*256)+B)/4). No SGBD/proprietary data.
- read_data and read_did return name + scaled value + unit for a recognized standard PID; unrecognized DIDs fall back to the existing named/raw behavior (never error on unknown — degrade to raw).
- Proprietary BMW DIDs (SGBD-defined scaling) are explicitly OUT — that's a later milestone after the SGBD format is cracked. Do not guess proprietary formulas.
- Formulas are pure functions, fully unit-testable offline against known input→output vectors. Real-car confirmation is a later manual step, but the math is verifiable now.
- LIVE FINDING (2026-07-03, verified against the F20 capture): this DDE does NOT map the SAE `0xF4xx` PIDs onto UDS 0x22 — `read_data F40C`/`F405` return `7F 22 31` (requestOutOfRange). The M5 scaler is correct but a no-op on this car; live data must come from the SGBD `SG_FUNKTIONEN` measurements (the M6 path). The capture also confirmed the M6 `2C`/`22` dynamic-read sequence byte-for-byte.

## Next milestones (M11+) — owner-requested, not yet started
See `docs/superpowers/specs/2026-07-03-m11-ista-parity-roadmap.md` for the full brief. In owner-priority order: (1) **freeze-frame metadata on faults** (km/RPM/state at occurrence — UDS `19 04`/`19 06`; today's read is code+status only. NOTE: the fault-memory *filter* already matches what the owner wants — showing active faults is fine — so do NOT change `RELEVANT_MASK`; only add snapshot reads); (2) **SVT + full identification dump** (ISTA-style "which ECUs exist by name + variant" in one gateway read — decide the read-vs-CLI invariant for its `0x31` frames); (3) **`embed-data` cargo feature** (self-contained binary via `include_bytes!`/`SQLite::deserialize` — PERSONAL-USE only, never published, per BYO-data); (4) **ISTA repair-doc catalog** (fault→procedure links from `DiagDocDb`); (5) **BEST-2/ABL guided multi-step procedures** (the big one; shares infra with the inline-scaling tail); (6) **mobile (iOS) over USB-C Ethernet** (Rust core via UniFFI + SwiftUI; manual-IP v1, discovery needs the iOS multicast entitlement). Highest-leverage research step for 1–4: decompile the ISTA .NET DLLs (`ilspycmd`) to read exactly how it reads/clears faults + runs procedures, then reimplement (facts only, never copy — AGPL).

**M11 Item 2 done:** the SVT fitted-list read is UDS `22 3F 07` (a read), exposed on the CLI (`identify`, and `scan` now reads the gateway SVT) and MCP (`identify_vehicle`); the `0x31 STEUERN_VCM_GENERATE_SVT` stays out.

**M11 Item 4 done (link+title layer):** the ISTA repair-doc catalog — fault→doc bridge `RG_ECUFAULT_DOCIDS.ECUFAULT_ID = XEP_FAULTCODES.ID` joined to `XEP_INFOOBJECTS` titles/type/docnumber/safety, extracted into the semantic DB — surfaced OFFLINE (no car) via CLI `fault-docs <code>` and MCP `fault_help`; the 50 GB document PROSE stays deferred (per-language content IDs preserved in `fault_doc`). **Item 3 (`embed-data`) deferred to the mobile milestone (item 6):** desktop payoff is marginal and any bundled `.prg` subset is arbitrary — revisit when the iOS core actually needs a self-contained payload.

**BEST/2 VM — Phase 1 done + a load-bearing finding (2026-07-06).** New crate `klartext-best`: a native-Rust BEST/2 bytecode VM (decoder + machine + ~110 opcodes + `UdsExchange`/mock + `Ecu::run_job`), reviewed byte-faithful against `ediabaslib`/`ediabasx` (read-as-facts, no copy). Spec `docs/superpowers/specs/2026-07-05-best2-vm-job-engine-design.md` (§11–12 = the outcome). **The oracle disproved its own premise:** the F20 DDE's passive read jobs are **raw-only** (bytecode emits the raw response; `klartext-semantic`'s `SG_FUNKTIONEN` table scaler does the scaling in Rust — confirmed across all 272 jobs; the old `sgbd-findings.md §4a` "bytecode scales" sketch was wrong, it mislabelled `atsp`=stack-peek). So **passive live-data scaling stays in Rust; the VM's value is job EXECUTION** — the step-by-step *service functions* (oil-level revs-and-holds-then-measures, DPF regen, calibrations). **Item 5 (BEST-2/ABL guided procedures) is therefore the active next milestone** (its own brainstorm), NOT "Phase 2 live reads". Two owner requirements recorded (2026-07-06): (a) reads must return **multiple named values, not a single scalar** (structured `RES_`-table decode); (b) if offline `.prg` work is insufficient, capture a real **pcap via the MCP protocol** (`docs/on-car-verification-protocol.md`, pcap now recommended). `Operand::Indexed` addressing + `gettmr/wait/jt` stay deferred until a target service-function job needs them.
