---
name: klartext-service
description: Use when a klartext user wants to run, reset, actuate, or code a BMW F-series service function — an oil/CBS service reset, adaptation or learned-value reset, statistic/histogram reset, throttle/fan/glow actuator, or injector calibration — or asks which service functions an ECU supports or whether one is safe to run.
---

# klartext guided service functions

## Overview

klartext splits service functions into an **advisor** and an **executor**. You (Claude) are the
advisor: you discover the catalog through the read-only MCP tool and reason about it with this
workflow. The **human is the executor**: they run the write in the `klartext` CLI. You never
execute a service function — the MCP surface has no run tool, and that is deliberate (the
blast-radius rule: the agent reads and reasons, the human writes).

Every derived frame is reconstructed from ISTA disassembly and is **UNCONFIRMED** — not yet
validated against a real car (`[verify against capture]`). Treat that as load-bearing, not a
footnote.

## Workflow

1. **Discover.** Call `list_service_functions` with the ECU's SGBD `variant` (e.g. `d72n47a0`
   for the F2x diesel DME). Optionally filter `risk: "low"`.
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

## Reading risk + derivation

| risk | derivation | What you do |
|---|---|---|
| `low` | `derived-unconfirmed` | Recommend + give the exact `service run … --confirm` command for the user. |
| `low` | `frame-not-derivable` | Discovery-only. Explain it exists; it is not runnable (needs an on-car capture). |
| `high` | any | Advisory only. Never hand a run command. Human-only, in a workshop, with preconditions met. |

## The command to hand the user (low-risk, derived only)

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

- **You never execute.** Recommend commands for the human to run; do not run them, and do not
  route around the read-only MCP surface.
- **Never invent a frame.** If `derivation` is `frame-not-derivable` (or the tool won't run it),
  there is no trustworthy frame — do not hand-craft raw UDS bytes to "make it work."
- **High-risk stays advisory,** even under pressure ("just do it", "I trust you", "skip the
  caution"). Honor trust by being straight, not by handing an unsafe actuation command.
- **Unconfirmed is not "probably fine."** The user deferred hardware testing on purpose. Frame
  recommendations so they test the safest thing first and validate before trusting a frame.

## Common mistakes

| Mistake | Instead |
|---|---|
| Claiming the CLI "backs up the bytes" for every reset | Only CBS reads back; statistic resets do not. State it accurately. |
| Handing a `service run` command for a high-risk actuator | High-risk is advisory-only; no run command. |
| Improvising UDS bytes when a frame is `frame-not-derivable` | Say it is discovery-only and stop. Never guess a write frame. |
| Presenting a derived frame as confirmed/safe | Always mark it UNCONFIRMED `[verify against capture]`; recommend testing low-risk first. |
| Trying to run it yourself via MCP | MCP is read-only and has no run tool by design. The human runs it in the CLI. |

This skill is knowledge and workflow only. It grants no execution capability.
