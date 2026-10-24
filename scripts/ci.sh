#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

usage() {
    cat <<'USAGE'
Usage: scripts/ci.sh [all|fast|boundary|db|parity|e2e|skew]
       scripts/ci.sh mutants [k/n]          full run, or shard k of n (0-based)
       scripts/ci.sh mutants-diff <base>    only mutants on lines changed since <base>

No argument runs all tiers except mutants.
The boundary tier checks engine source, migrations, and the old label without compiling.
USAGE
}

TIER="${1:-all}"
export SQLX_OFFLINE="${SQLX_OFFLINE:-true}"
export CARGO_TERM_COLOR="${CARGO_TERM_COLOR:-always}"

log_dir() {
    mkdir -p "target/ci/$1"
}

run_logged() {
    local tier="$1"
    local name="$2"
    shift 2

    log_dir "$tier"
    echo "==> [$tier] $name"
    {
        printf '$'
        printf ' %q' "$@"
        printf '\n'
        "$@"
    } 2>&1 | tee "target/ci/$tier/$name.log"
    local status="${PIPESTATUS[0]}"
    if [[ "$status" -ne 0 ]]; then
        echo "Step failed: $tier/$name"
        exit "$status"
    fi
}

require_cargo_command() {
    local command="$1"
    if ! cargo "$command" --version >/dev/null 2>&1; then
        cat >&2 <<EOF
Missing cargo subcommand: cargo $command

Install the pinned CI version, then rerun:
  cargo install ${2} --version ${3} --locked ${4:-}
EOF
        exit 127
    fi
}

run_sqlx_check() {
    # Plain grep: CI runners have no ripgrep, and a missing tool must not read as "no queries".
    if ! grep -rqE --include='*.rs' \
        'sqlx::query(_as|_scalar)?!|query(_as|_scalar|_file|_file_as|_file_scalar)?!' crates; then
        log_dir fast
        echo "no sqlx checked queries registered yet" | tee "target/ci/fast/sqlx.log"
        return
    fi

    # Uses DATABASE_URL when set; otherwise a throwaway migrated Postgres container.
    run_logged fast sqlx scripts/sqlx-prepare.sh --check
}

check_engine_boundary() {
    # Plans and subscription state belong to the control plane (production-proxy plan
    # section 8). Explicit glob pathspecs include nested source and migration files.
    local status
    if git grep -n -i -I -E 'stripe|billing|past_due|subscription_status' -- \
        ':(top,glob)crates/*/src/**' ':(top,glob)crates/*/migrations/**'; then
        echo "Engine source and migrations must not contain commerce logic."
        echo "Commerce logic belongs to the control plane (plans/production-proxy-plan.md section 8)."
        return 1
    else
        status=$?
    fi
    if [[ "$status" -ne 1 ]]; then
        echo "Engine boundary check failed: git grep exited $status." >&2
        return "$status"
    fi
    echo "Engine boundary check passed."
}

check_label() {
    # The old try-it label is gone from the repository (production-proxy plan section 8).
    # The pattern is a character class, so this script never matches itself. .beads/ is
    # history, and the v1.1 spec is the control-plane document, so both stay excluded.
    local status
    if git grep -n -i -I -e 'tr[i]al' -- . ':(exclude).beads' \
        ':(exclude)specs/model-maverick-v1.1-spec.md'; then
        echo "Reword every match of the old label (plans/production-proxy-plan.md section 8)."
        return 1
    else
        status=$?
    fi
    if [[ "$status" -ne 1 ]]; then
        echo "Label check failed: git grep exited $status." >&2
        return "$status"
    fi
    echo "Label check passed."
}

check_private_keys() {
    # Bundle signing keys never enter the repository (D49, bead xq7v.5): no tracked file is
    # a private-key file by name, and no tracked text holds a PEM private key. Public keys
    # live in crates/maverick-bundle/src/trust.rs as hex. The PEM pattern ends in a
    # character class, so this script never matches itself.
    local named status=0
    named="$(git ls-files -- '*.key' '*.p8' '*.pk8' '*.seed' '*signing*.pem' '*private*.pem')"
    if [[ -n "$named" ]]; then
        printf '%s\n' "$named"
        echo "Private key files are tracked; remove them and rotate those keys."
        return 1
    fi
    if git grep -n -I -e 'BEGIN [A-Z ]*PRIVATE KE[Y]' -- .; then
        echo "A private key is tracked; remove it and rotate that key."
        return 1
    else
        status=$?
    fi
    if [[ "$status" -ne 1 ]]; then
        echo "Private key check failed: git grep exited $status." >&2
        return "$status"
    fi
    echo "Private key check passed."
}

run_boundary() {
    run_logged fast engine-boundary-tests bash scripts/tests/ci-checks.sh
    run_logged fast engine-boundary check_engine_boundary
    run_logged fast label check_label
    run_logged fast private-keys check_private_keys
}

run_fast() {
    run_boundary
    require_cargo_command deny cargo-deny 0.20.2 ""
    require_cargo_command sqlx sqlx-cli 0.9.0 "--no-default-features --features rustls,postgres"

    run_logged fast fmt cargo fmt --all --check
    run_logged fast clippy cargo clippy --workspace --all-targets -- -D warnings
    run_logged fast test cargo test --workspace
    run_logged fast deny cargo deny check --hide-inclusion-graph -A duplicate -A license-not-encountered
    run_sqlx_check

    # The final manifest audit (bead 9.1): every Go test in parity/manifest/ has a tagged
    # Rust test, every tag sits in test code, and every DIVERGENCES row is named by a test
    # (and no test or the replay allowlist cites a row that does not exist).
    run_logged fast parity-coverage cargo run -q -p parity-support --bin parity_coverage -- --strict --list-rows

    if git ls-files --error-unmatch scripts/check-instrumentation.sh >/dev/null 2>&1 &&
        [[ -x scripts/check-instrumentation.sh ]]; then
        run_logged fast instrument scripts/check-instrumentation.sh
    else
        log_dir fast
        echo "instrumentation lint not registered yet" | tee "target/ci/fast/instrument.log"
    fi
}

run_db() {
    log_dir db
    # Placeholder: the store and job tests on Postgres run today inside the fast tier's `cargo test --workspace`.
    # This tier gets its own suite when one exists (API/proxy replay, e2e script).
    echo "no separate db suite yet; its tests run in the fast tier (cargo test --workspace)" \
        | tee "target/ci/db/placeholder.log"
}

run_parity() {
    log_dir parity
    # The proxy replay (bead 6.11): every scenario in parity/fixtures/proxy/scenarios.json
    # through the real proxy against a mock engine, mock upstreams, and a mock OTLP
    # collector, checked against the reviewed D-row allowlist; plus the unit.json rows in
    # maverick-core and maverick-proxy. Each scenario writes its outcome under
    # target/ci/parity/proxy-replay/. The other parity fixture tests still run in the
    # fast tier's `cargo test --workspace`.
    rm -rf target/ci/parity/proxy-replay
    run_logged parity proxy-replay cargo test -p maverick-proxy -p maverick-core
    local replayed
    replayed="$(find target/ci/parity/proxy-replay -name '*.txt' | wc -l)"
    {
        echo "proxy replay: ${replayed} scenarios"
        cat target/ci/parity/proxy-replay/*.txt
    } | tee target/ci/parity/proxy-replay-report.log
    if [[ "$replayed" -ne 54 ]]; then
        echo "Step failed: parity/proxy-replay reported ${replayed} of 54 scenarios"
        exit 1
    fi

    # The black-box API replay (bead 5.15): every recorded Go exchange and seed statement
    # in parity/blackbox/ against one database, compared byte for byte after masking.
    # Each remaining difference must match parity/blackbox/replay-allowlist.json, which
    # names its DIVERGENCES row; the JSON report lists every explained difference.
    run_logged parity api-replay env MAVERICK_REPLAY_REPORT=target/ci/parity/api-replay.json \
        cargo test -p maverick-api --test replay -- --nocapture

    # The frozen SDK conformance scenarios (bead 8.3) through the real proxy on
    # maverick-core: groups 1 to 4 and 7 to 9. The fail-open, exhaustion, and OpenRouter
    # groups run in the core-direct runner (bead model-maverick-5fs).
    run_logged parity conformance-proxy \
        cargo test -p sdk-conformance --test conformance -- --nocapture

    # The same scenarios against maverick-core directly (bead model-maverick-5fs): the
    # groups the proxy is exempt from by position. Phase 1 covers exhaustion relay (group
    # 5), OpenRouter (groups 10 to 12), and concrete-model passthrough (S09a).
    run_logged parity conformance-core \
        cargo test -p sdk-conformance --test conformance_core -- --nocapture
}

run_e2e() {
    # The full-stack end-to-end run (bead 9.10): the real maverick and maverick-proxy
    # binaries as separate processes, on Postgres, against the fake provider over HTTP.
    # Needs Docker. Process logs and the fake provider's request log land in
    # target/e2e/<timestamp>/; a failure prints every process log's tail.
    run_logged e2e full-stack scripts/e2e.sh
}

run_skew() {
    # The skew matrix (D50, bead xq7v.22): each release in crates/e2e/skew-releases.txt,
    # built from its git ref, with the current engine and proxy both ways: its engine with
    # the current proxy, and the current engine with its proxy. Needs Docker.
    run_logged skew matrix scripts/skew.sh
}

# The full maverick-domain run is about 1,900 mutants: hours on a CI runner, so CI
# splits it into shards (`--shard k/n`) on a schedule, and checks each push with
# `mutants-diff` (only the mutants on changed lines).
run_mutants() {
    local shard="$1"
    require_cargo_command mutants cargo-mutants 27.1.0 ""
    local args=(--package maverick-domain --output target/ci/mutants)
    if [[ -n "$shard" ]]; then
        args+=(--shard "$shard")
    fi
    run_logged mutants maverick-domain cargo mutants "${args[@]}"
}

run_mutants_diff() {
    local base="$1"
    require_cargo_command mutants cargo-mutants 27.1.0 ""
    log_dir mutants
    git diff "$base" HEAD -- crates/maverick-domain >target/ci/mutants/diff.patch
    if [[ ! -s target/ci/mutants/diff.patch ]]; then
        echo "no maverick-domain changes since $base" | tee target/ci/mutants/maverick-domain.log
        return
    fi
    run_logged mutants maverick-domain cargo mutants --package maverick-domain \
        --in-diff target/ci/mutants/diff.patch --output target/ci/mutants
}

case "$TIER" in
    all)
        run_fast
        run_db
        run_parity
        run_e2e
        run_skew
        ;;
    fast)
        run_fast
        ;;
    boundary)
        # This tier checks the repository only; it needs neither Cargo nor Postgres.
        run_logged fast engine-boundary check_engine_boundary
        run_logged fast label check_label
        run_logged fast private-keys check_private_keys
        ;;
    db)
        run_db
        ;;
    parity)
        run_parity
        ;;
    e2e)
        run_e2e
        ;;
    skew)
        run_skew
        ;;
    mutants)
        run_mutants "${2:-}"
        ;;
    mutants-diff)
        run_mutants_diff "${2:?usage: scripts/ci.sh mutants-diff <base>}"
        ;;
    -h | --help | help)
        usage
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
