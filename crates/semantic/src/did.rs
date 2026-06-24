//! Data-identifier (DID) semantics — ISO-standard naming and raw rendering.
//!
//! BMW-specific live-data DIDs are read through EDIABAS jobs whose scaling lives
//! in the SGBD, not the SQLiteDB (see `docs/sqlite-findings.md`), so this module
//! deliberately names only the **ISO-standard identification DIDs** (0xF1xx) plus
//! the BMW IP-config DID from the protocol report, and otherwise returns the raw
//! value. Scaling of arbitrary DIDs is deferred until the SGBD path exists.

/// A decoded data identifier: its name (if standard), raw bytes, and text view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedDid {
    /// The 2-byte DID that was read.
    pub did: u16,
    /// The ISO-standard name, or `None` for an ECU-specific DID.
    pub name: Option<&'static str>,
    /// The raw value bytes, exactly as returned by the ECU.
    pub raw: Vec<u8>,
    /// An ASCII/UTF-8 rendering, present only when the bytes are printable text.
    pub text: Option<String>,
}

/// Decode a DID and its raw value into a [`DecodedDid`].
///
/// Attaches the ISO-standard [`standard_name`] (if any) and renders the bytes as
/// text when they are valid, non-empty, control-free UTF-8 (so VINs and part
/// numbers read naturally while binary values keep `text == None`).
pub fn decode(did: u16, raw: &[u8]) -> DecodedDid {
    let text = std::str::from_utf8(raw)
        .ok()
        .filter(|s| !s.is_empty() && s.chars().all(|c| !c.is_control()))
        .map(str::to_owned);
    DecodedDid {
        did,
        name: standard_name(did),
        raw: raw.to_vec(),
        text,
    }
}

/// The ISO-standard name for an identification DID, if known.
///
/// Covers the standardized 0xF1xx identification range (ISO 14229-1, report
/// §1.5) plus the BMW IP-configuration DID 0x172A the report calls out. Returns
/// `None` for DIDs whose meaning is ECU-specific and not in this static table.
pub fn standard_name(did: u16) -> Option<&'static str> {
    let name = match did {
        0xF180 => "bootSoftwareIdentification",
        0xF181 => "applicationSoftwareIdentification",
        0xF182 => "applicationDataIdentification",
        0xF187 => "vehicleManufacturerSparePartNumber",
        0xF188 => "vehicleManufacturerECUSoftwareNumber",
        0xF189 => "vehicleManufacturerECUSoftwareVersionNumber",
        0xF18A => "systemSupplierIdentifier",
        0xF18C => "ECUSerialNumber",
        0xF190 => "VIN",
        0xF191 => "vehicleManufacturerECUHardwareNumber",
        0xF192 => "systemSupplierECUHardwareNumber",
        0xF193 => "systemSupplierECUHardwareVersionNumber",
        0xF194 => "systemSupplierECUSoftwareNumber",
        0xF195 => "systemSupplierECUSoftwareVersionNumber",
        0xF197 => "systemName",
        0xF19E => "ASAMODXFileIdentifier",
        // BMW-specific, but documented in the report (dissec.to capture).
        0x172A => "IPConfiguration",
        0xFF00 => "UDSVersionDataIdentifier",
        _ => return None,
    };
    Some(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standard_name_knows_vin() {
        assert_eq!(standard_name(0xF190), Some("VIN"));
    }

    #[test]
    fn standard_name_unknown_did_is_none() {
        assert_eq!(standard_name(0x1234), None);
    }

    #[test]
    fn decode_vin_names_and_renders_ascii() {
        let d = decode(0xF190, b"WBA1234567890ABCD");
        assert_eq!(d.name, Some("VIN"));
        assert_eq!(d.text.as_deref(), Some("WBA1234567890ABCD"));
        assert_eq!(d.raw, b"WBA1234567890ABCD");
    }

    #[test]
    fn decode_binary_value_has_no_text_and_no_name() {
        let d = decode(0x4242, &[0x00, 0x9C, 0xFF]);
        assert_eq!(d.name, None);
        assert_eq!(d.text, None);
        assert_eq!(d.raw, vec![0x00, 0x9C, 0xFF]);
    }
}
