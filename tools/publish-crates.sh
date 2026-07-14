#!/usr/bin/env bash
# Publish all workspace crates to crates.io in dependency order.
#
# cargo publish waits for each crate to appear in the index before
# returning, so serial publishing is safe.
#
# Dependency graph (→ means "depends on"):
#   protocol          (leaf)
#   compression       → protocol
#   usbredir          → protocol
#   renderer          → protocol + compression + usbredir
#   webrtc            → renderer
#   ryll              → all of the above
#
# Run via `make publish-crates`, which executes this inside the
# devcontainer with CARGO_REGISTRY_TOKEN forwarded from the caller's
# environment.

set -euo pipefail

if [ -z "${CARGO_REGISTRY_TOKEN:-}" ]; then
    echo "ERROR: CARGO_REGISTRY_TOKEN is not set"
    exit 1
fi

for crate in \
        shakenfist-spice-protocol \
        shakenfist-spice-compression \
        shakenfist-spice-usbredir \
        shakenfist-spice-renderer \
        shakenfist-spice-webrtc \
        ryll; do
    echo "=== Publishing ${crate} ==="
    cargo publish -p "${crate}"
done
