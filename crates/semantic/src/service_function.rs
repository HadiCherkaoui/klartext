//! BMW service functions (resets, adaptations, actuations, calibrations) from SGBD.
//!
//! Where `SG_FUNKTIONEN` (see [`crate::measurement`]) is the *read* catalog, the DDE
//! ships its *control* functions as four sibling tables, each parsed here into one
//! discoverable [`ServiceFunction`] catalog:
//!
//! - `CBSKENNUNG` — Condition-Based-Service maintenance counter resets (e.g. engine oil),
//! - `LERNWERTE_RUECK` — learned-value / adaptation resets,
//! - `STELLER` — physical actuators (throttle, fan, glow relay …),
//! - `ABGLEICH` — sensor / injector calibration writes.
//!
//! Alongside those, a small curated set of standalone DDE statistic-reset jobs whose
//! frames were derived from disassembly (`DERIVED_RESETS`) is injected when the ECU
//! actually defines the job.
//!
//! Every entry carries a [`Category`], a [`Risk`], and a [`Derivation`] status. The
//! [`Category`]/[`Risk`] gate *execution* by blast radius — low-risk resets are
//! confirmable on-car, high-risk actuation/calibration is human-driven only. The
//! [`Derivation`] records whether an execution frame could be **derived offline** (from
//! the BEST/2 disassembly, cited but UNCONFIRMED — `[verify against capture]`) or not
//! (the job's telegram is computed by data-dependent bytecode needing a capture or a
//! BEST/2 interpreter). Parsing follows the crate's degrade-quietly contract: an
//! unparsable row is skipped, never fatal, so an ECU without these tables yields an
//! empty catalog.
//!
//! This module is the *discovery* layer (what functions exist, and whether a frame is
//! known). Execution — gating, backup, read-back — lives in the CLI, never over MCP.

use klartext_sgbd::{Prg, SgbdError, Table};
use klartext_uds::{read_data_by_identifier, write_data_by_identifier};

/// Physical blast radius of a service function — the execution-gating axis.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Risk {
    /// State-only: a counter or adaptation reset; no component moves, reversible.
    Low,
    /// Moves a physical component, or alters combustion / safety behavior.
    High,
}

/// The class of operation a service function performs.
///
/// The category fixes the [`Risk`] and (later) which UDS service performs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    /// Condition-Based-Service maintenance counter reset (`CBSKENNUNG`).
    CbsReset,
    /// Learned-value / adaptation reset (`LERNWERTE_RUECK`).
    LearnedValueReset,
    /// Diagnostic statistic / histogram reset (standalone `STEUERN_*_RESET` job).
    StatisticReset,
    /// Physical actuator control (`STELLER`) — drives a component.
    ActuatorControl,
    /// Sensor / injector calibration write (`ABGLEICH`).
    Calibration,
}

impl Category {
    /// The blast-radius [`Risk`] of this category.
    ///
    /// Resets (CBS, learned-value, statistic) are [`Risk::Low`]; actuation and
    /// calibration are [`Risk::High`].
    pub fn risk(self) -> Risk {
        match self {
            Self::CbsReset | Self::LearnedValueReset | Self::StatisticReset => Risk::Low,
            Self::ActuatorControl | Self::Calibration => Risk::High,
        }
    }
}

/// How a service function's execution frame was obtained — the trust axis for running it.
///
/// A [`Derivation::Derived`] frame is read from the ECU's BEST/2 disassembly but has
/// **not** been validated against an on-car capture, so every use must be treated as
/// `[verify against capture]`. A [`Derivation::NotDerivable`] function is discovery-only:
/// its telegram is produced by data-dependent bytecode (read-modify-write, per-width
/// branching, or run-time value scaling) that offline analysis cannot pin.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Derivation {
    /// The UDS request was DERIVED from disassembly — UNCONFIRMED, `[verify against
    /// capture]`. Carries the request bytes and the disassembly citation.
    Derived {
        /// The derived UDS request bytes (no transport framing).
        request: Vec<u8>,
        /// Where the frame was read: job name, bytecode address, and SGBD.
        cite: &'static str,
    },
    /// No frame is derivable offline; carries a human explanation of why.
    NotDerivable {
        /// Why the frame cannot be derived offline (what it would take instead).
        reason: &'static str,
    },
}

impl Derivation {
    /// Whether a derived (though still unconfirmed) execution frame is available.
    pub fn is_derived(&self) -> bool {
        matches!(self, Self::Derived { .. })
    }

    /// The derived UDS request bytes, or `None` when the frame is not derivable.
    pub fn request(&self) -> Option<&[u8]> {
        match self {
            Self::Derived { request, .. } => Some(request),
            Self::NotDerivable { .. } => None,
        }
    }

    /// The disassembly citation for a derived frame, or `None`.
    pub fn citation(&self) -> Option<&'static str> {
        match self {
            Self::Derived { cite, .. } => Some(cite),
            Self::NotDerivable { .. } => None,
        }
    }

    /// The reason a frame is not derivable offline, or `None` for a derived one.
    pub fn reason(&self) -> Option<&'static str> {
        match self {
            Self::NotDerivable { reason } => Some(reason),
            Self::Derived { .. } => None,
        }
    }

    /// A short status word for display: `"derived-unconfirmed"` or `"frame-not-derivable"`.
    pub fn status(&self) -> &'static str {
        match self {
            Self::Derived { .. } => "derived-unconfirmed",
            Self::NotDerivable { .. } => "frame-not-derivable",
        }
    }
}

/// One discoverable service function: what it is, its class, its risk, its frame status.
///
/// The owned analogue of [`crate::measurement::Measurement`] for the control side.
/// `id` is the value the frame targets — the CBS counter id for a [`Category::CbsReset`],
/// the DID/routine id for a derived statistic reset, else the table's local identifier
/// (`LID`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceFunction {
    /// Short code / label, e.g. `"Oel"`, `"IBSRE"`, `"DRO"`, `"MSA2Hist"`.
    pub label: String,
    /// Human description, e.g. `"Motoroel"`, `"Rücksetzen IBS-Erkennung"`.
    pub name: String,
    /// The class of operation.
    pub category: Category,
    /// The identifier the frame targets: the CBS counter id, a DID/RID, else the `LID`.
    pub id: u16,
    /// Whether an execution frame was derived offline (and its bytes/citation), or not.
    pub derivation: Derivation,
}

impl ServiceFunction {
    /// The blast-radius [`Risk`] of this function (from its [`Category`]).
    pub fn risk(&self) -> Risk {
        self.category.risk()
    }

    /// Whether a derived (unconfirmed) execution frame is available for this function.
    pub fn is_derived(&self) -> bool {
        self.derivation.is_derived()
    }

    /// The derived UDS request bytes, or `None` when the frame is not derivable.
    ///
    /// The bytes are UNCONFIRMED (`[verify against capture]`); the caller still gates
    /// execution by [`Risk`] and requires explicit confirmation.
    pub fn request(&self) -> Option<&[u8]> {
        self.derivation.request()
    }
}

/// A disassembly-derived reset whose BEST/2 emits ONE fixed UDS telegram.
///
/// These DDE statistic/histogram-reset jobs are not carried by the four control
/// tables; each is a standalone job whose frame is a single `move S1,{…}` literal (no
/// table-driven byte splicing, exactly one `xsend`), so it is derivable offline and
/// cited to one disassembly line. A reset is surfaced only when its `job` is present in
/// the ECU's SGBD (see [`ServiceFunctions::from_prg`]). Frames are UNCONFIRMED.
struct DerivedReset {
    /// The `service run` label, e.g. `"MSA2Hist"`.
    label: &'static str,
    /// Human description.
    name: &'static str,
    /// The BEST/2 job whose presence in the SGBD gates this entry.
    job: &'static str,
    /// A stable identifier for display: the DID (or routine id) the frame targets.
    id: u16,
    /// The derived UDS request bytes.
    request: &'static [u8],
    /// The disassembly citation (job, address, SGBD).
    cite: &'static str,
}

impl DerivedReset {
    /// Materialize this registry entry as an owned [`ServiceFunction`].
    fn to_service_function(&self) -> ServiceFunction {
        ServiceFunction {
            label: self.label.to_string(),
            name: self.name.to_string(),
            category: Category::StatisticReset,
            id: self.id,
            derivation: Derivation::Derived {
                request: self.request.to_vec(),
                cite: self.cite,
            },
        }
    }
}

/// The DDE statistic-reset frames derived from the `d72n47a0` BEST/2 (M8 oracle).
///
/// Each is a single writeDataByIdentifier (`0x2E`) or routineControl (`0x31`) telegram
/// read as a literal from the job's bytecode. All are LOW risk (diagnostic-statistic
/// reset; no physical actuation) and UNCONFIRMED — `[verify against capture]`.
/// See `docs/service-functions-findings.md` §12a for the derivation.
const DERIVED_RESETS: &[DerivedReset] = &[
    DerivedReset {
        label: "MSA2Hist",
        name: "MSA2 history table + ring-buffer reset",
        job: "STEUERN_MSA2HISTORIERESET",
        id: 0x5F84,
        request: &[0x2E, 0x5F, 0x84],
        cite: "STEUERN_MSA2HISTORIERESET @0x128A75 (d72n47a0)",
    },
    DerivedReset {
        label: "PMHist",
        name: "Power-management histogram reset",
        job: "STEUERN_PM_HISTOGRAM_RESET",
        id: 0x5FF5,
        request: &[0x2E, 0x5F, 0xF5, 0x04],
        cite: "STEUERN_PM_HISTOGRAM_RESET @0x16DD4E (d72n47a0)",
    },
    DerivedReset {
        label: "DAROL",
        name: "DAROL load-collective data reset",
        job: "STEUERN_DAROL_RESET",
        id: 0x6200,
        request: &[0x2E, 0x62, 0x00, 0x01],
        cite: "STEUERN_DAROL_RESET @0x1BBF27 (d72n47a0)",
    },
    DerivedReset {
        label: "LLKETA",
        name: "LLK-ETA statistic reset (charge-air-cooler)",
        job: "STEUERN_LLKETA_RESET",
        id: 0xF065,
        request: &[0x31, 0x01, 0xF0, 0x65],
        cite: "STEUERN_LLKETA_RESET @0x12D0CC (d72n47a0)",
    },
];

/// Why the learned-value resets cannot be derived offline (M8 disassembly finding).
///
/// `LERNWERTE_RUECKSETZEN` reads a status DID and computes its write from the ECU's
/// *live* response — a value that does not exist offline.
const LERNWERTE_REASON: &str = "LERNWERTE_RUECKSETZEN is read-modify-write (reads 22 5F D3, \
    then computes the 2E 5F 8A write from the live ECU response) with LID-width branching and \
    per-LID special cases — needs an on-car capture or a BEST/2 interpreter";

/// Why actuator control cannot be derived offline (and is refused regardless).
const STELLER_REASON: &str = "STEUERN_SELECTIV is 0x2F IO-control whose value is scaled from the \
    STELLER table at run time — physical actuation (refused); the frame needs a BEST/2 interpreter";

/// Why calibration writes cannot be derived offline (and are refused regardless).
const CALIBRATION_REASON: &str = "ABGLEICH_PROGRAMMIEREN_* writes externally-sourced calibration / \
    injector codes — refused; not derivable offline";

/// The control-side service functions of one ECU, in discovery order.
///
/// Built from an SGBD's four control tables plus the job-gated `DERIVED_RESETS`; an
/// ECU lacking those (e.g. one whose control lives only in job bytecode) yields an
/// empty catalog rather than an error.
#[derive(Debug, Clone, Default)]
pub struct ServiceFunctions {
    functions: Vec<ServiceFunction>,
}

impl ServiceFunctions {
    /// Build the catalog from a parsed SGBD: its control tables + job-gated resets.
    pub fn from_prg(prg: &Prg) -> Self {
        Self::from_tables_and_jobs(prg.tables(), |job| prg.has_job(job))
    }

    /// Build the catalog from control tables only (no standalone derived resets).
    ///
    /// Tables are read in the order given, so the catalog groups by source table.
    /// Any table that is not a recognized control table is ignored. Without a
    /// job-presence oracle the standalone `DERIVED_RESETS` cannot be confirmed, so
    /// none are added — use [`ServiceFunctions::from_prg`] for the full catalog.
    pub fn from_tables(tables: &[Table]) -> Self {
        Self::from_tables_and_jobs(tables, |_| false)
    }

    /// Build the catalog from `tables` plus a job-presence predicate.
    ///
    /// `has_job(name)` reports whether the ECU's SGBD defines a BEST/2 job; it gates
    /// the standalone `DERIVED_RESETS` so a DDE-specific reset frame is offered only
    /// for an ECU that actually has the job. [`ServiceFunctions::from_prg`] supplies
    /// [`Prg::has_job`].
    pub fn from_tables_and_jobs(tables: &[Table], has_job: impl Fn(&str) -> bool) -> Self {
        let mut functions = Vec::new();
        for table in tables {
            match table.name.as_str() {
                "CBSKENNUNG" => parse_cbs(table, &mut functions),
                "LERNWERTE_RUECK" => {
                    parse_labelled(
                        table,
                        Category::LearnedValueReset,
                        LERNWERTE_REASON,
                        &mut functions,
                    );
                }
                "STELLER" => {
                    parse_labelled(
                        table,
                        Category::ActuatorControl,
                        STELLER_REASON,
                        &mut functions,
                    );
                }
                "ABGLEICH" => {
                    parse_labelled(
                        table,
                        Category::Calibration,
                        CALIBRATION_REASON,
                        &mut functions,
                    );
                }
                _ => {}
            }
        }
        for reset in DERIVED_RESETS {
            if has_job(reset.job) {
                functions.push(reset.to_service_function());
            }
        }
        Self { functions }
    }

    /// Load the catalog from an SGBD `.prg` file at `path`.
    ///
    /// # Errors
    /// Returns [`SgbdError`] if the file cannot be read or parsed. A file that parses
    /// but carries none of the control tables or reset jobs yields an empty catalog
    /// (not an error).
    pub fn from_sgbd(path: impl AsRef<std::path::Path>) -> Result<Self, SgbdError> {
        Ok(Self::from_prg(&Prg::open(path)?))
    }

    /// All service functions, in discovery order.
    pub fn all(&self) -> &[ServiceFunction] {
        &self.functions
    }

    /// The first function whose `label` matches exactly, if any.
    pub fn by_label(&self, label: &str) -> Option<&ServiceFunction> {
        self.functions.iter().find(|f| f.label == label)
    }

    /// The functions of one [`Category`], in discovery order.
    pub fn by_category(&self, category: Category) -> impl Iterator<Item = &ServiceFunction> + '_ {
        self.functions
            .iter()
            .filter(move |f| f.category == category)
    }

    /// The number of discovered service functions.
    pub fn len(&self) -> usize {
        self.functions.len()
    }

    /// Whether the catalog is empty (the ECU exposes no control tables or reset jobs).
    pub fn is_empty(&self) -> bool {
        self.functions.is_empty()
    }
}

/// Parse `CBSKENNUNG` (`NR, CBS_K, CBS_K_TEXT`) into [`Category::CbsReset`] entries.
fn parse_cbs(table: &Table, out: &mut Vec<ServiceFunction>) {
    let nr = column(table, "NR");
    let code = column(table, "CBS_K");
    let text = column(table, "CBS_K_TEXT");
    let (Some(nr), Some(code), Some(text)) = (nr, code, text) else {
        return;
    };
    for row in &table.rows {
        let Some(id) = row.get(nr).and_then(|c| parse_hex(c)) else {
            continue;
        };
        let label = cell(row, code);
        if is_blank(&label) {
            continue;
        }
        out.push(ServiceFunction {
            label,
            name: cell(row, text),
            category: Category::CbsReset,
            id,
            derivation: cbs_derivation(id),
        });
    }
}

/// The derivation for CBS counter `id`: the derived write frame, or a not-derivable
/// note when the id does not fit the single-byte CBS_RESET record field.
///
/// Unlike the standalone [`DERIVED_RESETS`] (gated on [`Prg::has_job`]), a CBS row is
/// derived directly from the `CBSKENNUNG` table: the table's presence *is* the DDE
/// control catalog, which always defines `CBS_RESET`, so the table implies its job.
fn cbs_derivation(id: u16) -> Derivation {
    match u8::try_from(id) {
        Ok(cbs_id) => Derivation::Derived {
            request: build_cbs_reset_request(cbs_id),
            cite: "CBS_RESET @0x969BD (d72n47a0)",
        },
        Err(_) => Derivation::NotDerivable {
            reason: "CBS counter id exceeds the single-byte CBS_RESET record field",
        },
    }
}

/// Parse a `LABEL, TEXT, …, LID`-shaped control table into `category` entries.
///
/// Shared by `LERNWERTE_RUECK`, `STELLER`, and `ABGLEICH`, which differ in their
/// action-job columns but agree on the `LABEL`/`TEXT`/`LID` identity columns this
/// discovery layer needs. Each entry is [`Derivation::NotDerivable`] with `reason` (the
/// frame is bytecode-computed, not a static literal). A row without a parsable `LID`
/// is skipped.
fn parse_labelled(
    table: &Table,
    category: Category,
    reason: &'static str,
    out: &mut Vec<ServiceFunction>,
) {
    let label = column(table, "LABEL");
    let text = column(table, "TEXT");
    let lid = column(table, "LID");
    let (Some(label), Some(text), Some(lid)) = (label, text, lid) else {
        return;
    };
    for row in &table.rows {
        let Some(id) = row.get(lid).and_then(|c| parse_hex(c)) else {
            continue;
        };
        let label_cell = cell(row, label);
        if is_blank(&label_cell) {
            continue;
        }
        out.push(ServiceFunction {
            label: label_cell,
            name: cell(row, text),
            category,
            id,
            derivation: Derivation::NotDerivable { reason },
        });
    }
}

/// Whether a label cell is a blank or separator placeholder (empty or only dashes).
fn is_blank(label: &str) -> bool {
    label.is_empty() || label.chars().all(|c| c == '-')
}

/// The index of column `name` in `table`'s header, if present.
fn column(table: &Table, name: &str) -> Option<usize> {
    table.columns.iter().position(|c| c == name)
}

/// The trimmed cell at column index `i`, or an empty string if absent.
fn cell(row: &[String], i: usize) -> String {
    row.get(i)
        .map_or_else(String::new, |c| c.trim().to_string())
}

/// Parse a hex identifier like `0x602A` or `0x01` (with or without `0x`) into a `u16`.
///
/// Returns `None` for a blank (`-`/empty) or out-of-range cell, so that row degrades.
fn parse_hex(s: &str) -> Option<u16> {
    let t = s.trim();
    if t.is_empty() || t == "-" {
        return None;
    }
    let hex = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u16::from_str_radix(hex, 16).ok()
}

// ---------------------------------------------------------------------------
// CBS reset — the M7 first vertical slice. Reset a Condition-Based-Service
// maintenance counter (e.g. engine oil) via UDS 0x2E, read it back via 0x22.
//
// The frames are DERIVED from the `d72n47a0` `CBS_RESET` / `CBS_DATEN_LESEN`
// BEST/2 disassembly (the ediabasx offline oracle of M6), NOT a packet capture —
// pending on-car confirmation, exactly as M6 Part B's measurement frames.
// [verify against capture]
// ---------------------------------------------------------------------------

/// The DDE DID a CBS record is written to (`0x2E`) and read back from (`0x22`).
///
/// DERIVED from the `CBS_RESET` / `CBS_DATEN_LESEN` disassembly — [verify against capture].
pub const CBS_DID: u16 = 0x1001;

/// Leading record-count byte of a CBS write — always one record.
const CBS_RECORD_COUNT: u8 = 0x01;

/// CBS-record bytes that follow the component id in a reset write.
///
/// The `CBS_RESET` template defaults: availability `0x64` = 100 % (counter back to
/// full), service-count `0x1F` = "+1", remaining-distance `0x80 0x00` = "no change",
/// then don't-care unit/way/month/year/time/reserve (`0F FF 0F 3F FF 00`). A reset
/// differs between components only in the id byte before this tail. DERIVED from
/// disassembly — [verify against capture].
const CBS_RESET_RECORD_TAIL: [u8; 10] =
    [0x64, 0x1F, 0x80, 0x00, 0x0F, 0xFF, 0x0F, 0x3F, 0xFF, 0x00];

/// Build the UDS request that resets CBS counter `cbs_id` (e.g. `0x01` = engine oil).
///
/// Produces `2E 10 01 01 <cbs_id> 64 1F 80 00 0F FF 0F 3F FF 00` — WriteDataByIdentifier
/// to [`CBS_DID`] writing one CBS record whose availability is reset to 100 %.
/// `cbs_id` is the `CBSKENNUNG` id of a [`Category::CbsReset`] [`ServiceFunction`]
/// (its [`ServiceFunction::id`]). DERIVED from disassembly — [verify against capture].
pub fn build_cbs_reset_request(cbs_id: u8) -> Vec<u8> {
    let mut data = Vec::with_capacity(2 + CBS_RESET_RECORD_TAIL.len());
    data.push(CBS_RECORD_COUNT);
    data.push(cbs_id);
    data.extend_from_slice(&CBS_RESET_RECORD_TAIL);
    write_data_by_identifier(CBS_DID, &data)
}

/// Build the UDS request that reads the CBS block back (`22 10 01`) for verification.
///
/// The `62 10 01 <ANZ_CBS> …` response carries the per-component availability the
/// reset wrote; reading it after a reset confirms the write landed.
pub fn build_cbs_read_request() -> [u8; 3] {
    read_data_by_identifier(CBS_DID)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a [`Table`] with the given name, header, and rows (no BMW data).
    fn table(name: &str, columns: &[&str], rows: &[&[&str]]) -> Table {
        Table {
            name: name.to_string(),
            columns: columns.iter().map(|s| (*s).to_string()).collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|s| (*s).to_string()).collect())
                .collect(),
        }
    }

    fn cbs_table() -> Table {
        // CBSKENNUNG shape: NR, CBS_K, CBS_K_TEXT (engine oil + brake pad).
        table(
            "CBSKENNUNG",
            &["NR", "CBS_K", "CBS_K_TEXT"],
            &[
                &["0x01", "Oel", "Motoroel"],
                &["0x02", "Br_v", "Bremsbelag vorne"],
            ],
        )
    }

    fn lernwerte_table() -> Table {
        // LERNWERTE_RUECK identity columns (a learned-value reset row).
        table(
            "LERNWERTE_RUECK",
            &["LABEL", "TEXT", "LID", "JOB_PROG", "VALUE"],
            &[&[
                "IBSRE",
                "Rücksetzen IBS-Erkennung",
                "0xA0F7",
                "LERNWERTE_RUECKSETZEN",
                "0x00",
            ]],
        )
    }

    fn steller_table() -> Table {
        // STELLER identity columns (a throttle actuator row).
        table(
            "STELLER",
            &["LABEL", "TEXT", "LID", "JOB_EIN", "JOB_AUS"],
            &[&[
                "DRO",
                "Drosselklappe",
                "0x602A",
                "STEUERN_SELECTIV",
                "STEUERN_ENDE_SELECTIV",
            ]],
        )
    }

    #[test]
    fn cbs_rows_become_low_risk_derived_reset_functions() {
        let mut out = Vec::new();
        parse_cbs(&cbs_table(), &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "Oel");
        assert_eq!(out[0].name, "Motoroel");
        assert_eq!(out[0].id, 0x01);
        assert_eq!(out[0].category, Category::CbsReset);
        assert_eq!(out[0].risk(), Risk::Low);
        // The engine-oil CBS reset carries the derived (unconfirmed) frame.
        assert!(out[0].is_derived());
        assert_eq!(
            out[0].request(),
            Some(
                [
                    0x2E, 0x10, 0x01, 0x01, 0x01, 0x64, 0x1F, 0x80, 0x00, 0x0F, 0xFF, 0x0F, 0x3F,
                    0xFF, 0x00
                ]
                .as_slice()
            )
        );
        assert!(out[0].derivation.citation().unwrap().contains("CBS_RESET"));
    }

    #[test]
    fn learned_value_reset_is_low_risk_but_not_derivable() {
        let mut out = Vec::new();
        parse_labelled(
            &lernwerte_table(),
            Category::LearnedValueReset,
            LERNWERTE_REASON,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "IBSRE");
        assert_eq!(out[0].id, 0xA0F7);
        assert_eq!(out[0].risk(), Risk::Low);
        // Low risk, but the read-modify-write frame is not derivable offline.
        assert!(!out[0].is_derived());
        assert_eq!(out[0].request(), None);
        assert!(
            out[0]
                .derivation
                .reason()
                .unwrap()
                .contains("read-modify-write")
        );
    }

    #[test]
    fn actuator_control_is_high_risk_and_not_derivable() {
        let mut out = Vec::new();
        parse_labelled(
            &steller_table(),
            Category::ActuatorControl,
            STELLER_REASON,
            &mut out,
        );
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "DRO");
        assert_eq!(out[0].id, 0x602A);
        assert_eq!(out[0].category, Category::ActuatorControl);
        assert_eq!(out[0].risk(), Risk::High);
        assert!(!out[0].is_derived());
    }

    #[test]
    fn category_risk_split_is_blast_radius() {
        assert_eq!(Category::CbsReset.risk(), Risk::Low);
        assert_eq!(Category::LearnedValueReset.risk(), Risk::Low);
        assert_eq!(Category::StatisticReset.risk(), Risk::Low);
        assert_eq!(Category::ActuatorControl.risk(), Risk::High);
        assert_eq!(Category::Calibration.risk(), Risk::High);
    }

    #[test]
    fn derived_reset_is_injected_only_when_its_job_is_present() {
        // With the MSA2 job present, the derived statistic reset is discovered.
        let present =
            ServiceFunctions::from_tables_and_jobs(&[], |job| job == "STEUERN_MSA2HISTORIERESET");
        let msa2 = present.by_label("MSA2Hist").expect("MSA2 reset present");
        assert_eq!(msa2.category, Category::StatisticReset);
        assert_eq!(msa2.risk(), Risk::Low);
        assert!(msa2.is_derived());
        assert_eq!(msa2.request(), Some([0x2E, 0x5F, 0x84].as_slice()));

        // Without the job, it must not be surfaced (it is DDE-specific).
        let absent = ServiceFunctions::from_tables_and_jobs(&[], |_| false);
        assert!(absent.by_label("MSA2Hist").is_none());
        assert!(absent.is_empty());
    }

    #[test]
    fn llketa_reset_is_a_routine_control_frame() {
        let funcs =
            ServiceFunctions::from_tables_and_jobs(&[], |job| job == "STEUERN_LLKETA_RESET");
        let llketa = funcs.by_label("LLKETA").expect("LLKETA present");
        // RoutineControl startRoutine (0x31 0x01) of routine 0xF065.
        assert_eq!(llketa.request(), Some([0x31, 0x01, 0xF0, 0x65].as_slice()));
    }

    #[test]
    fn from_tables_unions_all_control_tables_without_jobs() {
        // Three control tables → one merged catalog; no derived resets without jobs.
        let funcs =
            ServiceFunctions::from_tables(&[cbs_table(), lernwerte_table(), steller_table()]);
        assert_eq!(funcs.len(), 4); // 2 CBS + 1 learned-value + 1 actuator
        assert_eq!(funcs.by_category(Category::CbsReset).count(), 2);
        assert_eq!(funcs.by_category(Category::StatisticReset).count(), 0);
        assert!(funcs.by_label("IBSRE").unwrap().risk() == Risk::Low);
        assert!(funcs.by_label("DRO").unwrap().risk() == Risk::High);
    }

    #[test]
    fn ecu_without_control_tables_or_jobs_yields_empty_catalog() {
        let funcs = ServiceFunctions::from_tables_and_jobs(
            &[table(
                "SG_FUNKTIONEN",
                &["ARG", "ID"],
                &[&["ITMOT", "0x4BC3"]],
            )],
            |_| false,
        );
        assert!(funcs.is_empty());
    }

    #[test]
    fn rows_with_unparsable_id_are_skipped() {
        let mut out = Vec::new();
        parse_cbs(
            &table(
                "CBSKENNUNG",
                &["NR", "CBS_K", "CBS_K_TEXT"],
                &[&["-", "x", "y"]],
            ),
            &mut out,
        );
        assert!(out.is_empty());
    }

    #[test]
    fn blank_or_separator_label_rows_are_skipped() {
        // SGBD tables carry `--` / `-` separator rows; they must not become functions.
        let funcs = ServiceFunctions::from_tables(&[table(
            "LERNWERTE_RUECK",
            &["LABEL", "TEXT", "LID"],
            &[
                &["--", "-", "0xA000"],
                &["IBSRE", "Rücksetzen IBS-Erkennung", "0xA0F7"],
            ],
        )]);
        assert_eq!(funcs.len(), 1);
        assert_eq!(funcs.all()[0].label, "IBSRE");
    }

    #[test]
    fn cbs_reset_request_for_engine_oil_is_the_derived_frame() {
        // Engine oil = CBSKENNUNG id 0x01; the DERIVED d72n47a0 CBS_RESET frame
        // (docs/service-functions-findings.md; ediabasx oracle) — [verify against capture].
        assert_eq!(
            build_cbs_reset_request(0x01),
            vec![
                0x2E, 0x10, 0x01, 0x01, 0x01, 0x64, 0x1F, 0x80, 0x00, 0x0F, 0xFF, 0x0F, 0x3F, 0xFF,
                0x00
            ]
        );
    }

    #[test]
    fn cbs_reset_request_splices_the_component_id() {
        // Brake fluid = id 0x03; only the id byte (index 4) changes vs oil.
        assert_eq!(build_cbs_reset_request(0x03)[4], 0x03);
        assert_eq!(build_cbs_reset_request(0x03).len(), 15);
    }

    #[test]
    fn cbs_read_back_request_is_22_1001() {
        assert_eq!(build_cbs_read_request(), [0x22, 0x10, 0x01]);
    }

    // End-to-end on the real DDE SGBD: load the `.prg`, build the control catalog.
    // Ignored by default (BYO data); asserts structure, the engine-oil CBS reset,
    // and the job-gated derived statistic resets.
    #[test]
    #[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
    fn real_dde_catalog_lists_control_and_derived_functions() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/Testmodule(1)/Ecu/d72n47a0.prg");
        let funcs = ServiceFunctions::from_sgbd(&path).expect("load real SGBD");
        // The DDE exposes all four control tables → a rich catalog.
        assert!(
            funcs.len() > 100,
            "expected many functions, got {}",
            funcs.len()
        );
        assert!(funcs.by_category(Category::CbsReset).count() >= 1);
        assert!(funcs.by_category(Category::ActuatorControl).count() >= 1);
        // The engine-oil CBS reset is present, low-risk, and carries a derived frame.
        let oil = funcs.by_label("Oel").expect("engine-oil CBS entry");
        assert_eq!(oil.category, Category::CbsReset);
        assert_eq!(oil.risk(), Risk::Low);
        assert!(oil.is_derived());
        // All four disassembly-derived statistic resets are discovered and derived.
        for label in ["MSA2Hist", "PMHist", "DAROL", "LLKETA"] {
            let f = funcs
                .by_label(label)
                .unwrap_or_else(|| panic!("derived reset {label} present"));
            assert_eq!(f.category, Category::StatisticReset);
            assert!(f.is_derived(), "{label} must carry a derived frame");
        }
    }
}
