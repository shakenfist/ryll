# Ryll Style Guide

Conventions and patterns for the ryll codebase. New code should follow
these patterns for consistency. See `AGENTS.md` for a full table of
protocol reference sources. Key references:

- **shakenfist/kerbside** -- Python SPICE proxy with protocol docs in
  `docs/` and a reference test client in `testclient/ryll/`
- `/srv/src-reference/spice/spice-protocol/` -- canonical SPICE enum
  definitions and message structures
- `/srv/src-reference/spice/spice-gtk/` -- reference C client
- `/srv/src-reference/qemu/qemu/ui/spice-*` -- server-side SPICE

## General

- Wrap lines at 120 characters.
- Use single quotes for Rust strings where the language permits (raw
  strings, byte strings). Standard `"strings"` are fine everywhere else
  since Rust requires them.
- Trim trailing whitespace.
- All code must pass `pre-commit run --all-files` (rustfmt, clippy with
  `-D warnings`, shellcheck).

## Channel handler structure

Every channel handler follows the same skeleton. Keep them consistent --
when you add a new channel, copy the structure from an existing one.

### Struct field ordering

```rust
pub struct FooChannel {
    stream: SpiceStream,                    // wire connection (always first)
    event_tx: mpsc::Sender<ChannelEvent>,   // outbound events
    input_rx: mpsc::Receiver<InputEvent>,   // inbound events (inputs channel only)
    buffer: Vec<u8>,                        // message accumulation buffer

    // Channel-specific state
    previous_images: HashMap<u64, Vec<u8>>, // (display only, etc.)

    // ACK management (omit for main channel)
    ack_generation: u32,
    ack_window: u32,
    message_count: u32,
    last_ack: u32,

    // Telemetry (always last)
    bytes_in: u64,
    bytes_out: u64,
}
```

### Run loop

All channels use the same loop pattern:

```rust
pub async fn run(&mut self) -> Result<()> {
    info!("channel_name: channel started");

    loop {
        let mut chunk = [0u8; CHUNK_SIZE];
        let n = match &mut self.stream { ... };

        if n == 0 {
            info!("channel_name: channel disconnected");
            self.event_tx.send(ChannelEvent::Disconnected(...)).await.ok();
            break;
        }

        self.buffer.extend_from_slice(&chunk[..n]);
        self.bytes_in += n as u64;
        self.process_messages().await?;
    }
    Ok(())
}
```

Chunk sizes: 4KB for low-bandwidth channels (main, cursor, inputs),
256KB for display.

### Message dispatch

```rust
async fn handle_message(&mut self, msg_type: u16, payload: &[u8]) -> Result<()> {
    let msg_type_str = message_names::channel_server(msg_type);

    if settings::is_verbose() {
        logging::log_message("received", "channel_name", msg_type, msg_type_str, ...);
    }

    match msg_type {
        channel_server::KNOWN_TYPE => { ... }
        _ => {
            logging::log_unknown("channel_name", "received", msg_type, ...);
        }
    }
    Ok(())
}
```

### Send helpers

Every channel implements an identical pair:

```rust
async fn send_with_log(&mut self, msg_type: u16, data: &[u8]) -> Result<()> {
    if settings::is_verbose() {
        let msg_type_str = message_names::channel_client(msg_type);
        let payload_size = data.len().saturating_sub(6) as u32;
        logging::log_message("sent", "channel_name", msg_type, msg_type_str, payload_size);
    }
    self.send(data).await
}

async fn send(&mut self, data: &[u8]) -> Result<()> {
    self.stream.write_all(data).await?;
    self.stream.flush().await?;
    self.bytes_out += data.len() as u64;
    Ok(())
}
```

### ACK handling

Channels that receive `SET_ACK` track `ack_generation` and `ack_window`,
and send `ACK` when `message_count - last_ack >= ack_window`. The main
channel is the exception -- it responds to `SET_ACK` with `ACK_SYNC` but
does not track a message window.

## Logging

### Channel name prefix

All log messages from channel handlers must include the channel name:

```rust
info!("display: surface created: id={}, {}x{}", ...);
warn!("inputs: read error: {}", e);
debug!("cursor: hide");
```

The structured logging helpers (`logging::log_message`,
`logging::log_unknown`) already include the channel name as a parameter.

### Verbose and intimate guards

- `settings::is_verbose()` gates detailed protocol logging. All
  channels use this.
- `settings::is_intimate()` additionally gates keystroke/mouse logging.
  Only the inputs channel uses this.
- Never log passwords or authentication material at any level.

### Log levels

| Level | Use for |
|-------|---------|
| `info!` | Session lifecycle (connect, disconnect, init), surface create/destroy |
| `debug!` | Per-message details when not in verbose mode, draw operations |
| `warn!` | Recoverable errors (malformed messages, decompression failures, cache misses) |
| `error!` | Only via `tracing::error!` in app.rs for fatal connection errors |

### Adding a new message type

When adding support for a new SPICE message:

1. Add the constant to the appropriate module in `protocol/constants.rs`.
2. Add the name mapping in the corresponding `message_names::` function
   in `protocol/logging.rs`.
3. Add the handler in the channel's `handle_message` match arm.
4. If the message has structured fields, add a parser struct in
   `protocol/messages.rs`.

Keep constants.rs and logging.rs in sync -- every constant should have
a name mapping so it doesn't show as "unknown" in logs.

### warn_once for protocol gaps

Any time the client deliberately drops, partially-handles, or falls
back on something it received from the server, emit `warn_once!` with
a colon-delimited `"<channel>:<kind>:<detail>"` static key. This
includes:

* Known-but-unimplemented opcodes -- every such match arm calls
  `warn_once!("<channel>:unimpl:<opcode_name>", "...")` and then
  `log_unknown_once("<channel>", msg_type, payload)` for a
  first-occurrence hex dump.
* Ignored sub-features on an otherwise-handled op -- non-`OP_PUT`
  ROP descriptors, non-solid brushes, non-null `SpiceQMask`, non-zero
  `alpha_flags`, etc. Key shape:
  `"<channel>:<opcode>:<subfeature>"`.
* Decode failures the channel recovers from (malformed image,
  truncated payload) -- key shape `"<channel>:<opcode>:<failure>"`.

Rules:

* Keys must be `&'static str`; use string literals at call sites.
  For dynamically-composed keys (e.g. `log_unknown_once` keying off
  `channel × msg_type`), use `logging::intern_key`.
* One key per distinct kind per session -- do not vary by instance
  count or payload. Pedantic mode reads the key registry to build
  its gap counter; per-instance keys would blow it up.
* Silent repeats: `warn_once!` only fires its `tracing::warn!` the
  first time per session. The rest of the flow (skip / unmasked
  paint / etc.) still runs normally on every call.
* The truly-unknown-opcode `_ =>` arms in every channel handler
  call `logging::log_unknown_once(channel, msg_type, payload)`.
  `log_unknown_once` enters the same warn_once registry with key
  `"<channel>:hexdump:<msg_type>"`, hex-dumps the payload on first
  occurrence, and stays silent on repeats. The older `log_unknown`
  is no longer used in channel handlers.

The registry is append-only within a session; there is no
remove-or-reset API. Tests query via `warn_once_keys()` and key off
literal strings containing the test's own name to avoid
cross-pollination (`cargo test` runs tests in parallel against the
shared registry).

## Protocol message structs

Message structs in `protocol/messages.rs` follow this pattern:

```rust
pub struct FooMessage {
    pub field_a: u32,
    pub field_b: u16,
}

impl FooMessage {
    pub const SIZE: usize = 6;

    pub fn read(data: &[u8]) -> io::Result<Self> {
        if data.len() < Self::SIZE {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "Not enough data for FooMessage",
            ));
        }
        let mut cursor = Cursor::new(data);
        Ok(FooMessage {
            field_a: cursor.read_u32::<LittleEndian>()?,
            field_b: cursor.read_u16::<LittleEndian>()?,
        })
    }
}
```

- Wire byte order is **little-endian** for all SPICE messages.
- `SPICE_ADDRESS` is **u32** in mini-header mode (not u64).
- Use `Cursor` + `ReadBytesExt`/`WriteBytesExt` for all parsing.
- Size validation first, then parsing.

## Image decompression

### Wire format

After the `ImageDescriptor` (18 bytes), the image data format
depends on the image type:

- **LZ_RGB (101) and GLZ_RGB (102)**: 4-byte `data_size` (u32 LE)
  prefix, then the LZ/GLZ stream. Skip the 4 bytes before passing
  to the decompressor.
- **ZLIB_GLZ_RGB (107)**: 8-byte header — `glz_data_size` (u32 LE)
  + `compressed_size` (u32 LE) — then zlib-compressed GLZ data.
  Decompress with zlib first, then pass to the GLZ decompressor.
- **LZ4 (109)**: NO `data_size` prefix. Data starts with 1-byte
  `top_down`, 1-byte `spice_format`, then per-row LZ4 blocks each
  with a 4-byte big-endian size prefix.
- **Pixmap (0)**: raw BGRX pixels, no header.
- **FromCache (103)**: no pixel data, look up by `image_id`.

### Decompressor headers

LZ and GLZ headers are **big-endian** (unlike the rest of SPICE). LZ
magic is `b"  ZL"` (two spaces + ZL). GLZ magic is also `b"  ZL"`
(the image type in the ImageDescriptor distinguishes them, not the
magic).

### Pixel format

- Wire format: BGRX (blue, green, red, padding) -- 4 bytes per pixel.
- Internal format: RGBA (red, green, blue, alpha) -- 4 bytes per pixel.
- All decompressors and the Pixmap handler must convert BGRX to RGBA.
- Alpha is always set to 255 (fully opaque).

### Dimension safety

Always use checked arithmetic for `width * height * 4` calculations.
A malicious server could send extreme dimensions to cause panics or
multi-gigabyte allocations:

```rust
let output_size = (width as usize)
    .checked_mul(height as usize)
    .and_then(|n| n.checked_mul(4))
    .ok_or_else(|| anyhow!("dimensions overflow: {}x{}", width, height))?;
```

## Error handling

- **Channel handlers never panic.** All methods return `Result<()>`.
  Propagate errors with `?`.
- **Malformed messages**: `warn!()` and return `Ok(())` -- don't crash
  the channel for one bad message.
- **Decompression failures**: return `None` (not `Err`) from the match
  arm so the channel continues processing.
- **Unknown message types**: log via `logging::log_unknown()` with a
  hex dump, then continue.

### unwrap() policy

Clippy's `unwrap_used` lint is enabled workspace-wide (`Cargo.toml`),
with test code exempted via `allow-unwrap-in-tests` in `clippy.toml`.
In production code:

- **Never `unwrap()` on anything derived from outside the process**
  (network input, config, files). Handle it per the rules above.
- **Provably-infallible cases** use `expect("why this cannot fail")`
  so the invariant is documented and the panic message is
  self-explanatory. Established messages: `"lock poisoned"` for mutex
  guards (escalating a poisoned lock is correct -- don't run on
  possibly-corrupt shared state), `"length checked above"` for
  slice-to-array conversions behind a length guard, and
  `"write to Vec cannot fail"` for `io::Write` into a `Vec`.
  For new length-prefixed writes prefer
  `buf.extend_from_slice(&x.to_le_bytes())`, which is infallible.
- **Guarded `Option` access** uses `if let` / `let-else`, not
  an `is_some()` check followed by `unwrap()`.
- **In tests, plain `unwrap()` is fine** -- a panic is a test failure.

## Events

Channel handlers communicate with the UI via `ChannelEvent` variants
sent through `event_tx`. Add new variants to `channels/mod.rs`. Use
`.await.ok()` when sending events -- dropping an event is preferable
to blocking the channel on a full queue.

## Capture mode (`--capture <DIR>`)

Ryll has an opt-in capture mode activated by `--capture <DIR>`.
When enabled, it writes:

- **Pcap files** (one per channel) with fake TCP/IP headers
  for Wireshark analysis. Decrypted SPICE payloads only
  (post-TLS).
- **MP4 video** of the primary display surface (surface 0),
  with H.264 encoding and variable-rate timestamps matching
  the real session timing. Frames are emitted on MARK
  boundaries, not on every draw_copy.

### Adding capture points

When adding a new channel or modifying message handling:

- Call `capture.packet_sent(channel, &bytes)` in every
  `send()` method.
- Call `capture.packet_received(channel, &bytes)` at the
  top of every message read loop iteration.
- Video frames are captured in `app.rs` on `DisplayMark`
  events via `capture.frame(0, pixels, w, h)`.

### Zero overhead when disabled

All capture is gated behind `Option<Arc<CaptureSession>>`.
Check with `if let Some(ref c) = self.capture` before
doing any work. Do not allocate buffers, format strings,
or do I/O when capture is disabled.

### Pcap details

- One pcap per channel: `main.pcap`, `display.pcap`,
  `cursor.pcap`, `inputs.pcap`.
- Fake TCP/IP headers via `etherparse` (client
  `10.0.0.1`, server `10.0.0.2:5900`).
- Unique source port per channel (10001-10004).
- TCP sequence numbers tracked per direction.
- Timestamps relative to session start.

### Video details

- `display.mp4` — H.264 Baseline profile via `openh264`.
- Lazy initialisation: encoder created on first frame
  once surface dimensions are known.
- SPS/PPS extracted from first encoded bitstream by
  scanning for Annex B start codes.
- `force_intra_frame()` before first encode to ensure
  IDR keyframe.
- RGBA → YUV420 via `openh264::formats::RgbaSliceU8`.
- MP4 requires `write_end()` on clean shutdown to write
  the moov atom. A SIGTERM-killed process will produce
  an incomplete file.

### File naming

```
<DIR>/
  main.pcap
  display.pcap
  cursor.pcap
  inputs.pcap
  display.mp4
```

## Testing

- Unit tests go in `#[cfg(test)] mod tests` within the source file.
- Use `make test` (runs in Docker) or `cargo test` locally.
- Test against a real SPICE server with `make test-qemu` for
  integration verification.
- The UEFI latency guest changes screen colour on keystrokes, making
  it useful for visual confirmation of display rendering.
