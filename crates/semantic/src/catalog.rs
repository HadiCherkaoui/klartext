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

/// One ISTA measurement-catalog entry for an ECU variant (the "index").
///
/// Sourced from `XEP_ECURESULTS` through the ECU function tree (see
/// `scripts/build-semantic-db.sh`): the readable result name, its unit and ISTA
/// linear post-scaling, and the EDIABAS job that reads it. ISTA-grade labeling and
/// scaling metadata as DATA — complementary to the SGBD/BEST-2 VM, which decodes the
/// raw values. Present only in a v4+ extract (the `measurement` table).
#[derive(Debug, Clone, PartialEq)]
pub struct MeasurementCatalogEntry {
    /// The EDIABAS result name, e.g. `STAT_MOTOROEL_TEMPERATUR_WERT`.
    pub name: String,
    /// The engineering unit, if any (e.g. `°C`, `V`, `1/min`).
    pub unit: Option<String>,
    /// The ISTA post-scale multiplier (often 1.0 — the job already scales).
    pub mul: Option<f64>,
    /// The ISTA post-scale offset (often 0.0).
    pub offset: Option<f64>,
    /// The rounding hint (decimal places), if the DB records one.
    pub round: Option<i64>,
    /// The number-format hint, if the DB records one.
    pub format: Option<String>,
    /// The EDIABAS job that reads this result, e.g. `STATUS_LESEN`.
    pub job: Option<String>,
}

/// One ISTA job-parameter row: one positional argument of one documented job
/// invocation.
///
/// Sourced from `XEP_ECUPARAMETERS` through the ECU function tree (see
/// `scripts/build-semantic-db.sh`): each ISTA fixed function (a named UI action,
/// e.g. "601 Electric fan: Activation signal") invokes an EDIABAS job with
/// positional arguments `P1..Pn`; one row is one such argument. Joining a
/// function's rows in `position` order with `;` yields the EDIABAS argument
/// buffer ISTA sends (e.g. `3;JA;ARG;FanCtl_nSetPoint`). `phase` is the
/// actuation lifecycle step (`Main`, `Preset`, `Reset`). Present only in a v4+
/// extract (the `job_param` table).
#[derive(Debug, Clone, PartialEq)]
pub struct JobParameterEntry {
    /// The owning fixed function's catalog id — rows sharing it (and `phase`)
    /// form one invocation's argument set.
    pub function_id: i64,
    /// The function's English title, if any.
    pub function_en: Option<String>,
    /// The function's German title, if any.
    pub function_de: Option<String>,
    /// The actuation phase: `Main`, `Preset`, or `Reset`.
    pub phase: Option<String>,
    /// The 1-based argument position (from `P1..Pn`).
    pub position: i64,
    /// The argument value ISTA passes (e.g. `ARG`, `90`, `FanCtl_nSetPoint`).
    pub value: Option<String>,
    /// The human label of what the parameter means, if any.
    pub label: Option<String>,
}

/// Read-only handle to the klartext semantic database (ISTA-derived).
#[derive(Debug)]
pub struct Catalog {
    conn: Connection,
    /// Optional sibling `klartext-docs.db` (Phase 1 FKB doc store), when present.
    docs: Option<Connection>,
}

impl Catalog {
    /// Open the semantic database read-only at `path`.
    ///
    /// If a sibling `klartext-docs.db` (the Phase 1 FKB doc store) sits in the same
    /// directory, it is opened read-only too for [`fault_body`](Self::fault_body);
    /// its absence or an open failure is not an error — doc bodies degrade to empty.
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
        let docs = path
            .parent()
            .map(|dir| dir.join("klartext-docs.db"))
            .filter(|p| p.exists())
            .and_then(|p| Connection::open_with_flags(&p, OpenFlags::SQLITE_OPEN_READ_ONLY).ok());
        Ok(Self { conn, docs })
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
        let rows = stmt.query_map((i64::from(address), i64::from(code_number(code))), |row| {
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
        })?;
        let mut docs = Vec::new();
        for row in rows {
            docs.push(row?);
        }
        Ok(docs)
    }

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
    pub fn fault_body(&self, address: u8, code: [u8; 3]) -> Result<Vec<String>, SemanticError> {
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
            .query_map((i64::from(address), i64::from(code_number(code))), |r| {
                r.get(0)
            })?
            .collect::<rusqlite::Result<_>>()?;

        let mut bodies = Vec::new();
        let mut body_stmt =
            docs.prepare("SELECT body_md_gz FROM fkb_body WHERE content_dede = ?1")?;
        for id in ids {
            let gz: Option<Vec<u8>> = body_stmt.query_row([id], |r| r.get(0)).optional()?;
            if let Some(gz) = gz {
                bodies.push(gunzip_utf8(&gz)?);
            }
        }
        Ok(bodies)
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

    /// List the ISTA measurement catalog for an ECU `variant` (the "index").
    ///
    /// Returns every readable result ISTA records for the variant — name, unit,
    /// linear scaling, and the reading job — from the `measurement` table (see
    /// [`MeasurementCatalogEntry`] and `scripts/build-semantic-db.sh`). Empty when the
    /// variant is unknown or the extract predates the table (a pre-v4 DB) — the
    /// missing-table case degrades to empty, not an error.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn measurements(
        &self,
        variant: &str,
    ) -> Result<Vec<MeasurementCatalogEntry>, SemanticError> {
        if !self.has_table("measurement")? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT name, unit, mul, offset, round, zahlenformat, job \
             FROM measurement WHERE ecu_variant = ?1 ORDER BY name",
        )?;
        let rows = stmt.query_map([variant], |row| {
            Ok(MeasurementCatalogEntry {
                name: row.get(0)?,
                unit: row.get(1)?,
                mul: row.get(2)?,
                offset: row.get(3)?,
                round: row.get(4)?,
                format: row.get(5)?,
                job: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// List ISTA's documented invocations of `job` on an ECU `variant`.
    ///
    /// Returns the job's argument rows from the `job_param` table (see
    /// [`JobParameterEntry`] and `scripts/build-semantic-db.sh`), ordered so that
    /// rows sharing (`function_id`, `phase`) are adjacent with their positions
    /// ascending — group them to reconstruct each invocation's `;`-joined
    /// argument buffer. Empty when the job or variant is unknown or the extract
    /// predates the table (a pre-v4 DB) — the missing-table case degrades to
    /// empty, not an error.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn job_parameters(
        &self,
        variant: &str,
        job: &str,
    ) -> Result<Vec<JobParameterEntry>, SemanticError> {
        if !self.has_table("job_param")? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT function_id, function_en, function_de, phase, position, value, label \
             FROM job_param WHERE ecu_variant = ?1 AND job = ?2 \
             ORDER BY function_id, phase, position",
        )?;
        let rows = stmt.query_map([variant, job], |row| {
            Ok(JobParameterEntry {
                function_id: row.get(0)?,
                function_en: row.get(1)?,
                function_de: row.get(2)?,
                phase: row.get(3)?,
                position: row.get(4)?,
                value: row.get(5)?,
                label: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }
}

/// Gunzip a stored body blob to a UTF-8 string. A decode failure means a corrupt
/// store, surfaced as a query error rather than a panic.
fn gunzip_utf8(gz: &[u8]) -> Result<String, SemanticError> {
    use std::io::Read;
    let mut out = String::new();
    flate2::read::GzDecoder::new(gz)
        .read_to_string(&mut out)
        .map_err(|e| SemanticError::Query(rusqlite::Error::ToSqlConversionFailure(Box::new(e))))?;
    Ok(out)
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
            // The v4 extract adds ISTA's measurement catalog (the "index"). Synthetic
            // rows only (no BMW data); realistic shape: variant, name, unit, scaling, job.
            conn.execute_batch(
                "CREATE TABLE measurement (ecu_variant TEXT, name TEXT, unit TEXT, mul REAL, offset REAL, round INTEGER, zahlenformat TEXT, job TEXT);
                 INSERT INTO measurement VALUES ('dde_a','STAT_EXAMPLE_TEMP_WERT','°C',1.0,0.0,0,NULL,'STATUS_LESEN');
                 INSERT INTO measurement VALUES ('dde_a','STAT_EXAMPLE_VOLT_WERT','V',0.001,0.0,3,NULL,'STATUS_BLOCK_LESEN');
                 INSERT INTO measurement VALUES ('fem_20','STAT_OTHER_WERT','%',1.0,0.0,1,NULL,'STATUS_LESEN');",
            )
            .unwrap();
            // The v4 extract's invocation half: per fixed function, the job's
            // positional args. Two functions share a job (multi-invocation), one
            // has Main+Reset phases, positions include >9 (numeric order).
            conn.execute_batch(
                "CREATE TABLE job_param (ecu_variant TEXT, function_id INTEGER, function_en TEXT, function_de TEXT, phase TEXT, position INTEGER, value TEXT, label TEXT, job TEXT);
                 INSERT INTO job_param VALUES ('dde_a',9002,'EXAMPLE fan: activation',NULL,'Main',1,'3',NULL,'STATUS_BLOCK_LESEN');
                 INSERT INTO job_param VALUES ('dde_a',9002,'EXAMPLE fan: activation',NULL,'Main',2,'JA',NULL,'STATUS_BLOCK_LESEN');
                 INSERT INTO job_param VALUES ('dde_a',9002,'EXAMPLE fan: activation',NULL,'Main',10,'FanArg',NULL,'STATUS_BLOCK_LESEN');
                 INSERT INTO job_param VALUES ('dde_a',9001,NULL,'BEISPIEL Ventil','Main',1,'90','Ansteuerwert','STEUERN_EXAMPLE');
                 INSERT INTO job_param VALUES ('dde_a',9001,NULL,'BEISPIEL Ventil','Reset',1,'0','Ansteuerwert','STEUERN_EXAMPLE');
                 INSERT INTO job_param VALUES ('fem_20',9003,'EXAMPLE other',NULL,'Main',1,'X',NULL,'STATUS_BLOCK_LESEN');",
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
             INSERT INTO dtc VALUES (18, 'd72n47a0', 4919860, 'P123400', 'Glow plug', NULL);",
        )
        .unwrap();
        if with_docs {
            // fault at address 18 (0x12), code 0x4B1234 = 4919860 → two docs.
            conn.execute_batch(
                "CREATE TABLE fault_doc (address INTEGER, code INTEGER, infoobject_id INTEGER, content_engb INTEGER, content_dede INTEGER);
                 INSERT INTO fault_doc VALUES (18, 4919860, 1001, 55501, 55502);
                 INSERT INTO fault_doc VALUES (18, 4919860, 1002, 55601, 55602);
                 CREATE TABLE infoobject (id INTEGER, infotype TEXT, docnumber TEXT, safety_relevant INTEGER, title_en TEXT, title_de TEXT);
                 INSERT INTO infoobject VALUES (1001, 'FKB', 'DOC-1', 0, 'Glow plug fault', 'Gluehkerzenfehler');
                 INSERT INTO infoobject VALUES (1002, 'ABL', 'DOC-2', 1, NULL, 'Gluehkerze pruefen');",
            )
            .unwrap();
        }
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
    fn measurements_lists_the_catalog_scoped_by_variant() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        let ms = cat.measurements("dde_a").unwrap();
        assert_eq!(ms.len(), 2);
        let temp = ms
            .iter()
            .find(|m| m.name == "STAT_EXAMPLE_TEMP_WERT")
            .unwrap();
        assert_eq!(temp.unit.as_deref(), Some("°C"));
        assert_eq!(temp.mul, Some(1.0));
        assert_eq!(temp.job.as_deref(), Some("STATUS_LESEN"));
        let volt = ms
            .iter()
            .find(|m| m.name == "STAT_EXAMPLE_VOLT_WERT")
            .unwrap();
        assert_eq!(volt.mul, Some(0.001));
        assert_eq!(volt.round, Some(3));
        // Scoped by variant: a different variant sees only its own rows; an unknown
        // variant is empty (not an error).
        assert_eq!(cat.measurements("fem_20").unwrap().len(), 1);
        assert!(cat.measurements("nope").unwrap().is_empty());
    }

    #[test]
    fn measurements_degrade_to_empty_without_the_table() {
        // A pre-v4 extract (no titles branch -> no measurement table) must not error.
        let (_dir, path) = fixture_opts(false);
        let cat = Catalog::open(&path).unwrap();
        assert!(cat.measurements("dde_a").unwrap().is_empty());
    }

    #[test]
    fn job_parameters_group_invocations_in_numeric_position_order() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        // Scoped by (variant, job): fem_20's row for the same job is not listed.
        let rows = cat.job_parameters("dde_a", "STATUS_BLOCK_LESEN").unwrap();
        assert_eq!(rows.len(), 3);
        assert!(rows.iter().all(|r| r.function_id == 9002));
        // Positions come back numerically ascending — P10 sorts after P2, so the
        // ';'-joined argument buffer reconstructs in send order.
        assert_eq!(
            rows.iter().map(|r| r.position).collect::<Vec<_>>(),
            [1, 2, 10]
        );
        assert_eq!(
            rows.iter()
                .map(|r| r.value.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["3", "JA", "FanArg"]
        );
        assert_eq!(
            rows[0].function_en.as_deref(),
            Some("EXAMPLE fan: activation")
        );

        // A two-phase actuation keeps its phases as separate adjacent groups.
        let steuern = cat.job_parameters("dde_a", "STEUERN_EXAMPLE").unwrap();
        assert_eq!(steuern.len(), 2);
        assert_eq!(
            steuern
                .iter()
                .map(|r| (r.phase.as_deref().unwrap(), r.value.as_deref().unwrap()))
                .collect::<Vec<_>>(),
            [("Main", "90"), ("Reset", "0")]
        );
        assert_eq!(steuern[0].function_de.as_deref(), Some("BEISPIEL Ventil"));
        assert_eq!(steuern[0].label.as_deref(), Some("Ansteuerwert"));

        // Unknown job or variant: empty, not an error.
        assert!(cat.job_parameters("dde_a", "NOPE").unwrap().is_empty());
        assert!(
            cat.job_parameters("nope", "STATUS_BLOCK_LESEN")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn job_parameters_degrade_to_empty_without_the_table() {
        // A pre-v4 extract (no job_param table) must not error.
        let (_dir, path) = fixture_opts(false);
        let cat = Catalog::open(&path).unwrap();
        assert!(cat.job_parameters("dde_a", "ANY").unwrap().is_empty());
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

    #[test]
    fn fault_body_reads_rendered_markdown_from_sibling_docs_db() {
        use flate2::Compression;
        use flate2::write::GzEncoder;
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
        docs.execute_batch(
            "CREATE TABLE fkb_body (content_dede INTEGER PRIMARY KEY, body_md_gz BLOB NOT NULL);",
        )
        .unwrap();
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        enc.write_all(b"## Ma\xC3\x9fnahme im Service\n\nSteuergeraet pruefen.")
            .unwrap();
        let gz = enc.finish().unwrap();
        docs.execute("INSERT INTO fkb_body VALUES (7002, ?1)", [gz])
            .unwrap();

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
        assert!(
            links > 0,
            "fault_doc should be populated by the item-4 extract"
        );
        assert!(
            docs > 0,
            "infoobject should be populated by the item-4 extract"
        );
    }

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
}
