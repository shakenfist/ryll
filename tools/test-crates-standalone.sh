#!/bin/bash
# Runs each published library crate's tests in its own cargo
# resolution, which is how a crates.io consumer builds it and how
# nothing else in CI does.
#
# `make test` is one invocation over the whole workspace, and cargo
# unifies features across every package in an invocation. So a library
# crate's tests run with whatever its siblings switch on, not with the
# features it has by itself. The case that produced this script:
# ryll's `rdp` feature reaches flate2's `zlib` backend through
# ironrdp-server -> sspi -> winscard, so under `make test` the
# renderer's zlib-GLZ decoder runs on native zlib, while a consumer of
# the published crate gets flate2's default miniz_oxide. The two
# inflate differently at a boundary a test depended on: in #633 a
# declared-size test passed under `make test` and failed under
# `cargo test -p shakenfist-spice-renderer --lib`. A local mutation run
# caught it; CI could not have. See shakenfist/ryll#634.
#
# One `-p` per invocation, deliberately: naming several together brings
# feature unification back on a smaller scale, which is the whole bug.
#
# Each crate is run with its own default features -- what a consumer
# gets from a plain dependency -- plus the slim renderer
# (`--no-default-features`), the configuration a server-role consumer
# builds and the one `tools/check-slim-renderer.sh` lints but never
# tests.
#
# Not covered here, and deliberately: the full product of every
# feature combination. The lint lane already builds those (clippy, per
# feature, for the renderer and the compression crate); this is about
# test behaviour differing between resolutions, and the default and
# slim sets are where a consumer actually lives.
#
# Run INSIDE the devcontainer (make test-crates does this): the cargo
# cache is mounted offline, so --frozen is required.

set -euo pipefail

# The library crates published to crates.io, in dependency order (the
# order tools/publish-crates.sh uses). `ryll` itself is the binary and
# is covered by `make test`.
CRATES=(
    shakenfist-spice-protocol
    shakenfist-spice-compression
    shakenfist-spice-usbredir
    shakenfist-spice-renderer
    shakenfist-spice-webrtc
    shakenfist-spice-rdp
)

failed=()

for crate in "${CRATES[@]}"; do
    echo "==> cargo test -p ${crate} (its own default features)"
    if ! cargo test --frozen -p "${crate}"; then
        failed+=("${crate}")
    fi
done

echo "==> cargo test -p shakenfist-spice-renderer --no-default-features"
if ! cargo test --frozen -p shakenfist-spice-renderer --no-default-features; then
    failed+=("shakenfist-spice-renderer --no-default-features")
fi

if [ "${#failed[@]}" -ne 0 ]; then
    echo >&2
    echo "Standalone crate tests FAILED for:" >&2
    for crate in "${failed[@]}"; do
        echo "  - ${crate}" >&2
    done
    echo >&2
    echo "These pass under 'make test' only because the workspace build" >&2
    echo "switches on a sibling's features. Fix the crate or its test so" >&2
    echo "it holds in the resolution a consumer gets." >&2
    exit 1
fi

echo "Standalone crate tests passed."
