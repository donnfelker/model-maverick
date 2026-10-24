#!/usr/bin/env bash
# The skew matrix (D50, spec §14.5; bead xq7v.22).
#
# For every row of crates/e2e/skew-releases.txt, builds that release's `maverick` and
# `maverick-proxy` from its git ref, then runs crates/e2e/tests/skew.rs twice: that
# release's engine with the current proxy, and the current engine with that release's
# proxy. Each run starts Postgres (testcontainers), the fake provider, the engine, and the
# proxy as separate processes, so it needs Docker. Nothing leaves localhost.
#
# A release's source comes from `git archive` (no worktree), and it builds in its own
# target directory under $MAVERICK_SKEW_WORK (default: <target>/skew-releases), so the
# current build is never touched. A release already built there is reused. Process logs
# and step reports go to target/skew/<timestamp>/.
#
# CARGO names the cargo command (default: cargo; for example `mm-cargo` on a shared
# host).
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

read -r -a CARGO_CMD <<<"${CARGO:-cargo}"
export SQLX_OFFLINE="${SQLX_OFFLINE:-true}"
export MAVERICK_TEST_REQUIRE_DB="${MAVERICK_TEST_REQUIRE_DB:-1}"
RELEASES="crates/e2e/skew-releases.txt"

target_dir="$("${CARGO_CMD[@]}" metadata --format-version 1 --no-deps |
    grep -o '"target_directory":"[^"]*"' | cut -d'"' -f4)"
work="${MAVERICK_SKEW_WORK:-${target_dir}/skew-releases}"

echo "==> building the current maverick and maverick-proxy"
"${CARGO_CMD[@]}" build -p maverick -p maverick-proxy
current="${target_dir}/debug"
export MAVERICK_E2E_BIN_DIR="$current"

# Builds release $1 at ref $2 into $work/<name>-<ref>/bin, unless it is there already.
# The directory names the ref, so a row whose ref changes is built again.
release_dir() {
    echo "${work}/${1}-${2}"
}

build_release() {
    local name="$1" ref="$2"
    local dir
    dir="$(release_dir "$name" "$ref")"
    local bin="${dir}/bin"
    if [[ -x "${bin}/maverick" && -x "${bin}/maverick-proxy" ]]; then
        echo "==> ${name}: reusing ${bin}"
        return
    fi
    if ! git cat-file -e "${ref}^{commit}" 2>/dev/null; then
        echo "==> ${name}: fetching ${ref}"
        git fetch --quiet --depth 1 origin "$ref"
    fi
    local src
    src="$(mktemp -d "${dir}-src.XXXXXX")"
    echo "==> ${name}: building ${ref} in ${src}"
    git archive "$ref" | tar -x -C "$src"
    local target="${dir}/target"
    (cd "$src" &&
        CARGO_TARGET_DIR="$target" "${CARGO_CMD[@]}" build -p maverick -p maverick-proxy)
    mkdir -p "$bin"
    cp "${target}/debug/maverick" "${target}/debug/maverick-proxy" "$bin/"
}

run_pair() {
    local label="$1" engine="$2" proxy="$3"
    echo "==> skew: ${label}"
    MAVERICK_SKEW_LABEL="$label" MAVERICK_SKEW_ENGINE="$engine" \
        MAVERICK_SKEW_PROXY="$proxy" \
        "${CARGO_CMD[@]}" test -p maverick-e2e --test skew -- --ignored --nocapture
}

mkdir -p "$work"
rows=0
while read -r name ref rest; do
    [[ -z "${name:-}" || "$name" == \#* ]] && continue
    if [[ -z "${ref:-}" || -n "${rest:-}" ]]; then
        echo "skew: bad row in ${RELEASES}: ${name} ${ref:-} ${rest:-}" >&2
        exit 2
    fi
    rows=$((rows + 1))
    build_release "$name" "$ref"
    old="$(release_dir "$name" "$ref")/bin"
    run_pair "${name} engine + current proxy" "${old}/maverick" "${current}/maverick-proxy"
    run_pair "current engine + ${name} proxy" "${current}/maverick" "${old}/maverick-proxy"
done <"$RELEASES"

if [[ "$rows" -eq 0 ]]; then
    echo "skew: ${RELEASES} lists no release" >&2
    exit 2
fi
echo "==> skew matrix: ${rows} release(s), $((rows * 2)) pairs passed"
