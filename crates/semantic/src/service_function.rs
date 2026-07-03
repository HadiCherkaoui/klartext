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
//! Every entry is tagged with a [`Category`] and a [`Risk`] so the CLI can gate
//! execution by blast radius — low-risk resets are confirmable on-car, high-risk
//! actuation/calibration is human-driven only. Parsing follows the crate's
//! degrade-quietly contract: an unparsable row is skipped, never fatal, so an ECU
//! with none of these tables simply yields an empty catalog.
//!
//! This module is the *discovery* layer (what functions exist). Building the UDS
//! request that performs one is the separate, capture-gated request builder.

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
    /// Physical actuator control (`STELLER`) — drives a component.
    ActuatorControl,
    /// Sensor / injector calibration write (`ABGLEICH`).
    Calibration,
}

impl Category {
    /// The blast-radius [`Risk`] of this category.
    ///
    /// Resets are [`Risk::Low`]; actuation and calibration are [`Risk::High`].
    pub fn risk(self) -> Risk {
        match self {
            Self::CbsReset | Self::LearnedValueReset => Risk::Low,
            Self::ActuatorControl | Self::Calibration => Risk::High,
        }
    }
}

/// One discoverable service function: what it is, its class, and its risk.
///
/// The owned analogue of [`crate::measurement::Measurement`] for the control side.
/// `id` is the value the request is built around — the CBS counter id for a
/// [`Category::CbsReset`], else the table's local identifier (`LID`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceFunction {
    /// Short code / label, e.g. `"Oel"`, `"IBSRE"`, `"DRO"`.
    pub label: String,
    /// Human description, e.g. `"Motoroel"`, `"Rücksetzen IBS-Erkennung"`.
    pub name: String,
    /// The class of operation.
    pub category: Category,
    /// The identifier the request targets: the CBS counter id, else the `LID`.
    pub id: u16,
}

impl ServiceFunction {
    /// The blast-radius [`Risk`] of this function (from its [`Category`]).
    pub fn risk(&self) -> Risk {
        self.category.risk()
    }
}

/// The control-side service functions of one ECU, in discovery order.
///
/// Built from an SGBD's four control tables; an ECU lacking them (e.g. one whose
/// control lives only in job bytecode) yields an empty catalog rather than an error.
#[derive(Debug, Clone, Default)]
pub struct ServiceFunctions {
    functions: Vec<ServiceFunction>,
}

impl ServiceFunctions {
    /// Build the catalog from a parsed SGBD, reading every control table present.
    pub fn from_prg(prg: &Prg) -> Self {
        Self::from_tables(prg.tables())
    }

    /// Build the catalog from a set of SGBD tables, reading the control tables among them.
    ///
    /// Tables are read in the order given, so the catalog groups by source table.
    /// Any table that is not a recognized control table is ignored.
    pub fn from_tables(tables: &[Table]) -> Self {
        let mut functions = Vec::new();
        for table in tables {
            match table.name.as_str() {
                "CBSKENNUNG" => parse_cbs(table, &mut functions),
                "LERNWERTE_RUECK" => {
                    parse_labelled(table, Category::LearnedValueReset, &mut functions);
                }
                "STELLER" => parse_labelled(table, Category::ActuatorControl, &mut functions),
                "ABGLEICH" => parse_labelled(table, Category::Calibration, &mut functions),
                _ => {}
            }
        }
        Self { functions }
    }

    /// Load the catalog from an SGBD `.prg` file at `path`.
    ///
    /// # Errors
    /// Returns [`SgbdError`] if the file cannot be read or parsed. A file that parses
    /// but carries none of the control tables yields an empty catalog (not an error).
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

    /// Whether the catalog is empty (the ECU exposes no control tables).
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
        });
    }
}

/// Parse a `LABEL, TEXT, …, LID`-shaped control table into `category` entries.
///
/// Shared by `LERNWERTE_RUECK`, `STELLER`, and `ABGLEICH`, which differ in their
/// action-job columns but agree on the `LABEL`/`TEXT`/`LID` identity columns this
/// discovery layer needs. A row without a parsable `LID` is skipped.
fn parse_labelled(table: &Table, category: Category, out: &mut Vec<ServiceFunction>) {
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
    fn cbs_rows_become_low_risk_reset_functions() {
        let mut out = Vec::new();
        parse_cbs(&cbs_table(), &mut out);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].label, "Oel");
        assert_eq!(out[0].name, "Motoroel");
        assert_eq!(out[0].id, 0x01);
        assert_eq!(out[0].category, Category::CbsReset);
        assert_eq!(out[0].risk(), Risk::Low);
    }

    #[test]
    fn learned_value_reset_is_low_risk() {
        let mut out = Vec::new();
        parse_labelled(&lernwerte_table(), Category::LearnedValueReset, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "IBSRE");
        assert_eq!(out[0].id, 0xA0F7);
        assert_eq!(out[0].risk(), Risk::Low);
    }

    #[test]
    fn actuator_control_is_high_risk() {
        let mut out = Vec::new();
        parse_labelled(&steller_table(), Category::ActuatorControl, &mut out);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].label, "DRO");
        assert_eq!(out[0].id, 0x602A);
        assert_eq!(out[0].category, Category::ActuatorControl);
        assert_eq!(out[0].risk(), Risk::High);
    }

    #[test]
    fn category_risk_split_is_blast_radius() {
        assert_eq!(Category::CbsReset.risk(), Risk::Low);
        assert_eq!(Category::LearnedValueReset.risk(), Risk::Low);
        assert_eq!(Category::ActuatorControl.risk(), Risk::High);
        assert_eq!(Category::Calibration.risk(), Risk::High);
    }

    #[test]
    fn from_tables_unions_all_control_tables() {
        // Three control tables → one merged catalog, grouped by source order.
        let funcs =
            ServiceFunctions::from_tables(&[cbs_table(), lernwerte_table(), steller_table()]);
        assert_eq!(funcs.len(), 4); // 2 CBS + 1 learned-value + 1 actuator
        assert_eq!(funcs.by_category(Category::CbsReset).count(), 2);
        assert_eq!(funcs.by_label("IBSRE").unwrap().risk(), Risk::Low);
        assert_eq!(funcs.by_label("DRO").unwrap().risk(), Risk::High);
    }

    #[test]
    fn ecu_without_control_tables_yields_empty_catalog() {
        let funcs = ServiceFunctions::from_tables(&[table(
            "SG_FUNKTIONEN",
            &["ARG", "ID"],
            &[&["ITMOT", "0x4BC3"]],
        )]);
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
    // Ignored by default (BYO data); asserts structure and the engine-oil CBS reset.
    #[test]
    #[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
    fn real_dde_catalog_lists_control_functions() {
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
        // The engine-oil CBS reset is present and low-risk.
        let oil = funcs.by_label("Oel").expect("engine-oil CBS entry");
        assert_eq!(oil.category, Category::CbsReset);
        assert_eq!(oil.risk(), Risk::Low);
    }
}
