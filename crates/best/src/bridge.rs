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
//! 1. **decodes** the VM's outgoing request telegram with
//!    [`decode_request`](crate::telegram::decode_request), recovering the bare UDS
//!    payload and the telegram's embedded destination. The VM's `xsend` emits the
//!    checksum-LESS short frame (in EDIABAS the interface appends the additive
//!    checksum, not the job bytecode), so the request decode is checksum-lenient —
//!    unlike the strict [`crate::decode`] used for a fully-framed reply;
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
//! read-only SID gate and the live session sit on either side of it, so `mcp` —
//! and a future mobile-app core — can reuse this one translation. Keeping the
//! seam a bare trait is also what keeps `klartext-best` free of a
//! `klartext-client` dependency.

use crate::exchange::{ExchangeError, UdsExchange};
use crate::telegram;
use async_trait::async_trait;
use klartext_uds::FUNCTIONAL_ADDRESS_F01;

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

    /// Transmit bare `uds` FUNCTIONALLY and return every responder's
    /// `(source address, bare response)`, in arrival order.
    ///
    /// A functional request is answered by many ECUs at once, so it cannot use
    /// [`BareUdsTransport::call`]'s one-response shape. `f01.prg`'s
    /// `IDENT_FUNKTIONAL` — ISTA's BN2000 ECU discovery — needs exactly this: it
    /// broadcasts `22 F1 50` and walks the concatenated answers.
    ///
    /// The default refuses, so a transport that cannot broadcast says so loudly
    /// instead of silently answering as though nobody replied.
    ///
    /// # Errors
    /// As [`BareUdsTransport::call`]; the default returns
    /// [`ExchangeError::Transport`].
    async fn call_functional(
        &self,
        _target: u8,
        _uds: &[u8],
    ) -> Result<Vec<(u8, Vec<u8>)>, ExchangeError> {
        Err(ExchangeError::Transport(
            "this transport cannot address functionally".to_string(),
        ))
    }
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

    /// The wrapped transport, for tests that assert on what it was handed.
    #[cfg(test)]
    fn inner_for_test(&self) -> &T {
        &self.inner
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
    /// the outgoing telegram fails the crate-internal checksum-lenient request
    /// decode (`telegram::decode_request`) or its embedded destination disagrees
    /// with `target`; propagates any [`ExchangeError`] the inner
    /// [`BareUdsTransport::call`] returns (e.g. [`ExchangeError::Transport`]).
    async fn request(&self, target: u8, frame: &[u8]) -> Result<Vec<u8>, ExchangeError> {
        // Decode the VM's outgoing request telegram back to bare UDS. The VM's
        // `xsend` emits the checksum-LESS short frame `[0x80|len][tgt][src][uds…]`
        // (the interface, not the job bytecode, appends the additive checksum), so
        // this uses the checksum-lenient [`telegram::decode_request`], NOT the strict
        // `decode`. A malformed frame is carried out via `Unexpected` — the
        // "offending bytes" variant — rather than degrading to a silent empty response.
        let decoded = telegram::decode_request(frame)
            .map_err(|_| ExchangeError::Unexpected(frame.to_vec()))?;
        // A FUNCTIONAL telegram is the job's own choice of destination, not the run
        // loop's, so the target check below does not apply to it. Every responder's
        // reply is re-framed and CONCATENATED into one buffer, which is what the job
        // then walks: `IDENT_FUNKTIONAL` advances by each telegram's length until it
        // has consumed `slen` bytes (`f01.prg` ops 221-224).
        //
        // **The concatenation format is [verify against capture].** Driven against a
        // three-responder mock, `IDENT_FUNKTIONAL` parses the FIRST telegram (one
        // `OKAY`) and then loses sync (`ERROR_ECU_INCORRECT_LEN`), so the per-entry
        // framing here is not yet the one EDIABAS hands the job. Dropping the
        // trailing checksum was tried and changes nothing, so that is not the
        // difference. Settle it with an on-car capture of the real
        // `IDENT_FUNKTIONAL` exchange rather than by guessing — the bytes are the
        // only authority, and a wrong guess would silently mis-split the ECU list.
        if decoded.target == FUNCTIONAL_ADDRESS_F01 {
            let responders = self
                .inner
                .call_functional(decoded.target, &decoded.uds)
                .await?;
            let mut buffer = Vec::new();
            for (source, response) in responders {
                buffer.extend_from_slice(&telegram::encode(0xF1, source, &response));
            }
            return Ok(buffer);
        }
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
    use klartext_uds::FUNCTIONAL_ADDRESS_F01;

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

    /// A transport that records whether the FUNCTIONAL half was used.
    struct MockFunctional {
        responders: Vec<(u8, Vec<u8>)>,
        seen: std::sync::Mutex<Option<Vec<u8>>>,
    }

    #[async_trait::async_trait]
    impl BareUdsTransport for MockFunctional {
        async fn call(&self, _target: u8, _uds: &[u8]) -> Result<Vec<u8>, ExchangeError> {
            panic!("a functional telegram must NOT take the physical path");
        }
        async fn call_functional(
            &self,
            target: u8,
            uds: &[u8],
        ) -> Result<Vec<(u8, Vec<u8>)>, ExchangeError> {
            assert_eq!(target, FUNCTIONAL_ADDRESS_F01);
            *self.seen.lock().unwrap() = Some(uds.to_vec());
            Ok(self.responders.clone())
        }
    }

    /// A telegram addressed to `0xDF` takes the BROADCAST path, and every
    /// responder's reply comes back in one buffer.
    ///
    /// `IDENT_FUNKTIONAL` addresses `0xDF` itself, so the run loop's `target` is
    /// deliberately something else here — routing must key on the telegram's own
    /// destination, not on the caller's.
    #[tokio::test]
    async fn a_functional_telegram_is_broadcast_and_every_reply_returned() {
        let mock = MockFunctional {
            responders: vec![
                (0x12, vec![0x62, 0xF1, 0x50, 0x0A]),
                (0x78, vec![0x62, 0xF1, 0x50, 0x0B]),
            ],
            seen: std::sync::Mutex::new(None),
        };
        let ex = TelegramExchange::new(mock);
        let request = crate::encode(FUNCTIONAL_ADDRESS_F01, 0xF1, &[0x22, 0xF1, 0x50]);

        let response = ex.request(0x00, &request).await.unwrap();

        // The bare UDS reached the broadcast half unchanged…
        {
            let seen = ex.inner_for_test().seen.lock().unwrap().clone();
            assert_eq!(seen.as_deref(), Some(&[0x22u8, 0xF1, 0x50][..]));
        }
        // …and BOTH replies are present, each framed from its own ECU.
        let first = crate::decode(&response[..]).unwrap();
        assert_eq!(first.source, 0x12);
        assert!(
            response.len() > crate::encode(0xF1, 0x12, &[0x62, 0xF1, 0x50, 0x0A]).len(),
            "the second responder must be concatenated too, not dropped"
        );
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
    async fn bridge_accepts_the_checksum_less_vm_request() {
        // Regression: the VM's real `xsend` output is the checksum-LESS short frame
        // `83 12 F1 22 45 17` (6 bytes — the interface, not the job, appends the
        // additive checksum). The bridge must strip its header to bare UDS and
        // forward it, exactly as it does a fully-framed frame; a strict checksum
        // decode would reject this and break every live job.
        let bare = MockBare {
            expect_target: 0x12,
            expect_uds: vec![0x22, 0x45, 0x17],
            respond: vec![0x62, 0x45, 0x17, 0x0A, 0xBC],
        };
        let ex = TelegramExchange::new(bare);
        // No trailing checksum byte — this is exactly what the VM builds.
        let request = vec![0x83, 0x12, 0xF1, 0x22, 0x45, 0x17];
        let response = ex.request(0x12, &request).await.unwrap();
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
