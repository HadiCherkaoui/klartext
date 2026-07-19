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
use crate::quantity::Quantity;

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
/// `scripts/build-semantic-db.sh`): the readable result name, its unit, ISTA
/// linear post-scaling, the EDIABAS job that reads it, and ISTA's own title.
/// ISTA-grade labeling and scaling metadata as DATA — complementary to the
/// SGBD/BEST-2 VM, which decodes the raw values. Present only in a v4+ extract
/// (the `measurement` table); `title` specifically is `None` on an extract
/// built before this field was added, even if the table itself is present.
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
    /// ISTA's own human title, e.g. `104 Battery voltage` — the fleet-wide
    /// semantic key. Present on every ISTA result; prefer it over the EDIABAS
    /// name, whose spelling varies per ECU.
    pub title: Option<String>,
}

/// A quantity resolved to a concrete measurement on one ECU variant.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedQuantity {
    /// The EDIABAS result name to read.
    pub name: String,
    /// Multiply the read value by this to get [`Quantity::canonical_unit`].
    pub factor: f64,
}

/// One ISTA job-parameter row: one positional argument of one documented job
/// invocation.
///
/// Sourced from `XEP_ECUPARAMETERS` through the ECU function tree (see
/// `scripts/build-semantic-db.sh`): each ISTA fixed function (a named UI action,
/// e.g. "601 Electric fan: Activation signal") invokes one or more EDIABAS jobs
/// per actuation phase (`Main`, `Preset`, `Reset`) — ISTA runs a phase's jobs in
/// `rank` order (`RheingoldSessionController.DoTriggerComponent`:
/// `GetJobsByPhase(phase).OrderBy(x => x.Rank)`), e.g. `DIAGNOSE_MODE` (rank 1)
/// then `STEUERN_IO` (rank 2) — and can even invoke the SAME job name more than
/// once in one phase, at different ranks. Each invocation takes positional
/// arguments `P1..Pn`; one row is one such argument. Joining a (`function_id`,
/// `phase`, `rank`) group's rows in `position` order with `;` yields the
/// EDIABAS argument buffer ISTA sends (e.g. `3;JA;ARG;FanCtl_nSetPoint`).
/// Present only in a v4+ extract (the `job_param` table); `rank` specifically
/// is `None` on an extract that predates the `XEP_REFECUJOBS` join.
#[derive(Debug, Clone, PartialEq)]
pub struct JobParameterEntry {
    /// The owning fixed function's catalog id — rows sharing it, `phase`, and
    /// `rank` form one invocation's argument set.
    pub function_id: i64,
    /// The function's English title, if any.
    pub function_en: Option<String>,
    /// The function's German title, if any.
    pub function_de: Option<String>,
    /// The actuation phase: `Main`, `Preset`, or `Reset`.
    pub phase: Option<String>,
    /// The job's rank within its (function, phase) — ISTA's execution order
    /// when a phase invokes more than one job (or the same job repeatedly).
    /// `None` on an extract that predates the `rank` column; such rows are
    /// still returned, ordered by `position` alone.
    pub rank: Option<i64>,
    /// The 1-based argument position (from `P1..Pn`).
    pub position: i64,
    /// The argument value ISTA passes (e.g. `ARG`, `90`, `FanCtl_nSetPoint`).
    pub value: Option<String>,
    /// The human label of what the parameter means, if any.
    pub label: Option<String>,
    /// The EDIABAS job this argument's phase runs — an invocation is scoped by
    /// (`function_id`, `phase`, `rank`) and carries its own job name, since a
    /// single phase may run several distinct jobs (e.g. `DIAGNOSE_MODE` then
    /// `STEUERN_IO`).
    pub job: String,
}

/// One ISTA fixed function's post-Main hold parameters and operator text.
///
/// Sourced from `XEP_ECUFIXEDFUNCTIONS` (see `scripts/build-semantic-db.sh`):
/// ISTA's post-Main hold is driven by `ACTIVATION`/`ACTIVATION_DURATION_MS`
/// (`Activation > 0` with a Reset phase → hold for the duration; `Activation ==
/// 0` with a Reset phase → hold until an explicit stop), and the
/// PREPARING/PROCESSING/POST operator text is the prose ISTA shows the human in
/// place of a machine-checked precondition. Keyed by `function_id`. Present only
/// in a v6+ extract (the `fixed_function` table); absent-table and absent-row
/// both resolve to `None`, never an error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FixedFunction {
    /// The fixed function's catalog id (matches [`JobParameterEntry::function_id`]).
    pub function_id: i64,
    /// ISTA's `ACTIVATION`: `> 0` means a timed hold, `0` means hold-until-stop.
    pub activation: Option<i64>,
    /// The timed-hold duration in milliseconds, when `activation > 0`.
    pub activation_duration_ms: Option<i64>,
    /// The operator text ISTA shows before actuation (preparing), if any.
    pub preparing_text: Option<String>,
    /// The operator text ISTA shows during actuation (processing), if any.
    pub processing_text: Option<String>,
    /// The operator text ISTA shows after actuation (post), if any.
    pub post_text: Option<String>,
}

/// One runnable ISTA service function for an ECU variant, for discovery.
///
/// The [`JobParameterEntry`]/[`FixedFunction`] pair keyed by `function_id`, folded
/// to one row per DISTINCT catalog function so an agent can DISCOVER the
/// `function_id` a service runner takes — the integer catalog id is a different
/// identifier space from the SGBD-derived string labels, and this is where it comes
/// from. Carries ISTA's own title, whether the function defines a return-to-safe
/// (`Reset`) phase, and the post-Main hold summary (`activation` /
/// `activation_duration_ms`) plus the operator instruction (`preparing_text`) ISTA
/// shows a human before actuation. Sourced from `job_param` ⋈ `fixed_function` (see
/// `scripts/build-semantic-db.sh`). Present only in a v4+ extract (the `job_param`
/// table); the hold fields are additionally `None` on a pre-v6 extract (no
/// `fixed_function` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceFunctionCatalogEntry {
    /// The ISTA fixed-function catalog id — the value a service runner takes.
    pub function_id: i64,
    /// ISTA's human title (English preferred, German fallback), if any.
    pub title: Option<String>,
    /// True when the function defines a `Reset` (return-to-safe) teardown phase.
    pub has_reset: bool,
    /// ISTA's `ACTIVATION`: `> 0` timed hold, `0` hold-until-stop, when known.
    pub activation: Option<i64>,
    /// The timed-hold duration in milliseconds, when `activation > 0`.
    pub activation_duration_ms: Option<i64>,
    /// The operator instruction ISTA shows before actuation (preparing), if any.
    pub preparing_text: Option<String>,
}

/// One node of ISTA's per-platform ECU tree (the graph view).
///
/// Sourced from the platform's `BNT-XML-<series>` bordnet (extracted by
/// `scripts/build-semantic-db.sh` + `klartext-docbuild`): the diagnostic
/// address, ISTA's short display name (e.g. `DME`, `FEM`), its group SGBD, the
/// bus it sits on (enum + display label, e.g. `FACAN` / `PT-CAN`), the graph
/// grid position, and whether the address belongs to the platform's minimal
/// configuration — the always-present core boxes of ISTA's vehicle view (~11 on
/// an F25, where the VCM lists 30+ configured addresses). Present only in a
/// v5+ extract (the `ecu_tree` table).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcuTreeEntry {
    /// The diagnostic address (e.g. `0x12`).
    pub address: u8,
    /// ISTA's short display name for the box (e.g. `DME`, `FEM`, `KOMBI`).
    pub name: Option<String>,
    /// The group SGBD that identifies the slot (e.g. `G_MOTOR`).
    pub group_sgbd: Option<String>,
    /// The bus enum name (e.g. `FACAN`, `KCAN`, `FLEXRAY`, `MOST`).
    pub bus: Option<String>,
    /// The bus display label (e.g. `PT-CAN`, `K-CAN`).
    pub bus_label: Option<String>,
    /// The graph-view grid column.
    pub col: Option<i64>,
    /// The graph-view grid row.
    pub row: Option<i64>,
    /// True when the address is part of the platform's minimal configuration.
    pub minimal: bool,
}

/// Resolve the bordnet series for a vehicle from an I-Stufe level string.
///
/// `istufe` is `SERIES-YY-MM-PPP` (e.g. `F025-17-03-505`) — ideally the FACTORY
/// level, since ISTA's dated per-platform splits key on the construction date.
/// `known` is the extract's series list ([`Catalog::ecu_tree_series`]). The
/// match ladder: the exact series (case-insensitive), else the zero-dropped
/// form (`F025` → `F25`, as I-Stufe series pad the model code); among dated
/// variants `<base>_<YYMM>` (e.g. `F25_1404`) the latest whose date is at or
/// before the level's `YY-MM` wins over the plain base — mirroring ISTA's
/// `C_DATETIME >= <facelift date> → dated characteristics` rule.
pub fn bordnet_series_for(istufe: &str, known: &[String]) -> Option<String> {
    let mut parts = istufe.split('-');
    let series = parts.next()?.trim().to_ascii_uppercase();
    let yy: u32 = parts.next()?.trim().parse().ok()?;
    let mm: u32 = parts.next()?.trim().parse().ok()?;
    let build_yymm = yy * 100 + mm;
    // The I-Stufe pads the model code to four chars (`F025`); bordnet keys don't.
    let dropped = match series.as_bytes() {
        [letter, b'0', rest @ ..] if !rest.is_empty() => {
            let mut s = String::new();
            s.push(*letter as char);
            s.push_str(std::str::from_utf8(rest).ok()?);
            s
        }
        _ => series.clone(),
    };
    for base in [&series, &dropped] {
        let mut plain: Option<&str> = None;
        let mut best_dated: Option<(u32, &str)> = None;
        for candidate in known {
            let upper = candidate.to_ascii_uppercase();
            if upper == *base {
                plain = Some(candidate);
            } else if let Some(suffix) = upper.strip_prefix(&format!("{base}_"))
                && let Ok(date) = suffix.parse::<u32>()
                && date <= build_yymm
                && best_dated.is_none_or(|(d, _)| date > d)
            {
                best_dated = Some((date, candidate));
            }
        }
        if let Some((_, dated)) = best_dated {
            return Some(dated.to_string());
        }
        if let Some(plain) = plain {
            return Some(plain.to_string());
        }
    }
    None
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
    /// linear scaling, the reading job, and ISTA's own title (English preferred,
    /// German fallback) — from the `measurement` table (see
    /// [`MeasurementCatalogEntry`] and `scripts/build-semantic-db.sh`). Empty when the
    /// variant is unknown or the extract predates the table (a pre-v4 DB) — the
    /// missing-table case degrades to empty, not an error. `title` comes back
    /// `None` on an extract whose `measurement` table predates the title columns.
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
        let has_titles = self.has_column("measurement", "title_en")?;
        let sql = if has_titles {
            "SELECT name, unit, mul, offset, round, zahlenformat, job, title_en, title_de \
             FROM measurement WHERE ecu_variant = ?1 ORDER BY name"
        } else {
            "SELECT name, unit, mul, offset, round, zahlenformat, job, NULL, NULL \
             FROM measurement WHERE ecu_variant = ?1 ORDER BY name"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([variant], |row| {
            let title_en: Option<String> = row.get(7)?;
            let title_de: Option<String> = row.get(8)?;
            Ok(MeasurementCatalogEntry {
                name: row.get(0)?,
                unit: row.get(1)?,
                mul: row.get(2)?,
                offset: row.get(3)?,
                round: row.get(4)?,
                format: row.get(5)?,
                job: row.get(6)?,
                title: title_en.or(title_de),
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Resolve `quantity` on `variant` by matching ISTA's own result title.
    ///
    /// Titles are present on every ISTA result, so this generalises to ECUs
    /// klartext has never seen — where the EDIABAS name would differ. The unit
    /// comes from the catalog, never from the name.
    ///
    /// Returns `None` — never a guess — when no title matches, when the match's
    /// unit is absent or unrecognised, when two DIFFERENT measurements match
    /// equally well (ambiguous), or when the extract predates the `measurement`
    /// table or its title columns. Callers must treat `None` as "cannot check" and
    /// degrade to advisory rather than bind to a plausible-looking wrong sensor.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn resolve_quantity(
        &self,
        variant: &str,
        quantity: Quantity,
    ) -> Result<Option<ResolvedQuantity>, SemanticError> {
        if !self.has_table("measurement")? || !self.has_column("measurement", "title_en")? {
            return Ok(None);
        }
        let mut stmt = self.conn.prepare(
            "SELECT name, unit, title_en FROM measurement \
             WHERE ecu_variant = ?1 AND title_en IS NOT NULL",
        )?;
        let rows: Vec<(String, Option<String>, String)> = stmt
            .query_map([variant], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut found: Option<ResolvedQuantity> = None;
        for (name, unit, title) in rows {
            if !quantity.matches_title(&title) {
                continue;
            }
            let Some(factor) = quantity.unit_factor(unit.as_deref()) else {
                continue;
            };
            match &found {
                // The same measurement listed twice is not ambiguity.
                Some(prev) if prev.name == name => {}
                // Two DIFFERENT measurements both claim the label: refuse rather
                // than pick one arbitrarily.
                Some(_) => return Ok(None),
                None => found = Some(ResolvedQuantity { name, factor }),
            }
        }
        Ok(found)
    }

    /// List ISTA's documented invocations of `job` on an ECU `variant`.
    ///
    /// Returns the job's argument rows from the `job_param` table (see
    /// [`JobParameterEntry`] and `scripts/build-semantic-db.sh`), ordered by
    /// (`function_id`, `phase`, `rank`, `position`) — ISTA's own execution
    /// order. A phase can invoke more than one job, including the same job name
    /// more than once (`rank` distinguishes them); rows sharing (`function_id`,
    /// `phase`, `rank`) are one invocation's argument set — join them in
    /// `position` order with `;` to reconstruct the EDIABAS argument buffer.
    /// Empty when the job or variant is unknown or the extract predates the
    /// table (a pre-v4 DB) — the missing-table case degrades to empty, not an
    /// error. On an extract that predates the `rank` column, every row's `rank`
    /// comes back `None` and the order falls back to `position` alone (the
    /// pre-fix behaviour) rather than erroring on the missing column.
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
        let has_rank = self.has_column("job_param", "rank")?;
        let sql = if has_rank {
            "SELECT function_id, function_en, function_de, phase, rank, position, value, label, job \
             FROM job_param WHERE ecu_variant = ?1 AND job = ?2 \
             ORDER BY function_id, phase, rank, position"
        } else {
            "SELECT function_id, function_en, function_de, phase, NULL, position, value, label, job \
             FROM job_param WHERE ecu_variant = ?1 AND job = ?2 \
             ORDER BY function_id, phase, position"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([variant, job], |row| {
            Ok(JobParameterEntry {
                function_id: row.get(0)?,
                function_en: row.get(1)?,
                function_de: row.get(2)?,
                phase: row.get(3)?,
                rank: row.get(4)?,
                position: row.get(5)?,
                value: row.get(6)?,
                label: row.get(7)?,
                job: row.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// List every documented invocation of a fixed function on an ECU `variant`.
    ///
    /// Returns all `job_param` rows for the `(variant, function_id)` pair —
    /// across every phase, job, and rank — ordered by (`phase`, `rank`,
    /// `position`) for execution. Unlike [`job_parameters`](Self::job_parameters),
    /// which keys on a single job name, one function can run several DISTINCT
    /// jobs per phase (e.g. `DIAGNOSE_MODE` then `STEUERN_IO`); each returned row
    /// carries its own [`job`](JobParameterEntry::job), so the caller reconstructs
    /// one invocation per (`phase`, `job`, `rank`) group. Empty when the function
    /// or variant is unknown or the extract predates the table (a pre-v4 DB) —
    /// the missing-table case degrades to empty, not an error. On an extract that
    /// predates the `rank` column, every row's `rank` comes back `None` and the
    /// order falls back to (`phase`, `position`).
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn job_parameters_for_function(
        &self,
        variant: &str,
        function_id: i64,
    ) -> Result<Vec<JobParameterEntry>, SemanticError> {
        if !self.has_table("job_param")? {
            return Ok(Vec::new());
        }
        let has_rank = self.has_column("job_param", "rank")?;
        let sql = if has_rank {
            "SELECT function_id, function_en, function_de, phase, rank, position, value, label, job \
             FROM job_param WHERE ecu_variant = ?1 AND function_id = ?2 \
             ORDER BY phase, rank, position"
        } else {
            "SELECT function_id, function_en, function_de, phase, NULL, position, value, label, job \
             FROM job_param WHERE ecu_variant = ?1 AND function_id = ?2 \
             ORDER BY phase, position"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map(rusqlite::params![variant, function_id], |row| {
            Ok(JobParameterEntry {
                function_id: row.get(0)?,
                function_en: row.get(1)?,
                function_de: row.get(2)?,
                phase: row.get(3)?,
                rank: row.get(4)?,
                position: row.get(5)?,
                value: row.get(6)?,
                label: row.get(7)?,
                job: row.get(8)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// The hold parameters and operator text for a fixed function.
    ///
    /// Returns the [`FixedFunction`] row for `function_id` from the
    /// `fixed_function` table (see [`FixedFunction`] and
    /// `scripts/build-semantic-db.sh`). `None` when the function is not in the
    /// table or the extract predates it (a pre-v6 DB) — the missing-table and
    /// missing-row cases both degrade to `None`, never an error.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn fixed_function(&self, function_id: i64) -> Result<Option<FixedFunction>, SemanticError> {
        if !self.has_table("fixed_function")? {
            return Ok(None);
        }
        let mut stmt = self.conn.prepare(
            "SELECT function_id, activation, activation_duration_ms, preparing_text, \
             processing_text, post_text FROM fixed_function WHERE function_id = ?1",
        )?;
        let row = stmt
            .query_row([function_id], |r| {
                Ok(FixedFunction {
                    function_id: r.get(0)?,
                    activation: r.get(1)?,
                    activation_duration_ms: r.get(2)?,
                    preparing_text: r.get(3)?,
                    processing_text: r.get(4)?,
                    post_text: r.get(5)?,
                })
            })
            .optional()?;
        Ok(row)
    }

    /// List the DISTINCT catalog service functions an ECU `variant` can run.
    ///
    /// Folds the variant's `job_param` rows to one [`ServiceFunctionCatalogEntry`]
    /// per DISTINCT `(function_id, function_en, function_de)` — the discovery half
    /// of the write path: it yields the integer `function_id` a service runner
    /// takes (a different identifier space from the SGBD-derived string labels).
    /// `has_reset` is true when ANY of the function's rows is a `Reset` phase
    /// (case-insensitive); the hold summary (`activation`,
    /// `activation_duration_ms`, `preparing_text`) is LEFT-joined from
    /// `fixed_function`, so a function with no fixed-function row keeps its id with
    /// `None` hold fields. Ordered by `function_id`. Empty when the variant is
    /// unknown or the extract predates the `job_param` table (a pre-v4 DB) — the
    /// missing-table case degrades to empty, not an error; the hold fields are
    /// additionally `None` on a pre-v6 extract (no `fixed_function` table).
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn service_functions_for_variant(
        &self,
        variant: &str,
    ) -> Result<Vec<ServiceFunctionCatalogEntry>, SemanticError> {
        if !self.has_table("job_param")? {
            return Ok(Vec::new());
        }
        // Aggregate the phases per function in a subquery (so `has_reset` is a clean
        // aggregate over the whole function), then LEFT JOIN the one-row-per-function
        // `fixed_function` for the hold summary. The join is dropped entirely on a
        // pre-v6 extract that lacks the table, leaving the hold fields NULL.
        let sql = if self.has_table("fixed_function")? {
            "SELECT g.function_id, g.function_en, g.function_de, g.has_reset, \
                    ff.activation, ff.activation_duration_ms, ff.preparing_text \
             FROM (SELECT function_id, function_en, function_de, \
                          MAX(CASE WHEN phase = 'Reset' COLLATE NOCASE THEN 1 ELSE 0 END) AS has_reset \
                   FROM job_param WHERE ecu_variant = ?1 \
                   GROUP BY function_id, function_en, function_de) g \
             LEFT JOIN fixed_function ff ON ff.function_id = g.function_id \
             ORDER BY g.function_id"
        } else {
            "SELECT function_id, function_en, function_de, \
                    MAX(CASE WHEN phase = 'Reset' COLLATE NOCASE THEN 1 ELSE 0 END), \
                    NULL, NULL, NULL \
             FROM job_param WHERE ecu_variant = ?1 \
             GROUP BY function_id, function_en, function_de \
             ORDER BY function_id"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([variant], |row| {
            let function_en: Option<String> = row.get(1)?;
            let function_de: Option<String> = row.get(2)?;
            Ok(ServiceFunctionCatalogEntry {
                function_id: row.get(0)?,
                title: function_en.or(function_de),
                has_reset: row.get::<_, i64>(3)? != 0,
                activation: row.get(4)?,
                activation_duration_ms: row.get(5)?,
                preparing_text: row.get(6)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// List ISTA's ECU tree for a platform `series` (the graph view).
    ///
    /// Returns every catalogued address of the platform's bordnet — display
    /// name, group SGBD, bus, grid position, minimal-configuration flag — from
    /// the `ecu_tree` table (see [`EcuTreeEntry`]). The series matches
    /// case-insensitively (`f25_1404` == `F25_1404`). Empty when the series is
    /// unknown or the extract predates the table (a pre-v5 DB) — the
    /// missing-table case degrades to empty, not an error.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn ecu_tree(&self, series: &str) -> Result<Vec<EcuTreeEntry>, SemanticError> {
        if !self.has_table("ecu_tree")? {
            return Ok(Vec::new());
        }
        let mut stmt = self.conn.prepare(
            "SELECT address, name, group_sgbd, bus, bus_label, col, row, minimal \
             FROM ecu_tree WHERE series = ?1 COLLATE NOCASE \
               AND address BETWEEN 0 AND 255 \
             ORDER BY bus, row, col, address",
        )?;
        let rows = stmt.query_map([series], |row| {
            Ok(EcuTreeEntry {
                address: row.get::<_, i64>(0)? as u8,
                name: row.get(1)?,
                group_sgbd: row.get(2)?,
                bus: row.get(3)?,
                bus_label: row.get(4)?,
                col: row.get(5)?,
                row: row.get(6)?,
                minimal: row.get::<_, i64>(7)? != 0,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// List the platform series the ECU-tree extract knows (e.g. `F20`,
    /// `F25_1404`), sorted. Empty on a pre-v5 DB.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn ecu_tree_series(&self) -> Result<Vec<String>, SemanticError> {
        if !self.has_table("ecu_tree")? {
            return Ok(Vec::new());
        }
        let mut stmt = self
            .conn
            .prepare("SELECT DISTINCT series FROM ecu_tree ORDER BY series")?;
        let rows = stmt.query_map([], |row| row.get(0))?;
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
            // rows only (no BMW data); realistic shape: variant, name, unit, scaling,
            // job, and ISTA's own title (the fleet-wide semantic key). One row omits
            // the English title to exercise the German fallback.
            conn.execute_batch(
                "CREATE TABLE measurement (ecu_variant TEXT, name TEXT, unit TEXT, mul REAL, offset REAL, round INTEGER, zahlenformat TEXT, job TEXT, title_en TEXT, title_de TEXT);
                 INSERT INTO measurement VALUES ('dde_a','STAT_EXAMPLE_TEMP_WERT','°C',1.0,0.0,0,NULL,'STATUS_LESEN','101 EXAMPLE engine temperature','101 BEISPIEL Motortemperatur');
                 INSERT INTO measurement VALUES ('dde_a','STAT_EXAMPLE_VOLT_WERT','V',0.001,0.0,3,NULL,'STATUS_BLOCK_LESEN',NULL,'104 BEISPIEL Batteriespannung');
                 INSERT INTO measurement VALUES ('fem_20','STAT_OTHER_WERT','%',1.0,0.0,1,NULL,'STATUS_LESEN','201 EXAMPLE other value',NULL);",
            )
            .unwrap();
            // Task 2 (resolve_quantity) rows: each variant name below is scoped to
            // its own test, so it cannot change the counts asserted elsewhere in
            // this file. Mirrors real evidence from the plan: STAT_UBATT_WERT is
            // volts on some variants and millivolts on others under the SAME
            // title, and ISTA titles the accelerator pedal and the IBS-qualified
            // rail differently from plain "Battery voltage". eng_ambig reproduces
            // a second real EDIABAS name for a second battery rail
            // (STAT_UBATT2_WERT) that ALSO carries a matching title — two
            // different measurements must refuse to resolve rather than pick one.
            conn.execute_batch(
                "INSERT INTO measurement VALUES ('eng_v','STAT_UBATT_WERT','V',1.0,0.0,1,NULL,'STATUS_LESEN','104 Battery voltage',NULL);
                 INSERT INTO measurement VALUES ('eng_mv','STAT_UBATT_WERT','mV',1.0,0.0,0,NULL,'STATUS_LESEN','Battery voltage',NULL);
                 INSERT INTO measurement VALUES ('eng_nounit','STAT_UBATT_WERT',NULL,1.0,0.0,0,NULL,'STATUS_LESEN','Battery voltage',NULL);
                 INSERT INTO measurement VALUES ('eng_trap','STAT_PWG1_SPANNUNG_WERT','mV',1.0,0.0,0,NULL,'STATUS_LESEN','907 Accelerator pedal, hall effect sensor 1: Voltage',NULL);
                 INSERT INTO measurement VALUES ('eng_qual','STAT_UBATT_IBS_WERT','V',1.0,0.0,0,NULL,'STATUS_LESEN','803 Battery voltage, IBS',NULL);
                 INSERT INTO measurement VALUES ('eng_ambig','STAT_UBATT_WERT','V',1.0,0.0,0,NULL,'STATUS_LESEN','Battery voltage',NULL);
                 INSERT INTO measurement VALUES ('eng_ambig','STAT_UBATT2_WERT','V',1.0,0.0,0,NULL,'STATUS_LESEN','104 Battery voltage',NULL);",
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
            // The v6 extract adds each fixed function's hold params + operator
            // text. Synthetic rows only: 9001 is a timed 5 s hold with preparing
            // text; 9002 is Activation==0 (hold-until-stop) with no text.
            conn.execute_batch(
                "CREATE TABLE fixed_function (function_id INTEGER, activation INTEGER, activation_duration_ms INTEGER, preparing_text TEXT, processing_text TEXT, post_text TEXT);
                 INSERT INTO fixed_function VALUES (9001, 1, 5000, 'Ansteuerung 5s', NULL, NULL);
                 INSERT INTO fixed_function VALUES (9002, 0, NULL, NULL, NULL, NULL);",
            )
            .unwrap();
            // The v5 extract adds the per-platform ISTA ECU tree (bordnet).
            // Synthetic rows only; realistic shape incl. a minimal-config core.
            conn.execute_batch(
                "CREATE TABLE ecu_tree (series TEXT, address INTEGER, name TEXT, group_sgbd TEXT, bus TEXT, bus_label TEXT, col INTEGER, row INTEGER, minimal INTEGER);
                 INSERT INTO ecu_tree VALUES ('X20',18,'DME','G_MOTOR','FACAN','PT-CAN',1,2,1);
                 INSERT INTO ecu_tree VALUES ('X20',64,'FEM','G_FEM','KCAN','K-CAN',0,5,1);
                 INSERT INTO ecu_tree VALUES ('X20',99,'EXTRA','G_EXTRA','KCAN','K-CAN',3,5,0);
                 INSERT INTO ecu_tree VALUES ('X25',18,'DDE','G_MOTOR','FACAN','PT-CAN',1,2,1);
                 CREATE TABLE ecu_housing (series TEXT, col INTEGER, row INTEGER, address INTEGER);
                 INSERT INTO ecu_housing VALUES ('X20',0,5,64);
                 INSERT INTO ecu_housing VALUES ('X20',0,5,99);",
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

    /// Build a synthetic semantic DB whose `measurement` table exists (a real v4
    /// extract) but, when `with_titles=false`, predates the title_en/title_de
    /// columns this task adds — a real intermediate shape distinct from the
    /// whole-table-missing case `fixture_opts(false)` covers, needed to prove
    /// `Catalog::measurements` degrades `title` to `None` instead of erroring on
    /// the missing columns.
    fn fixture_measurement_titles(with_titles: bool) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sem.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE ecu (address INTEGER, variant TEXT, group_name TEXT, title_en TEXT, title_de TEXT);
             INSERT INTO ecu VALUES (18, 'dde_a', 'd_0012', 'Engine', NULL);",
        )
        .unwrap();
        if with_titles {
            conn.execute_batch(
                "CREATE TABLE measurement (ecu_variant TEXT, name TEXT, unit TEXT, mul REAL, offset REAL, round INTEGER, zahlenformat TEXT, job TEXT, title_en TEXT, title_de TEXT);
                 INSERT INTO measurement VALUES ('dde_a','STAT_UBATT_WERT','V',1.0,0.0,0,NULL,'STATUS_LESEN','104 EXAMPLE Battery voltage','104 BEISPIEL Batteriespannung');",
            )
            .unwrap();
        } else {
            conn.execute_batch(
                "CREATE TABLE measurement (ecu_variant TEXT, name TEXT, unit TEXT, mul REAL, offset REAL, round INTEGER, zahlenformat TEXT, job TEXT);
                 INSERT INTO measurement VALUES ('dde_a','STAT_UBATT_WERT','V',1.0,0.0,0,NULL,'STATUS_LESEN');",
            )
            .unwrap();
        }
        (dir, path)
    }

    /// Build a synthetic semantic DB whose `job_param` table exists (a real v4
    /// extract) but, when `with_rank=false`, predates the `rank` column the
    /// `XEP_REFECUJOBS` join adds — a real intermediate shape distinct from the
    /// whole-table-missing case `fixture_opts(false)` covers, needed to prove
    /// `Catalog::job_parameters` degrades `rank` to `None` (falling back to
    /// `position`-only order) instead of erroring on the missing column.
    ///
    /// `with_rank=true` reproduces real ISTA data: the SAME job name
    /// (`STEUERN_LAMPEN_DIGITAL`) invoked TWICE in one phase, at ranks 1 and 2
    /// (see `scripts/build-semantic-db.sh`). Rows are inserted rank-2-then-
    /// rank-1 with position values that overlap (`1,2` and `1,2`), so a fix that
    /// adds the column but orders by `position` alone — ignoring `rank` — would
    /// interleave the two invocations instead of keeping each one's positions
    /// together.
    fn fixture_job_param_rank(with_rank: bool) -> (TempDir, PathBuf) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sem.db");
        let conn = Connection::open(&path).unwrap();
        if with_rank {
            conn.execute_batch(
                "CREATE TABLE job_param (ecu_variant TEXT, function_id INTEGER, function_en TEXT, function_de TEXT, phase TEXT, rank INTEGER, position INTEGER, value TEXT, label TEXT, job TEXT);
                 INSERT INTO job_param VALUES ('ccu_01',9010,'EXAMPLE lamps: activation',NULL,'Main',2,1,'RIGHT-1',NULL,'STEUERN_LAMPEN_DIGITAL');
                 INSERT INTO job_param VALUES ('ccu_01',9010,'EXAMPLE lamps: activation',NULL,'Main',2,2,'RIGHT-2',NULL,'STEUERN_LAMPEN_DIGITAL');
                 INSERT INTO job_param VALUES ('ccu_01',9010,'EXAMPLE lamps: activation',NULL,'Main',1,1,'LEFT-1',NULL,'STEUERN_LAMPEN_DIGITAL');
                 INSERT INTO job_param VALUES ('ccu_01',9010,'EXAMPLE lamps: activation',NULL,'Main',1,2,'LEFT-2',NULL,'STEUERN_LAMPEN_DIGITAL');",
            )
            .unwrap();
        } else {
            conn.execute_batch(
                "CREATE TABLE job_param (ecu_variant TEXT, function_id INTEGER, function_en TEXT, function_de TEXT, phase TEXT, position INTEGER, value TEXT, label TEXT, job TEXT);
                 INSERT INTO job_param VALUES ('ccu_01',9010,'EXAMPLE lamps: activation',NULL,'Main',1,'LEFT-1',NULL,'STEUERN_LAMPEN_DIGITAL');
                 INSERT INTO job_param VALUES ('ccu_01',9010,'EXAMPLE lamps: activation',NULL,'Main',2,'LEFT-2',NULL,'STEUERN_LAMPEN_DIGITAL');",
            )
            .unwrap();
        }
        (dir, path)
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
        assert_eq!(
            temp.title.as_deref(),
            Some("101 EXAMPLE engine temperature")
        );
        let volt = ms
            .iter()
            .find(|m| m.name == "STAT_EXAMPLE_VOLT_WERT")
            .unwrap();
        assert_eq!(volt.mul, Some(0.001));
        assert_eq!(volt.round, Some(3));
        // No English title on this row — falls back to German, not None.
        assert_eq!(volt.title.as_deref(), Some("104 BEISPIEL Batteriespannung"));
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
    fn measurements_title_degrades_to_none_on_a_pre_title_measurement_table() {
        // The `measurement` TABLE is present (a real v4 extract) but predates the
        // title_en/title_de columns this task adds — a real shape, distinct from
        // the whole-table-missing case above. Must degrade `title` to None rather
        // than erroring on the missing columns.
        let (_dir, path) = fixture_measurement_titles(false);
        let cat = Catalog::open(&path).unwrap();
        let ms = cat.measurements("dde_a").unwrap();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].title, None);
    }

    #[test]
    fn measurements_title_prefers_english_over_german() {
        let (_dir, path) = fixture_measurement_titles(true);
        let cat = Catalog::open(&path).unwrap();
        let ms = cat.measurements("dde_a").unwrap();
        assert_eq!(ms.len(), 1);
        assert_eq!(ms[0].title.as_deref(), Some("104 EXAMPLE Battery voltage"));
    }

    #[test]
    fn resolve_quantity_uses_istas_label_and_normalises_the_unit() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();

        let v = cat
            .resolve_quantity("eng_v", Quantity::BatteryVoltage)
            .unwrap()
            .unwrap();
        assert_eq!(v.name, "STAT_UBATT_WERT");
        assert_eq!(v.factor, 1.0);

        // Same label, millivolts: without normalisation a 12.0 V floor would pass
        // at 12 mV and a flat battery would sail through.
        let mv = cat
            .resolve_quantity("eng_mv", Quantity::BatteryVoltage)
            .unwrap()
            .unwrap();
        assert_eq!(mv.factor, 0.001);
        assert!(
            11_800.0 * mv.factor < 12.0,
            "a flat battery must fail a 12 V floor"
        );

        // No unit -> unresolvable, never assumed.
        assert!(
            cat.resolve_quantity("eng_nounit", Quantity::BatteryVoltage)
                .unwrap()
                .is_none()
        );
        // The pedal sensor must NOT satisfy battery voltage.
        assert!(
            cat.resolve_quantity("eng_trap", Quantity::BatteryVoltage)
                .unwrap()
                .is_none()
        );
        // A qualified label is a different sensor.
        assert!(
            cat.resolve_quantity("eng_qual", Quantity::BatteryVoltage)
                .unwrap()
                .is_none()
        );
        // Unknown variant resolves to nothing rather than erroring.
        assert!(
            cat.resolve_quantity("nope", Quantity::BatteryVoltage)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolve_quantity_refuses_when_two_different_measurements_match_the_same_title() {
        // Real DDEs carry more than one battery-rail measurement (STAT_UBATT_WERT
        // and a second rail, STAT_UBATT2_WERT — see the plan's evidence table).
        // eng_ambig gives both a title that matches BatteryVoltage. A plausible
        // wrong implementation ("take the first match") would still pass every
        // other test in this file, since none of them puts two DIFFERENT names on
        // one variant — so this test exists specifically to kill that mutant.
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        assert!(
            cat.resolve_quantity("eng_ambig", Quantity::BatteryVoltage)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolve_quantity_degrades_without_the_table_or_titles() {
        // A pre-v4 extract has no `measurement` table at all.
        let (_dir, path) = fixture_opts(false);
        let cat = Catalog::open(&path).unwrap();
        assert!(
            cat.resolve_quantity("eng_v", Quantity::BatteryVoltage)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolve_quantity_degrades_when_the_table_lacks_title_columns() {
        // A real v4 extract whose `measurement` table predates the title_en
        // column — distinct from the whole-table-missing case above, and not
        // exercised by it: a fix that checked `has_table` alone (dropping the
        // `has_column` half of the guard) would still pass that test but would
        // surface a raw SQL error here instead of degrading to None.
        let (_dir, path) = fixture_measurement_titles(false);
        let cat = Catalog::open(&path).unwrap();
        assert!(
            cat.resolve_quantity("dde_a", Quantity::BatteryVoltage)
                .unwrap()
                .is_none()
        );
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
    fn job_parameters_orders_by_rank_before_position_when_a_phase_repeats_a_job() {
        // Real ISTA data invokes the same job name more than once within one
        // phase (e.g. STEUERN_LAMPEN_DIGITAL, left/right lamps), distinguished
        // only by rank. The fixture inserts rank 2 before rank 1, and both
        // invocations reuse positions 1 and 2 — so a fix that adds the `rank`
        // column but forgets to put it in `ORDER BY` (sorting by `position`
        // alone) would interleave the two invocations (RIGHT-1, LEFT-1,
        // RIGHT-2, LEFT-2) instead of keeping each one contiguous.
        let (_dir, path) = fixture_job_param_rank(true);
        let cat = Catalog::open(&path).unwrap();
        let rows = cat
            .job_parameters("ccu_01", "STEUERN_LAMPEN_DIGITAL")
            .unwrap();
        assert_eq!(
            rows.len(),
            4,
            "both invocations' rows must survive: {rows:?}"
        );
        assert_eq!(
            rows.iter()
                .map(|r| r.value.as_deref().unwrap())
                .collect::<Vec<_>>(),
            ["LEFT-1", "LEFT-2", "RIGHT-1", "RIGHT-2"],
            "rank 1's invocation must come fully before rank 2's, not interleave by position"
        );
        assert_eq!(
            rows.iter().map(|r| r.rank).collect::<Vec<_>>(),
            [Some(1), Some(1), Some(2), Some(2)]
        );
    }

    #[test]
    fn job_parameters_rank_degrades_to_none_on_a_pre_rank_extract() {
        // The job_param TABLE is present (a real v4 extract) but predates the
        // `rank` column this task adds — distinct from the whole-table-missing
        // case above. Must degrade `rank` to None and keep working via
        // position-only order, not error on the missing column.
        let (_dir, path) = fixture_job_param_rank(false);
        let cat = Catalog::open(&path).unwrap();
        let rows = cat
            .job_parameters("ccu_01", "STEUERN_LAMPEN_DIGITAL")
            .unwrap();
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.rank.is_none()));
        assert_eq!(rows.iter().map(|r| r.position).collect::<Vec<_>>(), [1, 2]);
    }

    #[test]
    fn job_parameters_for_function_returns_all_phases_and_jobs_ordered() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        // Function 9001 on dde_a spans Main and Reset, job STEUERN_EXAMPLE.
        let rows = cat.job_parameters_for_function("dde_a", 9001).unwrap();
        assert!(rows.iter().all(|r| r.function_id == 9001), "{rows:?}");
        assert!(rows.iter().all(|r| r.job == "STEUERN_EXAMPLE"), "{rows:?}");
        // Phase order Main before Reset, and every returned row belongs to 9001 only.
        let phases: Vec<&str> = rows
            .iter()
            .map(|r| r.phase.as_deref().unwrap_or(""))
            .collect();
        let main_at = phases.iter().position(|p| *p == "Main").unwrap();
        let reset_at = phases.iter().position(|p| *p == "Reset").unwrap();
        assert!(main_at < reset_at, "Main must precede Reset: {phases:?}");
        // A different function on the same variant is NOT included.
        assert!(
            cat.job_parameters_for_function("dde_a", 9002)
                .unwrap()
                .iter()
                .all(|r| r.function_id == 9002)
        );
    }

    #[test]
    fn fixed_function_reads_hold_params_and_missing_table_is_none() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        let ff = cat.fixed_function(9001).unwrap().expect("9001 present");
        assert_eq!(ff.activation, Some(1));
        assert_eq!(ff.activation_duration_ms, Some(5000));
        assert_eq!(ff.preparing_text.as_deref(), Some("Ansteuerung 5s"));
        // A function absent from the table resolves to None, not an error.
        assert!(cat.fixed_function(4242).unwrap().is_none());
    }

    #[test]
    fn service_functions_for_variant_lists_distinct_functions_scoped_with_reset_and_hold() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        let fns = cat.service_functions_for_variant("dde_a").unwrap();
        // dde_a has exactly two DISTINCT catalog functions, ordered by id. 9002's
        // three Main rows fold to ONE entry — not three.
        assert_eq!(
            fns.iter().map(|f| f.function_id).collect::<Vec<_>>(),
            [9001, 9002]
        );
        let f9001 = fns.iter().find(|f| f.function_id == 9001).unwrap();
        let f9002 = fns.iter().find(|f| f.function_id == 9002).unwrap();
        // has_reset is computed per function: 9001 has a Main+Reset pair, 9002 is
        // Main-only. Kills a `has_reset` mutated to a constant either way.
        assert!(f9001.has_reset, "9001 defines a Reset phase");
        assert!(!f9002.has_reset, "9002 is Main-only, no Reset");
        // Title precedence: 9001 carries only a German function title; 9002 English.
        assert_eq!(f9001.title.as_deref(), Some("BEISPIEL Ventil"));
        assert_eq!(f9002.title.as_deref(), Some("EXAMPLE fan: activation"));
        // Hold summary is LEFT-joined from fixed_function: 9001 is a timed 5 s hold
        // with operator text; 9002 is Activation==0 (hold-until-stop), no text.
        assert_eq!(f9001.activation, Some(1));
        assert_eq!(f9001.activation_duration_ms, Some(5000));
        assert_eq!(f9001.preparing_text.as_deref(), Some("Ansteuerung 5s"));
        assert_eq!(f9002.activation, Some(0));
        assert_eq!(f9002.activation_duration_ms, None);
        assert_eq!(f9002.preparing_text, None);

        // Scoped by variant: fem_20's function set is DISTINCT. dde_a must not leak
        // 9003, and fem_20 must not see 9001/9002 — kills a WHERE that stops scoping.
        let fem = cat.service_functions_for_variant("fem_20").unwrap();
        assert_eq!(
            fem.iter().map(|f| f.function_id).collect::<Vec<_>>(),
            [9003]
        );
        // 9003 has no fixed_function row — the LEFT JOIN yields NULL hold fields,
        // not an error, and the id still comes back.
        assert_eq!(fem[0].activation, None);
        assert_eq!(fem[0].title.as_deref(), Some("EXAMPLE other"));
        // An unknown variant is empty, not an error.
        assert!(
            cat.service_functions_for_variant("nope")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn service_functions_for_variant_degrades_to_empty_without_the_table() {
        // A pre-v4 extract (no job_param table) must not error.
        let (_dir, path) = fixture_opts(false);
        let cat = Catalog::open(&path).unwrap();
        assert!(
            cat.service_functions_for_variant("dde_a")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn service_functions_for_variant_hold_degrades_to_none_without_fixed_function() {
        // A real v4 extract whose `job_param` table exists but that predates the v6
        // `fixed_function` table — distinct from the whole-table-missing case. The
        // ids and has_reset still resolve; the hold fields degrade to None rather
        // than erroring on the missing table.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("sem.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE job_param (ecu_variant TEXT, function_id INTEGER, function_en TEXT, function_de TEXT, phase TEXT, rank INTEGER, position INTEGER, value TEXT, label TEXT, job TEXT);
             INSERT INTO job_param VALUES ('dde_a',9001,'EXAMPLE valve',NULL,'Main',1,1,'90',NULL,'STEUERN_EXAMPLE');
             INSERT INTO job_param VALUES ('dde_a',9001,'EXAMPLE valve',NULL,'Reset',1,1,'0',NULL,'STEUERN_EXAMPLE');",
        )
        .unwrap();
        let cat = Catalog::open(&path).unwrap();
        let fns = cat.service_functions_for_variant("dde_a").unwrap();
        assert_eq!(fns.len(), 1);
        assert_eq!(fns[0].function_id, 9001);
        assert!(
            fns[0].has_reset,
            "Reset detection works without fixed_function"
        );
        assert_eq!(fns[0].activation, None);
        assert_eq!(fns[0].activation_duration_ms, None);
        assert_eq!(fns[0].preparing_text, None);
    }

    #[test]
    fn ecu_tree_lists_a_platform_case_insensitively() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();
        // Series is scoped and case-insensitive (a user may type `x20`).
        let tree = cat.ecu_tree("x20").unwrap();
        assert_eq!(tree.len(), 3);
        let dme = tree.iter().find(|e| e.address == 18).unwrap();
        assert_eq!(dme.name.as_deref(), Some("DME"));
        assert_eq!(dme.bus_label.as_deref(), Some("PT-CAN"));
        assert!(dme.minimal);
        let extra = tree.iter().find(|e| e.address == 99).unwrap();
        assert!(!extra.minimal);
        // The other platform's tree stays out; unknown series is empty.
        assert_eq!(cat.ecu_tree("X25").unwrap().len(), 1);
        assert!(cat.ecu_tree("nope").unwrap().is_empty());
        // Discoverability: the extract's series list.
        assert_eq!(cat.ecu_tree_series().unwrap(), ["X20", "X25"]);
    }

    #[test]
    fn bordnet_series_resolution_follows_the_dated_variant_ladder() {
        let known: Vec<String> = ["F20", "F25", "F25_1404", "X99_2001"]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
        // Zero-dropped match + dated variant: a 2017-03 factory level is past the
        // 2014-04 facelift, so the dated characteristics win (the owner's X3).
        assert_eq!(
            bordnet_series_for("F025-17-03-505", &known).as_deref(),
            Some("F25_1404")
        );
        // A pre-facelift build keeps the plain base.
        assert_eq!(
            bordnet_series_for("F025-14-03-505", &known).as_deref(),
            Some("F25")
        );
        // A platform with no dated variant maps plainly (the F20), case-insensitively.
        assert_eq!(
            bordnet_series_for("f020-13-11-500", &known).as_deref(),
            Some("F20")
        );
        // A dated variant in the future of the build date does not apply, and with
        // no plain base either the series is unresolved.
        assert_eq!(bordnet_series_for("X099-19-01-001", &known), None);
        // Unknown series and malformed levels degrade to None.
        assert_eq!(bordnet_series_for("G031-20-11-500", &known), None);
        assert_eq!(bordnet_series_for("garbage", &known), None);
    }

    #[test]
    fn ecu_tree_degrades_to_empty_without_the_table() {
        // A pre-v5 extract (no ecu_tree table) must not error.
        let (_dir, path) = fixture_opts(false);
        let cat = Catalog::open(&path).unwrap();
        assert!(cat.ecu_tree("X20").unwrap().is_empty());
        assert!(cat.ecu_tree_series().unwrap().is_empty());
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
