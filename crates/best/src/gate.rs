//! The SID gate: refuse a frame the active policy forbids before it reaches the car.
//!
//! [`GatedExchange`] wraps any [`UdsExchange`] and inspects the UDS service ID
//! embedded in each outgoing BMW-FAST telegram *at the transmit boundary*, before
//! the telegram is translated to bare UDS. It is the OUTERMOST layer of the live
//! stack — `GatedExchange::read_only(...)` or `GatedExchange::confirmed_write(...)`
//! wrapping a `TelegramExchange::new(session)` — so it sees the frame the VM built
//! and can veto it. This is the single seam that makes the whole guided surface
//! incapable of transmitting anything its active [`Policy`] forbids:
//! [`Policy::ReadOnly`] admits only reads and session plumbing;
//! [`Policy::ConfirmedWrite`] additionally admits the gated writes/actuation
//! services; flashing is refused under both, forever.
//!
//! ## The classes (spec §6)
//! [`classify`] sorts a service ID into three [`SidClass`]es per §6 of the design
//! spec (`docs/superpowers/specs/2026-07-06-item5-guided-service-procedures-design.md`):
//! - **[`SidClass::Pass`]** — session plumbing `0x10`/`0x3E` and the reads
//!   `0x22`/`0x2C`/`0x19`. These delegate straight to the inner exchange.
//! - **[`SidClass::Gated`]** — the writes/actuation/security services `0x2E`,
//!   `0x31`, `0x2F`, `0x14`, `0x27`. Refused under [`Policy::ReadOnly`].
//! - **[`SidClass::RefuseAlways`]** — flashing `0x34..=0x37`, refused under EVERY
//!   policy, forever.
//!
//! ## Fail closed
//! An unlisted service ID classifies as [`SidClass::Gated`], never
//! [`SidClass::Pass`]: an unknown service is treated as a write and refused, never
//! silently sent. A telegram too short to carry a service ID — where
//! [`peek_sid`](crate::peek_sid) returns `None` — is a hard
//! [`ExchangeError::Unexpected`], because the gate never guesses at an
//! unclassifiable frame.
//!
//! Both policies ship as of P3: [`Policy::ReadOnly`] (the default for every read
//! path) and [`Policy::ConfirmedWrite`], selected only by a caller holding the
//! human's `confirm`. Flashing is [`SidClass::RefuseAlways`] under both.

use crate::exchange::{ExchangeError, UdsExchange};
use crate::telegram;
use async_trait::async_trait;

/// The gate's transmit policy — the safety posture applied to each frame.
///
/// [`Policy::ReadOnly`] is the default for every read path (`run_job`, measurement
/// reads); [`Policy::ConfirmedWrite`] is selected explicitly by a caller that has a
/// human's `confirm` in hand. Neither policy can open flashing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Policy {
    /// Refuse every [`SidClass::Gated`] and [`SidClass::RefuseAlways`] service;
    /// pass only reads and session plumbing.
    ReadOnly,
    /// Pass reads AND [`SidClass::Gated`] writes/actuation; still refuse
    /// [`SidClass::RefuseAlways`] flashing, forever.
    ConfirmedWrite,
}

/// How [`classify`] sorts a UDS service ID for the read-only gate (spec §6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SidClass {
    /// A read or session-plumbing service that passes the gate untouched.
    Pass,
    /// A write/actuation/security service, refused under [`Policy::ReadOnly`].
    Gated,
    /// A flashing service, refused under EVERY policy, forever.
    RefuseAlways,
}

/// Classifies a UDS service ID into its [`SidClass`] per the spec §6 table.
///
/// The design spec's §6
/// (`docs/superpowers/specs/2026-07-06-item5-guided-service-procedures-design.md`)
/// fixes the three classes:
/// - **[`SidClass::Pass`]**: session `0x10`/`0x3E` and reads `0x22`/`0x2C`/`0x19`.
/// - **[`SidClass::Gated`]**: writes/actuation/security `0x2E`, `0x31`, `0x2F`,
///   `0x14`, `0x27`.
/// - **[`SidClass::RefuseAlways`]**: flashing `0x34..=0x37`.
///
/// Any service ID outside those lists classifies as [`SidClass::Gated`] — failing
/// closed, because an unrecognized service is never assumed to be a safe read.
///
/// # Examples
/// ```
/// use klartext_best::{classify, SidClass};
/// assert!(matches!(classify(0x22), SidClass::Pass)); // readDataByIdentifier
/// assert!(matches!(classify(0x2E), SidClass::Gated)); // writeDataByIdentifier
/// assert!(matches!(classify(0x34), SidClass::RefuseAlways)); // requestDownload
/// assert!(matches!(classify(0x99), SidClass::Gated)); // unknown → fail closed
/// ```
pub fn classify(sid: u8) -> SidClass {
    match sid {
        // Reads + session plumbing pass untouched (spec §6).
        0x10 | 0x3E | 0x22 | 0x2C | 0x19 => SidClass::Pass,
        // Flashing is refused under EVERY policy, forever (spec §6).
        0x34..=0x37 => SidClass::RefuseAlways,
        // Gated: the writes/actuation/security the spec names — 0x2E, 0x31, 0x2F,
        // 0x14, 0x27, 0x11 — AND, failing closed, any unlisted SID: an unknown
        // service is never a read (spec §6). 0x11 (ECUReset) is named explicitly
        // because klartext now transmits it after a confirmed clear: it must stay
        // Gated, so `run_job` and every other read path still refuse it.
        _ => SidClass::Gated,
    }
}

/// A [`UdsExchange`] that vetoes frames its [`Policy`] forbids before they reach the car.
///
/// Wraps an inner exchange `E` and, on each request, peeks the outgoing telegram's
/// embedded UDS service ID and [`classify`]s it against the gate's [`Policy`]. Under
/// [`Policy::ReadOnly`] a [`SidClass::Pass`] service delegates the whole telegram to
/// the inner exchange, while [`SidClass::Gated`] and [`SidClass::RefuseAlways`] are
/// refused at the seam; under [`Policy::ConfirmedWrite`] a [`SidClass::Gated`]
/// service delegates too, and only [`SidClass::RefuseAlways`] (flashing) is still
/// refused. This is the outermost layer of the live stack, so it inspects the frame
/// the VM built before any translation, whichever policy it was constructed with.
///
/// The single wrapped field is the inner exchange, so `Debug` is derived and
/// present whenever `E: Debug`.
#[derive(Debug)]
pub struct GatedExchange<E: UdsExchange> {
    /// The wrapped exchange a passed frame is delegated to.
    inner: E,
    /// The safety policy applied to every outgoing frame's service ID.
    policy: Policy,
}

impl<E: UdsExchange> GatedExchange<E> {
    /// Wraps `inner` with the read-only policy: reads pass, everything else refuses.
    pub fn read_only(inner: E) -> Self {
        Self {
            inner,
            policy: Policy::ReadOnly,
        }
    }

    /// Wraps `inner` with the confirmed-write policy: reads and writes pass,
    /// flashing is still refused.
    ///
    /// Select this ONLY on a call carrying the human's `confirm` (spec §2 D3).
    pub fn confirmed_write(inner: E) -> Self {
        Self {
            inner,
            policy: Policy::ConfirmedWrite,
        }
    }
}

#[async_trait]
impl<E: UdsExchange + Sync> UdsExchange for GatedExchange<E> {
    /// Classifies the telegram's service ID and either delegates or refuses.
    ///
    /// Peeks the outgoing telegram's UDS service ID with
    /// [`peek_sid`](crate::peek_sid); a [`SidClass::Pass`] service always delegates
    /// the whole `frame` to the inner exchange, a [`SidClass::Gated`] service
    /// delegates only under [`Policy::ConfirmedWrite`], and
    /// [`SidClass::RefuseAlways`] is refused before the inner exchange is touched
    /// under EVERY policy.
    ///
    /// # Errors
    /// Returns [`ExchangeError::Refused`] (carrying the gated service ID and the
    /// full frame) when the policy forbids the service; returns
    /// [`ExchangeError::Unexpected`] when the frame is too short to carry a service
    /// ID (no silent degrade); otherwise propagates whatever the inner exchange
    /// returns.
    async fn request(&self, target: u8, frame: &[u8]) -> Result<Vec<u8>, ExchangeError> {
        // No-degrade: a frame with no UDS service byte is a hard error carrying the
        // offending bytes, never silently forwarded to the inner transport.
        let Some(sid) = telegram::peek_sid(frame) else {
            return Err(ExchangeError::Unexpected(frame.to_vec()));
        };
        match (self.policy, classify(sid)) {
            // Flashing: refused under EVERY policy, present and future. Matched
            // before the policy is even inspected, so a new `Policy` variant added
            // later cannot open it by omission — the arms below name specific
            // policies and would leave that variant's Pass/Gated combinations
            // unmatched (a compile error) rather than silently falling through here.
            (_, SidClass::RefuseAlways) => Err(ExchangeError::Refused {
                sid,
                frame: frame.to_vec(),
            }),
            // A read or session-plumbing service passes under either policy.
            (Policy::ReadOnly | Policy::ConfirmedWrite, SidClass::Pass) => {
                self.inner.request(target, frame).await
            }
            // ConfirmedWrite admits the write/actuation services ReadOnly refuses.
            (Policy::ConfirmedWrite, SidClass::Gated) => self.inner.request(target, frame).await,
            // Refused: every write under ReadOnly, before the inner exchange (and
            // thus the car) is ever touched.
            (Policy::ReadOnly, SidClass::Gated) => Err(ExchangeError::Refused {
                sid,
                frame: frame.to_vec(),
            }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{GatedExchange, SidClass, classify};
    use crate::exchange::{ExchangeError, UdsExchange};

    /// A [`UdsExchange`] double that records the last frame it received.
    ///
    /// Lets a test assert whether a frame reached the inner transport — the crux
    /// of the refusal tests, which must prove NO write frame ever got through.
    #[derive(Default)]
    struct RecordingExchange {
        last: std::sync::Mutex<Option<Vec<u8>>>,
    }

    impl RecordingExchange {
        fn last(&self) -> Option<Vec<u8>> {
            self.last.lock().expect("recording mutex poisoned").clone()
        }
    }

    #[async_trait::async_trait]
    impl UdsExchange for RecordingExchange {
        async fn request(&self, _target: u8, frame: &[u8]) -> Result<Vec<u8>, ExchangeError> {
            *self.last.lock().expect("recording mutex poisoned") = Some(frame.to_vec());
            // A canned ECU→tester positive reply; the pass-through test checks only
            // that the call reached the inner, not the reply's contents.
            Ok(crate::encode(0xF1, 0x12, &[0x62, 0x45, 0x17]))
        }
    }

    /// Reaches through the gate to the recording inner's last frame (test-only).
    impl GatedExchange<RecordingExchange> {
        fn inner_last(&self) -> Option<Vec<u8>> {
            self.inner.last()
        }
    }

    #[test]
    fn classify_covers_the_spec_6_classes() {
        for sid in [0x10, 0x3E, 0x22, 0x2C, 0x19] {
            assert!(
                matches!(classify(sid), SidClass::Pass),
                "0x{sid:02X} should pass"
            );
        }
        // 0x11 (ECUReset) is in this list deliberately: klartext transmits it
        // after a confirmed clear, so a future change moving it to `Pass` —
        // reasoning "it is part of the clear flow" — must fail here.
        for sid in [0x2E, 0x31, 0x2F, 0x14, 0x27, 0x11] {
            assert!(
                matches!(classify(sid), SidClass::Gated),
                "0x{sid:02X} should be gated"
            );
        }
        for sid in [0x34, 0x35, 0x36, 0x37] {
            assert!(
                matches!(classify(sid), SidClass::RefuseAlways),
                "0x{sid:02X} refuse"
            );
        }
    }

    #[tokio::test]
    async fn read_only_passes_a_22_read() {
        // A 0x22 read telegram must reach the inner transport unchanged.
        let inner = RecordingExchange::default();
        let gate = GatedExchange::read_only(inner);
        let frame = crate::encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
        gate.request(0x12, &frame).await.unwrap();
        assert_eq!(gate.inner_last(), Some(frame));
    }

    #[tokio::test]
    async fn read_only_refuses_a_2e_write_at_the_seam() {
        // A 0x2E writeDataByIdentifier is refused, and — critically — the write
        // frame never reaches the inner transport.
        let gate = GatedExchange::read_only(RecordingExchange::default());
        let frame = crate::encode(0x12, 0xF1, &[0x2E, 0x10, 0x01, 0xFF]);
        match gate.request(0x12, &frame).await {
            Err(ExchangeError::Refused { sid, .. }) => assert_eq!(sid, 0x2E),
            other => panic!("expected Refused, got {other:?}"),
        }
        // No write frame reached the inner transport.
        assert_eq!(gate.inner_last(), None);
    }

    #[tokio::test]
    async fn read_only_refuses_flashing_services() {
        // 0x34 requestDownload (flashing) is refused under ReadOnly.
        let gate = GatedExchange::read_only(RecordingExchange::default());
        let frame = crate::encode(0x12, 0xF1, &[0x34, 0x00]);
        assert!(matches!(
            gate.request(0x12, &frame).await,
            Err(ExchangeError::Refused { .. })
        ));
    }

    #[tokio::test]
    async fn unparseable_frame_is_unexpected_not_passed() {
        // No-degrade: a frame too short to hold a UDS service byte (peek_sid →
        // None) is a hard Unexpected, never silently forwarded to the inner.
        let gate = GatedExchange::read_only(RecordingExchange::default());
        let frame = vec![0x80, 0x12];
        assert!(matches!(
            gate.request(0x12, &frame).await,
            Err(ExchangeError::Unexpected(_))
        ));
        assert_eq!(gate.inner_last(), None);
    }

    #[tokio::test]
    async fn read_only_refuses_a_long_form_write_whose_length_byte_mimics_a_read_sid() {
        // The bypass regression (Task 8 review): an 8-bit LONG-form telegram
        // `[0x80][tgt][src][len][SID…]` carries its SID at index 4, but a
        // short-form-only peek read the LENGTH byte at index 3. With len = 0x22 —
        // a 34-byte payload whose length byte happens to spell readDataByIdentifier
        // — a 0x2E WRITE classified as a read and passed the gate, and the
        // checksum-lenient request decode then forwarded the write onward. The
        // form-aware peek must classify by the true SID: Refused{0x2E}, and
        // NOTHING reaches the inner.
        let gate = GatedExchange::read_only(RecordingExchange::default());
        let mut frame = vec![0x80, 0x12, 0xF1, 0x22, 0x2E, 0x10, 0x01];
        // Complete the declared 34-byte payload (VM-style frame, no checksum).
        frame.resize(4 + 0x22, 0x00);
        match gate.request(0x12, &frame).await {
            Err(ExchangeError::Refused { sid, .. }) => assert_eq!(sid, 0x2E),
            other => panic!("expected Refused, got {other:?}"),
        }
        assert_eq!(gate.inner_last(), None);
    }

    #[tokio::test]
    async fn read_only_passes_a_long_form_read() {
        // Form-awareness must not break a legitimate LONG-form READ: the true SID
        // (0x22 at index 4) passes to the inner — even though the length byte at
        // index 3 (0x05, not a read SID) would have wrongly REFUSED it under the
        // old short-form-only peek.
        let inner = RecordingExchange::default();
        let gate = GatedExchange::read_only(inner);
        let frame = vec![0x80, 0x12, 0xF1, 0x05, 0x22, 0x45, 0x17, 0x00, 0x00];
        gate.request(0x12, &frame).await.unwrap();
        assert_eq!(gate.inner_last(), Some(frame));
    }

    #[tokio::test]
    async fn confirmed_write_passes_a_gated_write_to_the_inner() {
        // The whole point of the new policy: a 0x2E write that ReadOnly refuses
        // must reach the inner transport under ConfirmedWrite.
        let gate = GatedExchange::confirmed_write(RecordingExchange::default());
        let frame = crate::encode(0x12, 0xF1, &[0x2E, 0x10, 0x01, 0xFF]);
        gate.request(0x12, &frame).await.unwrap();
        assert_eq!(gate.inner_last(), Some(frame));
    }

    #[tokio::test]
    async fn confirmed_write_still_passes_reads() {
        let gate = GatedExchange::confirmed_write(RecordingExchange::default());
        let frame = crate::encode(0x12, 0xF1, &[0x22, 0x45, 0x17]);
        gate.request(0x12, &frame).await.unwrap();
        assert_eq!(gate.inner_last(), Some(frame));
    }

    #[tokio::test]
    async fn confirmed_write_never_passes_flashing() {
        // RefuseAlways means ALWAYS — the write policy must not open flashing.
        let gate = GatedExchange::confirmed_write(RecordingExchange::default());
        for sid in [0x34u8, 0x35, 0x36, 0x37] {
            let frame = crate::encode(0x12, 0xF1, &[sid, 0x00]);
            match gate.request(0x12, &frame).await {
                Err(ExchangeError::Refused { sid: got, .. }) => assert_eq!(got, sid),
                other => panic!("expected Refused for 0x{sid:02X}, got {other:?}"),
            }
        }
        assert_eq!(gate.inner_last(), None);
    }

    #[tokio::test]
    async fn confirmed_write_rejects_an_unparseable_frame() {
        // No-degrade applies under every policy.
        let gate = GatedExchange::confirmed_write(RecordingExchange::default());
        let frame = vec![0x80, 0x12];
        assert!(matches!(
            gate.request(0x12, &frame).await,
            Err(ExchangeError::Unexpected(_))
        ));
        assert_eq!(gate.inner_last(), None);
    }
}
