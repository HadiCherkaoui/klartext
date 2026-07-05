//! Standard OBD-II / SAE J1979 PID scaling — public, documented formulas only.
//!
//! The J1979 "current data" PIDs (service 0x01) carry the universal powertrain
//! signals — coolant temperature, RPM, vehicle speed — with scaling that is
//! public (ISO 15031-5 / SAE J1979), not vendor data. ISO 14229-1 mirrors those
//! PIDs into the **OBDDataIdentifier** DID range `0xF400–0xF4FF`, so the existing
//! ReadDataByIdentifier (0x22) path reads them as `0xF4{PID}`: see [`pid_for_did`].
//!
//! This is deliberately *only* the standard set. BMW-proprietary DIDs scale via
//! the EDIABAS SGBD (see `docs/sqlite-findings.md`) and stay raw until that path
//! exists; [`scale`] returns `None` for anything not in `PIDS`, so an
//! unknown identifier always degrades to the raw value rather than erroring.

use std::ops::RangeInclusive;

/// A standard PID scaled to an engineering value: its name, value, and unit.
#[derive(Debug, Clone, PartialEq)]
pub struct ScaledPid {
    /// Human name of the signal, e.g. `"Engine coolant temperature"`.
    pub name: &'static str,
    /// The scaled engineering value (raw f64; consumers round for display).
    pub value: f64,
    /// The engineering unit, e.g. `"°C"`, `"rpm"`, `"km/h"`.
    pub unit: &'static str,
}

/// The ISO 14229-1 OBDDataIdentifier range: DID `0xF4{PID}` reads J1979 PID.
pub const OBD_DID_RANGE: RangeInclusive<u16> = 0xF400..=0xF4FF;

/// The J1979 PID a DID addresses, when it is in the OBDDataIdentifier range.
///
/// ISO 14229-1 maps OBD-II service-0x01 PIDs into `0xF400–0xF4FF`: the low byte
/// is the PID (e.g. `0xF40C` → PID `0x0C`, engine RPM). Returns `None` outside
/// that range, so non-OBD DIDs fall through to the existing named/raw handling.
pub fn pid_for_did(did: u16) -> Option<u8> {
    OBD_DID_RANGE.contains(&did).then_some(did as u8)
}

/// One row of the J1979 scaling table: a PID, its name/unit, and how to scale it.
struct PidDef {
    /// The OBD-II service-0x01 PID number.
    pid: u8,
    /// Human name of the signal.
    name: &'static str,
    /// Engineering unit of the scaled value.
    unit: &'static str,
    /// Minimum data bytes the formula needs (`data[0..bytes]` is then in range).
    bytes: usize,
    /// The byte-to-value formula. `data.len() >= bytes` is guaranteed by [`scale`]
    /// before this runs, so indexing `data[0..bytes]` cannot panic.
    formula: fn(&[u8]) -> f64,
}

/// The supported standard PIDs and their public J1979 formulas.
///
/// Formulas are ISO 15031-5 / SAE J1979 (the public OBD-II set); see
/// `docs/standard-pids.md` for the per-PID source. Bytes are named `A`, `B` after
/// the standard (A = first data byte). Nothing here is BMW-proprietary.
const PIDS: &[PidDef] = &[
    PidDef {
        pid: 0x04,
        name: "Calculated engine load",
        unit: "%",
        bytes: 1,
        formula: |d| f64::from(d[0]) * 100.0 / 255.0,
    },
    PidDef {
        pid: 0x05,
        name: "Engine coolant temperature",
        unit: "°C",
        bytes: 1,
        formula: |d| f64::from(d[0]) - 40.0,
    },
    PidDef {
        pid: 0x0C,
        name: "Engine RPM",
        unit: "rpm",
        bytes: 2,
        formula: |d| (256.0 * f64::from(d[0]) + f64::from(d[1])) / 4.0,
    },
    PidDef {
        pid: 0x0D,
        name: "Vehicle speed",
        unit: "km/h",
        bytes: 1,
        formula: |d| f64::from(d[0]),
    },
    PidDef {
        pid: 0x0E,
        name: "Timing advance",
        unit: "°",
        bytes: 1,
        formula: |d| f64::from(d[0]) / 2.0 - 64.0,
    },
    PidDef {
        pid: 0x0F,
        name: "Intake air temperature",
        unit: "°C",
        bytes: 1,
        formula: |d| f64::from(d[0]) - 40.0,
    },
    PidDef {
        pid: 0x10,
        name: "MAF air flow rate",
        unit: "g/s",
        bytes: 2,
        formula: |d| (256.0 * f64::from(d[0]) + f64::from(d[1])) / 100.0,
    },
    PidDef {
        pid: 0x11,
        name: "Throttle position",
        unit: "%",
        bytes: 1,
        formula: |d| f64::from(d[0]) * 100.0 / 255.0,
    },
    PidDef {
        pid: 0x23,
        name: "Fuel rail gauge pressure",
        unit: "kPa",
        bytes: 2,
        formula: |d| (256.0 * f64::from(d[0]) + f64::from(d[1])) * 10.0,
    },
    PidDef {
        pid: 0x46,
        name: "Ambient air temperature",
        unit: "°C",
        bytes: 1,
        formula: |d| f64::from(d[0]) - 40.0,
    },
];

/// Scale a standard J1979 PID's raw data bytes to an engineering value.
///
/// Pure: no I/O, no allocation. Returns `None` when `pid` is not a supported
/// standard PID, or when `data` is shorter than that PID needs — both cases mean
/// "I can't scale this", and the caller keeps the raw bytes. STANDARD PIDs only;
/// proprietary scaling is out of scope (see the module docs).
pub fn scale(pid: u8, data: &[u8]) -> Option<ScaledPid> {
    let def = PIDS.iter().find(|p| p.pid == pid)?;
    if data.len() < def.bytes {
        return None;
    }
    Some(ScaledPid {
        name: def.name,
        value: (def.formula)(data),
        unit: def.unit,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert two engineering values are equal within float tolerance.
    fn approx(actual: f64, expected: f64) {
        assert!(
            (actual - expected).abs() < 1e-6,
            "expected {expected}, got {actual}"
        );
    }

    /// Convenience: scale and unwrap, failing loudly if the PID went unrecognized.
    fn scaled(pid: u8, data: &[u8]) -> ScaledPid {
        scale(pid, data).unwrap_or_else(|| panic!("PID 0x{pid:02X} did not scale"))
    }

    #[test]
    fn calculated_engine_load_is_percent_of_255() {
        let s = scaled(0x04, &[0x33]); // 51 * 100 / 255 = 20 %
        assert_eq!(s.name, "Calculated engine load");
        assert_eq!(s.unit, "%");
        approx(s.value, 20.0);
        approx(scaled(0x04, &[0x00]).value, 0.0); // min
        approx(scaled(0x04, &[0xFF]).value, 100.0); // max
    }

    #[test]
    fn coolant_temp_is_celsius_minus_40() {
        let s = scaled(0x05, &[0x7B]); // 123 - 40 = 83 °C
        assert_eq!(s.name, "Engine coolant temperature");
        assert_eq!(s.unit, "°C");
        approx(s.value, 83.0);
        approx(scaled(0x05, &[0x00]).value, -40.0); // min
        approx(scaled(0x05, &[0xFF]).value, 215.0); // max
    }

    #[test]
    fn engine_rpm_is_quarter_counts() {
        let s = scaled(0x0C, &[0x0D, 0x48]); // (256*13 + 72)/4 = 850 rpm
        assert_eq!(s.name, "Engine RPM");
        assert_eq!(s.unit, "rpm");
        approx(s.value, 850.0);
        approx(scaled(0x0C, &[0x00, 0x00]).value, 0.0); // min
        approx(scaled(0x0C, &[0xFF, 0xFF]).value, 16383.75); // max
    }

    #[test]
    fn vehicle_speed_is_raw_kmh() {
        let s = scaled(0x0D, &[0x64]); // 100 km/h
        assert_eq!(s.name, "Vehicle speed");
        assert_eq!(s.unit, "km/h");
        approx(s.value, 100.0);
        approx(scaled(0x0D, &[0x00]).value, 0.0); // min
        approx(scaled(0x0D, &[0xFF]).value, 255.0); // max
    }

    #[test]
    fn timing_advance_is_half_degrees_minus_64() {
        let s = scaled(0x0E, &[0x80]); // 128/2 - 64 = 0 ° (at TDC)
        assert_eq!(s.name, "Timing advance");
        assert_eq!(s.unit, "°");
        approx(s.value, 0.0);
        approx(scaled(0x0E, &[0x00]).value, -64.0); // min
        approx(scaled(0x0E, &[0xFF]).value, 63.5); // max
    }

    #[test]
    fn intake_air_temp_is_celsius_minus_40() {
        let s = scaled(0x0F, &[0x28]); // 40 - 40 = 0 °C
        assert_eq!(s.name, "Intake air temperature");
        assert_eq!(s.unit, "°C");
        approx(s.value, 0.0);
        approx(scaled(0x0F, &[0x00]).value, -40.0); // min
        approx(scaled(0x0F, &[0xFF]).value, 215.0); // max
    }

    #[test]
    fn maf_rate_is_grams_per_second() {
        let s = scaled(0x10, &[0x01, 0xF4]); // (256 + 244)/100 = 5.00 g/s
        assert_eq!(s.name, "MAF air flow rate");
        assert_eq!(s.unit, "g/s");
        approx(s.value, 5.0);
        approx(scaled(0x10, &[0x00, 0x00]).value, 0.0); // min
        approx(scaled(0x10, &[0xFF, 0xFF]).value, 655.35); // max
    }

    #[test]
    fn throttle_position_is_percent_of_255() {
        let s = scaled(0x11, &[0xFF]); // 100 %
        assert_eq!(s.name, "Throttle position");
        assert_eq!(s.unit, "%");
        approx(s.value, 100.0);
        approx(scaled(0x11, &[0x00]).value, 0.0); // min
    }

    #[test]
    fn fuel_rail_gauge_pressure_is_ten_kpa_counts() {
        let s = scaled(0x23, &[0x01, 0x00]); // 256 * 10 = 2560 kPa
        assert_eq!(s.name, "Fuel rail gauge pressure");
        assert_eq!(s.unit, "kPa");
        approx(s.value, 2560.0);
        approx(scaled(0x23, &[0x00, 0x00]).value, 0.0); // min
        approx(scaled(0x23, &[0xFF, 0xFF]).value, 655350.0); // max
    }

    #[test]
    fn ambient_air_temp_is_celsius_minus_40() {
        let s = scaled(0x46, &[0x37]); // 55 - 40 = 15 °C
        assert_eq!(s.name, "Ambient air temperature");
        assert_eq!(s.unit, "°C");
        approx(s.value, 15.0);
        approx(scaled(0x46, &[0x00]).value, -40.0); // min
        approx(scaled(0x46, &[0xFF]).value, 215.0); // max
    }

    #[test]
    fn unknown_pid_does_not_scale() {
        // 0x99 is not a PID we support — degrade to raw (None), never guess.
        assert_eq!(scale(0x99, &[0x12, 0x34]), None);
    }

    #[test]
    fn two_byte_pid_with_one_byte_does_not_scale() {
        // RPM needs two bytes; one byte can't be scaled — degrade to raw.
        assert_eq!(scale(0x0C, &[0x0D]), None);
    }

    #[test]
    fn empty_data_does_not_scale() {
        assert_eq!(scale(0x05, &[]), None);
    }

    #[test]
    fn pid_for_did_maps_the_obd_range() {
        assert_eq!(pid_for_did(0xF405), Some(0x05)); // coolant
        assert_eq!(pid_for_did(0xF40C), Some(0x0C)); // RPM
        assert_eq!(pid_for_did(0xF400), Some(0x00)); // range start
        assert_eq!(pid_for_did(0xF4FF), Some(0xFF)); // range end
    }

    #[test]
    fn pid_for_did_outside_the_obd_range_is_none() {
        assert_eq!(pid_for_did(0xF190), None); // VIN — identification DID
        assert_eq!(pid_for_did(0xF3FF), None); // just below the range
        assert_eq!(pid_for_did(0xF500), None); // just above the range
        assert_eq!(pid_for_did(0x000C), None); // bare PID, not a DID
    }
}
