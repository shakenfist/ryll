# AGENTS.md - Guide for AI Coding Assistants

This file helps AI coding assistants understand the ryll project.

## Project Purpose

Ryll is a Rust SPICE VDI test client designed specifically for **performance testing
the Kerbside SPICE proxy** (shakenfist/kerbside). It is not intended to be a
general-purpose SPICE client - it exists to:

1. Generate controlled SPICE traffic as a client
2. Be instrumented to gather performance metrics
3. Measure latency from input events to display updates
4. Run in headless mode for automated benchmarking

## Related Projects

- **shakenfist/kerbside** - The SPICE protocol native proxy being tested
- **shakenfist/kerbside-patches** - OpenStack integration patches for kerbside
- **shakenfist/kerbside/testclient** - The original Python version of ryll

## Protocol Reference Sources

When working on SPICE protocol implementation details, these
local source trees are available for reference:

| Source | Path | Use for |
|--------|------|---------|
| Kerbside Python proxy | `shakenfist/kerbside/` | Protocol docs in `docs/`, packet parsing in `kerbside/spiceprotocol/packets/`, reference test client in `testclient/ryll/` |
| SPICE protocol headers | `/srv/src-reference/spice/spice-protocol/` | Canonical enum definitions, message structures, capability flags |
| SPICE common library | `/srv/src-reference/spice/spice-common/` | Shared marshalling code used by both server and client |
| SPICE GTK client | `/srv/src-reference/spice/spice-gtk/` | Reference client implementation (C/GTK) |
| spice-html5 | `/srv/src-reference/spice/spice-html5/` | JavaScript SPICE client (useful for LZ/GLZ decompressor reference) |
| virt-viewer | `/srv/src-reference/spice/virt-viewer/` | The standard SPICE client, .vv file handling |
| QEMU | `/srv/src-reference/qemu/qemu/` | Server-side SPICE implementation in `ui/spice-*` |
| Linux kernel | `/srv/src-reference/torvalds/linux/` | QXL driver in `drivers/gpu/drm/qxl/` |

## Architecture Overview

```
┌─────────────┐     ┌──────────────┐     ┌─────────────┐
│   ryll      │────▶│   kerbside   │────▶│ SPICE server│
│ (test client)│     │   (proxy)    │     │  (QEMU)     │
└─────────────┘     └──────────────┘     └─────────────┘
```

Ryll uses:
- **Tokio** for async networking (one task per SPICE channel)
- **egui/eframe** for immediate-mode GUI rendering
- **mpsc channels** for inter-task communication

## Key Design Decisions

1. **Immediate mode rendering** - egui was chosen because SPICE sends bitmap
   tiles to blit onto surfaces. Retained-mode GUIs (like tkinter) accumulate
   objects, causing memory issues. egui just redraws the current surface state
   each frame.

2. **Async over threads** - The Python version used threads with queues. Rust
   uses tokio async tasks with mpsc channels, which is more idiomatic and
   efficient.

3. **Headless mode** - Essential for automated testing. Runs the full protocol
   stack without GUI overhead. Headless is also the first evidence of the
   project's broader **multi-modal client** stance: the SPICE stack is
   frontend-agnostic, and additional frontends (a browser-facing web mode is in
   concept-plan stage in `docs/plans/PLAN-web-frontend.md`) are intended to be
   first-class peers of the GUI rather than retrofits. When you add or modify a
   feature, ask which modes it should be reachable from; if a mode physically
   cannot host the feature, say so in the docs rather than leaving the gap
   unstated.

4. **Cadence mode** - Sends automatic keystrokes every 2 seconds to generate
   predictable input→display latency measurements.

5. **Graceful Ctrl+C shutdown** - A SIGINT handler in `main.rs` sets a global
   `SHUTDOWN_REQUESTED` AtomicBool. The eframe update loop (`app.rs`) and the
   headless tokio select loop both poll this flag and shut down cleanly,
   ensuring capture sessions are finalized.

6. **Unbuffered capture I/O** - Pcap and MP4 writers in `capture.rs` write
   directly to `File` (no `BufWriter`), so packet data is always on disk and
   survives SIGINT without explicit flush.

7. **Display channel capabilities** - Ryll advertises COMPOSITE, MONITORS_CONFIG,
   SIZED_STREAM, and A8_SURFACE capabilities during the display channel
   handshake. Without COMPOSITE, the guest QXL driver falls back to a slow
   software rendering path that sends only raw Pixmap data via `draw_copy`,
   making keyboard input appear to have no effect because the client is
   overwhelmed with uncompressed frames.

8. **GLZ win_head_dist eviction** - The GLZ dictionary evicts cached images
   based on the `win_head_dist` field from each GLZ header, rather than using
   a fixed cache size. This matches the server's reference window and prevents
   both premature eviction (corrupting cross-frame references) and unbounded
   memory growth.

9. **Pcap TCP segmentation** - Large SPICE messages are split into multiple
   TCP segments in the pcap writer to avoid exceeding the IPv4 maximum packet
   length (65535 bytes), which would panic in the header construction code.

10. **USB panel uses identity-based commands** - The GUI sends device identity
    (bus/address for physical, path/read-only for virtual) rather than
    pre-opened device handles via `UsbCommand`. The channel handler does async
    device lookup and open in its tokio context. This avoids async operations
    in the synchronous egui render loop and keeps device lifecycle management
    co-located in the channel handler. Physical USB device support
    (`RealDevice`, `DeviceSource::Physical`, `UsbCommand::ConnectPhysical`)
    is gated with `#[cfg(target_os = "linux")]` — on macOS/Windows only
    virtual disk devices are available. The file picker for adding virtual disks
    also runs on a background thread with results polled via `try_recv()`.

11. **WebDAV shares local directory via embedded HTTP server** - Each mux
    client gets a `tokio::io::DuplexStream`; hyper parses HTTP/1.1 and
    dav-server handles WebDAV operations against the local filesystem.
    Response data flows back to the main loop via `mpsc::Sender<MuxResponse>`,
    the same pattern used by usbredir's interrupt polling tasks. The Folders
    UI panel mirrors the USB panel structure.

12. **QUIC decoder is a bespoke pure-Rust port** - SPICE QUIC is a
    proprietary image codec (not the IETF QUIC network protocol). No
    pre-existing Rust crate provides SPICE QUIC decoding, so the
    decoder was ported from the canonical C source in
    `spice-common/common/quic.c`. Constant tables (TABRAND_CHAOS,
    BESTTRIGTAB, J) have been verified against the C reference.
    Golomb coding parameters are clamped to safe bounds before use
    to prevent out-of-bounds panics on malformed data.

13. **Multi-monitor via agent infrastructure** - Multiple display channels
    are opened (one per `--monitors N`) and the main channel sends
    `VDAgentMonitorsConfig` to the guest via the VDI port agent protocol.
    The GLZ dictionary is shared across display channels via a
    `GlzDictionary` struct (with notify-based cross-frame reference
    resolution). Surfaces are keyed by `(display_channel_id,
    surface_id)` to prevent cross-channel collisions.

14. **Dedicated audio thread with lock-free ring buffer** - The cpal audio
    output stream runs on a dedicated `std::thread`, not in the tokio
    runtime. This avoids the `unsafe impl Send` that was previously needed
    (cpal streams are `!Send` on macOS/Windows). The tokio network task
    pushes decoded PCM samples into an `rtrb` single-producer
    single-consumer ring buffer; the audio thread drains it into a local
    `VecDeque` for the resampler. This eliminates mutex contention in the
    real-time cpal callback.

15. **Paste-as-keystrokes: cooperative state machine in the select! loop** -
    The paste feature translates text to US-QWERTY scancodes and types them
    as synthetic key events. A `PasteState` struct tracks the current
    character index and sub-step (Press/Release). A conditional third arm in
    the inputs channel's `select!` loop uses `tokio::time::sleep_until` to
    fire at the right moment; between firings the other two arms (server reads
    and UI events) run normally. The `advance_paste` method sends one sub-step
    per invocation and updates the next-fire time. Modifier keys (Ctrl, Shift,
    Alt) are tracked via `KeyDown`/`KeyUp` observations and saved/restored
    around the paste. The `send_key_down`/`send_key_up` helpers bypass event
    recording and modifier tracking for synthetic paste events. Public API:
    `translate_paste(text: &str) -> Result<Vec<PasteKey>, PasteError>`,
    `PasteKey` (struct with press, release, shift fields), `PasteError`
    (enum with Unrepresentable variant).

16. **Mouse mode negotiation** - On session init, ryll requests client mouse
    mode (absolute positioning) via `MOUSE_MODE_REQUEST` if the server
    supports it. If the server remains in server mode (e.g. no SPICE agent),
    ryll sends relative `MOUSE_MOTION` messages instead of absolute
    `MOUSE_POSITION`. The mode is checked on every pointer move in app.rs.

17. **Event-driven egui repaints via `repaint_notify`** - egui only repaints
    when something asks it to. Channel handlers run on the tokio runtime
    and have no direct access to `egui::Context`. Every channel handler
    therefore holds an `Arc<tokio::sync::Notify>` (`repaint_notify`)
    alongside its `event_tx: mpsc::Sender<ChannelEvent>`, and a small
    "repaint bridge" tokio task (spawned from `RyllApp::new`) waits on
    `notify.notified().await` and calls `ctx.request_repaint()` whenever
    a notification arrives. **Convention: every `event_tx.send(...)` call
    in a channel handler must be immediately followed by
    `repaint_notify.notify_one()`.** A 1 Hz fallback in `update()` covers
    time-based UI like the bandwidth and latency sparklines. New channel
    handlers must accept `Arc<tokio::sync::Notify>` in their constructor
    and follow this pairing convention or idle CPU will silently regress.

18. **Draw-op coverage: one `decode_*` per opcode, warn-once everything
    skipped** - Every implemented `DRAW_*` opcode on the display channel
    follows the same shape: a pure `fn decode_<op>(payload) ->
    io::Result<<Op>Outcome>` classifier that parses the phase-1 wire
    struct and returns an Outcome enum describing what to do (`Paint`,
    `SkipNonOpPut { rop }`, etc.), then an `async fn handle_<op>` shim
    that destructures the outcome, fires `warn_once!` on each skip
    variant, and emits a typed `ChannelEvent`. Any feature the handler
    deliberately ignores (non-`OP_PUT` ROP descriptors, non-solid
    brushes, non-null `SpiceQMask`, non-zero `alpha_flags`, etc.) must
    fire `warn_once!` with a stable colon-delimited static key so the
    gap enters the process-global warn_once registry. Unknown opcodes
    use `log_unknown_once` which registers the same way but includes a
    first-occurrence hex dump. See STYLEGUIDE.md §"warn_once for
    protocol gaps" for the full convention (key format, test
    discipline, append-only contract).

19. **Colour conversion in the channel, not the surface** - SPICE
    colour fields (brush colours, chroma keys, BGRX image pixels) are
    BGRX on the wire; `DisplaySurface` stores pixels as RGBA. The
    conversion lives exclusively in the channel handler (before event
    emission) so surface helpers trust their inputs are already RGBA.
    Concretely: `FillRect.colour`, `ImageReadyChroma.chroma_rgba`, and
    every `ImageReady*.pixels` buffer reach `app.rs` pre-converted.
    The idiom at the channel site is `[(c>>16)&0xff, (c>>8)&0xff,
    c&0xff, 0xff]` for a wire `u32` colour. Do NOT add BGRX handling
    inside `DisplaySurface` — surfaces are RGBA-only.

20. **`--pedantic` mode: registry observer pattern** - The warn_once
    registry is a process-global `HashSet<&'static str>` with a
    `register_gap_observer(Fn(&'static str))` hook. The observer fires
    once per newly-inserted key (with replay-on-late-registration so
    observers don't miss keys fired before they registered). Two
    layers sit on top today: an always-visible `Gaps: N` status-bar
    widget that polls `warn_once_count()` each frame (no observer
    needed), and `--pedantic` mode which registers an observer that
    spawns a tokio task per new gap to write a bug-report zip via
    `BugReport::write_pedantic`. The observer is registered inside
    `RyllApp::new` / `run_headless` so it captures live
    `TrafficBuffers` and `ChannelSnapshots` rather than stubs — this
    matters because the traffic pcap is what makes a pedantic report
    actionable for debugging.

21. **Notifications go through the unified store, not direct UI
    calls** - The notification store at `ryll/src/notifications.rs`
    is the single producer boundary. Channel handlers, the bug-report
    writer, the screenshot dialog, and the gap observer all push
    `NotificationEntry` values via `Arc<Mutex<NotificationStore>>`; the
    GUI side panel and the status-bar bell read from the same store.
    Adding a new notification producer means: build a
    `NotificationEntry::new(severity, source, message)` (optionally
    `.with_visibility(v)`), then `notifications.lock().push(entry)`.
    New `NotificationSource` variants are added to the enum in
    `notifications.rs`; the side panel's `NotificationSource::label()`
    impl dictates how the new variant renders. Bug-report zips
    automatically include any new entries via `notifications.json`.

## Code Organisation

The repository is a Cargo workspace. Ryll itself lives at
`ryll/`; future extracted reusable crates (see
`docs/plans/PLAN-crate-extraction.md`) will sit alongside it as
additional workspace members. Cargo invocations from the
workspace root should use `-p ryll` to target the ryll package
specifically (e.g. `cargo build -p ryll`,
`cargo deb --no-build -p ryll`), or `--workspace` to act on
every member (e.g. `cargo test --workspace`).

```
ryll/src/
├── main.rs              # CLI entry, mode selection, SIGINT handler
├── app.rs               # egui App, event loop, headless runner,
│                        #   bandwidth sparkline, bug report dialog,
│                        #   live traffic viewer panel, USB device
│                        #   management panel, WebDAV folders panel
├── bugreport.rs         # Traffic ring buffer (TrafficEntry,
│                        #   TrafficRingBuffer, TrafficBuffers),
│                        #   channel state snapshots (DisplaySnapshot,
│                        #   InputsSnapshot, CursorSnapshot,
│                        #   MainSnapshot, AppSnapshot,
│                        #   ChannelSnapshots), bug report assembly
│                        #   (BugReport, BugReportType, ReportMetadata),
│                        #   traffic viewer (TrafficViewEntry)
├── capture.rs           # Pcap + MP4 capture (PcapChannelWriter,
│                        #   VideoWriter, CaptureSession)
├── config.rs            # .vv file parsing, CLI args
├── metrics.rs           # /proc-based runtime metrics sampler
│                        #   (RuntimeMetrics, sample()).  Linux only;
│                        #   non-Linux returns Unavailable variant.
│                        #   Embedded in bug reports as
│                        #   runtime-metrics.json
├── protocol/            # SPICE protocol implementation
│   ├── constants.rs     # Enums, message IDs, capability flags
│   ├── messages.rs      # Binary serialization
│   ├── link.rs          # Handshake, RSA auth, capability
│   │                    #   advertisement
│   └── client.rs        # Connection management
├── channels/            # Per-channel handlers
│   ├── main_channel.rs  # Session init, ping/pong
│   ├── display.rs       # Surface management, image decoding,
│   │                    #   GLZ dictionary eviction
│   ├── cursor.rs        # Cursor position tracking
│   ├── inputs.rs        # Keyboard scancodes (with E0 extended
│   │                    #   prefix for nav cluster), mouse events,
│   │                    #   motion coalescing to prevent channel
│   │                    #   backpressure, paste-as-keystrokes state
│   │                    #   machine
│   ├── playback.rs      # Audio playback (PCM/Opus → rtrb → cpal)
│   ├── usbredir.rs      # USB redirection (SpiceVMC transport)
│   └── webdav.rs        # WebDAV folder sharing (SpiceVMC transport)
├── usbredir/            # usbredir protocol parser
│   ├── constants.rs     # Message types, capabilities, status codes
│   ├── messages.rs      # Wire format structs, read/write
│   └── parser.rs        # Byte-stream parser, unit tests
├── usb/                 # USB device backend abstraction
│   ├── mod.rs           # UsbDeviceBackend trait, TransferResult,
│   │                    #   DeviceSource, UsbDeviceInfo, enumeration
│   ├── real.rs          # Physical device backend (nusb, Linux only)
│   └── virtual_msc.rs   # Virtual USB mass storage (RAW images,
│                        #   BOT protocol, SCSI command set)
├── webdav/              # WebDAV folder sharing
│   ├── mod.rs           # Module declaration
│   ├── mux.rs           # Mux protocol (client multiplexing, unit tests)
│   └── server.rs        # Embedded WebDAV server (dav-server + hyper)
├── decompression/       # Image decompression
│   ├── glz.rs           # GLZ (dictionary-based, cross-frame refs)
│   └── lz.rs            # LZ (simpler, single-frame)
└── display/
    └── surface.rs       # Pixel buffer for egui rendering
```

## Common Tasks

### Adding a new CLI option
1. Add to `Args` struct in `ryll/src/config.rs`
2. Pass through to relevant code in `ryll/src/main.rs` or
   `ryll/src/app.rs`

### Adding a new statistic
1. Add variant to `ChannelEvent` enum in
   `ryll/src/channels/mod.rs`
2. Send from relevant channel handler
3. Handle in `process_events()` in `ryll/src/app.rs`

### Modifying protocol handling
1. Message definitions in `ryll/src/protocol/messages.rs`
2. Constants/enums in `ryll/src/protocol/constants.rs`
3. Channel-specific logic in `ryll/src/channels/*.rs`

### Inspecting a `--capture` pcap

`tools/pcap-inspect.py` is a pure-Python helper (no tshark
or scapy dependency) for sifting through a ryll capture.
Three subcommands:

```
tools/pcap-inspect.py opcodes   <path>                 # histogram of SPICE message types
tools/pcap-inspect.py draw-copy <path>                 # DRAW_COPY breakdown by surface / image type
tools/pcap-inspect.py timeline  <path> [--since-last N]  # server-side messages in order
```

Typical use: when investigating a rendering artefact,
`opcodes` tells you whether the problem window even
contains the draw ops you thought it did (phase-3 found
that a "static" artefact was 100% DRAW_COPY, not missing
draw ops); `draw-copy` narrows further to the image types
involved; `timeline --since-last 5` dumps the last five
seconds of traffic when the user has pressed F8 right
after seeing the artefact.

ryll's pcap files are big-endian libpcap format carrying
synthetic TCP frames around the raw post-link SPICE
stream. The helper handles that without any extra flags.

## Process templates

Four templates at the repo root capture the workflows we
use repeatedly. Read the template before starting one of
these activities so the resulting plan/PR follows the
established structure.

- **`PLAN-TEMPLATE.md`** — used as the starting point for
  new plan files in `docs/plans/`. Defines the prompt
  preamble, situation/mission/execution sections, and the
  sub-agent execution model.
- **`PUSH-TEMPLATE.md`** — pre-push audit for our own
  branches. Two-wave parallel sub-agent review (build /
  style, then code quality / tests / docs / security).
- **`MERGE-TEMPLATE.md`** — review and merge process for
  external contributor PRs. Adds deterministic-scanner
  Wave 0, prompt-injection sub-agent, and a mandatory
  follow-up plan that lands as our own PR immediately
  after the contributor's merge.
- **`REVIEW-STATE-TEMPLATE.md`** — skeleton for the local-
  only `REVIEW-STATE.md` file that lives in each
  external-PR review's worktree. Captures findings,
  branch state, contributor history, plan B, and a
  "How to resume" entry point for picking the work back
  up after a pause. Never committed.

## Testing

- Unit tests exist for decompression algorithms
- Integration testing requires a real SPICE server
- `make test-qemu` starts a local QEMU instance with SPICE on port 5900
  running the UEFI latency guest (keystrokes change screen colour) for testing
- `make test-qemu-stop` cleans it up
- Headless mode can be used in CI for protocol-level testing

## Build System

- **Devcontainer** for consistent local builds (`.devcontainer/`)
- **Makefile** for common local operations
- Cargo cache persisted in `.cargo-cache/` for faster rebuilds
- **Pre-commit hooks** for code quality (rustfmt, clippy, shellcheck)
- **GitHub Actions CI** (`.github/workflows/ci.yml`) builds and tests
  on Linux, macOS (ARM), and Windows on every push to `develop` and
  on pull requests. CI runs native `cargo` (not Docker). PRs also
  receive an automated code review via the shared
  `shakenfist/actions/review-pr-with-claude` action.
- **Bot-triggered workflows** for PR automation:
  `@shakenfist-bot please re-review`, `please address comments`,
  `please retest`
- **Renovate** for automated dependency updates (`renovate.json`)
- **CodeQL** for security scanning (`.github/workflows/codeql-analysis.yml`)
- **macOS native development** -- see `docs/development-macos.md` for
  building and testing locally on macOS without Docker or Homebrew

### Pre-commit

Run `pre-commit install` after cloning. The hooks check:
- Code formatting (rustfmt)
- Linting (clippy with `-D warnings`)
- Shell script quality (shellcheck, applied to `scripts/` and `tools/`)
- Committed credentials (gitleaks)
- Bidi and zero-width Unicode control characters
  (`tools/check-bidi.sh`, guards against Trojan Source —
  CVE-2021-42574)

Use `./scripts/check-rust.sh fix` to auto-fix issues.

All five pre-commit hooks are also enforced in CI (rustfmt
and clippy via `ci.yml`, the remaining three via
`supply-chain.yml`). Skipping pre-commit locally therefore
does not bypass enforcement — it only defers the failure to
CI.

## Security scanners

ryll runs five deterministic scanners on every PR in
addition to the LLM-driven automated reviewer. They are
defined in `.github/workflows/supply-chain.yml`. All jobs
run on self-hosted VM runners with the `s` size label
(2 vCPU / 4 GB RAM; the scanners are I/O-bound).
`cargo-audit`, `shellcheck`, and `bidi-check` run on
`[self-hosted, vm, debian-12, s]`; `gitleaks` runs on
`[self-hosted, vm, debian-13, s]` because gitleaks is
only packaged from Debian 13 (trixie) onward — bookworm
has no gitleaks package; `cargo-deny` runs on
`[self-hosted, vm, debian-12-docker, s]` because the
`cargo-deny-action` wrapper runs cargo-deny inside a
Docker container and needs a runner image with docker
preinstalled. The `vm` label matters — bare-metal runners
have different OSes and no passwordless sudo.

| Scanner | What it checks | Policy location |
|---------|----------------|-----------------|
| `cargo audit` | RustSec advisories against `Cargo.lock` (plus a weekly cron on `develop` to catch drift) | `.cargo/audit.toml` — ignore list mirrors `deny.toml` |
| `cargo deny` | License allowlist, dependency sources, version bans, advisory ignores | `deny.toml` at repo root |
| `gitleaks` | Credential-like patterns in the diff (upstream binary invoked directly; the `gitleaks-action` wrapper requires a paid licence for org repos) | Upstream default ruleset; add a `.gitleaksignore` if a legitimate pattern needs to be suppressed (include a comment explaining why) |
| `shellcheck` | Shell-script lint across `scripts/` and `tools/` (invoked via `tools/run-shellcheck.sh`) | Per-script `# shellcheck` directives as needed |
| `tools/check-bidi.sh` | Bidi and zero-width Unicode codepoints (CVE-2021-42574 Trojan Source) | The script itself; PCRE character class at the top |

Policy maintenance:

- **Adding a new license** to `deny.toml` requires
  confirming the licence is permissive and listing it in
  the `allow` array. Only add `[[licenses.exceptions]]`
  for crates that declare non-SPDX identifiers (see the
  `epaint_default_fonts` / UFL-1.0 entry as the canonical
  example).
- **Ignoring a new advisory** requires adding the RustSec
  ID to *both* `deny.toml` and `.cargo/audit.toml`, with
  an inline comment on each entry linking to a rationale
  section in `docs/plans/PLAN-supply-chain-followups.md`.
  The two ignore lists must stay in sync — CI runs both
  scanners and both must pass. Ignores are debt and should
  not accumulate silently.
- **Suppressing a gitleaks false positive** goes in a
  `.gitleaksignore` file with a comment explaining the
  pattern and why it is safe.

## CI workflow conventions

Every job in a workflow that can be triggered by a pull
request or PR comment MUST declare a `concurrency:` block
that cancels superseded runs. Without it, pushing a fixup
commit (or re-commenting `@shakenfist-bot please retest`)
leaves the old run consuming a self-hosted runner slot
while the new run waits behind it. With `MAX_WORKERS = 6`
on the runner fleet, a handful of stale runs can starve
the queue for every other repo.

Use the job-level form (not workflow-level) so unrelated
jobs in the same workflow do not cancel each other:

```yaml
jobs:
  my-job:
    runs-on: [self-hosted, vm, debian-12, s]
    concurrency:
      group: ${{ github.workflow }}-${{ github.ref }}-my-job
      cancel-in-progress: true
```

For comment-triggered workflows (`pr-retest`,
`pr-re-review`, etc.) use
`group: <workflow-name>-${{ github.event.issue.number }}`
instead so the PR number — not `github.ref`, which points
at the default branch for `issue_comment` events — scopes
the group.

Scheduled, push-to-default, and release workflows should
**not** enable `cancel-in-progress`. Cancelling a release
mid-publish or a renovate run mid-PR-creation leaves
partial state.

## Dependencies to Know

| Crate | Purpose |
|-------|---------|
| eframe | egui application framework |
| tokio | Async runtime |
| tokio-rustls | TLS connections |
| clap | CLI argument parsing |
| rsa | RSA-OAEP for SPICE auth |
| byteorder | Binary protocol parsing |
| lz4_flex | LZ4 decompression (image type 109) |
| flate2 | Zlib decompression (ZLIB_GLZ_RGB, type 107) |
| tracing-appender | File logging to /tmp/ryll.log |
| pcap-file | Pcap file writing for --capture mode (optional, `capture` feature) |
| etherparse | Fake TCP/IP header construction for pcap (optional, `capture` feature) |
| openh264 | H.264 video encoding for --capture mode (optional, `capture` feature) |
| mp4 | MP4 container writing for --capture mode (optional, `capture` feature) |
| cpal | Cross-platform audio output (ALSA on Linux, CoreAudio on macOS, WASAPI on Windows). Runs on a dedicated audio thread. |
| opus-decoder | Pure-Rust Opus audio decoder (RFC 8251 conformant) |
| rtrb | Lock-free single-producer single-consumer ring buffer for audio sample transfer between the tokio network task and the cpal audio thread |
| image | JPEG decoding (with `jpeg` feature only) |
| serde / serde_json | JSON serialisation of channel state snapshots for bug reports |
| zip | Zip file output for bug reports |
| png | PNG encoding for bug report screenshots |
| ctrlc | Cross-platform Ctrl+C handler for graceful shutdown |
| libc | POSIX bindings; `sysconf(_SC_CLK_TCK)` for the runtime metrics module that reads `/proc/self/*` for bug reports |
| rfd | Native file dialogs for the screenshot save flow and bug-report save |
