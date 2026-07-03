# Design: live ECU discovery + fully-dynamic core (M10)

**Date:** 2026-07-03 · **Status:** in progress on `fix/list-ecus-null-address`
**Sources:** `docs/field-findings-2026-07-03.md` (the first live-car session) + the owner's
follow-up requests: remove hardcoded values, make ECU discovery fully live, show only
fitted ECUs, whole-car fault read, whole-car fault clear, multi-step functions,
MCP disconnect on exit, optimization.

## 1. Problem

The first live session (F20, N47 diesel) exposed a cluster of related problems:

1. **The generic ECU map ≠ the car.** `list_ecus` returns ISTA's whole 170-address model
   map; the car has ~15 modules. Probing an absent address (EGS `0x18` on this
   manual-gearbox car) blocks on the full read timeout — a whole-car scan stalls.
2. **Hardcoded, wrong names.** `BUILTIN_ALIASES` labels `0x12` "DME" (this car is a
   diesel DDE) and `0x40` "CAS" (it is the FEM on F20). The alias made the assistant
   mis-call the engine petrol.
3. **One TCP connection per (tester, ECU) pair.** Retargeting reconnects TCP; the session
   and keepalive are bound to a single target. A multi-ECU scan pays a full reconnect per
   module and can never overlap probes.
4. **Fault noise.** `19 02 FF` returns the supported-DTC catalog including ~147
   status-`0x40` "not tested this cycle" entries on the FEM — the real faults drown.
5. **Hand-supplied `variant` everywhere.** Every SGBD-backed tool needs the caller to
   know the ECU's SGBD variant (e.g. `d72n47a0`); there are 153 candidates for address
   0x12 in the ISTA DB.
6. **Swallowed errors.** `if let Ok(x) = fallible()` silently discarded the NULL-address
   query failure while reporting `db_available: true` — the root cause of the
   3-builtins bug this branch fixed.
7. **Lifecycle.** Killing the MCP without `disconnect` leaves the car session to time
   out server-side; there is no signal handling or shutdown cleanup.

## 2. Decisions (approaches weighed)

### 2.1 Multi-target session over ONE TCP connection (demultiplexed)

**Chosen:** one `Session` per gateway TCP connection; the target ECU address moves from
connection state to a **per-request parameter**. A background reader task routes each
response frame to the pending request for that frame's **source address** (HSFZ frames
carry SRC/TGT both ways, so responses are attributable). At most one in-flight request
per target; N different targets may be in flight concurrently (bounded by the caller).
Keepalive (`3E 80`) goes to the gateway and holds the link.

Alternatives rejected:
- *Per-request short timeout, still lockstep* — simple, but a fitted-155-address scan
  costs ~47 s serially; concurrency gets it under ~8 s.
- *Keep reconnect-per-target* — the current design; N TCP handshakes per scan and it
  can never overlap probes.

Risk: whether the ZGW tolerates interleaved requests to different targets is
**[verify live]** (ISTA does parallel ECU communication; our pcap is lockstep). The scan
concurrency is a knob (`--scan-concurrency`, default 8); `1` degrades to strictly
sequential probing, which must remain correct.

Correctness note: matching by source address (not just SID) also fixes a latent bug —
a late response from a timed-out probe can no longer be mis-attributed to the next
request that happens to share a SID. **The capture confirms this is sound**: a response
from ECU `0x12` carries HSFZ source `0x12` (request `f4 12` → response `12 f4`), so the
frame's source address unambiguously identifies the answering ECU (§6).

### 2.2 Fitted-ECU discovery = live probe scan (SVT read deferred)

**Chosen:** probe each address from the ISTA DB map (170 addresses) with **TesterPresent
`3E 00`** and a short per-probe timeout (default 300 ms, configurable). *Any* frame from
the source address — positive `7E 00` or a negative response — proves presence; silence
means absent. Results are cached in the MCP session (`rescan: true` refreshes).

Why not the SVT (Systemverbautabelle) from the ZGW — the way ISTA learns the fitted
list *and* each ECU's variant? Because deriving the `STEUERN_VCM_*` job frames from
`zgw_01.prg` requires BEST-2 bytecode disassembly (the ediabasx oracle), which is not in
this repo, and the VCM jobs are RoutineControl-class (`0x31`) — a derived-unconfirmed
non-read frame, which the MCP invariant forbids executing. **The SVT read is the right
future milestone** (it would also solve variant auto-detection); this design gets the
fitted list with standard, autonomous-safe reads today. Documented in §6.

Probe choice: `3E 00` is universal (every UDS ECU supports TesterPresent in the default
session), side-effect-free, and forces a reply. `10 03`/`22 F190` were rejected: the
first changes session state, the second is not served by every module.

### 2.3 Fault relevance partition

**Chosen:** keep requesting `19 02 FF` (one request, full information), then partition
client-side with a **relevance mask `0xAF`** — a DTC is *relevant* when any of
`testFailed (0x01) | testFailedThisOperationCycle (0x02) | pending (0x04) |
confirmed (0x08) | testFailedSinceLastClear (0x20) | warningIndicatorRequested (0x80)`
is set; a status of only `testNotCompletedSinceLastClear (0x10)` and/or
`testNotCompletedThisOperationCycle (0x40)` is catalog noise ("not tested"). Tools
return relevant faults plus a `not_tested_count`; `include_not_tested: true` (MCP) /
`--all` (CLI) shows everything. Rejected: requesting a narrower mask (e.g. `19 02 2D`)
— saves nothing material and loses the noise count and the raw picture.

### 2.4 Whole-car operations as concrete multi-step procedures

**Chosen:** implement the two requested whole-car procedures as explicit orchestrations
in `klartext-client`, shared by MCP and CLI:

- **`scan`** — probe all catalog addresses → for each present ECU, read + partition its
  faults (bounded-concurrent). One ECU's failure is recorded, never aborts the scan.
- **`clear all`** — for each present ECU (from a scan): pre-read faults → extended
  session → standard `14 FF FF FF` → post-read verify → per-ECU report
  `{before, cleared, verified_clean}`. Strictly sequential — writes stay lockstep.

A *generic* sequence/step engine (ISTA ABL guided procedures: preconditions → operator
prompts → write → verify loops) is **deliberately deferred** to the BEST-2/ABL
milestone; building the abstraction now, from two hardcoded procedures and zero ABL
semantics, is the kind of speculative scaffolding CLAUDE.md forbids. The field-findings
doc already records it as a future direction.

### 2.5 MCP invariant refinement: the one write, batched

`clear_all_faults` is **the same write** the invariant already admits — standard UDS
0x14, non-physical, reversible-by-reappearance — iterated over fitted ECUs, still behind
`confirm: true`, still pre-read and post-verified per ECU. The absolute line is
unchanged: no physical actuation, no derived-unconfirmed write frame, ever. CLAUDE.md
and the `klartext-service` skill get the refinement recorded (whole-car clear also
discards *every* module's freeze-frames — the skill tells the agent to enumerate what
will be lost before asking for the go-ahead).

### 2.6 Variant resolution ladder (no hardcoding, honest limits)

Resolution order for an SGBD `variant` when a tool needs one:

1. **Explicit parameter** (unchanged) — always wins.
2. **Learned car profile** — a small JSON per VIN (`~/.local/state/klartext/profiles/
   <VIN>.json`, dir configurable): `address → variant`, recorded automatically when an
   explicit-variant read **succeeds with a scaled value** on that ECU. Pass `d72n47a0`
   once; from then on `read_data`/`list_measurements`/`list_service_functions` default
   to it for 0x12 on this car.
3. **DB-unique candidate** — when the ISTA DB lists exactly one variant for the address
   whose `.prg` exists in `--sgbd-dir`, use it (logged, surfaced in the result).
4. **Fail with candidates** — the error lists the DB's variant names for that address
   (with their human titles), so the caller picks instead of guessing blind.

Rejected as primary mechanisms: the `.grp` ident tables (`HW_TABELLE` etc. — verified
to cover only legacy-era variants; this car's `d72n47a0` is absent from every
`d_0012.grp` table, the FEM group has no tables at all — UDS-era identification lives in
IDENTIFIKATION bytecode) and guessing identification DIDs from memory. True
auto-detection arrives with the SVT milestone (§6).

### 2.7 ECU naming from data (drop `BUILTIN_ALIASES`)

- `scripts/build-semantic-db.sh` v2: the `ecu` table gains `title_en`, `title_de` (from
  `XEP_ECUVARIANTS.TITLE_ENGB/TITLE_DEDE`). The DB is rebuilt from the owner's
  DiagDocDb (BYO, gitignored, as before).
- `Catalog::ecus()` returns per-address entries with group names + a representative
  title; `Catalog::variants(address)` returns the per-address variant candidates
  (name + title) for resolution ladders and error messages.
- Display heuristic: when several ISTA groups share an address, the canonical name is
  the `d_00XX` group whose hex matches the address; others are listed as extras.
- `BUILTIN_ALIASES` is **deleted**. `resolve()` accepts a hex address, an ISTA group
  name, or a **variant name** (each variant maps to one address). Well-known aliases
  ("DME", "CAS") are gone — they were wrong on this car. Without the DB the tool is
  raw-hex-only and says so (no misleading names).
- The gateway constant `ZGW_ADDRESS = 0x10` stays — it is a protocol-report constant
  (the HSFZ routing endpoint), not a per-car alias.

### 2.8 Lifecycle: disconnect on exit

`klartext-mcp` handles SIGINT/SIGTERM and stdin-close (client exit) uniformly: stop
serving, take any held connection, drop it cleanly (aborts keepalive, closes TCP), log
to stderr. `KlartextServer` exposes `disconnect_now()` for the shutdown path; the
`disconnect` tool is unchanged.

### 2.9 Optimization

- **No reconnect on retarget** (falls out of §2.1) — switching ECUs is free.
- **Catalog cached** in server state (was: SQLite open per tool call).
- **Measurements/ServiceFunctions cached per variant** in server state (was: full
  `.prg` parse per call — the DDE table alone is 1787 rows).
- Whole-car fault reads run bounded-concurrent over fitted ECUs.

### 2.10 Swallowed-error policy

Every `if let Ok`/`.ok()` that can hide a real failure either propagates or logs the
`Err` at `warn` to stderr. `list_ecus` reports `db_error` when the catalog failed
instead of pretending `db_available: false` was a choice. Fixtures gain NULL /
mixed-storage-class rows (the class of bug that caused the 3-builtins regression);
`describe_dtc` and the new queries get the same NULL-hardening as `ecus()`.

### 2.11 The committed VIN session log

`captures/SESSION-2026-07-03.md` (contains the VIN) was force-added despite the
gitignore — a BYO-data violation per CLAUDE.md. Fix: `git rm --cached` in this branch
**and** drop commit `371186f` from the branch history before merge (rebase; the branch
is unmerged feature work). The file stays on disk, ignored, as intended.

## 3. Component changes

### `crates/uds`
- `dtc::status::RELEVANT_MASK: u8 = 0xAF` + `Dtc::is_relevant()` (+ tests). Nothing else.

### `crates/hsfz`
- Unchanged (framing already carries addresses; `read_frame`/`write_frame` reused).
  HSFZ error control words (0x40–0x45, 0xFF) get names so the session can log them.

### `crates/client` (the core refactor)
- `Session`: background **reader task** owns the read half; `pending:
  HashMap<u8, Pending>` (keyed by target address) routes responses; `request(&self,
  target, uds)` and `request_with_timeout(&self, target, uds, timeout)`; per-target
  one-in-flight enforced; NRC `0x78` keeps the slot pending (existing retry bound);
  unmatched frames and HSFZ error frames are logged, never mis-delivered. Keepalive
  targets the gateway. `Drop` aborts both tasks.
- `DiagnosticClient`: methods take `target: u8` (`read_dtcs(target, mask)`,
  `read_did(target, did)`, `clear_all_dtcs(target)`, …). `ClientConfig` loses `ecu`.
- New: `probe(target, timeout) -> Probe` (`3E 00`; `Present {latency, kind} | Silent`),
  `scan_present(&[u8], ScanOptions) -> Vec<ProbeResult>`,
  `scan_faults(&[u8], ScanOptions) -> Vec<EcuFaults>` (present → read → partition;
  per-ECU errors recorded),
  `clear_faults_verified(target) -> ClearReport` (pre-read → extended → clear →
  post-read verify) and its whole-car iteration.
- `ScanOptions { probe_timeout: Duration = 300ms, concurrency: usize = 8 }`.

### `crates/semantic`
- Extract script v2 (`title_en`, `title_de` columns; rebuild instructions unchanged).
- `Catalog::ecus()` → `Vec<EcuSlot { address, group_name, extra_groups, title }>`;
  `Catalog::variants(address) -> Vec<VariantInfo { name, title }>`;
  both NULL-hardened and backward-compatible when the DB lacks the new columns
  (older extracts keep working, titles just come back `None`).

### `mcp`
- `ecu.rs`: aliases deleted; resolve = hex | group | variant; list built from
  `Catalog::ecus()`; db errors surfaced.
- `server.rs`: cached `Catalog` + per-variant `Measurements`/`ServiceFunctions`;
  variant ladder (explicit → profile → db-unique → error-with-candidates);
  `read_faults` partition + `include_not_tested`;
  **new tools:** `scan_ecus` (live fitted list; cached; `rescan`),
  `read_all_faults` (scan + per-ECU relevant faults),
  `clear_all_faults` (confirm-gated whole-car clear with per-ECU verify).
- `profile.rs`: VIN-keyed learned profile (load/save atomic, `--profile-dir`,
  `--no-profile`).
- `main.rs`: signal handling + shutdown disconnect (§2.8).
- `session.rs`: one connection, no retarget-reconnect; holds `fitted` scan cache.
- Tool descriptions rewritten to steer agents: live data via `list_measurements` +
  SGBD variants (standard `F4xx` PIDs are **known not to work** on this car's DDE —
  the note from the live session).

### `cli`
- `scan` subcommand (fitted ECUs + per-ECU relevant fault summary + timing),
  `clear-faults --all-ecus` (confirm-gated, per-ECU verify report),
  `read-faults` gains the partition (+`--all`), `--probe-timeout`/`--scan-concurrency`
  flags; `--target` now feeds per-call targets.

### Docs / skill
- `skills/klartext-service/SKILL.md`: connection prerequisites (NetworkManager,
  firewall, DHCP/link-local, ZGW wake ritual), fitted-scan workflow, whole-car clear
  guidance, standard-PID limitation, disconnect hygiene.
- `docs/field-findings-2026-07-03.md`: items checked off with pointers.
- `CLAUDE.md`: invariant refinement (§2.5); `docs/standard-pids.md`: field result.
- `README.md`: new tools/flags.

## 4. Testing

- **Mock car** (loopback TCP gateway) hosting several mock ECUs by target address;
  absent addresses stay silent. Drives: scan finds exactly the fitted set; absent
  probes cost ~probe-timeout not read-timeout; concurrency > 1 overlaps (scan of N
  absents completes ≪ N × probe-timeout); late/stray responses are dropped by source
  matching; whole-car clear = pre-read → `10 03` → `14 FF FF FF` → post-read per ECU.
- **Partition tests** on masks (0x40-only noise vs 0x08/0x01/0x2C relevant …).
- **Profile tests**: learn on scaled success, reuse, explicit override, corrupt file.
- **Catalog fixtures** with NULL addresses, NULL titles, mixed storage classes; old
  extract schema (no title columns) still lists ECUs.
- `#[ignore]`d real-DB/real-SGBD tests extended for the new columns.
- **`scan_vin` regression** (§6): a synthetic `DIAGADR…BMWMAC…BMWVIN<vin>` body asserts the
  marker-anchored parse returns the true VIN, not the false prefix run.
- Live checks the owner runs (not claimed by CI): scan on the F20 (expects ~15 fitted,
  no hang on 0x18), concurrency knob, whole-car read < ~10 s, clear-all on a junk ECU,
  and — since this pcap lacks `0x19` traffic — the `59 02` DTC record framing.

## 5. Out of scope (explicit)

- **SVT/VCM read** (fitted list + variants from the gateway, ISTA-style) — next
  milestone; needs BEST-2 disassembly of `zgw_01.prg` VCM jobs + an invariant decision
  for its RoutineControl frames.
- **BEST-2/ABL interpreter & generic guided-procedure engine** — future direction.
- **Standard-PID → SGBD equivalence mapping** — documented limitation instead.
- **DoIP**, per CLAUDE.md.

## 6. Capture verification (`captures/klartext-session-2026-07-03.pcap`, decoded 2026-07-03)

The owner supplied the real 1216-frame pcap (BYO, gitignored). Decoded with the
Wireshark HSFZ dissector (`tshark -d tcp.port==6801,hsfz`). Results:

**Confirmed — promote these `[verify against capture]` markers to `[verified 2026-07-03]`:**
- **HSFZ length convention** — `LENGTH = 2 + SRC + TGT + UDS`, big-endian, control word
  excluded. E.g. the VIN response is `00 00 00 16 00 01 12 f4 62 f1 90 <17-byte VIN>`
  (LENGTH 0x16 = 2+3+17). Matches `frame.rs` exactly.
- **Responses swap SRC/TGT.** Request `f4 12 · 22 f1 90` → response `12 f4 · 62 f1 90 …`;
  the responding ECU's address is the HSFZ **source**. **This is the property §2.1's demux
  routing (route by frame source address) depends on — it holds on the real car.**
- **Control words** — only `0x01` (req/resp) and `0x02` (ack) on the diagnostic channel.
  The gateway ACKs every diagnostic frame with a `0x02` echoing the request's src/tgt,
  *before* the `0x01` response; the reader's "skip control ≠ 0x01" is correct.
- **Keepalive** — UDS `3E 80` (control 0x01) at ~2 s cadence, ACK'd by `0x02`. **No HSFZ
  `0x12` alive-check frame is ever used** — so not implementing it is correct.
- **The M6 dynamic-measurement `2C`/`22` sequence is byte-exact** (this was the biggest open
  `[verify against capture]` in `docs/sgbd-findings.md §7a`): `2C 03 F3 03`→`6C 03 F3 03`,
  `2C 01 F3 03 <id> 01 <size>`→`6C 01 F3 03`, `22 F3 03`→`62 F3 03 <raw>`. Confirmed for
  ids `0x4517` (oil temp, raw `41 00`), `0x5955` (RPM, `06 7F`), `0x44BE` (DPF soot,
  `08 E8`) — values match the live session log.
- **Standard PIDs are a no-op on this DDE** — `22 F40C`/`F405` → `7F 22 31` (M5 finding).
- **0x11 discovery layout** — the announcement body is `DIAGADR<addr>BMWMAC<mac:12>BMWVIN<vin:17>`
  (ASCII), on UDP 6811→7811. The gateway IP (169.254.90.33), tester `0xF4`, and ECU
  addresses `0x12`/`0x40` are confirmed.

**Bug the capture exposed (fold into the plan):** the best-effort `scan_vin`
(`crates/hsfz/src/discover.rs`) returns a **false VIN** — `AGADR10BMWMAC001A` — because
it takes the first 17-char run of VIN-alphabet characters, which occurs inside the
`DIAGADR…BMWMAC…` prefix, before the real VIN. Fix: anchor on the `BMWVIN` marker (now that
the layout is known) and take the following 17 chars. Latent today (VIN comes from DID
F190 first; discovery VIN is the fallback), but real.

**Still NOT covered by this capture — keep as `[verify …]`:**
- **DTC record framing** (`59 02 <records>`) — the capture has **no `0x19`/`0x59` traffic**
  at all (only 3E/2C/22/62/6C/7F). `decode_dtcs` stays `[verify against capture]`; do not
  claim otherwise.
- **Multi-target interleaving (§2.1)** — the capture is strictly lockstep single-target;
  concurrent requests to different ECUs stay `[verify live]` (the `--scan-concurrency 1`
  fallback exists for exactly this uncertainty).
- **Keepalive to gateway `0x10`** — `0x10` is **never directly addressed** in the capture
  (the proven keepalive target is the *active ECU*). §2.1 sends the keepalive to the gateway
  because a demuxed link has no single active ECU and reads are stateless; this is
  `[verify live]`, with the trivial fallback of targeting the most-recently-used ECU.

## 7. Follow-up milestone seeds

1. SVT read: disassemble `STEUERN_VCM_GENERATE_SVT_START/GET_RESULTS` +
   `STATUS_VERSION_GATEWAYTABELLE`; decide CLI-only vs invariant refinement; gives
   fitted list + per-ECU variants in one read and completes variant auto-detection.
2. On-car DTC-framing capture: this pcap lacks `0x19` traffic, so a whole-car scan on
   the F20 (with faults present) is still needed to verify the `59 02` record layout.
