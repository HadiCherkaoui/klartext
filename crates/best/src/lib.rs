//! BEST/2 bytecode VM + EDIABAS job engine for klartext (offline Phase 1).
//!
//! Decodes and interprets a BMW BEST/2 job to execute one named EDIABAS job:
//! build the UDS request(s), exchange them (Phase 1: a mock), and parse the
//! response into named, scaled results. See
//! `docs/superpowers/specs/2026-07-05-best2-vm-job-engine-design.md`.

mod decode;
mod opcode;

#[doc(inline)]
pub use decode::{AddrMode, DecodeError, IndexArg, Op, Operand, RegBank, RegId, decode_job};
#[doc(inline)]
pub use opcode::{OpClass, OpInfo, info};

#[cfg(test)]
mod tests {
    #[test]
    fn crate_builds() {
        assert_eq!(2 + 2, 4);
    }
}
