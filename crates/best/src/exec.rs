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
//! `a2y`/`hex2y`, `y2bcd`/`y2hex`, `y42flt`/`y82flt`), the **string-buffer** ops
//! (`scmp`, `scat`, `scut`, `slen`, `spaste`, `serase`, `strcat`, `strcmp`,
//! `strlen`) with the in-place byte-reversing `swap` and writes through indexed
//! destinations, the **result-store** ops (`ergb`..`ergs`, `ergy`, `ergc`, `ergl`,
//! `enewset`, `etag`), the **param** reads of the job's input arguments
//! (`parb`/`parw`/`parl`, `pars`, `parr`, `pary`, `parn`), and the **table-cursor**
//! ops (`tabset`, `tabseek`/`tabseeku`, `tabget`, `tabline`, `tabcols`/`tabrows`)
//! with the data-stack peek `atsp`, and the **error-trap** subsystem
//! (`gettmr`/`settmr` — which move the trap *mask*, not a clock — `clrt`, and the
//! error-detected branches `jt`/`jnt`). Every other opcode byte returns
//! [`ExecError::Unimplemented`] until its task lands — including `jtsr`/`ret`,
//! which EDIABAS itself never runs here (see [`step`]).
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

use crate::decode::{AddrMode, IndexArg, Op, Operand, RegBank, RegId};
use crate::machine::{ARRAY_MAX_SIZE, Flags, Machine, MachineError, Value};
use crate::opcode::info;
use crate::result::{ResultData, ResultSet};
use klartext_sgbd::Table;

/// What executing one instruction tells the run loop to do next.
///
/// [`Flow::Exchange`] carries owned request bytes and a destination [`Operand`],
/// so `Flow` is [`Clone`] but not [`Copy`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Flow {
    /// Advance to the next sequential instruction.
    Next,
    /// A branch/call set the program counter; do not auto-advance.
    Jumped,
    /// The job's `eoj` was reached; stop executing.
    EndOfJob,
    /// An `xsend` (0x2A) built a request the run loop must transmit: send
    /// `request` to the ECU and write the response into `dest`. The sync executor
    /// only *describes* the exchange — the async transmit happens at the run-loop
    /// boundary (Task 13) — so `step` never awaits. The ECU target address is the
    /// run loop's knowledge, not the bytecode's, so it is not carried here.
    Exchange {
        /// The raw UDS request bytes built in the job's `S` register (`xsend`'s
        /// `arg1`).
        request: Vec<u8>,
        /// The register the response payload is written back into (`xsend`'s
        /// `arg0`).
        dest: Operand,
    },
    /// A `wait` (0x6B) asked the run loop to pause, then resume at the
    /// already-advanced PC.
    ///
    /// `seconds` is whole SECONDS: EDIABAS's `OpWait` sleeps `arg0 × 1000` ms
    /// (EdOperations.cs:3267), so `arg0` counts seconds (the millisecond
    /// `waitex` is a separate, out-of-scope opcode). The sleep is carried out of
    /// [`step`] and performed at the async run-loop boundary so the synchronous
    /// executor never blocks a thread — the reference sleeps inline only because
    /// its machine is synchronous.
    Wait {
        /// Whole seconds to pause before resuming the job.
        seconds: u32,
    },
}

/// External state threaded through execution, alongside the [`Machine`].
///
/// The arithmetic/logic/flag/float opcodes touch only the machine, but the
/// result-store, param, and (future) comm opcodes reach out here. This is the
/// executor's first real context extension:
///
/// * `results` is where the result-store ops (`ergb`..`ergy`) push named values
///   and where `enewset` starts a new set — Task 6's [`ResultSet`].
/// * `args` is the job's raw input-argument buffer (EDIABAS's single `BinData`).
///   The string param ops (`parb`/`parw`/`parl`, `parr`, `pars`, `parn`) decode
///   it as a Windows-1252/Latin-1 string and split it on `;` into fields; `pary`
///   reads it raw. It is empty for a job invoked with no arguments (the offline
///   Phase-1 oracle job takes none).
/// * `tables` is the SGBD's decoded tables (`SG_FUNKTIONEN`, the `RES_*`
///   sub-result tables, …); the `tab*` ops resolve names against it. Phase 1
///   carries only the variant `.prg`'s tables — the group `.grp` base file
///   (EDIABAS's `_sgbdBaseFs`) is deferred (spec §2).
/// * `current_table`/`current_row` are the table cursor EDIABAS keeps as
///   `_tableIndex`/`_tableRowIndex`: `tabset` selects the table and resets the
///   row; `tabseek`/`tabseeku`/`tabline` move the row. `None` is EDIABAS's `-1`
///   ("no table selected" / "no row selected"). The cursor persists across the
///   job's steps, so it lives here rather than on the [`Machine`].
///
/// Later tasks add the UDS exchange here.
#[derive(Debug)]
pub struct ExecCtx<'a> {
    /// The job's result sets; result-store ops push here, `enewset` splits here.
    pub results: &'a mut ResultSet,
    /// The job's raw input-argument buffer (see the type docs for how the param
    /// ops interpret it).
    pub args: &'a [u8],
    /// The SGBD's decoded tables (Phase 1: the variant `.prg`'s only), which the
    /// `tab*` ops resolve names against. Production threads `Prg::tables()` here.
    pub tables: &'a [Table],
    /// The selected table's index into [`ExecCtx::tables`], or `None` when no
    /// `tabset` has selected one (EDIABAS's `_tableIndex`, where `-1` = unselected).
    pub current_table: Option<usize>,
    /// The row cursor into the selected table's data rows, or `None`
    /// (EDIABAS's `_tableRowIndex = -1`): reset by `tabset`, set by
    /// `tabseek`/`tabseeku`/`tabline`.
    pub current_row: Option<usize>,
}

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
    /// `EDIABAS_BIP_0008` and aborts the job. No-degrade: a hard stop — `break`
    /// aborts directly and is not routed through the error-trap subsystem
    /// (`set_error`/`jt`), so a job's trap mask does not suppress it here.
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
    /// A `tab*` op ran with no table selected (EDIABAS's `_tableIndex < 0`), which
    /// the reference reports as `EDIABAS_BIP_0010`. No-degrade: a hard stop, never
    /// a silent empty/zero. (`tabcols`/`tabrows` are the deliberate exception —
    /// they report 0, faithfully, and never raise this.)
    #[error("opcode `{0}` ran with no table selected (EDIABAS_BIP_0010)")]
    TableNotSelected(&'static str),
    /// `tabset` named a table this SGBD does not contain. The reference falls back
    /// to the last table and raises `EDIABAS_BIP_0010`; no-degrade makes it a hard
    /// stop rather than selecting the wrong table.
    #[error("tabset: no table named `{0}` in this SGBD (EDIABAS_BIP_0010)")]
    TableNotFound(String),
    /// A `tab*` op named a column its selected table does not contain
    /// (EDIABAS_BIP_0010). No-degrade: a hard stop.
    #[error("opcode `{op}` names column `{column}` not in the current table (EDIABAS_BIP_0010)")]
    TableColumn {
        /// The opcode mnemonic that named the missing column.
        op: &'static str,
        /// The column name that was not found.
        column: String,
    },
    /// A `tab*` op could not select a valid data row — the selected row is out of
    /// range, or a `tabseek`/`tabline` on a table with no data rows (the
    /// reference's `rowIndex < 0` → `EDIABAS_BIP_0010`). No-degrade: a hard stop.
    #[error("opcode `{0}` could not select a valid data row (EDIABAS_BIP_0010)")]
    TableRow(&'static str),
    /// `atsp` (0x50) tried to read more bytes than the data stack holds, or from a
    /// position below the bytes available — EDIABAS's `EDIABAS_BIP_0005` /
    /// invalid-stack-index. No-degrade: a hard stop, never a zero-fill.
    #[error("opcode `atsp` read past the data stack (EDIABAS_BIP_0005)")]
    AtspStack,
    /// A recorded ECU/VM error whose trap bit the running job does not mask —
    /// EDIABAS's `SetError` → `RaiseError` abort (EdiabasNet.cs:4140-4166). The
    /// executor's `set_error` records the bit in the machine's `trap_bit` first,
    /// then raises this iff `(1 << bit)` is clear in its `trap_mask`; a job that
    /// masks the class via `settmr` keeps running and tests the bit with
    /// `jt`/`jnt`.
    #[error("an ECU/VM error raised trap bit {bit} and the job does not mask it")]
    Trapped {
        /// The recorded trap-bit number (the error's dictionary bit).
        bit: u32,
    },
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
/// (including the null-handled `jtsr`/`ret`),
/// [`ExecError::InvalidOperand`] for an operand the opcode cannot use,
/// [`ExecError::DivideByZero`] for a `divs` fault, [`ExecError::StackUnderflow`]
/// when `pop` outruns the data stack, [`ExecError::Break`] when a `break`
/// user-break instruction executes, and [`ExecError::Machine`] when an operand
/// read/write against the machine fails.
pub fn step(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    // EDIABAS advances `_pcCounter` to the byte just past this instruction before
    // running its handler; a taken jump then rewrites it to the target. Mirror
    // that here so the run loop and the jump handlers share one PC model
    // (EdiabasNet.cs:5816-5822).
    m.pc = op.offset + op.len;
    // IdxRegImm collapses its index-increment into the `len` slot at decode time
    // (crate::decode), indistinguishable from a Len-mode length once it reaches
    // an `Operand`. Fold it here — where `op.mode_byte` still names each operand's
    // addressing mode — so `Machine::read` resolves the intended to-buffer-end
    // slice (EdiabasNet.cs:273-295). The common no-`IdxRegImm` op clones nothing.
    let folded = fold_index_increments(m, op)?;
    let op = folded.as_ref().unwrap_or(op);
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
        // Task 4: the EDIABAS error-trap subsystem. `gettmr`/`settmr` are
        // misleadingly named — they move the trap MASK, not a clock — `clrt`
        // clears the recorded trap bit, and `jt`/`jnt` branch on it.
        0x43 => {
            // gettmr (EdOperations.cs:1279): read the trap MASK into arg0; Z/S update.
            let len = arg_width("gettmr", &op.arg0)?;
            m.write(&op.arg0, Value::Int(i64::from(m.trap_mask)))?;
            update_zs(&mut m.flags, m.trap_mask, len);
            Ok(Flow::Next)
        }
        0x44 => {
            // settmr (EdOperations.cs:2130): set the trap MASK from arg0.
            m.trap_mask = read_value_data(m, "settmr", &op.arg0)?;
            Ok(Flow::Next)
        }
        0x46 => {
            // clrt (EdOperations.cs:412): clear the recorded trap bit (EDIABAS's -1).
            m.trap_bit = None;
            Ok(Flow::Next)
        }
        0x47 => branch(m, "jt", op, trap_detected(m, op)?),
        0x48 => branch(m, "jnt", op, !trap_detected(m, op)?),
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
        0xA1 => op_fcomp(m, op),
        // `wait` (0x6B) surfaces an async pause to the run loop (its `arg0` is
        // whole seconds, EdOperations.cs:3267); the sleep stays out of `step`.
        // `fix2hex`/`fix2dez` format an integer into a `0x`-hex / signed-decimal
        // string sized by the source's width.
        0x6B => Ok(Flow::Wait {
            seconds: read_value_data(m, "wait", &op.arg0)?,
        }),
        0x79 => op_fix2(m, "fix2hex", op),
        0x7A => op_fix2(m, "fix2dez", op),
        // Task 10: string-buffer ops.
        0x20 => op_scmp(m, op),
        0x21 => op_scat(m, op),
        0x22 => op_scut(m, op),
        0x23 => op_slen(m, op),
        0x24 => op_spaste(m, op),
        0x25 => op_serase(m, op),
        0x7E => op_strcat(m, op),
        0x8F => op_strcmp(m, op),
        0x90 => op_strlen(m, op),
        // swap (0x51): the in-place byte reverse of an indexed S-register slice.
        0x51 => op_swap(m, op),
        // Task 10: result-store ops.
        0x34 => op_ergb(m, op, ctx),
        0x35 => op_ergw(m, op, ctx),
        0x36 => op_ergd(m, op, ctx),
        0x37 => op_ergi(m, op, ctx),
        0x38 => op_ergr(m, op, ctx),
        0x39 => op_ergs(m, op, ctx),
        0x3F => op_ergy(m, op, ctx),
        0x81 => op_ergc(m, op, ctx),
        0x82 => op_ergl(m, op, ctx),
        0x40 => op_enewset(ctx),
        0x41 => op_etag(),
        // Task 10: param reads of the job's input arguments. `parb`/`parw`/`parl`
        // share one handler (they differ only in `arg0`'s register width).
        0x55..=0x57 => op_parl(m, op, ctx),
        0x58 => op_pars(m, op, ctx),
        0x69 => op_parr(m, op, ctx),
        0x7F => op_pary(m, op, ctx),
        0x80 => op_parn(m, op, ctx),
        // Task 11: table-cursor ops + the data-stack peek `atsp`.
        0x50 => op_atsp(m, op),
        0x7B => op_tabset(m, op, ctx),
        0x7C => op_tabseek(m, op, ctx),
        0x7D => op_tabget(m, op, ctx),
        0x83 => op_tabline(m, op, ctx),
        0x9A => op_tabseeku(m, op, ctx),
        0xB6 => op_tabcols(m, op, ctx),
        0xB7 => op_tabrows(m, op, ctx),
        // Task 12: the comm bridge — the request/response exchange opcodes. The
        // async transmit lives at the run-loop boundary (Task 13); `step` only
        // surfaces the request/destination via `Flow::Exchange` and stays sync.
        0x2A => op_xsend(m, op),
        // xrequf (0x2C): `OpXrequf` (EdOperations.cs:3021) is "receive frequent" —
        // a streaming receive that repeatedly polls the interface (`ReceiveFrequent`)
        // for the next frame. It has no single request to key the mock on and no
        // place in the one-shot request/response `Flow::Exchange` model, so Phase 1
        // defers it as a loud `Unimplemented` (the offline oracle drives its reads
        // through `xsend`, not `xrequf`; streaming is a Phase 2 concern). Every
        // other `OpClass::Comm` opcode reaches the `Unimplemented` arm below.
        0x2C => Err(ExecError::Unimplemented("xrequf")),
        // `jtsr` (0x0C) / `ret` (0x0D) deliberately reach this `Unimplemented`
        // arm rather than getting a handler — this is faithful, not a gap:
        // EDIABAS registers null handlers for them and throws "not implemented"
        // if a job ever executes one (EdiabasNet.cs:5851-5853); modern jobs never
        // use them.
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
/// — is an [`ExecError::InvalidOperand`]. The arithmetic ops that also accept an
/// indexed target (`comp`/`adds`, whose EDIABAS `arg0.OpData1 is Register` gate
/// an indexed base passes) size it through [`arith_arg0_len`] instead.
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

/// Arithmetic `arg0` width in bytes, EDIABAS `GetArgsValueLength`
/// (`arg0.GetDataLen(true)`).
///
/// Extends [`arg_width`] with the no-length indexed targets EDIABAS accepts:
/// `IdxImm`/`IdxReg`/`IdxRegImm` (an [`Operand::Indexed`] with `len: None` after
/// the executor's increment fold), whose write-mode `GetDataLen(true)` is **1**
/// (EdiabasNet.cs:191-206). An indexed operand passes EDIABAS's
/// `arg0.OpData1 is Register` gate because its base is a register, so `comp`/
/// `adds` may target a byte inside an `S` buffer (`adds S1[0], B6` in the real
/// F20 DDE jobs). A `B`/`I`/`L` register keeps its bank width; every other shape
/// — an immediate, an `S`/float register, or a length-bearing indexed target —
/// stays a loud [`ExecError::InvalidOperand`], as no executed job uses one as an
/// arithmetic target.
fn arith_arg0_len(mnemonic: &'static str, arg0: &Operand) -> Result<u32, ExecError> {
    match arg0 {
        Operand::Indexed { len: None, .. } => Ok(1),
        _ => arg_width(mnemonic, arg0),
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
fn read_int(
    m: &mut Machine,
    mnemonic: &'static str,
    op: &Operand,
    len: u32,
) -> Result<u32, ExecError> {
    match read_source(m, op)? {
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

/// Reads a source operand, mapping an indexed out-of-bounds fault to EDIABAS's
/// `SetError(EDIABAS_BIP_0001)` + empty array.
///
/// [`Machine::read`] surfaces an over-`ArrayMaxSize` indexed reach as
/// [`MachineError::IndexOutOfBounds`]; the reference instead records the error
/// and returns `ByteArray0` from `GetRawData` (EdiabasNet.cs:283-289). Here that
/// becomes [`set_error`] with [`TRAP_BIT_UNMAPPED`] — which aborts the job unless
/// it masks the class — and, for a masking job, an empty [`Value::Bytes`] the
/// caller reads as zero/empty. Every other [`MachineError`] propagates unchanged.
///
/// # Errors
/// Returns [`ExecError::Trapped`] when the recorded bounds fault is unmasked, or
/// the propagated [`ExecError::Machine`] for any non-bounds machine error.
fn read_source(m: &mut Machine, op: &Operand) -> Result<Value, ExecError> {
    match m.read(op) {
        Ok(value) => Ok(value),
        Err(MachineError::IndexOutOfBounds { .. }) => {
            set_error(m, TRAP_BIT_UNMAPPED)?;
            Ok(Value::Bytes(Vec::new()))
        }
        Err(other) => Err(other.into()),
    }
}

/// Writes through an indexed destination, mapping an over-`ArrayMaxSize` reach
/// to EDIABAS's `SetError(EDIABAS_BIP_0001)` + skipped store.
///
/// The write-side mirror of [`read_source`]'s fault conversion:
/// [`Machine::write_indexed`] surfaces the bounds fault as
/// [`MachineError::IndexOutOfBounds`], where the reference's indexed
/// `SetRawData` records the error and returns without storing
/// (EdiabasNet.cs:536-541). Here that becomes [`set_error`] with
/// [`TRAP_BIT_UNMAPPED`] — aborting the job unless it masks the class — and,
/// for a masking job, `Ok` with the destination untouched, so the opcode's
/// post-write flag updates still run as the reference's handlers do. Every
/// other [`MachineError`] propagates unchanged.
///
/// # Errors
/// Returns [`ExecError::Trapped`] when the recorded bounds fault is unmasked,
/// or the propagated [`ExecError::Machine`] for any non-bounds machine error.
fn write_indexed_dest(
    m: &mut Machine,
    op: &Operand,
    value: &Value,
    len: usize,
) -> Result<(), ExecError> {
    match m.write_indexed(op, value, len) {
        Ok(()) => Ok(()),
        Err(MachineError::IndexOutOfBounds { .. }) => {
            set_error(m, TRAP_BIT_UNMAPPED)?;
            Ok(())
        }
        Err(other) => Err(other.into()),
    }
}

/// Folds any `IdxRegImm` operand of `op` into its no-length equivalent.
///
/// EDIABAS's `IdxRegImm` addressing mode reads `S[reg + increment ..]` to the
/// buffer's end (EdiabasNet.cs:273-295), but [`crate::decode`] stores that
/// increment in the same `len` slot a `Len`-mode length uses, so an
/// [`Operand::Indexed`] alone cannot tell the two apart. Here — where
/// `op.mode_byte`'s nibbles still name each operand's addressing mode — an
/// `IdxRegImm` operand's increment is added to its index and its `len` cleared,
/// yielding the to-buffer-end operand [`Machine::read`] resolves correctly.
///
/// Returns `Some` only when an operand was folded, so the common instruction
/// (no `IdxRegImm` operand) allocates nothing.
///
/// # Errors
/// Propagates the [`MachineError`] from reading the increment's index register.
fn fold_index_increments(m: &Machine, op: &Op) -> Result<Option<Op>, ExecError> {
    const IDX_REG_IMM: u8 = AddrMode::IdxRegImm as u8;
    let arg0_folds = (op.mode_byte >> 4) == IDX_REG_IMM;
    let arg1_folds = (op.mode_byte & 0x0F) == IDX_REG_IMM;
    if !arg0_folds && !arg1_folds {
        return Ok(None);
    }
    let mut folded = op.clone();
    if arg0_folds {
        folded.arg0 = fold_increment(m, &folded.arg0)?;
    }
    if arg1_folds {
        folded.arg1 = fold_increment(m, &folded.arg1)?;
    }
    Ok(Some(folded))
}

/// Rewrites one `IdxRegImm` operand — `Indexed { index, len: Some(increment) }`
/// — into the no-length `Indexed { index: index + increment, len: None }` the
/// machine reads to the buffer's end. Any other shape (never produced by the
/// decoder for this mode) passes through unchanged.
///
/// # Errors
/// Propagates the [`MachineError`] from resolving the index or the increment.
fn fold_increment(m: &Machine, operand: &Operand) -> Result<Operand, ExecError> {
    let Operand::Indexed {
        base,
        index,
        len: Some(increment),
    } = operand
    else {
        return Ok(operand.clone());
    };
    let effective = m.resolve_index(index)? + m.resolve_index(increment)?;
    Ok(Operand::Indexed {
        base: *base,
        index: IndexArg::Imm(effective as i64),
        len: None,
    })
}

// ---- opcode handlers ----

/// `move` (0x00): copy the source into `arg0`'s register.
///
/// Three target shapes are handled, all faithful to EDIABAS's `OpMove`
/// (EdOperations.cs:1289):
/// * an **integer** register (`B`/`I`/`L`) — Task 7's path — takes an integer
///   source masked to the target's width, clears Carry/Overflow, and sets Z/S.
/// * an **`S`** (byte-buffer) register — Task 12's path, needed to build an
///   `xsend` request — takes a byte-buffer source (an `S` register or a string
///   literal) and copies it over the FRONT of the destination, growing the
///   destination when the source is longer but KEEPING any tail beyond the
///   source's length (the reference's RegS partial overwrite, lines 1320-1329),
///   then clears Carry/Zero/Sign/Overflow.
/// * an **indexed `S` slice** (`S1[i]`-style destination): an integer source
///   stores exactly ONE little-endian byte at the index — the reference's
///   one-arg `SetRawData(value)` defaults `dataLen` to 1 (EdiabasNet.cs:441-444;
///   `GetDataLen(write: true)` is likewise 1 for the writable indexed modes,
///   EdiabasNet.cs:200-206) — then clears Carry/Overflow and sets Z/S from the
///   value at width 1 (`UpdateFlags(value, 1)`, EdOperations.cs:1310-1316). A
///   byte-buffer source stores all its own bytes at the index
///   (EdOperations.cs:1330-1334) and clears Carry/Zero/Sign/Overflow. An
///   over-`ArrayMaxSize` reach records `BIP_0001` and skips the store (see
///   [`write_indexed_dest`]).
///
/// An integer source into a plain `S` target (the reference's
/// `SetRawData(value)` on a whole-register array) is not built by any Phase-1
/// job and stays a hard [`ExecError::InvalidOperand`] via [`read_bytes`]; a
/// float target likewise errors via [`arg_width`].
fn op_move(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    if let Operand::Reg {
        bank: RegBank::S, ..
    } = op.arg0
    {
        let source = read_bytes(m, "move", &op.arg1)?;
        let mut dest = read_bytes(m, "move", &op.arg0)?;
        if dest.len() < source.len() {
            dest.resize(source.len(), 0);
        }
        dest[..source.len()].copy_from_slice(&source);
        m.write(&op.arg0, Value::Bytes(dest))?;
        m.flags.c = false;
        m.flags.z = false;
        m.flags.s = false;
        m.flags.v = false;
        return Ok(Flow::Next);
    }
    if let Operand::Indexed { .. } = op.arg0 {
        match read_source(m, &op.arg1)? {
            Value::Int(v) => {
                write_indexed_dest(m, &op.arg0, &Value::Int(v), 1)?;
                m.flags.c = false;
                m.flags.v = false;
                update_zs(&mut m.flags, v as u32, 1);
            }
            Value::Bytes(bytes) => {
                write_indexed_dest(m, &op.arg0, &Value::Bytes(bytes), 1)?;
                m.flags.c = false;
                m.flags.z = false;
                m.flags.s = false;
                m.flags.v = false;
            }
            Value::Float(_) => return Err(ExecError::InvalidOperand("move")),
        }
        return Ok(Flow::Next);
    }
    if let Operand::Reg {
        bank: RegBank::F, ..
    } = op.arg0
    {
        // A float-register TARGET. The generic framework moves floats with a
        // plain `move F<d>, F<s>` encoded in `RegS` addressing (not a
        // dedicated float opcode): `Operand.GetDataType` keys off the addressing
        // MODE — only `RegS` (with the `ImmStr`/`Idx*` modes) yields `byte[]`;
        // `RegAb` yields the integer type, whose branch throws on a float
        // register in the reference — so both operands read as `byte[]`, and
        // OpMove's byte-array branch
        // does `arg0.SetRawData(arg1.GetRawData())` — which, for float registers,
        // delegates to `Register.GetRawData`/`SetRawData` (EdiabasNet.cs:1715/1789)
        // and copies the float value, clearing Carry/Zero/Sign/Overflow
        // (EdOperations.cs:1306-1316, OpMove byte[]/byte[] case). The net effect is `F<d> =
        // F<s>`; a non-float source is not built by any Phase-1 job and stays a
        // loud [`ExecError::InvalidOperand`].
        let Value::Float(v) = read_source(m, &op.arg1)? else {
            return Err(ExecError::InvalidOperand("move"));
        };
        m.write(&op.arg0, Value::Float(v))?;
        m.flags.c = false;
        m.flags.z = false;
        m.flags.s = false;
        m.flags.v = false;
        return Ok(Flow::Next);
    }
    let len = arg_width("move", &op.arg0)?;
    let value = read_int(m, "move", &op.arg1, len)?;
    m.write(&op.arg0, Value::Int(i64::from(value)))?;
    m.flags.c = false;
    m.flags.v = false;
    update_zs(&mut m.flags, value, len);
    Ok(Flow::Next)
}

/// `swap` (0x51): byte-reverse the addressed `S`-register slice in place.
///
/// EDIABAS's `OpSwap` (EdOperations.cs:2406-2425) reverses `dataLen` bytes at
/// `startIdx` of the COMPLETE backing buffer (`GetArrayData(true)`) and stores
/// it back with `keepLength = true`, so the register's used LENGTH never
/// changes — a slice overrunning the used bytes pulls the buffer's zeros INTO
/// the used range (see [`Machine::swap_s_slice`] for the used-bytes-model
/// equivalent). A `startIdx + dataLen` reach past `ArrayMaxSize` records
/// `EDIABAS_BIP_0001` and skips the reverse (`SetError` + return), mirrored
/// here as [`set_error`] + [`Flow::Next`]. No flags are touched. The operand
/// must be an indexed one WITH a length: the reference reads the index and
/// length sub-operands directly (`OpData2`/`OpData3`) and faults on any other
/// shape, a hard [`ExecError::InvalidOperand`] here.
fn op_swap(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let Operand::Indexed {
        base,
        index,
        len: Some(len_arg),
    } = &op.arg0
    else {
        return Err(ExecError::InvalidOperand("swap"));
    };
    let start = m.resolve_index(index)?;
    let len = m.resolve_index(len_arg)?;
    if start + len > ARRAY_MAX_SIZE {
        set_error(m, TRAP_BIT_UNMAPPED)?;
        return Ok(Flow::Next);
    }
    m.swap_s_slice(base, start, len)?;
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
///
/// `arg0` may be an indexed `S`-register byte (`comp S1[0], B6`), sized through
/// [`arith_arg0_len`]; `comp` reads but never writes it, so no store routing is
/// needed.
fn op_comp(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arith_arg0_len("comp", &op.arg0)?;
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
///
/// `arg0` may be an indexed `S`-register byte (`adds S1[0], B6` in the real F20
/// DDE jobs), sized through [`arith_arg0_len`]. EDIABAS's `OpAdds` stores the
/// sum with the one-arg `SetRawData`, which defaults `dataLen` to 1 for an
/// indexed target (EdiabasNet.cs:441-444), so the store routes through
/// [`write_indexed_dest`] — recording `BIP_0001` and skipping on an
/// over-`ArrayMaxSize` reach, exactly as the reference does — while a register
/// target keeps its bank-width store.
fn op_adds(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = arith_arg0_len("adds", &op.arg0)?;
    let v0 = read_int(m, "adds", &op.arg0, len)?;
    let v1 = read_int(m, "adds", &op.arg1, len)?;
    let sum = u64::from(v0) + u64::from(v1);
    match &op.arg0 {
        Operand::Indexed { .. } => {
            write_indexed_dest(m, &op.arg0, &Value::Int(i64::from(sum as u32)), 1)?;
        }
        _ => m.write(&op.arg0, Value::Int(i64::from(sum as u32)))?,
    }
    update_zs(&mut m.flags, sum as u32, len);
    set_overflow(&mut m.flags, v0, v1, sum as u32, len);
    set_carry(&mut m.flags, sum, len);
    Ok(Flow::Next)
}

/// `mult` (0x05): signed product into `arg0` (low word) and, if `arg1` is a
/// register or an indexed `S` slice, its high word into `arg1`; `Overflow`
/// cleared, Z/S updated.
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
    // The high half of the product goes into arg1 when its leading datum is a
    // register — which includes an INDEXED arg1: OpMult stores it through the
    // width-carrying `SetRawData(resultHigh, len)` (EdOperations.cs:1740-1746),
    // i.e. `len` little-endian bytes at the index.
    let result_high = (u64::from(result) >> (len * 8)) as u32;
    match &op.arg1 {
        Operand::Reg { .. } => m.write(&op.arg1, Value::Int(i64::from(result_high)))?,
        Operand::Indexed { .. } => {
            write_indexed_dest(
                m,
                &op.arg1,
                &Value::Int(i64::from(result_high)),
                len as usize,
            )?;
        }
        _ => {}
    }
    Ok(Flow::Next)
}

/// `divs` (0x06): signed 32-bit quotient into `arg0`, remainder into `arg1` when
/// it is a register or an indexed `S` slice; `Overflow` cleared, Z/S updated. A
/// divide-by-zero or the signed `MIN / -1` overflow is a hard
/// [`ExecError::DivideByZero`].
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
    // The remainder goes into arg1 when its leading datum is a register —
    // which includes an INDEXED arg1: OpDivs stores it through the
    // width-carrying `SetRawData(remainder, len)` (EdOperations.cs:519-522).
    match &op.arg1 {
        Operand::Reg { .. } => m.write(&op.arg1, Value::Int(i64::from(remainder as u32)))?,
        Operand::Indexed { .. } => {
            write_indexed_dest(
                m,
                &op.arg1,
                &Value::Int(i64::from(remainder as u32)),
                len as usize,
            )?;
        }
        _ => {}
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

/// EDIABAS trap bit for an error absent from the trap-bit dictionary — `SetError`
/// records `0` for it (EdiabasNet.cs:4153), which `jt`/`jnt` then match via the
/// conventional test bit 32 (EdOperations.cs:1567).
pub(crate) const TRAP_BIT_UNMAPPED: u32 = 0;

/// EDIABAS trap bit for `IFH_0009` "no response from ECU": bit 19 in the trap-bit
/// dictionary (EdiabasNet.cs:3194). The run loop records this via [`set_error`]
/// when an ECU exchange fails at its [`crate::Flow::Exchange`] boundary.
pub(crate) const TRAP_BIT_NO_RESPONSE: u32 = 19;

/// The `jt`/`jnt` error-detected predicate (`OpJt`/`OpJnt`,
/// EdOperations.cs:1481-1591).
///
/// A test bit > 0 matches its exact recorded trap bit — with a recorded `0` (an
/// unmapped error) also answering to the conventional test bit 32 — while a
/// zero/absent test bit asks "any unclassifiable error", which the reference
/// expresses as `trap >= 0x40000000` (no dictionary bit ever reaches that value,
/// so it is ported literally). With no error recorded (`None` = EDIABAS's `-1`)
/// nothing is detected. `jt` (0x47) branches when this holds, `jnt` (0x48) when
/// it does not.
///
/// The unified predicate follows `OpJnt`'s no-argument branch for both ops; the
/// reference's `OpJt` no-argument branch differs (it jumps on *any* recorded
/// error, EdOperations.cs:1582), but a real job always encodes the test bit, so
/// the no-argument path is never exercised.
fn trap_detected(m: &Machine, op: &Op) -> Result<bool, ExecError> {
    let test_bit = match &op.arg1 {
        Operand::None => 0,
        arg1 => read_value_data(m, "jt", arg1)?,
    };
    Ok(match (m.trap_bit, test_bit) {
        (Some(trap), bit) if bit > 0 => trap == bit || (trap == 0 && bit == 32),
        (Some(trap), _) => trap >= 0x4000_0000,
        (None, _) => false,
    })
}

/// Records error trap `bit` and, unless the running job masks that class, aborts.
///
/// Faithful to EDIABAS `SetError` (EdiabasNet.cs:4140-4166): the bit is stored in
/// [`Machine::trap_bit`] *first* (so a later `jt`/`jnt` can test it), then the
/// abort — [`ExecError::Trapped`] — fires iff `(1 << bit)` is clear in
/// [`Machine::trap_mask`]. A job that masks the class via `settmr` gets `Ok` and
/// handles the fault itself. Trap-bit numbers come from EDIABAS's dictionary
/// (EdiabasNet.cs:3180-3210); see [`TRAP_BIT_UNMAPPED`]/[`TRAP_BIT_NO_RESPONSE`].
pub(crate) fn set_error(m: &mut Machine, bit: u32) -> Result<(), ExecError> {
    m.trap_bit = Some(bit);
    if (1u64 << bit) & !u64::from(m.trap_mask) != 0 {
        return Err(ExecError::Trapped { bit });
    }
    Ok(())
}

/// EDIABAS `Operand.GetDataLen()` (EdiabasNet.cs:174), the byte width of a value
/// operand, keyed on the operand's addressing mode.
///
/// A `B`/`I`/`L` register reports its bank width (1/2/4) mode-independently; an
/// immediate reports its encoded width — `Imm8`/`Imm16`/`Imm32` → 1/2/4 —
/// recovered from the operand's addressing-mode nibble (`mode_nibble`: the high
/// nibble of [`Op::mode_byte`] for `arg0`, the low for `arg1`), because the
/// decoder collapses an immediate's width into one [`Operand::Imm`]. This is
/// what lets `push #imm` and `fix2hex`/`fix2dez` size an immediate the way
/// EDIABAS's `GetDataLen` does, unlike [`arg_width`], which serves the
/// arithmetic targets that must be registers. Any other operand (an
/// `S`/float/indexed source) is an [`ExecError::InvalidOperand`]; no Phase-1
/// job pushes one.
fn value_data_len(mode_nibble: u8, op: &Operand, mnemonic: &'static str) -> Result<u32, ExecError> {
    match op {
        Operand::Reg {
            bank: RegBank::B, ..
        } => Ok(1),
        Operand::Reg {
            bank: RegBank::I, ..
        } => Ok(2),
        Operand::Reg {
            bank: RegBank::L, ..
        } => Ok(4),
        // Imm8 = 5, Imm16 = 6, Imm32 = 7 (the decoder's addressing-mode numbers).
        Operand::Imm(_) => match mode_nibble {
            5 => Ok(1),
            6 => Ok(2),
            7 => Ok(4),
            _ => Err(ExecError::InvalidOperand(mnemonic)),
        },
        _ => Err(ExecError::InvalidOperand(mnemonic)),
    }
}

/// `push` (0x1E): push `arg0`'s value onto the data stack, least-significant byte
/// first, for as many bytes as `arg0`'s [`value_data_len`] (`GetDataLen`).
///
/// `arg0` is a `B`/`I`/`L` register (bank width) or an `Imm8`/`Imm16`/`Imm32`
/// immediate (its encoded width, from the addressing mode) — the real DDE jobs
/// push both a `push #imm` count and a `push L0` register.
fn op_push(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let len = value_data_len(op.mode_byte >> 4, &op.arg0, "push")?;
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

// ---- Task 12: the comm bridge (request/response exchange) ----

/// `xsend` (0x2A): surface the request the run loop must transmit to the ECU.
///
/// EDIABAS's `OpXsend` (EdOperations.cs:3058) transmits `arg1`'s raw request
/// bytes (`GetArrayData` — the buffer a prior `move` built in an `S` register)
/// and writes the response into `arg0`'s register (`SetRawData`). The transmit is
/// async and belongs at the run-loop boundary (Task 13), so `step` stays sync: it
/// reads the request and returns [`Flow::Exchange`] describing the send and the
/// response destination, WITHOUT awaiting or touching the exchange. The run loop
/// performs `exchange.request(target, &request).await` and writes the response
/// back into `dest`. The ECU target address is the run loop's knowledge, not the
/// bytecode's, so [`Flow::Exchange`] does not carry it. A non-byte `arg1` (no
/// request buffer to send) is a hard [`ExecError::InvalidOperand`]. Touches no
/// flags here; the run loop writes the response bytes into `dest`. (EDIABAS's
/// `OpXsend` sets no flags either; a faithful post-exchange flag model, if any
/// live job needs one, is a Phase-2 concern for the run loop — TODO.)
fn op_xsend(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let request = read_bytes(m, "xsend", &op.arg1)?;
    Ok(Flow::Exchange {
        request,
        dest: op.arg0.clone(),
    })
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
fn read_bytes(m: &mut Machine, mnemonic: &'static str, op: &Operand) -> Result<Vec<u8>, ExecError> {
    match read_source(m, op)? {
        Value::Bytes(bytes) => Ok(bytes),
        _ => Err(ExecError::InvalidOperand(mnemonic)),
    }
}

/// Reads `op` as EDIABAS's NUL-terminated string: the buffer's bytes up to the
/// first `0x00`, each taken as a Latin-1 code point (EdiabasNet.cs:427).
fn read_string(m: &mut Machine, mnemonic: &'static str, op: &Operand) -> Result<String, ExecError> {
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

/// `fcomp` (0xA1): compare two `F`-register floats and set the condition flags.
///
/// EDIABAS's `OpFcomp` (EdOperations.cs:642-657): reads `arg0`/`arg1` as floats, sets
/// Zero when they are equal, Sign when `arg0 < arg1`, clears Overflow, and sets
/// Carry ONLY when the difference is non-finite (infinity/NaN) — leaving Carry
/// untouched in the ordinary finite case. No value is stored; this is the float
/// analogue of the integer [`op_comp`]. The generic framework uses it to branch
/// on scaled thresholds.
fn op_fcomp(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let v0 = read_float(m, "fcomp", &op.arg0)?;
    let v1 = read_float(m, "fcomp", &op.arg1)?;
    if !(v0 - v1).is_finite() {
        m.flags.c = true;
    }
    #[expect(
        clippy::float_cmp,
        reason = "faithful to OpFcomp's exact `val0 == val1` equality test"
    )]
    {
        m.flags.z = v0 == v1;
    }
    m.flags.s = v0 < v1;
    m.flags.v = false;
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

/// `fix2hex` (0x79) / `fix2dez` (0x7A): format integer `arg1` into a string in
/// `arg0`'s `S` register — `0x`-prefixed uppercase hex, or signed decimal.
///
/// The width follows the reference's rule
/// `arg1.GetDataType() != typeof(EdValueType) ? 1 : arg1.GetDataLen()`
/// (EdOperations.cs:694/757): a `B`/`I`/`L` register formats at its bank width
/// (1/2/4, via [`data_len`]), an immediate at its ENCODED width
/// (`Imm8`/`Imm16`/`Imm32` → 1/2/4, recovered from `arg1`'s addressing-mode
/// nibble via [`value_data_len`]), and any non-value source — an `S` register,
/// string literal, or indexed slice — at ONE byte, i.e. its first byte. At that
/// width `fix2dez` casts to `i8`/`i16`/`i32` and prints signed decimal, while
/// `fix2hex` prints `0x%02X`/`0x%04X`/`0x%08X`; [`write_string`] then appends
/// EDIABAS's NUL terminator. A float `arg1` errors in [`read_int`] and an
/// immediate without an `Imm` mode nibble (never decoder-built) in
/// [`value_data_len`] — both hard [`ExecError::InvalidOperand`]s, matching the
/// reference's throw on `GetValueData`. Touches no flags.
fn op_fix2(m: &mut Machine, mnemonic: &'static str, op: &Op) -> Result<Flow, ExecError> {
    let len = match &op.arg1 {
        Operand::Reg {
            bank: RegBank::B | RegBank::I | RegBank::L,
            ..
        } => data_len(m, mnemonic, &op.arg1)?,
        Operand::Imm(_) => value_data_len(op.mode_byte & 0x0F, &op.arg1, mnemonic)?,
        _ => 1,
    };
    let raw = read_int(m, mnemonic, &op.arg1, len)?;
    let text = match (mnemonic, len) {
        ("fix2dez", 1) => format!("{}", raw as u8 as i8),
        ("fix2dez", 2) => format!("{}", raw as u16 as i16),
        ("fix2dez", 4) => format!("{}", raw as i32),
        ("fix2hex", 1) => format!("0x{raw:02X}"),
        ("fix2hex", 2) => format!("0x{raw:04X}"),
        ("fix2hex", 4) => format!("0x{raw:08X}"),
        _ => return Err(ExecError::InvalidOperand(mnemonic)),
    };
    write_string(m, &op.arg0, &text)?;
    Ok(Flow::Next)
}

// ---- Task 10: string / result-store / param helpers ----

/// Reads `op` as EDIABAS `GetValueData()` with no explicit length: an integer
/// source (a `B`/`I`/`L` register or an immediate) yields its own-width value.
///
/// A byte-buffer or float source has no such reading — the reference's
/// `GetValueData(0)` throws (`dataLen 0` is invalid for an array, and a float is
/// the wrong raw type) — so it is a hard [`ExecError::InvalidOperand`] here
/// (EdiabasNet.cs:376). Used for the `len`/`pos`/index value operands of the
/// string and param ops.
fn read_value_data(m: &Machine, mnemonic: &'static str, op: &Operand) -> Result<u32, ExecError> {
    match m.read(op)? {
        Value::Int(v) => Ok(v as u32),
        _ => Err(ExecError::InvalidOperand(mnemonic)),
    }
}

/// EDIABAS `GetDataLen()` (EdiabasNet.cs:174): an integer register's byte width
/// (`B` = 1, `I` = 2, `L` = 4), or a byte-buffer operand's (`S` register or
/// string literal) current raw length — including any NUL terminator and any
/// bytes past it.
///
/// An immediate's width is collapsed by the decoder (the same limitation
/// [`arg_width`] documents) and a float has no data length, so both — and the
/// not-yet-wired [`Operand::Indexed`] — are a hard [`ExecError::InvalidOperand`].
/// This is `slen`'s length source, and it is deliberately distinct from the
/// NUL-terminated string length `strlen` uses.
fn data_len(m: &Machine, mnemonic: &'static str, op: &Operand) -> Result<u32, ExecError> {
    match op {
        Operand::Reg {
            bank: RegBank::B, ..
        } => Ok(1),
        Operand::Reg {
            bank: RegBank::I, ..
        } => Ok(2),
        Operand::Reg {
            bank: RegBank::L, ..
        } => Ok(4),
        Operand::Reg {
            bank: RegBank::S, ..
        }
        | Operand::Str(_) => match m.read(op)? {
            Value::Bytes(bytes) => Ok(bytes.len() as u32),
            _ => Err(ExecError::InvalidOperand(mnemonic)),
        },
        _ => Err(ExecError::InvalidOperand(mnemonic)),
    }
}

/// Resolves the base register and start index of an `IdxImm`/`IdxReg` operand —
/// the addressing shape `serase`/`spaste` require.
///
/// Those ops read the start position from `arg0`'s own addressing mode
/// (EdOperations.cs:2067/2198) and reject any other mode. A plain register, a
/// length-carrying indexed mode, or anything else is a hard
/// [`ExecError::InvalidOperand`]. A register index sub-operand is read at its
/// natural width via [`read_value_data`].
fn indexed_base_and_start(
    m: &Machine,
    mnemonic: &'static str,
    op: &Operand,
) -> Result<(RegId, u32), ExecError> {
    match op {
        Operand::Indexed {
            base,
            index,
            len: None,
        } => {
            let start = match index {
                IndexArg::Imm(v) => *v as u32,
                IndexArg::Reg(r) => read_value_data(
                    m,
                    mnemonic,
                    &Operand::Reg {
                        bank: r.bank,
                        idx: r.idx,
                    },
                )?,
            };
            Ok((*base, start))
        }
        _ => Err(ExecError::InvalidOperand(mnemonic)),
    }
}

/// EDIABAS `GetActiveArgStrings` (EdiabasNet.cs:2727): the job's argument buffer
/// decoded as a Windows-1252/Latin-1 string and split on `;` into fields.
///
/// An empty buffer yields no fields; a buffer with no `;` yields a single field.
/// The whole buffer is decoded (a NUL byte is not a terminator here, unlike
/// [`read_string`]). Job argument strings are ASCII in practice, for which the
/// 1252 and Latin-1 decodings agree.
fn arg_strings(args: &[u8]) -> Vec<String> {
    if args.is_empty() {
        return Vec::new();
    }
    let text: String = args.iter().map(|&b| char::from(b)).collect();
    text.split(';').map(str::to_string).collect()
}

/// EDIABAS `StringToFloat` (EdiabasNet.cs:7263): parse a decimal (comma or dot)
/// float, returning 0 on any failure — the opcode's defined behavior, not a
/// guess. A non-ASCII string fails the reference's ASCII round-trip and yields 0.
///
/// Unlike `a2flt` (which raises an error on an unparseable string), the `parr`
/// caller uses this return-0 behavior and signals presence through the Zero flag
/// instead, so this never errors.
fn string_to_float(number: &str) -> f64 {
    if !number.is_ascii() {
        return 0.0;
    }
    number
        .replace(',', ".")
        .trim()
        .parse::<f64>()
        .unwrap_or(0.0)
}

// ---- Task 10: result-store handlers ----
//
// Each `ergX` op takes the result NAME from `arg0` (`GetStringData`) and the
// value from `arg1`, then appends a typed [`ResultData`] to the current set.
// The width/signedness of the value is fixed per opcode by EDIABAS's cast in
// `OpErgX` (EdOperations.cs:551-593); the `read_int(..) as <cast>` below
// reproduces that cast exactly. None of them touch the flags.

/// `ergb` (0x34): store `arg1`'s low byte as an unsigned [`ResultData::Byte`].
fn op_ergb(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let name = read_string(m, "ergb", &op.arg0)?;
    let value = read_int(m, "ergb", &op.arg1, 1)? as u8;
    ctx.results.push_named(&name, ResultData::Byte(value));
    Ok(Flow::Next)
}

/// `ergw` (0x35): store `arg1`'s low word as an unsigned [`ResultData::Word`].
fn op_ergw(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let name = read_string(m, "ergw", &op.arg0)?;
    let value = read_int(m, "ergw", &op.arg1, 2)? as u16;
    ctx.results.push_named(&name, ResultData::Word(value));
    Ok(Flow::Next)
}

/// `ergd` (0x36): store `arg1`'s low dword as an unsigned [`ResultData::Dword`].
fn op_ergd(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let name = read_string(m, "ergd", &op.arg0)?;
    let value = read_int(m, "ergd", &op.arg1, 4)?;
    ctx.results.push_named(&name, ResultData::Dword(value));
    Ok(Flow::Next)
}

/// `ergi` (0x37): store `arg1`'s low word as a **signed** 16-bit
/// [`ResultData::Int`], sign-extended to `i64`.
fn op_ergi(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let name = read_string(m, "ergi", &op.arg0)?;
    let value = i64::from(read_int(m, "ergi", &op.arg1, 2)? as u16 as i16);
    ctx.results.push_named(&name, ResultData::Int(value));
    Ok(Flow::Next)
}

/// `ergr` (0x38): store `arg1`'s float value as a [`ResultData::Real`].
fn op_ergr(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let name = read_string(m, "ergr", &op.arg0)?;
    let value = read_float(m, "ergr", &op.arg1)?;
    ctx.results.push_named(&name, ResultData::Real(value));
    Ok(Flow::Next)
}

/// `ergs` (0x39): store `arg1`'s NUL-terminated string as a [`ResultData::Text`].
fn op_ergs(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let name = read_string(m, "ergs", &op.arg0)?;
    let value = read_string(m, "ergs", &op.arg1)?;
    ctx.results.push_named(&name, ResultData::Text(value));
    Ok(Flow::Next)
}

/// `ergy` (0x3F): store `arg1`'s raw byte array as a [`ResultData::Binary`].
fn op_ergy(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let name = read_string(m, "ergy", &op.arg0)?;
    let value = read_bytes(m, "ergy", &op.arg1)?;
    ctx.results.push_named(&name, ResultData::Binary(value));
    Ok(Flow::Next)
}

/// `ergc` (0x81): store `arg1`'s low byte as a **signed** 8-bit
/// [`ResultData::Int`], sign-extended to `i64`.
fn op_ergc(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let name = read_string(m, "ergc", &op.arg0)?;
    let value = i64::from(read_int(m, "ergc", &op.arg1, 1)? as u8 as i8);
    ctx.results.push_named(&name, ResultData::Int(value));
    Ok(Flow::Next)
}

/// `ergl` (0x82): store `arg1`'s low dword as a **signed** 32-bit
/// [`ResultData::Int`], sign-extended to `i64`.
fn op_ergl(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let name = read_string(m, "ergl", &op.arg0)?;
    let value = i64::from(read_int(m, "ergl", &op.arg1, 4)? as i32);
    ctx.results.push_named(&name, ResultData::Int(value));
    Ok(Flow::Next)
}

/// `enewset` (0x40): commit the current result set and start a fresh one.
///
/// EDIABAS commits only a **non-empty** set (OpEnewset, EdOperations.cs:542-548:
/// guarded on `_resultDict.Count > 0`), so no two consecutive empty sets are
/// ever produced. Task 6's [`ResultSet::new_set`] is unconditional, so this
/// guards it on the current set having at least one entry. Touches no flags.
fn op_enewset(ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    if ctx.results.iter_current().next().is_some() {
        ctx.results.new_set();
    }
    Ok(Flow::Next)
}

/// `etag` (0x41): a Phase-1 no-op fall-through.
///
/// EDIABAS's `etag` jumps past a result block (to `arg0`'s near-address) only
/// when the caller's requested-results filter is non-empty AND `arg1`'s tag is
/// not in it (OpEtag, EdOperations.cs:990-1002). Offline Phase 1 has no
/// result-request filter (`_resultsRequestDict` is always empty), so the guard is
/// never entered: `etag` falls through and every result is emitted. If a
/// result-request filter is ever added, this must jump (to `arg0`) when the tag
/// is not requested. Touches no flags.
fn op_etag() -> Result<Flow, ExecError> {
    Ok(Flow::Next)
}

// ---- Task 10: param handlers (read the job's input arguments) ----

/// `parb`/`parw`/`parl` (0x55-0x57): read job arg string at 1-based position
/// `arg1`, parse it via `StringToValue`, and write it into integer register
/// `arg0` (truncated to `arg0`'s width by the register write).
///
/// The three opcodes share EDIABAS's `OpParl` (EdOperations.cs:1765), differing
/// only in `arg0`'s register width. Zero is **set** when the argument is absent
/// or empty and **cleared** when a value is read — the flag jobs test to detect a
/// missing parameter; Carry/Sign/Overflow are cleared. A `pos` of 0 (1-based)
/// underflows to a huge index and reads as absent, per the reference.
fn op_parl(m: &mut Machine, op: &Op, ctx: &ExecCtx<'_>) -> Result<Flow, ExecError> {
    let pos = read_value_data(m, "parl", &op.arg1)?;
    let args = arg_strings(ctx.args);
    let mut result: u32 = 0;
    let mut found = false;
    if let Some(field) = args.get(pos.wrapping_sub(1) as usize)
        && !field.is_empty()
    {
        result = string_to_value(field) as u32;
        found = true;
    }
    m.write(&op.arg0, Value::Int(i64::from(result)))?;
    m.flags.z = !found;
    m.flags.c = false;
    m.flags.s = false;
    m.flags.v = false;
    Ok(Flow::Next)
}

/// `parr` (0x69): read job arg string at 1-based position `arg1`, parse it via
/// `StringToFloat`, and write it into `arg0`'s `F` register.
///
/// Per `OpParr` (EdOperations.cs:1808): Zero is set when the argument is absent
/// or empty and cleared when one is read (an unparseable-but-present argument
/// still clears Zero and stores 0.0); Carry/Sign/Overflow are cleared.
fn op_parr(m: &mut Machine, op: &Op, ctx: &ExecCtx<'_>) -> Result<Flow, ExecError> {
    let pos = read_value_data(m, "parr", &op.arg1)?;
    let args = arg_strings(ctx.args);
    let mut result = 0.0f64;
    let mut found = false;
    if let Some(field) = args.get(pos.wrapping_sub(1) as usize)
        && !field.is_empty()
    {
        result = string_to_float(field);
        found = true;
    }
    m.write(&op.arg0, Value::Float(result))?;
    m.flags.z = !found;
    m.flags.c = false;
    m.flags.s = false;
    m.flags.v = false;
    Ok(Flow::Next)
}

/// `pars` (0x58): read job arg string at 1-based position `arg1` into `arg0`'s
/// `S` register (NUL-terminated, via [`write_string`]).
///
/// Per `OpPars` (EdOperations.cs:1836): only Zero is touched — set when the
/// argument is absent or empty (an empty string is written), cleared when a
/// non-empty one is read.
fn op_pars(m: &mut Machine, op: &Op, ctx: &ExecCtx<'_>) -> Result<Flow, ExecError> {
    let pos = read_value_data(m, "pars", &op.arg1)?;
    let args = arg_strings(ctx.args);
    let mut result = "";
    if let Some(field) = args.get(pos.wrapping_sub(1) as usize)
        && !field.is_empty()
    {
        result = field;
    }
    write_string(m, &op.arg0, result)?;
    m.flags.z = result.is_empty();
    Ok(Flow::Next)
}

/// `pary` (0x7F): copy the whole raw binary argument buffer into `arg0`'s `S`
/// register (raw, no NUL terminator).
///
/// Per `OpPary` (EdOperations.cs:1861): this uses `GetActiveArgBinary` — the raw
/// buffer, not the `;`-split fields — and ignores `arg1`. Only Zero is touched:
/// set when the buffer is empty, cleared otherwise.
fn op_pary(m: &mut Machine, op: &Op, ctx: &ExecCtx<'_>) -> Result<Flow, ExecError> {
    m.write(&op.arg0, Value::Bytes(ctx.args.to_vec()))?;
    m.flags.z = ctx.args.is_empty();
    Ok(Flow::Next)
}

/// `parn` (0x80): write the number of job arg fields into integer register
/// `arg0`; clear Overflow and set Zero/Sign from the count at `arg0`'s width.
///
/// Per `OpParn` (EdOperations.cs:1794). Carry is left untouched.
fn op_parn(m: &mut Machine, op: &Op, ctx: &ExecCtx<'_>) -> Result<Flow, ExecError> {
    let width = arg_width("parn", &op.arg0)?;
    let count = arg_strings(ctx.args).len() as u32;
    m.write(&op.arg0, Value::Int(i64::from(count)))?;
    m.flags.v = false;
    update_zs(&mut m.flags, count, width);
    Ok(Flow::Next)
}

// ---- Task 10: string-buffer handlers ----

/// `scmp` (0x20): set Zero when `arg0`'s and `arg1`'s raw byte arrays are equal.
///
/// `datacmp` (EdOperations.cs:2051) compares the raw arrays (same length and
/// bytes), not NUL-terminated strings, and touches only Zero. Note the polarity:
/// `scmp` sets Zero on **equality** — the inverse of [`op_strcmp`].
fn op_scmp(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let a = read_bytes(m, "scmp", &op.arg0)?;
    let b = read_bytes(m, "scmp", &op.arg1)?;
    m.flags.z = a == b;
    Ok(Flow::Next)
}

/// `scat` (0x21): append `arg1`'s raw bytes onto `arg0`'s and store back into
/// `arg0`'s `S` register (raw, no NUL). Touches no flags.
///
/// `datacat` (EdOperations.cs:2029). The reference's `ArrayMaxSize` overflow
/// guard is a per-job cap (`jobInfo.ArraySize`) not modeled in Phase 1.
fn op_scat(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let mut a = read_bytes(m, "scat", &op.arg0)?;
    let b = read_bytes(m, "scat", &op.arg1)?;
    a.extend_from_slice(&b);
    m.write(&op.arg0, Value::Bytes(a))?;
    Ok(Flow::Next)
}

/// `scut` (0x22): drop the last `arg1` bytes of `arg0`'s raw array; if `arg1`
/// exceeds the length, `arg0` becomes empty. Stored back raw. Touches no flags.
///
/// `strcut` (EdOperations.cs:2318); the cut length "includes terminating 0".
fn op_scut(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let data = read_bytes(m, "scut", &op.arg0)?;
    let len = read_value_data(m, "scut", &op.arg1)? as usize;
    let result = if len > data.len() {
        Vec::new()
    } else {
        data[..data.len() - len].to_vec()
    };
    m.write(&op.arg0, Value::Bytes(result))?;
    Ok(Flow::Next)
}

/// `slen` (0x23): write `arg1`'s raw data length (`GetDataLen`) into integer
/// register `arg0`; clear Overflow, set Zero/Sign from the value at `arg0`'s
/// width.
///
/// `OpSlen` (EdOperations.cs:2178). For a string/array `arg1` this is the full
/// buffer byte count — including a NUL terminator and any bytes past it — which
/// is why `slen` and the NUL-terminated `strlen` can differ.
fn op_slen(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let width = arg_width("slen", &op.arg0)?;
    let value = data_len(m, "slen", &op.arg1)?;
    m.write(&op.arg0, Value::Int(i64::from(value)))?;
    m.flags.v = false;
    update_zs(&mut m.flags, value, width);
    Ok(Flow::Next)
}

/// `spaste` (0x24): insert `arg1`'s raw bytes into the base `S` register (from
/// `arg0`'s indexed operand) at the start index, shifting the tail right.
///
/// `datainsert` (EdOperations.cs:2190): if the index is at or past the current
/// length the op is a no-op (no write). Stored back raw. Touches no flags. The
/// reference's `ArrayMaxSize` cap is not modeled in Phase 1.
fn op_spaste(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let (base, start) = indexed_base_and_start(m, "spaste", &op.arg0)?;
    let base_op = Operand::Reg {
        bank: base.bank,
        idx: base.idx,
    };
    let dest = read_bytes(m, "spaste", &base_op)?;
    let source = read_bytes(m, "spaste", &op.arg1)?;
    let start = start as usize;
    if start < dest.len() {
        let mut result = Vec::with_capacity(dest.len() + source.len());
        result.extend_from_slice(&dest[..start]);
        result.extend_from_slice(&source);
        result.extend_from_slice(&dest[start..]);
        m.write(&base_op, Value::Bytes(result))?;
    }
    Ok(Flow::Next)
}

/// `serase` (0x25): remove `arg1` bytes starting at the index (from `arg0`'s
/// indexed operand) from the base `S` register. Stored back raw. Touches no
/// flags.
///
/// `dataerase` (EdOperations.cs:2060): keeps each byte whose position is before
/// the start or at/after `start + len`.
fn op_serase(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let (base, start) = indexed_base_and_start(m, "serase", &op.arg0)?;
    let base_op = Operand::Reg {
        bank: base.bank,
        idx: base.idx,
    };
    let data = read_bytes(m, "serase", &base_op)?;
    let len = read_value_data(m, "serase", &op.arg1)? as usize;
    let start = start as usize;
    let end = start.saturating_add(len);
    let result: Vec<u8> = data
        .into_iter()
        .enumerate()
        .filter_map(|(i, byte)| (i < start || i >= end).then_some(byte))
        .collect();
    m.write(&base_op, Value::Bytes(result))?;
    Ok(Flow::Next)
}

/// `strcat` (0x7E): append `arg1`'s NUL-terminated string onto `arg0`'s and store
/// back into `arg0`'s `S` register via [`write_string`] (NUL-terminated). Touches
/// no flags.
///
/// `OpStrcat` (EdOperations.cs:2289). The reference's `ArrayMaxSize` truncation
/// is a per-job cap not modeled in Phase 1.
fn op_strcat(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let mut s = read_string(m, "strcat", &op.arg0)?;
    let t = read_string(m, "strcat", &op.arg1)?;
    s.push_str(&t);
    write_string(m, &op.arg0, &s)?;
    Ok(Flow::Next)
}

/// `strcmp` (0x8F): set Zero when `arg0`'s and `arg1`'s NUL-terminated strings
/// **differ**.
///
/// `OpStrcmp` (EdOperations.cs:2309) sets `Zero = String.Compare(..) != 0`, so
/// Zero is set on **inequality** — the deliberate inverse of [`op_scmp`], which
/// sets Zero on equality. This asymmetry is faithful to the reference oracle, not
/// a bug; it is pinned by a test. Only Zero is touched.
fn op_strcmp(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let a = read_string(m, "strcmp", &op.arg0)?;
    let b = read_string(m, "strcmp", &op.arg1)?;
    m.flags.z = a != b;
    Ok(Flow::Next)
}

/// `strlen` (0x90): write `arg1`'s NUL-terminated string length into integer
/// register `arg0`; clear Overflow, set Zero/Sign from the value at `arg0`'s
/// width.
///
/// `OpStrlen` (EdOperations.cs:2351) uses `GetStringData().Length` — the count of
/// characters up to the first NUL — which is why it can differ from `slen`'s raw
/// data length.
fn op_strlen(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let width = arg_width("strlen", &op.arg0)?;
    let value = read_string(m, "strlen", &op.arg1)?.chars().count() as u32;
    m.write(&op.arg0, Value::Int(i64::from(value)))?;
    m.flags.v = false;
    update_zs(&mut m.flags, value, width);
    Ok(Flow::Next)
}

// ---- Task 11: table-cursor helpers ----
//
// EDIABAS tracks a table cursor as `_tableIndex` / `_tableRowIndex`, which live in
// [`ExecCtx`] here (`current_table` / `current_row`, `None` == EDIABAS's `-1`).
// The `tab*` ops resolve names — table and column — CASE-INSENSITIVELY, exactly as
// EDIABAS does via `.ToUpper()` in `GetTableIndex`/`GetTableColumnIdx` and as
// [`klartext_sgbd::Prg::table_ci`] does with `eq_ignore_ascii_case`; the `RES_*`
// tables are referenced mixed-case (`RES_0x5001`) but stored uppercase.

/// The currently selected table, or `None` if `tabset` has selected none.
///
/// The returned borrow is tied to the tables' lifetime (`'a`), not to the `&ctx`
/// borrow, so a caller may mutate the cursor (`current_row`) while holding it.
fn current_table_opt<'a>(ctx: &ExecCtx<'a>) -> Option<&'a Table> {
    let tables: &'a [Table] = ctx.tables;
    tables.get(ctx.current_table?)
}

/// The currently selected table, or [`ExecError::TableNotSelected`] when none is
/// selected — EDIABAS's `_tableIndex < 0` → `EDIABAS_BIP_0010`.
fn selected_table<'a>(ctx: &ExecCtx<'a>, mnemonic: &'static str) -> Result<&'a Table, ExecError> {
    current_table_opt(ctx).ok_or(ExecError::TableNotSelected(mnemonic))
}

/// The index of the table named `name`, matched case-insensitively — mirrors
/// [`klartext_sgbd::Prg::table_ci`]'s rule but yields the index EDIABAS's
/// `_tableIndex` needs.
fn table_index(tables: &[Table], name: &str) -> Option<usize> {
    tables
        .iter()
        .position(|t| t.name.eq_ignore_ascii_case(name))
}

/// The index of the column named `name`, matched case-insensitively — EDIABAS's
/// `GetTableColumnIdx` (EdiabasNet.cs:5279).
fn column_index(columns: &[String], name: &str) -> Option<usize> {
    columns.iter().position(|c| c.eq_ignore_ascii_case(name))
}

// ---- Task 11: table-cursor and stack-peek handlers ----

/// `atsp` (0x50): read `arg0`'s register width in bytes from the data stack at
/// position `arg1`, WITHOUT popping, and store the big-endian value in `arg0`.
///
/// EDIABAS's `OpAtsp` (EdOperations.cs:299) takes `length` = `arg0`'s width and
/// `pos` = `arg1`'s value, then reads `length` bytes from `_stackList.ToArray()`
/// — a `Stack<byte>`, so `ToArray()` is **top-of-stack first** — starting at
/// `index = pos - length`, big-endian (`value = (value << 8) | stack[index++]`).
/// Our `data_stack` keeps the top at the END, so `stackArray[j]` is
/// `data_stack[len - 1 - j]` — the same top-first view. Fewer than `length` bytes
/// on the stack is EDIABAS's `EDIABAS_BIP_0005`; a `pos < length` (negative index)
/// or a read past the stack is the reference's invalid-stack-index throw — both are
/// a hard [`ExecError::AtspStack`], never a zero-fill. Only Zero/Sign are updated
/// (`UpdateFlags`); Carry/Overflow are untouched.
fn op_atsp(m: &mut Machine, op: &Op) -> Result<Flow, ExecError> {
    let length = arg_width("atsp", &op.arg0)? as usize;
    let pos = read_value_data(m, "atsp", &op.arg1)? as usize;
    let count = m.data_stack.len();
    if count < length {
        return Err(ExecError::AtspStack); // EDIABAS_BIP_0005
    }
    // `index = pos - length` (a negative index is the reference's hard throw), and
    // the field must lie within the stack (the reference would index past its array
    // otherwise; no-degrade turns that into a loud error, not a Rust panic).
    let index = pos.checked_sub(length).ok_or(ExecError::AtspStack)?;
    if index + length > count {
        return Err(ExecError::AtspStack);
    }
    let mut value: u32 = 0;
    for k in 0..length {
        // stackArray[index + k] (top-first) == data_stack[count - 1 - (index + k)].
        let byte = m.data_stack[count - 1 - (index + k)];
        value = (value << 8) | u32::from(byte);
    }
    m.write(&op.arg0, Value::Int(i64::from(value)))?;
    update_zs(&mut m.flags, value, length as u32);
    Ok(Flow::Next)
}

/// `tabset` (0x7B): select the table named by `arg0` and reset the row cursor.
///
/// EDIABAS's `OpTabset` (EdOperations.cs:2537) resolves the name CASE-INSENSITIVELY
/// (`GetTableIndex` → `TableNameDict[name.ToUpper()]`), sets `_tableIndex` to the
/// found table and `_tableRowIndex = -1`, EXCEPT when re-selecting the SAME table,
/// where the prior row cursor is restored. A name the SGBD does not contain makes
/// the reference fall back to the last table and raise `EDIABAS_BIP_0010`;
/// no-degrade makes that a hard [`ExecError::TableNotFound`] rather than selecting
/// the wrong table. Phase-1 note: tables resolve ONLY from the variant `.prg` the
/// context carries; the group `.grp` base file (`_sgbdBaseFs`) is deferred (spec
/// §2). Touches no flags.
fn op_tabset(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let name = read_string(m, "tabset", &op.arg0)?;
    let idx = match table_index(ctx.tables, &name) {
        Some(i) => i,
        None => return Err(ExecError::TableNotFound(name)),
    };
    // Reset the row cursor on select, but keep it when the SAME table is
    // re-selected (OpTabset:2561-2566 restores `_tableRowIndex`).
    if ctx.current_table != Some(idx) {
        ctx.current_row = None;
    }
    ctx.current_table = Some(idx);
    Ok(Flow::Next)
}

/// `tabseek` (0x7C): find the first data row whose `arg0`-named column equals the
/// `arg1` key (case-insensitive string), set the row cursor, and set Zero on a miss.
///
/// EDIABAS's `OpTabseek` → the string `SeekTable` (EdOperations.cs:2499,
/// EdiabasNet.cs:5175): the column is matched by name; each data cell and the key
/// are compared uppercased; the FIRST match wins. A hit sets the cursor and clears
/// Zero; a MISS points the cursor at the last data row and SETS Zero (the
/// not-found signal, not an error). No table selected → `EDIABAS_BIP_0010`; a
/// column the table lacks → `EDIABAS_BIP_0010`; a miss on a table with no data rows
/// is the reference's `rowIndex < 0` → `EDIABAS_BIP_0010`. Only Zero is touched.
fn op_tabseek(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let table = selected_table(ctx, "tabseek")?;
    let column = read_string(m, "tabseek", &op.arg0)?;
    let key = read_string(m, "tabseek", &op.arg1)?;
    let col = column_index(&table.columns, &column).ok_or(ExecError::TableColumn {
        op: "tabseek",
        column,
    })?;
    let hit = table.rows.iter().position(|r| {
        r.get(col)
            .is_some_and(|cell| cell.eq_ignore_ascii_case(&key))
    });
    seek_result(m, ctx, "tabseek", table.rows.len(), hit)
}

/// `tabseeku` (0x9A): like `tabseek`, but the `arg1` key is a NUMERIC value matched
/// against each cell parsed via EDIABAS `StringToValue`.
///
/// EDIABAS's `OpTabseeku` → the value `SeekTable` (EdOperations.cs:2518,
/// EdiabasNet.cs:5226): the `arg0`-named column is matched by name; each data cell
/// is parsed with `StringToValue` (compared at `EdValueType`/32-bit width) against
/// `arg1`'s value; the FIRST match wins. Hit/miss/error behavior is `tabseek`'s.
/// Only Zero is touched.
fn op_tabseeku(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let table = selected_table(ctx, "tabseeku")?;
    let column = read_string(m, "tabseeku", &op.arg0)?;
    let key = read_value_data(m, "tabseeku", &op.arg1)?;
    let col = column_index(&table.columns, &column).ok_or(ExecError::TableColumn {
        op: "tabseeku",
        column,
    })?;
    let hit = table.rows.iter().position(|r| {
        r.get(col)
            .is_some_and(|cell| string_to_value(cell) as u32 == key)
    });
    seek_result(m, ctx, "tabseeku", table.rows.len(), hit)
}

/// Shared tail of `tabseek`/`tabseeku` (EDIABAS's `SeekTable` return + `OpTabseek`
/// flag handling): a hit sets the row cursor and clears Zero; a miss clamps the
/// cursor to the last data row and SETS Zero, or — on a table with no data rows —
/// is the reference's `rowIndex < 0` → [`ExecError::TableRow`] hard error.
fn seek_result(
    m: &mut Machine,
    ctx: &mut ExecCtx<'_>,
    mnemonic: &'static str,
    rows: usize,
    hit: Option<usize>,
) -> Result<Flow, ExecError> {
    match hit {
        Some(r) => {
            ctx.current_row = Some(r);
            m.flags.z = false;
        }
        None => {
            // Not found: the reference clamps to the last data row (`Rows - 1`); an
            // empty data table makes that `-1` → EDIABAS_BIP_0010.
            let last = rows.checked_sub(1).ok_or(ExecError::TableRow(mnemonic))?;
            ctx.current_row = Some(last);
            m.flags.z = true;
        }
    }
    Ok(Flow::Next)
}

/// `tabget` (0x7D): read the cell at the current row and the `arg1`-named column
/// into `arg0` as a string.
///
/// EDIABAS's `OpTabget` → `GetTableEntry` (EdOperations.cs:2442,
/// EdiabasNet.cs:5289). No table selected → `EDIABAS_BIP_0010`. The column is
/// matched by name; a column the table lacks → `EDIABAS_BIP_0010`. If the column IS
/// valid but no row has been selected yet (`_tableRowIndex < 0`), the reference
/// deliberately returns an EMPTY string WITHOUT error (its "table changed"
/// garbage-but-no-error path, :2453-2456) — reproduced faithfully here. A selected
/// row that is out of range is `EDIABAS_BIP_0010`. Touches no flags.
fn op_tabget(m: &mut Machine, op: &Op, ctx: &ExecCtx<'_>) -> Result<Flow, ExecError> {
    let table = selected_table(ctx, "tabget")?;
    let column = read_string(m, "tabget", &op.arg1)?;
    let col = column_index(&table.columns, &column).ok_or(ExecError::TableColumn {
        op: "tabget",
        column,
    })?;
    let entry = match ctx.current_row {
        // Valid column but no row selected yet: EDIABAS returns "" without error.
        None => String::new(),
        Some(r) => table
            .rows
            .get(r)
            .and_then(|row| row.get(col))
            .cloned()
            .ok_or(ExecError::TableRow("tabget"))?,
    };
    write_string(m, &op.arg0, &entry)?;
    Ok(Flow::Next)
}

/// `tabline` (0x83): select the current row by 0-based line number `arg0`.
///
/// EDIABAS's `OpTabline` → `GetTableLine` (EdOperations.cs:2467,
/// EdiabasNet.cs:5157): a line within range sets the cursor and clears Zero; a line
/// at or past the row count CLAMPS the cursor to the last data row and SETS Zero
/// (not an error). No table selected → `EDIABAS_BIP_0010`; a table with no data
/// rows makes `GetTableLine` return `-1` → `EDIABAS_BIP_0010`. Only Zero is touched.
fn op_tabline(m: &mut Machine, op: &Op, ctx: &mut ExecCtx<'_>) -> Result<Flow, ExecError> {
    let rows = selected_table(ctx, "tabline")?.rows.len();
    let line = read_value_data(m, "tabline", &op.arg0)? as usize;
    if rows == 0 {
        return Err(ExecError::TableRow("tabline")); // GetTableLine -> -1 -> BIP_0010
    }
    let (row, found) = if line >= rows {
        (rows - 1, false) // clamp to the last data row
    } else {
        (line, true)
    };
    ctx.current_row = Some(row);
    m.flags.z = !found;
    Ok(Flow::Next)
}

/// `tabcols` (0xB6): write the current table's column count into `arg0`, or 0 when
/// no table is selected.
///
/// EDIABAS's `OpTabcols` (EdOperations.cs:2428) writes 0 — NOT an error — when
/// `_tableIndex < 0`; this is one of the few reference paths that returns empty
/// without raising, reproduced faithfully. Touches no flags.
fn op_tabcols(m: &mut Machine, op: &Op, ctx: &ExecCtx<'_>) -> Result<Flow, ExecError> {
    let columns = current_table_opt(ctx).map_or(0, |t| t.columns.len() as u32);
    m.write(&op.arg0, Value::Int(i64::from(columns)))?;
    Ok(Flow::Next)
}

/// `tabrows` (0xB7): write the current table's row count INCLUDING the header row
/// into `arg0`, or 0 when no table is selected.
///
/// EDIABAS's `OpTabrows` (EdOperations.cs:2485) returns `GetTableRows + 1` (the
/// `+1` counts the header row) and writes 0 without error when no table is
/// selected. Our [`Table::rows`] excludes the header, so this is `rows.len() + 1`.
/// Touches no flags.
fn op_tabrows(m: &mut Machine, op: &Op, ctx: &ExecCtx<'_>) -> Result<Flow, ExecError> {
    let rows = current_table_opt(ctx).map_or(0, |t| t.rows.len() as u32 + 1);
    m.write(&op.arg0, Value::Int(i64::from(rows)))?;
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

    /// Builds a trap-branch op (`jt`/`jnt`): a PC-relative target `rel` in `arg0`
    /// and an `Imm` test-bit in `arg1`, the shape the error-detected branches
    /// decode to. `len`/`offset` stay zero, so a taken branch lands at `rel`.
    fn op_jump_with_arg1(byte: u8, rel: i64, test_bit: i64) -> Op {
        op(byte, Operand::Imm(rel), Operand::Imm(test_bit))
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

    /// Runs one op against `m` with a throwaway result set and no job args,
    /// asserting it succeeds and returns `Flow::Next`.
    fn run(m: &mut Machine, o: &Op) {
        let mut results = ResultSet::new();
        let mut ctx = ExecCtx {
            results: &mut results,
            args: &[],
            tables: &[],
            current_table: None,
            current_row: None,
        };
        assert_eq!(step(m, o, &mut ctx).unwrap(), Flow::Next);
    }

    /// Steps one op against `m` with a throwaway result set and no job args,
    /// returning the raw result. For tests that assert on an error or a specific
    /// [`Flow`]; the result-store/param ops that need a live context build their
    /// own [`ExecCtx`] in-test.
    fn step_bare(m: &mut Machine, o: &Op) -> Result<Flow, ExecError> {
        let mut results = ResultSet::new();
        let mut ctx = ExecCtx {
            results: &mut results,
            args: &[],
            tables: &[],
            current_table: None,
            current_row: None,
        };
        step(m, o, &mut ctx)
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
    fn adds_into_an_indexed_s_byte() {
        // `adds S1[0], B6`: the real F20 DDE jobs add a byte register into a byte
        // inside an S buffer. An indexed base passes EDIABAS's `OpData1 is
        // Register` gate, and `GetDataLen(write=true)` is 1 for the no-length
        // index, so the op runs at width 1 and stores the low byte back
        // (one-arg `SetRawData`, dataLen 1). The tail byte is untouched.
        let mut m = Machine::new();
        m.write(&reg_s(1), Value::Bytes(vec![0x05, 0xAA])).unwrap();
        m.write(&reg_b(6), Value::Int(0x03)).unwrap();
        let arg0 = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(0),
            len: None,
        };
        run(&mut m, &op_adds(arg0, reg_b(6)));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(vec![0x08, 0xAA]));
        assert!(!m.flags.z);
        assert!(!m.flags.c);
        assert!(!m.flags.v);
    }

    #[test]
    fn comp_indexed_s_byte_sets_flags_without_storing() {
        // `comp S1[0], B6`: `comp` accepts the same indexed target as `adds` but
        // only reads it. Equal bytes set Zero and leave the buffer untouched.
        let mut m = Machine::new();
        m.write(&reg_s(1), Value::Bytes(vec![0x05, 0xAA])).unwrap();
        m.write(&reg_b(6), Value::Int(0x05)).unwrap();
        let arg0 = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(0),
            len: None,
        };
        run(&mut m, &op_comp(arg0, reg_b(6)));
        assert!(m.flags.z); // 0x05 - 0x05 == 0
        assert!(!m.flags.c);
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(vec![0x05, 0xAA]));
    }

    #[test]
    fn adds_rejects_a_length_bearing_indexed_target() {
        // Only the no-length indexed target is supported (`GetDataLen(write=true)`
        // = 1); a length-bearing indexed `arg0` stays a loud `InvalidOperand`
        // until a job needs it — no silent guess at its `SetRawData` width.
        let mut m = Machine::new();
        let arg0 = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(0),
            len: Some(IndexArg::Imm(2)),
        };
        assert_eq!(
            step_bare(&mut m, &op_adds(arg0, reg_b(6))),
            Err(ExecError::InvalidOperand("adds"))
        );
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
            step_bare(&mut m, &op_divs(reg_i(0), reg_i(1))),
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
        // 0x26 = xconnect, a comm opcode deferred to Phase 2 (xsend/xrequf are the
        // only comm opcodes Task 12 routes; every other reaches the default arm).
        let mut m = Machine::new();
        assert_eq!(
            step_bare(&mut m, &op(0x26, Operand::None, Operand::None)),
            Err(ExecError::Unimplemented("xconnect"))
        );
    }

    #[test]
    fn arithmetic_on_immediate_target_is_rejected() {
        // adds requires a writable integer register as arg0.
        let mut m = Machine::new();
        assert_eq!(
            step_bare(&mut m, &op_adds(imm(1), imm(2))),
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
        let mut results = ResultSet::new();
        let mut ctx = ExecCtx {
            results: &mut results,
            args: &[],
            tables: &[],
            current_table: None,
            current_row: None,
        };
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
        let flow = step_bare(&mut m, &j).unwrap();
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
        assert_eq!(step_bare(&mut m, &j).unwrap(), Flow::Jumped);
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
        assert_eq!(step_bare(&mut m, &j).unwrap(), Flow::Jumped);
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
            step_bare(&mut m, &op(0x1D, Operand::None, Operand::None)).unwrap(),
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
    fn push_immediate_uses_addressing_mode_width() {
        // `push #1` as an Imm32 (mode hi-nibble 7) pushes four bytes — EDIABAS's
        // `OpPush` sizes by `GetDataLen`, which for an immediate is its encoded
        // width. The real DDE job's `push #0x1` (mode 0x70) relies on this; the old
        // register-only `arg_width` rejected it. LSB first: 01 00 00 00.
        let mut m = Machine::new();
        let push_imm32 = Op {
            byte: 0x1E,
            mode_byte: 0x70, // arg0 = Imm32, arg1 = None
            arg0: Operand::Imm(1),
            arg1: Operand::None,
            len: 6,
            offset: 0,
        };
        step_bare(&mut m, &push_imm32).unwrap();
        assert_eq!(m.data_stack, vec![0x01, 0x00, 0x00, 0x00]);

        // An Imm8 (mode hi-nibble 5) pushes a single byte.
        let mut m = Machine::new();
        let push_imm8 = Op {
            byte: 0x1E,
            mode_byte: 0x50, // arg0 = Imm8, arg1 = None
            arg0: Operand::Imm(0x42),
            arg1: Operand::None,
            len: 3,
            offset: 0,
        };
        step_bare(&mut m, &push_imm8).unwrap();
        assert_eq!(m.data_stack, vec![0x42]);
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
            step_bare(&mut m, &op(0x1F, reg_i(0), Operand::None)),
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
            step_bare(&mut m, &op(0x1F, reg_i(0), Operand::None)),
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
            step_bare(&mut m, &op(0x4B, Operand::None, Operand::None)),
            Err(ExecError::Break)
        );
        // No flag was touched — Carry survives the aborted instruction.
        assert!(m.flags.c);
    }

    #[test]
    fn jtsr_and_ret_are_loud_unimplemented() {
        let mut m = Machine::new();
        assert_eq!(
            step_bare(&mut m, &op(0x0C, Operand::Imm(0), Operand::None)),
            Err(ExecError::Unimplemented("jtsr"))
        );
        assert_eq!(
            step_bare(&mut m, &op(0x0D, Operand::None, Operand::None)),
            Err(ExecError::Unimplemented("ret"))
        );
    }

    #[test]
    fn jt_jumps_only_when_the_tested_trap_bit_matches() {
        let mut m = Machine::new();
        // No trap set: jt +4 with test bit 5 falls through.
        let jt = op_jump_with_arg1(0x47, 4, 5);
        assert_eq!(step_bare(&mut m, &jt), Ok(Flow::Next));
        // Trap bit 5 set: it jumps.
        m.trap_bit = Some(5);
        let pc_before = m.pc;
        assert_eq!(step_bare(&mut m, &jt), Ok(Flow::Jumped));
        assert_eq!(m.pc, pc_before + jt.len + 4);
        // Special case (EdOperations.cs:1567): trap == 0 matches test bit 32.
        m.pc = 0;
        m.trap_bit = Some(0);
        let jt32 = op_jump_with_arg1(0x47, 4, 32);
        assert_eq!(step_bare(&mut m, &jt32), Ok(Flow::Jumped));
    }

    #[test]
    fn jnt_is_the_complement_and_clrt_clears() {
        let mut m = Machine::new();
        let jnt = op_jump_with_arg1(0x48, 4, 5);
        assert_eq!(step_bare(&mut m, &jnt), Ok(Flow::Jumped)); // no trap -> jumps
        m.trap_bit = Some(5);
        m.pc = 0;
        assert_eq!(step_bare(&mut m, &jnt), Ok(Flow::Next)); // trap matches -> falls through
        // clrt (0x46) clears the trap.
        assert_eq!(
            step_bare(&mut m, &op(0x46, Operand::None, Operand::None)),
            Ok(Flow::Next)
        );
        assert_eq!(m.trap_bit, None);
    }

    #[test]
    fn settmr_and_gettmr_move_the_trap_mask() {
        // EDIABAS's names are misleading: settmr/gettmr set/read the TRAP MASK
        // (EdOperations.cs:2130, 1279), not a clock.
        let mut m = Machine::new();
        let set = op(0x44, Operand::Imm(0b1010_0000), Operand::None);
        step_bare(&mut m, &set).unwrap();
        assert_eq!(m.trap_mask, 0b1010_0000);
        let get = op(
            0x43,
            Operand::Reg {
                bank: RegBank::L,
                idx: 0,
            },
            Operand::None,
        );
        step_bare(&mut m, &get).unwrap();
        assert_eq!(m.read(&get.arg0).unwrap(), Value::Int(0b1010_0000));
    }

    #[test]
    fn set_error_respects_the_mask() {
        let mut m = Machine::new();
        m.trap_mask = 1 << 19; // job masked "no response"
        assert_eq!(set_error(&mut m, 19), Ok(()));
        assert_eq!(m.trap_bit, Some(19));
        // Unmasked bit aborts.
        assert_eq!(set_error(&mut m, 2), Err(ExecError::Trapped { bit: 2 }));
    }

    // ---- Task 5: indexed source reads ----

    /// An [`Op`] with an explicit `mode_byte`, for the addressing-mode-sensitive
    /// indexed reads (the plain [`op`] helper leaves `mode_byte` zero).
    fn op_with_mode(byte: u8, mode_byte: u8, arg0: Operand, arg1: Operand) -> Op {
        Op {
            byte,
            mode_byte,
            arg0,
            arg1,
            len: 0,
            offset: 0,
        }
    }

    /// The base [`RegId`] of `S<idx>`.
    fn s_id(idx: u8) -> RegId {
        RegId {
            bank: RegBank::S,
            idx,
        }
    }

    #[test]
    fn indexed_source_reads_little_endian_into_ints() {
        // move I0, S1[0,2] with S1 = [0x34, 0x12] -> I0 = 0x1234: a byte-buffer
        // source is read least-significant byte first (EdiabasNet.cs:399-403).
        let mut m = Machine::new();
        m.write(&reg_s(1), Value::Bytes(vec![0x34, 0x12])).unwrap();
        let src = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(0),
            len: Some(IndexArg::Imm(2)),
        };
        run(&mut m, &op_move(reg_i(0), src));
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0x1234));
    }

    #[test]
    fn idxregimm_source_reads_to_buffer_end_through_step() {
        // The "extra test" the brief asks for once the IdxRegImm encoding is
        // confirmed: `move S0, S1[B0 + 1]` with S1 = [AA,BB,CC,DD] and B0 = 1
        // resolves to index 1 + increment 1 = 2, then slices to the buffer's end
        // -> S0 = [CC, DD]. The increment lives in the `len` slot, so only the
        // step-level fold (which sees mode_byte) reads it correctly.
        let mut m = Machine::new();
        m.write(&reg_s(1), Value::Bytes(vec![0xAA, 0xBB, 0xCC, 0xDD]))
            .unwrap();
        m.write(&reg_b(0), Value::Int(1)).unwrap();
        let mode = ((AddrMode::RegS as u8) << 4) | (AddrMode::IdxRegImm as u8);
        let mv = op_with_mode(
            0x00,
            mode,
            reg_s(0),
            Operand::Indexed {
                base: s_id(1),
                index: IndexArg::Reg(RegId {
                    bank: RegBank::B,
                    idx: 0,
                }),
                len: Some(IndexArg::Imm(1)),
            },
        );
        run(&mut m, &mv);
        assert_eq!(m.read(&reg_s(0)).unwrap(), Value::Bytes(vec![0xCC, 0xDD]));
    }

    #[test]
    fn len_mode_operands_are_not_folded_as_increments() {
        // IdxRegLenImm (mode 14) decodes to the same Indexed shape as IdxRegImm
        // (mode 11) but its third sub-operand IS a length; only the IdxRegImm
        // nibble triggers the increment fold, so a Len-mode op is untouched.
        let m = Machine::new();
        let mode = ((AddrMode::RegS as u8) << 4) | (AddrMode::IdxRegLenImm as u8);
        let len_mode = op_with_mode(
            0x00,
            mode,
            reg_s(0),
            Operand::Indexed {
                base: s_id(1),
                index: IndexArg::Reg(RegId {
                    bank: RegBank::B,
                    idx: 0,
                }),
                len: Some(IndexArg::Imm(2)),
            },
        );
        assert!(fold_index_increments(&m, &len_mode).unwrap().is_none());
    }

    #[test]
    fn indexed_read_past_array_max_traps_the_job() {
        // An indexed reach past ArrayMaxSize records EDIABAS's BIP_0001 and, with
        // the class unmasked, aborts the running instruction (EdiabasNet.cs:283).
        let mut m = Machine::new();
        let src = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(1024),
            len: None,
        };
        assert_eq!(
            step_bare(&mut m, &op_move(reg_i(0), src)),
            Err(ExecError::Trapped {
                bit: TRAP_BIT_UNMAPPED
            })
        );
        assert_eq!(m.trap_bit, Some(TRAP_BIT_UNMAPPED));
    }

    #[test]
    fn indexed_read_past_array_max_is_recoverable_when_masked() {
        // A job that masks the class keeps running and reads the empty array
        // (ByteArray0) as zero (EdiabasNet.cs:286-290).
        let mut m = Machine::new();
        m.trap_mask = 1 << TRAP_BIT_UNMAPPED; // masked via settmr
        let src = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(1024),
            len: None,
        };
        run(&mut m, &op_move(reg_i(0), src));
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0));
        assert_eq!(m.trap_bit, Some(TRAP_BIT_UNMAPPED));
    }

    // ---- Task 6: indexed destination writes + swap ----

    #[test]
    fn swap_reverses_the_addressed_slice_in_place() {
        // swap S1[1,3] on [1,2,3,4,5] -> [1,4,3,2,5]; length unchanged
        // (EdOperations.cs:2406-2425, keepLength=true).
        let mut m = Machine::new();
        m.write(&reg_s(1), Value::Bytes(vec![1, 2, 3, 4, 5]))
            .unwrap();
        let arg0 = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(1),
            len: Some(IndexArg::Imm(3)),
        };
        run(&mut m, &op(0x51, arg0, Operand::None));
        assert_eq!(
            m.read(&reg_s(1)).unwrap(),
            Value::Bytes(vec![1, 4, 3, 2, 5])
        );
    }

    #[test]
    fn swap_past_the_used_length_pulls_in_zeros_but_keeps_length() {
        // used = [1,2]; swap [1,3) touches zeros from the backing buffer; the
        // register's used length stays 2 (keepLength=true), so only byte 1
        // changes: reversing [2,0,0] gives [0,0,2] and the first `used` bytes
        // survive -> [1, 0].
        let mut m = Machine::new();
        m.write(&reg_s(1), Value::Bytes(vec![1, 2])).unwrap();
        let arg0 = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(1),
            len: Some(IndexArg::Imm(3)),
        };
        run(&mut m, &op(0x51, arg0, Operand::None));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(vec![1, 0]));
    }

    #[test]
    fn swap_past_array_max_traps_the_job() {
        // start 1021 + len 3 = 1024 > ArrayMaxSize (1023): BIP_0001, and the
        // unmasked class aborts (EdOperations.cs:2418-2422).
        let mut m = Machine::new();
        let arg0 = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(1021),
            len: Some(IndexArg::Imm(3)),
        };
        assert_eq!(
            step_bare(&mut m, &op(0x51, arg0, Operand::None)),
            Err(ExecError::Trapped {
                bit: TRAP_BIT_UNMAPPED
            })
        );
        assert_eq!(m.trap_bit, Some(TRAP_BIT_UNMAPPED));
    }

    #[test]
    fn swap_past_array_max_skips_when_masked() {
        // A masking job records the bit and skips the reverse (SetError +
        // return): the register is untouched and execution continues.
        let mut m = Machine::new();
        m.trap_mask = 1 << TRAP_BIT_UNMAPPED;
        m.write(&reg_s(1), Value::Bytes(vec![1, 2])).unwrap();
        let arg0 = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(1021),
            len: Some(IndexArg::Imm(3)),
        };
        run(&mut m, &op(0x51, arg0, Operand::None));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(vec![1, 2]));
        assert_eq!(m.trap_bit, Some(TRAP_BIT_UNMAPPED));
    }

    #[test]
    fn swap_without_a_length_is_invalid() {
        // OpSwap reads the index and length sub-operands directly; an operand
        // without a length (or a non-indexed one) is not a swap shape.
        let mut m = Machine::new();
        let arg0 = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(0),
            len: None,
        };
        assert_eq!(
            step_bare(&mut m, &op(0x51, arg0, Operand::None)),
            Err(ExecError::InvalidOperand("swap"))
        );
        assert_eq!(
            step_bare(&mut m, &op(0x51, reg_s(1), Operand::None)),
            Err(ExecError::InvalidOperand("swap"))
        );
    }

    #[test]
    fn move_int_through_indexed_dest_writes_one_byte() {
        // OpMove's byte[]-dest + int-source path stores through the ONE-arg
        // SetRawData (dataLen defaults to 1, EdiabasNet.cs:441-444): only the
        // value's low byte lands at the index, the gap zero-fills, and Z/S come
        // from the value at width 1 (UpdateFlags(value, 1),
        // EdOperations.cs:1310-1316).
        let mut m = Machine::new();
        m.write(&reg_s(1), Value::Bytes(vec![0xAA])).unwrap();
        let dest = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(3),
            len: None,
        };
        run(&mut m, &op_move(dest, imm(0x1200)));
        assert_eq!(
            m.read(&reg_s(1)).unwrap(),
            Value::Bytes(vec![0xAA, 0, 0, 0x00])
        );
        // Low byte 0x00 -> Zero set at width 1 (a width-2 update would clear it).
        assert!(m.flags.z);
        assert!(!m.flags.s);
        assert!(!m.flags.c);
        assert!(!m.flags.v);
    }

    #[test]
    fn move_bytes_through_indexed_dest_writes_their_own_length() {
        // A byte-buffer source through an indexed dest stores all its bytes
        // (SetRawData(byte[]) uses the array's own length,
        // EdiabasNet.cs:530-534); C/Z/S/V all clear (EdOperations.cs:1335-1339).
        let mut m = Machine::new();
        m.flags.z = true;
        m.write(&reg_s(0), Value::Bytes(vec![0xDE, 0xAD])).unwrap();
        let dest = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(1),
            len: None,
        };
        run(&mut m, &op_move(dest, reg_s(0)));
        assert_eq!(
            m.read(&reg_s(1)).unwrap(),
            Value::Bytes(vec![0, 0xDE, 0xAD])
        );
        assert!(!m.flags.z);
        assert!(!m.flags.c);
        assert!(!m.flags.s);
        assert!(!m.flags.v);
    }

    #[test]
    fn move_indexed_write_past_array_max_traps_the_job() {
        // index 1023 + 1 byte = 1024 > ArrayMaxSize: the reference records
        // BIP_0001 and skips the store (EdiabasNet.cs:536-541); unmasked, the
        // class aborts the job.
        let mut m = Machine::new();
        let dest = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(1023),
            len: None,
        };
        assert_eq!(
            step_bare(&mut m, &op_move(dest, imm(0xFF))),
            Err(ExecError::Trapped {
                bit: TRAP_BIT_UNMAPPED
            })
        );
        assert_eq!(m.trap_bit, Some(TRAP_BIT_UNMAPPED));
    }

    #[test]
    fn move_indexed_write_past_array_max_skips_when_masked() {
        // A masking job keeps running; the write is skipped and the register
        // stays untouched (SetError + return), while OpMove still updates the
        // flags afterwards.
        let mut m = Machine::new();
        m.trap_mask = 1 << TRAP_BIT_UNMAPPED;
        let dest = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(1023),
            len: None,
        };
        run(&mut m, &op_move(dest, imm(0xFF)));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(vec![]));
        assert_eq!(m.trap_bit, Some(TRAP_BIT_UNMAPPED));
    }

    #[test]
    fn divs_remainder_lands_through_an_indexed_arg1() {
        // OpDivs writes the remainder through arg1 whenever its OpData1 is a
        // Register — which includes an INDEXED arg1 — via the width-carrying
        // SetRawData(remainder, len) (EdOperations.cs:519-522). I-width divs:
        // 20 / 6 -> remainder 2 stored as 2 little-endian bytes at the index.
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(20)).unwrap();
        m.write(&reg_s(1), Value::Bytes(vec![6])).unwrap();
        let arg1 = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(0),
            len: None,
        };
        run(&mut m, &op_divs(reg_i(0), arg1));
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(3));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(vec![2, 0]));
    }

    #[test]
    fn mult_high_word_lands_through_an_indexed_arg1() {
        // Same Register gate in OpMult (EdOperations.cs:1740-1746): the high
        // half of the product stores through an indexed arg1 at arg0's width.
        // B-width: 5 * 3 = 15 -> high byte 0 written over the source byte.
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(5)).unwrap();
        m.write(&reg_s(1), Value::Bytes(vec![9, 3])).unwrap();
        let arg1 = Operand::Indexed {
            base: s_id(1),
            index: IndexArg::Imm(1),
            len: None,
        };
        run(&mut m, &op_mult(reg_b(0), arg1));
        assert_eq!(m.read(&reg_b(0)).unwrap(), Value::Int(15));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(vec![9, 0]));
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
    fn op_fcomp(a0: Operand, a1: Operand) -> Op {
        op(0xA1, a0, a1)
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
    fn op_fix2hex(a0: Operand, a1: Operand) -> Op {
        op(0x79, a0, a1)
    }
    fn op_fix2dez(a0: Operand, a1: Operand) -> Op {
        op(0x7A, a0, a1)
    }

    #[test]
    fn fmul_then_fadd_scales_engine_temp() {
        // F0 = raw 3631, F1 = 0.1  ; fmul -> 363.1 ; then fadd offset -273.14 -> 89.96
        let mut m = Machine::new();
        m.f[0] = 3631.0;
        m.f[1] = 0.1;
        step_bare(&mut m, &op_fmul(reg_f(1), reg_f(0))).unwrap(); // F1 *= F0
        assert!((m.f[1] - 363.1).abs() < 1e-6);
        m.f[0] = -273.14;
        step_bare(&mut m, &op_fadd(reg_f(1), reg_f(0))).unwrap();
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
            step_bare(&mut m, &op_fdiv(reg_f(0), reg_f(1))),
            Err(ExecError::NonFinite("fdiv"))
        );
    }

    #[test]
    fn float_arithmetic_rejects_a_non_float_operand() {
        // fadd requires F-register operands; an integer register is invalid.
        let mut m = Machine::new();
        assert_eq!(
            step_bare(&mut m, &op_fadd(reg_i(0), reg_f(0))),
            Err(ExecError::InvalidOperand("fadd"))
        );
    }

    #[test]
    fn move_copies_a_float_register_to_a_float_register() {
        // The generic scaler moves floats with a plain `move F<d>, F<s>` (encoded
        // in a byte-array addressing mode, not a float opcode). The net effect is
        // `F<d> = F<s>`; it clears Carry/Zero/Sign/Overflow.
        let mut m = Machine::new();
        m.f[2] = -72.52;
        m.flags.z = true;
        m.flags.c = true;
        run(&mut m, &op_move(reg_f(0), reg_f(2)));
        assert!((m.f[0] - (-72.52)).abs() < 1e-9);
        assert!(!m.flags.z && !m.flags.c && !m.flags.s && !m.flags.v);
    }

    #[test]
    fn move_rejects_a_non_float_source_into_a_float_register() {
        // No Phase-1 job moves a non-float into an F register; keep it loud.
        let mut m = Machine::new();
        assert_eq!(
            step_bare(&mut m, &op_move(reg_f(0), reg_i(0))),
            Err(ExecError::InvalidOperand("move"))
        );
    }

    #[test]
    fn fcomp_sets_zero_sign_and_clears_overflow() {
        // OpFcomp: Zero on equality, Sign when arg0 < arg1, Overflow cleared; a
        // finite difference leaves Carry untouched (only inf/NaN sets it).
        let mut m = Machine::new();
        m.f[0] = 1.5;
        m.f[1] = 1.5;
        m.flags.c = true; // pre-set: a finite compare must not clear it
        run(&mut m, &op_fcomp(reg_f(0), reg_f(1)));
        assert!(m.flags.z && !m.flags.s && !m.flags.v && m.flags.c);

        m.f[0] = 1.0;
        m.f[1] = 2.0;
        run(&mut m, &op_fcomp(reg_f(0), reg_f(1)));
        assert!(!m.flags.z && m.flags.s); // 1.0 < 2.0

        m.f[0] = 3.0;
        m.f[1] = 2.0;
        run(&mut m, &op_fcomp(reg_f(0), reg_f(1)));
        assert!(!m.flags.z && !m.flags.s); // 3.0 > 2.0
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
            step_bare(&mut m, &op_a2flt(reg_f(0), reg_s(0))),
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
            step_bare(&mut m, &op_flt2fix(reg_l(0), reg_f(0))),
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
    fn wait_surfaces_seconds_to_the_run_loop() {
        // `wait #2` surfaces Flow::Wait{seconds: 2} without blocking; the sleep
        // itself happens at the async run-loop boundary. arg0 is whole SECONDS
        // (EdOperations.cs:3267 sleeps arg0 × 1000 ms).
        let mut m = Machine::new();
        assert_eq!(
            step_bare(&mut m, &op(0x6B, imm(2), Operand::None)),
            Ok(Flow::Wait { seconds: 2 })
        );
    }

    #[test]
    fn fix2dez_formats_signed_by_counterpart_width() {
        // The counterpart register's width picks the signed cast (i8/i16/i32);
        // write_string NUL-terminates the result (EdOperations.cs:687-715).
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0xFF)).unwrap(); // 1-byte -> i8 -> -1
        run(&mut m, &op_fix2dez(reg_s(1), reg_b(0)));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(b"-1\0".to_vec()));

        m.write(&reg_i(0), Value::Int(0xFFFE)).unwrap(); // 2-byte -> i16 -> -2
        run(&mut m, &op_fix2dez(reg_s(2), reg_i(0)));
        assert_eq!(m.read(&reg_s(2)).unwrap(), Value::Bytes(b"-2\0".to_vec()));

        m.write(&reg_l(0), Value::Int(0xFFFF_FFFF)).unwrap(); // 4-byte -> i32 -> -1
        run(&mut m, &op_fix2dez(reg_s(3), reg_l(0)));
        assert_eq!(m.read(&reg_s(3)).unwrap(), Value::Bytes(b"-1\0".to_vec()));
    }

    #[test]
    fn fix2hex_formats_prefixed_uppercase_by_width() {
        // The counterpart width picks the zero-padded hex width (0x%02X/%04X/%08X);
        // write_string NUL-terminates the result (EdOperations.cs:750-780).
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0x05)).unwrap(); // 1-byte -> 0x05
        run(&mut m, &op_fix2hex(reg_s(1), reg_b(0)));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(b"0x05\0".to_vec()));

        m.write(&reg_i(0), Value::Int(0x0ABC)).unwrap(); // 2-byte -> 0x0ABC
        run(&mut m, &op_fix2hex(reg_s(2), reg_i(0)));
        assert_eq!(
            m.read(&reg_s(2)).unwrap(),
            Value::Bytes(b"0x0ABC\0".to_vec())
        );

        m.write(&reg_l(0), Value::Int(0x0000_BEEF)).unwrap(); // 4-byte -> 0x0000BEEF
        run(&mut m, &op_fix2hex(reg_s(3), reg_l(0)));
        assert_eq!(
            m.read(&reg_s(3)).unwrap(),
            Value::Bytes(b"0x0000BEEF\0".to_vec())
        );
    }

    #[test]
    fn fix2dez_immediate_formats_at_its_encoded_width() {
        // An immediate IS a value type, so len is its ENCODED width from the
        // arg1 mode nibble, not 1 (EdOperations.cs:694). Imm16 (mode 6) 0x0100
        // -> (i16) -> "256"; Imm8 (mode 5) 0xFF -> (i8) -> "-1".
        let mut m = Machine::new();
        run(&mut m, &op_with_mode(0x7A, 0x16, reg_s(1), imm(0x0100)));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(b"256\0".to_vec()));

        run(&mut m, &op_with_mode(0x7A, 0x15, reg_s(2), imm(0xFF)));
        assert_eq!(m.read(&reg_s(2)).unwrap(), Value::Bytes(b"-1\0".to_vec()));
    }

    #[test]
    fn fix2hex_immediate_formats_at_its_encoded_width() {
        // Imm16 (mode 6) 0x0100 -> "0x0100" (0x%04X); Imm32 (mode 7) ->
        // "0x00000100" (0x%08X) (EdOperations.cs:757).
        let mut m = Machine::new();
        run(&mut m, &op_with_mode(0x79, 0x16, reg_s(1), imm(0x0100)));
        assert_eq!(
            m.read(&reg_s(1)).unwrap(),
            Value::Bytes(b"0x0100\0".to_vec())
        );

        run(&mut m, &op_with_mode(0x79, 0x17, reg_s(2), imm(0x0100)));
        assert_eq!(
            m.read(&reg_s(2)).unwrap(),
            Value::Bytes(b"0x00000100\0".to_vec())
        );
    }

    #[test]
    fn fix2_byte_buffer_arg1_formats_its_first_byte() {
        // A byte buffer is NOT a value type -> len 1: format only the FIRST
        // byte (EdOperations.cs:694/757), never a buffer-length-wide LE value.
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(vec![0xAB, 0xCD])).unwrap();
        run(&mut m, &op_fix2hex(reg_s(1), reg_s(0)));
        assert_eq!(m.read(&reg_s(1)).unwrap(), Value::Bytes(b"0xAB\0".to_vec()));

        run(&mut m, &op_fix2dez(reg_s(2), reg_s(0))); // (i8)0xAB = -85
        assert_eq!(m.read(&reg_s(2)).unwrap(), Value::Bytes(b"-85\0".to_vec()));
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
            step_bare(&mut m, &op_y42flt(reg_f(0), reg_s(0))),
            Err(ExecError::InvalidOperand("y42flt"))
        );
    }

    // ---- Task 10: string / result-store / param ----

    /// A string-literal operand (`ImmStr`), used for result names and inputs.
    fn str_lit(s: &str) -> Operand {
        Operand::Str(s.as_bytes().to_vec())
    }

    /// An `S`-register operand indexed by an immediate start position with no
    /// trailing length — the `IdxImm` shape `serase`/`spaste` require.
    fn idx_s(idx: u8, start: i64) -> Operand {
        Operand::Indexed {
            base: RegId {
                bank: RegBank::S,
                idx,
            },
            index: IndexArg::Imm(start),
            len: None,
        }
    }

    /// Builds an [`ExecCtx`] over the given result set and job-arg buffer, with no
    /// tables and a fresh (unselected) table cursor.
    fn mk_ctx<'a>(results: &'a mut ResultSet, args: &'a [u8]) -> ExecCtx<'a> {
        ExecCtx {
            results,
            args,
            tables: &[],
            current_table: None,
            current_row: None,
        }
    }

    #[test]
    fn ergr_pushes_real_under_name() {
        // ergr "TEMP", F1 -> ResultData::Real(89.96) in the current set.
        let mut m = Machine::new();
        m.f[1] = 89.96;
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, &[]);
        assert_eq!(
            step(&mut m, &op(0x38, str_lit("TEMP"), reg_f(1)), &mut c).unwrap(),
            Flow::Next
        );
        assert!(
            matches!(c.results.get("TEMP"), Some(ResultData::Real(v)) if (*v - 89.96).abs() < 1e-9)
        );
    }

    #[test]
    fn ergs_pushes_text() {
        // ergs "UNIT", S0("degC") -> ResultData::Text("degC") (NUL-terminated read).
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(b"degC\0".to_vec()))
            .unwrap();
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, &[]);
        step(&mut m, &op(0x39, str_lit("UNIT"), reg_s(0)), &mut c).unwrap();
        assert_eq!(
            c.results.get("UNIT"),
            Some(&ResultData::Text("degC".into()))
        );
    }

    #[test]
    fn ergb_ergw_ergd_store_unsigned_widths() {
        // One L0 = 0x1234_5678 stored under three widths truncates the low bytes.
        let mut m = Machine::new();
        m.write(&reg_l(0), Value::Int(0x1234_5678)).unwrap();
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, &[]);
        step(&mut m, &op(0x34, str_lit("B"), reg_l(0)), &mut c).unwrap(); // ergb
        step(&mut m, &op(0x35, str_lit("W"), reg_l(0)), &mut c).unwrap(); // ergw
        step(&mut m, &op(0x36, str_lit("D"), reg_l(0)), &mut c).unwrap(); // ergd
        assert_eq!(c.results.get("B"), Some(&ResultData::Byte(0x78)));
        assert_eq!(c.results.get("W"), Some(&ResultData::Word(0x5678)));
        assert_eq!(c.results.get("D"), Some(&ResultData::Dword(0x1234_5678)));
    }

    #[test]
    fn ergc_ergi_ergl_sign_extend() {
        // All-ones low bytes read back as -1 through the signed casts.
        let mut m = Machine::new();
        m.write(&reg_l(0), Value::Int(0xFFFF_FFFF)).unwrap();
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, &[]);
        step(&mut m, &op(0x81, str_lit("C"), reg_l(0)), &mut c).unwrap(); // ergc: SByte
        step(&mut m, &op(0x37, str_lit("I"), reg_l(0)), &mut c).unwrap(); // ergi: Int16
        step(&mut m, &op(0x82, str_lit("L"), reg_l(0)), &mut c).unwrap(); // ergl: Int32
        assert_eq!(c.results.get("C"), Some(&ResultData::Int(-1)));
        assert_eq!(c.results.get("I"), Some(&ResultData::Int(-1)));
        assert_eq!(c.results.get("L"), Some(&ResultData::Int(-1)));
    }

    #[test]
    fn ergy_pushes_raw_bytes() {
        // ergy "RAW", S0 -> ResultData::Binary of the raw buffer (no NUL trim).
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(vec![0x01, 0x00, 0x02]))
            .unwrap();
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, &[]);
        step(&mut m, &op(0x3F, str_lit("RAW"), reg_s(0)), &mut c).unwrap();
        assert_eq!(
            c.results.get("RAW"),
            Some(&ResultData::Binary(vec![0x01, 0x00, 0x02]))
        );
    }

    #[test]
    fn enewset_on_nonempty_starts_a_new_set() {
        // A value before enewset lands in set 0; one after lands in set 1, so the
        // current set sees only the second.
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(1)).unwrap();
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, &[]);
        step(&mut m, &op(0x34, str_lit("BEFORE"), reg_b(0)), &mut c).unwrap();
        step(&mut m, &op(0x40, Operand::None, Operand::None), &mut c).unwrap(); // enewset
        step(&mut m, &op(0x34, str_lit("AFTER"), reg_b(0)), &mut c).unwrap();
        assert_eq!(c.results.get("AFTER"), Some(&ResultData::Byte(1)));
        assert_eq!(c.results.get("BEFORE"), None); // committed to the previous set
    }

    #[test]
    fn enewset_on_empty_set_is_a_noop() {
        // enewset on an empty current set must NOT create a second empty set: a
        // value pushed afterwards is still visible in the same current set.
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(7)).unwrap();
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, &[]);
        step(&mut m, &op(0x40, Operand::None, Operand::None), &mut c).unwrap(); // enewset (empty)
        step(&mut m, &op(0x34, str_lit("X"), reg_b(0)), &mut c).unwrap();
        assert_eq!(c.results.get("X"), Some(&ResultData::Byte(7)));
    }

    #[test]
    fn etag_falls_through_without_jumping() {
        // Phase 1 has no result-request filter, so etag is a no-op fall-through.
        let mut m = Machine::new();
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, &[]);
        let e = op(0x41, str_lit("TAG"), Operand::None);
        assert_eq!(step(&mut m, &e, &mut c).unwrap(), Flow::Next);
    }

    #[test]
    fn parl_reads_arg_value_and_clears_zero() {
        // Job args "0x2A;99"; parl I0, #1 reads field 1 ("0x2A") = 42, Zero clear.
        let mut m = Machine::new();
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, b"0x2A;99");
        step(&mut m, &op(0x57, reg_i(0), imm(1)), &mut c).unwrap(); // parl
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0x2A));
        assert!(!m.flags.z);
    }

    #[test]
    fn parl_absent_arg_sets_zero_and_reads_zero() {
        // No args: parl finds nothing -> result 0, Zero SET (the "arg missing"
        // signal). This is faithful reference behavior, not a hard error.
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(0x1234)).unwrap();
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, &[]);
        step(&mut m, &op(0x57, reg_i(0), imm(1)), &mut c).unwrap();
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0));
        assert!(m.flags.z);
    }

    #[test]
    fn parn_counts_arg_fields() {
        // "A;B;C" splits into 3 fields.
        let mut m = Machine::new();
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, b"A;B;C");
        step(&mut m, &op(0x80, reg_i(0), Operand::None), &mut c).unwrap(); // parn
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(3));
        assert!(!m.flags.z);
    }

    #[test]
    fn pary_copies_raw_binary_args() {
        // pary S0 copies the whole raw arg buffer (not split), clears Zero.
        let mut m = Machine::new();
        let mut results = ResultSet::new();
        let mut c = mk_ctx(&mut results, &[0x01, 0x3B, 0x02]);
        step(&mut m, &op(0x7F, reg_s(0), Operand::None), &mut c).unwrap();
        assert_eq!(
            m.read(&reg_s(0)).unwrap(),
            Value::Bytes(vec![0x01, 0x3B, 0x02])
        );
        assert!(!m.flags.z);
    }

    #[test]
    fn scmp_sets_zero_on_equal_arrays() {
        // scmp sets Zero on EQUALITY (contrast strcmp below).
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(vec![1, 2, 3])).unwrap();
        m.write(&reg_s(1), Value::Bytes(vec![1, 2, 3])).unwrap();
        run(&mut m, &op(0x20, reg_s(0), reg_s(1)));
        assert!(m.flags.z);
        m.write(&reg_s(1), Value::Bytes(vec![1, 2, 4])).unwrap();
        run(&mut m, &op(0x20, reg_s(0), reg_s(1)));
        assert!(!m.flags.z);
    }

    #[test]
    fn strcmp_sets_zero_on_difference_inverse_of_scmp() {
        // strcmp sets Zero when the strings DIFFER -- the faithful inverse of scmp.
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(b"abc\0".to_vec())).unwrap();
        m.write(&reg_s(1), Value::Bytes(b"abc\0".to_vec())).unwrap();
        run(&mut m, &op(0x8F, reg_s(0), reg_s(1)));
        assert!(!m.flags.z); // equal -> Zero CLEAR
        m.write(&reg_s(1), Value::Bytes(b"abd\0".to_vec())).unwrap();
        run(&mut m, &op(0x8F, reg_s(0), reg_s(1)));
        assert!(m.flags.z); // differ -> Zero SET
    }

    #[test]
    fn slen_is_raw_length_strlen_is_nul_terminated() {
        // S0 = "AB\0CD": raw length 5 (slen) vs NUL-terminated length 2 (strlen).
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(b"AB\0CD".to_vec()))
            .unwrap();
        run(&mut m, &op(0x23, reg_i(0), reg_s(0))); // slen
        run(&mut m, &op(0x90, reg_i(1), reg_s(0))); // strlen
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(5));
        assert_eq!(m.read(&reg_i(1)).unwrap(), Value::Int(2));
    }

    #[test]
    fn scat_concatenates_raw_bytes() {
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(vec![0x01, 0x02])).unwrap();
        m.write(&reg_s(1), Value::Bytes(vec![0x03])).unwrap();
        run(&mut m, &op(0x21, reg_s(0), reg_s(1)));
        assert_eq!(
            m.read(&reg_s(0)).unwrap(),
            Value::Bytes(vec![0x01, 0x02, 0x03])
        );
    }

    #[test]
    fn scut_drops_trailing_bytes() {
        // Cut 2 bytes off the end of a 5-byte array; cutting past the end empties it.
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(vec![1, 2, 3, 4, 5]))
            .unwrap();
        run(&mut m, &op(0x22, reg_s(0), imm(2)));
        assert_eq!(m.read(&reg_s(0)).unwrap(), Value::Bytes(vec![1, 2, 3]));
        run(&mut m, &op(0x22, reg_s(0), imm(9)));
        assert_eq!(m.read(&reg_s(0)).unwrap(), Value::Bytes(vec![]));
    }

    #[test]
    fn serase_removes_a_range() {
        // Erase 2 bytes starting at index 1 of [1,2,3,4,5] -> [1,4,5].
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(vec![1, 2, 3, 4, 5]))
            .unwrap();
        run(&mut m, &op(0x25, idx_s(0, 1), imm(2)));
        assert_eq!(m.read(&reg_s(0)).unwrap(), Value::Bytes(vec![1, 4, 5]));
    }

    #[test]
    fn spaste_inserts_at_index() {
        // Insert [9,9] into [1,2,3] at index 1 -> [1,9,9,2,3].
        let mut m = Machine::new();
        m.write(&reg_s(0), Value::Bytes(vec![1, 2, 3])).unwrap();
        m.write(&reg_s(1), Value::Bytes(vec![9, 9])).unwrap();
        run(&mut m, &op(0x24, idx_s(0, 1), reg_s(1)));
        assert_eq!(
            m.read(&reg_s(0)).unwrap(),
            Value::Bytes(vec![1, 9, 9, 2, 3])
        );
    }

    // ---- Task 11: table ops + atsp ----

    /// Build a synthetic [`Table`] from string slices (no BMW data).
    fn tbl(name: &str, columns: &[&str], rows: &[&[&str]]) -> Table {
        Table {
            name: name.into(),
            columns: columns.iter().map(|s| (*s).into()).collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|s| (*s).into()).collect())
                .collect(),
        }
    }

    /// Builds an [`ExecCtx`] over a table set with a fresh (unselected) cursor.
    fn mk_table_ctx<'a>(results: &'a mut ResultSet, tables: &'a [Table]) -> ExecCtx<'a> {
        ExecCtx {
            results,
            args: &[],
            tables,
            current_table: None,
            current_row: None,
        }
    }

    #[test]
    fn tabset_resolves_case_insensitively() {
        // Stored uppercase `RES_0X5001`; a mixed-case `tabset "res_0x5001"` must
        // still find it (the whole reason `Prg::table_ci` exists).
        let mut m = Machine::new();
        let tables = vec![tbl("RES_0X5001", &["A"], &[&["1"]])];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        step(
            &mut m,
            &op(0x7B, str_lit("res_0x5001"), Operand::None),
            &mut c,
        )
        .unwrap();
        assert_eq!(c.current_table, Some(0));
        assert_eq!(c.current_row, None); // reset on select
    }

    #[test]
    fn tabset_unknown_table_is_hard_error() {
        let mut m = Machine::new();
        let tables = vec![tbl("RES_0X5001", &["A"], &[&["1"]])];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        assert_eq!(
            step(&mut m, &op(0x7B, str_lit("NOSUCH"), Operand::None), &mut c),
            Err(ExecError::TableNotFound("NOSUCH".into()))
        );
    }

    #[test]
    fn tabset_reselecting_same_table_preserves_row_cursor() {
        // tabset T; tabline 1; tabset T again -> row cursor stays Some(1) (EDIABAS
        // restores `_tableRowIndex` when the SAME table is re-selected).
        let mut m = Machine::new();
        let tables = vec![tbl("T", &["A"], &[&["r0"], &["r1"]])];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        step(&mut m, &op(0x7B, str_lit("T"), Operand::None), &mut c).unwrap();
        step(&mut m, &op(0x83, imm(1), Operand::None), &mut c).unwrap(); // tabline 1
        assert_eq!(c.current_row, Some(1));
        step(&mut m, &op(0x7B, str_lit("T"), Operand::None), &mut c).unwrap(); // re-select
        assert_eq!(c.current_row, Some(1)); // preserved
    }

    #[test]
    fn tabset_switching_tables_resets_row_cursor() {
        let mut m = Machine::new();
        let tables = vec![
            tbl("A", &["X"], &[&["a0"], &["a1"]]),
            tbl("B", &["Y"], &[&["b0"]]),
        ];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        step(&mut m, &op(0x7B, str_lit("A"), Operand::None), &mut c).unwrap();
        step(&mut m, &op(0x83, imm(1), Operand::None), &mut c).unwrap(); // row 1 in A
        assert_eq!(c.current_row, Some(1));
        step(&mut m, &op(0x7B, str_lit("B"), Operand::None), &mut c).unwrap(); // switch
        assert_eq!(c.current_table, Some(1));
        assert_eq!(c.current_row, None); // reset
    }

    #[test]
    fn tabseek_hit_and_miss_set_zero() {
        // Column "NAME" rows alpha/beta; seek "BETA" (case-insensitive) hits row 1
        // and clears Zero; seek "gamma" misses -> cursor clamps to last row, Zero
        // SET.
        let mut m = Machine::new();
        let tables = vec![tbl(
            "T",
            &["NAME", "VAL"],
            &[&["alpha", "1"], &["beta", "2"]],
        )];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        step(&mut m, &op(0x7B, str_lit("T"), Operand::None), &mut c).unwrap();
        step(&mut m, &op(0x7C, str_lit("NAME"), str_lit("BETA")), &mut c).unwrap();
        assert_eq!(c.current_row, Some(1));
        assert!(!m.flags.z);
        step(&mut m, &op(0x7C, str_lit("NAME"), str_lit("gamma")), &mut c).unwrap();
        assert_eq!(c.current_row, Some(1)); // clamped to the last data row
        assert!(m.flags.z);
    }

    #[test]
    fn tabseek_on_empty_table_is_hard_error() {
        // A header-only table (no data rows): a seek miss has no last row to fall
        // back to -> the reference's `rowIndex < 0` -> EDIABAS_BIP_0010 hard error.
        let mut m = Machine::new();
        let tables = vec![tbl("T", &["NAME"], &[])];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        step(&mut m, &op(0x7B, str_lit("T"), Operand::None), &mut c).unwrap();
        assert_eq!(
            step(&mut m, &op(0x7C, str_lit("NAME"), str_lit("x")), &mut c),
            Err(ExecError::TableRow("tabseek"))
        );
    }

    #[test]
    fn tabget_reads_named_cell_from_current_row() {
        let mut m = Machine::new();
        let tables = vec![tbl(
            "T",
            &["NAME", "VAL"],
            &[&["alpha", "10"], &["beta", "20"]],
        )];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        step(&mut m, &op(0x7B, str_lit("T"), Operand::None), &mut c).unwrap();
        step(&mut m, &op(0x7C, str_lit("NAME"), str_lit("beta")), &mut c).unwrap();
        step(&mut m, &op(0x7D, reg_s(0), str_lit("VAL")), &mut c).unwrap();
        // tabget writes a NUL-terminated string, like EDIABAS `SetStringData`.
        assert_eq!(m.read(&reg_s(0)).unwrap(), Value::Bytes(b"20\0".to_vec()));
    }

    #[test]
    fn tabget_unknown_column_is_hard_error() {
        let mut m = Machine::new();
        let tables = vec![tbl("T", &["NAME"], &[&["x"]])];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        step(&mut m, &op(0x7B, str_lit("T"), Operand::None), &mut c).unwrap();
        step(&mut m, &op(0x83, imm(0), Operand::None), &mut c).unwrap(); // select row 0
        assert_eq!(
            step(&mut m, &op(0x7D, reg_s(0), str_lit("NOPE")), &mut c),
            Err(ExecError::TableColumn {
                op: "tabget",
                column: "NOPE".into()
            })
        );
    }

    #[test]
    fn tabseeku_matches_numeric_cell_value() {
        // ID column holds hex DIDs; tabseeku "ID", #0x5001 finds the row by parsing
        // each cell with StringToValue and comparing numerically.
        let mut m = Machine::new();
        let tables = vec![tbl(
            "SG_FUNKTIONEN",
            &["ID", "RESULTNAME"],
            &[&["0x4BC3", "TEMP"], &["0x5001", "UBAT"]],
        )];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        step(
            &mut m,
            &op(0x7B, str_lit("SG_FUNKTIONEN"), Operand::None),
            &mut c,
        )
        .unwrap();
        step(&mut m, &op(0x9A, str_lit("ID"), imm(0x5001)), &mut c).unwrap();
        assert_eq!(c.current_row, Some(1));
        assert!(!m.flags.z);
        step(&mut m, &op(0x7D, reg_s(0), str_lit("RESULTNAME")), &mut c).unwrap();
        assert_eq!(m.read(&reg_s(0)).unwrap(), Value::Bytes(b"UBAT\0".to_vec()));
    }

    #[test]
    fn tabline_selects_row_and_clamps_past_end() {
        let mut m = Machine::new();
        let tables = vec![tbl("T", &["A"], &[&["r0"], &["r1"], &["r2"]])];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        step(&mut m, &op(0x7B, str_lit("T"), Operand::None), &mut c).unwrap();
        step(&mut m, &op(0x83, imm(1), Operand::None), &mut c).unwrap(); // tabline 1
        assert_eq!(c.current_row, Some(1));
        assert!(!m.flags.z);
        step(&mut m, &op(0x83, imm(9), Operand::None), &mut c).unwrap(); // past end
        assert_eq!(c.current_row, Some(2)); // clamped to the last row
        assert!(m.flags.z);
    }

    #[test]
    fn tabcols_and_tabrows_report_dimensions() {
        // 3 columns, 2 data rows -> tabcols 3, tabrows 3 (2 data + 1 header).
        let mut m = Machine::new();
        let tables = vec![tbl(
            "T",
            &["A", "B", "C"],
            &[&["1", "2", "3"], &["4", "5", "6"]],
        )];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        step(&mut m, &op(0x7B, str_lit("T"), Operand::None), &mut c).unwrap();
        step(&mut m, &op(0xB6, reg_i(0), Operand::None), &mut c).unwrap(); // tabcols
        step(&mut m, &op(0xB7, reg_i(1), Operand::None), &mut c).unwrap(); // tabrows
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(3));
        assert_eq!(m.read(&reg_i(1)).unwrap(), Value::Int(3)); // includes the header
    }

    #[test]
    fn tabcols_with_no_table_selected_is_zero() {
        // The documented non-erroring exception: tabcols reports 0 (not a hard
        // error) when no table is selected, faithful to `OpTabcols`.
        let mut m = Machine::new();
        let tables: Vec<Table> = Vec::new();
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        step(&mut m, &op(0xB6, reg_i(0), Operand::None), &mut c).unwrap();
        assert_eq!(m.read(&reg_i(0)).unwrap(), Value::Int(0));
    }

    #[test]
    fn table_ops_without_selected_table_are_hard_errors() {
        // No tabset -> current_table None -> EDIABAS_BIP_0010 hard error for the
        // ops that require a selected table.
        let mut m = Machine::new();
        let tables = vec![tbl("T", &["NAME"], &[&["x"]])];
        let mut results = ResultSet::new();
        let mut c = mk_table_ctx(&mut results, &tables);
        assert_eq!(
            step(&mut m, &op(0x7D, reg_s(0), str_lit("NAME")), &mut c),
            Err(ExecError::TableNotSelected("tabget"))
        );
        assert_eq!(
            step(&mut m, &op(0x7C, str_lit("NAME"), str_lit("x")), &mut c),
            Err(ExecError::TableNotSelected("tabseek"))
        );
    }

    #[test]
    fn atsp_reads_word_from_stack_without_popping() {
        // push I0=0x1234 -> stack [0x34,0x12]; atsp I1,#2 peeks the top word 0x1234
        // (big-endian) WITHOUT popping.
        let mut m = Machine::new();
        m.write(&reg_i(0), Value::Int(0x1234)).unwrap();
        run(&mut m, &op(0x1E, reg_i(0), Operand::None)); // push I0
        run(&mut m, &op(0x50, reg_i(1), imm(2))); // atsp I1, #2
        assert_eq!(m.read(&reg_i(1)).unwrap(), Value::Int(0x1234));
        assert!(!m.flags.z);
        assert!(!m.flags.s); // 0x1234: sign bit (bit 15) clear
        assert_eq!(m.data_stack, vec![0x34, 0x12]); // NOT popped
    }

    #[test]
    fn atsp_updates_sign_flag_from_peeked_value() {
        // push B0=0x80 -> stack [0x80]; atsp B1,#1 peeks 0x80 -> Sign set (bit 7).
        let mut m = Machine::new();
        m.write(&reg_b(0), Value::Int(0x80)).unwrap();
        run(&mut m, &op(0x1E, reg_b(0), Operand::None)); // push B0 (1 byte)
        run(&mut m, &op(0x50, reg_b(1), imm(1))); // atsp B1, #1
        assert_eq!(m.read(&reg_b(1)).unwrap(), Value::Int(0x80));
        assert!(m.flags.s);
        assert!(!m.flags.z);
    }

    #[test]
    fn atsp_underflow_is_hard_error() {
        // Empty stack, atsp a 2-byte word -> EDIABAS_BIP_0005 hard error.
        let mut m = Machine::new();
        assert_eq!(
            step_bare(&mut m, &op(0x50, reg_i(0), imm(2))),
            Err(ExecError::AtspStack)
        );
    }

    // ---- Task 12: move-to-S (request-builder) + the xsend/xrequf comm bridge ----

    #[test]
    fn move_str_literal_into_s_register_writes_bytes() {
        // move S1, {2C 03 F3 03}: the request-builder move Task 12 enables — a
        // byte-string literal into an S register writes those bytes verbatim.
        let mut m = Machine::new();
        run(
            &mut m,
            &op_move(reg_s(1), Operand::Str(vec![0x2C, 0x03, 0xF3, 0x03])),
        );
        assert_eq!(
            m.read(&reg_s(1)).unwrap(),
            Value::Bytes(vec![0x2C, 0x03, 0xF3, 0x03])
        );
    }

    #[test]
    fn move_shorter_source_into_s_keeps_the_longer_tail() {
        // move S2, S3 (an S-reg -> S-reg move): OpMove's RegS branch
        // (EdOperations.cs:1320-1329) copies the source over the FRONT of the
        // destination and keeps any tail beyond the source's length — a partial
        // overwrite, not a replace. Pin that faithful quirk.
        let mut m = Machine::new();
        m.write(&reg_s(2), Value::Bytes(vec![0xAA, 0xBB, 0xCC, 0xDD]))
            .unwrap();
        m.write(&reg_s(3), Value::Bytes(vec![0x11, 0x22])).unwrap();
        run(&mut m, &op_move(reg_s(2), reg_s(3)));
        assert_eq!(
            m.read(&reg_s(2)).unwrap(),
            Value::Bytes(vec![0x11, 0x22, 0xCC, 0xDD])
        );
    }

    #[test]
    fn xsend_returns_exchange_flow_with_request_and_dest() {
        // Build the request in S1 (move S1, {2C 03 F3 03}), then xsend S4, S1:
        // step surfaces the request bytes (arg1) and S4 (arg0) as the response
        // destination WITHOUT awaiting — the run loop (Task 13) does the transmit.
        let mut m = Machine::new();
        run(
            &mut m,
            &op_move(reg_s(1), Operand::Str(vec![0x2C, 0x03, 0xF3, 0x03])),
        );
        let flow = step_bare(&mut m, &op(0x2A, reg_s(4), reg_s(1))).unwrap();
        assert_eq!(
            flow,
            Flow::Exchange {
                request: vec![0x2C, 0x03, 0xF3, 0x03],
                dest: reg_s(4),
            }
        );
    }

    #[test]
    fn xsend_non_byte_request_is_hard_error() {
        // arg1 must be a byte buffer (the built request). An integer register is
        // not byte-readable -> a hard InvalidOperand, never a guessed frame.
        let mut m = Machine::new();
        assert_eq!(
            step_bare(&mut m, &op(0x2A, reg_s(4), reg_i(0))),
            Err(ExecError::InvalidOperand("xsend"))
        );
    }

    #[test]
    fn xrequf_is_unimplemented_in_phase_1() {
        // xrequf (0x2C) is a streaming receive with no single-shot request/response
        // shape; Phase 1 defers it as a loud Unimplemented.
        let mut m = Machine::new();
        assert_eq!(
            step_bare(&mut m, &op(0x2C, reg_s(0), Operand::None)),
            Err(ExecError::Unimplemented("xrequf"))
        );
    }
}
