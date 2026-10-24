#!/usr/bin/env bash
# The sealed-environment test (D49, spec §14.4, self-hosted plan §6; bead xq7v.12).
#
# Runs crates/e2e/tests/sealed.rs in a new network namespace that has only loopback. The
# test runs `maverick upgrade`, a replica `serve`, and `maverick matrix import` with every
# outbound setting at its default, under strace, and fails on any outbound connection
# attempt. Postgres runs in Docker outside the namespace with no published port. It
# listens on a unix socket in a shared directory, so the namespace never needs a route.
#
# `unshare --net` needs root, so sudo runs that one step. Everything in the namespace runs
# as the calling user. Needs Docker, strace, sudo, and util-linux (unshare, setpriv).
# Process logs and the traces go to target/sealed/<timestamp>/.
#
# CARGO names the cargo command (default: cargo; for example `mm-cargo` on a shared host).
# The socket directory goes under TMPDIR (default /tmp), whose path must stay short: a unix
# socket path has at most 107 bytes.
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

read -r -a CARGO_CMD <<<"${CARGO:-cargo}"
export SQLX_OFFLINE="${SQLX_OFFLINE:-true}"
# The image and readiness rule of crates/maverick-testdb.
IMAGE=postgres:16-alpine
READY_LINE="database system is ready to accept connections"

for tool in docker strace sudo unshare setpriv; do
    if ! command -v "$tool" >/dev/null; then
        echo "sealed: ${tool} is not installed" >&2
        exit 2
    fi
done

echo "==> building maverick (debug, test-keys) and the sealed test"
# Only a debug build with the test keys accepts the test-signed fixture bundle the test
# imports (docs/matrix-bundles.md). A release build never does.
"${CARGO_CMD[@]}" build -p maverick --features test-keys
target_dir="$("${CARGO_CMD[@]}" metadata --format-version 1 --no-deps |
    grep -o '"target_directory":"[^"]*"' | cut -d'"' -f4)"
test_bin="$("${CARGO_CMD[@]}" test -p maverick-e2e --test sealed --no-run --message-format=json |
    grep -o '"executable":"[^"]*"' | cut -d'"' -f4 | tail -n 1)"
if [[ ! -x "$test_bin" ]]; then
    echo "sealed: cargo test --no-run named no test binary" >&2
    exit 1
fi

work="$(mktemp -d "${TMPDIR:-/tmp}/mm-sealed.XXXXXX")"
sock="${work}/pg"
socket="${sock}/.s.PGSQL.5432"
if ((${#socket} > 107)); then
    echo "sealed: ${socket} is too long for a unix socket; set a shorter TMPDIR" >&2
    rm -rf "$work"
    exit 2
fi
# The container's postgres user creates the socket here.
mkdir -m 0777 "$sock"
name="maverick-sealed-$$"
cleanup() {
    docker stop "$name" >/dev/null 2>&1 || true
    rm -rf "$work" 2>/dev/null || echo "sealed: could not remove ${work}" >&2
}
trap cleanup EXIT

echo "==> Postgres (${IMAGE}) on ${socket}, no published port"
docker run -d --rm --name "$name" \
    -e POSTGRES_PASSWORD="$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')" \
    -v "${sock}:/var/run/postgresql" "$IMAGE" -c fsync=off >/dev/null
# The image's init step starts and stops a temporary server first, so the line appears
# twice before the real server is up.
for _ in $(seq 1 120); do
    if [[ $(docker logs "$name" 2>&1 | grep -c "$READY_LINE") -ge 2 && -S $socket ]]; then
        break
    fi
    sleep 0.5
done
if [[ ! -S $socket ]]; then
    echo "sealed: Postgres did not start" >&2
    docker logs "$name" >&2 || true
    exit 1
fi

echo "==> the sealed test, in a network namespace with only loopback"
url="postgres://postgres@localhost/postgres?host=${sock}"
# Root brings up loopback in the new namespace, then setpriv runs the test as this user.
# shellcheck disable=SC2016 # $1, $2, and $@ belong to the inner shell.
sudo unshare --net -- sh -c 'ip link set lo up && uid=$1 gid=$2 && shift 2 &&
    exec setpriv --reuid="$uid" --regid="$gid" --init-groups -- "$@"' \
    sealed "$(id -u)" "$(id -g)" \
    env -i PATH="$PATH" HOME="$HOME" RUST_BACKTRACE=1 \
    MAVERICK_TEST_DATABASE_URL="$url" MAVERICK_TEST_REQUIRE_DB=1 \
    MAVERICK_E2E_BIN_DIR="${target_dir}/debug" MAVERICK_SEALED_PG_SOCKET="$socket" \
    "$test_bin" --ignored --nocapture
