# Field findings & TODOs — first live car session (2026-07-03)

First real connection to a car (F20, N47 diesel) over ENET. This file is **committable**:
behaviours, bugs, and improvement items only. The BYO-data companion (VIN, live values,
pcap) is `captures/SESSION-2026-07-03.md` (gitignored).

> **RESOLVED in M10** (`docs/superpowers/specs/2026-07-03-live-discovery-dynamic-core-design.md`):
> - Drop-hardcoded #1 (BUILTIN_ALIASES) → ✅ deleted; ECU names come from the DB (`scan_ecus`/`list_ecus`).
> - Drop-hardcoded #2 (hand-supplied variant) → ✅ variant ladder (explicit → learned per-VIN profile → single DB candidate).
> - Drop-hardcoded #3 + Owner feedback (fitted list, fast-fail, absent-module hang) → ✅ `scan_ecus` probes the fitted set; absent ECUs skipped fast.
> - Drop-hardcoded #4 (verify constants against the pcap) → ✅ decoded 2026-07-03; see design §6 (length/response-swap/2C-sequence confirmed; DTC `59 02` framing still open — no 0x19 traffic in the capture).
> - Behaviours-not-documented #1/#2 (host setup + ZGW wake), #3 (standard PIDs), #4 (disconnect), #5 (clear-all) → ✅ skill updated; `clear_all_faults` added; disconnect-on-exit added.
> - Owner feedback (status-0x40 noise) → ✅ relevance partition (`RELEVANT_MASK` 0xAF); reads/scan surface real faults, count the noise.
> - Code-quality #1 (swallowed errors) → ✅ `list_ecus` surfaces `db_error`; fallible `ecu::list`. #2 (fixtures) → ✅ NULL/mixed-storage + pre-v2 fixtures.
> - Feature-gap #1 (multi-step guided procedures / BEST-2 ABL) → **still future** (design §5), as is the SVT read for full variant auto-detection.

---

## Fixed this session
- **`list_ecus` returned only the 3 hardcoded builtins** despite `db_available:true`.
  Root cause: the `ecu` table has 42 rows with a **NULL `address`** (ISTA's virtual/internal
  SGBDs — `VIRTSG9x`, `zcs_all`). `Catalog::ecus()` read `address` as a non-optional `u8`,
  so rusqlite returned `InvalidColumnType(0,"address",Null)`, failing the whole call; then
  `ecu::list()` swallowed it via `if let Ok(entries) = catalog.ecus()` and fell back to
  builtins. Fixed in `crates/semantic/src/catalog.rs` with `WHERE address IS NOT NULL` +
  a NULL-row fixture and a real-DB `#[ignore]` test. Unlocks the full **170-address** map
  (155 distinct entries by name). ✅ verified live.

---

## Drop hardcoded values
1. **`BUILTIN_ALIASES` in `mcp/src/ecu.rs` (ZGW/DME/CAS).** Now redundant with the DB map —
   and *wrong* on this car: `"DME"` mislabels a **diesel (DDE)**, and `"CAS"` at 0x40 is the
   **FEM** on F20. The hardcoded name literally made me mis-call the engine "petrol." Either
   drop them and derive names from the DB, or keep only a minimal raw-hex fallback for the
   no-DB case and stop surfacing misleading names.
2. **Hand-supplied `variant` everywhere.** `read_data`/`list_measurements`/`list_service_functions`
   all require the caller to hardcode the SGBD variant (e.g. `d72n47a0`). There is no way to
   know it for an arbitrary ECU. → **Auto-detect the variant** from ECU identification
   (F1xx DIDs / the gateway's installed-ECU+variant table), like ISTA does. Blocks a lot of
   ergonomics until fixed.
3. **Generic ECU map ≠ fitted ECUs.** `list_ecus` returns the whole 170-entry BMW model map,
   most of which isn't on this car. A real whole-car scan needs the **installed-ECU list from
   the gateway** (ISTA queries it). Today absent modules just error.
4. **`[verify against capture]` protocol constants.** We now have a real pcap
   (`captures/klartext-session-2026-07-03.pcap`, 1203 frames on TCP 6801). Verify the flagged
   constants against it: alive-check cadence/sender, HSFZ framing, the 0x11 ident body layout.

---

## Behaviours the `klartext-service` skill does NOT document (it should)
1. **ENET on F20 is not plug-and-play — three silent host-side blockers:**
   - **NetworkManager** manages the NIC and flushes the link-local IP on every carrier bounce
     → set the interface unmanaged + pin a static `169.254.x.x/16 scope link`, or use an NM
     link-local profile.
   - **Default-drop firewall (ufw)** silently eats inbound `udp/67` *and* would eat HSFZ replies
     → `ufw allow in on <iface>`.
   - The car does **DHCP-then-link-local**; with no DHCP server it can sit begging → run a tiny
     DHCP server on the link, or wait for its link-local fallback.
   The skill assumes a clean link-local link. Document the prerequisites + exact commands.
2. **F20 OBD-Ethernet topology + ZGW wake.** OBD Ethernet reaches the **head unit**
   (`L7ENTRYHU`, Continental/Aumovio), which DHCP-begs and is the only device on the wire until
   the ZGW joins. The ZGW's DHCP hostname is `DIAGADR10BMWVIN<vin>`. The ZGW sleeps/hangs and
   must be woken: **unplug → let the car sleep ~3–5 min → ignition ON first → then plug in**.
   Known F-series failure mode ("ZGW not visible on ethernet").
3. **Standard SAE-J1979 PIDs don't work on the BMW DME.** `read_data F40C`/`F405` return
   `requestOutOfRange (0x31)`. The M5 "standard PID scaling" path is a no-op on real BMW ECUs —
   live data must come from proprietary SGBD measurements. Document this and route live data
   through the SGBD (or map standard PIDs → their SGBD equivalents).
4. **Clean disconnect.** Restarting the MCP without `disconnect` leaves a dangling HSFZ session
   (times out server-side; harmless but untidy). Call `disconnect` before teardown.
5. **`clear_faults` is per-ECU.** No "clear all" — a whole-car clear means iterating fitted
   ECUs. Document, or add a batch clear behind the same `confirm` gate.

---

## Feature gaps
1. **Multi-step / guided service procedures (ISTA "Servicefunktionen" / ABL) — NOT implemented.**
   klartext models each service function as a **single derived frame**
   (`crates/semantic/src/service_function.rs`: one `ServiceFunction` = one frame; many are
   `frame-not-derivable`). ISTA runs *step-by-step* guided procedures: read conditions → prompt/
   act → write → verify → loop (DPF regeneration, injector-quantity coding, adaptations, bleed
   routines). Doing anything ISTA-like needs (a) a **sequence model** — ordered steps with
   per-step preconditions/reads/writes/verifies and operator prompts — and (b) an interpreter for
   ISTA's ABL/BEST-2 sequences. Large feature; note as future direction.
2. Variant auto-detection (Drop-hardcoded #2) is the prerequisite for most ergonomic wins.

---

## Code-quality patterns to audit
1. **Swallowed errors via `if let Ok(x) = fallible()`** — the NULL bug's real cause. Grep for the
   pattern; at minimum log the `Err` to stderr. And don't report success (`db_available:true` +
   "merged with the ISTA ECU map") when a sub-step silently failed.
2. **Test fixtures don't mirror real-data edge cases.** The synthetic `ecu`/`dtc` fixtures used
   clean, non-null INTEGER data, so the NULL-address (and mixed SQLite storage-class) case never
   surfaced. Add NULL / mixed-`typeof` rows to fixtures, and audit the `dtc` + measurement reads
   for the same "non-optional `u8`/`String` over a nullable column" fragility.

---

## Owner feedback (from the live scan) — "klartext has to be smarter"
The whole-car scan stalled because it blindly probed the generic 170-address map; **absent
modules hang on a gateway timeout** (observed: EGS `0x18` on this manual-gearbox car — no
automatic-transmission ECU — blocked until killed). Make the scan smart:
- **Get the fitted-ECU list from the gateway** and scan only those — never probe phantom
  addresses (ties to Drop-hardcoded #3).
- **Fast-fail on absent/non-responding ECUs** instead of blocking on the gateway timeout;
  a full-car scan must not hang on missing modules.
- **Separate real stored faults from status-`0x40` "not-tested-this-cycle" catalog noise**
  (FEM returned ~147 of these with the engine off) so a scan surfaces what matters — as ISTA does.
- General: auto-detect variant, know the car's actual config, degrade gracefully.

## Verified working
- ecu-map fix, live end-to-end (full ~155-ECU map, targetable by name).
- Live reads via SGBD scaling (RPM, speed, coolant, oil temp, DPF soot) — raw+scaled in the pcap.
- Reusable connection recipe (host setup + ZGW wake) — see the session log.
