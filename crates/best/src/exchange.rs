//! The UDS exchange abstraction the comm opcodes transmit through.
//!
//! A BEST/2 job's `xsend` opcode (0x2A) transmits a request telegram to the ECU
//! and waits for the response. [`UdsExchange`] is that transmit/receive seam:
//! the run loop (Task 13) calls [`UdsExchange::request`] whenever the executor
//! surfaces a [`crate::Flow::Exchange`], then writes the response back into the
//! job's destination register. It is the sync executor's one async dependency,
//! held at the run-loop boundary so [`crate::step`] itself never awaits.
//!
//! Phase 1 ships one implementor: [`MockExchange`], a table of canned
//! request→response pairs that drives the offline oracle with no car attached.
//! The live implementor — a thin adapter over `klartext-client`'s session — is a
//! Task 13 concern and adds no new protocol logic here.
//!
//! ## Async at a `dyn` boundary
//! Task 13 dispatches through `&dyn UdsExchange` so the run loop is generic over
//! mock vs. live. A bare `async fn` in a trait is not `dyn`-compatible, so the
//! trait uses [`macro@async_trait`] to box the returned future — the standard
//! path for an object-safe async trait method.
//!
//! ## No degrade-to-raw
//! The mock answers only an EXACT request-byte match; an unknown request is a
//! hard [`ExchangeError::Unexpected`] carrying the offending bytes, never a
//! silent empty response. A wrong answer is worse than a loud stop.

use async_trait::async_trait;
use std::collections::HashMap;

/// An error from a [`UdsExchange::request`].
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ExchangeError {
    /// The [`MockExchange`] had no canned response for these exact request bytes.
    #[error("no canned response for request {0:02X?}")]
    Unexpected(Vec<u8>),
    /// A gated UDS service ID reached the read-only seam and was refused (spec §6).
    /// Carries the SID and the full telegram that was blocked.
    #[error("read-only gate refused service 0x{sid:02X} (frame {frame:02X?})")]
    Refused {
        /// The UDS service ID (the byte after the telegram header) that was gated.
        sid: u8,
        /// The full outgoing telegram that was refused.
        frame: Vec<u8>,
    },
    /// The live transport failed. Message-only so `klartext-best` need not depend on
    /// `klartext-client`; the binary's bridge formats its `ClientError` into this.
    #[error("transport error: {0}")]
    Transport(String),
}

/// A UDS request/response transport the comm opcodes exchange through.
///
/// Models the live analogue `klartext-client`'s `Session::request(target, uds)`:
/// transmit the raw UDS `uds` bytes to ECU address `target` and return the raw
/// response payload. The run loop calls this from an async context; the sync
/// executor only *describes* the exchange via [`crate::Flow::Exchange`].
///
/// The trait is `dyn`-compatible via [`macro@async_trait`] so Task 13 can hold a
/// `&dyn UdsExchange` and pick the implementor (mock or live) at run time.
#[async_trait]
pub trait UdsExchange {
    /// Transmit `uds` to ECU `target` and return the raw response payload.
    ///
    /// # Errors
    /// Implementation-defined; the [`MockExchange`] returns
    /// [`ExchangeError::Unexpected`] for a request it has no canned answer for.
    async fn request(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError>;
}

/// An offline [`UdsExchange`]: a table of canned request→response pairs.
///
/// Keys on the EXACT request bytes — there is no telegram framing at this layer,
/// so the map holds the raw UDS request a job's `xsend` builds (e.g. an `S`
/// register's contents) mapped to the raw response the ECU would return. The
/// `target` address is ignored: the mock is keyed purely by request bytes.
#[derive(Debug, Clone, Default)]
pub struct MockExchange {
    /// Canned request→response pairs; a request absent here is an error.
    map: HashMap<Vec<u8>, Vec<u8>>,
}

impl MockExchange {
    /// Creates a mock with no canned responses; every request errors until one is
    /// registered with [`MockExchange::on`].
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers `response` as the canned answer for the exact `request` bytes.
    ///
    /// A later `on` with the same `request` replaces the earlier response.
    pub fn on(&mut self, request: Vec<u8>, response: Vec<u8>) {
        self.map.insert(request, response);
    }
}

#[async_trait]
impl UdsExchange for MockExchange {
    async fn request(&self, _target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
        // `HashMap<Vec<u8>, _>` borrows its key as `[u8]`, so the slice matches
        // without an allocation; only the miss path clones, to carry the bytes.
        self.map
            .get(uds)
            .cloned()
            .ok_or_else(|| ExchangeError::Unexpected(uds.to_vec()))
    }
}

#[cfg(test)]
mod tests {
    use super::{ExchangeError, MockExchange, UdsExchange};

    #[tokio::test]
    async fn mock_exchange_returns_canned_response() {
        let mut mock = MockExchange::new();
        mock.on(vec![0x22, 0xF3, 0x03], vec![0x62, 0xF3, 0x03, 0x0E, 0x2F]);
        // The mock keys on the exact request bytes; the target address is ignored.
        assert_eq!(
            mock.request(0x12, &[0x22, 0xF3, 0x03]).await.unwrap(),
            vec![0x62, 0xF3, 0x03, 0x0E, 0x2F]
        );
    }

    #[tokio::test]
    async fn mock_exchange_unexpected_request_is_error() {
        // No-degrade: a request with no canned answer is a hard error carrying the
        // offending bytes, never a silent empty response.
        let mock = MockExchange::new();
        assert_eq!(
            mock.request(0x12, &[0x99, 0x00]).await,
            Err(ExchangeError::Unexpected(vec![0x99, 0x00]))
        );
    }

    #[tokio::test]
    async fn mock_exchange_is_usable_as_a_trait_object() {
        // Task 13 dispatches through `&dyn UdsExchange`; pin that the trait is
        // dyn-compatible (a bare async-fn trait would fail to compile here).
        let mut mock = MockExchange::new();
        mock.on(vec![0x10, 0x03], vec![0x50, 0x03]);
        let exchange: &dyn UdsExchange = &mock;
        assert_eq!(
            exchange.request(0x40, &[0x10, 0x03]).await.unwrap(),
            vec![0x50, 0x03]
        );
    }

    #[test]
    fn refused_and_transport_variants_carry_context() {
        // The read-only gate refusal shows the gated SID; the live-transport
        // failure shows its message. Both carry the context a caller needs to act.
        let r = ExchangeError::Refused {
            sid: 0x2E,
            frame: vec![0x83, 0x12, 0xF1, 0x2E, 0x50],
        };
        assert!(format!("{r}").contains("2E"));
        let t = ExchangeError::Transport("no response".into());
        assert!(format!("{t}").contains("no response"));
    }
}
