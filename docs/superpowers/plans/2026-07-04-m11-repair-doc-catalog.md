# M11 Item 4 — ISTA repair-doc catalog (link+title) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Map a fault (DTC) to its linked ISTA documents (title, type, doc-number, safety flag) via a new offline, DB-only `fault_help` lookup, surfaced on the CLI and MCP.

**Architecture:** Extend the existing `scripts/build-semantic-db.sh` extract with two normalized tables (`fault_doc` link + `infoobject` detail) from the already-decrypted DiagDocDb; add `Catalog::fault_help` mirroring `describe_dtc` (with a `has_table` back-compat guard); expose it as an offline `fault-docs` CLI command and `fault_help` MCP tool (no car connection — pure DB read).

**Tech Stack:** Rust edition 2024, rusqlite 0.40 (bundled), rmcp (MCP), clap (CLI), the pinned SQLite3MC amalgamation (extract only). Spec: `docs/superpowers/specs/2026-07-04-m11-repair-doc-catalog-design.md`.

## Global Constraints

- **Pure DB reads, offline.** `fault_help` touches no car, no UDS, no writes — autonomous-safe, MCP-exposable, and needs no `connect` (unlike `read_fault_detail`, which reads the live ECU).
- **Degrade, never error, on absence.** No linked docs / unknown code / a semantic DB whose extract predates the tables → empty result (+ a note), never an error. Missing-table detection uses the existing `Catalog::has_table` (`SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1`).
- **Grounded schema (from DiagDocDb, verified 2026-07-04):** `RG_ECUFAULT_DOCIDS.ECUFAULT_ID = XEP_FAULTCODES.ID` (the fault-code PK); titles/type/docnumber/safety live in `XEP_INFOOBJECTS`. `RG_ECUFAULTS` does NOT exist.
- **Normalized extract:** each linked document's title is stored once in `infoobject`; `fault_doc` holds only the `(address, code) → infoobject_id` link + the preserved per-language content IDs (`content_engb`/`content_dede`) for the deferred prose layer.
- **BYO-data.** The extract reads the owner's DiagDocDb; the new tables ship only in the gitignored `data/klartext-semantic.db`. No BMW proprietary data in any committed file — same as the existing `dtc` table.
- **Title precedence:** `title_en` → `title_de` → `None`.
- **Green before done:** `cargo fmt --all` and `cargo clippy --workspace --all-targets -- -D warnings` clean; conventional commits per task.

---

## Task 1: semantic — `FaultDoc` + `Catalog::fault_help`

**Files:**
- Modify: `crates/semantic/src/catalog.rs` (add `FaultDoc`, `fault_help`, fixtures, tests)
- Modify: `crates/semantic/src/lib.rs` (export `FaultDoc`)

**Interfaces:**
- Consumes: `Catalog`, `self.conn`, `self.has_table(&str)` (exists at `catalog.rs:167`), `crate::dtc::code_number` (already imported at `catalog.rs:13`).
- Produces: `FaultDoc { infoobject_id: i64, infotype: Option<String>, docnumber: Option<String>, safety_relevant: bool, title: Option<String> }`; `Catalog::fault_help(&self, address: u8, code: [u8; 3]) -> Result<Vec<FaultDoc>, SemanticError>`.

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/semantic/src/catalog.rs`. First extend the fixture builder to add the two tables (find the existing `fixture_opts(titles: bool)` helper and add an optional docs layer — a new helper keeps it simple):

```rust
    /// Build a synthetic semantic DB with the repair-doc tables (no BMW data).
    /// `with_docs=false` reproduces a pre-item-4 extract to prove degrade-to-empty.
    fn fixture_with_docs(with_docs: bool) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sem.db");
        let conn = Connection::open(&path).unwrap();
        // Minimal ecu + dtc so resolve/describe still work alongside docs.
        conn.execute_batch(
            "CREATE TABLE ecu (address INTEGER, variant TEXT, group_name TEXT, title_en TEXT, title_de TEXT);
             INSERT INTO ecu VALUES (18, 'd72n47a0', 'd_0012', 'Engine', NULL);
             CREATE TABLE dtc (address INTEGER, ecu_variant TEXT, code INTEGER, saecode TEXT, title_en TEXT, title_de TEXT);
             INSERT INTO dtc VALUES (18, 'd72n47a0', 4923956, 'P123400', 'Glow plug', NULL);",
        )
        .unwrap();
        if with_docs {
            // fault at address 18 (0x12), code 0x4B1234 = 4923956 → two docs.
            conn.execute_batch(
                "CREATE TABLE fault_doc (address INTEGER, code INTEGER, infoobject_id INTEGER, content_engb INTEGER, content_dede INTEGER);
                 INSERT INTO fault_doc VALUES (18, 4923956, 1001, 55501, 55502);
                 INSERT INTO fault_doc VALUES (18, 4923956, 1002, 55601, 55602);
                 CREATE TABLE infoobject (id INTEGER, infotype TEXT, docnumber TEXT, safety_relevant INTEGER, title_en TEXT, title_de TEXT);
                 INSERT INTO infoobject VALUES (1001, 'FKB', 'DOC-1', 0, 'Glow plug fault', 'Gluehkerzenfehler');
                 INSERT INTO infoobject VALUES (1002, 'ABL', 'DOC-2', 1, NULL, 'Gluehkerze pruefen');",
            )
            .unwrap();
        }
        (dir, path)
    }

    #[test]
    fn fault_help_returns_linked_docs_with_title_precedence() {
        let (_d, path) = fixture_with_docs(true);
        let cat = Catalog::open(&path).unwrap();
        let docs = cat.fault_help(0x12, [0x4B, 0x12, 0x34]).unwrap();
        assert_eq!(docs.len(), 2);
        // English title preferred; safety flag off; FKB type.
        let d1 = docs.iter().find(|d| d.infoobject_id == 1001).unwrap();
        assert_eq!(d1.title.as_deref(), Some("Glow plug fault"));
        assert_eq!(d1.infotype.as_deref(), Some("FKB"));
        assert!(!d1.safety_relevant);
        // German fallback when English is NULL; safety flag on.
        let d2 = docs.iter().find(|d| d.infoobject_id == 1002).unwrap();
        assert_eq!(d2.title.as_deref(), Some("Gluehkerze pruefen"));
        assert!(d2.safety_relevant);
        assert_eq!(d2.docnumber.as_deref(), Some("DOC-2"));
    }

    #[test]
    fn fault_help_unknown_code_is_empty() {
        let (_d, path) = fixture_with_docs(true);
        let cat = Catalog::open(&path).unwrap();
        assert!(cat.fault_help(0x12, [0x00, 0x00, 0x01]).unwrap().is_empty());
    }

    #[test]
    fn fault_help_degrades_when_tables_absent() {
        // A pre-item-4 extract (no fault_doc/infoobject) → empty, not an error.
        let (_d, path) = fixture_with_docs(false);
        let cat = Catalog::open(&path).unwrap();
        assert!(cat.fault_help(0x12, [0x4B, 0x12, 0x34]).unwrap().is_empty());
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p klartext-semantic fault_help`
Expected: FAIL — `no method named fault_help`.

- [ ] **Step 3: Implement `FaultDoc` + `fault_help`**

Add the struct near `DtcDescription` in `catalog.rs`:

```rust
/// One ISTA document linked to a fault: its title, kind, and identifiers.
///
/// Sourced from `RG_ECUFAULT_DOCIDS ⋈ XEP_INFOOBJECTS` in the ISTA DiagDocDb (the
/// link+title layer — the document prose is a deferred milestone). `infoobject_id`
/// is the stable global handle the prose layer will resolve.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultDoc {
    /// The ISTA INFOOBJECT id (stable handle for the deferred prose layer).
    pub infoobject_id: i64,
    /// ISTA info type (e.g. `FKB` fault description; procedure types differ).
    pub infotype: Option<String>,
    /// ISTA document number, if present.
    pub docnumber: Option<String>,
    /// True when ISTA flags the document safety-relevant.
    pub safety_relevant: bool,
    /// The document title (English preferred, German fallback).
    pub title: Option<String>,
}
```

Add the method to `impl Catalog` (near `describe_dtc`):

```rust
    /// ISTA documents linked to a fault at `address` with raw 24-bit `code`.
    ///
    /// DB-only (no car). Returns every linked document (fault descriptions and
    /// procedures alike — distinguish by `infotype`). Empty when the fault has no
    /// linked docs, the code is unknown, or the extract predates the `fault_doc`
    /// table (a pre-item-4 DB) — the missing-table case degrades to empty, not an
    /// error.
    ///
    /// # Errors
    /// [`SemanticError::Query`] on a query failure.
    pub fn fault_help(&self, address: u8, code: [u8; 3]) -> Result<Vec<FaultDoc>, SemanticError> {
        if !self.has_table("fault_doc")? || !self.has_table("infoobject")? {
            return Ok(Vec::new()); // pre-item-4 extract — degrade to empty
        }
        let mut stmt = self.conn.prepare(
            "SELECT io.id, io.infotype, io.docnumber, io.safety_relevant, io.title_en, io.title_de \
             FROM fault_doc fd JOIN infoobject io ON io.id = fd.infoobject_id \
             WHERE fd.address = ?1 AND fd.code = ?2 \
             ORDER BY io.id",
        )?;
        let rows = stmt.query_map(
            (i64::from(address), i64::from(code_number(code))),
            |row| {
                let title_en: Option<String> = row.get(4)?;
                let title_de: Option<String> = row.get(5)?;
                let safety: Option<i64> = row.get(3)?;
                Ok(FaultDoc {
                    infoobject_id: row.get(0)?,
                    infotype: row.get(1)?,
                    docnumber: row.get(2)?,
                    safety_relevant: safety.unwrap_or(0) != 0,
                    title: title_en.or(title_de),
                })
            },
        )?;
        let mut docs = Vec::new();
        for row in rows {
            docs.push(row?);
        }
        Ok(docs)
    }
```

- [ ] **Step 4: Export the type + run the tests**

In `crates/semantic/src/lib.rs`, add `FaultDoc` to the `pub use catalog::{…}` block.
Run: `cargo test -p klartext-semantic fault_help`
Expected: PASS (3 tests).

- [ ] **Step 5: Add an ignored real-DB test (flips green once Task 2's extract runs)**

Add to the `tests` module:

```rust
    // Cross-check against the owner's real semantic DB (built with the item-4 extract).
    // Ignored by default (BYO data). Probes the extract directly with a raw read-only
    // connection (Catalog's own conn is private) so the check needs no new accessor.
    #[test]
    #[ignore = "requires BYO data: data/klartext-semantic.db built with the item-4 extract"]
    fn real_db_fault_help_has_docs() {
        use rusqlite::OpenFlags;
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/klartext-semantic.db");
        // Catalog opens cleanly (schema present)…
        let _cat = Catalog::open(&path).expect("open semantic DB");
        // …and the item-4 extract populated the link table.
        let conn =
            Connection::open_with_flags(&path, OpenFlags::SQLITE_OPEN_READ_ONLY).expect("open ro");
        let links: i64 = conn
            .query_row("SELECT COUNT(*) FROM fault_doc", [], |r| r.get(0))
            .expect("fault_doc query");
        let docs: i64 = conn
            .query_row("SELECT COUNT(*) FROM infoobject", [], |r| r.get(0))
            .expect("infoobject query");
        assert!(links > 0, "fault_doc should be populated by the item-4 extract");
        assert!(docs > 0, "infoobject should be populated by the item-4 extract");
    }
```

- [ ] **Step 6: Commit**

```bash
cargo fmt --all
git add crates/semantic/src/catalog.rs crates/semantic/src/lib.rs
git commit -m "feat(semantic): fault_help — ISTA repair-doc link+title lookup"
```

---

## Task 2: extract — `fault_doc` + `infoobject` tables

**Files:**
- Modify: `scripts/build-semantic-db.sh` (add two `CREATE TABLE … AS SELECT` + two indexes to the extract heredoc)

**Interfaces:**
- Produces: `data/klartext-semantic.db` gains `fault_doc(address, code, infoobject_id, content_engb, content_dede)` and `infoobject(id, infotype, docnumber, safety_relevant, title_en, title_de)`, matching the columns Task 1 queries.

- [ ] **Step 1: Add the extract SQL**

In `scripts/build-semantic-db.sh`, inside the `"$MC_BIN" … <<SQL … SQL` heredoc, after the `CREATE TABLE sem.envcond …` block and before the `CREATE INDEX` lines, add:

```sql
CREATE TABLE sem.fault_doc AS
  SELECT DISTINCT g.DIAGNOSTIC_ADDRESS AS address,
         CAST(fc.CODE AS INTEGER)      AS code,
         d.INFOOBJECTID                AS infoobject_id,
         d.CONTENT_ENGB                AS content_engb,
         d.CONTENT_DEDE                AS content_dede
  FROM XEP_FAULTCODES fc
  JOIN XEP_ECUVARIANTS v   ON v.ID = fc.ECUVARIANTID
  JOIN XEP_ECUGROUPS   g   ON g.ID = v.ECUGROUPID
  JOIN RG_ECUFAULT_DOCIDS d ON d.ECUFAULT_ID = fc.ID
  WHERE d.INFOOBJECTID IS NOT NULL AND g.DIAGNOSTIC_ADDRESS IS NOT NULL;
CREATE TABLE sem.infoobject AS
  SELECT DISTINCT io.ID           AS id,
         io.INFOTYPE              AS infotype,
         io.DOCNUMBER             AS docnumber,
         io.SICHERHEITSRELEVANT   AS safety_relevant,
         io.TITLE_ENGB            AS title_en,
         io.TITLE_DEDE            AS title_de
  FROM XEP_INFOOBJECTS io
  WHERE io.ID IN (SELECT INFOOBJECTID FROM RG_ECUFAULT_DOCIDS WHERE INFOOBJECTID IS NOT NULL)
    AND COALESCE(io.TITLE_ENGB, io.TITLE_DEDE) IS NOT NULL;
```

And add to the existing `CREATE INDEX` group:

```sql
CREATE INDEX sem.idx_fault_doc ON fault_doc(address, code);
CREATE INDEX sem.idx_infoobject ON infoobject(id);
```

- [ ] **Step 2: Rebuild the DB and verify the tables populate**

Run (the owner's DiagDocDb + the cached SQLite3MC are present; this rebuilds the gitignored `data/klartext-semantic.db`):

```bash
scripts/build-semantic-db.sh
```

Then verify with the bundled `sqlite3`:

```bash
sqlite3 -readonly data/klartext-semantic.db \
  "SELECT (SELECT COUNT(*) FROM fault_doc) AS links, (SELECT COUNT(*) FROM infoobject) AS docs, \
          (SELECT title_en FROM infoobject LIMIT 1) AS sample;"
```

Expected: `links` and `docs` both > 0 (design cites ~153k links). Record the printed DB size — if the file grew by more than ~2× (was 33 MB), note it and, per the spec, trim `infoobject` to fault-relevant `infotype`s in a follow-up (do NOT silently cap).

- [ ] **Step 3: Confirm the size is reasonable**

Run: `ls -lh data/klartext-semantic.db`
Expected: within ~+15–30 MB of the prior 33 MB (i.e. ≲ ~65 MB). If far larger, stop and report (a trim decision is the owner's).

- [ ] **Step 4: Flip the ignored real-DB test green (local check)**

Run: `cargo test -p klartext-semantic real_db_fault_help_has_docs -- --ignored`
Expected: PASS (the extract populated `fault_doc`). If it fails to compile due to a missing `conn()` accessor, use the alternative assertion form noted in Task 1 Step 5.

- [ ] **Step 5: Commit**

```bash
git add scripts/build-semantic-db.sh
git commit -m "feat(extract): fault_doc + infoobject tables (fault→ISTA doc link+title)"
```

(The rebuilt `data/klartext-semantic.db` is gitignored — do NOT commit it.)

---

## Task 3: mcp — `fault_help` tool (offline)

**Files:**
- Modify: `mcp/src/dto.rs` (`FaultDocDto`, `FaultHelpResult`)
- Modify: `mcp/src/server.rs` (the `fault_help` tool)
- Modify: `mcp/tests/integration.rs` (a fixture-DB test)

**Interfaces:**
- Consumes: `self.catalog() -> Option<Catalog>` (`server.rs:106`), `ecu::resolve(spec, catalog) -> Result<u8, String>` (`mcp/src/ecu.rs:21`), `Catalog::{describe_dtc, fault_help}`, `parse_dtc_code` (used by `read_fault_detail`), the existing `FaultDescription` DTO (`dto.rs:85`).
- Produces: MCP tool `fault_help(FaultHelpRequest { ecu, code }) -> Json<FaultHelpResult>`.

- [ ] **Step 1: Add the DTOs**

In `mcp/src/dto.rs`:

```rust
/// One ISTA document linked to a fault (link+title layer).
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FaultDocDto {
    pub title: Option<String>,
    pub infotype: Option<String>,
    pub docnumber: Option<String>,
    pub safety_relevant: bool,
    /// Stable ISTA INFOOBJECT id — the handle the deferred prose layer will resolve.
    pub infoobject_id: i64,
}

/// Arguments for `fault_help`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct FaultHelpRequest {
    /// ECU as hex address (e.g. `0x12`), ISTA group name, or variant name.
    pub ecu: String,
    /// The 3-byte DTC as hex, e.g. `4B1234` (a `code_hex` from read_faults).
    pub code: String,
}

/// Result of `fault_help`: the fault text plus its linked ISTA documents.
#[derive(Debug, Serialize, schemars::JsonSchema)]
pub struct FaultHelpResult {
    pub ecu: String,
    pub code_hex: String,
    pub descriptions: Vec<FaultDescription>,
    pub docs: Vec<FaultDocDto>,
    pub note: String,
}
```

- [ ] **Step 2: Write the failing integration test**

In `mcp/tests/integration.rs`, mirror an existing DB-only test (the crate already builds fixture semantic DBs for tests — reuse that helper; if the helper is per-test, add a small one creating the `ecu`/`dtc`/`fault_doc`/`infoobject` tables as in Task 1's `fixture_with_docs`). Assert `fault_help` returns the linked docs WITHOUT any `connect` call:

```rust
    #[tokio::test]
    async fn fault_help_returns_linked_docs_offline() {
        // A server with a fixture semantic DB, no car connection.
        let server = server_with_semantic_db(fixture_db_with_docs()); // reuse/add helper
        let out = server
            .fault_help(Parameters(FaultHelpRequest {
                ecu: "0x12".into(),
                code: "4B1234".into(),
            }))
            .await
            .unwrap();
        let r = out.0;
        assert_eq!(r.code_hex, "4B1234");
        assert_eq!(r.docs.len(), 2);
        assert!(r.docs.iter().any(|d| d.title.as_deref() == Some("Glow plug fault")));
    }
```

Run: `cargo test -p klartext-mcp fault_help` → FAIL (`no method fault_help`).

- [ ] **Step 3: Implement the tool**

In `mcp/src/server.rs`, add (import `FaultDocDto`, `FaultHelpRequest`, `FaultHelpResult` in the `use dto::{…}` block):

```rust
    /// Look up a fault's ISTA documentation — its meaning and linked repair procedures.
    ///
    /// DB-only: needs NO car connection (unlike read_fault_detail). Resolves the ECU
    /// and DTC, returns the ISTA fault text plus the titles/types of every linked ISTA
    /// document. The document prose itself is a deferred layer — this returns the
    /// pointers (title, type, doc number, safety flag, stable id).
    #[tool(
        description = "Look up an ISTA fault's meaning and its linked repair/diagnosis \
        documents by ECU + code — WITHOUT connecting to the car (pure semantic-DB read). \
        Pass `ecu` (hex like 0x12, group name, or variant) and `code` (the 3-byte DTC hex \
        from read_faults, e.g. 4B1234). Returns the fault text plus each linked ISTA \
        document's title, type (FKB = fault description; others are procedures), doc \
        number, and safety flag. The document prose is not yet extracted — this is the \
        title/pointer layer."
    )]
    pub async fn fault_help(
        &self,
        Parameters(req): Parameters<FaultHelpRequest>,
    ) -> Result<Json<FaultHelpResult>, McpError> {
        let catalog = self.catalog();
        let address = ecu::resolve(&req.ecu, catalog.as_ref())
            .map_err(|e| McpError::invalid_params(e, None))?;
        let dtc = parse_dtc_code(&req.code).map_err(|e| McpError::invalid_params(e, None))?;

        let descriptions = catalog
            .as_ref()
            .and_then(|c| c.describe_dtc(address, dtc).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|d| FaultDescription {
                variant: d.ecu_variant,
                saecode: d.saecode,
                text: d.title_en.or(d.title_de),
            })
            .collect();

        let docs: Vec<FaultDocDto> = catalog
            .as_ref()
            .and_then(|c| c.fault_help(address, dtc).ok())
            .unwrap_or_default()
            .into_iter()
            .map(|d| FaultDocDto {
                title: d.title,
                infotype: d.infotype,
                docnumber: d.docnumber,
                safety_relevant: d.safety_relevant,
                infoobject_id: d.infoobject_id,
            })
            .collect();

        let note = if catalog.is_none() {
            "No semantic DB — build it (scripts/build-semantic-db.sh) for fault docs.".to_string()
        } else if docs.is_empty() {
            "No ISTA documents linked to this fault (or the DB predates the repair-doc \
             extract — rebuild it).".to_string()
        } else {
            format!(
                "{} ISTA document(s) linked; prose bodies are not yet extracted (title layer only).",
                docs.len()
            )
        };

        Ok(Json(FaultHelpResult {
            ecu: req.ecu,
            code_hex: format!("{:02X}{:02X}{:02X}", dtc[0], dtc[1], dtc[2]),
            descriptions,
            docs,
            note,
        }))
    }
```

- [ ] **Step 4: Run the test**

Run: `cargo test -p klartext-mcp fault_help`
Expected: PASS.
Also update the exact-tool-surface test if one exists (the crate has a test asserting the tool list — add `fault_help` to its expected set).

- [ ] **Step 5: Commit**

```bash
cargo fmt --all
git add mcp/src/dto.rs mcp/src/server.rs mcp/tests/integration.rs
git commit -m "feat(mcp): fault_help tool — offline ISTA repair-doc lookup"
```

---

## Task 4: cli — `fault-docs` command (offline)

**Files:**
- Modify: `cli/src/main.rs` (a `FaultDocs` command + printer)

**Interfaces:**
- Consumes: `open_catalog(&cli.semantic_db) -> Option<Catalog>` (`main.rs:768`), the global `--target: u8` (`main.rs:56`), `parse_dtc_arg` (used by `FaultDetail`), `Catalog::{describe_dtc, fault_help}`, the existing `print_fault_descriptions` printer (`main.rs:953`).
- Produces: `klartext fault-docs <code>` (offline; uses `--target` for the ECU address, like `fault-detail`).

- [ ] **Step 1: Add the command variant**

In the `Command` enum in `cli/src/main.rs`, after `FaultDetail`:

```rust
    /// Look up a fault's ISTA docs (meaning + linked procedures) — offline, no car.
    ///
    /// Pure semantic-DB read: needs `--target <ecu hex>` and the DTC code, no
    /// connection. Prints the fault text plus each linked ISTA document's title,
    /// type, doc number, and safety flag. Document prose is a deferred layer.
    FaultDocs {
        /// The 3-byte DTC as hex, e.g. 4B1234 (a code from `read-faults`).
        #[arg(value_parser = parse_dtc_arg)]
        code: [u8; 3],
    },
```

- [ ] **Step 2: Add the match arm (offline — no connect)**

Find where commands are dispatched. `FaultDocs` must NOT connect (unlike `FaultDetail`). Mirror the offline `service list` path (which uses the catalog/SGBD without a car). Add:

```rust
        Command::FaultDocs { code } => {
            let catalog = open_catalog(&cli.semantic_db);
            print_fault_descriptions(catalog.as_ref(), cli.target, code);
            match catalog.as_ref().map(|c| c.fault_help(cli.target, code)) {
                Some(Ok(docs)) if !docs.is_empty() => {
                    println!("\nISTA documents ({}):", docs.len());
                    for d in &docs {
                        let title = d.title.as_deref().unwrap_or("(untitled)");
                        let ty = d.infotype.as_deref().unwrap_or("-");
                        let num = d.docnumber.as_deref().unwrap_or("-");
                        let safety = if d.safety_relevant { "  [safety-relevant]" } else { "" };
                        println!("  [{ty}] {title}  (doc {num}, id {}){safety}", d.infoobject_id);
                    }
                    println!("\n(Document prose is a deferred layer — titles/pointers only.)");
                }
                Some(Ok(_)) => println!("\nNo ISTA documents linked to this fault."),
                Some(Err(e)) => eprintln!("fault-doc lookup failed: {e}"),
                None => println!(
                    "\nNo semantic DB — build it (scripts/build-semantic-db.sh) for fault docs."
                ),
            }
            Ok(())
        }
```

(If the dispatch is an `async` match that expects a connection for every arm, place `FaultDocs` alongside the other offline arms — e.g. `Discover`/`Service List` — so it runs without `connect`.)

- [ ] **Step 3: Build + smoke-check**

Run: `cargo build -p klartext-cli`
Run: `cargo run -p klartext-cli -- --help` → confirm `fault-docs` is listed.
Run (offline, against the rebuilt DB from Task 2): `cargo run -p klartext-cli -- --target 0x12 fault-docs 4B1234`
Expected: prints the fault text + linked ISTA docs (or "No ISTA documents linked" for a code with none). No connection attempted.

- [ ] **Step 4: Commit**

```bash
cargo fmt --all
git add cli/src/main.rs
git commit -m "feat(cli): fault-docs — offline ISTA repair-doc lookup"
```

---

## Task 5: workspace green + docs

**Files:**
- Modify: `README.md`, `docs/sqlite-findings.md`, `CLAUDE.md`

- [ ] **Step 1: Full workspace check**

Run: `cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings && cargo test --workspace`
Expected: fmt clean, no clippy warnings, all tests pass (the real-DB tests stay `#[ignore]`).

- [ ] **Step 2: Document the finding + surface**

- `docs/sqlite-findings.md`: record the confirmed fault→doc bridge (`RG_ECUFAULT_DOCIDS.ECUFAULT_ID = XEP_FAULTCODES.ID`, `RG_ECUFAULTS` absent), that titles/type/docnumber/safety live in `XEP_INFOOBJECTS` (no 50 GB prose DB needed for the link+title layer), and that the prose bodies (`xmlvalueprimitive_DEDE`, keyed by the preserved `content_*` IDs) are a deferred layer.
- `README.md`: document the offline `fault-docs` CLI command and `fault_help` MCP tool; bump the MCP tool count; note both need no car connection.
- `CLAUDE.md`: add a one-line M11 note — item 4 (repair-doc catalog, link+title layer) done: offline `fault_help`/`fault-docs`, prose deferred; AND record that item 3 (embed-data) is deferred to the mobile milestone.

- [ ] **Step 3: Commit**

```bash
git add README.md docs/sqlite-findings.md CLAUDE.md
git commit -m "docs(m11): record repair-doc catalog + fault_help surface; note item-3 deferral"
```

---

## Self-Review

**Spec coverage:**
- Extract `fault_doc` + `infoobject` (normalized, content IDs preserved) → Task 2. ✓
- `Catalog::fault_help` mirroring `describe_dtc`, `has_table` degrade → Task 1. ✓
- Offline CLI `fault-docs` → Task 4; offline MCP `fault_help` → Task 3. ✓
- Degrade paths (no docs / no DB / old extract / unknown code) → Task 1 tests + Task 3/4 notes. ✓
- Blast radius (pure DB reads, no car) → global constraints; both surfaces are offline. ✓
- BYO-data (DB gitignored, no proprietary data committed) → Task 2 Step 5. ✓
- Size guard (~+15–30 MB, trim-if-large) → Task 2 Steps 2–3. ✓

**Placeholder scan:** Task 1 Step 5's real-DB test has a documented fallback (if no `conn()` accessor) — an explicit branch, not a hand-wave. No `TODO`/"add error handling"/"similar to Task N" elsewhere.

**Type consistency:** `FaultDoc { infoobject_id, infotype, docnumber, safety_relevant, title }` (Task 1) → mapped to `FaultDocDto` (Task 3) and printed (Task 4) with the same field names. `fault_help(address, code)` signature identical across Tasks 1/3/4. The extract columns (Task 2) exactly match the `SELECT` in `fault_help` (Task 1): `fault_doc(address, code, infoobject_id, content_engb, content_dede)`, `infoobject(id, infotype, docnumber, safety_relevant, title_en, title_de)`. ✓
