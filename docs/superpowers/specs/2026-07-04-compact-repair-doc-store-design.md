# Compact repair-doc store — design (M11 follow-on)

**Date:** 2026-07-04
**Status:** design — approved forks, pending spec review
**Milestone context:** builds on M11 Item 4 (fault→doc link+title layer). This is the deferred
"own compact repair-doc store" milestone: an offline, car-aware document catalog built from the
user's ISTA DBs, replacing dependence on ISTA's ~50 GB corpus. Owner chose **full coverage** (build
the `VALIDITYINFO` evaluator now; compact is a build-time dial) and **all four extra doc types**.

---

## 1. Goal & non-goals

**Goal.** When klartext reads a fault, surface "what this fault means and the service measure" from
the fault-description sheet; and let the user browse/search the actual repair procedures, function
descriptions, specs, wiring diagrams, and reference sheets for the connected car — all offline, from
a compact self-built store, with embedded graphics.

**In scope.**
- Extract + render document **bodies** (not just Item 4's pointers) to compact German markdown.
- Content types: **FKB** (fault descriptions, the spine), **REP** (repair procedures — legacy *and*
  modular formats), **FUB** (function descriptions), **TED** (technical data), **SSP** (wiring
  diagrams, SVG), and tier-2 reference (**PIB, AZD, REH, SIT, STA, SWZ**).
- **Embedded graphics** (PNG + SVG) referenced by procedures, via the structural resolver.
- **Per-doc applicability** metadata + a **`VALIDITYINFO` evaluator** so the runtime shows only docs
  that apply to the connected car; the same evaluator drives build-time coverage scoping.
- **Offline full-text search** (FTS5) over rendered text — the reachability mechanism for procedures.
- Surfaces: CLI (`fault-docs` shows bodies; new `search`, `doc`) + MCP (`fault_help` returns body;
  new `search_docs`). All reads, autonomous-safe.

**Non-goals (deferred to Item 5 — guided diagnosis).**
- **Automatic fault→procedure navigation.** The only direct fault→document edge is `FaultcodeFkbLink`
  (→FKB). REP/FUB/SSP/etc. hang off the diagnosis-object tree (`DiagobjDocumentLink`); traversing it
  is Item 5. Procedures are reached here by **search/browse/component**, not auto-attached to a DTC.
- Precondition/branch logic (`XEP_QUERYOBJECTS`), the diagnosis tree
  (`XEP_DIAGNOSISOBJECTS`/`XEP_REFDIAGNOSISTREE`/`XEP_REFDIAGOBJECTS`), and **ABL `FLOWXML` execution**
  (ABL has *no* prose body — it is BEST-2 bytecode; 0 of 22,920 ABL objects carry an xmlvalue body).
- The store is built so these edges drop in later against existing rows — no rework.

**Non-goals (other).** No English bodies (this ISTA install has no `xmlvalueprimitive_ENGB`; bodies
are German, titles stay multilingual from Item 4). No video/audio (MP4/WMV/MP3 ≈ 3.5 GB, excluded).

**Non-goals (portability — a later milestone).** This milestone is **core + implementation only**:
the build pipeline, `VALIDITYINFO` evaluator, renderers, graphics pipeline, storage, runtime, and
CLI/MCP surfaces, for the owner's car. **Deferred to the portability milestone:** the mobile/embedded
story — a dedicated **scope-down-and-package script** (reduce the store to a chosen set of cars,
`embed-data`/`include_bytes!` bundling for iOS/embedded apps) and any UniFFI/mobile surface. The
`--coverage {legacy|f2x|all}` dial here is a plain build argument, **not** that packaging tooling;
`embed-data` (M11 Item 3) references below are forward-looking synergy notes, not deliverables here.

---

## 2. Corpus facts (measured against the on-disk DBs — nothing committed)

All figures from the user's `data/Testmodule(1)/SQLiteDBs/`. Sizes are per-doc unless noted.

- **Prose** lives in `xmlvalueprimitive_DEDE.sqlite` (49.7 GB, FTS5; body XML in the `c3` column of
  the `xmlvalueprimitive_content` shadow table, keyed by global id `c0`, TEXT).
- **Media** (PNG/SVG/MP4) all live in `streamdataprimitive_OTHER.sqlite` (22 GB, `(id, …, stream BLOB)`,
  243,840 blobs; PNG 238,685 ≈ 14.6 GB). Language-neutral.
- **Metadata** lives in the RC4-encrypted `DiagDocDb.sqlite` (opened build-time only via SQLite3MC,
  `cipher='rc4'`, key in the build script).

| Type | count (all-models) | format | avg body | fault-linked? | notes |
|---|---:|---|---:|---|---|
| FKB | 82,114 | XML | 1.9 KB | **yes** (`FaultcodeFkbLink`) | 15 sections incl. `MASSNAHMEIMSERVICE` |
| REP | 123,778 | XML | 279 KB | no (diag tree) | **bimodal**, see §5 |
| SSP | 55,287 | SVG | 49 KB | no | wiring diagrams |
| FUB | 11,673 | XML | 10.6 KB | no | function descriptions |
| TED | 5,961 | XML | 3.7 KB | no | technical data |
| PIB/AZD/REH/SIT/STA/SWZ | ~21k total | XML | small | no | reference tier |
| ABL | 22,920 | — | **no body** | no | `FLOWXML` bytecode → **Item 5** |

**FKB fault coverage:** `fault_doc` (Item 4) holds 72,256 distinct `(address, code)`, **all** with a
`content_dede` body pointer; the `dtc` universe is 91,882 distinct `(address, code)` → **78.6%** of
known DTCs have a description sheet.

---

## 3. Content model — two tiers

**Tier 1 — FKB spine (fault-linked, universal, no graphics).** Rendered German markdown for every
fault we can name. ~30 MB gzipped for all faults. Auto-attached to DTCs via Item 4's `(address, code)
→ content_dede` pointers. Highest value per byte: it already answers "what's wrong + the measure."

**Tier 2 — procedures & reference (browseable, car-scoped, with graphics).** REP + FUB + TED + SSP +
tier-2. Reached by full-text search and by component/ECU, **not** auto-attached to DTCs. Each doc
carries applicability metadata; the runtime filters by the connected car.

---

## 4. Data sources & verified join map

All joins verified during research. Body of any doc:

```
XEP_INFOOBJECTS(INFOTYPE, ID, IDENTIFIER, DOCNUMBER, TITLE_*)   -- the doc
  └─ XEP_IOCONTENTS  ON IOCONTENTS.CONTROLID = INFOOBJECTS.ID   -- its content record
       └─ CONTENT_DEDE  = xmlvalueprimitive_content.c0          -- German body (c3)
```

Fault→FKB (Item 4, already extracted into `fault_doc`):
`RG_ECUFAULT_DOCIDS.ECUFAULT_ID = XEP_FAULTCODES.ID`, `CONTENT_DEDE` → FKB body global id.

**Graphics — structural resolver (verified 100%, 208,257/208,257):**
```
body <GRAPHIC LINKID="G1">
  └─ XEP_REFINFOSEGMENTS(ID = IOCONTENTS.ID, LINKID)  -- G-links live here (NOT REFDOCUMENTS)
       └─ BAUSTEINID = XEP_INFOSEGMENTS.ID
            └─ INFOSEGMENTS.CONTENT_DEDE = streamdataprimitive_OTHER.id  -- the PNG/SVG bytes
               INFOSEGMENTS.INFORMATIONSFORMAT ∈ {PIX-PIX-PNG-1, EGR-EGR-PNG-1, GRA-SVG-SVG-1, …}
```
(Filename fallback `SRC`→`GRAFIKNUMMER` exists at 99.6% but is non-unique/has empties — the
structural path is preferred and deterministic.) Cross-doc hyperlinks (`HOTSPOT LINKID="H1"`) resolve
via `XEP_REFDOCUMENTS` instead; rendered as "see also" links, not required for completeness.

---

## 5. REP bimodality & the coverage dial

REP splits cleanly by `IDENTIFIER LIKE '%-P-%'` (verified, no crossovers):

- **Legacy** (`REP-…-RA<chassis>`, root `<REPAIRMANUALDOCUMENT>`): 49,484 docs, 224 MB total,
  **single-chassis**, chassis in the IDENTIFIER — filterable at the DB level with **no body parse**.
  The **F20 family lives here** (a 2011–2019 car predating the modular format).
- **Modular / MOD** (`REP-…-P-…`, root `<rep:REPAIRMANUALDOCUMENT_MOD>`): 74,294 docs, **35 GB
  (99.4% of bytes)**, cross-platform containers with **per-fragment `VALIDITYINFO`** (11–394
  expressions/doc, 24–162 distinct chassis each). Getting F20-correct content means **evaluating
  `VALIDITYINFO` per fragment** and keeping matches — the evaluator (§7).

**Coverage is a build-time dial** (measured; rendered-markdown projections):

| Tier (`--coverage`) | REP text | REP images | Store total | Embeddable? | Needs evaluator? |
|---|---|---|---|---|---|
| **`legacy`** (native F2x floor) | 2.1 MB gz (1,867 docs) | 228 MB (4,853 imgs) | ~280 MB | yes (desktop+mobile) | no |
| **`f2x`** (legacy + MOD fragments) | ~0.37 GB gz | multi-GB (subset of 13.75 GB) | multi-GB | desktop only | **yes** |
| **`all`** (all-models) | ~1 GB gz | 13.75 GB | ~15 GB | no | yes |

**Decision:** build the full logic; **default the dial to the `f2x` tier** (the 1/2-series family
F20/F21/F22/F23) for the owner's own build, and expose the dial so a compact legacy-floor artifact
(mobile) is a config knob. `F20-family ≈ F20-only` within <1% (they share the same MOD containers) →
offer **one `f2x` tier**, not F20-only and family separately.

**Per-type scoping.** `VALIDITYINFO`/modularization is **REP-only** (FKB/SSP/FUB/TED/tier-2 all have
0 `-P-` docs). Each type encodes chassis in its own IDENTIFIER convention (e.g. EBO `…EO_E65`, STA
`…F01_SX`) or is model-generic. The applicability extractor (§6) is **per-type**; determining each
type's IDENTIFIER regex is a small build-time RE step listed in §16 open items.

---

## 6. Applicability & car-scoping

Every stored doc carries `applicable_chassis` — a normalized set of chassis tokens (`EBezeichnung`
values). Runtime shows a doc iff `car_chassis ∈ applicable_chassis` OR the doc is universal. The car's
chassis comes from the existing `identify_vehicle`/type-key path.

Extraction rule (per doc):
1. **MOD REP** (`IDENTIFIER LIKE '%-P-%'`): union of every `EBezeichnung` value across body
   `VALIDITYINFO`. Build-time parse; chassis is **never negated** (0 `EBezeichnung !=`), so the union
   is exact — no include/exclude logic for chassis.
2. **Legacy REP**: chassis from IDENTIFIER `-RA<chassis>` (single). 44,137/49,484 standard; the ~5,347
   `RAGRP…` group-level docs are universal.
3. **Other types**: chassis from that type's IDENTIFIER convention where present; genuinely
   model-generic docs (no token) → universal.
4. **`no token ⇒ universal` is only true where the IDENTIFIER carries no chassis** — for legacy REP the
   chassis IS in the IDENTIFIER, so absence there means group-level, not "applies to everything." The
   extractor is per-type-aware to avoid showing E-series procedures to an F20.

No DB table maps a doc→chassis directly (`XEP_VEHICLES` etc. describe vehicles, not docs); IDENTIFIER
is the only DB-level signal (legacy), MOD requires the body parse. Result is stored as the token set →
runtime is a cheap set-membership test.

---

## 7. `VALIDITYINFO` evaluator (the one substantial new algorithm)

A boolean expression over attribute equality, used in two modes with one implementation.

**Grammar** (measured over 383,135 expressions): keys `EBezeichnung` (chassis), `MOTBaureihe`
(engine series), `Marke` (brand), `MOTUeberarbeitung`, `MOTLeistungsklasse`, `Hybridkennzeichen`,
plus a long tail (`FAElement`, `Produktlinie`, `Antrieb`, `Getriebe`, `Typschluessel`). Operators
`AND`, `OR`, `NOT` (only ever negating engine/hybrid attributes, never chassis); `!=` never occurs.
Expression = boolean tree of `Key="Value"` leaves.

**Mode A — build-time coverage filter** (which fragments/docs to keep for a coverage tier): keep a
fragment if its expression is **satisfiable for some chassis in the coverage set** — i.e. the chassis
disjunction intersects the set (union test; ignore engine/option leaves). Cheap, no per-car data.

**Mode B — runtime precision filter** (which fragments to show for the connected car): bind the car's
full attribute set (chassis + engine + options from `identify_vehicle`) and **fully evaluate** the
boolean; show only fragments that evaluate true. v1 may bind chassis only (engine-level precision is a
later refinement; non-matching engine variants are rendered but annotated with their applicability).

**Build rendering decision:** render **per chassis in the coverage set**. For each chassis, keep
fragments satisfiable for it (Mode A with a singleton set) + universal fragments, then flatten to
markdown. Store keyed by `(docnumber, chassis)`. This bakes chassis-scoping (avoids per-car variants)
and preserves engine-variant fragments inline (annotated). Runtime selects the `(docnumber,
connected-chassis)` variant. Rationale: composition research showed one raw MOD doc multiplexes many
models' `PROCESS` fragments; rendering without scoping mixes them, so scope-then-flatten is mandatory.

The evaluator is a small, pure, fully unit-testable module (expression parser + tree evaluator) with
known input→output vectors — no car needed to test it.

---

## 8. Rendering (XML → compact German markdown)

Store **rendered markdown**, not raw XML (REP XML is ~90% boilerplate `VALIDITYINFO`/namespaces;
rendered ≈ 10% of raw; FKB ≈ 50–60%). Rendering is pure and unit-testable against fixture bodies.

**FKB** — fixed section schema, drop empty sections, document order, German headings:
`FEHLERBESCHREIBUNG→Fehlerbeschreibung`, `SETZBEDINGUNG→Setzbedingung`,
`UEBERWACHUNGSBEDINGUNG→Überwachungsbedingung`, `ZEITBEDINGUNG→Zeitbedingung`,
**`MASSNAHMEIMSERVICE→Maßnahme im Service`** (highest value), `FEHLERAUSWIRKUNG→Fehlerauswirkung`, …
`PARAGRAPH`→paragraph; `KLEMME`→"**Klemme:** Klemme 15 — an"; `CCMELDUNG`→own block; lists→`-`.
No links, no graphics.

**REP (MOD schema)** — `rep:TITLE`→`#`, `PREREQUISITES/PREPROCESSES/KEYPROCESS/POSTPROCESSES`→`##`
(Voraussetzungen/Vorarbeiten/Hauptarbeit/Nacharbeiten); unwrap `INCLUDE_PROCESS`→`PROCESS` (the body
is inline — no external fetch); `proc:TITLE`→`###`; `OPERATINGSTEP`→ordered-list item;
`proc:GRAPHIC LINKID`→`![](graphic:<blob-id>)`; `INSTRUCTION`→step text; `tigh:TIGHTENING(VALUE,UNIT)`
→"**Anziehdrehmoment** … 8 Nm"; `hint:DAMAGE`→"> ⚠️ **Beschädigungsgefahr** …"; `HOTSPOT`→see-also
link; `AWNUMBER`→muted labor code. Watch-icons (`GRCI0000-*`) → admonition markers, not images.

**REP (legacy schema)** — same mapping, renamed tags (`PROCESSTITLE`, `OPERATINGSTEP`,
`ILLUSTRATION>GRAPHIC`, `HINT`). Renderer handles both roots.

**FUB/TED/tier-2** — `<DIAGNOSISDOCUMENT>`-family; render headings + paragraphs + tables to markdown;
same graphic-ref handling. **SSP** — SVG stored as a graphic asset (not markdown), referenced from a
wiring index; scope by IDENTIFIER.

---

## 9. Graphics pipeline

At build, for each included doc, resolve its `GRAPHIC LINKID`s via the structural resolver (§4) to
distinct blob ids, whitelist `INFORMATIONSFORMAT ∈ {PIX-PIX-PNG-1, EGR-EGR-PNG-1, GRA-SVG-SVG-1}`
(skip MP4/MP3/WMV), and store each blob **once** (dedup by blob id — ~46× reuse across REP docs). The
rendered markdown references `graphic:<blob-id>`. Graphics are language-neutral (one copy serves all
languages). PNGs are stored as-is (already compressed); SVGs gzipped.

---

## 10. Storage schema

Two **separate** gitignored SQLite files (keeps the lean 33 MB `semantic.db` untouched; images are an
optional heavy layer that can be omitted for a text-only build):

**`klartext-docs.db`** (rendered text + search):
```sql
doc(infoobject_id INT PK, infotype TEXT, docnumber TEXT, title_de TEXT, title_en TEXT);
doc_variant(infoobject_id INT, chassis TEXT, body_md_gz BLOB,   -- per-chassis rendered body
            PRIMARY KEY(infoobject_id, chassis));               -- chassis='*' = universal (one variant)
doc_applicability(infoobject_id INT, chassis TEXT);             -- runtime membership filter; '*' matches any car
doc_graphic(infoobject_id INT, chassis TEXT, blob_id INT, seq INT);  -- refs used by a variant
fkb_body(content_dede INT PK, body_md_gz BLOB);                 -- fault-spine, keyed by Item 4 pointer
doc_fts USING fts5(title, body, content='');                   -- offline search over rendered text
```
**`klartext-graphics.db`** (optional): `graphic(blob_id INT PK, format TEXT, bytes BLOB)`.

`fault_help` (Item 4) already maps `(address, code) → content_dede`; Tier-1 lookup joins that to
`fkb_body`. Tier-2 is reached via `doc_fts`/`doc_applicability`.

---

## 11. Build pipeline

Extends `scripts/build-semantic-db.sh` (the only place the encrypted DB + SQLite3MC live). New stage,
gated by a **coverage argument** (`--coverage legacy | f2x | all`, default `f2x`):
1. Decrypt `DiagDocDb` (as today).
2. Select included docs per type + coverage (IDENTIFIER filters; MOD via applicability parse).
3. For each doc: extract body from `xmlvalue_DEDE`; for MOD REP, scope fragments per chassis (§7);
   render to markdown (§8); collect `GRAPHIC` blob ids.
4. Resolve + dedup + whitelist graphics (§9); write `klartext-graphics.db`.
5. Write `klartext-docs.db` tables; build the FTS index.
6. Emit sizes to stderr (log what each coverage tier produced; **log any doc/type skipped**, per the
   no-silent-caps rule).
Performance note: `xmlvalue` FTS5 `c0` is unindexed — build a `c0→rowid` map once (~0.4 s over 328k
rows), fetch bodies by rowid PK. Full REP pass ≈ 5 min.

---

## 12. Runtime API (`klartext-semantic`)

Read-only, bundled rusqlite. Loads `klartext-docs.db` (and `klartext-graphics.db` if present) when
configured; degrades gracefully when absent (today's pointer behavior).
- `Catalog::fault_body(address, code) -> Option<String>` — Tier-1 FKB markdown (via `fault_help`
  pointer → `fkb_body`).
- `Catalog::document(infoobject_id, chassis) -> Option<RenderedDoc>` — Tier-2 body + graphic refs.
- `Catalog::search(query, chassis) -> Vec<DocHit>` — FTS over rendered text, filtered by applicability.
- `Catalog::graphic(blob_id) -> Option<(format, bytes)>`.

---

## 13. Surfaces

**CLI** (`klartext`): `fault-docs <code>` now prints the **FKB body** (was pointer/title only);
new `search <query>` (FTS, car-filtered) and `doc <docnumber>` (render a procedure; `--export-images
<dir>` writes referenced PNGs/SVGs and rewrites refs to file paths for viewing). All offline, no car.

**MCP** (`klartext-mcp`): `fault_help` returns the FKB **body text** (plus existing title/pointer);
new `search_docs(query)` returns hits (title, docnumber, snippet). Images are returned as
`graphic:<blob-id>` refs (text protocol); a `get_graphic` tool can return a path/base64 if needed.
All read-only, autonomous-safe (per the M9/M10 invariant — no new write surface).

---

## 14. BYO-data & safety invariants (unchanged)

- Runtime reads only plaintext, self-built DBs (`semantic.db`, `klartext-docs.db`,
  `klartext-graphics.db`). The encrypted `DiagDocDb` + SQLite3MC + password stay **only** in
  `scripts/build-semantic-db.sh`. No ISTA content, no VIN, no blobs are ever committed or embedded.
- All new features are **pure reads**, offline, no car → autonomous/MCP-safe. No write surface added;
  the M9/M10 blast-radius invariant holds.
- `embed-data` (M11 Item 3) synergy: the legacy-floor tier (~280 MB) is a viable `include_bytes!`
  payload; the coverage dial makes the embed size explicit. Personal-use only, never published.

---

## 15. Testing

Everything here is offline and unit-testable; **no hardware round-trip is claimed**.
- **Renderers** (FKB, REP MOD, REP legacy, FUB/TED): fixture XML bodies → expected markdown vectors.
- **`VALIDITYINFO` evaluator**: expression strings → expected chassis sets (Mode A) and boolean
  results under given attribute bindings (Mode B), incl. `NOT`/`AND`/`OR` and universal cases.
- **Graphics resolver**: known `(infoobject, LINKID)` → expected blob id (structural), against a small
  fixture extracted at test time from the user's DB (not committed).
- **Applicability extractor**: IDENTIFIER samples per type → expected chassis token / universal.
- **Store round-trip**: build a tiny fixture DB, assert `fault_body`/`document`/`search` return
  expected content; assert graceful degradation when the doc DBs are absent.
- `cargo fmt` + `cargo clippy -D warnings` clean before done.

---

## 16. Implementation phasing (for the plan) & open items

Suggested order (each phase independently verifiable, offline):
1. **FKB spine** — `fkb_body` extract + FKB renderer + `fault_body` API + CLI/MCP wiring. Ships the
   fault→meaning→measure win at ~30 MB, no graphics, no evaluator.
2. **Storage + FTS + search surface** — `klartext-docs.db` schema, FTS index, `search` CLI/MCP.
3. **REP legacy** — legacy renderer + IDENTIFIER applicability + `document` API (the 2.1 MB floor).
4. **Graphics pipeline** — structural resolver + `klartext-graphics.db` + image refs/export.
5. **`VALIDITYINFO` evaluator + MOD REP** — the full-coverage tier + per-chassis rendering + dial.
6. **FUB/TED/SSP/tier-2** — per-type renderers + per-type IDENTIFIER scoping.

**Open items (resolve during implementation, all small):**
- Per-type IDENTIFIER chassis conventions for FUB/TED/SSP/PIB/AZD/REH/SIT/STA/SWZ (a short RE per type).
- Exact F20/F-series image payload for the MOD tier (enumerate the scoped doc set through the
  structural resolver at build; log it).
- Engine/option-level runtime precision (Mode B beyond chassis) — deferred refinement.
- SSP wiring-diagram surfacing (index + how the CLI/MCP presents an SVG) — thin, define in phase 6.

---

## 17. Deferred to Item 5 (recorded for the drop-in)

fault→procedure decision tree, `DiagobjDocumentLink` traversal, `XEP_QUERYOBJECTS` precondition/branch
logic, ABL `FLOWXML` execution. When built, these add fault→doc edges over the rows this store already
produces — no migration.
