# M11 Item 2 — SVT + full vehicle-identification dump (design)

**Date:** 2026-07-04 · **Status:** design; implementation not started.
**Roadmap parent:** `docs/superpowers/specs/2026-07-03-m11-ista-parity-roadmap.md` item 2.
**Pattern:** follows `2026-07-03-m11-freeze-frames-design.md` (SGBD/DB-grounded decode +
degrade-to-raw + `[verify against capture]` gate). Item 1 (freeze-frames) is merged.

## 0. TL;DR — the research overturned the roadmap's premise

The roadmap asked an open question: *is the SVT read a read (MCP-exposable) or does its
`0x31` RoutineControl frame stay CLI-only?* Decompiling ISTA (`ilspycmd`) and disassembling
`zgw_01.prg` (ediabasx oracle, offline, never shipped) settled it with facts:

- **ISTA reads the fitted-ECU list with a plain `0x22` ReadDataByIdentifier**, not a `0x31`.
  `VehicleIdent.GetAllEcus` runs the EDIABAS job `STATUS_VCM_GET_ECU_LIST_ALL` on `zgw_01`;
  its bytecode builds **UDS `22 3F 07`** (DID `0x3F07`) to the gateway. Same service class as
  the VIN read klartext already does.
- The `STEUERN_VCM_GENERATE_SVT_START/GET_RESULTS` jobs the roadmap named ARE `0x31`
  (`31 01 02 07` / `31 03 02 07`), but they are a **different operation**: they *regenerate*
  the coding Sollverbauung (a programming-path build step). They are not needed to answer
  "which ECUs exist," and stay out of scope.

**Invariant (decided):** the identification/SVT reads (`22 3F 07`, `22 3F 06`, `22 10 0B`, the
`F1xx` per-ECU DIDs) are standard reads — autonomous-safe, exposed on **both CLI and MCP**,
same blast radius as the VIN. The `0x31` GENERATE_SVT routines are **not** exposed anywhere in
this milestone. There is no derived-`0x31`-on-MCP dilemma: the read we want is `0x22`.

## 1. Goal & scope

On connect, produce an ISTA-style **vehicle identity**: the model/type/build/vehicle-order, the
integration level, the authoritative **fitted-ECU list by name**, and each ECU's identification
block — all from autonomous-safe reads. Retire the M10 probe-scan by sourcing the fitted list
from the gateway SVT.

**In scope (all reads):**
1. **SVT fitted-ECU list** — `22 3F 07` → the installed ECUs' diagnostic addresses in one
   gateway read. **Becomes the discovery source** (replaces the M10 probe-scan). Names resolved
   from the semantic DB (not the gateway's stale `GROBNAME`).
2. **Per-ECU identification block** — the ISO/BMW `F1xx` DIDs (`F190` VIN, `F191` HW number,
   `F193/F195` HW/SW versions, `F197` system name, `F19E` ODX, `F187` spare-part, `F188` SW
   number, `F18A` supplier, `F18C` serial). Which DIDs answer is ECU-specific → degrade-to-skip.
3. **I-Stufe (integration level)** — `22 10 0B` (`STATUS_VCM_I_STUFE_LESEN`).
4. **FA / vehicle order (Fahrzeugauftrag)** — `22 3F 06` (`STATUS_VCM_GET_FA`) decoded into
   Baureihe (model series), Typschlüssel (type key), Lackcode (paint), Polstercode (upholstery),
   Zeitkriterium (build date), and the SALAPA/HO-Worte option-code list.
5. **Aggregate report** — `identify_vehicle` (MCP) / `identify` (CLI) tying 1–4 together.

**Out of scope (explicit):**
- The `0x31` `STEUERN_VCM_GENERATE_SVT_*` Sollverbauung generation (control-class; programming
  path). Not exposed on CLI or MCP.
- Per-ECU **SGBD-variant** auto-detection as a hard deliverable. The SVT gives address + name,
  not the SGBD variant (e.g. `d72n47a0`). A **best-effort** variant enrichment is included (§5.6)
  but the M10 variant ladder stays as the mechanism; this milestone does not remove it.
- Full option-code → human-text catalog (SALAPA code "$1CA" → "Comfort Access"). We decode the
  *codes*; naming them is a later DB-catalog item (relates to roadmap item 4).
- DoIP / G-series. HSFZ only, per CLAUDE.md.

## 2. How ISTA does it (established by decompile + disassembly)

Confirmed offline against the ISTA .NET DLLs (`ilspycmd`: `RheingoldDiagnostics.dll`,
`BMW.ISPI.TRIC.ISTA.VehicleIdentification.dll`) and `zgw_01.prg` (ediabasx as an offline oracle
— never shipped). Facts/protocol only; no ISTA source copied (AGPL / CLAUDE.md).

- **`VehicleIdent.GetAllEcus`** runs `STATUS_VCM_GET_ECU_LIST_ALL`, then loops result sets
  `1..N` reading `STAT_SG_DIAG_ADRESSE` (address string `"0xNN"`) and `STAT_SG_NAME_TEXT`
  (coarse name), plus `STAT_SG_ANZAHL` (count). It parses **address + name only** — no variant.
- **Job → UDS**, read directly from bytecode `move S1,{…}` request literals:

  | EDIABAS job | UDS request | Meaning |
  |---|---|---|
  | `STATUS_VCM_GET_ECU_LIST_ALL` | `22 3F 07` | fitted ECU list |
  | `STATUS_VCM_GET_FA` | `22 3F 06` | vehicle order (Fahrzeugauftrag) |
  | `STATUS_VCM_I_STUFE_LESEN` | `22 10 0B` | integration level (I-Stufe) |
  | `STATUS_VCM_GET_ECU_LIST_ISO14229` | `22 3F 15` | UDS-capable subset (not needed) |
  | `STEUERN_VCM_GENERATE_SVT_START` | `31 01 02 07` | **control — out of scope** |
  | `STEUERN_VCM_GENERATE_SVT_GET_RESULTS` | `31 03 02 07` | **control — out of scope** |

  (EDIABAS wraps the payload in a KWP `80|len / target / source` telegram; over HSFZ that framing
  is replaced by the HSFZ header, so the on-wire UDS payload is exactly the bytes above. The read
  targets the gateway, address `0x10` = `ZGW_ADDRESS`.)
- **Names are coarse and stale in the gateway.** The name in `STAT_SG_NAME_TEXT` comes from the
  `zgw_01.prg` local table `GROBNAME [ADR, GROBNAME]` (111 rows), whose `0x40` entry is `"CAS"` —
  the exact legacy mislabel M10 caught (this diesel F20's `0x40` is the FEM). **So klartext uses
  the SVT for the fitted address list only, and resolves the precise name from the semantic DB
  (`Catalog::ecus()`).**

## 3. Wire framing

Legend: **[ISO]** ISO 14229-1 · **[BMW]** BMW-specific, confirmed offline via disassembly ·
**[CAP]** structure derived from ISO + disassembly, **byte offsets need one on-car capture.**

### 3.1 Requests — certain (from bytecode literals)
```
22 3F 07        [BMW] fitted ECU list      (to ZGW 0x10)
22 3F 06        [BMW] vehicle order (FA)   (to ZGW 0x10)
22 10 0B        [BMW] I-Stufe              (to ZGW 0x10)
22 F1 90        [ISO] VIN                  (per ECU; already implemented)
22 F1 xx        [ISO] identification DIDs  (per ECU; §5.3)
```

### 3.2 `62 3F 07` ECU-list response — [CAP] on preamble offsets
Derived from the `STATUS_VCM_GET_ECU_LIST_ALL` result-parse bytecode (reads count at data
offset 0..1, then one address byte per ECU):
```
62 3F 07          [BMW] echoed SID + DID (3 bytes) — stripped by decode_read_data_by_identifier
NN NN             [BMW] STAT_SG_ANZAHL = ECU count, u16 big-endian   (data region offset 0..1)
AA                [BMW] one byte per ECU = diagnostic address        (repeated count times)
...
```
`decode_ecu_list` operates on the **data region** (after the `62 3F 07` echo is stripped by the
existing `decode_read_data_by_identifier`), so the count is at data offset 0..1. Name is **not on
the wire** — resolved from `Catalog::ecus()` by address (§5.4).

### 3.3 `62 3F 06` vehicle-order (FA) response — [CAP] on field offsets
The bytecode extracts these result fields (confirmed names); it reads `STAT_VERSION` first, then
unpacks the option list with the `TABKOMPRIMIERUNG` 2-bit table (§4.2). Fixed-header field
offsets/widths are extracted from the `STATUS_VCM_GET_FA` bytecode during implementation and
confirmed against a capture:

| Field | result name | note |
|---|---|---|
| version | `STAT_VERSION` | selects the layout/branch (u16) |
| model series | `STAT_BAUREIHE` | e.g. `F20` |
| type key | `STAT_TYP_SCHLUESSEL` | 4-char, e.g. `1K21` |
| paint | `STAT_LACKCODE` | |
| upholstery | `STAT_POLSTERCODE` | |
| build date | `STAT_ZEIT_KRITERIUM` | week/year criterion |
| options | `STAT_SALAPA` / `STAT_HO_WORTE` / `STAT_E_WORTE` | 3-char SA codes (2-bit packed) |

### 3.4 `62 10 0B` I-Stufe response — [CAP]
Integration-level string(s) (factory + current). Layout confirmed at implementation from the
`STATUS_VCM_I_STUFE_LESEN` bytecode + capture.

**Negative response** `7F 22 <NRC>` is normal for a DID an ECU does not serve (e.g. an
identification DID a module doesn't implement, or the FA on a car whose VCM is empty). Treat as
"absent," not an error (see §6).

## 4. Decode definitions

### 4.1 ECU-list structure — pure, no BMW data
Count (u16 BE) + `count` address bytes. A pure structural decoder in `crates/uds` (like
`decode_dtcs` handles BMW's 3-byte DTC format without embedding data).

### 4.2 FA SALAPA unpack — reimplemented algorithm, tiny fixed tables
Two fixed encodings, reimplemented from the bytecode (facts — not proprietary BMW content, no
VIN, no bulk tables):
- `TABKOMPRIMIERUNG`: 2-bit header → ASCII digit — `00→'0'`, `01→'3'`, `10→'4'`, `11→'5'`.
- `TabHexBin`: hex nibble ↔ 4-bit binary (standard).

The SALAPA list is bit-packed 3-character option codes; the decoder walks the packed bytes,
expands each code's leading category via the 2-bit table, and emits the code strings. This is
bit-unpacking + string assembly (no arithmetic-heavy compression). Pure function, offline-testable
against known packed→codes vectors.

### 4.3 Identification DID names — ISO facts
The `F1xx` DID → human name map is standard ISO 14229-1 / documented in `protocol-reference §1.5`
(public facts). A small static table in `crates/uds` (`did::name(did) -> Option<&str>`). Values
are ASCII (VIN, part numbers) or bytes; rendering is generic (already handled by `read_did`).

## 5. Architecture (layer by layer)

### 5.1 `crates/uds` — constants + one structural decoder (pure)
`service.rs`:
```rust
pub mod did {
    pub const VIN: u16 = 0xF190;            // existing
    pub const IP_CONFIG: u16 = 0x172A;      // existing
    pub const ECU_LIST_ALL: u16 = 0x3F07;   // SVT fitted list
    pub const VEHICLE_ORDER: u16 = 0x3F06;  // FA
    pub const I_STUFE: u16 = 0x100B;        // integration level
    pub const HW_NUMBER: u16 = 0xF191;
    pub const SYSTEM_NAME: u16 = 0xF197;
    pub const ODX_FILE: u16 = 0xF19E;
    pub const SPARE_PART_NUMBER: u16 = 0xF187;
    pub const SW_NUMBER: u16 = 0xF188;
    pub const SUPPLIER: u16 = 0xF18A;
    pub const SERIAL: u16 = 0xF18C;
    // …the full identification set enumerated in §1 scope item 2
    pub fn name(did: u16) -> Option<&'static str>;   // ISO F1xx names
}
```
New `identity.rs` in uds (pure, no BMW data):
```rust
/// Decode the DATA REGION of a `62 3F 07` ECU-list response (after the echo is
/// stripped by decode_read_data_by_identifier) into diagnostic addresses.
/// [verify against capture] — count-offset + 1-byte-per-ECU derived from bytecode.
pub fn decode_ecu_list(data: &[u8]) -> Result<EcuList, UdsError>;
pub struct EcuList { pub count: u16, pub addresses: Vec<u8> }
```
Reads themselves reuse the existing `read_data_by_identifier` builder + the
`decode_read_data_by_identifier` echo-stripping decoder (and `client.read_did`, which already
guards against a DID-echo desync) — no new request builders.

### 5.2 `klartext-semantic` — new `identity.rs`
The FA decoder (pure; needs no DB, no SGBD file — the two encoding tables are reimplemented
constants):
```rust
pub struct VehicleOrder {
    pub version: u16,
    pub baureihe: Option<String>,          // model series
    pub typ_schluessel: Option<String>,    // type key
    pub lackcode: Option<String>,          // paint
    pub polstercode: Option<String>,       // upholstery
    pub build_date: Option<String>,        // Zeitkriterium
    pub options: Vec<String>,              // SALAPA codes (undecoded names)
    pub raw: Vec<u8>,                       // always kept for degrade-to-raw
}
pub fn decode_vehicle_order(region: &[u8]) -> Result<VehicleOrder, SemanticError>;
```
ECU-name overlay — precise names from the DB, reusing M10's `Catalog::ecus()`:
```rust
/// Map SVT addresses to named slots via the semantic DB; addresses with no DB
/// entry keep a raw-hex name so an unknown ECU is never dropped.
pub fn name_ecu_list(catalog: Option<&Catalog>, addresses: &[u8]) -> Vec<NamedEcu>;
pub struct NamedEcu { pub address: u8, pub name: Option<String>, pub title: Option<String> }
```

### 5.3 `klartext-client` — the reads (autonomous-safe; no gate)
```rust
/// SVT fitted-ECU list from the gateway (22 3F 07 → decode_ecu_list). Discovery source.
pub async fn read_ecu_list(&self) -> Result<EcuList, ClientError>;
/// FA vehicle order (22 3F 06 → semantic decode_vehicle_order).
pub async fn read_vehicle_order(&self) -> Result<VehicleOrder, ClientError>;
/// I-Stufe (22 10 0B).
pub async fn read_i_stufe(&self) -> Result<String, ClientError>;
/// One ECU's identification block; each DID that answers negatively is skipped.
pub async fn read_ecu_identification(&self, target: u8) -> Result<EcuIdentification, ClientError>;
/// Whole-vehicle identity: SVT list → per-ECU identification → FA → I-Stufe, aggregated.
pub async fn identify_vehicle(&self) -> Result<VehicleIdentity, ClientError>;

pub struct EcuIdentification { pub address: u8, pub fields: Vec<IdField> }
pub struct IdField { pub did: u16, pub name: Option<String>, pub ascii: Option<String>, pub raw: Vec<u8> }
pub struct VehicleIdentity {
    pub vin: Option<String>, pub vehicle_order: Option<VehicleOrder>, pub i_stufe: Option<String>,
    pub ecus: Vec<NamedEcu>, pub identification: Vec<EcuIdentification>,
}
```
The gateway reads target `ZGW_ADDRESS` (0x10). A **negative** response to any one DID → that
field is `None`; a transport error is a real `Err`. No extended session / security precondition.

### 5.4 Discovery rewire — SVT replaces the probe-scan (per owner decision)
- The fitted list is the **SVT** (`read_ecu_list`), not the M10 catalog probe. `scan_faults`
  iterates the SVT addresses; each ECU's per-read errors are already recorded per-ECU (a
  listed-but-silent ECU shows as an error in its slot, never aborting the scan).
- **No probe-scan fallback.** If the SVT read fails (transport error / negative / unsupported),
  discovery returns an honest error — it does not silently probe. (Rationale: `22 3F 07` is
  ISTA's production path on millions of cars; the owner chose to rely on it outright.)
- `probe(target)` and `scan_present(addrs)` remain in the client as primitives (small, tested,
  and semantically distinct: "installed per SVT" vs "answers right now"). They are **not** part
  of the default discovery or fault-scan path. Not deleted; not wired as a fallback.

### 5.5 Surface — MCP + CLI
- **MCP** `identify_vehicle` → `VehicleIdentity` DTO (VIN, decoded FA, I-Stufe, named fitted
  list, per-ECU identification). Autonomous read (standard `0x22`, no derived write frame).
  `scan_ecus` now sources the fitted list from the SVT (faster, authoritative; still cached,
  `rescan: true` re-reads). Optional `read_ecu_identification(ecu)` tool for a single ECU.
- **CLI** `klartext identify` → prints the identity report: VIN, model/type/build/paint/upholstery,
  option codes, I-Stufe, and the fitted ECU table (address · name · title). `scan` uses the SVT.

MCP DTOs (`mcp/src/dto.rs`): `VehicleIdentityResult { vin, vehicle_order: VehicleOrderDto,
i_stufe, ecus: Vec<NamedEcuDto>, identification: Vec<EcuIdentDto>, notes: Vec<String> }`, with
`raw_hex` on any field that could not be decoded.

### 5.6 Variant enrichment (best-effort, not a hard deliverable)
When an ECU's identification block yields a system name / part number that maps unambiguously to
one DB variant for that address (`Catalog::variants(address)`), record it into the M10 learned
profile (`address → variant`) so the existing ladder's rung 2 improves over time. If nothing
resolves, the ladder is unchanged. This connects Item 2 to the M10 ladder **without** removing it
and without over-promising SGBD-variant auto-detection from the SVT (which the SVT does not carry).

## 6. Degrade paths (never error on unknown; always give raw)
- **SVT read fails** → honest `Err` from discovery (no silent probe fallback, §5.4). The MCP/CLI
  surfaces the error text.
- **FA absent / VCM empty** (`7F 22 …`) → `vehicle_order: None`, note "no vehicle order stored."
- **FA present but a field/branch is unknown** → decoded-so-far + `raw` bytes retained; a note
  flags the undecoded region. Never throw away the raw FA.
- **Identification DID a module doesn't serve** (`7F 22 …`) → that field skipped; the block lists
  only the DIDs that answered.
- **No semantic DB** → fitted list shows raw-hex names (addresses), FA still decodes (DB-independent),
  identification still reads. The tool says names are unavailable rather than inventing them.
- **Unknown ECU address in the SVT** (not in the DB) → kept with a raw-hex name, never dropped.

## 7. Testing & the capture gate
Offline, hermetic, **all derived** (synthetic bytes following the documented framing — same
discipline as `decode_dtcs` and the freeze-frame fixtures):
- **uds:** `decode_ecu_list` over synthetic `62 3F 07` (count + addresses; short/malformed;
  zero ECUs); `did::name` lookups; request builders already covered.
- **semantic:** `decode_vehicle_order` over synthetic FA vectors — a known 2-bit-packed SALAPA
  run → expected option codes, header fields sliced correctly, an unknown-version → raw retained.
  `name_ecu_list` over a synthetic `Catalog` (named, unnamed, and NULL-address rows).
- **client:** loopback mock answering `22 3F 07 / 3F 06 / 10 0B` and the `F1xx` set (plus a
  negative for an unsupported DID and an empty-FA negative) → full `identify_vehicle` path offline.
- **mcp:** `identify_vehicle` integration test over the loopback mock; `scan_ecus`-via-SVT test.
- `cargo fmt` + `cargo clippy -- -D warnings` clean.

**Manual on-car step (human runs; Claude cannot reach the car).** The current pcap has **no**
`22 3F07 / 3F06 / 100B` traffic, so the `62 3F 07` count-offset + 1-byte-per-ECU stride, the FA
field offsets and SALAPA packing, and the I-Stufe layout are **derived, not observed**. Capture a
real gateway identification read on the F20, confirm the framing, then flip the
`[verify against capture]` markers to confirmed. **Never claim a hardware round-trip** — only that
unit tests pass and the manual test is ready.

## 8. Blast radius / safety
Every operation is a **read** (UDS `0x22` ReadDataByIdentifier): autonomous-safe, MCP-exposable,
no confirmation gate, no backup-before-write — same class as the existing VIN read, well within
the M9/M10 read invariant. The `0x31` `STEUERN_VCM_GENERATE_SVT_*` routines are **not** exposed on
any surface. No change to the clear path or any write surface.

## 9. Uncertain — resolved only by an on-car capture
1. `62 3F 07`: exact data offset of `STAT_SG_ANZAHL` in the raw HSFZ payload (EDIABAS strips a KWP
   header before its bytecode walks; over HSFZ the offset may differ) and confirmation of the
   1-byte-per-ECU stride (no trailing status/variant byte).
2. FA (`62 3F 06`): fixed-header field offsets/widths, the `STAT_VERSION` branch actually present
   on this VCM, and the SALAPA packing boundary conditions.
3. I-Stufe (`62 10 0B`): factory-vs-current field layout.
4. Whether every SVT-listed ECU actually answers diagnostics (installed ≠ responding) — affects
   how the fault scan annotates a silent-but-listed ECU (already handled as a per-ECU error).

## 10. File-by-file change list
- `crates/uds/src/service.rs` — DID constants (`ECU_LIST_ALL`, `VEHICLE_ORDER`, `I_STUFE`, the
  `F1xx` identification set) + `did::name`.
- `crates/uds/src/identity.rs` — **new**; `EcuList` + `decode_ecu_list` (operates on the data
  region) + tests.
- `crates/uds/src/lib.rs` — export the new items.
- `crates/semantic/src/identity.rs` — **new**; `VehicleOrder` + `decode_vehicle_order`
  (SALAPA unpack) + `NamedEcu` + `name_ecu_list` + tests.
- `crates/semantic/src/lib.rs` — export `identity`.
- `crates/client/src/client.rs` — `read_ecu_list`, `read_vehicle_order`, `read_i_stufe`,
  `read_ecu_identification`, `identify_vehicle` + loopback tests.
- `crates/client/src/scan.rs` — `scan_faults` sources the fitted list from the SVT; probe
  primitives retained but no longer the discovery default.
- `mcp/src/dto.rs` — `VehicleIdentityResult` + field DTOs (with `raw_hex` fallbacks).
- `mcp/src/server.rs` — `identify_vehicle` tool; `scan_ecus` via SVT; optional per-ECU
  `read_ecu_identification` tool.
- `mcp/tests/integration.rs` — `identify_vehicle` + SVT-scan tests.
- `cli/src/main.rs` — `identify` subcommand + printer; `scan` via SVT.
- `docs/protocol-reference.md` — record the VCM DIDs (`0x3F07`/`0x3F06`/`0x100B`) and the
  `STATUS_` (read) vs `STEUERN_` (control) job-class convention.
- `docs/field-findings-2026-07-03.md` — note the identification/SVT capture as a pending manual step.
- `CLAUDE.md` (M-notes) — record the Item-2 invariant resolution (SVT list = `0x22` read).
