//! Bridges the VM's BMW-FAST telegram exchange onto a bare-UDS transport.
//!
//! A BEST/2 job's `xsend` opcode emits a full BMW-FAST telegram
//! `[0x80|len][target][source][uds…][checksum]`, but a live transport — a
//! binary's thin adapter over `klartext-client`'s `Session::request` — speaks
//! *bare* UDS (`[SID …]`, no framing). [`TelegramExchange`] is the translation
//! seam between those two worlds: it lets the run loop drive a real ECU through
//! the very same [`UdsExchange`] the offline [`crate::MockExchange`] satisfies.
//!
//! ## The two directions
//! On each exchange the bridge:
//! 1. **decodes** the VM's outgoing request telegram with [`crate::decode`],
//!    recovering the bare UDS payload and the telegram's embedded destination;
//! 2. **cross-checks** that destination against the `target` the run loop passed
//!    — [`crate::Ecu::run_job`]'s `target` is authoritative, so a telegram
//!    addressed elsewhere is a hard error, never silently misrouted;
//! 3. forwards the bare `(target, uds)` to [`BareUdsTransport::call`]; and
//! 4. **re-encodes** the bare response as the ECU→tester reply telegram the VM
//!    parses next.
//!
//! ## Why the reply carries source `0xF1`
//! The VM's request is `[fmt][ecu][0xF1]…` — destination = the ECU, source = the
//! tester `0xF1`. The reply the job accepts is the mirror: it checks
//! `resp[1] == 0xF1` and `resp[2] == ecu` (the job length-checks the frame and
//! verifies exactly those two address bytes — frozen contract,
//! `crates/best/tests/differential.rs` lines 18-20 and 56). So the response is
//! re-framed with [`crate::encode`] as `encode(0xF1, target, &bare)`: the tester
//! `0xF1` in the destination byte, the ECU `target` in the source byte — NOT the
//! other way round.
//!
//! ## No new protocol logic
//! The bridge only reframes bytes; it decides nothing about UDS services. The
//! read-only SID gate and the live session sit on either side of it, so `cli`
//! and `mcp` share this one translation. Keeping the seam a bare trait is also
//! what keeps `klartext-best` free of a `klartext-client` dependency.

use crate::exchange::{ExchangeError, UdsExchange};
use crate::telegram;
use async_trait::async_trait;

/// A bare-UDS request/response transport: bare UDS in, bare UDS out.
///
/// This is the `client`-free seam a binary implements over `klartext-client`'s
/// `Session::request(target, uds)`: transmit the raw UDS `uds` bytes to ECU
/// address `target` and return the raw response payload, with no BMW-FAST
/// framing at this layer. [`TelegramExchange`] wraps an implementor to bridge it
/// onto the framed [`UdsExchange`] the VM drives; the offline unit tests supply a
/// mock. Holding the trait here — rather than depending on `klartext-client` —
/// is what keeps `klartext-best` free of that dependency.
///
/// The trait is `dyn`-compatible via [`macro@async_trait`], matching
/// [`UdsExchange`]'s style, so a binary can hold an implementor behind a
/// reference.
#[async_trait]
pub trait BareUdsTransport {
    /// Transmit bare `uds` to ECU `target` and return the bare response payload.
    ///
    /// # Errors
    /// Returns an [`ExchangeError`] — typically [`ExchangeError::Transport`] —
    /// when the underlying transport cannot complete the exchange.
    async fn call(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError>;
}

/// A [`UdsExchange`] that reframes VM telegrams onto a [`BareUdsTransport`].
///
/// Wraps a bare transport `T` and performs the telegram↔bare-UDS translation
/// described in the module documentation: decode the VM's request telegram,
/// forward the bare `(target, uds)` to the inner transport, and re-encode the
/// bare response as the reply telegram the VM parses. The one wrapped field is
/// the inner transport, so `Debug` is derived and present whenever `T: Debug`.
#[derive(Debug)]
pub struct TelegramExchange<T: BareUdsTransport> {
    /// The wrapped bare-UDS transport reframed requests are forwarded to.
    inner: T,
}

impl<T: BareUdsTransport> TelegramExchange<T> {
    /// Wraps `inner` so its bare-UDS transport drives the VM's framed exchange.
    pub fn new(inner: T) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl<T: BareUdsTransport + Sync> UdsExchange for TelegramExchange<T> {
    /// Reframes one VM telegram exchange onto the inner bare-UDS transport.
    ///
    /// Decodes the outgoing telegram, checks its embedded destination equals
    /// `target`, forwards the bare `(target, uds)` to [`BareUdsTransport::call`],
    /// and re-encodes the bare response as the `[fmt][0xF1][target][resp][cksum]`
    /// reply telegram the VM expects.
    ///
    /// # Errors
    /// Returns [`ExchangeError::Unexpected`] (carrying the offending frame) when
    /// the outgoing telegram fails to [`decode`](crate::decode) or its embedded
    /// destination disagrees with `target`; propagates any [`ExchangeError`] the
    /// inner [`BareUdsTransport::call`] returns (e.g. [`ExchangeError::Transport`]).
    async fn request(&self, target: u8, frame: &[u8]) -> Result<Vec<u8>, ExchangeError> {
        // Decode the VM's outgoing request telegram back to bare UDS. A malformed
        // frame is carried out via `Unexpected` — the "offending bytes" variant —
        // rather than degrading to a silent empty response.
        let decoded =
            telegram::decode(frame).map_err(|_| ExchangeError::Unexpected(frame.to_vec()))?;
        // The run loop's `target` is authoritative: a telegram addressed to a
        // different ECU is a hard error, never forwarded to the wrong address.
        if decoded.target != target {
            return Err(ExchangeError::Unexpected(frame.to_vec()));
        }
        let response = self.inner.call(target, &decoded.uds).await?;
        // Re-frame as the ECU→tester reply: tester `0xF1` in the destination byte,
        // the ECU `target` in the source byte — the mirror of the request, which
        // the job's `resp[1]==0xF1` / `resp[2]==ecu` checks accept (differential.rs).
        Ok(telegram::encode(0xF1, target, &response))
    }
}

#[cfg(test)]
mod tests {
    use super::{BareUdsTransport, TelegramExchange};
    use crate::exchange::{ExchangeError, UdsExchange};

    /// A bare-transport double asserting the exact `(target, uds)` it is handed.
    struct MockBare {
        expect_target: u8,
        expect_uds: Vec<u8>,
        respond: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl BareUdsTransport for MockBare {
        async fn call(&self, target: u8, uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
            assert_eq!(target, self.expect_target);
            assert_eq!(uds, &self.expect_uds[..]);
            Ok(self.respond.clone())
        }
    }

    #[tokio::test]
    async fn bridge_translates_telegram_to_bare_and_back() {
        // The VM hands a request telegram; the bridge must strip framing, call the
        // bare transport with (target, uds), and re-frame the bare response.
        let bare = MockBare {
            expect_target: 0x12,
            expect_uds: vec![0x22, 0x45, 0x17],
            respond: vec![0x62, 0x45, 0x17, 0x0A, 0xBC],
        };
        let ex = TelegramExchange::new(bare);
        let request = crate::encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
        let response = ex.request(0x12, &request).await.unwrap();
        // The reply is the ECU→tester telegram: destination = tester 0xF1, source
        // = the ECU 0x12 (the frozen `resp[1]==0xF1` / `resp[2]==ecu` contract,
        // crates/best/tests/differential.rs), carrying the bare response bytes.
        let t = crate::decode(&response).unwrap();
        assert_eq!(t.target, 0xF1);
        assert_eq!(t.source, 0x12);
        assert_eq!(t.uds, vec![0x62, 0x45, 0x17, 0x0A, 0xBC]);
    }

    #[tokio::test]
    async fn bridge_rejects_a_target_mismatch() {
        // A telegram addressed to 0x12 but run_job called with target 0x40 is a
        // hard error — the run loop's target is authoritative, so the mismatch is
        // rejected before the inner transport is ever called.
        let bare = MockBare {
            expect_target: 0x12,
            expect_uds: vec![],
            respond: vec![],
        };
        let ex = TelegramExchange::new(bare);
        let request = crate::encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
        assert!(ex.request(0x40, &request).await.is_err());
    }

    #[tokio::test]
    async fn bridge_surfaces_a_transport_error() {
        // A failure from the inner bare transport propagates unchanged.
        struct Failing;
        #[async_trait::async_trait]
        impl BareUdsTransport for Failing {
            async fn call(&self, _target: u8, _uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
                Err(ExchangeError::Transport("no response".into()))
            }
        }
        let ex = TelegramExchange::new(Failing);
        let request = crate::encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
        assert!(matches!(
            ex.request(0x12, &request).await,
            Err(ExchangeError::Transport(_))
        ));
    }
}
