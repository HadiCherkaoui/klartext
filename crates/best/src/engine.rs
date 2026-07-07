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
//! The `tests/oracle.rs` proofs drive hand-assembled jobs through
//! [`Ecu::run_job`] and reproduce `klartext-semantic`'s engine transform
//! (`raw × 0.1 − 273.14 → 89.96 °C`) in the VM. The real F20 DDE
//! `STATUS_MOTORTEMPERATUR`/`STATUS_OELNIVEAU` jobs (BYO data) now run
//! un-ignored, full-range, all the way to `eoj`: indexed `S`-register addressing
//! ([`crate::Operand::Indexed`]) landed in the engine (Tasks 5–6), so the
//! harness executes each whole real body rather than just its arg-validation
//! stub. The earlier Phase-1 "raw-only" reading was a first-`eoj` truncation
//! artifact — `decode_job` stopped at the first `eoj` — corrected in §1 of
//! `docs/superpowers/specs/2026-07-06-item5-guided-service-procedures-design.md`:
//! the generic framework jobs do scale in bytecode (the scaled values themselves
//! stay `[verify against capture]` until the on-car session). `klartext-semantic`'s
//! on-car-verified table path remains the DDE fast path (`docs/sgbd-findings.md`
//! §5).
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
use crate::exec::{ExecCtx, ExecError, Flow, TRAP_BIT_NO_RESPONSE, set_error, step};
use crate::machine::{Machine, Value};
use crate::result::ResultSet;

/// Upper bound on instructions executed before a job is declared non-terminating.
///
/// A guard against a control-flow bug spinning forever. The bound must clear the
/// worst *legitimate* job by an order of magnitude while still tripping a runaway
/// loop quickly:
/// * the largest single F20 job decodes to ~47k static instructions
///   (`docs/sgbd-findings.md` §3) — an absolute floor on the bound;
/// * a generic measurement job loops over its ECU's `SG_FUNKTIONEN` rows (up to
///   ~1,800 on the F20), running tens of instructions per row, so roughly
///   10^5–10^6 instructions actually execute in the largest real case.
///
/// `2_000_000` sits an order of magnitude above that worst legitimate case, yet a
/// non-terminating jump loop still reaches it in well under a second (the executor
/// runs far more than 2M simple ops per second), turning a hang into a hard
/// [`RunError::LoopBound`].
const MAX_INSTRUCTIONS: usize = 2_000_000;

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
/// boundary, and a job that outran `MAX_INSTRUCTIONS`.
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
    /// instruction — a jump to a mid-instruction offset, or into never-decoded
    /// padding past the decoded range (the full-range decode keeps every
    /// instruction to the job's end but stops at a post-`eoj` run of trailing
    /// bytes that no longer decode, and a jump into that tail lands here).
    #[error("program counter {0} is not a decoded instruction boundary")]
    BadPc(usize),
    /// The job executed `MAX_INSTRUCTIONS` instructions without reaching `eoj`.
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

    /// Runs the job named `name` against ECU address `target`, returning its
    /// emitted [`ResultSet`].
    ///
    /// Decodes the job's bytecode, then drives the [`step`] loop over a fresh
    /// [`Machine`]: it fetches the instruction at `m.pc` (which [`step`]
    /// pre-advances for every op), executes it, and acts on the returned
    /// [`Flow`] — advancing on [`Flow::Next`]/[`Flow::Jumped`], stopping on
    /// [`Flow::EndOfJob`], sleeping on [`Flow::Wait`], and on [`Flow::Exchange`]
    /// transmitting the request to ECU address `target` via `exchange` and
    /// writing the response bytes back into the destination register. `target` is
    /// the caller's knowledge (the bytecode does not carry it); `args` is the
    /// job's raw input argument buffer (empty for a no-argument job); the SGBD's
    /// tables are threaded in so the `tab*` opcodes resolve.
    ///
    /// The `exchange` is `Sync` (`&(dyn UdsExchange + Sync)`) so this method's
    /// future is `Send` and can be awaited from a multi-threaded async server — the
    /// MCP `run_job` tool, whose futures rmcp boxes as `Send` — not only from a
    /// single-threaded CLI. Every real exchange (mock, gated, telegram bridge) is
    /// already `Sync`, so this bound costs callers nothing.
    ///
    /// A failed exchange is not an automatic abort: it records EDIABAS's
    /// `IFH_0009` "no response" trap (bit 19) and, when the job masks that class,
    /// writes an empty response so the job's own `jt` path can produce its
    /// `JOB_STATUS` text. A job that does not mask it aborts, carrying the original
    /// transport fault (see [`RunError::Exchange`]).
    ///
    /// # Errors
    /// Returns [`RunError::JobNotFound`] when the SGBD has no such job,
    /// [`RunError::Decode`] when the bytecode does not decode, [`RunError::Exec`]
    /// when an instruction faults (including an unimplemented opcode or an unmasked
    /// trap), [`RunError::Exchange`] when an ECU exchange fails and the job does
    /// not mask the resulting no-response trap, [`RunError::BadPc`] when control
    /// reaches a non-instruction offset, and [`RunError::LoopBound`] when the job
    /// runs past `MAX_INSTRUCTIONS` without terminating.
    pub async fn run_job(
        &self,
        name: &str,
        target: u8,
        args: &[u8],
        exchange: &(dyn UdsExchange + Sync),
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
                        // loop continues sequentially after handling the response.
                        match exchange.request(target, &request).await {
                            Ok(response) => {
                                m.write(&dest, Value::Bytes(response))
                                    .map_err(ExecError::from)?;
                            }
                            Err(e) => {
                                // The reference records `IFH_0009` ("no response",
                                // trap bit 19, EdiabasNet.cs:3194) and lets the job's
                                // own `jt` error path handle it. A job that masks the
                                // class keeps running with an empty response; one that
                                // does not aborts — and we return the ORIGINAL
                                // transport error, which names the actual fault rather
                                // than just "trapped".
                                if set_error(&mut m, TRAP_BIT_NO_RESPONSE).is_err() {
                                    return Err(RunError::Exchange(e));
                                }
                                m.write(&dest, Value::Bytes(Vec::new()))
                                    .map_err(ExecError::from)?;
                            }
                        }
                    }
                    Flow::Wait { seconds } => {
                        // The other async boundary: `step` surfaced a `wait` pause
                        // without blocking (the reference sleeps inline only because
                        // its machine is synchronous). `seconds` is whole seconds
                        // (EdOperations.cs:3267 sleeps arg0 × 1000 ms); the PC is
                        // already advanced, so the loop resumes after the sleep.
                        tokio::time::sleep(std::time::Duration::from_secs(seconds.into())).await;
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
