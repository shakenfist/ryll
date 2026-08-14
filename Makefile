# Ryll - Rust SPICE VDI Client
# Build and development targets

RYLL_IMAGE := ryll-dev
RYLL_FUZZ_IMAGE := ryll-fuzz
DEVCONTAINER_DIR := .devcontainer
CARGO_CACHE := .cargo-cache

# Test QEMU SPICE server settings
QEMU_SPICE_PORT := 5900
QEMU_PID_FILE := /tmp/ryll-test-qemu.pid
QEMU_TEST_IMAGE := testdata/uefi-latency-guest.qcow2
QEMU_TEST_IMAGE_URL := https://images.shakenfist.com/testimages/uefi-latency-guest.qcow2
OVMF_CODE := /usr/share/OVMF/OVMF_CODE_4M.fd
OVMF_VARS := /usr/share/OVMF/OVMF_VARS_4M.fd
QEMU_VARS_COPY := /tmp/ryll-test-ovmf-vars.fd

# Detect user/group for permission-safe container builds
UID := $(shell id -u)
GID := $(shell id -g)

# Embed the current git SHA into the binary so a running ryll can
# identify which build it is. Computed once on the host (the
# devcontainer can't see the worktree's gitdir) and passed as an
# env var to ryll/build.rs. Falls back to "unknown" if git is
# unavailable. "-dirty" is appended when the working tree has
# uncommitted changes, so dogfooding a quick edit doesn't quietly
# masquerade as the last committed SHA.
RYLL_GIT_SHA := $(shell git rev-parse --short=8 HEAD 2>/dev/null)$(shell test -n "$$(git status --porcelain 2>/dev/null)" && echo -dirty)
RYLL_GIT_SHA := $(if $(RYLL_GIT_SHA),$(RYLL_GIT_SHA),unknown)

# Shared docker-run invocation for working inside the devcontainer.
# CARGO_BUILD_JOBS is forwarded only when set in the caller's
# environment, so parallelism can be bounded on small machines
# (docker omits the variable entirely when it is unset on the
# host). Targets append extra -e flags and the image name; a later
# -w flag overrides the default working directory.
DOCKER_RUN := docker run --rm \
	-v "$(CURDIR)":/workspace \
	-v "$(CURDIR)/$(CARGO_CACHE)/registry":/build/.cargo/registry \
	-v "$(CURDIR)/$(CARGO_CACHE)/git":/build/.cargo/git \
	-w /workspace \
	-u $(UID):$(GID) \
	-e HOME=/build \
	-e CARGO_BUILD_JOBS \
	-e RYLL_GATHERING_SOAK

.PHONY: all build release propose-release tag-release clean clean-testdata \
	devcontainer fuzz-devcontainer ensure-cache lint lint-fix test help \
	deb rpm web-smoke web-smoke-tls fuzz-fmt-check publish-crates \
	test-qemu test-qemu-usb test-qemu-stop test-k1-idle \
	macos-prereqs macos-build macos-release \
	build-tokio-console check-windows

all: build

help:
	@echo "Ryll build targets:"
	@echo "  make build                  - Build debug version"
	@echo "  make release                - Build release version"
	@echo "  make propose-release X.Y.Z  - Branch, bump versions, push for PR review"
	@echo "  make tag-release X.Y.Z      - After PR merge: tag develop, trigger release"
	@echo "  make test                   - Run tests"
	@echo "  make lint                   - Run rustfmt and clippy checks"
	@echo "  make lint-fix               - Run rustfmt and clippy with auto-fix"
	@echo "  make check-windows          - Cross-check the Windows (gnu) target"
	@echo "  make deb                    - Package the release binary as a .deb"
	@echo "  make rpm                    - Package the release binary as an .rpm"
	@echo "  make web-smoke              - Smoke-test ryll --web (plain HTTP)"
	@echo "  make web-smoke-tls          - Smoke-test ryll --web (TLS)"
	@echo "  make devcontainer           - Build the development container"
	@echo "  make fuzz-devcontainer      - Build the fuzzing container (nightly + cargo-fuzz)"
	@echo "  make fuzz-fmt-check         - Format-check the detached fuzz workspace"
	@echo "  make fuzz-build-TARGET      - Build one cargo-fuzz target"
	@echo "  make fuzz-smoke-TARGET      - Smoke-run one cargo-fuzz target (~30s)"
	@echo "  make clean                  - Remove build artifacts"
	@echo ""
	@echo "macOS native (run on a Mac, not in the devcontainer):"
	@echo "  make macos-build            - Native debug build for the host Mac"
	@echo "  make macos-release          - Native release build for the host Mac"
	@echo ""
	@echo "Test SPICE server:"
	@echo "  make test-qemu              - Start a QEMU instance with SPICE on port $(QEMU_SPICE_PORT)"
	@echo "  make test-qemu-usb          - Same, with USB redirection enabled"
	@echo "  make test-qemu-stop         - Stop the test QEMU instance"

# Build the devcontainer image
devcontainer:
	docker build -t $(RYLL_IMAGE) $(DEVCONTAINER_DIR)

# Create cargo cache directories
$(CARGO_CACHE)/registry $(CARGO_CACHE)/git:
	mkdir -p $@

# Ensure cargo cache directories are writable by the build user.
# A previous root-owned docker run can leave these owned by root.
ensure-cache: devcontainer $(CARGO_CACHE)/registry $(CARGO_CACHE)/git
	@if [ ! -w "$(CARGO_CACHE)/registry" ] || [ ! -w "$(CARGO_CACHE)/git" ]; then \
		echo "Fixing cargo cache permissions..."; \
		docker run --rm \
			-v "$(CURDIR)/$(CARGO_CACHE)":/cache \
			$(RYLL_IMAGE) \
			chown -R $(UID):$(GID) /cache; \
	fi

# Build debug version
build: ensure-cache
	$(DOCKER_RUN) \
		-e RYLL_GIT_SHA="$(RYLL_GIT_SHA)" \
		$(RYLL_IMAGE) \
		cargo build -p ryll

# Diagnostic-only build: compile ryll with the tokio-console
# feature on, plus RUSTFLAGS=--cfg tokio_unstable so tokio's
# instrumentation hooks are active. The resulting binary, when
# run with RYLL_TOKIO_CONSOLE=1, exposes a unix socket on
# 127.0.0.1:6669 that the `tokio-console` TUI viewer connects
# to. Used during the K1 hang investigation; will go away when
# the feature is removed.
build-tokio-console: ensure-cache
	$(DOCKER_RUN) \
		-e RYLL_GIT_SHA="$(RYLL_GIT_SHA)" \
		-e RUSTFLAGS="--cfg tokio_unstable" \
		$(RYLL_IMAGE) \
		cargo build -p ryll --features tokio-console

# Build release version
release: ensure-cache
	$(DOCKER_RUN) \
		-e RYLL_GIT_SHA="$(RYLL_GIT_SHA)" \
		$(RYLL_IMAGE) \
		cargo build --release -p ryll

# Cheap smoke-tier proxy for the Windows builds that run in the merge
# tier: `cargo check` against the gnu triple, which mingw-w64 lets us
# cross-compile from Linux. The msvc triples CI actually builds
# cannot be cross-checked this way -- aws-lc-sys needs an MSVC
# toolchain to compile its vendored BoringSSL C sources -- but the
# gnu triple shares the cfg(windows)/windows-sys surface with msvc,
# so it catches the common case cheaply. See
# docs/plans/PLAN-two-stage-ci-phase-02-windows-check.md.
check-windows: ensure-cache
	$(DOCKER_RUN) \
		$(RYLL_IMAGE) \
		cargo check --target x86_64-pc-windows-gnu --no-default-features -p ryll

# Cutting a release is a two-phase operation so the version bump
# goes through the normal PR review gate rather than landing
# directly on develop.
#
# Phase 1: `make propose-release X.Y.Z` creates a release-X.Y.Z
# branch off develop, bumps the workspace version, pushes the
# branch, and prints the PR creation URL.
#
# Phase 2: after the PR merges, `make tag-release X.Y.Z` tags
# the resulting commit on develop and pushes the tag, which
# triggers .github/workflows/release.yml.
#
# The second word of MAKECMDGOALS is the version; the no-op
# rules below catch X.Y.Z-shaped goals so make does not
# complain about "no rule to make target 0.1.4".
RELEASE_VERSION := $(word 2,$(MAKECMDGOALS))
propose-release:
	@if [ -z "$(RELEASE_VERSION)" ]; then \
		echo "usage: make propose-release X.Y.Z"; exit 1; \
	fi
	./tools/propose-release.sh $(RELEASE_VERSION)

tag-release:
	@if [ -z "$(RELEASE_VERSION)" ]; then \
		echo "usage: make tag-release X.Y.Z"; exit 1; \
	fi
	./tools/tag-release.sh $(RELEASE_VERSION)

# Absorb a version-shaped second word as a no-op target. This only
# matches X.Y.Z numeric forms, so typos in real targets still fail
# loudly.
ifneq ($(filter propose-release tag-release,$(MAKECMDGOALS)),)
$(RELEASE_VERSION):
	@:
endif

# Run tests
test: ensure-cache
	$(DOCKER_RUN) \
		$(RYLL_IMAGE) \
		cargo test --workspace

# Run linting checks (rustfmt + clippy)
lint: ensure-cache
	$(DOCKER_RUN) \
		$(RYLL_IMAGE) \
		sh -c "cargo fmt --all --check && cargo clippy --workspace --all-targets -- -D warnings"

# Run linting with auto-fix
lint-fix: ensure-cache
	$(DOCKER_RUN) \
		$(RYLL_IMAGE) \
		sh -c "cargo fmt --all && cargo clippy --fix --allow-dirty --workspace --all-targets -- -D warnings"

# Package the release binary as a .deb. cargo-deb is baked into the
# devcontainer image; --no-build packages the binary produced by
# `make release`.
deb: release
	$(DOCKER_RUN) \
		$(RYLL_IMAGE) \
		cargo deb --no-build -p ryll

# Package the release binary as an .rpm.
rpm: release
	$(DOCKER_RUN) \
		$(RYLL_IMAGE) \
		cargo generate-rpm -p ryll

# Smoke-test `ryll --web` startup/shutdown. Runs inside the
# devcontainer, which has the runtime libraries the binary links
# against (the self-hosted CI runners do not).
web-smoke: release
	$(DOCKER_RUN) \
		$(RYLL_IMAGE) \
		tools/web-smoke.sh target/release/ryll

web-smoke-tls: release
	$(DOCKER_RUN) \
		$(RYLL_IMAGE) \
		tools/web-smoke.sh --tls target/release/ryll

# Build the fuzzing container: the base devcontainer plus the
# nightly toolchain and cargo-fuzz.
fuzz-devcontainer: devcontainer
	docker build -t $(RYLL_FUZZ_IMAGE) \
		-f $(DEVCONTAINER_DIR)/fuzz/Dockerfile $(DEVCONTAINER_DIR)/fuzz

# The fuzz crate is a detached workspace (see fuzz/Cargo.toml's
# `[workspace]` table), so the top-level `make lint` does not reach
# it. Format-check it here.
fuzz-fmt-check: ensure-cache fuzz-devcontainer
	$(DOCKER_RUN) \
		-w /workspace/shakenfist-spice-protocol/fuzz \
		$(RYLL_FUZZ_IMAGE) \
		cargo +nightly fmt --check

# Build one cargo-fuzz target, e.g. `make fuzz-build-fuzz_link_mess_parse`.
fuzz-build-%: ensure-cache fuzz-devcontainer
	$(DOCKER_RUN) \
		-w /workspace/shakenfist-spice-protocol \
		$(RYLL_FUZZ_IMAGE) \
		cargo +nightly fuzz build $*

# Smoke-run one cargo-fuzz target for a bounded time. This is a
# build-and-doesn't-panic gate, not a real fuzz campaign — long
# coverage-guided fuzzing is tracked in shakenfist/ryll#135.
fuzz-smoke-%: ensure-cache fuzz-devcontainer
	$(DOCKER_RUN) \
		-w /workspace/shakenfist-spice-protocol \
		$(RYLL_FUZZ_IMAGE) \
		cargo +nightly fuzz run $* -- -max_total_time=30 -runs=100000

# Publish all workspace crates to crates.io in dependency order.
# Requires CARGO_REGISTRY_TOKEN in the environment; used by the
# release workflow.
publish-crates: ensure-cache
	$(DOCKER_RUN) \
		-e CARGO_REGISTRY_TOKEN \
		$(RYLL_IMAGE) \
		tools/publish-crates.sh

# Native macOS build. Run this on a Mac — the devcontainer can't
# produce a binary that talks to the host window server, and ryll
# is a GUI app. Mirrors the CI matrix in
# .github/workflows/{ci,release}.yml: same
# MACOSX_DEPLOYMENT_TARGET, same CMAKE_POLICY_VERSION_MINIMUM
# (audiopus_sys source-builds libopus and the bundled CMakeLists
# uses a pre-3.5 cmake_minimum_required which CMake 4.x rejects
# without this override).
macos-prereqs:
	@if [ "$$(uname -s)" != "Darwin" ]; then \
		echo "error: macOS targets must run on macOS (uname -s says $$(uname -s))."; \
		echo "       use 'make build' / 'make release' for the Linux devcontainer."; \
		exit 1; \
	fi
	@missing=""; \
	for tool in cargo cmake pkg-config; do \
		if ! command -v $$tool >/dev/null 2>&1; then \
			missing="$$missing $$tool"; \
		fi; \
	done; \
	if [ -n "$$missing" ]; then \
		echo "error: missing required tool(s):$$missing"; \
		echo ""; \
		echo "  install with:"; \
		echo "    brew install cmake pkg-config"; \
		echo "    brew install rustup-init && rustup-init -y    # if cargo missing"; \
		echo ""; \
		echo "  cmake is needed because audiopus_sys source-builds libopus."; \
		echo "  pkg-config lets the build find system libraries cleanly."; \
		exit 1; \
	fi

macos-build: macos-prereqs
	MACOSX_DEPLOYMENT_TARGET=14.0 \
	CMAKE_POLICY_VERSION_MINIMUM=3.5 \
		cargo build -p ryll
	@echo ""
	@echo "Built debug binary: target/debug/ryll"

macos-release: macos-prereqs
	MACOSX_DEPLOYMENT_TARGET=14.0 \
	CMAKE_POLICY_VERSION_MINIMUM=3.5 \
		cargo build --release -p ryll
	@echo ""
	@echo "Built release binary: target/release/ryll"

# Clean build artifacts
clean:
	rm -rf target/
	rm -rf $(CARGO_CACHE)/

# Clean test data files
clean-testdata:
	rm -f testdata/usb-test.raw

# Clean devcontainer image
clean-devcontainer:
	docker rmi -f $(RYLL_IMAGE) 2>/dev/null || true

# Download the UEFI latency test image
$(QEMU_TEST_IMAGE):
	mkdir -p testdata
	curl -L -o $(QEMU_TEST_IMAGE) $(QEMU_TEST_IMAGE_URL)

# Start a test QEMU instance with SPICE enabled, booting the UEFI latency
# guest. Keystrokes change the screen colour, useful for latency testing.
# Connect with: ryll --direct localhost:$(QEMU_SPICE_PORT)
test-qemu: test-qemu-stop $(QEMU_TEST_IMAGE)
	cp $(OVMF_VARS) $(QEMU_VARS_COPY)
	qemu-system-x86_64 \
		-display none \
		-machine q35 \
		-m 128 \
		-drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
		-drive if=pflash,format=raw,file=$(QEMU_VARS_COPY) \
		-drive file=$(QEMU_TEST_IMAGE),format=qcow2,if=virtio \
		-vga qxl \
		-spice port=$(QEMU_SPICE_PORT),disable-ticketing=on \
		-daemonize \
		-pidfile $(QEMU_PID_FILE)
	@echo "QEMU SPICE server running on port $(QEMU_SPICE_PORT) (PID $$(cat $(QEMU_PID_FILE)))"
	@echo "Connect with: ryll --direct localhost:$(QEMU_SPICE_PORT)"

# Stop the test QEMU instance
test-qemu-stop:
	@if [ -f $(QEMU_PID_FILE) ]; then \
		kill $$(cat $(QEMU_PID_FILE)) 2>/dev/null || true; \
		rm -f $(QEMU_PID_FILE); \
		echo "Stopped test QEMU instance"; \
	fi
	@rm -f $(QEMU_VARS_COPY)

# Long-idle regression test for K1 (main-channel-wedge). Requires a
# SPICE server reachable at $(HOST_PORT) — typically start one with
# `make test-qemu` first. Default idle window is 540s (~9 min, well
# past the historical T+466s wedge threshold). See
# tools/test-k1-idle.sh for the full assertion set.
test-k1-idle:
	./tools/test-k1-idle.sh

# Create a test RAW image for USB disk passthrough
testdata/usb-test.raw:
	mkdir -p testdata
	dd if=/dev/zero of=$@ bs=1M count=64 2>/dev/null
	@echo "Created 64MB test image: $@"

# Start a test QEMU instance with SPICE and USB redirection enabled.
# Connect with: ryll --direct localhost:$(QEMU_SPICE_PORT) --usb-disk testdata/usb-test.raw
test-qemu-usb: test-qemu-stop $(QEMU_TEST_IMAGE) testdata/usb-test.raw
	cp $(OVMF_VARS) $(QEMU_VARS_COPY)
	qemu-system-x86_64 \
		-display none \
		-machine q35 \
		-m 256 \
		-drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
		-drive if=pflash,format=raw,file=$(QEMU_VARS_COPY) \
		-drive file=$(QEMU_TEST_IMAGE),format=qcow2,if=virtio \
		-vga qxl \
		-spice port=$(QEMU_SPICE_PORT),disable-ticketing=on \
		-device qemu-xhci,id=xhci \
		-chardev spicevmc,id=usbredir1,name=usbredir \
		-device usb-redir,chardev=usbredir1,id=redir1 \
		-daemonize \
		-pidfile $(QEMU_PID_FILE)
	@echo "QEMU SPICE+USB server on port $(QEMU_SPICE_PORT) (PID $$(cat $(QEMU_PID_FILE)))"
	@echo "Connect: ryll --direct localhost:$(QEMU_SPICE_PORT) --usb-disk testdata/usb-test.raw"

# Start a test QEMU instance with SPICE and WebDAV folder sharing enabled.
# Connect with: ryll --direct localhost:$(QEMU_SPICE_PORT) --share-dir /tmp/test-share
test-qemu-webdav: test-qemu-stop $(QEMU_TEST_IMAGE)
	cp $(OVMF_VARS) $(QEMU_VARS_COPY)
	qemu-system-x86_64 \
		-display none \
		-machine q35 \
		-m 256 \
		-drive if=pflash,format=raw,readonly=on,file=$(OVMF_CODE) \
		-drive if=pflash,format=raw,file=$(QEMU_VARS_COPY) \
		-drive file=$(QEMU_TEST_IMAGE),format=qcow2,if=virtio \
		-vga qxl \
		-spice port=$(QEMU_SPICE_PORT),disable-ticketing=on \
		-device virtio-serial-pci,id=virtio-serial0 \
		-chardev spiceport,name=org.spice-space.webdav.0,id=webdav0 \
		-device virtserialport,chardev=webdav0,name=org.spice-space.webdav.0 \
		-daemonize \
		-pidfile $(QEMU_PID_FILE)
	@echo "QEMU SPICE+WebDAV server on port $(QEMU_SPICE_PORT) (PID $$(cat $(QEMU_PID_FILE)))"
	@echo "Connect: ryll --direct localhost:$(QEMU_SPICE_PORT) --share-dir /tmp/test-share"
