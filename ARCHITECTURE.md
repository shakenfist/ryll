# Architecture

This document describes the technical architecture of ryll.

## Repository Layout

The repository is a Cargo workspace with **6 crates**:

| Crate | Role |
|-------|------|
| `ryll` | The binary: egui GUI, headless runner, CLI, Ctrl+C, trait impls for host-side concerns (capture, notifications, clipboard, USB devices, WebDAV server) |
| `shakenfist-spice-protocol` | Protocol constants, message framing, handshake, auth, warn-once gap registry |
| `shakenfist-spice-compression` | GLZ/LZ decompression, shared GLZ dictionary (cross-channel) |
| `shakenfist-spice-usbredir` | usbredir wire-format parser and message types |
| `shakenfist-spice-renderer` | SPICE substrate shared by all frontends: channels, display surface, encoder pipeline, session orchestrator, trait surface for host-side concerns |
| `shakenfist-spice-webrtc` | WebRTC bridge: wraps an `RTCPeerConnection` with a video track, audio track, and control datachannel; consumes `EncodedFrame`s from the renderer's encoder |

See `docs/plans/PLAN-crate-extraction.md` for the earlier
extractions and `docs/plans/PLAN-web-frontend-phase-01-extract.md`
for the Phase 1 renderer extraction.

When invoking cargo from the workspace root, use `-p ryll` to
target the ryll package (e.g. `cargo build -p ryll`,
`cargo deb --no-build -p ryll`) or `--workspace` to act on
every member (e.g. `cargo test --workspace`).

## Overview

Ryll is a SPICE (Simple Protocol for Independent Computing Environments) client
implemented in Rust. It connects to SPICE servers (typically QEMU virtual machines)
and displays their framebuffer, while sending keyboard and mouse input.

Ryll is designed as a **multi-modal SPICE client**: the
SPICE protocol stack, channel handlers, decompression, audio
pipeline, and display surface compositing are all frontend-
agnostic, and each delivery mode is a thin layer over that
shared core. The supported modes are:

| Mode | Frontend | Status | Primary use |
|------|----------|--------|-------------|
| GUI | egui / eframe desktop window | Shipping | Interactive day-to-day VDI access from the operator's own machine |
| Headless | none (stdout + metrics) | Shipping | Automated testing, CI, cadence latency probing, scripted USB / WebDAV scenarios |
| Web | Browser via WebRTC | Shipping (all 8 phases complete) | Interactive VDI access from any browser on the LAN; see `docs/plans/PLAN-web-frontend.md` |

A feature is not considered complete when it works in only
one mode. Every feature should be reachable from every mode
that can physically support it; intrinsic mode-specific
features (egui-only UI panels, browser-only clipboard APIs)
should be documented as such so the parity gaps are visible.

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

## Renderer-crate Extraction

The SPICE substrate (channel handlers, display surface, encoder,
session orchestrator) was extracted from the `ryll` binary into
`shakenfist-spice-renderer` during Phase 1 of the web-frontend
plan (see `docs/plans/PLAN-web-frontend-phase-01-extract.md`).
This crate is intentionally **egui-free**: no `eframe` or `egui`
types appear in its source. The goal is to share the substrate
across GUI, headless, and (planned) `--web` modes without each
frontend dragging in the others' dependencies.

### Communication upward: ChannelEvent

Channel handlers communicate state changes upstream via the
`ChannelEvent` enum (variants for surface lifecycle, image
arrivals, latency samples, notifications, etc.). Producers send
on `mpsc::Sender<ChannelEvent>`; the frontend (GUI event loop or
headless runner) drains the receiver and reacts.

### Trait surface for host-side concerns

Some channel concerns are long-lived sinks that need to be
injected at construction time rather than emitted as events:

| Trait | Defined in | Implemented in `ryll/src/` | Purpose |
|-------|-----------|---------------------------|---------|
| `TrafficSink` | `shakenfist-spice-renderer` | `bugreport::TrafficBuffers` | Per-channel raw-byte ring buffer for bug-report traffic capture and the live traffic viewer |
| `CaptureSink` | `shakenfist-spice-renderer` | `capture::CaptureSession` | pcap + MP4 frame recording; also has a no-op stub when the `capture` feature is disabled |
| `NotificationSink` | `shakenfist-spice-renderer` | `notifications::NotificationStoreSink` | Pushes `NotificationEntry` values into the in-app notification store |
| `ClipboardBackend` | `shakenfist-spice-renderer` | `clipboard_arboard` | Host clipboard read/write via `arboard` |
| `UsbBackend` | `shakenfist-spice-renderer` | usbredir channel constructor; `RealDevice` / `VirtualMsc` concrete types live in the renderer's `usb/` directory | USB host-side device attachment |
| `WebdavBackend` | `shakenfist-spice-renderer` | webdav channel constructor; mux + embedded HTTP server live in the renderer's `webdav/` directory | WebDAV directory share lifecycle |

**When to use `ChannelEvent` vs a trait**: prefer a
`ChannelEvent` variant when the concern is event-shaped
(a one-shot notification, a surface lifecycle signal, a latency
sample). Prefer a trait when the concern is a long-lived sink
that the channel writes to continuously (traffic recording,
capture frames). This distinction keeps the event channel
lightweight and the trait surface minimal.

### Value config types

`LogConfig` is passed by value into channel constructors to
carry protocol-logging gates (primarily the verbose flag) without
the channels reaching back into global settings state.

## Encoder Module

`shakenfist-spice-renderer/src/encoder/` is the live H.264
encoder pipeline built in Phase 2
(`docs/plans/PLAN-web-frontend-phase-02-encoder.md`).

### `H264Encoder`

A stateful wrapper around openh264. Takes an RGBA pixel buffer,
converts to YUV 4:2:0, encodes to Annex-B framed NAL units
(each NAL prefixed with the 4-byte start code `00 00 00 01`).
Every IDR frame is accompanied by SPS (NAL type 7) and PPS
(NAL type 8) NALs so keyframes are self-contained. Even-dimension
constraint: width and height are rounded down to even numbers
before encoding. The `force_keyframe: bool` parameter to
`encode()` calls openh264's `force_intra_frame()` for the
next encode.

### `EncoderTask`

Async driver that lives on tokio's blocking pool (openh264 is a
synchronous C library; `spawn_blocking` keeps it off the async
executor). The task loop:

- Ticks at a configurable FPS cap (default 30; period ≈ 33 333 µs).
- On each tick, calls `source.next_frame()`. If `Some`, encodes
  and sends `EncodedFrame` on the output channel. If `None`,
  skips the tick — no idle frames are produced.
- Handles `EncoderControl::RequestKeyframe` by setting a
  `keyframe_pending` flag consumed on the next encode.
- Handles `EncoderControl::Stop` by breaking the loop.

### `FrameSource` and `FrameRef`

`FrameSource` is a trait that decouples the encoder from how
pixels arrive. The implementing type handles dirty tracking and
synchronisation with concurrent display-channel writers. It
returns `Option<FrameRef<'_>>` where `FrameRef` carries width,
height, RGBA bytes, and a `timestamp_us` used to derive RTP
timestamps in Phase 3. `SyntheticFrameSource` is a test/CI
source that generates animated gradient frames.

## WebRTC Bridge (`shakenfist-spice-webrtc`)

`shakenfist-spice-webrtc` is a separate crate (not part of the
renderer) because the webrtc-rs dependency tree (DTLS, SRTP,
ICE, SCTP, STUN) is heavy and not all SPICE-client consumers
need it. The renderer stays a pure SPICE substrate; the bridge
is one specific delivery mechanism.

### `WebrtcBridge`

Wraps an `RTCPeerConnection` and owns:

- A video `TrackLocalStaticRTP` (H.264, 90 kHz clock rate).
- An audio `TrackLocalStaticRTP` (Opus, 48 kHz clock rate).
- A "control" `RTCDataChannel` (ordered + reliable).
- An `mpsc::Sender<EncoderControl>` to request keyframes.

Construction via `WebrtcBridge::new(WebrtcBridgeConfig)`:
builds the PC via webrtc-rs's `APIBuilder` + `MediaEngine`
pattern, registers H.264 and Opus codecs, creates both tracks,
adds them to the PC, creates the control DC, and registers the
connection-state-change handler.

`WebrtcBridgeConfig` carries the ICE server list (empty for
LAN-only use) and the `EncoderControl` sender.

### SDP flow

`accept_offer(sdp: String) -> Result<String>` is the single SDP
entry point for Phase 4's HTTP `/offer` handler. It:
1. Sets the remote description (browser's offer).
2. Creates an answer.
3. Sets the local description.
4. Waits for ICE gathering to complete.
5. Returns the fully-resolved answer SDP.

ICE gathering completion is awaited so the answer already
contains all host candidates — trickle ICE is not needed for
the LAN-only MVP.

### Video pump

`spawn_video_pump(rx: mpsc::Receiver<EncodedFrame>)` drives the
video track:

- Consumes `EncodedFrame`s from the encoder output channel.
- Strips Annex-B start codes from each NAL.
- Payloads raw NALs via `H264Payloader` (from `rtp::codecs::h264`).
- Sets the `marker` bit on the last RTP packet of each access unit
  (per RFC 6184 §5.1 — decoder pacing depends on this).
- Derives RTP timestamps from `EncodedFrame::timestamp_us` at
  90 kHz: `rtp_ts = (timestamp_us × 90_000) / 1_000_000`.
- Sequence numbers increment via `wrapping_add`.

SPS/PPS NALs produce empty payload sets (the `H264Payloader`
caches them and bundles them as STAP-A with the next IDR slice);
the pump skips empty sets cleanly.

### Audio pump (synthetic for Phase 3)

`spawn_synthetic_audio_pump()` emits a 440 Hz sine wave encoded
as Opus at 50 fps (20 ms per frame, 960 samples at 48 kHz).
Real Opus passthrough from the SPICE playback channel lands in
Phase 5. The pump uses the same `TrackLocalStaticRTP` consumer
interface so Phase 5 is a mechanical source swap.

### Control datachannel

Ordered + reliable, labelled "control". Phase 3 implements
ping/pong for smoke testing. Phase 5 carries input events
(scancodes, pointer coordinates) and cursor overlay updates.
`send_control(&[u8])` and `control_rx()` are the public API.

### Keyframe-on-attach

The bridge sends `EncoderControl::RequestKeyframe` when the
`RTCPeerConnection` transitions to the `Connected` state, so the
first frame the browser sees is always a full IDR. A PLI
(Picture Loss Indication) RTCP handler is also registered for
the same purpose when a viewer requests a refresh.

### webrtc-rs convention: `on_track` must spawn a task

The webrtc-rs `on_track` callback's returned future is awaited
serially by the ICE/DTLS machinery. A long-lived `read_rtp`
loop inside the callback blocks subsequent `on_track` firings
(e.g. audio track never fires if video track's callback loops).
Always spawn a separate tokio task for the `read_rtp` loop inside
`on_track`. This is a webrtc-rs idiom that differs from what
plain intuition suggests; document it explicitly when writing
any new receiver-side WebRTC code.

## Phase 5: Real SPICE Wire-Up (`--web` mode)

Phase 5 of the web-frontend plan (`docs/plans/PLAN-web-frontend-phase-05-iac.md`)
delivers the four real-data connections that replace the Phase 4
synthetic sources: display frames, audio, keyboard/mouse input,
and cursor overlay.

### `SurfaceMirror`

`shakenfist-spice-renderer/src/surface_mirror.rs` — subscribes
to the renderer's broadcast `ChannelEvent` stream and maintains
a `HashMap<(u8, u32), DisplaySurface>` keyed by
`(channel_id, surface_id)`. The mirror is the authoritative
surface state for the web encoder path; it is separate from
the `RyllApp` surface map so the `--web` mode can run without
any egui dependency.

### `RealFrameSource`

`shakenfist-spice-renderer/src/encoder/frame_source.rs` — a
`FrameSource` implementation that reads from a `SurfaceMirror`
under `try_lock`. Returns `None` on lock contention (the encoder
skips the tick rather than blocking) and also returns `None` when
the primary surface is not dirty, achieving genuine
encode-on-dirty behaviour within the 30 fps cap.

### `OpusPacketSink` trait

`shakenfist-spice-renderer/src/audio_sink.rs` — a pre-decode
tap on the SPICE playback channel. When a type implementing this
trait is injected into the playback channel constructor, raw Opus
packets from the SPICE server are delivered to `push_opus_packet`
before being decoded to PCM for the cpal path. The `--web` mode
uses this to route Opus packets to the WebRTC audio track without
re-encoding. When the SPICE server negotiates raw PCM (not Opus),
the sink receives no packets; the web audio track is silent (a
warning is logged).

### Web-mode input relay

`ryll/src/web/inputs.rs` — drains the bridge's control
datachannel, parses the JSON input events that the browser shell
posts, and emits `InputEvent` variants (key down/up with AT
scancodes, mouse position/button) into the renderer's existing
inputs channel handler. Viewport-resize messages from the browser
are forwarded to `maybe_send_monitors_resize` so the SPICE guest
can track the browser viewport size at connect time.

### Web-mode cursor relay

`ryll/src/web/cursor.rs` — subscribes to the renderer's
broadcast `ChannelEvent` stream and watches for `CursorImage`
and `CursorPos` events (the same events the egui frontend
consumes). Cursor shapes are encoded as PNG (`base64 = "0.22"`
for the data-URL wrapper) and sent as JSON over the control
datachannel. The browser shell decodes the data-URL, updates
an `<img>` overlay element, and repositions it to follow cursor
motion events — keeping cursor latency on the datachannel path
rather than the video encoder path.

### Web-mode audio adapter

`ryll/src/web/audio.rs` — `WebOpusSink` implements
`OpusPacketSink`. When the `RTCPeerConnection` reaches
`Connected`, the bridge activates the audio pump; `WebOpusSink`
routes each incoming Opus packet to the bridge's audio track
via the WebRTC audio pump. PCM-only SPICE servers do not
trigger any `push_opus_packet` calls, so the audio track emits
silence (and a one-time warning is logged).

### End-to-end data flow (`--web` mode)

```
SPICE server
    │
    ▼
shakenfist-spice-renderer::run_connection
    │
    ├─► DisplayChannel ──► broadcast ChannelEvent ──► SurfaceMirror
    │                                              └─► CursorRelay (cursor.rs)
    │
    ├─► PlaybackChannel ──► OpusPacketSink (audio.rs) ──► WebRTC audio track
    │
    └─► InputsChannel ◄── web inputs relay (inputs.rs) ◄── control DC ◄── browser
                                                                              │
shakenfist-spice-webrtc::WebrtcBridge                                         │
    ├─► video track ◄── EncoderTask ◄── RealFrameSource ◄── SurfaceMirror    │
    ├─► audio track ◄── WebOpusSink ◄── PlaybackChannel (Opus path)          │
    └─► control DC ◄──────────────────────────────────── cursor/input relay ──┘
    │
    ▼
Browser (RTCPeerConnection)
    ├─ <video> H.264 display
    ├─ <audio> Opus audio
    └─ datachannel: cursor overlay + input events
```

The `on_track`-must-spawn-a-task webrtc-rs idiom (documented
in "webrtc-rs convention" above) and the rustls
`CryptoProvider` init (required once at process start, before
any TLS handshake) both apply to the `--web` mode and are
handled in `ryll/src/main.rs` before `run_web()` is called.

## Phase 6: Bridge Lifecycle (`--web` mode)

Phase 6 of the web-frontend plan
(`docs/plans/PLAN-web-frontend-phase-06-lifecycle.md`) adds
proactive bridge reaping and browser-side auto-reconnect.

### Bridge dead signal

`WebrtcBridge` grows two fields:

- `dead: Arc<tokio::sync::Notify>` — notified exactly once
  when the `RTCPeerConnection` reaches `Failed`,
  `Disconnected`, or `Closed`.
- `dead_flag: Arc<AtomicBool>` — sticky flag that lets late
  callers avoid waiting on an already-dead bridge.

Public API:

- `wait_for_dead(&self) -> impl Future` — resolves when the
  PC reaches a terminal state. Returns immediately if the
  flag is already set (late-subscriber safety).
- `dead_handle(&self) -> Arc<Notify>` — exposes the raw
  notify for consumers that hold their own clone without
  keeping a reference to the bridge.
- `dead_flag_handle(&self) -> Arc<AtomicBool>` — exposes the
  sticky flag for the same purpose.

### Server-side reaper (`ryll/src/web/lifecycle.rs`)

`run_bridge_reaper(state: Arc<WebState>)` is a long-lived
task spawned from `run_web`. Its loop:

1. Peeks at the active bridge's `dead_handle()` without
   holding the slot lock for long.
2. If no bridge is active, sleeps 500 ms and retries.
3. Awaits the dead signal.
4. Takes the bridge out of `bridge_slot`, calls
   `bridge.close().await`.
5. Calls `EncoderInfra::stop()` — sends `EncoderControl::Stop`
   and awaits the encoder task handle (2-second ceiling).
6. Clears `opus_active_tx`.

The SPICE session (`run_connection`) is left completely
untouched. A subsequent `/offer` from the browser rebuilds
a fresh bridge and encoder from the same live SPICE state.

Race condition: a new `/offer` and the reaper both race to
take the bridge via `bridge_slot.lock()`. Both serialise on
the mutex; whichever arrives first takes the slot, the other
observes `None` and no-ops.

### `EncoderInfra::stop`

A new helper alongside `restart()`: sends
`EncoderControl::Stop` to the running encoder task and joins
the handle with a 2-second timeout. Used by both the reaper
(bridge died) and the shutdown path (process exiting). Any
orphaned task exits naturally on its next send error.

### Graceful shutdown sequence

After Phase 6, `run_web`'s shutdown sequence is:

1. Ctrl-C → `SHUTDOWN_REQUESTED.store(true)`.
2. The bridge between `SHUTDOWN_REQUESTED` and the SPICE
   `cancel` flag flips the cancel.
3. `axum::serve(…).with_graceful_shutdown(…).await` drains.
4. **New:** take the active bridge from the slot and close
   it (2-second ceiling) so DTLS/SRTP tears down cleanly.
5. **New:** call `EncoderInfra::stop()` to release the
   encoder task.
6. `run_web` returns; the tokio runtime drops.

### Browser-side auto-reconnect

`app.js` is refactored so the `RTCPeerConnection` setup and
SDP offer flow live inside a callable `connect()` function
rather than a one-shot IIFE. On ICE-failed or
connection-state-failed events, `scheduleReconnect()` is
called with backoff delays of 1 s, 2 s, 4 s, 8 s, 16 s
(max 5 attempts). A hidden "Click to reconnect" button is
revealed when all attempts are exhausted. Each attempt
constructs a brand-new `RTCPeerConnection`; the backoff
counter resets on a successful `Connected` transition.

## Phase 7: CI + Packaging (`--web` mode)

Phase 7 of the web-frontend plan
(`docs/plans/PLAN-web-frontend-phase-07-ci.md`) gates the
`--web` stack through CI and crates.io publishing.

Key changes:

- `shakenfist-spice-renderer` and `shakenfist-spice-webrtc`
  are added to the `publish-crates` step in `release.yml`
  in dependency order (renderer after compression; webrtc
  after usbredir; ryll last).
- `libopus-dev` is installed on the Linux CI runner so the
  `opus` crate dynamic-links against a system libopus.
  The deb/rpm packaging metadata records `libopus0` as a
  runtime dependency. On macOS and Windows the `audiopus_sys`
  source-build fallback applies (pkg-config absent → compile
  from source → no runtime dep).
- `tools/web-smoke.sh` is a CI step on the Linux matrix
  entry only. It launches `ryll --web` with a stub `.vv`,
  asserts the process stays alive for 3 seconds, sends
  SIGTERM, and verifies clean exit within 5 seconds.
  macOS and Windows CI verifies the `--web` dependencies
  link but does not run the smoke test.

## Phase 8: Operator Docs + Native TLS (`--web` mode)

Phase 8 of the web-frontend plan
(`docs/plans/PLAN-web-frontend-phase-08-docs.md`) closes
the operator-facing gaps and ships native TLS.

`axum-server` (with the `tls-rustls` feature) is added as
a dependency. Two CLI flags — `--web-tls-cert <PATH>` and
`--web-tls-key <PATH>` — activate HTTPS mode; clap's
`requires =` enforces that both are supplied together or
neither. When TLS is active the startup URL line prints
`https://` and the server uses
`axum_server::bind_rustls(addr, RustlsConfig)` with a
`Handle::graceful_shutdown` shim driven by
`SHUTDOWN_REQUESTED`, keeping the Phase 6 graceful-shutdown
semantics intact. A reference systemd unit at
`examples/ryll-web.service` shows the TLS-enabled
invocation with an `EnvironmentFile` pattern. With all 8
phases landed, the web-frontend project is complete.

## Code Organisation

```
ryll/src/
├── main.rs              # CLI entry, mode selection, Ctrl+C handler
├── app.rs               # egui App, event loop, GUI panels, headless
│                        #   runner, reconnect, egui trait impls
├── bugreport.rs         # Traffic ring buffer (TrafficBuffers,
│                        #   implements TrafficSink), bug-report ZIP
├── capture.rs           # Pcap + MP4 capture (CaptureSession,
│                        #   implements CaptureSink)
├── clipboard_arboard.rs # Host clipboard (implements ClipboardBackend)
├── config.rs            # CLI args, .vv parsing
├── display_gui.rs       # GuiSurface: egui TextureHandle wrapper
│                        #   around DisplaySurface
├── input_egui.rs        # egui::Key → LogicalKey adapter
├── notifications.rs     # NotificationStore + NotificationStoreSink
├── settings.rs          # is_verbose() gate
└── web/                 # --web mode (Phase 4–6)
    ├── mod.rs           # run_web(), HTTP server, /offer endpoint,
    │                    #   token auth, EncoderInfra stop helper
    ├── audio.rs         # WebOpusSink (implements OpusPacketSink)
    ├── cursor.rs        # Cursor relay → control datachannel
    ├── inputs.rs        # Input relay ← control datachannel
    └── lifecycle.rs     # run_bridge_reaper: watches dead signal,
                         #   reaps bridge + encoder when PC dies

shakenfist-spice-renderer/src/
├── channels/            # Per-channel handlers
│   ├── main_channel.rs  # Session negotiation, ping/pong
│   ├── display.rs       # Display, GLZ dictionary, draw-op decode
│   ├── cursor.rs        # Cursor tracking
│   ├── inputs.rs        # Keyboard/mouse, paste-as-keystrokes,
│   │                    #   LogicalKey enum, scancode tables
│   ├── playback.rs      # Audio (PCM/Opus → rtrb → cpal)
│   ├── usbredir.rs      # USB redirection (SpiceVMC)
│   └── webdav.rs        # WebDAV sharing (SpiceVMC)
├── display/
│   └── surface.rs       # DisplaySurface pixel buffer + draw-op API
├── encoder/
│   ├── mod.rs           # Re-exports
│   ├── frame_source.rs  # FrameSource trait, FrameRef, SyntheticFrameSource
│   ├── h264.rs          # H264Encoder, EncodedFrame
│   └── task.rs          # EncoderTask, EncoderControl
├── usb/                 # USB device backends (RealDevice, VirtualMsc)
├── webdav/              # WebDAV mux + embedded server
├── session.rs           # run_connection, run_headless orchestrators
├── capture_sink.rs      # CaptureSink trait
├── clipboard.rs         # ClipboardBackend trait
├── notification.rs      # NotificationEntry, NotificationSource
├── notification_sink.rs # NotificationSink trait
├── traffic.rs           # TrafficSink trait
├── log_config.rs        # LogConfig value type
├── snapshots.rs         # Channel-state snapshot types
└── byte_counter.rs      # ByteCounter

shakenfist-spice-webrtc/src/
└── bridge.rs            # WebrtcBridge, WebrtcBridgeConfig;
                         #   Phase 6: wait_for_dead(), dead_handle(),
                         #   dead_flag_handle()
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
  on the server's current mouse mode (SERVER → relative MOUSE_MOTION,
  CLIENT → absolute MOUSE_POSITION). See "Mouse-Mode Negotiation" under
  the SPICE Protocol section for the full negotiation flow including the
  post-reboot recovery path

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
| Display (2) | Graphics | surface_create, draw_fill, draw_copy, draw_blackness, draw_whiteness, draw_invers, copy_bits, draw_opaque, draw_blend, draw_transparent, draw_alpha_blend, mark |
| Inputs (3) | User input | key_down, key_up, mouse_position, mouse_motion (see "Mouse-Mode Negotiation" below) |
| Cursor (4) | Pointer | cursor_set, cursor_move, cursor_hide |
| Playback (5) | Audio playback | playback_start, playback_data, playback_mode, playback_stop |
| Usbredir (9) | USB redirection | vmc_data, vmc_compressed_data (SpiceVMC transport) |
| WebDAV (11) | Folder sharing | vmc_data, vmc_compressed_data (SpiceVMC transport) |

### Mouse-Mode Negotiation

The SPICE server can drive input dispatch in either of two
mouse modes, and which mode is in effect changes how ryll
sends pointer events:

- **SERVER mode (1, "relative")** — the server expects
  `MOUSE_MOTION` messages with `(dx, dy)` deltas. Common
  on minimal setups without a guest agent.
- **CLIENT mode (2, "absolute")** — the server expects
  `MOUSE_POSITION` messages with `(x, y)` in screen
  coordinates. Required for the cursor to track a
  windowed client cleanly. CLIENT is what ryll asks for
  whenever the server says it is supported.

#### Wire format

Both directions use 16-bit fields, even though the
in-memory C struct is `u32` — `spice.proto` declares
`mouse_mode` as `flags16`, and the marshaller narrows it
on the wire. Misreading the wire as `u32` produces
nonsense values like 131075 (`0x00020003` for
supported=3 / current=2) which fail every mode check.

| Direction | Message | Payload |
|-----------|---------|---------|
| Server → client | `MAIN_MOUSE_MODE` (`SpiceMsgMainMouseMode`) | Two little-endian u16: `supported_modes`, then `current_mode` |
| Client → server | `MAIN_MOUSE_MODE_REQUEST` (`SpiceMsgcMainMouseModeRequest`) | One little-endian u16: requested mode flags |

`parse_mouse_mode_payload` and
`build_mouse_mode_request_payload` in
[`ryll/src/channels/main_channel.rs`](ryll/src/channels/main_channel.rs)
own the read and write sides; both have unit tests next
to them.

#### Negotiation flow

1. **At session INIT**, the server announces both
   `supported_modes` (a bitmask) and `current_mode`. Ryll
   calls `maybe_request_client_mouse_mode`, which sends a
   `MOUSE_MODE_REQUEST(CLIENT)` if CLIENT is supported but
   not current.
2. **On any subsequent `MAIN_MOUSE_MODE`** — typically
   triggered by guest events such as a guest reboot
   (which often reverts the server to SERVER/relative
   while the agent reattaches) — ryll re-evaluates the
   same predicate. This is the recovery path that keeps
   absolute pointer events working without a manual
   reconnect.

#### Request-loop guard

`MainChannel::mouse_mode_request_pending` tracks whether
a `MOUSE_MODE_REQUEST` is outstanding.
`maybe_request_client_mouse_mode` skips sending if this
flag is already set, and the flag clears when a
subsequent `MAIN_MOUSE_MODE` arrives announcing
`current_mode == CLIENT`. This caps outbound requests at
one per round trip, so a flappy or buggy server that
never honours the request cannot amplify its
`MAIN_MOUSE_MODE` traffic into a storm of client-side
requests.

The predicate `should_request_client_mouse_mode` and the
encoder `build_mouse_mode_request_payload` are pure
functions with their own tests — three branches and a
byte-shape assertion respectively — so a regression in
either the negotiation logic or the wire format fails
loudly during `cargo test`.

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

### Draw-op coverage

The display channel handles the full set of
`DRAW_*` / `COPY_BITS` opcodes that modern QXL emits in
practice. Each opcode parses through a phase-1 protocol
struct, runs through a per-op `decode_*` classifier (a
pure free function that returns an `Outcome` enum), and
emits a typed `ChannelEvent` that the app-side handler
turns into a `DisplaySurface` mutation.

| Opcode | Status | Channel event | Surface helper |
|--------|--------|---------------|----------------|
| `COPY_BITS` (104) | implemented | `CopyBits` | `copy_bits` (snapshot-safe for overlap) |
| `DRAW_FILL` (302) | implemented | `FillRect` | `fill_rect` |
| `DRAW_OPAQUE` (303) | implemented | `ImageReady` | `blit` |
| `DRAW_COPY` (304) | implemented | `ImageReady` | `blit` |
| `DRAW_BLEND` (305) | implemented | `ImageReady` | `blit` |
| `DRAW_BLACKNESS` (306) | implemented | `FillRect` (colour `[0,0,0,255]`) | `fill_rect` |
| `DRAW_WHITENESS` (307) | implemented | `FillRect` (colour `[255,255,255,255]`) | `fill_rect` |
| `DRAW_INVERS` (308) | implemented | `Invert` | `invert_rect` |
| `DRAW_ROP3` (309) | warn-once | — | — |
| `DRAW_STROKE` (310) | warn-once | — | — |
| `DRAW_TEXT` (311) | warn-once | — | — |
| `DRAW_TRANSPARENT` (312) | implemented | `ImageReadyChroma` | `blit_chroma` (chroma-key) |
| `DRAW_ALPHA_BLEND` (313) | implemented | `ImageReadyAlpha` | `blit_alpha` (constant-alpha source-over) |
| `DRAW_COMPOSITE` (318) | warn-once | — | — |

Implemented ops silently ignore a handful of sub-features
that modern QXL rarely uses (non-`SPICE_ROPD_OP_PUT`
rops, non-solid brushes in `DRAW_FILL`/`DRAW_OPAQUE`,
non-null `SpiceQMask`). Each such fallback fires a
`warn_once!` with a stable registry key (see
STYLEGUIDE.md §"warn_once for protocol gaps" for the
convention) so the fallback is visible exactly once per
session. The `--pedantic` mode and always-visible
`Gaps: N` status-bar counter surface these the moment
they happen. See the pedantic-mode entry below.

### Colour byte-order convention

All SPICE colour fields (brush colours, chroma keys, etc.)
are BGRX on the wire: a `u32` read little-endian gives
bytes `[B, G, R, X]`. `DisplaySurface` stores pixels as
RGBA. **The BGRX → RGBA conversion happens in the channel
handler, not in the surface helpers** — surfaces trust
their inputs. This means:

* `FillRect.colour`, `ImageReadyChroma.chroma_rgba`, and
  every `ImageReady*.pixels` buffer is RGBA by the time
  it reaches the app-side handler.
* The conversion idiom at the channel site is:
  `[r, g, b, a] = [(c>>16)&0xff, (c>>8)&0xff, c&0xff, 0xff]`
  where `c` is the wire `u32`. Decoded image pixels are
  byte-swapped (when the source format isn't already
  RGBA) at decode time in `decode_image_and_emit`.

### `--pedantic` mode and the warn_once registry

Every protocol gap — truly-unknown opcode, known-but-
unimplemented opcode, ignored sub-feature on an
implemented op, recoverable decode failure — is
registered in the process-global warn_once registry
defined in
[shakenfist-spice-protocol/src/logging.rs](../shakenfist-spice-protocol/src/logging.rs).
Each call site holds a stable `&'static str` key shaped
`"<channel>:<kind>:<detail>"`; the registry fires
`tracing::warn!` exactly once per key per session.

The registry has a subscribe-and-replay hook
(`register_gap_observer`). `--pedantic` mode registers
an observer that writes one bug-report zip per new gap
into `--pedantic-dir` (default `./ryll-pedantic-reports/`,
capped at 50 zips per session). The observer runs inside
the app constructor (`RyllApp::new` for the GUI,
`run_headless` for headless) so the zips capture live
`TrafficBuffers` and `ChannelSnapshots` at the moment
the gap fires.

The always-visible `Gaps: N` button in the bottom status
panel polls `warn_once_count()` each frame; clicking opens
a floating window listing every fired key. The counter
works without `--pedantic` — `--pedantic` only adds the
bug-report-per-gap automation on top.

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

## Window sizing

The ryll window auto-fits to the guest display surface.
On every primary `SURFACE_CREATE` (and on the
`ImageReady` auto-create fallback for surface 0), the
event-handling path queues a viewport resize via
`pending_resize`. `RyllApp::update` consumes the pending
value, runs the pure `compute_auto_resize` decision
helper, and — if the helper returns Some — issues a
`ViewportCommand::InnerSize` to ask egui to make the
window match the surface. The aligned target is also
seeded into `last_sent_resize` so the next frame's
`maybe_send_monitors_resize` dedupes and we do not echo
our own resize back to the guest as a fresh
`VDAgentMonitorsConfig`.

The reverse direction — user drags the ryll window —
runs each frame in `maybe_send_monitors_resize`. The
viewport's inner-rect size is reduced by
`STATS_BAR_HEIGHT` (zero when maximised or fullscreen),
8-pixel aligned, and clamped to a floor of 8 on each
axis via `compute_outgoing_resize`. The result is sent
to the guest as a `VDAgentMonitorsConfig` if it differs
from `last_sent_resize`. The guest may honour the hint
exactly, pick the closest supported mode, or decline —
whatever resolution the guest actually chooses comes back
as a fresh `SURFACE_CREATE`, and the auto-fit pipeline
above re-syncs the window.

Three short-circuits keep the loop stable:

* `compute_auto_resize` returns None when the viewport is
  maximised or fullscreen; the surface renders at native
  size inside the available area rather than fighting
  egui for the inner size.
* `compute_auto_resize` dedupes against `last_auto_resize`
  so a no-op resize event does not refire the
  `ViewportCommand` every frame.
* `compute_outgoing_resize` plus `last_sent_resize`
  dedupes the outgoing side, so an auto-fit's seeded
  target does not bounce back to the guest as if the
  user had just dragged the window.

Both decision helpers are pure functions and are
unit-tested in `ryll/src/app.rs`'s `tests` module.

The auto-fit can be turned off with the
`Obey guest size hints` checkbox in the hamburger menu
(or the `--no-obey-guest-size` CLI flag at launch).
With the toggle off, the window stays where the user put
it and the surface renders at native pixel size inside
it — overflowing or letterboxing as the dimensions
require. The toggle is a session-level preference and
is **not** reset across a reconnect.

Every primary-surface mode change is also surfaced as an
Info notification ("Display resolution: WxH") through the
existing notification panel, debounced by
`RESOLUTION_NOTIFY_DEBOUNCE` (500 ms) so a burst of
events — boot probes that step `640×480 → 800×600 →
1024×768` over a second, or a drag-resize that steps
through dozens of 8-pixel-aligned sizes — collapses to a
single entry carrying the latest resolution. The
debounce is on top of the 30-second
`NOTIFICATION_DEDUP_WINDOW` from
`ryll/src/notifications.rs`, which folds same-resolution
repeats into a `count++` on the existing entry. The
decision is in the pure
`resolution_notification_due` helper next to the
window-fit helpers, and is unit-tested alongside them.

`pending_resize` is only set when the affected surface
key is `(display_channel_id == 0, surface_id == 0)`
(centralised as `is_primary_surface`), so a secondary
monitor's surface event cannot resize the primary
window.

Both auto-fit arms additionally refuse to honour
`SurfaceCreated` dimensions above
`MAX_AUTO_FIT_DIMENSION` (16384 px per axis,
`GL_MAX_TEXTURE_SIZE` on common hardware). A hostile
SPICE server can announce
`SurfaceCreated { width: u32::MAX, height: u32::MAX }`;
without the bound, ryll would forward that as
`ViewportCommand::InnerSize` (platform-dependent
behaviour, possibly large internal allocations) and
emit a notification carrying the absurd value. The cap
is checked at the trigger sites by
`auto_fit_size_acceptable`, which is unit-tested with
the other pure helpers; rejected sizes log a `warn!`
and leave the SPICE renderer's own surface bookkeeping
untouched.

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

Each frame's `process_events` drains the `ChannelEvent`
channel and dispatches image / fill / copy-bits /
invert / chroma / alpha events into the corresponding
`DisplaySurface` helper. See the draw-op coverage table
above for the full mapping from opcode to event to
surface method.

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

## Notifications

Ryll surfaces three categories of operator-relevant events through a
unified in-memory store and a single GUI surface:

1. **Protocol gaps** — distinct `warn_once!` keys registered in
   `shakenfist-spice-protocol/src/logging.rs`. Each new key produces
   one Warn-severity Gap entry via the gap observer registered in
   `notifications.rs`.

2. **SPICE_MSG_NOTIFY** — opcode 7 messages parsed on every channel
   handler; each is pushed as a Spice-source entry tagged with the
   receiving channel and the SPICE `what` enum value.

3. **Internal status** — bug-report writer success/failure,
   screenshot Ok/Err/no-surface, paste-completed.

The store (`ryll/src/notifications.rs`) is a 500-entry
`VecDeque<NotificationEntry>` behind `Arc<Mutex<NotificationStore>>`.
Pushes apply a 30-second deduplication window: identical
`(source, severity, message, visibility)` tuples within the window
fold into the most recent entry's `count`, incrementing the `[N×]`
suffix the side panel renders.

The bell glyph in the status-bar right-edge cluster tints by the
highest-severity unread entry's colour (default text colour for Info,
amber for Warn, muted red for Error). Low-visibility SPICE entries are
excluded from the bell colour calculation — they record but do not
flash.
Clicking the bell toggles a right-side Notifications panel that lists
entries newest first; closing the panel marks every visible entry
read.

The `register_gap_observer` hook in
`shakenfist-spice-protocol/src/logging.rs` supports multiple
observers, so the `--pedantic` zip writer and the notifications
observer coexist independently.

Bug-report zips include a `notifications.json` with the full store
snapshot at submit time, alongside the existing `metadata.json`,
`session.json`, `channel-state.json`, and `runtime-metrics.json`.
Operators handing zips to third parties should be aware that
notification messages can include server-side text such as
hostnames, paths, and error strings.

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

The USB panel is a right-side panel toggled by Menu → USB,
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

The Folders panel is a right-side panel toggled by
Menu → Folders. It mirrors the USB panel structure:
channel status indicator, active
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

## Reconnection

When the SPICE main channel closes or any secondary channel
reports an unrecoverable error, ryll surfaces a "Disconnected"
dialog with two buttons: Close and Reconnect. The Reconnect
path is implemented in
[`RyllApp::reconnect`](ryll/src/app.rs) and is a user gesture
— ryll never auto-reconnects.

### What is recreated

Every reconnect allocates a fresh copy of the per-session
machinery. This is what makes a reconnect equivalent to a
clean session against the same target rather than a
"resume":

- All five mpsc channels (`event`, `input`, `usb`, `webdav`,
  `resize`).
- A new `tokio::runtime::Runtime` inside a freshly spawned
  `std::thread::spawn`, with its own repaint-bridge task.
- A new `Arc<Notify>` for repaint wake-ups, a new
  `ByteCounter`, new `TrafficBuffers`, new
  `ChannelSnapshots`, a new `BandwidthTracker`, and a new
  `VolumeControl`.

Per-session UI state is reset in place: surfaces, cursor
position / visibility / image / texture, the cached
surface rectangle, statistics, last-cadence-key timestamp,
mouse mode, mouse-button state, modifier state,
last-sent resize, pending resize, USB connection state
(channel-ready, connecting / disconnecting flags, error
message, device description, connected-at), WebDAV
connection state (channel-ready, shared-dir, sharing
flag, connected-at, error message), and the disconnect
dialog itself.

### What survives

A reader investigating "did my settings carry over?" wants
this list first. The reconnect path **does not** touch:

- The parsed CLI configuration (target host, port, TLS,
  monitor count).
- The configured virtual-disk list and the configured
  shared folder. Both are stashed in
  `RyllApp::reconnect_virtual_disks` and
  `reconnect_share_dir` at construction so they survive
  the reset.
- The paste-as-keystrokes toggle and inter-character
  delay.
- The "Obey guest size hints" toggle. It is a
  session-level preference (set via the hamburger menu
  or `--no-obey-guest-size`) and is not touched by the
  reconnect path, so a reconnect inherits whatever
  value the user last left.
- The in-app notification store (history of past
  notifications). The store is an `Arc<Mutex<…>>` and
  the same `Arc` is handed to the new connection.
- The egui `Context`, which means window position and
  size, dock layouts, and any open side panels survive
  the reconnect — the Reconnect button feels like the
  same window resuming, not a new one.
- The active capture session, if any.

Anything not in either list above is unintentional and
should be considered a documentation bug; cross-check
against `RyllApp::reconnect` if in doubt.

### Threading and runtime lifecycle

Each reconnect spawns a fresh OS thread with a fresh
`tokio::runtime::Runtime`. The previous attempt is
explicitly cancelled before the new thread spawns:
`RyllApp` holds an `Arc<AtomicBool>` per attempt
(`connection_cancel`); `reconnect()` raises the previous
flag via `prev.store(true, Ordering::Relaxed)`, allocates
a fresh flag, and passes it to the new `run_connection`.

Inside `run_connection`, a small cancel-watcher task
polls the flag every 100 ms and calls `abort()` on the
`AbortHandle` of every channel `JoinHandle` once the flag
is set. The wait loop sees the channel tasks complete
with cancelled `JoinError`s, returns from
`run_connection`, the spawned thread's `block_on`
returns, and the tokio runtime is dropped. End to end,
a superseded attempt exits within roughly 100 ms of the
flag being raised, regardless of whether the underlying
TCP socket is responsive — well inside human reaction
time, so spamming Reconnect no longer accumulates threads
or sockets.

This mirrors the cooperative-cancel pattern used for
`SHUTDOWN_REQUESTED` (the global SIGINT flag). Both
signals share the same 100 ms poll cadence; both rely on
the runtime drop to release the underlying sockets.

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
│                         #   (submit), triggered_at (dialog-open),
│                         #   session_uptime_secs (submit),
│                         #   triggered_uptime_secs (dialog-open)
├── session.json          # AppSnapshot (FPS, bandwidth, surfaces)
├── channel-state.json    # snapshot of the affected channel
├── traffic.pcap          # ring buffer pcap (capture feature only)
├── screenshot.png        # trigger-time full surface (Display only)
├── screenshot-region.png # submit-time crop at the selected region
│                         #   (Display only, when a region was drawn)
└── runtime-metrics.json  # process and per-thread CPU%, RSS, VmSize
                          #   sampled over a 2-second window at
                          #   report-creation time (Linux only;
                          #   non-Linux platforms record
                          #   available:false with a reason)
```

Report types are `Display`, `Input`, `Cursor`, `Connection`, `Usb`,
and `Pedantic`, each mapping to one SPICE channel or the
--pedantic observer path.  `BugReport::new()` samples runtime
metrics over a 2-second window (blocking the caller), then gathers
and serialises all data synchronously.  `BugReport::write_zip()`
writes the zip to the capture directory's `bug-reports/`
subdirectory (if `--capture` is active) or the current working
directory.

`RyllApp::generate_bug_report()` is the high-level entry point
that collects surface pixels, constructs the `BugReport`, and
writes the zip.

Display bug reports carry two PNGs. `screenshot.png` is the
surface captured the moment the dialog opens — a background
`std::thread` PNG-encodes the cloned RGBA while the user types a
description. `screenshot-region.png` (when a region was drawn) is
a crop of the submit-time surface at the selected rectangle,
encoded on the UI thread after the user finishes the drag. The
two images are deliberately different moments in time.

Non-Display submissions drop the precomputed PNG even if one was
captured — the dialog captures unconditionally on open (so an
artefact doesn't fade while the user decides what to submit), but
only includes the PNG when the user actually submits as Display.

## Bug Report Dialog

Pressing **F12** or using **Menu → Report** opens a
centred modal dialog for generating bug reports.  The
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

### Trigger-time snapshot

On dialog open, `RyllApp::begin_trigger_snapshot` clones the
largest surface's RGBA and spawns a named `std::thread`
(`ryll-bugreport-png`) that PNG-encodes into a shared
`Arc<Mutex<Option<Result<Vec<u8>>>>>`. The submit path
(`finish_bug_report` → `take_trigger_for_submit`) consumes the
encoded bytes via `try_lock`, falling back to a live encode if
the encoder hasn't finished. Close-without-submit paths (Escape,
Cancel, F12 toggle-off) drop the `Arc`; the thread finishes into
what becomes garbage.

Keyboard and mouse input is not forwarded to the SPICE server
during selection.  Coordinates are clamped to the surface bounds.

## Live Traffic Viewer

Pressing **F11** or using **Menu → Traffic** toggles a
right-side panel showing a live feed of recent SPICE
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

## Paste-as-Keystrokes

The inputs channel includes a cooperative paste state machine for
typing text into guests that lack a vdagent clipboard channel.
Characters are translated to US-QWERTY AT scancodes via
`char_to_scancode()` and `translate_paste()` (both in `inputs.rs`),
capped at 4096 characters per paste.

The state machine (`PasteState`) runs as a conditional third arm in
the inputs channel's `tokio::select!` loop. A `tokio::time::sleep_until`
future fires on schedule; each firing sends one sub-step (press or
release) and yields back to the loop so the other two arms (server
reads and UI input events) remain responsive.

Per-character event sequence:
1. If shifted: KeyDown(Left Shift)
2. KeyDown(scancode)
3. Sleep half the inter-character delay
4. KeyUp(scancode)
5. If shifted: KeyUp(Left Shift)
6. Sleep the remaining half

At paste start, held modifier keys (Ctrl, Shift, Alt) are released
and saved; at paste end they are restored. Translation errors
(non-ASCII characters) emit `ChannelEvent::PasteFailed` and cause
a non-zero exit in headless mode.

CLI flags: `--enable-paste-as-keystrokes` (master gate),
`--paste-text TEXT` (headless trigger, implies enable),
`--paste-char-delay-ms N` (default 16ms).

GUI surface: When enabled, a "Paste" entry appears in the hamburger
menu with "Ctrl+Alt+V" shortcut text. The entry is disabled (greyed
out) when vdagent is connected, with a tooltip explaining to use
normal Ctrl+V. The Ctrl+Alt+V shortcut is detected before
`handle_input()` to prevent the V keypress from reaching the guest.
Pre-validation via `translate_paste()` catches unrepresentable
characters and shows an error dialog listing up to three sample
codepoints. The clipboard is read via `arboard::Clipboard` (lazily
initialised in `RyllApp::clipboard()`, separate from the
`MainChannel` instance).

## Keyboard Scancodes

Ryll maps egui key events to AT keyboard scancodes for the SPICE protocol.
Keys in the navigation cluster (arrow keys, Home, End, Insert, Delete,
PageUp, PageDown) require the E0 extended prefix to distinguish them from
their numpad equivalents. These are encoded in the u32 scancode field as
`(scancode << 8) | 0xE0`, matching spice-gtk's `spice_make_scancode()`.
The mapping table uses the 0x1xx convention internally (bit 8 set = extended).
