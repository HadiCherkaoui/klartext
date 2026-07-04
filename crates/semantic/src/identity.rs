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
