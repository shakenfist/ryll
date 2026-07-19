#!/bin/bash
# Runs INSIDE quay.io/pypa/manylinux_2_28_<arch> (AlmaLinux 8, glibc
# 2.28). Invoked by tools/build-ryll-wheel.sh -- not meant to be run on
# a normal host. Builds the ryll GUI binary into a native
# manylinux_2_28 wheel with maturin (bindings=bin), skipping auditwheel.
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

HOST_UID="${HOST_UID:-1000}"
HOST_GID="${HOST_GID:-1000}"

# Build-time dev libraries. Only alsa-lib-devel plus the C/C++ toolchain
# (gcc/g++/cmake, needed by aws-lc-sys and openh264 in the dep graph)
# are strictly required to link today. The GUI/EGL/wayland/xcb/xkb and
# opus -devel packages are belt-and-suspenders -- those libs are
# dlopen'd, not DT_NEEDED -- kept so a future crate bump that starts
# hard-linking one of them still builds.
dnf -y install epel-release
dnf config-manager --set-enabled powertools
dnf -y install \
    gcc gcc-c++ make cmake pkgconf-pkg-config \
    alsa-lib-devel opus-devel \
    libxcb-devel libxkbcommon-devel libxkbcommon-x11-devel \
    wayland-devel libX11-devel \
    mesa-libGL-devel mesa-libEGL-devel

# Toolchain and build caches live on the container filesystem, not the
# mounted repo, so the only thing written back to /work is the wheel.
export CARGO_HOME=/root/.cargo
export RUSTUP_HOME=/root/.rustup
export CARGO_TARGET_DIR=/root/ryll-target
export PATH="$CARGO_HOME/bin:$PATH"
if ! command -v cargo >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
        | sh -s -- -y --default-toolchain stable --profile minimal
fi
rustc --version

PY=/opt/python/cp311-cp311/bin
"$PY/pip" install --quiet 'maturin>=1.7,<2'

cd /work/ryll
"$PY/maturin" build --release \
    --compatibility manylinux_2_28 \
    --skip-auditwheel \
    --out /work/target/wheels

# The container runs as root; hand the wheel back to the host user.
chown -R "${HOST_UID}:${HOST_GID}" /work/target/wheels
