#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

usage() {
    cat <<'USAGE'
Usage: scripts/ci.sh [all|fast|db|parity|e2e]
       scripts/ci.sh mutants [k/n]          full run, or shard k of n (0-based)
       scripts/ci.sh mutants-diff <base>    only mutants on lines changed since <base>

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
    # Placeholder: the store and job tests on Postgres run today inside the fast tier's `cargo test --workspace`.
    # This tier gets its own suite when one exists (API/proxy replay, e2e script).
    echo "no separate db suite yet; its tests run in the fast tier (cargo test --workspace)" \
        | tee "target/ci/db/placeholder.log"
}

run_parity() {
    log_dir parity
    # Placeholder: the parity fixture tests run today inside the fast tier's `cargo test --workspace`.
    # This tier gets its own suite when one exists (API/proxy replay, e2e script).
    echo "no separate parity suite yet; its tests run in the fast tier (cargo test --workspace)" \
        | tee "target/ci/parity/placeholder.log"
}

run_e2e() {
    log_dir e2e
    # Placeholder: the end-to-end script (bead 9.10) run today inside the fast tier's `cargo test --workspace`.
    # This tier gets its own suite when one exists (API/proxy replay, e2e script).
    echo "no separate e2e suite yet; its tests run in the fast tier (cargo test --workspace)" \
        | tee "target/ci/e2e/placeholder.log"
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
