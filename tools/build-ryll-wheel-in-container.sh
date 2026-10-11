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

# The manylinux_2_28 library allowlist, read from the policy of the
# auditwheel this image ships rather than copied by hand, so it follows
# the base image. Then the dynamic linker, which the policy leaves
# implicit, and the one deliberate exception, libasound.so.2. The
# python is auditwheel's own venv's, the only one it is importable
# from; it exits non-zero unless it finds exactly one non-empty list.
AUDITWHEEL_PYTHON="$(dirname "$(readlink -f "$(command -v auditwheel)")")/python"
POLICY_LIBS="$("${AUDITWHEEL_PYTHON}" -I -c '
import importlib.resources, json, sys
policies = json.loads(importlib.resources.files("auditwheel.policy")
                      .joinpath("manylinux-policy.json").read_text())
found = [p["lib_whitelist"] for p in policies if p["name"] == "manylinux_2_28"]
if len(found) != 1 or not found[0]:
    sys.exit("no manylinux_2_28 lib_whitelist in auditwheel policy")
print(" ".join(found[0]))
')"
read -ra ALLOWED <<< "${POLICY_LIBS}"
ALLOWED+=(ld-linux-x86-64.so.2 ld-linux-aarch64.so.1 libasound.so.2)
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
