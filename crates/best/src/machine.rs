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
//! errors — never a silent default or a guessed value. [`Operand::Indexed`]
//! access is not yet wired and returns an error until a later task implements
//! the `S`-register slicing it needs.

use crate::decode::{Operand, RegBank};

/// The mutable state one decoded BEST/2 job executes against.
///
/// Holds the five register banks, the condition [`Flags`], the call and data
/// stacks, and the program counter. Build a zeroed machine with
/// [`Machine::new`]; the executor mutates the fields in place and moves values
/// through [`Machine::read`] and [`Machine::write`].
#[derive(Debug, Clone)]
pub struct Machine {
    /// Byte registers `B0..BF` (`0..=15`) and `A0..AF` (`16..=31`); writes truncate to `u8`.
    pub(crate) b: [u8; 32],
    /// 16-bit word registers `I0..IF`.
    pub(crate) i: [u16; 16],
    /// 32-bit long registers `L0..L7`.
    pub(crate) l: [u32; 8],
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
    /// An operand that cannot be evaluated as requested in this context.
    #[error("unsupported operand access: {0}")]
    Unsupported(String),
}

impl Machine {
    /// Creates a machine with all registers, flags, stacks, and PC zeroed.
    pub fn new() -> Self {
        Self {
            b: [0; 32],
            i: [0; 16],
            l: [0; 8],
            s: std::array::from_fn(|_| Vec::new()),
            f: [0.0; 8],
            flags: Flags::default(),
            call_stack: Vec::new(),
            data_stack: Vec::new(),
            pc: 0,
        }
    }

    /// Reads the [`Value`] an operand resolves to.
    ///
    /// A register yields its stored value (`B`/`I`/`L` as an unsigned
    /// [`Value::Int`], `F` as [`Value::Float`], `S` as [`Value::Bytes`]); an
    /// immediate yields its [`Value::Int`]; a string literal yields its
    /// [`Value::Bytes`].
    ///
    /// # Errors
    /// Returns [`MachineError::OutOfRange`] for a register index past its bank,
    /// and [`MachineError::Unsupported`] for [`Operand::None`] (nothing to read)
    /// or an [`Operand::Indexed`] (not yet wired).
    pub fn read(&self, op: &Operand) -> Result<Value, MachineError> {
        match op {
            Operand::Imm(v) => Ok(Value::Int(*v)),
            Operand::Str(bytes) => Ok(Value::Bytes(bytes.clone())),
            Operand::Reg { bank, idx } => self.read_reg(*bank, *idx),
            Operand::None => Err(MachineError::Unsupported(
                "operand `None` has no value to read".to_string(),
            )),
            // Generic Indexed operand access (S-register slicing) is deferred to
            // Phase 2; string/table ops that need an index route around it.
            Operand::Indexed { .. } => Err(MachineError::Unsupported(
                "indexed operand access is not yet implemented".to_string(),
            )),
        }
    }

    /// Writes `value` to the location an operand resolves to.
    ///
    /// Register writes coerce by bank: `B`/`I`/`L` take a [`Value::Int`] and
    /// truncate it to 8/16/32 bits, `F` takes a [`Value::Float`], and `S` takes
    /// [`Value::Bytes`]. Immediates and string literals are read-only.
    ///
    /// # Errors
    /// Returns [`MachineError::OutOfRange`] for a register index past its bank,
    /// and [`MachineError::Unsupported`] when the target is not writable
    /// ([`Operand::None`], [`Operand::Imm`], [`Operand::Str`], or a not-yet-wired
    /// [`Operand::Indexed`]) or when `value`'s kind does not match the bank.
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
            // Generic Indexed operand access (S-register slicing) is deferred to
            // Phase 2; string/table ops that need an index route around it.
            Operand::Indexed { .. } => Err(MachineError::Unsupported(
                "indexed operand access is not yet implemented".to_string(),
            )),
        }
    }

    /// Reads register `idx` of `bank`, widening integer banks unsigned.
    ///
    /// # Errors
    /// Returns [`MachineError::OutOfRange`] if `idx` is past the bank's array.
    fn read_reg(&self, bank: RegBank, idx: u8) -> Result<Value, MachineError> {
        let n = usize::from(idx);
        let oor = MachineError::OutOfRange { bank, idx };
        let value = match bank {
            RegBank::B => Value::Int(i64::from(*self.b.get(n).ok_or(oor)?)),
            RegBank::I => Value::Int(i64::from(*self.i.get(n).ok_or(oor)?)),
            RegBank::L => Value::Int(i64::from(*self.l.get(n).ok_or(oor)?)),
            RegBank::S => Value::Bytes(self.s.get(n).ok_or(oor)?.clone()),
            RegBank::F => Value::Float(*self.f.get(n).ok_or(oor)?),
        };
        Ok(value)
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
            // Integer banks store unsigned and truncate to the bank's width.
            (RegBank::B, Value::Int(v)) => *self.b.get_mut(n).ok_or(oor)? = v as u8,
            (RegBank::I, Value::Int(v)) => *self.i.get_mut(n).ok_or(oor)? = v as u16,
            (RegBank::L, Value::Int(v)) => *self.l.get_mut(n).ok_or(oor)? = v as u32,
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
    fn indexed_operand_access_is_unsupported_for_now() {
        let mut m = Machine::new();
        let indexed = Operand::Indexed {
            base: RegId {
                bank: RegBank::S,
                idx: 0,
            },
            index: IndexArg::Imm(0),
            len: None,
        };
        assert!(matches!(
            m.read(&indexed),
            Err(MachineError::Unsupported(_))
        ));
        assert!(matches!(
            m.write(&indexed, Value::Int(0)),
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
