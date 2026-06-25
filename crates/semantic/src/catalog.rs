//! DB-backed fault descriptions over the ISTA-derived semantic SQLiteDB.
//!
//! The database is a compact, plaintext extract of ISTA's `DiagDocDb` (see
//! `docs/sqlite-findings.md` and `scripts/build-semantic-db.sh`). It is opened
//! **read-only** at a caller-supplied path; this crate never writes to it, embeds
//! it, or copies its contents.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};
use thiserror::Error;

use crate::dtc::code_number;

/// An error from the semantic catalog: opening or querying the SQLiteDB.
#[derive(Debug, Error)]
pub enum SemanticError {
    /// The SQLiteDB could not be opened (missing file, not a database, …).
    #[error("opening semantic database at {path}: {source}")]
    Open {
        /// The path that failed to open.
        path: PathBuf,
        /// The underlying SQLite error.
        #[source]
        source: rusqlite::Error,
    },
    /// A query against the SQLiteDB failed.
    #[error("querying semantic database: {0}")]
    Query(#[from] rusqlite::Error),
}

/// A human fault description for a DTC at a specific ECU variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DtcDescription {
    /// The ISTA ECU variant the description belongs to (e.g. `fem_20`).
    pub ecu_variant: String,
    /// The 24-bit DTC code number (see [`code_number`]).
    pub code: u32,
    /// The SAE J2012 code (e.g. `P0306`), when the fault carries one.
    pub saecode: Option<String>,
    /// The English fault text, if present.
    pub title_en: Option<String>,
    /// The German fault text, if present.
    pub title_de: Option<String>,
}

/// A diagnostic ECU address paired with its ISTA group name.
///
/// Sourced from ISTA's `XEP_ECUVARIANTS ⋈ XEP_ECUGROUPS` — the general BMW ECU
/// model, not specific to one car.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcuEntry {
    /// The diagnostic address (e.g. `0x12` for the DME).
    pub address: u8,
    /// The ISTA group name, e.g. `d_0012`.
    pub group_name: String,
}

/// Read-only handle to the klartext semantic database (ISTA-derived).
#[derive(Debug)]
pub struct Catalog {
    conn: Connection,
}

impl Catalog {
    /// Open the semantic database read-only at `path`.
    ///
    /// # Errors
    /// Returns [`SemanticError::Open`] if the file is missing or not a database.
    pub fn open(path: &Path) -> Result<Self, SemanticError> {
        let conn = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY).map_err(
            |source| SemanticError::Open {
                path: path.to_path_buf(),
                source,
            },
        )?;
        Ok(Self { conn })
    }

    /// Look up fault descriptions for a raw DTC at an ECU diagnostic address.
    ///
    /// The 3-byte `code` is bridged to ISTA's code number via [`code_number`] and
    /// matched against the `dtc` table for the given diagnostic `ecu_address`.
    /// Several ISTA ECU variants can share a diagnostic address, so this returns
    /// every matching variant's description; an unknown code yields an empty list.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn describe_dtc(
        &self,
        ecu_address: u8,
        code: [u8; 3],
    ) -> Result<Vec<DtcDescription>, SemanticError> {
        let mut stmt = self.conn.prepare(
            "SELECT ecu_variant, code, saecode, title_en, title_de \
             FROM dtc WHERE address = ?1 AND code = ?2",
        )?;
        let rows = stmt.query_map(
            (i64::from(ecu_address), i64::from(code_number(code))),
            |row| {
                Ok(DtcDescription {
                    ecu_variant: row.get(0)?,
                    code: row.get(1)?,
                    saecode: row.get(2)?,
                    title_en: row.get(3)?,
                    title_de: row.get(4)?,
                })
            },
        )?;

        let mut descriptions = Vec::new();
        for row in rows {
            descriptions.push(row?);
        }
        Ok(descriptions)
    }

    /// List the distinct ECU slots known to the DB, ordered by address.
    ///
    /// Returns each diagnostic address with its ISTA group name (`d_00XX`),
    /// de-duplicated across the many variants ISTA records per address. This is
    /// the general BMW ECU map; an empty DB yields an empty list.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn ecus(&self) -> Result<Vec<EcuEntry>, SemanticError> {
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT address, group_name FROM ecu ORDER BY address")?;
        let rows = stmt.query_map([], |row| {
            Ok(EcuEntry {
                address: row.get(0)?,
                group_name: row.get(1)?,
            })
        })?;
        let mut entries = Vec::new();
        for row in rows {
            entries.push(row?);
        }
        Ok(entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Build a synthetic semantic DB (no BMW data) matching the extract schema.
    fn fixture() -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("semantic.db");
        let conn = Connection::open(&path).unwrap();
        // Synthetic text only — no ISTA/BMW data is embedded in the repo. The
        // codes and addresses are arbitrary; two variants share a code/address
        // to exercise the multi-variant case.
        conn.execute_batch(
            "CREATE TABLE dtc (address INT, ecu_variant TEXT, code INT, saecode TEXT, title_de TEXT, title_en TEXT);
             CREATE TABLE ecu (address INT, variant TEXT, group_name TEXT);
             INSERT INTO dtc VALUES (64,'variant_a',14222346,NULL,'BEISPIEL Fehler A','EXAMPLE fault A: powertrain bus, no communication');
             INSERT INTO dtc VALUES (64,'variant_b',14222346,NULL,'BEISPIEL Fehler B','EXAMPLE fault B: bus communication fault');
             INSERT INTO dtc VALUES (18,'variant_c',1234,'P0306','BEISPIEL Fehler C','EXAMPLE fault C: cylinder misfire');
             INSERT INTO ecu VALUES (16,'zgw_x','d_0010');
             INSERT INTO ecu VALUES (18,'dme_x','d_0012');
             INSERT INTO ecu VALUES (64,'fem_20','d_0040');
             INSERT INTO ecu VALUES (64,'fem_21','d_0040');",
        )
        .unwrap();
        (dir, path)
    }

    #[test]
    fn describe_dtc_resolves_text_for_address_and_raw_code() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        // D9 04 0A == 14222346, at address 0x12=18? No — that code is at 0x40=64.
        let descs = cat.describe_dtc(0x40, [0xD9, 0x04, 0x0A]).unwrap();
        // Both variants at address 0x40 carry that code.
        assert_eq!(descs.len(), 2);
        let variant = descs.iter().find(|d| d.ecu_variant == "variant_a").unwrap();
        assert_eq!(
            variant.title_en.as_deref(),
            Some("EXAMPLE fault A: powertrain bus, no communication")
        );
        assert_eq!(variant.code, 14_222_346);
    }

    #[test]
    fn describe_dtc_carries_saecode_when_present() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        // 1234 == 0x0004D2.
        let descs = cat.describe_dtc(0x12, [0x00, 0x04, 0xD2]).unwrap();
        assert_eq!(descs.len(), 1);
        assert_eq!(descs[0].saecode.as_deref(), Some("P0306"));
        assert_eq!(
            descs[0].title_en.as_deref(),
            Some("EXAMPLE fault C: cylinder misfire")
        );
    }

    #[test]
    fn describe_dtc_unknown_code_is_empty() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        assert!(
            cat.describe_dtc(0x40, [0x00, 0x00, 0x01])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn open_missing_file_errors() {
        let err = Catalog::open(Path::new("/nonexistent/semantic.db")).unwrap_err();
        assert!(matches!(err, SemanticError::Open { .. }));
    }

    #[test]
    fn ecus_lists_distinct_addresses_ordered() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        let ecus = cat.ecus().unwrap();
        assert_eq!(
            ecus,
            vec![
                EcuEntry {
                    address: 16,
                    group_name: "d_0010".to_string()
                },
                EcuEntry {
                    address: 18,
                    group_name: "d_0012".to_string()
                },
                EcuEntry {
                    address: 64,
                    group_name: "d_0040".to_string()
                },
            ]
        );
    }

    // Smoke test against the real, BYO-data semantic DB. Ignored by default so
    // the suite needs no BMW data; run with `--ignored` once the DB is built.
    // Asserts structure only (no ISTA text is embedded in the repo).
    #[test]
    #[ignore = "requires data/klartext-semantic.db (run scripts/build-semantic-db.sh)"]
    fn real_db_resolves_a_known_fault() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/klartext-semantic.db");
        let cat = Catalog::open(&path).unwrap();
        // D9 04 0A at the FEM (0x40) resolves to a real fault description.
        let descriptions = cat.describe_dtc(0x40, [0xD9, 0x04, 0x0A]).unwrap();
        assert!(
            descriptions.iter().any(|d| d.title_en.is_some()),
            "expected at least one fault description with English text"
        );
    }
}
