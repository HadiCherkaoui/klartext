# Car session 1 — handoff (BEST/2 live read path)

**Purpose:** confirm the P2 live read path works on the real F20 — that `klartext job run`
(CLI) and `run_job` (MCP) execute real EDIABAS read jobs and that the BMW-FAST codec is
wire-correct. **Everything here is a READ** (`0x22`/`0x2C`/`0x19`); the read-only gate refuses
writes at the transmit seam. **No clear, no write, nowhere in this session.**

The authoritative step-by-step (what each read confirms, capture hygiene, VIN redaction) is
`docs/on-car-verification-protocol.md` **Part 6**. This file is the actionable handoff:
what to move to the laptop, how to build, and the exact commands + the on-car agent prompt.

---

## Step 1 — transfer to the car laptop

`main` is local-only (never pushed) and the BMW data is gitignored, so **two things move
separately**: the code (a git bundle) and the BYO data (copied by hand).

### 1a. Code — the git bundle (already created)

| Artifact | Size | Location on this machine |
|---|---|---|
| `klartext-main.bundle` | **836 K** | `~/klartext-main.bundle` (code at `d7838d6`, the P2 merge) |

Copy that one file to the laptop. (If the laptop is the same Linux x86_64 as this box, you may
instead copy the two prebuilt binaries `target/release/klartext` and `target/release/klartext-mcp`
and skip Step 2 — but building from the bundle is the safe path.)

### 1b. BYO data — copy by hand (NOT in the bundle; gitignored, contains the VIN)

Place these at the **same relative paths** under the laptop's repo (`<repo>/data/…`):

| Data | Size | Path (under `data/`) | Needed for |
|---|---|---|---|
| Semantic DB | **51 M** | `klartext-semantic.db` | ECU names, the measurement catalog, fault text |
| DDE SGBD | **3.9 M** | `Testmodule(1)/Ecu/d72n47a0.prg` | the CLI + MCP DDE `STATUS_LESEN` read |
| DSC SGBD | **1.4 M** | `Testmodule(1)/Ecu/dsc_10.prg` | the MCP DSC multi-result read |

- **Lean transfer (~57 MB)** — the three files above. Enough for every Part 6 step.
- **Complete transfer (~1.65 GB)** — the whole `data/Testmodule(1)/Ecu/` dir (1405 `.prg`).
  Only needed if you also want `identify_vehicle`/`scan_ecus` to fully resolve every fitted
  ECU by variant. Skip it for Part 6 unless you have the space and want the extra reads.

> **DSC variant caveat:** the MCP `run_job {ecu:"DSC"}` resolves the SGBD variant from the DB
> ladder. If it complains it can't pick one, pass `variant:"dsc_10"` explicitly in the call
> (and make sure `dsc_10.prg` is present). The CLI already pins the DDE via `--sgbd`.

---

## Step 2 — build on the laptop

```bash
git clone ~/…/klartext-main.bundle klartext      # clones `main` at d7838d6
cd klartext
# drop the data/ files from Step 1b into ./data/ at the paths in the table
cargo build --release -p klartext-cli -p klartext-mcp
```

The binaries land at `target/release/klartext` and `target/release/klartext-mcp`.

---

## Step 3 — network + capture setup (human)

Per `docs/on-car-verification-protocol.md` Part 0:

- ENET cable in the OBD port; car awake (terminal 15 / ignition on, engine off is fine).
- Host link-local IP on the ENET interface (`169.254.x.x/16`); allow TCP 6801 / UDP 6811.
- Know the gateway IP (discovery, or the known one, e.g. `169.254.90.33`).
- **Start the pcap** in parallel (this is the byte-exact record I flip markers from):
  ```bash
  doas tcpdump -i <enet-iface> -w captures/car-session-1.pcapng 'tcp port 6801 or udp port 6811'
  ```
  `captures/` is gitignored — it holds the VIN. Never commit it.

---

## Step 4 — CLI capture (human runs these)

Replace `<IP>` with the gateway IP. `--target 12` is the DDE (engine) diagnostic address.

```bash
# (a) list the DDE's jobs — offline, proves the SGBD loads
./target/release/klartext --sgbd "data/Testmodule(1)/Ecu/d72n47a0.prg" job list

# (b) the live read — this is the headline
./target/release/klartext --gateway-ip <IP> \
  --sgbd "data/Testmodule(1)/Ecu/d72n47a0.prg" --target 12 \
  job run STATUS_LESEN ARG ITOEL
```

**Expect (b):** result rows `STAT_…OEL…_WERT` (a number), `…_EINH` (a unit), `…_INFO`
(German text), and **`JOB_STATUS = OKAY`**.

> ⚠️ **The one thing to watch:** `JOB_STATUS`. **`OKAY`** = the codec is wire-correct (a real
> ECU accepted our additive checksum + response framing). **`ERROR_ECU_INCORRECT_LEN`** (or a
> transport/checksum error) = the wire disagrees with what we derived — capture it and send it;
> that is exactly what this session exists to catch.

---

## Step 5 — MCP capture

**Human — launch the server** (stdout is the JSON-RPC stream; logs + frame trace go to stderr):

```bash
RUST_LOG=klartext_client=trace \
./target/release/klartext-mcp \
  --gateway-ip <IP> \
  --sgbd-dir "data/Testmodule(1)/Ecu" \
  2> captures/frames-$(date +%Y%m%d).log
```

Connect it to your MCP client (Claude Desktop / Claude Code), then hand the on-car agent the
prompt below.

**On-car agent prompt (copy-paste):**

> You are connected to the klartext MCP server for a live BMW F20 read-only capture (car
> session 1). Every tool you call here is a READ — do NOT call `clear_faults`,
> `clear_all_faults`, or anything that writes. Run these in order and paste back each tool's
> full JSON result plus a one-line note if anything looks wrong:
>
> 1. `connect`
> 2. `run_job` with `{ "ecu": "DDE", "job": "STATUS_LESEN", "args": ["ARG", "ITOEL"] }`
>    — expect `sets` with `STAT_…OEL…_WERT`/`_EINH`/`_INFO` and `JOB_STATUS = OKAY`.
> 3. `list_measurements` for the DSC (ecu `"DSC"` or `0x29`) — pick a status/bitfield
>    measurement (the wheel-speed / deflation block) and note its **ARG name**.
> 4. `run_job` with `{ "ecu": "DSC", "job": "STATUS_LESEN", "args": ["ARG", "<that ARG name>"] }`
>    — expect **several** distinct `STAT_*` stems in `sets` (this is the multi-value proof). If
>    it errors that the variant can't be resolved, retry with `"variant": "dsc_10"` added.
> 5. `disconnect`
>
> Report any tool that errors verbatim (an error is useful data). Do not retry-loop.

---

## Step 6 — what to send me back

1. **`captures/car-session-1.pcapng`** and **`captures/frames-*.log`** (VIN redactable — replace
   the 17 ASCII bytes after `62 F1 90` with `XX…`; I need the framing, not the VIN).
2. The **CLI output** of `job run STATUS_LESEN ARG ITOEL` (esp. the `JOB_STATUS` line).
3. The **MCP JSON** from steps 2 and 4 above.
4. Any tool/command that errored, and its exact message.

From that I flip the four `[verify against capture]` markers (request telegram shape, the
additive checksum a real ECU accepts, response layout, multi-value surfacing) to confirmed —
or fix whatever the wire disagrees with — and then we start **P3** (the guided oil-level flow
and the first gated write).
