# M11 Item 4 — ISTA repair-doc catalog (link + title layer) design

**Date:** 2026-07-04 · **Status:** design; implementation not started.
**Roadmap parent:** `docs/superpowers/specs/2026-07-03-m11-ista-parity-roadmap.md` **§3** (the
repair-doc catalog), which is CLAUDE.md's milestone **item 4** (CLAUDE.md lists the deferred
embed-data feature at item 3; the roadmap doc numbers it §5 — the two orderings differ).
**Pattern:** extends the existing `scripts/build-semantic-db.sh` extract + `Catalog` the same
way the `dtc` / `envcond` tables were added. Fully offline (DB-only) — no car interaction.

## 0. Scope (decided with the owner)

**In:** the **link + title** layer — map a fault (DTC) to the ISTA documents linked to it, with
each document's **title, type, doc-number, and safety flag**. Surfaced as an offline `fault_help`
lookup so the agent/user can say "this fault links to ISTA docs 'Sensor dejustiert' (fault
description) and '<procedure>' (diagnosis)."

**Out (deferred to a later milestone):** the actual document **prose** (the step-by-step body).
The bodies live in `xmlvalueprimitive_DEDE.sqlite` (**50 GB**, plaintext FTS5), keyed by the
per-language content IDs (`RG_ECUFAULT_DOCIDS.CONTENT_DEDE/ENGB`). Rendering them needs an
XML→text renderer + a 50 GB runtime dependency; not worth coupling into this milestone. The
extract will **preserve the content IDs** so the prose layer can be added later without re-deriving.

**Also out:** component→document links (`XEP_REFINFOOBJECTS` for non-fault entities), guided
procedures (Item 5), and the `streamdataprimitive` PDFs.

## 1. Established facts (grounded against DiagDocDb, 2026-07-04)

Read directly from the owner's `DiagDocDb.sqlite` via the pinned SQLite3MC (rc4, password
`6505EFBDC3E5F324` — the ISTAGUI.exe public-key-token; the same decrypt the extract already uses).
Facts only; no DB content committed.

- **The link is direct and richly populated.** `RG_ECUFAULT_DOCIDS.ECUFAULT_ID` **=**
  `XEP_FAULTCODES.ID` (join yields **153,103** rows; **149,574** distinct faults have ≥1 doc).
  `RG_ECUFAULTS` does **not** exist — the decompiled class name was a red herring; the bridge is
  the direct fault-code primary key.
- **Titles + metadata are in DiagDocDb, not the 50 GB prose DB.** `XEP_INFOOBJECTS` carries
  `TITLE_DEDE`/`TITLE_ENGB`, `INFOTYPE` (e.g. `FKB` = fault description; diagnosis-procedure types
  distinct), `DOCNUMBER`, `IDENTIFIER`, and `SICHERHEITSRELEVANT` (safety-relevant flag). So the
  link+title layer needs **only** DiagDocDb — the DB the extract already decrypts.
- **Relevant tables:** `RG_ECUFAULT_DOCIDS` (fault→INFOOBJECTID + per-language content IDs),
  `XEP_INFOOBJECTS` (the document: title/type/docnumber/safety), `XEP_FAULTCODES`
  (`ID`, `CODE`, `ECUVARIANTID`), `XEP_ECUVARIANTS`/`XEP_ECUGROUPS` (address bridge — already used).
- **Fault-code kinds:** many linked docs are `FKB` fault descriptions ("what this fault means");
  others are diagnosis procedures ("how to check/fix"). The MVP surfaces **all** linked docs with
  their `infotype`, so the caller distinguishes them.

## 2. Data extract (`scripts/build-semantic-db.sh`)

Add two tables, **normalized** so a document linked to many faults stores its title once (keeps the
DB growth modest). Same immutable-source, empty-key-output, `CREATE TABLE … AS SELECT` shape as the
existing `dtc`/`ecu`/`envcond` extracts.

```sql
-- The fault → document link, keyed like the dtc table: (address, raw 24-bit code) → doc id.
-- Content IDs are preserved (unused now) so the deferred prose layer needs no re-derive.
CREATE TABLE sem.fault_doc AS
  SELECT DISTINCT g.DIAGNOSTIC_ADDRESS AS address,
         CAST(fc.CODE AS INTEGER)      AS code,
         d.INFOOBJECTID                AS infoobject_id,
         d.CONTENT_ENGB                AS content_engb,   -- global id into xmlvalueprimitive_ENGB (prose, later)
         d.CONTENT_DEDE                AS content_dede
  FROM XEP_FAULTCODES fc
  JOIN XEP_ECUVARIANTS v   ON v.ID = fc.ECUVARIANTID
  JOIN XEP_ECUGROUPS   g   ON g.ID = v.ECUGROUPID
  JOIN RG_ECUFAULT_DOCIDS d ON d.ECUFAULT_ID = fc.ID
  WHERE d.INFOOBJECTID IS NOT NULL;

-- Each linked document once: title, type, doc number, safety flag.
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

CREATE INDEX sem.idx_fault_doc ON fault_doc(address, code);
CREATE INDEX sem.idx_infoobject ON infoobject(id);
```

**Size:** ~153k link rows (small ints) + the distinct-linked-doc subset of `XEP_INFOOBJECTS`
(titles). Measured during implementation; expected **+~15–30 MB** on the 33 MB DB. If it proves
larger than ~2× the current DB, trim to fault-relevant `infotype`s (documented, not silent).

**Compat:** older extracts without these tables still load — `fault_help` degrades to empty when
the tables are absent (checked via `sqlite_master`, like the M10 title-column back-compat).

## 3. `klartext-semantic` — the lookup (`catalog.rs`)

Mirrors `describe_dtc`: a DB-only, read-only query keyed by `(address, code)`.

```rust
/// One ISTA document linked to a fault: its title, kind, and identifiers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FaultDoc {
    /// The ISTA INFOOBJECT id (stable global id; the handle for the deferred prose layer).
    pub infoobject_id: i64,
    /// ISTA info type (e.g. "FKB" fault description; diagnosis-procedure types differ).
    pub infotype: Option<String>,
    /// ISTA document number, if present.
    pub docnumber: Option<String>,
    /// True when ISTA flags the document safety-relevant.
    pub safety_relevant: bool,
    /// The document title (English preferred, German fallback).
    pub title: Option<String>,
}

impl Catalog {
    /// ISTA documents linked to a fault at `address` with raw 24-bit `code`.
    ///
    /// DB-only (no car). Returns every linked document (fault descriptions and
    /// procedures alike — distinguish by `infotype`); an empty vec when the fault
    /// has no linked docs, the code is unknown, or the extract predates the tables.
    ///
    /// # Errors
    /// [`SemanticError::Query`] on a query failure (a *missing table* is not an
    /// error — it degrades to empty, checked once via `sqlite_master`).
    pub fn fault_help(&self, address: u8, code: [u8; 3]) -> Result<Vec<FaultDoc>, SemanticError>;
}
```

`code` bridges to the DB's integer code via the existing `code_number` helper (as `describe_dtc`
does). Title precedence: `title_en` → `title_de` → `None`. Reuse the existing NULL-hardening.

## 4. Surface — offline `fault_help` (CLI + MCP)

Both are **DB-only, no connection required** — you can look up a bare code's meaning + procedures
without the car (unlike `read_fault_detail`, which reads the live ECU). Pure reads,
autonomous-safe.

- **CLI** `klartext fault-docs <ecu> <code>` — resolves the ECU (hex/name/variant via the existing
  `resolve`), decodes the fault text (existing `describe_dtc`), then prints the linked ISTA docs as
  a table: `type · title · doc# · [safety]`. No `--target`/connection needed (offline).
- **MCP** `fault_help(ecu, code)` → `FaultHelpResult { ecu, code_hex, descriptions, docs: Vec<FaultDocDto>, notes }`.
  Offline (DB-only); one of the few tools that needs no `connect`. Autonomous read.

MCP DTO (`mcp/src/dto.rs`):
```rust
pub struct FaultDocDto {
    pub title: Option<String>,
    pub infotype: Option<String>,
    pub docnumber: Option<String>,
    pub safety_relevant: bool,
    pub infoobject_id: i64,   // stable handle; the deferred prose layer will resolve it
}
pub struct FaultHelpResult {
    pub ecu: String, pub code_hex: String,
    pub descriptions: Vec<FaultDescription>,   // reuse the existing fault-text DTO
    pub docs: Vec<FaultDocDto>,
    pub notes: Vec<String>,                    // e.g. "N ISTA docs linked; prose bodies not yet extracted"
}
```

**Optional (nice-to-have, in scope only if cheap):** add a `doc_count` hint to `read_faults` /
`read_fault_detail` ("3 ISTA docs — call fault_help"). Keep it a hint, not the full list, to avoid
bloating the fault scan. Deferrable if it complicates the fault DTOs.

## 5. Degrade paths (never error on absence)
- No linked docs for the code → empty `docs` (+ note "no ISTA docs linked to this fault").
- No semantic DB, or an older extract without `fault_doc`/`infoobject` → empty + note "repair-doc
  catalog not available (rebuild the semantic DB)". Detected once via `sqlite_master`.
- Unknown ECU/code → empty (mirrors `describe_dtc`).
- A doc with no title → still listed by `infotype`/`docnumber` (title `None`).

## 6. Blast radius / safety
Pure **DB reads** — no UDS, no car, no writes. Autonomous-safe and MCP-exposable with room to
spare (safer than every existing read, which at least touch the car). No change to any car path.

## 7. Testing (offline, hermetic)
- **build script:** manual, BYO — run against the owner's `DiagDocDb`; assert the `fault_doc`/
  `infoobject` tables populate and a known fault resolves to a titled doc. (Same discipline as the
  existing extract; not in CI.)
- **semantic:** `fault_help` over a synthetic DB fixture (`fault_doc` + `infoobject` rows) — a fault
  with two docs (one `FKB`, one procedure) returns both with titles; an unknown code → empty; a
  fixture DB *without* the tables → empty (back-compat), not an error. Title precedence + NULL
  hardening covered. `#[ignore]`d real-DB test asserts a known fault has linked docs.
- **mcp/cli:** `fault_help`/`fault-docs` over the fixture DB (offline — no mock car needed).
- `cargo fmt` + `cargo clippy --workspace --all-targets -- -D warnings` clean.

## 8. File-by-file change list
- `scripts/build-semantic-db.sh` — add the `fault_doc` + `infoobject` tables + indexes (§2).
- `crates/semantic/src/catalog.rs` — `FaultDoc` + `fault_help` + `sqlite_master` table-presence
  guard + fixture rows + tests.
- `crates/semantic/src/lib.rs` — export `FaultDoc`.
- `mcp/src/dto.rs` — `FaultDocDto` + `FaultHelpResult`.
- `mcp/src/server.rs` — `fault_help` tool (offline; resolve ecu → describe_dtc + fault_help).
- `mcp/tests/integration.rs` — `fault_help` test over a fixture DB.
- `cli/src/main.rs` — `fault-docs <ecu> <code>` subcommand + printer.
- `README.md` — document `fault-docs` / `fault_help` (offline lookup) + the tool-count bump.
- `docs/sqlite-findings.md` — record the confirmed `RG_ECUFAULT_DOCIDS.ECUFAULT_ID = XEP_FAULTCODES.ID`
  bridge, the title-in-DiagDocDb finding, and the deferred prose layer (content IDs preserved).
- `CLAUDE.md` — one-line M11-item-4 note (repair-doc link+title layer done; prose deferred).
