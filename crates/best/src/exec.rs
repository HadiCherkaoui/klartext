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
//! `or`, `xor`, `not`, `clrc`, `setc`, `asr`, `lsl`, `lsr`, `asl`, `nop`),
//! **control flow** — the unconditional `jump`, the flag-testing conditional
//! jumps (`jz`/`jnz`, `jc`/`jae`, `jv`/`jnv`, `jmi`/`jpl`, and the signed/unsigned
//! combos `jg`/`jge`/`jl`/`jle`/`ja`/`jbe`), the data-stack `push`/`pop`,
//! `break`, and `eoj` — and the **float arithmetic and byte/number conversions**
//! (`fadd`/`fsub`/`fmul`/`fdiv`, `a2flt`/`a2fix`, `fix2flt`/`flt2fix`, `flt2a`,
//! `a2y`/`hex2y`, `y2bcd`/`y2hex`, `y42flt`/`y82flt`). Every other opcode byte
//! returns [`ExecError::Unimplemented`] until its task lands — including
//! `jtsr`/`ret` and the error-trap `jt`/`jnt`, which EDIABAS itself never runs
//! here (see [`step`]).
//!
//! ## No degrade-to-raw
//! Inside the VM an unimplemented opcode, an operand the opcode cannot use, or a
//! division fault is a hard [`ExecError`] — never a silent no-op or a guessed
//! result. A wrong result is worse than a loud stop.
//!
//! ## Program counter
//! The machine's `pc` is a **byte offset** (EDIABAS's `_pcCounter`). [`step`]
//! pre-advances it to the byte just past the current instruction before
//! dispatching; a taken jump then rewrites it to `pc + rel`, where `rel` is
//! `arg0`'s raw 32 bits read as a signed `i32` so a backward branch resolves
//! correctly. Task 13's run loop maps that offset back to the next instruction.
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
    /// `pop` requested more bytes than the data stack holds — an EDIABAS
    /// data-stack underflow. No-degrade: a hard error, never a silent zero-fill.
    #[error("opcode `pop` underflowed the data stack")]
    StackUnderflow,
    /// `break` (0x4B) executed: EDIABAS's user-break instruction, which raises
    /// `EDIABAS_BIP_0008` and aborts the job. No-degrade: a hard stop, never a
    /// silent continue (the error-trap subsystem that would catch it is deferred
    /// alongside `jt`/`jnt`).
    #[error("break (0x4B): user-break instruction (EDIABAS_BIP_0008)")]
    Break,
    /// A float opcode produced or received a non-finite value (Inf/NaN) where a
    /// finite one is required: `fadd`/`fsub`/`fmul`/`fdiv` (an overflowing or
    /// divide-by-zero result), or `flt2fix`/`flt2a` (a non-finite input). The
    /// reference raises `EDIABAS_BIP_0011` for the arithmetic case; no-degrade
    /// makes it a hard stop here, never a stored garbage value.
    #[error("opcode `{0}` produced or received a non-finite float (Inf/NaN)")]
    NonFinite(&'static str),
    /// `a2flt` could not parse its operand string as a finite float. No-degrade:
    /// a hard error, matching the reference's `>= 7.60` `EDIABAS_BIP_0011` path.
    #[error("opcode `a2flt` could not parse `{0}` as a finite float")]
    BadFloatString(String),
}

/// Executes one decoded instruction against `m`, returning the control [`Flow`].
///
/// Dispatches on `op.byte` (the opcode class is only a coarse hint, per the
/// decoder's design). Handles this build's arithmetic/logic/move/flag/shift and
/// control-flow opcodes; any other byte is an [`ExecError::Unimplemented`].
///
/// Before dispatching, `m.pc` is pre-advanced to `op.offset + op.len` — the byte
/// just past this instruction — so a taken jump can rewrite it relative to the
/// following instruction and every other opcode leaves the PC correctly advanced.
///
/// # Errors
/// Returns [`ExecError::Unimplemented`] for an opcode with no handler yet
/// (including the null-handled `jtsr`/`ret` and the error-trap `jt`/`jnt`),
/// [`ExecError::InvalidOperand`] for an operand the opcode cannot use,
/// [`ExecError::DivideByZero`] for a `divs` fault, [`ExecError::StackUnderflow`]
/// when `pop` outruns the data stack, [`ExecError::Break`] when a `break`
/// user-break instruction executes, and [`ExecError::Machine`] when an operand
/// read/write against the machine fails.
pub fn step(m: &mut Machine, op: &Op, _ctx: &mut ExecCtx) -> Result<Flow, ExecError> {
    // EDIABAS advances `_pcCounter` to the byte just past this instruction before
    // running its handler; a taken jump then rewrites it to the target. Mirror
    // that here so the run loop and the jump handlers share one PC model
    // (EdiabasNet.cs:5816-5822).
    m.pc = op.offset + op.len;
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
        0x0B => branch(m, "jump", op, true),
        0x0E => branch(m, "jc", op, m.flags.c),
        0x0F => branch(m, "jae", op, !m.flags.c),
        0x10 => branch(m, "jz", op, m.flags.z),
        0x11 => branch(m, "jnz", op, !m.flags.z),
        0x12 => branch(m, "jv", op, m.flags.v),
        0x13 => branch(m, "jnv", op, !m.flags.v),
        0x14 => branch(m, "jmi", op, m.flags.s),
        0x15 => branch(m, "jpl", op, !m.flags.s),
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
        0x1C => Ok(Flow::Next),     // nop
        0x1D => Ok(Flow::EndOfJob), // eoj
        0x1E => op_push(m, op),
        0x1F => op_pop(m, op),
        0x4B => op_break(),
        0x5A => branch(m, "jg", op, m.flags.s == m.flags.v && !m.flags.z),
        0x5B => branch(m, "jge", op, m.flags.z || m.flags.s == m.flags.v),
        0x5C => branch(m, "jl", op, !m.flags.z && m.flags.s != m.flags.v),
        0x5D => branch(m, "jle", op, m.flags.s != m.flags.v || m.flags.z),
        0x5E => branch(m, "ja", op, !m.flags.c && !m.flags.z),
        0x5F => branch(m, "jbe", op, m.flags.c || m.flags.z),
        0x3A => op_a2flt(m, op),
        0x3B => op_float_arith(m, "fadd", op, |a, b| a + b),
        0x3C => op_float_arith(m, "fsub", op, |a, b| a - b),
        0x3D => op_float_arith(m, "fmul", op, |a, b| a * b),
        0x3E => op_float_arith(m, "fdiv", op, |a, b| a / b),
        0x67 => op_a2fix(m, op),
        0x68 => op_fix2flt(m, op),
        0x87 => op_flt2a(m, op),
        0x8C => op_a2y(m, op),
        0x8E => op_hex2y(m, op),
        0x91 => op_y2bcd(m, op),
        0x92 => op_y2hex(m, op),
        0x96 => op_flt2fix(m, op),
        0x9D => op_y_to_flt(m, "y42flt", op, 4),
        0x9E => op_y_to_flt(m, "y82flt", op, 8),
        // Two BEST/2 opcode groups deliberately reach this `Unimplemented` arm
        // rather than getting a handler — this is faithful, not a gap:
        //   * `jtsr` (0x0C) / `ret` (0x0D): EDIABAS registers null handlers and
        //     throws "not implemented" if a job ever executes one
        //     (EdiabasNet.cs:5851-5853); modern jobs never use them.
        //   * `jt` (0x47) / `jnt` (0x48): error-trap branches that test
        //     `_errorTrapBitNr`, part of the eerr/generr error-trap subsystem not
        //     built in Phase 1; Task 13 can add the trap state if the oracle
        //     needs them.
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

/// Applies a conditional or unconditional PC-relative branch.
///
/// [`step`] has already pre-advanced `m.pc` to this instruction's end. When
/// `taken`, EDIABAS adds `arg0` — the raw 32 immediate bits read back as a
/// **signed** `i32`, so a backward branch carries a negative displacement — to
/// that post-instruction PC and reports [`Flow::Jumped`]; otherwise the
/// pre-advance stands and it reports [`Flow::Next`]. A branch whose target
/// operand is not an immediate is a hard [`ExecError::InvalidOperand`] — a
/// well-formed job always encodes the target as `Imm32`.
fn branch(
    m: &mut Machine,
    mnemonic: &'static str,
    op: &Op,
    taken: bool,
) -> Result<Flow, ExecError> {
    if !taken {
        return Ok(Flow::Next);
    }
    let rel = match op.arg0 {
        Operand::Imm(raw) => raw as i32,
        _ => return Err(ExecError::InvalidOperand(mnemonic)),
    };
    m.pc = (m.pc as i64).wrapping_add(i64::from(rel)) as usize;
    Ok(Flow::Jumped)
}

/// `push` (0x1E): push `arg0`'s value onto the data stack, least-significant byte
/// first, for as many bytes as `arg0`'s register width (`GetArgsValueLength`).
fn op_push(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("push", &op.arg0)?;
    let mut value = read_int(m, "push", &op.arg0, len)?;
    for _ in 0..len {
        m.data_stack.push(value as u8);
        value >>= 8;
    }
    Ok(Flow::Next)
}

/// `pop` (0x1F): pop `arg0`'s register width in bytes off the data stack and
/// reassemble them into `arg0`, reversing [`op_push`]'s little-endian order, then
/// clear `Overflow` and set Z/S from the popped value at the operand width. Too
/// few bytes on the stack is an EDIABAS data-stack underflow — a hard
/// [`ExecError::StackUnderflow`], never a silent zero-fill.
fn op_pop(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("pop", &op.arg0)?;
    if m.data_stack.len() < len as usize {
        return Err(ExecError::StackUnderflow);
    }
    let mut value = 0u32;
    for _ in 0..len {
        let byte = m.data_stack.pop().expect("data stack length checked above");
        value = (value << 8) | u32::from(byte);
    }
    m.write(&op.arg0, Value::Int(i64::from(value)))?;
    // EDIABAS clears Overflow, then sets Z/S from the popped value at the operand
    // width (OpPop: `_flags.Overflow = false; _flags.UpdateFlags(value, length)`).
    m.flags.v = false;
    update_zs(&mut m.flags, value, len);
    Ok(Flow::Next)
}

/// `break` (0x4B): the BEST/2 user-break instruction — a hard stop.
///
/// EDIABAS's `OpBreak` is exactly `SetError(EDIABAS_BIP_0008)` and touches no
/// flag (EdOperations.cs:338-341): it raises a user-break error that aborts the
/// job. `break` belongs to the same error-trap subsystem as the deferred
/// `jt`/`jnt`; until that subsystem exists, the faithful Phase 1 behavior is a
/// loud [`ExecError::Break`], never a silent continue past the break.
fn op_break() -> Result<Flow, ExecError> {
    Err(ExecError::Break)
}

// ---- Task 9: float arithmetic and byte/number conversion helpers ----

/// EDIABAS's default float display precision, in significant digits.
///
/// `_floatPrecision` defaults to 4 and is only changed by a config op not built
/// in Phase 1 (EdiabasNet.cs:2528), so the default is hardcoded here.
const FLOAT_PRECISION: usize = 4;

/// Reads `op` as an `f64`, requiring a float source (an `F` register).
///
/// EDIABAS's `GetFloatData` throws for any non-float operand; here that is an
/// [`ExecError::InvalidOperand`] (EdiabasNet.cs:407).
fn read_float(m: &Machine, mnemonic: &'static str, op: &Operand) -> Result<f64, ExecError> {
    match m.read(op)? {
        Value::Float(f) => Ok(f),
        _ => Err(ExecError::InvalidOperand(mnemonic)),
    }
}

/// Reads `op` as a byte buffer, requiring an `S` register or string literal.
///
/// EDIABAS's `GetArrayData` throws for any non-array operand; here that is an
/// [`ExecError::InvalidOperand`] (EdiabasNet.cs:417).
fn read_bytes(m: &Machine, mnemonic: &'static str, op: &Operand) -> Result<Vec<u8>, ExecError> {
    match m.read(op)? {
        Value::Bytes(bytes) => Ok(bytes),
        _ => Err(ExecError::InvalidOperand(mnemonic)),
    }
}

/// Reads `op` as EDIABAS's NUL-terminated string: the buffer's bytes up to the
/// first `0x00`, each taken as a Latin-1 code point (EdiabasNet.cs:427).
fn read_string(m: &Machine, mnemonic: &'static str, op: &Operand) -> Result<String, ExecError> {
    let bytes = read_bytes(m, mnemonic, op)?;
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    Ok(bytes[..end].iter().map(|&b| char::from(b)).collect())
}

/// Writes `text` to an `S`-register `op` the way EDIABAS `SetStringData` does.
///
/// Stores the text's bytes plus a `0x00` terminator, unless the text is empty or
/// already ends in one (EdiabasNet.cs:568). The strings this module produces are
/// ASCII, so their UTF-8 bytes equal the reference's `Encoding` bytes.
fn write_string(m: &mut Machine, op: &Operand, text: &str) -> Result<(), ExecError> {
    let mut bytes: Vec<u8> = text.bytes().collect();
    if bytes.last().is_some_and(|&last| last != 0) {
        bytes.push(0);
    }
    m.write(op, Value::Bytes(bytes))?;
    Ok(())
}

/// Parses a BEST/2 numeric string as EDIABAS `StringToValue` does.
///
/// Recognizes a `0x` hex, `0y` binary, or decimal literal (a decimal is cut at
/// the first `.`/`,`); a leading letter, a lone `-`/`--`, or any parse failure
/// yields 0 — the defined behavior, not a guess (EdiabasNet.cs:7202). The result
/// is the `i64` value before `a2fix`'s clamp.
fn string_to_value(number: &str) -> i64 {
    let trimmed = number.trim_end();
    if trimmed.is_empty() {
        return 0;
    }
    let lower = trimmed.to_ascii_lowercase();
    if let Some(hex) = lower.strip_prefix("0x") {
        // The reference requires the first post-prefix char to be a hex digit.
        if hex.chars().next().is_some_and(|c| c.is_ascii_hexdigit()) {
            return i64::from_str_radix(&trimmed[2..], 16).unwrap_or(0);
        }
        return 0;
    }
    if let Some(bin) = lower.strip_prefix("0y") {
        return i64::from_str_radix(bin, 2).unwrap_or(0);
    }
    if lower == "-" || lower == "--" {
        return 0;
    }
    if lower
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic())
    {
        return 0;
    }
    let dec = trimmed.trim_start();
    let cut = dec.find(['.', ',']).unwrap_or(dec.len());
    dec[..cut].parse::<i64>().unwrap_or(0)
}

/// Decodes complete hex-digit pairs of `text` into bytes, `None` on any bad pair.
///
/// Mirrors `HexToByteArray` (EdiabasNet.cs:7284): a trailing odd nibble is
/// dropped, and any non-hex character makes the whole decode fail.
fn hex_to_bytes(text: &str) -> Option<Vec<u8>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::with_capacity(chars.len() / 2);
    for pair in chars.chunks_exact(2) {
        let hi = pair[0].to_digit(16)?;
        let lo = pair[1].to_digit(16)?;
        out.push((hi * 16 + lo) as u8);
    }
    Some(out)
}

/// Rounds `value` to `digits` significant digits, per `RoundToSignificantDigits`.
///
/// Reproduces the reference's f64 sequence (EdiabasNet.cs:7186); Rust's
/// half-away-from-zero rounding differs from C#'s banker's rounding only on an
/// exact half at the rounding position.
fn round_to_significant_digits(value: f64, digits: i32) -> f64 {
    if value == 0.0 {
        return 0.0;
    }
    let scale = 10f64.powf(value.abs().log10().floor() + 1.0);
    let factor = 10f64.powi(digits);
    scale * ((value / scale) * factor).round() / factor
}

/// Renders a 4-bit nibble as a BCD digit, or `*` when it is not a decimal digit.
fn bcd_nibble(nibble: u8) -> char {
    if nibble > 9 {
        '*'
    } else {
        char::from(b'0' + nibble)
    }
}

/// Renders a 4-bit nibble as one uppercase hex digit.
fn hex_digit(nibble: u8) -> char {
    char::from_digit(u32::from(nibble), 16)
        .expect("a 4-bit nibble is always a valid base-16 digit")
        .to_ascii_uppercase()
}

// ---- Task 9: float arithmetic and byte/number conversion handlers ----

/// `fadd`/`fsub`/`fmul`/`fdiv` (0x3B-0x3E): `arg0 = arg0 OP arg1` in `F` regs.
///
/// A non-finite result (Inf/NaN — including `fdiv` by zero) is a hard
/// [`ExecError::NonFinite`]; the reference raises `EDIABAS_BIP_0011` and stores
/// nothing usable (EdOperations.cs:615/931/903/659). Touches no flags.
fn op_float_arith(
    m: &mut Machine,
    mnemonic: &'static str,
    op: &Op,
    f: impl Fn(f64, f64) -> f64,
) -> Result<Flow, ExecError> {
    let v0 = read_float(m, mnemonic, &op.arg0)?;
    let v1 = read_float(m, mnemonic, &op.arg1)?;
    let result = f(v0, v1);
    if !result.is_finite() {
        return Err(ExecError::NonFinite(mnemonic));
    }
    m.write(&op.arg0, Value::Float(result))?;
    Ok(Flow::Next)
}

/// `a2flt` (0x3A): parse `arg1`'s string into `arg0`'s `F` register as a float.
///
/// `StringToFloat` reads the operand's NUL-terminated bytes, swaps a decimal
/// comma for a dot, and parses (EdiabasNet.cs:7263). An unparseable or non-finite
/// string is a hard [`ExecError::BadFloatString`] — the no-degrade choice,
/// matching the reference's `>= 7.60` `EDIABAS_BIP_0011` (EdOperations.cs:45).
/// Touches no flags.
fn op_a2flt(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let text = read_string(m, "a2flt", &op.arg1)?;
    match text.replace(',', ".").trim().parse::<f64>() {
        Ok(value) if value.is_finite() => {
            m.write(&op.arg0, Value::Float(value))?;
            Ok(Flow::Next)
        }
        _ => Err(ExecError::BadFloatString(text)),
    }
}

/// `a2fix` (0x67): parse `arg1`'s string into integer register `arg0`.
///
/// Uses `StringToValue` (an unparseable string yields 0 — the opcode's defined
/// behavior), clamps to the reference's `[i32::MIN, 0xFFFF_FFFF]` band, writes
/// the low 32 bits, and forces Zero/Sign/Overflow false (EdOperations.cs:22).
fn op_a2fix(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let text = read_string(m, "a2fix", &op.arg1)?;
    let mut value = string_to_value(&text);
    if value < i64::from(i32::MIN) {
        value = i64::from(i32::MIN);
    }
    if value > i64::from(i32::MAX) {
        value = 0xFFFF_FFFF;
    }
    m.write(&op.arg0, Value::Int(i64::from(value as u32)))?;
    m.flags.z = false;
    m.flags.s = false;
    m.flags.v = false;
    Ok(Flow::Next)
}

/// `fix2flt` (0x68): sign-extend integer register `arg1` by its width and store
/// it in `arg0`'s `F` register as a float (EdOperations.cs:718).
///
/// An immediate `arg1` is an [`ExecError::InvalidOperand`]: the decoder collapses
/// an immediate's 8/16/32-bit width, so its sign-extension width is unrecoverable
/// (the same limitation [`arg_width`] documents). Touches no flags.
fn op_fix2flt(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arg_width("fix2flt", &op.arg1)?;
    let raw = read_int(m, "fix2flt", &op.arg1, len)?;
    let value = match len {
        1 => f64::from(raw as u8 as i8),
        2 => f64::from(raw as u16 as i16),
        _ => f64::from(raw as i32),
    };
    m.write(&op.arg0, Value::Float(value))?;
    Ok(Flow::Next)
}

/// `flt2fix` (0x96): truncate float `arg1` toward zero into integer `arg0`.
///
/// The reference casts `(EdValueType)value` — a truncation, not a round — then
/// runs `Overflow = false; UpdateFlags(result, 4)` (EdOperations.cs:811). A
/// non-finite `arg1` has no integer image and is a hard [`ExecError::NonFinite`]
/// (no-degrade; it also avoids the platform-dependent `(uint)NaN`). Finite values
/// truncate toward zero and keep the low 32 bits.
fn op_flt2fix(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let value = read_float(m, "flt2fix", &op.arg1)?;
    if !value.is_finite() {
        return Err(ExecError::NonFinite("flt2fix"));
    }
    // `value as i64` truncates toward zero; the low 32 bits mirror `(uint32)`.
    let result = value as i64 as u32;
    m.write(&op.arg0, Value::Int(i64::from(result)))?;
    m.flags.v = false;
    update_zs(&mut m.flags, result, 4);
    Ok(Flow::Next)
}

/// `flt2a` (0x87): format float `arg1` into a string in `arg0`'s `S` register.
///
/// Rounds to [`FLOAT_PRECISION`] significant digits, formats, then keeps only up
/// to the `FLOAT_PRECISION`-th digit character (EdOperations.cs:781). A
/// non-finite `arg1` is a hard [`ExecError::NonFinite`] (no-degrade; a non-finite
/// has no faithful decimal text across runtimes). Touches no flags.
fn op_flt2a(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let value = read_float(m, "flt2a", &op.arg1)?;
    if !value.is_finite() {
        return Err(ExecError::NonFinite("flt2a"));
    }
    let formatted = format!(
        "{}",
        round_to_significant_digits(value, FLOAT_PRECISION as i32)
    );
    let mut digit_count = 0;
    let mut cut = formatted.len();
    for (idx, ch) in formatted.char_indices() {
        if ch.is_ascii_digit() {
            digit_count += 1;
            if digit_count >= FLOAT_PRECISION {
                cut = idx + ch.len_utf8();
                break;
            }
        }
    }
    write_string(m, &op.arg0, &formatted[..cut])?;
    Ok(Flow::Next)
}

/// `a2y` (0x8C): parse `arg1`'s hex-token string into bytes in `arg0`'s `S` reg.
///
/// Mirrors `OpA2Y` (EdOperations.cs:76): truncate at the first char outside the
/// hex-digit / space / `;` / `,` set, split on `,` and `;`, emit `len+1` zero
/// bytes for an empty field, otherwise parse each space-separated token as a hex
/// byte; a token that is not a valid hex byte stops the scan. Touches no flags.
fn op_a2y(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let text = read_string(m, "a2y", &op.arg1)?;
    let mut result: Vec<u8> = Vec::new();
    if !text.is_empty() {
        // The accepted-char scan runs on the lowercased string; for these ASCII
        // characters a char index equals the matching byte index in `text`.
        let lower = text.to_ascii_lowercase();
        let end = lower
            .char_indices()
            .find(|&(_, c)| {
                !(c.is_ascii_digit()
                    || ('a'..='f').contains(&c)
                    || c == ' '
                    || c == ';'
                    || c == ',')
            })
            .map_or(text.len(), |(i, _)| i);
        'outer: for field in text[..end].split([',', ';']) {
            if field.trim().is_empty() {
                result.extend(std::iter::repeat_n(0u8, field.chars().count() + 1));
            } else {
                for token in field.trim().split(' ') {
                    if !token.is_empty() {
                        match u8::from_str_radix(token, 16) {
                            Ok(byte) => result.push(byte),
                            Err(_) => break 'outer,
                        }
                    }
                }
            }
        }
    }
    m.write(&op.arg0, Value::Bytes(result))?;
    Ok(Flow::Next)
}

/// `hex2y` (0x8E): decode `arg1`'s hex string into bytes in `arg0`'s `S` reg.
///
/// `HexToByteArray` keeps complete hex-digit pairs; success clears Carry, and any
/// invalid pair yields an empty result and sets Carry (EdOperations.cs:968). Carry
/// is the reference's failure channel here, not a hard error.
fn op_hex2y(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let text = read_string(m, "hex2y", &op.arg1)?;
    let (bytes, bad) = hex_to_bytes(&text).map_or((Vec::new(), true), |b| (b, false));
    m.write(&op.arg0, Value::Bytes(bytes))?;
    m.flags.c = bad;
    Ok(Flow::Next)
}

/// `y2bcd` (0x91): render each byte of `arg1` as two BCD nibbles into `arg0`.
///
/// Each nibble 0-9 becomes its digit; a nibble `> 9` becomes `*`
/// (EdOperations.cs:2749, EdiabasNet.cs:7303). Touches no flags.
fn op_y2bcd(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let bytes = read_bytes(m, "y2bcd", &op.arg1)?;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(bcd_nibble(byte >> 4));
        text.push(bcd_nibble(byte & 0x0F));
    }
    write_string(m, &op.arg0, &text)?;
    Ok(Flow::Next)
}

/// `y2hex` (0x92): render each byte of `arg1` as two uppercase hex digits.
///
/// Writes the result into `arg0`'s `S` register (EdOperations.cs:2766). Touches
/// no flags.
fn op_y2hex(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let bytes = read_bytes(m, "y2hex", &op.arg1)?;
    let mut text = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        text.push(hex_digit(byte >> 4));
        text.push(hex_digit(byte & 0x0F));
    }
    write_string(m, &op.arg0, &text)?;
    Ok(Flow::Next)
}

/// `y42flt`/`y82flt` (0x9D/0x9E): reinterpret the first `width` bytes of `arg1`
/// as a little-endian IEEE-754 float and store it in `arg0`'s `F` register.
///
/// The reference reads "intel byte order" (little-endian) via `BitConverter`
/// (EdOperations.cs:2715/2732); `y42flt` widens the `f32` to `f64`. Too few bytes
/// is a hard [`ExecError::InvalidOperand`]. The bits are stored verbatim,
/// including any non-finite value. Touches no flags.
fn op_y_to_flt(
    m: &mut Machine,
    mnemonic: &'static str,
    op: &Op,
    width: usize,
) -> Result<Flow, ExecError> {
    let bytes = read_bytes(m, mnemonic, &op.arg1)?;
    if bytes.len() < width {
        return Err(ExecError::InvalidOperand(mnemonic));
    }
    let value = if width == 4 {
        f64::from(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    } else {
        f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    };
    m.write(&op.arg0, Value::Float(value))?;
    Ok(Flow::Next)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decode::decode_job;
    use std::collections::HashMap;

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

    // ---- Task 8: control flow ----

    /// A minimal stand-in for Task 13's `run_job`: build an `offset → index` map
    /// from the decoded ops, then fetch the instruction at the machine's
    /// byte-offset PC, [`step`] it, and repeat until `eoj`. The iteration guard
    /// turns a non-terminating loop (a control-flow bug) into a test failure
    /// rather than a hang.
    fn drive(ops: &[Op]) -> Machine {
        let index: HashMap<usize, usize> =
            ops.iter().enumerate().map(|(i, o)| (o.offset, i)).collect();
        let mut m = Machine::new();
        let mut ctx = ExecCtx::default();
        for _ in 0..10_000 {
            let &i = index
                .get(&m.pc)
                .unwrap_or_else(|| panic!("pc {} is not an instruction boundary", m.pc));
            if step(&mut m, &ops[i], &mut ctx).unwrap() == Flow::EndOfJob {
                return m;
            }
        }
        panic!("job did not terminate within the iteration guard");
    }

    /// Steps a single jump `byte` against `flags`, returning the resulting
    /// [`Flow`] and PC. The jump sits at offset 0, length 6, with a `+4` relative
    /// target: post-instruction PC is 6, so a taken branch lands at 10 and an
    /// untaken one stays at 6.
    fn jump_flow(byte: u8, flags: Flags) -> (Flow, usize) {
        let mut m = Machine::new();
        m.flags = flags;
        let j = Op {
            byte,
            mode_byte: 0x70,
            arg0: Operand::Imm(4),
            arg1: Operand::None,
            len: 6,
            offset: 0,
        };
        let flow = step(&mut m, &j, &mut ExecCtx::default()).unwrap();
        (flow, m.pc)
    }

    #[test]
    fn countdown_loop_terminates_with_counter_zero() {
        // move I0,#3 ; (loop) subb I0,#1 ; jnz loop ; eoj
        // jnz at offset 8 len 6 -> post-PC 14; rel -10 (0xFFFFFFF6) -> back to 4.
        let code = [
            0x00, 0x35, 0x10, 0x03, // move I0, #3
            0x03, 0x35, 0x10, 0x01, // subb I0, #1     (loop top @ offset 4)
            0x11, 0x70, 0xF6, 0xFF, 0xFF, 0xFF, // jnz -10 -> offset 4
            0x1D, 0x00, // eoj
        ];
        let ops = decode_job(&code).unwrap();
        let m = drive(&ops);
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0));
    }

    #[test]
    fn unconditional_jump_skips_the_next_instruction() {
        // jump +4 (over the move) ; move I0,#0x7F (skipped) ; eoj
        let code = [
            0x0B, 0x70, 0x04, 0x00, 0x00, 0x00, // jump +4 -> offset 10 (eoj)
            0x00, 0x35, 0x10, 0x7F, // move I0, #0x7F  (offset 6, skipped)
            0x1D, 0x00, // eoj (offset 10)
        ];
        let ops = decode_job(&code).unwrap();
        let m = drive(&ops);
        // The move was jumped over, so I0 keeps its zero-initialised value.
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0));
    }

    #[test]
    fn jump_sets_pc_relative_to_following_instruction() {
        // Unconditional jump at offset 8, len 6 -> post-PC 14; rel +4 -> 18.
        let mut m = Machine::new();
        let j = Op {
            byte: 0x0B,
            mode_byte: 0x70,
            arg0: Operand::Imm(4),
            arg1: Operand::None,
            len: 6,
            offset: 8,
        };
        assert_eq!(
            step(&mut m, &j, &mut ExecCtx::default()).unwrap(),
            Flow::Jumped
        );
        assert_eq!(m.pc, 18);
    }

    #[test]
    fn backward_jump_uses_signed_negative_relative() {
        // jnz at offset 20, len 6 -> post-PC 26; arg0 = 0xFFFFFFF6 (raw) = -10.
        let mut m = Machine::new();
        m.flags.z = false; // !Z -> taken
        let j = Op {
            byte: 0x11,
            mode_byte: 0x70,
            arg0: Operand::Imm(i64::from(0xFFFF_FFF6_u32)),
            arg1: Operand::None,
            len: 6,
            offset: 20,
        };
        assert_eq!(
            step(&mut m, &j, &mut ExecCtx::default()).unwrap(),
            Flow::Jumped
        );
        assert_eq!(m.pc, 16); // 26 + (-10)
    }

    #[test]
    fn untaken_conditional_advances_past_the_instruction() {
        // jnz with Z set -> !Z false -> not taken -> PC = pre-advance only.
        let (flow, pc) = jump_flow(
            0x11,
            Flags {
                z: true,
                ..Flags::default()
            },
        );
        assert_eq!(flow, Flow::Next);
        assert_eq!(pc, 6);
    }

    #[test]
    fn single_flag_conditionals_branch_on_their_flag() {
        let z = Flags {
            z: true,
            ..Flags::default()
        };
        let nz = Flags::default();
        assert_eq!(jump_flow(0x10, z).0, Flow::Jumped); // jz taken when Z
        assert_eq!(jump_flow(0x10, nz).0, Flow::Next);
        assert_eq!(jump_flow(0x11, nz).0, Flow::Jumped); // jnz taken when !Z
        assert_eq!(jump_flow(0x11, z).0, Flow::Next);

        let c = Flags {
            c: true,
            ..Flags::default()
        };
        let nc = Flags::default();
        assert_eq!(jump_flow(0x0E, c).0, Flow::Jumped); // jc taken when C
        assert_eq!(jump_flow(0x0E, nc).0, Flow::Next);
        assert_eq!(jump_flow(0x0F, nc).0, Flow::Jumped); // jae taken when !C
        assert_eq!(jump_flow(0x0F, c).0, Flow::Next);

        let v = Flags {
            v: true,
            ..Flags::default()
        };
        assert_eq!(jump_flow(0x12, v).0, Flow::Jumped); // jv taken when V
        assert_eq!(jump_flow(0x13, v).0, Flow::Next); // jnv not taken when V

        let s = Flags {
            s: true,
            ..Flags::default()
        };
        assert_eq!(jump_flow(0x14, s).0, Flow::Jumped); // jmi taken when S
        assert_eq!(jump_flow(0x15, s).0, Flow::Next); // jpl not taken when S
    }

    #[test]
    fn combo_conditionals_follow_signed_and_unsigned_relations() {
        // Each row: a flag state -> expected "taken" for jg,jge,jl,jle,ja,jbe.
        let rows: [(Flags, [(u8, bool); 6]); 3] = [
            // "greater": Z=0 S=0 V=0 C=0
            (
                Flags::default(),
                [
                    (0x5A, true),
                    (0x5B, true),
                    (0x5C, false),
                    (0x5D, false),
                    (0x5E, true),
                    (0x5F, false),
                ],
            ),
            // "equal": Z=1
            (
                Flags {
                    z: true,
                    ..Flags::default()
                },
                [
                    (0x5A, false),
                    (0x5B, true),
                    (0x5C, false),
                    (0x5D, true),
                    (0x5E, false),
                    (0x5F, true),
                ],
            ),
            // signed "less" (S!=V) and unsigned "below" (C set): Z=0 S=1 V=0 C=1
            (
                Flags {
                    s: true,
                    c: true,
                    ..Flags::default()
                },
                [
                    (0x5A, false),
                    (0x5B, false),
                    (0x5C, true),
                    (0x5D, true),
                    (0x5E, false),
                    (0x5F, true),
                ],
            ),
        ];
        for (flags, expected) in rows {
            for (byte, want) in expected {
                let (flow, pc) = jump_flow(byte, flags);
                if want {
                    assert_eq!(
                        flow,
                        Flow::Jumped,
                        "op {byte:#04X} should jump for {flags:?}"
                    );
                    assert_eq!(pc, 10);
                } else {
                    assert_eq!(
                        flow,
                        Flow::Next,
                        "op {byte:#04X} should fall through for {flags:?}"
                    );
                    assert_eq!(pc, 6);
                }
            }
        }
    }

    #[test]
    fn eoj_signals_end_of_job() {
        let mut m = Machine::new();
        assert_eq!(
            step(
                &mut m,
                &op(0x1D, Operand::None, Operand::None),
                &mut ExecCtx::default()
            )
            .unwrap(),
            Flow::EndOfJob
        );
    }

    #[test]
    fn push_lays_bytes_least_significant_first() {
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(0x1234)).unwrap();
        run(&mut m, &op(0x1E, reg_i(0), Operand::None)); // push I0 (2 bytes)
        // LSB 0x34 pushed first (bottom), MSB 0x12 last (top).
        assert_eq!(m.data_stack, vec![0x34, 0x12]);
    }

    #[test]
    fn push_then_pop_roundtrips_word() {
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(0x1234)).unwrap();
        run(&mut m, &op(0x1E, reg_i(0), Operand::None)); // push I0
        m.write(&reg_i(0), Value::Int(0)).unwrap(); // clobber
        run(&mut m, &op(0x1F, reg_i(0), Operand::None)); // pop I0
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0x1234));
        assert!(m.data_stack.is_empty());
    }

    #[test]
    fn push_then_pop_roundtrips_long() {
        let mut m = Machine::new();
        m.write(&reg_l(0), Value::Int(0xDEAD_BEEF)).unwrap();
        run(&mut m, &op(0x1E, reg_l(0), Operand::None)); // push L0 (4 bytes)
        assert_eq!(m.data_stack, vec![0xEF, 0xBE, 0xAD, 0xDE]);
        m.write(&reg_l(0), Value::Int(0)).unwrap();
        run(&mut m, &op(0x1F, reg_l(0), Operand::None)); // pop L0
        assert_eq!(m.read(&reg_l(0)).unwrap(), Value::Int(0xDEAD_BEEF));
        assert!(m.data_stack.is_empty());
    }

    #[test]
    fn pop_from_empty_stack_is_hard_error() {
        let mut m = Machine::new();
        assert_eq!(
            step(
                &mut m,
                &op(0x1F, reg_i(0), Operand::None),
                &mut ExecCtx::default()
            ),
            Err(ExecError::StackUnderflow)
        );
    }

    #[test]
    fn pop_wider_than_stack_is_hard_error() {
        // Push one byte, then pop a two-byte word: one byte short -> underflow.
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0x42)).unwrap();
        run(&mut m, &op(0x1E, reg_b(0), Operand::None)); // push 1 byte
        assert_eq!(
            step(
                &mut m,
                &op(0x1F, reg_i(0), Operand::None),
                &mut ExecCtx::default()
            ),
            Err(ExecError::StackUnderflow)
        );
    }

    #[test]
    fn pop_updates_zero_flag_and_clears_overflow() {
        // On success EDIABAS runs `Overflow = false; UpdateFlags(value, length)`
        // (OpPop, EdOperations.cs:1945-1947): popping zero sets Z and clears V.
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(0)).unwrap();
        run(&mut m, &op(0x1E, reg_i(0), Operand::None)); // push 0x0000
        m.flags.v = true; // ensure the pop actually clears Overflow
        run(&mut m, &op(0x1F, reg_i(1), Operand::None)); // pop into I1
        assert_eq!(m.read(&reg_i(1)).unwrap(), Value::Int(0));
        assert!(m.flags.z);
        assert!(!m.flags.v);

        // A nonzero pop clears Z.
        m.write(&reg_i(0), Value::Int(0x1234)).unwrap();
        run(&mut m, &op(0x1E, reg_i(0), Operand::None)); // push 0x1234
        run(&mut m, &op(0x1F, reg_i(1), Operand::None)); // pop into I1
        assert_eq!(m.read(&reg_i(1)).unwrap(), Value::Int(0x1234));
        assert!(!m.flags.z);
    }

    #[test]
    fn break_is_a_hard_user_break_error() {
        // `break` (0x4B) is EDIABAS's user-break: it raises EDIABAS_BIP_0008 and
        // aborts the job, touching no flag. It must be a loud hard error, never a
        // silent continue that would run past the break (EdOperations.cs:338-341).
        let mut m = Machine::new();
        m.flags.c = true;
        assert_eq!(
            step(
                &mut m,
                &op(0x4B, Operand::None, Operand::None),
                &mut ExecCtx::default()
            ),
            Err(ExecError::Break)
        );
        // No flag was touched — Carry survives the aborted instruction.
        assert!(m.flags.c);
    }

    #[test]
    fn jtsr_and_ret_are_loud_unimplemented() {
        let mut m = Machine::new();
        assert_eq!(
            step(
                &mut m,
                &op(0x0C, Operand::Imm(0), Operand::None),
                &mut ExecCtx::default()
            ),
            Err(ExecError::Unimplemented("jtsr"))
        );
        assert_eq!(
            step(
                &mut m,
                &op(0x0D, Operand::None, Operand::None),
                &mut ExecCtx::default()
            ),
            Err(ExecError::Unimplemented("ret"))
        );
    }

    #[test]
    fn error_trap_jumps_are_loud_unimplemented() {
        let mut m = Machine::new();
        assert_eq!(
            step(
                &mut m,
                &op(0x47, Operand::Imm(0), Operand::None),
                &mut ExecCtx::default()
            ),
            Err(ExecError::Unimplemented("jt"))
        );
        assert_eq!(
            step(
                &mut m,
                &op(0x48, Operand::Imm(0), Operand::None),
                &mut ExecCtx::default()
            ),
            Err(ExecError::Unimplemented("jnt"))
        );
    }

    // ---- Task 9: float arithmetic and byte/number conversions ----

    /// A float register `F<idx>` operand.
    fn reg_f(idx: u8) -> Operand {
        Operand::Reg {
            bank: RegBank::F,
            idx,
        }
    }
    /// A string / byte-buffer register `S<idx>` operand.
    fn reg_s(idx: u8) -> Operand {
        Operand::Reg {
            bank: RegBank::S,
            idx,
        }
    }
    fn op_a2flt(a0: Operand, a1: Operand) -> Op {
        op(0x3A, a0, a1)
    }
    fn op_fadd(a0: Operand, a1: Operand) -> Op {
        op(0x3B, a0, a1)
    }
    fn op_fsub(a0: Operand, a1: Operand) -> Op {
        op(0x3C, a0, a1)
    }
    fn op_fmul(a0: Operand, a1: Operand) -> Op {
        op(0x3D, a0, a1)
    }
    fn op_fdiv(a0: Operand, a1: Operand) -> Op {
        op(0x3E, a0, a1)
    }
    fn op_a2fix(a0: Operand, a1: Operand) -> Op {
        op(0x67, a0, a1)
    }
    fn op_fix2flt(a0: Operand, a1: Operand) -> Op {
        op(0x68, a0, a1)
    }
    fn op_flt2a(a0: Operand, a1: Operand) -> Op {
        op(0x87, a0, a1)
    }
    fn op_a2y(a0: Operand, a1: Operand) -> Op {
        op(0x8C, a0, a1)
    }
    fn op_hex2y(a0: Operand, a1: Operand) -> Op {
        op(0x8E, a0, a1)
    }
    fn op_y2bcd(a0: Operand, a1: Operand) -> Op {
        op(0x91, a0, a1)
    }
    fn op_y2hex(a0: Operand, a1: Operand) -> Op {
        op(0x92, a0, a1)
    }
    fn op_flt2fix(a0: Operand, a1: Operand) -> Op {
        op(0x96, a0, a1)
    }
    fn op_y42flt(a0: Operand, a1: Operand) -> Op {
        op(0x9D, a0, a1)
    }
    fn op_y82flt(a0: Operand, a1: Operand) -> Op {
        op(0x9E, a0, a1)
    }

    #[test]
    fn fmul_then_fadd_scales_engine_temp() {
        // F0 = raw 3631, F1 = 0.1  ; fmul -> 363.1 ; then fadd offset -273.14 -> 89.96
        let mut m = Machine::new();
        m.f[0] = 3631.0;
        m.f[1] = 0.1;
        step(
            &mut m,
            &op_fmul(reg_f(1), reg_f(0)),
            &mut ExecCtx::default(),
        )
        .unwrap(); // F1 *= F0
        assert!((m.f[1] - 363.1).abs() < 1e-6);
        m.f[0] = -273.14;
        step(
            &mut m,
            &op_fadd(reg_f(1), reg_f(0)),
            &mut ExecCtx::default(),
        )
        .unwrap();
        assert!((m.f[1] - 89.96).abs() < 1e-6);
    }

    #[test]
    fn fsub_subtracts_arg1_from_arg0() {
        let mut m = Machine::new();
        m.f[0] = 10.0;
        m.f[1] = 3.0;
        run(&mut m, &op_fsub(reg_f(0), reg_f(1)));
        assert!((m.f[0] - 7.0).abs() < 1e-9);
    }

    #[test]
    fn fdiv_divides_arg0_by_arg1() {
        let mut m = Machine::new();
        m.f[0] = 10.0;
        m.f[1] = 4.0;
        run(&mut m, &op_fdiv(reg_f(0), reg_f(1)));
        assert!((m.f[0] - 2.5).abs() < 1e-9);
    }

    #[test]
    fn fdiv_by_zero_is_a_hard_nonfinite_error() {
        // 1.0 / 0.0 -> +Inf: the reference raises EDIABAS_BIP_0011; no-degrade
        // makes it a hard error rather than storing an infinity.
        let mut m = Machine::new();
        m.f[0] = 1.0;
        m.f[1] = 0.0;
        assert_eq!(
            step(
                &mut m,
                &op_fdiv(reg_f(0), reg_f(1)),
                &mut ExecCtx::default()
            ),
            Err(ExecError::NonFinite("fdiv"))
        );
    }

    #[test]
    fn float_arithmetic_rejects_a_non_float_operand() {
        // fadd requires F-register operands; an integer register is invalid.
        let mut m = Machine::new();
        assert_eq!(
            step(
                &mut m,
                &op_fadd(reg_i(0), reg_f(0)),
                &mut ExecCtx::default()
            ),
            Err(ExecError::InvalidOperand("fadd"))
        );
    }

    #[test]
    fn a2flt_parses_ascii_digit_string() {
        // S0 holds ASCII "3631"; a2flt parses it into F0 as 3631.0.
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(b"3631".to_vec())).unwrap();
        run(&mut m, &op_a2flt(reg_f(0), reg_s(0)));
        assert!((m.f[0] - 3631.0).abs() < 1e-9);
    }

    #[test]
    fn a2flt_parses_decimal_comma_as_dot() {
        // EDIABAS StringToFloat replaces a decimal comma with a dot before parsing.
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(b"12,5".to_vec())).unwrap();
        run(&mut m, &op_a2flt(reg_f(0), reg_s(0)));
        assert!((m.f[0] - 12.5).abs() < 1e-9);
    }

    #[test]
    fn a2flt_unparseable_string_is_a_hard_error() {
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(b"nope".to_vec())).unwrap();
        assert_eq!(
            step(
                &mut m,
                &op_a2flt(reg_f(0), reg_s(0)),
                &mut ExecCtx::default()
            ),
            Err(ExecError::BadFloatString("nope".to_string()))
        );
    }

    #[test]
    fn a2fix_parses_string_into_integer_register() {
        // Decimal, hex (0x), and an unparseable string (-> 0, the defined behavior).
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(b"420".to_vec())).unwrap();
        run(&mut m, &op_a2fix(reg_i(0), reg_s(0)));
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(420));

        m.write(&reg_s(0), Value::Bytes(b"0xFF".to_vec())).unwrap();
        run(&mut m, &op_a2fix(reg_i(0), reg_s(0)));
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0xFF));

        m.write(&reg_s(0), Value::Bytes(b"junk".to_vec())).unwrap();
        run(&mut m, &op_a2fix(reg_i(0), reg_s(0)));
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0));
        assert!(!m.flags.z); // a2fix forces Zero/Sign/Overflow false regardless
    }

    #[test]
    fn fix2flt_sign_extends_integer_to_float() {
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(3631)).unwrap();
        run(&mut m, &op_fix2flt(reg_f(0), reg_i(0)));
        assert!((m.f[0] - 3631.0).abs() < 1e-9);

        // A byte register 0x80 is -128 once sign-extended by its 1-byte width.
        m.write(&reg_b(0), Value::Int(0x80)).unwrap();
        run(&mut m, &op_fix2flt(reg_f(1), reg_b(0)));
        assert!((m.f[1] - (-128.0)).abs() < 1e-9);
    }

    #[test]
    fn flt2fix_truncates_toward_zero() {
        // 89.96 truncates to 89 (truncation, not rounding to 90).
        let mut m = Machine::new();
        m.f[0] = 89.96;
        run(&mut m, &op_flt2fix(reg_l(0), reg_f(0)));
        assert_eq!(m.read(&reg_l(0)).unwrap(), Value::Int(89));
        assert!(!m.flags.z);
    }

    #[test]
    fn flt2fix_rejects_non_finite_input() {
        let mut m = Machine::new();
        m.f[0] = f64::INFINITY;
        assert_eq!(
            step(
                &mut m,
                &op_flt2fix(reg_l(0), reg_f(0)),
                &mut ExecCtx::default()
            ),
            Err(ExecError::NonFinite("flt2fix"))
        );
    }

    #[test]
    fn flt2a_formats_float_to_string() {
        // 1.5 rounds to itself at 4 significant digits -> "1.5", NUL-terminated.
        let mut m = Machine::new();
        m.f[0] = 1.5;
        run(&mut m, &op_flt2a(reg_s(0), reg_f(0)));
        assert_eq!(m.read(&reg_s(0)).unwrap(), Value::Bytes(b"1.5\0".to_vec()));
    }

    #[test]
    fn a2y_parses_space_and_comma_separated_hex() {
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(b"12 34,56".to_vec()))
            .unwrap();
        run(&mut m, &op_a2y(reg_s(1), reg_s(0)));
        assert_eq!(
            m.read(&reg_s(1)).unwrap(),
            Value::Bytes(vec![0x12, 0x34, 0x56])
        );
    }

    #[test]
    fn hex2y_decodes_hex_pairs_and_clears_carry() {
        let mut m = Machine::new();
        m.flags.c = true;
        m.write(&reg_s(0), Value::Bytes(b"0AFF".to_vec())).unwrap();
        run(&mut m, &op_hex2y(reg_s(1), reg_s(0)));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(vec![0x0A, 0xFF]));
        assert!(!m.flags.c);
    }

    #[test]
    fn hex2y_bad_hex_yields_empty_and_sets_carry() {
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(b"0AZZ".to_vec())).unwrap();
        run(&mut m, &op_hex2y(reg_s(1), reg_s(0)));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(vec![]));
        assert!(m.flags.c);
    }

    #[test]
    fn y2bcd_renders_nibbles_with_star_for_invalid() {
        // 0x12 -> "12"; 0x3A -> "3*" (the 0xA nibble is not a BCD digit).
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(vec![0x12, 0x3A])).unwrap();
        run(&mut m, &op_y2bcd(reg_s(1), reg_s(0)));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(b"123*\0".to_vec()));
    }

    #[test]
    fn y2hex_renders_uppercase_hex() {
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(vec![0x0A, 0xFF])).unwrap();
        run(&mut m, &op_y2hex(reg_s(1), reg_s(0)));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(b"0AFF\0".to_vec()));
    }

    #[test]
    fn y42flt_reads_little_endian_f32() {
        // "intel byte order": the 4 bytes are a little-endian IEEE-754 f32.
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(1.0f32.to_le_bytes().to_vec()))
            .unwrap();
        run(&mut m, &op_y42flt(reg_f(0), reg_s(0)));
        assert!((m.f[0] - 1.0).abs() < 1e-9);
    }

    #[test]
    fn y82flt_reads_little_endian_f64() {
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(2.5f64.to_le_bytes().to_vec()))
            .unwrap();
        run(&mut m, &op_y82flt(reg_f(0), reg_s(0)));
        assert!((m.f[0] - 2.5).abs() < 1e-12);
    }

    #[test]
    fn y42flt_too_few_bytes_is_a_hard_error() {
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(vec![0x00, 0x00])).unwrap();
        assert_eq!(
            step(
                &mut m,
                &op_y42flt(reg_f(0), reg_s(0)),
                &mut ExecCtx::default()
            ),
            Err(ExecError::InvalidOperand("y42flt"))
        );
    }
}
