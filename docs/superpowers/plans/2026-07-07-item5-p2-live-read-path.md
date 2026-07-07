# M11 Item 5 / P2 — Live Read Path Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run real EDIABAS read jobs against the live car — `klartext job run/list` on the CLI and a read-only `run_job` MCP tool — by bridging `klartext-best`'s VM to `klartext-client`'s `Session` through a BMW-FAST telegram codec and a read-only SID gate, surfacing multi-set named results.

**Architecture:** The VM's `xsend` emits a full **BMW-FAST telegram** (`[0x80|len][target][source][uds…][checksum]`), while `Session::request(target, uds)` speaks **bare UDS** and does its own HSFZ framing. A new `telegram` codec (in `klartext-best`) translates between them; a `TelegramExchange<T: BareUdsTransport>` wraps the codec around a bare-UDS transport; a `GatedExchange<E>` with a `ReadOnly` policy classifies the embedded SID and refuses writes. The live stack is `GatedExchange::read_only(TelegramExchange::new(SessionBridge{client}))`, assembled in each binary (the only place `klartext-best` and `klartext-client` meet — spec §4 "no client↔best dependency").

**Tech Stack:** Rust edition 2024, tokio, clap (derive), rmcp 1.x, thiserror, async-trait. Reference (facts only, cite `file:line`, never copy): ediabaslib at `/tmp/[local-scratch]/scratchpad/ref/ediabaslib/EdiabasLib/EdiabasLib/`.

## Global Constraints

- **License wall:** ediabaslib is GPL; this repo is AGPL-3.0. Read for FACTS (framing, checksum, length rules) and cite `file:line` in doc comments; NEVER copy code, identifiers, or comment prose. Write your own test vectors.
- **BYO-data:** never commit `.prg`/DB bytes or anything under `data/`. Tests needing real SGBDs skip-if-absent (`data/Testmodule(1)/Ecu/<file>.prg`). Hand-built byte fixtures are fine.
- **No degrade-to-raw in the VM/codec:** a malformed telegram, a failed length/target check, or a gated SID is a hard, named error carrying the offending bytes — never a silent empty or guessed value.
- **No hardcoding** (owner rule): ECU addresses, variants, and job names come from args/resolution, never baked in.
- **MCP stdout is sacred:** only the JSON-RPC stream may touch stdout. All logging → stderr (`tracing`, already configured `mcp/src/main.rs:16-23`). A single stray `println!` in the MCP path silently kills the transport.
- **P2 boundary (spec §9):** this plan builds the **ReadOnly** gate and read jobs only. Do NOT build `Policy::ConfirmedWrite`, the write ritual, the flow runner, the oil-level flow, or the LERNWERTE write — those are P3. The gate enum gets exactly one variant this milestone.
- **MCP invariant (spec §6, owner ruling 2026-07-06):** `run_job` is exposed on MCP as **read-only** — the `ReadOnly` gate refuses every write SID at the transmit seam, so an agent can run measurement/status jobs but a write-emitting job dies at the gate. The full CLAUDE.md tier-ladder rewrite is P3; P2 only updates the MCP module-doc invariant + surface test to admit `run_job` as a gated read tool.
- **Gates per task, each checked by direct exit code — never pipe a gate through `| tail` or mask it:** `cargo fmt` (run via Bash, not the editor hook), `cargo clippy --workspace --all-targets -- -D warnings`, `cargo test -p <crate>` (or `--workspace` where noted).
- **No ms-rust marker comments** (e.g. `// Rust guideline compliant`) anywhere.
- Conventional commits; commit exactly the files each task names.
- Doc comments state the EDIABAS/BMW semantics being mirrored + the `file:line` cite, never narrate the Rust.

## Reference facts pinned during planning (cite these)

| Fact | Source |
|---|---|
| BMW-FAST telegram = `[fmt][target][source][data…][checksum]`; `fmt = 0x80\|len` (short form, `len` = data length 1..63 in the low 6 bits) | frozen contract `crates/best/tests/differential.rs:10-24`; EdInterfaceObd.cs BMW-FAST path |
| Telegram length: `len = fmt & 0x3F`; if `len != 0` → total data bytes = `len`, header = 3 (`telLength = len + 3`). If `len == 0` → length byte at `data[3]`: `data[3]!=0` → `telLength = data[3] + 4`; `data[3]==0` → `telLength = (data[4]<<8)+data[5] + 6` | `EdInterfaceBase.cs:881-905` (`TelLengthBmwFast`) |
| Checksum = **additive `u8` sum** of all bytes before the checksum position (`sum += data[i]`), NOT XOR | `EdInterfaceBase.cs:933-941` (`CalcChecksumBmwFast`) |
| The DDE/DSC `STATUS_LESEN` rows use the STATIC `0x22` read; observed request telegram `83 12 F1 22 45 17` (checksum omitted in that note; the job strips the trailing checksum without verifying its value, but a real ECU verifies what we SEND) | frozen contract `differential.rs:15-19` |
| Response the VM accepts: `[0x80\|len][0xF1][ecu][uds…][checksum]`; job checks `resp[1]==0xF1`, `resp[2]==ecu`, and `total == 1 + headerSize + dataLen`; wrong length → `JOB_STATUS=ERROR_ECU_INCORRECT_LEN` | frozen contract `differential.rs:19-24` |
| Job argument buffer = EDIABAS `;`-joined string, e.g. `"ARG;ITOEL"` (`SPALTE;value…`), decoded per-byte Latin-1 and split on `;` | `differential.rs:12-15`; `crates/best/src/exec.rs:1757-1763` |
| `Session::request(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ClientError>` — bare UDS in, bare UDS out (HSFZ framing internal) | `crates/client/src/session.rs:183` |
| `DiagnosticClient.session` is private with no accessor | `crates/client/src/client.rs:139` |
| SID gate classes (spec §6): pass `0x10 0x3E 0x22 0x2C 0x19`; gated `0x2E 0x31 0x2F 0x14 0x27`; refuse-always `0x34..=0x37` | spec §6 |
| `Ecu::open(path) -> Result<Ecu, RunError>`; `Ecu::run_job(&self, name: &str, target: u8, args: &[u8], exchange: &dyn UdsExchange) -> Result<ResultSet, RunError>` | `crates/best/src/engine.rs:119,142` |
| `ResultSet` exposes only the current (last) set publicly; `sets` field private | `crates/best/src/result.rs:12-17,68-71` |
| MCP tool registration: `#[tool(description=…)]` on `async fn(&self, Parameters(req): Parameters<Req>) -> Result<Json<Res>, McpError>`; `advertised_tools()` at `server.rs:97-103`; surface test `mcp/tests/integration.rs:854-904` asserts exact 14-tool list + forbidden substrings incl. `"run"` | recon |
| `resolve_list_variant(&self, variant: Option<&str>, ecu: Option<&str>) -> Result<String, McpError>`; `sgbd_path(&self, variant: &str) -> Option<PathBuf>` | `mcp/src/server.rs:250,123` |
| `list_measurements` truncation guard: `const MAX_LISTED_MEASUREMENTS: usize = 200`; `total = matching.len()`; `.take(MAX)`; note when `shown < total` | `mcp/src/server.rs:52,935-986` |
| sgbd table cells already decode CP-1252 (`cp1252::decode`); exec.rs `read_string` uses per-byte Latin-1 (`char::from(b)`) — the two differ only for `0x80..=0x9F` | `crates/sgbd/src/prg.rs:257`; `crates/best/src/exec.rs:1257-1261` |

---

### Task 1: Enabling seams — `ExchangeError` variants + client request passthrough

**Files:**
- Modify: `crates/best/src/exchange.rs` (the `ExchangeError` enum + its doc)
- Modify: `crates/client/src/client.rs` (add `DiagnosticClient::request`)
- Test: inline in both files' `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: existing `ExchangeError::Unexpected(Vec<u8>)`; `Session::request` (client-internal).
- Produces (later tasks rely on EXACTLY):
  - `ExchangeError::Refused { sid: u8, frame: Vec<u8> }` — a gated SID at the read-only seam.
  - `ExchangeError::Transport(String)` — a live transport failure, message-only (keeps
    `klartext-best` free of a `klartext-client` dependency; the binary formats `ClientError` into it).
  - `pub async fn DiagnosticClient::request(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, klartext_client::ClientError>` — a thin passthrough to the private `session`.

- [ ] **Step 1: Write the failing tests**

In `crates/best/src/exchange.rs` tests:
```rust
#[test]
fn refused_and_transport_variants_carry_context() {
    let r = ExchangeError::Refused { sid: 0x2E, frame: vec![0x83, 0x12, 0xF1, 0x2E, 0x50] };
    assert!(format!("{r}").contains("2E"));
    let t = ExchangeError::Transport("no response".into());
    assert!(format!("{t}").contains("no response"));
}
```
In `crates/client/src/client.rs` tests, follow the file's existing loopback/mock test pattern (grep for how other `DiagnosticClient` methods are unit-tested — they drive a mock transport):
```rust
#[tokio::test]
async fn request_forwards_bare_uds_to_the_session() {
    // Build a DiagnosticClient over the crate's existing test transport/loopback,
    // arrange a canned response for a 22 F1 90 read, and assert `request` returns it.
    // Mirror the setup the neighboring read_did test uses.
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-best refused_and_transport && cargo test -p klartext-client request_forwards`
Expected: FAIL — variants/method do not exist yet (compile error).

- [ ] **Step 3: Implement**

`exchange.rs` — add to `ExchangeError`:
```rust
/// A gated UDS service ID reached the read-only seam and was refused (spec §6).
/// Carries the SID and the full telegram that was blocked.
#[error("read-only gate refused service 0x{sid:02X} (frame {frame:02X?})")]
Refused {
    /// The UDS service ID (the byte after the telegram header) that was gated.
    sid: u8,
    /// The full outgoing telegram that was refused.
    frame: Vec<u8>,
},
/// The live transport failed. Message-only so `klartext-best` need not depend on
/// `klartext-client`; the binary's bridge formats its `ClientError` into this.
#[error("transport error: {0}")]
Transport(String),
```

`client.rs` — add to `impl DiagnosticClient`:
```rust
/// Sends a raw UDS request to `target` and returns the raw response payload.
///
/// A thin passthrough to the managed [`Session`], exposing the one primitive the
/// BEST/2 job engine's live exchange bridge needs without leaking the session type.
///
/// # Errors
/// As [`Session::request`].
pub async fn request(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ClientError> {
    self.session.request(target, uds).await
}
```
Confirm `ClientError` is `pub` and re-exported from `crates/client/src/lib.rs` (recon says the crate root re-exports `Session`; ensure `ClientError` is reachable as `klartext_client::ClientError` — add the re-export if missing).

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p klartext-best && cargo test -p klartext-client`
Expected: PASS. Update any exhaustive `match` on `ExchangeError` the compiler flags (the `MockExchange` and `differential.rs` only construct `Unexpected`, so a wildcard or the added arms may be needed — add explicit arms, no wildcard).

- [ ] **Step 5: Gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best && cargo test -p klartext-client
git add crates/best/src/exchange.rs crates/client/src/client.rs crates/client/src/lib.rs
git commit -m "feat(best,client): ExchangeError Refused/Transport + DiagnosticClient::request"
```

---

### Task 2: BMW-FAST telegram codec

**Files:**
- Create: `crates/best/src/telegram.rs`
- Modify: `crates/best/src/lib.rs` (declare `mod telegram;` + re-export)
- Modify: `crates/best/tests/differential.rs` (refactor `DidExchange::frame` to use the codec — DRY the test onto the production codec)

**Interfaces:**
- Consumes: nothing from earlier tasks.
- Produces (later tasks rely on EXACTLY):
  - `pub fn encode(target: u8, source: u8, uds: &[u8]) -> Vec<u8>` — builds a short-form
    telegram `[0x80|len][target][source][uds…][checksum]` with the **additive** checksum.
    Panics (debug) / returns a `uds.len() <= 63` precondition — a job never emits a longer
    single frame; document the bound.
  - `pub struct Telegram { pub target: u8, pub source: u8, pub uds: Vec<u8> }`
  - `pub fn decode(frame: &[u8]) -> Result<Telegram, TelegramError>` — validates the length
    per `TelLengthBmwFast` (short form + the two long forms), verifies the additive checksum,
    and returns the header + UDS payload.
  - `pub fn peek_sid(frame: &[u8]) -> Option<u8>` — the UDS SID (`uds[0]`) without full
    validation, for the gate to classify a frame cheaply.
  - `pub enum TelegramError { TooShort, BadLength { declared: usize, actual: usize }, BadChecksum { expected: u8, found: u8 } }` (thiserror).

- [ ] **Step 1: Write the failing tests** (`crates/best/src/telegram.rs` tests)

```rust
#[test]
fn encode_matches_the_observed_static_read_telegram() {
    // Frozen contract: 83 12 F1 22 45 17 is a static 0x22 read of DID 0x4517
    // to target 0x12 from source 0xF1. 0x83 = 0x80|3 (three UDS bytes).
    // The additive checksum (CalcChecksumBmwFast, EdInterfaceBase.cs:933) of
    // [0x83,0x12,0xF1,0x22,0x45,0x17] = 0x83+0x12+0xF1+0x22+0x45+0x17 mod 256.
    let frame = encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
    assert_eq!(&frame[..6], &[0x83, 0x12, 0xF1, 0x22, 0x45, 0x17]);
    let sum = frame[..6].iter().fold(0u8, |a, &b| a.wrapping_add(b));
    assert_eq!(frame[6], sum);
    assert_eq!(frame.len(), 7);
}

#[test]
fn decode_roundtrips_encode() {
    let frame = encode(0x12, 0xF1, &[0x62, 0x45, 0x17, 0x0A, 0xBC]);
    let t = decode(&frame).unwrap();
    assert_eq!(t.target, 0x12);
    assert_eq!(t.source, 0xF1);
    assert_eq!(t.uds, vec![0x62, 0x45, 0x17, 0x0A, 0xBC]);
}

#[test]
fn decode_rejects_a_bad_additive_checksum() {
    let mut frame = encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
    let last = frame.len() - 1;
    frame[last] ^= 0xFF;
    assert!(matches!(decode(&frame), Err(TelegramError::BadChecksum { .. })));
}

#[test]
fn decode_rejects_a_truncated_frame() {
    assert!(matches!(decode(&[0x83, 0x12]), Err(TelegramError::BadLength { .. } | TelegramError::TooShort)));
}

#[test]
fn peek_sid_reads_the_uds_service_byte() {
    let frame = encode(0x12, 0xF1, &[0x2E, 0x10, 0x01]);
    assert_eq!(peek_sid(&frame), Some(0x2E));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-best --lib telegram`
Expected: FAIL — module does not exist.

- [ ] **Step 3: Implement** `crates/best/src/telegram.rs`

Implement `encode`/`decode`/`peek_sid`/`TelegramError` per the Interfaces block. Length logic
mirrors `TelLengthBmwFast` (`EdInterfaceBase.cs:881-905`): short form `len = fmt & 0x3F`,
`telLength = len + 3` when `len != 0`; handle the two long forms (`len == 0`) for `decode`
robustness even though `encode` only emits short form (a job's reads fit 63 bytes). Checksum is
the additive sum (`CalcChecksumBmwFast`, `EdInterfaceBase.cs:933-941`) — cite both. `decode`
computes `telLength`, requires `frame.len() >= telLength + 1` (the +1 is the checksum), verifies
`sum(frame[..telLength]) == frame[telLength]`, and returns `target = frame[1]`, `source = frame[2]`,
`uds = frame[3..telLength]`.

Add to `lib.rs`:
```rust
mod telegram;
#[doc(inline)]
pub use telegram::{Telegram, TelegramError, decode, encode, peek_sid};
```

- [ ] **Step 4: Refactor the differential test onto the codec**

In `crates/best/tests/differential.rs`, replace `DidExchange::frame`'s hand-rolled XOR framing
with `klartext_best::encode(ecu, 0xF1, uds)`. NOTE the behavioral change: the test previously
used XOR; the real codec uses the additive sum. The VM job strips the checksum without verifying
its value (contract note `differential.rs:21-22`), so both tests still pass — confirm they do.
This proves the codec produces frames the VM accepts and DRYs the test onto production code.

- [ ] **Step 5: Run + gates + commit**

Run: `cargo test -p klartext-best` (lib + differential, real data present → differential must still pass)
Expected: PASS.
```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/src/telegram.rs crates/best/src/lib.rs crates/best/tests/differential.rs
git commit -m "feat(best): BMW-FAST telegram codec (additive checksum, length rules)"
```

---

### Task 3: `TelegramExchange` live bridge + `BareUdsTransport`

**Files:**
- Create: `crates/best/src/bridge.rs`
- Modify: `crates/best/src/lib.rs` (declare + re-export)

**Interfaces:**
- Consumes: `telegram::{encode, decode, Telegram}` (Task 2); `UdsExchange`/`ExchangeError` (`exchange.rs`, `ExchangeError::Transport` from Task 1); `async_trait` (already a dep).
- Produces (later tasks rely on EXACTLY):
  - `#[async_trait] pub trait BareUdsTransport { async fn call(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError>; }`
    — bare UDS in, bare UDS out; the seam a binary implements over `DiagnosticClient::request`.
  - `pub struct TelegramExchange<T: BareUdsTransport> { inner: T }` + `pub fn new(inner: T) -> Self`.
  - `impl<T: BareUdsTransport + Sync> UdsExchange for TelegramExchange<T>` — decodes the VM's
    outgoing telegram, cross-checks its embedded target against `run_job`'s `target` param,
    calls `inner.call(target, uds)`, and re-encodes the bare response as `[0x80|len][0xF1][target][resp][checksum]`.

Rationale for a new trait (clears the anti-overengineering bar): `BareUdsTransport` has TWO impls
— each binary's session bridge (Task 7/8) and this task's unit-test mock — and it draws the
`client`-free boundary spec §4 mandates. `TelegramExchange` holds all the translation logic once,
so cli and mcp share it.

- [ ] **Step 1: Write the failing tests** (`crates/best/src/bridge.rs` tests)

```rust
struct MockBare { expect_target: u8, expect_uds: Vec<u8>, respond: Vec<u8> }

#[async_trait::async_trait]
impl BareUdsTransport for MockBare {
    async fn call(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
        assert_eq!(target, self.expect_target);
        assert_eq!(uds, &self.expect_uds[..]);
        Ok(self.respond.clone())
    }
}

#[tokio::test]
async fn bridge_translates_telegram_to_bare_and_back() {
    // The VM hands a telegram; the bridge must strip framing, call the bare
    // transport with (target, uds), and re-frame the bare response.
    let bare = MockBare {
        expect_target: 0x12,
        expect_uds: vec![0x22, 0x45, 0x17],
        respond: vec![0x62, 0x45, 0x17, 0x0A, 0xBC],
    };
    let ex = TelegramExchange::new(bare);
    let request = crate::encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
    let response = ex.request(0x12, &request).await.unwrap();
    // Response must be a valid telegram from 0xF1/ecu carrying the bare bytes.
    let t = crate::decode(&response).unwrap();
    assert_eq!(t.source, 0xF1);
    assert_eq!(t.target, 0x12);
    assert_eq!(t.uds, vec![0x62, 0x45, 0x17, 0x0A, 0xBC]);
}

#[tokio::test]
async fn bridge_rejects_a_target_mismatch() {
    // A telegram addressed to 0x12 but run_job called with target 0x40 is a hard error.
    let bare = MockBare { expect_target: 0x12, expect_uds: vec![], respond: vec![] };
    let ex = TelegramExchange::new(bare);
    let request = crate::encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
    assert!(ex.request(0x40, &request).await.is_err());
}

#[tokio::test]
async fn bridge_surfaces_a_transport_error() {
    struct Failing;
    #[async_trait::async_trait]
    impl BareUdsTransport for Failing {
        async fn call(&self, _t: u8, _u: &[u8]) -> Result<Vec<u8>, ExchangeError> {
            Err(ExchangeError::Transport("no response".into()))
        }
    }
    let ex = TelegramExchange::new(Failing);
    let request = crate::encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
    assert!(matches!(ex.request(0x12, &request).await, Err(ExchangeError::Transport(_))));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-best --lib bridge`
Expected: FAIL — module/types do not exist.

- [ ] **Step 3: Implement** `crates/best/src/bridge.rs`

`request` decodes the incoming telegram (`telegram::decode`; a `TelegramError` maps to
`ExchangeError::Unexpected(frame.to_vec())`), returns `ExchangeError::Unexpected` if
`decoded.target != target` (the target cross-check — the run loop's target is authoritative),
calls `self.inner.call(target, &decoded.uds).await?`, and re-encodes with
`telegram::encode(target, 0xF1, &bare_response)` so the source byte is the tester `0xF1` the VM
expects (contract `differential.rs:20`). Add to `lib.rs`:
```rust
mod bridge;
#[doc(inline)]
pub use bridge::{BareUdsTransport, TelegramExchange};
```

- [ ] **Step 4: Run + gates + commit**

Run: `cargo test -p klartext-best`
Expected: PASS.
```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/src/bridge.rs crates/best/src/lib.rs
git commit -m "feat(best): TelegramExchange bridge + BareUdsTransport seam"
```

---

### Task 4: `GatedExchange` — the read-only SID gate

**Files:**
- Create: `crates/best/src/gate.rs`
- Modify: `crates/best/src/lib.rs` (declare + re-export)

**Interfaces:**
- Consumes: `telegram::peek_sid` (Task 2); `UdsExchange`/`ExchangeError::Refused` (Task 1);
  `async_trait`.
- Produces (later tasks rely on EXACTLY):
  - `pub enum Policy { ReadOnly }` — the ONLY variant this milestone (P3 adds `ConfirmedWrite`).
  - `pub enum SidClass { Pass, Gated, RefuseAlways }` + `pub fn classify(sid: u8) -> SidClass`.
  - `pub struct GatedExchange<E: UdsExchange> { inner: E, policy: Policy }` +
    `pub fn read_only(inner: E) -> Self`.
  - `impl<E: UdsExchange + Sync> UdsExchange for GatedExchange<E>` — peeks the telegram's SID;
    under `ReadOnly`, a `Gated` or `RefuseAlways` SID returns `ExchangeError::Refused { sid, frame }`;
    a `Pass` SID delegates the whole telegram to `inner`.

- [ ] **Step 1: Write the failing tests** (`crates/best/src/gate.rs` tests)

```rust
#[test]
fn classify_covers_the_spec_6_classes() {
    for sid in [0x10, 0x3E, 0x22, 0x2C, 0x19] {
        assert!(matches!(classify(sid), SidClass::Pass), "0x{sid:02X} should pass");
    }
    for sid in [0x2E, 0x31, 0x2F, 0x14, 0x27] {
        assert!(matches!(classify(sid), SidClass::Gated), "0x{sid:02X} should be gated");
    }
    for sid in [0x34, 0x35, 0x36, 0x37] {
        assert!(matches!(classify(sid), SidClass::RefuseAlways), "0x{sid:02X} refuse");
    }
}

#[tokio::test]
async fn read_only_passes_a_22_read() {
    // Inner records what it received; a 0x22 telegram must reach it unchanged.
    let inner = RecordingExchange::default();
    let gate = GatedExchange::read_only(inner);
    let frame = crate::encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
    gate.request(0x12, &frame).await.unwrap();
    assert_eq!(gate.inner_last(), Some(frame));
}

#[tokio::test]
async fn read_only_refuses_a_2e_write_at_the_seam() {
    let gate = GatedExchange::read_only(RecordingExchange::default());
    let frame = crate::encode(0x12, 0xF1, &[0x2E, 0x10, 0x01, 0xFF]);
    match gate.request(0x12, &frame).await {
        Err(ExchangeError::Refused { sid, .. }) => assert_eq!(sid, 0x2E),
        other => panic!("expected Refused, got {other:?}"),
    }
    // No write frame reached the inner transport.
    assert_eq!(gate.inner_last(), None);
}

#[tokio::test]
async fn read_only_refuses_flashing_services() {
    let gate = GatedExchange::read_only(RecordingExchange::default());
    let frame = crate::encode(0x12, 0xF1, &[0x34, 0x00]);
    assert!(matches!(gate.request(0x12, &frame).await, Err(ExchangeError::Refused { .. })));
}
```
Write a small `RecordingExchange` test double (impl `UdsExchange`, stores the last request,
returns a canned framed response) inside the test module; expose `inner_last()` via a helper
on `GatedExchange` gated behind `#[cfg(test)]`, or make the double hold an `Arc<Mutex<..>>` the
test can read directly — your call, keep it test-local.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-best --lib gate`
Expected: FAIL — module/types do not exist.

- [ ] **Step 3: Implement** `crates/best/src/gate.rs`

`classify` per the spec §6 table (cite spec §6). The `UdsExchange` impl: `peek_sid(frame)` →
`None` (unparseable) is itself a refusal-worthy `ExchangeError::Unexpected(frame.to_vec())`;
`Some(sid)` → `match (self.policy, classify(sid))`: `(ReadOnly, Pass)` delegates
`self.inner.request(target, frame).await`; `(ReadOnly, Gated | RefuseAlways)` →
`Err(ExchangeError::Refused { sid, frame: frame.to_vec() })`. Add to `lib.rs`:
```rust
mod gate;
#[doc(inline)]
pub use gate::{GatedExchange, Policy, SidClass, classify};
```

- [ ] **Step 4: Run + gates + commit**

Run: `cargo test -p klartext-best`
Expected: PASS.
```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/src/gate.rs crates/best/src/lib.rs
git commit -m "feat(best): GatedExchange read-only SID gate (spec §6 classes)"
```

---

### Task 5: `ResultSet` all-sets accessor

**Files:**
- Modify: `crates/best/src/result.rs`
- Test: `crates/best/src/result.rs` tests

**Interfaces:**
- Consumes: existing `ResultSet` (private `sets: Vec<Vec<(String, ResultData)>>`).
- Produces (Tasks 7/8 rely on EXACTLY):
  - `pub fn sets_len(&self) -> usize` — number of result sets.
  - `pub fn iter_sets(&self) -> impl Iterator<Item = impl Iterator<Item = (&str, &ResultData)>>`
    — every set in order, each yielding its name/value pairs. The existing current-set API
    (`get`, `iter_current`) is unchanged.

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn iter_sets_exposes_every_set_in_order() {
    let mut rs = ResultSet::new();
    rs.push_named("A", ResultData::Byte(1));
    rs.new_set();
    rs.push_named("B", ResultData::Byte(2));
    rs.push_named("C", ResultData::Byte(3));
    assert_eq!(rs.sets_len(), 2);
    let collected: Vec<Vec<(&str, &ResultData)>> =
        rs.iter_sets().map(|s| s.collect()).collect();
    assert_eq!(collected[0], vec![("A", &ResultData::Byte(1))]);
    assert_eq!(collected[1], vec![("B", &ResultData::Byte(2)), ("C", &ResultData::Byte(3))]);
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p klartext-best --lib iter_sets`
Expected: FAIL — method not found.

- [ ] **Step 3: Implement**

```rust
/// The number of result sets (EDIABAS job result-set count).
pub fn sets_len(&self) -> usize {
    self.sets.len()
}

/// Iterates every result set in order, each yielding its name/value pairs.
///
/// Unlike [`iter_current`](Self::iter_current) (the last set only), this surfaces
/// a multi-set job's full output (e.g. one set per cylinder).
pub fn iter_sets(&self) -> impl Iterator<Item = impl Iterator<Item = (&str, &ResultData)>> {
    self.sets
        .iter()
        .map(|set| set.iter().map(|(n, v)| (n.as_str(), v)))
}
```

- [ ] **Step 4: Run + gates + commit**

Run: `cargo test -p klartext-best`
Expected: PASS.
```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-best
git add crates/best/src/result.rs
git commit -m "feat(best): ResultSet::iter_sets + sets_len for multi-set surfacing"
```

---

### Task 6: CLI `job list` (offline)

**Files:**
- Modify: `cli/Cargo.toml` (add the `klartext-sgbd` dependency if not already direct)
- Modify: `cli/src/main.rs` (the `Command` enum + a `JobAction` enum + a handler)

**Interfaces:**
- Consumes: `klartext_sgbd::Prg::open(path)?.job_names()` for listing (`Ecu` has no public
  `job_names`; it wraps a `Prg`). The global `--sgbd <PathBuf>` arg (`cli.sgbd`, `main.rs:88`).
- Produces: `klartext job list` subcommand printing the SGBD's job names; the `Command::Job` +
  `JobAction` scaffold Task 7 extends with `Run`.

- [ ] **Step 1: Add the dependency**

```bash
# klartext-sgbd may already be a direct dep; add it only if `cargo tree -p klartext-cli`
# does not list it as direct:
cargo add --package klartext-cli --path crates/sgbd
```
Verify the manifest line uses a path dep consistent with the workspace (no version invented).

- [ ] **Step 2: Write the failing test**

The CLI is integration-tested by invoking the binary. Follow the existing CLI test pattern
(grep `cli/tests/` or the in-file `#[cfg(test)]`). If the CLI has no test harness, add a thin
unit test on a pure `format_job_list(names: &[String]) -> String` helper:
```rust
#[test]
fn format_job_list_lists_names_sorted() {
    let out = format_job_list(&["STATUS_LESEN".into(), "CBS_RESET".into()]);
    assert!(out.contains("CBS_RESET"));
    assert!(out.contains("STATUS_LESEN"));
    // deterministic order
    assert!(out.find("CBS_RESET").unwrap() < out.find("STATUS_LESEN").unwrap());
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p klartext-cli format_job_list`
Expected: FAIL — helper/subcommand absent.

- [ ] **Step 4: Implement**

Add to `enum Command` (mirroring `Service`, `main.rs:165`):
```rust
/// Run or list EDIABAS jobs from an SGBD via the BEST/2 engine.
Job {
    #[command(subcommand)]
    action: JobAction,
},
```
```rust
#[derive(Subcommand)]
enum JobAction {
    /// List the jobs defined in the SGBD (`--sgbd <ecu>.prg`).
    List,
    /// Run a job against the live car and print its results.
    Run {
        /// The EDIABAS job name (e.g. `STATUS_LESEN`).
        job: String,
        /// Job arguments, joined with `;` into the EDIABAS arg buffer
        /// (e.g. `ARG ITOEL` → `"ARG;ITOEL"`).
        args: Vec<String>,
    },
}
```
Add match arms in `run()` (mirroring `main.rs:302`): `Command::Job { action } => match action {
JobAction::List => run_job_list(&cli)?, JobAction::Run { job, args } => run_job_run(&cli, job, args).await? }`.
`run_job_list` opens the `--sgbd` path via `Prg::open`, collects `job_names()`, prints via
`format_job_list`. Require `--sgbd` (bail with a clear message if `cli.sgbd` is `None`, mirroring
how `open_measurements` handles a missing SGBD).

- [ ] **Step 5: Run + gates + commit**

Run: `cargo test -p klartext-cli`
Expected: PASS.
```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-cli
git add cli/Cargo.toml Cargo.lock cli/src/main.rs
git commit -m "feat(cli): job list — enumerate an SGBD's jobs offline"
```

---

### Task 7: CLI `job run` (live) + `SessionBridge`

**Files:**
- Modify: `cli/Cargo.toml` (add `klartext-best` + `async-trait`)
- Modify: `cli/src/main.rs` (the `run_job_run` handler + a `SessionBridge` impl)

**Interfaces:**
- Consumes: `connect(&cli)` (`main.rs:491`) → `DiagnosticClient`; `cli.target` (`parse_hex_u8`);
  `cli.sgbd`; `klartext_best::{Ecu, GatedExchange, TelegramExchange, BareUdsTransport, ExchangeError, ResultData, ResultSet}` (Tasks 3-5);
  `DiagnosticClient::request` (Task 1); `ResultSet::iter_sets` (Task 5).
- Produces: `klartext job run <job> [args…]` executing a read job over the read-only gate.

- [ ] **Step 1: Add the dependencies**

```bash
cargo add --package klartext-cli --path crates/best
cargo add --package klartext-cli async-trait
```
Verify the `klartext-best` line is a workspace path dep; `async-trait` resolves the newest
compatible version via the CLI (do not hand-type it).

- [ ] **Step 2: Write the failing test**

A pure results-formatting helper is the testable unit (the live path needs a car). Add:
```rust
#[test]
fn format_result_sets_renders_named_values_per_set() {
    use klartext_best::{ResultData, ResultSet};
    let mut rs = ResultSet::new();
    rs.push_named("STAT_OEL_WERT", ResultData::Real(89.96));
    rs.push_named("STAT_OEL_EINH", ResultData::Text("degC".into()));
    let out = format_result_sets(&rs);
    assert!(out.contains("STAT_OEL_WERT"));
    assert!(out.contains("89.96"));
    assert!(out.contains("degC"));
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p klartext-cli format_result_sets`
Expected: FAIL — helper absent.

- [ ] **Step 4: Implement**

The `SessionBridge` (the CLI's ~8-line client coupling):
```rust
/// Bridges the BEST/2 engine's bare-UDS transport seam onto the live client.
struct SessionBridge<'a> {
    client: &'a klartext_client::DiagnosticClient,
}

#[async_trait::async_trait]
impl klartext_best::BareUdsTransport for SessionBridge<'_> {
    async fn call(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, klartext_best::ExchangeError> {
        self.client
            .request(target, uds)
            .await
            .map_err(|e| klartext_best::ExchangeError::Transport(format!("{e}")))
    }
}
```
`run_job_run(cli, job, args)`: open the SGBD (`Ecu::open(cli.sgbd…)`), `connect(cli)` for the
client, build the stack
`let gate = GatedExchange::read_only(TelegramExchange::new(SessionBridge { client: &client }));`,
join args with `;` into a `Vec<u8>` (`args.join(";").into_bytes()`), then
`let results = ecu.run_job(&job, cli.target, &arg_bytes, &gate).await?;` and print
`format_result_sets(&results)`. `format_result_sets` iterates `results.iter_sets()`, printing a
header per set when `sets_len() > 1`, and each `(name, value)` with a `ResultData` display helper
(mirror `round3`/`hex_bytes` for `Real`/`Binary`).

- [ ] **Step 5: Run + gates + commit**

Run: `cargo test -p klartext-cli`
Expected: PASS. (The live path is the manual on-car step — Task 10; the unit test covers formatting.)
```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-cli
git add cli/Cargo.toml Cargo.lock cli/src/main.rs
git commit -m "feat(cli): job run — execute a read job over the read-only gate"
```

---

### Task 8: MCP `run_job` tool (read-only)

**Files:**
- Modify: `mcp/Cargo.toml` (add `klartext-best` + `async-trait` via CLI)
- Modify: `mcp/src/dto.rs` (request/result DTOs)
- Modify: `mcp/src/server.rs` (the tool + `SessionBridge` + `advertised_tools` + module-doc invariant)

**Interfaces:**
- Consumes: the **`read_data` resolution pattern** (`server.rs:733-751`) — `ecu::resolve(ecu)`
  → `address`, then `resolve_variant(address, req.variant, catalog, vin)` → variant (NOT
  `resolve_list_variant`, which is the non-transmitting list path and yields no address);
  `sgbd_path` (`server.rs:123`); the session under lock (`state.lock().await`, pattern at
  `server.rs:776`); `DiagnosticClient::request` (Task 1); the best stack (Tasks 3-5);
  `MAX_LISTED_MEASUREMENTS`-style guard (`server.rs:52`). An explicit `variant` that cannot be
  loaded is a hard `no_sgbd` error (mirror `server.rs:749-751`); never degrade silently.
- Produces: a `run_job` MCP tool returning multi-set named results, read-only.

- [ ] **Step 1: Add deps**

```bash
cargo add --package klartext-mcp --path crates/best
cargo add --package klartext-mcp async-trait
```

- [ ] **Step 2: Write the failing test** (`mcp/tests/integration.rs`)

```rust
#[tokio::test]
async fn run_job_refuses_a_write_emitting_job_at_the_gate() {
    // Against the frame-recording mock transport the suite already uses, invoke
    // run_job with a job whose bytecode emits a 0x2E write; assert the tool errors
    // (gate Refused) and that NO write frame reached the recording transport.
    // Mirror the setup of clear_faults_with_confirm_…_only_standard_frames (server-side
    // mock at integration.rs:809).
}
```
If constructing a write-emitting job against the mock is heavy, split the read-only proof between
this behavioral test and Task 9's surface test; at minimum assert here that a `run_job` of a
read job (`STATUS_LESEN` on the DDE, data-gated skip-if-absent) returns named results.

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p klartext-mcp run_job`
Expected: FAIL — tool absent.

- [ ] **Step 4: Implement the DTOs** (`mcp/src/dto.rs`, mirroring `ReadDataRequest`/`ReadDataResult`)

```rust
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct RunJobRequest {
    /// The ECU to target (name or hex address); resolved to a transmit address
    /// and, via the M10 ladder, to a variant. Required — the job transmits to it.
    pub ecu: String,
    /// The SGBD variant (`.prg` stem, e.g. "d72n47a0") overriding ladder resolution.
    #[serde(default)]
    pub variant: Option<String>,
    /// The EDIABAS job name (e.g. "STATUS_LESEN").
    pub job: String,
    /// Job arguments, joined with `;` into the EDIABAS arg buffer (e.g. ["ARG","ITOEL"]).
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct RunJobResult {
    /// Result sets, each an ordered list of named values.
    pub sets: Vec<Vec<NamedValue>>,
    /// Total named values across all sets before truncation.
    pub total: usize,
    /// Set when results were truncated (never silent).
    pub note: Option<String>,
}

#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct NamedValue {
    pub name: String,
    /// The value rendered as a string (number, text, or hex for binary).
    pub value: String,
    /// EDIABAS result type tag (B/W/D/I/R/S/Y).
    pub kind: String,
}
```

- [ ] **Step 5: Implement the tool** (`mcp/src/server.rs`)

Add a `SessionBridge` newtype (same shape as Task 7 but over the locked session — the bridge
holds `&DiagnosticClient` obtained under the state lock for the run's duration). Add:
```rust
#[tool(description = "Run a read-only EDIABAS job (e.g. STATUS_LESEN) and return its \
    named result sets. Executes the ECU's own bytecode over a read-only gate that \
    refuses any write service at the transmit boundary; a write-emitting job is rejected.")]
pub async fn run_job(&self, Parameters(req): Parameters<RunJobRequest>)
    -> Result<Json<RunJobResult>, McpError>
```
Body (mirror `read_data`, `server.rs:733-751`): `req.ecu` is required by the DTO;
`address = ecu::resolve(&req.ecu, catalog).map_err(McpError::invalid_params)`;
`vin` from the locked state; `variant = self.resolve_variant(address, req.variant.as_deref(), catalog, vin)`
(an explicit-but-unloadable `variant` is a hard `no_sgbd` error, `server.rs:749-751`);
`path = self.sgbd_path(&variant).ok_or(no_sgbd)`; `ecu = Ecu::open(path)`; take the session under lock, build
`GatedExchange::read_only(TelegramExchange::new(SessionBridge{ client }))`, join args with `;`,
`ecu.run_job(&req.job, address, &arg_bytes, &gate).await` mapping `RunError` → `McpError::internal_error`
and a `Refused` inside it to `McpError::invalid_request` ("job emits a write; run it from the CLI").
Surface `results.iter_sets()` into `RunJobResult`, applying a `const MAX_RUN_JOB_RESULTS: usize = 200`
truncation with `total` + `note` exactly like `list_measurements` (never silent). Add `run_job` to
`advertised_tools()` (`server.rs:97`). Update the module-doc invariant (`server.rs:5-14`) to state
that `run_job` executes read jobs and the read-only gate refuses writes (do NOT rewrite CLAUDE.md —
that is P3).

- [ ] **Step 6: Run + gates + commit**

Run: `cargo test -p klartext-mcp` (some subtests data-gated)
Expected: PASS.
```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-mcp
git add mcp/Cargo.toml Cargo.lock mcp/src/dto.rs mcp/src/server.rs
git commit -m "feat(mcp): run_job read-only tool over the SID gate"
```

---

### Task 9: MCP surface-test alignment

**Files:**
- Modify: `mcp/tests/integration.rs` (the surface test `advertises_exactly_the_refined_tool_surface`, ~`:854-904`)

**Interfaces:**
- Consumes: the `run_job` tool (Task 8).
- Produces: a surface test that admits `run_job` as a gated read tool while still forbidding
  write/actuation verbs — the P2 form of the M9 invariant.

- [ ] **Step 1: Update the failing assertions**

The current test (`integration.rs:859-877`) asserts an exact 14-tool sorted list and
(`:881-899`) bans substrings incl. `"run"`. Update:
- Add `"run_job"` to the expected sorted list → **15 tools**.
- The forbidden-substring guard must still catch actuation/write verbs but not `run_job`. Change
  the `"run"` entry: replace the blanket `"run"` substring with the specific banned verbs it was
  proxying for (`"actuat"`, `"io_control"`, `"execut"`, `"routine"`, `"regen"`, `"calibrat"`,
  `"write"`, `"code"`, `"coding"`, `"reset"`, `"flash"` stay; drop `"run"`). Add an explicit
  assertion that the ONLY tool containing `"run"` is exactly `"run_job"` (so a future `run_actuator`
  would still trip a dedicated check):
```rust
let run_tools: Vec<&str> = tools.iter().filter(|t| t.contains("run")).copied().collect();
assert_eq!(run_tools, vec!["run_job"], "only run_job may contain 'run'");
```

- [ ] **Step 2: Run to verify it now reflects the new surface**

Run: `cargo test -p klartext-mcp advertises_exactly`
Expected: PASS with 15 tools incl. `run_job`; the write-verb bans still hold.

- [ ] **Step 3: Confirm the behavioral read-only seam**

Ensure Task 8's behavioral test (or add one here) proves — against the frame-recording mock —
that a `run_job` whose job would emit a write SID sends NO write frame to the transport (the gate
refuses first). This is the behavioral half of the invariant; the surface test is the structural half.

- [ ] **Step 4: Gates + commit**

```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test -p klartext-mcp
git add mcp/tests/integration.rs
git commit -m "test(mcp): admit run_job as a gated read tool; keep write verbs banned"
```

---

### Task 10: On-car protocol appendix + `_INFO` diagnosis + P2 close

**Files:**
- Modify: `docs/on-car-verification-protocol.md` (append a car-session-1 section)
- Modify: `crates/sgbd/src/prg.rs` OR `crates/best/src/exec.rs` (only if the `_INFO` diagnosis finds a real bug)
- Modify: `crates/best/tests/differential.rs` (only if the `_INFO` fix changes an asserted value)

**Interfaces:**
- Consumes: everything above.
- Produces: the written protocol the owner runs for car session 1; a resolved `_INFO` decode
  question; a P2 close note.

- [ ] **Step 1: Write the on-car appendix**

Append a `## Part 6 — BEST/2 live read path (car session 1)` section to
`docs/on-car-verification-protocol.md`. It must specify, in the doc's existing voice:
- **CLI capture:** `klartext --gateway-ip <ip> --sgbd "data/Testmodule(1)/Ecu/d72n47a0.prg" --target 12 job list` (expect the DDE's job names), then
  `… job run STATUS_LESEN ARG ITOEL` (expect `STAT_…OEL…_WERT`/`_EINH`/`_INFO` + `JOB_STATUS`),
  with `tcpdump` running per Part 0 step 6.
- **MCP capture:** the on-car Claude calls `run_job` with `{ecu:"DDE", job:"STATUS_LESEN", args:["ARG","ITOEL"]}`
  and a structured multi-result read on the DSC (`{ecu:"DSC", job:"STATUS_LESEN", args:["ARG","<a RES_ measurement>"]}`),
  pasting the JSON.
- **What it confirms:** the live telegram exchange path byte-for-byte (the frozen
  `83 12 F1 22 45 17` request shape and the `[0x80|len][0xF1][ecu]…` response), the additive
  checksum a real ECU accepts, `JOB_STATUS=OKAY` (not `ERROR_ECU_INCORRECT_LEN`) on a real
  response, and multi-value surfacing. Flag each as flipping a `[verify against capture]` marker.
- **Safety:** every step is a `0x22`/`0x2C`/`0x19` read; the gate refuses writes — but the
  human still runs it. No `clear`, no write.

- [ ] **Step 2: Diagnose the `_INFO` decode**

Decode a real `_INFO` value's raw bytes from the DDE `SG_FUNKTIONEN`/`RES_` `INFO` column and
compare CP-1252 (`crates/sgbd/src/cp1252.rs`) vs the per-byte Latin-1 in
`crates/best/src/exec.rs:1257-1261`. Determine where the mojibake the Task-10 report noted
originates: (a) if the VM emits `_INFO` from a table cell, sgbd already decoded it CP-1252 →
likely NO bug (the report saw its own raw print); (b) if the VM emits `_INFO` from a bytecode
string literal via exec's `read_string`, the `0x80..=0x9F` range diverges (Latin-1 vs CP-1252) →
a real, tiny fix (make exec's `read_string` use CP-1252 to match `EdiabasNet.cs`'s encoding, or
confirm EDIABAS uses Latin-1 there and the mojibake is elsewhere). **Fix only if the diagnosis
finds a real, reachable divergence with a byte-vector test; otherwise document the finding in the
report and the ledger and move on** (YAGNI — it is cosmetic, spec §10 defers it).

- [ ] **Step 3: If a fix was made, add its regression test + re-run**

If Step 2 produced a fix, add a byte-vector unit test pinning the corrected decode (a real
`0x80..=0x9F`-bearing string → expected glyphs) and re-run the owning crate's suite.

- [ ] **Step 4: P2 close**

Run the full workspace gates one final time:
```bash
cargo fmt && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace && cargo doc --workspace --no-deps
```
Each must exit 0. Then update the memory (`best2-vm-milestone.md`): P2 done, the live stack shape,
the car-session-1 handoff pending. Commit the doc + any fix:
```bash
git add docs/on-car-verification-protocol.md
git commit -m "docs: car session 1 protocol for the BEST/2 live read path; resolve _INFO decode"
```

---

## Self-review notes (run after all tasks)

- **Spec §9 P2 coverage:** `FnExchange`/live bridge = Tasks 2-3 (`TelegramExchange` + `SessionBridge`);
  `GatedExchange(ReadOnly)` = Task 4; `DiagnosticClient::session` accessor = Task 1 (as a `request`
  passthrough, more encapsulated than exposing `Session`); CLI `job run/list` = Tasks 6-7; MCP
  `run_job` read-only = Tasks 8-9; multi-value surfacing = Task 5 + Tasks 7-8; car session 1 = Task 10.
- **Spec §6 invariant:** the read-only gate (Task 4) + the MCP module-doc update + surface test
  (Tasks 8-9) implement the P2 slice; `ConfirmedWrite`, the CLAUDE.md tier-ladder rewrite, and the
  write ritual are explicitly P3 and NOT in this plan (Global Constraints).
- **No client↔best dependency:** honored — `klartext-best` gains no `klartext-client` dep; the two
  meet only in the binaries' `SessionBridge` (Tasks 7, 8), each impl `BareUdsTransport` (Task 3).
- **Checksum correctness:** the codec (Task 2) uses the additive `CalcChecksumBmwFast`, not the
  test's old XOR — the one correctness point a real ECU depends on.
- **Deferred (tracked, not built here):** `Policy::ConfirmedWrite`, the flow runner, oil-level flow,
  LERNWERTE write, CLAUDE.md rewrite — all P3. The `_INFO` decode is resolved-or-documented in Task 10.
