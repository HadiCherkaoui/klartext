//! The job engine: load an SGBD and run one named BEST/2 job to results.
//!
//! [`Ecu`] wraps a parsed [`Prg`] (its BEST/2 jobs and tables); [`Ecu::run_job`]
//! is the integration payoff of the whole VM — it decodes a job's bytecode,
//! drives the [`step`] loop over a [`Machine`], threads the job's tables and
//! arguments through an [`ExecCtx`], and performs each ECU exchange the executor
//! surfaces via [`Flow::Exchange`] against a [`UdsExchange`], collecting the job's
//! emitted [`ResultSet`].
//!
//! ## What the engine oracle showed
//! The `tests/oracle.rs` proof drives a hand-assembled scaling job through
//! [`Ecu::run_job`] and reproduces `klartext-semantic`'s engine transform
//! (`raw × 0.1 − 273.14 → 89.96 °C`) in the VM. Running the *real* F20 DDE
//! `STATUS_MOTORTEMPERATUR` job (BYO data) revealed that it does NOT scale in
//! bytecode: it returns the RAW response telegram (`_RESPONSE_1`) plus a status
//! text, and the 89.96 scaling is `klartext-semantic`'s table path
//! (`docs/sgbd-findings.md` §5), not this job's. The harness runs that real
//! bytecode correctly up to the deferred indexed `S`-register addressing mode
//! ([`crate::Operand::Indexed`]); see the oracle test for the full account.
//!
//! ## The run loop
//! [`step`] pre-advances `m.pc` to the byte just past each instruction before
//! dispatching (a taken jump then rewrites it), so the loop is simple: fetch the
//! op whose `offset == m.pc` via an `offset → index` map, [`step`] it, and act on
//! the returned [`Flow`]. On [`Flow::Exchange`] the loop is the async boundary —
//! it transmits the request and writes the response back into the destination
//! register — keeping [`step`] itself synchronous.
//!
//! ## No degrade-to-raw
//! A missing job, a program counter that is not an instruction boundary, an
//! unimplemented opcode, a decode fault, or a runaway loop are each a hard
//! [`RunError`], never a silent empty result. A wrong answer is worse than a
//! loud stop.

use std::collections::HashMap;

use klartext_sgbd::{Prg, SgbdError};

use crate::decode::{DecodeError, decode_job};
use crate::exchange::{ExchangeError, UdsExchange};
use crate::exec::{ExecCtx, ExecError, Flow, step};
use crate::machine::{Machine, Value};
use crate::result::ResultSet;

/// The ECU diagnostic address `Flow::Exchange` requests are sent to.
///
/// The DDE (diesel engine ECU) answers on diagnostic address `0x12` — the target
/// every F20 DDE measurement job addresses (`docs/sgbd-findings.md` §4, §7a). The
/// bytecode does not carry the target (it is the caller's knowledge), and the
/// Phase-1 [`crate::MockExchange`] ignores it entirely, so a single documented
/// default suffices here. Real per-job target-addressing (reading it from the
/// job's request header) is a Phase-2 concern.
const DEFAULT_TARGET: u8 = 0x12;

/// Upper bound on instructions executed before a job is declared non-terminating.
///
/// A guard against a control-flow bug spinning forever, mirroring the test-only
/// `drive` helper's iteration cap. The whole F20 DDE SGBD is ~400k instructions
/// across its 272 jobs (`docs/sgbd-findings.md` §3), so a single job runs to `eoj`
/// in far fewer than this; exceeding it means a jump loop that never terminates,
/// which is a hard [`RunError::LoopBound`] rather than a hang.
const MAX_INSTRUCTIONS: usize = 100_000;

/// A loadable ECU: a parsed SGBD whose named BEST/2 jobs can be run.
///
/// Built from a [`Prg`] via [`Ecu::load`] (or [`Ecu::open`] from a file path);
/// [`Ecu::run_job`] executes one job by name against a [`UdsExchange`].
#[derive(Debug)]
pub struct Ecu {
    /// The parsed SGBD: its job bytecode ([`Prg::job_bytecode`]) and the tables
    /// ([`Prg::tables`]) the `tab*` opcodes resolve against.
    prg: Prg,
}

/// An error from loading an [`Ecu`] or running one of its jobs.
///
/// Wraps the lower layers' faults ([`DecodeError`], [`ExecError`],
/// [`ExchangeError`], [`SgbdError`]) and adds the run loop's own hard stops: a
/// job that is not in the SGBD, a program counter that lands off an instruction
/// boundary, and a job that outran [`MAX_INSTRUCTIONS`].
#[derive(Debug, thiserror::Error)]
pub enum RunError {
    /// No job with the requested name exists in this SGBD.
    #[error("job `{0}` not found in this SGBD")]
    JobNotFound(String),
    /// Decoding the job's BEST/2 bytecode failed.
    #[error(transparent)]
    Decode(#[from] DecodeError),
    /// Executing one of the job's instructions failed.
    #[error(transparent)]
    Exec(#[from] ExecError),
    /// A `Flow::Exchange` transmit to the ECU failed.
    #[error(transparent)]
    Exchange(#[from] ExchangeError),
    /// Reading or parsing the SGBD file failed (from [`Ecu::open`]).
    #[error(transparent)]
    Sgbd(#[from] SgbdError),
    /// The program counter reached an offset that is not the start of any decoded
    /// instruction — a jump into the middle of an instruction, or past the decoded
    /// range (the reachable code ran beyond the job's first `eoj`).
    #[error("program counter {0} is not a decoded instruction boundary")]
    BadPc(usize),
    /// The job executed [`MAX_INSTRUCTIONS`] instructions without reaching `eoj`.
    #[error("job exceeded the {0}-instruction execution bound (non-terminating loop?)")]
    LoopBound(usize),
}

impl Ecu {
    /// Wraps an already-parsed [`Prg`] as a runnable ECU.
    pub fn load(prg: Prg) -> Self {
        Self { prg }
    }

    /// Reads and parses an SGBD file at `path`, then wraps it as an [`Ecu`].
    ///
    /// # Errors
    /// Returns [`RunError::Sgbd`] if the file cannot be read or is not a valid
    /// SGBD container.
    pub fn open(path: impl AsRef<std::path::Path>) -> Result<Self, RunError> {
        Ok(Self::load(Prg::open(path)?))
    }

    /// Runs the job named `name`, returning its emitted [`ResultSet`].
    ///
    /// Decodes the job's bytecode, then drives the [`step`] loop over a fresh
    /// [`Machine`]: it fetches the instruction at `m.pc` (which [`step`]
    /// pre-advances for every op), executes it, and acts on the returned
    /// [`Flow`] — advancing on [`Flow::Next`]/[`Flow::Jumped`], stopping on
    /// [`Flow::EndOfJob`], and on [`Flow::Exchange`] transmitting the request to
    /// the ECU via `exchange` (at [`DEFAULT_TARGET`]) and writing the response
    /// bytes back into the destination register. `args` is the job's raw input
    /// argument buffer (empty for a no-argument job); the SGBD's tables are
    /// threaded in so the `tab*` opcodes resolve.
    ///
    /// # Errors
    /// Returns [`RunError::JobNotFound`] when the SGBD has no such job,
    /// [`RunError::Decode`] when the bytecode does not decode, [`RunError::Exec`]
    /// when an instruction faults (including an unimplemented opcode),
    /// [`RunError::Exchange`] when an ECU transmit fails, [`RunError::BadPc`] when
    /// control reaches a non-instruction offset, and [`RunError::LoopBound`] when
    /// the job runs past [`MAX_INSTRUCTIONS`] without terminating.
    pub async fn run_job(
        &self,
        name: &str,
        args: &[u8],
        exchange: &dyn UdsExchange,
    ) -> Result<ResultSet, RunError> {
        let code = self
            .prg
            .job_bytecode(name)
            .ok_or_else(|| RunError::JobNotFound(name.to_string()))?;
        let ops = decode_job(code)?;
        // The offset → index map mirrors the pre-advanced byte-offset PC back onto
        // the op to run next; `step` leaves `m.pc` at the following instruction.
        let index: HashMap<usize, usize> =
            ops.iter().enumerate().map(|(i, o)| (o.offset, i)).collect();

        let mut m = Machine::new();
        let mut results = ResultSet::new();
        // `ctx` mutably borrows `results` and immutably borrows the SGBD's tables;
        // scope it so the borrow ends before `results` is returned below.
        let mut reached_eoj = false;
        {
            let mut ctx = ExecCtx {
                results: &mut results,
                args,
                tables: self.prg.tables(),
                current_table: None,
                current_row: None,
            };
            for _ in 0..MAX_INSTRUCTIONS {
                let &i = index.get(&m.pc).ok_or(RunError::BadPc(m.pc))?;
                match step(&mut m, &ops[i], &mut ctx)? {
                    Flow::Next | Flow::Jumped => {}
                    Flow::EndOfJob => {
                        reached_eoj = true;
                        break;
                    }
                    Flow::Exchange { request, dest } => {
                        // The async boundary: `step` only described the exchange.
                        // `m.pc` is already pre-advanced past the `xsend`, so the
                        // loop continues sequentially after writing the response.
                        let response = exchange.request(DEFAULT_TARGET, &request).await?;
                        m.write(&dest, Value::Bytes(response))
                            .map_err(ExecError::from)?;
                    }
                }
            }
        }
        if reached_eoj {
            Ok(results)
        } else {
            Err(RunError::LoopBound(MAX_INSTRUCTIONS))
        }
    }
}
