---
name: klartext-service
description: Use when a klartext user works on their BMW F-series through Claude — reading live data (oil/coolant temperature, DPF soot load or regeneration status, RPM), clearing fault codes, or running/resetting/actuating a service function (oil/CBS reset, adaptation or learned-value reset, statistic reset, throttle/fan/glow actuator, injector calibration) — or asks what an ECU can measure or do, or whether an operation is safe.
---

# klartext guided diagnostics and service functions

## Overview

klartext splits work by blast radius. You (Claude) read and reason freely over the MCP tools.
Exactly **one** write is yours to invoke — `clear_faults`, standard UDS 0x14 — and only with the
human's explicit, informed go-ahead. Every other state change (service functions, actuation,
coding) is the **human's to execute** in the `klartext` CLI: the MCP surface has no tool for
them, and that is deliberate.

Every derived service-function frame is reconstructed from ISTA disassembly and is
**UNCONFIRMED** — not yet validated against a real car (`[verify against capture]`). Treat that
as load-bearing, not a footnote.

## Reading live data (discover → read)

1. **Discover.** Call `list_measurements` with the ECU's SGBD `variant` (e.g. `d72n47a0` for
   the F2x diesel DME). The catalog is big (~1800 entries on the DDE) and a call returns at
   most 200 of `total` — always narrow with `search`. Terms are mostly German:
   `Öltemperatur` (oil temp), `Kühlmittel` (coolant), `Rußmasse` (DPF soot mass), `Asche`
   (ash), `Regeneration` (DPF regen status/history), `Drehzahl` (RPM).
2. **Read.** Call `read_data` with the entry's `arg` or name as `name` (or its `id_hex` as
   `did`) plus the same `variant`. The reply carries the scaled engineering value + unit; raw
   bytes are always included.
3. **Only what the catalog defines exists.** If a name doesn't resolve, search again with a
   different term; if it is genuinely absent from `SG_FUNKTIONEN`, it is not readable — say so.
   Never invent a measurement name or guess a DID.

## Clearing fault codes (the one MCP write)

`clear_faults` erases an ECU's stored DTCs **and their freeze-frame/snapshot data**, and can
reset OBD readiness monitors. It is reversible only in that a still-active fault sets its code
again on a later drive cycle. The workflow is fixed:

1. **Read first.** `read_faults` on the target ECU; show the human what is stored.
2. **Advise the consequence** before asking: freeze-frames are lost (they are diagnostic
   evidence), readiness monitors may reset, and an unfixed fault will return.
3. **Get an explicit go-ahead to clear.** "Just fix it" is not consent to erase evidence —
   confirm they want the codes cleared, knowing the above.
4. **Clear.** `clear_faults` with `confirm: true`. The result echoes the codes it discarded.
5. **Verify.** Re-run `read_faults`; advise that a code reappearing later means the underlying
   fault is active and needs diagnosis, not another clear.

Without `confirm: true` the tool refuses by design — that refusal is the gate working, not an
error to route around.

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

- **Your only write is `clear_faults`,** and `confirm: true` is the human's decision relayed —
  never your default, never inferred from an ambiguous "fix it", never chained silently after a
  read. Everything else that changes state is human-CLI.
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
| Treating MCP as fully read-only (stale M4 rule) or as generally writable | It exposes reads plus exactly one confirmation-gated write: `clear_faults`. Nothing else mutates, by design. |

This skill is knowledge and workflow only. The sole capability it grants you is the
confirmation-gated fault clear; everything else stays with the human.
