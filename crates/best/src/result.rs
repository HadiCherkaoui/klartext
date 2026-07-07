//! EDIABAS job results: named, typed values grouped into ordered sets.
//!
//! An EDIABAS job emits its output as one or more *result sets*, each an
//! ordered list of `name -> value` pairs (a per-cylinder job, for example,
//! yields one set per cylinder). [`ResultSet`] is the container the executor
//! (a later task) fills: its result-store opcodes (`ergb`/`ergw`/...) call
//! [`ResultSet::push_named`] to append to the current set, and `enewset` calls
//! [`ResultSet::new_set`] to start the next one.
//!
//! ## Current-set convention
//! The last set in the list is the *current* set. [`ResultSet::new`] seeds one
//! empty set, so a job that never calls `enewset` still accumulates into set 0.
//! [`ResultSet::push_named`] appends to the current set, [`ResultSet::new_set`]
//! commits it and starts a fresh one, and both [`ResultSet::get`] and
//! [`ResultSet::iter_current`] read the current set only. Names may repeat
//! across sets and insertion order is significant, so a set is a `Vec` of
//! pairs, not a map.
//!
//! ## Value types
//! EDIABAS distinguishes eleven result types (`EdiabasNet.cs` `ResultType`);
//! [`ResultData`] collapses them into seven Rust-native variants:
//!
//! | EDIABAS type   | Variant                | Rust type |
//! |----------------|------------------------|-----------|
//! | `B`, `C`       | [`ResultData::Byte`]   | `u8`      |
//! | `W`            | [`ResultData::Word`]   | `u16`     |
//! | `D`            | [`ResultData::Dword`]  | `u32`     |
//! | `I`, `L`, `LL` | [`ResultData::Int`]    | `i64`     |
//! | `R`            | [`ResultData::Real`]   | `f64`     |
//! | `S`            | [`ResultData::Text`]   | `String`  |
//! | `Y`            | [`ResultData::Binary`] | `Vec<u8>` |
//!
//! Signed integers widen into `i64`; the 64-bit unsigned `Q` has no store
//! opcode in the executed subset. The store opcode picks the variant, so this
//! model performs no scaling of its own — that stays the caller's job.

/// Message for the broken-invariant panic in the current-set accessors.
const CURRENT_SET_INVARIANT: &str = "ResultSet always holds at least one (current) set";

/// A single named result value in EDIABAS's Rust-native representation.
///
/// EDIABAS's eleven result types collapse into these seven variants; each
/// variant's docs name the EDIABAS type(s) it carries.
#[derive(Debug, Clone, PartialEq)]
pub enum ResultData {
    /// An unsigned 8-bit result (EDIABAS `B`, or a `C` character's byte).
    Byte(u8),
    /// An unsigned 16-bit result (EDIABAS `W`).
    Word(u16),
    /// An unsigned 32-bit result (EDIABAS `D`).
    Dword(u32),
    /// A signed integer result widened to 64-bit (EDIABAS `I`/`L`/`LL`).
    Int(i64),
    /// A floating-point result (EDIABAS `R`).
    Real(f64),
    /// A text result (EDIABAS `S`).
    Text(String),
    /// A raw byte-array result (EDIABAS `Y`).
    Binary(Vec<u8>),
}

/// A job's result sets: one or more ordered lists of name-value pairs.
///
/// The last set is the current one. [`push_named`](Self::push_named) and
/// [`new_set`](Self::new_set) write it; [`get`](Self::get) and
/// [`iter_current`](Self::iter_current) read the current set only.
#[derive(Debug, Clone, PartialEq)]
pub struct ResultSet {
    /// Each entry is one set; the last entry is the current set. Never empty:
    /// `new`/`new_set` uphold that at least one set is always present.
    sets: Vec<Vec<(String, ResultData)>>,
}

impl ResultSet {
    /// Creates a result set holding one empty current set, ready for pushes.
    ///
    /// The seed set is EDIABAS set 0: a job that never starts a new set still
    /// has somewhere to accumulate its results.
    pub fn new() -> Self {
        Self {
            sets: vec![Vec::new()],
        }
    }

    /// Appends a `name`-`value` pair to the current (last) set.
    pub fn push_named(&mut self, name: &str, value: ResultData) {
        self.current_mut().push((name.to_string(), value));
    }

    /// Commits the current set and starts a fresh empty one (EDIABAS `enewset`).
    ///
    /// Later pushes land in the new set; the committed one stays readable only
    /// by re-reading order, not through [`get`](Self::get) or
    /// [`iter_current`](Self::iter_current), which see the current set only.
    pub fn new_set(&mut self) {
        self.sets.push(Vec::new());
    }

    /// Iterates the current set's name-value pairs in insertion order.
    pub fn iter_current(&self) -> impl Iterator<Item = (&str, &ResultData)> {
        self.current()
            .iter()
            .map(|(name, value)| (name.as_str(), value))
    }

    /// Looks up `name` in the current set, returning the first match.
    ///
    /// EDIABAS result names are unique within a set; if one repeats, the
    /// earliest-inserted value wins.
    pub fn get(&self, name: &str) -> Option<&ResultData> {
        self.current()
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, value)| value)
    }

    /// The number of result sets (EDIABAS job result-set count).
    pub fn sets_len(&self) -> usize {
        self.sets.len()
    }

    /// Iterates every result set in order, each yielding its name/value pairs.
    ///
    /// Unlike [`iter_current`](Self::iter_current) (the last set only), this surfaces
    /// a multi-set job's full output (e.g. one set per cylinder).
    pub fn iter_sets(&self) -> impl Iterator<Item = impl Iterator<Item = (&str, &ResultData)>> {
        self.sets
            .iter()
            .map(|set| set.iter().map(|(n, v)| (n.as_str(), v)))
    }

    /// The current (last) set; `new`/`new_set` uphold that one always exists.
    fn current(&self) -> &[(String, ResultData)] {
        self.sets.last().expect(CURRENT_SET_INVARIANT)
    }

    /// The current (last) set, mutably; see [`current`](Self::current).
    fn current_mut(&mut self) -> &mut Vec<(String, ResultData)> {
        self.sets.last_mut().expect(CURRENT_SET_INVARIANT)
    }
}

impl Default for ResultSet {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_results_store_and_read_back() {
        let mut rs = ResultSet::new();
        rs.push_named("STAT_MOTORTEMPERATUR_WERT", ResultData::Real(89.96));
        rs.push_named("STAT_MOTORTEMPERATUR_EINH", ResultData::Text("degC".into()));
        assert!(
            matches!(rs.get("STAT_MOTORTEMPERATUR_WERT"), Some(ResultData::Real(v)) if (*v-89.96).abs()<1e-9)
        );
        assert!(
            matches!(rs.get("STAT_MOTORTEMPERATUR_EINH"), Some(ResultData::Text(t)) if t=="degC")
        );
    }

    #[test]
    fn new_set_isolates_the_current_set() {
        let mut rs = ResultSet::new();
        rs.push_named("BEFORE", ResultData::Byte(1));
        rs.new_set();
        rs.push_named("AFTER", ResultData::Byte(2));

        // `get` sees the current (second) set only: AFTER is visible, BEFORE is
        // not, even though BEFORE is still stored in the committed first set.
        assert_eq!(rs.get("AFTER"), Some(&ResultData::Byte(2)));
        assert_eq!(rs.get("BEFORE"), None);
    }

    #[test]
    fn iter_current_preserves_insertion_order() {
        let mut rs = ResultSet::new();
        rs.push_named("FIRST", ResultData::Int(10));
        rs.push_named("SECOND", ResultData::Int(20));
        rs.push_named("THIRD", ResultData::Int(30));

        let pairs: Vec<(&str, &ResultData)> = rs.iter_current().collect();
        assert_eq!(
            pairs,
            vec![
                ("FIRST", &ResultData::Int(10)),
                ("SECOND", &ResultData::Int(20)),
                ("THIRD", &ResultData::Int(30)),
            ]
        );
    }

    #[test]
    fn iter_current_reads_current_set_only() {
        let mut rs = ResultSet::new();
        rs.push_named("OLD", ResultData::Byte(1));
        rs.new_set();
        rs.push_named("NEW", ResultData::Byte(2));

        let names: Vec<&str> = rs.iter_current().map(|(name, _)| name).collect();
        assert_eq!(names, vec!["NEW"]);
    }

    #[test]
    fn each_variant_roundtrips_through_get() {
        let mut rs = ResultSet::new();
        rs.push_named("B", ResultData::Byte(0xAB));
        rs.push_named("W", ResultData::Word(0xABCD));
        rs.push_named("D", ResultData::Dword(0xDEAD_BEEF));
        rs.push_named("I", ResultData::Int(-42));
        rs.push_named("R", ResultData::Real(-12.5));
        rs.push_named("S", ResultData::Text("hello".into()));
        rs.push_named("Y", ResultData::Binary(vec![1, 2, 3]));

        assert_eq!(rs.get("B"), Some(&ResultData::Byte(0xAB)));
        assert_eq!(rs.get("W"), Some(&ResultData::Word(0xABCD)));
        assert_eq!(rs.get("D"), Some(&ResultData::Dword(0xDEAD_BEEF)));
        assert_eq!(rs.get("I"), Some(&ResultData::Int(-42)));
        assert_eq!(rs.get("R"), Some(&ResultData::Real(-12.5)));
        assert_eq!(rs.get("S"), Some(&ResultData::Text("hello".into())));
        assert_eq!(rs.get("Y"), Some(&ResultData::Binary(vec![1, 2, 3])));
    }

    #[test]
    fn get_returns_first_match_on_repeated_name() {
        // Documented behavior: within a set the earliest-inserted value wins.
        let mut rs = ResultSet::new();
        rs.push_named("DUP", ResultData::Byte(1));
        rs.push_named("DUP", ResultData::Byte(2));
        assert_eq!(rs.get("DUP"), Some(&ResultData::Byte(1)));
    }

    #[test]
    fn missing_name_returns_none() {
        let rs = ResultSet::new();
        assert_eq!(rs.get("NOPE"), None);
    }

    #[test]
    fn default_matches_new() {
        assert_eq!(ResultSet::default(), ResultSet::new());
    }

    #[test]
    fn iter_sets_exposes_every_set_in_order() {
        let mut rs = ResultSet::new();
        rs.push_named("A", ResultData::Byte(1));
        rs.new_set();
        rs.push_named("B", ResultData::Byte(2));
        rs.push_named("C", ResultData::Byte(3));
        assert_eq!(rs.sets_len(), 2);
        let collected: Vec<Vec<(&str, &ResultData)>> =
            rs.iter_sets().map(|s| s.collect()).collect();
        assert_eq!(collected[0], vec![("A", &ResultData::Byte(1))]);
        assert_eq!(
            collected[1],
            vec![("B", &ResultData::Byte(2)), ("C", &ResultData::Byte(3))]
        );
    }
}
