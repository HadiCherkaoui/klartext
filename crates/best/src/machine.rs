//! The BEST/2 virtual machine's register/stack state and operand access.
//!
//! [`Machine`] is the mutable state a decoded job runs against: the five
//! register banks, the Z/S/C/V condition [`Flags`], the separate call and data
//! stacks, and the program counter. The executor (a later task) drives it by
//! mutating those fields in place and by moving [`Value`]s through
//! [`Machine::read`] and [`Machine::write`], which resolve a decoded
//! [`Operand`] to or from storage.
//!
//! ## Register banks
//! The banks mirror EDIABAS's machine model and the widths [`crate::decode`]
//! resolves register selectors into:
//!
//! | Bank | Storage        | Registers            | Semantics                 |
//! |------|----------------|----------------------|---------------------------|
//! | `B`  | `[u8; 32]`     | `B0..BF` + `A0..AF`  | 8-bit; writes truncate    |
//! | `I`  | `[u16; 16]`    | `I0..IF`             | 16-bit; writes truncate   |
//! | `L`  | `[u32; 8]`     | `L0..L7`             | 32-bit; writes truncate   |
//! | `S`  | `[Vec<u8>; 16]`| `S0..SF`             | variable-length byte buffer |
//! | `F`  | `[f64; 8]`     | `F0..F7`             | IEEE-754 double           |
//!
//! The `B` bank holds **32** slots, not 16: [`crate::decode`] resolves the
//! `A0..AF` byte registers into `B`'s upper half (indices `16..=31`), so both
//! sets of byte registers share this one array. Integer banks store their value
//! unsigned; interpreting a result as signed belongs to the result layer, not
//! to the raw register read.
//!
//! ## No degrade-to-raw
//! Every access is either total or a hard [`MachineError`]. Reading an
//! [`Operand::None`], writing an immediate or string literal, an out-of-range
//! register index, or a [`Value`] whose kind does not match its bank are all
//! errors — never a silent default or a guessed value. An [`Operand::Indexed`]
//! *read* slices its base `S` register (see [`Machine::read`]); the matching
//! *write* stores back into it (see [`Machine::write_indexed`]), growing and
//! zero-filling the buffer exactly as EDIABAS's `SetRawData` does.

use crate::decode::{IndexArg, Operand, RegBank, RegId};

/// EDIABAS's `ArrayMaxSize`: the largest valid `index + length` an indexed
/// `S`-register access may reach.
///
/// The reference derives it as `_arrayMaxBufSize - 1` (`1024 - 1`) — the last
/// addressable byte of a string buffer (EdiabasNet.cs:2504/2935). An indexed
/// read whose reach exceeds it is a bounds fault the reference reports as
/// `EDIABAS_BIP_0001`.
pub(crate) const ARRAY_MAX_SIZE: usize = 1023;

/// The mutable state one decoded BEST/2 job executes against.
///
/// Holds the five register banks, the condition [`Flags`], the call and data
/// stacks, and the program counter. Build a zeroed machine with
/// [`Machine::new`]; the executor mutates the fields in place and moves values
/// through [`Machine::read`] and [`Machine::write`].
#[derive(Debug, Clone)]
pub struct Machine {
    /// The 32-byte integer register file EDIABAS's `B`/`I`/`L` banks all view
    /// (`_byteRegisters = new byte[32]`, EdiabasNet.cs:3216). The banks OVERLAP,
    /// little-endian: `B<n>` is byte `n`; `I<n>` is bytes `2n..2n+2`; `L<n>` is
    /// bytes `4n..4n+4` (Register.GetValueData/SetRawData, EdiabasNet.cs:1682/1789).
    /// So `L0`'s low byte is `B0`, `I0` is `B0`+`B1`, etc. — a job routinely
    /// writes a wide register and reads its bytes back through a narrow one.
    pub(crate) regs: [u8; 32],
    /// String / byte-buffer registers `S0..SF`.
    pub(crate) s: [Vec<u8>; 16],
    /// IEEE-754 double registers `F0..F7`.
    pub(crate) f: [f64; 8],
    /// The Z/S/C/V condition flags.
    pub(crate) flags: Flags,
    /// Return addresses pushed by call instructions.
    #[expect(
        dead_code,
        reason = "driven by the executor's call/return ops in a later task"
    )]
    pub(crate) call_stack: Vec<usize>,
    /// The data stack: `push`/`pop` move a register's `length` bytes here
    /// little-endian, so EDIABAS models it as a byte stack rather than one of
    /// widened values.
    pub(crate) data_stack: Vec<u8>,
    /// The program counter: byte offset of the next instruction to execute.
    pub(crate) pc: usize,
    /// EDIABAS's error-trap bit (`_errorTrapBitNr`, EdiabasNet.cs:2506): `None`
    /// mirrors the reference's `-1` (no error recorded). A comm/bounds fault
    /// records its dictionary bit here (via the executor's `set_error`) and
    /// `jt`/`jnt` branch on it.
    pub(crate) trap_bit: Option<u32>,
    /// EDIABAS's `_errorTrapMask` (EdiabasNet.cs:2505): a set bit suppresses the
    /// hard abort for that error class, letting the job handle the fault itself
    /// via `jt`. The `gettmr`/`settmr` opcodes read/write this mask — despite the
    /// misleading "timer" mnemonics they move the trap *mask*, not a clock
    /// (EdOperations.cs:1279, 2130).
    pub(crate) trap_mask: u32,
}

/// The BEST/2 condition flags, set by arithmetic and comparison opcodes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Flags {
    /// Zero flag: the last result was zero.
    pub z: bool,
    /// Sign flag: the last result was negative.
    pub s: bool,
    /// Carry flag: the last operation carried or borrowed.
    pub c: bool,
    /// Overflow flag: the last signed operation overflowed.
    pub v: bool,
}

/// A value moved between an [`Operand`] and the machine's storage.
///
/// Integer banks (`B`/`I`/`L`) and immediates carry [`Value::Int`], the float
/// bank carries [`Value::Float`], and `S` registers and string literals carry
/// [`Value::Bytes`].
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// An integer widened to `i64`; register reads yield the unsigned value.
    Int(i64),
    /// An IEEE-754 double, read from or written to an `F` register.
    Float(f64),
    /// A byte buffer, from or to an `S` register or a string literal.
    Bytes(Vec<u8>),
}

/// An error from resolving an [`Operand`] against the machine's state.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MachineError {
    /// A register index past the end of its bank's backing array.
    #[error("register index {idx} out of range for bank {bank:?}")]
    OutOfRange {
        /// The bank whose index was out of range.
        bank: RegBank,
        /// The offending zero-based index.
        idx: u8,
    },
    /// An indexed `S`-register access whose `index + length` exceeds
    /// [`ARRAY_MAX_SIZE`] — EDIABAS's `ArrayMaxSize` bounds check on reads
    /// (EdiabasNet.cs:283/349) and writes (EdiabasNet.cs:536-541). Surfaced
    /// here as a value error; the executor converts it to
    /// `SetError(EDIABAS_BIP_0001)` plus an empty array (reads) or a skipped
    /// store (writes).
    #[error("indexed access out of bounds: index {index} + length {len} exceeds ARRAY_MAX_SIZE")]
    IndexOutOfBounds {
        /// The resolved index into the `S` buffer.
        index: usize,
        /// The accessed length (`0` for a no-length to-end read).
        len: usize,
    },
    /// An operand that cannot be evaluated as requested in this context.
    #[error("unsupported operand access: {0}")]
    Unsupported(String),
}

impl Machine {
    /// Creates a machine with all registers, flags, stacks, and PC zeroed.
    pub fn new() -> Self {
        Self {
            regs: [0; 32],
            s: std::array::from_fn(|_| Vec::new()),
            f: [0.0; 8],
            flags: Flags::default(),
            call_stack: Vec::new(),
            data_stack: Vec::new(),
            pc: 0,
            trap_bit: None,
            trap_mask: 0,
        }
    }

    /// Reads the [`Value`] an operand resolves to.
    ///
    /// A register yields its stored value (`B`/`I`/`L` as an unsigned
    /// [`Value::Int`], `F` as [`Value::Float`], `S` as [`Value::Bytes`]); an
    /// immediate yields its [`Value::Int`]; a string literal yields its
    /// [`Value::Bytes`].
    ///
    /// An [`Operand::Indexed`] slices its base `S` register into [`Value::Bytes`]:
    /// `len: None` yields the used bytes from `index` to the buffer's end (empty
    /// once `index` reaches the used length), and `len: Some(n)` yields exactly
    /// `n` bytes, zero-extended past the used length.
    ///
    /// # Errors
    /// Returns [`MachineError::OutOfRange`] for a register index past its bank,
    /// [`MachineError::IndexOutOfBounds`] when an indexed read's `index + length`
    /// exceeds [`ARRAY_MAX_SIZE`], and [`MachineError::Unsupported`] for
    /// [`Operand::None`] (nothing to read) or an [`Operand::Indexed`] whose base
    /// is not the `S` bank.
    pub fn read(&self, op: &Operand) -> Result<Value, MachineError> {
        match op {
            Operand::Imm(v) => Ok(Value::Int(*v)),
            Operand::Str(bytes) => Ok(Value::Bytes(bytes.clone())),
            Operand::Reg { bank, idx } => self.read_reg(*bank, *idx),
            Operand::None => Err(MachineError::Unsupported(
                "operand `None` has no value to read".to_string(),
            )),
            // An indexed read slices the base `S` register. EDIABAS only ever
            // indexes the string bank in the executed subset, so any other base
            // bank is a loud `Unsupported` rather than a guess.
            //
            // EDIABAS's no-len indexed read slices the COMPLETE 1024-byte backing
            // buffer (GetData(true), EdiabasNet.cs:1550-1559), so its tail is zeros
            // beyond the used length. We slice the used bytes only: every prefix
            // consumer (little-endian value reads, NUL-terminated string reads)
            // behaves identically, and a job that stored the zero-tail through a
            // plain register write would poison later xsend requests with a 1 KiB
            // buffer. The len-variant zero-extends exactly like the reference. If a
            // capture ever shows a job depending on the zero-tail of a no-len
            // slice, revisit (spec §10).
            Operand::Indexed { base, index, len } => {
                let data = match base.bank {
                    RegBank::S => {
                        self.s
                            .get(usize::from(base.idx))
                            .ok_or(MachineError::OutOfRange {
                                bank: RegBank::S,
                                idx: base.idx,
                            })?
                    }
                    other => {
                        return Err(MachineError::Unsupported(format!(
                            "indexed access on a {other:?} register is not part of the executed subset"
                        )));
                    }
                };
                let idx = self.resolve_index(index)?;
                match len {
                    None => {
                        if idx + 1 > ARRAY_MAX_SIZE {
                            return Err(MachineError::IndexOutOfBounds { index: idx, len: 0 });
                        }
                        Ok(Value::Bytes(
                            data.get(idx..).map(<[u8]>::to_vec).unwrap_or_default(),
                        ))
                    }
                    Some(l) => {
                        let l = self.resolve_index(l)?;
                        if idx + l > ARRAY_MAX_SIZE {
                            return Err(MachineError::IndexOutOfBounds { index: idx, len: l });
                        }
                        let mut out = vec![0u8; l];
                        if idx < data.len() {
                            let take = (data.len() - idx).min(l);
                            out[..take].copy_from_slice(&data[idx..idx + take]);
                        }
                        Ok(Value::Bytes(out))
                    }
                }
            }
        }
    }

    /// Writes `value` to the location an operand resolves to.
    ///
    /// Register writes coerce by bank: `B`/`I`/`L` take a [`Value::Int`] and
    /// truncate it to 8/16/32 bits, `F` takes a [`Value::Float`], and `S` takes
    /// [`Value::Bytes`]. Immediates and string literals are read-only.
    ///
    /// An [`Operand::Indexed`] target delegates to [`Machine::write_indexed`]
    /// with `len = 1`: EDIABAS's plain `SetRawData(data)` defaults `dataLen` to
    /// 1 (EdiabasNet.cs:441-444), so an integer written through an index with no
    /// caller-supplied width stores exactly its low byte, while a byte buffer
    /// carries its own length regardless of `len`. An opcode that knows a width
    /// (`move`'s integer path, `mult`/`divs`' high-word/remainder) calls
    /// [`Machine::write_indexed`] directly instead.
    ///
    /// # Errors
    /// Returns [`MachineError::OutOfRange`] for a register index past its bank,
    /// [`MachineError::IndexOutOfBounds`] when an indexed write's reach exceeds
    /// [`ARRAY_MAX_SIZE`], and [`MachineError::Unsupported`] when the target is
    /// not writable ([`Operand::None`], [`Operand::Imm`], [`Operand::Str`]) or
    /// when `value`'s kind does not match the destination.
    pub fn write(&mut self, op: &Operand, value: Value) -> Result<(), MachineError> {
        match op {
            Operand::Reg { bank, idx } => self.write_reg(*bank, *idx, value),
            Operand::None => Err(MachineError::Unsupported(
                "operand `None` is not a writable location".to_string(),
            )),
            Operand::Imm(_) => Err(MachineError::Unsupported(
                "cannot write to an immediate operand".to_string(),
            )),
            Operand::Str(_) => Err(MachineError::Unsupported(
                "cannot write to a string-literal operand".to_string(),
            )),
            Operand::Indexed { .. } => self.write_indexed(op, &value, 1),
        }
    }

    /// Writes `value` through an indexed `S`-register operand — EDIABAS's
    /// indexed `SetRawData` (EdiabasNet.cs:470-556).
    ///
    /// A [`Value::Int`] serializes to `len` **little-endian** bytes
    /// (`sourceArray[i] = value >> (8 * i)`, EdiabasNet.cs:520-529); a
    /// [`Value::Bytes`] writes all its own bytes and ignores `len`
    /// (EdiabasNet.cs:530-534). The used data grows to `index + n` when shorter,
    /// zero-filling the gap between the old used length and `index`
    /// (`Array.Resize`, EdiabasNet.cs:542-545), and never shrinks — bytes beyond
    /// the written span survive. The operand's own trailing `len` sub-operand is
    /// ignored: after the executor's `IdxRegImm` fold it can only be a
    /// `…Len…`-mode length, which sizes *reads* — the reference's `SetRawData`
    /// does not accept the `…Len…` modes at all (its address-mode switch ends at
    /// `IdxRegImm`, EdiabasNet.cs:474-476).
    ///
    /// # Errors
    /// Returns [`MachineError::IndexOutOfBounds`] when `index + n` exceeds
    /// [`ARRAY_MAX_SIZE`] — the reference records `EDIABAS_BIP_0001` and skips
    /// the store (EdiabasNet.cs:536-541); the executor converts this error the
    /// same way. Returns [`MachineError::OutOfRange`] for a base register index
    /// past the bank, and [`MachineError::Unsupported`] for a non-indexed
    /// operand, a non-`S` base bank, or a [`Value::Float`] (the reference's
    /// `SetRawData` accepts only an integer or a byte array,
    /// EdiabasNet.cs:477-480).
    pub(crate) fn write_indexed(
        &mut self,
        op: &Operand,
        value: &Value,
        len: usize,
    ) -> Result<(), MachineError> {
        let Operand::Indexed {
            base,
            index,
            len: _,
        } = op
        else {
            return Err(MachineError::Unsupported(
                "write_indexed on a non-indexed operand".to_string(),
            ));
        };
        if base.bank != RegBank::S {
            return Err(MachineError::Unsupported(format!(
                "indexed access on a {:?} register is not part of the executed subset",
                base.bank
            )));
        }
        let bytes: Vec<u8> = match value {
            Value::Int(v) => (0..len).map(|i| (*v >> (i * 8)) as u8).collect(),
            Value::Bytes(b) => b.clone(),
            Value::Float(_) => {
                return Err(MachineError::Unsupported(
                    "cannot write a float through an indexed operand".to_string(),
                ));
            }
        };
        let idx = self.resolve_index(index)?;
        if idx + bytes.len() > ARRAY_MAX_SIZE {
            return Err(MachineError::IndexOutOfBounds {
                index: idx,
                len: bytes.len(),
            });
        }
        let data = self
            .s
            .get_mut(usize::from(base.idx))
            .ok_or(MachineError::OutOfRange {
                bank: RegBank::S,
                idx: base.idx,
            })?;
        if data.len() < idx + bytes.len() {
            data.resize(idx + bytes.len(), 0);
        }
        data[idx..idx + bytes.len()].copy_from_slice(&bytes);
        Ok(())
    }

    /// Byte-reverses `[start, start + len)` of an `S` register, preserving the
    /// used length — the storage half of `swap` (0x51).
    ///
    /// EDIABAS's `OpSwap` reverses the slice on the COMPLETE 1024-byte backing
    /// buffer (`GetArrayData(true)`) and stores it back with `keepLength = true`
    /// (EdOperations.cs:2406-2425; `StringData.SetData`,
    /// EdiabasNet.cs:1561-1572), so the register's used LENGTH never changes —
    /// but a slice overrunning the used bytes reverses the buffer's zero tail
    /// INTO the used range. Reproduced on this machine's used-bytes model by
    /// extending a copy with zeros to `start + len`, reversing the slice, and
    /// truncating back to the original used length — observably identical under
    /// the crate's zeros-beyond-used-length model (see [`Machine::read`]'s
    /// indexed-read note).
    ///
    /// The caller performs EDIABAS's `ArrayMaxSize` bounds check (and its
    /// `SetError` conversion) before calling, as `OpSwap` does before its
    /// `Array.Reverse`; `start + len` is trusted here.
    ///
    /// # Errors
    /// Returns [`MachineError::Unsupported`] for a non-`S` base bank and
    /// [`MachineError::OutOfRange`] for a base register index past the bank.
    pub(crate) fn swap_s_slice(
        &mut self,
        base: &RegId,
        start: usize,
        len: usize,
    ) -> Result<(), MachineError> {
        if base.bank != RegBank::S {
            return Err(MachineError::Unsupported(format!(
                "indexed access on a {:?} register is not part of the executed subset",
                base.bank
            )));
        }
        let data = self
            .s
            .get_mut(usize::from(base.idx))
            .ok_or(MachineError::OutOfRange {
                bank: RegBank::S,
                idx: base.idx,
            })?;
        let used = data.len();
        if data.len() < start + len {
            data.resize(start + len, 0);
        }
        data[start..start + len].reverse();
        data.truncate(used);
        Ok(())
    }

    /// Reads register `idx` of `bank`, widening integer banks unsigned.
    ///
    /// # Errors
    /// Returns [`MachineError::OutOfRange`] if `idx` is past the bank's array.
    fn read_reg(&self, bank: RegBank, idx: u8) -> Result<Value, MachineError> {
        let n = usize::from(idx);
        let oor = MachineError::OutOfRange { bank, idx };
        let value = match bank {
            RegBank::B => Value::Int(i64::from(*self.regs.get(n).ok_or(oor)?)),
            RegBank::I => {
                let o = n << 1;
                let b = self.regs.get(o..o + 2).ok_or(oor)?;
                Value::Int(i64::from(u16::from_le_bytes([b[0], b[1]])))
            }
            RegBank::L => {
                let o = n << 2;
                let b = self.regs.get(o..o + 4).ok_or(oor)?;
                Value::Int(i64::from(u32::from_le_bytes([b[0], b[1], b[2], b[3]])))
            }
            RegBank::S => Value::Bytes(self.s.get(n).ok_or(oor)?.clone()),
            RegBank::F => Value::Float(*self.f.get(n).ok_or(oor)?),
        };
        Ok(value)
    }

    /// Resolves an indexed operand's index (or length) sub-operand to a `usize`.
    ///
    /// An [`IndexArg::Imm`] yields its stored value; an [`IndexArg::Reg`] yields
    /// the referenced integer register's value. This is the index/length
    /// recovery EDIABAS performs before an indexed fetch (EdiabasNet.cs:255-270).
    ///
    /// # Errors
    /// Returns [`MachineError::OutOfRange`] for a register index past its bank,
    /// and [`MachineError::Unsupported`] if the sub-operand names a non-integer
    /// register (an index must be a `B`/`I`/`L` value, never bytes or a float).
    pub(crate) fn resolve_index(&self, arg: &IndexArg) -> Result<usize, MachineError> {
        match arg {
            IndexArg::Imm(v) => Ok(*v as usize),
            IndexArg::Reg(RegId { bank, idx }) => match self.read_reg(*bank, *idx)? {
                Value::Int(v) => Ok(v as usize),
                _ => Err(MachineError::Unsupported(
                    "an indexed operand's index register must be an integer bank".to_string(),
                )),
            },
        }
    }

    /// Writes `value` into register `idx` of `bank`, truncating integer banks.
    ///
    /// # Errors
    /// Returns [`MachineError::OutOfRange`] if `idx` is past the bank's array,
    /// and [`MachineError::Unsupported`] if `value`'s kind does not match the
    /// bank (e.g. a [`Value::Float`] into an integer bank).
    fn write_reg(&mut self, bank: RegBank, idx: u8, value: Value) -> Result<(), MachineError> {
        let n = usize::from(idx);
        let oor = MachineError::OutOfRange { bank, idx };
        match (bank, value) {
            // Integer banks store unsigned, little-endian, into the shared file.
            (RegBank::B, Value::Int(v)) => *self.regs.get_mut(n).ok_or(oor)? = v as u8,
            (RegBank::I, Value::Int(v)) => {
                let o = n << 1;
                self.regs
                    .get_mut(o..o + 2)
                    .ok_or(oor)?
                    .copy_from_slice(&(v as u16).to_le_bytes());
            }
            (RegBank::L, Value::Int(v)) => {
                let o = n << 2;
                self.regs
                    .get_mut(o..o + 4)
                    .ok_or(oor)?
                    .copy_from_slice(&(v as u32).to_le_bytes());
            }
            (RegBank::S, Value::Bytes(bytes)) => *self.s.get_mut(n).ok_or(oor)? = bytes,
            (RegBank::F, Value::Float(f)) => *self.f.get_mut(n).ok_or(oor)? = f,
            (bank, value) => {
                return Err(MachineError::Unsupported(format!(
                    "cannot write {value:?} to a {bank:?} register"
                )));
            }
        }
        Ok(())
    }
}

impl Default for Machine {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::{IndexArg, RegId};

    /// Builds a single-register operand for the given bank and index.
    fn reg(bank: RegBank, idx: u8) -> Operand {
        Operand::Reg { bank, idx }
    }

    /// An `S`-register operand `S<idx>` (a string/byte buffer).
    fn s_reg(idx: u8) -> Operand {
        Operand::Reg {
            bank: RegBank::S,
            idx,
        }
    }

    /// The [`RegId`] naming `S<idx>` — the base of an indexed operand.
    fn s_reg_id(idx: u8) -> RegId {
        RegId {
            bank: RegBank::S,
            idx,
        }
    }

    /// A `B`-register operand `B<idx>` (an 8-bit integer register).
    fn b_reg(idx: u8) -> Operand {
        Operand::Reg {
            bank: RegBank::B,
            idx,
        }
    }

    /// The [`RegId`] naming `B<idx>` — an integer index register.
    fn b_reg_id(idx: u8) -> RegId {
        RegId {
            bank: RegBank::B,
            idx,
        }
    }

    #[test]
    fn reg_write_then_read_roundtrips() {
        let mut m = Machine::new();
        m.write(&reg(RegBank::I, 3), Value::Int(0x0E2F)).unwrap();
        assert_eq!(m.read(&reg(RegBank::I, 3)).unwrap(), Value::Int(0x0E2F));
    }

    #[test]
    fn b_register_truncates_to_u8() {
        let mut m = Machine::new();
        m.write(&reg(RegBank::B, 0), Value::Int(0x1FF)).unwrap();
        assert_eq!(m.read(&reg(RegBank::B, 0)).unwrap(), Value::Int(0xFF));
    }

    #[test]
    fn i_register_truncates_to_u16() {
        let mut m = Machine::new();
        m.write(&reg(RegBank::I, 0), Value::Int(0x1_0000 | 0x1234))
            .unwrap();
        assert_eq!(m.read(&reg(RegBank::I, 0)).unwrap(), Value::Int(0x1234));
    }

    #[test]
    fn l_register_roundtrips_u32() {
        let mut m = Machine::new();
        m.write(&reg(RegBank::L, 7), Value::Int(0xDEAD_BEEF))
            .unwrap();
        assert_eq!(
            m.read(&reg(RegBank::L, 7)).unwrap(),
            Value::Int(0xDEAD_BEEF)
        );
    }

    #[test]
    fn integer_banks_overlap_in_one_little_endian_file() {
        // EDIABAS's `B`/`I`/`L` banks all view one `_byteRegisters[32]` array,
        // little-endian: writing `L0` is visible through `I0`/`I1` and `B0..B3`.
        // The generic framework relies on this (e.g. it builds a 2-byte length by
        // writing two `B` bytes and reading the `I`, and the response length-check
        // adds a header size into a wide register through its low byte).
        let mut m = Machine::new();
        m.write(&reg(RegBank::L, 0), Value::Int(0x0403_0201))
            .unwrap();
        assert_eq!(m.read(&reg(RegBank::B, 0)).unwrap(), Value::Int(0x01));
        assert_eq!(m.read(&reg(RegBank::B, 3)).unwrap(), Value::Int(0x04));
        assert_eq!(m.read(&reg(RegBank::I, 0)).unwrap(), Value::Int(0x0201));
        assert_eq!(m.read(&reg(RegBank::I, 1)).unwrap(), Value::Int(0x0403));
        // …and the reverse: two byte writes compose into the overlapping word.
        m.write(&reg(RegBank::B, 4), Value::Int(0xAA)).unwrap();
        m.write(&reg(RegBank::B, 5), Value::Int(0xBB)).unwrap();
        assert_eq!(m.read(&reg(RegBank::I, 2)).unwrap(), Value::Int(0xBBAA));
        // `L1` (bytes 4..8) sees those same low bytes; the high half stays zero.
        assert_eq!(m.read(&reg(RegBank::L, 1)).unwrap(), Value::Int(0xBBAA));
    }

    #[test]
    fn f_register_roundtrips_float() {
        let mut m = Machine::new();
        m.write(&reg(RegBank::F, 2), Value::Float(-12.5)).unwrap();
        assert_eq!(m.read(&reg(RegBank::F, 2)).unwrap(), Value::Float(-12.5));
    }

    #[test]
    fn s_register_roundtrips_bytes() {
        let mut m = Machine::new();
        m.write(&reg(RegBank::S, 5), Value::Bytes(vec![1, 2, 3]))
            .unwrap();
        assert_eq!(
            m.read(&reg(RegBank::S, 5)).unwrap(),
            Value::Bytes(vec![1, 2, 3])
        );
    }

    #[test]
    fn high_index_byte_register_roundtrips() {
        // Selector 0x80 (A0) resolves to bank B, index 16 in the decoder, so
        // the B array must hold 32 slots — a 16-slot array would panic here.
        let mut m = Machine::new();
        m.write(&reg(RegBank::B, 16), Value::Int(0x7A)).unwrap();
        assert_eq!(m.read(&reg(RegBank::B, 16)).unwrap(), Value::Int(0x7A));
    }

    #[test]
    fn new_zeroes_registers() {
        // The register banks, the condition flags, the program counter, and the
        // data stack all start cleared; `call_stack` stays deferred to the
        // call/return task, so it is not asserted here.
        let m = Machine::new();
        assert_eq!(m.read(&reg(RegBank::B, 0)).unwrap(), Value::Int(0));
        assert_eq!(m.read(&reg(RegBank::I, 0)).unwrap(), Value::Int(0));
        assert_eq!(m.read(&reg(RegBank::L, 0)).unwrap(), Value::Int(0));
        assert_eq!(m.read(&reg(RegBank::F, 0)).unwrap(), Value::Float(0.0));
        assert_eq!(m.read(&reg(RegBank::S, 0)).unwrap(), Value::Bytes(vec![]));
        assert_eq!(m.flags, Flags::default());
        assert!(!m.flags.z);
        assert!(!m.flags.s);
        assert!(!m.flags.c);
        assert!(!m.flags.v);
        assert_eq!(m.pc, 0);
        assert!(m.data_stack.is_empty());
        // The error-trap state starts cleared: no error recorded (EDIABAS's -1)
        // and an all-zero mask (no error class suppressed).
        assert_eq!(m.trap_bit, None);
        assert_eq!(m.trap_mask, 0);
    }

    #[test]
    fn reads_immediate_operand() {
        let m = Machine::new();
        assert_eq!(m.read(&Operand::Imm(0x42)).unwrap(), Value::Int(0x42));
    }

    #[test]
    fn reads_string_literal_operand() {
        let m = Machine::new();
        assert_eq!(
            m.read(&Operand::Str(vec![0x41, 0x42])).unwrap(),
            Value::Bytes(vec![0x41, 0x42])
        );
    }

    #[test]
    fn writing_immediate_is_an_error() {
        let mut m = Machine::new();
        assert!(matches!(
            m.write(&Operand::Imm(1), Value::Int(1)),
            Err(MachineError::Unsupported(_))
        ));
    }

    #[test]
    fn writing_string_literal_is_an_error() {
        let mut m = Machine::new();
        assert!(matches!(
            m.write(&Operand::Str(vec![1]), Value::Bytes(vec![1])),
            Err(MachineError::Unsupported(_))
        ));
    }

    #[test]
    fn reading_none_operand_is_an_error() {
        let m = Machine::new();
        assert!(matches!(
            m.read(&Operand::None),
            Err(MachineError::Unsupported(_))
        ));
    }

    #[test]
    fn indexed_read_slices_the_used_bytes_to_the_end() {
        let mut m = Machine::new();
        m.write(&s_reg(1), Value::Bytes(vec![0xAA, 0xBB, 0xCC, 0xDD]))
            .unwrap();
        let op = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Imm(1),
            len: None,
        };
        assert_eq!(m.read(&op).unwrap(), Value::Bytes(vec![0xBB, 0xCC, 0xDD]));
    }

    #[test]
    fn indexed_read_past_the_used_length_is_empty() {
        let mut m = Machine::new();
        m.write(&s_reg(1), Value::Bytes(vec![0xAA])).unwrap();
        let op = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Imm(5),
            len: None,
        };
        assert_eq!(m.read(&op).unwrap(), Value::Bytes(vec![]));
    }

    #[test]
    fn indexed_len_read_zero_extends_like_the_reference_buffer() {
        let mut m = Machine::new();
        m.write(&s_reg(1), Value::Bytes(vec![0xAA, 0xBB])).unwrap();
        let op = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Imm(1),
            len: Some(IndexArg::Imm(4)),
        };
        assert_eq!(m.read(&op).unwrap(), Value::Bytes(vec![0xBB, 0, 0, 0]));
    }

    #[test]
    fn indexed_read_index_comes_from_a_register() {
        let mut m = Machine::new();
        m.write(&s_reg(1), Value::Bytes(vec![1, 2, 3])).unwrap();
        m.write(&b_reg(0), Value::Int(2)).unwrap();
        let op = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Reg(b_reg_id(0)),
            len: None,
        };
        assert_eq!(m.read(&op).unwrap(), Value::Bytes(vec![3]));
    }

    #[test]
    fn indexed_read_beyond_array_max_is_a_bounds_fault() {
        let m = Machine::new();
        let op = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Imm(1024),
            len: None,
        };
        assert!(matches!(
            m.read(&op),
            Err(MachineError::IndexOutOfBounds { .. })
        ));
    }

    #[test]
    fn indexed_write_extends_and_zero_fills() {
        let mut m = Machine::new();
        m.write(&s_reg(1), Value::Bytes(vec![0xAA])).unwrap();
        let dest = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Imm(3),
            len: None,
        };
        m.write_indexed(&dest, &Value::Int(0x1234), 2).unwrap();
        // Gap zero-filled, value little-endian (EdiabasNet.cs:520-529).
        assert_eq!(
            m.read(&s_reg(1)).unwrap(),
            Value::Bytes(vec![0xAA, 0x00, 0x00, 0x34, 0x12])
        );
    }

    #[test]
    fn indexed_write_of_bytes_uses_their_own_length() {
        let mut m = Machine::new();
        let dest = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Imm(0),
            len: None,
        };
        m.write_indexed(&dest, &Value::Bytes(vec![1, 2, 3]), 1)
            .unwrap();
        assert_eq!(m.read(&s_reg(1)).unwrap(), Value::Bytes(vec![1, 2, 3]));
    }

    #[test]
    fn indexed_write_into_the_middle_keeps_the_tail() {
        // The reference resizes the used array only upward, then stores the whole
        // array back (EdiabasNet.cs:532-544): a short write into the middle leaves
        // the bytes beyond it untouched — it never shrinks the used length.
        let mut m = Machine::new();
        m.write(&s_reg(1), Value::Bytes(vec![1, 2, 3, 4, 5]))
            .unwrap();
        let dest = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Imm(1),
            len: None,
        };
        m.write_indexed(&dest, &Value::Int(0xEE), 1).unwrap();
        assert_eq!(
            m.read(&s_reg(1)).unwrap(),
            Value::Bytes(vec![1, 0xEE, 3, 4, 5])
        );
    }

    #[test]
    fn indexed_write_past_array_max_is_a_bounds_fault() {
        // index 1023 + 1 byte = 1024 > ARRAY_MAX_SIZE (1023): the reference records
        // EDIABAS_BIP_0001 and skips (EdiabasNet.cs:536-541); here that surfaces as
        // the same bounds error the read side raises.
        let mut m = Machine::new();
        let dest = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Imm(1023),
            len: None,
        };
        assert!(matches!(
            m.write_indexed(&dest, &Value::Int(0xFF), 1),
            Err(MachineError::IndexOutOfBounds { .. })
        ));
    }

    #[test]
    fn write_arm_delegates_indexed_int_as_one_byte() {
        // Plain `write` of an Int through an indexed operand uses len = 1 — the
        // default `dataLen` of EDIABAS's one-arg `SetRawData(data)`
        // (EdiabasNet.cs:441-444) — so only the low byte lands.
        let mut m = Machine::new();
        let dest = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Imm(0),
            len: None,
        };
        m.write(&dest, Value::Int(0x1234)).unwrap();
        assert_eq!(m.read(&s_reg(1)).unwrap(), Value::Bytes(vec![0x34]));
    }

    #[test]
    fn write_arm_delegates_indexed_bytes_by_own_length() {
        let mut m = Machine::new();
        let dest = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Imm(0),
            len: None,
        };
        m.write(&dest, Value::Bytes(vec![0xDE, 0xAD])).unwrap();
        assert_eq!(m.read(&s_reg(1)).unwrap(), Value::Bytes(vec![0xDE, 0xAD]));
    }

    #[test]
    fn indexed_write_of_a_float_is_unsupported() {
        // A byte buffer position cannot hold a float; the reference has no such
        // path (SetRawData takes only an int or a byte[]).
        let mut m = Machine::new();
        let dest = Operand::Indexed {
            base: s_reg_id(1),
            index: IndexArg::Imm(0),
            len: None,
        };
        assert!(matches!(
            m.write_indexed(&dest, &Value::Float(1.0), 1),
            Err(MachineError::Unsupported(_))
        ));
    }

    #[test]
    fn indexed_write_on_a_non_s_bank_is_unsupported() {
        // EDIABAS only indexes the string bank, so an indexed write to any other
        // base is a loud error, mirroring the read side.
        let mut m = Machine::new();
        let dest = Operand::Indexed {
            base: b_reg_id(0),
            index: IndexArg::Imm(0),
            len: None,
        };
        assert!(matches!(
            m.write_indexed(&dest, &Value::Bytes(vec![1]), 1),
            Err(MachineError::Unsupported(_))
        ));
    }

    #[test]
    fn indexed_read_on_a_non_s_bank_is_unsupported() {
        // EDIABAS only indexes the string bank in the executed subset, so an
        // indexed base in any other bank is a loud error, not a guess.
        let m = Machine::new();
        let indexed = Operand::Indexed {
            base: b_reg_id(0),
            index: IndexArg::Imm(0),
            len: None,
        };
        assert!(matches!(
            m.read(&indexed),
            Err(MachineError::Unsupported(_))
        ));
    }

    #[test]
    fn out_of_range_register_index_is_an_error() {
        let mut m = Machine::new();
        assert!(matches!(
            m.read(&reg(RegBank::L, 8)),
            Err(MachineError::OutOfRange {
                bank: RegBank::L,
                idx: 8
            })
        ));
        assert!(matches!(
            m.write(&reg(RegBank::B, 200), Value::Int(0)),
            Err(MachineError::OutOfRange {
                bank: RegBank::B,
                idx: 200
            })
        ));
    }

    #[test]
    fn writing_wrong_value_kind_to_register_is_an_error() {
        let mut m = Machine::new();
        // A float into an integer bank is a type error, not a silent coercion.
        assert!(matches!(
            m.write(&reg(RegBank::I, 0), Value::Float(1.0)),
            Err(MachineError::Unsupported(_))
        ));
    }
}
