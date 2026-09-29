#!/usr/bin/env bash
# The hot-path latency harness (bead 9.3; master plan §3.2; spec §2.3; D3, D20).
#
# Builds `maverick` and `maverick-proxy` in release, then runs
# crates/e2e/tests/latency.rs (also in release). That test starts Postgres 16
# (testcontainers), seeds a realistic catalog (12 columns, a week of aggregates per cell),
# starts `serve --api-only` and the proxy as processes against the HTTP fake provider, and
# measures p50/p95/p99 for:
#   - the declared recommend path (target: p95 < 50 ms);
#   - the uncached prompt path, with the fake classifier at MAVERICK_LATENCY_CLASSIFIER_MS
#     (target: p95 < 1.8 s), plus one call over the 1.5 s classifier deadline;
#   - the proxy's added time to first byte (reported).
# It needs Docker. No real provider is called. The table prints at the end, and the report
# goes to target/latency/<timestamp>/report.json with the process logs.
#
# Knobs: MAVERICK_LATENCY_{DECLARED,PROMPT,PROXY,WARMUP,CONCURRENCY,CLASSIFIER_MS}.
# By default the run only reports. MAVERICK_LATENCY_ASSERT=1 also asserts the targets: use
# it for the M6 gate run on a quiet, production-like host. On a shared development host the
# environment-floor rows (Postgres round trip, commit, /up) show how much the host adds.
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

export SQLX_OFFLINE="${SQLX_OFFLINE:-true}"
export MAVERICK_TEST_REQUIRE_DB="${MAVERICK_TEST_REQUIRE_DB:-1}"
export MAVERICK_LATENCY_ASSERT="${MAVERICK_LATENCY_ASSERT:-0}"

echo "==> building maverick and maverick-proxy (release)"
cargo build --release -p maverick -p maverick-proxy

target_dir="$(cargo metadata --format-version 1 --no-deps |
    grep -o '"target_directory":"[^"]*"' | cut -d'"' -f4)"
export MAVERICK_E2E_BIN_DIR="${target_dir}/release"
echo "==> binaries in ${MAVERICK_E2E_BIN_DIR}"

exec cargo test --release -p maverick-e2e --test latency -- --ignored --nocapture
