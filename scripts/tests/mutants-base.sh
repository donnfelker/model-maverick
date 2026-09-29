#!/usr/bin/env bash
# Tests scripts/mutants-base.sh (bead model-maverick-hhum) against a throwaway git repo
# and a fake `gh` on PATH. No network.
set -euo pipefail

repo_root="$(cd "$(dirname "$0")/../.." && pwd)"
script="$repo_root/scripts/mutants-base.sh"

work="$(mktemp -d)"
trap 'rm -rf -- "$work"' EXIT

# Four linear commits; HEAD is c4.
git -C "$work" init -q repo
repo="$work/repo"
for n in 1 2 3 4; do
    git -C "$repo" -c user.name=t -c user.email=t@example.com commit -q --allow-empty -m "c$n"
done
c1="$(git -C "$repo" rev-parse HEAD~3)"
c2="$(git -C "$repo" rev-parse HEAD~2)"
c3="$(git -C "$repo" rev-parse HEAD~1)"
c4="$(git -C "$repo" rev-parse HEAD)"
stranger="0123456789abcdef0123456789abcdef01234567"

# The fake `gh api <path> --jq <filter>` prints the file named for the path, already in the
# shape the real filter prints, and fails when the file is missing.
fake="$work/fake"
mkdir -p "$fake/bin"
cat >"$fake/bin/gh" <<'EOF'
#!/usr/bin/env bash
[[ "$1" == "api" ]] || exit 2
case "$2" in
    */workflows/ci.yml/runs\?*) file="$FAKE_GH/runs" ;;
    */runs/*/jobs\?*) id="${2#*/runs/}"; file="$FAKE_GH/jobs-${id%%/*}" ;;
    *) exit 2 ;;
esac
[[ -f "$file" ]] || { echo "fake gh: no $file" >&2; exit 1; }
cat "$file"
EOF
chmod +x "$fake/bin/gh"

tab="$(printf '\t')"
diff_ok="mutants (changed lines)${tab}success"
diff_cancelled="mutants (changed lines)${tab}cancelled"
diff_failed="mutants (changed lines)${tab}failure"
push_skipped_shards="mutants (shard \${{ matrix.shard }}/8)${tab}skipped"

shards() {
    local conclusion_of_last="$1" k
    for k in 0 1 2 3 4 5 6; do
        printf 'mutants (shard %s/8)\tsuccess\n' "$k"
    done
    printf 'mutants (shard 7/8)\t%s\n' "$conclusion_of_last"
}

# base <runs lines> -- <id> <jobs lines> ...: writes the fake responses, runs the script
# in the throwaway repo, and prints its stdout.
base() {
    rm -f "$fake"/runs "$fake"/jobs-*
    printf '%s\n' "$1" >"$fake/runs"
    shift
    while [[ $# -ge 2 ]]; do
        printf '%s\n' "$2" >"$fake/jobs-$1"
        shift 2
    done
    (cd "$repo" && PATH="$fake/bin:$PATH" FAKE_GH="$fake" GITHUB_REPOSITORY=o/r "$script" fallback 2>/dev/null)
}

expect() {
    local name="$1" got="$2" want="$3"
    if [[ "$got" != "$want" ]]; then
        printf 'FAIL %s: got %q, want %q\n' "$name" "$got" "$want" >&2
        exit 1
    fi
    printf 'ok   %s\n' "$name"
}

expect "newest push run whose mutants passed" \
    "$(base "30 $c3 push
20 $c2 push" 30 "$diff_ok"$'\n'"$push_skipped_shards" 20 "$diff_ok")" "$c3"

expect "a cancelled or failed mutants job does not qualify" \
    "$(base "40 $c3 push
30 $c2 push
20 $c1 push" 40 "$diff_cancelled" 30 "$diff_failed" 20 "$diff_ok")" "$c1"

expect "a run on HEAD itself is skipped" \
    "$(base "50 $c4 push
30 $c2 push" 50 "$diff_ok" 30 "$diff_ok")" "$c2"

expect "a head that is not an ancestor is skipped" \
    "$(base "60 $stranger push
30 $c2 push" 30 "$diff_ok")" "$c2"

expect "a nightly run qualifies when every shard passed" \
    "$(base "70 $c3 schedule
30 $c2 push" 70 "$(shards success)" 30 "$diff_ok")" "$c3"

expect "a nightly run with a failed shard does not qualify" \
    "$(base "70 $c3 schedule
30 $c2 push" 70 "$(shards failure)" 30 "$diff_ok")" "$c2"

expect "a pull request run never qualifies" \
    "$(base "80 $c3 pull_request
30 $c2 push" 80 "$diff_ok" 30 "$diff_ok")" "$c2"

expect "no qualifying run falls back" \
    "$(base "40 $c3 push" 40 "$diff_cancelled")" "fallback"

expect "a failing jobs call falls back" \
    "$(base "90 $c3 push")" "fallback"

rm -f "$fake"/runs "$fake"/jobs-*
expect "a failing runs call falls back" \
    "$(cd "$repo" && PATH="$fake/bin:$PATH" FAKE_GH="$fake" GITHUB_REPOSITORY=o/r "$script" fallback 2>/dev/null)" \
    "fallback"

expect "no GITHUB_REPOSITORY falls back" \
    "$(cd "$repo" && env -u GITHUB_REPOSITORY PATH="$fake/bin:$PATH" "$script" before 2>/dev/null)" \
    "before"

echo "mutants-base: all tests passed"
