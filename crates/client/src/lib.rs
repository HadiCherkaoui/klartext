//! klartext diagnostic client — the managed UDS session and typed read services.
//!
//! This crate is the layer that *talks to the car*: it composes `klartext-uds`
//! (pure UDS messages and decoders) with `klartext-hsfz` (the HSFZ transport and
//! gateway discovery) into a [`DiagnosticClient`] that auto-discovers the gateway,
//! holds a [`Session`] alive with a background keepalive, and exposes the M2
//! services — read DTCs, read a DID, and a confirmation-gated DTC clear.
//!
//! It is the reusable core for both the CLI today and the MCP server later; both
//! drive the same [`DiagnosticClient`] surface. Reads are autonomous-safe; the
//! caller is responsible for gating the clear behind explicit confirmation.

mod client;
mod error;
mod scan;
mod session;

pub use client::{
    ClientConfig, DEFAULT_BROADCAST, DiagnosticClient, EcuIdentification, FaultDetailRaw,
    IDENTIFICATION_DIDS, IdField, VehicleIdentity,
};
pub use error::ClientError;
pub use scan::{ClearReport, EcuFaults};
pub use session::{KEEPALIVE_INTERVAL, Session};

/// The gateway discovered on the link, re-exported from the transport crate.
pub use klartext_hsfz::Gateway;

// The primary types must stay `Send` so they can be held across `.await` points
// and used under tokio (and behind a future runtime abstraction).
const _: () = {
    const fn assert_send<T: Send>() {}
    assert_send::<Session>();
    assert_send::<DiagnosticClient>();
    assert_send::<ClientError>();
};
