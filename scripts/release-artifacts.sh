#!/usr/bin/env bash
# Builds one release's artifacts and publishes nothing (bead xq7v.21; spec §14.4 "Release
# artifacts"; docker/README.md "Releases"). The release workflow runs it, and anyone can
# run it locally:
#
#   scripts/release-artifacts.sh <version> <out dir>
#
# <version> must equal the workspace version in Cargo.toml. <out dir> must not exist yet.
# It gets:
#   maverick-<v>-x86_64-unknown-linux-musl.tar.gz  maverick and maverick-proxy (static), LICENSE,
#                                                  README.md
#   maverick-<v>.cdx.json, maverick-proxy-<v>.cdx.json
#                                                  CycloneDX 1.5 SBOMs of the two binaries
#   cargo-deny-<v>.json                            the `cargo deny check` report (a failing check
#                                                  fails the script)
#   maverick-offline-kit-<v>.tar                   both image tarballs, the engine migrations, and
#                                                  their SHA256SUMS
#   SHA256SUMS                                     every file above
# The images stay tagged maverick:<v> and maverick-proxy:<v> for the workflow to push.
#
# `maverick --version` in the binaries reports MAVERICK_RELEASE_DATE (default: today, UTC)
# and MAVERICK_RELEASE_COMMIT (default: HEAD, which needs a git checkout whose build inputs
# are committed). The script checks the version, date, and commit in the tarball's binary,
# and that the image runs the same binary. Signing and publishing are the workflow's.
#
# MAVERICK_RELEASE_DRY_RUN=1 (the workflow's manual run) builds even when the bundle trust
# table is not ready for a release, and says so. A tag never sets it.
#
# Needs docker, cargo-deny, and cargo-cyclonedx.
set -Eeuo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ $# -ne 2 ]]; then
    echo "usage: scripts/release-artifacts.sh <version> <out dir>" >&2
    exit 2
fi
VERSION=$1
OUT=$2
TARGET=x86_64-unknown-linux-musl
SBOM_NAME="release-sbom-$$"

step=0
log() {
    step=$((step + 1))
    printf '==> [%d] %s\n' "$step" "$*"
}

workspace_version() {
    cargo metadata --no-deps --format-version 1 --offline |
        python3 -c 'import json, sys
print(next(p["version"] for p in json.load(sys.stdin)["packages"] if p["name"] == "maverick"))'
}

log "checking the version"
if [[ ! $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+(-[0-9A-Za-z.-]+)?$ ]]; then
    echo "$VERSION is not a SemVer version" >&2
    exit 1
fi
if [[ $(workspace_version) != "$VERSION" ]]; then
    echo "$VERSION is not the workspace version $(workspace_version)" >&2
    exit 1
fi
if [[ -e $OUT ]]; then
    echo "$OUT already exists" >&2
    exit 1
fi
RELEASE_DATE="${MAVERICK_RELEASE_DATE:-$(date -u +%F)}"
if [[ -n ${MAVERICK_RELEASE_COMMIT:-} ]]; then
    RELEASE_COMMIT=$MAVERICK_RELEASE_COMMIT
else
    if ! git rev-parse --is-inside-work-tree >/dev/null 2>&1; then
        echo "not a git checkout: set MAVERICK_RELEASE_COMMIT to the commit being built" >&2
        exit 1
    fi
    if [[ -n $(git status --porcelain -- Cargo.toml Cargo.lock crates .sqlx docker) ]]; then
        echo "the build inputs have uncommitted changes, so HEAD is not what gets built" >&2
        exit 1
    fi
    RELEASE_COMMIT=$(git rev-parse HEAD)
fi

# A release trusts exactly one current and one standby bundle signing key, and no test key
# (D49, bead xq7v.5; crates/maverick-bundle/src/trust.rs; docs/operations.md "Bundle
# signing keys"). Until the owner's public keys are in the table, no release builds. A dry
# run reports the failure and goes on, so the rest of the pipeline still gets checked.
log "checking the bundle trust table"
if ! MAVERICK_RELEASE_BUILD=1 cargo test -p maverick-bundle --lib --locked -q -- trust::; then
    if [[ ${MAVERICK_RELEASE_DRY_RUN:-} != 1 ]]; then
        echo "the bundle trust table is not ready for a release" >&2
        exit 1
    fi
    echo "dry run: the bundle trust table is not ready for a release; a tag stops here" >&2
fi

# One fixed time for every archive member, so the same inputs give the same bytes.
EPOCH=$(date -u -d "$RELEASE_DATE" +%s)

mkdir -p "$OUT"
OUT="$(cd "$OUT" && pwd)"
WORK="$(mktemp -d)"
CONTAINER=""
cleanup() {
    if [[ -n $CONTAINER ]]; then
        docker rm -f "$CONTAINER" >/dev/null 2>&1 || true
    fi
    find crates -name "$SBOM_NAME.json" -delete || true
    rm -rf "$WORK"
}
trap cleanup EXIT

archive() { # <out file> <dir> <member>: a deterministic tar of <dir>/<member>
    tar --sort=name --mtime="@$EPOCH" --owner=0 --group=0 --numeric-owner \
        --mode='u+rwX,go+rX,go-w' -C "$2" -cf "$1" "$3"
}

log "building the images (release $VERSION, $RELEASE_DATE, $RELEASE_COMMIT)"
for target in engine proxy; do
    image=maverick
    [[ $target == proxy ]] && image=maverick-proxy
    docker build -f docker/Dockerfile --target "$target" \
        --build-arg MAVERICK_RELEASE_DATE="$RELEASE_DATE" \
        --build-arg MAVERICK_RELEASE_COMMIT="$RELEASE_COMMIT" \
        -t "$image:$VERSION" .
done

log "extracting the static binaries"
DIST="maverick-$VERSION-$TARGET"
mkdir -p "$WORK/$DIST"
CONTAINER=$(docker create "maverick:$VERSION")
docker cp "$CONTAINER:/usr/local/bin/maverick" "$WORK/$DIST/maverick"
docker rm "$CONTAINER" >/dev/null
CONTAINER=$(docker create "maverick-proxy:$VERSION")
docker cp "$CONTAINER:/usr/local/bin/maverick-proxy" "$WORK/$DIST/maverick-proxy"
docker rm "$CONTAINER" >/dev/null
CONTAINER=""
cp LICENSE README.md "$WORK/$DIST/"

log "checking maverick --version"
reported=$("$WORK/$DIST/maverick" --version)
expected="maverick $VERSION
release date: $RELEASE_DATE
commit: $RELEASE_COMMIT"
if [[ $reported != "$expected"* ]]; then
    printf 'maverick --version reported:\n%s\nexpected it to start with:\n%s\n' \
        "$reported" "$expected" >&2
    exit 1
fi
if [[ $(docker run --rm "maverick:$VERSION" --version) != "$reported" ]]; then
    echo "the engine image runs a different binary" >&2
    exit 1
fi

log "packing the tarball"
archive "$WORK/$DIST.tar" "$WORK" "$DIST"
gzip -n -9 -c "$WORK/$DIST.tar" > "$OUT/$DIST.tar.gz"

log "writing the SBOMs"
cargo cyclonedx --format json --spec-version 1.5 --target "$TARGET" \
    --override-filename "$SBOM_NAME" -q
cp "crates/maverick/$SBOM_NAME.json" "$OUT/maverick-$VERSION.cdx.json"
cp "crates/maverick-proxy/$SBOM_NAME.json" "$OUT/maverick-proxy-$VERSION.cdx.json"

log "running cargo deny"
if ! cargo deny --format json check 2> "$OUT/cargo-deny-$VERSION.json"; then
    echo "cargo deny check failed; the report is $OUT/cargo-deny-$VERSION.json" >&2
    exit 1
fi

log "assembling the offline kit"
KIT="maverick-offline-kit-$VERSION"
mkdir -p "$WORK/$KIT/images" "$WORK/$KIT/migrations"
docker save "maverick:$VERSION" | gzip -n -9 > "$WORK/$KIT/images/maverick-$VERSION.tar.gz"
docker save "maverick-proxy:$VERSION" |
    gzip -n -9 > "$WORK/$KIT/images/maverick-proxy-$VERSION.tar.gz"
cp crates/maverick-store/migrations/*.sql "$WORK/$KIT/migrations/"
(cd "$WORK/$KIT" && find images migrations -type f | LC_ALL=C sort | xargs sha256sum > SHA256SUMS)
archive "$OUT/$KIT.tar" "$WORK" "$KIT"

log "writing SHA256SUMS"
(cd "$OUT" && find . -maxdepth 1 -type f -printf '%f\n' | LC_ALL=C sort |
    xargs sha256sum > "$WORK/SHA256SUMS")
cp "$WORK/SHA256SUMS" "$OUT/SHA256SUMS"
cat "$OUT/SHA256SUMS"
echo "release $VERSION artifacts are in $OUT"
