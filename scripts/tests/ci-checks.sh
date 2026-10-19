#!/usr/bin/env bash
# Exercise the engine boundary check through the real CI entry point. Each case uses
# tracked files in a throwaway repository and needs neither Cargo nor network access.
set -Eeuo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

new_repo() {
    local repo="$work/$1"
    git init -q "$repo"
    mkdir -p "$repo/scripts/tests" "$repo/crates/core/src" "$repo/crates/core/migrations"
    cp "$repo_root/scripts/ci.sh" "$repo/scripts/ci.sh"
    # The fast-tier wiring case must not recursively invoke this test suite.
    printf '#!/usr/bin/env bash\nexit 0\n' >"$repo/scripts/tests/ci-checks.sh"
    printf 'pub fn route() {}\n' >"$repo/crates/core/src/lib.rs"
    printf 'create table tenants (id text);\n' >"$repo/crates/core/migrations/0001.sql"
    git -C "$repo" add -- scripts/ci.sh scripts/tests/ci-checks.sh \
        crates/core/src/lib.rs crates/core/migrations/0001.sql
    printf '%s\n' "$repo"
}

plant() {
    local repo="$1" path="$2" content="$3"
    mkdir -p "$(dirname "$repo/$path")"
    printf '%s\n' "$content" >"$repo/$path"
    git -C "$repo" add -- "$path"
}

expect() {
    local name="$1" want="$2" repo="$3" tier="${4:-boundary}" status=0
    bash "$repo/scripts/ci.sh" "$tier" >"$work/output" 2>&1 || status=$?
    if [[ "$status" -ne "$want" ]]; then
        printf 'FAIL %s: exit %s, expected %s\n' "$name" "$status" "$want" >&2
        cat "$work/output" >&2
        exit 1
    fi
    printf 'ok   %s\n' "$name"
}

repo="$(new_repo clean)"
expect "clean engine passes" 0 "$repo"

repo="$(new_repo source)"
plant "$repo" crates/core/src/lib.rs '// StRiPe webhook'
expect "source match is rejected regardless of case" 1 "$repo"

repo="$(new_repo nested)"
plant "$repo" crates/core/src/seam/nested.rs 'const STATE: &str = "past_due";'
expect "nested source match is rejected" 1 "$repo"

repo="$(new_repo migration)"
plant "$repo" crates/core/migrations/0002.sql 'alter table tenants add column billing_email text;'
expect "migration match is rejected" 1 "$repo"

repo="$(new_repo nested-migration)"
plant "$repo" crates/core/migrations/nested/0003.sql 'select subscription_status from tenants;'
expect "nested migration match is rejected" 1 "$repo"

repo="$(new_repo excluded)"
plant "$repo" crates/core/tests/seam.rs '// subscription_status fixture'
plant "$repo" crates/conformance/vectors/sample.json '"You are a billing assistant."'
plant "$repo" specs/model-maverick-v1.1-spec.md 'Billing belongs to the control plane.'
expect "tests, vectors, and control-plane documents are excluded" 0 "$repo"

repo="$(new_repo working-tree)"
printf '// billing state\n' >"$repo/crates/core/src/lib.rs"
expect "unstaged changes in tracked source are checked" 1 "$repo"

repo="$(new_repo fast)"
plant "$repo" crates/core/src/lib.rs '// subscription_status must stay outside the engine'
mkdir -p "$work/no-cargo"
cat >"$work/no-cargo/cargo" <<'EOF'
#!/usr/bin/env bash
echo "Cargo must not run before the boundary check" >&2
exit 73
EOF
chmod +x "$work/no-cargo/cargo"
PATH="$work/no-cargo:$PATH" expect "fast tier rejects matches before running Cargo" 1 "$repo" fast

repo="$(new_repo git-error)"
mkdir -p "$work/bin"
cat >"$work/bin/git" <<'EOF'
#!/usr/bin/env bash
echo "simulated git failure" >&2
exit 2
EOF
chmod +x "$work/bin/git"
PATH="$work/bin:$PATH" expect "git errors fail the check" 2 "$repo"

printf 'ci-checks: all tests passed\n'
