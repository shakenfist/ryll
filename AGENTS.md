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

## Code Organisation

```
src/
├── main.rs              # CLI entry, mode selection (GUI vs headless)
├── app.rs               # egui App, event loop, headless runner
├── config.rs            # .vv file parsing, CLI args
├── protocol/            # SPICE protocol implementation
│   ├── constants.rs     # Enums, message IDs
│   ├── messages.rs      # Binary serialization
│   ├── link.rs          # Handshake, RSA auth
│   └── client.rs        # Connection management
├── channels/            # Per-channel handlers
│   ├── main_channel.rs  # Session init, ping/pong
│   ├── display.rs       # Surface management, image decoding
│   ├── cursor.rs        # Cursor position tracking
│   └── inputs.rs        # Keyboard scancodes, mouse events
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

- **Devcontainer** for consistent builds (`.devcontainer/`)
- **Makefile** for common operations
- Cargo cache persisted in `.cargo-cache/` for faster rebuilds
- **Pre-commit hooks** for code quality (rustfmt, clippy, shellcheck)

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
| pcap-file | Pcap file writing for --capture mode |
| etherparse | Fake TCP/IP header construction for pcap |
| openh264 | H.264 video encoding for --capture mode |
| mp4 | MP4 container writing for --capture mode |
