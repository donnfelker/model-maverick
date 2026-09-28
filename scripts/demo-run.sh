#!/usr/bin/env bash
# Phase-0 exit check (`make demo-run`): seed one column + one task, run one
# batch against the fake provider, print the persisted runs.
#
# Requirements: either export MAVERICK_DATABASE_URL pointing at a Postgres
# you provide, or have Docker running — the script then boots a throwaway
# postgres:16-alpine container and removes it on exit.
set -euo pipefail
cd "$(dirname "$0")/.."

if [[ -z "${MAVERICK_DATABASE_URL:-}" ]]; then
  if ! command -v docker > /dev/null; then
    echo "demo-run: Docker is required when MAVERICK_DATABASE_URL is not set" >&2
    exit 1
  fi
  container="maverick-demo-pg-$$"
  docker run --detach --rm --name "$container" \
    --env POSTGRES_USER=maverick \
    --env POSTGRES_PASSWORD=maverick \
    --env POSTGRES_DB=maverick \
    --publish 127.0.0.1:0:5432 \
    postgres:16-alpine > /dev/null
  trap 'docker stop "$container" > /dev/null 2>&1 || true' EXIT

  port="$(docker port "$container" 5432/tcp | head -n1 | awk -F: '{print $NF}')"
  export MAVERICK_DATABASE_URL="postgres://maverick:maverick@127.0.0.1:${port}/maverick?sslmode=disable"

  # The postgres entrypoint starts once for init and once for real; wait for
  # the second "ready to accept connections" like testcontainers does.
  ready=0
  for _ in $(seq 1 120); do
    if [[ "$(docker logs "$container" 2>&1 | grep -c 'ready to accept connections')" -ge 2 ]]; then
      ready=1
      break
    fi
    sleep 0.5
  done
  if [[ "$ready" -ne 1 ]]; then
    echo "demo-run: Postgres container did not become ready" >&2
    exit 1
  fi
fi

export MAVERICK_INTERNAL_TOKENS="${MAVERICK_INTERNAL_TOKENS:-demo:demo-secret}"
export MAVERICK_CAPTURE_MASTER_KEY="${MAVERICK_CAPTURE_MASTER_KEY:-$(printf 'ab%.0s' {1..32})}"

maverick demo-run  # was "go run ./cmd/maverick demo-run" in the Go engine; use the Rust binary
