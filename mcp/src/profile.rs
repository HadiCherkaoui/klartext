//! A learned, per-VIN map of diagnostic address → SGBD variant.
//!
//! Variant auto-detection from the gateway (the SVT read) is a future milestone;
//! until then, when a caller reads an ECU with an explicit `variant` and it scales
//! a value, we remember it for that VIN. Later reads of the same ECU on the same
//! car then default to that variant, so the human types it once. Stored as small
//! JSON per VIN under a state dir — no BMW data, just address → variant.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// A car's learned address → variant map, keyed on VIN by its file name.
#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct CarProfile {
    /// Diagnostic address → SGBD variant learned for this car.
    pub variants: BTreeMap<u8, String>,
}

impl CarProfile {
    /// The learned variant for `address`, if any.
    pub fn get(&self, address: u8) -> Option<&str> {
        self.variants.get(&address).map(String::as_str)
    }
}

/// The profile file path for `vin` under `dir` (VIN sanitized to a safe stem).
pub fn profile_path(dir: &Path, vin: &str) -> PathBuf {
    let stem: String = vin
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect();
    dir.join(format!("{stem}.json"))
}

/// Load the profile for `vin`; a missing or unreadable/corrupt file yields empty.
pub fn load(dir: &Path, vin: &str) -> CarProfile {
    let path = profile_path(dir, vin);
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
            tracing::warn!(%error, path = %path.display(), "ignoring corrupt car profile");
            CarProfile::default()
        }),
        Err(_) => CarProfile::default(),
    }
}

/// Record `address → variant` for `vin`, merging into any existing profile.
///
/// Writes atomically (temp file + rename) so a crash mid-write cannot corrupt it.
/// A no-op when the mapping is already present, to avoid needless writes.
///
/// # Errors
/// Returns the I/O error if the directory cannot be created or the file written.
pub fn record(dir: &Path, vin: &str, address: u8, variant: &str) -> std::io::Result<()> {
    let mut profile = load(dir, vin);
    if profile.get(address) == Some(variant) {
        return Ok(()); // unchanged
    }
    std::fs::create_dir_all(dir)?;
    profile.variants.insert(address, variant.to_string());
    let path = profile_path(dir, vin);
    let tmp = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(&profile).expect("a CarProfile always serializes");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn record_then_load_roundtrips_by_vin() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "WBAVIN0000000001", 0x12, "d72n47a0").unwrap();
        record(dir.path(), "WBAVIN0000000001", 0x40, "fem_20").unwrap();
        let p = load(dir.path(), "WBAVIN0000000001");
        assert_eq!(p.get(0x12), Some("d72n47a0"));
        assert_eq!(p.get(0x40), Some("fem_20"));
        // A different VIN is a separate profile.
        assert!(load(dir.path(), "WBAVIN0000000002").get(0x12).is_none());
    }

    #[test]
    fn record_updates_an_existing_address() {
        let dir = tempfile::tempdir().unwrap();
        record(dir.path(), "V", 0x12, "old").unwrap();
        record(dir.path(), "V", 0x12, "d72n47a0").unwrap();
        assert_eq!(load(dir.path(), "V").get(0x12), Some("d72n47a0"));
    }

    #[test]
    fn missing_or_corrupt_profile_loads_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(load(dir.path(), "nope").variants.is_empty());
        std::fs::write(profile_path(dir.path(), "bad"), b"{ not json").unwrap();
        assert!(load(dir.path(), "bad").variants.is_empty());
    }
}
