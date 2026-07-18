# P3 Write-Tier Foundation Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add the confirmed-write gate policy and UDS `0x11` ECUReset, then make a confirmed clear reset the ECUs afterwards — closing the observed gap where ISTA's clear-all reboots the instrument cluster and klartext's does not.

**Architecture:** `klartext-best`'s existing transmit-seam gate gains a second `Policy` variant so writes can be admitted deliberately; `klartext-uds` gains the ECUReset service; `klartext-client` gains a reset call and a clear-all sequence that clears every ECU first and only then resets them, excluding the gateway (resetting `0x10` would kill our own session). MCP and the CLI expose it behind the existing confirmation.

**Tech Stack:** Rust edition 2024, tokio, async-trait, thiserror (libraries) / anyhow (binaries), rmcp (MCP), clap (CLI).

## Global Constraints

- Latest stable Rust, edition 2024. Errors: `thiserror` in libraries, `anyhow` at binary boundaries.
- `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` must be clean before a task is done. **Run `cargo fmt` via Bash, not the editor hook** (the hook uses an older rustfmt and produces a different result).
- Conventional commits.
- **Never claim a hardware round-trip works.** Unit tests prove framing/logic only; on-car verification is a manual owner step.
- `Policy::ReadOnly` remains the default for every read path. `run_job` is NOT the write path and must keep its existing guarantee — its tests must pass unmodified.
- Flashing (`0x34`–`0x37`) is refused under **every** policy, forever.
- BYO-data: never commit ISTA data, SGBDs, captures, or VINs.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/best/src/gate.rs` | Transmit-seam policy + classification | Add `Policy::ConfirmedWrite`, `GatedExchange::confirmed_write`, match arms |
| `crates/uds/src/lib.rs` | UDS service IDs + sub-function modules | Add `sid::ECU_RESET`, `reset_subfn` module, re-export `ecu_reset` |
| `crates/uds/src/service.rs` | UDS request builders | Add `ecu_reset()` builder |
| `crates/client/src/client.rs` | Typed per-ECU services | Add `ecu_reset()` |
| `crates/client/src/scan.rs` | Whole-car sequences | Add `reset_targets()`, `clear_faults_all_with_reset()`, extend `ClearReport` |
| `mcp/src/dto.rs` | MCP request/response types | Add `reset` to both clear requests, `reset_performed` to results |
| `mcp/src/server.rs` | MCP handlers | Pass `reset` through |
| `cli/src/main.rs` | CLI | Add `--no-reset` to `clear-faults` |

---

### Task 1: `Policy::ConfirmedWrite` at the gate

**Files:**
- Modify: `crates/best/src/gate.rs`

**Interfaces:**
- Consumes: existing `classify(sid) -> SidClass`, `GatedExchange<E>`, `ExchangeError::Refused { sid, frame }`
- Produces: `Policy::ConfirmedWrite`, `GatedExchange::confirmed_write(inner) -> Self`

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/best/src/gate.rs`:

```rust
    #[tokio::test]
    async fn confirmed_write_passes_a_gated_write_to_the_inner() {
        // The whole point of the new policy: a 0x2E write that ReadOnly refuses
        // must reach the inner transport under ConfirmedWrite.
        let gate = GatedExchange::confirmed_write(RecordingExchange::default());
        let frame = crate::encode(0x12, 0xF1, &[0x2E, 0x10, 0x01, 0xFF]);
        gate.request(0x12, &frame).await.unwrap();
        assert_eq!(gate.inner_last(), Some(frame));
    }

    #[tokio::test]
    async fn confirmed_write_still_passes_reads() {
        let gate = GatedExchange::confirmed_write(RecordingExchange::default());
        let frame = crate::encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
        gate.request(0x12, &frame).await.unwrap();
        assert_eq!(gate.inner_last(), Some(frame));
    }

    #[tokio::test]
    async fn confirmed_write_never_passes_flashing() {
        // RefuseAlways means ALWAYS — the write policy must not open flashing.
        let gate = GatedExchange::confirmed_write(RecordingExchange::default());
        for sid in [0x34u8, 0x35, 0x36, 0x37] {
            let frame = crate::encode(0x12, 0xF1, &[sid, 0x00]);
            match gate.request(0x12, &frame).await {
                Err(ExchangeError::Refused { sid: got, .. }) => assert_eq!(got, sid),
                other => panic!("expected Refused for 0x{sid:02X}, got {other:?}"),
            }
        }
        assert_eq!(gate.inner_last(), None);
    }

    #[tokio::test]
    async fn confirmed_write_rejects_an_unparseable_frame() {
        // No-degrade applies under every policy.
        let gate = GatedExchange::confirmed_write(RecordingExchange::default());
        let frame = vec![0x80, 0x12];
        assert!(matches!(
            gate.request(0x12, &frame).await,
            Err(ExchangeError::Unexpected(_))
        ));
        assert_eq!(gate.inner_last(), None);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p klartext-best gate:: 2>&1 | tail -20`
Expected: FAIL — `no function or associated item named 'confirmed_write' found`

- [ ] **Step 3: Add the policy variant and constructor**

In `crates/best/src/gate.rs`, replace the `Policy` enum:

```rust
/// The gate's transmit policy — the safety posture applied to each frame.
///
/// [`Policy::ReadOnly`] is the default for every read path (`run_job`, measurement
/// reads); [`Policy::ConfirmedWrite`] is selected explicitly by a caller that has a
/// human's `confirm` in hand. Neither policy can open flashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Refuse every [`SidClass::Gated`] and [`SidClass::RefuseAlways`] service;
    /// pass only reads and session plumbing.
    ReadOnly,
    /// Pass reads AND [`SidClass::Gated`] writes/actuation; still refuse
    /// [`SidClass::RefuseAlways`] flashing, forever.
    ConfirmedWrite,
}
```

Add the constructor beside `read_only`:

```rust
    /// Wraps `inner` with the confirmed-write policy: reads and writes pass,
    /// flashing is still refused.
    ///
    /// Select this ONLY on a call carrying the human's `confirm` (spec §2 D3).
    pub fn confirmed_write(inner: E) -> Self {
        Self {
            inner,
            policy: Policy::ConfirmedWrite,
        }
    }
```

- [ ] **Step 4: Handle the new policy at the seam**

Replace the `match` in the `UdsExchange` impl:

```rust
        match (self.policy, classify(sid)) {
            // A read or session-plumbing service passes under either policy.
            (Policy::ReadOnly | Policy::ConfirmedWrite, SidClass::Pass) => {
                self.inner.request(target, frame).await
            }
            // ConfirmedWrite admits the write/actuation services ReadOnly refuses.
            (Policy::ConfirmedWrite, SidClass::Gated) => self.inner.request(target, frame).await,
            // Refused: every write under ReadOnly, and flashing under BOTH policies —
            // before the inner exchange (and thus the car) is ever touched.
            (Policy::ReadOnly, SidClass::Gated)
            | (Policy::ReadOnly | Policy::ConfirmedWrite, SidClass::RefuseAlways) => {
                Err(ExchangeError::Refused {
                    sid,
                    frame: frame.to_vec(),
                })
            }
        }
```

Also update the module doc-comment at the top of `gate.rs`: it currently says the confirmed-write policy "is deliberately absent" and that adding it will force the write path to be handled. Replace that paragraph with:

```rust
//! Both policies ship as of P3: [`Policy::ReadOnly`] (the default for every read
//! path) and [`Policy::ConfirmedWrite`], selected only by a caller holding the
//! human's `confirm`. Flashing is [`SidClass::RefuseAlways`] under both.
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p klartext-best gate:: 2>&1 | tail -20`
Expected: PASS — all gate tests, including the pre-existing `read_only_*` ones **unmodified**.

- [ ] **Step 6: Verify the read guarantee did not regress**

Run: `cargo test -p klartext-best 2>&1 | grep "test result"`
Expected: `ok`, 0 failed.

- [ ] **Step 7: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/best/src/gate.rs
git commit -m "feat(best): add Policy::ConfirmedWrite to the transmit-seam gate"
```

---

### Task 2: UDS `0x11` ECUReset

**Files:**
- Modify: `crates/uds/src/lib.rs` (sid constant, `reset_subfn` module, re-export)
- Modify: `crates/uds/src/service.rs` (builder + tests)

**Interfaces:**
- Consumes: existing `sid` module conventions
- Produces: `sid::ECU_RESET: u8`, `reset_subfn::{HARD, KEY_OFF_ON, SOFT}`, `ecu_reset(subfunction: u8) -> [u8; 2]`

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/uds/src/service.rs`:

```rust
    #[test]
    fn ecu_reset_builds_the_two_byte_request() {
        // 0x11 ECUReset: [SID, sub-function]. Hard reset is the default klartext
        // sends after a clear — the one consistent with the cluster reboot
        // observed on the car. [verify against capture]
        assert_eq!(ecu_reset(reset_subfn::HARD), [0x11, 0x01]);
        assert_eq!(ecu_reset(reset_subfn::KEY_OFF_ON), [0x11, 0x02]);
        assert_eq!(ecu_reset(reset_subfn::SOFT), [0x11, 0x03]);
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p klartext-uds ecu_reset 2>&1 | tail -15`
Expected: FAIL — `cannot find function 'ecu_reset' in this scope`

- [ ] **Step 3: Add the service ID and sub-functions**

In `crates/uds/src/lib.rs`, inside `pub mod sid`, after `CLEAR_DIAGNOSTIC_INFORMATION`:

```rust
    /// ECUReset (0x11) — reboots the ECU; a state change, gate behind confirmation.
    pub const ECU_RESET: u8 = 0x11;
```

Add a new module beside `session`:

```rust
/// ECUReset (0x11) sub-functions (ISO 14229-1).
///
/// klartext sends [`reset_subfn::HARD`] after a confirmed clear: it is the reset
/// consistent with the instrument-cluster reboot ISTA produces. Which sub-function
/// ISTA actually sends is `[verify against capture]`.
pub mod reset_subfn {
    /// 0x01 — hardReset: a full power-on-equivalent restart.
    pub const HARD: u8 = 0x01;
    /// 0x02 — keyOffOnReset: behaves as if the ignition were cycled.
    pub const KEY_OFF_ON: u8 = 0x02;
    /// 0x03 — softReset: restarts the application without a full reboot.
    pub const SOFT: u8 = 0x03;
}
```

- [ ] **Step 4: Add the builder**

In `crates/uds/src/service.rs`, after `clear_all_dtcs`:

```rust
/// Build an ECUReset request (`11 <sub-function>`).
///
/// A STATE CHANGE: the ECU reboots and briefly stops answering. Callers must hold
/// the human's confirmation, and must never reset the gateway on the connection
/// they are using — that drops the session (see `klartext_client::reset_targets`).
///
/// Use [`reset_subfn::HARD`] unless you have a reason not to.
pub fn ecu_reset(subfunction: u8) -> [u8; 2] {
    [sid::ECU_RESET, subfunction]
}
```

Add to the `pub use service::{…}` list in `crates/uds/src/lib.rs` (keep alphabetical): `ecu_reset,`

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p klartext-uds ecu_reset 2>&1 | tail -15`
Expected: PASS

- [ ] **Step 6: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/uds/src/lib.rs crates/uds/src/service.rs
git commit -m "feat(uds): add ECUReset (0x11) service and sub-functions"
```

---

### Task 3: `DiagnosticClient::ecu_reset`

**Files:**
- Modify: `crates/client/src/client.rs`

**Interfaces:**
- Consumes: `klartext_uds::{ecu_reset, reset_subfn, sid}`, existing `self.session.request`
- Produces: `DiagnosticClient::ecu_reset(&self, target: u8, subfunction: u8) -> Result<(), ClientError>`

- [ ] **Step 1: Write the failing test**

Add to the `mod tests` block in `crates/client/src/client.rs`:

```rust
    #[tokio::test]
    async fn ecu_reset_sends_11_01_and_accepts_the_positive_response() {
        // The DDE answers 51 01 (positive ECUReset). The client must send exactly
        // 11 01 and treat 51 01 as success.
        let addr = spawn_gateway_multi(&[(DDE, vec![0x11, 0x01], vec![0x51, 0x01])]).await;
        let c = client(addr).await;
        c.ecu_reset(DDE, klartext_uds::reset_subfn::HARD)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn ecu_reset_surfaces_a_negative_response() {
        // 7F 11 22 (conditionsNotCorrect) must be an error, not a silent success.
        let addr = spawn_gateway_multi(&[(DDE, vec![0x11, 0x01], vec![0x7F, 0x11, 0x22])]).await;
        let c = client(addr).await;
        assert!(
            c.ecu_reset(DDE, klartext_uds::reset_subfn::HARD)
                .await
                .is_err()
        );
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p klartext-client ecu_reset 2>&1 | tail -15`
Expected: FAIL — `no method named 'ecu_reset' found`

- [ ] **Step 3: Implement the method**

In `crates/client/src/client.rs`, add to the `impl DiagnosticClient` block beside `clear_dtcs`:

```rust
    /// Reset one ECU (`11 <sub-function>`) — a state change; gate behind confirmation.
    ///
    /// The ECU reboots and stops answering briefly. NEVER call this on the gateway
    /// address serving the current connection: the session dies with it (see
    /// [`crate::reset_targets`]).
    ///
    /// # Errors
    /// As [`crate::Session::request`] on a transport error, and [`ClientError::Uds`]
    /// if the ECU answers negatively.
    pub async fn ecu_reset(&self, target: u8, subfunction: u8) -> Result<(), ClientError> {
        self.session
            .enter_session(target, session::EXTENDED)
            .await?;
        self.session
            .request(target, &klartext_uds::ecu_reset(subfunction))
            .await?;
        Ok(())
    }
```

**Call the builder fully qualified as `klartext_uds::ecu_reset(...)`, as written above.** Do NOT add `ecu_reset` to the file's `use klartext_uds::{…}` list: the client method is also named `ecu_reset`, and importing the free function alongside it makes the call site ambiguous to read (and shadow-prone if the method later takes a default sub-function).

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p klartext-client ecu_reset 2>&1 | tail -15`
Expected: PASS (both)

- [ ] **Step 5: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/client/src/client.rs
git commit -m "feat(client): add ecu_reset over UDS 0x11"
```

---

### Task 4: Clear-then-reset sequencing, gateway excluded

**Files:**
- Modify: `crates/client/src/scan.rs`
- Modify: `crates/client/src/lib.rs` (export `reset_targets`)

**Interfaces:**
- Consumes: `DiagnosticClient::ecu_reset` (Task 3), existing `clear_faults_verified`, `ClearReport`
- Produces: `reset_targets(addrs: &[u8]) -> Vec<u8>`; `ClearReport.reset_performed: Option<bool>`; `DiagnosticClient::clear_faults_all_with_reset(&self, addrs: &[u8], reset: bool) -> Vec<ClearReport>`

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/client/src/scan.rs`:

```rust
    #[test]
    fn reset_targets_excludes_the_gateway() {
        // Resetting 0x10 would kill the connection we are issuing resets over, so
        // it is never a reset target — every other address is, order preserved.
        let addrs = [0x10u8, 0x12, 0x40, 0x60];
        assert_eq!(reset_targets(&addrs), vec![0x12, 0x40, 0x60]);
    }

    #[test]
    fn reset_targets_handles_a_gateway_only_and_empty_list() {
        assert!(reset_targets(&[0x10]).is_empty());
        assert!(reset_targets(&[]).is_empty());
    }

    #[test]
    fn reset_targets_keeps_duplicates_out() {
        // The SVT can list an address twice; resetting it twice is pointless churn.
        assert_eq!(reset_targets(&[0x12, 0x12, 0x40]), vec![0x12, 0x40]);
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p klartext-client reset_targets 2>&1 | tail -15`
Expected: FAIL — `cannot find function 'reset_targets' in this scope`

- [ ] **Step 3: Implement `reset_targets`**

At the top level of `crates/client/src/scan.rs` (outside the `impl`), add:

```rust
/// The addresses a whole-car clear may reset, in order, de-duplicated.
///
/// Excludes the gateway ([`ZGW_ADDRESS`]): the reset would tear down the very
/// connection the resets are being issued over. Duplicates are dropped — the SVT
/// can list an address more than once and resetting it twice is pointless churn.
pub fn reset_targets(addrs: &[u8]) -> Vec<u8> {
    let mut out: Vec<u8> = Vec::with_capacity(addrs.len());
    for &address in addrs {
        if address != ZGW_ADDRESS && !out.contains(&address) {
            out.push(address);
        }
    }
    out
}
```

Ensure `ZGW_ADDRESS` is imported in `scan.rs` (`use klartext_hsfz::ZGW_ADDRESS;` — check the existing imports first and add only if absent).

Export it from `crates/client/src/lib.rs` by extending the scan re-export line to:

```rust
pub use scan::{ClearReport, EcuFaults, reset_targets};
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p klartext-client reset_targets 2>&1 | tail -15`
Expected: PASS (all three)

- [ ] **Step 5: Add the reset outcome to `ClearReport`**

In `crates/client/src/scan.rs`, add a field to `ClearReport` (after `verified_clean`):

```rust
    /// Whether this ECU was reset after the clear: `Some(true)` reset OK,
    /// `Some(false)` the reset was attempted and failed, `None` not attempted
    /// (reset disabled, or the address is the excluded gateway).
    pub reset_performed: Option<bool>,
```

Initialise it as `reset_performed: None` in the `ClearReport { … }` literal inside `clear_faults_verified`. Fix any other construction sites the compiler points at:

Run: `cargo build -p klartext-client 2>&1 | grep -E "^error" | head`

- [ ] **Step 6: Write the failing opt-out test**

This test pins real behaviour: with `reset = false`, no ECU is reset and every report
says so — a caller can never mistake "not attempted" for "succeeded". Add to the same
`mod tests` block (it needs the client-crate loopback helpers; if `spawn_gateway_multi`
and `client` are private to `client.rs`'s test module, make that module
`pub(crate) mod tests` or move the two helpers into a shared `#[cfg(test)]`
`crate::testutil` module and import them here — do NOT duplicate them):

```rust
    #[tokio::test]
    async fn clear_all_with_reset_disabled_resets_nothing() {
        // Opt-out path. The mock answers the pre-read (19 02 FF), the extended
        // session entry the clear performs (10 03), the clear itself, and the
        // post-read verify — but NO 0x11 entry exists, so an attempted reset
        // would time out and show up as an error on the report.
        let addr = spawn_gateway_multi(&[
            (0x12, vec![0x19, 0x02, 0xFF], vec![0x59, 0x02, 0xFF]),
            (
                0x12,
                vec![0x10, 0x03],
                vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88],
            ),
            (0x12, vec![0x14, 0xFF, 0xFF, 0xFF], vec![0x54]),
        ])
        .await;
        let c = client(addr).await;
        let reports = c.clear_faults_all_with_reset(&[0x12], false).await;
        assert_eq!(reports.len(), 1);
        // The clear must actually have SUCCEEDED. Without this, the
        // `reset_performed: None` assertion below would be vacuous — a clear that
        // timed out also resets nothing, so the test would pass while proving
        // nothing about the opt-out.
        assert_eq!(reports[0].error, None, "the clear itself must succeed");
        assert!(reports[0].verified_clean, "post-read verify must have run");
        assert_eq!(
            reports[0].reset_performed, None,
            "reset must not be attempted when disabled"
        );
    }
```

**Note on the mock table:** `clear_faults_verified` performs FOUR exchanges — pre-read `19 02 FF`, the extended-session entry `10 03` (inside `clear_dtcs`), the clear `14 FF FF FF`, and the post-read `19 02 FF` again. Every one needs an entry or the call stalls on a read timeout. If you add a reset-enabled test, remember it needs a `(addr, vec![0x11, 0x01], vec![0x51, 0x01])` entry too.

- [ ] **Step 7: Implement `clear_faults_all_with_reset`**

In `crates/client/src/scan.rs`, replace the existing `clear_faults_all` with the pair below (keeping `clear_faults_all` as the no-reset shorthand so existing callers still compile):

```rust
    /// Clear every ECU in `addrs`, sequentially, returning a per-ECU report.
    ///
    /// Sequential by design — writes stay lockstep even though reads fan out.
    /// Equivalent to [`clear_faults_all_with_reset`](Self::clear_faults_all_with_reset)
    /// with `reset = false`.
    pub async fn clear_faults_all(&self, addrs: &[u8]) -> Vec<ClearReport> {
        self.clear_faults_all_with_reset(addrs, false).await
    }

    /// Clear every ECU in `addrs`, then optionally reset them (ISTA parity).
    ///
    /// ORDER MATTERS: every ECU is cleared FIRST, and only then are the resets
    /// issued. Resetting mid-sweep would drop ECUs that still have to be cleared.
    /// The gateway is never reset (see [`reset_targets`]) — that would tear down
    /// this connection. A failed reset is recorded on the ECU's report and does not
    /// abort the remaining resets.
    pub async fn clear_faults_all_with_reset(
        &self,
        addrs: &[u8],
        reset: bool,
    ) -> Vec<ClearReport> {
        let mut reports = Vec::with_capacity(addrs.len());
        for &address in addrs {
            reports.push(self.clear_faults_verified(address).await);
        }
        if !reset {
            return reports;
        }
        for address in reset_targets(addrs) {
            let outcome = self.ecu_reset(address, klartext_uds::reset_subfn::HARD).await;
            if let Some(report) = reports.iter_mut().find(|r| r.address == address) {
                report.reset_performed = Some(outcome.is_ok());
                if let Err(error) = outcome {
                    // The clear itself succeeded; record the reset failure without
                    // overwriting an earlier, more important error.
                    if report.error.is_none() {
                        report.error = Some(format!("reset failed: {error}"));
                    }
                }
            }
        }
        reports
    }
```

- [ ] **Step 8: Run the full client suite**

Run: `cargo test -p klartext-client 2>&1 | grep "test result"`
Expected: `ok`, 0 failed.

- [ ] **Step 9: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add crates/client/src/scan.rs crates/client/src/lib.rs
git commit -m "feat(client): reset ECUs after a whole-car clear, gateway excluded"
```

---

### Task 5: Expose `reset` on MCP and the CLI

**Files:**
- Modify: `mcp/src/dto.rs`
- Modify: `mcp/src/server.rs`
- Modify: `cli/src/main.rs`

**Interfaces:**
- Consumes: `clear_faults_all_with_reset` (Task 4), `DiagnosticClient::ecu_reset` (Task 3)
- Produces: `ClearAllFaultsRequest.reset: bool` (default `true`), `ClearFaultsRequest.reset: bool` (default `true`), `--no-reset` CLI flag

- [ ] **Step 1: Write the failing DTO default test**

Add to `mcp/src/dto.rs` (create a `#[cfg(test)] mod tests` block at the end of the file if none exists):

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clear_requests_default_to_resetting() {
        // ISTA parity: a confirmed clear resets afterwards unless the caller opts
        // out, so an omitted `reset` must deserialize as true.
        let one: ClearFaultsRequest = serde_json::from_str(r#"{"ecu":"0x12","confirm":true}"#).unwrap();
        assert!(one.reset);
        let all: ClearAllFaultsRequest = serde_json::from_str(r#"{"confirm":true}"#).unwrap();
        assert!(all.reset);

        // ...and an explicit false is honoured.
        let off: ClearAllFaultsRequest =
            serde_json::from_str(r#"{"confirm":true,"reset":false}"#).unwrap();
        assert!(!off.reset);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p klartext-mcp clear_requests_default 2>&1 | tail -15`
Expected: FAIL — `no field 'reset' on type 'ClearFaultsRequest'`

- [ ] **Step 3: Add the field and its default**

In `mcp/src/dto.rs`, add this free function near the top of the file:

```rust
/// serde default for the clear tools' `reset` flag: ISTA parity is reset-on.
fn default_true() -> bool {
    true
}
```

Add to **both** `ClearFaultsRequest` and `ClearAllFaultsRequest`:

```rust
    /// Reset the ECU(s) after clearing, as ISTA does — this is what reboots the
    /// instrument cluster. Defaults to true; set false to clear without resetting.
    /// The gateway is never reset (it would drop the connection).
    #[serde(default = "default_true")]
    pub reset: bool,
```

Add to `ClearFaultsResult` and to `EcuClearInfo`:

```rust
    /// Whether the ECU was reset after the clear: `Some(true)` reset OK,
    /// `Some(false)` attempted and failed, `None` not attempted.
    pub reset_performed: Option<bool>,
```

**No dependency change is needed:** `serde_json` is already a regular dependency of
`klartext-mcp` (`mcp/Cargo.toml:25`, `serde_json = "1.0.150"`), so it is available to the
test module as-is. Do NOT run `cargo add` — adding it again as a dev-dependency would
create a redundant duplicate entry.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p klartext-mcp clear_requests_default 2>&1 | tail -15`
Expected: PASS

- [ ] **Step 5: Wire the flag through the handlers**

In `mcp/src/server.rs`:

- In `clear_all_faults`, replace the `clear_faults_all(&addrs)` call with
  `clear_faults_all_with_reset(&addrs, req.reset)`.
- In `clear_faults` (single ECU), after the existing clear+verify succeeds, add the reset:

```rust
        // ISTA parity: reset the ECU after clearing so it reinitialises, unless the
        // caller opted out. The gateway is never reset — it would drop this session.
        let reset_performed = if req.reset && address != klartext_hsfz::ZGW_ADDRESS {
            Some(
                conn.client
                    .ecu_reset(address, klartext_uds::reset_subfn::HARD)
                    .await
                    .is_ok(),
            )
        } else {
            None
        };
```

Populate `reset_performed` in the returned `ClearFaultsResult`, and populate each `EcuClearInfo.reset_performed` from its `ClearReport.reset_performed` in the whole-car path. Extend the human `note` on both tools to state whether a reset was performed.

Add any missing imports (`klartext_hsfz` / `klartext_uds`) that the compiler reports.

- [ ] **Step 6: Add the CLI flag**

In `cli/src/main.rs`, add to the `ClearFaults` variant:

```rust
        /// Skip the post-clear ECU reset (klartext resets by default, as ISTA does).
        #[arg(long)]
        no_reset: bool,
```

Update the `Command::ClearFaults` match arm to pass it through:

```rust
        Command::ClearFaults {
            confirm,
            all_ecus,
            no_reset,
        } => {
            run_clear_faults(&cli, *confirm, *all_ecus, !*no_reset).await?;
        }
```

Change `run_clear_faults`'s signature to `async fn run_clear_faults(cli: &Cli, confirm: bool, all_ecus: bool, reset: bool) -> Result<()>`. In its `--all-ecus` branch, replace the `clear_faults_all(&addrs)` call with:

```rust
    let reports = client.clear_faults_all_with_reset(&addrs, reset).await;
```

In its single-ECU branch, after the existing successful clear and verify, add:

```rust
    // ISTA parity: reset the ECU so it reinitialises. Never the gateway — that
    // would drop this connection.
    if reset && cli.target != ZGW_ADDRESS {
        match client
            .ecu_reset(cli.target, klartext_uds::reset_subfn::HARD)
            .await
        {
            Ok(()) => println!("✔ ECU 0x{:02X} reset after clear.", cli.target),
            Err(e) => eprintln!("warning: clear succeeded but reset failed: {e}"),
        }
    } else if !reset {
        println!("(post-clear reset skipped: --no-reset)");
    }
```

`ZGW_ADDRESS` is already imported in `cli/src/main.rs`; add `klartext_uds::reset_subfn` usage as shown (fully qualified, no new import).

- [ ] **Step 7: Run the full workspace suite**

Run: `cargo test --workspace 2>&1 | grep "test result:" | grep -v "ok\."`
Expected: no output (every suite reports ok).

- [ ] **Step 8: Format, lint, commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
git add mcp/src/dto.rs mcp/src/server.rs cli/src/main.rs
git commit -m "feat: reset ECUs after a confirmed clear on MCP and CLI"
```

---

## Manual on-car verification (owner step — NOT claimed by tests)

After the plan is implemented, the owner runs, with a pcap capturing:

1. `klartext --target 12 read-faults` — note the stored faults.
2. `klartext --target 12 clear-faults --confirm` — expect the clear, then `11 01`, then a positive `51 01`.
3. Confirm the ECU reinitialises; on a whole-car clear (`--all-ecus --confirm`) confirm the **instrument cluster reboots**, matching ISTA's behaviour.
4. Confirm the gateway was never sent `11` and the session survived.
5. Record the result in `docs/car-session-2-results.md`, including which reset sub-function ISTA uses if a comparison capture is available — that resolves the `[verify against capture]` marker on `reset_subfn::HARD`.
