# Ryll Style Guide

Conventions and patterns for the ryll codebase. New code should follow
these patterns for consistency. The `shakenfist/kerbside` repository
contains a working SPICE proxy in Python and protocol documentation in
`docs/` -- refer to it when working on protocol questions.

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

After the `ImageDescriptor` (18 bytes), compressed image types (LzRgb,
GlzRgb) have a 4-byte `data_size: u32` prefix before the actual
compressed data. Skip it before passing to the decompressor.

### Decompressor headers

LZ and GLZ headers are **big-endian** (unlike the rest of SPICE). LZ
magic is `b"  ZL"` (two spaces + ZL). GLZ magic is `b"ZL.G"`.

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

## Events

Channel handlers communicate with the UI via `ChannelEvent` variants
sent through `event_tx`. Add new variants to `channels/mod.rs`. Use
`.await.ok()` when sending events -- dropping an event is preferable
to blocking the channel on a full queue.

## Testing

- Unit tests go in `#[cfg(test)] mod tests` within the source file.
- Use `make test` (runs in Docker) or `cargo test` locally.
- Test against a real SPICE server with `make test-qemu` for
  integration verification.
- The UEFI latency guest changes screen colour on keystrokes, making
  it useful for visual confirmation of display rendering.
