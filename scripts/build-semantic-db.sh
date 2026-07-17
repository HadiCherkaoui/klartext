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
# name + unit + linear scaling + owning job, denormalised from XEP_ECURESULTS
# through the ECU function tree (var-function → func-structure → fixed-function),
# keyed by the variant (.prg) name. ~50k rows over ~1280 variants. The job_param
# table is the invocation half of that index: per fixed function (an ISTA UI
# action, with its human title), the EDIABAS job it calls and the positional
# P1..Pn argument values (';'-joined = the argument buffer), with the actuation
# phase (Main/Preset/Reset). ~65k rows via XEP_REFECUPARAMETERS.
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
  WHERE io.ID IN (SELECT INFOOBJECTID FROM RG_ECUFAULT_DOCIDS WHERE INFOOBJECTID IS NOT NULL)
    AND COALESCE(io.TITLE_ENGB, io.TITLE_DEDE) IS NOT NULL;
CREATE TABLE sem.measurement AS
  SELECT DISTINCT vf.NAME AS ecu_variant, r.NAME AS name,
         NULLIF(r.UNIT, '')                        AS unit,
         CAST(NULLIF(r.MULTIPLIKATOR, '') AS REAL) AS mul,
         CAST(NULLIF(r.OFFSET, '') AS REAL)        AS offset,
         CAST(NULLIF(r.RUNDEN, '') AS INTEGER)     AS round,
         NULLIF(r.ZAHLENFORMAT, '')                AS zahlenformat,
         j.NAME AS job
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
         rp.PHASE                            AS phase,
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
  WHERE p.NAME GLOB 'P*';
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
CREATE INDEX sem.idx_fault_doc ON fault_doc(address, code);
CREATE INDEX sem.idx_infoobject ON infoobject(id);
CREATE INDEX sem.idx_measurement ON measurement(ecu_variant, name);
CREATE INDEX sem.idx_job_param ON job_param(ecu_variant, job);
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
