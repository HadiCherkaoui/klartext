# Car session 1 — handoff (full on-car capture, MCP-driven)

**Purpose:** one car session that runs the **entire** pending on-car protocol — not just the P2
job-run additions — and captures every `[verify against capture]` marker at once: the M11
identification/fault/freeze-frame reads (Part 3), the structured BEST/2 reads (Part 3.5), and
the P2 live job-run path (Part 6). **Drive it through the MCP server** (`run_job` and every read
are MCP tools); the CLI is only an optional cross-check.

**Everything here is a READ** (`0x22`/`0x2C`/`0x19`). Do **not** clear faults — we want the fault
memory populated so `read_fault_detail` has freeze-frames to capture. No writes anywhere.

The authoritative step detail is `docs/on-car-verification-protocol.md` (Parts 3, 3.5, 6). This
file is the actionable handoff: what to move, how to update the laptop, and the agent prompt.

---

## Step 1 — what to transfer to the laptop

You already have the SGBD `.prg` data on the laptop, so **the only thing that must move is the
code.** The DB is a maybe (see 1b).

### 1a. Code — the git bundle (required)

| Artifact | Size | Location on this machine |
|---|---|---|
| `klartext-main.bundle` | **840 K** | `~/klartext-main.bundle` (all of `main` at `b49bdfc` — P1 + P2 + docs) |

If the laptop already has a klartext clone (it does — that's where your SGBD data lives),
**update the code in place without disturbing `data/`:**

```bash
cd <laptop repo>
git fetch ~/klartext-main.bundle main            # brings b49bdfc in
git checkout main && git merge --ff-only FETCH_HEAD   # fast-forward; your data/ dir is untouched
cargo build --release -p klartext-cli -p klartext-mcp
```

(If it has no clone: `git clone ~/…/klartext-main.bundle klartext`, then drop your SGBD into
`data/Testmodule(1)/Ecu/` and build.)

### 1b. The semantic DB — probably keep yours; here's how to be sure

**P1 and P2 changed nothing in the DB** — they were all VM/protocol code. So the car session
reads the DB exactly the way your earlier fault/scan reads did. What the session uses the DB for:

| Tool | DB table it needs |
|---|---|
| `scan_ecus`, `identify_vehicle`, and ECU-name→address in `read_data`/`run_job` | `ecu` (names) |
| `read_faults` | `dtc` (fault text) |
| `read_fault_detail` | `envcond` (freeze-frame labels) |
| `run_job`/`list_measurements`/`read_data` measurements | **the SGBD `.prg`, not the DB** |

The "docs linking" you're thinking of (Item 4: the `fault_doc` + `infoobject` tables) feeds the
**offline `fault_help` tool only** — the car session never calls it. So whether your laptop DB
has those tables is irrelevant here.

**Decision:**
- If your laptop DB already reads faults / scans ECUs fine → **use it, it's sufficient.** Quick
  check: `sqlite3 <your.db> "select count(*) from ecu; select count(*) from dtc;"` — both
  non-trivial ⇒ good.
- Want zero doubt (or your `ecu`/`dtc`/`envcond` might predate M10's name work) → **copy this
  machine's DB**, it's current and has everything:

  | Data | Size | Path | Dated |
  |---|---|---|---|
  | `data/klartext-semantic.db` | **51 M** | put at `<repo>/data/klartext-semantic.db` | 2026-07-04 |

- If your **local DB is newer** than 2026-07-04 (you rebuilt it from a fresh ISTA export), keep
  yours — newer is strictly fine; nothing the session needs regressed.

---

## Step 2 — network + capture setup (human)

Per `docs/on-car-verification-protocol.md` Part 0:

- ENET cable in the OBD port; car awake (terminal 15 / ignition on, engine off is fine).
- Host link-local IP on the ENET interface (`169.254.x.x/16`); allow TCP 6801 / UDP 6811.
- Know the gateway IP (discovery, or the known one, e.g. `169.254.90.33`).
- **pcap the whole session** (not just one part — this is the byte-exact record I flip markers
  from):
  ```bash
  doas tcpdump -i <enet-iface> -w captures/car-session-1.pcapng 'tcp port 6801 or udp port 6811'
  ```
  `captures/` is gitignored (holds the VIN). Never commit it.

**Launch the MCP server with frame tracing → stderr log** (stdout stays the JSON-RPC stream):

```bash
RUST_LOG=klartext_client=trace \
./target/release/klartext-mcp \
  --gateway-ip <IP> \
  --sgbd-dir "data/Testmodule(1)/Ecu" \
  2> captures/frames-$(date +%Y%m%d).log
```

Connect it to your MCP client (Claude Desktop / Claude Code) and hand the on-car agent the
prompt below. The pcap + `frames.log` capture **everything** the agent does, for all parts.

---

## Step 3 — on-car agent prompt (copy-paste)

> You are connected to the klartext MCP server for a live BMW F20 **read-only** capture (car
> session 1). Every tool here is a READ — do **NOT** call `clear_faults` or `clear_all_faults`
> or anything that writes; we need the fault memory intact for freeze-frame reads. Run the
> sequence below **in order**, and after each call paste back **(a)** the tool's full JSON
> result and **(b)** a one-line note if anything looks wrong or errored (an error is useful
> data — record it and continue, don't retry-loop).
>
> **A. Identification & faults (M11 Part 3)**
> 1. `connect`
> 2. `identify_vehicle` — expect a real VIN, ~15 ECUs by name, an I-Stufe, and a vehicle order.
>    If `ecus` is empty or `vin` is missing, STOP and report (the SVT framing is the #1 thing to
>    catch).
> 3. `scan_ecus`, then `scan_ecus` again with `{ "rescan": true }`.
> 4. `read_faults` on two or three fitted ECUs (e.g. the DDE `0x12`, the FEM `0x40`) — ones
>    likely to have stored faults.
> 5. `read_fault_detail` for one fault code from step 4 (prefer a DDE `0x12` fault — its decode
>    is the one we can fully check). This is the freeze-frame read.
>
> **B. Live measurement & structured multi-result (Part 3.5 + P2 Part 6)**
> 6. `list_measurements` for the DDE (`0x12`), then `run_job`
>    `{ "ecu": "0x12", "job": "STATUS_LESEN", "args": ["ARG", "ITOEL"] }` — expect `sets` with
>    `STAT_…OEL…_WERT` / `_EINH` / `_INFO` and **`JOB_STATUS = OKAY`**. (Also `read_data` the
>    same oil-temp measurement so we can compare the two read paths.) Use the hex address for
>    `ecu` (name aliases like "DDE" are not guaranteed to resolve).
> 7. `list_measurements` for the DSC (`0x29`), pick a status/bitfield measurement (the
>    wheel-speed / deflation-detection block), note its **ARG name**, then `run_job`
>    `{ "ecu": "0x29", "job": "STATUS_LESEN", "args": ["ARG", "<that ARG name>"] }` — expect
>    **several** distinct `STAT_*` stems in `sets` (the multi-value proof). If it errors that the
>    variant can't be resolved, retry with `"variant": "dsc_10"` added.
>
> **C. Whole-car + close**
> 8. `read_all_faults` — note the wall-clock (this exercises multi-target interleaving).
> 9. `disconnect`

*(Optional CLI cross-check, human, any time — same DDE read over the other surface:*
`./target/release/klartext --gateway-ip <IP> --sgbd "data/Testmodule(1)/Ecu/d72n47a0.prg" --target 12 job run STATUS_LESEN ARG ITOEL`*)*

---

## Step 4 — the one value that matters most

On the `run_job STATUS_LESEN` calls (steps 6–7), watch **`JOB_STATUS`**:
- **`OKAY`** ⇒ the BMW-FAST codec is wire-correct — a real ECU accepted our additive checksum
  and response framing.
- **`ERROR_ECU_INCORRECT_LEN`** (or a transport/checksum error) ⇒ the wire disagrees with what
  we derived offline. That is exactly what this session exists to catch — capture it and send it.

---

## Step 5 — what to send me back

1. **`captures/car-session-1.pcapng`** and **`captures/frames-*.log`** — the whole session
   (VIN redactable: replace the 17 ASCII bytes after `62 F1 90` with `XX…`; I need the framing,
   not the VIN). **This is the most important artifact.**
2. The **JSON** of `identify_vehicle`, one `read_faults`, the `read_fault_detail`, both
   `run_job` results (DDE + DSC), and `scan_ecus`.
3. Any tool/command that errored, with its exact message, and the `read_all_faults` timing.

From that I flip every pending `[verify against capture]` marker (SVT/FA/I-Stufe layout, DTC and
freeze-frame record framing, the four P2 telegram/checksum/multi-value markers) to confirmed — or
fix whatever the wire disagrees with — and then we start **P3** (guided oil-level flow + the first
gated write).
