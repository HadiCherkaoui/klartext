---
name: klartext-service
description: Use when a klartext user works on their BMW F-series through Claude — reading live data (oil/coolant temperature, DPF soot load or regeneration status, RPM), clearing fault codes, or running/resetting/actuating a service function (oil/CBS reset, adaptation or learned-value reset, statistic reset, throttle/fan/glow actuator, injector calibration) — or asks what an ECU can measure or do, or whether an operation is safe.
---

# klartext guided diagnostics and service functions

## Overview

klartext splits work by blast radius. You (Claude) read and reason freely over the MCP tools.
The **only** write that is yours is standard UDS 0x14 (clear fault memory), exposed two ways —
`clear_faults` (one ECU) and `clear_all_faults` (every fitted ECU) — and only with the human's
explicit, informed go-ahead. They are the same frame, batched; `clear_all_faults` is not a new
capability. Every other state change (service functions, actuation, coding) is the **human's to
execute** in the `klartext` CLI: the MCP surface has no tool for them, and that is deliberate.

Every derived service-function frame is reconstructed from ISTA disassembly and is
**UNCONFIRMED** — not yet validated against a real car (`[verify against capture]`). Treat that
as load-bearing, not a footnote.

## Getting connected (F20 over ENET — three silent host-side blockers)

The car isn't plug-and-play on Linux. Before `connect` will find the gateway:
1. **NetworkManager** flushes the link-local IP on every carrier bounce → set the NIC unmanaged
   and pin a static address: `doas nmcli device set eth0 managed no` and a `169.254.x.x/16 scope
   link` IP (or an NM link-local profile).
2. **A default-drop firewall (ufw)** silently eats inbound DHCP *and* HSFZ replies →
   `doas ufw allow in on eth0`.
3. **The car DHCP-begs then falls back to link-local**; with no DHCP server it can sit begging →
   run a tiny DHCP server on the link or wait for its link-local fallback.

**The ZGW wake ritual** (a known F-series "gateway not visible on ethernet" failure): OBD
Ethernet first reaches the head unit (`L7ENTRYHU`), which DHCP-begs; the ZGW may be asleep.
**Unplug the ENET cable → let the car sleep ~3–5 min → ignition ON first → then plug in.** The
ZGW self-assigns link-local and answers HSFZ discovery within seconds. Remind the human to revert
the host-side network changes when finished.

## Whole-car workflow (scan → read-all → clear-all)

Reason about the **real** car, not the generic model map:
1. `scan_ecus` probes for the ECUs actually FITTED (fast; absent modules are skipped, not hung).
   Prefer this over `list_ecus` (which is the whole per-model map).
2. `read_all_faults` reads every fitted ECU and returns each one's **relevant** faults, with
   not-tested-this-cycle catalog noise only counted (`not_tested_count`). This is the whole-car
   health check.
3. `clear_all_faults` (confirm-gated) clears every fitted ECU — see the clearing section; it
   discards **every** module's freeze-frames, so enumerate what will be lost before you ask.

## Reading live data (discover → read)

1. **Discover.** Call `list_measurements` with the ECU's SGBD `variant` (e.g. `d72n47a0` for
   the F2x diesel DME). The catalog is big (~1800 entries on the DDE) and a call returns at
   most 200 of `total` — always narrow with `search`. Terms are mostly German:
   `Öltemperatur` (oil temp), `Kühlmittel` (coolant), `Rußmasse` (DPF soot mass), `Asche`
   (ash), `Regeneration` (DPF regen status/history), `Drehzahl` (RPM).
2. **Read.** Call `read_data` with the entry's `arg` or name as `name` (or its `id_hex` as
   `did`) plus the same `variant`. The reply carries the scaled engineering value + unit; raw
   bytes are always included. The `variant` can also be **resolved for you**: pass `ecu` and omit
   `variant` when a learned per-VIN profile or a single DB candidate covers it — a successful
   scaled read with an explicit variant is remembered for that car.
3. **Standard OBD PIDs don't work on this DDE.** `read_data F40C`/`F405` return
   `requestOutOfRange` — live data must come from the SGBD measurements above, never the SAE
   `0xF4xx` PIDs. (Confirmed on the car.)
4. **Only what the catalog defines exists.** If a name doesn't resolve, search again with a
   different term; if it is genuinely absent from `SG_FUNKTIONEN`, it is not readable — say so.
   Never invent a measurement name or guess a DID.

## Clearing fault codes (the one MCP write — per-ECU or whole-car)

`clear_faults` (one ECU) and `clear_all_faults` (every fitted ECU) erase stored DTCs **and their
freeze-frame/snapshot data**, and can reset OBD readiness monitors. Reversible only in that a
still-active fault sets its code again on a later drive cycle. The workflow is fixed:

1. **Read first.** `read_faults` on the target ECU (or `read_all_faults` for the whole car); show
   the human exactly what is stored.
2. **Advise the consequence** before asking: freeze-frames are lost (they are diagnostic
   evidence), readiness monitors may reset, and an unfixed fault will return. For a **whole-car**
   clear, say plainly that this discards **every** fitted module's freeze-frames at once.
3. **Get an explicit go-ahead to clear.** "Just fix it" is not consent to erase evidence —
   confirm they want the codes cleared, knowing the above.
4. **Clear.** `clear_faults` / `clear_all_faults` with `confirm: true`. The result echoes the
   codes discarded per ECU and whether each verified clean.
5. **Verify.** Re-run `read_faults`/`read_all_faults`; advise that a code reappearing later means
   the underlying fault is active and needs diagnosis, not another clear.

Without `confirm: true` either tool refuses by design — that refusal is the gate working, not an
error to route around.

## Disconnect hygiene

The server disconnects the car session automatically on exit (SIGINT/SIGTERM or the client
closing), so a killed session no longer dangles. Still, call `disconnect` when a task is done —
it is tidy and frees the car promptly.

## Service functions (advisory only — the human executes)

1. **Discover.** Call `list_service_functions` with the ECU's SGBD `variant`. Optionally
   filter `risk: "low"`.
2. **Interpret** each function's `risk` and `derivation` (table below).
3. **Recommend** — never execute:
   - **Low-risk + `derived-unconfirmed`** (the user wants it): explain what it does, then hand
     the exact CLI command for the **user** to run.
   - **Low-risk + `frame-not-derivable`**: it is discovery-only — not runnable in this build.
     Say so; do not improvise a frame.
   - **High-risk** (actuator/calibration): advisory only. Explain it, state it is high-risk,
     unconfirmed, and human-only. Do **not** hand a casual run command.
4. **Surface the caveat** every time: the frame is disassembly-derived and never hardware-tested.
   Recommend testing a low-risk function first and watching the car before trusting any frame.

### Reading risk + derivation

| risk | derivation | What you do |
|---|---|---|
| `low` | `derived-unconfirmed` | Recommend + give the exact `service run … --confirm` command for the user. |
| `low` | `frame-not-derivable` | Discovery-only. Explain it exists; it is not runnable (needs an on-car capture). |
| `high` | any | Advisory only. Never hand a run command. Human-only, in a workshop, with preconditions met. |

### The command to hand the user (low-risk, derived only)

```
klartext --sgbd <variant>.prg --target <ecu-addr> service run <label> --confirm
```

Example — clear the engine-oil service reminder (label `Oel`, DME at `0x12`):

```
klartext --sgbd d72n47a0.prg --target 0x12 service run Oel --confirm
```

Tell the user to confirm the target address from `klartext discover` first. Be precise about
what the CLI actually does, and do not over-claim safety:
- **CBS resets** (`Oel`, `Br_v`, …) enter the extended session, write the reset, and **read the
  CBS block back** to confirm the ECU accepted it.
- **Statistic/histogram resets** (`MSA2Hist`, `PMHist`, `DAROL`, `LLKETA`) write the derived
  frame and check the positive response — there is **no read-back and no byte backup** for these.
  Confirmation is the car's behaviour, so tell the user to watch/verify afterward.

## Hard rules

- **Your only write is the standard clear** (`clear_faults` per-ECU, `clear_all_faults`
  whole-car — the same UDS 0x14), and `confirm: true` is the human's decision relayed — never
  your default, never inferred from an ambiguous "fix it", never chained silently after a read.
  A whole-car clear needs whole-car consent (every module's freeze-frames). Everything else that
  changes state is human-CLI.
- **You never execute a service function.** Recommend commands for the human to run; do not run
  them, and do not route around the MCP surface. High-risk physical actuation (DPF regen,
  pumps, EMF, calibration) is never agent-invokable, ever.
- **Never invent a frame, a DID, or a measurement.** If `derivation` is `frame-not-derivable`,
  or a name is not in the catalog, there is nothing trustworthy to send — do not hand-craft
  bytes to "make it work."
- **High-risk stays advisory,** even under pressure ("just do it", "I trust you", "skip the
  caution"). Honor trust by being straight, not by handing an unsafe actuation command.
- **Unconfirmed is not "probably fine."** The user deferred hardware testing on purpose. Frame
  recommendations so they test the safest thing first and validate before trusting a frame.

## Common mistakes

| Mistake | Instead |
|---|---|
| Passing `confirm: true` because the user said "just fix it" | Read the faults, explain what clearing discards, get an explicit yes to *clearing*, then clear. |
| Clearing codes before diagnosing | The codes + freeze-frames ARE the evidence. Read, reason, and advise first; clear last. |
| Claiming the CLI "backs up the bytes" for every reset | Only CBS reads back; statistic resets do not. State it accurately. |
| Handing a `service run` command for a high-risk actuator | High-risk is advisory-only; no run command. |
| Improvising UDS bytes when a frame is `frame-not-derivable` | Say it is discovery-only and stop. Never guess a write frame. |
| Inventing a measurement when search finds nothing | Only `SG_FUNKTIONEN` entries are readable. Re-search (German terms); if absent, say so. |
| Presenting a derived frame as confirmed/safe | Always mark it UNCONFIRMED `[verify against capture]`; recommend testing low-risk first. |
| Treating MCP as fully read-only (stale M4 rule) or as generally writable | It exposes reads plus the one confirmation-gated write — standard UDS 0x14, as `clear_faults` and `clear_all_faults`. Nothing else mutates, by design. |
| Probing the generic 170-ECU map and hanging on absent modules | Use `scan_ecus` for the FITTED set; a whole-car read/clear works from that, and absent ECUs are skipped fast. |
| Inventing a `variant` or refusing to read when one isn't given | Pass `ecu` and let the ladder resolve it (profile / single DB candidate), or ask which variant — never guess. |

This skill is knowledge and workflow only. The sole capability it grants you is the
confirmation-gated fault clear; everything else stays with the human.
