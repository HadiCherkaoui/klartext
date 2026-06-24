# ISTA data survey — Milestone 3, Phase 1 findings

Read-only exploration of the user-supplied ISTA dataset to locate the semantic data
the M3 layer needs: (a) DTC/fault text keyed by code+ECU, (b) DID/measurement
definitions with scaling+unit, (c) the ECU name ↔ diagnostic-address map.

- **Dataset path (gitignored, BYO-data):** `data/Testmodule(1)/` — an ISTA export with
  `SQLiteDBs/`, `Ecu/`, `EcuFunctions/`, `Ediabas/`.
- **Method:** every DB opened `sqlite3 -readonly`; no contents copied into the repo or
  the binary. No car, no capture (gateway down) — offline survey only.
- **TL;DR:** the structured semantic tables (DTC text, DID/measurement defs, ECU map)
  are **not in accessible plaintext**. They live in `DiagDocDb.sqlite`, which is
  **encrypted** (not a SQLite file we can open). The plaintext DBs hold only *documents*
  (PDFs, GUI forms, repair procedures) keyed by opaque global IDs. DID **scaling is
  absent** from all plaintext data — it is compiled into the EDIABAS SGBD `.prg` files.
  **This is a fork; Phase 2 is blocked pending a decision (see end).**

## SQLiteDBs/ — file inventory and format

| File | Size | Format | Role |
|---|---|---|---|
| `DiagDocDb.sqlite` | 12 GB | **ENCRYPTED** | The master diagnostic-documentation DB (DTCs, DIDs, ECU model). **Locked.** |
| `ConWoyDb.sqlite` | 303 MB | **ENCRYPTED** | Secondary master (config/workshop). WAL-mode. **Locked.** |
| `xmlvalueprimitive_DEDE.sqlite` | 50 GB | plaintext SQLite | German document text (FTS5). Documents, not lookup tables. |
| `xmlvalueprimitive_OTHER.sqlite` | 599 MB | plaintext SQLite | Language-neutral GUI form XML (FTS5). |
| `streamdataprimitive_OTHER.sqlite` | 22 GB | plaintext SQLite | Binary attachments — **PDFs**, keyed by global ID. |
| `streamdataprimitive_DEDE.sqlite` | 328 KB | plaintext SQLite | 1 row (near-empty). |
| `streamdataprimitive_ENGB.sqlite` | 295 KB | plaintext SQLite | 0 data rows (empty shell). |

### The two master DBs are encrypted

`DiagDocDb.sqlite` and `ConWoyDb.sqlite` do not start with the SQLite magic
(`53 51 4C 69 74 65…` "SQLite format 3"). Both begin with an identical encrypted
header:

```
8E 9D 09 41 4F 37 D0 04 …   (identical 26-byte prefix in both files, then diverges)
```

`sqlite3 -readonly DiagDocDb.sqlite` → `file is not a database (26)`. This is ISTA's
known database encryption; ISTA decrypts these in memory at runtime. **We do not attempt
to decrypt BMW's proprietary data.** Everything the milestone primarily needs — the
DTC-code→text table, the DID/measurement definitions, and the ECU↔address map — lives
inside `DiagDocDb` and is therefore not readable here.

### The plaintext DBs hold documents, not lookup tables

Schema is the same RG-export shape in each:

- `streamdataprimitive` — `(id INTEGER, modified, deleted, stream BLOB)`. The `stream`
  blobs are whole **PDF files** (`%PDF-1.7…`) — repair/wiring documents. `id`s are
  global ISTA IDs (e.g. `2000088287332602`), resolvable only via the encrypted master.
- `xmlvalueprimitive` — FTS5 virtual table `(id, modified, deleted, data, compressed_data)`.
  `data` is XML. In `_OTHER` it is ISTA **`<PresentationForm>`** GUI layouts; in `_DEDE`
  it is documents whose root elements are:

  | Root element (DEDE, 4000-row sample) | count | kind |
  |---|---|---|
  | `<DIAGNOSISDOCUMENT>` | 1529 | diagnosis *procedures* (test plans), not code→text |
  | `<TIGHTENINGTORQUES>` | 1235 | torque specs |
  | `<SI-ENCLOSURE>` | 872 | service-info enclosures |
  | `<INTRODUCTION>` / `<SERVICEDOCUMENT>` / `<RegulationAndGuidelines>` | 364 | manuals |

  The German text body is large (FTS MATCH: "Kurzschluss" 31 187, "Fehlerspeicher"
  24 695, "Kühlmittel" 28 679) — but it is **prose keyed by global ID**. There is no
  `(DTC code, ECU) → description` table and no `(DID) → name/scaling` table here. The
  index that maps a DTC code or DID to one of these documents is in the encrypted master.
- `RG_VERSION_*` (version `4.58.30`, created 2026-03-05), `DELTA_INFO`, `sqlite_stat*` —
  metadata only.

## EcuFunctions/ — plaintext XML (partial measurement defs, NO scaling)

26 XML files. Two shapes:

- **`ArrayOfXEP_ECUFUNCSTRUCTURES`** (`eme_i1.xml`, `sme_82.xml`) — `XEP_ECUFIXEDFUNCTIONS`:
  fixed service functions ("Fehlerspeicher löschen" / "Delete fault memory") with
  multilingual operator texts. No DTCs, no measurements.
- **`ArrayOfXEP_ECUJOBSEX`** (24 numbered files) — the function→EDIABAS-job bridge. Each
  `XEP_ECURESULTSEX` gives:
  - a multilingual **measurement name** (`Title_dede`/`Title_engb`, e.g. "Battery transmitter 2"),
  - `Name` + `AdapterPath` — an **EDIABAS job result** path, e.g.
    `FBD02_UBAT_STAT_FLAG` / `/Result/Rows/Row[0]/FBD02_UBAT_STAT_FLAG`,
  - `Format` (String/Binary/Decimal/Hexadecimal), `Unit` (°C, V, bar, A, %) when fixed,
  - `StateValues` → `XEP_STATEVALUES`: discrete **value→text** enums
    (`Statevalue=0` → "in Ordnung"/"O.K.").

Two hard limits for our purpose:

1. **No scaling.** There are no `Mult`/`Offset`/`Factor`/`Scale` fields anywhere in these
   files. ISTA does not scale here — it calls an EDIABAS job that returns an
   *already-scaled* value, and the XML only labels it (unit + enum). The linear scaling
   lives in the SGBD `.prg`.
2. **Keyed by EDIABAS result name, not by UDS DID.** Nothing maps a raw UDS
   `22 XX XX` DID to one of these results (no `Identifier`/`DID`/`RDBI`/hex-DID tokens
   exist in the files). So even the *name* of a value read by raw DID is not reachable
   from here. Coverage is also a small subset (24 ECUs of 1405).

These files give good **state-enum decode** and **names/units for known EDIABAS results**,
but cannot, on their own, turn `(DID, raw bytes)` into a named/scaled value.

## Ecu/ — EDIABAS SGBD binaries (opaque) + address-keyed groups

- **1405 `.prg`** — EDIABAS SGBD ECU descriptions. Binary/encoded: `strings` on
  `sme_82.prg` yields only `@EDIABAS OBJECT` and `SME_82.B2V`. The jobs and the **scaling
  formulas live here**, compiled — extracting them needs a real EDIABAS SGBD
  interpreter/disassembler (the "separate path" the milestone names), not string-scraping.
- **427 `.grp`** — group files named by **diagnostic address**: `d_0010.grp` (ZGW 0x10),
  `d_0012.grp` (DME 0x12), `d_0040.grp` (CAS 0x40) — matching the protocol reference's
  address conventions. So an address→group hint exists in the filenames, but the
  authoritative ECU-name↔address map and the variant resolution are inside these binaries
  / the encrypted master.
- `Ediabas/BIN/` — the Windows EDIABAS runtime (api/ebas `*.dll`/`*.exe`, `EDIABAS.INI`)
  that *interprets* the `.prg` files. Tooling, not parseable data.

## What this means for the three M3 targets

| Need | Found? | Where |
|---|---|---|
| (a) DTC text by code+ECU | **No** (not in plaintext) | encrypted `DiagDocDb` |
| (b) DID name | **Partial / wrong key** | EcuFunctions has names keyed by EDIABAS result name, not UDS DID; full DID model in encrypted `DiagDocDb` |
| (b) DID scaling + unit | **No scaling** (units only, for some) | scaling compiled in SGBD `.prg`; full unit in encrypted `DiagDocDb` |
| (c) ECU name ↔ address | **Partial** | address hinted by `d_<addr>.grp` filenames; names in binaries / encrypted master |

Pure-ISO work that needs **no** DB and is unaffected: decoding the **DTC status byte**
into ISO 14229 flags, and naming the **ISO-standard identification DIDs** (0xF1xx — VIN
0xF190, etc.) from the report.

## Fork — Phase 2 blocked, decision pending

Both fork triggers in the milestone brief are hit, and the root cause is broader than
anticipated: the SQLite the milestone assumed would be the source is **encrypted**, and
scaling is in the **compiled SGBD**. So "read-faults prints human descriptions keyed by
code+ECU" and "read-did prints a named/scaled value keyed by UDS DID" cannot be built
from the data as it currently sits.

Options put to the user (see the question accompanying this report):

- **A — Provide a decrypted/plaintext `DiagDocDb`** (BYO-data; ISTA decrypts it at
  runtime). This is the only path to the milestone as written: DTC text + DID names +
  ECU map keyed from SQLite (scaling TBD — confirm whether it lives in `DiagDocDb` or
  still only in the SGBD once readable).
- **B — Ship the reduced ISO-only layer now**: ISO 14229 DTC status-flag decode +
  standard 0xF1xx DID names + raw values; defer all DB-keyed text and scaling until a
  readable DB exists. (This is the brief's "named-DID + raw value now, defer scaling"
  fallback, narrowed because DTC text/DID names also can't be keyed without the DB.)
- **C — Pursue the SGBD/EDIABAS `.prg` disassembly path** — large, separate effort.

**Chosen fork: _pending user decision_** (recorded here once answered).

## Update — decryption feasibility spike (user opted to try via ISTA binaries)

Decompiled the Rheingold .NET assemblies present in the dump (`Testmodule/`, via
`ilspycmd`). Result: **the approach is viable and the exact blocker is pinpointed.**

- The DBs are opened by a provider selected in
  `BMW.Rheingold.CoreFramework.DatabaseProvider.DatabaseProviderFactory`:
  `GetDatabaseProviderSQLite()` →
  `LoadAssembly("BMW.Rheingold.Data.ConWoyConnector.ConWoyDataProviderSQLite", "RheingoldConWoyDataConnector.dll")`.
  So the **connection string + DB password/codec live in `RheingoldConWoyDataConnector.dll`**,
  which is **not in this dump** (only the 9 core framework DLLs + ~22.8k `ABL_*` modules).
- The credential cipher *is* recovered: `ISTACryptography` = AES-256-CBC, key =
  `SHA384("jnn9yz70byims1qhiv0f")[0..32]`, IV = `[32..48]` — but this decrypts ISTA
  *credential strings*, not the DB file. Tested against `DiagDocDb` header (AES-256
  CBC and ECB): does **not** yield `SQLite format 3`, so the DB uses a different
  (System.Data.SQLite page-codec) scheme whose password is in the missing connector.
- The decrypted schema is confirmed (decompiled entity classes / `SqLiteDatabaseTables`
  enum): `RG_ECUFAULTS`, `RG_ECUFAULT_DOCIDS`, `XEP_FAULTCODES`, `XEP_FAULTLABELS`,
  `XEP_FAULTMODELABELS`, `XEP_FAULTCLASSES`, `XEP_ENVCONDSLABELS`, `XEP_COMBINEDFAULTS`,
  `XEP_ECUVARIANTS`, `XEP_ECUGROUPS`, `XEP_REF_DIAGCODE_ECU`, `XEP_ECUJOBS`,
  `XEP_ECURESULTS`, `XEP_ECUPARAMETERS`, … — exactly the DTC-text / DID / ECU-map model M3 needs.

**To unblock decryption I need from the user's ISTA install:**
`RheingoldConWoyDataConnector.dll` (decisive — holds the password/connection string)
and `System.Data.SQLite.dll` (to confirm the page codec/version), plus any
`*.exe.config`. Then: recover password+codec → decrypt the user's `DiagDocDb` locally
into gitignored `data/` → build M3 against it. (Scaling presence still TBD until the DB
is readable.)

### RESOLVED — decryption works; DB is fully readable

User supplied the ISTA `TesterGUI/` install. Decompiled
`RheingoldConWoyDataConnector.dll` → `BMW.Rheingold.Data.ConWoyConnector.SQLite.SQLiteConnectionMgr`.
The DBs are opened with **System.Data.SQLite 1.0.111** and `SQLiteConnection.SetPassword`:

```
connStr  = "data source=<file>;Read Only=True;FailIfMissing=True;cache_size=-80000;"
encrypted = config["...SQLiteConnector.IsDatabaseEncrypted"] (default TRUE for DiagDoc & ConWoy)
password = config["...SQLiteConnector.DatabasePassword"]  ?? GetEntryAssemblyKey()
GetEntryAssemblyKey() = uppercase-hex(public-key-token of the entry .exe = ISTAGUI.exe)
```

- **Password = `6505EFBDC3E5F324`** (ISTAGUI.exe public-key-token; no `DatabasePassword`
  config override exists in the dump). Not a secret — it's a public strong-name token,
  identical across all ISTA builds signed with this key.
- **Cipher = System.Data.SQLite legacy `rc4` codec.** Proven on Linux with
  **SQLite3 Multiple Ciphers** (utelle, v2.3.5 amalgamation): `PRAGMA cipher='rc4';
  PRAGMA key='6505EFBDC3E5F324';` opens `DiagDocDb` read-only — `sqlite_master` = 238
  objects. `aes128cbc`/`aes256cbc`/`sqlcipher`/`chacha20` all fail ("file is not a
  database"), confirming `rc4`.
- streamdataprimitive/xmlvalueprimitive use the same `SetPassword` plumbing but with
  `IsStreamDataDatabaseEncrypted`/`IsXmlDatabaseEncrypted` defaulting **false** — which is
  why those are plaintext (matches the survey above).

**What the decrypted `DiagDocDb` gives M3 (verified by reading the tables):**

| Need | Table(s) | Status |
|---|---|---|
| DTC text by code+ECU | `XEP_FAULTLABELS` (`CODE`, `SAECODE`, `TITLE_DEDE/ENGB/…`), `XEP_FAULTCODES` (`CODE`, `ECUVARIANTID`) | ✅ read-faults is fully DB-driven |
| ECU name ↔ diag address | `XEP_ECUGROUPS` (`NAME` `d_00XX`, `DIAGNOSTIC_ADDRESS`), `XEP_ECUVARIANTS` | ✅ e.g. `d_0012` → 18 (0x12 DME) |
| Measurement catalog | `XEP_ECURESULTS` (`NAME`, `TITLE_*`, `MULTIPLIKATOR`, `OFFSET`, `RUNDEN`, `UNIT`), `XEP_STATEVALUES`/`XEP_ENVCONDSLABELS` enums | ⚠ present incl. **scaling**, but keyed by EDIABAS job/result name |

**read-did caveat (unchanged by decryption):** the measurement catalog keys to EDIABAS
**job/result names** (`XEP_ECUJOBS.NAME` e.g. `STEUERN_DIGITAL`/`STATUS_…`,
`XEP_ECURESULTS.NAME` e.g. `STAT_MOTORTEMPERATUR_WERT`), and job params are component
codes (e.g. `MFBHA`), **not raw UDS DIDs**. The raw `22 <DID>` → result → physical-value
conversion lives in the SGBD `.prg`. So decoding an arbitrary raw DID off the wire is
still SGBD-bound; the DB's `MULTIPLIKATOR`/`OFFSET` is a *display* scaling on the
already-SGBD-decoded EDIABAS result. read-did from raw DIDs therefore remains
"ISO-standard 0xF1xx names (from the report) + raw bytes", plus we can expose the
measurement *catalog* (names/units/enums) from the DB.

Tooling used (all under scratchpad, **not** committed): `ilspycmd` (decompile),
SQLite3MC amalgamation compiled with gcc (read encrypted DB on Linux). The decompiled
ISTA source and the DB contents are **never** committed or embedded (CLAUDE.md).

**Chosen fork (resolved & built):** decryption solved → **read-faults builds fully from
the DB** (DTC text + ISO status flags + ECU map). **read-did** ships ISO-standard 0xF1xx
names + raw bytes; full raw-DID physical decode is deferred (SGBD path), so the
coolant-temp numeric sanity check is deferred with it (the DB *does* hold the scaling —
`XEP_ECURESULTS.MULTIPLIKATOR/OFFSET/UNIT`, e.g. `STAT_MOTORTEMPERATUR_WERT` ×1 +0 °C —
but it keys by EDIABAS result name, not raw UDS DID).

**Architecture (decided): Option B — decrypt to a compact plaintext extract.** Reading the
encrypted DB in place would need SQLite3MC wired into `libsqlite3-sys` (no turnkey crate;
fragile build), so per the project's "just works" rule we extract instead.
`scripts/build-semantic-db.sh` decrypts the user's `DiagDocDb` (System.Data.SQLite `rc4`
codec, password `6505EFBDC3E5F324`) via a pinned SQLite3MC amalgamation and writes a 33 MB
plaintext `data/klartext-semantic.db` (denormalized `dtc` + `ecu` tables; gitignored).
`klartext-semantic` reads it with plain `rusqlite` (bundled), read-only. The encrypted DB,
the decrypted extract, the decompiled ISTA assemblies, and the password are never committed
or embedded.

