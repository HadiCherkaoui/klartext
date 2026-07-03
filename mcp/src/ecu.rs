//! ECU targeting: resolve a name/hex/variant to a diagnostic address, and list
//! the targetable ECUs — all from the ISTA semantic DB, no hardcoded aliases.
//!
//! The first live session proved static aliases actively harmful: "DME" mislabels
//! this car's diesel DDE and "CAS" is really the FEM on F20. Names now come only
//! from the DB ([`Catalog::ecus`]/[`Catalog::variants`]); without the DB the tools
//! accept raw hex addresses and say so, rather than surfacing a wrong name.

use klartext_semantic::Catalog;

use crate::dto::EcuInfo;

/// Resolve an `ecu` parameter to a diagnostic address.
///
/// Order: a raw hex address (`0x12`), then (with the DB) an ISTA group name
/// (`d_0012`, case-insensitive), then an ISTA variant name (`d72n47a0`).
///
/// # Errors
/// Returns a human message (naming `list_ecus`) when `spec` matches none of these,
/// or when the DB is present but a lookup query fails (surfaced, not swallowed).
pub fn resolve(spec: &str, catalog: Option<&Catalog>) -> Result<u8, String> {
    let s = spec.trim();
    if let Some(addr) = parse_hex_address(s) {
        return Ok(addr);
    }
    if let Some(catalog) = catalog {
        let slots = catalog
            .ecus()
            .map_err(|e| format!("reading the ECU map: {e}"))?;
        // A group name — canonical or an extra group at the same address.
        if let Some(slot) = slots.iter().find(|slot| {
            slot.group_name.eq_ignore_ascii_case(s)
                || slot.extra_groups.iter().any(|g| g.eq_ignore_ascii_case(s))
        }) {
            return Ok(slot.address);
        }
        // A variant name → its address.
        for slot in &slots {
            let variants = catalog
                .variants(slot.address)
                .map_err(|e| format!("reading ECU variants: {e}"))?;
            if variants.iter().any(|v| v.name.eq_ignore_ascii_case(s)) {
                return Ok(slot.address);
            }
        }
    }
    Err(format!(
        "unknown ECU '{spec}'. Use a raw hex address like 0x12, or (with the semantic \
         DB) an ISTA group name like d_0012 or a variant name like d72n47a0 — call \
         list_ecus to see them."
    ))
}

/// List targetable ECUs from the semantic DB (empty without it).
///
/// # Errors
/// Returns a human message if the DB is present but the ECU query fails — the
/// caller surfaces that instead of silently reporting an empty list.
pub fn list(catalog: Option<&Catalog>) -> Result<Vec<EcuInfo>, String> {
    let Some(catalog) = catalog else {
        return Ok(Vec::new());
    };
    let slots = catalog
        .ecus()
        .map_err(|e| format!("reading the ECU map: {e}"))?;
    let mut out = Vec::with_capacity(slots.len());
    for slot in slots {
        let variants = catalog
            .variants(slot.address)
            .map_err(|e| format!("reading ECU variants: {e}"))?
            .into_iter()
            .map(|v| v.name)
            .collect();
        out.push(EcuInfo {
            address_hex: format!("0x{:02X}", slot.address),
            group_name: slot.group_name,
            extra_groups: slot.extra_groups,
            title: slot.title,
            variants,
        });
    }
    Ok(out)
}

/// Parse a raw diagnostic address written as `0x12` / `0X12`.
///
/// Bare decimals are rejected (ambiguous with hex), so addresses are explicit.
fn parse_hex_address(s: &str) -> Option<u8> {
    let hex = s.strip_prefix("0x").or_else(|| s.strip_prefix("0X"))?;
    u8::from_str_radix(hex, 16).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_raw_hex_address_without_db() {
        assert_eq!(resolve("0x12", None).unwrap(), 0x12);
        assert_eq!(resolve("0X40", None).unwrap(), 0x40);
    }

    #[test]
    fn resolve_unknown_without_db_names_list_ecus() {
        let err = resolve("DME", None).unwrap_err();
        assert!(err.contains("list_ecus"), "{err}");
        // The old wrong aliases are gone: "DME" no longer resolves to 0x12.
        assert!(resolve("DME", None).is_err());
        assert!(resolve("CAS", None).is_err());
    }

    #[test]
    fn list_without_db_is_empty_not_misleading_aliases() {
        assert!(list(None).unwrap().is_empty());
    }
}
