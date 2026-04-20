# Architecture

This document describes the technical architecture of ryll.

## Repository Layout

The repository is a Cargo workspace. Ryll itself lives in
`ryll/`, with `ryll/src/` containing all the source modules
described in the rest of this document. Future workspace
members will be the reusable crates extracted from ryll under
the `shakenfist-spice-*` prefix; see
`docs/plans/PLAN-crate-extraction.md` for the extraction plan.

When invoking cargo from the workspace root, use `-p ryll` to
target the ryll package (e.g. `cargo build -p ryll`,
`cargo deb --no-build -p ryll`) or `--workspace` to act on
every member (e.g. `cargo test --workspace`).

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
| Playback channel | Decode audio, push samples to ring buffer |
| Audio thread | Consume ring buffer, resample, write to cpal device |
| UI thread | egui rendering, input capture (GUI mode only) |
| Repaint bridge | Wait on `Arc<Notify>`; call `egui::Context::request_repaint()` when channel handlers signal a state change. GUI mode only. |

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
  to the UI thread. Each `event_tx.send()` is paired with a
  `repaint_notify.notify_one()` on a shared `Arc<tokio::sync::Notify>`,
  which a small bridge task forwards to `egui::Context::request_repaint()`.
  This lets egui sleep when nothing is happening rather than polling
  at 60 Hz; idle CPU drops by an order of magnitude on systems without
  GPU acceleration. A 1 Hz `request_repaint_after` fallback covers
  time-based UI elements (sparkline ticks, status-message expiry).
- **input_tx/input_rx**: UI thread sends input events (keys, mouse) to the
  inputs channel handler. The channel is bounded (256 slots). The consumer
  coalesces consecutive MouseMove events into a single position update (or
  accumulates MouseMotion deltas) to prevent the channel from filling
  during network stalls, which would cause the producer's `try_send` to
  silently drop critical button events. Mouse events are dispatched based
  on the server's mouse mode: client mode (2) sends absolute
  MOUSE_POSITION, server mode (1) sends relative MOUSE_MOTION. At session
  startup, ryll requests client mode via MOUSE_MODE_REQUEST if the server
  supports it

### TCP Keepalive

All channel sockets enable TCP keepalive to match spice-gtk: 30 s idle
before the first probe, then 3 probes at 15 s intervals (75 s total to
detect a dead peer).  This prevents NAT/firewall idle timeouts from
silently breaking channel connections, which is especially important for
secondary channels that can be idle for extended periods (the SPICE
server only pings them every 300 s).

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
| Inputs (3) | User input | key_down, key_up, mouse_position, mouse_motion |
| Cursor (4) | Pointer | cursor_set, cursor_move, cursor_hide |
| Playback (5) | Audio playback | playback_start, playback_data, playback_mode, playback_stop |
| Usbredir (9) | USB redirection | vmc_data, vmc_compressed_data (SpiceVMC transport) |
| WebDAV (11) | Folder sharing | vmc_data, vmc_compressed_data (SpiceVMC transport) |

## Image Types and Compression

SPICE uses several image types for display updates. The type is
specified in the `ImageDescriptor` that precedes each image's data.
Values from `spice-protocol/spice/enums.h`:

| Type | Name             | Status in ryll |
|-----:|------------------|----------------|
|    0 | Pixmap           | Supported (BitmapData header + raw BGRX/RGBA) |
|    1 | Quic             | Supported (Golomb-coded wavelet compression) |
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

MJPEG is handled separately: it is not an `ImageType` but a streaming video
codec delivered via `STREAM_DATA` / `STREAM_DATA_SIZED` messages. The codec
type byte in the stream header selects MJPEG (value 1). Frames are decoded
inline in `display.rs` using the same JPEG path as `ImageType::Jpeg`.

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
cached images whose id falls below `image_id - win_head_dist`. In
multi-monitor configurations, the GLZ dictionary is shared across
all display channels via a `GlzDictionary` struct (in the
`shakenfist-spice-compression` crate) that wraps the image HashMap
with a `tokio::sync::Notify`. When one channel inserts a decoded
image, any other channel waiting on a cross-frame reference to
that image is woken immediately instead of polling. Non-GLZ images
are only cached when the server sets `IMAGE_FLAGS_CACHE_ME` in the
image descriptor; GLZ images are always cached since they form the
cross-frame reference dictionary. Server-initiated invalidation
(`INVALIDATE_LIST`, `INVAL_ALL_PIXMAPS`) clears both the per-channel
image cache and the shared GLZ dictionary.

**LZ** — Simpler variant that only references pixels within the
current image. No cross-frame dependencies.

**ZLIB_GLZ_RGB** — GLZ data compressed with zlib for additional
bandwidth savings. Common for incremental updates from QEMU/KVM
through kerbside.

**LZ4** — Fast per-row compression. Each row is individually
LZ4-compressed with a big-endian size prefix. The `spice_format`
byte indicates the pixel format (4=BGRX, 6=BGRA, 3=BGR).

**QUIC** -- SPICE's proprietary image codec based on the SFALIC
algorithm (Simple Fast Adaptive Lossless Image Compression). Not
to be confused with the IETF QUIC network protocol. Each colour
channel (R, G, B, and optionally A) is coded independently with
adaptive Golomb coding. The decoder is a pure-Rust port of the
canonical C implementation in `spice-common/common/quic.c` — no
pre-existing Rust crate provides SPICE QUIC decoding (the
`spice-client` crate on crates.io only handles JPEG/PNG, and
`spice-client-glib` wraps the C library via FFI). The decoder
clamps Golomb coding parameters to safe bounds to prevent panics
on malformed data. QUIC images are preceded by a 4-byte
`data_size` (u32 LE), then a QUIC header containing the image
dimensions, version (major=0, minor=1), and codec type.

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

## Multi-Monitor Support

Ryll supports multiple monitors via the `--monitors N` CLI option.
Each monitor gets its own display channel, and the main channel
sends a `VDAgentMonitorsConfig` message to the guest via the VDI
port agent infrastructure to inform it of the desired monitor
layout.

Surfaces are isolated by a `(display_channel_id, surface_id)`
tuple so that draw operations from different display channels
target the correct surface even when surface IDs overlap across
channels. This prevents cross-channel surface corruption in
multi-head configurations.

## Audio Playback Pipeline

SPICE audio data arrives on the **Playback channel** (type 5) as
`PLAYBACK_DATA` messages containing a 4-byte multimedia timestamp
followed by encoded audio. The codec is negotiated via `PLAYBACK_MODE`
(raw PCM = 1, Opus = 3).

```
SPICE PLAYBACK_DATA message (tokio network task)
  │
  ├── raw PCM: i16 LE samples pushed directly
  └── Opus: decoded via `opus-decoder` crate → i16 samples
                │
                ▼
        rtrb::RingBuffer<i16>  (lock-free, ~2 s capacity at 48kHz stereo)
                │
                ▼
      dedicated std::thread ("audio")
                │
                ├── drains ring buffer into local VecDeque
                ├── Resampler: linear interpolation from source rate
                │   to device rate (ratio = source_rate / device_rate)
                └── cpal output stream callback → audio device
```

The tokio network task is the **producer**: it decodes incoming audio
and pushes i16 samples into the ring buffer via `rtrb::Producer<i16>`.
Back-pressure is applied by dropping samples when the ring buffer is
full (the server is sending faster than the device can consume).

The audio thread is the **consumer**: it owns the `cpal` output stream
and the `Resampler`. The cpal callback drains the ring buffer into a
local `VecDeque` and calls `Resampler::next_frame()` to produce
resampled output at the device's native sample rate. The resampler
uses linear interpolation and handles underruns silently (outputs
silence).

Volume control (`VolumeControl`) is shared between the UI thread and
the audio thread via `Arc<VolumeControl>`, using atomic operations to
avoid locking in the cpal real-time callback.

The audio thread is spawned on `PLAYBACK_START` and stopped (joined)
on `PLAYBACK_STOP` or channel disconnect.

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

The `UsbDeviceBackend` trait (`ryll/src/usb/mod.rs`) abstracts over device
types. The `DeviceBackend` enum provides non-object-safe dispatch:

- **RealDevice** (`ryll/src/usb/real.rs`): Physical USB device via the `nusb`
  crate. Linux only (`#[cfg(target_os = "linux")]`). Detaches kernel drivers,
  claims interfaces, forwards control/bulk/interrupt transfers. On non-Linux
  platforms, only virtual devices are available.
- **VirtualMsc** (`ryll/src/usb/virtual_msc.rs`): Emulated USB mass storage
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

### GUI Components

The USB panel is a right-side panel toggled by the "USB" status bar button,
rendered alongside the traffic viewer panel (both use `egui::SidePanel::right`
with different IDs).

**State tracking on RyllApp:**

- `usb_tx` — mpsc sender to the UsbredirChannel, created in `RyllApp::new()`
  and threaded through `run_connection()`. Mirrors the `input_tx` pattern.
- `usb_channel_ready` — set when `UsbChannelReady` event arrives, cleared on
  usbredir channel disconnect.
- `usb_connecting` / `usb_disconnecting` — operation in progress flags, cleared
  on success/failure events.
- `usb_device_description` — set by `UsbDeviceConnected`, cleared by
  `UsbDeviceDisconnected` and channel disconnect.
- `usb_connected_at` — timestamp for the elapsed connection timer.
- `usb_available_devices` — enumerated device list, refreshed on panel open
  and via Refresh button.
- `usb_virtual_disks` — session-scoped virtual disk paths from CLI flags and
  runtime additions.

**Command flow:**

The GUI sends identity-based `UsbCommand` variants (`ConnectPhysical { bus,
address }` (Linux only), `ConnectVirtualDisk { path, read_only }`,
`DisconnectDevice`) via
`usb_tx`. The channel handler does async device lookup and open in its tokio
context, sending `UsbDeviceConnected`, `UsbDeviceDisconnected`, or
`UsbConnectFailed` events back to the app. If a device is already connected
when a connect command arrives, the handler disconnects it first.

**File picker:**

The "Add Disk..." button spawns `rfd::FileDialog` on a background thread. The
result is polled via `std::sync::mpsc::try_recv()` each frame. Selected files
are validated (regular file, >= 512 bytes) and added to the session's virtual
disk list.

**Bug report integration:**

USB errors show a "Report this as a bug" button that opens the bug report
dialog pre-populated with `BugReportType::Usb`, which captures the usbredir
channel's pcap traffic. Generic channel errors (displayed in the central
panel) also offer a bug report button pre-populated with
`BugReportType::Connection`.

## WebDAV Folder Sharing

WebDAV folder sharing uses the SPICE WebDAV channel (type 11) to export a
local directory to the guest VM. Like usbredir, it uses the SpiceVMC transport
(`SPICEVMC_DATA` / `SPICEVMC_COMPRESSED_DATA` messages). The guest's
`spice-webdavd` daemon issues HTTP WebDAV requests through the channel;
ryll runs an embedded WebDAV server that fulfils them against the local
filesystem.

### Protocol layers

```
SPICE SpiceVMC (DATA/COMPRESSED_DATA messages)
  └── Mux protocol (client_id + size + HTTP data)
        └── HTTP/1.1 (parsed by hyper)
              └── WebDAV (RFC 4918, handled by dav-server with LocalFs)
                    └── Local filesystem I/O
```

### Mux protocol

The WebDAV channel multiplexes multiple concurrent HTTP clients over a
single byte stream. Each frame is:

```
client_id:  i64 LE  (8 bytes) — identifies the HTTP client
data_size:  u16 LE  (2 bytes) — payload size (0 = disconnect)
data:       [u8]    (data_size bytes) — raw HTTP bytes
```

The `MuxDemuxer` (`ryll/src/webdav/mux.rs`) accumulates bytes and extracts
complete frames, handling frames that span VMC messages or are packed
together.

### Per-client architecture

Each mux client gets a `tokio::io::DuplexStream` pair:

```
Guest HTTP request bytes
       │
       ▼
  DuplexStream (client end, split)
  ├── write half: held in MuxClient, main loop writes request data
  └── read half: reader task reads response data, sends via mpsc
       │
       ▼
  DuplexStream (server end)
       │
       ▼
  TokioIo → hyper http1::serve_connection() → dav-server DavHandler
       │
       ▼
  Local filesystem (via dav-server LocalFs)
```

Response data flows back through an `mpsc::Sender<MuxResponse>` from
the per-client reader task to the main `run()` loop, which muxes the
responses back to the guest. This is the same pattern used by usbredir's
interrupt polling tasks.

### Server lifecycle

The `WebdavServer` (`ryll/src/webdav/server.rs`) wraps `dav-server::DavHandler`
with `LocalFs` and is cheaply cloneable (inner `Arc`). It is created when
a `ShareDirectory` command arrives from the UI or `--share-dir` is
specified on the CLI, and destroyed on `StopSharing`. Read-only mode uses
`DavMethodSet::WEBDAV_RO` to restrict allowed HTTP methods.

### CLI usage

```bash
ryll --file conn.vv --share-dir /path/to/dir          # read-write
ryll --file conn.vv --share-dir /path/to/dir --share-dir-ro  # read-only
```

See `docs/configuration.md` for details. Use `make test-qemu-webdav` to start
a QEMU instance with WebDAV enabled.

### GUI Components

The Folders panel is a right-side panel toggled by the "Folders" status bar
button. It mirrors the USB panel structure: channel status indicator, active
share display with elapsed timer, error display with auto-clear, read-only
checkbox, and native directory picker via `rfd::FileDialog::pick_folder`.

## Capture Mode

When `--capture <DIR>` is specified, ryll records:

### Session metadata

`metadata.json` is written at session start with platform details
(OS, architecture), ryll version, and connection target (host, port).
This makes capture directories self-describing when shared for bug
reports or debugging.

### Protocol capture (pcap)

Each SPICE channel writes a separate pcap file (`main.pcap`,
`display.pcap`, `cursor.pcap`, `inputs.pcap`, `usbredir.pcap`,
`webdav.pcap`) containing
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
- **Latency**: Client-observed inter-PING interval on the main channel,
  in milliseconds. SPICE has no client-originated probe (`SPICE_MSG_PING`
  is server→client only), so ryll cannot measure absolute network RTT.
  Instead, the main-channel PING handler records `Instant::now()` and
  emits the gap to the previous PING as a sample. The number includes
  the server's send cadence and the client's receive turnaround;
  spikes indicate a network or server stall. Sparkline mirrors the
  bandwidth one (60-sample rolling history, amber bars). Implemented
  via `LatencyTracker` in `app.rs`.
- **Bandwidth sparkline**: A rolling 60-sample history of bytes/sec is
  displayed in the status bar as a small bar chart. Channel read loops
  increment a shared `AtomicU64` byte counter; the `BandwidthTracker`
  in `app.rs` samples it once per second and renders the sparkline.
- **Runtime metrics in bug reports**: each bug-report ZIP includes a
  `runtime-metrics.json` file with process and per-thread CPU%, RSS,
  and VmSize sampled over a 2-second window. Linux-only (reads
  `/proc/self/stat`, `/proc/self/status`, and `/proc/self/task/*/`);
  non-Linux platforms emit a graceful "unavailable" payload.
  Implemented in `ryll/src/metrics.rs`.

This instrumentation is the primary purpose of ryll -- measuring kerbside proxy
performance.

## Traffic Ring Buffer

Every SPICE message (sent and received) is recorded in a per-channel
ring buffer regardless of whether `--capture` is active. The ring
buffer retains the most recent traffic up to a 50 MB total cap
(12.5 MB per channel). Each entry stores structured metadata (channel
name, direction, message type ID and human-readable name, wire and
payload sizes, timestamp) alongside a full pcap frame for export.

The `TrafficBuffers` struct in `ryll/src/bugreport.rs` holds all four
per-channel `TrafficRingBuffer` instances behind `Mutex<>` and is
shared via `Arc<TrafficBuffers>` between all channel handler tasks
and the UI thread. This supports both bug report export
and the live traffic viewer.

## Channel State Snapshots

Each channel handler maintains an `Arc<Mutex<T>>` snapshot struct
that captures the channel's mutable state. The snapshots are updated
in-place after every batch of processed messages and after every sent
message. All snapshot structs derive `serde::Serialize` so they can be
written to JSON for bug reports.

| Snapshot struct | Channel | Key fields |
|----------------|---------|------------|
| `DisplaySnapshot` | Display | Image cache size/IDs, recent decode results (last 20), ACK state, bytes in/out |
| `InputsSnapshot` | Inputs | Button state, motion count, recent input events (last 50), bytes in/out |
| `CursorSnapshot` | Cursor | Cursor cache contents, ACK state, bytes in/out |
| `MainSnapshot` | Main | Session ID, bytes in/out |
| `AppSnapshot` | App (UI) | FPS, bandwidth, surfaces, cursor position, uptime |

The `ChannelSnapshots` struct in `ryll/src/bugreport.rs` holds the four
channel snapshot `Arc<Mutex<T>>` values and is created alongside
`TrafficBuffers` in `run_connection()`. The `AppSnapshot` is
maintained separately by the `RyllApp` event loop.

Updates hold the mutex only briefly (copying a handful of scalars
and small collections), so contention with the UI thread is
negligible.

## Bug Report Assembly

`BugReport` in `ryll/src/bugreport.rs` assembles a self-contained zip
file from the ring buffer, channel snapshots, and app state.  The
zip contains:

```
ryll-bugreport-YYYY-MM-DDTHH-MM-SSZ.zip
├── metadata.json         # report type, description, ryll version,
│                         #   platform, target host/port, timestamp
├── session.json          # AppSnapshot (FPS, bandwidth, surfaces)
├── channel-state.json    # snapshot of the affected channel
├── traffic.pcap          # ring buffer pcap (capture feature only)
├── screenshot.png        # display reports only (RGBA → PNG)
└── runtime-metrics.json  # process and per-thread CPU%, RSS, VmSize
                          #   sampled over a 2-second window at
                          #   report-creation time (Linux only;
                          #   non-Linux platforms record
                          #   available:false with a reason)
```

Report types are `Display`, `Input`, `Cursor`, and `Connection`,
each mapping to one SPICE channel.  `BugReport::new()` samples
runtime metrics over a 2-second window (blocking the caller), then
gathers and serialises all data synchronously.  `BugReport::write_zip()`
writes the zip to the capture directory's `bug-reports/`
subdirectory (if `--capture` is active) or the current working
directory.

`RyllApp::generate_bug_report()` is the high-level entry point
that collects surface pixels, constructs the `BugReport`, and
writes the zip.

## Bug Report Dialog

Pressing **F12** or clicking the **Report** button in the status
bar opens a centred modal dialog for generating bug reports.  The
dialog contains:

1. A privacy warning about sensitive data in reports.
2. Radio buttons to select the report type (Display, Input,
   Cursor, Connection).
3. An optional description text field.
4. Capture and Cancel buttons.

While the dialog is open, keyboard and mouse input is not forwarded
to the SPICE server.  F12 is always consumed by ryll (never sent to
the guest).  Escape closes the dialog.

The dialog uses a **two-pass pattern** to avoid egui borrow checker
conflicts: the UI is rendered in a closure that collects the user's
action into a local variable, then the action is executed on `self`
after the closure returns.

After a successful report, a transient status message ("Bug report
saved to ...") is displayed in the status bar for 5 seconds.

### Display region selection

When the user selects "Display" and clicks "Capture", the dialog
closes and the app enters **region selection mode**:

1. A translucent instruction banner appears at the top of the
   surface: "Click and drag to select the affected region.
   Press Escape to skip."
2. The OS cursor changes to a crosshair (the SPICE cursor overlay
   is hidden).
3. The user drags a rectangle; a translucent red overlay shows
   the selection.
4. On mouse release, the report is generated with the region
   coordinates in the metadata.
5. Pressing Escape skips selection and generates without a region.

Keyboard and mouse input is not forwarded to the SPICE server
during selection.  Coordinates are clamped to the surface bounds.

## Live Traffic Viewer

Pressing **F11** or clicking the **Traffic** button in the status
bar toggles a right-side panel showing a live feed of recent SPICE
protocol messages from the ring buffer.

The viewer collects entries from all four channels via
`TrafficBuffers::recent_view_entries()`, which returns lightweight
`TrafficViewEntry` structs (no pcap frame data).  Entries are cached
in `RyllApp` and refreshed every 250ms to minimise mutex contention.

Features:
- **Channel filters**: checkboxes to hide/show individual channels
- **Pause/Resume**: freezes the display for inspection
- **Auto-scroll**: sticks to the bottom when not paused
- **Colour-coded channels**: main=blue, display=green, inputs=orange,
  cursor=purple

Each row shows: relative timestamp, channel name, direction arrow
(sent/received), message name, and wire size.

F11 is consumed by ryll and not forwarded to the SPICE server.

## Keyboard Scancodes

Ryll maps egui key events to AT keyboard scancodes for the SPICE protocol.
Keys in the navigation cluster (arrow keys, Home, End, Insert, Delete,
PageUp, PageDown) require the E0 extended prefix to distinguish them from
their numpad equivalents. These are encoded in the u32 scancode field as
`(scancode << 8) | 0xE0`, matching spice-gtk's `spice_make_scancode()`.
The mapping table uses the 0x1xx convention internally (bit 8 set = extended).
