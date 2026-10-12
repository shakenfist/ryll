#!/usr/bin/env bash
# Build every workspace crate from its packaged form, offline, as the
# verify half of `make package-check`. Runs inside the devcontainer
# with --network none and the shared cargo registry mounted read-only
# at /build/.cargo/registry.
#
# `cargo package --workspace` verifies each dependent against its
# siblings' packages through a temporary local registry, and unpacks
# those packages into $CARGO_HOME/registry/src/<that registry>/. The
# shared registry is read-only here, and cargo reports the failed
# unpack misleadingly, as "no hash listed for <crate>". So run cargo
# against a throwaway CARGO_HOME: the shared index, downloads and
# unpacked crates.io sources are linked in (still read-only), and only
# registry/src itself is writable, inside this container.

set -euo pipefail

shared=/build/.cargo
home=$(mktemp -d)
trap 'rm -rf "${home}"' EXIT

mkdir -p "${home}/registry/src"
ln -s "${shared}/registry/index" "${home}/registry/index"
ln -s "${shared}/registry/cache" "${home}/registry/cache"
for src in "${shared}"/registry/src/*; do
    ln -s "${src}" "${home}/registry/src/"
done
ln -s "${shared}/git" "${home}/git"

CARGO_HOME="${home}" cargo package --workspace --frozen
