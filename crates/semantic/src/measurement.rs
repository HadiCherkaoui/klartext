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
    /// Scale `raw` to a [`ScaledMeasurement`], or `None` to degrade to raw.
    ///
    /// `None` means the response is too short for the data type (it never errors).
    /// The name is the description when present, else the EDIABAS result name.
    pub fn scaled(&self, raw: &[u8]) -> Option<ScaledMeasurement> {
        let value = scale(raw, self.datatype, self.mul, self.div, self.add)?;
        let name = if self.description.is_empty() || self.description == "-" {
            self.result_name.clone()
        } else {
            self.description.clone()
        };
        Some(ScaledMeasurement {
            name,
            value,
            unit: self.unit.clone(),
        })
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
fn parse_id(s: &str) -> Option<u16> {
    let t = s.trim();
    let hex = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u16::from_str_radix(hex, 16).ok()
}

/// Parse a factor cell: a blank (`-` or empty) yields `default`, else the number.
fn parse_factor(s: &str, default: f64) -> Option<f64> {
    let t = s.trim();
    if t.is_empty() || t == "-" {
        return Some(default);
    }
    t.parse::<f64>().ok()
}

// ---------------------------------------------------------------------------
// Part B (capture-gated): the proprietary-measurement REQUEST builder.
//
// Scaling above is offline-verifiable and built. Reading the value off the car is
// not a single static `0x22 <id>` read for a `SERVICE = "22;2C"` measurement: the
// DDE wants a UDS `0x2C` DynamicallyDefineDataIdentifier sequence (observed in the
// disassembly: define a dynamic DID `0xF303` from the measurement's internal id,
// then `0x22 F3 03` to read it). The exact framing must be confirmed against a real
// capture before it is built — guessing it would break the hardware-in-the-loop
// rule. This is stubbed with a clear boundary; see `docs/sgbd-findings.md` Part B.
// ---------------------------------------------------------------------------

/// Marker error: the proprietary-measurement request builder awaits a byte-trace.
#[derive(Debug, thiserror::Error)]
#[error(
    "proprietary-measurement request builder pending a real 2C/22 byte-trace \
     (see docs/sgbd-findings.md Part B; drop the capture in captures/)"
)]
pub struct RequestBuilderPending;

/// Build the UDS request sequence to read a proprietary `measurement` — STUBBED.
///
/// This is the capture-gated half of M6. Once built it will return the ordered UDS
/// request payloads — the `0x2C` DynamicallyDefineDataIdentifier define (and any
/// leading clear) then the `0x22` read of the dynamic DID — that a session driver
/// sends, after which [`Measurement::scaled`] turns the response into a value. It is
/// intentionally **not implemented**: the `0x2C` framing must be read off a real
/// exchange, not assumed.
///
/// # Errors
/// Always returns [`RequestBuilderPending`] until the byte-trace lands. For one DDE
/// measurement (e.g. `STAT_MOTORTEMPERATUR_WERT`, id `0x4BC3`) the trace must pin:
/// 1. the full `0x2C` define request frame(s): the subfunction byte (`0x01`
///    defineByIdentifier vs `0x02` defineByMemoryAddress, and any leading `0x03`
///    clear), the dynamic DID defined (observed `0xF303`), and exactly how the
///    internal id `0x4BC3` is encoded into the define (a source-DID list, or an
///    addressAndLengthFormatIdentifier + address + size);
/// 2. the `0x22 F3 03` read request and its `0x62 F3 03 …` positive response, so the
///    raw measurement bytes' offset and width are fixed;
/// 3. any session/security precondition (e.g. a preceding `0x10 0x03` extended
///    session) and whether a trailing clear is sent.
///
/// Drop the capture in `captures/` (gitignored); it then becomes an offline replay
/// fixture (cf. an EdiabasLib `.sim`) so the end-to-end read is tested without a car.
pub fn build_read_request(
    measurement: &Measurement,
) -> Result<Vec<Vec<u8>>, RequestBuilderPending> {
    let _ = measurement;
    Err(RequestBuilderPending)
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

    #[test]
    fn proprietary_request_builder_is_gated_pending_a_trace() {
        let m = Measurements::from_table(&sg_funktionen(vec![motor_temp()]));
        let measurement = m.get(0x4BC3).expect("measurement parsed");
        // Part B: the request sequence is not built from assumption (see module).
        assert!(build_read_request(measurement).is_err());
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
