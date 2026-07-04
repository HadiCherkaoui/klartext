//! Extract FKB bodies → render → gzip → write klartext-docs.db.
use std::collections::HashMap;
use std::io::Write;
use std::path::Path;

use anyhow::{Context, Result};
use flate2::Compression;
use flate2::write::GzEncoder;
use rusqlite::{Connection, OpenFlags};

use crate::fkb::render_fkb;

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
