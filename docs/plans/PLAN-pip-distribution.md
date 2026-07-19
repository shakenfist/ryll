# Pip-installable ryll (`shakenfist-ryll`)

This is **phase 3 of a cross-repository master plan** whose plan of
record lives in the Shaken Fist repository:

> `shakenfist/docs/plans/PLAN-kerbside-vdi-tokens.md`
> (branch `vdi-console-tokens` until merged)

It is self-contained and can land independently of the other phases;
it is also the highest-uncertainty phase, so it is worth doing early.

## Prompt

Make ryll pip-installable so that `pip install shakenfist-ryll`
(typically pulled in via the client's `shakenfist-client[vdi]` extra
in phase 4) puts a working `ryll` executable on the venv's PATH, the
way `pip install kerbside` ships `kerbside-proxy`. Phase 4's
`sf-client instance vdiconsole` then launches ryll with the kerbside
exchange URL and the user lands in a SPICE session with none of the
token plumbing visible. This phase deliberately absorbs all of the
packaging weirdness so phase 4 sees only a binary that either is or is
not on PATH.

## Situation

- **ryll is a GUI binary in a Cargo workspace.** The `ryll` crate
  (`ryll/Cargo.toml`, no `[[bin]]` table — binary name defaults to the
  package name) is the SPICE VDI client. Its default features
  `["capture", "gui", "audio"]` (`ryll/Cargo.toml:21`) pull in
  `eframe` 0.35 (`:56`, transitively winit/wayland/xkb), `arboard`
  (`:128`), `rfd` 0.17 (`:158`), and `cpal`/`opus`
  (`shakenfist-spice-renderer`, `shakenfist-spice-webrtc`). The crate
  documents that the produced binary "picks up libopus0 … libasound2,
  libxcb1, etc. automatically" (`ryll/Cargo.toml:213-217`). A
  `--no-default-features` slim build is headless — not what a user
  needs to see a console.
- **ryll builds in Docker.** A `Makefile` drives cargo inside the
  `ryll-dev` container (`Makefile:38-45`, `build:`/`release:` at
  `:105-130`); toolchain is `stable`
  (`.devcontainer/Dockerfile:48`). Host policy is no native Rust
  toolchain (build in Docker).
- **ryll already has a release pipeline** (`.github/workflows/release.yml`,
  on `v*` tags): it builds per-target artifacts and attaches them to a
  GitHub Release via `softprops/action-gh-release`
  (`release.yml:284-292`) — `.deb`, `.rpm`, a macOS `.tar.gz`, and a
  Windows `.zip`. It also `make publish-crates` to crates.io
  (`release.yml:251-266`). **Crucially, Linux ships only `.deb`/`.rpm`
  today — there is no bare Linux ELF or tarball on the release** (the
  raw binary is carried only inside the macOS tar and Windows zip).
- **The kerbside-proxy precedent** (`kerbside/rust/kerbside-proxy/`):
  a maturin `bindings = "bin"` wheel (`pyproject.toml:43-46`) lays the
  compiled binary into the wheel's `*.data/scripts/` so pip puts it on
  PATH. The wheel is built by `tools/build-proxy-wheel.sh` with
  `maturin build --release --target <triple> --zig --compatibility
  manylinux_2_28` (`:89-94`) — zig supplies a pinned-glibc 2.28
  sysroot. The top `kerbside` package pins `kerbside-proxy==X.Y.Z` at
  release time via `tools/stamp-proxy-version.sh`, and CI publishes the
  proxy wheels to PyPI before the umbrella package
  (`release.yml` `publish-proxy-pypi` → `publish-pypi`). Runtime
  resolution is env-override → `shutil.which` → repo tree
  (`proxy_supervisor.py:28-57`).
- **Why the proxy recipe does not transfer directly.**
  kerbside-proxy is a *headless* daemon; its build container installs
  only `build-essential pkg-config git ca-certificates`
  (`Dockerfile:10-15`) and the binary links essentially only glibc, so
  `--zig` (which provides a glibc sysroot but **not** libxcb /
  libwayland-client / libxkbcommon / libasound / libopus) both links
  and tags cleanly. ryll's GUI binary must link those GUI/audio system
  libraries at build time, which zig's sysroot lacks, and cannot vendor
  them at package time (X11/Wayland/GL/ALSA are not vendorable into a
  portable wheel). PyPI additionally rejects plain `linux_x86_64`
  wheels, so a bundled-binary wheel would have to claim a manylinux tag
  it cannot honestly satisfy.

## Mission and problem statement

Deliver a PyPI package `shakenfist-ryll` such that, in a venv,
`pip install shakenfist-ryll` (or `pip install shakenfist-client[vdi]`)
yields a `ryll` on PATH that phase 4 can launch with `ryll --url <…>`.
Constraints:

1. The PyPI artifact must upload and install cleanly with no manylinux
   contortions.
2. Reuse ryll's existing, tested native release binaries rather than
   standing up a second, GUI-capable manylinux build lane.
3. Contain all packaging complexity here; phase 4 sees only
   "ryll present or not" and degrades to `remote-viewer` otherwise.
4. Be honest about runtime system libraries pip cannot provide.

## Alternatives considered

### A. maturin `bindings = "bin"` wheel (the kerbside-proxy path)

Build a per-arch wheel with the ryll GUI binary in `*.data/scripts/`.
**Rejected as the primary path.** (a) `--zig` cannot link ryll's GUI
system libraries, so the proxy's exact cross-compile does not build
ryll — a manylinux build *container with GUI dev libraries* would be
needed, a whole new build lane. (b) Even then the `manylinux_2_28` tag
would be nominal: libxcb/wayland/xkb/alsa/opus cannot be vendored, so
the wheel would silently require those libs at runtime anyway — the
same runtime-libs caveat the fetch path carries, but now coupled to a
mislabeled tag and a large per-arch binary wheel. Kept only as the
subject of the phase-1 confirmation spike so the rejection is on the
record with evidence.

### B. Fetch the release binary (recommended)

`shakenfist-ryll` is a **pure-Python** package: a trivially valid
`py3-none-any` wheel (PyPI accepts it with zero manylinux concern)
whose `ryll` console-script, on first run, resolves-or-downloads the
matching ryll **native release binary** into a per-user cache and
`os.execv`s it, passing argv through. This reuses the artifacts
`release.yml` already builds and tests, and moves the platform-specific
bytes out of the wheel entirely. Its costs — a raw Linux binary must be
added to the release, and first run needs network — are small and
contained.

### C. Compile from source at pip-install time (maturin sdist)

Rejected: requires a Rust toolchain and every GUI/audio `-dev` library
on the user's machine at install time; catastrophic UX and unusable in
CI.

## Open questions (resolved inline)

1. **Wheel vs fetch.** Recommend **B (fetch-release)** for the reasons
   above; phase 1 is a short, time-boxed spike to confirm A's
   build-lane/nominal-tag obstacle with evidence, then commit to B. If
   the spike surprisingly shows A is cheap and honest, the rest of the
   plan reshapes around a maturin wheel.
2. **Package location.** A new `python/shakenfist-ryll/` directory in
   the ryll repo — the mirror image of kerbside keeping
   `rust/kerbside-proxy/` beside its Python package.
3. **Version coupling.** `shakenfist-ryll` version == the ryll release
   it fetches. A `tools/stamp-ryll-version.sh` (mirroring kerbside's
   `stamp-proxy-version.sh`) writes the `v*` tag into the package
   version and the pinned release tag at release time, so
   `pip install shakenfist-ryll==0.1.5` fetches ryll `v0.1.5`.
4. **What asset to fetch.** A new portable `ryll-<version>-linux-<arch>.tar.gz`
   (raw ELF) added to the release in phase 2, plus a `SHA256SUMS`
   asset. macOS/Windows raw binaries already exist inside the release
   tar/zip; Linux is the primary target for SF clients and is done
   first, with mac/win as a documented extension.
5. **Download timing.** **First run, lazy**, not install-time — an
   install-time build hook would break offline installs, wheel caching,
   and network-free CI. Provide `python -m shakenfist_ryll.download`
   (and a `--prefetch` flag) to pre-warm the cache deliberately.
6. **Integrity.** Verify the downloaded binary against the published
   `SHA256SUMS` before caching/exec. Signature verification
   (minisign/cosign) is a noted optional follow-up, not a blocker.
7. **Runtime system libraries.** pip cannot install libxcb / wayland /
   xkbcommon / libasound2 / libopus0. Document them; on an exec failure
   that looks like a missing shared library, the launcher prints a
   clear message naming the packages and pointing at phase 4's
   `remote-viewer` fallback.
8. **PyPI project.** New `shakenfist-ryll` project; validate the whole
   flow against TestPyPI first (mirror how kerbside staged the proxy).
9. **Refresh and staleness (decided, operator preference,
   2026-07-19).** Exact-version lockstep, and the launcher performs
   **no runtime staleness or version check** — not on every launch, not
   periodically. The cache is keyed by the exact pinned version and a
   released asset is immutable, so once the binary for the pinned
   version is present and SHA256-verified there is nothing to re-check.
   The refresh path is `pip install -U shakenfist-client[vdi]`, which
   bumps the pin and causes a one-off fetch of the new version on next
   launch. This keeps every client environment deterministic (a given
   install always runs a known binary) and keeps the integrity check
   meaningful (we verify against the exact expected hash, not "whatever
   is newest"). Two alternatives were considered and rejected: a
   *floor-pin + periodic refresh* (launcher tracks the newest compatible
   release via a last-checked marker) — rejected for trading
   determinism and the known-hash guarantee for auto-pulling binaries
   from GitHub under the user; and an *upgrade-available hint* (run the
   pinned binary but periodically print a `pip install -U` nudge) —
   rejected as low value given pip's own tooling and the added
   network/offline surface. If security-fix propagation to
   rarely-updated clients later proves painful, revisit the floor-pin
   option as a v2 enhancement behind an explicit opt-in.

## Execution

All work in the `ryll-wt-vdi-tokens` worktree (branch
`vdi-console-tokens`), by sub-agents. Review each step's **actual
files**, not its summary. Steps 3c-3e depend on 3b; 3b depends on the
3a decision (but is near-certain, so 3b can be drafted in parallel and
finalised once 3a lands).

| Step | Effort | Model | Isolation | Brief for sub-agent |
|------|--------|-------|-----------|---------------------|
| 3a   | high   | opus  | worktree  | **Confirmation spike (time-boxed, ~2h, throwaway).** Attempt the maturin `bindings = "bin"` path for the ryll GUI binary: add a `[[bin]] name = "ryll"` to `ryll/Cargo.toml` if maturin needs it, and try to build a wheel in Docker following `kerbside/tools/build-proxy-wheel.sh` (`maturin build --release --zig --compatibility manylinux_2_28`). Demonstrate concretely whether it (i) fails to link the GUI system libs under `--zig`, and/or (ii) only builds inside a manylinux container with GUI `-dev` libs, and/or (iii) produces a wheel whose manylinux tag is nominal. Write a short findings note and append the verdict to this plan's open-question 1 (wheel vs fetch). **Do not ship code**; the worktree is throwaway. If B is confirmed, say so decisively. |
| 3b   | medium | sonnet | none     | In `.github/workflows/release.yml`, add a portable `ryll-<version>-linux-<arch>.tar.gz` (the raw release ELF the `.deb` is built from) and a `SHA256SUMS` covering it to the release assets, without disturbing the existing `.deb`/`.rpm`/macOS/Windows outputs. Reuse the `build-linux` job's already-built binary (`release.yml:73-76`); tar the ELF and add it to the `github-release` `files:` list (`release.yml:284-292`). Cover x86_64 (and aarch64 if that matrix leg builds a Linux binary). Keep CI-script bodies in `tools/` per repo convention if more than a few lines. |
| 3c   | high   | opus  | none      | Create `python/shakenfist-ryll/`: a pure-Python package. `pyproject.toml` (setuptools or hatchling, `py3-none-any`, project name `shakenfist-ryll`, `[project.scripts] ryll = "shakenfist_ryll.launcher:main"`). `shakenfist_ryll/launcher.py`: resolve the binary — cache dir via `platformdirs.user_cache_dir('shakenfist-ryll')/<version>/ryll`; if absent, download `ryll-<version>-linux-<arch>.tar.gz` from the GitHub release for the pinned version, verify against `SHA256SUMS`, unpack, `chmod +x`, cache; then `os.execv` it passing `sys.argv[1:]` through. Do **not** add any
staleness or newer-version check — a cache hit for the pinned version
execs immediately with no network (open question 9). Map `platform.machine()`/`platform.system()` to the release asset name; raise a clear error on unsupported platforms. On an exec/loader failure indicating a missing shared library, print the runtime-system-lib guidance (open question 7). Add `shakenfist_ryll/download.py` with a `python -m shakenfist_ryll.download [--prefetch]` entry. Add `tools/stamp-ryll-version.sh` (mirror `kerbside/tools/stamp-proxy-version.sh`) that stamps the package version and the pinned release tag. Never require network at import time. |
| 3d   | medium | sonnet | none     | Release + tests. Add a job to `release.yml` (or a dedicated lane) that builds the `shakenfist-ryll` sdist + `py3-none-any` wheel and publishes to PyPI, ordered **after** the release binaries + `SHA256SUMS` are attached (mirror kerbside's `publish-proxy-pypi` → `publish-pypi` ordering) and gated on TestPyPI first (open question 8). Unit tests for `launcher.py`/`download.py`: arch/OS→asset mapping, cache-hit skips download, checksum-mismatch aborts (no exec, no cache write), argv passthrough (mock `os.execv`), and the missing-shared-lib guidance path. Mock all network. |
| 3e   | low    | sonnet | none      | Docs. Add a README install section: `pip install shakenfist-ryll` and the `shakenfist-client[vdi]` route (phase 4), the required runtime system packages (open question 7), the first-run download / offline `python -m shakenfist_ryll.download --prefetch` note, and a pointer to phase 4's viewer-selection chain. Keep the existing build-from-source instructions. Register this plan in `docs/plans/index.md` and `docs/plans/order.yml` (done at plan-commit time; this step only touches the README). |

## Success criteria

- In a clean venv, `pip install shakenfist-ryll` installs a
  `py3-none-any` wheel with no manylinux/compilation step; `ryll --help`
  works (downloading the binary on first run) and argv passes through.
- The published PyPI artifact contains no platform binary; the binary
  is fetched from the matching ryll release and SHA256-verified.
- `shakenfist-ryll==<v>` fetches ryll `<v>`; the stamp script keeps the
  two in lockstep at release time.
- A missing runtime system library produces a clear, actionable message
  (not a bare loader traceback), pointing at the docs and the
  remote-viewer fallback.
- CI publishes `shakenfist-ryll` after the release binaries + checksums
  exist, validated against TestPyPI first.
- Phase 4 can rely on `ryll` being on PATH after the `[vdi]` extra, and
  on graceful absence otherwise.

## Agent guidance

- The binary is a GUI app: never assume a headless CI runner can
  *launch* it. Tests exercise resolution/download/exec-wiring with
  mocks, not a real window.
- Do not weaken integrity checks to make a test pass; a checksum
  mismatch must abort.
- Match repo conventions: CI-script bodies over a few lines live in
  `tools/`; Python follows the surrounding style.

## Administration and logistics

- Cross-repo sequencing: phase 4 (client) depends on this phase but
  degrades gracefully via `remote-viewer` if it slips. Nothing here
  depends on the Shaken Fist phases 1-2 already landed.
- The shakenfist master plan's phase-3 row is flipped to *In progress*
  when this plan is committed.
