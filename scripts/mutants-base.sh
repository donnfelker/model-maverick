#!/usr/bin/env bash
# Prints the base commit for `scripts/ci.sh mutants-diff` on a push to main (bead
# model-maverick-hhum).
#
# On main a newer push replaces a pending CI run, so the replaced push never runs the
# changed-lines mutants job. A diff from the push's own `before` commit would leave the
# replaced pushes' domain lines untested. So the base is the head of the newest completed
# run on main whose head is an ancestor of HEAD and whose mutants passed:
# - a push run: its `mutants (changed lines)` job concluded success;
# - a nightly or on-demand run: every `mutants (shard k/n)` job concluded success.
# The diff from that commit covers every push since, the replaced ones included. The test
# is the mutants jobs, not the whole run, so a red fast tier does not grow the diff.
#
# Usage: scripts/mutants-base.sh <fallback>
#
# Prints <fallback> when GITHUB_REPOSITORY is unset, `gh` is missing or fails, or none of
# the last 30 completed runs qualifies. `gh` reads GH_TOKEN (set by the workflow). To run
# it locally: `GITHUB_REPOSITORY=<owner>/<repo> scripts/mutants-base.sh HEAD~1` after
# `gh auth login`. scripts/tests/mutants-base.sh tests it with a fake `gh`.
set -uo pipefail

# mutants_passed <event> <jobs>: <jobs> has one "<job name><TAB><conclusion>" line per
# mutants job of one run.
mutants_passed() {
    local event="$1" jobs="$2"
    case "$event" in
        push)
            grep -qxF "$(printf 'mutants (changed lines)\tsuccess')" <<<"$jobs"
            ;;
        schedule | workflow_dispatch)
            local shards passed
            shards="$(grep -c '^mutants (shard [0-9]' <<<"$jobs")"
            passed="$(grep -c "^mutants (shard [0-9][0-9]*/[0-9][0-9]*)$(printf '\t')success\$" <<<"$jobs")"
            [[ "$shards" -gt 0 && "$shards" -eq "$passed" ]]
            ;;
        *)
            return 1
            ;;
    esac
}

# pick_base: prints the qualifying commit, or fails.
pick_base() {
    local repo="${GITHUB_REPOSITORY:-}"
    [[ -n "$repo" ]] || return 1
    command -v gh >/dev/null 2>&1 || return 1
    local head runs
    head="$(git rev-parse HEAD)" || return 1
    runs="$(gh api "repos/$repo/actions/workflows/ci.yml/runs?branch=main&status=completed&per_page=30" \
        --jq '.workflow_runs[] | "\(.id) \(.head_sha) \(.event)"')" || return 1
    local id sha event jobs
    while read -r id sha event; do
        [[ -n "$id" && -n "$sha" && "$sha" != "$head" ]] || continue
        git merge-base --is-ancestor "$sha" "$head" 2>/dev/null || continue
        jobs="$(gh api "repos/$repo/actions/runs/$id/jobs?per_page=100" \
            --jq '.jobs[] | select(.name | startswith("mutants")) | "\(.name)\t\(.conclusion)"')" ||
            return 1
        if mutants_passed "$event" "$jobs"; then
            echo "mutants base: $sha (run $id, $event)" >&2
            echo "$sha"
            return 0
        fi
    done <<<"$runs"
    return 1
}

main() {
    local fallback="${1-}"
    if ! pick_base; then
        echo "mutants base: no qualifying run; using the fallback '${fallback}'" >&2
        echo "$fallback"
    fi
}

if [[ "${BASH_SOURCE[0]}" == "$0" ]]; then
    main "$@"
fi
