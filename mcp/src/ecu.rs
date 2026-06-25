//! ECU targeting: a built-in BMW-wide alias table plus the semantic DB's general
//! ECU map, with name/address resolution for the read tools.
//!
//! The built-in aliases are the few addresses the protocol report documents for
//! BMW broadly (not one car); the semantic DB ([`Catalog::ecus`]) supplies the
//! full, per-model map when present. Resolution accepts a raw hex address, an
//! alias, or a DB group name, so a target is reachable with or without the DB.

use std::collections::BTreeMap;

use klartext_hsfz::ZGW_ADDRESS;
use klartext_semantic::Catalog;

use crate::dto::EcuInfo;

/// BMW-wide documented ECU aliases (report §2.4). Not car-specific; the full
/// per-model map comes from the semantic DB. [verify against capture].
const BUILTIN_ALIASES: &[(&str, u8)] = &[
    ("ZGW", ZGW_ADDRESS), // 0x10 — central gateway
    ("DME", 0x12),        // engine
    ("CAS", 0x40),        // car access system / body (FEM on later F-series)
];

/// Resolve an `ecu` parameter to a diagnostic address.
///
/// Resolution order: a raw hex address (`0x12`), then a built-in alias
/// (case-insensitive), then a semantic-DB group name (`d_0012`).
///
/// # Errors
/// Returns a human message (naming `list_ecus`) when `spec` matches none of these
/// forms.
pub fn resolve(spec: &str, catalog: Option<&Catalog>) -> Result<u8, String> {
    let s = spec.trim();
    if let Some(addr) = parse_hex_address(s) {
        return Ok(addr);
    }
    if let Some((_, addr)) = BUILTIN_ALIASES
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(s))
    {
        return Ok(*addr);
    }
    if let Some(catalog) = catalog
        && let Ok(entries) = catalog.ecus()
        && let Some(entry) = entries
            .iter()
            .find(|e| e.group_name.eq_ignore_ascii_case(s))
    {
        return Ok(entry.address);
    }
    Err(format!(
        "unknown ECU '{spec}'. Use a name (call list_ecus), an ISTA group name like \
         d_0012, or a raw hex address like 0x12."
    ))
}

/// List targetable ECUs: built-in aliases merged with the DB map, by address.
pub fn list(catalog: Option<&Catalog>) -> Vec<EcuInfo> {
    /// Per-address accumulator while merging the two sources.
    #[derive(Default)]
    struct Acc {
        names: Vec<String>,
        group_name: Option<String>,
        from_builtin: bool,
        from_db: bool,
    }

    let mut map: BTreeMap<u8, Acc> = BTreeMap::new();
    for (name, addr) in BUILTIN_ALIASES {
        let acc = map.entry(*addr).or_default();
        acc.names.push((*name).to_string());
        acc.from_builtin = true;
    }
    if let Some(catalog) = catalog
        && let Ok(entries) = catalog.ecus()
    {
        for entry in entries {
            let acc = map.entry(entry.address).or_default();
            if acc.group_name.is_none() {
                acc.group_name = Some(entry.group_name.clone());
            }
            if !acc.names.contains(&entry.group_name) {
                acc.names.push(entry.group_name);
            }
            acc.from_db = true;
        }
    }
    map.into_iter()
        .map(|(addr, acc)| EcuInfo {
            address_hex: format!("0x{addr:02X}"),
            names: acc.names,
            group_name: acc.group_name,
            source: match (acc.from_builtin, acc.from_db) {
                (true, true) => "builtin+db",
                (true, false) => "builtin",
                _ => "db",
            }
            .to_string(),
        })
        .collect()
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
    fn resolve_raw_hex_address() {
        assert_eq!(resolve("0x12", None).unwrap(), 0x12);
        assert_eq!(resolve("0X40", None).unwrap(), 0x40);
    }

    #[test]
    fn resolve_builtin_alias_case_insensitive() {
        assert_eq!(resolve("DME", None).unwrap(), 0x12);
        assert_eq!(resolve("zgw", None).unwrap(), 0x10);
    }

    #[test]
    fn resolve_unknown_without_db_errors_with_hint() {
        let err = resolve("d_0012", None).unwrap_err();
        assert!(err.contains("list_ecus"), "{err}");
    }

    #[test]
    fn list_without_db_returns_builtins() {
        let ecus = list(None);
        let names: Vec<&str> = ecus
            .iter()
            .flat_map(|e| e.names.iter().map(String::as_str))
            .collect();
        assert!(names.contains(&"ZGW"));
        assert!(names.contains(&"DME"));
        assert!(names.contains(&"CAS"));
        assert!(ecus.iter().all(|e| e.source == "builtin"));
    }
}
