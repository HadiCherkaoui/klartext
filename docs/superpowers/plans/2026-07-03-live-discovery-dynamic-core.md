# M10: Live ECU Discovery + Fully-Dynamic Core Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking. Also REQUIRED before touching any `.rs` file: invoke the `ms-rust` skill.

**Goal:** Turn klartext's static, hardcoded, one-ECU-at-a-time diagnostic core into a fully live one — discover the car's actually-fitted ECUs, name them from data, read and clear faults across the whole car, resolve SGBD variants without the caller hardcoding them, and shut the MCP down cleanly.

**Architecture:** One TCP/HSFZ connection per gateway, demultiplexed so requests to different ECU addresses ride the same socket concurrently (responses routed by HSFZ source address). Fitted-ECU discovery is a bounded-concurrent `3E 00` probe sweep. Fault relevance is partitioned client-side by status mask. Whole-car read/clear are concrete orchestrations in `klartext-client`, shared by MCP and CLI. Variant resolution is a ladder: explicit → learned per-VIN profile → DB-unique → error-with-candidates.

**Tech Stack:** Rust 2024, tokio, rmcp 1.x, rusqlite (bundled), thiserror/anyhow, serde/serde_json, futures (new, for `buffer_unordered`).

## Global Constraints

- Latest stable Rust, edition 2024; async via tokio. (workspace already pins these — never hand-edit versions; use `cargo add`.)
- Errors: `thiserror` in library crates, `anyhow` at the binary boundary.
- `cargo fmt` and `cargo clippy --all-targets -- -D warnings` must be clean before a task is "done". Run `cargo fmt` **via Bash** (the Edit hook uses an older rustfmt — see memory `rustfmt-hook-mismatch`).
- Conventional commits.
- BYO-data: never commit BMW proprietary data (ISTA DBs, PSdZData, `.prg`, `.pcap`, anything with the VIN). `captures/`, `*.pcap`, `data/` are gitignored.
- Safety/blast-radius: reads run autonomously. The MCP exposes exactly ONE write — `clear_faults` / `clear_all_faults` (standard UDS 0x14, confirm-gated). NO physical actuation and NO derived-unconfirmed write frame is ever MCP-executable. Whole-car clear reads+records each ECU's faults before clearing.
- stdio MCP: nothing may write to stdout except the JSON-RPC stream. ALL logging → stderr.
- No speculative abstraction (CLAUDE.md `anti-overengineering`): no `Transport` trait, no generic guided-procedure engine, no SVT read — those are named future milestones.
- HIL: Claude cannot reach the car. Unit-test against byte vectors and loopback mock gateways only; never claim a hardware round-trip. New protocol assumptions (interleaved multi-target requests) are marked `[verify live]`.

---

## Task 1: DTC relevance mask (`klartext-uds`)

**Files:**
- Modify: `crates/uds/src/dtc.rs` (add to `status` mod + `Dtc` impl + tests)

**Interfaces:**
- Produces: `klartext_uds::dtc::status::RELEVANT_MASK: u8` (0xAF); `Dtc::is_relevant(self) -> bool`.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `crates/uds/src/dtc.rs`:

```rust
    #[test]
    fn relevant_mask_partitions_stored_faults_from_not_tested_noise() {
        use status::RELEVANT_MASK;
        // Relevant bits: testFailed|thisCycle|pending|confirmed|failedSinceClear|warning.
        assert_eq!(RELEVANT_MASK, 0xAF);
        // The two "not completed" bits are exactly the complement.
        assert_eq!(RELEVANT_MASK | 0x50, 0xFF);

        let confirmed = Dtc { code: [0, 0, 1], status: 0x08 };
        let failed = Dtc { code: [0, 0, 2], status: 0x01 };
        let warn = Dtc { code: [0, 0, 3], status: 0x80 };
        assert!(confirmed.is_relevant() && failed.is_relevant() && warn.is_relevant());

        // Catalog noise: only "not tested this / since" bits, or all-clear.
        let not_tested = Dtc { code: [0, 0, 4], status: 0x40 };
        let not_tested_since = Dtc { code: [0, 0, 5], status: 0x50 };
        let all_clear = Dtc { code: [0, 0, 6], status: 0x00 };
        assert!(!not_tested.is_relevant());
        assert!(!not_tested_since.is_relevant());
        assert!(!all_clear.is_relevant());
    }
```

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test -p klartext-uds relevant_mask 2>&1 | tail -15`
Expected: FAIL — `RELEVANT_MASK` / `is_relevant` not found.

- [ ] **Step 3: Implement**

In `crates/uds/src/dtc.rs`, add to `pub mod status` (after `WARNING_INDICATOR_REQUESTED`):

```rust
    /// Bits that mark a DTC as a *real* fault worth surfacing: any of testFailed
    /// (0x01), testFailedThisOperationCycle (0x02), pending (0x04), confirmed
    /// (0x08), testFailedSinceLastClear (0x20), warningIndicatorRequested (0x80).
    ///
    /// The complement (0x50 = testNotCompletedSinceLastClear |
    /// testNotCompletedThisOperationCycle) is "not tested this cycle" catalog
    /// noise: a `19 02 FF` scan of an idle ECU returns many such entries (the FEM
    /// returned ~147 with the engine off). A status of only those bits — or all
    /// zero — is not a stored fault.
    pub const RELEVANT_MASK: u8 = 0xAF;
```

Add to `impl Dtc` (after `warning_indicator_requested`):

```rust
    /// True if this DTC is a real fault worth surfacing (see [`status::RELEVANT_MASK`]).
    ///
    /// False for "not tested this cycle" catalog noise and an all-clear status.
    pub fn is_relevant(self) -> bool {
        self.status & status::RELEVANT_MASK != 0
    }
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo test -p klartext-uds 2>&1 | tail -8` — Expected: all pass.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo fmt -p klartext-uds
cargo clippy -p klartext-uds --all-targets -- -D warnings 2>&1 | tail -3
git add crates/uds/src/dtc.rs
git commit -m "feat(uds): add DTC relevance mask to split real faults from not-tested noise"
```

---

## Task 1b: Capture-verified 0x11 VIN parse + constant promotion (`klartext-hsfz`)

**Files:**
- Modify: `crates/hsfz/src/discover.rs` (fix `scan_vin`; add a marker-anchored parse + tests)
- Modify: `crates/hsfz/src/frame.rs`, `crates/hsfz/src/lib.rs` (promote confirmed `[verify against capture]` comments to `[verified 2026-07-03]`)

**Context:** The real capture (`captures/klartext-session-2026-07-03.pcap`, BYO/gitignored) was decoded with the Wireshark HSFZ dissector. The 0x11 announcement body is ASCII `DIAGADR<addr>BMWMAC<mac:12>BMWVIN<vin:17>`. The current `scan_vin` (first 17-char VIN-alphabet run) returns the **false** `AGADR10BMWMAC001A` from the `DIAGADR…MAC…` prefix instead of the real VIN. See design §6.

**Interfaces:**
- Produces: `discover::scan_vin` now prefers a `BMWVIN`-anchored parse, falling back to the run scan.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module of `crates/hsfz/src/discover.rs` (synthetic bytes mirroring the real layout — no real VIN):

```rust
    // The real 0x11 body shape: DIAGADR<addr>BMWMAC<mac>BMWVIN<vin>. The prefix
    // contains a 17-char VIN-alphabet run (AGADR10BMWMAC001A) that the naive scan
    // wrongly returns; the marker-anchored parse must return the true VIN.
    #[test]
    fn scan_vin_prefers_the_bmwvin_marker_over_a_false_prefix_run() {
        let mut datagram = vec![0x00, 0x00, 0x00, 0x32, 0x00, 0x11];
        datagram.extend_from_slice(b"DIAGADR10BMWMAC001A37265429BMWVINWBA3B5C50EK123456");
        assert_eq!(scan_vin(&datagram).as_deref(), Some("WBA3B5C50EK123456"));
    }
```

The two existing `scan_vin` tests (`finds_a_17_char_vin_in_the_body`, `returns_none_without_a_vin_run`) must still pass — the marker parse falls back to the run scan when no `BMWVIN` marker is present.

- [ ] **Step 2: Run test, verify it fails**

Run: `cargo test -p klartext-hsfz scan_vin 2>&1 | tail -12`
Expected: FAIL — returns `AGADR10BMWMAC001A`, not the VIN.

- [ ] **Step 3: Implement the marker-anchored parse**

In `crates/hsfz/src/discover.rs`, add a marker constant and rewrite `scan_vin`:

```rust
/// Marker preceding the VIN in the HSFZ 0x11 identification body — confirmed from
/// a real F20 announcement (2026-07-03): `DIAGADR<addr>BMWMAC<mac>BMWVIN<vin>`.
const VIN_MARKER: &[u8] = b"BMWVIN";

/// Extract the VIN from a 0x11 announcement body.
///
/// Prefers the confirmed layout: the 17 VIN-alphabet chars immediately after the
/// `BMWVIN` marker. Falls back to the first 17-char VIN-alphabet run when the
/// marker is absent (older/other announcements) — but the marker parse avoids the
/// false run inside the `DIAGADR…BMWMAC…` prefix (which is itself valid VIN chars).
fn scan_vin(bytes: &[u8]) -> Option<String> {
    // Marker-anchored: the 17 chars after "BMWVIN", if they are all VIN chars.
    if let Some(pos) = bytes
        .windows(VIN_MARKER.len())
        .position(|w| w == VIN_MARKER)
    {
        let start = pos + VIN_MARKER.len();
        if let Some(vin) = bytes.get(start..start + VIN_LEN)
            && vin.iter().all(|&b| is_vin_char(b))
        {
            return Some(String::from_utf8_lossy(vin).into_owned());
        }
    }
    // Fallback: first 17-char VIN-alphabet run anywhere in the body.
    bytes
        .windows(VIN_LEN)
        .find(|window| window.iter().all(|&b| is_vin_char(b)))
        .map(|window| String::from_utf8_lossy(window).into_owned())
}
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo test -p klartext-hsfz 2>&1 | tail -8` — Expected: all pass (new + the two existing scan_vin tests + discovery round-trip).

- [ ] **Step 5: Promote confirmed constants' comments**

Update comments where the capture confirmed the value (comment-only; no logic change):
- `crates/hsfz/src/frame.rs` module doc: the length-convention note — append `Confirmed against a real F20 capture 2026-07-03 (the VIN response LENGTH 0x16 = 2+3+17).` and change the trailing `[verify against capture]` on that paragraph to `[verified 2026-07-03]`.
- `crates/hsfz/src/lib.rs`: `DIAG_PORT` (TCP 6801) and `CONTROL_PORT` (6811) and `TESTER_ADDRESS` (0xF4) — change `[verify against capture]` to `[verified 2026-07-03]`. Leave `ZGW_ADDRESS` note as-is (0x10 was never directly addressed — see design §6).
- `crates/hsfz/src/discover.rs` module doc: note the 0x11 layout is now known (`DIAGADR<addr>BMWMAC<mac>BMWVIN<vin>`, verified 2026-07-03) and that the VIN is marker-anchored.

Do **not** touch DTC/`59 02` comments (`crates/uds/src/dtc.rs`) — the capture has no `0x19` traffic; that framing stays `[verify against capture]`.

- [ ] **Step 6: fmt + clippy + commit**

```bash
cargo fmt -p klartext-hsfz
cargo clippy -p klartext-hsfz --all-targets -- -D warnings 2>&1 | tail -3
git add crates/hsfz/src
git commit -m "fix(hsfz): anchor VIN parse on the BMWVIN marker; promote capture-verified constants"
```

---

## Task 2: DB-driven ECU naming (`build-semantic-db.sh` v2 + `klartext-semantic` catalog)

**Files:**
- Modify: `scripts/build-semantic-db.sh` (add title columns to the `ecu` extract)
- Modify: `crates/semantic/src/catalog.rs` (`EcuEntry`→`EcuSlot`, `ecus()`, new `variants()`, NULL/column hardening, fixtures)
- Modify: `crates/semantic/src/lib.rs` (re-exports)
- Modify: `mcp/src/ecu.rs`, `mcp/src/dto.rs`, `mcp/src/server.rs` only where they name `EcuEntry` (compile fix; full rework is Task 6)

**Interfaces:**
- Produces:
  - `EcuSlot { address: u8, group_name: String, extra_groups: Vec<String>, title: Option<String> }`
  - `VariantInfo { name: String, title: Option<String> }`
  - `Catalog::ecus(&self) -> Result<Vec<EcuSlot>, SemanticError>` (one entry per address; canonical group + extras aggregated in Rust)
  - `Catalog::variants(&self, address: u8) -> Result<Vec<VariantInfo>, SemanticError>`
- Consumes: none new.

- [ ] **Step 1: Update the extract script**

In `scripts/build-semantic-db.sh`, replace the `CREATE TABLE sem.ecu AS` statement with:

```sql
CREATE TABLE sem.ecu AS
  SELECT DISTINCT g.DIAGNOSTIC_ADDRESS AS address, v.NAME AS variant, g.NAME AS group_name,
         v.TITLE_ENGB AS title_en, v.TITLE_DEDE AS title_de
  FROM XEP_ECUVARIANTS v JOIN XEP_ECUGROUPS g ON g.ID = v.ECUGROUPID;
```

(Columns `TITLE_ENGB`/`TITLE_DEDE` were confirmed present on `XEP_ECUVARIANTS`. The old 3-column extract still loads — Step 4 detects columns at runtime.)

- [ ] **Step 2: Write failing tests**

Replace the existing `ecus_lists_distinct_addresses_ordered` test and the `fixture()` in `crates/semantic/src/catalog.rs` with the versions below, and add the new tests. First the fixture — support both new-schema and old-schema DBs:

```rust
    /// Build a synthetic semantic DB (no BMW data) matching the v2 extract schema
    /// (with title columns). `titles=false` reproduces a pre-v2 extract to prove
    /// backward compatibility.
    fn fixture_opts(titles: bool) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("semantic.db");
        let conn = Connection::open(&path).unwrap();
        let ecu_cols = if titles {
            "address INT, variant TEXT, group_name TEXT, title_en TEXT, title_de TEXT"
        } else {
            "address INT, variant TEXT, group_name TEXT"
        };
        conn.execute_batch(&format!(
            "CREATE TABLE dtc (address INT, ecu_variant TEXT, code INT, saecode TEXT, title_de TEXT, title_en TEXT);
             CREATE TABLE ecu ({ecu_cols});"
        )).unwrap();
        // dtc rows (unchanged from the prior fixture).
        conn.execute_batch(
            "INSERT INTO dtc VALUES (64,'variant_a',14222346,NULL,'BEISPIEL Fehler A','EXAMPLE fault A: powertrain bus, no communication');
             INSERT INTO dtc VALUES (64,'variant_b',14222346,NULL,'BEISPIEL Fehler B','EXAMPLE fault B: bus communication fault');
             INSERT INTO dtc VALUES (18,'variant_c',1234,'P0306','BEISPIEL Fehler C','EXAMPLE fault C: cylinder misfire');",
        ).unwrap();
        if titles {
            conn.execute_batch(
                "INSERT INTO ecu VALUES (16,'zgw_x','d_0010','Gateway','Gateway');
                 INSERT INTO ecu VALUES (18,'dde_a','d_0012','Digital Diesel Electronics','DDE');
                 INSERT INTO ecu VALUES (18,'dde_b','g_motor','Engine (group)','Motor');
                 INSERT INTO ecu VALUES (64,'fem_20','d_0040','Front Electronic Module','FEM');
                 INSERT INTO ecu VALUES (64,'fem_21','d_0040',NULL,NULL);
                 INSERT INTO ecu VALUES (NULL,'virtsg98','D_VIRT98','Virtual','Virtuell');",
            ).unwrap();
        } else {
            conn.execute_batch(
                "INSERT INTO ecu VALUES (16,'zgw_x','d_0010');
                 INSERT INTO ecu VALUES (18,'dde_a','d_0012');
                 INSERT INTO ecu VALUES (64,'fem_20','d_0040');
                 INSERT INTO ecu VALUES (NULL,'virtsg98','D_VIRT98');",
            ).unwrap();
        }
        (dir, path)
    }

    fn fixture() -> (TempDir, PathBuf) {
        fixture_opts(true)
    }
```

Now the tests:

```rust
    #[test]
    fn ecus_aggregates_by_address_with_canonical_group_and_title() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        let ecus = cat.ecus().unwrap();
        // NULL-address virtual SGBD skipped; one slot per address, ordered.
        assert_eq!(ecus.iter().map(|e| e.address).collect::<Vec<_>>(), [16, 18, 64]);
        // 0x12 has two groups; canonical is the d_00XX matching the address.
        let dde = ecus.iter().find(|e| e.address == 18).unwrap();
        assert_eq!(dde.group_name, "d_0012");
        assert_eq!(dde.extra_groups, ["g_motor"]);
        assert_eq!(dde.title.as_deref(), Some("Digital Diesel Electronics"));
    }

    #[test]
    fn variants_lists_candidates_for_an_address() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        let mut vs = cat.variants(0x12).unwrap();
        vs.sort_by(|a, b| a.name.cmp(&b.name));
        assert_eq!(vs.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(), ["dde_a", "dde_b"]);
        assert_eq!(vs[0].title.as_deref(), Some("Digital Diesel Electronics"));
    }

    #[test]
    fn ecus_works_on_a_pre_v2_extract_without_title_columns() {
        let (_dir, path) = fixture_opts(false);
        let cat = Catalog::open(&path).unwrap();
        let ecus = cat.ecus().unwrap();
        assert_eq!(ecus.iter().map(|e| e.address).collect::<Vec<_>>(), [16, 18, 64]);
        assert!(ecus.iter().all(|e| e.title.is_none()));
    }
```

Delete the old `ecus_lists_distinct_addresses_ordered` test (replaced). Update the two `#[ignore]`d real-DB tests to use `.address`/`EcuSlot` field names (they assert `ecus.len() > 3` — still valid).

- [ ] **Step 3: Run tests, verify they fail**

Run: `cargo test -p klartext-semantic 2>&1 | tail -20`
Expected: compile error (`EcuSlot`/`variants`/`extra_groups` unknown).

- [ ] **Step 4: Implement**

In `crates/semantic/src/catalog.rs`, replace the `EcuEntry` struct and the `ecus()` method with:

```rust
/// A diagnostic ECU slot: one address, its canonical ISTA group name, any other
/// group names ISTA records at that address, and a representative human title.
///
/// Sourced from ISTA's `XEP_ECUVARIANTS ⋈ XEP_ECUGROUPS` — the general BMW ECU
/// model, not specific to one car.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcuSlot {
    /// The diagnostic address (e.g. `0x12` for the engine).
    pub address: u8,
    /// The canonical ISTA group name — the `d_00XX` matching the address when
    /// present, else the first group seen.
    pub group_name: String,
    /// Other ISTA group names recorded at this address (e.g. `g_motor`).
    pub extra_groups: Vec<String>,
    /// A representative human title for the address, if the DB has one.
    pub title: Option<String>,
}

/// One ECU variant candidate for an address (for variant resolution + messages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantInfo {
    /// The SGBD variant name (the `.prg` stem), e.g. `d72n47a0`.
    pub name: String,
    /// The variant's human title, if the DB has one.
    pub title: Option<String>,
}

impl Catalog {
    /// Whether column `column` exists on `table` (for pre-v2 extract compatibility).
    fn has_column(&self, table: &str, column: &str) -> Result<bool, SemanticError> {
        let mut stmt = self
            .conn
            .prepare(&format!("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2"))?;
        Ok(stmt.exists((table, column))?)
    }

    /// List the fitted-independent ECU map: one [`EcuSlot`] per diagnostic address.
    ///
    /// Aggregates ISTA's many per-address variants/groups in Rust: the canonical
    /// group is the `d_00XX` whose hex equals the address (else the first). NULL
    /// addresses (ISTA virtual/internal SGBDs) are skipped so one cannot abort the
    /// query. Titles come back `None` on a pre-v2 extract without the columns.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup fails.
    pub fn ecus(&self) -> Result<Vec<EcuSlot>, SemanticError> {
        let has_titles = self.has_column("ecu", "title_en")?;
        let sql = if has_titles {
            "SELECT DISTINCT address, group_name, title_en, title_de FROM ecu \
             WHERE address IS NOT NULL ORDER BY address, group_name"
        } else {
            "SELECT DISTINCT address, group_name, NULL, NULL FROM ecu \
             WHERE address IS NOT NULL ORDER BY address, group_name"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            let address: u8 = row.get(0)?;
            let group_name: String = row.get(1)?;
            let title_en: Option<String> = row.get(2)?;
            let title_de: Option<String> = row.get(3)?;
            Ok((address, group_name, title_en.or(title_de)))
        })?;

        // Aggregate by address, preserving first-seen order.
        let mut slots: Vec<EcuSlot> = Vec::new();
        for row in rows {
            let (address, group_name, title) = row?;
            match slots.iter_mut().find(|s| s.address == address) {
                Some(slot) => {
                    slot.extra_groups.push(group_name);
                    if slot.title.is_none() {
                        slot.title = title;
                    }
                }
                None => slots.push(EcuSlot {
                    address,
                    group_name,
                    extra_groups: Vec::new(),
                    title,
                }),
            }
        }
        // Choose the canonical group per address: prefer d_00XX matching the address.
        for slot in &mut slots {
            let canonical = format!("d_{:04x}", slot.address);
            if slot.group_name != canonical
                && let Some(pos) = slot.extra_groups.iter().position(|g| *g == canonical)
            {
                let promoted = slot.extra_groups.remove(pos);
                slot.extra_groups.push(std::mem::replace(&mut slot.group_name, promoted));
            }
        }
        Ok(slots)
    }

    /// List the ECU variant candidates for a diagnostic `address`.
    ///
    /// Used by the variant-resolution ladder and to make "which variant?" errors
    /// actionable. Empty when the address is unknown.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup fails.
    pub fn variants(&self, address: u8) -> Result<Vec<VariantInfo>, SemanticError> {
        let has_titles = self.has_column("ecu", "title_en")?;
        let sql = if has_titles {
            "SELECT DISTINCT variant, title_en, title_de FROM ecu WHERE address = ?1 ORDER BY variant"
        } else {
            "SELECT DISTINCT variant, NULL, NULL FROM ecu WHERE address = ?1 ORDER BY variant"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([i64::from(address)], |row| {
            let name: String = row.get(0)?;
            let title_en: Option<String> = row.get(1)?;
            let title_de: Option<String> = row.get(2)?;
            Ok(VariantInfo { name, title: title_en.or(title_de) })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}
```

Keep the existing `describe_dtc` method as-is (it already NULL-hardens via its WHERE). Remove the now-unused `EcuEntry` import sites.

In `crates/semantic/src/lib.rs`, update the re-export: replace `EcuEntry` with `EcuSlot, VariantInfo` in the `pub use catalog::{...}` line.

- [ ] **Step 5: Fix compile in `mcp` (minimal — full rework is Task 6)**

`mcp/src/ecu.rs` `list()` uses `entry.group_name`/`entry.address`; adjust the field accesses so it compiles against `EcuSlot` (temporary: push `group_name` into `names`, ignore `extra_groups`/`title` for now — Task 6 rewrites this). Change `catalog.ecus()` iteration `for entry in entries` body to use `entry.address` and `entry.group_name` (unchanged names) — it already does. It only needs the type to resolve; if the code references `EcuEntry` by name anywhere, replace with `EcuSlot`.

- [ ] **Step 6: Run tests + real-DB smoke (rebuild DB first)**

```bash
bash scripts/build-semantic-db.sh                 # rebuilds data/klartext-semantic.db (BYO DiagDocDb)
cargo test -p klartext-semantic 2>&1 | tail -10
cargo test -p klartext-semantic -- --ignored 2>&1 | tail -10
```
Expected: unit tests pass; the two `#[ignore]`d real-DB tests pass against the rebuilt DB.

- [ ] **Step 7: fmt + clippy + commit**

```bash
cargo fmt -p klartext-semantic
cargo clippy -p klartext-semantic --all-targets -- -D warnings 2>&1 | tail -3
git add scripts/build-semantic-db.sh crates/semantic/src/catalog.rs crates/semantic/src/lib.rs mcp/src/ecu.rs
git commit -m "feat(semantic): DB-driven ECU slots + per-address variant candidates"
```

---

## Task 3: Demultiplexed multi-target session (`klartext-client`)

**Files:**
- Modify: `crates/client/src/session.rs` (full rewrite of `Session`)
- Modify: `crates/client/src/error.rs` (add variants)
- Modify: `crates/client/Cargo.toml` (add `futures` — via `cargo add`)

**Interfaces:**
- Produces:
  - `Session::open(conn: HsfzConnection, source: u8, gateway: u8) -> Session` (keepalive targets `gateway`)
  - `Session::request(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ClientError>` (uses the connection's default read timeout)
  - `Session::request_with_timeout(&self, target: u8, uds: &[u8], timeout: Duration) -> Result<Vec<u8>, ClientError>`
  - `Session::enter_session(&self, target: u8, session: u8) -> Result<(), ClientError>`
  - `ClientError::ConnectionClosed` and `ClientError::RequestInFlight { target: u8 }`
- Consumes: `klartext_hsfz::{HsfzConnection, HsfzFrame, control, read_frame, write_frame}`; `klartext_uds` as before.

Note: `request` takes `&self` (was `&mut self`) so different targets run concurrently over one socket.

- [ ] **Step 1: Add the `futures` dependency**

```bash
cargo add futures -p klartext-client
```
Expected: `futures` added to `crates/client/Cargo.toml` at its current version.

- [ ] **Step 2: Add error variants**

In `crates/client/src/error.rs`, add to `ClientError`:

```rust
    /// The demuxed reader task ended (the gateway closed the connection or a read
    /// failed) while a request was waiting — the session is no longer usable.
    #[error("HSFZ connection closed while awaiting a response")]
    ConnectionClosed,
    /// A second request was issued to a target that already has one in flight.
    /// klartext issues at most one request per target at a time.
    #[error("a request to ECU 0x{target:02X} is already in flight")]
    RequestInFlight { target: u8 },
```

- [ ] **Step 3: Write the failing tests**

Replace the `tests` module of `crates/client/src/session.rs`. Keep the two `read_matching`-style intents as behavior tests through the public API. New mock supports multiple targets and per-target latency:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use klartext_hsfz::{HsfzConnection, HsfzFrame, control, read_frame, write_frame};
    use tokio::net::TcpListener;

    /// A loopback gateway hosting several mock ECUs keyed by target address.
    /// `present` addresses answer `22 F1 90` with a per-address VIN byte and any
    /// `3E 00` with `7E 00`; absent addresses never reply. Counts keepalives.
    async fn spawn_multi_ecu_gateway(present: &[u8]) -> (std::net::SocketAddr, Arc<AtomicUsize>) {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let present: Vec<u8> = present.to_vec();
        let keepalives = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&keepalives);
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC {
                    continue;
                }
                let (src, tgt) = frame.addr.unwrap(); // tester -> ecu
                let ecu = tgt;
                if frame.payload == [0x3E, 0x80] {
                    counter.fetch_add(1, Ordering::SeqCst); // keepalive
                    continue;
                }
                if !present.contains(&ecu) {
                    continue; // absent ECU: silence
                }
                let uds = match frame.payload.as_slice() {
                    [0x3E, 0x00] => vec![0x7E, 0x00],
                    [0x22, 0xF1, 0x90] => vec![0x62, 0xF1, 0x90, ecu], // 1-byte "VIN"
                    [0x19, 0x02, _] => vec![0x59, 0x02, 0xFF, ecu, 0x00, 0x00, 0x08],
                    _ => continue,
                };
                // reply: swap src/tgt (ecu -> tester)
                let reply = HsfzFrame::diagnostic(ecu, src, uds);
                let _ = write_frame(&mut stream, &reply).await;
            }
        });
        (addr, keepalives)
    }

    async fn open_session(addr: std::net::SocketAddr) -> Session {
        let conn = HsfzConnection::connect(addr.ip(), addr.port(), Duration::from_secs(2), Duration::from_secs(5))
            .await
            .unwrap();
        Session::start(conn, 0xF4, 0x10, Duration::from_millis(50))
    }

    #[tokio::test]
    async fn routes_responses_to_the_right_target() {
        let (addr, _) = spawn_multi_ecu_gateway(&[0x12, 0x40]).await;
        let session = open_session(addr).await;
        let a = session.request(0x12, &[0x22, 0xF1, 0x90]).await.unwrap();
        let b = session.request(0x40, &[0x22, 0xF1, 0x90]).await.unwrap();
        assert_eq!(a, vec![0x62, 0xF1, 0x90, 0x12]);
        assert_eq!(b, vec![0x62, 0xF1, 0x90, 0x40]);
    }

    #[tokio::test]
    async fn concurrent_requests_to_distinct_targets_share_one_socket() {
        let (addr, _) = spawn_multi_ecu_gateway(&[0x12, 0x40, 0x60]).await;
        let session = open_session(addr).await;
        let (a, b, c) = tokio::join!(
            session.request(0x12, &[0x22, 0xF1, 0x90]),
            session.request(0x40, &[0x22, 0xF1, 0x90]),
            session.request(0x60, &[0x22, 0xF1, 0x90]),
        );
        assert_eq!(a.unwrap()[3], 0x12);
        assert_eq!(b.unwrap()[3], 0x40);
        assert_eq!(c.unwrap()[3], 0x60);
    }

    #[tokio::test]
    async fn absent_target_times_out_without_blocking_others() {
        let (addr, _) = spawn_multi_ecu_gateway(&[0x12]).await;
        let session = open_session(addr).await;
        // Absent 0x18 times out fast; 0x12 still answers.
        let absent = session
            .request_with_timeout(0x18, &[0x3E, 0x00], Duration::from_millis(150))
            .await;
        assert!(matches!(absent, Err(ClientError::Hsfz(klartext_hsfz::Error::ReadTimeout { .. }))));
        let present = session.request(0x12, &[0x3E, 0x00]).await.unwrap();
        assert_eq!(present, vec![0x7E, 0x00]);
    }

    #[tokio::test]
    async fn keepalive_targets_the_gateway_during_idle() {
        let (addr, keepalives) = spawn_multi_ecu_gateway(&[0x12]).await;
        let session = open_session(addr).await;
        let _ = session.request(0x12, &[0x22, 0xF1, 0x90]).await.unwrap();
        tokio::time::sleep(Duration::from_millis(180)).await;
        assert!(keepalives.load(Ordering::SeqCst) >= 1, "keepalive should fire when idle");
    }
}
```

- [ ] **Step 4: Run tests, verify they fail**

Run: `cargo test -p klartext-client session 2>&1 | tail -20`
Expected: compile error (`request` signature / `Session::start` mismatch).

- [ ] **Step 5: Rewrite `Session`**

Replace everything above the `#[cfg(test)]` in `crates/client/src/session.rs` with:

```rust
//! A managed, demultiplexed UDS session over one HSFZ connection.
//!
//! One TCP/HSFZ connection to the gateway carries requests to *many* ECU
//! addresses. A background reader task owns the read half and routes each
//! response frame to the pending request for that frame's **source address**
//! (HSFZ frames carry SRC/TGT both ways), so requests to different targets can
//! be in flight at once over the single socket. At most one request per target
//! is outstanding at a time. A second background task sends the TesterPresent
//! keepalive (`3E 80`) to the gateway so the link never lapses.
//!
//! Routing by source address (not merely by SID, as the single-target M2 code
//! did) also means a late response from a timed-out probe can no longer be
//! mis-attributed to a later request. The real F20 capture confirms the property
//! this relies on: a request `f4 12` draws a response `12 f4` — the answering
//! ECU's address is the HSFZ source (verified 2026-07-03). What stays [verify
//! live] is whether the ZGW tolerates *interleaved* requests to different targets;
//! the pcap is lockstep, and a scan concurrency of 1 degrades this to sequential.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use klartext_hsfz::{HsfzConnection, HsfzFrame, control, read_frame, write_frame};
use klartext_uds::{
    NRC_RESPONSE_PENDING, Nrc, positive_response_sid, sid, tester_present_suppressed,
};
use tokio::net::tcp::OwnedWriteHalf;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::error::ClientError;

/// How often the background keepalive sends `3E 80` to the gateway.
///
/// Comfortably under the S3 inactivity timeout (~5 s, report §1.4). [verify against capture].
pub const KEEPALIVE_INTERVAL: Duration = Duration::from_secs(2);

/// Max NRC 0x78 "response pending" ticks for one request before giving up.
const MAX_PENDING_TICKS: u32 = 10;

/// What the reader delivers to a waiting request.
enum Delivery {
    /// The final response payload (positive, or our own non-0x78 negative kept
    /// as an error by the waiter).
    Final(Result<Vec<u8>, ClientError>),
    /// An NRC 0x78 for this target: keep waiting, re-arm the timeout.
    Pending,
}

/// Per-target pending slot: where the reader sends deliveries for one request.
type Pending = Arc<Mutex<HashMap<u8, mpsc::UnboundedSender<Delivery>>>>;

/// A held, demuxed UDS session: concurrent per-target requests + a keepalive.
#[derive(Debug)]
pub struct Session {
    write: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    pending: Pending,
    reader: JoinHandle<()>,
    keepalive: JoinHandle<()>,
    source: u8,
    read_timeout: Duration,
}

impl Drop for Session {
    fn drop(&mut self) {
        self.reader.abort();
        self.keepalive.abort();
    }
}

impl Session {
    /// Open a managed session over `conn`; the keepalive targets `gateway`.
    pub fn open(conn: HsfzConnection, source: u8, gateway: u8) -> Self {
        Self::start(conn, source, gateway, KEEPALIVE_INTERVAL)
    }

    /// Open with an explicit keepalive interval (tests run it fast).
    fn start(conn: HsfzConnection, source: u8, gateway: u8, interval: Duration) -> Self {
        let (mut read, write, _peer, read_timeout) = conn.into_parts();
        let write = Arc::new(tokio::sync::Mutex::new(write));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));

        let reader_pending = Arc::clone(&pending);
        let reader = tokio::spawn(async move {
            loop {
                // No overall timeout on the reader: individual requests time out
                // themselves. A very long read_timeout bounds a truly dead link.
                match read_frame(&mut read, Duration::from_secs(3600)).await {
                    Ok(frame) => route_frame(&reader_pending, frame),
                    Err(_) => break, // connection closed / fatal read error
                }
            }
            // Fail every waiter so no request hangs forever.
            let mut map = reader_pending.lock().unwrap();
            for (_target, tx) in map.drain() {
                let _ = tx.send(Delivery::Final(Err(ClientError::ConnectionClosed)));
            }
        });

        let keepalive = spawn_keepalive(Arc::clone(&write), source, gateway, interval);
        Self { write, pending, reader, keepalive, source, read_timeout }
    }

    /// Send a UDS request to `target` and return its response payload, using the
    /// connection's default read timeout.
    ///
    /// # Errors
    /// [`ClientError::RequestInFlight`] if `target` already has a request pending,
    /// [`ClientError::Hsfz`] on a transport/timeout error,
    /// [`ClientError::ConnectionClosed`] if the reader ended, and
    /// [`ClientError::Negative`] if the ECU rejects the request.
    pub async fn request(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ClientError> {
        self.request_with_timeout(target, uds, self.read_timeout).await
    }

    /// As [`Session::request`], but with an explicit per-request read timeout
    /// (used by fast presence probes).
    pub async fn request_with_timeout(
        &self,
        target: u8,
        uds: &[u8],
        timeout: Duration,
    ) -> Result<Vec<u8>, ClientError> {
        let request_sid = uds.first().copied().unwrap_or_default();
        let (tx, mut rx) = mpsc::unbounded_channel();
        // Register the pending slot; reject a second in-flight request per target.
        {
            let mut map = self.pending.lock().unwrap();
            if map.contains_key(&target) {
                return Err(ClientError::RequestInFlight { target });
            }
            map.insert(target, tx);
        }
        // Send the frame. On write failure, clear the slot and surface the error.
        let frame = HsfzFrame::diagnostic(self.source, target, uds.to_vec());
        if let Err(e) = {
            let mut writer = self.write.lock().await;
            write_frame(&mut *writer, &frame).await
        } {
            self.pending.lock().unwrap().remove(&target);
            return Err(e.into());
        }
        // Await delivery; NRC 0x78 re-arms the timeout, bounded by MAX_PENDING_TICKS.
        let _ = request_sid; // reader validates the SID via positive_response_sid
        let mut ticks = 0u32;
        loop {
            match tokio::time::timeout(timeout, rx.recv()).await {
                Ok(Some(Delivery::Final(result))) => {
                    self.pending.lock().unwrap().remove(&target);
                    return result;
                }
                Ok(Some(Delivery::Pending)) => {
                    ticks += 1;
                    if ticks > MAX_PENDING_TICKS {
                        self.pending.lock().unwrap().remove(&target);
                        return Err(ClientError::Hsfz(klartext_hsfz::Error::ReadTimeout { timeout }));
                    }
                    continue;
                }
                Ok(None) => {
                    self.pending.lock().unwrap().remove(&target);
                    return Err(ClientError::ConnectionClosed);
                }
                Err(_) => {
                    self.pending.lock().unwrap().remove(&target);
                    return Err(ClientError::Hsfz(klartext_hsfz::Error::ReadTimeout { timeout }));
                }
            }
        }
    }

    /// Move `target` into `session` (e.g. extended) via DiagnosticSessionControl.
    ///
    /// # Errors
    /// As [`Session::request`]; a rejected change surfaces as [`ClientError::Negative`].
    pub async fn enter_session(&self, target: u8, session: u8) -> Result<(), ClientError> {
        self.request(target, &klartext_uds::diagnostic_session_control(session))
            .await?;
        Ok(())
    }
}

/// Route one received frame to the pending request for its source address.
///
/// The `request_sid` the waiter expects is recovered from the frame: a positive
/// response's SID minus 0x40, or the echoed SID in a `7F <sid> <nrc>` negative.
/// A frame with no matching pending target (a stray/late reply) is dropped.
fn route_frame(pending: &Pending, frame: HsfzFrame) {
    if frame.control != control::DIAGNOSTIC {
        return; // ack / keepalive echo / other
    }
    let Some((src, _tgt)) = frame.addr else { return };
    let payload = frame.payload;
    // Is there a waiter for this source ECU?
    let tx = {
        let map = pending.lock().unwrap();
        map.get(&src).cloned()
    };
    let Some(tx) = tx else { return }; // stray/late — drop

    match payload.first().copied() {
        Some(sid::NEGATIVE_RESPONSE) => {
            let nrc = payload.get(2).copied().unwrap_or_default();
            let echoed = payload.get(1).copied().unwrap_or_default();
            if nrc == NRC_RESPONSE_PENDING {
                let _ = tx.send(Delivery::Pending);
            } else {
                let _ = tx.send(Delivery::Final(Err(ClientError::Negative {
                    sid: echoed,
                    nrc: Nrc::from(nrc),
                })));
            }
        }
        Some(_) => {
            // Positive (or unexpected) — deliver the payload; the caller decodes it.
            let _ = tx.send(Delivery::Final(Ok(payload)));
        }
        None => {
            let _ = tx.send(Delivery::Final(Err(ClientError::ConnectionClosed)));
        }
    }
    let _ = positive_response_sid; // kept for symmetry with the M2 decoder
}

/// Spawn the background keepalive: send `3E 80` to `gateway` every `interval`.
fn spawn_keepalive(
    write: Arc<tokio::sync::Mutex<OwnedWriteHalf>>,
    source: u8,
    gateway: u8,
    interval: Duration,
) -> JoinHandle<()> {
    let frame = HsfzFrame::diagnostic(source, gateway, tester_present_suppressed());
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(interval);
        ticker.tick().await; // consume the immediate first tick
        loop {
            ticker.tick().await;
            let mut writer = write.lock().await;
            if write_frame(&mut *writer, &frame).await.is_err() {
                break;
            }
        }
    })
}
```

Note the `positive_response_sid`/`request_sid` "kept" lines are placeholders to avoid unused-import churn while writing; **delete both** and the `positive_response_sid` import if clippy flags them unused. (The reader matches purely by source address + negative-SID echo, which is sufficient and simpler than SID matching.)

- [ ] **Step 6: Run tests, verify pass**

Run: `cargo test -p klartext-client session 2>&1 | tail -20`
Expected: the four session tests pass. If `positive_response_sid`/`request_sid` are unused, remove them and re-run.

- [ ] **Step 7: fmt + clippy + commit** (client won't fully compile until Task 4 updates `client.rs`; commit together with Task 4). Skip the commit here; proceed to Task 4.

---

## Task 4: Target-parameterized client methods + presence probe (`klartext-client`)

**Files:**
- Modify: `crates/client/src/client.rs` (methods take `target`; drop `ecu` from `ClientConfig`; add `probe`)
- Modify: `crates/client/src/lib.rs` (re-exports: add `Probe`, `ProbeResult`)

**Interfaces:**
- Produces:
  - `ClientConfig { port, tester, connect_timeout, read_timeout }` (no `ecu`)
  - `DiagnosticClient::read_dtcs(&self, target: u8, mask: u8) -> Result<Vec<Dtc>, ClientError>`
  - `read_all_dtcs(&self, target)`, `read_did(&self, target, did)`, `clear_all_dtcs(&self, target)`, `clear_dtcs(&self, target, dtc)`, `tester_present(&self, target)`, `read_dynamic_measurement(&self, target, requests)`, `reset_cbs(&self, target, ..)`, `run_service_reset(&self, target, ..)`
  - `probe(&self, target: u8, timeout: Duration) -> Result<ProbeOutcome, ClientError>`
  - `enum ProbeOutcome { Present { answered_positive: bool, latency: Duration }, Silent }`
- Consumes: Task 3's `Session::request(&self, target, uds)` etc.

- [ ] **Step 1: Write failing tests**

The existing `client.rs` tests already spawn per-purpose gateways; update them to the new signatures and add the probe test. Replace `spawn_dde_gateway`/`spawn_cbs_gateway`/`spawn_reset_gateway` call sites so requests pass a target, and add:

```rust
    #[tokio::test]
    async fn probe_reports_present_on_positive_and_silent_on_timeout() {
        // Reuse the DDE gateway: it answers 3E 00 on 0x12 (present) but nothing on 0x18.
        let addr = spawn_dde_gateway().await;
        let config = ClientConfig { port: addr.port(), ..ClientConfig::default() };
        let client = DiagnosticClient::connect(addr.ip(), &config).await.unwrap();

        let present = client.probe(0x12, Duration::from_millis(300)).await.unwrap();
        assert!(matches!(present, ProbeOutcome::Present { answered_positive: true, .. }));

        let silent = client.probe(0x18, Duration::from_millis(150)).await.unwrap();
        assert!(matches!(silent, ProbeOutcome::Silent));
    }
```

Extend `spawn_dde_gateway` to also answer `[0x3E, 0x00] => vec![0x7E, 0x00]` for target `0x12`, and to ignore requests to other targets (so 0x18 stays silent). It must read `frame.addr` and only reply for `ecu == 0x12`.

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p klartext-client 2>&1 | tail -20` — Expected: compile errors (signatures).

- [ ] **Step 3: Implement**

In `crates/client/src/client.rs`:

1. Remove `ecu` from `ClientConfig` and its `Default`. Update `connect` and `discover_and_connect` to open the session with the ZGW as the keepalive target: `Session::open(conn, config.tester, ZGW_ADDRESS)`.

2. Change every service method to take `&self` + `target: u8` and pass it through. Example replacements:

```rust
    pub async fn read_dtcs(&self, target: u8, mask: u8) -> Result<Vec<Dtc>, ClientError> {
        let response = self.session.request(target, &read_dtc_by_status_mask(mask)).await?;
        Ok(decode_dtcs(&response)?)
    }

    pub async fn read_all_dtcs(&self, target: u8) -> Result<Vec<Dtc>, ClientError> {
        self.read_dtcs(target, ALL_DTC_STATUS_MASK).await
    }

    pub async fn read_did(&self, target: u8, did: u16) -> Result<(u16, Vec<u8>), ClientError> {
        let response = self.session.request(target, &read_data_by_identifier(did)).await?;
        Ok(decode_read_data_by_identifier(&response)?)
    }

    pub async fn clear_dtcs(&self, target: u8, dtc: [u8; 3]) -> Result<(), ClientError> {
        self.session.enter_session(target, session::EXTENDED).await?;
        self.session.request(target, &clear_diagnostic_information(dtc)).await?;
        Ok(())
    }

    pub async fn clear_all_dtcs(&self, target: u8) -> Result<(), ClientError> {
        self.clear_dtcs(target, CLEAR_ALL_DTCS).await
    }

    pub async fn tester_present(&self, target: u8) -> Result<(), ClientError> {
        self.session.request(target, &tester_present()).await?;
        Ok(())
    }
```

Apply the same `&self, target` change to `read_dynamic_measurement`, `reset_cbs`, and `run_service_reset` (each threads `target` into its `session.request(...)`/`enter_session(...)` calls).

3. Add the probe:

```rust
    /// The outcome of a presence probe against one ECU address.
    // (defined at module level — see below)

    /// Probe whether `target` is fitted, with a short per-probe `timeout`.
    ///
    /// Sends a side-effect-free TesterPresent (`3E 00`). *Any* answer — a positive
    /// `7E 00` or a negative response — proves the ECU is present and routing to it
    /// works; a read timeout means it is absent (or asleep). Only a fatal transport
    /// error (a closed connection) is returned as `Err`, so a whole-car scan can
    /// treat a `Silent` result as "not fitted" without aborting.
    ///
    /// # Errors
    /// [`ClientError::ConnectionClosed`] if the session's connection dropped.
    pub async fn probe(&self, target: u8, timeout: Duration) -> Result<ProbeOutcome, ClientError> {
        let started = tokio::time::Instant::now();
        match self.session.request_with_timeout(target, &tester_present(), timeout).await {
            Ok(_) => Ok(ProbeOutcome::Present { answered_positive: true, latency: started.elapsed() }),
            Err(ClientError::Negative { .. }) => {
                Ok(ProbeOutcome::Present { answered_positive: false, latency: started.elapsed() })
            }
            Err(ClientError::Hsfz(klartext_hsfz::Error::ReadTimeout { .. })) => Ok(ProbeOutcome::Silent),
            Err(ClientError::RequestInFlight { .. }) => Ok(ProbeOutcome::Silent),
            Err(other) => Err(other),
        }
    }
```

Add the module-level type (near `ClientConfig`):

```rust
/// The outcome of [`DiagnosticClient::probe`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeOutcome {
    /// The ECU answered — it is fitted and reachable.
    Present {
        /// True if it answered positively (`7E 00`); false if it answered with a
        /// negative response (still proves presence).
        answered_positive: bool,
        /// How long the answer took.
        latency: Duration,
    },
    /// No answer within the probe timeout — treat as not fitted.
    Silent,
}
```

Add `use std::time::Duration;` if not present (it is). Update `crates/client/src/lib.rs` to `pub use client::{ClientConfig, DiagnosticClient, ProbeOutcome, ...};`.

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo test -p klartext-client 2>&1 | tail -15` — Expected: all client tests pass (session + client + probe).

- [ ] **Step 5: fmt + clippy + commit** (still won't link into cli/mcp until Tasks 6–11; that's fine — the crate compiles and tests pass)

```bash
cargo fmt -p klartext-client
cargo clippy -p klartext-client --all-targets -- -D warnings 2>&1 | tail -3
git add crates/client
git commit -m "feat(client): demuxed multi-target session + per-target methods + presence probe"
```

---

## Task 5: Whole-car scan + verified clear orchestrations (`klartext-client`)

**Files:**
- Create: `crates/client/src/scan.rs`
- Modify: `crates/client/src/lib.rs` (module + re-exports)

**Interfaces:**
- Consumes: `DiagnosticClient::{probe, read_dtcs, clear_all_dtcs}`, `Dtc`, `Dtc::is_relevant`.
- Produces:
  - `struct ScanOptions { probe_timeout: Duration, concurrency: usize }` + `Default` (300 ms, 8)
  - `struct FittedEcu { address: u8, latency: Duration }`
  - `struct EcuFaults { address: u8, relevant: Vec<Dtc>, not_tested: usize, error: Option<String> }`
  - `struct ClearReport { address: u8, before: Vec<Dtc>, after_relevant: Vec<Dtc>, verified_clean: bool, error: Option<String> }`
  - `impl DiagnosticClient { async fn scan_present(&self, addrs: &[u8], opts: ScanOptions) -> Vec<FittedEcu>; async fn scan_faults(&self, addrs: &[u8], opts: ScanOptions) -> Vec<EcuFaults>; async fn clear_faults_verified(&self, target: u8) -> ClearReport; async fn clear_faults_all(&self, addrs: &[u8]) -> Vec<ClearReport> }`

- [ ] **Step 1: Write failing tests**

Create `crates/client/src/scan.rs` with a tests module using a multi-ECU mock (reuse the pattern from `session.rs` tests; the mock lives in this file's tests):

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;
    use std::time::Duration;

    use klartext_hsfz::{HsfzConnection, HsfzFrame, control, read_frame, write_frame};
    use tokio::net::TcpListener;

    use crate::{ClientConfig, DiagnosticClient};

    /// Loopback gateway: `present` ECUs answer 3E00, 1902 (one confirmed + one
    /// not-tested DTC), 14 clear (then read clean), and the extended-session 1003.
    async fn spawn(present: &[u8]) -> std::net::SocketAddr {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let addr = listener.local_addr().unwrap();
        let present: Vec<u8> = present.to_vec();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            // per-ECU "cleared" state
            let mut cleared: std::collections::HashSet<u8> = Default::default();
            while let Ok(frame) = read_frame(&mut stream, Duration::from_secs(5)).await {
                if frame.control != control::DIAGNOSTIC { continue; }
                let (src, ecu) = frame.addr.unwrap();
                if frame.payload == [0x3E, 0x80] { continue; }
                if !present.contains(&ecu) { continue; }
                let uds = match frame.payload.as_slice() {
                    [0x3E, 0x00] => vec![0x7E, 0x00],
                    [0x10, 0x03] => vec![0x50, 0x03, 0x00, 0x32, 0x13, 0x88],
                    [0x14, 0xFF, 0xFF, 0xFF] => { cleared.insert(ecu); vec![0x54] }
                    [0x19, 0x02, _] if cleared.contains(&ecu) => vec![0x59, 0x02, 0xFF],
                    [0x19, 0x02, _] => vec![
                        0x59, 0x02, 0xFF,
                        0x00, 0x00, 0x01, 0x08, // confirmed (relevant)
                        0x00, 0x00, 0x02, 0x40, // not tested this cycle (noise)
                    ],
                    _ => continue,
                };
                let _ = write_frame(&mut stream, &HsfzFrame::diagnostic(ecu, src, uds)).await;
            }
        });
        addr
    }

    async fn client(addr: std::net::SocketAddr) -> DiagnosticClient {
        let config = ClientConfig { port: addr.port(), ..ClientConfig::default() };
        DiagnosticClient::connect(addr.ip(), &config).await.unwrap()
    }

    #[tokio::test]
    async fn scan_present_finds_only_fitted_ecus() {
        let addr = spawn(&[0x10, 0x12, 0x40]).await;
        let client = client(addr).await;
        let opts = ScanOptions { probe_timeout: Duration::from_millis(200), concurrency: 4 };
        let fitted = client.scan_present(&[0x10, 0x12, 0x18, 0x40, 0x60], opts).await;
        let mut addrs: Vec<u8> = fitted.iter().map(|f| f.address).collect();
        addrs.sort_unstable();
        assert_eq!(addrs, [0x10, 0x12, 0x40]);
    }

    #[tokio::test]
    async fn scan_faults_partitions_relevant_from_not_tested() {
        let addr = spawn(&[0x12]).await;
        let client = client(addr).await;
        let opts = ScanOptions { probe_timeout: Duration::from_millis(200), concurrency: 4 };
        let faults = client.scan_faults(&[0x12, 0x18], opts).await;
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].address, 0x12);
        assert_eq!(faults[0].relevant.len(), 1);
        assert_eq!(faults[0].not_tested, 1);
        assert!(faults[0].error.is_none());
    }

    #[tokio::test]
    async fn clear_faults_verified_reads_clears_and_confirms_clean() {
        let addr = spawn(&[0x12]).await;
        let client = client(addr).await;
        let report = client.clear_faults_verified(0x12).await;
        assert_eq!(report.before.len(), 2); // 2 stored before (all statuses)
        assert!(report.after_relevant.is_empty());
        assert!(report.verified_clean);
        assert!(report.error.is_none());
    }
}
```

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p klartext-client scan 2>&1 | tail -20` — Expected: `scan.rs` module unknown / types missing.

- [ ] **Step 3: Implement `scan.rs`**

```rust
//! Whole-car orchestrations layered on the demuxed client: fitted-ECU discovery
//! (bounded-concurrent presence probes), whole-car fault reads (present → read →
//! partition relevant vs not-tested), and a verified whole-car clear (per ECU:
//! pre-read → extended session → standard `14 FF FF FF` → post-read verify).
//!
//! These are concrete procedures, not a general guided-procedure engine (that is
//! a named future milestone). Reads are autonomous-safe and fan out concurrently;
//! the clear is a state change, stays strictly sequential, and records each ECU's
//! stored faults before erasing them.

use std::time::Duration;

use futures::stream::{self, StreamExt};
use klartext_uds::Dtc;

use crate::client::{DiagnosticClient, ProbeOutcome};
use crate::error::ClientError;

/// Tuning for a whole-car scan.
#[derive(Debug, Clone, Copy)]
pub struct ScanOptions {
    /// Per-ECU presence-probe timeout. An absent ECU costs at most this, not the
    /// full read timeout — so a scan never hangs on a missing module.
    pub probe_timeout: Duration,
    /// How many ECUs to probe/read at once over the single connection.
    /// `1` = strictly sequential (the safe fallback if the ZGW dislikes overlap).
    pub concurrency: usize,
}

impl Default for ScanOptions {
    fn default() -> Self {
        Self { probe_timeout: Duration::from_millis(300), concurrency: 8 }
    }
}

/// A fitted ECU found by [`DiagnosticClient::scan_present`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FittedEcu {
    /// The diagnostic address that answered.
    pub address: u8,
    /// How quickly it answered the probe.
    pub latency: Duration,
}

/// One ECU's faults after partitioning relevant faults from not-tested noise.
#[derive(Debug, Clone)]
pub struct EcuFaults {
    /// The diagnostic address.
    pub address: u8,
    /// Real faults worth surfacing (see [`Dtc::is_relevant`]).
    pub relevant: Vec<Dtc>,
    /// Count of "not tested this cycle" catalog entries suppressed.
    pub not_tested: usize,
    /// Set if reading this ECU failed (the scan continues past it).
    pub error: Option<String>,
}

/// The record of a verified per-ECU clear.
#[derive(Debug, Clone)]
pub struct ClearReport {
    /// The diagnostic address.
    pub address: u8,
    /// Every DTC stored immediately before the clear (all statuses) — the record
    /// of what was discarded (with its freeze-frame/snapshot data).
    pub before: Vec<Dtc>,
    /// Relevant faults still present after the clear (empty = clean).
    pub after_relevant: Vec<Dtc>,
    /// True if the post-clear re-read showed no relevant faults.
    pub verified_clean: bool,
    /// Set if any step failed for this ECU (others still processed).
    pub error: Option<String>,
}

impl DiagnosticClient {
    /// Probe `addrs` and return those that answer, bounded by `opts.concurrency`.
    pub async fn scan_present(&self, addrs: &[u8], opts: ScanOptions) -> Vec<FittedEcu> {
        let mut fitted: Vec<FittedEcu> = stream::iter(addrs.iter().copied())
            .map(|address| async move {
                match self.probe(address, opts.probe_timeout).await {
                    Ok(ProbeOutcome::Present { latency, .. }) => Some(FittedEcu { address, latency }),
                    _ => None,
                }
            })
            .buffer_unordered(opts.concurrency.max(1))
            .filter_map(|x| async move { x })
            .collect()
            .await;
        fitted.sort_unstable_by_key(|f| f.address);
        fitted
    }

    /// Scan `addrs`, then read + partition faults for each fitted ECU.
    ///
    /// A per-ECU read failure is recorded in [`EcuFaults::error`], never aborting
    /// the whole scan.
    pub async fn scan_faults(&self, addrs: &[u8], opts: ScanOptions) -> Vec<EcuFaults> {
        let fitted = self.scan_present(addrs, opts).await;
        let mut out: Vec<EcuFaults> = stream::iter(fitted.into_iter().map(|f| f.address))
            .map(|address| async move {
                match self.read_all_dtcs(address).await {
                    Ok(dtcs) => {
                        let (relevant, noise): (Vec<Dtc>, Vec<Dtc>) =
                            dtcs.into_iter().partition(|d| d.is_relevant());
                        EcuFaults { address, relevant, not_tested: noise.len(), error: None }
                    }
                    Err(e) => EcuFaults {
                        address,
                        relevant: Vec::new(),
                        not_tested: 0,
                        error: Some(e.to_string()),
                    },
                }
            })
            .buffer_unordered(opts.concurrency.max(1))
            .collect()
            .await;
        out.sort_unstable_by_key(|e| e.address);
        out
    }

    /// Clear one ECU with a pre-read record and a post-clear verification.
    ///
    /// A state change (UDS 0x14): reads and records the stored DTCs, enters the
    /// extended session, clears all, then re-reads to confirm no relevant fault
    /// remains. Never aborts a batch — a failure is captured in the report.
    pub async fn clear_faults_verified(&self, target: u8) -> ClearReport {
        let mut report = ClearReport {
            address: target,
            before: Vec::new(),
            after_relevant: Vec::new(),
            verified_clean: false,
            error: None,
        };
        match self.read_all_dtcs(target).await {
            Ok(before) => report.before = before,
            Err(e) => {
                report.error = Some(format!("pre-read failed: {e}"));
                return report; // never clear blind
            }
        }
        if let Err(e) = self.clear_all_dtcs(target).await {
            report.error = Some(format!("clear failed: {e}"));
            return report;
        }
        match self.read_all_dtcs(target).await {
            Ok(after) => {
                report.after_relevant = after.into_iter().filter(Dtc::is_relevant).collect();
                report.verified_clean = report.after_relevant.is_empty();
            }
            Err(e) => report.error = Some(format!("post-read verify failed: {e}")),
        }
        report
    }

    /// Clear every ECU in `addrs`, sequentially, returning a per-ECU report.
    ///
    /// Sequential by design — writes stay lockstep even though reads fan out.
    pub async fn clear_faults_all(&self, addrs: &[u8]) -> Vec<ClearReport> {
        let mut reports = Vec::with_capacity(addrs.len());
        for &address in addrs {
            reports.push(self.clear_faults_verified(address).await);
        }
        reports
    }
}
```

Wire the module in `crates/client/src/lib.rs`: add `mod scan;` and `pub use scan::{ClearReport, EcuFaults, FittedEcu, ScanOptions};`. Make `DiagnosticClient`'s `session` field reachable from `scan.rs` — it's the same crate, and `scan.rs` calls only public methods, so no visibility change needed. `ProbeOutcome` must be crate-visible: it already is `pub`.

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo test -p klartext-client 2>&1 | tail -15` — Expected: all client tests pass including scan.

- [ ] **Step 5: Add a concurrency-overlap timing test**

Add to `scan.rs` tests — proves concurrency actually overlaps (5 absent addresses with a 200 ms probe finish well under the 1 s a serial sweep would take):

```rust
    #[tokio::test]
    async fn scan_concurrency_overlaps_absent_probes() {
        let addr = spawn(&[]).await; // nothing present
        let client = client(addr).await;
        let opts = ScanOptions { probe_timeout: Duration::from_millis(200), concurrency: 8 };
        let started = tokio::time::Instant::now();
        let fitted = client.scan_present(&[1, 2, 3, 4, 5], opts).await;
        assert!(fitted.is_empty());
        assert!(started.elapsed() < Duration::from_millis(600), "8-wide scan should overlap");
    }
```

Run: `cargo test -p klartext-client scan_concurrency 2>&1 | tail -6` — Expected: pass.

- [ ] **Step 6: fmt + clippy + commit**

```bash
cargo fmt -p klartext-client
cargo clippy -p klartext-client --all-targets -- -D warnings 2>&1 | tail -3
git add crates/client
git commit -m "feat(client): whole-car scan + verified whole-car clear orchestrations"
```

---

## Task 6: ECU resolution & listing from the DB, aliases removed (`klartext-mcp`)

**Files:**
- Modify: `mcp/src/ecu.rs` (delete `BUILTIN_ALIASES`; resolve = hex | group | variant; list from `EcuSlot`)
- Modify: `mcp/src/dto.rs` (`EcuInfo` gains `title`, `variants`; `ListEcusResult` gains `db_error`)

**Interfaces:**
- Consumes: `Catalog::{ecus, variants}`, `EcuSlot`, `VariantInfo`.
- Produces: `ecu::resolve(spec, catalog) -> Result<u8, String>`; `ecu::list(catalog) -> Result<Vec<EcuInfo>, String>` (now fallible so a DB error is surfaced, not swallowed).

- [ ] **Step 1: Write failing tests**

Replace the `tests` module of `mcp/src/ecu.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_raw_hex_address_without_db() {
        assert_eq!(resolve("0x12", None).unwrap(), 0x12);
        assert_eq!(resolve("0X40", None).unwrap(), 0x40);
    }

    #[test]
    fn resolve_unknown_without_db_names_list_ecus() {
        let err = resolve("DME", None).unwrap_err();
        assert!(err.contains("list_ecus"), "{err}");
        // The old wrong aliases are gone: "DME" no longer resolves to 0x12.
        assert!(resolve("DME", None).is_err());
    }

    #[test]
    fn list_without_db_is_empty_not_misleading_aliases() {
        let ecus = list(None).unwrap();
        assert!(ecus.is_empty());
    }
}
```

(DB-backed resolve/list are covered by `klartext-semantic`'s catalog tests + the MCP integration test in Task 9.)

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p klartext-mcp ecu:: 2>&1 | tail -15` — Expected: fail (aliases still resolve `DME`; `list` returns builtins / wrong return type).

- [ ] **Step 3: Implement**

Rewrite `mcp/src/ecu.rs`:

```rust
//! ECU targeting: resolve a name/hex/variant to a diagnostic address, and list
//! the targetable ECUs — all from the ISTA semantic DB, no hardcoded aliases.
//!
//! The first live session proved static aliases actively harmful: "DME" mislabels
//! this car's diesel DDE and "CAS" is really the FEM on F20. Names now come only
//! from the DB (`Catalog::ecus`/`variants`); without the DB the tools accept raw
//! hex addresses and say so, rather than surfacing a wrong name.

use klartext_semantic::Catalog;

use crate::dto::EcuInfo;

/// Resolve an `ecu` parameter to a diagnostic address.
///
/// Order: a raw hex address (`0x12`), then (with the DB) an ISTA group name
/// (`d_0012`, case-insensitive), then an ISTA variant name (`d72n47a0`).
///
/// # Errors
/// Returns a human message (naming `list_ecus`) when `spec` matches none of these.
pub fn resolve(spec: &str, catalog: Option<&Catalog>) -> Result<u8, String> {
    let s = spec.trim();
    if let Some(addr) = parse_hex_address(s) {
        return Ok(addr);
    }
    if let Some(catalog) = catalog {
        let slots = catalog
            .ecus()
            .map_err(|e| format!("reading the ECU map: {e}"))?;
        // Group name (canonical or extra).
        if let Some(slot) = slots.iter().find(|slot| {
            slot.group_name.eq_ignore_ascii_case(s)
                || slot.extra_groups.iter().any(|g| g.eq_ignore_ascii_case(s))
        }) {
            return Ok(slot.address);
        }
        // Variant name → its address.
        for slot in &slots {
            let variants = catalog
                .variants(slot.address)
                .map_err(|e| format!("reading ECU variants: {e}"))?;
            if variants.iter().any(|v| v.name.eq_ignore_ascii_case(s)) {
                return Ok(slot.address);
            }
        }
    }
    Err(format!(
        "unknown ECU '{spec}'. Use a raw hex address like 0x12, or (with the semantic \
         DB) an ISTA group name like d_0012 or a variant name like d72n47a0 — call \
         list_ecus to see them."
    ))
}

/// List targetable ECUs from the semantic DB (empty without it).
///
/// # Errors
/// Returns a human message if the DB is present but the ECU query fails — the
/// caller surfaces that instead of silently reporting an empty list.
pub fn list(catalog: Option<&Catalog>) -> Result<Vec<EcuInfo>, String> {
    let Some(catalog) = catalog else {
        return Ok(Vec::new());
    };
    let slots = catalog.ecus().map_err(|e| format!("reading the ECU map: {e}"))?;
    let mut out = Vec::with_capacity(slots.len());
    for slot in slots {
        let variants = catalog
            .variants(slot.address)
            .map_err(|e| format!("reading ECU variants: {e}"))?
            .into_iter()
            .map(|v| v.name)
            .collect();
        out.push(EcuInfo {
            address_hex: format!("0x{:02X}", slot.address),
            group_name: slot.group_name,
            extra_groups: slot.extra_groups,
            title: slot.title,
            variants,
        });
    }
    Ok(out)
}

/// Parse a raw diagnostic address written as `0x12` / `0X12`.
fn parse_hex_address(s: &str) -> Option<u8> {
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    u8::from_str_radix(hex, 16).ok()
}
```

In `mcp/src/dto.rs`, replace `EcuInfo` and add `db_error` to `ListEcusResult`:

```rust
/// One targetable ECU for `list_ecus`.
#[derive(Debug, Clone, Serialize, schemars::JsonSchema)]
pub struct EcuInfo {
    /// Diagnostic address as hex, e.g. `0x12`.
    pub address_hex: String,
    /// The canonical ISTA group name, e.g. `d_0012`.
    pub group_name: String,
    /// Other ISTA group names at this address (e.g. `g_motor`).
    pub extra_groups: Vec<String>,
    /// A human title for the ECU, when the DB has one.
    pub title: Option<String>,
    /// The SGBD variant names ISTA records at this address (for read_data etc.).
    pub variants: Vec<String>,
}
```

Add to `ListEcusResult`:

```rust
    /// Set when the DB was present but the ECU query failed (surfaced, not swallowed).
    pub db_error: Option<String>,
```

- [ ] **Step 4: Update `list_ecus` in `server.rs` (compile fix; fuller wiring in Task 8)**

Change `list_ecus` to handle the fallible `ecu::list` and set `db_error`:

```rust
    pub async fn list_ecus(&self) -> Result<Json<ListEcusResult>, McpError> {
        let catalog = self.catalog();
        let db_available = catalog.is_some();
        let (ecus, db_error) = match ecu::list(catalog.as_ref()) {
            Ok(ecus) => (ecus, None),
            Err(e) => (Vec::new(), Some(e)),
        };
        let note = if !db_available {
            "No semantic DB — target ECUs by raw hex address like 0x12. Build the DB \
             (scripts/build-semantic-db.sh) for names and the full map."
                .to_string()
        } else if db_error.is_some() {
            "The semantic DB is present but the ECU query failed — see db_error.".to_string()
        } else {
            "ECU map from the ISTA semantic DB.".to_string()
        };
        Ok(Json(ListEcusResult { ecus, db_available, note, db_error }))
    }
```

Update `read_faults`/`clear_faults`/`read_data` calls to `ecu::resolve` — the signature is unchanged, so they still compile. Their `db_error` handling is refined in Task 8.

- [ ] **Step 5: Run tests + build**

Run: `cargo test -p klartext-mcp ecu:: 2>&1 | tail -10` then `cargo build -p klartext-mcp 2>&1 | tail -5`
Expected: ecu tests pass; mcp builds (server may still reference other unchanged items — fix any fallout minimally).

- [ ] **Step 6: fmt + clippy + commit**

```bash
cargo fmt -p klartext-mcp
cargo clippy -p klartext-mcp --lib -- -D warnings 2>&1 | tail -3
git add mcp/src/ecu.rs mcp/src/dto.rs mcp/src/server.rs
git commit -m "feat(mcp): resolve/list ECUs from the DB, drop misleading hardcoded aliases"
```

---

## Task 7: Learned per-VIN variant profile (`klartext-mcp`)

**Files:**
- Create: `mcp/src/profile.rs`
- Modify: `mcp/src/lib.rs` (add `pub mod profile;`), `mcp/src/config.rs` (add `--profile-dir`, `--no-profile`)

**Interfaces:**
- Produces:
  - `CarProfile { variants: BTreeMap<u8, String> }` with `get(address) -> Option<&str>`
  - `fn profile_path(dir: &Path, vin: &str) -> PathBuf`
  - `fn load(dir: &Path, vin: &str) -> CarProfile` (missing/corrupt → empty, logged)
  - `fn record(dir: &Path, vin: &str, address: u8, variant: &str) -> std::io::Result<()>` (atomic; merges)
- Consumes: `serde`, `serde_json`.

- [ ] **Step 1: Add serde_json (already a dep) + write failing tests**

Create `mcp/src/profile.rs` with tests first:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_load_roundtrips_by_vin() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "WBAVIN0000000001", 0x12, "d72n47a0").unwrap();
        record(dir.path(), "WBAVIN0000000001", 0x40, "fem_20").unwrap();
        let p = load(dir.path(), "WBAVIN0000000001");
        assert_eq!(p.get(0x12), Some("d72n47a0"));
        assert_eq!(p.get(0x40), Some("fem_20"));
        // A different VIN is a separate profile.
        assert!(load(dir.path(), "WBAVIN0000000002").get(0x12).is_none());
    }

    #[test]
    fn record_updates_an_existing_address() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "V", 0x12, "old").unwrap();
        record(dir.path(), "V", 0x12, "d72n47a0").unwrap();
        assert_eq!(load(dir.path(), "V").get(0x12), Some("d72n47a0"));
    }

    #[test]
    fn missing_or_corrupt_profile_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path(), "nope").variants.is_empty());
        std::fs::write(profile_path(dir.path(), "bad"), b"{ not json").unwrap();
        assert!(load(dir.path(), "bad").variants.is_empty());
    }
}
```

Add `tempfile` to `mcp` dev-deps if missing (`cargo add --dev tempfile -p klartext-mcp` — it's already there per Cargo.toml).

- [ ] **Step 2: Run tests, verify they fail**

Run: `cargo test -p klartext-mcp profile 2>&1 | tail -12` — Expected: module missing.

- [ ] **Step 3: Implement**

Prepend to `mcp/src/profile.rs`:

```rust
//! A learned, per-VIN map of diagnostic address → SGBD variant.
//!
//! Variant auto-detection from the gateway (the SVT read) is a future milestone;
//! until then, when a caller reads an ECU with an explicit `variant` and it scales
//! a value, we remember it for that VIN. Later reads of the same ECU on the same
//! car then default to that variant, so the human types it once. Stored as small
//! JSON per VIN under a state dir (BYO nothing; no BMW data — just addr→variant).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A car's learned address → variant map, keyed on VIN by its file name.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CarProfile {
    /// Diagnostic address → SGBD variant learned for this car.
    pub variants: BTreeMap<u8, String>,
}

impl CarProfile {
    /// The learned variant for `address`, if any.
    pub fn get(&self, address: u8) -> Option<&str> {
        self.variants.get(&address).map(String::as_str)
    }
}

/// The profile file path for `vin` under `dir` (VIN sanitized to a safe stem).
pub fn profile_path(dir: &Path, vin: &str) -> PathBuf {
    let stem: String = vin
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    dir.join(format!("{stem}.json"))
}

/// Load the profile for `vin`; a missing or unreadable/corrupt file → empty.
pub fn load(dir: &Path, vin: &str) -> CarProfile {
    let path = profile_path(dir, vin);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %path.display(), "ignoring corrupt car profile");
            CarProfile::default()
        }),
        Err(_) => CarProfile::default(),
    }
}

/// Record `address → variant` for `vin`, merging into any existing profile.
///
/// Writes atomically (temp file + rename) so a crash mid-write cannot corrupt it.
///
/// # Errors
/// Returns the I/O error if the directory cannot be created or the file written.
pub fn record(dir: &Path, vin: &str, address: u8, variant: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut profile = load(dir, vin);
    if profile.variants.get(&address).map(String::as_str) == Some(variant) {
        return Ok(()); // unchanged
    }
    profile.variants.insert(address, variant.to_string());
    let path = profile_path(dir, vin);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(&profile).expect("profile serializes");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}
```

Add `pub mod profile;` to `mcp/src/lib.rs`. Add to `ServerConfig` in `mcp/src/config.rs`:

```rust
    /// Directory for learned per-VIN variant profiles (address → SGBD variant).
    /// Defaults to `$XDG_STATE_HOME/klartext/profiles` (or `~/.local/state/...`).
    #[arg(long, env = "KLARTEXT_PROFILE_DIR")]
    pub profile_dir: Option<PathBuf>,

    /// Disable reading/writing the learned variant profile entirely.
    #[arg(long, default_value_t = false)]
    pub no_profile: bool,
```

And a resolver method on `ServerConfig`:

```rust
    /// The effective profile directory (default under XDG state home), or `None`
    /// when profiles are disabled.
    pub fn profile_dir(&self) -> Option<PathBuf> {
        if self.no_profile {
            return None;
        }
        Some(self.profile_dir.clone().unwrap_or_else(|| {
            let base = std::env::var_os("XDG_STATE_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| {
                    let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
                    home.join(".local/state")
                });
            base.join("klartext/profiles")
        }))
    }
```

- [ ] **Step 4: Run tests, verify pass**

Run: `cargo test -p klartext-mcp profile 2>&1 | tail -10` — Expected: pass.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo fmt -p klartext-mcp
cargo clippy -p klartext-mcp --all-targets -- -D warnings 2>&1 | tail -3
git add mcp/src/profile.rs mcp/src/lib.rs mcp/src/config.rs
git commit -m "feat(mcp): learned per-VIN variant profile with atomic writes"
```

---

## Task 8: Variant ladder, SGBD caching, fault partition, one-connection session (`klartext-mcp`)

**Files:**
- Modify: `mcp/src/session.rs` (drop retarget-reconnect; hold VIN + fitted cache; single target moves to per-request)
- Modify: `mcp/src/server.rs` (variant-resolution ladder; per-variant SGBD cache; `read_faults` partition + `include_not_tested`; record profile on scaled read)
- Modify: `mcp/src/dto.rs` (`ReadFaultsResult` partition fields; `ReadFaultsRequest.include_not_tested`; optional `ecu` on measurement/service requests)

**Interfaces:**
- Consumes: Tasks 3–7 (`DiagnosticClient` target methods, `profile`, `Catalog::variants`).
- Produces: `KlartextServer::resolve_variant(&self, address, explicit, catalog) -> Result<Option<String>, McpError>`; cached SGBD accessors.

- [ ] **Step 1: Simplify `session.rs` to one connection, no retarget-reconnect**

Rewrite `mcp/src/session.rs` `Connection` + `ensure_target`:

```rust
//! The car connection held in server state: establish (discover/connect + VIN),
//! and a per-connection cache of the fitted-ECU scan. One connection to the
//! gateway serves every ECU (the client demuxes by target), so switching ECUs is
//! free — no reconnect. `Drop` on the held [`DiagnosticClient`] aborts keepalive.
```

- Drop `target` field and `ensure_target` (retargeting is gone — every read passes its own target). Keep `gateway_ip`, `vin`, `vin_source`, `client`. Add `fitted: Option<Vec<u8>>` (cached scan) with helpers `set_fitted`/`fitted`.
- `establish` unchanged except it reads the VIN via `client.read_did(ZGW_ADDRESS, DID_VIN)` (now target-parameterized) and no longer stores a `target`.

- [ ] **Step 2: Server — remove every `ensure_target` call, pass targets directly**

In `server.rs`, `read_faults`/`clear_faults`/`read_data` change from:

```rust
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        session::ensure_target(conn, &self.config, address).await.map_err(...)?;
        let dtcs = conn.client.read_dtcs(ALL_DTC_STATUS_MASK).await...;
```

to (note `&conn.client`, target passed, no retarget):

```rust
        let conn = guard.as_ref().ok_or_else(not_connected)?;
        let dtcs = conn.client.read_all_dtcs(address).await
            .map_err(|e| McpError::internal_error(format!("reading DTCs: {e}"), None))?;
```

Apply the same shape to `read_data` (dynamic + static reads pass `address`) and `clear_faults` (pre-read + `clear_all_dtcs(address)`).

- [ ] **Step 3: Write failing tests for the variant ladder + partition**

Add to `server.rs` tests:

```rust
    #[test]
    fn partition_faults_splits_relevant_from_not_tested() {
        use klartext_uds::Dtc;
        let dtcs = vec![
            Dtc { code: [0, 0, 1], status: 0x08 }, // confirmed
            Dtc { code: [0, 0, 2], status: 0x40 }, // not tested
            Dtc { code: [0, 0, 3], status: 0x01 }, // failed
        ];
        let (relevant, not_tested) = partition_faults(dtcs);
        assert_eq!(relevant.len(), 2);
        assert_eq!(not_tested, 1);
    }
```

(The variant ladder's DB-unique and profile branches are exercised by Task 9's integration test and Task 7's profile tests; here we unit-test the pure partition helper.)

- [ ] **Step 4: Implement the partition helper + wire `read_faults`**

Add to `server.rs`:

```rust
/// Split DTCs into (relevant faults, count of not-tested-this-cycle noise).
fn partition_faults(dtcs: Vec<Dtc>) -> (Vec<Dtc>, usize) {
    let (relevant, noise): (Vec<Dtc>, Vec<Dtc>) = dtcs.into_iter().partition(|d| d.is_relevant());
    (relevant, noise.len())
}
```

Add `use klartext_uds::Dtc;` (already imported via `{ALL_DTC_STATUS_MASK, Dtc}`). In `read_faults`, after reading `dtcs`, branch on `req.include_not_tested`:

```rust
        let not_tested_count;
        let shown: Vec<Dtc> = if req.include_not_tested {
            not_tested_count = dtcs.iter().filter(|d| !d.is_relevant()).count();
            dtcs.clone()
        } else {
            let (relevant, count) = partition_faults(dtcs.clone());
            not_tested_count = count;
            relevant
        };
```

Build `faults` from `shown` (same mapping as today), and set the new result fields. Add to `ReadFaultsRequest`:

```rust
    /// Include "not tested this cycle" catalog entries (status 0x40/0x50 noise).
    /// Default false — those are suppressed and only counted.
    #[serde(default)]
    pub include_not_tested: bool,
```

Add to `ReadFaultsResult`:

```rust
    /// Count of "not tested this cycle" entries suppressed (unless include_not_tested).
    pub not_tested_count: usize,
```

- [ ] **Step 5: Implement the variant-resolution ladder + SGBD cache**

Add cache state to `KlartextServer`:

```rust
    /// Parsed SGBD measurement catalogs, cached per variant (the .prg parse is
    /// ~1800 rows on the DDE — expensive to redo per call).
    sgbd_cache: Arc<std::sync::Mutex<std::collections::HashMap<String, Arc<Measurements>>>>,
```

Initialize in `new()`. Replace `measurements(variant)` to consult the cache:

```rust
    fn measurements(&self, variant: Option<&str>) -> Option<Arc<Measurements>> {
        let variant = variant?;
        if let Some(hit) = self.sgbd_cache.lock().unwrap().get(variant).cloned() {
            return Some(hit);
        }
        let path = self.sgbd_path(variant)?;
        match Measurements::from_sgbd(&path) {
            Ok(m) => {
                let arc = Arc::new(m);
                self.sgbd_cache.lock().unwrap().insert(variant.to_string(), Arc::clone(&arc));
                Some(arc)
            }
            Err(error) => {
                tracing::warn!(%error, "SGBD measurement scaling unavailable; raw only");
                None
            }
        }
    }
```

(Callers using `measurements(...).as_ref()` still work via `Arc` deref; adjust `.get(did)` call sites to `m.get(did)` on the `Arc<Measurements>`.)

Add the ladder:

```rust
    /// Resolve which SGBD variant to use for `address`: explicit → learned profile
    /// → DB-unique-with-.prg → error listing candidates. `None` means "no variant
    /// needed / none resolvable but the caller may proceed raw"; callers that
    /// require one convert `None` into the candidate error.
    fn resolve_variant(
        &self,
        address: u8,
        explicit: Option<&str>,
        catalog: Option<&Catalog>,
        vin: Option<&str>,
    ) -> Option<String> {
        if let Some(v) = explicit {
            return Some(v.to_string());
        }
        // Learned profile for this car.
        if let (Some(dir), Some(vin)) = (self.config.profile_dir(), vin)
            && let Some(v) = crate::profile::load(&dir, vin).get(address)
        {
            return Some(v.to_string());
        }
        // DB-unique candidate whose .prg exists.
        if let Some(catalog) = catalog
            && let Ok(variants) = catalog.variants(address)
        {
            let available: Vec<String> = variants
                .into_iter()
                .map(|v| v.name)
                .filter(|name| self.sgbd_path(name).is_some_and(|p| p.exists()))
                .collect();
            if let [only] = available.as_slice() {
                tracing::info!(variant = %only, address, "variant auto-resolved (DB-unique)");
                return Some(only.clone());
            }
        }
        None
    }
```

In `read_data`: when `req.variant` is `None`, call `resolve_variant(address, None, catalog, vin)` and use the result as the effective variant (both to load `measurements` and to report). After a successful scaled read with an *explicit or resolved* variant, record it:

```rust
        if let (Some(dir), Some(vin), Some(variant)) =
            (self.config.profile_dir(), conn_vin.as_deref(), effective_variant.as_deref())
            && scaled_value.is_some()
        {
            if let Err(error) = crate::profile::record(&dir, vin, address, variant) {
                tracing::warn!(%error, "could not record learned variant");
            }
        }
```

(Read `conn_vin` from the held connection before dropping the guard.)

Add optional `ecu` to `ListMeasurementsRequest`/`ListServiceFunctionsRequest` and make `variant` optional there; when `variant` is absent, resolve it from `ecu` via the ladder (error with candidates from `catalog.variants(address)` if unresolved). The candidate error:

```rust
    fn variant_candidates_error(&self, address: u8, catalog: Option<&Catalog>) -> McpError {
        let list = catalog
            .and_then(|c| c.variants(address).ok())
            .map(|vs| {
                vs.iter()
                    .map(|v| match &v.title {
                        Some(t) => format!("{} ({t})", v.name),
                        None => v.name.clone(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ")
            })
            .unwrap_or_default();
        McpError::invalid_params(
            format!(
                "need a `variant` for ECU 0x{address:02X} and none could be resolved \
                 (no explicit variant, no learned profile, and the DB lists several). \
                 Pass one of: {list}"
            ),
            None,
        )
    }
```

- [ ] **Step 6: Run tests + build**

Run: `cargo test -p klartext-mcp 2>&1 | tail -15` then `cargo build -p klartext-mcp 2>&1 | tail -5`
Expected: unit tests pass; mcp builds.

- [ ] **Step 7: fmt + clippy + commit**

```bash
cargo fmt -p klartext-mcp
cargo clippy -p klartext-mcp --all-targets -- -D warnings 2>&1 | tail -3
git add mcp/src
git commit -m "feat(mcp): variant ladder + SGBD cache + fault partition over a single connection"
```

---

## Task 9: New MCP tools — scan_ecus, read_all_faults, clear_all_faults

**Files:**
- Modify: `mcp/src/server.rs` (three new `#[tool]` methods + server instructions)
- Modify: `mcp/src/dto.rs` (request/result DTOs)
- Modify: `mcp/tests/integration.rs` (loopback multi-ECU coverage)

**Interfaces:**
- Consumes: `DiagnosticClient::{scan_present, scan_faults, clear_faults_all}`, `ScanOptions`, `Connection::{fitted, set_fitted}`.
- Produces: tools `scan_ecus`, `read_all_faults`, `clear_all_faults`.

- [ ] **Step 1: Add DTOs to `dto.rs`**

```rust
/// Arguments for `scan_ecus`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ScanEcusRequest {
    /// Re-probe even if a fitted list is cached from an earlier scan this session.
    #[serde(default)]
    pub rescan: bool,
}

/// One fitted ECU in a live scan.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FittedEcuInfo {
    /// Diagnostic address as hex, e.g. `0x12`.
    pub address_hex: String,
    /// Canonical ISTA group name, when the DB has one.
    pub group_name: Option<String>,
    /// A human title, when the DB has one.
    pub title: Option<String>,
    /// Probe round-trip in milliseconds.
    pub latency_ms: u64,
}

/// Result of `scan_ecus`: the ECUs actually present on this car.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ScanEcusResult {
    /// The fitted ECUs, ordered by address.
    pub ecus: Vec<FittedEcuInfo>,
    /// How many addresses were probed.
    pub probed: usize,
    /// Human note (how the universe was chosen; cached vs fresh).
    pub note: String,
}

/// Arguments for `read_all_faults`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ReadAllFaultsRequest {
    /// Re-probe the fitted list before reading (else use the session cache).
    #[serde(default)]
    pub rescan: bool,
    /// Include not-tested-this-cycle noise in each ECU's fault list.
    #[serde(default)]
    pub include_not_tested: bool,
}

/// One ECU's faults in a whole-car read.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EcuFaultsInfo {
    /// Diagnostic address as hex.
    pub address_hex: String,
    /// A human title, when the DB has one.
    pub title: Option<String>,
    /// Decoded faults (relevant only unless include_not_tested).
    pub faults: Vec<FaultInfo>,
    /// Count of not-tested-this-cycle entries suppressed.
    pub not_tested_count: usize,
    /// Set if this ECU could not be read (the scan continued).
    pub error: Option<String>,
}

/// Result of `read_all_faults`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ReadAllFaultsResult {
    /// Per-ECU faults, ordered by address; ECUs with no relevant fault are included
    /// with an empty list so the caller sees the whole scanned set.
    pub ecus: Vec<EcuFaultsInfo>,
    /// Total relevant faults across all ECUs.
    pub total_relevant: usize,
    /// Whether the semantic DB was available for fault text.
    pub db_available: bool,
    /// Human note.
    pub note: String,
}

/// Arguments for `clear_all_faults`: whole-car clear, confirmation-gated.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ClearAllFaultsRequest {
    /// Must be `true`. Without it the tool refuses and explains what a whole-car
    /// clear discards (every ECU's freeze-frames; readiness monitors may reset).
    #[serde(default)]
    pub confirm: bool,
    /// Re-probe the fitted list before clearing (else use the session cache).
    #[serde(default)]
    pub rescan: bool,
}

/// One ECU's clear outcome in a whole-car clear.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct EcuClearInfo {
    /// Diagnostic address as hex.
    pub address_hex: String,
    /// The DTC codes (hex) stored before the clear — the record of what was discarded.
    pub codes_before: Vec<String>,
    /// Whether the post-clear re-read showed no relevant fault.
    pub verified_clean: bool,
    /// Set if this ECU's clear failed (others still processed).
    pub error: Option<String>,
}

/// Result of a confirmed `clear_all_faults`.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct ClearAllFaultsResult {
    /// Per-ECU clear outcomes, ordered by address.
    pub ecus: Vec<EcuClearInfo>,
    /// How many ECUs were cleared clean.
    pub cleared_clean: usize,
    /// Human note (verify guidance).
    pub note: String,
}
```

- [ ] **Step 2: Write the failing integration test**

In `mcp/tests/integration.rs`, add a loopback multi-ECU gateway (mirror the client `scan.rs` mock) and drive the server end-to-end. Assert: `scan_ecus` returns exactly the fitted set; `read_all_faults` returns per-ECU relevant faults with a not-tested count; `clear_all_faults` refuses without confirm and, with `confirm:true`, reports `verified_clean` per ECU. (Follow the existing integration-test harness in that file for how the server is constructed and connected to a mock.)

- [ ] **Step 3: Run test, verify it fails**

Run: `cargo test -p klartext-mcp --test integration scan 2>&1 | tail -15` — Expected: tools not found.

- [ ] **Step 4: Implement the three tools**

Add to the `#[tool_router] impl KlartextServer`. First a shared helper to get the address universe + fitted list:

```rust
    /// The address universe to probe: the DB's ECU map, or a full 0x00..=0xFF
    /// sweep when there is no DB.
    fn scan_universe(&self, catalog: Option<&Catalog>) -> Vec<u8> {
        match catalog.and_then(|c| c.ecus().ok()) {
            Some(slots) if !slots.is_empty() => slots.iter().map(|s| s.address).collect(),
            _ => (0u8..=0xFF).collect(),
        }
    }

    /// The scan options from server config.
    fn scan_options(&self) -> klartext_client::ScanOptions {
        klartext_client::ScanOptions {
            probe_timeout: std::time::Duration::from_millis(self.config.probe_timeout),
            concurrency: self.config.scan_concurrency,
        }
    }
```

(Add `probe_timeout: u64` (default 300) and `scan_concurrency: usize` (default 8) to `ServerConfig`.)

`scan_ecus`:

```rust
    #[tool(description = "Discover the ECUs actually FITTED on this car by probing \
        each candidate address with a harmless TesterPresent — not the full generic \
        model map. Requires a prior connect. Absent modules are skipped fast (they do \
        not hang the scan). Results are cached for the session; pass rescan=true to \
        re-probe. Use this before read_all_faults so you reason about the real car.")]
    pub async fn scan_ecus(
        &self,
        Parameters(req): Parameters<ScanEcusRequest>,
    ) -> Result<Json<ScanEcusResult>, McpError> {
        let catalog = self.catalog();
        let universe = self.scan_universe(catalog.as_ref());
        let mut guard = self.state.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;

        let fitted = if !req.rescan && conn.fitted().is_some() {
            conn.fitted().unwrap().to_vec()
        } else {
            let found = conn.client.scan_present(&universe, self.scan_options()).await;
            let addrs: Vec<u8> = found.iter().map(|f| f.address).collect();
            conn.set_fitted(addrs.clone());
            // Keep latencies for the reply.
            return Ok(Json(scan_result(found, &universe, catalog.as_ref())));
        };
        // Cached path: report addresses without fresh latencies.
        Ok(Json(scan_result_from_addrs(&fitted, &universe, catalog.as_ref())))
    }
```

Add the two free helpers `scan_result` (from `Vec<FittedEcu>`) and `scan_result_from_addrs`, each mapping addresses to `FittedEcuInfo` and enriching `group_name`/`title` from `catalog.ecus()`. (Keep them small and pure; they take `catalog: Option<&Catalog>`.)

`read_all_faults`:

```rust
    #[tool(description = "Read faults from EVERY fitted ECU in one call. Scans (or \
        reuses the cached fitted list), then reads and decodes each ECU's DTCs, \
        splitting real faults from 'not tested this cycle' catalog noise (counted, \
        not shown, unless include_not_tested). Requires connect. This is the \
        whole-car health check.")]
    pub async fn read_all_faults(
        &self,
        Parameters(req): Parameters<ReadAllFaultsRequest>,
    ) -> Result<Json<ReadAllFaultsResult>, McpError> {
        let catalog = self.catalog();
        let universe = self.scan_universe(catalog.as_ref());
        let mut guard = self.state.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;

        let addrs: Vec<u8> = match (req.rescan, conn.fitted()) {
            (false, Some(f)) => f.to_vec(),
            _ => {
                let found = conn.client.scan_present(&universe, self.scan_options()).await;
                let a: Vec<u8> = found.iter().map(|f| f.address).collect();
                conn.set_fitted(a.clone());
                a
            }
        };
        let scanned = conn.client.scan_faults(&addrs, self.scan_options()).await;
        // Map each EcuFaults into DTO, decoding text via the catalog per address.
        // (Build EcuFaultsInfo; total_relevant = sum of relevant lens.)
        Ok(Json(build_read_all_result(scanned, catalog.as_ref(), req.include_not_tested)))
    }
```

Add `build_read_all_result` (free fn): for each `EcuFaults`, map `relevant` (or `relevant` + note when `include_not_tested` — the client already partitioned, so `not_tested` is a count only; when `include_not_tested` is requested and the raw list is wanted, re-read is out of scope — document that `read_all_faults` always shows relevant + count, and point to per-ECU `read_faults` with `include_not_tested` for the full list). Simpler: `read_all_faults` shows relevant + `not_tested_count`; drop `include_not_tested` from its request to avoid implying a behavior we don't do. **Decision: remove `include_not_tested` from `ReadAllFaultsRequest`** and its result note says "per-ECU read_faults shows not-tested entries." Update Step 1's DTO accordingly.

`clear_all_faults`:

```rust
    #[tool(description = "Clear stored fault codes on EVERY fitted ECU — the whole-car \
        version of clear_faults. Standard UDS 0x14 per ECU; the ONLY write this server \
        exposes, batched. REQUIRES confirm=true. It discards EVERY ECU's \
        freeze-frame/snapshot data and can reset OBD readiness monitors car-wide, so \
        run read_all_faults first, tell the human exactly what is stored across the \
        car, and pass confirm=true only on their explicit go-ahead. Each ECU is \
        pre-read (codes recorded), cleared, and re-read to verify. Cannot actuate, \
        run service functions, or code.")]
    pub async fn clear_all_faults(
        &self,
        Parameters(req): Parameters<ClearAllFaultsRequest>,
    ) -> Result<Json<ClearAllFaultsResult>, McpError> {
        if !req.confirm {
            return Err(McpError::invalid_params(
                "refusing to clear faults across the whole car: this erases EVERY fitted \
                 ECU's stored DTCs together with their freeze-frame data and can reset OBD \
                 readiness monitors car-wide. Run read_all_faults, confirm with the human, \
                 then re-call with confirm=true.".to_string(),
                None,
            ));
        }
        let catalog = self.catalog();
        let universe = self.scan_universe(catalog.as_ref());
        let mut guard = self.state.lock().await;
        let conn = guard.as_mut().ok_or_else(not_connected)?;
        let addrs: Vec<u8> = match (req.rescan, conn.fitted()) {
            (false, Some(f)) => f.to_vec(),
            _ => {
                let found = conn.client.scan_present(&universe, self.scan_options()).await;
                let a: Vec<u8> = found.iter().map(|f| f.address).collect();
                conn.set_fitted(a.clone());
                a
            }
        };
        let reports = conn.client.clear_faults_all(&addrs).await;
        Ok(Json(build_clear_all_result(reports)))
    }
```

Add `build_clear_all_result` (free fn) mapping `Vec<ClearReport>` → `ClearAllFaultsResult` (codes_before via the shared `dtc_code_hex`).

- [ ] **Step 5: Run tests, verify pass**

Run: `cargo test -p klartext-mcp 2>&1 | tail -15` — Expected: integration + unit tests pass.

- [ ] **Step 6: fmt + clippy + commit**

```bash
cargo fmt -p klartext-mcp
cargo clippy -p klartext-mcp --all-targets -- -D warnings 2>&1 | tail -3
git add mcp/src mcp/tests
git commit -m "feat(mcp): whole-car scan_ecus, read_all_faults, and confirm-gated clear_all_faults"
```

---

## Task 10: MCP lifecycle — disconnect on exit (`klartext-mcp`)

**Files:**
- Modify: `mcp/src/server.rs` (add `disconnect_now`)
- Modify: `mcp/src/main.rs` (signal handling + shutdown disconnect)

**Interfaces:**
- Produces: `KlartextServer::disconnect_now(&self) -> bool` (dropped a live connection?).

- [ ] **Step 1: Add `disconnect_now` (reuses the disconnect path)**

In `server.rs`, outside the `#[tool_router]` impl (a plain method):

```rust
impl KlartextServer {
    /// Drop any held car connection during shutdown (aborts keepalive, closes TCP).
    ///
    /// The same effect as the `disconnect` tool, callable from the binary's signal
    /// handler so a killed server never leaves a dangling session to time out.
    pub async fn disconnect_now(&self) -> bool {
        self.state.lock().await.take().is_some()
    }
}
```

- [ ] **Step 2: Signal handling in `main.rs`**

Replace the serve/wait tail of `main` with:

```rust
    let server = KlartextServer::new(config);
    let shutdown = server.clone();
    let service = server
        .serve(stdio())
        .await
        .inspect_err(|e| tracing::error!(error = %e, "failed to start MCP server"))?;

    tokio::select! {
        result = service.waiting() => {
            result?;
            tracing::info!("client closed the transport");
        }
        _ = shutdown_signal() => {
            tracing::info!("received shutdown signal");
        }
    }
    if shutdown.disconnect_now().await {
        tracing::info!("dropped the held car session on shutdown");
    }
    Ok(())
}

/// Resolve when the process receives SIGINT (Ctrl-C) or SIGTERM.
async fn shutdown_signal() {
    let ctrl_c = async { tokio::signal::ctrl_c().await.ok(); };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
```

Ensure `tokio` has the `signal` feature: `cargo add tokio -p klartext-mcp --features signal` (merges into the existing feature list).

- [ ] **Step 3: Build + a shutdown test**

Add to `server.rs` tests: construct a server, inject a state connection is hard without a socket, so test the simple path — `disconnect_now` returns false when never connected:

```rust
    #[tokio::test]
    async fn disconnect_now_is_false_when_never_connected() {
        let server = KlartextServer::new(ServerConfig::parse_from(["klartext-mcp"]));
        assert!(!server.disconnect_now().await);
    }
```

Run: `cargo test -p klartext-mcp disconnect_now 2>&1 | tail -6` — Expected: pass. Then `cargo build -p klartext-mcp` — Expected: clean.

- [ ] **Step 4: fmt + clippy + commit**

```bash
cargo fmt -p klartext-mcp
cargo clippy -p klartext-mcp --all-targets -- -D warnings 2>&1 | tail -3
git add mcp/src Cargo.toml mcp/Cargo.toml
git commit -m "feat(mcp): disconnect the car session on SIGINT/SIGTERM and client exit"
```

---

## Task 11: CLI — scan, whole-car clear, fault partition (`klartext-cli`)

**Files:**
- Modify: `cli/src/main.rs` (new `scan` subcommand; `clear-faults --all-ecus`; `read-faults --all`; per-call target; `--probe-timeout`/`--scan-concurrency`; drop `ecu` from `ClientConfig` usage)

**Interfaces:**
- Consumes: Tasks 3–5 client API (`scan_present`, `scan_faults`, `clear_faults_all`, target methods).

- [ ] **Step 1: Update `connect()` and every read/clear call site to the new signatures**

`ClientConfig` no longer has `ecu`; build it without that field. Each command passes `cli.target` to the method (`client.read_dtcs(cli.target, *mask)`, `client.read_did(cli.target, *did)`, `client.clear_all_dtcs(cli.target)`, `client.reset_cbs(cli.target, ..)`, `client.run_service_reset(cli.target, ..)`, `client.tester_present(cli.target)`, `client.read_dynamic_measurement(cli.target, &requests)`).

- [ ] **Step 2: Add global scan flags + the `scan` subcommand**

Add to `Cli`:

```rust
    /// Per-ECU presence-probe timeout in ms for `scan` / whole-car ops.
    #[arg(long, default_value_t = 300, global = true)]
    probe_timeout: u64,

    /// How many ECUs to probe/read at once (1 = strictly sequential).
    #[arg(long, default_value_t = 8, global = true)]
    scan_concurrency: usize,
```

Add to `Command`:

```rust
    /// Scan the whole car: find fitted ECUs and summarize each one's faults.
    Scan {
        /// Only list fitted ECUs; skip reading faults.
        #[arg(long)]
        ecus_only: bool,
    },
```

Add to `ClearFaults`:

```rust
        /// Clear across ALL fitted ECUs, not just --target (still needs --confirm).
        #[arg(long)]
        all_ecus: bool,
```

Add to `ReadFaults`:

```rust
        /// Also show "not tested this cycle" catalog entries (default: hidden, counted).
        #[arg(long)]
        all: bool,
```

- [ ] **Step 3: Implement the handlers**

A `scan_options` helper and the address universe (from the catalog, or 0x00..=0xFF):

```rust
fn scan_options(cli: &Cli) -> klartext_client::ScanOptions {
    klartext_client::ScanOptions {
        probe_timeout: Duration::from_millis(cli.probe_timeout),
        concurrency: cli.scan_concurrency,
    }
}

fn scan_universe(catalog: Option<&Catalog>) -> Vec<u8> {
    match catalog.and_then(|c| c.ecus().ok()) {
        Some(slots) if !slots.is_empty() => slots.iter().map(|s| s.address).collect(),
        _ => (0u8..=0xFF).collect(),
    }
}
```

`Command::Scan`:

```rust
        Command::Scan { ecus_only } => {
            let (client, _gateway) = connect(&cli).await?;
            let catalog = open_catalog(&cli.semantic_db);
            let universe = scan_universe(catalog.as_ref());
            if *ecus_only {
                let fitted = client.scan_present(&universe, scan_options(&cli)).await;
                println!("{} fitted ECU(s):", fitted.len());
                for f in &fitted {
                    let name = catalog.as_ref().and_then(|c| ecu_title(c, f.address));
                    println!("  0x{:02X}  {:>5}ms  {}", f.address, f.latency.as_millis(), name.unwrap_or_default());
                }
            } else {
                let faults = client.scan_faults(&universe, scan_options(&cli)).await;
                print_scan_faults(&faults, catalog.as_ref());
            }
            print_verify_list();
        }
```

`Command::ClearFaults { confirm, all_ecus }`:

```rust
        Command::ClearFaults { confirm, all_ecus } => {
            if !confirm {
                bail!(
                    "clear-faults {} — a state change. Re-run with --confirm.",
                    if *all_ecus { "erases stored codes on EVERY fitted ECU".into() }
                    else { format!("clears stored codes on ECU 0x{:02X}", cli.target) }
                );
            }
            let (client, _gateway) = connect(&cli).await?;
            if *all_ecus {
                let catalog = open_catalog(&cli.semantic_db);
                let universe = scan_universe(catalog.as_ref());
                let fitted = client.scan_present(&universe, scan_options(&cli)).await;
                let addrs: Vec<u8> = fitted.iter().map(|f| f.address).collect();
                println!("Clearing faults on {} fitted ECU(s) …", addrs.len());
                for report in client.clear_faults_all(&addrs).await {
                    match &report.error {
                        Some(e) => println!("  0x{:02X}  ERROR: {e}", report.address),
                        None => println!(
                            "  0x{:02X}  {} cleared, {}",
                            report.address, report.before.len(),
                            if report.verified_clean { "verified clean" } else { "STILL HAS FAULTS" }
                        ),
                    }
                }
            } else {
                let report = client.clear_faults_verified(cli.target).await;
                if let Some(e) = &report.error { bail!("clear failed: {e}"); }
                println!("✔ Cleared {} code(s) on 0x{:02X}; {}.", report.before.len(), cli.target,
                    if report.verified_clean { "verified clean" } else { "faults remain — diagnose" });
            }
        }
```

`Command::ReadFaults { mask, raw, all }`: read `client.read_dtcs(cli.target, *mask)`, then if `!all` partition and print `relevant` + a "(N not-tested entries hidden — pass --all)" line. Add `print_scan_faults` and `ecu_title` helpers (title from `catalog.ecus()`).

- [ ] **Step 4: Build + run the CLI help to confirm wiring**

```bash
cargo build -p klartext-cli 2>&1 | tail -5
cargo run -q -p klartext-cli -- scan --help 2>&1 | tail -12
cargo run -q -p klartext-cli -- clear-faults --help 2>&1 | tail -12
```
Expected: builds; `scan` and `--all-ecus`/`--all` appear.

- [ ] **Step 5: fmt + clippy + commit**

```bash
cargo fmt -p klartext-cli
cargo clippy -p klartext-cli --all-targets -- -D warnings 2>&1 | tail -3
git add cli/src
git commit -m "feat(cli): whole-car scan, all-ECU verified clear, and fault relevance filter"
```

---

## Task 12: Docs, skill, invariant, and remove the committed VIN capture

**Files:**
- Modify: `CLAUDE.md` (invariant refinement: whole-car clear is the same one write, batched)
- Modify: `skills/klartext-service/SKILL.md` (connection prerequisites; scan + whole-car workflows; standard-PID limitation; disconnect hygiene)
- Modify: `docs/field-findings-2026-07-03.md` (check off addressed items)
- Modify: `docs/standard-pids.md` (record the live no-op result)
- Modify: `README.md` (new tools/flags)
- Remove from git: `captures/SESSION-2026-07-03.md` (BYO-data / VIN — untrack + drop the commit)

- [ ] **Step 1: Untrack the committed VIN capture**

```bash
git rm --cached captures/SESSION-2026-07-03.md
git commit -m "chore: untrack the live-session log (BYO-data: contains the VIN)"
```

(The file stays on disk, now correctly ignored by the existing `captures/` rule. Dropping the original commit `371186f` from history is handled in the finish/rebase step — see Task 13.)

- [ ] **Step 2: CLAUDE.md invariant refinement**

In the `## MCP server (M4)` section, extend the invariant paragraph to note the whole-car batch: `clear_all_faults` is the same standard UDS 0x14, iterated over fitted ECUs, still confirm-gated, still per-ECU pre-read + post-verify; the absolute line (no physical actuation, no derived-unconfirmed write) is unchanged. Add one line under `## Standard-PID scaling (M5)` that on the real DDE the `0xF4xx` PIDs return `requestOutOfRange` — live data goes through SGBD measurements.

- [ ] **Step 3: Skill update**

Add to `skills/klartext-service/SKILL.md`:
- A "Getting connected" section: NetworkManager unmanage + static link-local, `ufw allow in on <iface>`, DHCP/link-local, and the ZGW wake ritual (unplug → sleep 3–5 min → ignition ON → plug in).
- A "Whole-car" section: use `scan_ecus` → `read_all_faults` to reason about the real car; `clear_all_faults` enumerates every ECU's losses before the go-ahead (car-wide freeze-frame loss + readiness reset).
- A note that standard `F4xx` PIDs do not work on this DDE — always go SGBD.
- A note that the MCP now disconnects on exit (no manual disconnect needed on shutdown), but calling `disconnect` when done is still tidy.

- [ ] **Step 4: field-findings + standard-pids + README**

- In `docs/field-findings-2026-07-03.md`, annotate each addressed item with `✅ M10 (<tool/flag>)`; leave SVT/variant-full-autodetect and pcap-verify as open with pointers to the design's §6.
- In `docs/standard-pids.md`, add a short "Live result (2026-07-03)" note: `F40C`/`F405` → `7F 22 31` on the DDE; the scaler is correct but this ECU does not map them.
- In `README.md`, add the new MCP tools (`scan_ecus`, `read_all_faults`, `clear_all_faults`) and CLI (`scan`, `clear-faults --all-ecus`, `read-faults --all`, `--probe-timeout`, `--scan-concurrency`, `--profile-dir`).

- [ ] **Step 5: Commit**

```bash
git add CLAUDE.md skills/klartext-service/SKILL.md docs/field-findings-2026-07-03.md docs/standard-pids.md README.md
git commit -m "docs(m10): record the live-discovery invariant, connection ritual, and new surface"
```

---

## Task 13: Whole-workspace verification + finish the branch

**Files:** none (verification + history hygiene)

- [ ] **Step 1: Full workspace gates**

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings 2>&1 | tail -5
cargo test --workspace 2>&1 | tail -20
cargo test --workspace -- --ignored 2>&1 | tail -20   # real DB + real SGBD (BYO data present)
```
Expected: fmt clean, clippy clean, all tests pass (including the `#[ignore]`d real-data tests against the rebuilt DB and the DDE `.prg`).

- [ ] **Step 2: Remove the throwaway example**

```bash
rm crates/sgbd/examples/dump.rs
```
(It was a research aid; CLAUDE.md forbids leaving scratch files. Confirm nothing references it.)

- [ ] **Step 3: Drop the VIN-bearing commit from branch history**

The branch has commit `371186f "captures"` which added `captures/SESSION-2026-07-03.md` (the VIN file). Rebase it out so the VIN never rides the branch to `main`:

```bash
git log --oneline main..HEAD          # confirm 371186f is present and unmerged
git rebase --onto 371186f^ 371186f    # drop exactly that commit
git log --oneline main..HEAD          # confirm it is gone and later commits replayed
```

If the rebase conflicts (the later untrack commit touches the same file), resolve by keeping the file untracked/absent from git, then `git rebase --continue`. Verify the file is not tracked and not in history:

```bash
git log --all --oneline -- captures/SESSION-2026-07-03.md   # expect no output
git ls-files captures/                                        # expect no output
```

- [ ] **Step 4: Re-run gates after the rebase, then hand off**

```bash
cargo test --workspace 2>&1 | tail -8
```
Expected: still green. Then use `superpowers:requesting-code-review` and `superpowers:finishing-a-development-branch` to review and integrate.

---

## Self-review notes

- **Spec coverage:** §2.1 demux → T3; §2.2 fitted scan → T4/T5/T9/T11; §2.3 partition → T1/T5/T8; §2.4 whole-car procedures → T5/T9/T11; §2.5 invariant → T9/T12; §2.6 variant ladder → T7/T8; §2.7 drop aliases → T2/T6; §2.8 lifecycle → T10; §2.9 optimization → T3(no reconnect)/T8(cache); §2.10 swallowed errors → T2/T6; §2.11 VIN capture → T12/T13; §6 capture verification → T1b (scan_vin fix + constant promotion).
- **Deferred (spec §5):** SVT read, BEST-2/ABL engine, standard-PID→SGBD map — no tasks, by design.
- **Type consistency:** `EcuSlot`/`VariantInfo` (T2) used in T6/T8/T9; `ScanOptions`/`FittedEcu`/`EcuFaults`/`ClearReport` (T5) used in T9/T11; `ProbeOutcome` (T4) used in T5; `partition_faults` (T8) used in T8. `read_all_faults` `include_not_tested` removed in T9 Step 4 to avoid a behavior the client does not implement — noted inline.
- **Open live-verification items (cannot be unit-tested):** ZGW tolerance of interleaved multi-target requests (`concurrency > 1`), whole-car scan finding ~15 ECUs with no hang on absent 0x18, and the derived `2C` measurement framing — all `[verify live]`, run by the owner on the F20.
