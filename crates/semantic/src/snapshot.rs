//! Freeze-frame / snapshot decoding for fault codes (UDS `19 04` / `19 06`).
//!
//! A fault's freeze-frame is the set of environmental conditions the ECU latched
//! when the fault occurred — mileage, timestamp, RPM, temperatures, ECU state. The
//! wire response ([`klartext_uds::DtcRecordRegion`]) carries a sequence of
//! 2-byte-identifier-keyed records with **no per-field length on the wire**: the
//! width, scaling, and unit come from the ECU's SGBD tables. This module reads
//! those tables and walks a record region into labeled, scaled fields.
//!
//! Three SGBD tables drive the decode (all in the DDE `d72n47a0.prg`, same
//! `.prg`-container format [`klartext_sgbd`] already parses for `SG_FUNKTIONEN`):
//!
//! - `FUMWELTTEXTE` — the measurement env-conditions (id, unit, data type,
//!   `MUL`/`DIV`/`ADD`), same 9-column schema as `SG_FUNKTIONEN`'s scaling subset.
//! - `DTCSNAPSHOTIDENTIFIER` — the standard identifiers (mileage, timestamp, SAE
//!   code), whose `UWTYP` is a hex width mask (`0xFFFFFF` = 3 bytes) that doubles as
//!   the "not available" sentinel.
//! - `DTCEXTENDEDDATARECORDNUMBER` — record-number → byte length for `19 06`.
//!
//! Optional English labels overlay from the ISTA DB ([`crate::Catalog`]); without
//! it, the German SGBD text is used, and without the SGBD the region cannot be
//! sized and stays raw. Every unhandled case degrades to raw — it never errors.
//!
//! **The record framing after the DTC + status byte is DERIVED from ISO 14229-1
//! §11.3 and the DDE disassembly; no `0x19` capture exists yet — [verify against
//! capture].** The walk defends against framing drift by stopping at the first
//! identifier it cannot size and surfacing the remainder as an undecoded tail.

use std::path::Path;

use klartext_sgbd::{Prg, SgbdError, Table};
use klartext_uds::DtcRecordRegion;

use crate::catalog::Catalog;
use crate::measurement::{parse_factor, parse_id};

/// SGBD table names (uppercase, as the `.prg` directory stores them).
const T_FUMWELTTEXTE: &str = "FUMWELTTEXTE";
const T_DTC_SNAPSHOT_IDS: &str = "DTCSNAPSHOTIDENTIFIER";
const T_DTC_EXT_DATA: &str = "DTCEXTENDEDDATARECORDNUMBER";

/// How a snapshot field's raw bytes are interpreted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Repr {
    /// A big-endian unsigned integer of the field's width.
    Unsigned,
    /// A 4-byte big-endian IEEE-754 float (`motorola float`).
    Float,
}

/// One environmental-condition definition: how to size, scale, and name a field.
///
/// Built from a `FUMWELTTEXTE` / `DTCSNAPSHOTIDENTIFIER` row. `sentinel` is the
/// all-ones value of a mask-typed standard identifier (e.g. `0xFFFFFF` for mileage),
/// meaning "not available"; it is `None` for ordinary typed measurements.
#[derive(Debug, Clone, PartialEq)]
struct EnvCondDef {
    uwnr: u16,
    text_de: String,
    unit_sgbd: String,
    width: usize,
    repr: Repr,
    mul: f64,
    div: f64,
    add: f64,
    sentinel: Option<u64>,
}

impl EnvCondDef {
    /// Scale `raw` (its own `width` prefix) to a value and its availability.
    ///
    /// Returns `(value, available)`. `available` is false when the raw integer
    /// equals the field's sentinel; `value` is `None` when the bytes are too short
    /// or the divisor is zero (degrade to raw).
    fn decode_value(&self, raw: &[u8]) -> (Option<f64>, bool) {
        let Some(bytes) = raw.get(..self.width) else {
            return (None, true);
        };
        match self.repr {
            Repr::Unsigned => {
                let mut int: u64 = 0;
                for &b in bytes {
                    int = (int << 8) | u64::from(b);
                }
                let available = self.sentinel != Some(int);
                if !available || self.div == 0.0 {
                    return (None, available);
                }
                (Some(int as f64 * self.mul / self.div + self.add), true)
            }
            Repr::Float => {
                if self.width != 4 || self.div == 0.0 {
                    return (None, true);
                }
                let f = f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                (Some(f64::from(f) * self.mul / self.div + self.add), true)
            }
        }
    }
}

/// Resolve an `UWTYP` cell to `(width, repr, sentinel)`, or `None` if unrecognized.
///
/// `UWTYP` is either an EDIABAS type name (`unsigned char`/`u char`, `unsigned
/// int`, `unsigned long`, `motorola float`) or a hex width mask (`0xFF`,
/// `0xFFFFFF`, `0xFFFFFFFF`) whose byte length is the width and whose value is the
/// "not available" sentinel. An unrecognized type returns `None`, so the field
/// cannot be sized and the walk stops there (rather than guess a width).
fn resolve_uwtyp(uwtyp: &str) -> Option<(usize, Repr, Option<u64>)> {
    let t = uwtyp.trim();
    if let Some(hex) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
        if hex.is_empty() || !hex.len().is_multiple_of(2) || hex.len() > 16 {
            return None;
        }
        let width = hex.len() / 2;
        let sentinel = u64::from_str_radix(hex, 16).ok()?;
        return Some((width, Repr::Unsigned, Some(sentinel)));
    }
    let (width, repr) = match t {
        "unsigned char" | "u char" => (1, Repr::Unsigned),
        "unsigned int" | "u int" => (2, Repr::Unsigned),
        "unsigned long" | "u long" => (4, Repr::Unsigned),
        "motorola float" => (4, Repr::Float),
        _ => return None,
    };
    Some((width, repr, None))
}

/// Column positions in a `FUMWELTTEXTE` / `DTCSNAPSHOTIDENTIFIER` table.
struct EnvColumns {
    uwnr: usize,
    uwtext: usize,
    unit: usize,
    uwtyp: usize,
    mul: usize,
    div: usize,
    add: usize,
}

impl EnvColumns {
    /// Resolve the columns by header name, or `None` if a needed one is absent.
    fn resolve(columns: &[String]) -> Option<Self> {
        let at = |name: &str| columns.iter().position(|c| c == name);
        Some(Self {
            uwnr: at("UWNR")?,
            uwtext: at("UWTEXT")?,
            unit: at("UW_EINH")?,
            uwtyp: at("UWTYP")?,
            mul: at("MUL")?,
            div: at("DIV")?,
            add: at("ADD")?,
        })
    }
}

/// Parse one env-condition row into an [`EnvCondDef`], or `None` to skip it.
fn parse_env_row(cols: &EnvColumns, row: &[String]) -> Option<EnvCondDef> {
    let cell = |i: usize| row.get(i).map(String::as_str);
    let uwnr = parse_id(cell(cols.uwnr)?)?;
    let (width, repr, sentinel) = resolve_uwtyp(cell(cols.uwtyp)?)?;
    let mul = parse_factor(cell(cols.mul)?, 1.0)?;
    let div = parse_factor(cell(cols.div)?, 1.0)?;
    let add = parse_factor(cell(cols.add)?, 0.0)?;
    Some(EnvCondDef {
        uwnr,
        text_de: cell(cols.uwtext).unwrap_or_default().to_string(),
        unit_sgbd: cell(cols.unit).unwrap_or_default().to_string(),
        width,
        repr,
        mul,
        div,
        add,
        sentinel,
    })
}

/// The env-condition definitions of one ECU, indexed by identifier (UWNR).
///
/// Built from an SGBD's `FUMWELTTEXTE` + `DTCSNAPSHOTIDENTIFIER` tables. An ECU with
/// neither yields an empty set, and a snapshot region then cannot be sized and stays
/// raw ([`SnapshotDefs::decode`]).
#[derive(Debug, Clone, Default)]
pub struct SnapshotDefs {
    by_id: std::collections::HashMap<u16, EnvCondDef>,
}

impl SnapshotDefs {
    /// Build the definitions from a parsed SGBD.
    ///
    /// Merges `FUMWELTTEXTE` and `DTCSNAPSHOTIDENTIFIER`; the standard identifiers
    /// take precedence on the (non-overlapping) rare collision. Returns an empty set
    /// when neither table is present.
    pub fn from_prg(prg: &Prg) -> Self {
        let mut by_id = std::collections::HashMap::new();
        // FUMWELTTEXTE first, then the standard identifiers override.
        for name in [T_FUMWELTTEXTE, T_DTC_SNAPSHOT_IDS] {
            let Some(table) = find_table(prg, name) else {
                continue;
            };
            let Some(cols) = EnvColumns::resolve(&table.columns) else {
                continue;
            };
            for row in &table.rows {
                if let Some(def) = parse_env_row(&cols, row) {
                    by_id.insert(def.uwnr, def);
                }
            }
        }
        Self { by_id }
    }

    /// Whether no definitions were found (a snapshot region will stay raw).
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// The number of env-condition definitions.
    pub fn len(&self) -> usize {
        self.by_id.len()
    }

    /// Decode a snapshot record region into labeled, scaled fields.
    ///
    /// Walks the region as ISO 14229-1 snapshot records — `[recordNumber]
    /// [numberOfIdentifiers]` then `[UWNR:2][data:width]` per identifier — sizing
    /// each field from its definition and scaling the value. English labels overlay
    /// from `labels` (the ISTA DB) when present, else the German SGBD text is used.
    /// The walk **stops** at the first identifier it cannot size (unknown UWNR or a
    /// short region) and returns the remainder as [`DecodedSnapshot::undecoded_tail`]
    /// — never guessing a width, never erroring.
    ///
    /// The framing is DERIVED — [verify against capture].
    pub fn decode(&self, region: &DtcRecordRegion, labels: Option<&Catalog>) -> DecodedSnapshot {
        let body = &region.body;
        let mut fields = Vec::new();
        let mut pos = 0usize;

        'records: while pos < body.len() {
            // Each record: [recordNumber][numberOfIdentifiers], then the identifiers.
            let Some(&num_ids) = body.get(pos + 1) else {
                break;
            };
            pos += 2;
            for _ in 0..num_ids {
                let Some(id_bytes) = body.get(pos..pos + 2) else {
                    break 'records;
                };
                let uwnr = u16::from_be_bytes([id_bytes[0], id_bytes[1]]);
                let Some(def) = self.by_id.get(&uwnr) else {
                    break 'records; // unknown width — cannot advance safely
                };
                let Some(data) = body.get(pos + 2..pos + 2 + def.width) else {
                    break 'records;
                };
                fields.push(build_field(uwnr, def, data, labels));
                pos += 2 + def.width;
            }
        }

        let undecoded_tail = (pos < body.len()).then(|| body[pos..].to_vec());
        DecodedSnapshot {
            fields,
            undecoded_tail,
        }
    }
}

/// Build one [`SnapshotField`] from a definition, its raw bytes, and optional labels.
fn build_field(
    uwnr: u16,
    def: &EnvCondDef,
    data: &[u8],
    labels: Option<&Catalog>,
) -> SnapshotField {
    let db = labels.and_then(|c| c.envcond_label(uwnr).ok().flatten());
    let label = db
        .as_ref()
        .and_then(|l| l.title_en.clone().or_else(|| l.title_de.clone()))
        .or_else(|| non_placeholder(&def.text_de))
        .unwrap_or_else(|| format!("UW 0x{uwnr:04X}"));
    let unit = db
        .as_ref()
        .and_then(|l| l.unit.as_deref().and_then(clean_unit))
        .or_else(|| clean_unit(&def.unit_sgbd));
    let (value, available) = def.decode_value(data);
    SnapshotField {
        uwnr,
        label,
        value,
        unit,
        available,
        raw: data.to_vec(),
    }
}

/// The result of decoding a `59 04` snapshot region.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedSnapshot {
    /// The decoded fields, in wire order.
    pub fields: Vec<SnapshotField>,
    /// Bytes the walk could not decode (unknown identifier or a short region).
    pub undecoded_tail: Option<Vec<u8>>,
}

/// One decoded freeze-frame field: identifier, label, value, unit, availability.
#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotField {
    /// The 2-byte environmental-condition identifier (UWNR).
    pub uwnr: u16,
    /// The best available human label (DB English → DB German → SGBD text → `UW …`).
    pub label: String,
    /// The scaled value, or `None` when unavailable (sentinel) or degraded to raw.
    pub value: Option<f64>,
    /// The engineering unit, when the field has a real one.
    pub unit: Option<String>,
    /// False when the ECU reported the "not available" sentinel (e.g. no mileage).
    pub available: bool,
    /// The raw field bytes, always present for the caller to render.
    pub raw: Vec<u8>,
}

/// Record-number → byte length for `19 06` extended data, from the SGBD.
///
/// Built from `DTCEXTENDEDDATARECORDNUMBER` (record number `WERT`, name `TEXT`, byte
/// length `ANZ_BYTE`). Empty when the table is absent.
#[derive(Debug, Clone, Default)]
pub struct ExtDataDefs {
    by_record: std::collections::HashMap<u8, (String, usize)>,
}

impl ExtDataDefs {
    /// Build the definitions from a parsed SGBD.
    pub fn from_prg(prg: &Prg) -> Self {
        let mut by_record = std::collections::HashMap::new();
        let Some(table) = find_table(prg, T_DTC_EXT_DATA) else {
            return Self::default();
        };
        let at = |name: &str| table.columns.iter().position(|c| c == name);
        let (Some(wert), Some(text), Some(anz)) = (at("WERT"), at("TEXT"), at("ANZ_BYTE")) else {
            return Self::default();
        };
        for row in &table.rows {
            let cell = |i: usize| row.get(i).map(String::as_str).unwrap_or_default();
            let Some(record) = parse_u8(cell(wert)) else {
                continue;
            };
            let Ok(len) = cell(anz).trim().parse::<usize>() else {
                continue;
            };
            by_record.insert(record, (cell(text).to_string(), len));
        }
        Self { by_record }
    }

    /// Whether no extended-data definitions were found.
    pub fn is_empty(&self) -> bool {
        self.by_record.is_empty()
    }

    /// Decode a `59 06` extended-data region into records.
    ///
    /// Walks `[recordNumber][data:len]` where `len` is the record's `ANZ_BYTE`. Stops
    /// at an unknown record number (unknown length) and returns the remainder as the
    /// undecoded tail. Framing is DERIVED — [verify against capture].
    pub fn decode(&self, region: &DtcRecordRegion) -> DecodedExtData {
        let body = &region.body;
        let mut records = Vec::new();
        let mut pos = 0usize;
        while pos < body.len() {
            let record = body[pos];
            let Some((name, len)) = self.by_record.get(&record) else {
                break;
            };
            let Some(data) = body.get(pos + 1..pos + 1 + len) else {
                break;
            };
            let value = (!data.is_empty())
                .then(|| data.iter().fold(0i64, |acc, &b| (acc << 8) | i64::from(b)));
            records.push(ExtDataField {
                record,
                label: name.clone(),
                value,
                raw: data.to_vec(),
            });
            pos += 1 + len;
        }
        let undecoded_tail = (pos < body.len()).then(|| body[pos..].to_vec());
        DecodedExtData {
            records,
            undecoded_tail,
        }
    }
}

/// The result of decoding a `59 06` extended-data region.
#[derive(Debug, Clone, PartialEq)]
pub struct DecodedExtData {
    /// The decoded records, in wire order.
    pub records: Vec<ExtDataField>,
    /// Bytes the walk could not decode (unknown record number or a short region).
    pub undecoded_tail: Option<Vec<u8>>,
}

/// One decoded extended-data record: number, label, value.
#[derive(Debug, Clone, PartialEq)]
pub struct ExtDataField {
    /// The extended-data record number (e.g. `0x02` = frequency counter).
    pub record: u8,
    /// The record's SGBD name (e.g. `"HFK"`).
    pub label: String,
    /// The record's value as a big-endian integer, or `None` for a zero-length record.
    pub value: Option<i64>,
    /// The raw record bytes.
    pub raw: Vec<u8>,
}

/// Both freeze-frame definition sets for one ECU: snapshot + extended-data.
///
/// Loads `FUMWELTTEXTE` + `DTCSNAPSHOTIDENTIFIER` (snapshot) and
/// `DTCEXTENDEDDATARECORDNUMBER` (extended) from one SGBD in a single parse — the
/// analogue of [`crate::Measurements`] for `19 04`/`19 06` decoding.
#[derive(Debug, Clone, Default)]
pub struct FreezeFrameDefs {
    /// The snapshot (`19 04`) field definitions.
    pub snapshot: SnapshotDefs,
    /// The extended-data (`19 06`) record definitions.
    pub extended: ExtDataDefs,
}

impl FreezeFrameDefs {
    /// Build both definition sets from a parsed SGBD.
    pub fn from_prg(prg: &Prg) -> Self {
        Self {
            snapshot: SnapshotDefs::from_prg(prg),
            extended: ExtDataDefs::from_prg(prg),
        }
    }

    /// Load both definition sets from an SGBD `.prg` file at `path`.
    ///
    /// # Errors
    /// Returns [`SgbdError`] if the file cannot be read or parsed. A file that parses
    /// but carries no freeze-frame tables yields empty sets (not an error), so a
    /// snapshot region then stays raw.
    pub fn from_sgbd(path: impl AsRef<Path>) -> Result<Self, SgbdError> {
        let prg = Prg::open(path)?;
        Ok(Self::from_prg(&prg))
    }

    /// Whether neither snapshot nor extended-data definitions were found.
    pub fn is_empty(&self) -> bool {
        self.snapshot.is_empty() && self.extended.is_empty()
    }
}

/// Find a table by case-insensitive name (EDIABAS table names are case-insensitive).
fn find_table<'a>(prg: &'a Prg, name: &str) -> Option<&'a Table> {
    prg.tables()
        .iter()
        .find(|t| t.name.eq_ignore_ascii_case(name))
}

/// Parse a `0x`-prefixed or bare hex byte (e.g. `"0x02"` → 2).
fn parse_u8(s: &str) -> Option<u8> {
    let t = s.trim();
    let hex = t
        .strip_prefix("0x")
        .or_else(|| t.strip_prefix("0X"))
        .unwrap_or(t);
    u8::from_str_radix(hex, 16).ok()
}

/// A trimmed non-placeholder string, or `None` for `-` / empty.
fn non_placeholder(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty() && t != "-").then(|| t.to_string())
}

/// A real engineering unit, or `None` for placeholders (`-`, `0-n`, empty).
///
/// The SGBD writes `-` or a range hint like `0-n` where there is no unit; neither is
/// a unit to display.
fn clean_unit(s: &str) -> Option<String> {
    let t = s.trim();
    (!t.is_empty() && t != "-" && t != "0-n").then(|| t.to_string())
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

    /// A `FUMWELTTEXTE`/`DTCSNAPSHOTIDENTIFIER`-shaped table (9 columns) over rows.
    fn env_table(name: &str, rows: Vec<Vec<&str>>) -> Table {
        let columns = [
            "UWNR", "UWTEXT", "UW_EINH", "L/H", "UWTYP", "NAME", "MUL", "DIV", "ADD",
        ]
        .iter()
        .map(|s| (*s).to_string())
        .collect();
        Table {
            name: name.to_string(),
            columns,
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(str::to_string).collect())
                .collect(),
        }
    }

    /// Build [`SnapshotDefs`] straight from env-table rows (no `.prg` needed).
    fn defs_from(rows: Vec<Vec<&str>>) -> SnapshotDefs {
        let table = env_table("FUMWELTTEXTE", rows);
        let cols = EnvColumns::resolve(&table.columns).unwrap();
        let by_id = table
            .rows
            .iter()
            .filter_map(|r| parse_env_row(&cols, r).map(|d| (d.uwnr, d)))
            .collect();
        SnapshotDefs { by_id }
    }

    fn region(body: Vec<u8>) -> DtcRecordRegion {
        DtcRecordRegion {
            dtc: [0x24, 0x00, 0x00],
            status: 0x08,
            body,
        }
    }

    // The real DDE rows (no BMW data: these are the public scaling formulas).
    const COOLANT: [&str; 9] = [
        "0x5205",
        "Kühlmitteltemperatur",
        "degC",
        "-",
        "unsigned char",
        "-",
        "1.000000",
        "1",
        "-40.000000",
    ];
    const RPM: [&str; 9] = [
        "0x5955",
        "Motordrehzahl",
        "rpm",
        "-",
        "unsigned int",
        "-",
        "0.500000",
        "1",
        "0.000000",
    ];
    const KM: [&str; 9] = [
        "0x1700", "KM_STAND", "0-n", "-", "0xFFFFFF", "-", "1", "1", "0.000000",
    ];

    #[test]
    fn resolve_uwtyp_handles_type_names_and_masks() {
        assert_eq!(
            resolve_uwtyp("unsigned char"),
            Some((1, Repr::Unsigned, None))
        );
        assert_eq!(resolve_uwtyp("u char"), Some((1, Repr::Unsigned, None)));
        assert_eq!(
            resolve_uwtyp("unsigned int"),
            Some((2, Repr::Unsigned, None))
        );
        assert_eq!(
            resolve_uwtyp("motorola float"),
            Some((4, Repr::Float, None))
        );
        // Masks: byte width = nibble pairs; value = the sentinel.
        assert_eq!(resolve_uwtyp("0xFF"), Some((1, Repr::Unsigned, Some(0xFF))));
        assert_eq!(
            resolve_uwtyp("0xFFFFFF"),
            Some((3, Repr::Unsigned, Some(0xFF_FFFF)))
        );
        assert_eq!(
            resolve_uwtyp("0xFFFFFFFF"),
            Some((4, Repr::Unsigned, Some(0xFFFF_FFFF)))
        );
        // Unrecognized → None (field can't be sized).
        assert_eq!(resolve_uwtyp("signed int"), None);
        assert_eq!(resolve_uwtyp("0xF"), None); // odd nibble count
    }

    #[test]
    fn decode_snapshot_scales_coolant_and_rpm() {
        let defs = defs_from(vec![COOLANT.to_vec(), RPM.to_vec()]);
        // One record, 2 identifiers: coolant 0x5205 = 0x7B (123-40=83°C),
        // RPM 0x5955 = 0x1068 (4200 * 0.5 = 2100 rpm).
        let r = region(vec![
            0x01, 0x02, // recordNumber, numberOfIdentifiers
            0x52, 0x05, 0x7B, // coolant
            0x59, 0x55, 0x10, 0x68, // RPM (u16)
        ]);
        let decoded = defs.decode(&r, None);
        assert_eq!(decoded.undecoded_tail, None);
        assert_eq!(decoded.fields.len(), 2);
        assert_eq!(decoded.fields[0].uwnr, 0x5205);
        assert_eq!(decoded.fields[0].label, "Kühlmitteltemperatur");
        assert_eq!(decoded.fields[0].unit.as_deref(), Some("degC"));
        approx(decoded.fields[0].value.unwrap(), 83.0);
        approx(decoded.fields[1].value.unwrap(), 2100.0);
    }

    #[test]
    fn decode_snapshot_mileage_sentinel_is_not_available() {
        let defs = defs_from(vec![KM.to_vec()]);
        // KM present: 0x022C2A = 142_378 km.
        let present = defs.decode(
            &region(vec![0x01, 0x01, 0x17, 0x00, 0x02, 0x2C, 0x2A]),
            None,
        );
        assert!(present.fields[0].available);
        approx(present.fields[0].value.unwrap(), 142_378.0);
        // KM sentinel 0xFFFFFF: not available, value None.
        let absent = defs.decode(
            &region(vec![0x01, 0x01, 0x17, 0x00, 0xFF, 0xFF, 0xFF]),
            None,
        );
        assert!(!absent.fields[0].available);
        assert_eq!(absent.fields[0].value, None);
        assert_eq!(absent.fields[0].raw, vec![0xFF, 0xFF, 0xFF]);
    }

    #[test]
    fn decode_snapshot_stops_at_unknown_identifier() {
        let defs = defs_from(vec![COOLANT.to_vec()]);
        // Coolant decodes; then an unknown UWNR 0x9999 — we cannot size it, so the
        // walk stops and hands back the rest as an undecoded tail.
        let r = region(vec![
            0x01, 0x02, // 2 identifiers claimed
            0x52, 0x05, 0x7B, // coolant (known)
            0x99, 0x99, 0xAB, 0xCD, // unknown — stop here
        ]);
        let decoded = defs.decode(&r, None);
        assert_eq!(decoded.fields.len(), 1);
        assert_eq!(decoded.undecoded_tail, Some(vec![0x99, 0x99, 0xAB, 0xCD]));
    }

    #[test]
    fn decode_snapshot_empty_region_is_no_fields() {
        let defs = defs_from(vec![COOLANT.to_vec()]);
        let decoded = defs.decode(&region(vec![]), None);
        assert!(decoded.fields.is_empty());
        assert_eq!(decoded.undecoded_tail, None);
    }

    #[test]
    fn decode_snapshot_unlabeled_falls_back_to_sgbd_text() {
        let defs = defs_from(vec![RPM.to_vec()]);
        let decoded = defs.decode(&region(vec![0x01, 0x01, 0x59, 0x55, 0x10, 0x68]), None);
        // No DB → the German SGBD UWTEXT is the label.
        assert_eq!(decoded.fields[0].label, "Motordrehzahl");
    }

    #[test]
    fn standard_ids_override_measurement_table_on_merge() {
        // Both tables define 0x1700; DTCSNAPSHOTIDENTIFIER (the standard id) wins.
        let mut prg_tables = std::collections::HashMap::new();
        let fumwelt = env_table("FUMWELTTEXTE", vec![COOLANT.to_vec()]);
        let snap = env_table("DTCSNAPSHOTIDENTIFIER", vec![KM.to_vec()]);
        for t in [fumwelt, snap] {
            let cols = EnvColumns::resolve(&t.columns).unwrap();
            for r in &t.rows {
                if let Some(d) = parse_env_row(&cols, r) {
                    prg_tables.insert(d.uwnr, d);
                }
            }
        }
        assert!(prg_tables.contains_key(&0x1700));
        assert!(prg_tables.contains_key(&0x5205));
    }

    /// A `DTCEXTENDEDDATARECORDNUMBER`-shaped table.
    fn ext_table(rows: Vec<Vec<&str>>) -> Table {
        Table {
            name: "DTCEXTENDEDDATARECORDNUMBER".to_string(),
            columns: ["WERT", "TEXT", "ANZ_BYTE"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            rows: rows
                .into_iter()
                .map(|r| r.into_iter().map(str::to_string).collect())
                .collect(),
        }
    }

    fn ext_defs_from(rows: Vec<Vec<&str>>) -> ExtDataDefs {
        let table = ext_table(rows);
        let at = |name: &str| table.columns.iter().position(|c| c == name).unwrap();
        let (wert, text, anz) = (at("WERT"), at("TEXT"), at("ANZ_BYTE"));
        let by_record = table
            .rows
            .iter()
            .filter_map(|row| {
                let cell = |i: usize| row[i].as_str();
                Some((
                    parse_u8(cell(wert))?,
                    (
                        cell(text).to_string(),
                        cell(anz).trim().parse::<usize>().ok()?,
                    ),
                ))
            })
            .collect();
        ExtDataDefs { by_record }
    }

    #[test]
    fn decode_extended_data_reads_frequency_counter() {
        // The real DDE extended-data records: HFK (0x02, 1 byte) = occurrence count.
        let defs = ext_defs_from(vec![
            vec!["0x01", "CONDITION_BYTE", "1"],
            vec!["0x02", "HFK", "1"],
            vec!["0x03", "HLZ", "1"],
        ]);
        // record 0x02 (HFK) value 0x1F, then record 0x03 (HLZ) value 0x02.
        let decoded = defs.decode(&region(vec![0x02, 0x1F, 0x03, 0x02]));
        assert_eq!(decoded.records.len(), 2);
        assert_eq!(decoded.records[0].label, "HFK");
        assert_eq!(decoded.records[0].value, Some(0x1F));
        assert_eq!(decoded.records[1].label, "HLZ");
        assert_eq!(decoded.records[1].value, Some(0x02));
        assert_eq!(decoded.undecoded_tail, None);
    }

    #[test]
    fn decode_extended_data_stops_at_unknown_record() {
        let defs = ext_defs_from(vec![vec!["0x02", "HFK", "1"]]);
        let decoded = defs.decode(&region(vec![0x02, 0x1F, 0x77, 0x00]));
        assert_eq!(decoded.records.len(), 1);
        assert_eq!(decoded.undecoded_tail, Some(vec![0x77, 0x00]));
    }

    // End-to-end on the real DDE SGBD: load the three tables, decode a synthetic
    // (but definition-sized) region. Ignored by default (BYO data). The region bytes
    // are DERIVED, not captured — this proves the table-driven decode, not the wire.
    #[test]
    #[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
    fn real_dde_defs_size_and_scale_known_fields() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/Testmodule(1)/Ecu/d72n47a0.prg");
        let prg = Prg::open(&path).expect("open real SGBD");
        let defs = SnapshotDefs::from_prg(&prg);
        assert!(defs.len() > 400, "expected the FUMWELTTEXTE catalog");
        // Coolant 0x5205 as u8 − 40: raw 0x7B → 83 °C.
        let decoded = defs.decode(&region(vec![0x01, 0x01, 0x52, 0x05, 0x7B]), None);
        approx(decoded.fields[0].value.unwrap(), 83.0);
        let ext = ExtDataDefs::from_prg(&prg);
        assert!(!ext.is_empty(), "expected DTCEXTENDEDDATARECORDNUMBER");
    }
}
