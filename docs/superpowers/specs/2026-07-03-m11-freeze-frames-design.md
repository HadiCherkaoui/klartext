# M11 Item 1 — Freeze-frame / snapshot metadata on faults (design)

**Date:** 2026-07-03 · **Status:** design approved; implementation not started.
**Roadmap parent:** `docs/superpowers/specs/2026-07-03-m11-ista-parity-roadmap.md` item 1.
**Owner priority:** HIGH — "extremely useful." Today a fault is code + 1-byte status only;
this adds the environmental metadata captured when the fault latched (km, RPM, temps, ECU
state, occurrence/healing counters, severity).

## 1. Goal & scope

Add ISTA-parity fault detail: on demand, read and decode a single fault's freeze-frame
(snapshot), extended data, and severity, and surface it through the CLI and MCP.

**In scope (all three UDS reads ISTA's `FS_LESEN_DETAIL` emits):**
- `19 04` reportDTCSnapshotRecordByDTCNumber — the freeze-frame (km, time, RPM, temps, ECU
  state at the moment the fault latched).
- `19 06` reportDTCExtendedDataRecordByDTCNumber — occurrence/frequency counter, healing
  counter, condition byte.
- `19 09` reportSeverityInformationOfDTC — severity / fault class.

**Out of scope (unchanged from the roadmap):** no fault-filter change (`RELEVANT_MASK`
stays 0xAF — the current fault-memory read already matches what the owner wants); no BEST2
interpreter; no writes; no SVT / identification dump (Item 2); ECUs other than the DDE
degrade to raw until their SGBD tables are added. DDE-first (variant `d72n47a0`).

## 2. How ISTA reads a fault's detail (established by decompile + disassembly)

Confirmed offline against the decrypted `DiagDocDb`, the ISTA .NET DLLs (`ilspycmd`), and
the DDE SGBD `d72n47a0.prg` (`ediabasx` as an offline oracle — never shipped). Facts and
protocol only; no ISTA source copied (AGPL / CLAUDE.md).

- **Two-stage model, not auto-fetch.** ISTA's whole-memory scan (`FS_LESEN`) reads only the
  `19 02` code+status list. The freeze-frame detail is a **separate per-fault job**
  (`FS_LESEN_DETAIL`, arg `F_CODE` = one chosen DTC) that emits `19 04` + `19 06` + `19 09`.
  So "what ISTA does" == a lean scan + on-demand per-fault detail. This is the fetch policy
  below.
- **Requests are byte-certain** (read directly from the SGBD bytecode `move S1,{…}` literals;
  the 3 DTC bytes are patched big-endian from `F_CODE`, the trailing record byte is `0xFF`):

  | UDS | Bytes | Meaning |
  |---|---|---|
  | snapshot | `19 04 DD DD DD FF` | DTC (hi,mid,lo) big-endian; record `0xFF` = **all records** |
  | extended | `19 06 DD DD DD FF` | record `0xFF` = **all records** |
  | severity | `19 09 DD DD DD` | no record byte |

- **Decode is table-driven — no interpreter needed.** Per-field width, scaling, and unit
  come from three tables in the SGBD `.prg` (the same XOR-`0xF7` / `0x84`-directory /
  `0x50`-byte-record format `klartext-sgbd` already parses for `SG_FUNKTIONEN`). The
  decrypted DB adds localized (English) field names. Verified present in `d72n47a0.prg`.

## 3. Wire framing

Legend: **[ISO]** standard ISO 14229-1 · **[BMW]** BMW/DDE-specific, confirmed offline ·
**[CAP]** structure derived from ISO + disassembly, **byte offsets need one on-car capture.**

### 3.1 Requests — certain
```
19 04 <dtc_hi> <dtc_mid> <dtc_lo> FF      [ISO] snapshot,  record 0xFF = all
19 06 <dtc_hi> <dtc_mid> <dtc_lo> FF      [ISO] extended,  record 0xFF = all
19 09 <dtc_hi> <dtc_mid> <dtc_lo>         [ISO] severity,  no record byte
```

### 3.2 `59 04` snapshot response — [CAP] on preamble offsets
```
59 04
DD DD DD          [ISO] echoed DTC (3 bytes)
ST                [ISO] statusOfDTC (1 byte; the 8 flags from protocol-reference §1.5)
  ── per snapshot record: ──
RN                [ISO] DTCSnapshotRecordNumber (1 byte)
NI                [ISO] numberOfIdentifiers in this record (1 byte)
  ── NI times: ──
II II             [BMW] 2-byte identifier = UWNR (e.g. 0x5205 coolant, 0x5955 RPM)
<data>            [BMW] width from the SGBD definition — NO length byte precedes it
```
There is **no per-field length on the wire**; width is definition-driven. Multi-byte values
big-endian **[CAP]**.

### 3.3 `59 06` extended-data response — [CAP] on preamble offsets
```
59 06
DD DD DD          [ISO] DTC (3 bytes)
ST                [ISO] statusOfDTC (1 byte)
  ── per extended record: ──
RR                [ISO] extendedDataRecordNumber (1 byte: 0x01/0x02/0x03)
<data>            [BMW] length = DTCExtendedDataRecordNumber.ANZ_BYTE (1 byte each on this DDE)
```

### 3.4 `59 09` severity response — fixed layout, parse the certain part
Standard ISO layout: `59 09 [DTCStatusAvailabilityMask:1] [DTCSeverity:1]
[DTCFunctionalUnit:1] [DTC:3] [statusOfDTC:1]` (8 bytes after the SID). Parse the
severity / functional-unit / status and mark the rest **[CAP]**. Note the decode
depends on the leading `DTCStatusAvailabilityMask` byte being present — if a real BMW
`59 09` omits it, every field shifts by one; confirm on the capture.

**Negative response** `7F 19 <NRC>` is normal — a fault with no stored freeze-frame answers
negatively (e.g. `requestOutOfRange`). Treat as "no detail," not an error (see §6).

## 4. Decode definitions (SGBD tables + DB labels)

### 4.1 SGBD tables in `d72n47a0.prg` (verified present)

**`FUMWELTTEXTE`** (484 rows) — the numeric decode spec. Columns:
`UWNR, UWTEXT, UW_EINH, L/H, UWTYP, NAME, MUL, DIV, ADD`. Decode a field as
`value = raw × MUL / DIV + ADD` over `UWTYP`-width big-endian bytes, unit = `UW_EINH`. Same
math as `SG_FUNKTIONEN` (M6) — reuse `measurement::DataType` + `measurement::scale`.
Representative rows (facts, small sample — same quantity the M6 doc quotes):

| UWNR | meaning | unit | UWTYP | MUL | DIV | ADD |
|---|---|---|---|---|---|---|
| 0x4202 | battery voltage | mV | unsigned char | 100.0 | 1 | 0 |
| 0x5205 | coolant temperature | degC | unsigned char | 1.0 | 1 | −40.0 |
| 0x5955 | engine speed | rpm | unsigned int | 0.5 | 1 | 0 |
| 0x474A | rail pressure (max 10 ms) | bar | unsigned char | 7.843137 | 1 | 0 |
| 0x45EC | current engine status | — (enum) | unsigned char | 1.0 | 1 | 0 |

**`DTCSnapshotIdentifier`** (7 rows) — the standard/obligatory identifiers. Width is encoded
as a hex **mask** (count the `0xFF` bytes), which doubles as the "not available" sentinel:

| UWNR | name | width (mask) | sentinel = N/A |
|---|---|---|---|
| 0x1700 | KM_STAND (mileage) | 3 bytes (`0xFFFFFF`) | `0xFFFFFF` |
| 0x1701 | ABS_ZEIT (timestamp, s) | 4 bytes (`0xFFFFFFFF`) | `0xFFFFFFFF` |
| 0x1702 | SAE_CODE | 3 bytes | — |
| 0x1731 | Fehlerklasse_DTC | 1 byte | — |
| 0x1750 | PWF_Basisnetz | 1 byte | — |
| 0x1751 | PWF_Teilnetz | 3 bytes | — |

**`DTCExtendedDataRecordNumber`** — record-number → byte length for `19 06`:

| record | name | ANZ_BYTE |
|---|---|---|
| 0x01 | CONDITION_BYTE | 1 |
| 0x02 | HFK (frequency / occurrence counter) | 1 |
| 0x03 | HLZ (healing counter) | 1 |
| 0xFF | RECORD_UNKNOWN | 0 |

**Width sources are two:** `FUMWELTTEXTE.UWTYP` → the `DataType` enum (`unsigned char`=1,
`unsigned int`=2, `unsigned long`/`float`=4); `DTCSnapshotIdentifier.UWTYP` → a byte mask
whose length is the width. The decoder derives a plain `usize` width from either. KM/time
scale as identity integers with the sentinel → "not available."

### 4.2 DB labels (English names) — extend the extract

`XEP_ENVCONDSLABELS` (22,432 rows) holds localized env-condition labels keyed by `UWIDENT`
(decimal) which equals `FUMWELTTEXTE.UWNR` (hex). `TYPE` is empty (no scaling in the DB —
scaling is SGBD-only), so the DB is used **only for names/units/kind**. `NODECLASS 5658114`
marks a status/enum field (render as state text), `5656962` a numeric measurement.

Extend `scripts/build-semantic-db.sh` with an `envcond` table:
`SELECT DISTINCT UWIDENT, UNIT, TITLE_ENGB, TITLE_DEDE, TYPE, NODECLASS FROM XEP_ENVCONDSLABELS`
(pick one row per `UWIDENT` that has a title). `klartext-semantic.db` gains this table;
`Catalog::envcond_label(uwnr)` reads it.

> The per-fault field set/order (`XEP_FAULTCODES → XEP_REFFAULTLABELS → XEP_ENVCONDSLABELS`)
> is **not needed for decode** — the wire response tells us which UWNRs are present, and we
> label each globally. The per-fault join is a future enrichment (validate/order expected
> fields); it is explicitly out of scope for v1.

## 5. Architecture (layer by layer)

### 5.1 `crates/uds` — ISO builders + decoders (pure; no BMW data)
`service.rs` (builders, allocation-free arrays):
```rust
pub mod dtc_subfn {
    pub const REPORT_DTC_SNAPSHOT_BY_DTC: u8 = 0x04;
    pub const REPORT_DTC_EXT_DATA_BY_DTC: u8 = 0x06;
    pub const REPORT_SEVERITY_INFO_OF_DTC: u8 = 0x09;
}
pub const ALL_DTC_RECORDS: u8 = 0xFF;
pub fn read_dtc_snapshot_by_dtc(dtc: [u8;3], record: u8) -> [u8;6];   // 19 04 dtc record
pub fn read_dtc_extended_data_by_dtc(dtc: [u8;3], record: u8) -> [u8;6]; // 19 06 dtc record
pub fn read_dtc_severity_by_dtc(dtc: [u8;3]) -> [u8;5];               // 19 09 dtc
```
`dtc.rs` (decoders — validate SID, strip echoed DTC + status, return the raw record region;
**uds does not split records** because per-field width is not on the wire):
```rust
pub struct DtcRecordRegion { pub dtc: [u8;3], pub status: u8, pub body: Vec<u8> }
pub fn decode_dtc_snapshot(payload: &[u8]) -> Result<DtcRecordRegion, UdsError>;   // 59 04
pub fn decode_dtc_extended_data(payload: &[u8]) -> Result<DtcRecordRegion, UdsError>; // 59 06
pub struct DtcSeverity { pub dtc: [u8;3], pub status: u8, pub severity: u8, pub functional_unit: u8 }
pub fn decode_dtc_severity(payload: &[u8]) -> Result<DtcSeverity, UdsError>;       // 59 09
```
All response layouts carry the `[verify against capture]` marker in doc comments, mirroring
today's `decode_dtcs`.

### 5.2 `klartext-semantic` — new `snapshot.rs`
Parse the three SGBD tables via the existing `klartext-sgbd` `Prg`/`Table` API; reuse
`measurement::{DataType, scale}`:
```rust
pub enum FieldKind { Numeric, Status }
pub struct EnvCondDef { pub uwnr: u16, pub width: usize, pub mul: f64, pub div: f64,
                        pub add: f64, pub unit: Option<String>, pub text_de: Option<String>,
                        pub kind: FieldKind }
pub struct SnapshotDefs { /* HashMap<u16, EnvCondDef> from FUMWELTTEXTE + DTCSnapshotIdentifier */ }
pub struct ExtDataDef  { pub record: u8, pub name: String, pub byte_len: usize }
pub struct ExtDataDefs { /* HashMap<u8, ExtDataDef> from DTCExtendedDataRecordNumber */ }

pub struct SnapshotField { pub uwnr: u16, pub label: String, pub raw: Vec<u8>,
                           pub value: Option<f64>, pub unit: Option<String>,
                           pub state: Option<String>, pub available: bool }
pub struct DecodedSnapshot { pub fields: Vec<SnapshotField>, pub undecoded_tail: Option<Vec<u8>> }
pub struct ExtDataField { pub record: u8, pub label: String, pub raw: Vec<u8>, pub value: Option<i64> }

impl SnapshotDefs {
    pub fn from_prg(prg: &Prg) -> Result<Self, SemanticError>;
    pub fn decode(&self, region: &DtcRecordRegion, labels: Option<&Catalog>) -> DecodedSnapshot;
}
impl ExtDataDefs {
    pub fn from_prg(prg: &Prg) -> Result<Self, SemanticError>;
    pub fn decode(&self, region: &DtcRecordRegion, labels: Option<&Catalog>) -> Vec<ExtDataField>;
}
```
Walker: read `RN`, `NI`, then per identifier `[UWNR:2][data:width]` → scale → value+unit;
`Status` fields render `state` text where resolvable, else raw. **Unknown UWNR → stop the
walk** (unknown width would misalign) and return decoded-so-far + `undecoded_tail`.
Sentinels → `available = false`, value `None`.

`catalog.rs`:
```rust
pub struct EnvCondLabel { pub uwnr: u32, pub title_en: Option<String>, pub title_de: Option<String>,
                          pub unit: Option<String>, pub is_status: bool }
pub fn envcond_label(&self, uwnr: u16) -> Result<Option<EnvCondLabel>, SemanticError>;
```
Label precedence per field: DB `TITLE_ENGB` → DB `TITLE_DEDE` → SGBD `UWTEXT` (German) → `"UW <hex>"`.

### 5.3 `klartext-client` — the fetch (reads; autonomous-safe, no gate)
```rust
pub struct FaultDetailRaw { pub snapshot: Option<DtcRecordRegion>,
                            pub extended: Option<DtcRecordRegion>,
                            pub severity: Option<DtcSeverity> }
pub async fn read_fault_detail(&self, target: u8, dtc: [u8;3]) -> Result<FaultDetailRaw, ClientError>;
```
Issues `19 04`/`19 06`/`19 09` in turn; a **negative** response to any one → that field is
`None` (no detail stored), not an error. A transport error is a real `Err`. No extended
session / security precondition (these are reads).

### 5.4 Surface — fetch policy (approved)
- **Lean by default:** the whole-car scan and per-ECU `read_faults` / `read-faults` are
  unchanged (code + status + text). No extra round-trips; scan stays fast. == ISTA `FS_LESEN`.
- **On-demand detail** == ISTA `FS_LESEN_DETAIL`:
  - **MCP** `read_fault_detail(ecu, code)` → returns the fault plus decoded `snapshot`,
    `extended`, `severity`. MCP-exposable: standard ISO read, no derived write frame (cleaner
    than the M6 `2C` read-side exception). Mirrors `read_data`'s existing `--sgbd-dir` +
    variant-resolution wiring to load the target's `.prg`.
  - **CLI** `klartext fault-detail <ecu> <code>` — prints code + text, the snapshot as a
    `label = value unit` table, extended counters, and severity.

MCP DTOs (`mcp/src/dto.rs`):
```rust
pub struct SnapshotFieldDto { pub label: String, pub value: Option<f64>, pub unit: Option<String>,
                              pub state: Option<String>, pub available: bool, pub raw_hex: String }
pub struct ExtDataFieldDto  { pub label: String, pub value: Option<i64>, pub raw_hex: String }
pub struct FaultDetailResult { pub ecu: String, pub address: String, pub code_hex: String,
                               pub status_flags: Vec<String>, pub descriptions: Vec<FaultDescription>,
                               pub snapshot: Vec<SnapshotFieldDto>, pub extended: Vec<ExtDataFieldDto>,
                               pub severity: Option<u8>, pub sgbd_available: bool, pub notes: Vec<String> }
```

## 6. Degrade paths (never error on unknown; always give raw)
- **No SGBD `.prg` for the variant** → widths unknown → cannot split fields → return the raw
  region as hex with a note ("freeze-frame present; no SGBD to decode it"). `sgbd_available:false`.
- **SGBD present, DB absent** → values + units + German labels; no English names.
- **Unknown UWNR mid-walk** → decoded-so-far + `undecoded_tail` raw, with a note.
- **Fault has no snapshot** (negative response) → empty `snapshot`, note "no freeze-frame
  stored for this DTC."
- **Unsupported `DATENTYP` / mask** → that field degrades to raw; the walk stops if the width
  is unknowable.

## 7. Testing & the capture gate
Offline, hermetic, **all DERIVED** (synthetic bytes following the documented framing — same
discipline as today's `decode_dtcs` and the `2C` fixtures):
- **uds:** request byte-vector tests (`19 04/06/09`); decoder tests over synthetic
  `59 04/06/09` payloads (SID validation, DTC+status strip, region body, short/malformed).
- **semantic:** synthetic `.prg` fixtures carrying `FUMWELTTEXTE` + `DTCSnapshotIdentifier` +
  `DTCExtendedDataRecordNumber`; assert the worked decodes — coolant `0x5205` raw `0x?` →
  °C, RPM `0x5955` → rpm (×0.5), KM `0x1700` `FF FF FF` → "not available", one enum/status
  field, and an unknown-UWNR → `undecoded_tail`. `envcond` label lookup over a synthetic DB
  fixture. `#[ignore]` tests against the real DDE `.prg` and real `klartext-semantic.db`.
- **client:** loopback mock answering `19 04/06/09` (and a negative for a snapshot-less DTC)
  → full `read_fault_detail` path offline.
- **mcp:** `read_fault_detail` integration test over the loopback mock.
- `cargo fmt` + `cargo clippy -- -D warnings` clean.

**Manual on-car step (human runs; Claude cannot reach the car).** The existing capture has no
`0x19` detail traffic, so the response record-preamble offsets, value endianness, and record
counts are derived, not observed. Capture a real `59 04`/`59 06`/`59 09` on the F20, confirm
the framing, then flip the `[verify against capture]` constants to confirmed. **Never claim a
hardware round-trip** — only that unit tests pass and the manual test is ready.

## 8. Blast radius / safety
Every operation here is a **read** (UDS `0x19`): autonomous-safe, MCP-exposable, no
confirmation gate, no backup-before-write. Standard ISO reads — not a derived write frame —
so within the M9/M10 read invariant with room to spare. No change to the clear path or any
write surface.

## 9. Uncertain — resolved only by an on-car capture
1. Exact byte offsets of `statusOfDTC` / `DTCSnapshotRecordNumber` / `numberOfIdentifiers` in
   the raw HSFZ payload (ISTA's SGBD strips an EDIABAS transport header before walking).
2. Number of snapshot records returned and how record numbers are assigned (request uses
   `0xFF` = all).
3. Endianness of multi-byte snapshot values (big-endian assumed; verify on RPM/coolant).
4. Which extended records (`0x01/0x02/0x03`) are actually populated; CONDITION_BYTE semantics.
5. `59 09` severity/fault-class exact layout (lowest priority within this item).

## 10. File-by-file change list
- `crates/uds/src/service.rs` — 3 sub-function consts, `ALL_DTC_RECORDS`, 3 request builders.
- `crates/uds/src/dtc.rs` — `DtcRecordRegion`, `DtcSeverity`, 3 decoders + tests.
- `crates/semantic/src/snapshot.rs` — **new**; `SnapshotDefs`/`ExtDataDefs` + walkers + tests.
- `crates/semantic/src/measurement.rs` — ensure `DataType`/`scale` reused (already public).
- `crates/semantic/src/catalog.rs` — `EnvCondLabel` + `envcond_label` + fixture rows.
- `crates/semantic/src/lib.rs` — export `snapshot`.
- `crates/client/src/client.rs` — `FaultDetailRaw` + `read_fault_detail` + loopback test.
- `mcp/src/dto.rs` — `SnapshotFieldDto`/`ExtDataFieldDto`/`FaultDetailResult`.
- `mcp/src/server.rs` — `read_fault_detail` tool (mirror `read_data` SGBD loading).
- `mcp/tests/integration.rs` — `read_fault_detail` test.
- `cli/src/main.rs` — `fault-detail <ecu> <code>` subcommand + printer.
- `scripts/build-semantic-db.sh` — add the `envcond` table to the extract.
- `docs/field-findings-2026-07-03.md` — note the freeze-frame capture as a pending manual step.
