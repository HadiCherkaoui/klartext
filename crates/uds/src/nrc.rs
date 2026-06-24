//! Typed UDS negative-response codes (NRC), from `docs/protocol-reference.md` §1.2.
//!
//! A UDS negative response is `7F <rejected-sid> <nrc>`. [`Nrc`] turns that raw
//! `<nrc>` byte into a named, matchable value so callers handle failures by
//! meaning ("security access denied", "request out of range") rather than by
//! magic number. Every one of the 256 possible bytes maps to a variant: the ISO
//! 14229-1 table is named exhaustively, the secured-data range (0x38–0x4F) and
//! the manufacturer range (0xF0–0xFF) keep their raw byte, and everything else
//! is [`Nrc::Reserved`]. [`Nrc::from`] and [`Nrc::code`] round-trip for all bytes.
//!
//! The raw parse ([`crate::parse`]) deliberately keeps the byte un-typed; lift it
//! to an `Nrc` at the service boundary via [`crate::UdsResponse::negative_nrc`].

use thiserror::Error;

/// A UDS negative-response code, per ISO 14229-1 (report §1.2).
///
/// Implements [`std::error::Error`] and [`std::fmt::Display`] (the ISO name plus
/// the hex code) so it composes directly into higher-level error types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum Nrc {
    #[error("generalReject (0x10)")]
    GeneralReject,
    #[error("serviceNotSupported (0x11)")]
    ServiceNotSupported,
    #[error("subFunctionNotSupported (0x12)")]
    SubFunctionNotSupported,
    #[error("incorrectMessageLengthOrInvalidFormat (0x13)")]
    IncorrectMessageLengthOrInvalidFormat,
    #[error("responseTooLong (0x14)")]
    ResponseTooLong,
    #[error("busyRepeatRequest (0x21)")]
    BusyRepeatRequest,
    #[error("conditionsNotCorrect (0x22)")]
    ConditionsNotCorrect,
    #[error("requestSequenceError (0x24)")]
    RequestSequenceError,
    #[error("noResponseFromSubnetComponent (0x25)")]
    NoResponseFromSubnetComponent,
    #[error("failurePreventsExecutionOfRequestedAction (0x26)")]
    FailurePreventsExecutionOfRequestedAction,
    #[error("requestOutOfRange (0x31)")]
    RequestOutOfRange,
    #[error("securityAccessDenied (0x33)")]
    SecurityAccessDenied,
    #[error("authenticationRequired (0x34)")]
    AuthenticationRequired,
    #[error("invalidKey (0x35)")]
    InvalidKey,
    #[error("exceededNumberOfAttempts (0x36)")]
    ExceededNumberOfAttempts,
    #[error("requiredTimeDelayNotExpired (0x37)")]
    RequiredTimeDelayNotExpired,
    #[error("uploadDownloadNotAccepted (0x70)")]
    UploadDownloadNotAccepted,
    #[error("transferDataSuspended (0x71)")]
    TransferDataSuspended,
    #[error("generalProgrammingFailure (0x72)")]
    GeneralProgrammingFailure,
    #[error("wrongBlockSequenceCounter (0x73)")]
    WrongBlockSequenceCounter,
    /// 0x78: received OK, ECU still working — the tester must keep waiting.
    #[error("requestCorrectlyReceived-ResponsePending (0x78)")]
    RequestCorrectlyReceivedResponsePending,
    #[error("subFunctionNotSupportedInActiveSession (0x7E)")]
    SubFunctionNotSupportedInActiveSession,
    #[error("serviceNotSupportedInActiveSession (0x7F)")]
    ServiceNotSupportedInActiveSession,
    #[error("rpmTooHigh (0x81)")]
    RpmTooHigh,
    #[error("rpmTooLow (0x82)")]
    RpmTooLow,
    #[error("engineIsRunning (0x83)")]
    EngineIsRunning,
    #[error("engineIsNotRunning (0x84)")]
    EngineIsNotRunning,
    #[error("engineRunTimeTooLow (0x85)")]
    EngineRunTimeTooLow,
    #[error("temperatureTooHigh (0x86)")]
    TemperatureTooHigh,
    #[error("temperatureTooLow (0x87)")]
    TemperatureTooLow,
    #[error("vehicleSpeedTooHigh (0x88)")]
    VehicleSpeedTooHigh,
    #[error("vehicleSpeedTooLow (0x89)")]
    VehicleSpeedTooLow,
    #[error("throttle/pedalTooHigh (0x8A)")]
    ThrottlePedalTooHigh,
    #[error("throttle/pedalTooLow (0x8B)")]
    ThrottlePedalTooLow,
    #[error("transmissionRangeNotInNeutral (0x8C)")]
    TransmissionRangeNotInNeutral,
    #[error("transmissionRangeNotInGear (0x8D)")]
    TransmissionRangeNotInGear,
    #[error("brakeSwitch(es)NotClosed (0x8F)")]
    BrakeSwitchesNotClosed,
    #[error("shifterLeverNotInPark (0x90)")]
    ShifterLeverNotInPark,
    #[error("torqueConverterClutchLocked (0x91)")]
    TorqueConverterClutchLocked,
    #[error("voltageTooHigh (0x92)")]
    VoltageTooHigh,
    #[error("voltageTooLow (0x93)")]
    VoltageTooLow,
    /// 0x38–0x4F: reserved for secured data transmission (ISO 15764).
    #[error("securedDataTransmissionReserved (0x{0:02X})")]
    SecuredDataReserved(u8),
    /// 0xF0–0xFF: vehicle-manufacturer specific. BMW may define custom codes
    /// here — meanings are [verify against capture].
    #[error("manufacturerSpecific (0x{0:02X})")]
    Manufacturer(u8),
    /// Any byte ISO leaves reserved (0x00–0x0F, 0x94–0xEF, and the gaps).
    #[error("reserved (0x{0:02X})")]
    Reserved(u8),
}

impl Nrc {
    /// The single-byte code for this NRC; the inverse of [`Nrc::from`].
    pub fn code(self) -> u8 {
        match self {
            Self::GeneralReject => 0x10,
            Self::ServiceNotSupported => 0x11,
            Self::SubFunctionNotSupported => 0x12,
            Self::IncorrectMessageLengthOrInvalidFormat => 0x13,
            Self::ResponseTooLong => 0x14,
            Self::BusyRepeatRequest => 0x21,
            Self::ConditionsNotCorrect => 0x22,
            Self::RequestSequenceError => 0x24,
            Self::NoResponseFromSubnetComponent => 0x25,
            Self::FailurePreventsExecutionOfRequestedAction => 0x26,
            Self::RequestOutOfRange => 0x31,
            Self::SecurityAccessDenied => 0x33,
            Self::AuthenticationRequired => 0x34,
            Self::InvalidKey => 0x35,
            Self::ExceededNumberOfAttempts => 0x36,
            Self::RequiredTimeDelayNotExpired => 0x37,
            Self::UploadDownloadNotAccepted => 0x70,
            Self::TransferDataSuspended => 0x71,
            Self::GeneralProgrammingFailure => 0x72,
            Self::WrongBlockSequenceCounter => 0x73,
            Self::RequestCorrectlyReceivedResponsePending => 0x78,
            Self::SubFunctionNotSupportedInActiveSession => 0x7E,
            Self::ServiceNotSupportedInActiveSession => 0x7F,
            Self::RpmTooHigh => 0x81,
            Self::RpmTooLow => 0x82,
            Self::EngineIsRunning => 0x83,
            Self::EngineIsNotRunning => 0x84,
            Self::EngineRunTimeTooLow => 0x85,
            Self::TemperatureTooHigh => 0x86,
            Self::TemperatureTooLow => 0x87,
            Self::VehicleSpeedTooHigh => 0x88,
            Self::VehicleSpeedTooLow => 0x89,
            Self::ThrottlePedalTooHigh => 0x8A,
            Self::ThrottlePedalTooLow => 0x8B,
            Self::TransmissionRangeNotInNeutral => 0x8C,
            Self::TransmissionRangeNotInGear => 0x8D,
            Self::BrakeSwitchesNotClosed => 0x8F,
            Self::ShifterLeverNotInPark => 0x90,
            Self::TorqueConverterClutchLocked => 0x91,
            Self::VoltageTooHigh => 0x92,
            Self::VoltageTooLow => 0x93,
            Self::SecuredDataReserved(b) | Self::Manufacturer(b) | Self::Reserved(b) => b,
        }
    }
}

impl From<u8> for Nrc {
    fn from(byte: u8) -> Self {
        match byte {
            0x10 => Self::GeneralReject,
            0x11 => Self::ServiceNotSupported,
            0x12 => Self::SubFunctionNotSupported,
            0x13 => Self::IncorrectMessageLengthOrInvalidFormat,
            0x14 => Self::ResponseTooLong,
            0x21 => Self::BusyRepeatRequest,
            0x22 => Self::ConditionsNotCorrect,
            0x24 => Self::RequestSequenceError,
            0x25 => Self::NoResponseFromSubnetComponent,
            0x26 => Self::FailurePreventsExecutionOfRequestedAction,
            0x31 => Self::RequestOutOfRange,
            0x33 => Self::SecurityAccessDenied,
            0x34 => Self::AuthenticationRequired,
            0x35 => Self::InvalidKey,
            0x36 => Self::ExceededNumberOfAttempts,
            0x37 => Self::RequiredTimeDelayNotExpired,
            0x70 => Self::UploadDownloadNotAccepted,
            0x71 => Self::TransferDataSuspended,
            0x72 => Self::GeneralProgrammingFailure,
            0x73 => Self::WrongBlockSequenceCounter,
            0x78 => Self::RequestCorrectlyReceivedResponsePending,
            0x7E => Self::SubFunctionNotSupportedInActiveSession,
            0x7F => Self::ServiceNotSupportedInActiveSession,
            0x81 => Self::RpmTooHigh,
            0x82 => Self::RpmTooLow,
            0x83 => Self::EngineIsRunning,
            0x84 => Self::EngineIsNotRunning,
            0x85 => Self::EngineRunTimeTooLow,
            0x86 => Self::TemperatureTooHigh,
            0x87 => Self::TemperatureTooLow,
            0x88 => Self::VehicleSpeedTooHigh,
            0x89 => Self::VehicleSpeedTooLow,
            0x8A => Self::ThrottlePedalTooHigh,
            0x8B => Self::ThrottlePedalTooLow,
            0x8C => Self::TransmissionRangeNotInNeutral,
            0x8D => Self::TransmissionRangeNotInGear,
            0x8F => Self::BrakeSwitchesNotClosed,
            0x90 => Self::ShifterLeverNotInPark,
            0x91 => Self::TorqueConverterClutchLocked,
            0x92 => Self::VoltageTooHigh,
            0x93 => Self::VoltageTooLow,
            // Ranges come after the named codes; none of the named codes fall in
            // these spans, so first-match ordering is correct.
            0x38..=0x4F => Self::SecuredDataReserved(byte),
            0xF0..=0xFF => Self::Manufacturer(byte),
            _ => Self::Reserved(byte),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_codes_map_from_the_report() {
        assert_eq!(Nrc::from(0x31), Nrc::RequestOutOfRange);
        assert_eq!(Nrc::from(0x33), Nrc::SecurityAccessDenied);
        assert_eq!(
            Nrc::from(0x78),
            Nrc::RequestCorrectlyReceivedResponsePending
        );
        assert_eq!(Nrc::from(0x7F), Nrc::ServiceNotSupportedInActiveSession);
    }

    #[test]
    fn ranges_keep_their_raw_byte() {
        assert_eq!(Nrc::from(0x42), Nrc::SecuredDataReserved(0x42));
        assert_eq!(Nrc::from(0xF1), Nrc::Manufacturer(0xF1));
        assert_eq!(Nrc::from(0x05), Nrc::Reserved(0x05));
        assert_eq!(Nrc::from(0x80), Nrc::Reserved(0x80));
    }

    #[test]
    fn from_and_code_round_trip_for_every_byte() {
        for byte in 0u8..=0xFF {
            assert_eq!(
                Nrc::from(byte).code(),
                byte,
                "round-trip failed at 0x{byte:02X}"
            );
        }
    }

    #[test]
    fn display_includes_name_and_hex() {
        assert_eq!(
            Nrc::RequestOutOfRange.to_string(),
            "requestOutOfRange (0x31)"
        );
        assert_eq!(
            Nrc::Manufacturer(0xF1).to_string(),
            "manufacturerSpecific (0xF1)"
        );
    }
}
