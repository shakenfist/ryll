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
# for a network that is not there. The wheel is written to the
# container's filesystem and copied into the checkout only once the
# check below passes, so a failed check leaves no wheel behind.
WHEEL_DIR=/build/wheels
cd /work/ryll
maturin build --release --frozen \
    --compatibility manylinux_2_28 \
    --skip-auditwheel \
    --out "${WHEEL_DIR}"
WHEELS=("${WHEEL_DIR}"/*.whl)
if [ "${#WHEELS[@]}" -ne 1 ] || [ ! -f "${WHEELS[0]}" ]; then
    echo "expected one wheel in ${WHEEL_DIR}, found: ${WHEELS[*]}" >&2
    exit 1
fi

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

# Check the binary the wheel ships rather than the one in the cargo
# target directory, so the check cannot drift from the artifact. Each
# step is a plain assignment, which set -e sees fail. An empty DT_NEEDED
# is an error too: every dynamically linked glibc binary needs
# libc.so.6, so an empty list means nothing was read, not a pass.
BINARY="$(python3 -I - "${WHEELS[0]}" /build/wheel-contents <<'EOF'
import sys
import zipfile

wheel, dest = sys.argv[1:]
with zipfile.ZipFile(wheel) as zf:
    names = [n for n in zf.namelist() if n.endswith('.data/scripts/ryll')]
    if len(names) != 1:
        sys.exit(f'expected one ryll binary in {wheel}, found {names}')
    print(zf.extract(names[0], dest))
EOF
)"
DYNAMIC="$(readelf -d "${BINARY}")"
NEEDED_LIBS="$(sed -n 's/.*(NEEDED).*\[\(.*\)\]/\1/p' <<< "${DYNAMIC}")"
if [ -z "${NEEDED_LIBS}" ]; then
    echo "no DT_NEEDED entries read from ${BINARY}; the check did not run" >&2
    exit 1
fi
mapfile -t NEEDED <<< "${NEEDED_LIBS}"
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
cp "${WHEELS[0]}" /work/target/wheels/
