# Architecture

This document describes the technical architecture of ryll.

## Overview

Ryll is a SPICE (Simple Protocol for Independent Computing Environments) client
implemented in Rust. It connects to SPICE servers (typically QEMU virtual machines)
and displays their framebuffer, while sending keyboard and mouse input.

```
┌─────────────────────────────────────────────────────────────┐
│                         ryll                                 │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐  ┌─────────┐        │
│  │  Main   │  │ Display │  │  Cursor │  │  Inputs │        │
│  │ Channel │  │ Channel │  │ Channel │  │ Channel │        │
│  └────┬────┘  └────┬────┘  └────┬────┘  └────┬────┘        │
│       │            │            │            │              │
│       └────────────┴─────┬──────┴────────────┘              │
│                          │                                   │
│                    ┌─────▼─────┐                            │
│                    │  SpiceClient                           │
│                    │  (TLS/TCP)│                            │
│                    └─────┬─────┘                            │
└──────────────────────────┼──────────────────────────────────┘
                           │
                           ▼
                    ┌─────────────┐
                    │ SPICE Server│
                    │   (QEMU)    │
                    └─────────────┘
```

## Concurrency Model

Ryll uses **tokio async tasks** for concurrency, with **mpsc channels** for
communication between tasks.

### Tasks

| Task | Responsibility |
|------|----------------|
| Main channel | Session negotiation, ping/pong, channel discovery |
| Display channel | Receive images, decompress, queue for rendering |
| Cursor channel | Track cursor position and visibility |
| Inputs channel | Send keyboard/mouse events to server |
| Usbredir channel | USB redirection via SpiceVMC data transport |
| UI thread | egui rendering, input capture (GUI mode only) |

### Channel Communication

```
                    ┌──────────────┐
                    │   UI Thread  │
                    │    (egui)    │
                    └──────┬───────┘
                           │
              ┌────────────┼────────────┐
              │            │            │
              ▼            │            ▼
        ┌─────────┐        │      ┌──────────┐
        │event_rx │◀───────┼──────│ input_tx │
        │(receive │        │      │  (send   │
        │ events) │        │      │  input)  │
        └─────────┘        │      └──────────┘
              ▲            │            │
              │            │            ▼
    ┌─────────┴────┐       │     ┌─────────────┐
    │ Display/Main │       │     │   Inputs    │
    │   Channels   │───────┘     │   Channel   │
    └──────────────┘             └─────────────┘
         event_tx                   input_rx
```

- **event_tx/event_rx**: Channel handlers send events (images, cursor pos, stats)
  to the UI thread
- **input_tx/input_rx**: UI thread sends input events (keys, mouse) to the
  inputs channel handler

## SPICE Protocol

### Connection Sequence

```
1. TCP connect to server
2. TLS handshake (if secure port)
3. Link handshake (exchange capabilities)
4. Authentication (RSA-OAEP encrypted password)
5. Main channel: receive session ID and channel list
6. Connect secondary channels (display, cursor, inputs)
7. Event loop: process messages, render display, send input
```

### Message Format

All SPICE messages use a 6-byte mini-header:

```
┌──────────────┬──────────────────────┐
│ message_type │    message_size      │
│   (u16 LE)   │      (u32 LE)        │
├──────────────┴──────────────────────┤
│            payload                   │
│         (variable length)            │
└─────────────────────────────────────┘
```

### Channel Types

| Channel | Purpose | Message Examples |
|---------|---------|------------------|
| Main (1) | Session control | init, channels_list, ping/pong |
| Display (2) | Graphics | surface_create, draw_copy, mark |
| Inputs (3) | User input | key_down, key_up, mouse_position |
| Cursor (4) | Pointer | cursor_set, cursor_move, cursor_hide |
| Usbredir (9) | USB redirection | vmc_data, vmc_compressed_data (SpiceVMC transport) |

## Image Types and Compression

SPICE uses several image types for display updates. The type is
specified in the `ImageDescriptor` that precedes each image's data.
Values from `spice-protocol/spice/enums.h`:

| Type | Name             | Status in ryll |
|-----:|------------------|----------------|
|    0 | Pixmap           | Supported (BitmapData header + raw BGRX/RGBA) |
|    1 | Quic             | Not implemented |
|  100 | LZ_PLT           | Not implemented |
|  101 | LZ_RGB           | Supported |
|  102 | GLZ_RGB          | Supported (with cross-frame dictionary) |
|  103 | FromCache        | Supported (image cache lookup) |
|  104 | Surface          | Not implemented |
|  105 | Jpeg             | Supported (via the `image` crate) |
|  106 | FromCacheLossless| Not implemented |
|  107 | ZlibGlzRgb      | Supported (zlib-wrapped GLZ) |
|  108 | JpegAlpha        | Not implemented |
|  109 | LZ4              | Supported (per-row compressed) |

### Wire format differences

- **LZ_RGB and GLZ_RGB**: preceded by a 4-byte `data_size` (u32 LE),
  then the LZ/GLZ stream with its own big-endian header.
- **ZLIB_GLZ_RGB**: preceded by `glz_data_size` (u32 LE) +
  `compressed_size` (u32 LE), then zlib-compressed GLZ data.
- **LZ4**: NO `data_size` prefix. Data starts immediately with a
  1-byte `top_down` flag, 1-byte `spice_format`, then per-row
  LZ4 blocks each with a 4-byte big-endian size prefix.
- **Pixmap**: preceded by an 18-byte `BitmapData` header (format u8,
  flags u8, x u32, y u32, stride u32, palette_addr u32), then raw pixel
  rows. Only 32-bit formats (BGRX=8, RGBA=9) are supported. The
  `top_down` flag (bit 2 of flags) controls row ordering.
- **JPEG**: preceded by a 4-byte `data_size` (u32 LE), then a standard
  JPEG stream. Decoded via the `image` crate and converted to RGBA.
- **FromCache**: no pixel data, uses `image_id` from the descriptor
  to look up a previously cached decompressed image.

### Compression algorithms

**GLZ** -- Dictionary-based compression that can reference pixels from
previous images (cross-frame). The GLZ decompressor maintains a cache
of decompressed images keyed by `image_id`. Cross-frame references
use `image_dist` to compute the source image ID. Each GLZ header
includes a `win_head_dist` field that defines the reference window
size; after decompressing an image, the display channel evicts all
cached images whose id falls below `image_id - win_head_dist`. This
replaced an earlier fixed-size cache that could evict images still
needed by subsequent frames.

**LZ** — Simpler variant that only references pixels within the
current image. No cross-frame dependencies.

**ZLIB_GLZ_RGB** — GLZ data compressed with zlib for additional
bandwidth savings. Common for incremental updates from QEMU/KVM
through kerbside.

**LZ4** — Fast per-row compression. Each row is individually
LZ4-compressed with a big-endian size prefix. The `spice_format`
byte indicates the pixel format (4=BGRX, 6=BGRA, 3=BGR).

All decompressors output RGBA pixels (BGRX/BGRA/BGR on the wire
is converted to RGBA with alpha=255 for opaque formats).

## Display Channel Capabilities

During the link handshake, ryll advertises per-channel capability flags
to the server. The display channel capabilities are particularly
important:

| Flag | Bit | Effect |
|------|----:|--------|
| SIZED_STREAM | 0 | Streaming video support |
| MONITORS_CONFIG | 1 | Multi-monitor configuration |
| COMPOSITE | 2 | Compositing operations (DRAW_COMPOSITE opcode 318) |
| A8_SURFACE | 3 | Alpha-only surface support |

Without **COMPOSITE**, the guest QXL driver falls back to a slow
software rendering path that produces only `draw_copy` messages with
Pixmap images. With it, the driver uses hardware-accelerated
compositing and sends compressed image types (GLZ, LZ, JPEG). This
was the root cause of an earlier issue where keyboard input appeared
to have no effect -- the server was rendering via the slow path and
flooding the client with uncompressed data.

The correct display server opcodes are:
- `SURFACE_CREATE` = 314 (not 1, as some references suggest)
- `MONITORS_CONFIG` = 317
- `DRAW_COMPOSITE` = 318

## Display Rendering

### GUI Mode (egui)

Ryll uses **immediate mode rendering** via egui:

```rust
// Each frame:
fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
    // 1. Process incoming events (new images, cursor updates)
    self.process_events();

    // 2. For each surface, get texture and draw
    for surface in &mut self.surfaces {
        let texture = surface.texture(ctx);  // Upload pixels to GPU
        ui.image(texture, size);              // Draw texture
    }
}
```

No objects accumulate - the surface pixel buffer is updated in place, and the
texture is re-uploaded each frame when dirty.

### Headless Mode

In headless mode, the egui/eframe code is bypassed entirely:

```rust
// Just run tokio runtime
tokio::runtime::Runtime::new()?.block_on(async {
    run_connection(config, event_tx, input_rx).await
});

// Process events without rendering
loop {
    match event_rx.recv().await {
        ChannelEvent::ImageReady { .. } => stats.frames += 1,
        // ... track stats, no rendering
    }
}
```

## Configuration

### .vv File Format

Standard virt-viewer INI format:

```ini
[virt-viewer]
host=192.168.1.100
port=5900
tls-port=5901
password=secret
ca=/path/to/ca.pem
```

### Connection Methods

1. **URL**: Fetch .vv file via HTTP
2. **File**: Load local .vv file
3. **Direct**: Specify host:port directly

### Local Test Server

`make test-qemu` launches a headless QEMU instance (q35 machine, 128MB RAM,
QXL VGA, UEFI firmware) with SPICE on port 5900 and no authentication. It boots
the UEFI latency guest image, which changes screen colour on each keystroke -
ideal for input-to-display latency testing. The image is downloaded on first
run to `testdata/`. `make test-qemu-stop` shuts it down via PID file.

## USB Redirection

USB device redirection uses the SPICE usbredir channel (type 9) to
forward USB devices to the remote VM. The implementation spans
several protocol layers:

```
SPICE SpiceVMC (DATA/COMPRESSED_DATA messages)
  └── usbredir protocol (hello, device_connect, control/bulk/interrupt packets)
        └── USB Mass Storage Bulk-Only Transport (for virtual disks)
              └── SCSI commands (INQUIRY, READ/WRITE(10), etc.)
                    └── RAW file I/O (seek + read/write at LBA * 512)
```

### Device backends

The `UsbDeviceBackend` trait (`src/usb/mod.rs`) abstracts over device
types. The `DeviceBackend` enum provides non-object-safe dispatch:

- **RealDevice** (`src/usb/real.rs`): Physical USB device via the `nusb`
  crate. Detaches kernel drivers, claims interfaces, forwards control/bulk/
  interrupt transfers.
- **VirtualMsc** (`src/usb/virtual_msc.rs`): Emulated USB mass storage
  device backed by a RAW disk image. Implements BOT protocol (CBW/CSW) and
  8 SCSI commands. Reports as a USB 2.0 High Speed removable disk.

### Channel handler flow

1. Channel connects, sends usbredir hello with capabilities.
2. Server responds with hello.
3. If `--usb-disk` is configured, auto-connects after hello.
4. Device attachment sends `ep_info`, `interface_info`, `device_connect`.
5. Server sends lifecycle messages (`set_configuration`, `reset`, etc.)
   and data transfers (`control_packet`, `bulk_packet`).
6. Interrupt endpoints use background tokio polling tasks.
7. Disconnection aborts polling tasks and sends `device_disconnect`.

### CLI usage

```bash
ryll --file conn.vv --usb-disk /path/to/image.raw       # read-write
ryll --file conn.vv --usb-disk-ro /path/to/image.raw     # read-only
```

See `docs/configuration.md` for details. Use `make test-qemu-usb` to start
a QEMU instance with USB redirection enabled.

## Capture Mode

When `--capture <DIR>` is specified, ryll records:

### Session metadata

`metadata.json` is written at session start with platform details
(OS, architecture), ryll version, and connection target (host, port).
This makes capture directories self-describing when shared for bug
reports or debugging.

### Protocol capture (pcap)

Each SPICE channel writes a separate pcap file (`main.pcap`,
`display.pcap`, `cursor.pcap`, `inputs.pcap`, `usbredir.pcap`) containing
decrypted SPICE mini-header messages wrapped in fake TCP/IP
headers. Wireshark can open these directly.

Implementation: `capture::PcapChannelWriter` per channel, using
`pcap-file` for pcap output and `etherparse` for header
construction. Packets are recorded in `send()` and the read
loop of each channel handler. Writers use unbuffered I/O (no
`BufWriter`) so every packet hits disk immediately.

Large SPICE messages (e.g. uncompressed display updates) can
exceed the IPv4 maximum packet size (65535 bytes). The pcap
writer splits these into multiple TCP segments with sequential
sequence numbers, so Wireshark can reassemble them and the
pcap file never triggers a length-overflow panic.

### Display capture (video)

`display.mp4` contains an H.264 encoded video of the primary
surface (surface 0). Frames are emitted on MARK boundaries
with real timestamps for variable-rate playback.

Implementation: `capture::VideoWriter` lazily initialised on
the first `DisplayMark` event. Uses `openh264` for RGBA →
YUV420 → H.264 encoding, and the `mp4` crate for MP4 muxing.

The capture session is `Arc<CaptureSession>` shared across all
channels and the app. When `--capture` is not specified, the
field is `None` and all capture code paths are skipped. The
`CaptureSession` uses an `AtomicBool` guard to ensure `close()`
is idempotent -- it may be called both explicitly during
shutdown and again from the `Drop` implementation.

## Graceful Shutdown

Ryll installs a SIGINT handler (via `libc::signal`) in `main.rs` that sets a
global `AtomicBool` flag (`SHUTDOWN_REQUESTED`). This allows Ctrl+C to trigger
a clean shutdown instead of killing the process immediately.

- **GUI mode**: The `eframe::App::update()` loop in `app.rs` checks the flag
  each frame and calls `ctx.send_viewport_cmd(ViewportCommand::Close)` when
  set, which lets eframe run its normal teardown path and finalize the capture
  session.
- **Headless mode**: The tokio `select!` loop polls the flag alongside channel
  events and breaks out cleanly when shutdown is requested.

### Unbuffered capture I/O

The pcap channel writers (`PcapChannelWriter` in `capture.rs`) write directly
to `File` without `BufWriter`. This means every packet is persisted to disk
immediately, so pcap data is never lost if the process is interrupted by
SIGINT or any other signal. The MP4 video writer also uses unbuffered `File`
I/O for the same reason.

## Statistics and Instrumentation

Ryll tracks:

- **FPS**: Sliding-window frames-per-second derived from `DisplayMark`
  boundaries (true frame completions), not individual draw operations.
  The window keeps the most recent 120 timestamps for an accurate
  short-term reading.
- **Bytes in/out**: Network throughput per channel
- **Latency**: Time from key press to display update (cadence mode)
- **Bandwidth sparkline**: A rolling 60-sample history of bytes/sec is
  displayed in the status bar as a small bar chart. Channel read loops
  increment a shared `AtomicU64` byte counter; the `BandwidthTracker`
  in `app.rs` samples it once per second and renders the sparkline.

This instrumentation is the primary purpose of ryll -- measuring kerbside proxy
performance.

## Traffic Ring Buffer

Every SPICE message (sent and received) is recorded in a per-channel
ring buffer regardless of whether `--capture` is active. The ring
buffer retains the most recent traffic up to a 50 MB total cap
(12.5 MB per channel). Each entry stores structured metadata (channel
name, direction, message type ID and human-readable name, wire and
payload sizes, timestamp) alongside a full pcap frame for export.

The `TrafficBuffers` struct in `src/bugreport.rs` holds all four
per-channel `TrafficRingBuffer` instances behind `Mutex<>` and is
shared via `Arc<TrafficBuffers>` between all channel handler tasks
and the UI thread. This supports both bug report export (Phase 3)
and a live traffic viewer (Phase 6).

## Keyboard Scancodes

Ryll maps egui key events to AT keyboard scancodes for the SPICE protocol.
Keys in the navigation cluster (arrow keys, Home, End, Insert, Delete,
PageUp, PageDown) require the E0 extended prefix to distinguish them from
their numpad equivalents. These are encoded in the u32 scancode field as
`(scancode << 8) | 0xE0`, matching spice-gtk's `spice_make_scancode()`.
The mapping table uses the 0x1xx convention internally (bit 8 set = extended).
