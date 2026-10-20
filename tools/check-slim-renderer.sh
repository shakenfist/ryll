#!/bin/bash
# Guards the slim build of shakenfist-spice-renderer: the renderer with
# --no-default-features must still compile, lint clean, and must not pull
# in any of the heavy or native-library crates its optional features gate
# (H.264/JPEG codecs, USB, WebDAV, audio output). A server-role consumer
# such as an RDP frontend builds the renderer this way, and nothing else in
# CI does, so an ungated `use` or a dependency that quietly loses its
# `optional = true` would otherwise go unnoticed.
#
# Three checks, all with --frozen:
#   1. clippy with no default features, warnings denied.
#   2. clippy with each feature on its own, warnings denied, so no
#      feature silently depends on another and no gating mistake hides
#      as a warning (an unused import, say) in a combination nothing
#      else builds.
#   3. the normal+build dependency tree of the slim build must not contain
#      any banned package.
#
# Run INSIDE the devcontainer (make lint does this): the cargo cache is
# mounted offline, so --frozen is required.
#
# Banned-name matching: `cargo tree --prefix none --format '{p}'` prints one
# "name vX.Y.Z (source)" line per package. We take the first field (the
# exact package name) and compare it for whole-string equality against the
# banned list, so `hyper` matches only `hyper` and never `hyper-util`, and
# `hyper-util` does not hide a real `hyper`.

set -euo pipefail

PKG=shakenfist-spice-renderer

# Features of the renderer, each checked in isolation.
FEATURES=(audio usbredir webdav encoder h264-decode mozjpeg digest-decode)

# Packages that must not appear in the slim build's dependency tree.
BANNED=(openh264 openh264-sys2 mozjpeg-sys nusb dav-server hyper cpal alsa-sys)

echo "==> clippy: ${PKG} --no-default-features"
cargo clippy --frozen -p "${PKG}" --no-default-features --all-targets -- -D warnings

for f in "${FEATURES[@]}"; do
    echo "==> clippy: ${PKG} --no-default-features --features ${f}"
    cargo clippy --frozen -p "${PKG}" --no-default-features --features "${f}" --all-targets -- -D warnings
done

echo "==> dependency tree: ${PKG} --no-default-features"
present=$(cargo tree --frozen -p "${PKG}" --no-default-features -e normal,build \
    --prefix none --format '{p}' | awk '{print $1}' | sort -u)

failed=0
for crate in "${BANNED[@]}"; do
    if grep -Fxq -- "${crate}" <<<"${present}"; then
        failed=1
        echo "ERROR: banned crate '${crate}' is in the slim ${PKG} dependency tree." >&2
        echo "Pulled in via:" >&2
        cargo tree --frozen -i "${crate}" -p "${PKG}" --no-default-features -e normal,build >&2 || true
    fi
done

if [ "${failed}" -ne 0 ]; then
    echo "Slim renderer check FAILED: gate the dependency behind a feature (optional = true)." >&2
    exit 1
fi
echo "Slim renderer check passed."
