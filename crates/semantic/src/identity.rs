//! Vehicle-identity decoding: the ECU-name overlay for the gateway SVT list, and
//! (later) the FA vehicle-order decode.
//!
//! The SVT read gives diagnostic addresses only; the gateway's own name table is
//! coarse and stale (its 0x40 says "CAS", wrong for many cars). So names come from
//! the ISTA-derived semantic DB (`Catalog::ecus()`) — generic across BMW — and an
//! address the DB doesn't know keeps a raw-hex name rather than being dropped.

use crate::catalog::{Catalog, EcuSlot};

/// One installed ECU with a DB-resolved name, or a raw-hex fallback name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedEcu {
    /// The diagnostic address from the SVT.
    pub address: u8,
    /// The ISTA group name for this address, or `None` if the DB lacks it.
    pub name: Option<String>,
    /// A human title for the address, if the DB has one.
    pub title: Option<String>,
}

/// Overlay DB names onto SVT addresses. Order and multiplicity follow `addresses`.
///
/// With no catalog, or for an address the DB does not know, `name`/`title` are
/// `None` — the address is always kept (never dropped or guessed).
pub fn name_ecu_list(catalog: Option<&Catalog>, addresses: &[u8]) -> Vec<NamedEcu> {
    let slots: Vec<EcuSlot> = catalog.and_then(|c| c.ecus().ok()).unwrap_or_default();
    addresses
        .iter()
        .map(|&address| {
            let slot = slots.iter().find(|s| s.address == address);
            NamedEcu {
                address,
                name: slot.map(|s| s.group_name.clone()),
                title: slot.and_then(|s| s.title.clone()),
            }
        })
        .collect()
}

/// The decoded vehicle order (Fahrzeugauftrag / FA) from gateway DID 0x3F06.
///
/// The **request** framing `22 3F06` is byte-confirmed against the F20's own gateway
/// SGBD (`zgw_01.prg`, deobfuscated XOR 0xF7 from offset 0xA0; offline, no car). The
/// **response** decode is not: `version` and `raw` are decoded now, but the header
/// fields and option list are **capture-gated** (the FA byte layout is version-branched,
/// compressed bytecode that needs an on-car capture to confirm) and stay `None`/empty
/// until then. `raw` is always kept so nothing is lost. The [verify against capture]
/// caveat covers the response layout only — the request DID is confirmed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VehicleOrder {
    pub version: Option<u16>,
    pub baureihe: Option<String>,
    pub typ_schluessel: Option<String>,
    pub lackcode: Option<String>,
    pub polstercode: Option<String>,
    pub build_date: Option<String>,
    pub options: Vec<String>,
    pub raw: Vec<u8>,
}

/// FA data-region offset of the version byte, read by STATUS_VCM_GET_FA (`move L0,#5`).
/// [verify against capture] — the EDIABAS bytecode reads this over its KWP framing;
/// the offset in the raw HSFZ region is confirmed on capture.
const FA_VERSION_OFFSET: usize = 5;

/// Decode the FA (vehicle order) data region. Extracts the version and keeps the raw
/// bytes; header fields and the option list are capture-gated (see [`VehicleOrder`]).
pub fn decode_vehicle_order(region: &[u8]) -> VehicleOrder {
    let version = region.get(FA_VERSION_OFFSET).map(|&b| u16::from(b));
    VehicleOrder {
        version,
        baureihe: None,
        typ_schluessel: None,
        lackcode: None,
        polstercode: None,
        build_date: None,
        options: Vec::new(),
        raw: region.to_vec(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_catalog_addresses_keep_raw_names() {
        let ecus = name_ecu_list(None, &[0x10, 0x12]);
        assert_eq!(
            ecus,
            vec![
                NamedEcu {
                    address: 0x10,
                    name: None,
                    title: None
                },
                NamedEcu {
                    address: 0x12,
                    name: None,
                    title: None
                },
            ]
        );
    }

    // Cross-check against the owner's real semantic DB. Ignored by default (BYO data).
    #[test]
    #[ignore = "requires BYO data: data/klartext-semantic.db"]
    fn real_db_names_known_addresses() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../data/klartext-semantic.db");
        let catalog = Catalog::open(&path).expect("open semantic DB");
        // 0x10 is the gateway on F/G-series; the DB should name it, not guess.
        let ecus = name_ecu_list(Some(&catalog), &[0x10]);
        assert_eq!(ecus.len(), 1);
        assert_eq!(ecus[0].address, 0x10);
        assert!(ecus[0].name.is_some(), "DB should name the gateway address");
    }
}

#[cfg(test)]
mod fa_tests {
    use super::*;

    #[test]
    fn decodes_version_and_keeps_raw_fields_pending_capture() {
        // Synthetic FA region: version byte 0x02 at the derived offset. [verify]
        let region = vec![0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x11, 0x22];
        let fa = decode_vehicle_order(&region);
        assert_eq!(fa.version, Some(2));
        assert_eq!(fa.raw, region);
        // Field decode is capture-gated: None/empty until the FA layout is confirmed.
        assert_eq!(fa.baureihe, None);
        assert!(fa.options.is_empty());
    }

    #[test]
    fn short_region_has_no_version_but_keeps_raw() {
        let fa = decode_vehicle_order(&[0x01, 0x02]);
        assert_eq!(fa.version, None);
        assert_eq!(fa.raw, vec![0x01, 0x02]);
    }
}
