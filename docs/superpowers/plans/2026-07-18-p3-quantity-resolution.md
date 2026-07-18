# P3 Quantity Resolution Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Turn `klartext-service`'s precondition seam from unit-safe-but-inert into something that can actually resolve a physical quantity on a real car — by mapping a `Quantity` to a *curated* measurement on the **engine ECU's** variant and normalising its unit, refusing to guess when it cannot.

**Architecture:** Resolution is pure semantic work, so it lives in `klartext-semantic` beside the catalog it reads. It is a lookup + unit normalisation returning a measurement name and a scale factor; the binaries perform the actual read. `klartext-service` gains two small corrections found in verification.

**Tech Stack:** Rust edition 2024, rusqlite (already a `klartext-semantic` dependency).

## Global Constraints

- Latest stable Rust, edition 2024. `thiserror` in libraries, `anyhow` at binary boundaries.
- `cargo fmt --all` (run via Bash, not the editor hook) and `cargo clippy --workspace --all-targets -- -D warnings` clean before a task is done. Verify exit codes DIRECTLY (`cmd ; echo "rc=$?"`), never behind `| tail`.
- Conventional commits.
- **Never claim a hardware round-trip works.** Everything here is offline.
- **No surface.** No MCP tool, no CLI subcommand — that is the next plan.
- Never commit BMW data. Tests use synthetic fixtures; a test that needs the real DB must skip when absent.
- No ms-rust guideline-marker comments.

## AMENDMENT 2026-07-18 — resolve by ISTA's own labels, not by curated names

The owner's standing principle: **klartext does exactly what ISTA does, and must work on ANY car
plugged in — no hardcoding.** Task 1's curated EDIABAS-name list violates that: it covers only
65–107 of 1,281 variants, so on an ECU whose measurement is named differently the gate silently goes
advisory and protects nothing.

**The correct key, verified in ISTA's data:** `XEP_ECURESULTS` carries human titles we never
extracted, and **all 136,885 rows have one** — 100% coverage, fleet-wide. They are BMW's own
vocabulary and they disambiguate exactly the trap that defeats name matching:

| Name | Unit | ISTA title |
|---|---|---|
| `STAT_UBATT_WERT` | V | **Battery voltage** |
| `STAT_BATTERIESPANNUNG_IBS_WERT` | V | Battery voltage, IBS |
| `STAT_PWG1_SPANNUNG_WERT` | mV | Accelerator pedal, hall effect sensor 1: Voltage |
| `STAT_SOLLWERT_GENERATORSPANNUNG_WERT` | V | Alternator: Target voltage |
| `STAT_MOTORDREHZAHL_WERT` | 1/min | **Engine speed** |

So resolution matches **ISTA's title**, constrained by unit. Some mapping from klartext's `Quantity`
to a label must exist — the code has to know `BatteryVoltage` means ISTA's "Battery voltage" — but
that mapping is now one line per quantity against BMW's canonical vocabulary, present on every
result, instead of a list of per-ECU identifiers covering 8% of the fleet.

(A numeric prefix exists on some titles — "104 Battery voltage", "101 Engine speed" — and is stable
where present, but only ~29 variants use it, so the title TEXT is the universal key, not the number.)

**Consequences for the task list:** Task 1 (`930ea45`) is KEPT — its `unit_factor`/`canonical_unit`
are correct and still needed. Its `candidates()` name list is superseded by title matching. A new
task extracts the titles first.

## The safety rule this plan exists to encode

**An unresolvable quantity must degrade to advisory — it must NEVER silently bind to a plausible-looking wrong measurement.**

This is not hypothetical. On the owner's DDE (`d72n47a0`), a name-pattern search for battery voltage (`%UBATT%` OR `%SPANNUNG%`) returns six hits, including:

| Name | Unit | What it actually is |
|---|---|---|
| `STAT_UBATT_WERT` | V | battery voltage — correct |
| `STAT_UBATT2_WERT` | mV | a second battery rail |
| `STAT_PWG1_SPANNUNG_WERT` | mV | **accelerator-pedal sensor** |
| `STAT_SOLLWERT_GENERATORSPANNUNG_WERT` | V | alternator setpoint |

Binding "battery voltage" to the accelerator pedal would be worse than not checking at all — a `BatteryAbove(12.0)` gate reading a pedal at ~800 mV either refuses everything or, normalised wrongly, passes everything. **Therefore: curated allowlist only, never pattern matching.**

## Verified facts this plan is built on (queried against the real catalog)

- `STAT_UBATT_WERT` is **volts on 33 variants and millivolts on 28** — the same name, different units. Unit normalisation is mandatory, and must come from the catalog's `unit` column, not the name.
- A few rows carry an **empty/NULL unit** (e.g. one `STAT_MOTORDREHZAHL_WERT`, one `STAT_GESCHWINDIGKEIT_WERT`). Those are NOT normalisable and must be treated as unresolvable.
- Coverage of a curated list is 65–107 of 1,281 variants per quantity. That is expected and correct: these are *engine* measurements and most variants are body modules. Preconditions read the vehicle via the **engine ECU**, so engine-variant coverage is what matters.
- `TerminalOn` has **no measurement source on `d72n47a0` at all** — see Task 3.

## File Structure

| File | Responsibility | Change |
|---|---|---|
| `crates/semantic/src/quantity.rs` | `Quantity` ↔ curated measurement names, unit normalisation | Create |
| `crates/semantic/src/lib.rs` | Re-exports | Modify |
| `crates/semantic/src/catalog.rs` | `Catalog::resolve_quantity` | Modify |
| `crates/service/src/runner.rs` | `ServiceReport.function_id`; `Teardown::NotAttempted` | Modify |
| `crates/service/src/precondition.rs` | `Quantity` re-exported from semantic instead of defined locally | Modify |

---

### Task 1: Curated quantity → measurement mapping with unit normalisation

**Files:**
- Create: `crates/semantic/src/quantity.rs`
- Modify: `crates/semantic/src/lib.rs`

**Interfaces:**
- Produces: `Quantity::{EngineSpeed, BatteryVoltage, CoolantTemp, RoadSpeed}`; `Quantity::candidates(self) -> &'static [&'static str]`; `Quantity::unit_factor(self, catalog_unit: Option<&str>) -> Option<f64>`; `Quantity::canonical_unit(self) -> &'static str`

- [ ] **Step 1: Write the failing tests**

Create `crates/semantic/src/quantity.rs` containing only:

```rust
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
        assert_eq!(Quantity::BatteryVoltage.unit_factor(Some("mV")), Some(0.001));
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
}
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p klartext-semantic quantity:: 2>&1 | tail -15`
Expected: FAIL — `cannot find type 'Quantity' in this scope`

- [ ] **Step 3: Implement**

Prepend to `crates/semantic/src/quantity.rs`:

```rust
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
```

Add to `crates/semantic/src/lib.rs`: `pub mod quantity;` and `pub use quantity::Quantity;`

- [ ] **Step 4: Run the tests**

Run: `cargo test -p klartext-semantic quantity:: 2>&1 | tail -12`
Expected: PASS, 4 tests.

- [ ] **Step 5: Gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings ; echo "clippy rc=$?"
git add crates/semantic/src/quantity.rs crates/semantic/src/lib.rs
git commit -m "feat(semantic): map physical quantities to curated measurements"
```

---

### Task 1b: Extract ISTA's result titles into the measurement catalog

**Files:**
- Modify: `scripts/build-semantic-db.sh`
- Modify: `crates/semantic/src/catalog.rs` (`MeasurementCatalogEntry` gains `title`)

**Why:** the titles are the fleet-wide semantic key (see the amendment above) and we already traverse
the exact rows that carry them — the extraction simply never took the column.

- [ ] **Step 1: Add the columns to the extraction**

In `scripts/build-semantic-db.sh`, the `CREATE TABLE sem.measurement AS SELECT …` already selects
from `XEP_ECURESULTS r`. Add two columns to that SELECT list, beside `r.NAME`:

```sql
         NULLIF(r.TITLE_ENGB, '')                  AS title_en,
         NULLIF(r.TITLE_DEDE, '')                  AS title_de,
```

- [ ] **Step 2: Rebuild and verify against the real catalog**

```bash
scripts/build-semantic-db.sh
sqlite3 data/klartext-semantic.db "SELECT name, unit, title_en FROM measurement WHERE ecu_variant='d72n47a0' AND name IN ('STAT_UBATT_WERT','STAT_PWG1_SPANNUNG_WERT','STAT_MOTORDREHZAHL_WERT');"
```
Expected — record the real output in your report:
`STAT_UBATT_WERT|V|104 Battery voltage`, `STAT_PWG1_SPANNUNG_WERT|mV|907 Accelerator pedal…`,
`STAT_MOTORDREHZAHL_WERT|1/min|101 Engine speed`.

- [ ] **Step 3: Surface it on the API**

Add to `MeasurementCatalogEntry` in `crates/semantic/src/catalog.rs`:

```rust
    /// ISTA's own human title, e.g. `104 Battery voltage` — the fleet-wide
    /// semantic key. Present on every ISTA result; prefer it over the EDIABAS
    /// name, whose spelling varies per ECU.
    pub title: Option<String>,
```
Select `title_en` (falling back to `title_de`) in `Catalog::measurements`, and add it to the
synthetic fixture rows. **Guard for older extracts:** a pre-title DB has no such column, so use the
existing `has_column`-style check the file already uses for backward compatibility (see how the v2
title columns on `ecu` are handled) and degrade to `None` rather than erroring.

- [ ] **Step 4: Gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings ; echo "clippy rc=$?"
cargo test -p klartext-semantic ; echo "rc=$?"
git add scripts/build-semantic-db.sh crates/semantic/src/catalog.rs
git commit -m "feat(semantic): extract ISTA's result titles, the fleet-wide semantic key"
```

---

### Task 2: `Catalog::resolve_quantity`

**Files:**
- Modify: `crates/semantic/src/catalog.rs`

**Interfaces:**
- Produces: `ResolvedQuantity { name: String, factor: f64 }`; `Catalog::resolve_quantity(&self, variant: &str, quantity: Quantity) -> Result<Option<ResolvedQuantity>, SemanticError>`

- [ ] **Step 1: Write the failing test**

Add to `crates/semantic/src/catalog.rs`'s test module. The `fixture()` DB already has a `measurement` table; extend the fixture insert block with these rows (synthetic, no BMW data):

```rust
                 INSERT INTO measurement VALUES ('eng_v','STAT_UBATT_WERT','V',1.0,0.0,1,NULL,'STATUS_LESEN');
                 INSERT INTO measurement VALUES ('eng_mv','STAT_UBATT_WERT','mV',1.0,0.0,0,NULL,'STATUS_LESEN');
                 INSERT INTO measurement VALUES ('eng_nounit','STAT_UBATT_WERT',NULL,1.0,0.0,0,NULL,'STATUS_LESEN');
                 INSERT INTO measurement VALUES ('eng_pedal','STAT_PWG1_SPANNUNG_WERT','mV',1.0,0.0,0,NULL,'STATUS_LESEN');
```

then add:

```rust
    #[test]
    fn resolve_quantity_normalises_units_and_refuses_to_guess() {
        let (_dir, path) = fixture();
        let cat = Catalog::open(&path).unwrap();

        // Volts resolve 1:1.
        let v = cat
            .resolve_quantity("eng_v", Quantity::BatteryVoltage)
            .unwrap()
            .expect("a volts variant resolves");
        assert_eq!(v.name, "STAT_UBATT_WERT");
        assert_eq!(v.factor, 1.0);

        // The SAME name in millivolts must normalise, or a 12.0 V floor would
        // pass at 12 mV and a flat battery would sail through.
        let mv = cat
            .resolve_quantity("eng_mv", Quantity::BatteryVoltage)
            .unwrap()
            .expect("a millivolts variant resolves");
        assert_eq!(mv.factor, 0.001);
        assert!((11_800.0 * mv.factor) < 12.0, "a flat battery must fail a 12 V floor");

        // No unit -> unresolvable, NOT assumed.
        assert!(
            cat.resolve_quantity("eng_nounit", Quantity::BatteryVoltage)
                .unwrap()
                .is_none()
        );

        // A variant carrying ONLY the pedal sensor must not resolve battery
        // voltage — the curated list is what prevents that binding.
        assert!(
            cat.resolve_quantity("eng_pedal", Quantity::BatteryVoltage)
                .unwrap()
                .is_none()
        );

        // An unknown variant resolves to nothing rather than erroring.
        assert!(
            cat.resolve_quantity("nope", Quantity::BatteryVoltage)
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn resolve_quantity_degrades_to_none_without_the_table() {
        // A pre-v4 extract has no `measurement` table; that must not error.
        let (_dir, path) = fixture_opts(false);
        let cat = Catalog::open(&path).unwrap();
        assert!(
            cat.resolve_quantity("eng_v", Quantity::BatteryVoltage)
                .unwrap()
                .is_none()
        );
    }
```

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p klartext-semantic resolve_quantity 2>&1 | tail -15`
Expected: FAIL — `no method named 'resolve_quantity'`

- [ ] **Step 3: Implement**

Add near `MeasurementCatalogEntry` in `crates/semantic/src/catalog.rs`:

```rust
/// A quantity resolved to a concrete measurement on one ECU variant.
#[derive(Debug, Clone, PartialEq)]
pub struct ResolvedQuantity {
    /// The EDIABAS result name to read.
    pub name: String,
    /// Multiply the read value by this to get [`Quantity::canonical_unit`].
    pub factor: f64,
}
```

Add to `impl Catalog`:

```rust
    /// Resolve `quantity` to a measurement on `variant`, normalised to its
    /// canonical unit.
    ///
    /// Matches ISTA's OWN title (`Quantity::ista_title`, e.g. "Battery voltage")
    /// against the variant's measurements, constrained by a normalisable unit.
    /// Titles are present on all 136,885 ISTA results, so this generalises to any
    /// car; EDIABAS names do not, and name matching would bind "battery voltage"
    /// to the accelerator-pedal sensor on a real DDE.
    ///
    /// A title may be numbered ("104 Battery voltage") or qualified
    /// ("Battery voltage, IBS"); match the canonical label as a whole word,
    /// preferring an exact/unqualified match over a qualified one, and NEVER
    /// accept a title that merely contains the words in another sense
    /// ("Accelerator pedal, hall effect sensor 1: Voltage" must not match).
    ///
    /// Returns `None` — never a guess — when no title matches, when the matches
    /// carry an absent or unrecognised unit, when the result is AMBIGUOUS (two
    /// different measurements match equally well), or when the extract predates
    /// the `measurement` table. A caller must treat `None` as "cannot check" and
    /// degrade to advisory rather than binding to a plausible-looking wrong sensor.
    ///
    /// # Errors
    /// Returns [`SemanticError::Query`] if the lookup query fails.
    pub fn resolve_quantity(
        &self,
        variant: &str,
        quantity: Quantity,
    ) -> Result<Option<ResolvedQuantity>, SemanticError> {
        if !self.has_table("measurement")? {
            return Ok(None);
        }
        let mut stmt = self
            .conn
            .prepare("SELECT unit FROM measurement WHERE ecu_variant = ?1 AND name = ?2")?;
        for candidate in quantity.candidates() {
            let units: Vec<Option<String>> = stmt
                .query_map(rusqlite::params![variant, candidate], |row| row.get(0))?
                .collect::<rusqlite::Result<_>>()?;
            for unit in units {
                if let Some(factor) = quantity.unit_factor(unit.as_deref()) {
                    return Ok(Some(ResolvedQuantity {
                        name: (*candidate).to_string(),
                        factor,
                    }));
                }
            }
        }
        Ok(None)
    }
```

Add `Quantity` to the file's imports (`use crate::quantity::Quantity;`) and export `ResolvedQuantity` from `lib.rs`.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p klartext-semantic 2>&1 | tail -12`
Expected: PASS.

- [ ] **Step 5: Sanity-check against the REAL catalog (informational, not a test)**

```bash
cargo test -p klartext-semantic ; echo "rc=$?"
sqlite3 data/klartext-semantic.db "SELECT unit FROM measurement WHERE ecu_variant='d72n47a0' AND name='STAT_UBATT_WERT';"
```
Expected: the DDE reports `V`, so `resolve_quantity("d72n47a0", BatteryVoltage)` would yield factor `1.0`. Record the observed value in your report. Do NOT add a test that depends on the real DB — it is BYO-data and absent in CI.

- [ ] **Step 6: Gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings ; echo "clippy rc=$?"
git add crates/semantic/src/catalog.rs crates/semantic/src/lib.rs
git commit -m "feat(semantic): resolve a quantity to a measurement, refusing to guess"
```

---

### Task 3: Adopt the shared `Quantity`, and two verification fixes

**Files:**
- Modify: `crates/service/src/precondition.rs`, `crates/service/src/runner.rs`, `crates/service/Cargo.toml` (only if a dependency is genuinely missing — via `cargo add`, never hand-edited)

**Interfaces:**
- Changes: `klartext_service::Quantity` becomes a re-export of `klartext_semantic::Quantity` (one definition, not two)
- Produces: `ServiceReport.function_id: i64`; `Teardown::NotAttempted`

- [ ] **Step 1: Write the failing tests**

Add to `crates/service/src/runner.rs`'s test module:

```rust
    #[tokio::test]
    async fn a_blocked_report_names_the_function_id_it_refused() {
        // The surfaces must echo WHICH function was refused; a human confirming an
        // actuation needs it, and after the multi-function fix the job name alone
        // is ambiguous.
        let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: None };
        let reader = TableReader(vec![(Quantity::BatteryVoltage, 10.0)]);
        let report = run_service(
            &spy, &reader, "STEUERN_X", 0x12, 7,
            klartext_semantic::Category::ActuatorControl,
            &[inv_for(7, Phase::Main, "GO")],
        )
        .await;
        assert!(report.blocked);
        assert_eq!(report.function_id, 7);
        // A refusal is NOT the same as "this function defines no teardown".
        assert_eq!(report.teardown, Teardown::NotAttempted);
    }

    #[tokio::test]
    async fn not_defined_and_not_attempted_are_different_states() {
        // NotDefined is a fact about the catalog (2,733 functions DO define a Reset
        // phase); NotAttempted is a fact about this run. Conflating them makes a
        // refused actuation state something false about the car.
        let spy = SpyEcu { ran: Mutex::new(Vec::new()), fail_on: None };
        let reader = TableReader(vec![(Quantity::BatteryVoltage, 13.0)]);
        let report = run_service(
            &spy, &reader, "STEUERN_X", 0x12, 7,
            klartext_semantic::Category::CbsReset,
            &[inv_for(7, Phase::Main, "GO")],
        )
        .await;
        assert!(!report.blocked);
        assert_eq!(report.teardown, Teardown::NotDefined);
    }
```

Add whatever small test helper the existing module needs (`inv_for(function_id, phase, arg)`) alongside the existing `inv`.

- [ ] **Step 2: Run to see it fail**

Run: `cargo test -p klartext-service 2>&1 | tail -15`
Expected: FAIL — no `function_id` field / no `NotAttempted` variant.

- [ ] **Step 3: Implement**

1. In `crates/service/src/precondition.rs`, DELETE the locally-defined `Quantity` enum and instead `pub use klartext_semantic::Quantity;`. Keep `Precondition::quantity()` returning it. One definition only — a second copy would let the crates disagree about units, which is the whole failure mode this design prevents.
2. In `crates/service/src/runner.rs`, add to `ServiceReport`:
```rust
    /// The ISTA function this report is about — the surfaces echo it so a human
    /// can see WHICH function ran or was refused.
    pub function_id: i64,
```
Populate it on both the executed and blocked paths from `run_service`/`run_cycle`'s `function_id` parameter.
3. Add to `Teardown`:
```rust
    /// The cycle was refused before anything ran, so no teardown was attempted.
    /// Distinct from [`Teardown::NotDefined`], which is a fact about the catalog.
    NotAttempted,
```
Use it on the blocked path in `run_service`. Handle the new variant everywhere `Teardown` is matched.

- [ ] **Step 4: Run the full workspace**

```bash
cargo test --workspace ; echo "test rc=$?"
```
Expected: rc=0, everything green.

- [ ] **Step 5: Gates and commit**

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings ; echo "clippy rc=$?"
git add crates/service
git commit -m "feat(service): share the semantic Quantity, report function_id and NotAttempted"
```

---

## Open decision for the NEXT plan (do not resolve here)

`Precondition::TerminalOn` has **no measurement source on the owner's DDE**, yet it is the check present in *every* category's defaults — so for the three reset categories it is currently their ONLY check, and it will always resolve advisory. Options for the surface plan: derive it from the engine ECU answering at all, source it from another ECU, or drop it and let those categories carry no gate explicitly rather than implicitly. **Flag it to the owner; do not silently pick one.**

## Out of scope (the surface plan)

The MCP `run_service_function` tool, extended `list_service_functions`, `watch` sampling, the CLI rewire, and the binaries' `MeasurementReader`/`JobRunner` implementations that consume `resolve_quantity`.
