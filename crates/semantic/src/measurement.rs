//! Proprietary BMW measurement scaling from the SGBD `SG_FUNKTIONEN` table.
//!
//! M5 scaled the *standard* OBD-II PIDs (see [`crate::pid`]); this module scales the
//! *proprietary* measurements, whose recipe is data, not code. Each `SG_FUNKTIONEN`
//! row gives an internal id, a raw data type, a multiplier, a divisor, an offset,
//! and a unit; the physical value is the table-driven linear transform
//!
//! ```text
//! value = raw · MUL / DIV + ADD
//! ```
//!
//! read at the row's data type (big-endian). A [`Measurements`] set is built from a
//! parsed SGBD (via [`klartext_sgbd`]) and looked up by the measurement id; the same
//! id keys both a static `0x22` read and the dynamic `0x2C`/`0x22` read once that
//! request path lands (see `docs/sgbd-findings.md`). Anything not understood — an
//! unhandled data type, a too-short response, an unparsable row — degrades to raw
//! rather than erroring, matching the M3/M5 contract.

use std::collections::HashMap;
use std::path::Path;

use klartext_sgbd::{Prg, SgbdError, Table};
use klartext_uds::{
    clear_dynamic_data_identifier, define_dynamic_data_by_identifier, read_data_by_identifier,
};

/// The raw data type of a measurement, as named in `SG_FUNKTIONEN.DATENTYP`.
///
/// Only the types actually present in the data are modeled; an unrecognized
/// `DATENTYP` yields `None` from [`DataType::from_datentyp`] so the caller degrades
/// to the raw value. All numeric reads are big-endian (Motorola byte order).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// `unsigned char` — one unsigned byte.
    U8,
    /// `unsigned int` — two-byte unsigned, big-endian.
    U16,
    /// `unsigned long` — four-byte unsigned, big-endian.
    U32,
    /// `motorola float` — four-byte IEEE-754 single, big-endian.
    F32Be,
}

impl DataType {
    /// Map an `SG_FUNKTIONEN.DATENTYP` string to a [`DataType`], if recognized.
    pub fn from_datentyp(datentyp: &str) -> Option<Self> {
        match datentyp.trim() {
            "unsigned char" => Some(Self::U8),
            "unsigned int" => Some(Self::U16),
            "unsigned long" => Some(Self::U32),
            "motorola float" => Some(Self::F32Be),
            _ => None,
        }
    }

    /// The number of raw bytes this data type reads.
    pub fn width(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::U16 => 2,
            Self::U32 | Self::F32Be => 4,
        }
    }

    /// Read this type's value from the big-endian prefix of `raw`, as an `f64`.
    ///
    /// Returns `None` when `raw` is shorter than [`DataType::width`].
    fn read_be(self, raw: &[u8]) -> Option<f64> {
        let b = raw.get(..self.width())?;
        let value = match self {
            Self::U8 => f64::from(b[0]),
            Self::U16 => f64::from(u16::from_be_bytes([b[0], b[1]])),
            Self::U32 => f64::from(u32::from_be_bytes([b[0], b[1], b[2], b[3]])),
            Self::F32Be => f64::from(f32::from_be_bytes([b[0], b[1], b[2], b[3]])),
        };
        Some(value)
    }
}

/// Case-fold `s` for measurement-name matching: trim, Unicode-lowercase, ß → ss.
///
/// The SGBD catalog is German, so folding must handle more than ASCII: an LLM
/// client lowercases "Öltemperatur" and Rust's `to_lowercase` does not fold ß to
/// "ss" (while `to_uppercase("ß") == "SS"` — the round trip breaks without the
/// explicit substitution). Both the name lookup ([`Measurements::find_by_name`])
/// and the MCP search filter fold with this same function so discover→read never
/// disagrees on what "case-insensitive" means.
pub fn fold_for_match(s: &str) -> String {
    s.trim().to_lowercase().replace('ß', "ss")
}

/// Scale raw response bytes to a physical value: `raw · mul / div + add`.
///
/// `raw` is read big-endian at `dtype`'s width. Returns `None` — degrade to raw —
/// when `raw` is shorter than that width or `div` is zero; it never errors or panics.
pub fn scale(raw: &[u8], dtype: DataType, mul: f64, div: f64, add: f64) -> Option<f64> {
    if div == 0.0 {
        return None;
    }
    let value = dtype.read_be(raw)?;
    Some(value * mul / div + add)
}

/// A proprietary measurement scaled to a physical value: name, value, and unit.
///
/// The owned analogue of [`crate::pid::ScaledPid`] for SGBD-sourced measurements,
/// whose names and units are runtime strings from the user's `.prg`, not `&'static`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScaledMeasurement {
    /// Human name of the signal (the description, e.g. `"Motortemperatur"`).
    pub name: String,
    /// The scaled engineering value (consumers round for display).
    pub value: f64,
    /// The engineering unit, e.g. `"degC"`, `"bar"`, `"rpm"`.
    pub unit: String,
}

/// One `SG_FUNKTIONEN` measurement: how to read and scale a proprietary value.
///
/// The scaling fields (`datatype`, `mul`, `div`, `add`, `unit`) drive [`scale`];
/// the routing fields (`sg_adr`, `service`, `arg`) describe how to *request* the
/// value and are consumed by the capture-gated request builder, not by scaling.
#[derive(Debug, Clone, PartialEq)]
pub struct Measurement {
    /// The job argument / short id, e.g. `"ITMOT"`.
    pub arg: String,
    /// The internal data identifier used to read the value, e.g. `0x4BC3`.
    pub id: u16,
    /// The EDIABAS result name, e.g. `"STAT_MOTORTEMPERATUR_WERT"`.
    pub result_name: String,
    /// The human description, e.g. `"Motortemperatur"` (may be empty).
    pub description: String,
    /// The engineering unit, e.g. `"degC"` (may be empty).
    pub unit: String,
    /// The raw data type read from the response.
    pub datatype: DataType,
    /// The multiplier in `raw · mul / div + add`.
    pub mul: f64,
    /// The divisor in `raw · mul / div + add` (1.0 when the table leaves it blank).
    pub div: f64,
    /// The offset in `raw · mul / div + add`.
    pub add: f64,
    /// The ECU diagnostic address as written in the table (e.g. `"12"`); routing.
    pub sg_adr: String,
    /// The UDS service(s) used to read it (e.g. `"22;2C"`); routing.
    pub service: String,
}

impl Measurement {
    /// The human name of the signal: the description, else the EDIABAS result name.
    pub fn name(&self) -> &str {
        if self.description.is_empty() || self.description == "-" {
            &self.result_name
        } else {
            &self.description
        }
    }

    /// Scale `raw` to a [`ScaledMeasurement`], or `None` to degrade to raw.
    ///
    /// `None` means the response is too short for the data type (it never errors).
    /// The name is [`Measurement::name`].
    pub fn scaled(&self, raw: &[u8]) -> Option<ScaledMeasurement> {
        let value = scale(raw, self.datatype, self.mul, self.div, self.add)?;
        Some(ScaledMeasurement {
            name: self.name().to_string(),
            value,
            unit: self.unit.clone(),
        })
    }

    /// Whether this measurement is read via the dynamic `0x2C` define sequence.
    ///
    /// True when the SGBD `SERVICE` column lists `2C` (e.g. `"22;2C"`): such a
    /// measurement is read with [`build_read_request`]. A plain `22` service is
    /// read directly with `0x22 <id>`.
    pub fn is_dynamic(&self) -> bool {
        self.service.split(';').any(|s| s.trim() == "2C")
    }
}

/// The proprietary measurements of one ECU, indexed by internal id.
///
/// Built from an SGBD's `SG_FUNKTIONEN` table; rows whose id, data type, or factors
/// do not parse are dropped, so the set holds only measurements that can be scaled.
/// An ECU with no `SG_FUNKTIONEN` (e.g. an inline-scaling supplier SGBD) yields an
/// empty set, and every lookup degrades to raw.
#[derive(Debug, Clone, Default)]
pub struct Measurements {
    by_id: HashMap<u16, Measurement>,
}

impl Measurements {
    /// Build the measurement set from a parsed `SG_FUNKTIONEN` [`Table`].
    pub fn from_table(table: &Table) -> Self {
        let Some(idx) = ColumnIndex::resolve(&table.columns) else {
            return Self::default();
        };
        let mut by_id = HashMap::new();
        for row in &table.rows {
            if let Some(measurement) = parse_row(&idx, row) {
                by_id.entry(measurement.id).or_insert(measurement);
            }
        }
        Self { by_id }
    }

    /// Build from an SGBD, or `None` when it has no `SG_FUNKTIONEN` table.
    pub fn from_prg(prg: &Prg) -> Option<Self> {
        prg.table("SG_FUNKTIONEN").map(Self::from_table)
    }

    /// Load the measurement set from an SGBD `.prg` file at `path`.
    ///
    /// # Errors
    /// Returns [`SgbdError`] if the file cannot be read or parsed. A file that
    /// parses but carries no `SG_FUNKTIONEN` yields an empty set (not an error), so
    /// every lookup then degrades to raw.
    pub fn from_sgbd(path: impl AsRef<Path>) -> Result<Self, SgbdError> {
        let prg = Prg::open(path)?;
        Ok(Self::from_prg(&prg).unwrap_or_default())
    }

    /// The measurement with internal id `id`, if known.
    pub fn get(&self, id: u16) -> Option<&Measurement> {
        self.by_id.get(&id)
    }

    /// Scale the measurement with id `id` from `raw`, or `None` to degrade to raw.
    pub fn scale(&self, id: u16, raw: &[u8]) -> Option<ScaledMeasurement> {
        self.get(id)?.scaled(raw)
    }

    /// Every measurement in the set, sorted by internal id.
    ///
    /// The stable order makes catalog listings (the MCP `list_measurements` tool)
    /// deterministic across runs despite the map-backed storage.
    pub fn all(&self) -> Vec<&Measurement> {
        let mut all: Vec<&Measurement> = self.by_id.values().collect();
        all.sort_by_key(|m| m.id);
        all
    }

    /// Find measurements named `name` (trimmed, case-folded, exact).
    ///
    /// A measurement matches when `name` equals its short job argument (`ITMOT`),
    /// its EDIABAS result name (`STAT_MOTORTEMPERATUR_WERT`), or its human
    /// description (`Motortemperatur`) under [`fold_for_match`]; the `-`
    /// placeholder and empty fields never match. All fields rank equally: names
    /// are not unique in real data (descriptions repeat, and one row's arg can be
    /// another row's description), and preferring one field would silently read
    /// the wrong sensor — so every matching measurement is returned, sorted by
    /// id, and disambiguation is the caller's call. An empty `name` matches
    /// nothing.
    pub fn find_by_name(&self, name: &str) -> Vec<&Measurement> {
        let query = fold_for_match(name);
        if query.is_empty() {
            return Vec::new();
        }
        let matched = |field: &str| {
            let field = field.trim();
            field != "-" && !field.is_empty() && fold_for_match(field) == query
        };
        let mut matches: Vec<&Measurement> = self
            .by_id
            .values()
            .filter(|m| matched(&m.arg) || matched(&m.result_name) || matched(&m.description))
            .collect();
        matches.sort_by_key(|m| m.id);
        matches
    }

    /// The number of scalable measurements in the set.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Whether the set has no scalable measurements.
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }
}

/// Resolved column positions for the `SG_FUNKTIONEN` fields this crate reads.
struct ColumnIndex {
    arg: usize,
    id: usize,
    result_name: usize,
    info: usize,
    unit: usize,
    datentyp: usize,
    mul: usize,
    div: usize,
    add: usize,
    sg_adr: usize,
    service: usize,
}

impl ColumnIndex {
    /// Resolve column positions by header name, or `None` if a needed one is absent.
    fn resolve(columns: &[String]) -> Option<Self> {
        let at = |name: &str| columns.iter().position(|c| c == name);
        Some(Self {
            arg: at("ARG")?,
            id: at("ID")?,
            result_name: at("RESULTNAME")?,
            info: at("INFO")?,
            unit: at("EINHEIT")?,
            datentyp: at("DATENTYP")?,
            mul: at("MUL")?,
            div: at("DIV")?,
            add: at("ADD")?,
            sg_adr: at("SG_ADR")?,
            service: at("SERVICE")?,
        })
    }
}

/// Parse one `SG_FUNKTIONEN` row into a [`Measurement`], or `None` to skip it.
///
/// A row is skipped when its id, data type, or a numeric factor does not parse —
/// the measurement then simply degrades to raw, never erroring the whole table.
fn parse_row(idx: &ColumnIndex, row: &[String]) -> Option<Measurement> {
    let cell = |i: usize| row.get(i).map(String::as_str);
    let id = parse_id(cell(idx.id)?)?;
    let datatype = DataType::from_datentyp(cell(idx.datentyp)?)?;
    let mul = parse_factor(cell(idx.mul)?, 1.0)?;
    let div = parse_factor(cell(idx.div)?, 1.0)?;
    let add = parse_factor(cell(idx.add)?, 0.0)?;
    Some(Measurement {
        arg: cell(idx.arg).unwrap_or_default().to_string(),
        id,
        result_name: cell(idx.result_name).unwrap_or_default().to_string(),
        description: cell(idx.info).unwrap_or_default().to_string(),
        unit: cell(idx.unit).unwrap_or_default().to_string(),
        datatype,
        mul,
        div,
        add,
        sg_adr: cell(idx.sg_adr).unwrap_or_default().to_string(),
        service: cell(idx.service).unwrap_or_default().to_string(),
    })
}

/// Parse a hex measurement id like `0x4BC3` (with or without the `0x` prefix).
pub(crate) fn parse_id(s: &str) -> Option<u16> {
    let t = s.trim();
    let hex = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u16::from_str_radix(hex, 16).ok()
}

/// Parse a factor cell: a blank (`-` or empty) yields `default`, else the number.
pub(crate) fn parse_factor(s: &str, default: f64) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() || t == "-" {
        return Some(default);
    }
    t.parse::<f64>().ok()
}

// ---------------------------------------------------------------------------
// Part B: the proprietary-measurement REQUEST builder.
//
// Reading a `SERVICE = "22;2C"` measurement off the DDE is not a single static
// `0x22 <id>` read: it is the EDIABAS "selektiv lesen" sequence — clear a dynamic
// DID, define it from the measurement's internal id, then read it. The frames
// below are DERIVED from the `d72n47a0` `STATUS_MOTORTEMPERATUR` disassembly
// (`docs/sgbd-findings.md` §7a), NOT from a packet capture; the real on-car
// response bytes are the manual hardware-in-the-loop confirmation step. Static-DID
// measurements ([`Measurement::is_dynamic`] is false) keep the plain `0x22 <id>`
// read and never reach this builder.
// ---------------------------------------------------------------------------

/// The BMW DDE dynamic data identifier for "selektiv lesen" (`0xF303`).
///
/// [`build_read_request`] defines this dynamic DID from a measurement's internal
/// id (UDS `0x2C`), then reads it (`0x22`). Observed as a literal in the
/// `d72n47a0` disassembly; [verify against capture].
pub const DYNAMIC_DID: u16 = 0xF303;

/// 1-based start position of the source data in the `0x2C` defineByIdentifier
/// request — a literal `0x01` in the disassembled job.
const SOURCE_POSITION: u8 = 0x01;

/// Build the UDS request sequence that reads a dynamic (`SERVICE = "22;2C"`) measurement.
///
/// Returns the ordered UDS request payloads a session driver sends in turn:
/// 1. clear dynamic DID `0xF303` — `2C 03 F3 03`,
/// 2. define `0xF303` from the measurement's internal id as a source DID, position
///    1, size = the data type's byte width — `2C 01 F3 03 <id> 01 <width>`,
/// 3. read `0xF303` — `22 F3 03`, whose `62 F3 03 <raw>` response feeds
///    [`Measurement::scaled`].
///
/// The sequence is DERIVED from the `d72n47a0` `STATUS_MOTORTEMPERATUR`
/// disassembly (`docs/sgbd-findings.md` §7a), not a capture — pending on-car
/// confirmation. Call this only for a [`Measurement::is_dynamic`] measurement; a
/// static one is read directly with `0x22 <id>`.
pub fn build_read_request(measurement: &Measurement) -> Vec<Vec<u8>> {
    // The define mirrors `size` bytes of the source id; `size` is the data type's
    // byte width (1/2/4), which always fits one UDS size byte.
    let size = measurement.datatype.width() as u8;
    vec![
        clear_dynamic_data_identifier(DYNAMIC_DID).to_vec(),
        define_dynamic_data_by_identifier(DYNAMIC_DID, measurement.id, SOURCE_POSITION, size)
            .to_vec(),
        read_data_by_identifier(DYNAMIC_DID).to_vec(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {expected}, got {actual}"
        );
    }

    #[test]
    fn motor_temperature_scales_decikelvin_to_celsius() {
        // STATUS_MOTORTEMPERATUR (d72n47a0): u16 · 0.1 + (−273.14), unit degC.
        // Raw 0x0E2F = 3631 → 363.1 − 273.14 = 89.96 °C (warm idle), per findings.
        let v = scale(&[0x0E, 0x2F], DataType::U16, 0.1, 1.0, -273.14).expect("scales");
        approx(v, 89.96);
    }

    #[test]
    fn one_byte_offset_signal_scales() {
        // A u8 with a pure offset (e.g. an ECU temperature in °C − 40).
        approx(scale(&[0x7B], DataType::U8, 1.0, 1.0, -40.0).unwrap(), 83.0);
    }

    #[test]
    fn divisor_is_applied() {
        // raw 100, div 100 → 1.00 (e.g. a percentage in centi-units).
        approx(
            scale(&[0x00, 0x64], DataType::U16, 1.0, 100.0, 0.0).unwrap(),
            1.0,
        );
    }

    #[test]
    fn unsigned_long_scales() {
        approx(
            scale(&[0x00, 0x01, 0x86, 0xA0], DataType::U32, 1.0, 1.0, 0.0).unwrap(),
            100_000.0,
        );
    }

    #[test]
    fn motorola_float_reads_big_endian_ieee754() {
        // 0x42C80000 = 100.0f32, big-endian.
        approx(
            scale(&[0x42, 0xC8, 0x00, 0x00], DataType::F32Be, 1.0, 1.0, 0.0).unwrap(),
            100.0,
        );
    }

    #[test]
    fn too_short_raw_degrades_to_none() {
        // u16 needs two bytes; one byte cannot scale — degrade to raw.
        assert_eq!(scale(&[0x0E], DataType::U16, 0.1, 1.0, -273.14), None);
        assert_eq!(scale(&[], DataType::U8, 1.0, 1.0, 0.0), None);
    }

    #[test]
    fn zero_divisor_degrades_to_none() {
        assert_eq!(scale(&[0x00, 0x64], DataType::U16, 1.0, 0.0, 0.0), None);
    }

    #[test]
    fn datentyp_maps_known_types_and_rejects_others() {
        assert_eq!(DataType::from_datentyp("unsigned char"), Some(DataType::U8));
        assert_eq!(DataType::from_datentyp("unsigned int"), Some(DataType::U16));
        assert_eq!(
            DataType::from_datentyp("unsigned long"),
            Some(DataType::U32)
        );
        assert_eq!(
            DataType::from_datentyp("motorola float"),
            Some(DataType::F32Be)
        );
        // Not yet handled (e.g. signed types, intel float) → degrade to raw.
        assert_eq!(DataType::from_datentyp("signed int"), None);
        assert_eq!(DataType::from_datentyp(""), None);
    }

    /// A `SG_FUNKTIONEN` table (the 16-column shape) over the given rows.
    fn sg_funktionen(rows: Vec<Vec<&str>>) -> Table {
        let columns = [
            "ARG",
            "ID",
            "RESULTNAME",
            "INFO",
            "EINHEIT",
            "LABEL",
            "L/H",
            "DATENTYP",
            "NAME",
            "MUL",
            "DIV",
            "ADD",
            "SG_ADR",
            "SERVICE",
            "ARG_TABELLE",
            "RES_TABELLE",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        let rows = rows
            .into_iter()
            .map(|r| r.into_iter().map(str::to_string).collect())
            .collect();
        Table {
            name: "SG_FUNKTIONEN".to_string(),
            columns,
            rows,
        }
    }

    /// The engine-temperature row from the F20 DDE (no BMW data: it is the formula).
    fn motor_temp() -> Vec<&'static str> {
        vec![
            "ITMOT",
            "0x4BC3",
            "STAT_MOTORTEMPERATUR_WERT",
            "Motortemperatur",
            "degC",
            "EngDa_tEng",
            "-",
            "unsigned int",
            "-",
            "0.100000",
            "-",
            "-273.140000",
            "12",
            "22;2C",
            "-",
            "-",
        ]
    }

    #[test]
    fn measurements_scale_proprietary_value_by_id() {
        let m = Measurements::from_table(&sg_funktionen(vec![motor_temp()]));
        let s = m.scale(0x4BC3, &[0x0E, 0x2F]).expect("scales");
        assert_eq!(s.name, "Motortemperatur");
        assert_eq!(s.unit, "degC");
        approx(s.value, 89.96);
    }

    #[test]
    fn unknown_id_degrades_to_none() {
        let m = Measurements::from_table(&sg_funktionen(vec![motor_temp()]));
        assert!(m.scale(0x9999, &[0x00, 0x00]).is_none());
    }

    #[test]
    fn rows_with_unhandled_datentyp_are_skipped() {
        let mut row = motor_temp();
        row[7] = "signed int"; // not yet handled → row dropped → degrade to raw
        let m = Measurements::from_table(&sg_funktionen(vec![row]));
        assert!(m.is_empty());
        assert!(m.scale(0x4BC3, &[0x0E, 0x2F]).is_none());
    }

    #[test]
    fn name_falls_back_to_result_name_when_description_blank() {
        let mut row = motor_temp();
        row[3] = "-"; // INFO blank
        let m = Measurements::from_table(&sg_funktionen(vec![row]));
        let s = m.scale(0x4BC3, &[0x0E, 0x2F]).unwrap();
        assert_eq!(s.name, "STAT_MOTORTEMPERATUR_WERT");
    }

    #[test]
    fn from_sgbd_missing_file_errors() {
        assert!(Measurements::from_sgbd("/nonexistent/none.prg").is_err());
    }

    /// A minimal `SG_FUNKTIONEN` row (u16 in degC) named by `arg`/`id`/`result`/`info`.
    fn named_row(
        arg: &'static str,
        id: &'static str,
        result: &'static str,
        info: &'static str,
    ) -> Vec<&'static str> {
        vec![
            arg,
            id,
            result,
            info,
            "degC",
            "-",
            "-",
            "unsigned int",
            "-",
            "0.100000",
            "-",
            "-273.140000",
            "12",
            "22;2C",
            "-",
            "-",
        ]
    }

    #[test]
    fn all_returns_measurements_sorted_by_id() {
        let m = Measurements::from_table(&sg_funktionen(vec![
            motor_temp(), // 0x4BC3
            named_row(
                "ITKUM",
                "0x461B",
                "STAT_KUEHLMITTELTEMPERATUR_WERT",
                "Kühlmitteltemperatur",
            ),
        ]));
        let ids: Vec<u16> = m.all().iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![0x461B, 0x4BC3]);
    }

    #[test]
    fn find_by_name_matches_the_short_arg_case_insensitively() {
        let m = Measurements::from_table(&sg_funktionen(vec![motor_temp()]));
        let found = m.find_by_name("itmot");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, 0x4BC3);
    }

    #[test]
    fn find_by_name_matches_the_result_name() {
        let m = Measurements::from_table(&sg_funktionen(vec![motor_temp()]));
        let found = m.find_by_name("stat_motortemperatur_wert");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, 0x4BC3);
    }

    #[test]
    fn find_by_name_matches_the_description() {
        let m = Measurements::from_table(&sg_funktionen(vec![motor_temp()]));
        let found = m.find_by_name("Motortemperatur");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, 0x4BC3);
    }

    #[test]
    fn find_by_name_folds_german_case() {
        // The catalog is German; folding must be Unicode, not ASCII-only — an LLM
        // client routinely lowercases a name it was just shown ("Öltemperatur").
        let m = Measurements::from_table(&sg_funktionen(vec![
            named_row("TOEL", "0x4517", "STAT_OELTEMPERATUR_WERT", "Öltemperatur"),
            named_row("IMRUP", "0x44BE", "STAT_RUSSMASSE_WERT", "Rußmasse"),
        ]));
        assert_eq!(m.find_by_name("öltemperatur")[0].id, 0x4517);
        assert_eq!(m.find_by_name("ÖLTEMPERATUR")[0].id, 0x4517);
        // ß folds to "ss" both ways: the uppercase round-trip ("RUSSMASSE") and
        // the common ASCII transliteration ("Russmasse") both resolve.
        assert_eq!(m.find_by_name("RUSSMASSE")[0].id, 0x44BE);
        assert_eq!(m.find_by_name("Russmasse")[0].id, 0x44BE);
        assert_eq!(m.find_by_name("rußmasse")[0].id, 0x44BE);
    }

    #[test]
    fn find_by_name_reports_cross_field_collisions_as_ambiguous() {
        // "TOEL" is one row's arg and another row's description. Guessing which one
        // the caller meant risks reading the wrong sensor under a trusted name, so
        // BOTH come back (sorted by id) and the caller disambiguates by id.
        let m = Measurements::from_table(&sg_funktionen(vec![
            named_row("TOEL", "0x4517", "STAT_OELTEMPERATUR_WERT", "Öltemperatur"),
            named_row("XYZ", "0x1234", "STAT_XYZ_WERT", "TOEL"),
        ]));
        let ids: Vec<u16> = m.find_by_name("toel").iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![0x1234, 0x4517]);
    }

    #[test]
    fn find_by_name_returns_every_match_sorted_by_id() {
        // Real DDE data has duplicate descriptions (e.g. "Statuswort" ×4).
        let m = Measurements::from_table(&sg_funktionen(vec![
            named_row("B_a", "0x1000", "STAT_A_WERT", "Statuswort"),
            named_row("B_b", "0x0999", "STAT_B_WERT", "Statuswort"),
        ]));
        let ids: Vec<u16> = m.find_by_name("statuswort").iter().map(|x| x.id).collect();
        assert_eq!(ids, vec![0x0999, 0x1000]);
    }

    #[test]
    fn find_by_name_matches_one_row_only_once_across_its_own_fields() {
        // A query hitting the same row's arg AND description is one match, not two.
        let m = Measurements::from_table(&sg_funktionen(vec![named_row(
            "Nkw",
            "0x427F",
            "STAT_Nkw_WERT",
            "Nkw",
        )]));
        let found = m.find_by_name("nkw");
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].id, 0x427F);
    }

    #[test]
    fn find_by_name_matches_nothing_for_unknown_empty_or_placeholder_names() {
        // The real DDE has rows with an empty ARG and "-" descriptions; neither an
        // empty query nor the "-" placeholder may resolve as a name.
        let mut placeholder_info = motor_temp();
        placeholder_info[3] = "-"; // INFO is the SGBD null marker
        let m = Measurements::from_table(&sg_funktionen(vec![
            placeholder_info,
            named_row("", "0x2000", "STAT_NO_ARG_WERT", "Ohne Argument"),
        ]));
        assert!(m.find_by_name("does-not-exist").is_empty());
        assert!(m.find_by_name("").is_empty());
        assert!(m.find_by_name("   ").is_empty());
        assert!(m.find_by_name("-").is_empty());
    }

    #[test]
    fn is_dynamic_detects_the_2c_service() {
        let m = Measurements::from_table(&sg_funktionen(vec![motor_temp()]));
        // SERVICE = "22;2C": the value is read via the 0x2C define + 0x22 read.
        assert!(m.get(0x4BC3).unwrap().is_dynamic());
    }

    #[test]
    fn is_dynamic_is_false_for_a_static_did_service() {
        let mut row = motor_temp();
        row[13] = "22"; // SERVICE: a plain static ReadDataByIdentifier
        let m = Measurements::from_table(&sg_funktionen(vec![row]));
        assert!(!m.get(0x4BC3).unwrap().is_dynamic());
    }

    #[test]
    fn build_read_request_emits_the_derived_dde_sequence() {
        let m = Measurements::from_table(&sg_funktionen(vec![motor_temp()]));
        let measurement = m.get(0x4BC3).expect("measurement parsed");
        // Replay fixture DERIVED from the d72n47a0 STATUS_MOTORTEMPERATUR
        // disassembly (docs/sgbd-findings.md §7a): clear F303, define F303 from
        // source DID 0x4BC3 (position 1, size 2 = u16 width), read F303.
        assert_eq!(
            build_read_request(measurement),
            vec![
                vec![0x2C, 0x03, 0xF3, 0x03],
                vec![0x2C, 0x01, 0xF3, 0x03, 0x4B, 0xC3, 0x01, 0x02],
                vec![0x22, 0xF3, 0x03],
            ]
        );
    }

    #[test]
    fn build_read_request_sizes_the_define_by_data_type() {
        // The define's size byte is the measurement's width: u32 -> 4.
        let mut row = motor_temp();
        row[7] = "unsigned long"; // DATENTYP -> u32 (width 4)
        let m = Measurements::from_table(&sg_funktionen(vec![row]));
        let define = build_read_request(m.get(0x4BC3).unwrap())[1].clone();
        assert_eq!(define, vec![0x2C, 0x01, 0xF3, 0x03, 0x4B, 0xC3, 0x01, 0x04]);
    }

    // End-to-end on the real DDE SGBD: load the `.prg`, scale a measurement.
    // Ignored by default (BYO data); the constants are the public formula.
    #[test]
    #[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
    fn real_dde_scales_motor_temperature() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/Testmodule(1)/Ecu/d72n47a0.prg");
        let m = Measurements::from_sgbd(&path).expect("load real SGBD");
        let s = m.scale(0x4BC3, &[0x0E, 0x2F]).expect("scales");
        assert_eq!(s.name, "Motortemperatur");
        assert_eq!(s.unit, "degC");
        assert!((s.value - 89.96).abs() < 0.01, "got {}", s.value);
    }
}
