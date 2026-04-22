#!/bin/bash
#
# Cut a release of the ryll workspace.
#
# Bumps the workspace version to the one given on the command line,
# runs tests, and (after confirmation) commits, tags, and pushes.
# The push of the tag triggers .github/workflows/release.yml, which
# builds binaries, publishes the four workspace crates to crates.io,
# creates a GitHub Release, and updates the Homebrew tap.
#
# Usage:
#   tools/cut-release.sh VERSION
#   make publish VERSION
#
# Example:
#   make publish 0.1.4
#
# Requirements on the host:
#   - cargo-release:  cargo install --locked cargo-release
#   - gh CLI:         for watching the release workflow
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

# --- tool availability ---

command -v cargo-release >/dev/null \
    || err "cargo-release not installed. Run: cargo install --locked cargo-release"
command -v gh >/dev/null \
    || err "gh CLI not installed"
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

info "Running workspace tests"
cargo test --workspace

# --- confirmation ---

echo
echo "About to release $TAG. Pending changes:"
git diff --stat
echo
read -rp "Release $TAG? [y/N] " REPLY
[[ "$REPLY" =~ ^[Yy]$ ]] || {
    info "Aborted. Version bumps left uncommitted; revert with: git checkout -- ."
    exit 1
}

# --- commit, tag, push ---

info "Creating release commit"
git add -u
git commit -m "Release ${VERSION}."

info "Creating annotated tag $TAG"
git tag -a "$TAG" -m "Release $VERSION"

info "Pushing develop"
git push origin develop

info "Pushing tag $TAG (this triggers the release workflow)"
git push origin "$TAG"

# --- watch the workflow ---

info "Waiting for release workflow to start"
sleep 5
RUN_ID=$(gh run list \
    --workflow=release.yml \
    --limit 1 \
    --json databaseId \
    --jq '.[0].databaseId')

if [[ -n "$RUN_ID" ]]; then
    info "Watching workflow run $RUN_ID"
    gh run watch "$RUN_ID" || info "workflow did not complete cleanly"
    info "Opening release page"
    gh release view "$TAG" --web || true
else
    info "Could not find workflow run. Check manually: gh run list --workflow=release.yml"
fi
