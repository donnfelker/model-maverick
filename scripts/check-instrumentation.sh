#!/usr/bin/env bash
# G5 telemetry redaction control (D19, plan §6, spec §14.3).
# Fails if any #[instrument] under crates/ lacks skip_all (see crates/instrument-lint).
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

lint() {
    cargo run --quiet --locked -p instrument-lint -- "$@"
}

# Self-test first: a lint that accepts the known-bad fixture checks nothing.
set +e
lint crates/instrument-lint/tests/fixtures/bad >/dev/null
status=$?
set -e
if [[ "$status" -ne 1 ]]; then
    echo "instrument-lint self-test failed: expected exit 1 on the bad fixture, got $status" >&2
    exit 1
fi
echo "instrument-lint self-test: bad fixture rejected"

lint crates
