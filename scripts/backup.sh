#!/bin/sh
# A Postgres backup of the engine database (bead 9.4). POSIX sh: it runs in the
# postgres:16-alpine backup accessory (busybox ash) and on a host (dash or bash).
#
#   scripts/backup.sh            one backup now
#   scripts/backup.sh --daemon   one backup every day at MAVERICK_BACKUP_AT (UTC)
#
# One run writes, in MAVERICK_BACKUP_DIR:
#   maverick-<UTC time>.dump     pg_dump in the custom format (restore with pg_restore)
#   maverick-<UTC time>.counts   `<table> <rows>` for every public table, counted in the
#                                dump's own snapshot, so a restore can be checked exactly
#                                (scripts/restore-test.sh)
# Then it keeps the newest MAVERICK_BACKUP_KEEP dumps.
#
# Off the box: when MAVERICK_BACKUP_TARGET is set (an rclone remote path such as
# `offsite:maverick-backups`, configured through RCLONE_CONFIG_* secrets), both files are
# copied there with rclone. When it is unset, the run logs a warning: the dump then exists
# on this host only.
#
# Environment:
#   MAVERICK_DATABASE_URL   required; the database to dump
#   MAVERICK_BACKUP_DIR     default /var/backups/maverick
#   MAVERICK_BACKUP_KEEP    default 7
#   MAVERICK_BACKUP_TARGET  default unset (see above)
#   MAVERICK_BACKUP_AT      default 02:30 (--daemon only; UTC HH:MM)
#   MAVERICK_BACKUP_INSTALL_RCLONE=1   --daemon installs rclone with apk when a target is
#                           set and rclone is missing (the Kamal accessory sets it)
#
# Logs: one JSON line per event on stdout, like the engine. A failed run exits 1 (the
# daemon logs it and waits for the next day).
set -eu

DIR="${MAVERICK_BACKUP_DIR:-/var/backups/maverick}"
KEEP="${MAVERICK_BACKUP_KEEP:-7}"
TARGET="${MAVERICK_BACKUP_TARGET:-}"
AT="${MAVERICK_BACKUP_AT:-02:30}"

# The exact row count of every public table, as `<table> <rows>` lines.
COUNT_SQL="select table_name || ' ' || (xpath('/row/c/text()', query_to_xml(format('select count(*) as c from public.%I', table_name), false, true, '')))[1]::text from information_schema.tables where table_schema = 'public' and table_type = 'BASE TABLE' order by table_name;"
END_MARK="maverick-counts-end"

json_string() {
    printf '"%s"' "$(printf '%s' "$1" | sed 's/\\/\\\\/g; s/"/\\"/g' | tr '\n\t' '  ')"
}

# log <LEVEL> <msg> [key value]...
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

fail() {
    log ERROR "backup failed" error "$1"
    return 1
}

# A write to psql after it exited must fail, not kill the script.
trap '' PIPE

# One backup, in a work dir that is always removed.
backup_once() {
    work="$(mktemp -d)"
    status=0
    backup_in "$work" || status=$?
    rm -rf "$work"
    return "$status"
}

# Holds a read-only repeatable-read transaction open in a background psql, exports its
# snapshot, dumps with that snapshot, and counts the rows in it. Checks every step itself:
# `set -e` does not apply in a function called with `||`.
backup_in() {
    work="$1"
    [ -n "${MAVERICK_DATABASE_URL:-}" ] || { fail "MAVERICK_DATABASE_URL is not set"; return 1; }
    mkdir -p "$DIR" || { fail "cannot create $DIR"; return 1; }
    stamp="$(date -u +%Y%m%dT%H%M%SZ)"
    dump="$DIR/maverick-$stamp.dump"
    counts="$DIR/maverick-$stamp.counts"
    mkfifo "$work/in" "$work/out" || { fail "mkfifo failed"; return 1; }

    psql "$MAVERICK_DATABASE_URL" -X -A -t -q -v ON_ERROR_STOP=1 \
        <"$work/in" >"$work/out" 2>"$work/err" &
    psql_pid=$!
    exec 3>"$work/in" 4<"$work/out"
    printf '%s\n' "begin isolation level repeatable read read only;" \
        "select pg_export_snapshot();" >&3 || true
    if ! IFS= read -r snapshot <&4 || [ -z "$snapshot" ]; then
        exec 3>&- 4<&-
        wait "$psql_pid" 2>/dev/null || true
        fail "could not open a snapshot: $(cat "$work/err" 2>/dev/null)"
        return 1
    fi

    if ! pg_dump --format=custom --snapshot="$snapshot" --file="$dump.partial" \
        "$MAVERICK_DATABASE_URL" 2>"$work/dump-err"; then
        exec 3>&- 4<&-
        wait "$psql_pid" 2>/dev/null || true
        fail "pg_dump: $(cat "$work/dump-err")"
        return 1
    fi

    printf '%s\n' "$COUNT_SQL" "select '$END_MARK';" >&3 || true
    : >"$counts.partial"
    while IFS= read -r line <&4; do
        [ "$line" = "$END_MARK" ] && break
        printf '%s\n' "$line" >>"$counts.partial"
    done
    printf '%s\n' "commit;" >&3 || true
    exec 3>&- 4<&-
    wait "$psql_pid" || { fail "psql: $(cat "$work/err")"; return 1; }
    [ -s "$counts.partial" ] || { fail "no table counts"; return 1; }

    if ! { mv "$dump.partial" "$dump" && mv "$counts.partial" "$counts"; }; then
        fail "cannot move the backup into place"
        return 1
    fi
    log INFO "backup written" dump "$dump" bytes "$(wc -c <"$dump" | tr -d ' ')" \
        tables "$(wc -l <"$counts" | tr -d ' ')"

    if [ -n "$TARGET" ]; then
        command -v rclone >/dev/null 2>&1 || { fail "MAVERICK_BACKUP_TARGET is set but rclone is not installed"; return 1; }
        if ! { rclone copy "$dump" "$TARGET" && rclone copy "$counts" "$TARGET"; }; then
            fail "rclone copy to the backup target failed"
            return 1
        fi
        log INFO "backup copied off the box" dump "$(basename "$dump")"
    else
        log WARN "backup target unset; the dump exists on this host only" \
            var MAVERICK_BACKUP_TARGET
    fi

    # Keep the newest KEEP dumps. The glob sorts by name, and the names sort by time.
    set -- "$DIR"/maverick-*.dump
    excess=$(($# - KEEP))
    for old in "$@"; do
        [ "$excess" -gt 0 ] || break
        rm -f "$old" "${old%.dump}.counts"
        log INFO "old backup removed" dump "$old"
        excess=$((excess - 1))
    done
    return 0
}

# Seconds until the next AT (UTC).
seconds_until() {
    now="$(date -u +%s)"
    next="$(date -u -d "$(date -u +%Y-%m-%d) $1" +%s)"
    [ "$next" -gt "$now" ] || next=$((next + 86400))
    echo $((next - now))
}

case "${1:-}" in
    "")
        backup_once
        ;;
    --daemon)
        # The Kamal backup accessory (postgres:16-alpine) has no rclone.
        if [ -n "$TARGET" ] && [ "${MAVERICK_BACKUP_INSTALL_RCLONE:-}" = 1 ] &&
            ! command -v rclone >/dev/null 2>&1; then
            if apk add --no-cache rclone >/dev/null 2>&1; then
                log INFO "rclone installed"
            else
                log ERROR "cannot install rclone; the dumps stay on this host"
            fi
        fi
        log INFO "backup daemon started" at "$AT UTC" dir "$DIR" keep "$KEEP"
        while :; do
            sleep "$(seconds_until "$AT")"
            backup_once || true
        done
        ;;
    *)
        echo "usage: scripts/backup.sh [--daemon]" >&2
        exit 2
        ;;
esac
