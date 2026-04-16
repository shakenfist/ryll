# Phase 4: Robustness and safety

Part of [PLAN-deferred-debt.md](PLAN-deferred-debt.md).

## Scope

Address missing error handling, unsafe platform
assumptions, and ungraceful shutdown paths. Five
sub-tasks:

| Step | Description | Source |
|------|-------------|--------|
| 4a | Abrupt exit in disconnect dialog | PLAN-pr20-followup.md #6 |
| 4b | Audio playback shutdown handling | PLAN-pr23-followup.md #7 |
| 4c | Audio Linux-only gate | PLAN-pr23-followup.md #6 |
| 4d | Mutex lock in audio callback | PLAN-pr23-followup.md #10 |
| 4e | Silent drop logging in build_tcp_frame | PLAN-pr20-followup.md #8 |

## 4a. Abrupt exit in disconnect dialog

### Bug

The disconnect dialog's "Close" button at app.rs:1969
calls `std::process::exit(0)`, which bypasses all
destructors and Drop impls. This can:

- Corrupt capture MP4 files (the moov atom is written
  in the `Drop` impl of `Mp4Writer`).
- Lose unflushed bug report data.
- Leave the audio device in an inconsistent state.

### Fix

Replace `std::process::exit(0)` with the same graceful
shutdown pattern used by the Ctrl+C handler at
app.rs:882-888:

```rust
if ui.button("Close").clicked() {
    if let Some(ref capture) = self.capture {
        capture.close();
    }
    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
}
```

This allows eframe to run its normal shutdown path,
executing all Drop impls. The `ctx` variable is already
in scope (it's the `&egui::Context` parameter of the
`update()` method).

### Files to modify

- `ryll/src/app.rs` -- disconnect dialog "Close" button

### Complexity

Trivial -- a 3-line replacement.

## 4b. Audio playback shutdown handling

### Bug

The playback channel's `run()` loop (playback.rs:215-243)
is a bare `loop { read().await }` with no shutdown check.
During graceful shutdown, it blocks on the socket read
until the tokio runtime is dropped, preventing clean
release of the cpal audio stream.

### Context

**No other channel checks SHUTDOWN_REQUESTED either.**
All channels (display, cursor, inputs) use the same bare
loop pattern and rely on tokio runtime cancellation.
However, the playback channel is the one that
specifically benefits from clean shutdown because it
holds the `cpal::Stream` which should be dropped
properly to release the audio device.

`SHUTDOWN_REQUESTED` is an `AtomicBool` defined in
`main.rs:43`, set by the Ctrl+C handler, and checked by
the GUI loop at app.rs:882 and the headless loop at
app.rs:2390.

### Fix

Wrap the socket read in `tokio::select!` with a
periodic shutdown check, following the headless loop
pattern:

```rust
pub async fn run(&mut self) -> Result<()> {
    info!("playback: channel started");
    loop {
        let mut chunk = [0u8; 65536];
        let stream = &mut self.stream;
        tokio::select! {
            result = async {
                match stream {
                    SpiceStream::Plain(s) => {
                        use tokio::io::AsyncReadExt;
                        s.read(&mut chunk).await
                    }
                    SpiceStream::Tls(s) => {
                        use tokio::io::AsyncReadExt;
                        s.read(&mut chunk).await
                    }
                }
            } => {
                let n = result?;
                if n == 0 {
                    // ... existing disconnect handling
                    break;
                }
                // ... existing message processing
            }
            _ = tokio::time::sleep(
                Duration::from_millis(100)
            ) => {
                if crate::SHUTDOWN_REQUESTED
                    .load(Ordering::Relaxed)
                {
                    info!("playback: shutdown requested");
                    break;
                }
            }
        }
    }
    Ok(())
}
```

The `self.stream` must be extracted into a local
reference before the `select!` to avoid borrow conflicts
with `self` across the async branches (the same pattern
used in main_channel.rs:115).

### Files to modify

- `ryll/src/channels/playback.rs` -- `run()` method

### Complexity

Low-medium. The `tokio::select!` transformation is
straightforward but requires care with mutable borrows.

## 4c. Audio Linux-only gate

### Bug

`unsafe impl Send for SendStream` (playback.rs:75) is
only sound on Linux where ALSA is thread-safe. On macOS
(CoreAudio) and Windows (WASAPI), the cpal stream has
thread-affinity requirements that make this unsafe.

### Context

Ryll's packaging targets include macOS and Windows (via
PLAN-packaging.md). The current code compiles but is
unsound on those platforms. The existing codebase has
established `#[cfg(target_os)]` patterns for
platform-specific code:

- `ryll/src/usb/mod.rs:9`:
  `#[cfg(target_os = "linux")] pub mod real;`
- `ryll/src/channels/usbredir.rs`: multiple cfg gates
- `ryll/src/app.rs:1353`: cfg gate on a match arm

### Fix

Gate the playback module behind
`#[cfg(target_os = "linux")]` and provide a stub on
other platforms.

**In `channels/mod.rs`:**

```rust
#[cfg(target_os = "linux")]
pub mod playback;
```

**New file `channels/playback_stub.rs`** (or inline
in mod.rs):

The stub must:

1. Export `VolumeControl` with the same API (used by the
   GUI volume slider regardless of platform).
2. Export `PlaybackChannel` with the same constructor
   signature.
3. The stub `run()` must still handle SPICE protocol
   messages (SET_ACK, PING/PONG) to keep the channel
   alive, but can discard all DATA messages. This
   prevents the SPICE server from disconnecting the
   session due to an unresponsive channel.
4. Log a warning on platforms where audio is not
   supported.

**In `app.rs`:** The `ChannelType::Playback` match arm
at line 2217 already constructs `PlaybackChannel` via
`crate::channels::playback::PlaybackChannel::new(...)`.
With cfg gating on the module, this will automatically
use the stub on non-Linux platforms. No conditional
compilation needed in app.rs itself.

**In `Cargo.toml`:** Gate `cpal` and `opus-decoder` deps
behind `[target.'cfg(target_os = "linux")'.dependencies]`
to avoid pulling in ALSA dev headers on macOS/Windows
builds.

### Complexity

Medium. The stub needs to replicate `VolumeControl`'s
public API and handle basic SPICE protocol messages.
The cleanest approach is to extract `VolumeControl` into
its own small module (or into `channels/mod.rs`) so it's
always available, and only gate the audio-specific code.

### Alternative: defer

This item provides safety on platforms we aren't
actively testing on yet. Since ryll is primarily a Linux
tool (for testing kerbside), this could be deferred
until macOS/Windows usage is more common. The unsafe
impl has a clear comment pointing to the plan. I
recommend implementing only if there is appetite for the
refactoring effort; otherwise mark as acknowledged.

## 4d. Mutex lock in audio callback

### Bug

The cpal real-time audio callback locks two mutexes:

1. `Arc<Mutex<VecDeque<i16>>>` (the audio buffer) --
   shared with the network thread that pushes samples.
2. `Arc<Mutex<Resampler>>` (the resampler) -- only
   used by the callback, but wrapped in a mutex to
   satisfy the `Send` bound.

If the network thread holds the buffer lock while
pushing a large batch of decoded audio, the audio
callback blocks, causing glitches (underruns).

### Fix

Replace the mutexed `VecDeque` with a lock-free
single-producer single-consumer ring buffer. The `rtrb`
crate (real-time ring buffer) is designed specifically
for this use case.

**Changes:**

1. Add `rtrb = "0.3"` to `Cargo.toml`.
2. Replace `type AudioBuffer = Arc<Mutex<VecDeque<i16>>>`
   with `rtrb::RingBuffer<i16>` split into
   `(rtrb::Producer<i16>, rtrb::Consumer<i16>)`.
3. The `Producer` goes to `PlaybackChannel` for
   network-side writes.
4. The `Consumer` goes into the cpal callback closure.
5. The `Resampler` can be owned directly by the callback
   closure (no mutex needed -- it's only accessed from
   the callback thread).

**Resampler rework:** The current `next_frame()` reads
from the `VecDeque` by index (`buffer[idx * ch + c]`),
which is a random-access pattern. A ring buffer consumer
only supports sequential reads. The fix is to maintain
a small local `VecDeque` inside the callback that is
topped up from the ring buffer consumer at the start of
each callback invocation. The resampler then operates
on the local buffer as before.

### Complexity

Medium-high. The changes touch the buffer type, producer
code, consumer code, and the resampler's relationship
to the buffer. The callback-local `VecDeque` approach
keeps the resampler's logic unchanged but adds a data
copy step (ring buffer → local VecDeque).

### Alternative: defer

The mutex contention only causes glitches under heavy
network activity. The existing code works correctly
(produces the right samples) -- it's a quality-of-service
issue rather than a correctness bug. I recommend deferring
this to a future iteration unless audio glitches are
observed in practice.

## 4e. Silent drop logging in build_tcp_frame

### Bug

`build_tcp_frame` (capture.rs:129-130) returns an empty
`Vec` when `ip_payload_len > 65515` with no logging.
This silently drops oversized packets from capture files.

The `write_segmented()` method already chunks payloads
into 65495-byte segments, so this guard should rarely
trigger from internal callers. However,
`build_tcp_frame` is `pub(crate)` and also called from
`bugreport.rs`, where the caller may not chunk.

### Fix

Add a `warn!` before the return:

```rust
if ip_payload_len > 65515 {
    warn!(
        "build_tcp_frame: payload too large for IPv4 \
         ({} bytes), dropping",
        ip_payload_len
    );
    return Vec::new();
}
```

**Note:** The `decode_mjpeg_frame` silent return (the
other half of the original PLAN-pr20-followup.md items
#8 and #9) was already fixed in phase 1 step 1d.

### Files to modify

- `ryll/src/capture.rs` -- `build_tcp_frame`

### Complexity

Trivial -- adding one `warn!` line.

## Commit sequence

1. **4a**: Replace `std::process::exit(0)` with graceful
   shutdown (standalone, no dependencies).
2. **4b**: Add shutdown handling to playback's `run()`
   (standalone).
3. **4e**: Add warn log to `build_tcp_frame`
   (standalone).
4. **4c**: Gate playback behind cfg(target_os = "linux")
   -- **recommend deferring** unless there is appetite
   for the refactoring.
5. **4d**: Lock-free audio buffer -- **recommend
   deferring** to a future iteration.

Steps 4a, 4b, and 4e are independent and low-risk.
Steps 4c and 4d are medium-complexity refactors that
address platform safety and quality-of-service rather
than correctness bugs. They can be deferred without
impacting the reliability of the Linux target.

## Administration

### Back brief

Before executing any step of this plan, please back
brief the operator as to your understanding of the plan
and how the work you intend to do aligns with that plan.
