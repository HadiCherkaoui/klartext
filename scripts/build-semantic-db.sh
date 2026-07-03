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
# 24-bit code) → text; ecu maps diagnostic address → variant.
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
CREATE INDEX sem.idx_dtc_lookup ON dtc(address, code);
CREATE INDEX sem.idx_ecu_addr ON ecu(address);
SQL

echo "Done. $(du -h "$OUT" | cut -f1) → $OUT"
