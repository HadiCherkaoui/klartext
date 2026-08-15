#!/usr/bin/env bash
# Build klartext's semantic database from a user-supplied ISTA DiagDocDb.
#
# ISTA ships its diagnostic databases encrypted. This script decrypts the user's
# own DiagDocDb (BYO-data) and extracts only the small set of tables klartext
# needs into a compact, plaintext SQLite at data/klartext-semantic.db, which
# klartext-semantic then reads read-only via rusqlite.
#
# The cipher and password were recovered from the user's own ISTA install (see
# docs/sqlite-findings.md): System.Data.SQLite's legacy "rc4" codec, password =
# the ISTAGUI.exe public-key-token. Neither the encrypted DB nor the decrypted
# output is ever committed (data/ is gitignored).
#
# Decryption uses SQLite3 Multiple Ciphers (utelle/SQLite3MultipleCiphers), built
# here from a pinned, checksum-verified amalgamation — no system SQLite codec or
# external package is required, only a C compiler.
#
# Usage:
#   scripts/build-semantic-db.sh [path/to/DiagDocDb.sqlite] [path/to/out.db]
# Env overrides: KLARTEXT_DIAGDOC, KLARTEXT_SEMANTIC_DB, KLARTEXT_DB_PASSWORD.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

SRC="${1:-${KLARTEXT_DIAGDOC:-$REPO_ROOT/data/Testmodule(1)/SQLiteDBs/DiagDocDb.sqlite}}"
OUT="${2:-${KLARTEXT_SEMANTIC_DB:-$REPO_ROOT/data/klartext-semantic.db}}"
# Default password is the ISTAGUI.exe public-key-token (a public strong-name
# token, not a secret); override if your ISTA build differs.
PASSWORD="${KLARTEXT_DB_PASSWORD:-6505EFBDC3E5F324}"

# Pinned SQLite3MC amalgamation (matches system SQLite 3.53.x).
MC_VERSION="2.3.5"
MC_SQLITE="3.53.2"
MC_ZIP="sqlite3mc-${MC_VERSION}-sqlite-${MC_SQLITE}-amalgamation.zip"
MC_URL="https://github.com/utelle/SQLite3MultipleCiphers/releases/download/v${MC_VERSION}/${MC_ZIP}"
MC_SHA256="4533dcdf82b9b0f00173067be2eee8fc42010f2678a1c1ed63434f7cedfbe5d3"

CACHE="${XDG_CACHE_HOME:-$HOME/.cache}/klartext-sqlite3mc/${MC_VERSION}"
MC_BIN="$CACHE/sqlite3mc"

build_sqlite3mc() {
	[ -x "$MC_BIN" ] && return 0
	echo "Building SQLite3MC ${MC_VERSION} (one-time) in $CACHE …"
	mkdir -p "$CACHE"
	curl -fsSL -o "$CACHE/$MC_ZIP" "$MC_URL"
	echo "${MC_SHA256}  $CACHE/$MC_ZIP" | sha256sum -c - >/dev/null
	unzip -oq "$CACHE/$MC_ZIP" -d "$CACHE/src"
	cc -O2 -DSQLITE_THREADSAFE=0 -DSQLITE_ENABLE_FTS5 \
		"$CACHE/src/sqlite3mc_amalgamation.c" "$CACHE/src/shell3mc_amalgamation.c" \
		-o "$MC_BIN" -lpthread -ldl -lm
}

[ -f "$SRC" ] || {
	echo "error: DiagDocDb not found at: $SRC" >&2
	exit 1
}
build_sqlite3mc
mkdir -p "$(dirname "$OUT")"
rm -f "$OUT"

echo "Extracting semantic tables from $SRC → $OUT …"
# Source opened immutable (never modified); output attached with empty key
# (plaintext). The dtc table denormalises the ISTA fault model to (address, raw
# 24-bit code) → text; ecu maps diagnostic address → variant. The measurement
# table is ISTA's per-variant readable-value catalog (the "index") — the result
# name + unit + linear scaling + owning job + ISTA's own title (the fleet-wide
# semantic key: present on all 136,885 XEP_ECURESULTS rows, where the EDIABAS
# name's spelling varies per ECU), denormalised from XEP_ECURESULTS through the
# ECU function tree (var-function → func-structure → fixed-function), keyed by
# the variant (.prg) name. ~50k rows over ~1280 variants. The job_param
# table is the invocation half of that index: per fixed function (an ISTA UI
# action, with its human title), the EDIABAS job(s) it calls and the positional
# P1..Pn argument values (';'-joined = the argument buffer), with the actuation
# phase (Main/Preset/Reset) and the job's rank within that phase. A phase can
# run more than one job (e.g. "enter diagnose mode, THEN actuate") — ISTA's
# own execution order (RheingoldSessionController.DoTriggerComponent runs
# GetJobsByPhase(phase).OrderBy(x => x.Rank)). phase/rank come from
# XEP_REFECUJOBS — the JOB's own record, authoritative over the parameter
# ref's phase (XEP_REFECUPARAMETERS.PHASE), which agrees wherever both exist
# but isn't where ISTA reads phase from. ~64k rows. The fixed_function table
# carries each service function's post-Main hold parameters — ACTIVATION (>0 =
# timed hold; 0 with Reset jobs = hold until an explicit stop) and
# ACTIVATION_DURATION_MS — plus the PREPARING/PROCESSING/POST operator text ISTA
# shows the technician in place of a machine-checked precondition
# (RheingoldSessionController.DoTriggerComponent). The operator text is a DIRECT
# *OPERATORTEXT_ENGB column on XEP_ECUFIXEDFUNCTIONS (not a content ref; ENGB and
# DEDE are equally populated, so English loses no rows). Keyed by function id, one
# row per fixed function that schedules >=1 job (XEP_REFECUJOBS). ~35.5k rows.
"$MC_BIN" "file:${SRC}?immutable=1" \
	-cmd "PRAGMA cipher='rc4';" \
	-cmd "PRAGMA key='${PASSWORD}';" <<SQL
ATTACH DATABASE '${OUT}' AS sem KEY '';
CREATE TABLE sem.ecu AS
  SELECT DISTINCT g.DIAGNOSTIC_ADDRESS AS address, v.NAME AS variant, g.NAME AS group_name,
         v.TITLE_ENGB AS title_en, v.TITLE_DEDE AS title_de
  FROM XEP_ECUVARIANTS v JOIN XEP_ECUGROUPS g ON g.ID = v.ECUGROUPID;
CREATE TABLE sem.dtc AS
  SELECT DISTINCT g.DIAGNOSTIC_ADDRESS AS address, v.NAME AS ecu_variant,
         CAST(fc.CODE AS INTEGER) AS code, l.SAECODE AS saecode,
         l.TITLE_DEDE AS title_de, l.TITLE_ENGB AS title_en
  FROM XEP_FAULTCODES fc
  JOIN XEP_ECUVARIANTS v ON v.ID = fc.ECUVARIANTID
  JOIN XEP_ECUGROUPS g ON g.ID = v.ECUGROUPID
  JOIN XEP_REFFAULTLABELS r ON r.ID = fc.ID
  JOIN XEP_FAULTLABELS l ON l.ID = r.LABELID
  WHERE COALESCE(l.TITLE_ENGB, l.TITLE_DEDE) IS NOT NULL;
CREATE TABLE sem.virtual_fault AS
  -- ISTA's SYNTHETIC fault entries: when an ECU does not answer at all, or has a
  -- programming error, it inserts a real fault-list entry rather than only logging
  -- (VehicleIdent AddVirtualErrorCodesIfNeeded / HandleVirtualErrorCodes). The
  -- lookup key is (answer state, ECU group): state 1 = the ECU answered nothing
  -- (!IDENT && !SVK && !FS), state 2 = a programming error, state 0 = the
  -- clamp-15-inactive case.
  SELECT DISTINCT g.NAME              AS group_name,
         CAST(v.ECUNOANSWER AS INTEGER) AS answer_state,
         v.CODE                       AS code,
         l.TITLE_ENGB                 AS title_en,
         l.TITLE_DEDE                 AS title_de
  FROM XEP_VIRTUALFAULTCODES v
  JOIN XEP_ECUGROUPS g ON g.ID = v.PARENTID
  LEFT JOIN XEP_REFFAULTLABELS r ON r.ID = v.ID
  LEFT JOIN XEP_FAULTLABELS   l ON l.ID = r.LABELID
  WHERE v.CODE IS NOT NULL;
CREATE TABLE sem.envcond AS
  SELECT DISTINCT CAST(UWIDENT AS INTEGER) AS uwnr, UNIT AS unit,
         TITLE_ENGB AS title_en, TITLE_DEDE AS title_de,
         (NODECLASS = 5658114) AS is_status
  FROM XEP_ENVCONDSLABELS
  WHERE UWIDENTTYP = 'UW-Nummer' AND UWIDENT GLOB '[0-9]*'
    AND COALESCE(TITLE_ENGB, TITLE_DEDE) IS NOT NULL;
CREATE TABLE sem.fault_doc AS
  SELECT DISTINCT g.DIAGNOSTIC_ADDRESS AS address,
         CAST(fc.CODE AS INTEGER)      AS code,
         d.INFOOBJECTID                AS infoobject_id,
         d.CONTENT_ENGB                AS content_engb,
         d.CONTENT_DEDE                AS content_dede
  FROM XEP_FAULTCODES fc
  JOIN XEP_ECUVARIANTS v   ON v.ID = fc.ECUVARIANTID
  JOIN XEP_ECUGROUPS   g   ON g.ID = v.ECUGROUPID
  JOIN RG_ECUFAULT_DOCIDS d ON d.ECUFAULT_ID = fc.ID
  WHERE d.INFOOBJECTID IS NOT NULL AND g.DIAGNOSTIC_ADDRESS IS NOT NULL;
CREATE TABLE sem.infoobject AS
  SELECT DISTINCT io.ID           AS id,
         io.INFOTYPE              AS infotype,
         io.DOCNUMBER             AS docnumber,
         io.SICHERHEITSRELEVANT   AS safety_relevant,
         io.TITLE_ENGB            AS title_en,
         io.TITLE_DEDE            AS title_de
  FROM XEP_INFOOBJECTS io
  WHERE (io.ID IN (SELECT INFOOBJECTID FROM RG_ECUFAULT_DOCIDS WHERE INFOOBJECTID IS NOT NULL)
         -- Also every document the test-plan spine below can reach, so a
         -- diag_info row resolves to a title instead of dangling.
         OR io.ID IN (SELECT INFOOBJECTID FROM XEP_REFINFOOBJECTS))
    AND COALESCE(io.TITLE_ENGB, io.TITLE_DEDE) IS NOT NULL;
CREATE TABLE sem.repair_doc AS
  -- The REPAIR document families, which the fault-linked infoobject extract above
  -- cannot reach: it bridges from a fault code (RG_ECUFAULT_DOCIDS), and only FKB
  -- hangs off that bridge. These are indexed by component and procedure instead, so
  -- they are extracted on their own and searched by title.
  --   REP repair instructions ("Nockenwelle ausbauen")
  --   EBO component/fuse locations
  --   SWZ special tools
  --   FUB function-test instructions ("Bauteilprüfung Drosselklappenschalter") —
  --       what the test-plan spine below points a fault at. A different root
  --       element (DIAGNOSISDOCUMENT, not REPAIRMANUALDOCUMENT); the renderer
  --       handles both. ABL (the other spine family) has no body here at all —
  --       it is an executable ISTA test module, not a document, so it stays
  --       title-only.
  SELECT DISTINCT I.ID                            AS id,
         I.INFOTYPE                               AS infotype,
         I.DOCNUMBER                              AS docnumber,
         I.SICHERHEITSRELEVANT                    AS safety_relevant,
         I.TITLE_ENGB                             AS title_en,
         I.TITLE_DEDE                             AS title_de,
         CAST(C.CONTENT_DEDE AS INTEGER)          AS content_dede,
         CAST(C.CONTENT_ENGB AS INTEGER)          AS content_engb
  FROM XEP_INFOOBJECTS I
  JOIN XEP_REFCONTENTS R ON R.ID = I.CONTROLID
  JOIN XEP_IOCONTENTS  C ON C.CONTROLID = R.CONTENTCONTROLID
  WHERE I.INFOTYPE IN ('REP', 'EBO', 'SWZ', 'FUB')
    AND COALESCE(I.TITLE_ENGB, I.TITLE_DEDE) IS NOT NULL;
-- ISTA's test-plan spine: what it puts in front of the technician once a fault is
-- read or a customer complaint is picked. Transcribed from the shipped provider,
-- BMW.Rheingold.Data.ConWoyConnector.ConWoyDataProviderSQLite:
--   GetDiagObjectsByFaultCode          (:916)  fault  -> XEP_REFDIAGOBJECTS
--   GetDiagAndInfoObjectsByPerceivedSymptomsId (:7365) symptom -> XEP_REFDIAGOBJECTS
--   GetInfoObjectsByDiagObjectControlId (:3449) diag object -> documents
-- Both entry points hit XEP_REFDIAGOBJECTS by bare ID, which is only sound because
-- the fault-code and symptom ID spaces are disjoint (verified: zero intersection).
-- Two gates in ISTA's path are NOT reproduced here and cannot be, so these tables
-- are the UNFILTERED candidate set:
--   EvaluateXepRulesById -- the XEP_RULES offline-fitment engine (a logic port, not
--                           data; a known-open item)
--   IsDiagObjectValid    -- per-vehicle validity
-- Callers must present the result as "what ISTA would consider", not "what ISTA
-- would show this car".
CREATE TABLE sem.diag_object AS
  -- One diagnostic step: a test, a check, a procedure. VERSTECKT = 0 is ISTA's own
  -- filter on every non-getHidden query.
  SELECT DISTINCT d.CONTROLID              AS control_id,
         d.NAME                            AS name,
         CAST(d.FAILUREWEIGHT AS INTEGER)  AS failure_weight,
         d.SICHERHEITSRELEVANT             AS safety_relevant,
         NULLIF(d.GROBZEICHEN, '')         AS grobzeichen,
         NULLIF(d.TITLE_ENGB, '')          AS title_en,
         NULLIF(d.TITLE_DEDE, '')          AS title_de
  FROM XEP_DIAGNOSISOBJECTS d
  -- CONTROLID 0 is a sentinel, not a step: nine rows share it, and nothing in the
  -- link tables points at it.
  WHERE d.VERSTECKT = 0 AND d.CONTROLID IS NOT NULL AND d.CONTROLID <> 0;
CREATE TABLE sem.fault_test_plan AS
  -- The fault entry point, keyed the way klartext reads faults off the car
  -- (diagnostic address + 24-bit code) rather than by ISTA's internal ids.
  -- SURESUSPICION marks the step ISTA treats as the confirmed cause rather than a
  -- candidate; PRIORITY is its running order.
  --
  -- ecu_variant is NOT decoration. ISTA scopes this lookup by ECUVARIANTID
  -- (GetDiagObjectsByFaultCode: "WHERE CODE = @code AND ECUVARIANTID = @ecuvariantid"),
  -- and it has to: 153 variants share diagnostic address 0x12, so an address-keyed
  -- plan pulls M54 petrol ignition steps into an N47 diesel's fault. Query scoped
  -- by variant wherever one is known.
  SELECT DISTINCT g.DIAGNOSTIC_ADDRESS         AS address,
         v.NAME                                AS ecu_variant,
         CAST(fc.CODE AS INTEGER)              AS code,
         rd.DIAGNOSISOBJECTCONTROLID           AS control_id,
         CAST(rd.PRIORITY AS INTEGER)          AS priority,
         CAST(rd.SURESUSPICION AS INTEGER)     AS sure_suspicion
  FROM XEP_FAULTCODES fc
  JOIN XEP_ECUVARIANTS v    ON v.ID = fc.ECUVARIANTID
  JOIN XEP_ECUGROUPS   g    ON g.ID = v.ECUGROUPID
  JOIN XEP_REFDIAGOBJECTS rd ON rd.ID = fc.ID
  WHERE g.DIAGNOSTIC_ADDRESS IS NOT NULL;
CREATE TABLE sem.symptom AS
  -- The customer-complaint tree ("engine juddering"), PARENTID-nested. SELECTABLE
  -- marks a leaf the technician may actually pick; WEIGHTING is ISTA's sort key.
  SELECT DISTINCT s.ID                     AS id,
         s.PARENTID                        AS parent_id,
         CAST(s.WEIGHTING AS INTEGER)      AS weighting,
         CAST(s.SELECTABLE AS INTEGER)     AS selectable,
         s.SICHERHEITSRELEVANT             AS safety_relevant,
         NULLIF(s.TITLE_ENGB, '')          AS title_en,
         NULLIF(s.TITLE_DEDE, '')          AS title_de
  FROM XEP_PERCEIVEDSYMPTOMS s
  WHERE COALESCE(s.TITLE_ENGB, s.TITLE_DEDE) IS NOT NULL;
CREATE TABLE sem.symptom_test_plan AS
  SELECT DISTINCT rd.ID                        AS symptom_id,
         rd.DIAGNOSISOBJECTCONTROLID           AS control_id,
         CAST(rd.PRIORITY AS INTEGER)          AS priority,
         CAST(rd.SURESUSPICION AS INTEGER)     AS sure_suspicion
  FROM XEP_REFDIAGOBJECTS rd
  WHERE rd.ID IN (SELECT ID FROM XEP_PERCEIVEDSYMPTOMS);
CREATE TABLE sem.diag_info AS
  -- Diagnostic step -> the documents that describe it. Resolves against
  -- sem.infoobject for the title and sem.repair_doc for a renderable body.
  SELECT DISTINCT ri.ID           AS control_id,
         ri.INFOOBJECTID          AS infoobject_id
  FROM XEP_REFINFOOBJECTS ri
  WHERE ri.INFOOBJECTID IS NOT NULL;
CREATE TABLE sem.measurement AS
  SELECT DISTINCT vf.NAME AS ecu_variant, r.NAME AS name,
         NULLIF(r.UNIT, '')                        AS unit,
         CAST(NULLIF(r.MULTIPLIKATOR, '') AS REAL) AS mul,
         CAST(NULLIF(r.OFFSET, '') AS REAL)        AS offset,
         CAST(NULLIF(r.RUNDEN, '') AS INTEGER)     AS round,
         NULLIF(r.ZAHLENFORMAT, '')                AS zahlenformat,
         j.NAME AS job,
         NULLIF(r.TITLE_ENGB, '')                  AS title_en,
         NULLIF(r.TITLE_DEDE, '')                  AS title_de
  FROM XEP_ECUVARFUNCTIONS vf
  JOIN XEP_REFECUFUNCSTRUCTS rfs ON rfs.ID = vf.ID
  JOIN XEP_ECUFIXEDFUNCTIONS ff  ON ff.PARENTID = rfs.ECUFUNCSTRUCTID
  JOIN XEP_REFECURESULTS    rr   ON rr.ID = ff.ID
  JOIN XEP_ECURESULTS       r    ON r.ID = rr.ECURESULTID
  LEFT JOIN XEP_ECUJOBS     j    ON j.ID = r.ECUJOBID
  WHERE r.NAME IS NOT NULL;
CREATE TABLE sem.job_param AS
  SELECT DISTINCT vf.NAME AS ecu_variant,
         ff.ID                               AS function_id,
         NULLIF(ff.TITLE_ENGB, '')           AS function_en,
         NULLIF(ff.TITLE_DEDE, '')           AS function_de,
         rj.PHASE                            AS phase,
         rj.RANK                             AS rank,
         CAST(SUBSTR(p.NAME, 2) AS INTEGER)  AS position,
         NULLIF(p.PARAMVALUE, '')            AS value,
         NULLIF(p.FUNCTIONNAMEPARAMETER, '') AS label,
         j.NAME                              AS job
  FROM XEP_ECUVARFUNCTIONS vf
  JOIN XEP_REFECUFUNCSTRUCTS rfs ON rfs.ID = vf.ID
  JOIN XEP_ECUFIXEDFUNCTIONS ff  ON ff.PARENTID = rfs.ECUFUNCSTRUCTID
  JOIN XEP_REFECUPARAMETERS rp   ON rp.ID = ff.ID
  JOIN XEP_ECUPARAMETERS p       ON p.ID = rp.ECUPARAMETERID
  JOIN XEP_ECUJOBS j             ON j.ID = p.ECUJOBID
  JOIN XEP_REFECUJOBS rj         ON rj.ID = ff.ID AND rj.ECUJOBID = p.ECUJOBID
  WHERE p.NAME GLOB 'P*';
CREATE TABLE sem.fixed_function AS
  SELECT DISTINCT ff.ID                              AS function_id,
         CAST(ff.ACTIVATION AS INTEGER)              AS activation,
         CAST(ff.ACTIVATION_DURATION_MS AS INTEGER)  AS activation_duration_ms,
         NULLIF(ff.PREPARINGOPERATORTEXT_ENGB, '')   AS preparing_text,
         NULLIF(ff.PROCESSINGOPERATORTEXT_ENGB, '')  AS processing_text,
         NULLIF(ff.POSTOPERATORTEXT_ENGB, '')        AS post_text
  FROM XEP_ECUFIXEDFUNCTIONS ff
  WHERE ff.ID IN (SELECT ID FROM XEP_REFECUJOBS);
CREATE TABLE sem.bordnet_doc AS
  SELECT DISTINCT SUBSTR(I.IDENTIFIER, 9)   AS series,
         CAST(C.CONTENT_DEDE AS INTEGER)    AS doc_id
  FROM XEP_INFOOBJECTS I
  JOIN XEP_REFCONTENTS R ON R.ID = I.CONTROLID
  JOIN XEP_IOCONTENTS  C ON C.CONTROLID = R.CONTENTCONTROLID
  WHERE I.IDENTIFIER LIKE 'BNT-XML-%' AND C.CONTENT_DEDE IS NOT NULL;
CREATE INDEX sem.idx_dtc_lookup ON dtc(address, code);
CREATE INDEX sem.idx_ecu_addr ON ecu(address);
CREATE INDEX sem.idx_envcond ON envcond(uwnr);
CREATE INDEX sem.idx_virtual_fault ON virtual_fault(group_name, answer_state);
CREATE INDEX sem.idx_fault_doc ON fault_doc(address, code);
CREATE INDEX sem.idx_infoobject ON infoobject(id);
CREATE INDEX sem.idx_repair_doc ON repair_doc(infotype);
CREATE INDEX sem.idx_diag_object ON diag_object(control_id);
CREATE INDEX sem.idx_fault_test_plan ON fault_test_plan(address, code, ecu_variant);
CREATE INDEX sem.idx_symptom_parent ON symptom(parent_id);
CREATE INDEX sem.idx_symptom_test_plan ON symptom_test_plan(symptom_id);
CREATE INDEX sem.idx_diag_info ON diag_info(control_id);
CREATE INDEX sem.idx_measurement ON measurement(ecu_variant, name);
CREATE INDEX sem.idx_job_param ON job_param(ecu_variant, job);
CREATE INDEX sem.idx_fixed_function ON fixed_function(function_id);
SQL

echo "Done. $(du -h "$OUT" | cut -f1) → $OUT"

# Phase 1 doc store: render fault-description (FKB) bodies into a sibling
# klartext-docs.db. Reads only plaintext DBs (the semantic extract above + ISTA's
# xmlvalueprimitive_DEDE); no SQLite3MC needed here. BYO-data: output is gitignored.
XMLVALUE="${KLARTEXT_XMLVALUE_DEDE:-$(dirname "$SRC")/xmlvalueprimitive_DEDE.sqlite}"
# The language-neutral store holds the BNT-XML bordnet bodies (the ISTA ECU
# tree); when present, docbuild parses them into the semantic DB's ecu_tree.
XMLVALUE_OTHER="${KLARTEXT_XMLVALUE_OTHER:-$(dirname "$SRC")/xmlvalueprimitive_OTHER.sqlite}"
DOCS_OUT="$(dirname "$OUT")/klartext-docs.db"
if [ -f "$XMLVALUE" ]; then
	echo "Building doc store (FKB bodies) → $DOCS_OUT …"
	OTHER_ARGS=()
	[ -f "$XMLVALUE_OTHER" ] && OTHER_ARGS=(--xmlvalue-other-db "$XMLVALUE_OTHER")
	cargo run --quiet --release -p klartext-docbuild -- \
		--semantic-db "$OUT" --xmlvalue-db "$XMLVALUE" --out "$DOCS_OUT" "${OTHER_ARGS[@]}"
else
	echo "note: $XMLVALUE not found — skipping doc store (pointers/titles still work)." >&2
fi
