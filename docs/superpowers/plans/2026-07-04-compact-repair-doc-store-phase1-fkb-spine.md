# Compact Repair-Doc Store — Phase 1: FKB Spine — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Render every fault's ISTA FKB description sheet (meaning + `MASSNAHMEIMSERVICE` service measure) to compact German markdown, store it in a new self-built `klartext-docs.db`, and surface it offline through `fault-docs` (CLI) and `fault_help` (MCP).

**Architecture:** A new build-only binary `klartext-docbuild` reads the (already-built, plaintext) `klartext-semantic.db` for the fault-linked FKB `content_dede` pointers and the plaintext `xmlvalueprimitive_DEDE.sqlite` for the bodies, renders XML→markdown, gzips each, and writes a sibling `klartext-docs.db` (`fkb_body` table). At runtime `Catalog` opens that sibling DB read-only if present and adds `fault_body(address, code)`; the CLI/MCP fault surfaces print the body. No SQLite3MC and no encrypted DB are touched in this phase — the FKB pointers already live in `semantic.db` from M11 Item 4, and the bodies are plaintext.

**Tech Stack:** Rust (edition 2024), rusqlite 0.40 (bundled SQLite), quick-xml (streaming XML), flate2 (gzip), clap + anyhow (binary boundary), tempfile (test fixtures). Full spec: `docs/superpowers/specs/2026-07-04-compact-repair-doc-store-design.md`.

## Global Constraints

- **Edition/toolchain:** edition `2024`, latest stable Rust. Add all dependencies with `cargo add` (never hand-write versions). License `AGPL-3.0-only`.
- **Workspace layout:** library crates under `crates/`; each binary in its own top-level dir. New binary package name keeps the `klartext-` prefix (`klartext-docbuild`), dir is short (`docbuild/`).
- **BYO-data (hard rule):** never commit or embed ISTA data or a VIN. `data/` is gitignored. All test fixtures use **synthetic** German-ish text only — never real ISTA bodies. The encrypted `DiagDocDb` + SQLite3MC + password stay confined to `scripts/build-semantic-db.sh`; this phase does not use them.
- **Runtime reads only plaintext, self-built DBs.** `klartext-semantic` opens everything **read-only** and never writes, embeds, or copies DB contents.
- **Graceful degradation:** a missing `klartext-docs.db` or a pre-Phase-1 `semantic.db` must degrade to today's behavior (pointers/titles only), never error.
- **Errors:** `thiserror` in libraries, `anyhow` at the binary boundary. `cargo fmt` and `cargo clippy --all-targets -- -D warnings` clean before the phase is done. Run `cargo fmt` via Bash (the Edit hook uses an older rustfmt — see memory `rustfmt-hook-mismatch`). Conventional commits.
- **Language:** bodies are German (this ISTA install has no English prose DB). Titles remain multilingual (from Item 4).

---

## File Structure

- **Create `docbuild/Cargo.toml`** — new build-only binary package `klartext-docbuild`.
- **Create `docbuild/src/main.rs`** — CLI entry (clap args: semantic-db path, xmlvalue path, out path), orchestration.
- **Create `docbuild/src/fkb.rs`** — the FKB XML→markdown renderer (pure, unit-tested). Build-only, single-caller → lives in the binary crate (YAGNI; extract to a lib only when a second caller appears).
- **Create `docbuild/src/build.rs`** — the extract+render+gzip+write pipeline (rusqlite over two plaintext DBs → `klartext-docs.db`).
- **Modify root `Cargo.toml`** — add `"docbuild"` to `workspace.members`.
- **Modify `crates/semantic/Cargo.toml`** — add `flate2` (runtime gunzip).
- **Modify `crates/semantic/src/catalog.rs`** — open a sibling `klartext-docs.db` if present; add `fault_body`.
- **Modify `crates/semantic/src/lib.rs`** — no new public types needed (fault_body returns `Vec<String>`); re-export nothing new. (Left explicit so the implementer does not invent exports.)
- **Modify `cli/src/main.rs`** — `FaultDocs` handler prints the rendered body above the existing pointer list.
- **Modify `mcp/src/dto.rs`** — add `body: Vec<String>` to `FaultHelpResult`.
- **Modify `mcp/src/server.rs`** — populate `body` in the `fault_help` handler.
- **Modify `scripts/build-semantic-db.sh`** — after building `semantic.db`, invoke `klartext-docbuild` to produce `klartext-docs.db`.
- **Modify `docs/sqlite-findings.md`** — record the Phase 1 fkb_body layer (short note).

---

## Task 1: Scaffold the `klartext-docbuild` binary crate

**Files:**
- Create: `docbuild/Cargo.toml`, `docbuild/src/main.rs`
- Modify: `Cargo.toml` (workspace members)

**Interfaces:**
- Produces: a compiling `klartext-docbuild` binary with a `--help`; no logic yet.

- [ ] **Step 1: Scaffold via cargo (writes a current-edition manifest)**

Run:
```bash
cargo new --bin docbuild --name klartext-docbuild
```
Then add `"docbuild"` to `members` in the root `Cargo.toml` if `cargo new` did not (verify with `rg 'docbuild' Cargo.toml`). The members line should read:
```toml
members = ["crates/uds", "crates/hsfz", "crates/client", "cli", "crates/semantic", "mcp", "crates/sgbd", "docbuild"]
```

- [ ] **Step 2: Add dependencies via CLI (never hand-write versions)**

Run:
```bash
cargo add -p klartext-docbuild anyhow clap --features clap/derive
cargo add -p klartext-docbuild rusqlite --features bundled
cargo add -p klartext-docbuild quick-xml
cargo add -p klartext-docbuild flate2
```

- [ ] **Step 3: Minimal clap entry**

Replace `docbuild/src/main.rs` with:
```rust
//! klartext-docbuild — build the compact repair-doc store (`klartext-docs.db`)
//! from the plaintext semantic DB (FKB pointers) and ISTA's plaintext
//! `xmlvalueprimitive_DEDE.sqlite` (bodies). Build-only, BYO-data: reads the
//! user's own decrypted data, writes a gitignored artifact, embeds nothing.
mod build;
mod fkb;

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;

/// Build klartext-docs.db (Phase 1: FKB fault-description bodies).
#[derive(Parser)]
#[command(version, about)]
struct Args {
    /// The plaintext semantic DB (from scripts/build-semantic-db.sh).
    #[arg(long, default_value = "data/klartext-semantic.db")]
    semantic_db: PathBuf,
    /// ISTA's plaintext German prose DB (xmlvalueprimitive_DEDE.sqlite).
    #[arg(long)]
    xmlvalue_db: PathBuf,
    /// Output doc-store DB (sibling of the semantic DB at runtime).
    #[arg(long, default_value = "data/klartext-docs.db")]
    out: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let n = build::build_fkb(&args.semantic_db, &args.xmlvalue_db, &args.out)?;
    eprintln!("klartext-docs.db: wrote {n} FKB bodies → {}", args.out.display());
    Ok(())
}
```

- [ ] **Step 4: Stub the modules so it compiles**

Create `docbuild/src/fkb.rs`:
```rust
//! FKB (Fehlerkennblatt) XML → compact German markdown renderer.
```
Create `docbuild/src/build.rs`:
```rust
//! Extract FKB bodies → render → gzip → write klartext-docs.db.
use std::path::Path;

use anyhow::Result;

/// Build the `fkb_body` table. Returns the number of bodies written.
pub fn build_fkb(_semantic_db: &Path, _xmlvalue_db: &Path, _out: &Path) -> Result<usize> {
    Ok(0)
}
```

- [ ] **Step 5: Verify it builds**

Run: `cargo build -p klartext-docbuild`
Expected: compiles clean.

- [ ] **Step 6: Commit**

```bash
cargo fmt
git add Cargo.toml docbuild/
git commit -m "feat(docbuild): scaffold klartext-docbuild binary (Phase 1 FKB spine)"
```

---

## Task 2: FKB renderer — sections to markdown

**Files:**
- Modify: `docbuild/src/fkb.rs`

**Interfaces:**
- Produces: `pub fn render_fkb(xml: &str) -> Result<String, FkbError>` and `pub enum FkbError`. Consumed by Task 4 (`build.rs`). Returns rendered markdown; empty-section elements are dropped; `MASSNAHMEIMSERVICE` renders under heading "Maßnahme im Service".

FKB bodies are `<FKB LANGUAGE="de-DE">` trees. Known *content* sections directly hold `<PARAGRAPH>` text and may be nested inside grouping elements (`FEHLERBESCHREIBUNG`, `UEBERWACHUNGSBEDINGUNG`). The renderer walks all elements; when it enters a known section it emits its German heading and the paragraph text collected until the section closes (paragraphs belonging to a nested known section are attributed to that nested section). No links, no graphics in FKB.

- [ ] **Step 1: Write the failing test (synthetic XML — no ISTA data)**

Add to `docbuild/src/fkb.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;

    // Synthetic FKB body — invented German-ish text, not ISTA content.
    const SAMPLE: &str = r#"<FKB LANGUAGE="de-DE">
      <FEHLERBESCHREIBUNG>
        <BESCHREIBUNG><PARAGRAPH/></BESCHREIBUNG>
        <SETZBEDINGUNG>
          <PARAGRAPH>Beispiel: Fehler wird bei Kurzschluss erkannt.</PARAGRAPH>
          <PARAGRAPH>Zweiter Absatz.</PARAGRAPH>
        </SETZBEDINGUNG>
      </FEHLERBESCHREIBUNG>
      <ZEITBEDINGUNG><PARAGRAPH>Mindestens 2 s.</PARAGRAPH></ZEITBEDINGUNG>
      <MASSNAHMEIMSERVICE><PARAGRAPH>Steuergeraet pruefen.</PARAGRAPH></MASSNAHMEIMSERVICE>
    </FKB>"#;

    #[test]
    fn renders_known_sections_in_order_dropping_empty() {
        let md = render_fkb(SAMPLE).unwrap();
        // Empty BESCHREIBUNG dropped; three non-empty sections, in document order.
        assert_eq!(
            md,
            "## Setzbedingung\n\nBeispiel: Fehler wird bei Kurzschluss erkannt.\n\nZweiter Absatz.\n\n\
             ## Zeitbedingung\n\nMindestens 2 s.\n\n\
             ## Maßnahme im Service\n\nSteuergeraet pruefen."
        );
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p klartext-docbuild fkb::tests::renders_known_sections -- --nocapture`
Expected: FAIL — `render_fkb` not found.

- [ ] **Step 3: Implement the renderer**

Prepend to `docbuild/src/fkb.rs` (above the tests):
```rust
use quick_xml::events::Event;
use quick_xml::Reader;
use thiserror::Error;

/// A failure rendering an FKB body.
#[derive(Debug, Error)]
pub enum FkbError {
    /// The XML could not be parsed.
    #[error("parsing FKB XML: {0}")]
    Xml(#[from] quick_xml::Error),
}

/// Known FKB content sections → German markdown heading, in a stable render
/// order. Grouping elements (FEHLERBESCHREIBUNG/UEBERWACHUNGSBEDINGUNG) are NOT
/// listed — only the leaf sections that carry paragraph text.
const SECTIONS: &[(&str, &str)] = &[
    ("FEHLERBESCHREIBUNG", "Fehlerbeschreibung"),
    ("BESCHREIBUNG", "Beschreibung"),
    ("SETZBEDINGUNG", "Setzbedingung"),
    ("SPANNUNGSBEDINGUNG", "Spannungsbedingung"),
    ("FAHRZUSTAND", "Fahrzustand"),
    ("ZEITBEDINGUNG", "Zeitbedingung"),
    ("MASSNAHMEIMSERVICE", "Maßnahme im Service"),
    ("FEHLERAUSWIRKUNG", "Fehlerauswirkung"),
    ("SICHTBAREAUSWIRKUNG", "Sichtbare Auswirkung"),
    ("PANNENHINWEIS", "Pannenhinweis"),
    ("FAHRERINFORMATION", "Fahrerinformation"),
    ("WARNLEUCHTE", "Warnleuchte"),
    ("SERVICEHINWEIS", "Servicehinweis"),
    ("FEHLERORTTEXT", "Fehlerort"),
];

fn heading_for(tag: &str) -> Option<&'static str> {
    SECTIONS.iter().find(|(el, _)| *el == tag).map(|(_, h)| *h)
}

/// Render an FKB body to compact German markdown: one `## Heading` per non-empty
/// known section, paragraphs blank-line separated, empty sections dropped.
pub fn render_fkb(xml: &str) -> Result<String, FkbError> {
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut blocks: Vec<String> = Vec::new(); // rendered "## H\n\npara\n\npara"
    // Stack of (heading, paragraphs) for the currently-open known sections.
    let mut open: Vec<(&'static str, Vec<String>)> = Vec::new();
    let mut in_paragraph = false;
    let mut para = String::new();

    loop {
        match reader.read_event()? {
            Event::Start(e) => {
                let name = e.name();
                let tag = String::from_utf8_lossy(name.as_ref()).to_string();
                if tag == "PARAGRAPH" {
                    in_paragraph = true;
                    para.clear();
                } else if let Some(h) = heading_for(&tag) {
                    open.push((h, Vec::new()));
                }
            }
            Event::Empty(_) => { /* e.g. <PARAGRAPH/> — no text, ignored */ }
            Event::Text(t) => {
                if in_paragraph {
                    para.push_str(&t.unescape()?);
                }
            }
            Event::End(e) => {
                let name = e.name();
                let tag = String::from_utf8_lossy(name.as_ref()).to_string();
                if tag == "PARAGRAPH" {
                    in_paragraph = false;
                    let text = para.trim();
                    if !text.is_empty() && let Some((_, paras)) = open.last_mut() {
                        paras.push(text.to_string());
                    }
                } else if heading_for(&tag).is_some() && let Some((h, paras)) = open.pop() {
                    if !paras.is_empty() {
                        blocks.push(format!("## {h}\n\n{}", paras.join("\n\n")));
                    }
                }
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(blocks.join("\n\n"))
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p klartext-docbuild fkb::tests::renders_known_sections -- --nocapture`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add docbuild/src/fkb.rs
git commit -m "feat(docbuild): FKB section renderer (XML -> German markdown)"
```

---

## Task 3: FKB renderer — KLEMME, CCMELDUNG, and list flattening

**Files:**
- Modify: `docbuild/src/fkb.rs`

**Interfaces:**
- Extends `render_fkb`: `KLEMME` → "**Klemme:** \<name\> — \<status\>"; `CCMELDUNG` → its own "## Check-Control-Meldung" block; `LIST`/`LISTELEMENT` inside a section → `-` bullets.

- [ ] **Step 1: Write the failing tests**

Add to the `tests` module in `docbuild/src/fkb.rs`:
```rust
#[test]
fn renders_klemme_and_ccmeldung() {
    let xml = r#"<FKB LANGUAGE="de-DE">
      <UEBERWACHUNGSBEDINGUNG>
        <KLEMME><KLEMMENNAME>Klemme 15</KLEMMENNAME><KLEMMENSTATUS>an</KLEMMENSTATUS></KLEMME>
      </UEBERWACHUNGSBEDINGUNG>
      <CCMELDUNG><PARAGRAPH>Beispielhinweis im Display.</PARAGRAPH></CCMELDUNG>
    </FKB>"#;
    let md = render_fkb(xml).unwrap();
    assert_eq!(
        md,
        "**Klemme:** Klemme 15 — an\n\n\
         ## Check-Control-Meldung\n\nBeispielhinweis im Display."
    );
}

#[test]
fn flattens_lists_to_bullets() {
    let xml = r#"<FKB LANGUAGE="de-DE">
      <MASSNAHMEIMSERVICE>
        <LIST><LISTELEMENT><PARAGRAPH>Erster Schritt.</PARAGRAPH></LISTELEMENT>
        <LISTELEMENT><PARAGRAPH>Zweiter Schritt.</PARAGRAPH></LISTELEMENT></LIST>
      </MASSNAHMEIMSERVICE>
    </FKB>"#;
    let md = render_fkb(xml).unwrap();
    assert_eq!(
        md,
        "## Maßnahme im Service\n\n- Erster Schritt.\n- Zweiter Schritt."
    );
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p klartext-docbuild fkb::tests -- --nocapture`
Expected: the two new tests FAIL (KLEMME/CCMELDUNG unhandled; list paragraphs not bulleted).

- [ ] **Step 3: Extend the renderer**

In `docbuild/src/fkb.rs`:

(a) Add `CCMELDUNG` to `SECTIONS` (after `SERVICEHINWEIS`):
```rust
    ("CCMELDUNG", "Check-Control-Meldung"),
```

(b) Track list depth and KLEMME parts. Replace the render loop's state declarations and the `Start`/`Text`/`End` arms so that: inside a `LIST`, each paragraph becomes a `- ` bullet; a `KLEMME` collects `KLEMMENNAME`/`KLEMMENSTATUS` text and emits a standalone `**Klemme:** name — status` block. Concretely, add state:
```rust
    let mut list_depth: usize = 0;
    let mut klemme: Option<(String, String)> = None; // (name, status)
    let mut klemme_field: Option<bool> = None; // Some(true)=name, Some(false)=status
```
In the `Start` arm, before the `heading_for` branch, handle:
```rust
                match tag.as_str() {
                    "LIST" => list_depth += 1,
                    "KLEMME" => klemme = Some((String::new(), String::new())),
                    "KLEMMENNAME" => klemme_field = Some(true),
                    "KLEMMENSTATUS" => klemme_field = Some(false),
                    _ => {}
                }
```
In the `Text` arm, also capture KLEMME field text:
```rust
                if let (Some(field), Some((name, status))) = (klemme_field, klemme.as_mut()) {
                    let dst = if field { name } else { status };
                    dst.push_str(&t.unescape()?);
                }
```
In the `End` arm, handle list/klemme close and make paragraph flushing list-aware:
```rust
                match tag.as_str() {
                    "LIST" => { list_depth = list_depth.saturating_sub(1); }
                    "KLEMMENNAME" | "KLEMMENSTATUS" => klemme_field = None,
                    "KLEMME" => {
                        if let Some((name, status)) = klemme.take() {
                            let (name, status) = (name.trim(), status.trim());
                            if !name.is_empty() {
                                let line = if status.is_empty() {
                                    format!("**Klemme:** {name}")
                                } else {
                                    format!("**Klemme:** {name} — {status}")
                                };
                                blocks.push(line);
                            }
                        }
                    }
                    _ => {}
                }
```
And change the `PARAGRAPH` end handling to prefix a bullet when inside a list:
```rust
                if tag == "PARAGRAPH" {
                    in_paragraph = false;
                    let text = para.trim();
                    if !text.is_empty() && let Some((_, paras)) = open.last_mut() {
                        let rendered = if list_depth > 0 { format!("- {text}") } else { text.to_string() };
                        paras.push(rendered);
                    }
                }
```
When joining bulleted paragraphs, bullets should be newline-separated, not blank-line separated. Adjust the section flush: join with `"\n"` when every paragraph starts with `"- "`, else `"\n\n"`:
```rust
                } else if heading_for(&tag).is_some() && let Some((h, paras)) = open.pop() {
                    if !paras.is_empty() {
                        let bulleted = paras.iter().all(|p| p.starts_with("- "));
                        let sep = if bulleted { "\n" } else { "\n\n" };
                        blocks.push(format!("## {h}\n\n{}", paras.join(sep)));
                    }
                }
```
(KLEMME lines are pushed directly to `blocks` as standalone lines, matching the expected output where `**Klemme:** …` is its own block.)

- [ ] **Step 4: Run all renderer tests**

Run: `cargo test -p klartext-docbuild fkb`
Expected: all PASS (including Task 2's).

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add docbuild/src/fkb.rs
git commit -m "feat(docbuild): FKB KLEMME/CCMELDUNG/list rendering"
```

---

## Task 4: Build pipeline — extract, render, gzip, write `klartext-docs.db`

**Files:**
- Modify: `docbuild/src/build.rs`

**Interfaces:**
- Consumes: `crate::fkb::render_fkb`.
- Produces: `pub fn build_fkb(semantic_db: &Path, xmlvalue_db: &Path, out: &Path) -> Result<usize>`. Writes table `fkb_body(content_dede INTEGER PRIMARY KEY, body_md_gz BLOB)`. Returns count.

Extraction: FKB `content_dede` ids = `SELECT DISTINCT fd.content_dede FROM fault_doc fd JOIN infoobject io ON io.id = fd.infoobject_id WHERE io.infotype='FKB' AND fd.content_dede IS NOT NULL` on the semantic DB. Bodies: from `xmlvalueprimitive_DEDE`, build a `c0(global id)→rowid` map once (`SELECT id, c0 FROM xmlvalueprimitive_content`), then fetch each body by rowid PK (`SELECT c3 FROM xmlvalueprimitive_content WHERE id=?`). Gzip each rendered markdown (flate2, default level).

- [ ] **Step 1: Write the failing test (synthetic fixture DBs — no ISTA data)**

Replace `docbuild/src/build.rs` body-less stub tests by adding at the bottom:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use flate2::read::GzDecoder;
    use rusqlite::Connection;
    use std::io::Read;

    fn synth_semantic(path: &std::path::Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE fault_doc (address INT, code INT, infoobject_id INT, content_engb INT, content_dede INT);
             CREATE TABLE infoobject (id INT, infotype TEXT, docnumber TEXT, safety_relevant INT, title_en TEXT, title_de TEXT);
             INSERT INTO fault_doc VALUES (18, 4919860, 1001, 7001, 7002);
             INSERT INTO fault_doc VALUES (18, 4919860, 1002, 7003, 7004);
             INSERT INTO infoobject VALUES (1001,'FKB','D1',0,'t','t');
             INSERT INTO infoobject VALUES (1002,'ABL','D2',0,'t','t');",
        ).unwrap();
    }
    // Mimic the ISTA FTS5 shadow table shape: id=rowid PK, c0=global id, c3=body.
    fn synth_xmlvalue(path: &std::path::Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE TABLE xmlvalueprimitive_content (id INTEGER PRIMARY KEY, c0 TEXT, c3 TEXT);
             INSERT INTO xmlvalueprimitive_content VALUES
               (1,'7002','<FKB LANGUAGE=\"de-DE\"><MASSNAHMEIMSERVICE><PARAGRAPH>Steuergeraet pruefen.</PARAGRAPH></MASSNAHMEIMSERVICE></FKB>');",
        ).unwrap();
    }

    #[test]
    fn builds_fkb_bodies_only_for_fkb_docs() {
        let dir = tempfile::tempdir().unwrap();
        let sem = dir.path().join("semantic.db");
        let xml = dir.path().join("xmlvalue.db");
        let out = dir.path().join("docs.db");
        synth_semantic(&sem);
        synth_xmlvalue(&xml);

        let n = build_fkb(&sem, &xml, &out).unwrap();
        assert_eq!(n, 1); // only the FKB doc's content_dede=7002 has a body

        let c = Connection::open(&out).unwrap();
        let blob: Vec<u8> = c
            .query_row("SELECT body_md_gz FROM fkb_body WHERE content_dede=7002", [], |r| r.get(0))
            .unwrap();
        let mut md = String::new();
        GzDecoder::new(&blob[..]).read_to_string(&mut md).unwrap();
        assert_eq!(md, "## Maßnahme im Service\n\nSteuergeraet pruefen.");
        // The ABL doc's content_dede (7004) is not present.
        let missing: rusqlite::Result<i64> =
            c.query_row("SELECT 1 FROM fkb_body WHERE content_dede=7004", [], |r| r.get(0));
        assert!(missing.is_err());
    }
}
```
Add the test-only dep:
```bash
cargo add -p klartext-docbuild --dev tempfile
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p klartext-docbuild build::tests::builds_fkb_bodies_only_for_fkb_docs`
Expected: FAIL — `build_fkb` returns 0 / no table.

- [ ] **Step 3: Implement the pipeline**

Replace `docbuild/src/build.rs` (keep the `tests` module) with:
```rust
//! Extract FKB bodies → render → gzip → write klartext-docs.db.
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::write::GzEncoder;
use flate2::Compression;
use rusqlite::{Connection, OpenFlags};

use crate::fkb::render_fkb;

/// Build the `fkb_body` table in `out`. Returns the number of bodies written.
pub fn build_fkb(semantic_db: &Path, xmlvalue_db: &Path, out: &Path) -> Result<usize> {
    let sem = Connection::open_with_flags(semantic_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening semantic DB {}", semantic_db.display()))?;
    let xmlv = Connection::open_with_flags(xmlvalue_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening xmlvalue DB {}", xmlvalue_db.display()))?;

    // Wanted FKB content ids (German bodies).
    let mut stmt = sem.prepare(
        "SELECT DISTINCT fd.content_dede \
         FROM fault_doc fd JOIN infoobject io ON io.id = fd.infoobject_id \
         WHERE io.infotype = 'FKB' AND fd.content_dede IS NOT NULL",
    )?;
    let wanted: Vec<i64> = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<_>>()?;

    // Global-id (c0, TEXT) → rowid PK, built once (c0 is unindexed).
    let mut map_stmt = xmlv.prepare("SELECT id, c0 FROM xmlvalueprimitive_content")?;
    let mut id_of: HashMap<String, i64> = HashMap::new();
    let rows = map_stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    for row in rows {
        let (rowid, c0) = row?;
        id_of.insert(c0, rowid);
    }

    // Fresh output.
    if out.exists() {
        std::fs::remove_file(out).with_context(|| format!("removing old {}", out.display()))?;
    }
    let docs = Connection::open(out)?;
    docs.execute_batch(
        "CREATE TABLE fkb_body (content_dede INTEGER PRIMARY KEY, body_md_gz BLOB NOT NULL);",
    )?;
    let tx = docs.unchecked_transaction()?;
    let mut body_stmt = xmlv.prepare("SELECT c3 FROM xmlvalueprimitive_content WHERE id = ?1")?;
    let mut written = 0usize;
    for content_dede in wanted {
        let Some(&rowid) = id_of.get(&content_dede.to_string()) else {
            continue; // pointer with no body in this install — skip, not an error
        };
        let xml: String = body_stmt.query_row([rowid], |r| r.get(0))?;
        let md = render_fkb(&xml).context("rendering FKB body")?;
        if md.is_empty() {
            continue;
        }
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(md.as_bytes())?;
        let gz = enc.finish()?;
        tx.execute(
            "INSERT OR REPLACE INTO fkb_body (content_dede, body_md_gz) VALUES (?1, ?2)",
            rusqlite::params![content_dede, gz],
        )?;
        written += 1;
    }
    tx.commit()?;
    Ok(written)
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p klartext-docbuild build`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add docbuild/
git commit -m "feat(docbuild): FKB extract/render/gzip pipeline -> klartext-docs.db"
```

---

## Task 5: Runtime — `Catalog` opens the sibling docs DB and adds `fault_body`

**Files:**
- Modify: `crates/semantic/Cargo.toml` (add flate2)
- Modify: `crates/semantic/src/catalog.rs`

**Interfaces:**
- Consumes: `code_number(code)` (existing, `crate::dtc`).
- Produces: `pub fn fault_body(&self, address: u8, code: [u8; 3]) -> Result<Vec<String>, SemanticError>` on `Catalog` — the rendered FKB markdown body/bodies for the fault (usually one), empty when there is no docs DB, no FKB body, or the code is unknown.

Location: `Catalog::open` derives a sibling `klartext-docs.db` (same directory as the semantic DB) and opens it read-only if it exists, storing `docs: Option<Connection>`. A missing docs DB → `None` → `fault_body` returns empty. Bodies are gunzipped with flate2.

- [ ] **Step 1: Add the dependency**

Run: `cargo add -p klartext-semantic flate2`

- [ ] **Step 2: Write the failing test**

Add to the `tests` module in `crates/semantic/src/catalog.rs` (reuse the `fixture_with_docs` helper's DB dir so the docs DB is a sibling):
```rust
#[test]
fn fault_body_reads_rendered_markdown_from_sibling_docs_db() {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    // Build a semantic DB with a fault → FKB content pointer, plus a sibling
    // klartext-docs.db holding the gzipped rendered body (synthetic text).
    let dir = TempDir::new().unwrap();
    let sem = dir.path().join("klartext-semantic.db");
    let conn = Connection::open(&sem).unwrap();
    conn.execute_batch(
        "CREATE TABLE ecu (address INTEGER, variant TEXT, group_name TEXT, title_en TEXT, title_de TEXT);
         CREATE TABLE dtc (address INTEGER, ecu_variant TEXT, code INTEGER, saecode TEXT, title_en TEXT, title_de TEXT);
         CREATE TABLE fault_doc (address INTEGER, code INTEGER, infoobject_id INTEGER, content_engb INTEGER, content_dede INTEGER);
         CREATE TABLE infoobject (id INTEGER, infotype TEXT, docnumber TEXT, safety_relevant INTEGER, title_en TEXT, title_de TEXT);
         INSERT INTO fault_doc VALUES (18, 4919860, 1001, 7001, 7002);
         INSERT INTO infoobject VALUES (1001,'FKB','D1',0,'t','t');",
    ).unwrap();
    let docs = Connection::open(dir.path().join("klartext-docs.db")).unwrap();
    docs.execute_batch("CREATE TABLE fkb_body (content_dede INTEGER PRIMARY KEY, body_md_gz BLOB NOT NULL);").unwrap();
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(b"## Ma\xC3\x9fnahme im Service\n\nSteuergeraet pruefen.").unwrap();
    let gz = enc.finish().unwrap();
    docs.execute("INSERT INTO fkb_body VALUES (7002, ?1)", [gz]).unwrap();

    let cat = Catalog::open(&sem).unwrap();
    let bodies = cat.fault_body(0x12, [0x4B, 0x12, 0x34]).unwrap(); // 0x4B1234 = 4919860
    assert_eq!(bodies.len(), 1);
    assert!(bodies[0].contains("Maßnahme im Service"));
    assert!(bodies[0].contains("Steuergeraet pruefen"));
}

#[test]
fn fault_body_without_docs_db_is_empty() {
    // fixture_with_docs writes only the semantic DB — no sibling docs DB.
    let (_d, path) = fixture_with_docs(true);
    let cat = Catalog::open(&path).unwrap();
    assert!(cat.fault_body(0x12, [0x4B, 0x12, 0x34]).unwrap().is_empty());
}
```

- [ ] **Step 3: Run to verify they fail**

Run: `cargo test -p klartext-semantic fault_body`
Expected: FAIL — `fault_body` not found.

- [ ] **Step 4: Add the docs connection and method**

In `crates/semantic/src/catalog.rs`:

(a) Add the field to the struct:
```rust
pub struct Catalog {
    conn: Connection,
    docs: Option<Connection>,
}
```

(b) In `open`, after building `conn`, derive and open the sibling docs DB:
```rust
        let docs = path
            .parent()
            .map(|dir| dir.join("klartext-docs.db"))
            .filter(|p| p.exists())
            .and_then(|p| {
                Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY).ok()
            });
        Ok(Self { conn, docs })
```

(c) Add the method (place after `fault_help`):
```rust
    /// Rendered FKB fault-description markdown for the fault at `address` with raw
    /// 24-bit `code`. Reads the sibling `klartext-docs.db` (Phase 1 doc store).
    ///
    /// Returns the German markdown body/bodies (usually one). Empty when there is
    /// no docs DB, no FKB body for the fault, or the code is unknown — never an
    /// error for the missing-store case.
    ///
    /// # Errors
    /// [`SemanticError::Query`] on a query failure, or if a stored body is not
    /// valid gzip/UTF-8 (a corrupt store).
    pub fn fault_body(
        &self,
        address: u8,
        code: [u8; 3],
    ) -> Result<Vec<String>, SemanticError> {
        let Some(docs) = self.docs.as_ref() else {
            return Ok(Vec::new());
        };
        // FKB content ids linked to this fault (via the semantic DB's fault_doc).
        let mut stmt = self.conn.prepare(
            "SELECT DISTINCT fd.content_dede \
             FROM fault_doc fd JOIN infoobject io ON io.id = fd.infoobject_id \
             WHERE fd.address = ?1 AND fd.code = ?2 \
               AND io.infotype = 'FKB' AND fd.content_dede IS NOT NULL",
        )?;
        let ids: Vec<i64> = stmt
            .query_map((i64::from(address), i64::from(code_number(code))), |r| r.get(0))?
            .collect::<rusqlite::Result<_>>()?;

        let mut bodies = Vec::new();
        let mut body_stmt =
            docs.prepare("SELECT body_md_gz FROM fkb_body WHERE content_dede = ?1")?;
        for id in ids {
            let gz: Option<Vec<u8>> = body_stmt
                .query_row([id], |r| r.get(0))
                .optional()?;
            if let Some(gz) = gz {
                bodies.push(gunzip_utf8(&gz)?);
            }
        }
        Ok(bodies)
    }
```

(d) Add the helper (module-private, near the bottom of the file, before `#[cfg(test)]`):
```rust
/// Gunzip a stored body blob to a UTF-8 string. A decode failure means a corrupt
/// store, surfaced as a query error rather than a panic.
fn gunzip_utf8(gz: &[u8]) -> Result<String, SemanticError> {
    use std::io::Read;
    let mut out = String::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_string(&mut out)
        .map_err(|e| {
            SemanticError::Query(rusqlite::Error::ToSqlConversionFailure(Box::new(e)))
        })?;
    Ok(out)
}
```

(Note: `OptionalExtension` is already imported in this file for `.optional()`.)

- [ ] **Step 5: Run the tests**

Run: `cargo test -p klartext-semantic fault_body`
Expected: both PASS.

- [ ] **Step 6: Verify no regressions**

Run: `cargo test -p klartext-semantic`
Expected: all existing tests still PASS.

- [ ] **Step 7: Commit**

```bash
cargo fmt
git add crates/semantic/
git commit -m "feat(semantic): Catalog::fault_body reads sibling klartext-docs.db"
```

---

## Task 6: CLI — `fault-docs` prints the rendered body

**Files:**
- Modify: `cli/src/main.rs` (the `Command::FaultDocs` handler, ~line 225)

**Interfaces:**
- Consumes: `Catalog::fault_body`. No new public surface.

- [ ] **Step 1: Print the body above the pointer list**

In `cli/src/main.rs`, in the `Command::FaultDocs { code } => {` arm, after `print_fault_descriptions(...)` and before the `match catalog.as_ref().map(|c| c.fault_help(...))`, insert:
```rust
            match catalog.as_ref().map(|c| c.fault_body(cli.target, *code)) {
                Some(Ok(bodies)) if !bodies.is_empty() => {
                    for body in &bodies {
                        println!("\n{body}");
                    }
                }
                Some(Err(e)) => eprintln!("fault-docs body lookup failed: {e}"),
                _ => {} // no docs store or no body — the pointer list below still prints
            }
```
Then update the trailing note that currently claims prose is deferred. Change:
```rust
                    println!("\n(Document prose is a deferred layer — titles/pointers only.)");
```
to:
```rust
                    println!("\n(FKB fault-description prose shown above when built; procedure prose is a later phase.)");
```

- [ ] **Step 2: Build and smoke-check the help/handler compiles**

Run: `cargo build -p klartext-cli`
Expected: compiles clean.

- [ ] **Step 3: Manual offline check (no car) — degrades cleanly without a docs DB**

Run: `cargo run -p klartext-cli -- --target 12 fault-docs 4B1234 || true`
Expected: prints fault descriptions/pointers (or the "No semantic DB" note); does **not** panic when `klartext-docs.db` is absent.

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add cli/src/main.rs
git commit -m "feat(cli): fault-docs prints rendered FKB body"
```

---

## Task 7: MCP — `fault_help` returns the rendered body

**Files:**
- Modify: `mcp/src/dto.rs` (`FaultHelpResult`, ~line 581)
- Modify: `mcp/src/server.rs` (`fault_help` handler, ~line 495)

**Interfaces:**
- Consumes: `Catalog::fault_body`.
- Produces: `FaultHelpResult.body: Vec<String>` (rendered FKB markdown; empty when no store/body).

- [ ] **Step 1: Add the `body` field to the result DTO**

In `mcp/src/dto.rs`, in the `FaultHelpResult` struct (the one holding `pub docs: Vec<FaultDocDto>`), add a field with a doc comment matching the file's style, e.g.:
```rust
    /// The rendered FKB fault-description prose (German markdown), when the doc
    /// store is built. Empty otherwise — the `docs` pointers still apply.
    pub body: Vec<String>,
```
If `FaultHelpResult` derives `schemars::JsonSchema`/`Serialize` (match the existing derives on the struct), the new `Vec<String>` field needs no extra attributes.

- [ ] **Step 2: Populate it in the handler**

In `mcp/src/server.rs`, in `fault_help` (~line 495), where the result is assembled with `docs`, also compute and include the body. After the `let docs: Vec<FaultDocDto> = …;` block, add:
```rust
        let body = catalog
            .as_ref()
            .and_then(|c| c.fault_body(address, dtc).ok())
            .unwrap_or_default();
```
and add `body` to the returned `FaultHelpResult { … }` initializer.

- [ ] **Step 3: Build and run the MCP tests**

Run: `cargo test -p klartext-mcp`
Expected: compiles; existing tests PASS (add a body assertion only if the existing `fault_help` test already builds a docs DB — otherwise the field defaults to empty, which is correct).

- [ ] **Step 4: Verify stdout hygiene is untouched**

Confirm no `println!`/stdout writes were added in `mcp/` (stdio JSON-RPC transport — all logging is stderr via `tracing`). Run: `rg -n 'println!|print!' mcp/src` → expect no new hits.

- [ ] **Step 5: Commit**

```bash
cargo fmt
git add mcp/src/dto.rs mcp/src/server.rs
git commit -m "feat(mcp): fault_help returns rendered FKB body"
```

---

## Task 8: Build script wiring + docs note

**Files:**
- Modify: `scripts/build-semantic-db.sh`
- Modify: `docs/sqlite-findings.md`

**Interfaces:**
- After `semantic.db` is built, invoke `klartext-docbuild` to produce the sibling `klartext-docs.db` from `semantic.db` + `xmlvalueprimitive_DEDE.sqlite`.

- [ ] **Step 1: Locate the xmlvalue DB and invoke docbuild**

At the end of `scripts/build-semantic-db.sh` (after the final `echo "Done. …"`), append:
```bash
# Phase 1 doc store: render fault-description (FKB) bodies into a sibling
# klartext-docs.db. Reads only plaintext DBs (the semantic extract above + ISTA's
# xmlvalueprimitive_DEDE); no SQLite3MC needed here. BYO-data: output is gitignored.
XMLVALUE="${KLARTEXT_XMLVALUE_DEDE:-$(dirname "$SRC")/xmlvalueprimitive_DEDE.sqlite}"
DOCS_OUT="$(dirname "$OUT")/klartext-docs.db"
if [ -f "$XMLVALUE" ]; then
	echo "Building doc store (FKB bodies) → $DOCS_OUT …"
	cargo run --quiet --release -p klartext-docbuild -- \
		--semantic-db "$OUT" --xmlvalue-db "$XMLVALUE" --out "$DOCS_OUT"
else
	echo "note: $XMLVALUE not found — skipping doc store (pointers/titles still work)." >&2
fi
```

- [ ] **Step 2: Shellcheck / syntax**

Run: `bash -n scripts/build-semantic-db.sh`
Expected: no syntax errors. (If `shellcheck` is available: `shellcheck scripts/build-semantic-db.sh` — no new errors.)

- [ ] **Step 3: Record the layer in findings**

In `docs/sqlite-findings.md`, under the Phase/Update section area, add a short note (2–4 lines): the Phase 1 FKB prose layer renders `xmlvalueprimitive_DEDE` bodies (keyed by `fault_doc.content_dede`, filtered to `infotype='FKB'`) to gzipped German markdown in `klartext-docs.db(fkb_body)`, built by `klartext-docbuild`; no SQLite3MC/encrypted DB involved (pointers already in `semantic.db`).

- [ ] **Step 4: Commit**

```bash
git add scripts/build-semantic-db.sh docs/sqlite-findings.md
git commit -m "build(docbuild): wire FKB doc-store build into build-semantic-db.sh"
```

---

## Task 9: Real-DB smoke test (ignored, BYO-data) + phase verification

**Files:**
- Modify: `crates/semantic/src/catalog.rs` (add one `#[ignore]` test)

**Interfaces:** none (test only).

- [ ] **Step 1: Add an ignored smoke test asserting structure only**

Add to the `tests` module in `crates/semantic/src/catalog.rs`:
```rust
// Smoke test of the Phase 1 FKB body layer against the real BYO-data store.
// Ignored by default; run with `--ignored` after building klartext-docs.db.
// Asserts structure only — no ISTA text is embedded in the repo.
#[test]
#[ignore = "requires data/klartext-semantic.db + data/klartext-docs.db (run scripts/build-semantic-db.sh)"]
fn real_db_fault_body_renders_for_a_known_fault() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/klartext-semantic.db");
    let cat = Catalog::open(&path).unwrap();
    // Pick any fault that has an FKB doc; assert we get non-empty rendered prose.
    // (Replace address/code with a known one from `fault-docs` output on the real DB.)
    let bodies = cat.fault_body(0x40, [0xD9, 0x04, 0x0A]).unwrap();
    assert!(
        bodies.iter().any(|b| !b.trim().is_empty()),
        "expected rendered FKB prose for a known fault"
    );
}
```

- [ ] **Step 2: Full workspace verification**

Run:
```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test --workspace
```
Expected: fmt clean, clippy clean, all non-ignored tests PASS.

- [ ] **Step 3: (Manual, owner-run) Build the real store and run the ignored test**

This step is the owner's — it needs BYO data. Document it in the commit body; do not claim it passed:
```bash
scripts/build-semantic-db.sh
cargo test -p klartext-semantic real_db_fault_body -- --ignored
cargo run -p klartext-cli -- --target 40 fault-docs D9040A   # eyeball the prose
```

- [ ] **Step 4: Commit**

```bash
cargo fmt
git add crates/semantic/src/catalog.rs
git commit -m "test(semantic): ignored real-DB smoke test for FKB body layer"
```

---

## Self-Review

**Spec coverage (Phase 1 slice of §16):**
- FKB renderer → Tasks 2–3. ✅
- `fkb_body` extract + gzip + store → Task 4. ✅
- `fault_body` runtime API + sibling docs DB → Task 5. ✅
- CLI `fault-docs` shows body → Task 6. ✅
- MCP `fault_help` returns body → Task 7. ✅
- Build wiring + BYO-data confinement (no SQLite3MC this phase) → Task 8. ✅
- German-only bodies, graceful degradation, read-only → constraints honored across Tasks 5–8. ✅
- Out of Phase 1 (later phases, not gaps): FTS/search, REP legacy/MOD, `VALIDITYINFO` evaluator, graphics pipeline, FUB/TED/SSP/tier-2, storage split into `klartext-graphics.db`. These are separate plans per the spec's phasing.

**Placeholder scan:** No "TBD"/"handle errors"/"similar to". Every code step shows complete code. The one intentionally variable value — the real fault code in the ignored smoke test (Task 9) — is explicitly flagged for the owner to fill from real output; it does not block the default suite.

**Type consistency:** `render_fkb(&str) -> Result<String, FkbError>` (Tasks 2–4 consumer). `build_fkb(&Path,&Path,&Path) -> Result<usize>` (Tasks 1, 4, 8). `Catalog::fault_body(u8,[u8;3]) -> Result<Vec<String>, SemanticError>` (Tasks 5, 6, 7). `FaultHelpResult.body: Vec<String>` (Task 7). `Catalog.docs: Option<Connection>` (Task 5). Names/signatures match across tasks.

---

## Execution Handoff

Two execution options:
1. **Subagent-Driven (recommended)** — a fresh subagent per task, two-stage review between tasks.
2. **Inline Execution** — batch execution in this session with checkpoints.

Nine tasks, each ending in a green, committed, independently-testable deliverable; Task 1 has no dependencies, Tasks 2–5 are sequential (renderer → build → runtime), Tasks 6–7 depend on Task 5, Task 8 on Task 4, Task 9 last.
