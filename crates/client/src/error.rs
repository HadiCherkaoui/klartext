//! The unified error for the diagnostic client.

use std::net::Ipv4Addr;

use klartext_hsfz::Error as HsfzError;
use klartext_uds::{Nrc, UdsError};
use thiserror::Error;

/// An error from the diagnostic client: transport, decode, or protocol.
#[derive(Debug, Error)]
pub enum ClientError {
    /// The HSFZ transport failed (connect, discovery bind, I/O, or read timeout).
    #[error(transparent)]
    Hsfz(#[from] HsfzError),
    /// A UDS response payload could not be decoded.
    #[error(transparent)]
    Uds(#[from] UdsError),
    /// The ECU returned a negative response for the request.
    #[error("ECU rejected service 0x{sid:02X}: {nrc}")]
    Negative { sid: u8, nrc: Nrc },
    /// No interface had a link-local (169.254.x.x) address to discover from.
    #[error(
        "no link-local interface found — plug in the ENET cable (169.254.x.x) or pass a bind address"
    )]
    NoLinkLocalInterface,
    /// Discovery ran but no gateway answered.
    #[error(
        "no gateway answered discovery on {bind_ip} — is the ENET cable plugged in and the car awake (terminal 15)?"
    )]
    NoGatewayFound { bind_ip: Ipv4Addr },
    /// More than one gateway answered; the caller must choose one by IP.
    #[error("discovery found {count} gateways — connect to one explicitly by IP")]
    AmbiguousGateway { count: usize },
}
