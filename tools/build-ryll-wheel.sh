#!/usr/bin/env bash
# Build a manylinux_2_28 wheel of the ryll GUI binary for one arch by
# running the in-container build inside the matching manylinux image.
# The wheel EMBEDS the compiled `ryll` binary (maturin bindings=bin);
# pip installs it onto PATH with no runtime download. See
# tools/build-ryll-wheel-in-container.sh and
# docs/plans/PLAN-pip-distribution.md.
#
# Usage: tools/build-ryll-wheel.sh <x86_64|aarch64>
# Output: target/wheels/ryll-<version>-py3-none-manylinux_2_28_<arch>.whl
#
# The runner MUST be the same architecture as <arch>: the build runs a
# native container (no QEMU emulation of the compile).

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
IMAGE="quay.io/pypa/manylinux_2_28_${ARCH}"

docker run --rm \
    -v "${REPO_ROOT}:/work" \
    -e "HOST_UID=$(id -u)" \
    -e "HOST_GID=$(id -g)" \
    "$IMAGE" \
    /work/tools/build-ryll-wheel-in-container.sh

echo 'Built wheel(s):'
ls -l "${REPO_ROOT}/target/wheels"
