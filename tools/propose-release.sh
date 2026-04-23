#!/bin/bash
#
# Propose a release of the ryll workspace.
#
# Creates a `release-X.Y.Z` branch from develop, bumps the workspace
# version, runs the test suite, and (after confirmation) commits and
# pushes the branch. Does NOT open a PR and does NOT tag — both of
# those happen outside the script:
#
#   1. Run this script.
#   2. Open a PR from release-X.Y.Z to develop, get it reviewed
#      and merged like any other change.
#   3. Run tools/tag-release.sh X.Y.Z to tag the merge commit on
#      develop and push the tag (that is what triggers
#      .github/workflows/release.yml).
#
# Usage:
#   tools/propose-release.sh VERSION
#   make propose-release VERSION
#
# Example:
#   make propose-release 0.1.4
#
# Requirements on the host:
#   - cargo-release:  cargo install --locked cargo-release
#     (or cargo install --locked cargo-release@0.25.18 on rustc 1.85)
#   - curl, jq:       for querying crates.io

set -euo pipefail

CRATES=(
    ryll
    shakenfist-spice-protocol
    shakenfist-spice-compression
    shakenfist-spice-usbredir
)

err() { echo "error: $*" >&2; exit 1; }
info() { echo "==> $*"; }

# --- arg parsing ---

[[ $# -eq 1 ]] || err "usage: $0 VERSION (e.g. 0.1.4)"
VERSION="$1"

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || err "version must be X.Y.Z, got: $VERSION"

TAG="v$VERSION"
RELEASE_BRANCH="release-$VERSION"

# --- tool availability ---

command -v cargo-release >/dev/null \
    || err "cargo-release not installed. Run: cargo install --locked cargo-release"
command -v jq >/dev/null || err "jq not installed"

# --- working directory must be repo root ---

cd "$(dirname "$0")/.."
[[ -f Cargo.toml ]] || err "could not find repo root"

# --- git state checks ---

info "Checking git state"

BRANCH=$(git rev-parse --abbrev-ref HEAD)
[[ "$BRANCH" == "develop" ]] \
    || err "must be on develop, currently on: $BRANCH"

[[ -z "$(git status --porcelain)" ]] \
    || err "working tree is dirty; commit or stash first"

git fetch origin develop --quiet
LOCAL=$(git rev-parse HEAD)
REMOTE=$(git rev-parse origin/develop)
[[ "$LOCAL" == "$REMOTE" ]] \
    || err "local develop is not in sync with origin/develop"

if git rev-parse "$TAG" >/dev/null 2>&1; then
    err "tag $TAG already exists locally"
fi
if git ls-remote --tags --exit-code origin "$TAG" >/dev/null 2>&1; then
    err "tag $TAG already exists on origin"
fi

if git rev-parse --verify "$RELEASE_BRANCH" >/dev/null 2>&1; then
    err "branch $RELEASE_BRANCH already exists locally"
fi
if git ls-remote --heads --exit-code origin "$RELEASE_BRANCH" >/dev/null 2>&1; then
    err "branch $RELEASE_BRANCH already exists on origin"
fi

# --- crates.io version availability ---

info "Checking crates.io for existing $VERSION"

for crate in "${CRATES[@]}"; do
    # The crates.io versions endpoint returns a JSON object with a
    # "version" key when present, and 404 when the version does not
    # exist. We treat anything other than 404 as "taken".
    url="https://crates.io/api/v1/crates/$crate/$VERSION"
    code=$(curl -s -o /dev/null -w '%{http_code}' \
        -A 'ryll-release-script (mikal@stillhq.com)' "$url")
    case "$code" in
        404) ;;  # good, version is free
        200) err "$crate $VERSION already published on crates.io" ;;
        *)   err "unexpected HTTP $code checking $crate $VERSION" ;;
    esac
done

# --- create release branch ---

info "Creating branch $RELEASE_BRANCH from develop"
git switch -c "$RELEASE_BRANCH"

# Ensure we clean up the branch if the script aborts after this
# point without a successful push.
CLEANUP_BRANCH=1
cleanup() {
    if [[ "${CLEANUP_BRANCH:-0}" == "1" ]]; then
        info "Cleaning up: switching back to develop and deleting $RELEASE_BRANCH"
        git checkout -- . 2>/dev/null || true
        git switch develop 2>/dev/null || true
        git branch -D "$RELEASE_BRANCH" 2>/dev/null || true
    fi
}
trap cleanup EXIT

# --- pre-commit ---

info "Running pre-commit"
pre-commit run --all-files

# --- bump versions ---

info "Bumping workspace to $VERSION"
cargo release version "$VERSION" \
    --workspace \
    --execute \
    --no-confirm

# --- final test gate ---
#
# Delegate to `make test` so the test compile/run uses the
# devcontainer's toolchain. Running `cargo test` directly here
# would pick up the host's rustc, which is typically older than
# what the workspace's dependency tree requires.

info "Running workspace tests (via make test, in devcontainer)"
make test

# --- confirmation ---

echo
echo "About to propose release $VERSION on branch $RELEASE_BRANCH."
echo "Pending changes:"
git diff --stat
echo
read -rp "Commit and push $RELEASE_BRANCH? [y/N] " REPLY
[[ "$REPLY" =~ ^[Yy]$ ]] || {
    info "Aborted at confirmation."
    exit 1
}

# --- commit and push ---

info "Creating release proposal commit"
git add -u
git commit -m "Release ${VERSION}."

info "Pushing $RELEASE_BRANCH"
git push --set-upstream origin "$RELEASE_BRANCH"

# Successful push — disable cleanup so we leave the user on the
# release branch for PR creation.
CLEANUP_BRANCH=0

echo
info "Release proposed on branch $RELEASE_BRANCH."
echo
echo "Next steps:"
echo "  1. Open a PR from $RELEASE_BRANCH into develop:"
echo "     https://github.com/shakenfist/ryll/pull/new/$RELEASE_BRANCH"
echo "  2. Get it reviewed and merged."
echo "  3. Run 'make tag-release $VERSION' to tag develop and"
echo "     trigger the release workflow."
