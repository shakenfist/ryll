#!/bin/bash
# Run rustfmt and clippy checks for ryll
# Used by pre-commit hooks

set -e

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(dirname "$SCRIPT_DIR")"

# Docker image to use (the same devcontainer image used for builds)
IMAGE="ryll-dev"

# Check if docker image exists, build if not
if ! docker image inspect "$IMAGE" &>/dev/null; then
    echo "Building $IMAGE docker image..."
    docker build -t "$IMAGE" "$PROJECT_ROOT/.devcontainer/"
fi

MODE="${1:-check}"  # "check" or "fix"

# Detect user/group for permission-safe container builds
UID_VAL=$(id -u)
GID_VAL=$(id -g)

run_in_docker() {
    docker run --rm \
        -v "$PROJECT_ROOT":/workspace \
        -v "$PROJECT_ROOT/.cargo-cache/registry":/build/.cargo/registry \
        -v "$PROJECT_ROOT/.cargo-cache/git":/build/.cargo/git \
        -w /workspace \
        -u "$UID_VAL:$GID_VAL" \
        -e HOME=/build \
        "$IMAGE" \
        "$@"
}

FAILED=0

echo "=== Checking ryll ==="

# Run rustfmt
echo "Running rustfmt..."
if [ "$MODE" = "fix" ]; then
    run_in_docker cargo fmt || FAILED=1
else
    run_in_docker cargo fmt --check || FAILED=1
fi

# Run clippy
echo "Running clippy..."
if [ "$MODE" = "fix" ]; then
    run_in_docker cargo clippy --fix --allow-dirty -- -D warnings || FAILED=1
else
    run_in_docker cargo clippy -- -D warnings || FAILED=1
fi

echo ""

if [ $FAILED -ne 0 ]; then
    echo "Some checks failed!"
    exit 1
fi

echo "All checks passed!"
