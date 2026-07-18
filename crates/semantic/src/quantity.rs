//! Physical quantities a caller can ask for by MEANING, not by measurement name.
//!
//! A precondition needs "the battery voltage", but the catalog offers per-variant
//! names whose units differ: `STAT_UBATT_WERT` is volts on 33 variants and
//! millivolts on 28. Worse, a name-pattern search for it on a real DDE also matches
//! the accelerator-pedal sensor (`STAT_PWG1_SPANNUNG_WERT`, mV) and an alternator
//! setpoint. Binding a safety check to the wrong sensor is worse than not checking,
//! so this module maps each quantity to a CURATED candidate list and normalises the
//! unit from the catalog's own `unit` column — never from the name, and never by
//! guessing.

/// A physical quantity, resolvable to a measurement on a given ECU variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Quantity {
    /// Engine speed, canonically `1/min`.
    EngineSpeed,
    /// Battery voltage, canonically `V`.
    BatteryVoltage,
    /// Coolant temperature, canonically `°C`.
    CoolantTemp,
    /// Road speed, canonically `km/h`.
    RoadSpeed,
}

impl Quantity {
    /// Every quantity, for exhaustive tests and callers that enumerate.
    pub const ALL: [Quantity; 4] = [
        Quantity::EngineSpeed,
        Quantity::BatteryVoltage,
        Quantity::CoolantTemp,
        Quantity::RoadSpeed,
    ];

    /// The EDIABAS result names that genuinely carry this quantity, best first.
    ///
    /// Curated deliberately. Pattern matching is forbidden here: on a real DDE
    /// `%SPANNUNG%` also matches the accelerator-pedal sensor, and silently
    /// reading a pedal as a battery would defeat the check it guards.
    pub fn candidates(self) -> &'static [&'static str] {
        match self {
            Quantity::EngineSpeed => &[
                "STAT_MOTORDREHZAHL_WERT",
                "STAT_MOTORDREHZAHL",
                "STAT_MOTORDREHZAHL_N32_WERT",
            ],
            Quantity::BatteryVoltage => &[
                "STAT_UBATT_WERT",
                "STAT_UBATT",
                "STAT_BATTERIESPANNUNG_IBS_WERT",
                "STAT_UBATT_IBS_WERT",
            ],
            Quantity::CoolantTemp => &[
                "STAT_KUEHLMITTELTEMPERATUR_WERT",
                "STAT_KUEHLMITTELTEMPERATUR",
            ],
            Quantity::RoadSpeed => &[
                "STAT_GESCHWINDIGKEIT_WERT",
                "STAT_FAHRZEUGGESCHWINDIGKEIT_WERT",
            ],
        }
    }

    /// The unit this quantity's values are expressed in once normalised.
    pub fn canonical_unit(self) -> &'static str {
        match self {
            Quantity::EngineSpeed => "1/min",
            Quantity::BatteryVoltage => "V",
            Quantity::CoolantTemp => "°C",
            Quantity::RoadSpeed => "km/h",
        }
    }

    /// The factor converting a value in `catalog_unit` to [`Self::canonical_unit`].
    ///
    /// `None` means NOT NORMALISABLE — an absent, empty, or unrecognised unit, or a
    /// unit belonging to another quantity. The caller must treat that as
    /// unresolvable and degrade to advisory. Assuming a unit here is precisely the
    /// silent-wrong-binding this module exists to prevent.
    pub fn unit_factor(self, catalog_unit: Option<&str>) -> Option<f64> {
        let unit = catalog_unit?.trim();
        match (self, unit) {
            (Quantity::EngineSpeed, "1/min" | "rpm") => Some(1.0),
            (Quantity::BatteryVoltage, "V") => Some(1.0),
            (Quantity::BatteryVoltage, "mV") => Some(0.001),
            (Quantity::CoolantTemp, "°C" | "C" | "degC") => Some(1.0),
            (Quantity::RoadSpeed, "km/h") => Some(1.0),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn candidates_are_curated_never_patterns() {
        // The pedal-sensor trap: on the real DDE a `%SPANNUNG%` pattern also
        // matches STAT_PWG1_SPANNUNG_WERT (accelerator pedal, mV) and
        // STAT_SOLLWERT_GENERATORSPANNUNG_WERT (alternator setpoint). Binding
        // battery voltage to either is worse than not checking at all, so the
        // candidate list must be explicit and must NOT contain them.
        let battery = Quantity::BatteryVoltage.candidates();
        assert!(battery.contains(&"STAT_UBATT_WERT"));
        assert!(!battery.iter().any(|n| n.contains("PWG")));
        assert!(!battery.iter().any(|n| n.contains("SOLLWERT")));
        // Every quantity must offer at least one candidate, or it can never resolve.
        for q in Quantity::ALL {
            assert!(!q.candidates().is_empty(), "{q:?} has no candidates");
        }
    }

    #[test]
    fn unit_factor_normalises_millivolts_to_volts() {
        // STAT_UBATT_WERT is volts on 33 variants and millivolts on 28 — the SAME
        // name. Without normalisation a 12.0 V floor passes at 12 mV, so a flat
        // battery reading ~11800 mV sails through and the guard inverts into a
        // rubber stamp.
        assert_eq!(Quantity::BatteryVoltage.unit_factor(Some("V")), Some(1.0));
        assert_eq!(
            Quantity::BatteryVoltage.unit_factor(Some("mV")),
            Some(0.001)
        );
    }

    #[test]
    fn an_unknown_or_missing_unit_is_unresolvable_not_assumed() {
        // Some catalog rows carry an empty unit. Guessing "it's probably volts"
        // is exactly the silent-wrong-binding this design forbids.
        assert_eq!(Quantity::BatteryVoltage.unit_factor(None), None);
        assert_eq!(Quantity::BatteryVoltage.unit_factor(Some("")), None);
        assert_eq!(Quantity::BatteryVoltage.unit_factor(Some("bar")), None);
        // A unit belonging to a DIFFERENT quantity must not resolve either.
        assert_eq!(Quantity::EngineSpeed.unit_factor(Some("V")), None);
        assert_eq!(Quantity::CoolantTemp.unit_factor(Some("km/h")), None);
    }

    #[test]
    fn each_quantity_accepts_its_own_units() {
        assert_eq!(Quantity::EngineSpeed.unit_factor(Some("1/min")), Some(1.0));
        assert_eq!(Quantity::CoolantTemp.unit_factor(Some("°C")), Some(1.0));
        assert_eq!(Quantity::RoadSpeed.unit_factor(Some("km/h")), Some(1.0));
        // Canonical units are what a caller's thresholds are expressed in.
        assert_eq!(Quantity::BatteryVoltage.canonical_unit(), "V");
        assert_eq!(Quantity::EngineSpeed.canonical_unit(), "1/min");
    }

    #[test]
    fn each_canonical_unit_is_accepted_by_its_owner_alone() {
        // The tests above only spot-check a few cross-quantity pairs (e.g.
        // EngineSpeed rejects "V"). That leaves room for a plausible-looking bug
        // to slip through undetected — e.g. RoadSpeed's match arm accidentally
        // also accepting "°C", or CoolantTemp/RoadSpeed's canonical_unit() values
        // swapped — since neither is asserted anywhere else. A caller compares a
        // reading's normalised value against a threshold expressed in
        // canonical_unit(); if a quantity accepted a unit it doesn't own, a
        // reading of the wrong physical property would normalise and silently
        // pass a guard for something else entirely. Check the full cross-product
        // instead of spot pairs.
        let canonical = [
            (Quantity::EngineSpeed, "1/min"),
            (Quantity::BatteryVoltage, "V"),
            (Quantity::CoolantTemp, "°C"),
            (Quantity::RoadSpeed, "km/h"),
        ];
        for (owner, unit) in canonical {
            assert_eq!(
                owner.canonical_unit(),
                unit,
                "{owner:?} canonical unit mismatch"
            );
            for q in Quantity::ALL {
                let factor = q.unit_factor(Some(unit));
                if q == owner {
                    assert_eq!(factor, Some(1.0), "{q:?} must accept its own unit {unit:?}");
                } else {
                    assert_eq!(
                        factor, None,
                        "{q:?} must NOT accept {unit:?} (owned by {owner:?})"
                    );
                }
            }
        }
    }
}
