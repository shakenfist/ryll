# syntax=docker/dockerfile:1
FROM mcr.microsoft.com/devcontainers/base:debian

# Install build dependencies for egui/eframe GUI application
# Graphics libraries required by egui:
#   - libxcb, libx11: X11 windowing
#   - libgl, libegl: OpenGL rendering
#   - libwayland: Wayland support
#   - libxkbcommon: Keyboard handling
# Build tools:
#   - pkg-config: Finding library paths
#   - cmake: Build dependency for aws-lc-sys (via reqwest TLS)
# Audio:
#   - libasound2-dev: ALSA headers (cpal audio output)
#   - libopus-dev: audiopus_sys dynamic-links libopus.so.0 when
#     pkg-config finds it, so cargo-deb's $auto records libopus0 as
#     a runtime dep in the generated .deb. Without it audiopus_sys
#     falls back to source-building the bundled libopus via cmake.
# Test tooling:
#   - openssl: tools/web-smoke.sh --tls generates a throwaway cert
# Windows cross-check: gcc-mingw-w64-x86-64, g++-mingw-w64-x86-64 let
#   `make check-windows` cross-check the x86_64-pc-windows-gnu target
#   from Linux.
RUN apt-get update && apt-get install -y \
    build-essential \
    pkg-config \
    cmake \
    libasound2-dev \
    libopus-dev \
    libxcb-render0-dev \
    libxcb-shape0-dev \
    libxcb-xfixes0-dev \
    libxcb1-dev \
    libx11-dev \
    libxkbcommon-dev \
    libgl1-mesa-dev \
    libegl1-mesa-dev \
    libwayland-dev \
    libssl-dev \
    openssl \
    gcc-mingw-w64-x86-64 \
    g++-mingw-w64-x86-64 \
    && rm -rf /var/lib/apt/lists/*

# Create a shared build directory that any user can use
RUN mkdir -p /build && chmod 777 /build

# Install Rust stable via rustup into /build
# Use umask 0000 so directories are world-writable (any user can write to cargo cache)
ENV CARGO_HOME=/build/.cargo
ENV RUSTUP_HOME=/build/.rustup
RUN umask 0000 && curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | \
    sh -s -- -y --default-toolchain stable

# Add cargo to PATH
ENV PATH="/build/.cargo/bin:${PATH}"

# Install useful cargo tools
RUN umask 0000 && rustup component add rustfmt clippy

# Bake in the x86_64-pc-windows-gnu target for `make check-windows`.
# RUSTUP_HOME lives in the image, not a volume mount, so an ad-hoc
# `rustup target add` at check time would re-download the target on
# every container run instead of persisting.
RUN umask 0000 && rustup target add x86_64-pc-windows-gnu

# Packaging tools used by the deb/rpm Makefile targets and CI.
RUN umask 0000 && cargo install cargo-deb cargo-generate-rpm --locked

# Verify installation
RUN rustc --version && cargo --version
