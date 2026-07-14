# Ryll - A Rust SPICE VDI Test Client

Ryll is a Rust implementation of a SPICE (Simple Protocol for Independent Computing Environments) client, designed for testing the Kerbside SPICE proxy.

Ryll is intended to be a **multi-modal SPICE client**: every delivery mode is a first-class citizen and shares as much functionality as the mode itself can physically support. The supported modes today are a **GUI** (egui / eframe desktop window) for interactive day-to-day use, a **headless** mode for automated testing, CI, and cadence latency probing, and a **web** mode (browser frontend over WebRTC with native TLS, all 8 phases shipped) that lets any modern browser connect to a SPICE session without installing software; see `docs/plans/PLAN-web-frontend.md` and `docs/web-frontend.md`.

## Features

- **Immediate mode rendering** - Uses egui for efficient display rendering without accumulating objects
- **Full draw-op coverage** - Handles `DRAW_FILL`, `DRAW_OPAQUE`, `DRAW_COPY`, `DRAW_BLEND`, `DRAW_BLACKNESS`, `DRAW_WHITENESS`, `DRAW_INVERS`, `DRAW_TRANSPARENT`, `DRAW_ALPHA_BLEND`, and `COPY_BITS`. BIOS, GRUB, and kernel-console rendering now paints correctly (solid backgrounds, clean scroll regions). Deferred ops (`DRAW_ROP3`, `DRAW_STROKE`, `DRAW_TEXT`, `DRAW_COMPOSITE`) warn once per session with a first-occurrence hex dump so gaps are visible without flooding the log
- **Image decompression** - LZ, GLZ, ZLIB_GLZ_RGB, LZ4, JPEG, QUIC, and Pixmap image types; MJPEG via SPICE streaming with hardware-accelerated decoding where available (ImageIO on macOS, WIC on Windows, VA-API on Linux) and automatic fallback to software decoders
- **Audio playback** - SPICE playback channel with raw PCM and Opus codec support; lock-free ring buffer to dedicated audio thread via cpal
- **Multi-monitor support** - Connect multiple display channels with `--monitors N` for multi-head configurations
- **Window auto-fit** - The ryll window tracks the guest's display surface size: every primary `SURFACE_CREATE`
  resizes the window to match (modulo an 8-pixel alignment that mirrors what we send the guest via
  `VDAgentMonitorsConfig`). Maximised or fullscreen windows are left alone, and the surface renders at native
  size inside them. Toggle `Obey guest size hints` in the hamburger menu (or launch with
  `--no-obey-guest-size`) to pin the window — useful for fixed-size capture or for guests that flap between
  resolutions on their own. The toggle is a session-level preference and survives reconnect. Resolution
  changes are also surfaced as Info notifications ("Display resolution: WxH"), debounced over 500 ms so a
  boot-time mode probe storm or a drag-resize through many sizes collapses to a single entry rather than
  spamming the panel.
- **Multi-channel support** - Handles main, display, cursor, inputs, playback, usbredir, and webdav channels
- **USB device redirection** - Forward physical USB devices (Linux only) or present RAW disk images as virtual USB mass storage devices on all platforms. Interactive USB panel in the GUI for device enumeration, connect/disconnect, and adding disk images at runtime. CLI flags (`--usb-disk`, `--usb-disk-ro`) for headless/scripted use
- **WebDAV folder sharing** - Share a local directory with the guest VM via the SPICE WebDAV channel. The guest mounts the share via `spice-webdavd` + `davfs2`. Supports read-write and read-only modes. Interactive "Folders" panel in the GUI for directory selection and share management. CLI flags (`--share-dir`, `--share-dir-ro`) for headless/scripted use
- **TLS support** - Secure connections with inline CA certificates from .vv files
- **Reconnect on disconnect** - When a session ends unexpectedly, the disconnect dialog offers a Reconnect button that drops all per-session state and re-attempts the SPICE handshake against the same target without exiting the application. Preserves the configured virtual disk list, shared folder, paste-as-keystrokes toggle, and notification history; resets statistics, traffic buffers, and per-channel state. See ARCHITECTURE.md "Reconnection" for the full lifecycle
- **Window persistence** - Window size and position are restored across launches via eframe's `persistence` feature, which writes the egui memory snapshot to the platform's per-app config directory (`~/.local/share/ryll/` on Linux, `~/Library/Application Support/ryll/` on macOS, `%APPDATA%\ryll\` on Windows)
- **Cursor rendering** - Server cursor shapes with fallback default arrow
- **Headless mode** - Run without GUI for automated testing and benchmarking
- **Cadence mode** - Automatic keystroke injection every 2 seconds for latency testing
- **Paste-as-keystrokes** - Type arbitrary text into guests without vdagent by translating characters into US-QWERTY scancode sequences. Cooperative timer-driven state machine keeps the inputs channel responsive during long pastes. Triggered via Ctrl+Alt+V shortcut or Menu → Paste in the GUI (when enabled). Automatically disabled when vdagent is connected. Characters are mapped to US-QWERTY scancodes; guests with a different keyboard layout will see different characters. Maximum paste length is 4096 characters. CLI flags: `--enable-paste-as-keystrokes`, `--paste-text TEXT`, `--paste-char-delay-ms N`
- **Display channel capabilities** - Advertises COMPOSITE, MONITORS_CONFIG, SIZED_STREAM, and A8_SURFACE so the guest QXL driver uses efficient rendering paths instead of falling back to slow software blits
- **Statistics tracking** - Sliding-window FPS (from MARK boundaries), throughput, and latency measurements
- **Bandwidth sparkline** - Real-time bandwidth graph in the status bar showing rolling bytes/sec history
- **Screenshot capture** - Press F8 or use Menu → Screenshot to save the current display as a PNG via a native file dialog. With multiple monitors, one PNG per surface is saved with `-1`, `-2` suffixes.
- **Latency sparkline** - Bottom stats panel shows client-observed inter-PING interval from the main channel (lower variance is better; spikes indicate network or server stalls).
- **Streaming indicator** - Small triangle (▶) glyph in the status bar reflects the live SPICE display-stream state: grey (off), green (active), amber (a stream was destroyed in the last 5 s), red (≥3 destroys in 30 s with mean lifetime <3 s — fires a `Warn` notification once per minute). Hover for per-stream codec, dimensions, and decoded-frame counts. See [docs/troubleshooting.md § Streaming indicator](docs/troubleshooting.md#streaming-indicator).
- **Protocol-gap counter** - `Gaps: N` button in the status bar tracks the number of distinct protocol edge cases seen this session (unknown opcodes, deferred ops, recoverable decode failures). Highlights red when N > 0; click to open a floating window listing the keys. Complements `--pedantic` mode.
- **File logging** - Verbose mode writes to `/tmp/ryll.log` for debugging
- **Graceful Ctrl+C shutdown** - Cross-platform signal handling via `ctrlc` crate; the GUI and headless event loops check a flag and shut down cleanly, ensuring capture files are finalized
- **Unbuffered pcap I/O** - Packet writes go directly to disk so pcap data survives abrupt termination
- **Bug reports** - Press F12 or use Menu → Report to capture a self-contained zip with metadata, channel state, pcap traffic, runtime metrics, and a screenshot taken the moment the dialog opened so transient display artefacts survive the form-filling delay; Display reports with a region selection also include a crop of the submit-time surface
- **Live traffic viewer** - Press F11 or use Menu → Traffic for a real-time colour-coded feed of SPICE protocol messages with per-channel filters and pause/resume
- **USB device management** - Use Menu → USB for a side panel to browse available devices, connect/disconnect physical or virtual USB devices, add RAW disk images via native file picker, and monitor connection status with elapsed time; USB errors integrate with bug reporting
- **Folder sharing** - Use Menu → Folders for a side panel to select a local directory to share with the guest, toggle read-only mode, and monitor sharing status with elapsed time
- **In-app notifications panel** - Surfaces protocol gaps, bug-report status, SPICE_MSG_NOTIFY messages (e.g. QEMU's "channel is insecure" warnings), and connection-state transitions (connect / reconnect cycle / channel drop / agent state) on a single bell + side-panel surface. Each entry carries a per-row "File…" button that produces a bug-report zip — when a live ring-buffer snapshot exists for the notification (≤ 60 s old, within the last 5 notifications), the report contains the pcap and channel state from the moment the notification fired; otherwise it falls back to post-event ring contents

## Installation

Pre-built `.deb` packages for Debian/Ubuntu are available from
[GitHub Releases](https://github.com/shakenfist/ryll/releases). See
[docs/installation.md](docs/installation.md) for all platforms.

## CI and Automation

GitHub Actions CI builds and tests ryll on Linux (x86_64 + aarch64),
macOS (Apple Silicon), and Windows (x86_64 + aarch64) on every push to
`develop` and on pull requests. Linux x86_64 jobs run on self-hosted
runners with the build wrapped in the devcontainer (via the same
Makefile targets used locally); macOS, Windows, and aarch64 Linux use
GitHub-hosted runners because we own no matching hardware. PRs also
receive an automated code review via Claude Code. Changes that only
touch code-review artifacts (`REVIEWS.md`, `.vscode/*.weaudit*`,
`.vscode/review-scope.toml`) skip the CI and CodeQL workflows
entirely; the supply-chain content scanners still run on them.

Workflows in `.github/workflows/`:

| Workflow | Purpose |
|----------|---------|
| `ci.yml` | Lint, fuzz smoke, build, test (multi-platform), automated PR review |
| `manual-build.yml` | On-demand binary builds of arbitrary branches |
| `release.yml` | Build and publish release artifacts |
| `codeql-analysis.yml` | CodeQL security scanning |
| `supply-chain.yml` | Dependency advisories, license policy, secret scanning, bidi/unicode checks |
| `renovate.yml` | Automated dependency updates (hourly) |
| `export-repo-config.yml` | Daily repository configuration export |
| `pr-re-review.yml` | Bot-triggered PR re-review (`@shakenfist-bot please re-review`) |
| `pr-address-comments.yml` | Bot-triggered comment addressing (`@shakenfist-bot please address comments`) |
| `pr-retest.yml` | Bot-triggered CI re-run (`@shakenfist-bot please retest`) |

## Building

### Using the devcontainer (recommended)

The project includes a devcontainer for consistent builds:

```bash
# Build debug version
make build

# Build release version
make release

# Run tests
make test

# Run linting (rustfmt + clippy)
make lint

# Run linting with auto-fix
make lint-fix

# Start a test QEMU SPICE server (UEFI latency guest, downloads on first run)
make test-qemu

# Stop the test QEMU instance
make test-qemu-stop
```

### Pre-commit hooks

The project uses pre-commit hooks to enforce code quality:

```bash
# Install pre-commit hooks
pre-commit install

# Run checks manually on all files
pre-commit run --all-files

# Or use the script directly
./scripts/check-rust.sh check   # Check mode
./scripts/check-rust.sh fix     # Auto-fix mode
```

The pre-commit hooks run:
- **rustfmt** - Code formatting
- **clippy** - Linting with warnings as errors
- **shellcheck** - Shell script linting

### Using local Rust installation

If you have Rust installed locally with the required dependencies:

```bash
cargo build --release
```

**Required system dependencies** (Debian/Ubuntu):
```bash
apt-get install -y \
    libxcb-render0-dev libxcb-shape0-dev libxcb-xfixes0-dev libxcb1-dev \
    libx11-dev libxkbcommon-dev libgl1-mesa-dev libegl1-mesa-dev \
    libwayland-dev libssl-dev pkg-config
```

### Cargo features

ryll ships several default-on Cargo features that can be opted
out at build time:

- **`gui`** (default-on) — eframe, egui, arboard, rfd, and the
  whole interactive UI.  Disabling it produces a `--headless`
  / `--web`-only binary that does not link the X11/Wayland/
  winit runtime and the runtime image drops libgl1, libx11-6,
  libxcb1, libxkbcommon0, libwayland-client0.  Running such a
  binary without `--headless` or `--web` exits with a clear
  "this binary was built without the `gui` feature" message.
- **`audio`** (default-on) — cpal, opus-decoder, rtrb, and the
  SPICE playback channel in `shakenfist-spice-renderer`.
  Disabling it drops libasound2 from the runtime image and
  skips the SPICE playback channel at connect time (the rest
  of the session is unaffected).
- **`capture`** (default-on) — pcap-file + etherparse + mp4 for
  `--capture` recording.
- **`digest-decode`** (default-off) — adds the
  shakenfist-visual-digest crate as a git dependency and
  enables a polling task that scans the primary surface for a
  QR-encoded visual digest and emits a `digest_updated`
  control-socket event on each frame counter change.  Built
  only for the kerbside test harness; not in production ryll.

The slim test-harness binary is built with
`cargo build --release --no-default-features -p ryll`.  See
[docs/control-socket-protocol.md](docs/control-socket-protocol.md)
for the `surface_drawn` and `digest_updated` event shapes.

**macOS** (Apple Silicon): No additional system libraries are needed --
just Xcode Command Line Tools and Rust. See
[docs/development-macos.md](docs/development-macos.md) for full setup
instructions.

## Usage

### Connect using a .vv configuration file

```bash
# From URL
ryll --url http://example.com/vm.vv

# From local file
ryll --file /path/to/connection.vv
```

#### console.vv keys ryll honours

ryll parses the standard `host`, `port`, `tls-port`, `password`,
`ca`, and `host-subject` keys. It also reads two ticket-related
keys with ryll-specific behaviour:

- **`delete-this-file=1`** — the standard "remove this file
  after reading" hint. ryll additionally treats this as a
  signal that the SPICE ticket is **single-use**: any
  reconnect attempt would be rejected by the server, so
  auto-reconnect is suppressed and a "single-use ticket"
  modal is shown instead of the normal retry sequence.
- **`ticket-valid-until=<unix-ts>`** — a ryll extension key
  (no equivalent in remote-viewer). When set, ryll surfaces a
  T-30s warning notification, suppresses auto-reconnect once
  the deadline has passed, and shows a "ticket expired" modal
  with the expiry time.

Both keys degrade gracefully — absent values mean the previous
ryll behaviour. Producer-side documentation lives in the
companion doc
[`console-vv-extensions.md`](https://github.com/shakenfist/kerbside-wt-docs/blob/main/docs/spice/console-vv-extensions.md)
in the kerbside-wt-docs repository, which is the canonical
reference for SPICE deployment authors (Kerbside, oVirt,
custom gateways) who want their .vv output to drive ryll's
reconnect UX correctly.

### Direct connection

```bash
# Insecure connection
ryll --direct 192.168.1.100:5900

# With TLS port
ryll --direct 192.168.1.100:5900:5901
```

### Options

```
Options:
  --url <URL>            URL to fetch .vv configuration file from
  --file <PATH>          Path to local .vv configuration file
  --direct <HOST:PORT>   Direct connection string
  --headless             Run in headless mode (no GUI)
  --cadence              Enable cadence mode (automatic keystroke every 2 seconds)
  -v, --verbose          Enable verbose logging
  --monitors <N>         Number of monitors (default 1)
  --no-obey-guest-size   Start with "Obey guest size hints" turned off
  --capture <DIR>        Write pcap + video capture to directory
  --latency-file <PATH>  Path to write latency measurements
  --enable-paste-as-keystrokes  Enable paste-as-keystrokes fallback
  --paste-text <TEXT>    Type TEXT as keystrokes in headless mode
  --paste-char-delay-ms <N>  Inter-character delay in ms (default 16)
  --auto-snapshot-interval <N>  Fire a bug-report snapshot every N seconds
  --auto-snapshot-cap <N>  Rolling cap for auto-snapshots (default 20)
  --image-cache-cap-mib <N>  Max bytes for decoded image cache (default 256)
  --glz-dictionary-cap-mib <N>  Max bytes for GLZ dictionary (default 256)
  -h, --help             Print help
  -V, --version          Print version
```

### Capture mode

Record protocol traffic and display frames for debugging:

```bash
ryll --file connection.vv --capture /tmp/capture
```

This writes:
- `metadata.json` — session context (ryll version, platform, target host)
  for self-describing capture directories in bug reports
- `main.pcap`, `display.pcap`, `cursor.pcap`, `inputs.pcap`, `usbredir.pcap`,
  `webdav.pcap` — per-channel pcap files with fake TCP/IP headers, openable
  in Wireshark
- `display.mp4` — H.264 video of the display surface at real timing

See [STYLEGUIDE.md](STYLEGUIDE.md) for capture conventions.

### Headless mode

For automated testing without a GUI:

```bash
ryll --file connection.vv --headless --cadence
```

This will:
- Connect to the SPICE server
- Process display updates (decompress images)
- Send automatic keystrokes every 2 seconds
- Print statistics periodically

Headless mode supports a Unix-socket control interface via
`--control-socket <path>` for driving the session from external
tools. See [`docs/control-socket-protocol.md`](docs/control-socket-protocol.md)
for the full verb and event reference, and
[`examples/control-socket-demo.py`](examples/control-socket-demo.py)
for a runnable Python example client.

### Web frontend (`--web` mode)

```bash
ryll --web session.vv
```

Open the printed `http://<host>:<port>/?token=...` URL in
Firefox or Chrome. The desktop is visible and audible;
keyboard and mouse work; the cursor is rendered as an overlay.
Audio requires the SPICE server to negotiate Opus (xspice and
QEMU defaults). Click the volume button on the page to enable
audio after it loads (browser autoplay policy).

Optional flags:
- `--web-host 0.0.0.0` — bind address (defaults to loopback)
- `--web-port 8080` — port (defaults to ephemeral)
- `--web-tls-cert /path/cert.pem` + `--web-tls-key /path/key.pem` — serve over HTTPS with native TLS (both required together; see `docs/web-frontend.md` for cert recipes)

All 8 phases of the web-frontend plan are complete. Closing
the browser tab reaps the bridge and encoder within ~1 second
(SPICE session stays live); reopening the same URL establishes
a fresh connection. The browser auto-reconnects on transient
ICE failures with 1 s/2 s/4 s/8 s/16 s backoff. A reference
systemd unit is at `examples/ryll-web.service`. See
`docs/web-frontend.md` for the full operator guide.

### Auto-snapshot mode (`--auto-snapshot-interval`)

When `--auto-snapshot-interval N` is set, ryll fires a complete
bug-report zip every N seconds into a rolling subdirectory
`<bug-report-dir>/auto-snapshots/`. This "flight-data-recorder"
mode captures full session state regardless of whether the
operator notices a symptom — useful for intermittent issues like
audio silences that last only 30 seconds mid-session.

A startup `Info` notification confirms the mode is active. The
status bar shows `Auto-snapshot: {saved}/{cap}` while the mode
is enabled. The default rolling cap is 20 zips; oldest are pruned
when the cap is exceeded.

Each zip is a full bug-report artefact (channel-state.json with
all channels merged, traffic.pcap covering all channels, metadata,
runtime-metrics, notifications) equivalent to a manual F12 report.

```bash
# Fire every 30 s, keep last 20 zips
ryll --file connection.vv --auto-snapshot-interval 30

# Custom cap and output directory
ryll --file connection.vv --auto-snapshot-interval 60 \
     --auto-snapshot-cap 10 --bug-report-dir /tmp/session

# Minimum recommended interval is 10 s (assembly blocks ~2 s
# for runtime-metrics sampling; shorter intervals cause
# overlapping samples, which is harmless but wasteful).
```

The zip filename encodes the UTC timestamp and session uptime:
`ryll-auto-snapshot-2026-05-18T20-37-42Z-T+47.3s.zip`

### `--pedantic` mode

When enabled with `--pedantic`, ryll writes a bug-report
zip to `./ryll-pedantic-reports/` (or the directory
specified with `--pedantic-dir <path>`) the first time
each distinct protocol gap is seen — unsupported opcodes,
unhandled sub-features, recoverable decode errors.
Capped at 50 reports per session. Useful for surfacing
implementation gaps against a specific guest workload.
The always-visible `Gaps: N` status-bar counter works
without `--pedantic` too; the counter only counts, it
doesn't write.

```bash
ryll --file connection.vv --pedantic
ryll --file connection.vv --pedantic --pedantic-dir /tmp/my-gaps
```

### Notifications

A bell icon in the status bar shows unread notifications. Click it to
open the side panel; closing the panel marks everything read. The bell
tints amber or red when there are unread Warn or Error-severity entries
(such as SPICE "channel is insecure" warnings from QEMU).

Entries are tagged with a source label — `Connection` for connect /
disconnect / reconnect cycle / channel error / guest-agent state
transitions, `Gap` for protocol gaps, `BugReport` for save status,
`SPICE/<channel>` for server NOTIFY messages, `Internal` for everything
else.

Every entry carries a per-row **File…** button that produces a bug-report
zip. ryll captures a snapshot of the traffic ring buffer at the moment a
notification fires (Phase 10 of the session-001-feedback master plan). If
a live snapshot exists when the button is clicked (≤ 60 s old, within
the last 5 notifications), the zip's pcap and channel state come from
the moment the notification fired and the metadata records
`notification_snapshot: "AtFire"`. After the snapshot has expired the
button still produces a report, using the current ring contents, with
`notification_snapshot: "PostEventOnly"` — the button's visual state
(solid vs. dimmed + hover tooltip) tells you which path a click would
take.

## Architecture

The repository is a Cargo workspace with **6 crates**. The web
frontend (`--web` mode) shipped across all 8 phases; see
[docs/plans/PLAN-web-frontend.md](docs/plans/PLAN-web-frontend.md)
for the master plan. All phases (parity audit, renderer
extraction, encoder pipeline, WebRTC bridge, HTTP server,
real SPICE wire-up for display/audio/inputs/cursor, reconnect
/ bridge lifecycle, CI packaging, and operator docs with
native TLS) have landed. Quick-start: `docs/web-frontend.md`.

| Crate | Role |
|-------|------|
| `ryll` | The binary: egui GUI, headless runner, CLI, Ctrl+C, trait impls for host-side concerns (capture, notifications, clipboard, USB devices, WebDAV server) |
| `shakenfist-spice-protocol` | Protocol constants, message framing, link handshake and auth for both the client and server/proxy roles, the `BoundedReader` for panic-free parsing of untrusted input, warn-once gap registry. Fuzz targets for the internet-facing parsers live in `shakenfist-spice-protocol/fuzz/` (a detached workspace, run under nightly `cargo fuzz`) |
| `shakenfist-spice-compression` | GLZ/LZ decompression, shared GLZ dictionary (cross-channel), per-platform MJPEG decoders (ImageIO/WIC/VA-API with fallback to libjpeg-turbo and pure-Rust decoder); see `docs/plans/PLAN-stream-caps-and-flap.md` for platform decoder details |
| `shakenfist-spice-usbredir` | usbredir wire-format parser and message types |
| `shakenfist-spice-renderer` | SPICE substrate shared by all frontends: channels, display surface, encoder pipeline, session orchestrator, trait surface for host-side concerns |
| `shakenfist-spice-webrtc` | WebRTC bridge: wraps an `RTCPeerConnection` with a video track, audio track, and control datachannel; consumes `EncodedFrame`s from the renderer's encoder |

See [docs/plans/PLAN-crate-extraction.md](docs/plans/PLAN-crate-extraction.md)
for the earlier extraction work and
[docs/plans/PLAN-web-frontend-phase-01-extract.md](docs/plans/PLAN-web-frontend-phase-01-extract.md)
for the Phase 1 renderer extraction.

After Phase 1 the bulk of what was previously under `ryll/src/`
moved to `shakenfist-spice-renderer/src/`. The `ryll/src/` tree
is now thin:

```
ryll/src/
├── main.rs              # CLI entry, mode selection, Ctrl+C handler
├── app.rs               # egui App, event loop, GUI panels
├── bugreport.rs         # Traffic ring buffer + bug-report ZIP writer
├── capture.rs           # Pcap + MP4 capture (implements CaptureSink)
├── clipboard_arboard.rs # Host clipboard via arboard (ClipboardBackend impl)
├── config.rs            # CLI arg parsing
├── display_gui.rs       # GuiSurface egui texture wrapper
├── input_egui.rs        # egui::Key → LogicalKey adapter
├── notifications.rs     # In-app notification store
└── settings.rs          # Verbose-flag gate
```

The SPICE substrate lives in `shakenfist-spice-renderer/src/`:

```
shakenfist-spice-renderer/src/
├── channels/            # Per-channel handlers (main, display, cursor,
│                        #   inputs, playback, usbredir, webdav)
├── display/             # DisplaySurface pixel buffer + draw-op API
├── encoder/             # H.264 encoder pipeline (H264Encoder,
│                        #   EncoderTask, FrameSource, SyntheticFrameSource)
├── usb/                 # USB device backends (RealDevice, VirtualMsc)
├── webdav/              # WebDAV mux protocol + embedded server
├── session.rs           # run_connection / run_headless orchestrators
└── ... (traits, config, notification data types, snapshots)
```

## Dependencies

- **eframe/egui** - Immediate mode GUI
- **tokio** - Async runtime
- **tokio-rustls** - TLS support
- **clap** - CLI parsing
- **rsa/sha1** - Authentication encryption
- **image** - JPEG decoding (via the `image` crate with jpeg feature)
- **cpal** - Cross-platform audio output
- **rtrb** - Lock-free ring buffer for audio sample passing
- **opus-decoder** - Pure-Rust Opus audio decoding
- **openh264** - H.264 encoding in `shakenfist-spice-renderer`
  (the encoder pipeline). Capture mode in ryll consumes it
  transitively via the renderer.
- **nusb** - USB device access (pure Rust, no libusb)
- **dav-server** - WebDAV server (RFC 4918, LocalFs backend)
- **hyper** - HTTP/1.1 framing for WebDAV byte-stream transport
- **webrtc = "0.17.1"** - DTLS/SRTP/ICE/SCTP/STUN stack for
  `shakenfist-spice-webrtc` (browser-bridge crate; also pulls
  in `rtp = "0.17.1"` for H.264 RTP packetisation)
- **opus = "0.3"** - libopus bindings for the synthetic Opus
  pump in the webrtc crate; `audiopus_sys` builds libopus from
  source in the devcontainer
- **ctrlc** - Cross-platform Ctrl+C handling for graceful shutdown

## Comparison with Python version

| Feature | Python (ryll) | Rust (ryll) |
|---------|--------------|-------------|
| Threading | Python threads + queues | Tokio async + channels |
| GUI | Tkinter (retained mode) | egui (immediate mode) |
| Image handling | Pillow | Direct pixel buffers |
| Memory efficiency | Object accumulation issues | No accumulation |
| Deployment | Requires Python env | Single binary |

## Documentation

Additional documentation is available:

- [ARCHITECTURE.md](ARCHITECTURE.md) - Technical design and data flow
- [AGENTS.md](AGENTS.md) - Guide for AI coding assistants
- [STYLEGUIDE.md](STYLEGUIDE.md) - Code conventions and patterns

In the `docs/` directory:

- [Documentation Index](docs/index.md) - What ryll is and why it exists
- [Installation](docs/installation.md) - Pre-built packages and install instructions
- [Configuration](docs/configuration.md) - CLI options and .vv file format
- [Web frontend guide](docs/web-frontend.md) - Operator guide for `--web` mode
- [macOS Development](docs/development-macos.md) - Build and test locally on macOS
- [Troubleshooting](docs/troubleshooting.md) - Common issues and debugging
- [Binary Portability](docs/portability.md) - How to share binaries between machines
- [Releasing](docs/releasing.md) - How to publish a new release

## License

Apache-2.0
