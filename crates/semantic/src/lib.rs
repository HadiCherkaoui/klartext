//! Semantic layer for klartext — turns raw diagnostics into meaning.
//!
//! This crate sits above the UDS/transport layers and answers the "what does
//! this actually mean?" questions, sourced from the user's own ISTA SQLiteDB:
//!
//! - [`dtc`] — pure helpers to bridge a raw 3-byte UDS DTC to the ISTA code
//!   number and to decode the 1-byte status into ISO 14229 flags. No DB needed.
//! - [`did`] — pure naming of the ISO-standard identification DIDs (0xF1xx) from
//!   the protocol report, plus a raw-value renderer. BMW-specific DID scaling is
//!   not in the SQLiteDB (it lives in the EDIABAS SGBD), so this stays "name +
//!   raw" — see `docs/sqlite-findings.md`.
//! - [`catalog`] — the DB-backed lookups: a raw DTC at a given ECU address maps
//!   to a human fault description. Opens the ISTA-derived SQLiteDB **read-only**
//!   at a configurable path and never embeds or copies its contents.
//! - [`service_function`] — the SGBD-backed *control* catalog (resets, adaptations,
//!   actuations, calibrations), each tagged by category, blast-radius risk, and a
//!   derivation status (is an offline-derived — but unconfirmed — execution frame
//!   available, or not). The CLI gates execution by risk; MCP only ever lists it.

pub mod catalog;
pub mod did;
pub mod dtc;
pub mod identity;
pub mod measurement;
pub mod pid;
pub mod service_function;
pub mod snapshot;

pub use catalog::{
    Catalog, DtcDescription, EcuSlot, EcuTreeEntry, EnvCondLabel, FaultDoc, JobParameterEntry,
    MeasurementCatalogEntry, SemanticError, VariantInfo, bordnet_series_for,
};
pub use identity::{NamedEcu, VehicleOrder, decode_vehicle_order, name_ecu_list};
pub use klartext_sgbd::SgbdError;
pub use measurement::{
    DYNAMIC_DID, DataType, Measurement, Measurements, ScaledMeasurement, build_read_request,
    fold_for_match, misrouted_dynamic_measurement,
};
pub use service_function::{
    CBS_DID, Category, Derivation, Risk, ServiceFunction, ServiceFunctions, build_cbs_read_request,
    build_cbs_reset_request,
};
pub use snapshot::{
    DecodedExtData, DecodedSnapshot, ExtDataDefs, ExtDataField, FreezeFrameDefs, SnapshotDefs,
    SnapshotField,
};
