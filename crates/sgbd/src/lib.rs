//! EDIABAS SGBD (`.prg`/`.grp`) container parsing: header, body, and tables.
//!
//! An SGBD is BMW's compiled ECU-description file as shipped in EDIABAS/ISTA. This
//! crate reads the *container* — it does **not** interpret the BEST/2 bytecode. It
//! exposes the file's embedded **tables** as plain `(name, columns, rows)` string
//! data, which is all the klartext semantic layer needs to turn a proprietary
//! measurement's raw bytes into an engineering value (the `SG_FUNKTIONEN` table
//! carries the scaling: id, unit, data type, multiplier, divisor, offset). See
//! `docs/sgbd-findings.md`.
//!
//! ## Format (the `@EDIABAS OBJECT` container)
//! - A 16-byte magic `@EDIABAS OBJECT`, then a little-endian header of section
//!   pointers; the file type lives at `0x10` (0 = group, 1 = variant).
//! - Everything from offset `0xA0` to end is obfuscated by a single-byte XOR with
//!   `0xF7` (no compression, no key schedule). The plaintext header before it is
//!   why a raw `strings` scan shows only the magic and the `.B2V` source name.
//! - A **table directory** is pointed to from `0x84`: a count followed by fixed
//!   `0x50`-byte entries (name, then offsets/counts for the cell data). Cells are
//!   null-terminated CP1252 strings laid out row-major, header row first.
//!
//! ## Scope and non-goals
//! Read-only and degrading by design: parsing a malformed file returns an
//! [`SgbdError`] rather than panicking, and callers that only want one table
//! degrade gracefully when it is absent. BMW's SGBD content is BYO-data and is
//! never embedded; this crate only provides the lens to read a user-supplied file.

pub mod cp1252;
mod prg;

#[doc(inline)]
pub use prg::{Prg, SgbdError, Table};
