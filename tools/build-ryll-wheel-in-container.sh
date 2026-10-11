#!/bin/bash
# Runs INSIDE the ryll-wheel-<arch> image (.devcontainer/wheel/
# Dockerfile, built on quay.io/pypa/manylinux_2_28_<arch>: AlmaLinux 8,
# glibc 2.28), with no network and the cargo cache mounted read-only.
# Invoked by tools/build-ryll-wheel.sh -- not meant to be run on a
# normal host. Builds the ryll GUI binary into a native manylinux_2_28
# wheel with maturin (bindings=bin), skipping auditwheel.
#
# What the manylinux_2_28 tag does and does not promise. The glibc
# floor holds: the image's glibc is 2.28, so nothing can link against
# a newer symbol version. The GUI libraries (libGL/EGL, wayland, xcb,
# xkbcommon, X11) and opus are dlopen'd at runtime rather than
# DT_NEEDED. But libasound.so.2 IS DT_NEEDED (cpal links ALSA through
# alsa-sys), and it is not on the manylinux allowlist, so auditwheel
# would classify the wheel as plain linux_<arch>. The tag is asserted
# with --skip-auditwheel anyway, deliberately; docs/releasing.md ("The
# PyPI wheel's platform tag") says why and what it costs. What keeps
# that to one exception is the check after the build, which fails if
# any other library outside the policy appears in DT_NEEDED.

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

# The manylinux_2_28 library allowlist, from auditwheel 6.8.2's
# manylinux-policy.json in this image, plus the dynamic linker, plus
# the one deliberate exception, libasound.so.2.
ALLOWED=(
    libgcc_s.so.1 libstdc++.so.6 libm.so.6 libmvec.so.1 libdl.so.2
    librt.so.1 libc.so.6 libnsl.so.1 libutil.so.1 libpthread.so.0
    libresolv.so.2 libanl.so.1 libatomic.so.1 libexpat.so.1 libz.so.1
    libX11.so.6 libXext.so.6 libXrender.so.1 libICE.so.6 libSM.so.6
    libGL.so.1 libgobject-2.0.so.0 libgthread-2.0.so.0
    libglib-2.0.so.0 ld-linux-x86-64.so.2 ld-linux-aarch64.so.1
    libasound.so.2
)
mapfile -t NEEDED < <(readelf -d "${CARGO_TARGET_DIR}/release/ryll" \
    | sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p')
UNEXPECTED=()
for lib in "${NEEDED[@]}"; do
    if [[ " ${ALLOWED[*]} " != *" ${lib} "* ]]; then
        UNEXPECTED+=("$lib")
    fi
done
if [ "${#UNEXPECTED[@]}" -gt 0 ]; then
    echo "ryll links libraries outside the manylinux_2_28 policy: ${UNEXPECTED[*]}" >&2
    echo 'Make them dlopen-only, or see docs/releasing.md before allowing one.' >&2
    exit 1
fi
echo "DT_NEEDED is within the manylinux_2_28 policy plus libasound: ${NEEDED[*]}"
