# M11+ roadmap — toward ISTA-parity diagnostics (brief for the next session)

**Date:** 2026-07-03 · **Status:** not started — this is a scoping brief, not a plan.
**Context:** M10 (`2026-07-03-live-discovery-dynamic-core-design.md`) made discovery/naming
live and added whole-car read/clear. This doc captures the owner's follow-up asks so the
next session can turn each into its own brainstorm → spec → plan → implement cycle. **Do
not build all of this at once** — each numbered item is milestone-sized. Priorities are
the owner's words: get the *diagnostic* fidelity ISTA has, then think bigger (HU editor,
standalone app).

A recurring accelerator the owner offered and endorsed: **decompile the ISTA .NET DLLs**
(`ilspycmd`, as already done for the DB-decryption work in `docs/sqlite-findings.md`) to
read exactly how ISTA reads/clears faults, reads freeze-frames, and runs guided
procedures — then reimplement in Rust (never copy code; facts/protocol only, per CLAUDE.md
AGPL rule). This is the single highest-leverage research step for items 1–4.

---

## 1. Freeze-frame / snapshot metadata on faults (HIGH PRIORITY)

**Clarified with the owner (2026-07-03):** the "stored faults only" ask was a
misunderstanding on my part. The owner means the same thing ISTA reads — the ECU's **fault
memory** (the ~20-entry *Fehlerspeicher*) — and **showing active/current faults too is
perfectly fine**. M10's current behaviour already matches this: `read_faults` requests
`19 02 FF` (the whole fault memory) and only hides the status-`0x40`/`0x50` "not tested this
cycle" catalog noise. **So NO fault-filter change is needed — do not "fix" `RELEVANT_MASK`.**

**The real, confirmed ask:** each fault carries **freeze-frame / environmental metadata** —
odometer (km) at occurrence, engine RPM, temperatures, the ignition-cycle/time counter, and
the ECU state when it latched. The owner called this "extremely useful." M10 reads **code +
1-byte status only — no freeze-frame data at all.** This is the gap to close.

**Direction:**
- **Freeze-frames / extended data.** Add the UDS reads ISTA uses:
  - `19 04` reportDTCSnapshotRecordByDTCNumber — the snapshot (freeze-frame) records.
  - `19 06` reportDTCExtendedDataRecordByDTCNumber — extended data (counters, first/last
    occurrence, aging).
  Both return ECU-specific record layouts; **decoding them needs the SGBD/DB definitions**
  (the `XEP_ENVCONDSLABELS`/env-condition tables surveyed in `sqlite-findings.md`, plus the
  SGBD's snapshot result definitions). This is real work: a new UDS service builder + a
  per-ECU snapshot decoder + DB/SGBD-driven labels. Start with the DDE, whose SGBD we have.
- **New data shape.** `FaultInfo` grows an optional `snapshot: Vec<{ label, value, unit }>`
  and `extended: {...}` (occurrence counters, km, etc.). MCP `read_faults`/`read_all_faults`
  surface it; the agent can then say "misfire cyl 3, stored at 142 380 km, 2 100 rpm, coolant
  88 °C, 3 ignition cycles ago."
- Manual on-car step (the M10 capture had **no** `0x19` traffic): capture a real fault-memory
  read to confirm the `59 02` record framing **and** the `59 04`/`59 06` snapshot layouts.

## 2. Full first-connect vehicle-identification dump (like ISTA's)

**The ask:** on connect, ISTA dumps the car's identity — model, engine type, build date,
the vehicle order (FA/Fahrzeugauftrag), and the authoritative **list of which ECUs exist,
by name** — before the user does anything. klartext today reads only the VIN and
probe-scans for fitted addresses.

**Direction:**
- **Identification DIDs.** Read the ISO/BMW identification block per ECU (`F190` VIN,
  `F19E`/`F197` system name, `F1A0`+ BMW-specific: HW/SW part numbers, coding index, etc. —
  see `protocol-reference.md §1.5`). Aggregate into a "vehicle identity" report.
- **The SVT (Systemverbautabelle) — the real fix for "which ECUs exist, by name."** This is
  how ISTA *knows* the fitted ECUs + each one's exact variant in ONE read from the gateway,
  instead of probing. It was deliberately deferred in M10 because the `STEUERN_VCM_GENERATE_SVT_*`
  jobs in `zgw_01.prg` are RoutineControl-class (`0x31`) frames that need BEST-2 disassembly
  to derive, and executing a derived non-read frame is currently barred on the MCP surface.
  **Decision needed:** either (a) treat the SVT read as a *read* (it returns the installed
  list; the routine is a query, not an actuation) and allow it, or (b) keep it CLI-only.
  Deriving the frames from the SGBD + a capture is the work. Payoff is large: fitted list +
  per-ECU variant in one read → completes variant auto-detection (retires the M10 ladder's
  guesswork) and gives ISTA-grade "the car has EKPS, DSC, FEM, DDE…" by name.
- **FA / vehicle order** decode (model, options, build) lives in the gateway's coding data
  (`FA`/`VCM`) — a later sub-item once SVT works.

## 3. ISTA repair-documentation catalog (how to remove/replace/fix)

**The ask:** ISTA ships a huge catalog *next to* the SG_FUNKTIONEN measurements — the
step-by-step docs for how to remove/replace a part, what a fault means, what to check.

**Direction:** these are the `DIAGNOSISDOCUMENT` / `TIGHTENINGTORQUES` / `SI-ENCLOSURE`
documents surveyed in `sqlite-findings.md` (in `xmlvalueprimitive_DEDE.sqlite` + the
encrypted `DiagDocDb` index). The plaintext docs are keyed by opaque global IDs; the index
that maps a **DTC or component → its procedure** is in `DiagDocDb` (now decryptable — we
have the key). Work: extend `build-semantic-db.sh` to also extract the fault→document and
component→document link tables, and add a `describe_procedure`/`fault_help` lookup so the
agent can pull "here's the ISTA procedure for this fault." Large text corpus — scope
carefully (extract links + titles first, full prose on demand).

## 4. Multi-step guided procedures (ISTA Servicefunktionen / ABL)

**The ask:** functions like oil-level calculation, adaptations, bleeding routines are **not
one-shot requests** — they are guided sequences (read preconditions → prompt/act → write →
verify → loop). This is the big one, already flagged as future in M10 §5 and
`service-functions-findings.md`.

**Direction (unchanged from M10's assessment):** needs (a) a **sequence model** — ordered
steps with per-step preconditions/reads/writes/verifies and operator prompts — and (b) an
interpreter for ISTA's **ABL / BEST-2** sequences (the SGBD bytecode `service-functions-findings.md`
sizes at ~98 live opcodes). The BEST-2 interpreter is the same engine that would unlock the
inline-scaling measurement tail (`sgbd-findings.md §7`), so items 4 and the "inline scaling"
tail share infrastructure. **Blast-radius stays paramount:** guided procedures that actuate
remain human-in-the-loop (CLI), never autonomous MCP writes. The oil-level example is a good
first target — mostly reads + a compute, low blast radius.

## 5. `rust-embed` self-contained binary — FEASIBILITY: yes, with a licensing caveat

**The ask:** embed the semantic DB (~33 MB) + the `.prg` files at compile time so the binary
is fully self-contained and portable (~50 MB is fine).

**Assessment — technically yes:**
- **`.prg` files:** trivial. They're already read as byte slices (`Prg::parse(&[u8])`);
  `include_bytes!` or `rust-embed` embeds them directly, no runtime file needed.
- **The SQLite DB:** `rusqlite` needs either a file path or an in-memory DB. Two clean
  options: (a) embed the bytes and `SQLite::deserialize` them into an in-memory DB at
  startup (rusqlite exposes the serialize/deserialize API); or (b) write the embedded bytes
  to a temp file on first run and open read-only. (a) is cleaner and keeps it a single
  artifact. 33 MB in `.rodata` is fine.
- **THE BLOCKER is licensing, not tech.** The whole project's thesis (CLAUDE.md "BYO-data")
  is that BMW's proprietary data is **never committed or embedded**. Embedding the ISTA DB +
  SGBDs into a distributable binary = distributing BMW's proprietary/copyrighted data, which
  the AGPL project cannot ship. **So:** a self-contained build is fine as a **personal, local
  build** for the owner's own car (personal use of their own ISTA data), gated behind a
  cargo feature (e.g. `--features embed-data` that `include_bytes!`s from the gitignored
  `data/`), and the resulting binary is **never published**. The public repo/releases stay
  BYO-data. Recommend: implement the `embed-data` feature (off by default), document it as
  personal-use-only. Easy win once framed this way.

## 6. Mobile app (iOS) over USB-C Ethernet — FEASIBILITY: plausible, real iOS caveats

> **PROGRESS 2026-07-06 — kicked off, and the toolchain question is answered.** A pure-Swift
> networking **probe** is built and **installed on the physical iPhone from Linux/WSL via
> [xtool](https://xtool.sh) — no Mac.** Build → sign → install VERIFIED. This retires the
> "macOS VM" assumption: the whole loop runs on Linux. Spec + outcome:
> `2026-07-06-mobile-ios-networking-probe-design.md` (§7); how-to: `ios/LINUX-BUILD-GUIDE.md`.
> Open items: the on-car test (Interfaces/POSIX-connect/Read-VIN) and whether the Rust core
> (UniFFI, a binary target) links under xtool — do that link spike before the full app.

**The ask:** put the Rust core on a phone (iOS), use a USB-C→Ethernet adapter, and diagnose
the car from the phone.

**Assessment — plausible, with the hard parts being iOS networking, not Rust:**
- **Rust core on iOS:** solved path. Compile the core crates to `aarch64-apple-ios` as a
  static lib; wrap with **UniFFI** (or `swift-bridge`) to generate a Swift API; build a
  SwiftUI front-end. The core is already `async`/tokio and cleanly layered (client / semantic
  / uds / hsfz), so exposing `connect`/`scan`/`read_faults`/… over an FFI boundary is
  straightforward. This is the *easy* 40%.
- **USB-C Ethernet on iOS:** iOS *does* support USB Ethernet adapters (Settings shows an
  Ethernet pane), and apps can open TCP/UDP sockets over the active interface via
  `Network.framework`. The **hard parts**:
  - **Link-local discovery.** HSFZ discovery UDP-broadcasts to `169.254.255.255` and reads
    the reply's source IP. iOS sandboxes broadcast/multicast — you need the
    **`com.apple.developer.networking.multicast` entitlement** (Apple approval required), and
    binding a socket to a *specific* link-local interface / scope is restricted. Directed
    broadcast may be blocked. Mitigation: let the user enter the gateway IP manually (the
    `--gateway-ip` path already exists and skips discovery), sidestepping broadcast entirely
    for v1.
  - **Static link-local config.** On desktop we pin a `169.254.x.x/16` static IP and fight
    NetworkManager. iOS auto-configures the Ethernet interface (usually link-local when no
    DHCP) — you get *an* address but limited control; whether it lands in the car's
    `169.254/16` range and routes is the thing to test on-device.
  - **`Network.framework` vs raw tokio sockets.** tokio's `UdpSocket`/`TcpStream` may not
    bind to the iOS Ethernet interface the way we need; you may have to route socket I/O
    through `Network.framework` (NWConnection) on the Swift side and hand bytes to the Rust
    core, rather than letting Rust own the sockets. That's a bigger refactor of the transport
    boundary (make `klartext-hsfz` sans-I/O over a byte-stream trait, feed it from Swift).
- **Verdict:** feasible for a **manual-IP** v1 (skip discovery, user types the gateway IP),
  with auto-discovery as a later, entitlement-gated iOS-networking research spike. **Android
  is materially easier** (fewer networking restrictions, JNI/`uniffi` binding) if the goal is
  "phone-based diagnostics" and iOS isn't a hard requirement. Recommend prototyping the core
  FFI + a manual-IP flow first on whichever platform, then tackling discovery.

## 7. Bigger-picture (owner's stated direction, not scoped here)

- **HU (head-unit) editor** — the F20 OBD-Ethernet reaches the `L7ENTRYHU` head unit
  (`captures/…` §1). A separate, larger effort.
- **Standalone app** — the mobile/desktop GUI on top of the core (see item 6).

---

## Suggested ordering for the next sessions

1. **Item 1 (freeze-frame / snapshot metadata)** — highest owner value, self-contained,
   informed by an ISTA decompile + one on-car fault capture. Do this first. (No fault-filter
   change — the current fault-memory read already matches what the owner wants.)
2. **Item 2 (SVT + identification dump)** — retires the variant-ladder guesswork; decide the
   read-vs-CLI invariant question up front.
3. **Item 5 (`embed-data` feature)** — small, high-portability win; frame as personal-use.
4. **Item 3 (repair-doc catalog)** — extract links first.
5. **Item 4 (BEST-2 / guided procedures)** — the big one; shares infra with inline-scaling.
6. **Item 6 (mobile)** — after the core FFI story is decided.
