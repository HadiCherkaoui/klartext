# klartext

A native-Rust BMW diagnostic & coding tool for F-series cars (target: a 2014 F20), speaking
UDS over the BMW-proprietary **HSFZ** transport across an ENET (Ethernet) cable.

> **Milestone 1 — HSFZ transport bring-up (this milestone).** Connect to the gateway, complete
> the HSFZ exchange, send one UDS request, and print the decoded response. Full diagnostics,
> replay-coding, and an MCP server are later milestones.

## Status

- ✅ HSFZ framing (encode/decode) — implemented from `docs/protocol-reference.md`
- ✅ Async TCP connection — connect + `TCP_NODELAY`, segment reassembly, ack-skip, bounded NRC-0x78 retry
- ✅ Minimal UDS — TesterPresent and DiagnosticSessionControl request/response
- ✅ Thin CLI — connect → request → decode, plus an opt-in `discover` helper
- ⏳ End-to-end test against a real F20 — **your step** (see *Manual hardware test*)

HSFZ is reverse-engineered from the report; **no packet capture is committed** (BYO-data —
captures contain the VIN). Several protocol values are unverified, so the CLI prints them as a
checklist to confirm against your car (see *Verify against a capture*).

## Layout (Cargo workspace)

| Crate | Package | Role |
|---|---|---|
| `crates/uds` | `klartext-uds` | Pure UDS (ISO 14229) message encode/decode. No transport, no async. |
| `crates/hsfz` | `klartext-hsfz` | The concrete HSFZ transport: frame codec + async connection. |
| `crates/cli` | `klartext-cli` | The `klartext` binary; composes the two crates. |

The core (`uds`, `hsfz`) is reusable by later binaries. Future siblings: `klartext-semantic`,
`klartext-mcp`, `klartext-doip`. There is deliberately **no `Transport` trait** yet — one
transport exists today; a trait gets extracted when DoIP is added.

## Build & test

```sh
cargo build --workspace
cargo test --workspace                               # frame + UDS unit tests (report byte vectors)
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
```

## Usage

Find the gateway (BMW gateways usually sit on an unconfigured link-local `169.254.x.x` address):

```sh
klartext discover           # broadcasts 00 00 00 00 00 11 on UDP 6811, dumps replies + source IP
```

Run one request against the gateway:

```sh
klartext 169.254.0.10                        # default: TesterPresent (3E 00) -> expect 7E 00
klartext 169.254.0.10 --request dsc-extended # DiagnosticSessionControl 10 03
KLARTEXT_GATEWAY=169.254.0.10 klartext       # IP via env var
```

Flags: `--request {tester-present|dsc-default|dsc-extended}`, `--target <hex>` (default `10` = ZGW),
`--port` (default 6801), `--timeout` (read, ms), `--connect-timeout` (ms). See `klartext --help`.

## Manual hardware test (your step)

We can't reach the car — the unit tests only cover framing against known byte vectors, and we never
claim a hardware round-trip works. To validate end-to-end:

1. Connect the ENET cable; give your NIC a link-local `169.254.x.x` address; wake the car (terminal 15).
2. `klartext discover` → note the responder's IP (that's your gateway).
3. `klartext <that-ip>` → you should see `✔ POSITIVE response — SID 0x7E`.
4. If you instead get a read timeout or a decode error, suspect **item 1** below — the HSFZ length
   convention is the most likely culprit, and the error prints the raw bytes to compare.

## Verify against a capture

These reverse-engineered values are printed by the CLI after every real run. Confirm them with
Wireshark (it has HSFZ/DoIP dissectors) on the ENET link (report Part 6):

1. **HSFZ LENGTH semantics** — counts `SRC+TGT+UDS` (= `2 + len(UDS)`), excluding the 6-byte
   length+control header. *Highest priority:* the report self-contradicts; resolved via Scapy and
   the `00 00 00 00 00 11` discovery datagram (LENGTH=0 with a control word present).
2. Diagnostic port **TCP 6801**, control/ident port **UDP 6811** — ICOM setups reassign these.
3. Tester address **0xF4**, ZGW/gateway **0x10** — scan targets to see which answer.
4. Connect timeout **5000 ms** (ediabaslib) vs **20000 ms** (EDIABAS.INI) — set via `--connect-timeout`.
5. **P2 = 50 ms / P2\* = 5000 ms** — ISO defaults; the F20 reports its own in the `10 03` response.
6. Control words **0x01/0x02/0x11/0x12** — corroborated but proprietary.
7. **0x11 identification-string layout** — unparsed in M1; `klartext discover` dumps it raw to capture.
8. **Alive-check (0x12)** direction/interval, and whether `3E 80` alone holds the session — later milestone.

## Safety

Milestone 1 only sends TesterPresent (no side effects) or DiagnosticSessionControl (a session
change that auto-reverts after the ~5 s S3 timeout). No writes, no actuation, no flashing. Reads
are safe; state-changing operations in later milestones will require explicit confirmation and
read-back of the original bytes.

## License

AGPL-3.0-only. Protocols are implemented from the report and ISO standards (frame layouts and
handshakes are facts, not copyrightable); no code is copied from GPL reference libraries such as Scapy.
