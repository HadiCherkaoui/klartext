//! Execute an ECU service function as ISTA's own phase cycle, behind preconditions.
//!
//! Orchestration only: this crate resolves a function's per-phase arguments from
//! the ISTA catalog, checks preconditions, and drives each phase through the
//! BEST/2 VM against an INJECTED exchange. It never opens a connection itself and
//! deliberately does not depend on `klartext-client` or `klartext-hsfz` — binaries
//! compose those, keeping the VM and the client apart as elsewhere in the workspace.

pub mod phase;
pub mod precondition;
pub mod runner;

pub use phase::{Invocation, Phase, invocations};
pub use precondition::{
    MeasurementReader, Precondition, PreconditionOutcome, Verdict, blocks, defaults_for, evaluate,
};
pub use runner::{JobRunner, PhaseOutcome, ServiceReport, Teardown, run_cycle};
