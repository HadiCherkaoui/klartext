//! Extract FKB bodies → render → gzip → write klartext-docs.db.
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use rusqlite::{Connection, OpenFlags, OptionalExtension};

use crate::fkb::render_fkb;
use crate::rep::render_rep;

/// Fetch one document body by its ISTA content id.
///
/// The store is an FTS5 table whose shadow `xmlvalueprimitive_content` holds the
/// content id in `c0` and the body in `c3`. `c0` carries no b-tree index, so the
/// obvious `WHERE c0 = ?` scans a 50 GB table — and this runs once per document.
/// FTS5's own term index DOES cover the `id` column, so a column-filtered `MATCH`
/// resolves the same row from the index: milliseconds instead of a full scan. The
/// `c0` equality is kept as an exactness guard over the (tiny) match set, so a
/// tokenizer surprise can never substitute a different document.
///
/// `None` when this install has no body for the id — a normal skip, not an error.
fn body_for(
    stmt: &mut rusqlite::Statement<'_>,
    content_id: i64,
) -> rusqlite::Result<Option<String>> {
    stmt.query_row(
        rusqlite::params![format!("id:{content_id}"), content_id.to_string()],
        |row| row.get(0),
    )
    .optional()
}

/// The body lookup [`body_for`] runs. Prepared once per pass.
const BODY_SQL: &str = "SELECT c.c3 FROM xmlvalueprimitive_content c \
     WHERE c.rowid IN (SELECT rowid FROM xmlvalueprimitive WHERE xmlvalueprimitive MATCH ?1) \
       AND c.c0 = ?2";

/// Build the `fkb_body` table in `out`, returning the number of bodies written.
///
/// Reads FKB `content_dede` pointers from `semantic_db`, fetches each German
/// body from `xmlvalue_db`, renders it to compact markdown, gzips it, and
/// writes one `fkb_body` row per non-empty rendered body. A pointer with no
/// matching body in this install is skipped, not an error.
///
/// # Errors
///
/// Returns an error if an input DB cannot be opened or queried, if the output
/// DB cannot be created or written, or if an FKB body fails to render.
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

    // Fresh output.
    if out.exists() {
        std::fs::remove_file(out).with_context(|| format!("removing old {}", out.display()))?;
    }
    let docs = Connection::open(out)?;
    docs.execute_batch(
        "CREATE TABLE fkb_body (content_dede INTEGER PRIMARY KEY, body_md_gz BLOB NOT NULL);",
    )?;
    let tx = docs.unchecked_transaction()?;
    let mut body_stmt = xmlv.prepare(BODY_SQL)?;
    let mut written = 0usize;
    for content_dede in wanted {
        let Some(xml) = body_for(&mut body_stmt, content_dede)? else {
            continue; // pointer with no body in this install — skip, not an error
        };
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

/// Build the `repair_body` table in `out`, returning the number of bodies written.
///
/// The repair families (`REP` instructions, `EBO` component locations, `SWZ`
/// special tools) are what makes a fault actionable — `fault_help` can say what a
/// code means, but only these say what to do about it. They are read from the
/// `repair_doc` extract rather than `fault_doc`, because they are indexed by
/// component and procedure, not by fault code.
///
/// Appends to the SAME store [`build_fkb`] creates, so it must run after it.
/// A pointer with no matching body in this install is skipped, not an error.
///
/// # Errors
///
/// Returns an error if an input DB cannot be opened or queried, if the output DB
/// cannot be written, or if a body fails to render.
pub fn build_repair(semantic_db: &Path, xmlvalue_db: &Path, out: &Path) -> Result<usize> {
    let sem = Connection::open_with_flags(semantic_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening semantic DB {}", semantic_db.display()))?;
    // A DB built before the repair_doc extract simply has nothing to render.
    let has_table: bool = sem
        .prepare("SELECT 1 FROM sqlite_master WHERE type='table' AND name='repair_doc'")?
        .exists([])?;
    if !has_table {
        return Ok(0);
    }
    let xmlv = Connection::open_with_flags(xmlvalue_db, OpenFlags::SQLITE_OPEN_READ_ONLY)
        .with_context(|| format!("opening xmlvalue DB {}", xmlvalue_db.display()))?;

    let mut stmt =
        sem.prepare("SELECT DISTINCT content_dede FROM repair_doc WHERE content_dede IS NOT NULL")?;
    let wanted: Vec<i64> = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<rusqlite::Result<_>>()?;

    let docs = Connection::open(out)?;
    docs.execute_batch(
        "CREATE TABLE IF NOT EXISTS repair_body \
         (content_dede INTEGER PRIMARY KEY, body_md_gz BLOB NOT NULL);",
    )?;
    let tx = docs.unchecked_transaction()?;
    let mut body_stmt = xmlv.prepare(BODY_SQL)?;
    let mut written = 0usize;
    for content_dede in wanted {
        let Some(xml) = body_for(&mut body_stmt, content_dede)? else {
            continue;
        };
        // One malformed document must not lose the other 169,618: skip it loudly.
        let md = match render_rep(&xml) {
            Ok(md) => md,
            Err(error) => {
                eprintln!("skipping repair body {content_dede}: {error}");
                continue;
            }
        };
        if md.is_empty() {
            continue;
        }
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(md.as_bytes())?;
        let gz = enc.finish()?;
        tx.execute(
            "INSERT OR REPLACE INTO repair_body (content_dede, body_md_gz) VALUES (?1, ?2)",
            rusqlite::params![content_dede, gz],
        )?;
        written += 1;
    }
    tx.commit()?;
    Ok(written)
}

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
    /// A REAL FTS5 store, matching ISTA's `xmlvalueprimitive` schema.
    ///
    /// It must be the virtual table, not a hand-rolled shadow: the lookup resolves
    /// the content id through FTS5's term index, and a plain table would let a
    /// regression back into a full scan pass unnoticed. Note the rowid deliberately
    /// differs from the content id — they are unrelated in the shipped data.
    fn synth_xmlvalue(path: &std::path::Path) {
        let c = Connection::open(path).unwrap();
        c.execute_batch(
            "CREATE VIRTUAL TABLE xmlvalueprimitive USING fts5(id, modified, deleted, data, compressed_data);
             INSERT INTO xmlvalueprimitive(rowid, id, modified, deleted, data, compressed_data) VALUES
               (41,'7002',NULL,NULL,'<FKB LANGUAGE=\"de-DE\"><MASSNAHMEIMSERVICE><PARAGRAPH>Steuergeraet pruefen.</PARAGRAPH></MASSNAHMEIMSERVICE></FKB>',NULL),
               (42,'7009',NULL,NULL,'<FKB LANGUAGE=\"de-DE\"><MASSNAHMEIMSERVICE><PARAGRAPH>Nicht gesucht.</PARAGRAPH></MASSNAHMEIMSERVICE></FKB>',NULL);",
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
            .query_row(
                "SELECT body_md_gz FROM fkb_body WHERE content_dede=7002",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let mut md = String::new();
        GzDecoder::new(&blob[..]).read_to_string(&mut md).unwrap();
        assert_eq!(md, "## Maßnahme im Service\n\nSteuergeraet pruefen.");
        // The ABL doc's content_dede (7004) is not present.
        let missing: rusqlite::Result<i64> =
            c.query_row("SELECT 1 FROM fkb_body WHERE content_dede=7004", [], |r| {
                r.get(0)
            });
        assert!(missing.is_err());
    }
}
