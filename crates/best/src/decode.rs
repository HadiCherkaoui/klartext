//! BEST/2 instruction decoder: raw job bytecode into a `Vec<Op>`.
//!
//! Every BEST/2 instruction is laid out as `[opcode][mode][arg0…][arg1…]`: one
//! opcode byte (looked up via [`crate::info`]), one addressing-mode byte whose
//! high nibble selects `arg0`'s mode and low nibble selects `arg1`'s, then each
//! operand's bytes per its mode. [`decode_job`] walks this layout from offset 0
//! to the end of the slice.
//!
//! ## `eoj` ends execution, not decoding
//! `eoj` (`0x1D`) halts the *running* job, but jobs branch over early-exit
//! `eoj`s, so instructions the code only reaches past one still have to be
//! decoded. [`decode_job`] therefore keeps decoding after an `eoj` to the slice
//! bound instead of stopping at the first — stopping there truncated every real
//! F20 job to its arg-validation stub (the spec §1 finding:
//! `docs/superpowers/specs/2026-07-06-item5-guided-service-procedures-design.md`).
//! Once at least one `eoj` has been decoded, a decode error at a later offset is
//! trailing padding and truncates the result there; a decode error before any
//! `eoj` is a real fault and stays a hard [`DecodeError`]. A branch into bytes
//! that were never decoded is caught at run time by the engine's `BadPc`, not
//! here.
//!
//! ## Addressing modes
//! [`AddrMode`] enumerates the sixteen operand encodings (values `0..=15`),
//! from `None` (no bytes) through immediates, single registers, and the indexed
//! forms that combine a base register with an index and optional length. A
//! register selector byte resolves to a [`RegBank`] and index through the same
//! global register table EDIABAS uses; there is no float addressing mode (float
//! opcodes address `F` registers directly), so `RegBank::F` exists for the
//! machine but is never produced by decoding.
//!
//! ## No degrade-to-raw
//! Before the job's terminating `eoj`, an unknown opcode, an opcode EDIABAS
//! leaves unimplemented, an invalid register selector, or operand bytes running
//! past the end of the job are each a hard [`DecodeError`] — never a silent skip
//! or default. (Past the final `eoj` those same conditions are trailing padding
//! and truncate the result; see `eoj` ends execution, not decoding, above.)
//!
//! ## Where the facts come from
//! The instruction layout, the sixteen addressing-mode numbers, their per-mode
//! operand byte layouts, and the register selector table are **facts** about
//! BMW's BEST/2 binary format, transcribed from EDIABAS's decode path and
//! reimplemented in our own types — no source code is copied (klartext is
//! AGPL-3.0; the reference is read as an offline oracle only).

use crate::opcode::{OpClass, info};

/// The `eoj` (end-of-job) opcode. It ends the running job, but decoding
/// continues past it to the slice bound (see the module docs).
const EOJ: u8 = 0x1D;

/// One of the sixteen BEST/2 addressing modes (operand encodings).
///
/// Discriminants are the on-disk mode numbers `0..=15`; the addressing-mode
/// byte packs `arg0`'s mode in its high nibble and `arg1`'s in its low nibble.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddrMode {
    /// No operand; consumes no bytes.
    None = 0,
    /// String register selector (`S` bank).
    RegS = 1,
    /// Byte register selector (`B`/`A` bank).
    RegAb = 2,
    /// Word register selector (`I` bank).
    RegI = 3,
    /// Long register selector (`L` bank).
    RegL = 4,
    /// 8-bit immediate.
    Imm8 = 5,
    /// 16-bit little-endian immediate.
    Imm16 = 6,
    /// 32-bit little-endian immediate.
    Imm32 = 7,
    /// Length-prefixed byte string (`u16` length, then that many bytes).
    ImmStr = 8,
    /// Base register indexed by an immediate.
    IdxImm = 9,
    /// Base register indexed by a register.
    IdxReg = 10,
    /// Base register indexed by a register, plus an immediate increment.
    IdxRegImm = 11,
    /// Base register, immediate index, immediate length.
    IdxImmLenImm = 12,
    /// Base register, immediate index, register length.
    IdxImmLenReg = 13,
    /// Base register, register index, immediate length.
    IdxRegLenImm = 14,
    /// Base register, register index, register length.
    IdxRegLenReg = 15,
}

/// A BEST/2 register bank.
///
/// `B` holds the 8-bit `B0..BF` and `A0..AF` registers, `I` the 16-bit words,
/// `L` the 32-bit longs, `S` the string buffers, and `F` the IEEE-754 floats.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegBank {
    /// 8-bit byte registers (`B0..BF`, `A0..AF`).
    B,
    /// 16-bit word registers (`I0..IF`).
    I,
    /// 32-bit long registers (`L0..L7`).
    L,
    /// String-buffer registers (`S0..SF`).
    S,
    /// IEEE-754 float registers (`F0..F7`); addressed directly by float ops.
    F,
}

/// A resolved register reference: bank plus index within that bank.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RegId {
    /// Which register bank the selector resolved to.
    pub bank: RegBank,
    /// Zero-based index within the bank.
    pub idx: u8,
}

/// The index or length sub-operand of an indexed addressing mode.
///
/// An indexed operand's index (and its optional trailing length or increment)
/// is either an immediate value or a register reference.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IndexArg {
    /// An immediate value (the encoding stores it as a `u16`).
    Imm(i64),
    /// A register reference.
    Reg(RegId),
}

/// A decoded instruction operand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Operand {
    /// No operand (addressing mode `None`).
    None,
    /// An immediate value (from `Imm8`/`Imm16`/`Imm32`), stored unsigned.
    Imm(i64),
    /// A length-prefixed byte string literal (`ImmStr`).
    Str(Vec<u8>),
    /// A single register reference.
    Reg {
        /// Register bank the selector resolved to.
        bank: RegBank,
        /// Zero-based index within the bank.
        idx: u8,
    },
    /// A base register indexed by an immediate or register, with an optional
    /// trailing length (for the `…Len…` modes) or increment (for `IdxRegImm`).
    Indexed {
        /// The base register being indexed.
        base: RegId,
        /// The index applied to the base register.
        index: IndexArg,
        /// The trailing length or increment sub-operand, if the mode has one.
        len: Option<IndexArg>,
    },
}

/// One decoded BEST/2 instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Op {
    /// The leading opcode byte.
    pub byte: u8,
    /// The addressing-mode byte (`arg0` mode in the high nibble, `arg1` in low).
    pub mode_byte: u8,
    /// The first decoded operand.
    pub arg0: Operand,
    /// The second decoded operand.
    pub arg1: Operand,
    /// Total encoded length of this instruction in bytes.
    pub len: usize,
    /// This instruction's start byte offset within the job.
    pub offset: usize,
}

/// An error encountered while decoding a job's bytecode.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    /// A byte that is not a defined opcode (`> 0xB7`).
    #[error("unknown opcode byte 0x{0:02X}")]
    UnknownOpcode(u8),
    /// An opcode EDIABAS itself leaves unimplemented (extended communication).
    #[error("opcode `{0}` is not implemented by EDIABAS")]
    Unimplemented(&'static str),
    /// A register selector byte outside the valid register table.
    #[error("register selector byte 0x{0:02X} is not a valid register")]
    BadRegister(u8),
    /// Operand bytes ran past the end of the job buffer.
    #[error("truncated instruction: operand bytes run past end of job")]
    Truncated,
}

/// Decodes a job's BEST/2 bytecode into its linear list of instructions.
///
/// Walks `code` from offset 0 to the end of the slice, decoding each
/// `[opcode][mode][arg0][arg1]` instruction and recording its `offset`.
/// Decoding continues past every `eoj` (`0x1D`) — `eoj` ends execution, not
/// decoding — so a job that branches past an early-exit `eoj` keeps those later
/// instructions.
///
/// Once at least one `eoj` has been decoded, a decode error at a later offset
/// truncates the returned list there (the tail is treated as padding). Before
/// any `eoj`, every decode error is returned.
///
/// # Errors
/// Before the first `eoj`, returns [`DecodeError::UnknownOpcode`] for a byte
/// outside the opcode table, [`DecodeError::Unimplemented`] for an opcode
/// EDIABAS never runs, [`DecodeError::BadRegister`] for an invalid register
/// selector, and [`DecodeError::Truncated`] when an instruction's bytes run
/// past the end of `code`. Past the final `eoj` those same conditions truncate
/// the result instead of erroring.
pub fn decode_job(code: &[u8]) -> Result<Vec<Op>, DecodeError> {
    let mut reader = Reader::new(code);
    let mut ops = Vec::new();
    let mut seen_eoj = false;

    while reader.pos < code.len() {
        match decode_one(&mut reader) {
            Ok(op) => {
                if op.byte == EOJ {
                    seen_eoj = true;
                }
                ops.push(op);
            }
            // After the final eoj a decode error is trailing padding, so stop
            // and keep what decoded; before any eoj it is a real fault.
            Err(_) if seen_eoj => break,
            Err(e) => return Err(e),
        }
    }

    Ok(ops)
}

/// Decodes the single `[opcode][mode][arg0][arg1]` instruction at the reader's
/// current position, recording its `offset` and encoded length.
///
/// # Errors
/// Returns [`DecodeError::UnknownOpcode`] for a byte outside the opcode table,
/// [`DecodeError::Unimplemented`] for an opcode EDIABAS never runs,
/// [`DecodeError::BadRegister`] for an invalid register selector, and
/// [`DecodeError::Truncated`] when the instruction's bytes run past the end of
/// the slice. [`decode_job`] decides whether a failure past the job's final
/// `eoj` is tolerable trailing padding.
fn decode_one(reader: &mut Reader<'_>) -> Result<Op, DecodeError> {
    let offset = reader.pos;
    let byte = reader.read_u8()?;

    // Validate the opcode's identity before requiring its mode byte, so a
    // bare unknown/unimplemented opcode reports that rather than truncation.
    let opinfo = info(byte).ok_or(DecodeError::UnknownOpcode(byte))?;
    if opinfo.class == OpClass::Unimplemented {
        return Err(DecodeError::Unimplemented(opinfo.mnemonic));
    }

    let mode_byte = reader.read_u8()?;
    let arg0 = read_operand(reader, AddrMode::from_nibble(mode_byte >> 4))?;
    let arg1 = read_operand(reader, AddrMode::from_nibble(mode_byte & 0x0F))?;

    Ok(Op {
        byte,
        mode_byte,
        arg0,
        arg1,
        len: reader.pos - offset,
        offset,
    })
}

impl AddrMode {
    /// Maps a 4-bit addressing-mode nibble (`0..=15`) to its mode.
    ///
    /// # Panics
    /// Panics if `nibble > 15`; callers must pass a masked half-byte.
    fn from_nibble(nibble: u8) -> Self {
        match nibble {
            0 => Self::None,
            1 => Self::RegS,
            2 => Self::RegAb,
            3 => Self::RegI,
            4 => Self::RegL,
            5 => Self::Imm8,
            6 => Self::Imm16,
            7 => Self::Imm32,
            8 => Self::ImmStr,
            9 => Self::IdxImm,
            10 => Self::IdxReg,
            11 => Self::IdxRegImm,
            12 => Self::IdxImmLenImm,
            13 => Self::IdxImmLenReg,
            14 => Self::IdxRegLenImm,
            15 => Self::IdxRegLenReg,
            _ => unreachable!("addressing-mode nibble must be masked to 0..=15"),
        }
    }
}

/// A forward cursor over a job's bytecode with bounds-checked reads.
struct Reader<'a> {
    code: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    /// Creates a cursor positioned at the start of `code`.
    fn new(code: &'a [u8]) -> Self {
        Self { code, pos: 0 }
    }

    /// Reads the next byte and advances the cursor.
    ///
    /// # Errors
    /// Returns [`DecodeError::Truncated`] at the end of the buffer.
    fn read_u8(&mut self) -> Result<u8, DecodeError> {
        let byte = *self.code.get(self.pos).ok_or(DecodeError::Truncated)?;
        self.pos += 1;
        Ok(byte)
    }

    /// Reads the next `n` bytes as a slice and advances the cursor.
    ///
    /// # Errors
    /// Returns [`DecodeError::Truncated`] if fewer than `n` bytes remain.
    fn read_bytes(&mut self, n: usize) -> Result<&'a [u8], DecodeError> {
        let code = self.code;
        let end = self.pos.checked_add(n).ok_or(DecodeError::Truncated)?;
        let slice = code.get(self.pos..end).ok_or(DecodeError::Truncated)?;
        self.pos = end;
        Ok(slice)
    }

    /// Reads a little-endian `u16` and advances the cursor.
    ///
    /// # Errors
    /// Returns [`DecodeError::Truncated`] if fewer than two bytes remain.
    fn read_u16_le(&mut self) -> Result<u16, DecodeError> {
        let lo = self.read_u8()?;
        let hi = self.read_u8()?;
        Ok(u16::from_le_bytes([lo, hi]))
    }

    /// Reads a little-endian `u32` and advances the cursor.
    ///
    /// # Errors
    /// Returns [`DecodeError::Truncated`] if fewer than four bytes remain.
    fn read_u32_le(&mut self) -> Result<u32, DecodeError> {
        let b0 = self.read_u8()?;
        let b1 = self.read_u8()?;
        let b2 = self.read_u8()?;
        let b3 = self.read_u8()?;
        Ok(u32::from_le_bytes([b0, b1, b2, b3]))
    }
}

/// Resolves a register selector byte to its bank and index within that bank.
///
/// The selector space is the global register table EDIABAS uses: `0x00..=0x33`
/// covers `B`, `I`, `L`, `S`, and `F` in order, and `0x80..=0x9B` continues the
/// high `B`/`I`/`L` registers. Bytes outside those ranges are invalid.
///
/// # Errors
/// Returns [`DecodeError::BadRegister`] for a selector outside the table.
fn register(selector: u8) -> Result<RegId, DecodeError> {
    let (bank, idx) = match selector {
        0x00..=0x0F => (RegBank::B, selector),
        0x10..=0x17 => (RegBank::I, selector - 0x10),
        0x18..=0x1B => (RegBank::L, selector - 0x18),
        0x1C..=0x23 => (RegBank::S, selector - 0x1C),
        0x24..=0x2B => (RegBank::F, selector - 0x24),
        0x2C..=0x33 => (RegBank::S, selector - 0x2C + 8),
        0x80..=0x8F => (RegBank::B, selector - 0x80 + 16),
        0x90..=0x97 => (RegBank::I, selector - 0x90 + 8),
        0x98..=0x9B => (RegBank::L, selector - 0x98 + 4),
        _ => return Err(DecodeError::BadRegister(selector)),
    };
    Ok(RegId { bank, idx })
}

/// Reads one operand's bytes from `r` according to its addressing `mode`.
///
/// Immediates are stored unsigned and widened to `i64`; register selectors are
/// resolved via [`register`]. The `len` field of an [`Operand::Indexed`] holds
/// the trailing length sub-operand, or the increment for `IdxRegImm`.
///
/// # Errors
/// Returns [`DecodeError::Truncated`] if the operand's bytes run past the end of
/// the job, or [`DecodeError::BadRegister`] for an invalid register selector.
fn read_operand(r: &mut Reader<'_>, mode: AddrMode) -> Result<Operand, DecodeError> {
    let operand = match mode {
        AddrMode::None => Operand::None,
        AddrMode::RegS | AddrMode::RegAb | AddrMode::RegI | AddrMode::RegL => {
            let RegId { bank, idx } = register(r.read_u8()?)?;
            Operand::Reg { bank, idx }
        }
        AddrMode::Imm8 => Operand::Imm(i64::from(r.read_u8()?)),
        AddrMode::Imm16 => Operand::Imm(i64::from(r.read_u16_le()?)),
        AddrMode::Imm32 => Operand::Imm(i64::from(r.read_u32_le()?)),
        AddrMode::ImmStr => {
            let len = usize::from(r.read_u16_le()?);
            Operand::Str(r.read_bytes(len)?.to_vec())
        }
        AddrMode::IdxImm => {
            let base = register(r.read_u8()?)?;
            let index = IndexArg::Imm(i64::from(r.read_u16_le()?));
            Operand::Indexed {
                base,
                index,
                len: None,
            }
        }
        AddrMode::IdxReg => {
            let base = register(r.read_u8()?)?;
            let index = IndexArg::Reg(register(r.read_u8()?)?);
            Operand::Indexed {
                base,
                index,
                len: None,
            }
        }
        AddrMode::IdxRegImm => {
            let base = register(r.read_u8()?)?;
            let index = IndexArg::Reg(register(r.read_u8()?)?);
            let len = Some(IndexArg::Imm(i64::from(r.read_u16_le()?)));
            Operand::Indexed { base, index, len }
        }
        AddrMode::IdxImmLenImm => {
            let base = register(r.read_u8()?)?;
            let index = IndexArg::Imm(i64::from(r.read_u16_le()?));
            let len = Some(IndexArg::Imm(i64::from(r.read_u16_le()?)));
            Operand::Indexed { base, index, len }
        }
        AddrMode::IdxImmLenReg => {
            let base = register(r.read_u8()?)?;
            let index = IndexArg::Imm(i64::from(r.read_u16_le()?));
            let len = Some(IndexArg::Reg(register(r.read_u8()?)?));
            Operand::Indexed { base, index, len }
        }
        AddrMode::IdxRegLenImm => {
            let base = register(r.read_u8()?)?;
            let index = IndexArg::Reg(register(r.read_u8()?)?);
            let len = Some(IndexArg::Imm(i64::from(r.read_u16_le()?)));
            Operand::Indexed { base, index, len }
        }
        AddrMode::IdxRegLenReg => {
            let base = register(r.read_u8()?)?;
            let index = IndexArg::Reg(register(r.read_u8()?)?);
            let len = Some(IndexArg::Reg(register(r.read_u8()?)?));
            Operand::Indexed { base, index, len }
        }
    };
    Ok(operand)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Addressing-mode byte for `arg0 = byte register`, `arg1 = 8-bit immediate`.
    const MODE_REGB_IMM8: u8 = ((AddrMode::RegAb as u8) << 4) | (AddrMode::Imm8 as u8);
    /// Addressing-mode byte for `arg0 = 16-bit immediate`, `arg1 = none`.
    const MODE_IMM16_NONE: u8 = ((AddrMode::Imm16 as u8) << 4) | (AddrMode::None as u8);
    /// Addressing-mode byte for `arg0 = 32-bit immediate`, `arg1 = none`.
    const MODE_IMM32_NONE: u8 = ((AddrMode::Imm32 as u8) << 4) | (AddrMode::None as u8);
    /// Addressing-mode byte for `arg0 = string literal`, `arg1 = none`.
    const MODE_STR_NONE: u8 = ((AddrMode::ImmStr as u8) << 4) | (AddrMode::None as u8);
    /// Addressing-mode byte for `arg0 = byte register`, `arg1 = none`.
    const MODE_REGB_NONE: u8 = ((AddrMode::RegAb as u8) << 4) | (AddrMode::None as u8);
    /// Addressing-mode byte for `arg0 = indexed (imm idx, imm len)`, `arg1 = none`.
    const MODE_IDXIMMLENIMM_NONE: u8 =
        ((AddrMode::IdxImmLenImm as u8) << 4) | (AddrMode::None as u8);
    /// A fully `None`/`None` addressing-mode byte (used by `eoj`).
    const MODE_NONE_NONE: u8 = 0x00;

    #[test]
    fn decodes_move_immediate_into_register() {
        // move (0x00): arg0 = register B0 (selector 0x00), arg1 = imm8 0x2A.
        // Then eoj (0x1D) with its None/None mode byte.
        let code = [0x00, MODE_REGB_IMM8, 0x00, 0x2A, EOJ, MODE_NONE_NONE];
        let ops = decode_job(&code).unwrap();

        assert_eq!(ops.len(), 2); // move + eoj
        assert_eq!(ops[0].byte, 0x00);
        assert_eq!(ops[0].mode_byte, MODE_REGB_IMM8);
        assert_eq!(
            ops[0].arg0,
            Operand::Reg {
                bank: RegBank::B,
                idx: 0
            }
        );
        assert_eq!(ops[0].arg1, Operand::Imm(0x2A));
        assert_eq!(ops[0].len, 4);
        assert_eq!(ops[0].offset, 0);
        assert_eq!(ops[1].byte, EOJ);
        assert_eq!(ops[1].offset, 4);
        assert_eq!(ops[1].len, 2);
    }

    #[test]
    fn mode_constant_matches_hand_computed_value() {
        // RegAb = 2, Imm8 = 5 -> (2 << 4) | 5 = 0x25.
        assert_eq!(MODE_REGB_IMM8, 0x25);
    }

    #[test]
    fn unknown_opcode_is_hard_error() {
        assert!(matches!(
            decode_job(&[0xC0]),
            Err(DecodeError::UnknownOpcode(0xC0))
        ));
    }

    #[test]
    fn unimplemented_opcode_is_hard_error() {
        // 0xAF = xopen, which EDIABAS leaves unimplemented.
        assert!(matches!(
            decode_job(&[0xAF, MODE_NONE_NONE]),
            Err(DecodeError::Unimplemented("xopen"))
        ));
    }

    #[test]
    fn truncated_operand_is_hard_error() {
        // jump (0x0B) with an Imm32 arg0 but only two of four immediate bytes.
        let code = [0x0B, MODE_IMM32_NONE, 0x0A, 0x00];
        assert_eq!(decode_job(&code), Err(DecodeError::Truncated));
    }

    #[test]
    fn job_tiling_to_the_bound_without_eoj_decodes_cleanly() {
        // Decoding is a linear pass to the slice bound, so a job whose
        // instructions fill the buffer exactly is not a decode error even with
        // no terminating eoj. A PC that then runs past the last instruction is
        // the engine's `BadPc` concern at run time, not the decoder's.
        let code = [0x00, MODE_REGB_IMM8, 0x00, 0x2A];
        let ops = decode_job(&code).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].byte, 0x00);
        assert_eq!(ops[0].len, 4);
    }

    #[test]
    fn decode_continues_past_an_early_eoj() {
        // eoj (None/None), then a `move B0,#1` lying past it. Real jobs branch
        // over early-exit eojs, so the instruction after the first eoj must
        // still be decoded rather than dropped.
        let code = [EOJ, MODE_NONE_NONE, 0x00, MODE_REGB_IMM8, 0x00, 0x01];
        let ops = decode_job(&code).unwrap();
        assert_eq!(ops.len(), 2);
        assert_eq!(ops[1].offset, 2);
        assert_eq!(ops[1].byte, 0x00);
    }

    #[test]
    fn garbage_after_the_final_eoj_is_tolerated() {
        // Once an eoj has been decoded, a later decode error is trailing padding
        // and truncates the result there rather than failing the whole job.
        let code = [EOJ, MODE_NONE_NONE, 0xFF, 0xFF];
        let ops = decode_job(&code).unwrap();
        assert_eq!(ops.len(), 1);
        assert_eq!(ops[0].byte, EOJ);
        assert_eq!(ops[0].len, 2);
    }

    #[test]
    fn garbage_with_no_eoj_is_still_a_hard_error() {
        // Before any eoj a decode error is a real fault, never tolerated.
        let code = [0xFF, 0x00];
        assert!(matches!(
            decode_job(&code),
            Err(DecodeError::UnknownOpcode(0xFF))
        ));
    }

    #[test]
    fn decodes_imm16_little_endian() {
        let code = [0x00, MODE_IMM16_NONE, 0x34, 0x12, EOJ, MODE_NONE_NONE];
        let ops = decode_job(&code).unwrap();
        assert_eq!(ops[0].arg0, Operand::Imm(0x1234));
    }

    #[test]
    fn decodes_jump_imm32_as_unresolved_near_address() {
        // A jump's Imm32 arg0 is decoded verbatim; PC-relative resolution is the
        // executor's job, so the operand stays the raw little-endian value.
        let code = [
            0x0B,
            MODE_IMM32_NONE,
            0x0A,
            0x00,
            0x00,
            0x00,
            EOJ,
            MODE_NONE_NONE,
        ];
        let ops = decode_job(&code).unwrap();
        assert!(info(ops[0].byte).unwrap().is_jump);
        assert_eq!(ops[0].arg0, Operand::Imm(0x0A));
        assert_eq!(ops[0].len, 6);
    }

    #[test]
    fn decodes_indexed_imm_index_imm_len() {
        // IdxImmLenImm arg0: base register B3 (0x03), imm index 0x0010, imm len 0x0004.
        let code = [
            0x00,
            MODE_IDXIMMLENIMM_NONE,
            0x03,
            0x10,
            0x00,
            0x04,
            0x00,
            EOJ,
            MODE_NONE_NONE,
        ];
        let ops = decode_job(&code).unwrap();
        assert_eq!(
            ops[0].arg0,
            Operand::Indexed {
                base: RegId {
                    bank: RegBank::B,
                    idx: 3
                },
                index: IndexArg::Imm(0x10),
                len: Some(IndexArg::Imm(0x04)),
            }
        );
        assert_eq!(ops[0].len, 7);
    }

    #[test]
    fn decodes_string_literal() {
        // ImmStr arg0: u16 length 3, then bytes "ABC".
        let code = [
            0x00,
            MODE_STR_NONE,
            0x03,
            0x00,
            0x41,
            0x42,
            0x43,
            EOJ,
            MODE_NONE_NONE,
        ];
        let ops = decode_job(&code).unwrap();
        assert_eq!(ops[0].arg0, Operand::Str(vec![0x41, 0x42, 0x43]));
    }

    #[test]
    fn bad_register_selector_is_hard_error() {
        // Selector 0x40 falls in the invalid gap (0x34..0x7F) of the register table.
        let code = [0x00, MODE_REGB_NONE, 0x40];
        assert_eq!(decode_job(&code), Err(DecodeError::BadRegister(0x40)));
    }

    #[test]
    fn resolves_high_range_register_selector() {
        // Selector 0x80 maps to the A0 byte register (bank B, index 16).
        let code = [0x00, MODE_REGB_NONE, 0x80, EOJ, MODE_NONE_NONE];
        let ops = decode_job(&code).unwrap();
        assert_eq!(
            ops[0].arg0,
            Operand::Reg {
                bank: RegBank::B,
                idx: 16
            }
        );
    }
}
