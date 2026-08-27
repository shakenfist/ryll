pub mod cursor;
pub mod display;
pub mod inputs;
pub mod main_channel;
#[cfg(feature = "audio")]
pub mod playback;
pub mod usbredir;
pub mod volume;
pub mod webdav;

pub use cursor::CursorChannel;
pub use display::DisplayChannel;
#[allow(unused_imports)] // PasteKey is part of translate_paste's public return type
pub use inputs::{translate_paste, InputsChannel, PasteError, PasteKey};
pub use main_channel::MainChannel;
#[cfg(feature = "audio")]
pub use playback::PlaybackChannel;
pub use usbredir::UsbredirChannel;
pub use volume::VolumeControl;
pub use webdav::WebdavChannel;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, Notify};
use tracing::warn;

use crate::notification::NotificationEntry;
use crate::usb::UsbDeviceInfo;
use shakenfist_spice_protocol::ChannelType;

/// A caller-chosen correlation token carried in every control-socket
/// request and echoed in the matching response.  Lives here (rather
/// than in `control::protocol`) so that channel events can carry one
/// on every platform — the control module itself is Unix-only.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    Int(i64),
    Str(String),
}

/// Where every channel hands a `ChannelEvent` to the renderer.
///
/// An event only reaches the screen if the renderer is woken after it is
/// queued. The queue and the wake-up used to be two separate fields, paired
/// by hand at every send site, so a newly added event that forgot the
/// wake-up would leave the UI stale with nothing to diagnose. Binding them
/// into one type makes the wake-up part of sending rather than something a
/// call site has to remember.
#[derive(Clone)]
pub struct EventSink {
    tx: mpsc::Sender<ChannelEvent>,
    repaint: Arc<Notify>,
    send_timeout: Option<Duration>,
}

impl EventSink {
    pub fn new(tx: mpsc::Sender<ChannelEvent>, repaint: Arc<Notify>) -> Self {
        EventSink {
            tx,
            repaint,
            send_timeout: None,
        }
    }

    /// Abandon a send that blocks for longer than `limit`, warning instead of
    /// hanging. Off by default; see `MAIN_EVENT_SEND_TIMEOUT` in `session.rs`
    /// for why the main channel opts in.
    pub fn with_send_timeout(mut self, limit: Duration) -> Self {
        self.send_timeout = Some(limit);
        self
    }

    /// Queue `event` and wake the renderer. A closed receiver means the
    /// renderer is already shutting down, which is not worth reporting.
    pub async fn emit(&self, event: ChannelEvent) {
        match self.send_timeout {
            None => {
                self.tx.send(event).await.ok();
            }
            Some(limit) => {
                if tokio::time::timeout(limit, self.tx.send(event))
                    .await
                    .is_err()
                {
                    warn!(
                        "channels: event send timed out after {:?}; \
                         renderer event consumer is wedged or starved",
                        limit
                    );
                }
            }
        }
        self.repaint.notify_one();
    }
}

/// Events sent from channels to the main application
#[derive(Debug, Clone)]
pub enum ChannelEvent {
    /// Session initialized with session ID
    SessionInitialized(u32),

    /// Channel list received
    ChannelsAvailable(Vec<(ChannelType, u8)>),

    /// Surface created
    SurfaceCreated {
        display_channel_id: u8,
        surface_id: u32,
        width: u32,
        height: u32,
    },

    /// Surface destroyed
    SurfaceDestroyed {
        display_channel_id: u8,
        surface_id: u32,
    },

    /// Image data ready to display
    ImageReady {
        display_channel_id: u8,
        surface_id: u32,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        pixels: Vec<u8>, // RGBA
        #[allow(dead_code)]
        image_id: u64,
        /// Session-relative seconds at the moment the emitting channel called
        /// `event_tx.send`. Used by the app to compute mpsc-queue lag for
        /// renderer-side latency diagnostics. See
        /// `docs/plans/PLAN-video-keeping-up.md`.
        produced_at_secs: f64,
    },

    /// Image-bearing paint with chroma-keying (DRAW_TRANSPARENT).
    ///
    /// Pixels whose lower-24-bit RGB equals `chroma_rgba[0..3]`
    /// leave the destination untouched.
    ImageReadyChroma {
        display_channel_id: u8,
        surface_id: u32,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        pixels: Vec<u8>, // RGBA
        chroma_rgba: [u8; 4],
        #[allow(dead_code)]
        image_id: u64,
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// Image-bearing paint with constant-alpha blending
    /// (DRAW_ALPHA_BLEND).  Straight (non-premultiplied) alpha;
    /// per-pixel source alpha multiplies through the constant.
    ImageReadyAlpha {
        display_channel_id: u8,
        surface_id: u32,
        left: u32,
        top: u32,
        width: u32,
        height: u32,
        pixels: Vec<u8>, // RGBA
        alpha: u8,
        #[allow(dead_code)]
        image_id: u64,
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// Solid-colour fill of a destination rect.
    ///
    /// Used by DRAW_FILL (with a solid brush), DRAW_BLACKNESS,
    /// and DRAW_WHITENESS. `rect` is `(left, top, right, bottom)`
    /// in surface coordinates; `clip` is the SpiceClip rect list
    /// from the draw message (empty = no extra clipping).
    FillRect {
        display_channel_id: u8,
        surface_id: u32,
        rect: (u32, u32, u32, u32),
        colour: [u8; 4],
        clip: Vec<(u32, u32, u32, u32)>,
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// Intra-surface pixel copy (DRAW_COPY_BITS).
    CopyBits {
        display_channel_id: u8,
        surface_id: u32,
        src_x: u32,
        src_y: u32,
        dest_rect: (u32, u32, u32, u32),
        clip: Vec<(u32, u32, u32, u32)>,
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// In-place RGB inversion of a rect (DRAW_INVERS).
    Invert {
        display_channel_id: u8,
        surface_id: u32,
        rect: (u32, u32, u32, u32),
        clip: Vec<(u32, u32, u32, u32)>,
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// Display mark (frame boundary)
    DisplayMark {
        /// See `ImageReady::produced_at_secs`.
        produced_at_secs: f64,
    },

    /// Cursor position updated
    CursorPosition {
        x: u16,
        y: u16,
        visible: bool,
    },

    /// Cursor image shape updated
    CursorShape(CursorImage),

    /// Mouse mode from server (1=server, 2=client)
    MouseMode(u32),

    MonitorsConfig {
        width: u32,
        height: u32,
    },

    /// Statistics update (reserved for future use)
    #[allow(dead_code)]
    Statistics {
        channel: String,
        bytes_in: u64,
        bytes_out: u64,
    },

    /// Latency measurement (sample in milliseconds)
    Latency {
        sample_ms: f32,
    },

    /// Paste-as-keystrokes sequence completed.
    ///
    /// `request_id` is `Some` when the paste was initiated via the
    /// control socket (so subscribers can correlate the completion
    /// back to the originating `paste` request).  It is `None` for
    /// pastes initiated by the `--paste-text` CLI flag.
    PasteCompleted {
        chars: usize,
        elapsed_ms: u64,
        request_id: Option<RequestId>,
    },

    /// Paste-as-keystrokes failed (unrepresentable characters).
    ///
    /// `request_id` mirrors the semantics of `PasteCompleted`.
    PasteFailed {
        reason: String,
        request_id: Option<RequestId>,
    },

    /// vdagent connection state changed.
    AgentConnected(bool),

    /// Connection error
    Error {
        channel: ChannelType,
        message: String,
    },

    /// A channel-side notification destined for the host's
    /// notification store. Replaces the old direct
    /// `notifications.lock().push(entry)` calls inside channels
    /// — the host drains the event channel and pushes each
    /// `NotificationEntry` into its store.
    Notification(NotificationEntry),

    /// A USB redirection channel connected successfully
    UsbChannelReady,

    /// A USB device was successfully connected
    UsbDeviceConnected(String),

    /// A USB device was disconnected
    UsbDeviceDisconnected,

    /// A USB device connection attempt failed
    UsbConnectFailed(String),

    /// Available USB devices changed (enumeration result)
    #[allow(dead_code)]
    UsbDevicesChanged(Vec<UsbDeviceInfo>),

    /// A WebDAV channel connected successfully
    WebdavChannelReady,

    /// WebDAV folder sharing started
    WebdavSharingStarted {
        path: String,
        read_only: bool,
    },

    /// WebDAV folder sharing stopped
    WebdavSharingStopped,

    /// A WebDAV error occurred
    WebdavError(String),

    /// Channel disconnected
    Disconnected(ChannelType),

    /// A new visual digest was decoded from the primary surface.
    ///
    /// Emitted by the polling task in `crate::digest` when the
    /// `digest-decode` Cargo feature is on.  The control server's
    /// `translate_event` turns this into a `digest_updated` wire
    /// event for any client that subscribed.  Deduplication is by
    /// `frame_counter` on the producer side; consumers always see
    /// each event exactly once per counter change.
    #[cfg(feature = "digest-decode")]
    DigestUpdated {
        frame_counter: u32,
        framebuffer_hash: u32,
        /// Decoded raw events from the digest payload.  Stored as
        /// `serde_json::Value` so the renderer crate does not have
        /// to re-export the digest crate's `Event` type publicly.
        events: serde_json::Value,
    },
}

/// Events sent from the application to the inputs channel
///
/// The key variants carry a **wire-format** scancode, not a logical
/// one: the inputs channel writes the value straight into the SPICE
/// message without touching it. Build them with
/// [`make_scancode`](crate::make_scancode) rather than by hand — the
/// release bit and the byte order of `0xE0`-prefixed codes are both
/// easy to get wrong, and both failures are invisible until a guest
/// is watching.
#[derive(Debug, Clone)]
pub enum InputEvent {
    /// Key pressed. Wire-format scancode; see the type docs.
    KeyDown(u32),

    /// Key released. Wire-format scancode, release bit set; see the
    /// type docs.
    KeyUp(u32),

    /// Mouse moved (absolute position, client mode)
    MouseMove { x: u32, y: u32 },

    /// Mouse moved (relative delta, server mode)
    MouseMotion { dx: i32, dy: i32 },

    /// Mouse button pressed
    MouseDown { button: u32, x: u32, y: u32 },

    /// Mouse button released
    MouseUp { button: u32, x: u32, y: u32 },

    /// Paste a string as synthetic keystrokes (US-QWERTY).
    ///
    /// `request_id` and `cancel` are `Some` when the paste was
    /// initiated via the control socket.  The `cancel` token is
    /// polled between characters so a client disconnect can abort
    /// the in-progress paste without leaving synthetic key events
    /// running.  Both are `None` for the `--paste-text` CLI path.
    PasteText {
        text: String,
        char_delay_ms: u32,
        /// Correlation token echoed in the resulting
        /// `PasteCompleted` / `PasteFailed` channel event.
        request_id: Option<RequestId>,
        /// Optional cancellation token.  When `Some`, the paste
        /// state machine checks `cancel.is_cancelled()` before
        /// advancing each sub-step and aborts with a
        /// `PasteFailed` event if it fires.
        cancel: Option<tokio_util::sync::CancellationToken>,
    },
}

/// Commands sent from the app to the webdav channel.
pub enum WebdavCommand {
    /// Start sharing a local directory.
    ShareDirectory { path: PathBuf, read_only: bool },
    /// Stop sharing the current directory.
    StopSharing,
}

/// Commands sent from the app to the usbredir channel.
pub enum UsbCommand {
    /// Connect a physical USB device by bus/address (Linux only).
    #[cfg(target_os = "linux")]
    ConnectPhysical { bus: u8, address: u8 },
    /// Connect a virtual mass storage disk image.
    ConnectVirtualDisk { path: PathBuf, read_only: bool },
    /// Disconnect the currently connected device.
    DisconnectDevice,
}

/// Decoded cursor image in RGBA format
#[derive(Debug, Clone)]
pub struct CursorImage {
    pub width: u16,
    pub height: u16,
    pub hot_spot_x: u16,
    pub hot_spot_y: u16,
    pub pixels: Vec<u8>, // RGBA
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sink, its receiver, and the wake-up the renderer waits on.
    ///
    /// The receiver is returned rather than dropped so a test can decide
    /// whether the consumer exists, is wedged, or has gone away — the three
    /// states `emit` has to handle.
    fn sink_pair(capacity: usize) -> (EventSink, mpsc::Receiver<ChannelEvent>, Arc<Notify>) {
        let (tx, rx) = mpsc::channel(capacity);
        let repaint = Arc::new(Notify::new());
        (EventSink::new(tx, Arc::clone(&repaint)), rx, repaint)
    }

    #[tokio::test]
    async fn emit_queues_the_event_and_wakes_the_renderer() {
        let (sink, mut rx, repaint) = sink_pair(4);

        sink.emit(ChannelEvent::SessionInitialized(7)).await;

        match rx.try_recv() {
            Ok(ChannelEvent::SessionInitialized(id)) => assert_eq!(id, 7),
            other => panic!("expected SessionInitialized(7), got {:?}", other),
        }
        // notify_one leaves a permit behind when nobody is waiting, so this
        // resolves at once if -- and only if -- emit signalled.
        tokio::time::timeout(Duration::from_millis(100), repaint.notified())
            .await
            .expect("emit must wake the renderer");
    }

    #[tokio::test]
    async fn a_wedged_consumer_does_not_hang_the_sender() {
        let (sink, _rx, repaint) = sink_pair(1);
        let sink = sink.with_send_timeout(Duration::from_millis(50));

        // Fill the only slot. `_rx` is held, so the channel is not closed --
        // it is simply never polled, which is what a wedged renderer looks
        // like from here.
        sink.emit(ChannelEvent::SessionInitialized(1)).await;
        // Consume the permit that emit left, so the assertion at the end
        // is about the *second* emit rather than this one.  Bounded, so a
        // regression that stops signalling fails here instead of hanging.
        tokio::time::timeout(Duration::from_millis(100), repaint.notified())
            .await
            .expect("the first emit must wake the renderer");

        // The second send can never complete. It has to give up on its own
        // 50 ms deadline; the outer bound is a backstop so a regression
        // fails the test rather than hanging the suite.
        tokio::time::timeout(
            Duration::from_secs(5),
            sink.emit(ChannelEvent::SessionInitialized(2)),
        )
        .await
        .expect("emit must abandon a blocked send rather than hang");

        tokio::time::timeout(Duration::from_millis(100), repaint.notified())
            .await
            .expect("a timed-out emit must still wake the renderer");
    }

    #[tokio::test]
    async fn a_send_within_the_deadline_still_delivers() {
        // The timeout arm must not cost delivery on the normal path: the
        // distinction that matters is elapsed-deadline versus closed
        // receiver, and only the former is worth warning about.
        let (sink, mut rx, _repaint) = sink_pair(4);
        let sink = sink.with_send_timeout(Duration::from_millis(50));

        sink.emit(ChannelEvent::SessionInitialized(9)).await;

        match rx.try_recv() {
            Ok(ChannelEvent::SessionInitialized(id)) => assert_eq!(id, 9),
            other => panic!("expected SessionInitialized(9), got {:?}", other),
        }
    }

    #[tokio::test]
    async fn a_dropped_receiver_is_not_an_error() {
        let (sink, rx, repaint) = sink_pair(1);
        drop(rx);

        // A closed receiver means the renderer is already shutting down.
        // This must not panic, and must still signal: a bridge task may be
        // waiting on the notify to observe the shutdown.
        sink.emit(ChannelEvent::SessionInitialized(1)).await;

        tokio::time::timeout(Duration::from_millis(100), repaint.notified())
            .await
            .expect("emit must wake the renderer even after the receiver is gone");
    }
}
