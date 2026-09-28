#!/usr/bin/env bash
# Regenerates the sqlx offline query data in .sqlx/, or checks it with --check.
#
# The compile-time checked queries need a migrated database. With DATABASE_URL set,
# this script migrates that database and uses it. Without it, the script starts a
# throwaway postgres:16-alpine container, migrates it, and removes it on exit.
#
# Run it after you add or change a sqlx::query!/query_as! macro, then commit .sqlx/.
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

MODE=()
case "${1:-}" in
    "") ;;
    --check) MODE=(--check) ;;
    *)
        echo "usage: scripts/sqlx-prepare.sh [--check]" >&2
        exit 2
        ;;
esac

if [[ -z "${DATABASE_URL:-}" ]]; then
    if ! command -v docker >/dev/null 2>&1; then
        echo "sqlx-prepare: set DATABASE_URL to a Postgres database, or install Docker" >&2
        exit 2
    fi
    name="maverick-sqlx-prepare-$$"
    docker run -d --rm --name "$name" \
        -e POSTGRES_USER=maverick -e POSTGRES_PASSWORD=maverick -e POSTGRES_DB=maverick \
        -p 127.0.0.1::5432 postgres:16-alpine >/dev/null
    trap 'docker stop "$name" >/dev/null 2>&1 || true' EXIT
    # The init server listens on the Unix socket only, so a TCP check waits for the real one.
    for _ in $(seq 1 60); do
        if docker exec "$name" pg_isready -U maverick -h 127.0.0.1 >/dev/null 2>&1; then
            break
        fi
        sleep 1
    done
    port="$(docker port "$name" 5432/tcp | head -n 1 | sed 's/.*://')"
    export DATABASE_URL="postgres://maverick:maverick@127.0.0.1:${port}/maverick?sslmode=disable"
fi

export SQLX_OFFLINE=false
cargo sqlx migrate run --source crates/maverick-store/migrations
cargo sqlx prepare "${MODE[@]}" --workspace
