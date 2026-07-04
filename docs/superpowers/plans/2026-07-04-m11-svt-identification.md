# M11 Item 2 — SVT + Vehicle-Identification Dump Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** On connect, produce an ISTA-style vehicle identity — VIN, model/build (FA), integration level, the authoritative fitted-ECU list by name, and each ECU's identification block — all from autonomous-safe UDS `0x22` reads, and make the gateway SVT the discovery source in place of the M10 probe-scan.

**Architecture:** All reads reuse the existing `read_data_by_identifier` builder + `decode_read_data_by_identifier` + `client.read_did` (DID-echo-guarded). One new pure structural decoder (`decode_ecu_list`) lives in `crates/uds`; FA field decode and the ECU-name overlay live in `crates/semantic`; the reads + orchestration live in `crates/client`; MCP/CLI expose them. The M10 probe-scan subsystem is deleted, not retained.

**Tech Stack:** Rust edition 2024, tokio, thiserror (libs) / anyhow (binaries), rusqlite (semantic DB, read-only), rmcp (MCP), clap (CLI). Spec: `docs/superpowers/specs/2026-07-04-m11-svt-identification-design.md`.

## Global Constraints

- **Every operation is a UDS `0x22` read** — autonomous-safe, no confirmation gate, no backup-before-write. The `0x31` `STEUERN_VCM_GENERATE_SVT_*` routines are NOT implemented on any surface.
- **Stay BMW-generic, never car-specific.** No hardcoded addresses or ECU-name aliases; names come only from the semantic DB (`Catalog::ecus()`). An address the DB doesn't know keeps a raw-hex name, never dropped or guessed. The FA decoder is version-branched, not tied to one build.
- **Never assume — mark derived framing.** Any response byte offset/branch not provable from the disassembly carries a `[verify against capture]` doc comment. Decoders degrade to raw rather than guess. No hardware round-trip is ever claimed as working.
- **No legacy code left behind.** The M10 probe-scan machinery is deleted (Task 4), not kept as unused primitives.
- **No BMW data committed.** `.prg`/`.grp`/DLL/decompiles/captures stay out of git (already gitignored). Only facts/protocol appear in code and docs.
- **Green before done:** `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` clean; conventional commits per task.

---

## Task 1: uds — VCM DID constants + `decode_ecu_list`

**Files:**
- Modify: `crates/uds/src/service.rs` (add DID constants to the `did` module)
- Create: `crates/uds/src/identity.rs`
- Modify: `crates/uds/src/lib.rs` (declare `mod identity`; re-export)

**Interfaces:**
- Consumes: `UdsError` (`crates/uds/src/lib.rs`), `sid::READ_DATA_BY_IDENTIFIER`, `positive_response_sid`.
- Produces: `did::{ECU_LIST_ALL, VEHICLE_ORDER, I_STUFE}: u16`; `EcuList { count: u16, addresses: Vec<u8> }`; `decode_ecu_list(data: &[u8]) -> Result<EcuList, UdsError>` (operates on the **data region** after `decode_read_data_by_identifier` has stripped the `62 3F 07` echo).

- [ ] **Step 1: Add the VCM DID constants**

In `crates/uds/src/service.rs`, extend the `did` module (currently holds `VIN` and `IP_CONFIG`):

```rust
    /// 0x3F07 — BMW gateway VCM installed-ECU list (the SVT fitted list). The job
    /// `STATUS_VCM_GET_ECU_LIST_ALL` reads this; the response is decoded by
    /// [`crate::decode_ecu_list`]. [verify against capture]
    pub const ECU_LIST_ALL: u16 = 0x3F07;
    /// 0x3F06 — BMW gateway VCM vehicle order (Fahrzeugauftrag / FA). Read job
    /// `STATUS_VCM_GET_FA`. Decoded by `klartext_semantic::decode_vehicle_order`.
    pub const VEHICLE_ORDER: u16 = 0x3F06;
    /// 0x100B — BMW gateway VCM integration level (I-Stufe). Read job
    /// `STATUS_VCM_I_STUFE_LESEN`. Value is ASCII. [verify against capture]
    pub const I_STUFE: u16 = 0x100B;
```

- [ ] **Step 2: Write the failing test for `decode_ecu_list`**

Create `crates/uds/src/identity.rs`:

```rust
//! Structural decode of the BMW gateway VCM installed-ECU list (DID 0x3F07).
//!
//! The response data region (after the `62 3F 07` echo is stripped by
//! [`crate::decode_read_data_by_identifier`]) is a 2-byte big-endian ECU count
//! followed by one diagnostic-address byte per ECU. Names are NOT on the wire —
//! the caller resolves them from the semantic DB. This layout is DERIVED from the
//! `STATUS_VCM_GET_ECU_LIST_ALL` disassembly; no on-car capture exists yet, so it
//! is **[verify against capture]** and the decode is lenient (it returns the
//! address bytes actually present rather than trusting the declared count).

use crate::UdsError;

/// The BMW gateway's installed-ECU list: a declared count and the address bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcuList {
    /// The ECU count the gateway declares (u16 big-endian at data offset 0..2).
    pub count: u16,
    /// One diagnostic address per installed ECU (the bytes after the count).
    pub addresses: Vec<u8>,
}

/// Decode the data region of a `62 3F 07` response into an [`EcuList`].
///
/// `data` is the region after the `62 3F 07` echo (as returned by
/// [`crate::decode_read_data_by_identifier`]). Reads the declared count, then
/// takes every remaining byte as one ECU address. [verify against capture]
///
/// # Errors
/// [`UdsError::ShortResponse`] if `data` is fewer than the 2 count bytes.
pub fn decode_ecu_list(data: &[u8]) -> Result<EcuList, UdsError> {
    let count_bytes = data.get(..2).ok_or(UdsError::ShortResponse {
        sid: 0x62,
        need: 2,
        got: data.len(),
    })?;
    let count = u16::from_be_bytes([count_bytes[0], count_bytes[1]]);
    Ok(EcuList {
        count,
        addresses: data[2..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // DERIVED from the STATUS_VCM_GET_ECU_LIST_ALL disassembly (design §3.2): the
    // data region is a u16 BE count then one address byte per ECU. Synthetic bytes,
    // following the documented framing — no capture exists yet. [verify against capture]
    #[test]
    fn decodes_count_and_addresses() {
        // count = 0x0003, addresses 0x10 0x12 0x40
        let list = decode_ecu_list(&[0x00, 0x03, 0x10, 0x12, 0x40]).unwrap();
        assert_eq!(list.count, 3);
        assert_eq!(list.addresses, vec![0x10, 0x12, 0x40]);
    }

    #[test]
    fn empty_list_has_no_addresses() {
        let list = decode_ecu_list(&[0x00, 0x00]).unwrap();
        assert_eq!(list.count, 0);
        assert!(list.addresses.is_empty());
    }

    #[test]
    fn rejects_region_shorter_than_count() {
        assert!(matches!(
            decode_ecu_list(&[0x00]),
            Err(UdsError::ShortResponse { got: 1, .. })
        ));
    }
}
```

- [ ] **Step 3: Wire the module and run the test to see it fail**

In `crates/uds/src/lib.rs`, add `pub mod identity;` next to the other `pub mod` lines, and add to the `pub use` block:

```rust
pub use identity::{EcuList, decode_ecu_list};
```

Run: `cargo test -p klartext-uds identity`
Expected: FAIL to compile first if the module isn't wired, then PASS once Steps 2–3 are in. (If it compiles and passes immediately, that's fine — the test and impl were added together.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p klartext-uds identity`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/uds/src/service.rs crates/uds/src/identity.rs crates/uds/src/lib.rs
git commit -m "feat(uds): VCM DID constants + decode_ecu_list (SVT list)"
```

---

## Task 2: semantic — ECU-name overlay `name_ecu_list`

**Files:**
- Create: `crates/semantic/src/identity.rs`
- Modify: `crates/semantic/src/lib.rs` (declare `mod identity`; re-export)

**Interfaces:**
- Consumes: `Catalog` and `Catalog::ecus() -> Result<Vec<EcuSlot>, SemanticError>` (`crates/semantic/src/catalog.rs`), `EcuSlot { address, group_name, extra_groups, title }`.
- Produces: `NamedEcu { address: u8, name: Option<String>, title: Option<String> }`; `name_ecu_list(catalog: Option<&Catalog>, addresses: &[u8]) -> Vec<NamedEcu>`.

- [ ] **Step 1: Write the failing test**

Create `crates/semantic/src/identity.rs`:

```rust
//! Vehicle-identity decoding: the ECU-name overlay for the gateway SVT list, and
//! (later) the FA vehicle-order decode.
//!
//! The SVT read gives diagnostic addresses only; the gateway's own name table is
//! coarse and stale (its 0x40 says "CAS", wrong for many cars). So names come from
//! the ISTA-derived semantic DB (`Catalog::ecus()`) — generic across BMW — and an
//! address the DB doesn't know keeps a raw-hex name rather than being dropped.

use crate::catalog::{Catalog, EcuSlot};

/// One installed ECU with a DB-resolved name, or a raw-hex fallback name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedEcu {
    /// The diagnostic address from the SVT.
    pub address: u8,
    /// The ISTA group name for this address, or `None` if the DB lacks it.
    pub name: Option<String>,
    /// A human title for the address, if the DB has one.
    pub title: Option<String>,
}

/// Overlay DB names onto SVT addresses. Order and multiplicity follow `addresses`.
///
/// With no catalog, or for an address the DB does not know, `name`/`title` are
/// `None` — the address is always kept (never dropped or guessed).
pub fn name_ecu_list(catalog: Option<&Catalog>, addresses: &[u8]) -> Vec<NamedEcu> {
    let slots: Vec<EcuSlot> = catalog
        .and_then(|c| c.ecus().ok())
        .unwrap_or_default();
    addresses
        .iter()
        .map(|&address| {
            let slot = slots.iter().find(|s| s.address == address);
            NamedEcu {
                address,
                name: slot.map(|s| s.group_name.clone()),
                title: slot.and_then(|s| s.title.clone()),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_catalog_addresses_keep_raw_names() {
        let ecus = name_ecu_list(None, &[0x10, 0x12]);
        assert_eq!(
            ecus,
            vec![
                NamedEcu { address: 0x10, name: None, title: None },
                NamedEcu { address: 0x12, name: None, title: None },
            ]
        );
    }
}
```

(An `#[ignore]` test against the real DB is added in Task-2 Step 4.)

- [ ] **Step 2: Wire the module**

In `crates/semantic/src/lib.rs`, add `pub mod identity;` and extend the re-exports:

```rust
pub use identity::{NamedEcu, name_ecu_list};
```

- [ ] **Step 3: Run the test**

Run: `cargo test -p klartext-semantic identity`
Expected: PASS (1 test).

- [ ] **Step 4: Add an ignored real-DB smoke test**

Append to the `tests` module in `crates/semantic/src/identity.rs`:

```rust
    // Cross-check against the owner's real semantic DB. Ignored by default (BYO data).
    #[test]
    #[ignore = "requires BYO data: data/klartext-semantic.db"]
    fn real_db_names_known_addresses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/klartext-semantic.db");
        let catalog = Catalog::open(&path).expect("open semantic DB");
        // 0x10 is the gateway on F/G-series; the DB should name it, not guess.
        let ecus = name_ecu_list(Some(&catalog), &[0x10]);
        assert_eq!(ecus.len(), 1);
        assert_eq!(ecus[0].address, 0x10);
        assert!(ecus[0].name.is_some(), "DB should name the gateway address");
    }
```

Run: `cargo test -p klartext-semantic identity` (ignored test skipped) — Expected: PASS.
Optional local check: `cargo test -p klartext-semantic identity -- --ignored` — Expected: PASS if the DB is present.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/semantic/src/identity.rs crates/semantic/src/lib.rs
git commit -m "feat(semantic): name_ecu_list — DB name overlay for the SVT list"
```

---

## Task 3: client — `read_ecu_list` (the SVT read)

**Files:**
- Modify: `crates/client/src/client.rs` (add the method + a loopback test)

**Interfaces:**
- Consumes: `self.read_did(target, did)` (returns `(u16, Vec<u8>)`), `klartext_uds::{did, decode_ecu_list, EcuList}`, `ZGW_ADDRESS`.
- Produces: `DiagnosticClient::read_ecu_list(&self) -> Result<EcuList, ClientError>`.

- [ ] **Step 1: Write the failing loopback test**

The `client.rs` test module already has a loopback mock harness (see `crates/client/src/client.rs` around the `DDE` test). Add a test that stands up a gateway answering `22 3F 07`. Add near the other client tests in `crates/client/src/client.rs`:

```rust
    #[tokio::test]
    async fn read_ecu_list_decodes_svt_addresses() {
        // Gateway at 0x10 answers 22 3F 07 with: 62 3F 07 | count 0x0003 | 10 12 40
        let addr = spawn_gateway(&[
            (
                vec![0x22, 0x3F, 0x07],
                vec![0x62, 0x3F, 0x07, 0x00, 0x03, 0x10, 0x12, 0x40],
            ),
        ])
        .await;
        let client = client(addr).await;
        let list = client.read_ecu_list().await.unwrap();
        assert_eq!(list.count, 3);
        assert_eq!(list.addresses, vec![0x10, 0x12, 0x40]);
    }
```

If a reusable `spawn_gateway(&[(request, response)])` helper does not yet exist in this module, add one modeled on the `scan.rs` `spawn` mock (it accepts a request→response table and answers the ZGW address `0x10`, swapping SRC/TGT). Keep it minimal:

```rust
    /// A loopback gateway (address 0x10) answering a fixed request→response table.
    async fn spawn_gateway(table: &[(Vec<u8>, Vec<u8>)]) -> std::net::SocketAddr {
        use klartext_hsfz::{HsfzFrame, control, read_frame, write_frame};
        use std::net::Ipv4Addr;
        use tokio::net::TcpListener;
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let table: Vec<(Vec<u8>, Vec<u8>)> = table.to_vec();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                let (tester, ecu) = frame.addr.unwrap();
                if frame.payload == [0x3E, 0x80] {
                    continue;
                }
                if let Some((_, resp)) = table.iter().find(|(req, _)| *req == frame.payload) {
                    let _ = write_frame(
                        &mut stream,
                        &HsfzFrame::diagnostic(ecu, tester, resp.clone()),
                    )
                    .await;
                }
            }
        });
        addr
    }
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p klartext-client read_ecu_list`
Expected: FAIL with "no method named `read_ecu_list`".

- [ ] **Step 3: Implement `read_ecu_list`**

Add to the `impl DiagnosticClient` block in `crates/client/src/client.rs` (near `read_did`), and make sure `decode_ecu_list`, `EcuList`, and `did` are imported from `klartext_uds`:

```rust
    /// Read the gateway's installed-ECU list (the SVT) — UDS `22 3F 07` to the ZGW.
    ///
    /// Returns the diagnostic addresses the gateway reports as installed. Names are
    /// resolved separately from the semantic DB. This is the discovery source; there
    /// is no probe fallback. The response framing is DERIVED from the
    /// `STATUS_VCM_GET_ECU_LIST_ALL` disassembly — [verify against capture].
    ///
    /// # Errors
    /// As [`crate::Session::request`] (transport / negative), and [`ClientError::Uds`]
    /// if the positive response cannot be decoded.
    pub async fn read_ecu_list(&self) -> Result<EcuList, ClientError> {
        let (_did, data) = self.read_did(ZGW_ADDRESS, did::ECU_LIST_ALL).await?;
        Ok(decode_ecu_list(&data)?)
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p klartext-client read_ecu_list`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/client/src/client.rs
git commit -m "feat(client): read_ecu_list — SVT fitted-ECU read (22 3F 07)"
```

---

## Task 4: client — delete the M10 probe-scan; rewire `scan_faults`

**Files:**
- Modify: `crates/client/src/client.rs` (delete `probe` + `ProbeOutcome` + their test)
- Modify: `crates/client/src/scan.rs` (delete `scan_present`, `FittedEcu`, `ScanOptions::probe_timeout`; rewire `scan_faults`)
- Modify: `crates/client/src/lib.rs` (drop the deleted re-exports)

**Interfaces:**
- Produces: `scan_faults(&self, addrs: &[u8], concurrency: usize) -> Vec<EcuFaults>` (was `(&self, addrs, opts: ScanOptions)`); `ScanOptions` removed. `EcuFaults`, `ClearReport`, `clear_faults_verified`, `clear_faults_all` unchanged.

> This task deletes a subsystem. Do it after Task 3 so the SVT read that replaces discovery already exists. Callers in mcp/cli are fixed in Tasks 8–10; the workspace will not fully build until then, so verify this task with `cargo test -p klartext-client` (the crate builds and its tests pass in isolation).

- [ ] **Step 1: Delete `probe` and `ProbeOutcome`**

In `crates/client/src/client.rs`, delete the `ProbeOutcome` enum (around line 68–80), the `pub async fn probe(...)` method (around line 300), and the probe test (`probe` usages around line 630–644). In `crates/client/src/lib.rs`, remove `ProbeOutcome` from the `pub use client::{...}` list and remove `FittedEcu` and `ScanOptions` from the `pub use scan::{...}` list (leaving `ClearReport, EcuFaults`).

- [ ] **Step 2: Rewrite `scan.rs` without probing**

In `crates/client/src/scan.rs`: delete the `ScanOptions` struct, its `Default` impl, the `FittedEcu` struct, the `scan_present` method, and the `use crate::client::{DiagnosticClient, ProbeOutcome};` (change to `use crate::client::DiagnosticClient;`). Change `scan_faults` to take a `concurrency: usize` and read faults directly over the given addresses (no probe step):

```rust
    /// Read and partition faults for each address in `addrs`, bounded by `concurrency`.
    ///
    /// `addrs` is the fitted list from the gateway SVT ([`DiagnosticClient::read_ecu_list`]).
    /// A per-ECU read failure (e.g. an installed-but-silent ECU) is recorded in
    /// [`EcuFaults::error`], never aborting the scan. The result is sorted by address.
    pub async fn scan_faults(&self, addrs: &[u8], concurrency: usize) -> Vec<EcuFaults> {
        let mut out: Vec<EcuFaults> = stream::iter(addrs.iter().copied())
            .map(|address| async move {
                match self.read_all_dtcs(address).await {
                    Ok(dtcs) => {
                        let (relevant, noise): (Vec<Dtc>, Vec<Dtc>) =
                            dtcs.into_iter().partition(|d| d.is_relevant());
                        EcuFaults {
                            address,
                            relevant,
                            not_tested: noise.len(),
                            error: None,
                        }
                    }
                    Err(error) => EcuFaults {
                        address,
                        relevant: Vec::new(),
                        not_tested: 0,
                        error: Some(error.to_string()),
                    },
                }
            })
            .buffer_unordered(concurrency.max(1))
            .collect()
            .await;
        out.sort_unstable_by_key(|e| e.address);
        out
    }
```

- [ ] **Step 3: Update `scan.rs` tests**

Delete `scan_present_finds_only_fitted_ecus` and `scan_concurrency_overlaps_absent_probes` (they tested probing). Update `scan_faults_partitions_relevant_from_not_tested` to the new signature and remove the `opts()` helper:

```rust
    #[tokio::test]
    async fn scan_faults_partitions_relevant_from_not_tested() {
        let addr = spawn(&[0x12]).await;
        let client = client(addr).await;
        let faults = client.scan_faults(&[0x12], 4).await;
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].address, 0x12);
        assert_eq!(faults[0].relevant.len(), 1);
        assert_eq!(faults[0].not_tested, 1);
        assert!(faults[0].error.is_none());
    }
```

(A silent-but-listed ECU now yields an `error` entry rather than being skipped; add a small test if the mock supports an address that never answers `19 02`.)

- [ ] **Step 4: Run the client tests**

Run: `cargo test -p klartext-client`
Expected: PASS (probe/scan_present tests gone; `read_ecu_list` and the updated `scan_faults` pass).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/client/src/client.rs crates/client/src/scan.rs crates/client/src/lib.rs
git commit -m "refactor(client): delete M10 probe-scan; scan_faults reads over given addrs"
```

---

## Task 5: client — gateway reads (I-Stufe, vehicle order raw, identification)

**Files:**
- Modify: `crates/client/src/client.rs` (three read methods + types + tests)

**Interfaces:**
- Consumes: `self.read_did`, `self.request_optional` (already present — maps a negative to `None`), `klartext_uds::did`, `klartext_semantic::did::decode` (add `klartext-semantic` as a dependency of `klartext-client` if not already — check `crates/client/Cargo.toml`; if adding, use `cargo add`).
- Produces:
  - `read_i_stufe(&self) -> Result<Option<String>, ClientError>`
  - `read_vehicle_order(&self) -> Result<Vec<u8>, ClientError>` (raw FA bytes; decode is semantic, Task 7)
  - `read_ecu_identification(&self, target: u8) -> Result<EcuIdentification, ClientError>`
  - `EcuIdentification { address: u8, fields: Vec<IdField> }`, `IdField { did: u16, name: Option<&'static str>, text: Option<String>, raw: Vec<u8> }`
  - `IDENTIFICATION_DIDS: [u16; N]` (the ISO-standard set from protocol-reference §1.5)

- [ ] **Step 1: Write the failing tests**

Add to the `client.rs` test module (uses the `spawn_gateway` helper from Task 3, extended so a per-target table is possible — for identification, target the ECU address, not 0x10):

```rust
    #[tokio::test]
    async fn read_i_stufe_returns_ascii() {
        // 22 10 0B -> 62 10 0B "F020-21-11-500"
        let mut resp = vec![0x62, 0x10, 0x0B];
        resp.extend_from_slice(b"F020-21-11-500");
        let addr = spawn_gateway(&[(vec![0x22, 0x10, 0x0B], resp)]).await;
        let client = client(addr).await;
        assert_eq!(
            client.read_i_stufe().await.unwrap().as_deref(),
            Some("F020-21-11-500")
        );
    }

    #[tokio::test]
    async fn read_ecu_identification_collects_answered_dids_and_skips_negatives() {
        // The ECU answers F190 (VIN) and F197 (system name) but rejects the rest.
        let mut vin = vec![0x62, 0xF1, 0x90];
        vin.extend_from_slice(b"WBA1K2C50EV000000");
        let mut sysname = vec![0x62, 0xF1, 0x97];
        sysname.extend_from_slice(b"DDE");
        let addr = spawn_gateway_for(
            0x12,
            &[
                (vec![0x22, 0xF1, 0x90], vin),
                (vec![0x22, 0xF1, 0x97], sysname),
            ],
        )
        .await;
        let client = client(addr).await;
        let ident = client.read_ecu_identification(0x12).await.unwrap();
        assert_eq!(ident.address, 0x12);
        // Only the two answered DIDs are present (negatives skipped).
        let vin_field = ident.fields.iter().find(|f| f.did == 0xF190).unwrap();
        assert_eq!(vin_field.name, Some("VIN"));
        assert_eq!(vin_field.text.as_deref(), Some("WBA1K2C50EV000000"));
        assert!(ident.fields.iter().any(|f| f.did == 0xF197));
        assert!(!ident.fields.iter().any(|f| f.did == 0xF187)); // rejected -> skipped
    }
```

`spawn_gateway_for(target, table)` is `spawn_gateway` but the mock only answers when the frame's ECU target equals `target` and rejects other DIDs with `7F 22 31` (requestOutOfRange). Add it beside `spawn_gateway` (a few lines: check `ecu == target`, look up the table, else reply `vec![0x7F, 0x22, 0x31]`).

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p klartext-client identification`
Expected: FAIL (`read_i_stufe`/`read_ecu_identification` not found).

- [ ] **Step 3: Implement the reads**

Add the identification DID set and the three methods to `crates/client/src/client.rs`:

```rust
/// The ISO-standardized identification DIDs (protocol-reference §1.5). The same
/// allocation for any UDS ECU, so reading this set stays generic across BMW; an
/// ECU serves only some of them, so a negative answer for one is normal (skipped).
pub const IDENTIFICATION_DIDS: [u16; 12] = [
    0xF190, // VIN
    0xF187, // vehicleManufacturerSparePartNumber
    0xF188, // vehicleManufacturerECUSoftwareNumber
    0xF189, // vehicleManufacturerECUSoftwareVersionNumber
    0xF191, // vehicleManufacturerECUHardwareNumber
    0xF192, // systemSupplierECUHardwareNumber
    0xF193, // systemSupplierECUHardwareVersionNumber
    0xF194, // systemSupplierECUSoftwareNumber
    0xF195, // systemSupplierECUSoftwareVersionNumber
    0xF197, // systemName
    0xF19E, // ASAMODXFileIdentifier
    0xF18C, // ECUSerialNumber
];

/// One identification DID's value from an ECU.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IdField {
    /// The DID read.
    pub did: u16,
    /// The ISO-standard name, if known (`klartext_semantic::did::standard_name`).
    pub name: Option<&'static str>,
    /// An ASCII/UTF-8 rendering when the bytes are printable text.
    pub text: Option<String>,
    /// The raw value bytes, always present.
    pub raw: Vec<u8>,
}

/// One ECU's identification block: the standardized DIDs it actually served.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcuIdentification {
    /// The ECU's diagnostic address.
    pub address: u8,
    /// The DIDs that answered (negatives are skipped).
    pub fields: Vec<IdField>,
}
```

```rust
    /// Read the gateway's integration level (I-Stufe) — UDS `22 10 0B`.
    ///
    /// Returns the ASCII value, or `None` if the gateway rejects the DID. Framing is
    /// DERIVED from `STATUS_VCM_I_STUFE_LESEN` — [verify against capture].
    pub async fn read_i_stufe(&self) -> Result<Option<String>, ClientError> {
        let Some(resp) = self
            .request_optional(ZGW_ADDRESS, &read_data_by_identifier(did::I_STUFE))
            .await?
        else {
            return Ok(None);
        };
        let (_did, raw) = decode_read_data_by_identifier(&resp)?;
        Ok(String::from_utf8(raw).ok().filter(|s| !s.is_empty()))
    }

    /// Read the raw vehicle order (FA) bytes — UDS `22 3F 06`.
    ///
    /// Returns the raw FA region; field decode is the semantic layer's job
    /// (`klartext_semantic::decode_vehicle_order`). Framing DERIVED — [verify against capture].
    pub async fn read_vehicle_order(&self) -> Result<Vec<u8>, ClientError> {
        let (_did, raw) = self.read_did(ZGW_ADDRESS, did::VEHICLE_ORDER).await?;
        Ok(raw)
    }

    /// Read one ECU's identification block: the standardized DIDs it serves.
    ///
    /// Issues each of [`IDENTIFICATION_DIDS`] to `target`; a DID the ECU does not
    /// serve answers negatively and is skipped (not an error). Names come from the
    /// ISO-standard table (`klartext_semantic::did::standard_name`).
    pub async fn read_ecu_identification(
        &self,
        target: u8,
    ) -> Result<EcuIdentification, ClientError> {
        let mut fields = Vec::new();
        for did in IDENTIFICATION_DIDS {
            let Some(resp) = self
                .request_optional(target, &read_data_by_identifier(did))
                .await?
            else {
                continue; // ECU does not serve this DID
            };
            let (got, raw) = decode_read_data_by_identifier(&resp)?;
            if got != did {
                continue; // desynced echo; skip rather than mislabel
            }
            let decoded = klartext_semantic::did::decode(did, &raw);
            fields.push(IdField {
                did,
                name: decoded.name,
                text: decoded.text,
                raw,
            });
        }
        Ok(EcuIdentification { address: target, fields })
    }
```

Ensure the imports at the top of `client.rs` include `read_data_by_identifier`, `decode_read_data_by_identifier`, and `did` from `klartext_uds`.

- [ ] **Step 4: Export the new types**

In `crates/client/src/lib.rs`, add `EcuIdentification`, `IdField`, `IDENTIFICATION_DIDS` to the `pub use client::{...}` block.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p klartext-client identification i_stufe`
Expected: PASS.

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/client/src/client.rs crates/client/src/lib.rs crates/client/Cargo.toml
git commit -m "feat(client): I-Stufe, vehicle-order raw, per-ECU identification reads"
```

---

## Task 6: client — `identify_vehicle` orchestration

**Files:**
- Modify: `crates/client/src/client.rs` (orchestration + `VehicleIdentity` type + test)

**Interfaces:**
- Consumes: `read_ecu_list`, `read_ecu_identification`, `read_vehicle_order`, `read_i_stufe`, `read_did(_, did::VIN)`.
- Produces: `VehicleIdentity { vin: Option<String>, vehicle_order_raw: Vec<u8>, i_stufe: Option<String>, ecus: Vec<u8>, identification: Vec<EcuIdentification> }`; `identify_vehicle(&self) -> Result<VehicleIdentity, ClientError>`.

> `identify_vehicle` returns the raw FA bytes and the SVT address list; the semantic layer (Task 7) decodes the FA and (Task 2) names the addresses at the surface. Keeping the orchestration DB-free keeps `klartext-client` free of a DB dependency.

- [ ] **Step 1: Write the failing test**

```rust
    #[tokio::test]
    async fn identify_vehicle_aggregates_svt_and_identification() {
        let mut vin = vec![0x62, 0xF1, 0x90];
        vin.extend_from_slice(b"WBA1K2C50EV000000");
        // Gateway 0x10: SVT lists 0x12 only; VIN; I-Stufe; FA raw.
        let addr = spawn_gateway_multi(&[
            (0x10, vec![0x22, 0x3F, 0x07], vec![0x62, 0x3F, 0x07, 0x00, 0x01, 0x12]),
            (0x10, vec![0x22, 0xF1, 0x90], vin.clone()),
            (0x10, vec![0x22, 0x10, 0x0B], {
                let mut r = vec![0x62, 0x10, 0x0B];
                r.extend_from_slice(b"F020-21-11-500");
                r
            }),
            (0x10, vec![0x22, 0x3F, 0x06], vec![0x62, 0x3F, 0x06, 0xAA, 0xBB]),
            (0x12, vec![0x22, 0xF1, 0x90], vin),
        ])
        .await;
        let client = client(addr).await;
        let id = client.identify_vehicle().await.unwrap();
        assert_eq!(id.vin.as_deref(), Some("WBA1K2C50EV000000"));
        assert_eq!(id.ecus, vec![0x12]);
        assert_eq!(id.i_stufe.as_deref(), Some("F020-21-11-500"));
        assert_eq!(id.vehicle_order_raw, vec![0xAA, 0xBB]);
        assert_eq!(id.identification.len(), 1);
        assert_eq!(id.identification[0].address, 0x12);
    }
```

`spawn_gateway_multi(&[(target, req, resp)])` generalizes `spawn_gateway` to key the table on `(ecu_target, payload)`. Add it beside the other mocks.

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p klartext-client identify_vehicle`
Expected: FAIL (`identify_vehicle` / `VehicleIdentity` not found).

- [ ] **Step 3: Implement `identify_vehicle` and `VehicleIdentity`**

```rust
/// The whole-vehicle identity: VIN, raw FA, I-Stufe, the SVT address list, and each
/// fitted ECU's identification block. FA decode and ECU naming happen at the surface
/// (semantic layer) so this stays DB-free.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VehicleIdentity {
    /// The vehicle VIN (gateway `22 F190`), if it answered.
    pub vin: Option<String>,
    /// The raw vehicle-order (FA) bytes (`22 3F06`); decode with the semantic layer.
    pub vehicle_order_raw: Vec<u8>,
    /// The integration level (`22 100B`), if it answered.
    pub i_stufe: Option<String>,
    /// The installed diagnostic addresses from the SVT (`22 3F07`).
    pub ecus: Vec<u8>,
    /// Each installed ECU's identification block.
    pub identification: Vec<EcuIdentification>,
}
```

```rust
    /// Read the whole vehicle identity: SVT list, per-ECU identification, VIN, FA, I-Stufe.
    ///
    /// All autonomous-safe `0x22` reads. The SVT list is the discovery source (no
    /// probe). A per-ECU identification failure is recorded as an empty block for that
    /// ECU rather than aborting the whole read.
    ///
    /// # Errors
    /// [`ClientError`] if the SVT read itself fails (there is no probe fallback); a
    /// missing VIN / FA / I-Stufe degrades to `None`/empty, not an error.
    pub async fn identify_vehicle(&self) -> Result<VehicleIdentity, ClientError> {
        let list = self.read_ecu_list().await?;
        let vin = match self.read_did(ZGW_ADDRESS, did::VIN).await {
            Ok((_, raw)) => String::from_utf8(raw).ok().filter(|s| !s.is_empty()),
            Err(ClientError::Negative { .. }) => None,
            Err(e) => return Err(e),
        };
        let i_stufe = self.read_i_stufe().await?;
        let vehicle_order_raw = match self.read_vehicle_order().await {
            Ok(raw) => raw,
            Err(ClientError::Negative { .. }) => Vec::new(),
            Err(e) => return Err(e),
        };
        let mut identification = Vec::with_capacity(list.addresses.len());
        for &address in &list.addresses {
            let block = self
                .read_ecu_identification(address)
                .await
                .unwrap_or(EcuIdentification { address, fields: Vec::new() });
            identification.push(block);
        }
        Ok(VehicleIdentity {
            vin,
            vehicle_order_raw,
            i_stufe,
            ecus: list.addresses,
            identification,
        })
    }
```

Export `VehicleIdentity` from `crates/client/src/lib.rs`.

- [ ] **Step 4: Run the test**

Run: `cargo test -p klartext-client identify_vehicle`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/client/src/client.rs crates/client/src/lib.rs
git commit -m "feat(client): identify_vehicle — aggregate SVT + identification + FA + I-Stufe"
```

---

## Task 7: semantic — `decode_vehicle_order` (FA), version + raw now, fields capture-gated

**Files:**
- Modify: `crates/semantic/src/identity.rs` (add `VehicleOrder` + `decode_vehicle_order`)
- Modify: `crates/semantic/src/lib.rs` (re-export)

**Interfaces:**
- Consumes: the raw FA bytes from `client.read_vehicle_order` (the `62 3F 06` data region, i.e. bytes after the echo).
- Produces: `VehicleOrder { version: Option<u16>, baureihe: Option<String>, typ_schluessel: Option<String>, lackcode: Option<String>, polstercode: Option<String>, build_date: Option<String>, options: Vec<String>, raw: Vec<u8> }`; `decode_vehicle_order(region: &[u8]) -> VehicleOrder`.

> **Capture-gated scope.** The FA field layout is data-driven and version-branched in the `STATUS_VCM_GET_FA` bytecode with a 2-bit SALAPA packing (`TABKOMPRIMIERUNG`: `00→'0' 01→'3' 10→'4' 11→'5'`) — its exact byte offsets are NOT cleanly derivable offline and need the owner's on-car FA capture to confirm. This task implements the **certain** part now — expose `version` (data offset derived from the bytecode) and always keep `raw` — and leaves the header fields / option list as `None`/empty with a note until the capture lands. Do NOT invent offsets. When the capture arrives, extend this function (its tests use capture-confirmed vectors) — the surrounding code already degrades to raw.

- [ ] **Step 1: Write the test (version + raw; fields None pending capture)**

Add to `crates/semantic/src/identity.rs`:

```rust
/// The decoded vehicle order (Fahrzeugauftrag / FA) from gateway DID 0x3F06.
///
/// `version` and `raw` are decoded now; the header fields and option list are
/// **capture-gated** (the FA byte layout is version-branched, compressed bytecode
/// that needs an on-car capture to confirm) and stay `None`/empty until then. `raw`
/// is always kept so nothing is lost. [verify against capture]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VehicleOrder {
    pub version: Option<u16>,
    pub baureihe: Option<String>,
    pub typ_schluessel: Option<String>,
    pub lackcode: Option<String>,
    pub polstercode: Option<String>,
    pub build_date: Option<String>,
    pub options: Vec<String>,
    pub raw: Vec<u8>,
}

/// FA data-region offset of the version byte, read by STATUS_VCM_GET_FA (`move L0,#5`).
/// [verify against capture] — the EDIABAS bytecode reads this over its KWP framing;
/// the offset in the raw HSFZ region is confirmed on capture.
const FA_VERSION_OFFSET: usize = 5;
```

```rust
#[cfg(test)]
mod fa_tests {
    use super::*;

    #[test]
    fn decodes_version_and_keeps_raw_fields_pending_capture() {
        // Synthetic FA region: version byte 0x02 at the derived offset. [verify]
        let region = vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x11, 0x22];
        let fa = decode_vehicle_order(&region);
        assert_eq!(fa.version, Some(2));
        assert_eq!(fa.raw, region);
        // Field decode is capture-gated: None/empty until the FA layout is confirmed.
        assert_eq!(fa.baureihe, None);
        assert!(fa.options.is_empty());
    }

    #[test]
    fn short_region_has_no_version_but_keeps_raw() {
        let fa = decode_vehicle_order(&[0x01, 0x02]);
        assert_eq!(fa.version, None);
        assert_eq!(fa.raw, vec![0x01, 0x02]);
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p klartext-semantic fa_tests`
Expected: FAIL (`decode_vehicle_order` not found).

- [ ] **Step 3: Implement `decode_vehicle_order` (certain part only)**

```rust
/// Decode the FA (vehicle order) data region. Extracts the version and keeps the raw
/// bytes; header fields and the option list are capture-gated (see [`VehicleOrder`]).
pub fn decode_vehicle_order(region: &[u8]) -> VehicleOrder {
    let version = region
        .get(FA_VERSION_OFFSET)
        .map(|&b| u16::from(b));
    VehicleOrder {
        version,
        baureihe: None,
        typ_schluessel: None,
        lackcode: None,
        polstercode: None,
        build_date: None,
        options: Vec::new(),
        raw: region.to_vec(),
    }
}
```

Re-export from `crates/semantic/src/lib.rs`: add `VehicleOrder, decode_vehicle_order` to the `identity` re-export line.

- [ ] **Step 4: Run the test**

Run: `cargo test -p klartext-semantic fa_tests`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add crates/semantic/src/identity.rs crates/semantic/src/lib.rs
git commit -m "feat(semantic): decode_vehicle_order — FA version+raw (fields capture-gated)"
```

---

## Task 8: mcp — rewire discovery to the SVT; DTO cleanup

**Files:**
- Modify: `mcp/src/server.rs` (`scan_ecus`, `read_all_faults`: replace `scan_present(universe)` with `read_ecu_list`; delete `scan_universe`)
- Modify: `mcp/src/dto.rs` (`FittedEcuInfo` drop `latency_ms`; `ScanEcusResult` drop `probed`)
- Modify: `mcp/src/config.rs` (remove `probe_timeout`; keep `scan_concurrency`)

**Interfaces:**
- Consumes: `conn.client.read_ecu_list()`, `klartext_semantic::name_ecu_list` (or the existing `ecu_names(address, catalog)` helper), `conn.client.scan_faults(&addrs, concurrency)`.

- [ ] **Step 1: Update `config.rs`**

Remove the `probe_timeout` field and its CLI arg from `mcp/src/config.rs`, and replace `scan_options()` (which built `ScanOptions`) with a plain accessor `pub fn scan_concurrency(&self) -> usize { self.scan_concurrency }`. Keep the `scan_concurrency` arg.

- [ ] **Step 2: Update the DTOs**

In `mcp/src/dto.rs`: remove `latency_ms` from `FittedEcuInfo`, and remove `probed` from `ScanEcusResult`. Update their doc comments (no more "probe").

- [ ] **Step 3: Rewrite `scan_ecus` on the SVT**

Replace the `scan_ecus` body in `mcp/src/server.rs` so the fresh branch reads the SVT instead of probing:

```rust
        let (addrs, cached) = match (req.rescan, conn.fitted()) {
            (false, Some(fitted)) => (fitted.to_vec(), true),
            _ => {
                let list = conn.client.read_ecu_list().await.map_err(|e| {
                    McpError::internal_error(format!("reading the gateway SVT: {e}"), None)
                })?;
                conn.set_fitted(list.addresses.clone());
                (list.addresses, false)
            }
        };
        let ecus = addrs
            .iter()
            .map(|&address| {
                let (group_name, title) = ecu_names(address, catalog.as_ref());
                FittedEcuInfo { address_hex: format!("0x{address:02X}"), group_name, title }
            })
            .collect();
        let note = if cached {
            "Cached fitted-ECU list from an earlier read this session — pass rescan=true to re-read.".to_string()
        } else {
            format!("Read {} installed ECU(s) from the gateway SVT.", addrs.len())
        };
        Ok(Json(ScanEcusResult { ecus, note }))
```

Delete the `scan_universe` helper and its call; update the tool `description` from "probing each candidate address with a harmless TesterPresent" to "reading the gateway's installed-ECU list (SVT)".

- [ ] **Step 4: Rewrite the `read_all_faults` discovery the same way**

In `read_all_faults`, replace the `scan_present(&universe, …)` block with the SVT read (same `read_ecu_list` pattern) and call `conn.client.scan_faults(&addrs, self.config.scan_concurrency())`.

- [ ] **Step 5: Build and test the MCP crate**

Run: `cargo test -p klartext-mcp`
Expected: PASS (compiles against the new client API; existing integration tests that referenced probing are updated in Task 9).

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add mcp/src/server.rs mcp/src/dto.rs mcp/src/config.rs
git commit -m "refactor(mcp): discovery via gateway SVT; drop probe config + DTO fields"
```

---

## Task 9: mcp — `identify_vehicle` tool

**Files:**
- Modify: `mcp/src/dto.rs` (`VehicleIdentityResult` + field DTOs)
- Modify: `mcp/src/server.rs` (the `identify_vehicle` tool)
- Modify: `mcp/tests/integration.rs` (integration test)

**Interfaces:**
- Consumes: `conn.client.identify_vehicle()`, `klartext_semantic::{name_ecu_list, decode_vehicle_order}`, the catalog.
- Produces: MCP tool `identify_vehicle` returning `Json<VehicleIdentityResult>`.

- [ ] **Step 1: Add the DTOs**

In `mcp/src/dto.rs`:

```rust
/// One ECU's identification block for the MCP surface.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EcuIdentDto {
    pub address_hex: String,
    pub name: Option<String>,
    pub fields: Vec<IdFieldDto>,
}

/// One identification DID value.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct IdFieldDto {
    pub did_hex: String,
    pub name: Option<String>,
    pub text: Option<String>,
    pub raw_hex: String,
}

/// The decoded vehicle order (FA) for the MCP surface (fields capture-gated).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct VehicleOrderDto {
    pub version: Option<u16>,
    pub baureihe: Option<String>,
    pub typ_schluessel: Option<String>,
    pub lackcode: Option<String>,
    pub polstercode: Option<String>,
    pub build_date: Option<String>,
    pub options: Vec<String>,
    pub raw_hex: String,
}

/// Result of `identify_vehicle`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct VehicleIdentityResult {
    pub vin: Option<String>,
    pub i_stufe: Option<String>,
    pub vehicle_order: VehicleOrderDto,
    pub ecus: Vec<FittedEcuInfo>,
    pub identification: Vec<EcuIdentDto>,
    pub notes: Vec<String>,
}
```

- [ ] **Step 2: Add the tool**

In `mcp/src/server.rs`, mirror the `read_fault_detail` tool structure:

```rust
    /// Read the full vehicle identity — VIN, model/build (FA), I-Stufe, the fitted
    /// ECU list by name, and each ECU's identification block. All 0x22 reads.
    #[tool(
        description = "Read the car's full identity in one call: VIN, integration \
        level (I-Stufe), the vehicle order (FA — model/type/paint/upholstery/options, \
        where decodable), the authoritative list of FITTED ECUs by name (from the \
        gateway SVT), and each ECU's identification block (hardware/software part \
        numbers, system name, serial). All standard UDS 0x22 reads — safe and \
        non-mutating. NOTE: the FA field decode and the SVT/identification response \
        framing are derived from disassembly and pending an on-car capture, so treat \
        FA fields as provisional and expect some identification DIDs to be absent."
    )]
    pub async fn identify_vehicle(&self) -> Result<Json<VehicleIdentityResult>, McpError> {
        let catalog = self.catalog();
        let identity = {
            let guard = self.state.lock().await;
            let conn = guard.as_ref().ok_or_else(not_connected)?;
            conn.client
                .identify_vehicle()
                .await
                .map_err(|e| McpError::internal_error(format!("reading vehicle identity: {e}"), None))?
        };

        let named = klartext_semantic::name_ecu_list(catalog.as_ref(), &identity.ecus);
        let ecus = named
            .into_iter()
            .map(|n| FittedEcuInfo {
                address_hex: format!("0x{:02X}", n.address),
                group_name: n.name,
                title: n.title,
            })
            .collect();

        let fa = klartext_semantic::decode_vehicle_order(&identity.vehicle_order_raw);
        let vehicle_order = VehicleOrderDto {
            version: fa.version,
            baureihe: fa.baureihe,
            typ_schluessel: fa.typ_schluessel,
            lackcode: fa.lackcode,
            polstercode: fa.polstercode,
            build_date: fa.build_date,
            options: fa.options,
            raw_hex: hex_spaced(&fa.raw),
        };

        let identification = identity
            .identification
            .into_iter()
            .map(|block| EcuIdentDto {
                address_hex: format!("0x{:02X}", block.address),
                name: klartext_semantic::name_ecu_list(catalog.as_ref(), &[block.address])
                    .into_iter()
                    .next()
                    .and_then(|n| n.name),
                fields: block
                    .fields
                    .into_iter()
                    .map(|f| IdFieldDto {
                        did_hex: format!("{:04X}", f.did),
                        name: f.name.map(str::to_owned),
                        text: f.text,
                        raw_hex: hex_spaced(&f.raw),
                    })
                    .collect(),
            })
            .collect();

        Ok(Json(VehicleIdentityResult {
            vin: identity.vin,
            i_stufe: identity.i_stufe,
            vehicle_order,
            ecus,
            identification,
            notes: vec![
                "SVT/identification framing and FA field decode are derived from \
                 disassembly and pending an on-car capture — treat as provisional."
                    .to_string(),
            ],
        }))
    }
```

Use the crate's existing spaced-hex helper (the freeze-frame DTOs already format `raw_hex`); if it is named differently, reuse that instead of `hex_spaced`.

- [ ] **Step 3: Write the integration test**

In `mcp/tests/integration.rs`, add a test that connects to a loopback gateway answering the identity reads (mirror the existing `read_fault_detail` integration test's mock setup) and asserts `identify_vehicle` returns the VIN and the named fitted list. Run:

Run: `cargo test -p klartext-mcp identify_vehicle`
Expected: PASS.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add mcp/src/dto.rs mcp/src/server.rs mcp/tests/integration.rs
git commit -m "feat(mcp): identify_vehicle tool (VIN + FA + I-Stufe + named SVT + identification)"
```

---

## Task 10: cli — `identify` command; `scan` via SVT; remove `--probe-timeout`

**Files:**
- Modify: `cli/src/main.rs` (add `Identify` command + printer; rewire `Scan`; remove `--probe-timeout` and the address-universe helper; `scan_faults` call)

**Interfaces:**
- Consumes: `client.identify_vehicle()`, `client.read_ecu_list()`, `klartext_semantic::{name_ecu_list, decode_vehicle_order}`, `client.scan_faults(&addrs, concurrency)`.

- [ ] **Step 1: Remove the probe flag + universe helper**

In `cli/src/main.rs`: delete the `probe_timeout` arg (around line 73–75) and the `scan_options` helper (around line 526–531; replace with reading `cli.scan_concurrency` directly). Delete the `discovery_universe`/scan-universe helper (around line 534). Keep `--scan-concurrency`.

- [ ] **Step 2: Rewire `Scan` to the SVT**

Where `Scan` calls `client.scan_present(&universe, …)` (around lines 556 and 627), replace with:

```rust
    let list = client.read_ecu_list().await?;
    let named = klartext_semantic::name_ecu_list(catalog.as_ref(), &list.addresses);
    // print `named` (address · name · title); then, unless ecus_only:
    let faults = client.scan_faults(&list.addresses, cli.scan_concurrency).await;
```

- [ ] **Step 3: Add the `Identify` command**

Add to the `Command` enum in `cli/src/main.rs`:

```rust
    /// Read the full vehicle identity: VIN, FA (model/build), I-Stufe, fitted ECUs, identification.
    Identify,
```

Add a match arm that calls `client.identify_vehicle().await?`, then prints: VIN, I-Stufe, the decoded FA (`klartext_semantic::decode_vehicle_order` on `identity.vehicle_order_raw` — version + raw now; fields when present), and the fitted-ECU table via `klartext_semantic::name_ecu_list(catalog.as_ref(), &identity.ecus)`, then each ECU's identification fields (name · text · raw). Follow the existing printer style used by `FaultDetail`.

- [ ] **Step 4: Build and smoke-check the CLI**

Run: `cargo build -p klartext-cli`
Expected: builds clean.
Run: `cargo run -p klartext-cli -- --help` and confirm `identify` appears and `--probe-timeout` is gone.

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add cli/src/main.rs
git commit -m "feat(cli): identify command; scan via SVT; remove --probe-timeout"
```

---

## Task 11: workspace green + docs

**Files:**
- Modify: `docs/protocol-reference.md` (record the VCM DIDs + the STATUS_/STEUERN_ job-class convention)
- Modify: `docs/field-findings-2026-07-03.md` (note the identification/SVT on-car capture as a pending manual step)
- Modify: `CLAUDE.md` (M-note: item-2 invariant resolution — SVT list = 0x22 read)
- Modify: `README.md` (new `identify` command / tool; scan is SVT-based)

- [ ] **Step 1: Full workspace check**

Run: `cargo fmt --all && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: fmt clean, no clippy warnings, all tests pass. Fix anything that fails before continuing.

- [ ] **Step 2: Document the protocol facts**

In `docs/protocol-reference.md`, add to §1.5 (or a new short subsection): the BMW gateway VCM read DIDs — `0x3F07` installed-ECU list (SVT), `0x3F06` vehicle order (FA), `0x100B` I-Stufe — all read from the gateway (`0x10`) via `0x22`; and the EDIABAS job-class convention (`STATUS_*` = read → `0x22`/`0x19`, `STEUERN_*` = control → `0x31`/`0x2E`), marking the response framings `[verify against capture]`.

- [ ] **Step 3: Note the capture gate + M-note**

In `docs/field-findings-2026-07-03.md`, add a bullet: an on-car capture of `22 3F07 / 3F06 / 100B` on the F20 is the pending manual step to confirm the SVT/FA/I-Stufe framing (the current pcap has none). In `CLAUDE.md`, add a one-line M11-item-2 note under the milestone section: the SVT fitted-list read is UDS `22 3F 07` (a read), MCP-exposed; the `0x31` GENERATE_SVT stays out.

- [ ] **Step 4: README**

In `README.md`, document the new `identify` CLI command and MCP `identify_vehicle` tool, and note that `scan` now reads the gateway SVT (no probe flags).

- [ ] **Step 5: Commit**

```bash
git add docs/protocol-reference.md docs/field-findings-2026-07-03.md CLAUDE.md README.md
git commit -m "docs(m11): record VCM DIDs, job-class convention, identify surface"
```

---

## Self-Review

**Spec coverage:**
- SVT fitted-list read → Task 1 (decode) + Task 3 (read) + Tasks 8/10 (discovery). ✓
- Per-ECU identification block → Task 5 + surfaced in Tasks 9/10. ✓
- I-Stufe → Task 5. ✓
- FA vehicle-order decode → Task 5 (read raw) + Task 7 (version+raw now; fields capture-gated, honestly scoped per §3.3/§9). ✓
- Aggregate report → Task 6 (client) + Tasks 9/10 (surfaces). ✓
- Discovery replaces probe-scan, clean removal → Task 4 (+ Tasks 8/10 caller updates). ✓
- Stay-generic (DB names, no hardcoding, version-branched FA) → Tasks 2, 7, and the global constraints. ✓
- Blast radius (all 0x22 reads; no 0x31) → global constraints; every read method. ✓
- Verify-against-capture markers → Tasks 1, 5, 7, 11. ✓

**Placeholder scan:** The FA field decode (Task 7) is deliberately scoped to version+raw with fields capture-gated — this is an explicit, honest boundary (the spec §3.3 marks the offsets `[verify against capture]`), not a hand-wave; the code compiles and is tested. No `TODO`/"add error handling"/"similar to Task N" placeholders elsewhere.

**Type consistency:** `EcuList`/`decode_ecu_list` (Task 1) consumed by Task 3; `NamedEcu`/`name_ecu_list` (Task 2) consumed by Tasks 9/10; `EcuIdentification`/`IdField`/`IDENTIFICATION_DIDS` (Task 5) consumed by Tasks 6/9; `VehicleIdentity` (Task 6) consumed by Tasks 9/10; `VehicleOrder`/`decode_vehicle_order` (Task 7) consumed by Tasks 9/10; `scan_faults(&addrs, concurrency)` (Task 4) consumed by Tasks 8/10. Names match across tasks. ✓
