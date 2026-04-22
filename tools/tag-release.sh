#!/bin/bash
#
# Tag and push a release of the ryll workspace.
#
# Meant to be run after the release-X.Y.Z PR produced by
# tools/propose-release.sh has been reviewed and merged into develop.
# Fetches origin/develop, verifies its tip has the expected workspace
# version, and (after confirmation) creates an annotated tag vX.Y.Z
# pointing at that commit and pushes it. Pushing the tag triggers
# .github/workflows/release.yml, which builds binaries, publishes
# the four workspace crates to crates.io, creates a GitHub Release,
# and updates the Homebrew tap.
#
# Usage:
#   tools/tag-release.sh VERSION
#   make tag-release VERSION
#
# Example:
#   make tag-release 0.1.4
#
# Requirements on the host:
#   - gh CLI: for watching the release workflow
#   - jq:     for parsing gh output

set -euo pipefail

err() { echo "error: $*" >&2; exit 1; }
info() { echo "==> $*"; }

# --- arg parsing ---

[[ $# -eq 1 ]] || err "usage: $0 VERSION (e.g. 0.1.4)"
VERSION="$1"

[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
    || err "version must be X.Y.Z, got: $VERSION"

TAG="v$VERSION"

# --- tool availability ---

command -v gh >/dev/null || err "gh CLI not installed"
command -v jq >/dev/null || err "jq not installed"

# --- working directory must be repo root ---

cd "$(dirname "$0")/.."
[[ -f Cargo.toml ]] || err "could not find repo root"

# --- fetch latest develop ---

info "Fetching origin"
git fetch origin develop --tags --quiet

# --- tag must not already exist ---

if git rev-parse "$TAG" >/dev/null 2>&1; then
    err "tag $TAG already exists locally"
fi
if git ls-remote --tags --exit-code origin "$TAG" >/dev/null 2>&1; then
    err "tag $TAG already exists on origin"
fi

# --- verify workspace version on origin/develop ---

info "Verifying [workspace.package].version on origin/develop"
WORKSPACE_TOML=$(git show origin/develop:Cargo.toml)
ACTUAL=$(printf '%s\n' "$WORKSPACE_TOML" | awk '
    /^\[workspace\.package\]/ { in_wp=1; next }
    /^\[/ { in_wp=0 }
    in_wp && /^version *= */ {
        gsub(/version *= *"|"/, "")
        print
        exit
    }
')

[[ -n "$ACTUAL" ]] \
    || err "could not read [workspace.package].version from origin/develop"
[[ "$ACTUAL" == "$VERSION" ]] \
    || err "origin/develop workspace version is $ACTUAL, expected $VERSION. Has the release-$VERSION PR been merged?"

TARGET_SHA=$(git rev-parse origin/develop)
TARGET_SUBJECT=$(git log -1 --format=%s origin/develop)

# --- confirmation ---

echo
echo "About to tag $TAG at $TARGET_SHA on origin/develop:"
echo "  $TARGET_SUBJECT"
echo
echo "Pushing this tag will trigger the release workflow:"
echo "  - build binaries on Linux / macOS / Windows"
echo "  - publish all four crates to crates.io (IRREVERSIBLE)"
echo "  - create the GitHub Release"
echo "  - update the Homebrew tap"
echo
read -rp "Create and push tag $TAG? [y/N] " REPLY
[[ "$REPLY" =~ ^[Yy]$ ]] || {
    info "Aborted."
    exit 1
}

# --- tag and push ---

info "Creating annotated tag $TAG"
git tag -a "$TAG" -m "Release $VERSION" "$TARGET_SHA"

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
