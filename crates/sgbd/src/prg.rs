//! The `@EDIABAS OBJECT` container: header, XOR-`0xF7` body, and the table directory.
//!
//! [`Prg::parse`] takes the raw file bytes (sans-IO; [`Prg::open`] is the thin file
//! wrapper) and returns the embedded [`Table`]s as decoded `(name, columns, rows)`
//! strings. It deliberately ignores the BEST/2 bytecode and job sections — only the
//! tables are needed for measurement scaling.

use std::path::{Path, PathBuf};

use thiserror::Error;

use crate::cp1252;

/// The 16-byte container signature (15 ASCII bytes; byte 16 is a NUL).
const MAGIC: &[u8] = b"@EDIABAS OBJECT";

/// Offset where the obfuscated body begins; bytes below this are plaintext.
const DATA_OFFSET: usize = 0xA0;

/// The whole-body obfuscation key: every byte from [`DATA_OFFSET`] is XORed by it.
const XOR_KEY: u8 = 0xF7;

/// Header offset holding the little-endian pointer to the table directory.
const OFFSET_TABLE_DIR: usize = 0x84;

/// Size of one fixed-layout entry in the table directory.
const TABLE_ENTRY_SIZE: usize = 0x50;

/// Within a directory entry: offset of the cell-data pointer, column, and row counts.
const ENTRY_CELL_PTR: usize = 0x40;
const ENTRY_COLUMNS: usize = 0x48;
const ENTRY_ROWS: usize = 0x4C;

/// Upper bound on the table count, to reject a misread (garbage) directory header.
///
/// A real SGBD has at most a few hundred tables; the raw (still-obfuscated) read of
/// the count is astronomically large, so any plausible value is far below this.
const MAX_TABLES: u32 = 100_000;

/// Maximum bytes of a directory entry's name field.
const NAME_FIELD_LEN: usize = 64;

/// A decoded SGBD table: its name, column headers, and data rows.
///
/// `columns` is the table's header row; each entry of `rows` has one cell per
/// column (cells are raw strings — numbers are not parsed here).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Table {
    /// The table name, e.g. `SG_FUNKTIONEN`.
    pub name: String,
    /// The column header cells, in order.
    pub columns: Vec<String>,
    /// The data rows (header excluded), each a vector of cells.
    pub rows: Vec<Vec<String>>,
}

/// A parsed SGBD container, exposing its embedded [`Table`]s.
#[derive(Debug, Clone)]
pub struct Prg {
    tables: Vec<Table>,
}

/// An error from reading or parsing an SGBD file.
#[derive(Debug, Error)]
pub enum SgbdError {
    /// The bytes do not start with the `@EDIABAS OBJECT` signature.
    #[error("not an EDIABAS SGBD: missing '@EDIABAS OBJECT' signature")]
    BadMagic,
    /// The file is too short to hold the header the parser must read.
    #[error("SGBD file truncated: need at least {needed} bytes, got {got}")]
    Truncated {
        /// Minimum byte length the header requires.
        needed: usize,
        /// Actual byte length seen.
        got: usize,
    },
    /// The file at `path` could not be read.
    #[error("reading SGBD file at {path}: {source}")]
    Io {
        /// The path that failed to read.
        path: PathBuf,
        /// The underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

impl Prg {
    /// Parse an SGBD container from its raw bytes.
    ///
    /// # Errors
    /// Returns [`SgbdError::BadMagic`] if the signature is missing, or
    /// [`SgbdError::Truncated`] if the buffer is too short for the header.
    pub fn parse(bytes: &[u8]) -> Result<Self, SgbdError> {
        if bytes.len() < MAGIC.len() || &bytes[..MAGIC.len()] != MAGIC {
            return Err(SgbdError::BadMagic);
        }
        let header_end = OFFSET_TABLE_DIR + 4;
        if bytes.len() < header_end {
            return Err(SgbdError::Truncated {
                needed: header_end,
                got: bytes.len(),
            });
        }
        Ok(Self {
            tables: parse_tables(bytes),
        })
    }

    /// Read and parse an SGBD file from `path`.
    ///
    /// # Errors
    /// Returns [`SgbdError::Io`] if the file cannot be read, else as [`Prg::parse`].
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SgbdError> {
        let path = path.as_ref();
        let bytes = std::fs::read(path).map_err(|source| SgbdError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        Self::parse(&bytes)
    }

    /// Find a table by exact name, or `None` if this SGBD has no such table.
    pub fn table(&self, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name)
    }

    /// All tables in the container, in directory order.
    pub fn tables(&self) -> &[Table] {
        &self.tables
    }
}

/// De-obfuscate one byte: bytes from [`DATA_OFFSET`] onward are XORed by [`XOR_KEY`].
fn deobfuscate(byte: u8, offset: usize) -> u8 {
    if offset >= DATA_OFFSET {
        byte ^ XOR_KEY
    } else {
        byte
    }
}

/// Read a little-endian `u32` verbatim (header pointers and the directory count).
fn raw_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    Some(u32::from_le_bytes(slice.try_into().expect("4-byte slice")))
}

/// Read a little-endian `u32`, de-obfuscating each byte by its absolute offset.
fn deobf_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let slice = bytes.get(offset..offset + 4)?;
    let mut out = [0u8; 4];
    for (i, dst) in out.iter_mut().enumerate() {
        *dst = deobfuscate(slice[i], offset + i);
    }
    Some(u32::from_le_bytes(out))
}

/// Read a NUL-terminated, de-obfuscated CP1252 string capped at `max` bytes.
///
/// Returns the decoded text and the bytes consumed (including the terminator when
/// one is found within range), so a caller can advance to the next cell.
fn read_string(bytes: &[u8], offset: usize, max: usize) -> (String, usize) {
    let mut raw = Vec::new();
    let end = offset.saturating_add(max).min(bytes.len());
    let mut i = offset;
    while i < end {
        let c = deobfuscate(bytes[i], i);
        i += 1;
        if c == 0 {
            break;
        }
        raw.push(c);
    }
    (cp1252::decode(&raw), i - offset)
}

/// Parse the table directory pointed to from [`OFFSET_TABLE_DIR`] into [`Table`]s.
fn parse_tables(bytes: &[u8]) -> Vec<Table> {
    let Some(dir) = raw_u32(bytes, OFFSET_TABLE_DIR).map(|v| v as usize) else {
        return Vec::new();
    };
    if dir == 0 || dir + 4 > bytes.len() {
        return Vec::new();
    }
    // The count is plaintext in some files and obfuscated in others; accept
    // whichever read is plausible (a still-obfuscated misread dwarfs MAX_TABLES).
    let Some(count) =
        plausible_count(raw_u32(bytes, dir)).or_else(|| plausible_count(deobf_u32(bytes, dir)))
    else {
        return Vec::new();
    };

    let entries_start = dir + 4;
    let mut tables = Vec::new();
    for i in 0..count as usize {
        let entry = entries_start + i * TABLE_ENTRY_SIZE;
        if entry + TABLE_ENTRY_SIZE > bytes.len() {
            break;
        }
        let (name, _) = read_string(bytes, entry, NAME_FIELD_LEN);
        let cell_ptr = deobf_u32(bytes, entry + ENTRY_CELL_PTR).unwrap_or(0) as usize;
        let columns = deobf_u32(bytes, entry + ENTRY_COLUMNS).unwrap_or(0) as usize;
        let rows = deobf_u32(bytes, entry + ENTRY_ROWS).unwrap_or(0) as usize;
        let mut cells = parse_cells(bytes, cell_ptr, columns, rows);
        let header = if cells.is_empty() {
            Vec::new()
        } else {
            cells.remove(0)
        };
        tables.push(Table {
            name,
            columns: header,
            rows: cells,
        });
    }
    tables
}

/// Accept a directory count only when present and within the sane upper bound.
fn plausible_count(value: Option<u32>) -> Option<u32> {
    value.filter(|&c| (1..=MAX_TABLES).contains(&c))
}

/// Read the cell grid: `rows + 1` rows (header first) of `columns` NUL-terminated cells.
fn parse_cells(bytes: &[u8], start: usize, columns: usize, rows: usize) -> Vec<Vec<String>> {
    let mut grid = Vec::new();
    let mut offset = start;
    let total_rows = rows.saturating_add(1); // header row plus data rows
    for _ in 0..total_rows {
        let mut row = Vec::with_capacity(columns);
        for _ in 0..columns {
            if offset >= bytes.len() {
                break;
            }
            let (cell, consumed) = read_string(bytes, offset, bytes.len() - offset);
            row.push(cell);
            if consumed == 0 {
                break; // defensive: never stall on malformed input
            }
            offset += consumed;
        }
        if !row.is_empty() {
            grid.push(row);
        }
    }
    grid
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A table to embed in a synthetic `.prg` fixture (no BMW data).
    struct Tbl<'a> {
        name: &'a str,
        columns: &'a [&'a str],
        rows: &'a [&'a [&'a str]],
    }

    /// Build a minimal valid `@EDIABAS OBJECT` file embedding `tables`.
    ///
    /// Lays out a plaintext header (magic, version, table-directory pointer) then an
    /// XOR-`0xF7` body of `count`, fixed `0x50`-byte entries, and null-terminated
    /// cells (header row first) — exactly the shape [`Prg::parse`] must read back.
    fn build_prg(tables: &[Tbl]) -> Vec<u8> {
        let mut header = vec![0u8; DATA_OFFSET];
        header[..MAGIC.len()].copy_from_slice(MAGIC);
        header[0x10..0x14].copy_from_slice(&1u32.to_le_bytes()); // version: variant
        header[OFFSET_TABLE_DIR..OFFSET_TABLE_DIR + 4]
            .copy_from_slice(&u32::try_from(DATA_OFFSET).unwrap().to_le_bytes());

        let n = tables.len();
        let entries_start = DATA_OFFSET + 4;
        let cells_start = entries_start + n * TABLE_ENTRY_SIZE;

        // Cell blob (all tables) + each table's absolute cell-data offset.
        let mut cells = Vec::new();
        let mut cell_ptrs = Vec::new();
        for t in tables {
            cell_ptrs.push(cells_start + cells.len());
            for &c in t.columns {
                cells.extend_from_slice(&cp1252_encode(c));
                cells.push(0);
            }
            for row in t.rows {
                for &c in *row {
                    cells.extend_from_slice(&cp1252_encode(c));
                    cells.push(0);
                }
            }
        }

        let mut body = Vec::new();
        body.extend_from_slice(&u32::try_from(n).unwrap().to_le_bytes());
        for (i, t) in tables.iter().enumerate() {
            let mut entry = vec![0u8; TABLE_ENTRY_SIZE];
            entry[..t.name.len()].copy_from_slice(t.name.as_bytes());
            entry[ENTRY_CELL_PTR..ENTRY_CELL_PTR + 4]
                .copy_from_slice(&u32::try_from(cell_ptrs[i]).unwrap().to_le_bytes());
            entry[ENTRY_COLUMNS..ENTRY_COLUMNS + 4]
                .copy_from_slice(&u32::try_from(t.columns.len()).unwrap().to_le_bytes());
            entry[ENTRY_ROWS..ENTRY_ROWS + 4]
                .copy_from_slice(&u32::try_from(t.rows.len()).unwrap().to_le_bytes());
            body.extend_from_slice(&entry);
        }
        body.extend_from_slice(&cells);

        for b in &mut body {
            *b ^= XOR_KEY;
        }
        header.extend_from_slice(&body);
        header
    }

    #[test]
    fn parses_single_table_with_header_and_rows() {
        let bytes = build_prg(&[Tbl {
            name: "SG_FUNKTIONEN",
            columns: &["ID", "MUL", "ADD"],
            rows: &[
                &["0x4BC3", "0.100000", "-273.140000"],
                &["0x4BC4", "1.0", "0.0"],
            ],
        }]);
        let prg = Prg::parse(&bytes).expect("valid SGBD");
        let t = prg.table("SG_FUNKTIONEN").expect("SG_FUNKTIONEN present");
        assert_eq!(t.columns, ["ID", "MUL", "ADD"]);
        assert_eq!(t.rows.len(), 2);
        assert_eq!(t.rows[0], ["0x4BC3", "0.100000", "-273.140000"]);
        assert_eq!(t.rows[1], ["0x4BC4", "1.0", "0.0"]);
    }

    /// Encode Latin-1/ASCII text back to CP1252 bytes for fixtures (test-only).
    fn cp1252_encode(s: &str) -> Vec<u8> {
        s.chars()
            .map(|c| u8::try_from(u32::from(c)).expect("test cells stay in Latin-1"))
            .collect()
    }

    #[test]
    fn rejects_bad_magic() {
        let err = Prg::parse(b"not an sgbd file at all..........").unwrap_err();
        assert!(matches!(err, SgbdError::BadMagic));
    }

    #[test]
    fn rejects_truncated_header() {
        // Has the magic but is too short to hold the table-directory pointer.
        let err = Prg::parse(MAGIC).unwrap_err();
        assert!(matches!(err, SgbdError::Truncated { .. }));
    }

    #[test]
    fn unknown_table_is_none() {
        let bytes = build_prg(&[Tbl {
            name: "JOBRESULT",
            columns: &["A"],
            rows: &[&["1"]],
        }]);
        let prg = Prg::parse(&bytes).unwrap();
        assert!(prg.table("SG_FUNKTIONEN").is_none());
        assert_eq!(prg.tables().len(), 1);
    }

    #[test]
    fn selects_named_table_among_several_and_decodes_cp1252() {
        let bytes = build_prg(&[
            Tbl {
                name: "FORTTEXTE",
                columns: &["TEXT"],
                rows: &[&["x"]],
            },
            Tbl {
                name: "SG_FUNKTIONEN",
                columns: &["EINHEIT", "INFO"],
                rows: &[&["°C", "Kühlmitteltemperatur"]],
            },
        ]);
        let prg = Prg::parse(&bytes).unwrap();
        let t = prg.table("SG_FUNKTIONEN").expect("present");
        // CP1252 single bytes (0xB0, 0xFC), not UTF-8 multibyte — proves the decode.
        assert_eq!(t.rows[0][0], "°C");
        assert_eq!(t.rows[0][1], "Kühlmitteltemperatur");
    }

    #[test]
    fn parses_realistic_sg_funktionen_row() {
        // The 16-column SG_FUNKTIONEN shape with the engine-temperature row.
        let cols = [
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
        ];
        let row = [
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
        ];
        let bytes = build_prg(&[Tbl {
            name: "SG_FUNKTIONEN",
            columns: &cols,
            rows: &[&row],
        }]);
        let prg = Prg::parse(&bytes).unwrap();
        let t = prg.table("SG_FUNKTIONEN").unwrap();
        assert_eq!(t.columns.len(), 16);
        assert_eq!(t.rows[0][1], "0x4BC3"); // ID
        assert_eq!(t.rows[0][7], "unsigned int"); // DATENTYP
        assert_eq!(t.rows[0][9], "0.100000"); // MUL
        assert_eq!(t.rows[0][11], "-273.140000"); // ADD
        assert_eq!(t.rows[0][13], "22;2C"); // SERVICE
    }

    // Cross-check against the real DDE SGBD (and, transitively, the ediabasx oracle
    // whose JSON gave these same figures). Ignored by default — BYO data; asserts
    // structure plus the engine-temperature row's public formula constants only.
    #[test]
    #[ignore = "requires BYO SGBD data: data/Testmodule(1)/Ecu/d72n47a0.prg"]
    fn real_dde_sg_funktionen_matches_ediabasx() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/Testmodule(1)/Ecu/d72n47a0.prg");
        let prg = Prg::open(&path).expect("open real SGBD");
        let t = prg.table("SG_FUNKTIONEN").expect("SG_FUNKTIONEN present");
        assert_eq!(t.columns.len(), 16);
        assert_eq!(t.rows.len(), 1787, "row count must match ediabasx");
        let temp = t
            .rows
            .iter()
            .find(|r| r.get(2).is_some_and(|c| c == "STAT_MOTORTEMPERATUR_WERT"))
            .expect("engine-temperature row");
        assert_eq!(temp[1], "0x4BC3");
        assert_eq!(temp[4], "degC");
        assert_eq!(temp[7], "unsigned int");
        assert_eq!(temp[9], "0.100000");
        assert_eq!(temp[11], "-273.140000");
    }
}
