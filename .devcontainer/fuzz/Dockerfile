# syntax=docker/dockerfile:1
# Fuzzing devcontainer: extends the base ryll-dev image (build that
# first with `make devcontainer`) with the nightly toolchain and
# cargo-fuzz. cargo-fuzz needs nightly for coverage-instrumented
# builds; rustfmt is requested explicitly for the fuzz-crate format
# check (bare nightly does not include it). Built by
# `make fuzz-devcontainer`, used by the fuzz-* Makefile targets and
# the CI fuzz job.
FROM ryll-dev

# umask 0000 keeps the shared /build toolchain and cargo dirs
# world-writable so any host UID can use them, matching the base
# image's rustup install.
RUN umask 0000 && rustup toolchain install nightly --component rustfmt

RUN umask 0000 && cargo install cargo-fuzz --locked

# Verify installation
RUN cargo +nightly --version && cargo fuzz --version
