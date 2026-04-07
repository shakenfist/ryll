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
   stack without GUI overhead.

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
    The GLZ dictionary is shared across display channels via
    `Arc<Mutex<HashMap>>`. Surfaces are keyed by `(display_channel_id,
    surface_id)` to prevent cross-channel collisions.

## Code Organisation

```
src/
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
│                        #   prefix for nav cluster), mouse events,
│                        #   motion coalescing to prevent channel backpressure
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
1. Add to `Args` struct in `src/config.rs`
2. Pass through to relevant code in `src/main.rs` or `src/app.rs`

### Adding a new statistic
1. Add variant to `ChannelEvent` enum in `src/channels/mod.rs`
2. Send from relevant channel handler
3. Handle in `process_events()` in `src/app.rs`

### Modifying protocol handling
1. Message definitions in `src/protocol/messages.rs`
2. Constants/enums in `src/protocol/constants.rs`
3. Channel-specific logic in `src/channels/*.rs`

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
  on pull requests. CI runs native `cargo` (not Docker).
- **macOS native development** -- see `docs/development-macos.md` for
  building and testing locally on macOS without Docker or Homebrew

### Pre-commit

Run `pre-commit install` after cloning. The hooks check:
- Code formatting (rustfmt)
- Linting (clippy with `-D warnings`)
- Shell script quality (shellcheck)

Use `./scripts/check-rust.sh fix` to auto-fix issues.

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
| image | JPEG decoding (with `jpeg` feature only) |
| serde / serde_json | JSON serialisation of channel state snapshots for bug reports |
| zip | Zip file output for bug reports |
| png | PNG encoding for bug report screenshots |
| ctrlc | Cross-platform Ctrl+C handler for graceful shutdown |
