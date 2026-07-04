//! Structural decode of the BMW gateway VCM installed-ECU list (DID 0x3F07).
//!
//! The response data region (after the `62 3F 07` echo is stripped by
//! [`crate::decode_read_data_by_identifier`]) is a 2-byte big-endian ECU count
//! followed by one diagnostic-address byte per ECU. Names are NOT on the wire —
//! the caller resolves them from the semantic DB. This layout is DERIVED from the
//! `STATUS_VCM_GET_ECU_LIST_ALL` disassembly; no on-car capture exists yet, so it
//! is **[verify against capture]** and the decode is lenient (it returns the
//! address bytes actually present rather than trusting the declared count).

use crate::UdsError;

/// The BMW gateway's installed-ECU list: a declared count and the address bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EcuList {
    /// The ECU count the gateway declares (u16 big-endian at data offset 0..2).
    pub count: u16,
    /// One diagnostic address per installed ECU (the bytes after the count).
    pub addresses: Vec<u8>,
}

/// Decode the data region of a `62 3F 07` response into an [`EcuList`].
///
/// `data` is the region after the `62 3F 07` echo (as returned by
/// [`crate::decode_read_data_by_identifier`]). Reads the declared count, then
/// takes every remaining byte as one ECU address. [verify against capture]
///
/// # Errors
/// [`UdsError::ShortResponse`] if `data` is fewer than the 2 count bytes.
pub fn decode_ecu_list(data: &[u8]) -> Result<EcuList, UdsError> {
    let count_bytes = data.get(..2).ok_or(UdsError::ShortResponse {
        sid: 0x62,
        need: 2,
        got: data.len(),
    })?;
    let count = u16::from_be_bytes([count_bytes[0], count_bytes[1]]);
    Ok(EcuList {
        count,
        addresses: data[2..].to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    // DERIVED from the STATUS_VCM_GET_ECU_LIST_ALL disassembly (design §3.2): the
    // data region is a u16 BE count then one address byte per ECU. Synthetic bytes,
    // following the documented framing — no capture exists yet. [verify against capture]
    #[test]
    fn decodes_count_and_addresses() {
        // count = 0x0003, addresses 0x10 0x12 0x40
        let list = decode_ecu_list(&[0x00, 0x03, 0x10, 0x12, 0x40]).unwrap();
        assert_eq!(list.count, 3);
        assert_eq!(list.addresses, vec![0x10, 0x12, 0x40]);
    }

    #[test]
    fn empty_list_has_no_addresses() {
        let list = decode_ecu_list(&[0x00, 0x00]).unwrap();
        assert_eq!(list.count, 0);
        assert!(list.addresses.is_empty());
    }

    #[test]
    fn rejects_region_shorter_than_count() {
        assert!(matches!(
            decode_ecu_list(&[0x00]),
            Err(UdsError::ShortResponse { got: 1, .. })
        ));
    }
}
