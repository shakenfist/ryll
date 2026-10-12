#!/usr/bin/env bash
# Build a manylinux_2_28 wheel of the ryll GUI binary for one arch by
# running the in-container build inside an image derived from the
# matching manylinux image (.devcontainer/wheel/Dockerfile). The wheel
# EMBEDS the compiled `ryll` binary (maturin bindings=bin); pip installs
# it onto PATH with no runtime download. See
# tools/build-ryll-wheel-in-container.sh.
#
# Usage: tools/build-ryll-wheel.sh <x86_64|aarch64>
# Output: target/wheels/ryll-<version>-py3-none-manylinux_2_28_<arch>.whl
#
# The runner MUST be the same architecture as <arch>: the build runs a
# native container (no QEMU emulation of the compile).
#
# The build is split the way the Makefile splits every other compile
# (docs/ci.md, "Build network isolation"): `cargo fetch` downloads the
# crates in Cargo.lock with the network up but compiles nothing, so no
# dependency's build script runs then; the compile then runs with
# --network none and the download cache mounted read-only. The cache is
# the Makefile's (.cargo-cache, or $CARGO_CACHE), so a checkout that has
# already run `make fetch` only downloads what is missing.

set -euo pipefail

ARCH="${1:?usage: build-ryll-wheel.sh <x86_64|aarch64>}"
case "$ARCH" in
    x86_64 | aarch64) ;;
    *)
        echo "unsupported arch: $ARCH (expected x86_64 or aarch64)" >&2
        exit 1
        ;;
esac

REPO_ROOT="$(cd "$(dirname "$0")/.." && pwd)"
IMAGE="ryll-wheel-${ARCH}"
CACHE_DIR="$(cd "$REPO_ROOT" && realpath -m "${CARGO_CACHE:-.cargo-cache}")"

docker build -t "$IMAGE" --build-arg "ARCH=${ARCH}" \
    "${REPO_ROOT}/.devcontainer/wheel"

# Created here, as the invoking user, so docker does not create them
# root-owned. The build runs as the invoking user too, and writes only
# the wheel back into the checkout.
mkdir -p "${CACHE_DIR}/registry" "${CACHE_DIR}/git" "${REPO_ROOT}/target/wheels"

RUN_ARGS=(
    --rm
    -v "${REPO_ROOT}:/work"
    -w /work
    -u "$(id -u):$(id -g)"
    -e HOME=/build
)

docker run "${RUN_ARGS[@]}" \
    -v "${CACHE_DIR}/registry:/build/.cargo/registry" \
    -v "${CACHE_DIR}/git:/build/.cargo/git" \
    "$IMAGE" \
    cargo fetch --locked

docker run "${RUN_ARGS[@]}" \
    --network none \
    -v "${CACHE_DIR}/registry:/build/.cargo/registry:ro" \
    -v "${CACHE_DIR}/git:/build/.cargo/git:ro" \
    "$IMAGE" \
    /work/tools/build-ryll-wheel-in-container.sh

echo 'Built wheel(s):'
ls -l "${REPO_ROOT}/target/wheels"
