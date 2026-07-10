# Car session 1 — results + pcap verification (2026-07-10)

On-car acceptance run per `docs/car-session-1-handoff.md`, driving the reads through the MCP
server and the two writes explicitly. Full session captured to
`captures/captures/car-session-1.pcapng` (gitignored — contains VINs; 5 912 packets, 41 min).
This is the laptop session report **plus a byte-level verification addendum** (every claim
below was re-checked against the pcap on the desktop; where the addendum found more than the
session report, it says so).

## Session metadata — and a key discovery: this was a SECOND car

- **Vehicle:** BMW **F25 X3** (CAS4 at `0x40`, *not* a FEM), N47 diesel (DDE `d72n47a0`),
  VIN `[VIN-redacted]`, gateway `169.254.71.121` (ZGW `0x10`), **auto-discovered** on link-local.
- **This is not the F20 the project was built against.** The July-3 pcap's ident frame carries
  VIN `[VIN-redacted]` (an F20 1-series, gateway `169.254.90.33`) — different MAC, different car.
  Session 1 therefore doubles as a **chassis-portability proof**: 32 ECUs on a never-seen
  chassis, CAS4 instead of FEM, resolved by the live SVT + ISTA-DB naming with zero code
  changes. The same-N47 DDE means all offline `d72n47a0` work transferred untouched.
  **When comparing captures, always check the ident VIN first.**
- **Tester:** `169.254.238.203/16` on `eth0`; server
  `klartext-mcp --semantic-db data/klartext-semantic.db --sgbd-dir "data/Testmodule(1)/Ecu"`
  (1 405 `.prg`), code = current `main`.

## Coverage matrix — outcomes

| Feature | Result |
|---|---|
| HSFZ transport + handshake | ✅ verified on wire |
| Gateway discovery (link-local, no DHCP) | ✅ `169.254.71.121` |
| Read DTCs + DTC→text | ✅ 1 real fault car-wide (DAB antenna); text decodes |
| Freeze-frame read (`19 04`/`06`) | ✅ read works (3 snapshot records); `19 09` → `7F 19 12` (severity subfn unsupported) |
| SVT + identity (`22 3F07`) | ✅ 32 **configured** ECUs (VCM list) + VIN + per-ECU `F18C`. ⚠️ over-labeled "fitted" (finding 8), I-Stufe null, FA decode incomplete (4a/4b) |
| scan_ecus (fitted probe) | ✅ 32, stable across rescans |
| Proprietary scaling (M6, SG_FUNKTIONEN) | ✅ oil temp 46 °C; DPF soot measured 15.49 g vs modelled 15.5 g |
| list_measurements / read_data-by-name | ✅ |
| One connection → all ECUs (read_all_faults) | ✅ (⚠️ ~10 ECUs timed out — finding 5) |
| **Read-only SID gate** | ✅ verified on wire — see addendum §TX-census |
| **clear_faults (0x14 write)** | ✅ verified on wire — `10 03` → `14 FF FF FF` → `54` |
| CBS oil-service reset | ⏭️ skipped by owner (oil not changed — correct call) |
| **`run_job` live (BEST/2)** | ✅ FIXED 2026-07-10 — dynamic measurements redirect to `read_data`; finding 1 |
| ECU/variant auto-resolve ladder | ❌ finding 2 |
| ECU titles from DB | ⚠️ null everywhere — finding 3 |
| CP1252 `_INFO` umlauts | ⏳ not exercised (blocked by finding 1) |

## Findings (actionable, most-severe first)

### 1. `run_job` transmits the SG_FUNKTIONEN table-id as the UDS DID → `requestOutOfRange`

Same measurement (oil temp `ITOEL`), two paths, twenty seconds apart in the pcap:

```
358.8s  measurement.rs   TX F4→12: 2C 03 F3 03            (clear dynamic define)
                         TX F4→12: 2C 01 F3 03 45 17 01 02 (define F303 := table-id 4517, bytes 1..2)
                         TX F4→12: 22 F3 03
                         RX 12→F4: 62 F3 03 39 08          → 0x3908 → 46.0 °C  ✅

377.9s  BEST/2 VM        TX F4→12: 22 45 17                (table-id sent as a plain DID)
                         RX 12→F4: 7F 22 31                requestOutOfRange  ❌
```

`0x4517` is the `id_hex` that `list_measurements` reports (the SG_FUNKTIONEN row id); on this
DDE it is a **define-source**, not a readable DID. The offline differential oracle stayed green
because it validated *response decoding* against canned bytes — it never checked the *request*
the VM emits against what the working path sends. **Fix (P2.1):** route the VM's `STATUS_LESEN`
request through the same measurement→wire-DID resolution `measurement.rs` uses (the
`2C`-define + `22 F303` sequence), then re-run only the Part-6 litmus on-car
(`JOB_STATUS=OKAY`); the CP1252 `_INFO` check rides along.

Two silver linings, confirmed by the same exchange: the ECU **parsed our BMW-FAST telegram and
answered with a valid NRC** — so the request telegram shape (marker #1) and the additive
checksum (marker #2) are wire-accepted. Open question for later: whether real EDIABAS also
emits `22 <table-id>` for this job on some variants (our "static `22` read" fact came from our
own VM's execution of the bytecode, not from observing EDIABAS) — an ISTA session with
`tcpdump` running would settle it definitively.

### 2. Variant auto-resolve ladder does not resolve (no learned profile)
`list_measurements`/`read_data`/`run_job` with only `ecu` fail: *"no explicit variant, no
learned profile, and no single DB candidate."* The DDE (`0x12`) has 100+ candidates and this
VIN has no learned profile, so the M10 ladder can't pick — today you must pass `variant`
explicitly (`d72n47a0`). Fix: persist a learned per-VIN profile (now doubly needed — two cars).

### 3. ECU titles `null` — NOT a code bug; the session used a pre-title DB build
Group names and variants resolved but the human-readable *title* was empty everywhere in
`scan_ecus`/`identify_vehicle`/`list_ecus`. **Investigated 2026-07-10 and the code path is
correct:** the current semantic DB carries `title_en`/`title_de` for all 1 402 `ecu` rows
(e.g. addr 0x78 → "Automatic heating/air conditioning"); `Catalog::ecus()` selects them, and
both consumers (`mcp/src/ecu.rs:78`, `mcp/src/server.rs:1718`) propagate `slot.title`
faithfully — proven by the passing unit test `ecus_aggregates_by_address_with_canonical_group_and_title`
and by replaying the exact runtime queries (including the `pragma_table_info(?1)` bound-param
column check, which returns `1`) against the real DB. So the null-titles came from the
**semantic DB build used at session time predating title population**, not klartext's code.
**Resolution: rebuild the DB (`scripts/build-semantic-db.sh`); no code change.** (One of the
three "nothing works" symptoms is therefore not a bug at all.)

### 4a. I-Stufe — returned null, **but the layout is now cracked from the capture**
The gateway answered `22 100B` positively; our decoder just didn't understand it. Raw payload
(after the `62 10 0B` echo), 46 bytes:

```
46 30 32 35 17 07 02 12   = "F025" 0x17 0x07 0x0212  → F025-23-07-530  (current)
46 30 32 35 14 07 02 1c   = "F025" 0x14 0x07 0x021C  → F025-20-07-540  (previous)
46 30 32 35 11 03 01 f9   = "F025" 0x11 0x03 0x01F9  → F025-17-03-505  (factory)
00 14 6e 0e dd 1d a8 1a 80 7c 33 c6 77 5e 3d db d1 7a 9d e8 d0 0d   (22 trailing bytes, unknown)
```

Three 8-byte records: **4 ASCII series chars + binary year + binary month + u16-BE patch**.
The year bytes are plain binary, **not BCD** — discriminated by the owner: the car is a 2017
build, matching factory `0x11` = 17 (`F025-17-03-505`, March-2017 production) and ruling out
the BCD reading (which would have claimed 2011). Roles descend *current / previous / factory*
(the SGBD result names `STAT_I_STUFE_HO/WERK` name the current+factory pair); the car was last
programmed to the 2023-07 train. **FIXED + tested 2026-07-10:** `decode_i_stufe`
(`crates/client/src/client.rs`) parses the records; `read_i_stufe` now returns the current
level (e.g. `F025-23-07-530`) instead of null, and the DID's `[verify against capture]`/"ASCII"
note is flipped to confirmed (client+uds `fmt`/`test`/`clippy -D warnings` all green). Returns
the current/first record; exposing previous+factory is a small follow-up. This also incidentally
proved the chassis (`F025`) from the gateway itself.

### 4b. FA (vehicle order) — 214-byte test vector captured, decode still open
`62 3F 06` returned `00 BB` (= 187, block length?) + a packed SALAPA/`TABKOMPRIMIERUNG` block.
Our decode produced only `version:87`. The full raw block is in the pcap — the 2-bit-map decode
now has its real-world test vector.

### 5. read_all_faults times out on ~10/32 ECUs
`0x06,0x08,0x18,0x19,0x29,0x2C,0x30,0x5D,0x67,0x72` hit the 5 s timeout during the whole-car
sweep, dominating wall-clock — yet `0x29`/`0x18` answered *individual* reads moments earlier
(pcap confirms `0x29` delivered `59 02 FF` once). Consistent with sleep/low-power under rapid
sequential probing, not an addressing bug. Consider a longer per-ECU timeout, a wake
(TesterPresent) + retry pass, or accepting partial results explicitly.

### 6. Gateway drops the session on ignition-off; no client auto-reconnect
The pcap shows **two TCP sessions**: t=0–540.5 s (ends with gateway FIN then RST — ignition
off), then the gateway *self-announces* ident 3× on broadcast at t=581 s (wake), tester
re-discovers at t=741 s, second session t=846–2 452 s (ends with a clean tester FIN — the M10
disconnect-on-exit working). The client surfaced "I/O error on the HSFZ connection" and needed
a manual reconnect. Consider auto-reconnect or a clearer "car powered down" error.

### 7. Three modules report a foreign VIN (donor parts)
`0x43`/`0x44` → `[VIN-redacted]` (raw ASCII in their `62 F1 90`, verified); `0x6B` (tailgate) →
`[VIN-redacted]`. Everything else reports this car's VIN. Not a tool bug — the modules genuinely
carry those VINs. (Used-car part replacements, useful provenance data.)

## Vehicle health (for the owner)
- **One active fault car-wide:** `B7F805` — DAB L-band antenna open circuit (head unit `0x63`),
  status `0x2F`. Cleared at t=2 025 s (`54` positive); re-read at t=2 071 s shows it back with
  `0x2F` ⇒ the antenna is **genuinely disconnected right now** — physical check worthwhile.
  (The other `0x63` entries carry status `0x50` = not-tested-since-clear, i.e. noise.)
- **DPF healthy:** soot measured 15.49 g vs modelled 15.5 g — the two independent estimates
  agree; diff-pressure sensing OK.
- **Oil temp** 46 °C at read time. No AUC/air-quality fault stored anywhere.

### The AUC question (owner follow-up, forensics 2026-07-10)

The owner recalls ISTA showing a **failing AUC sensor** on a past session, which a clear-all
didn't remove. Pcap + DB forensics:

- On this platform the AUC sensor belongs to the **JBE (junction box, `0x00`)**, not the IHKA —
  the DB's AUC codes are `C90D60`/`C90D74`/`C90D79` ("internal sensor fault / does not respond /
  incorrectly fitted", variant family `jbbf3`), plus legacy `A6CF` on older JB variants.
- The JBE answered `19 02 FF` with 143 entries, **including the surrounding `C90Dxx` family —
  but the three AUC codes are absent**, and so is `A6CF`. Crucially, every code the JBE *did*
  return sits at status `0x50` (`notCompletedSinceClear` + `notCompletedThisCycle`): **the
  JBE's fault memory was cleared at some point and most tests have not re-run since.** Across
  all 25 ECUs the only bits with an active-failure flag were the DAB antenna's.
- **Owner confirms the AUC sensor IS on this F25** (not the F20). Reconciling that with a clean
  read: the session was run **engine-off** (protocol says that's fine for the reads it targets),
  and the AUC is an *air-quality* sensor whose self-test plausibly needs the engine/climate
  system running to sample — unlike the DAB antenna's pure key-on open-circuit check, which is
  why that one re-set within a second and the AUC didn't reappear. Combined with the JBE sitting
  entirely at "not-completed-since-clear," the most likely story is: the fault was real, the
  memory was cleared (by a prior ISTA session or otherwise), and the July-10 engine-off read
  caught it **before the re-test condition was met** — so it correctly showed absent/pending,
  not filtered. klartext read the `59 02` wire faithfully; there is no such code in the bytes.
- **Two concrete follow-ups this raises:**
  1. **Re-read the JBE `0x00` with the engine running** (AUC test conditions) — the direct settle.
     If it's genuinely failing it will set a `C90D6x`/`74`/`79` with a `testFailed` bit.
  2. **Coverage gap — klartext reads only fault memory (`19 02`).** BMW ECUs keep a second
     **Infospeicher** (secondary/info memory; ISTA shows it beside faults), read via **`22 2000`**
     (SGBD jobs `IS_LESEN` / `IS_LESEN_DETAIL`; per-entry detail `22 20 nn`). Confirmed 2026-07-10
     from the DDE `.prg` bytecode (primary source): it is a **distinct** memory from `19 02` — own
     service, own `IOrtTexte`/`IArtTexte` location/type tables, and an `F_EREIGNIS_DTC` event-vs-DTC
     flag. Session 1 sent zero `22 2000` (TX census). If ISTA's "AUC" line lived there, klartext
     could not have shown it. → **IMPLEMENTED + tested 2026-07-10:** read-only `read_info_memory` (MCP) / `info-memory` (CLI) tool (pure `0x22`,
     no new blast radius); response byte offsets stay `[verify against capture]` until a `22 2000`
     capture (the field *schema* is certain, the per-entry record width is not).

## Pcap verification addendum (desktop, 2026-07-10)

Parsed the full pcapng into HSFZ frames (tshark + reassembly; scratch tooling, not committed).

**TX census — the read-only invariant, on the wire.** Every diagnostic request the tester
transmitted in 41 minutes: `3E` ×1 072, `22` ×395, `19` ×42, `2C` ×6, `10` ×1, `14` ×1.
The single `0x14` is the human-confirmed `clear_faults`; the single `0x10` is its extended
session. **Zero** `2E`/`2F`/`31`/`34`–`37` frames exist in the capture. The `run_job
FS_LOESCHEN` attempt left no frame at all (refused above the wire) — exactly the gate's
contract.

**Response-layout markers flipped byte-for-byte:**
- **SVT `62 3F 07`**: `00 20` = u16-BE count (32), then exactly 32 × 1-byte ECU addresses
  (frame length 3+2+32 ✓, two identical samples 69 s apart). List includes oddballs `0x00`
  and `0x01` (answered identification NRCs later — real addressable somethings).
- **DTC `59 02`**: `[availability mask][3-byte DTC + 1-byte status]*` — 4-byte records
  confirmed across 20 ECUs; the DAB fault reads `b7 f8 05 2f`.
- **Freeze-frame `59 04`**: DTC echo + status, then numbered snapshot records
  `[recNum][didCount=05][DID][data]…` (records `00`, `01`, … with an identical 5-DID set).
  `59 06`: DTC echo + status + `[recNum][data…]`. `19 09` → NRC `0x12` on `0x63`.
- **M6 path re-confirmed** on a foreign chassis (three `2C 03`/`2C 01`/`22 F303` rounds:
  sources `4517` oil, `44BE`, `44C1` — the latter two the DPF soot reads).
- **Clear sequence**: `19 02 FF` (fault present) → `10 03` (with a `7F 10 78` responsePending
  first — handled) → `50 03 01 2b 01 f4` → `14 FF FF FF` → `54`.

**Session-1 conclusion:** the entire M0–M10 read stack + the M9 gated clear are now
**wire-verified on a second chassis**. The BEST/2 P2 litmus (`JOB_STATUS=OKAY`) remains open
pending the finding-1 fix — the telegram/checksum layer beneath it is already confirmed.

## Suggested next steps (priority order)
1. **P2.1** — fix the VM's measurement→DID resolution (finding 1); offline, oracle-extendable
   (assert on the *emitted request*, not just response decode).
2. Learned per-VIN variant profile (finding 2) + ECU title join (finding 3) — offline.
3. I-Stufe decoder from the cracked layout (4a); FA SALAPA decode against the captured vector (4b).
4. Timeout/wake strategy for the whole-car sweep (5); reconnect-on-drop UX (6).
5. Next car session is a 10-minute litmus: `job run STATUS_LESEN ARG ITOEL` → `OKAY`,
   plus the DSC multi-result read (markers #3/#4).
