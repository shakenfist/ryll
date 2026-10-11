#!/bin/bash
# Runs INSIDE the ryll-wheel-<arch> image (.devcontainer/wheel/
# Dockerfile, built on quay.io/pypa/manylinux_2_28_<arch>: AlmaLinux 8,
# glibc 2.28), with no network and the cargo cache mounted read-only.
# Invoked by tools/build-ryll-wheel.sh -- not meant to be run on a
# normal host. Builds the ryll GUI binary into a native manylinux_2_28
# wheel with maturin (bindings=bin), skipping auditwheel.
#
# Skipping auditwheel is correct here: the GUI/audio libraries
# (libGL/EGL, wayland, xcb, xkbcommon, X11, opus) are dlopen'd at
# runtime and are NOT in the binary's DT_NEEDED, so there is nothing for
# auditwheel to vendor; the only non-standard DT_NEEDED entry is
# libasound.so.2, and the max glibc symbol version is 2.28, so the
# manylinux_2_28 tag is honest. The GUI libs remain a runtime system
# requirement (documented in the README), as they must be for any Linux
# GUI application.

set -euo pipefail

# The build cache lives on the container filesystem, not the mounted
# repo, so the only thing written back to /work is the wheel.
export CARGO_TARGET_DIR=/build/ryll-target
rustc --version
maturin --version

# --frozen: build from Cargo.lock as fetched, and fail rather than reach
# for a network that is not there.
cd /work/ryll
maturin build --release --frozen \
    --compatibility manylinux_2_28 \
    --skip-auditwheel \
    --out /work/target/wheels
