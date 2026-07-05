//! The BEST/2 opcode executor: run one decoded [`Op`] against a [`Machine`].
//!
//! [`step`] dispatches on the instruction's opcode byte and mutates the machine
//! in place — moving [`Value`]s through [`Machine::read`]/[`Machine::write`] and
//! updating the Z/S/C/V [`crate::Flags`]. It returns a [`Flow`] telling the
//! (later) run loop whether to advance, follow a jump, or stop.
//!
//! This module grows one opcode class at a time across the executor tasks. It
//! currently implements the integer **arithmetic, logic, move, flag, and shift**
//! opcodes (`move`, `clear`, `comp`, `subb`, `adds`, `mult`, `divs`, `and`,
//! `or`, `xor`, `not`, `clrc`, `setc`, `asr`, `lsl`, `lsr`, `asl`, `nop`); every
//! other opcode byte returns [`ExecError::Unimplemented`] until its task lands.
//!
//! ## No degrade-to-raw
//! Inside the VM an unimplemented opcode, an operand the opcode cannot use, or a
//! division fault is a hard [`ExecError`] — never a silent no-op or a guessed
//! result. A wrong result is worse than a loud stop.
//!
//! ## Where the facts come from
//! Each opcode's exact effect — which flags it touches, how it computes carry and
//! overflow, whether it sign-extends by operand width — is a **fact** about
//! EDIABAS's BEST/2 machine, read from the reference handlers and the `Flags`
//! model and reimplemented in our own code (klartext is AGPL-3.0; the reference
//! is an offline oracle, never copied).

use crate::decode::{Op, Operand, RegBank};
use crate::machine::{Flags, Machine, MachineError, Value};
use crate::opcode::info;

/// What executing one instruction tells the run loop to do next.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flow {
    /// Advance to the next sequential instruction.
    Next,
    /// A branch/call set the program counter; do not auto-advance.
    Jumped,
    /// The job's `eoj` was reached; stop executing.
    EndOfJob,
}

/// External state threaded through execution, alongside the [`Machine`].
///
/// The arithmetic/logic/flag opcodes touch only the machine, so this is empty
/// today. Later executor tasks add the job arguments, the result set, the UDS
/// exchange, and the loaded tables here.
#[derive(Debug, Default)]
pub struct ExecCtx {}

/// An error from executing one BEST/2 instruction.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExecError {
    /// The opcode has no executor handler in this build yet.
    #[error("opcode `{0}` is not implemented by the executor")]
    Unimplemented(&'static str),
    /// Resolving an operand against the machine failed.
    #[error(transparent)]
    Machine(#[from] MachineError),
    /// An integer opcode received an operand it cannot use: a target that is not
    /// a byte/word/long register, a non-numeric source, or an operand whose
    /// width the decoder does not preserve (an immediate as an arithmetic
    /// target). The reference raises an equivalent hard error here.
    #[error("opcode `{0}` received an operand of the wrong type")]
    InvalidOperand(&'static str),
    /// `divs` divided by zero or hit the signed `MIN / -1` overflow (EDIABAS
    /// treats both as the same division fault).
    #[error("opcode `divs` divided by zero or overflowed")]
    DivideByZero,
}

/// Executes one decoded instruction against `m`, returning the control [`Flow`].
///
/// Dispatches on `op.byte` (the opcode class is only a coarse hint, per the
/// decoder's design). Handles this build's arithmetic/logic/move/flag/shift
/// opcodes; any other byte is an [`ExecError::Unimplemented`].
///
/// # Errors
/// Returns [`ExecError::Unimplemented`] for an opcode with no handler yet,
/// [`ExecError::InvalidOperand`] for an operand the opcode cannot use,
/// [`ExecError::DivideByZero`] for a `divs` fault, and [`ExecError::Machine`]
/// when an operand read/write against the machine fails.
pub fn step(m: &mut Machine, op: &Op, _ctx: &mut ExecCtx) -> Result<Flow, ExecError> {
    match op.byte {
        0x00 => op_move(m, op),
        0x01 => op_clear(m, op),
        0x02 => op_comp(m, op),
        0x03 => op_subb(m, op),
        0x04 => op_adds(m, op),
        0x05 => op_mult(m, op),
        0x06 => op_divs(m, op),
        0x07 => op_bitwise(m, "and", op, |a, b| a & b),
        0x08 => op_bitwise(m, "or", op, |a, b| a | b),
        0x09 => op_bitwise(m, "xor", op, |a, b| a ^ b),
        0x0A => op_not(m, op),
        0x16 => {
            m.flags.c = false; // clrc
            Ok(Flow::Next)
        }
        0x17 => {
            m.flags.c = true; // setc
            Ok(Flow::Next)
        }
        0x18 => op_asr(m, op),
        0x19 => op_shift_left(m, "lsl", op),
        0x1A => op_lsr(m, op),
        0x1B => op_shift_left(m, "asl", op),
        0x1C => Ok(Flow::Next), // nop
        other => Err(ExecError::Unimplemented(
            info(other).map_or("<unknown>", |i| i.mnemonic),
        )),
    }
}

// ---- operand width and flag helpers (faithful to EDIABAS's `Flags` model) ----

/// The operand width in bytes EDIABAS uses for an integer opcode: `arg0`'s
/// register width (`B` = 1, `I` = 2, `L` = 4), per `GetArgsValueLength`.
///
/// The integer opcodes require `arg0` to be one of these registers. Any other
/// operand — a string/float register, an immediate (whose 8/16/32-bit width the
/// decoder collapses, so it cannot serve as a sized target), or an indexed form
/// — is an [`ExecError::InvalidOperand`]; the reference throws here likewise.
fn arg_width(mnemonic: &'static str, arg0: &Operand) -> Result<u32, ExecError> {
    match arg0 {
        Operand::Reg {
            bank: RegBank::B, ..
        } => Ok(1),
        Operand::Reg {
            bank: RegBank::I, ..
        } => Ok(2),
        Operand::Reg {
            bank: RegBank::L, ..
        } => Ok(4),
        _ => Err(ExecError::InvalidOperand(mnemonic)),
    }
}

/// The `(value_mask, sign_mask)` for a `len`-byte width (`len` is 1, 2, or 4).
fn masks(len: u32) -> (u32, u32) {
    match len {
        1 => (0x0000_00FF, 0x0000_0080),
        2 => (0x0000_FFFF, 0x0000_8000),
        _ => (0xFFFF_FFFF, 0x8000_0000),
    }
}

/// `Flags.UpdateFlags`: set Zero and Sign from `value` masked to `len` bytes.
fn update_zs(flags: &mut Flags, value: u32, len: u32) {
    let (value_mask, sign_mask) = masks(len);
    flags.z = (value & value_mask) == 0;
    flags.s = (value & sign_mask) != 0;
}

/// `Flags.SetCarry`: Carry is the bit just above the `len`-byte width.
fn set_carry(flags: &mut Flags, value: u64, len: u32) {
    let carry_mask: u64 = match len {
        1 => 0x0000_0100,
        2 => 0x0001_0000,
        _ => 0x1_0000_0000,
    };
    flags.c = (value & carry_mask) != 0;
}

/// `Flags.SetOverflow`: signed overflow from the operands' and result's sign
/// bits — set only when the two operands share a sign bit that differs from the
/// result's.
fn set_overflow(flags: &mut Flags, v0: u32, v1: u32, result: u32, len: u32) {
    let sign_mask = masks(len).1;
    let s0 = v0 & sign_mask;
    let s1 = v1 & sign_mask;
    let sr = result & sign_mask;
    flags.v = s0 == s1 && s0 != sr;
}

/// Reads `op` as a `len`-byte integer, per EDIABAS `GetValueData(len)`.
///
/// An integer source (register or immediate) yields its own-width-masked value;
/// a byte-buffer source (`S` register or string literal) yields its first `len`
/// bytes little-endian, with missing bytes counting as zero. A float source is
/// an [`ExecError::InvalidOperand`] (EDIABAS rejects it here).
fn read_int(m: &Machine, mnemonic: &'static str, op: &Operand, len: u32) -> Result<u32, ExecError> {
    match m.read(op)? {
        Value::Int(v) => Ok(v as u32),
        Value::Bytes(bytes) => {
            let mut value = 0u32;
            for i in (0..len as usize).rev() {
                value = (value << 8) | u32::from(bytes.get(i).copied().unwrap_or(0));
            }
            Ok(value)
        }
        Value::Float(_) => Err(ExecError::InvalidOperand(mnemonic)),
    }
}

// ---- opcode handlers ----

/// `move` (0x00): copy the source into an integer register target.
///
/// Task 7 implements the integer-target form (`B`/`I`/`L`); a byte-buffer or
/// float target is left to a later executor task (it errors via [`arg_width`]).
fn op_move(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("move", &op.arg0)?;
    let value = read_int(m, "move", &op.arg1, len)?;
    m.write(&op.arg0, Value::Int(i64::from(value)))?;
    m.flags.c = false;
    m.flags.v = false;
    update_zs(&mut m.flags, value, len);
    Ok(Flow::Next)
}

/// `clear` (0x01): zero the target register; sets Zero, clears the rest.
fn op_clear(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let zero = match &op.arg0 {
        Operand::Reg {
            bank: RegBank::B | RegBank::I | RegBank::L,
            ..
        } => Value::Int(0),
        Operand::Reg {
            bank: RegBank::S, ..
        } => Value::Bytes(Vec::new()),
        Operand::Reg {
            bank: RegBank::F, ..
        } => Value::Float(0.0),
        _ => return Err(ExecError::InvalidOperand("clear")),
    };
    m.write(&op.arg0, zero)?;
    m.flags.c = false;
    m.flags.z = true;
    m.flags.s = false;
    m.flags.v = false;
    Ok(Flow::Next)
}

/// `comp` (0x02): compare `arg0 - arg1`, setting flags without storing.
fn op_comp(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("comp", &op.arg0)?;
    let v0 = read_int(m, "comp", &op.arg0, len)?;
    let v1 = read_int(m, "comp", &op.arg1, len)?;
    let diff = u64::from(v0).wrapping_sub(u64::from(v1));
    update_zs(&mut m.flags, diff as u32, len);
    set_overflow(&mut m.flags, v0, v1.wrapping_neg(), diff as u32, len);
    set_carry(&mut m.flags, diff, len);
    Ok(Flow::Next)
}

/// `subb` (0x03): `arg0 -= arg1`, updating Z/S/C/V.
fn op_subb(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("subb", &op.arg0)?;
    let v0 = read_int(m, "subb", &op.arg0, len)?;
    let v1 = read_int(m, "subb", &op.arg1, len)?;
    let diff = u64::from(v0).wrapping_sub(u64::from(v1));
    m.write(&op.arg0, Value::Int(i64::from(diff as u32)))?;
    update_zs(&mut m.flags, diff as u32, len);
    set_overflow(&mut m.flags, v0, v1.wrapping_neg(), diff as u32, len);
    set_carry(&mut m.flags, diff, len);
    Ok(Flow::Next)
}

/// `adds` (0x04): `arg0 += arg1`, updating Z/S/C/V.
fn op_adds(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("adds", &op.arg0)?;
    let v0 = read_int(m, "adds", &op.arg0, len)?;
    let v1 = read_int(m, "adds", &op.arg1, len)?;
    let sum = u64::from(v0) + u64::from(v1);
    m.write(&op.arg0, Value::Int(i64::from(sum as u32)))?;
    update_zs(&mut m.flags, sum as u32, len);
    set_overflow(&mut m.flags, v0, v1, sum as u32, len);
    set_carry(&mut m.flags, sum, len);
    Ok(Flow::Next)
}

/// `mult` (0x05): signed product into `arg0` (low word) and, if `arg1` is a
/// register, its high word into `arg1`; `Overflow` cleared, Z/S updated.
fn op_mult(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("mult", &op.arg0)?;
    let v0 = read_int(m, "mult", &op.arg0, len)?;
    let v1 = read_int(m, "mult", &op.arg1, len)?;
    // EDIABAS sign-extends each operand by its width before multiplying.
    let result: u32 = match len {
        1 => (i32::from(v0 as u8 as i8).wrapping_mul(i32::from(v1 as u8 as i8))) as u32,
        2 => (i32::from(v0 as u16 as i16).wrapping_mul(i32::from(v1 as u16 as i16))) as u32,
        _ => (v0 as i32).wrapping_mul(v1 as i32) as u32,
    };
    m.write(&op.arg0, Value::Int(i64::from(result)))?;
    m.flags.v = false;
    update_zs(&mut m.flags, result, len);
    // The high half of the product goes into arg1 when it is a register.
    if let Operand::Reg { .. } = op.arg1 {
        let result_high = (u64::from(result) >> (len * 8)) as u32;
        m.write(&op.arg1, Value::Int(i64::from(result_high)))?;
    }
    Ok(Flow::Next)
}

/// `divs` (0x06): signed 32-bit quotient into `arg0`, remainder into `arg1` when
/// it is a register; `Overflow` cleared, Z/S updated. A divide-by-zero or the
/// signed `MIN / -1` overflow is a hard [`ExecError::DivideByZero`].
fn op_divs(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("divs", &op.arg0)?;
    let v0 = read_int(m, "divs", &op.arg0, len)?;
    let v1 = read_int(m, "divs", &op.arg1, len)?;
    // EDIABAS divides using signed 32-bit arithmetic for every width.
    let dividend = v0 as i32;
    let divisor = v1 as i32;
    let quotient = dividend
        .checked_div(divisor)
        .ok_or(ExecError::DivideByZero)?;
    let remainder = dividend
        .checked_rem(divisor)
        .ok_or(ExecError::DivideByZero)?;
    let result = quotient as u32;
    m.write(&op.arg0, Value::Int(i64::from(result)))?;
    m.flags.v = false;
    update_zs(&mut m.flags, result, len);
    if let Operand::Reg { .. } = op.arg1 {
        m.write(&op.arg1, Value::Int(i64::from(remainder as u32)))?;
    }
    Ok(Flow::Next)
}

/// `and`/`or`/`xor` (0x07-0x09): bitwise `arg0 = f(arg0, arg1)`; `Overflow`
/// cleared, Z/S updated, Carry untouched.
fn op_bitwise(
    m: &mut Machine,
    mnemonic: &'static str,
    op: &Op,
    f: impl Fn(u32, u32) -> u32,
) -> Result<Flow, ExecError> {
    let len = arg_width(mnemonic, &op.arg0)?;
    let v0 = read_int(m, mnemonic, &op.arg0, len)?;
    let v1 = read_int(m, mnemonic, &op.arg1, len)?;
    let value = f(v0, v1);
    m.write(&op.arg0, Value::Int(i64::from(value)))?;
    m.flags.v = false;
    update_zs(&mut m.flags, value, len);
    Ok(Flow::Next)
}

/// `not` (0x0A): bitwise complement of `arg0`; `Overflow` cleared, Z/S updated,
/// Carry untouched.
fn op_not(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("not", &op.arg0)?;
    let value = !read_int(m, "not", &op.arg0, len)?;
    m.write(&op.arg0, Value::Int(i64::from(value)))?;
    m.flags.v = false;
    update_zs(&mut m.flags, value, len);
    Ok(Flow::Next)
}

/// `lsl`/`asl` (0x19/0x1B): shift `arg0` left by `arg1`; Carry takes the last
/// bit shifted out of the top, `Overflow` cleared, Z/S updated. Left arithmetic
/// and logical shifts are identical.
fn op_shift_left(m: &mut Machine, mnemonic: &'static str, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width(mnemonic, &op.arg0)?;
    let mut value = read_int(m, mnemonic, &op.arg0, len)?;
    let shift = read_int(m, mnemonic, &op.arg1, len)? as i32;
    let bits = (len * 8) as i32;
    if shift < 0 {
        // Negative shift: leave Carry and the value untouched (per reference).
    } else if shift == 0 {
        m.flags.c = false;
    } else {
        if shift > bits {
            m.flags.c = false;
        } else {
            let carry_shift = (bits - shift) as u32;
            m.flags.c = (value & (1u32 << carry_shift)) != 0;
        }
        value = if shift >= bits { 0 } else { value << shift };
    }
    m.write(&op.arg0, Value::Int(i64::from(value)))?;
    m.flags.v = false;
    update_zs(&mut m.flags, value, len);
    Ok(Flow::Next)
}

/// `lsr` (0x1A): logical shift `arg0` right by `arg1`; Carry takes the last bit
/// shifted out of the bottom, `Overflow` cleared, Z/S updated.
fn op_lsr(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("lsr", &op.arg0)?;
    let mut value = read_int(m, "lsr", &op.arg0, len)?;
    let shift = read_int(m, "lsr", &op.arg1, len)? as i32;
    let bits = (len * 8) as i32;
    if shift < 0 {
        // Negative shift: leave Carry and the value untouched.
    } else if shift == 0 {
        m.flags.c = false;
    } else {
        if shift > bits {
            m.flags.c = false;
        } else {
            let carry_shift = (shift - 1) as u32;
            m.flags.c = (value & (1u32 << carry_shift)) != 0;
        }
        value = if shift >= bits { 0 } else { value >> shift };
    }
    m.write(&op.arg0, Value::Int(i64::from(value)))?;
    m.flags.v = false;
    update_zs(&mut m.flags, value, len);
    Ok(Flow::Next)
}

/// `asr` (0x18): arithmetic shift `arg0` right by `arg1`, sign-extending; Carry
/// takes the last bit shifted out of the bottom, `Overflow` cleared, Z/S
/// updated.
fn op_asr(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("asr", &op.arg0)?;
    let mut value = read_int(m, "asr", &op.arg0, len)?;
    let shift = read_int(m, "asr", &op.arg1, len)? as i32;
    let bits = (len * 8) as i32;
    let sign_mask = masks(len).1;
    if shift < 0 {
        // Negative shift: leave Carry and the value untouched.
    } else if shift == 0 {
        m.flags.c = false;
    } else {
        if shift > bits {
            m.flags.c = (value & sign_mask) != 0;
        } else {
            let carry_shift = (shift - 1) as u32;
            m.flags.c = (value & (1u32 << carry_shift)) != 0;
        }
        value = if shift >= bits {
            if (value & sign_mask) != 0 {
                0xFFFF_FFFF
            } else {
                0
            }
        } else {
            let s = shift as u32;
            match len {
                1 => ((value as u8 as i8) >> s) as u32,
                2 => ((value as u16 as i16) >> s) as u32,
                _ => ((value as i32) >> s) as u32,
            }
        };
    }
    m.write(&op.arg0, Value::Int(i64::from(value)))?;
    m.flags.v = false;
    update_zs(&mut m.flags, value, len);
    Ok(Flow::Next)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A byte register `B<idx>` operand.
    fn reg_b(idx: u8) -> Operand {
        Operand::Reg {
            bank: RegBank::B,
            idx,
        }
    }
    /// A word register `I<idx>` operand.
    fn reg_i(idx: u8) -> Operand {
        Operand::Reg {
            bank: RegBank::I,
            idx,
        }
    }
    /// A long register `L<idx>` operand.
    fn reg_l(idx: u8) -> Operand {
        Operand::Reg {
            bank: RegBank::L,
            idx,
        }
    }
    /// An immediate operand.
    fn imm(v: i64) -> Operand {
        Operand::Imm(v)
    }

    /// Builds an [`Op`] with the given opcode byte and two operands; the mode
    /// byte, length, and offset are irrelevant to execution and left zero.
    fn op(byte: u8, arg0: Operand, arg1: Operand) -> Op {
        Op {
            byte,
            mode_byte: 0,
            arg0,
            arg1,
            len: 0,
            offset: 0,
        }
    }

    fn op_move(a0: Operand, a1: Operand) -> Op {
        op(0x00, a0, a1)
    }
    fn op_clear(a0: Operand) -> Op {
        op(0x01, a0, Operand::None)
    }
    fn op_comp(a0: Operand, a1: Operand) -> Op {
        op(0x02, a0, a1)
    }
    fn op_subb(a0: Operand, a1: Operand) -> Op {
        op(0x03, a0, a1)
    }
    fn op_adds(a0: Operand, a1: Operand) -> Op {
        op(0x04, a0, a1)
    }
    fn op_mult(a0: Operand, a1: Operand) -> Op {
        op(0x05, a0, a1)
    }
    fn op_divs(a0: Operand, a1: Operand) -> Op {
        op(0x06, a0, a1)
    }
    fn op_and(a0: Operand, a1: Operand) -> Op {
        op(0x07, a0, a1)
    }
    fn op_or(a0: Operand, a1: Operand) -> Op {
        op(0x08, a0, a1)
    }
    fn op_xor(a0: Operand, a1: Operand) -> Op {
        op(0x09, a0, a1)
    }
    fn op_not(a0: Operand) -> Op {
        op(0x0A, a0, Operand::None)
    }
    fn op_lsl(a0: Operand, a1: Operand) -> Op {
        op(0x19, a0, a1)
    }
    fn op_lsr(a0: Operand, a1: Operand) -> Op {
        op(0x1A, a0, a1)
    }
    fn op_asr(a0: Operand, a1: Operand) -> Op {
        op(0x18, a0, a1)
    }

    /// Runs one op against `m`, asserting it succeeds and returns `Flow::Next`.
    fn run(m: &mut Machine, o: &Op) {
        assert_eq!(step(m, o, &mut ExecCtx::default()).unwrap(), Flow::Next);
    }

    #[test]
    fn mult_multiplies_into_arg0_and_sets_zero_flag() {
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(3)).unwrap();
        m.write(&reg_i(1), Value::Int(4)).unwrap();
        run(&mut m, &op_mult(reg_i(0), reg_i(1)));
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(12));
        assert!(!m.flags.z);
        // The high word of the product lands in arg1 (here 0), per EDIABAS.
        assert_eq!(m.read(&reg_i(1)).unwrap(), Value::Int(0));
    }

    #[test]
    fn and_masks_bits() {
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0xC0)).unwrap();
        run(&mut m, &op_and(reg_b(0), imm(0x3F)));
        assert_eq!(m.read(&reg_b(0)).unwrap(), Value::Int(0x00));
        assert!(m.flags.z);
    }

    #[test]
    fn or_sets_bits_and_clears_zero() {
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0xC0)).unwrap();
        run(&mut m, &op_or(reg_b(0), imm(0x0F)));
        assert_eq!(m.read(&reg_b(0)).unwrap(), Value::Int(0xCF));
        assert!(!m.flags.z);
        assert!(m.flags.s); // bit 7 set
    }

    #[test]
    fn xor_toggles_bits() {
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0xFF)).unwrap();
        run(&mut m, &op_xor(reg_b(0), imm(0x0F)));
        assert_eq!(m.read(&reg_b(0)).unwrap(), Value::Int(0xF0));
    }

    #[test]
    fn not_inverts_within_width() {
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0x00)).unwrap();
        run(&mut m, &op_not(reg_b(0)));
        assert_eq!(m.read(&reg_b(0)).unwrap(), Value::Int(0xFF));
        assert!(!m.flags.z);
        assert!(m.flags.s);
    }

    #[test]
    fn adds_sets_carry_and_overflow() {
        // 0x80 + 0x80 in one byte: result 0x00, carry out, signed overflow.
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0x80)).unwrap();
        run(&mut m, &op_adds(reg_b(0), imm(0x80)));
        assert_eq!(m.read(&reg_b(0)).unwrap(), Value::Int(0x00));
        assert!(m.flags.z);
        assert!(m.flags.c);
        assert!(m.flags.v);
        assert!(!m.flags.s);
    }

    #[test]
    fn adds_without_carry_or_overflow() {
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0x01)).unwrap();
        run(&mut m, &op_adds(reg_b(0), imm(0x02)));
        assert_eq!(m.read(&reg_b(0)).unwrap(), Value::Int(0x03));
        assert!(!m.flags.z);
        assert!(!m.flags.c);
        assert!(!m.flags.v);
    }

    #[test]
    fn subb_borrows_and_wraps() {
        // 0 - 1 in one byte: 0xFF, borrow (carry), sign set, no signed overflow.
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0x00)).unwrap();
        run(&mut m, &op_subb(reg_b(0), imm(0x01)));
        assert_eq!(m.read(&reg_b(0)).unwrap(), Value::Int(0xFF));
        assert!(!m.flags.z);
        assert!(m.flags.s);
        assert!(m.flags.c);
        assert!(!m.flags.v);
    }

    #[test]
    fn comp_equal_sets_zero_without_storing() {
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(5)).unwrap();
        run(&mut m, &op_comp(reg_i(0), imm(5)));
        assert!(m.flags.z);
        assert!(!m.flags.c);
        // comp does not modify its target.
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(5));
    }

    #[test]
    fn comp_less_borrows() {
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(5)).unwrap();
        run(&mut m, &op_comp(reg_i(0), imm(8)));
        assert!(!m.flags.z);
        assert!(m.flags.c); // 5 - 8 borrows
        assert!(m.flags.s);
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(5));
    }

    #[test]
    fn divs_quotient_and_remainder() {
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(20)).unwrap();
        m.write(&reg_i(1), Value::Int(6)).unwrap();
        run(&mut m, &op_divs(reg_i(0), reg_i(1)));
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(3)); // 20 / 6
        assert_eq!(m.read(&reg_i(1)).unwrap(), Value::Int(2)); // 20 % 6
        assert!(!m.flags.z);
    }

    #[test]
    fn divs_by_zero_is_hard_error() {
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(5)).unwrap();
        m.write(&reg_i(1), Value::Int(0)).unwrap();
        assert_eq!(
            step(
                &mut m,
                &op_divs(reg_i(0), reg_i(1)),
                &mut ExecCtx::default()
            ),
            Err(ExecError::DivideByZero)
        );
    }

    #[test]
    fn move_copies_source_and_clears_carry() {
        let mut m = Machine::new();
        m.flags.c = true;
        run(&mut m, &op_move(reg_i(0), imm(0x1234)));
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0x1234));
        assert!(!m.flags.c);
        assert!(!m.flags.z);
        assert!(!m.flags.s); // 0x1234 has bit 15 clear
    }

    #[test]
    fn clear_zeroes_target_and_sets_zero_flag() {
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(0x1234)).unwrap();
        run(&mut m, &op_clear(reg_i(0)));
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0));
        assert!(m.flags.z);
        assert!(!m.flags.c);
        assert!(!m.flags.s);
        assert!(!m.flags.v);
    }

    #[test]
    fn lsl_shifts_msb_into_carry() {
        // 0x81 << 1 in one byte: 0x02, carry from the shifted-out MSB.
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0x81)).unwrap();
        run(&mut m, &op_lsl(reg_b(0), imm(1)));
        assert_eq!(m.read(&reg_b(0)).unwrap(), Value::Int(0x02));
        assert!(m.flags.c);
        assert!(!m.flags.z);
    }

    #[test]
    fn lsr_shifts_lsb_into_carry() {
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0x01)).unwrap();
        run(&mut m, &op_lsr(reg_b(0), imm(1)));
        assert_eq!(m.read(&reg_b(0)).unwrap(), Value::Int(0x00));
        assert!(m.flags.c);
        assert!(m.flags.z);
    }

    #[test]
    fn asr_sign_extends() {
        // 0x80 >> 1 arithmetic in one byte: 0xC0 (sign preserved), no carry.
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0x80)).unwrap();
        run(&mut m, &op_asr(reg_b(0), imm(1)));
        assert_eq!(m.read(&reg_b(0)).unwrap(), Value::Int(0xC0));
        assert!(m.flags.s);
        assert!(!m.flags.c);
    }

    #[test]
    fn clrc_and_setc_toggle_carry() {
        let mut m = Machine::new();
        run(&mut m, &op(0x17, Operand::None, Operand::None)); // setc
        assert!(m.flags.c);
        run(&mut m, &op(0x16, Operand::None, Operand::None)); // clrc
        assert!(!m.flags.c);
    }

    #[test]
    fn nop_does_nothing() {
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(0x1234)).unwrap();
        run(&mut m, &op(0x1C, Operand::None, Operand::None));
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0x1234));
    }

    #[test]
    fn long_register_arithmetic_uses_full_width() {
        let mut m = Machine::new();
        m.write(&reg_l(0), Value::Int(0xFFFF_FFFF)).unwrap();
        run(&mut m, &op_adds(reg_l(0), imm(1)));
        assert_eq!(m.read(&reg_l(0)).unwrap(), Value::Int(0)); // wraps at 32 bits
        assert!(m.flags.z);
        assert!(m.flags.c); // carry out of bit 31
    }

    #[test]
    fn unimplemented_opcode_is_hard_error() {
        // 0x2A = xsend, a comm opcode handled in a later task.
        let mut m = Machine::new();
        assert_eq!(
            step(
                &mut m,
                &op(0x2A, Operand::None, Operand::None),
                &mut ExecCtx::default()
            ),
            Err(ExecError::Unimplemented("xsend"))
        );
    }

    #[test]
    fn arithmetic_on_immediate_target_is_rejected() {
        // adds requires a writable integer register as arg0.
        let mut m = Machine::new();
        assert_eq!(
            step(&mut m, &op_adds(imm(1), imm(2)), &mut ExecCtx::default()),
            Err(ExecError::InvalidOperand("adds"))
        );
    }
}
