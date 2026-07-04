//! DB-backed fault descriptions over the ISTA-derived semantic SQLiteDB.
//!
//! The database is a compact, plaintext extract of ISTA's `DiagDocDb` (see
//! `docs/sqlite-findings.md` and `scripts/build-semantic-db.sh`). It is opened
//! **read-only** at a caller-supplied path; this crate never writes to it, embeds
//! it, or copies its contents.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
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

/// A diagnostic ECU slot: an address, its canonical ISTA group, and a title.
///
/// Sourced from ISTA's `XEP_ECUVARIANTS ⋈ XEP_ECUGROUPS` — the general BMW ECU
/// model, not specific to one car. `extra_groups` holds any other group names
/// ISTA records at the same address (e.g. `g_motor` alongside `d_0012`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcuSlot {
    /// The diagnostic address (e.g. `0x12` for the engine).
    pub address: u8,
    /// The canonical ISTA group name — the `d_00XX` matching the address when
    /// present, else the first group seen.
    pub group_name: String,
    /// Other ISTA group names recorded at this address.
    pub extra_groups: Vec<String>,
    /// A representative human title for the address, if the DB has one.
    pub title: Option<String>,
}

/// A localized label for a freeze-frame environmental condition.
///
/// Sourced from ISTA's `XEP_ENVCONDSLABELS`, keyed by the numeric identifier
/// (`UWIDENT`, the decimal of the SGBD's hex `UWNR`). Overlays English names and
/// units onto the SGBD-decoded snapshot fields (see [`crate::snapshot`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnvCondLabel {
    /// The 2-byte environmental-condition identifier (UWNR).
    pub uwnr: u32,
    /// The English label, if present.
    pub title_en: Option<String>,
    /// The German label, if present.
    pub title_de: Option<String>,
    /// The engineering unit, if the DB records one.
    pub unit: Option<String>,
    /// True for a status/enum field (ISTA node class), not a numeric measurement.
    pub is_status: bool,
}

/// One ECU variant candidate for an address (for resolution and messages).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VariantInfo {
    /// The SGBD variant name (the `.prg` stem), e.g. `d72n47a0`.
    pub name: String,
    /// The variant's human title, if the DB has one.
    pub title: Option<String>,
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

    /// Whether column `column` exists on `table` (for pre-v2 extract compatibility).
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the pragma query fails.
    fn has_column(&self, table: &str, column: &str) -> Result<bool, SemanticError> {
        let mut stmt = self
            .conn
            .prepare("SELECT 1 FROM pragma_table_info(?1) WHERE name = ?2")?;
        Ok(stmt.exists((table, column))?)
    }

    /// Whether `table` exists (for pre-v3 extracts without the `envcond` table).
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the query fails.
    fn has_table(&self, table: &str) -> Result<bool, SemanticError> {
        let mut stmt = self
            .conn
            .prepare("SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1")?;
        Ok(stmt.exists([table])?)
    }

    /// Look up the localized label for a freeze-frame identifier (UWNR).
    ///
    /// Returns `None` when the identifier is unknown or the extract predates the
    /// `envcond` table (a pre-v3 DB) — the caller then falls back to the SGBD's
    /// German text. See [`crate::snapshot`].
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn envcond_label(&self, uwnr: u16) -> Result<Option<EnvCondLabel>, SemanticError> {
        if !self.has_table("envcond")? {
            return Ok(None);
        }
        let mut stmt = self.conn.prepare(
            "SELECT uwnr, title_en, title_de, unit, is_status \
             FROM envcond WHERE uwnr = ?1 LIMIT 1",
        )?;
        let label = stmt
            .query_row([i64::from(uwnr)], |row| {
                Ok(EnvCondLabel {
                    uwnr: row.get(0)?,
                    title_en: row.get(1)?,
                    title_de: row.get(2)?,
                    unit: row.get(3)?,
                    is_status: row.get::<_, i64>(4)? != 0,
                })
            })
            .optional()?;
        Ok(label)
    }

    /// List the general ECU map: one [`EcuSlot`] per diagnostic address.
    ///
    /// Aggregates ISTA's many per-address variants/groups in Rust: the canonical
    /// group is the `d_00XX` whose hex equals the address (else the first seen).
    /// NULL addresses (ISTA virtual/internal SGBDs) are skipped so one cannot
    /// abort the query. Titles come back `None` on a pre-v2 extract lacking the
    /// columns. An empty DB yields an empty list.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
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
        // Prefer the canonical group per address: the d_00XX matching the address.
        for slot in &mut slots {
            let canonical = format!("d_{:04x}", slot.address);
            if slot.group_name != canonical
                && let Some(pos) = slot.extra_groups.iter().position(|g| *g == canonical)
            {
                let promoted = slot.extra_groups.remove(pos);
                slot.extra_groups
                    .push(std::mem::replace(&mut slot.group_name, promoted));
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
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn variants(&self, address: u8) -> Result<Vec<VariantInfo>, SemanticError> {
        let has_titles = self.has_column("ecu", "title_en")?;
        let sql = if has_titles {
            "SELECT DISTINCT variant, title_en, title_de FROM ecu \
             WHERE address = ?1 ORDER BY variant"
        } else {
            "SELECT DISTINCT variant, NULL, NULL FROM ecu \
             WHERE address = ?1 ORDER BY variant"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([i64::from(address)], |row| {
            let name: String = row.get(0)?;
            let title_en: Option<String> = row.get(1)?;
            let title_de: Option<String> = row.get(2)?;
            Ok(VariantInfo {
                name,
                title: title_en.or(title_de),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;
    use tempfile::TempDir;

    /// Build a synthetic semantic DB (no BMW data) matching the v2 extract schema
    /// (title columns). `titles=false` reproduces a pre-v2 extract to prove the
    /// column-detection backward compatibility.
    fn fixture_opts(titles: bool) -> (TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("semantic.db");
        let conn = Connection::open(&path).unwrap();
        // Synthetic text only — no ISTA/BMW data is embedded in the repo. The
        // codes and addresses are arbitrary; two variants share a code/address
        // to exercise the multi-variant case.
        let ecu_cols = if titles {
            "address INT, variant TEXT, group_name TEXT, title_en TEXT, title_de TEXT"
        } else {
            "address INT, variant TEXT, group_name TEXT"
        };
        conn.execute_batch(&format!(
            "CREATE TABLE dtc (address INT, ecu_variant TEXT, code INT, saecode TEXT, title_de TEXT, title_en TEXT);
             CREATE TABLE ecu ({ecu_cols});
             INSERT INTO dtc VALUES (64,'variant_a',14222346,NULL,'BEISPIEL Fehler A','EXAMPLE fault A: powertrain bus, no communication');
             INSERT INTO dtc VALUES (64,'variant_b',14222346,NULL,'BEISPIEL Fehler B','EXAMPLE fault B: bus communication fault');
             INSERT INTO dtc VALUES (18,'variant_c',1234,'P0306','BEISPIEL Fehler C','EXAMPLE fault C: cylinder misfire');"
        ))
        .unwrap();
        if titles {
            conn.execute_batch(
                "INSERT INTO ecu VALUES (16,'zgw_x','d_0010','Gateway','Gateway');
                 INSERT INTO ecu VALUES (18,'dde_a','d_0012','Digital Diesel Electronics','DDE');
                 INSERT INTO ecu VALUES (18,'dde_b','g_motor','Engine (group)','Motor');
                 INSERT INTO ecu VALUES (64,'fem_20','d_0040','Front Electronic Module','FEM');
                 INSERT INTO ecu VALUES (64,'fem_21','d_0040',NULL,NULL);
                 -- ISTA stores virtual/internal SGBDs with a NULL address; they are
                 -- not targetable ECUs and must be skipped, not abort the query.
                 INSERT INTO ecu VALUES (NULL,'virtsg98','D_VIRT98','Virtual','Virtuell');",
            )
            .unwrap();
            // The v3 extract adds the freeze-frame env-condition labels. Synthetic
            // rows only (no ISTA text): 0x5205 = 20997 (coolant), a status field.
            conn.execute_batch(
                "CREATE TABLE envcond (uwnr INT, unit TEXT, title_en TEXT, title_de TEXT, is_status INT);
                 INSERT INTO envcond VALUES (20997,'°C','EXAMPLE coolant temperature','BEISPIEL Kühlmitteltemperatur',0);
                 INSERT INTO envcond VALUES (5888,'km','EXAMPLE mileage','BEISPIEL Kilometerstand',0);
                 INSERT INTO envcond VALUES (17900,NULL,'EXAMPLE engine status','BEISPIEL Motorstatus',1);",
            )
            .unwrap();
        } else {
            conn.execute_batch(
                "INSERT INTO ecu VALUES (16,'zgw_x','d_0010');
                 INSERT INTO ecu VALUES (18,'dde_a','d_0012');
                 INSERT INTO ecu VALUES (64,'fem_20','d_0040');
                 INSERT INTO ecu VALUES (NULL,'virtsg98','D_VIRT98');",
            )
            .unwrap();
        }
        (dir, path)
    }

    fn fixture() -> (TempDir, PathBuf) {
        fixture_opts(true)
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
    fn envcond_label_resolves_english_name_and_status_flag() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        // 0x5205 = 20997: a numeric coolant field with an English label and unit.
        let coolant = cat.envcond_label(0x5205).unwrap().expect("known label");
        assert_eq!(
            coolant.title_en.as_deref(),
            Some("EXAMPLE coolant temperature")
        );
        assert_eq!(coolant.unit.as_deref(), Some("°C"));
        assert!(!coolant.is_status);
        // A status/enum field is flagged.
        assert!(cat.envcond_label(0x45EC).unwrap().unwrap().is_status);
        // Unknown identifier → None (caller falls back to SGBD text).
        assert!(cat.envcond_label(0x9999).unwrap().is_none());
    }

    #[test]
    fn envcond_label_absent_table_degrades_to_none() {
        // A pre-v3 extract (no titles branch → no envcond table) must not error.
        let (_dir, path) = fixture_opts(false);
        let cat = Catalog::open(&path).unwrap();
        assert!(cat.envcond_label(0x5205).unwrap().is_none());
    }

    #[test]
    fn ecus_aggregates_by_address_with_canonical_group_and_title() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        let ecus = cat.ecus().unwrap();
        // NULL-address virtual SGBD skipped; one slot per address, ordered.
        assert_eq!(
            ecus.iter().map(|e| e.address).collect::<Vec<_>>(),
            [16, 18, 64]
        );
        // 0x12 has two groups; the canonical is the d_00XX matching the address.
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
        assert_eq!(
            vs.iter().map(|v| v.name.as_str()).collect::<Vec<_>>(),
            ["dde_a", "dde_b"]
        );
        assert_eq!(vs[0].title.as_deref(), Some("Digital Diesel Electronics"));
    }

    #[test]
    fn ecus_works_on_a_pre_v2_extract_without_title_columns() {
        let (_dir, path) = fixture_opts(false);
        let cat = Catalog::open(&path).unwrap();
        let ecus = cat.ecus().unwrap();
        assert_eq!(
            ecus.iter().map(|e| e.address).collect::<Vec<_>>(),
            [16, 18, 64]
        );
        assert!(ecus.iter().all(|e| e.title.is_none()));
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

    // Smoke test of the freeze-frame label overlay against the real BYO-data DB.
    // Ignored by default; run with `--ignored` once the DB is built (v3 extract).
    // Asserts structure only (no ISTA text is embedded in the repo).
    #[test]
    #[ignore = "requires data/klartext-semantic.db (run scripts/build-semantic-db.sh)"]
    fn real_db_resolves_a_freeze_frame_label() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/klartext-semantic.db");
        let cat = Catalog::open(&path).unwrap();
        // 0x5205 = 20997 is the coolant-temperature env-condition; it has a label.
        let label = cat
            .envcond_label(0x5205)
            .unwrap()
            .expect("coolant env-condition label present in the real DB");
        assert!(label.title_en.is_some() || label.title_de.is_some());
    }

    // Smoke test of the ECU map against the real BYO-data DB, which contains
    // ISTA's virtual SGBD rows with a NULL address. Ignored by default; run with
    // `--ignored` once the DB is built. Asserts structure only.
    #[test]
    #[ignore = "requires data/klartext-semantic.db (run scripts/build-semantic-db.sh)"]
    fn real_db_lists_ecus_skipping_null_addresses() {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../data/klartext-semantic.db");
        let cat = Catalog::open(&path).unwrap();
        // Returns Ok despite NULL-address virtual entries, and yields the full
        // map — far more than the handful of built-in aliases.
        let ecus = cat.ecus().unwrap();
        assert!(
            ecus.len() > 3,
            "expected the full ECU map, got {} entries",
            ecus.len()
        );
    }
}
