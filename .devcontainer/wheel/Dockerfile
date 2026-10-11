# syntax=docker/dockerfile:1
# Wheel build image: the manylinux_2_28 base (AlmaLinux 8, glibc 2.28)
# with the C toolchain, the -devel libraries, a Rust toolchain and
# maturin baked in. Built by tools/build-ryll-wheel.sh, which then runs
# `cargo fetch` in it with the network up and the maturin compile in it
# with the network severed. Everything that needs a download has to
# happen here, at image build time, because the compile cannot reach
# the network. See docs/ci.md's "Build network isolation".
ARG ARCH
FROM quay.io/pypa/manylinux_2_28_${ARCH}

# Only alsa-lib-devel plus the C/C++ toolchain (gcc/g++/cmake, needed
# by aws-lc-sys and openh264 in the dependency graph) are strictly
# required to link today. The GUI/EGL/wayland/xcb/xkb and opus -devel
# packages are belt-and-braces -- those libraries are dlopen'd, not
# DT_NEEDED -- kept so a future crate bump that starts hard-linking one
# of them still builds.
RUN dnf -y install epel-release \
    && dnf config-manager --set-enabled powertools \
    && dnf -y install \
        gcc gcc-c++ make cmake pkgconf-pkg-config \
        alsa-lib-devel opus-devel \
        libxcb-devel libxkbcommon-devel libxkbcommon-x11-devel \
        wayland-devel libX11-devel \
        mesa-libGL-devel mesa-libEGL-devel \
    && dnf clean all

# The build runs as the invoking host user, which has no passwd entry
# here, so the toolchain lives in a world-writable /build like the
# ryll-dev devcontainer's. umask 0000 keeps what rustup writes there
# writable too: cargo takes its package-cache lock under CARGO_HOME.
RUN mkdir -p /build && chmod 777 /build
ENV CARGO_HOME=/build/.cargo
ENV RUSTUP_HOME=/build/.rustup
RUN umask 0000 && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --default-toolchain stable --profile minimal
ENV PATH="/build/.cargo/bin:/opt/python/cp311-cp311/bin:${PATH}"

RUN pip install --quiet 'maturin>=1.7,<2'

RUN rustc --version && cargo --version && maturin --version
