#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

usage() {
    cat <<'USAGE'
Usage: scripts/ci.sh [all|fast|db|parity|e2e|mutants]

No argument runs all tiers except mutants.
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
    if ! rg -q 'sqlx::query(_as|_scalar)?!|query(_as|_scalar|_file|_file_as|_file_scalar)?!' crates; then
        log_dir fast
        echo "no sqlx checked queries registered yet" | tee "target/ci/fast/sqlx.log"
        return
    fi

    # Uses DATABASE_URL when set; otherwise a throwaway migrated Postgres container.
    run_logged fast sqlx scripts/sqlx-prepare.sh --check
}

run_fast() {
    require_cargo_command deny cargo-deny 0.20.2 ""
    require_cargo_command sqlx sqlx-cli 0.9.0 "--no-default-features --features rustls,postgres"

    run_logged fast fmt cargo fmt --all --check
    run_logged fast clippy cargo clippy --workspace --all-targets -- -D warnings
    run_logged fast test cargo test --workspace
    run_logged fast deny cargo deny check --hide-inclusion-graph -A duplicate -A license-not-encountered
    run_sqlx_check

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
    echo "no db suites registered yet" | tee "target/ci/db/placeholder.log"
}

run_parity() {
    log_dir parity
    echo "no parity suites registered yet" | tee "target/ci/parity/placeholder.log"
}

run_e2e() {
    log_dir e2e
    echo "no e2e suites registered yet" | tee "target/ci/e2e/placeholder.log"
}

run_mutants() {
    require_cargo_command mutants cargo-mutants 27.1.0 ""
    run_logged mutants maverick-domain cargo mutants --package maverick-domain --output target/ci/mutants
}

case "$TIER" in
    all)
        run_fast
        run_db
        run_parity
        run_e2e
        ;;
    fast)
        run_fast
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
    mutants)
        run_mutants
        ;;
    -h | --help | help)
        usage
        ;;
    *)
        usage >&2
        exit 2
        ;;
esac
