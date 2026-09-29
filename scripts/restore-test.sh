#!/bin/sh
# The restore test for scripts/backup.sh dumps (bead 9.4). POSIX sh, like backup.sh.
#
#   scripts/restore-test.sh [dump]
#
# 1. Restore the dump (default: the newest in MAVERICK_BACKUP_DIR) into a new scratch
#    database on the server of MAVERICK_DATABASE_URL.
# 2. Check the rows: every table in the dump's .counts file (written by backup.sh in the
#    dump's own snapshot) must have exactly that many rows.
# 3. Run `maverick migrate` against the scratch database: the current binary must accept the
#    restored schema.
# 4. Drop the scratch database, pass or fail.
#
# Use a pg_restore of the server's major version. The postgres:16-alpine image matches the
# engine's Postgres 16, and the static `maverick` binary runs in it (docker/README.md).
#
# Environment:
#   MAVERICK_DATABASE_URL   required; the scratch database is created next to its database
#   MAVERICK_BACKUP_DIR     default /var/backups/maverick (to find the newest dump)
#   MAVERICK_BIN            default `maverick` (on PATH)
#   `maverick migrate` loads the full config, so MAVERICK_INTERNAL_TOKENS and
#   MAVERICK_CAPTURE_MASTER_KEY must be set too (any valid values).
#
# Logs: one JSON line per event on stdout. Exits 0 on pass, 1 on failure.
set -eu

DIR="${MAVERICK_BACKUP_DIR:-/var/backups/maverick}"
BIN="${MAVERICK_BIN:-maverick}"
COUNT_SQL="select table_name || ' ' || (xpath('/row/c/text()', query_to_xml(format('select count(*) as c from public.%I', table_name), false, true, '')))[1]::text from information_schema.tables where table_schema = 'public' and table_type = 'BASE TABLE' order by table_name;"

json_string() {
    printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr '\n\t' '  ')"
}

log() {
    level="$1"
    msg="$2"
    shift 2
    line="{\"time\":$(json_string "$(date -u +%Y-%m-%dT%H:%M:%SZ)"),\"level\":$(json_string "$level"),\"msg\":$(json_string "$msg")"
    while [ "$#" -ge 2 ]; do
        line="$line,$(json_string "$1"):$(json_string "$2")"
        shift 2
    done
    printf '%s}\n' "$line"
}

die() {
    log ERROR "restore test failed" error "$1"
    exit 1
}

[ -n "${MAVERICK_DATABASE_URL:-}" ] || die "MAVERICK_DATABASE_URL is not set"
URL="$MAVERICK_DATABASE_URL"

dump="${1:-}"
if [ -z "$dump" ]; then
    # The newest: the glob sorts by name, and the names sort by time.
    for candidate in "$DIR"/maverick-*.dump; do
        [ -e "$candidate" ] && dump="$candidate"
    done
    [ -n "$dump" ] || die "no dump in $DIR"
fi
[ -f "$dump" ] || die "no such dump: $dump"
counts="${dump%.dump}.counts"
[ -s "$counts" ] || die "no counts file next to the dump: $counts"

scratch="maverick_restore_test_$(date -u +%Y%m%d%H%M%S)_$$"
# The same URL with the database name replaced.
scratch_url="$(printf '%s' "$URL" | sed -E "s#^([a-z]+://[^/]*/)[^?]*#\1$scratch#")"
[ "$scratch_url" != "$URL" ] || die "cannot find the database name in MAVERICK_DATABASE_URL"

drop_scratch() {
    psql "$URL" -X -q -c "drop database if exists \"$scratch\" with (force)" >/dev/null 2>&1 || true
}

psql "$URL" -X -q -v ON_ERROR_STOP=1 -c "create database \"$scratch\"" >/dev/null \
    || die "cannot create the scratch database $scratch"
trap drop_scratch EXIT
log INFO "restoring" dump "$dump" database "$scratch"

pg_restore --no-owner --no-privileges --exit-on-error --dbname="$scratch_url" "$dump" \
    || die "pg_restore failed"

restored="$(psql "$scratch_url" -X -A -t -q -v ON_ERROR_STOP=1 -c "$COUNT_SQL")" \
    || die "cannot count the restored rows"
checked=0
mismatches=""
while read -r table expected; do
    [ -n "$table" ] || continue
    got="$(printf '%s\n' "$restored" | awk -v t="$table" '$1 == t { print $2 }')"
    if [ "$got" != "$expected" ]; then
        mismatches="$mismatches $table:$expected->${got:-missing}"
    fi
    checked=$((checked + 1))
done <"$counts"
[ -z "$mismatches" ] || die "row counts differ:$mismatches"
log INFO "row counts match" tables "$checked"

MAVERICK_DATABASE_URL="$scratch_url" "$BIN" migrate || die "maverick migrate failed on the restored database"
log INFO "restore test passed" dump "$dump" tables "$checked"
