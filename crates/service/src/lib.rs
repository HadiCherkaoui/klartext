//! Execute an ECU service function as ISTA's own phase cycle.
//!
//! Orchestration only: this crate resolves a function's per-phase arguments from
//! the ISTA catalog and drives each phase through the BEST/2 VM against an INJECTED
//! exchange. It never opens a connection itself and deliberately does not depend on
//! `klartext-client` or `klartext-hsfz` — binaries compose those, keeping the VM
//! and the client apart as elsewhere in the workspace.

pub mod phase;
pub mod runner;

pub use phase::{Invocation, Phase, function_ids, invocations};
// `run_cycle` is deliberately NOT re-exported: it is the crate-private cycle
// mechanism the unit tests drive directly. `run_service` (start/hold) and
// `stop_service` (the deferred teardown) are the public entries a binary uses.
pub use runner::{
    Hold, JobRunner, PhaseOutcome, ServiceReport, Teardown, hold_for, run_service, stop_service,
};
