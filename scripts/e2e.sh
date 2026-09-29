#!/usr/bin/env bash
# The full-stack end-to-end run (bead 9.10).
#
# Builds the real `maverick` and `maverick-proxy` binaries, then runs
# crates/e2e/tests/full_stack.rs. That test starts Postgres 16 (testcontainers) and the
# fake provider over HTTP, then runs `maverick migrate`, `serve --worker-only`,
# `serve --api-only`, the corpus CLI, and the proxy in self-hosted and hosted modes as
# separate processes. It needs Docker. No real provider is called and nothing leaves
# localhost.
#
# Each step prints a numbered header, its requests, statuses, and elapsed time. Process
# logs and the fake provider's request log (prompt hashes only) go to
# target/e2e/<timestamp>/. A failure prints the last 200 lines of every process log.
#
# MAVERICK_E2E_BREAK=<step> fails that step on purpose, to check the failure report.
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

export SQLX_OFFLINE="${SQLX_OFFLINE:-true}"
export MAVERICK_TEST_REQUIRE_DB="${MAVERICK_TEST_REQUIRE_DB:-1}"

echo "==> building maverick and maverick-proxy"
cargo build -p maverick -p maverick-proxy

target_dir="$(cargo metadata --format-version 1 --no-deps |
    grep -o '"target_directory":"[^"]*"' | cut -d'"' -f4)"
export MAVERICK_E2E_BIN_DIR="${target_dir}/debug"
echo "==> binaries in ${MAVERICK_E2E_BIN_DIR}"

exec cargo test -p maverick-e2e --test full_stack -- --ignored --nocapture
