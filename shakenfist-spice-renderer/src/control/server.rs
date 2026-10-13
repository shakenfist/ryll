//! Control-socket server task.
//!
//! Binds a Unix-domain socket at a caller-supplied path, enforces
//! single-client concurrency (a second connect gets a `busy` response
//! and is closed immediately), runs an NDJSON request/response loop,
//! and dispatches the v1 verbs.  The high-throughput event side is
//! served from a `tokio::sync::broadcast` fan-out (see
//! `session.rs`): each connected client owns its own
//! `broadcast::Receiver` plus a small bounded outbound queue.  If
//! the client falls behind, events are dropped on its side and a
//! `dropped` event is emitted once the queue drains — the SPICE
//! producers are never back-pressured by a slow control-socket
//! client.
//!
//! # Lifecycle
//!
//! 1. `Server::bind` runs at startup, before the session connects. It
//!    deletes any stale socket file, binds a new socket, `chmod`s it
//!    to `0600`, and only then listens, so nobody but the owner can
//!    ever connect. Any failure here is returned to the caller, which
//!    refuses to start: a caller that asked for a socket and drives
//!    ryll through it would otherwise wait for one that never appears.
//! 2. `Server::run` is spawned as a tokio task and accepts
//!    connections.  While a client is connected, additional accept
//!    attempts receive a `busy` response and are closed.
//! 3. When the `CancellationToken` fires (session shutdown), the
//!    accept loop exits and the task returns. The socket file is
//!    unlinked when the `Server` is dropped, so a server that is
//!    bound but never run, or whose task is aborted, does not leave
//!    it behind either.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use super::protocol::{
    major_version_matches, ClipboardChanged, ClipboardSetParams, ClipboardText, ErrorCode, Event,
    HelloParams, HelloResult, Request, RequestId, ResizeParams, Response, RpcError, StatusResult,
    SubscribeParams, SubscribeResult, UnsubscribeResult, PROTOCOL_VERSION, SUPPORTED_METHODS,
};
use crate::channels::{align_monitor_dimension, ChannelEvent, DisplaySize, InputEvent};
use crate::clipboard::MemoryClipboard;
use crate::display::MAX_SURFACE_DIMENSION;
use crate::surface_mirror::SurfaceMirror;
use shakenfist_spice_protocol::messages::agent_stream::MAX_AGENT_MESSAGE_SIZE;

/// Capacity of the per-client outbound queue (request responses +
/// translated event lines).  If the writer falls behind by more
/// than this, new events are counted as dropped and reported via
/// a `dropped` event once the queue drains.
const OUTBOUND_QUEUE_CAPACITY: usize = 256;

// ── StatusProvider ────────────────────────────────────────────────

/// Abstraction over the live headless session state.
///
/// The control server calls `snapshot()` each time a `status` request
/// arrives; the concrete implementation in `session.rs` reads the
/// real connection state without exposing session internals here.
pub trait StatusProvider: Send + Sync {
    fn snapshot(&self) -> StatusResult;
}

// ── SessionHandles ────────────────────────────────────────────────

/// The parts of a session the verbs act on.
///
/// Each accepted client gets a clone. Everything here is a handle --
/// a channel sender or an `Arc` -- so cloning is cheap and every
/// client acts on the same session.
#[derive(Clone)]
pub struct SessionHandles {
    /// The inputs channel's queue, for `send_key` and `paste`.
    pub input_tx: mpsc::Sender<InputEvent>,
    /// The live pixel store, for `screenshot`.
    pub surface_mirror: Arc<tokio::sync::Mutex<SurfaceMirror>>,
    /// The clipboard the main channel was given, for `clipboard_set`
    /// and `clipboard_get`, and the source of `clipboard_changed`.
    pub clipboard: Arc<MemoryClipboard>,
    /// The main channel's display-size queue, for `resize`.
    pub resize_tx: mpsc::Sender<DisplaySize>,
}

// ── Server ────────────────────────────────────────────────────────

/// A bound control socket, ready to `run`.
///
/// Binding is separate from running so that a caller can do it at
/// startup and refuse to start when it fails, rather than finding out
/// from a warning logged by a task that has already been spawned.
pub struct Server {
    listener: std::os::unix::net::UnixListener,
    socket_file: SocketFile,
}

/// Unlinks the socket file when dropped.
struct SocketFile(PathBuf);

/// Listen backlog for the control socket. Only one client is served
/// at a time, so this need only cover the `busy` rejections.
const LISTEN_BACKLOG: i32 = 128;

/// Bind a Unix-domain socket at `path` and set its file mode to
/// `0600`, without listening on it.
///
/// `UnixListener::bind` binds and listens in one call, so a `chmod`
/// after it leaves the socket connectable, with whatever mode the
/// umask gave it, until the `chmod` lands. Connecting needs write
/// permission on the socket, so under a `002` umask the owner's group
/// could connect in that window, and under `000` anyone who can reach
/// the directory. A socket that is bound but not listening refuses
/// every connect, so setting the mode between `bind` and `listen`
/// means nobody but the owner can ever connect. A restrictive umask
/// around the bind would also work, but umask is process-wide and
/// other threads may be creating files by then.
///
/// The mode is set on the path rather than the descriptor: `fchmod`
/// on a socket descriptor does not change the file's mode on Linux.
///
/// The returned [`SocketFile`] unlinks the path when dropped, so a
/// failure after the bind does not leave the file behind.
fn bind_owner_only(path: PathBuf) -> std::io::Result<(socket2::Socket, SocketFile)> {
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    socket.bind(&socket2::SockAddr::unix(&path)?)?;
    // From here on the file is ours.
    let socket_file = SocketFile(path);
    std::fs::set_permissions(&socket_file.0, std::fs::Permissions::from_mode(0o600))?;
    Ok((socket, socket_file))
}

impl Drop for SocketFile {
    fn drop(&mut self) {
        match std::fs::remove_file(&self.0) {
            Ok(()) => info!("control: removed socket {}", self.0.display()),
            Err(e) => warn!(
                "control: failed to remove socket {}: {}",
                self.0.display(),
                e
            ),
        }
    }
}

impl Server {
    /// Bind a Unix-domain socket at `socket_path`.
    ///
    /// A stale socket file at the path is removed first; any other
    /// kind of file there is an error rather than being deleted. The
    /// new socket's mode is set to `0600` before it listens (see
    /// [`bind_owner_only`]). Fails when the path is too long for
    /// `sockaddr_un`, its directory is missing or not writable, or the
    /// mode cannot be set.
    ///
    /// Needs no tokio runtime, so it can run before one exists.
    pub fn bind(socket_path: PathBuf) -> std::io::Result<Self> {
        let path = &socket_path;

        // Remove any stale socket file from a previous run — but only
        // if it IS a socket; don't blindly remove regular files.
        if path.exists() {
            let metadata = std::fs::metadata(path)?;
            // `metadata.file_type().is_socket()` works on Unix.
            use std::os::unix::fs::FileTypeExt;
            if metadata.file_type().is_socket() {
                std::fs::remove_file(path)?;
                info!("control: removed stale socket at {}", path.display());
            } else {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    format!(
                        "control socket path {} exists and is not a socket",
                        path.display()
                    ),
                ));
            }
        }

        let (socket, socket_file) = bind_owner_only(socket_path.clone())?;
        socket.listen(LISTEN_BACKLOG)?;
        let listener = std::os::unix::net::UnixListener::from(socket);

        // tokio requires a non-blocking listener in `run`.
        listener.set_nonblocking(true)?;

        info!("control: listening on {}", path.display());
        Ok(Self {
            listener,
            socket_file,
        })
    }

    /// The path the socket is bound at.
    pub fn path(&self) -> &Path {
        &self.socket_file.0
    }

    /// Run the accept loop until `shutdown` fires.
    ///
    /// `event_tx` is the broadcast bus that the headless event-loop
    /// fan-out publishes to.  Each accepted client subscribes a fresh
    /// `broadcast::Receiver` from this sender.
    ///
    /// `agent_connected` is the session's vdagent state.  The
    /// `agent_connected` event is translated from this watch rather
    /// than from the bus, so a client that lags the bus cannot miss a
    /// transition; each accepted client gets its own clone.
    ///
    /// `handles` are the session's input queue, surface mirror,
    /// clipboard and resize queue, which the verbs act on; each
    /// accepted client gets a clone. See [`SessionHandles`].
    pub async fn run(
        self,
        status_provider: Arc<dyn StatusProvider>,
        event_tx: broadcast::Sender<ChannelEvent>,
        agent_connected: watch::Receiver<bool>,
        handles: SessionHandles,
        shutdown: CancellationToken,
    ) -> std::io::Result<()> {
        // Owned for the whole loop, so the socket file outlives it.
        let socket_file = self.socket_file;
        let listener = UnixListener::from_std(self.listener)?;

        // Single-client enforcement: true while a client connection
        // is being handled.
        let client_active = Arc::new(AtomicBool::new(false));

        loop {
            tokio::select! {
                accept_result = listener.accept() => {
                    match accept_result {
                        Ok((stream, _)) => {
                            if client_active.load(Ordering::Acquire) {
                                // Write busy and close.
                                let busy_line = {
                                    let mut s = serde_json::to_string(&Response::busy())
                                        .unwrap_or_else(|_| r#"{"ok":false}"#.to_string());
                                    s.push('\n');
                                    s
                                };
                                let mut stream = stream;
                                let _ = stream.write_all(busy_line.as_bytes()).await;
                                let _ = stream.shutdown().await;
                                warn!("control: rejected second client (busy)");
                            } else {
                                client_active.store(true, Ordering::Release);
                                let flag = client_active.clone();
                                let provider = status_provider.clone();
                                let cancel = shutdown.clone();
                                let event_rx = event_tx.subscribe();
                                let agent_rx = agent_connected.clone();
                                let client_handles = handles.clone();
                                tokio::spawn(async move {
                                    handle_client(
                                        stream, provider, event_rx, agent_rx, client_handles, cancel,
                                    )
                                    .await;
                                    flag.store(false, Ordering::Release);
                                });
                            }
                        }
                        Err(e) => {
                            // Log the error; if the listener is broken we
                            // exit the loop so the task doesn't spin.
                            error!("control: accept error: {}", e);
                            break;
                        }
                    }
                }
                _ = shutdown.cancelled() => {
                    info!("control: shutdown requested");
                    break;
                }
            }
        }

        // Unlinks the socket file.
        drop(socket_file);

        Ok(())
    }
}

// ── Per-client handler ────────────────────────────────────────────

/// One outbound line waiting to be written to the client socket.
///
/// Responses and events share a single mpsc into the writer task so
/// that lines interleave in the correct chronological order on the
/// wire.
enum OutboundMessage {
    Response(Response),
    Event(Event),
}

/// Per-client state shared across the request handler, event
/// translator, and writer tasks within a single connection.
struct ClientState {
    /// Names the client is currently subscribed to.  Empty by
    /// default; mutated by `subscribe` / `unsubscribe` verbs.
    subscriptions: std::sync::Mutex<HashSet<String>>,

    /// In-flight paste operations keyed by request id.  Each entry
    /// holds a `CancellationToken` that is fired when the client
    /// disconnects (or when the paste completes / fails and the
    /// entry is removed).  The lock is taken briefly by the request
    /// handler to insert/remove, and by the disconnect path to fire
    /// all remaining tokens.
    in_flight_pastes: std::sync::Mutex<HashMap<RequestId, CancellationToken>>,
}

impl ClientState {
    fn new() -> Self {
        Self {
            subscriptions: std::sync::Mutex::new(HashSet::new()),
            in_flight_pastes: std::sync::Mutex::new(HashMap::new()),
        }
    }

    fn is_subscribed(&self, event: &str) -> bool {
        self.subscriptions
            .lock()
            .map(|s| s.contains(event))
            .unwrap_or(false)
    }

    /// Register a new in-flight paste and return the cancellation
    /// token to pass into the `InputEvent::PasteText`.
    fn register_paste(&self, request_id: RequestId) -> CancellationToken {
        let token = CancellationToken::new();
        if let Ok(mut map) = self.in_flight_pastes.lock() {
            map.insert(request_id, token.clone());
        }
        token
    }

    /// Remove the in-flight paste entry for the given request id.
    /// Called when a `PasteCompleted` or `PasteFailed` event arrives.
    fn complete_paste(&self, request_id: &RequestId) {
        if let Ok(mut map) = self.in_flight_pastes.lock() {
            map.remove(request_id);
        }
    }

    /// Cancel all in-flight pastes.  Called on client disconnect so
    /// background paste tasks stop generating synthetic key events.
    fn cancel_all_pastes(&self) {
        if let Ok(map) = self.in_flight_pastes.lock() {
            for token in map.values() {
                token.cancel();
            }
        }
    }
}

async fn handle_client(
    stream: tokio::net::UnixStream,
    status_provider: Arc<dyn StatusProvider>,
    event_rx: broadcast::Receiver<ChannelEvent>,
    agent_connected: watch::Receiver<bool>,
    handles: SessionHandles,
    shutdown: CancellationToken,
) {
    info!("control: client connected");

    let (read_half, write_half) = stream.into_split();

    // Outbound mpsc carrying responses + events to the writer task.
    // Capacity 256 matches the per-client backpressure budget called
    // out in the protocol doc.  Responses use the same queue so that
    // a slow writer also back-pressures request handling, but the
    // event-translation task uses `try_send` so it never blocks the
    // broadcast bus.
    let (out_tx, out_rx) = mpsc::channel::<OutboundMessage>(OUTBOUND_QUEUE_CAPACITY);

    let client_state = Arc::new(ClientState::new());

    // Writer task: drains the outbound mpsc and writes lines.
    let writer_handle = tokio::spawn(writer_task(write_half, out_rx));

    // Event-translation task: subscribes to the broadcast bus,
    // filters by `subscriptions`, formats wire `Event` payloads,
    // and tries to push them onto the outbound queue without
    // blocking.  On overflow, increments a dropped counter; when
    // the queue catches up, emits a single `dropped` event.
    let event_state = client_state.clone();
    let event_out_tx = out_tx.clone();
    let event_handle = tokio::spawn(event_translator_task(
        event_rx,
        agent_connected,
        handles.clipboard.subscribe_guest_copies(),
        event_state,
        event_out_tx,
    ));

    // Reader / dispatcher: parses each request line, dispatches to
    // the appropriate verb handler, and queues the response on the
    // outbound mpsc.
    let mut lines = BufReader::new(read_half).lines();
    let mut helloed = false;

    loop {
        tokio::select! {
            line_result = lines.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let (response, close_after) = dispatch_request(
                            &line,
                            &mut helloed,
                            &*status_provider,
                            &client_state,
                            &handles,
                        )
                        .await;

                        if out_tx
                            .send(OutboundMessage::Response(response))
                            .await
                            .is_err()
                        {
                            warn!("control: writer task closed; dropping client");
                            break;
                        }

                        if close_after {
                            // Protocol-version-mismatch path: drop the
                            // outbound sender so the writer drains and
                            // exits, then bail out of the reader loop.
                            break;
                        }
                    }
                    Ok(None) => {
                        info!("control: client disconnected");
                        break;
                    }
                    Err(e) => {
                        warn!("control: read error: {}", e);
                        break;
                    }
                }
            }
            _ = shutdown.cancelled() => {
                info!("control: client task cancelled by shutdown");
                break;
            }
        }
    }

    // Cancel every in-flight paste so background tasks stop
    // producing synthetic key events into an orphaned session.
    client_state.cancel_all_pastes();

    // Drop our outbound sender so the writer task exits cleanly
    // once it has drained the queue.  The event-translator task is
    // explicitly aborted because it would otherwise idle on the
    // broadcast receiver forever.
    drop(out_tx);
    event_handle.abort();
    let _ = event_handle.await;
    let _ = writer_handle.await;
}

/// Writer side of the per-client connection.  Owned by its own task
/// so the dispatcher / translator never block on `write_all`.
async fn writer_task(
    mut write_half: tokio::net::unix::OwnedWriteHalf,
    mut out_rx: mpsc::Receiver<OutboundMessage>,
) {
    while let Some(message) = out_rx.recv().await {
        let line = match &message {
            OutboundMessage::Response(r) => serde_json::to_string(r),
            OutboundMessage::Event(e) => serde_json::to_string(e),
        };
        let mut serialised = match line {
            Ok(s) => s,
            Err(e) => {
                error!("control: failed to serialise outbound message: {}", e);
                continue;
            }
        };
        serialised.push('\n');
        if let Err(e) = write_half.write_all(serialised.as_bytes()).await {
            warn!("control: write error: {}", e);
            break;
        }
    }
    let _ = write_half.shutdown().await;
}

/// Translator side of the per-client connection.  Subscribes to the
/// broadcast bus, filters by the active subscription set, formats
/// each `ChannelEvent` into a wire `Event`, and `try_send`s the
/// result onto the outbound mpsc.  The `agent_connected` event comes
/// from the session's agent-state watch instead of the bus, and
/// `clipboard_changed` from the clipboard's guest-text watch, so a
/// client that lags the bus cannot miss either.
///
/// Backpressure rule: on `try_send` failure (queue full) or on a
/// broadcast `Lagged(n)` error, increment a local dropped counter.
/// When the next send succeeds AND the counter is non-zero, emit a
/// `dropped` event with the cumulative count and reset.
async fn event_translator_task(
    mut event_rx: broadcast::Receiver<ChannelEvent>,
    mut agent_connected: watch::Receiver<bool>,
    mut guest_clipboard: watch::Receiver<Option<usize>>,
    state: Arc<ClientState>,
    out_tx: mpsc::Sender<OutboundMessage>,
) {
    // Cumulative dropped-events counter, since the last `dropped`
    // event was emitted.  Saturates at `u32::MAX` on emit.
    let mut dropped_count: u64 = 0;

    // Track the previous agent-connected state so we emit only on
    // transitions, as the protocol doc commits to.  Seeded with the
    // state at connect, so a client hears about changes rather than
    // a restatement of where things already stood.
    let mut last_agent_connected = *agent_connected.borrow_and_update();
    // Cleared when the session's state senders are dropped, so a
    // finished session does not spin on a `changed()` that fails at
    // once.  The bus closing ends the task shortly after.
    let mut agent_state_open = true;
    // The same, for the clipboard's watch. Subscribing marked its
    // current value seen, so only copies after connect are reported.
    let mut clipboard_open = true;

    loop {
        // Translate into the wire shape.  Anything the protocol
        // doesn't care about, or the client has not subscribed to,
        // comes back `None` and is silently discarded.
        let translated = tokio::select! {
            received = event_rx.recv() => match received {
                Ok(event) => translate_event(&event, &state),
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    dropped_count = dropped_count.saturating_add(n);
                    continue;
                }
                Err(broadcast::error::RecvError::Closed) => {
                    return;
                }
            },
            changed = agent_connected.changed(), if agent_state_open => match changed {
                Ok(()) => {
                    let connected = *agent_connected.borrow_and_update();
                    translate_agent_connected(connected, &state, &mut last_agent_connected)
                }
                Err(_) => {
                    agent_state_open = false;
                    None
                }
            },
            changed = guest_clipboard.changed(), if clipboard_open => match changed {
                Ok(()) => {
                    let bytes = *guest_clipboard.borrow_and_update();
                    translate_clipboard_changed(bytes, &state)
                }
                Err(_) => {
                    clipboard_open = false;
                    None
                }
            },
        };
        let Some(wire) = translated else {
            continue;
        };

        match out_tx.try_send(OutboundMessage::Event(wire)) {
            Ok(()) => {
                // If we previously dropped events and the
                // queue has now caught up, flush the
                // accumulated count.  We approximate
                // "caught up" by checking remaining capacity:
                // capacity == OUTBOUND_QUEUE_CAPACITY - 1
                // immediately after a successful send means
                // the queue is essentially empty.
                if dropped_count > 0 && out_tx.capacity() >= OUTBOUND_QUEUE_CAPACITY - 1 {
                    emit_dropped(&out_tx, &mut dropped_count).await;
                }
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                dropped_count = dropped_count.saturating_add(1);
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                // Writer has gone away; nothing more to do.
                return;
            }
        }
    }
}

/// Map a published vdagent state to an `agent_connected` wire event.
///
/// The main channel publishes on every report from the server, and
/// re-announces the same state at session start; the protocol commits
/// to delivering only transitions, so a value equal to the last one
/// this client saw is suppressed.  The last value is updated whether
/// or not the client is subscribed, so subscribing later does not
/// replay an old transition.
fn translate_agent_connected(
    connected: bool,
    state: &ClientState,
    last_agent_connected: &mut bool,
) -> Option<Event> {
    if *last_agent_connected == connected {
        return None;
    }
    *last_agent_connected = connected;
    if !state.is_subscribed("agent_connected") {
        return None;
    }
    Some(Event {
        event: "agent_connected".into(),
        data: serde_json::json!({ "connected": connected }),
    })
}

/// Map a guest copy, given by its length in bytes, to a
/// `clipboard_changed` wire event.
///
/// Every copy is reported, including one of the same text as the last:
/// the guest copied again, and a client waiting on a copy should hear
/// of it. The text is not carried; see [`ClipboardChanged`].
fn translate_clipboard_changed(bytes: Option<usize>, state: &ClientState) -> Option<Event> {
    if !state.is_subscribed("clipboard_changed") {
        return None;
    }
    // The watch starts at `None`; a copy is always `Some`.
    let bytes = bytes?;
    Some(Event {
        event: "clipboard_changed".into(),
        data: serde_json::to_value(ClipboardChanged {
            bytes: bytes as u64,
        })
        .unwrap_or(serde_json::Value::Object(Default::default())),
    })
}

/// Emit a `dropped` event with the accumulated count and reset the
/// counter.  Saturates the on-wire `count` field at `u32::MAX`.
async fn emit_dropped(out_tx: &mpsc::Sender<OutboundMessage>, dropped_count: &mut u64) {
    let count: u32 = (*dropped_count).min(u32::MAX as u64) as u32;
    let event = Event {
        event: "dropped".into(),
        data: serde_json::json!({ "count": count }),
    };
    // Use a non-blocking send for the dropped event too — if the
    // queue is full again, treat the dropped-event itself as
    // accumulating: leave the counter intact for a future attempt.
    if out_tx.try_send(OutboundMessage::Event(event)).is_ok() {
        *dropped_count = 0;
    }
}

/// Map a `ChannelEvent` to a wire `Event` if the client is currently
/// subscribed to a matching event name.
///
/// Returns `None` for `ChannelEvent` variants that do not correspond
/// to a v1 event, or for events the client has not subscribed to.
/// `agent_connected` is not here: see `translate_agent_connected`.
fn translate_event(event: &ChannelEvent, state: &ClientState) -> Option<Event> {
    match event {
        ChannelEvent::Latency { sample_ms } => {
            if !state.is_subscribed("latency") {
                return None;
            }
            // Wallclock timestamp is captured at translation time.
            // Wallclock can jump; this is "client-observable
            // timestamp", not a monotonic stamp.  Documented as
            // such in the protocol doc.
            let wallclock_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_micros() as u64)
                .unwrap_or(0);
            Some(Event {
                event: "latency".into(),
                data: serde_json::json!({
                    "sample_ms": *sample_ms as f64,
                    "wallclock_us": wallclock_us,
                }),
            })
        }
        ChannelEvent::PasteCompleted {
            chars,
            request_id: Some(request_id),
            ..
        } => {
            // Remove the in-flight entry regardless of whether the
            // client subscribed — the paste is done.
            state.complete_paste(request_id);

            if !state.is_subscribed("paste_completed") {
                return None;
            }
            // `paste_completed` carries `chars_sent` (u32 in the
            // protocol).  `chars` is `usize`; saturate at u32::MAX
            // to avoid wrapping on absurd inputs.
            let chars_sent = (*chars).min(u32::MAX as usize) as u32;
            Some(Event {
                event: "paste_completed".into(),
                data: serde_json::json!({
                    "request_id": request_id,
                    "chars_sent": chars_sent,
                }),
            })
        }
        ChannelEvent::PasteCompleted { .. } => {
            // CLI-initiated paste (request_id == None): no control-
            // socket event; already logged by the headless stats drain.
            None
        }
        ChannelEvent::PasteFailed {
            reason,
            request_id: Some(request_id),
        } => {
            state.complete_paste(request_id);

            if !state.is_subscribed("paste_failed") {
                return None;
            }
            Some(Event {
                event: "paste_failed".into(),
                data: serde_json::json!({
                    "request_id": request_id,
                    "reason": reason,
                }),
            })
        }
        ChannelEvent::PasteFailed { .. } => {
            // CLI-initiated paste: no control-socket event.
            None
        }

        // ── v1.1 surface_drawn ─────────────────────────────────
        //
        // Six display-event variants all map to a single
        // `surface_drawn` wire event.  The renderer's draw events
        // each carry a `produced_at_secs` monotonic timestamp;
        // wallclock is captured here at translation time, the
        // same pattern the `latency` arm uses, so the cross-
        // process consumer (the kerbside loadtest orchestrator)
        // can compute keypress-to-screen latency against its own
        // wallclock keypress record.
        ChannelEvent::ImageReady {
            display_channel_id,
            surface_id,
            produced_at_secs,
            ..
        }
        | ChannelEvent::ImageReadyChroma {
            display_channel_id,
            surface_id,
            produced_at_secs,
            ..
        }
        | ChannelEvent::ImageReadyAlpha {
            display_channel_id,
            surface_id,
            produced_at_secs,
            ..
        }
        | ChannelEvent::FillRect {
            display_channel_id,
            surface_id,
            produced_at_secs,
            ..
        }
        | ChannelEvent::CopyBits {
            display_channel_id,
            surface_id,
            produced_at_secs,
            ..
        }
        | ChannelEvent::Invert {
            display_channel_id,
            surface_id,
            produced_at_secs,
            ..
        } => {
            if !state.is_subscribed("surface_drawn") {
                return None;
            }
            let wallclock_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_micros() as u64)
                .unwrap_or(0);
            Some(Event {
                event: "surface_drawn".into(),
                data: serde_json::json!({
                    "display_channel_id": *display_channel_id,
                    "surface_id": *surface_id,
                    "produced_at_secs": *produced_at_secs,
                    "wallclock_us": wallclock_us,
                }),
            })
        }

        // ── v1.1 digest_updated (digest-decode feature) ────────
        #[cfg(feature = "digest-decode")]
        ChannelEvent::DigestUpdated {
            frame_counter,
            framebuffer_hash,
            events,
        } => {
            if !state.is_subscribed("digest_updated") {
                return None;
            }
            let wallclock_us = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_micros() as u64)
                .unwrap_or(0);
            Some(Event {
                event: "digest_updated".into(),
                data: serde_json::json!({
                    "frame_counter": *frame_counter,
                    "framebuffer_hash": *framebuffer_hash,
                    "events": events,
                    "wallclock_us": wallclock_us,
                }),
            })
        }

        _ => None,
    }
}

/// Dispatch one request line and return the appropriate `Response`
/// along with a flag indicating whether the connection should be
/// closed after writing the response (currently set only for the
/// `protocol_version_mismatch` error path).
///
/// `handles` is what the verb handlers act on, borrowed for the
/// lifetime of the client connection.
async fn dispatch_request(
    line: &str,
    helloed: &mut bool,
    status_provider: &dyn StatusProvider,
    client_state: &ClientState,
    handles: &SessionHandles,
) -> (Response, bool) {
    let request: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            warn!("control: failed to parse request: {}", e);
            return (
                Response {
                    id: None,
                    ok: false,
                    result: None,
                    error: Some(RpcError {
                        code: ErrorCode::BadParams,
                        message: format!("failed to parse request: {}", e),
                    }),
                },
                false,
            );
        }
    };

    let id = request.id.clone();
    let method = request.method.as_str();

    // Hello must precede all other verbs.
    if !*helloed && method != "hello" {
        return (
            Response::err(id, ErrorCode::NoHelloYet, "first request must be hello"),
            false,
        );
    }

    match method {
        "hello" => {
            let response = handle_hello(id, request.params, helloed);
            let close = response
                .error
                .as_ref()
                .map(|e| e.code == ErrorCode::ProtocolVersionMismatch)
                .unwrap_or(false);
            (response, close)
        }
        "status" => (handle_status(id, status_provider), false),
        "subscribe" => (handle_subscribe(id, request.params, client_state), false),
        "unsubscribe" => (handle_unsubscribe(id, request.params, client_state), false),
        "send_key" => (
            handle_send_key(id, request.params, &handles.input_tx),
            false,
        ),
        "paste" => (
            handle_paste(id, request.params, client_state, &handles.input_tx),
            false,
        ),
        "screenshot" => (
            handle_screenshot(id, request.params, &handles.surface_mirror).await,
            false,
        ),
        "clipboard_set" => (
            handle_clipboard_set(id, request.params, &handles.clipboard),
            false,
        ),
        "clipboard_get" => (handle_clipboard_get(id, &handles.clipboard), false),
        "resize" => (handle_resize(id, request.params, &handles.resize_tx), false),
        // Completely unknown:
        _ => (
            Response::err(
                id,
                ErrorCode::UnknownMethod,
                format!("unknown method \"{}\"", method),
            ),
            false,
        ),
    }
}

fn handle_hello(id: RequestId, params: serde_json::Value, helloed: &mut bool) -> Response {
    let p: HelloParams = match serde_json::from_value(params) {
        Ok(v) => v,
        Err(e) => {
            return Response::err(
                id,
                ErrorCode::BadParams,
                format!("invalid hello params: {}", e),
            );
        }
    };

    match major_version_matches(&p.protocol_version) {
        Ok(true) => {
            *helloed = true;
            info!(
                "control: hello from {:?} (protocol {})",
                p.client_name, p.protocol_version
            );
            let result = HelloResult {
                server_name: "ryll".into(),
                protocol_version: PROTOCOL_VERSION.into(),
                supported_methods: SUPPORTED_METHODS.iter().map(|s| s.to_string()).collect(),
                supported_events: super::protocol::supported_events(),
            };
            Response::ok(
                id,
                serde_json::to_value(result)
                    .unwrap_or(serde_json::Value::Object(Default::default())),
            )
        }
        Ok(false) => {
            let client_major = super::protocol::parse_protocol_version(&p.protocol_version)
                .map(|(maj, _)| maj)
                .unwrap_or(0);
            let server_major = super::protocol::parse_protocol_version(PROTOCOL_VERSION)
                .map(|(maj, _)| maj)
                .unwrap_or(1);
            Response::err(
                id,
                ErrorCode::ProtocolVersionMismatch,
                format!(
                    "server speaks major version {}; client requested major version {}",
                    server_major, client_major
                ),
            )
        }
        Err(e) => Response::err(
            id,
            ErrorCode::BadParams,
            format!("invalid protocol_version field: {}", e),
        ),
    }
}

fn handle_status(id: RequestId, status_provider: &dyn StatusProvider) -> Response {
    let snapshot = status_provider.snapshot();
    Response::ok(
        id,
        serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Object(Default::default())),
    )
}

fn handle_subscribe(id: RequestId, params: serde_json::Value, state: &ClientState) -> Response {
    let p: SubscribeParams = match serde_json::from_value(params) {
        Ok(v) => v,
        Err(e) => {
            return Response::err(
                id,
                ErrorCode::BadParams,
                format!("invalid subscribe params: {}", e),
            );
        }
    };

    // Filter requested names against the supported set; unknown
    // names are silently ignored for forward compatibility.
    // Use the feature-aware `supported_events()` so subscribers
    // can ask for `digest_updated` when the server was built with
    // `--features digest-decode`.
    let supported = super::protocol::supported_events();
    let mut subscribed: Vec<String> = Vec::new();
    if let Ok(mut subs) = state.subscriptions.lock() {
        for name in &p.events {
            if supported.iter().any(|s| s == name) {
                subs.insert(name.clone());
                subscribed.push(name.clone());
            }
        }
    }

    Response::ok(
        id,
        serde_json::to_value(SubscribeResult { subscribed })
            .unwrap_or(serde_json::Value::Object(Default::default())),
    )
}

fn handle_unsubscribe(id: RequestId, params: serde_json::Value, state: &ClientState) -> Response {
    let p: SubscribeParams = match serde_json::from_value(params) {
        Ok(v) => v,
        Err(e) => {
            return Response::err(
                id,
                ErrorCode::BadParams,
                format!("invalid unsubscribe params: {}", e),
            );
        }
    };

    let mut unsubscribed: Vec<String> = Vec::new();
    if let Ok(mut subs) = state.subscriptions.lock() {
        for name in &p.events {
            if subs.remove(name) {
                unsubscribed.push(name.clone());
            }
        }
    }

    Response::ok(
        id,
        serde_json::to_value(UnsubscribeResult { unsubscribed })
            .unwrap_or(serde_json::Value::Object(Default::default())),
    )
}

// ── Params structs for the new verbs ─────────────────────────────

/// Params for the `send_key` verb.
#[derive(Debug, Deserialize)]
struct SendKeyParams {
    scancode: u16,
    state: String,
}

/// Params for the `paste` verb.
#[derive(Debug, Deserialize)]
struct PasteParams {
    text: String,
    /// Milliseconds to wait between characters.  Falls back to the
    /// inputs channel's built-in default (10 ms) when absent.
    char_delay_ms: Option<u32>,
}

// ── Verb handlers for the new verbs ──────────────────────────────

/// Handle a `send_key` request.
///
/// The `scancode` param is the *logical* AT set-1 code, which is not
/// the wire form; [`make_scancode`](crate::make_scancode) converts it
/// and owns that conversion for every input producer in the tree.
///
/// Translates the `state` field into one or two `InputEvent`s:
/// - `"down"` → `InputEvent::KeyDown(make_scancode(scancode, false))`.
/// - `"up"` → `InputEvent::KeyUp(make_scancode(scancode, true))`,
///   which sets the AT set-1 break bit.  The caller may supply either
///   the make code or a logical code that already carries the bit.
/// - `"press"` → the `"down"` event followed immediately by the same
///   `InputEvent::KeyUp` as `"up"` (two separate sends).
///
/// Returns `{}` on success.  The two sends for `"press"` are
/// non-atomic from the inputs channel's perspective, but they arrive
/// in order because the mpsc preserves insertion order.
///
/// Does NOT emit any event.  Does NOT require the vdagent.
fn handle_send_key(
    id: RequestId,
    params: serde_json::Value,
    input_tx: &mpsc::Sender<InputEvent>,
) -> Response {
    let p: SendKeyParams = match serde_json::from_value(params) {
        Ok(v) => v,
        Err(e) => {
            return Response::err(
                id,
                ErrorCode::BadParams,
                format!("invalid send_key params: {}", e),
            );
        }
    };

    // Clients send the scancode in logical form, as the protocol
    // document specifies: a plain code like `0x1E`, or an extended one
    // with the 0xE0 prefix in the high byte, `0xE04B`.  Neither is what
    // SPICE wants on the wire, and this verb used to pass both through
    // unconverted:
    // a `u32` serialises little-endian, so `0xE04B` reached the guest
    // as `4B E0` with the prefix *second*, and `scancode | 0x80` put
    // the break bit on the prefix byte rather than the scancode.  Every
    // extended key was wrong in both directions.
    //
    // `make_scancode` is the single owner of that encoding for every
    // producer in the tree — GUI, web frontend, paste, and now here.
    // The web frontend shipped the identical pair of bugs by
    // reimplementing it; see its doc comment.
    //
    // A client that supplies a logical code with the break bit already
    // set for `"up"` still works, because `make_scancode` ORs the bit in
    // and OR-ing a set bit is a no-op.  kerbside's `press_key` relies on
    // that, and the protocol document guarantees it.
    let logical = p.scancode as u32;
    let press = crate::make_scancode(logical, false);
    let release = crate::make_scancode(logical, true);

    match p.state.as_str() {
        "down" => {
            if input_tx.try_send(InputEvent::KeyDown(press)).is_err() {
                return Response::err(id, ErrorCode::InternalError, "input channel closed or full");
            }
        }
        "up" => {
            if input_tx.try_send(InputEvent::KeyUp(release)).is_err() {
                return Response::err(id, ErrorCode::InternalError, "input channel closed or full");
            }
        }
        "press" => {
            // Send KeyDown then KeyUp.  If either fails, return an
            // error; the channel is likely closed.
            if input_tx.try_send(InputEvent::KeyDown(press)).is_err() {
                return Response::err(id, ErrorCode::InternalError, "input channel closed or full");
            }
            if input_tx.try_send(InputEvent::KeyUp(release)).is_err() {
                return Response::err(
                    id,
                    ErrorCode::InternalError,
                    "input channel closed or full (after key-down)",
                );
            }
        }
        other => {
            return Response::err(
                id,
                ErrorCode::BadState,
                format!(
                    "unrecognised state value {:?}: expected \"down\", \"up\", or \"press\"",
                    other
                ),
            );
        }
    }

    Response::ok(id, serde_json::Value::Object(Default::default()))
}

/// Handle a `paste` request.
///
/// The response is returned immediately (the paste queues
/// asynchronously).  Completion is reported via a `paste_completed`
/// or `paste_failed` event on the broadcast bus, which the
/// translation layer delivers to subscribed clients.
///
/// Per-request cancellation: a fresh `CancellationToken` is minted
/// for each paste and stored in `ClientState::in_flight_pastes` keyed
/// by request id.  When the client disconnects, `cancel_all_pastes`
/// fires every token so the inputs channel stops generating synthetic
/// key events.
///
/// Empty text is a trivial success: the inputs channel emits
/// `PasteCompleted { chars: 0 }` immediately (same as the existing
/// `--paste-text ""` behaviour), so we just queue the event and
/// return `{}`.
///
/// `agent_not_connected` is intentionally NOT returned: paste in Ryll
/// works by translating text to US-QWERTY scancodes and issuing
/// `KeyDown`/`KeyUp` events through the SPICE inputs channel — it
/// does NOT use the vdagent clipboard path.  No verb returns
/// `agent_not_connected` today: the clipboard verbs, which do use the
/// agent, hold their request until it connects instead.  The code
/// stays in the enum because the protocol document defines it.
/// See `InputsChannel::handle_input_event` at the `PasteText` arm in
/// `channels/inputs.rs` for confirmation that no agent is involved.
fn handle_paste(
    id: RequestId,
    params: serde_json::Value,
    client_state: &ClientState,
    input_tx: &mpsc::Sender<InputEvent>,
) -> Response {
    let p: PasteParams = match serde_json::from_value(params) {
        Ok(v) => v,
        Err(e) => {
            return Response::err(
                id,
                ErrorCode::BadParams,
                format!("invalid paste params: {}", e),
            );
        }
    };

    // Default char delay matches the inputs channel's existing
    // built-in default used by the --paste-text CLI path.
    const DEFAULT_CHAR_DELAY_MS: u32 = 10;
    let char_delay_ms = p.char_delay_ms.unwrap_or(DEFAULT_CHAR_DELAY_MS);

    // Register a cancellation token for this paste so we can abort
    // it if the client disconnects mid-type.
    let cancel_token = client_state.register_paste(id.clone());

    let event = InputEvent::PasteText {
        text: p.text,
        char_delay_ms,
        request_id: Some(id.clone()),
        cancel: Some(cancel_token),
    };

    // `try_send` is sufficient: the input channel is sized at 256
    // (INPUT_CHANNEL_SIZE) and a single paste message is tiny.
    // If the channel is full or closed, return an internal error.
    if input_tx.try_send(event).is_err() {
        // Remove the registration we just added — the paste never started.
        client_state.complete_paste(&id);
        return Response::err(
            id,
            ErrorCode::InternalError,
            "input channel closed or full; paste not queued",
        );
    }

    Response::ok(id, serde_json::Value::Object(Default::default()))
}

// ── Screenshot verb ──────────────────────────────────────────────

/// Params for the `screenshot` verb.  Both fields are optional and
/// fall back to documented defaults (`surface_id` → primary surface,
/// `format` → `"png"`).
#[derive(Debug, Deserialize)]
struct ScreenshotParams {
    #[serde(default)]
    surface_id: Option<u32>,
    #[serde(default)]
    format: Option<String>,
}

/// Result payload for `screenshot`.  Serialised inline so the handler
/// returns a `serde_json::Value` without a separate result struct.
const SCREENSHOT_FORMAT_PNG: &str = "png";
const SCREENSHOT_FORMAT_RGBA: &str = "rgba";

/// Handle a `screenshot` request.
///
/// Locks the surface mirror just long enough to find the requested
/// surface and clone its pixel buffer + dimensions, drops the lock,
/// and (for PNG) hands the encode off to `tokio::task::spawn_blocking`
/// so the per-client task is not stalled and the tokio runtime is not
/// blocked while the `image` crate compresses several megabytes.
///
/// Primary-surface selection uses [`SurfaceMirror::primary_key`] when
/// the caller omits `surface_id` — the same convention `--web` mode
/// already uses to pick a "default" surface.  When `surface_id` is
/// supplied, the primary's `channel_id` is used as the channel; if
/// that combination is not present, returns `no_such_surface`.
///
/// Pixel-order note: the renderer's `DisplaySurface` stores pixels in
/// RGBA order (see the `// RGBA format` comment in
/// `display/surface.rs`), the same order `image::RgbaImage` expects
/// and the same order the protocol commits to for the `"rgba"` format.
/// No BGRA→RGBA conversion is required.
async fn handle_screenshot(
    id: RequestId,
    params: serde_json::Value,
    surface_mirror: &Arc<tokio::sync::Mutex<SurfaceMirror>>,
) -> Response {
    let p: ScreenshotParams = match serde_json::from_value(params) {
        Ok(v) => v,
        Err(e) => {
            return Response::err(
                id,
                ErrorCode::BadParams,
                format!("invalid screenshot params: {}", e),
            );
        }
    };

    // Validate format before locking the mirror so a bad request
    // fails fast.  Default is "png" per the protocol doc.
    let format = p
        .format
        .as_deref()
        .unwrap_or(SCREENSHOT_FORMAT_PNG)
        .to_string();
    if format != SCREENSHOT_FORMAT_PNG && format != SCREENSHOT_FORMAT_RGBA {
        return Response::err(
            id,
            ErrorCode::UnsupportedFormat,
            format!(
                "unsupported screenshot format {:?}: expected \"png\" or \"rgba\"",
                format
            ),
        );
    }

    // Lock the mirror briefly: locate the target surface, clone its
    // pixel buffer + dimensions, then drop the lock before any
    // CPU-bound encode work.  The clone is the only allocation while
    // holding the lock, so contention with the apply task is bounded
    // by memcpy speed.
    let (pixels, width, height) = {
        let mirror = surface_mirror.lock().await;
        let key = match p.surface_id {
            None => match mirror.primary_key() {
                Some(k) => k,
                None => {
                    return Response::err(
                        id,
                        ErrorCode::NoSuchSurface,
                        "no surfaces present in mirror; SPICE session may still be initialising",
                    );
                }
            },
            Some(sid) => {
                // Use the primary surface's channel_id as the channel
                // for an explicit surface_id request.  Falls back to
                // (0, sid) when no primary exists yet.
                let channel_id = mirror.primary_key().map(|(c, _)| c).unwrap_or(0);
                (channel_id, sid)
            }
        };
        match mirror.surfaces.get(&key) {
            Some(surf) => {
                let (w, h) = surf.size();
                (surf.pixels().to_vec(), w, h)
            }
            None => {
                return Response::err(
                    id,
                    ErrorCode::NoSuchSurface,
                    format!("no surface with channel_id={}, surface_id={}", key.0, key.1),
                );
            }
        }
    };

    // Encode + base64 according to the requested format.
    let encoded: Vec<u8> = if format == SCREENSHOT_FORMAT_RGBA {
        pixels
    } else {
        // PNG encode on a blocking task: at 1024×768 this is ~5–10 ms
        // of pure CPU; running it on the tokio runtime would stall
        // every other task on the same worker.
        let w = width;
        let h = height;
        let result = tokio::task::spawn_blocking(move || {
            let img = match image::RgbaImage::from_raw(w, h, pixels) {
                Some(img) => img,
                None => {
                    return Err(format!(
                        "RgbaImage::from_raw failed for {}x{} ({} bytes)",
                        w,
                        h,
                        (w as usize).saturating_mul(h as usize).saturating_mul(4)
                    ));
                }
            };
            let mut buf: Vec<u8> = Vec::new();
            let encoder = image::codecs::png::PngEncoder::new(&mut buf);
            use image::ImageEncoder;
            encoder
                .write_image(img.as_raw(), w, h, image::ExtendedColorType::Rgba8)
                .map_err(|e| format!("PNG encode failed: {}", e))?;
            Ok(buf)
        })
        .await;
        match result {
            Ok(Ok(buf)) => buf,
            Ok(Err(msg)) => {
                return Response::err(id, ErrorCode::InternalError, msg);
            }
            Err(e) => {
                return Response::err(
                    id,
                    ErrorCode::InternalError,
                    format!("PNG encode task panicked: {}", e),
                );
            }
        }
    };

    let data_base64 = base64::engine::general_purpose::STANDARD.encode(&encoded);

    debug!(
        "control: screenshot {}x{} format={} encoded_bytes={} base64_bytes={}",
        width,
        height,
        format,
        encoded.len(),
        data_base64.len()
    );

    Response::ok(
        id,
        serde_json::json!({
            "width": width,
            "height": height,
            "format": format,
            "data_base64": data_base64,
        }),
    )
}

// ── Clipboard and resize verbs (v1.3) ────────────────────────────

/// The longest text `clipboard_set` accepts, in bytes: 100 MiB,
/// spice-gtk's default `max-clipboard`. It is the clipboard size the
/// agent-message budget in the protocol crate is built from (that
/// budget adds 4 KiB for the clipboard message's headers), so a client
/// can never set more than ryll itself would accept from a guest, or
/// than spice-gtk would send.
pub const MAX_CLIPBOARD_SET_BYTES: usize = MAX_AGENT_MESSAGE_SIZE - 4096;

/// Handle a `clipboard_set` request.
///
/// Stores the text as the host side of the clipboard. The main
/// channel polls it every 500 ms and grabs the guest's clipboard when
/// it changes, so the guest can paste it shortly after; nothing is
/// sent from here. The text is held whether or not the agent is
/// connected, and the grab follows once the agent and its
/// capabilities are in, so this never fails with
/// `agent_not_connected`.
fn handle_clipboard_set(
    id: RequestId,
    params: serde_json::Value,
    clipboard: &MemoryClipboard,
) -> Response {
    let p: ClipboardSetParams = match serde_json::from_value(params) {
        Ok(v) => v,
        Err(e) => {
            return Response::err(
                id,
                ErrorCode::BadParams,
                format!("invalid clipboard_set params: {}", e),
            );
        }
    };
    if p.text.len() > MAX_CLIPBOARD_SET_BYTES {
        return Response::err(
            id,
            ErrorCode::BadParams,
            format!(
                "clipboard text is {} bytes; the most is {}",
                p.text.len(),
                MAX_CLIPBOARD_SET_BYTES
            ),
        );
    }
    // Byte count only: clipboard text may be a password.
    debug!("control: clipboard_set ({} bytes)", p.text.len());
    clipboard.set_host_text(p.text);
    Response::ok(id, serde_json::Value::Object(Default::default()))
}

/// Handle a `clipboard_get` request: the clipboard's text, from
/// whichever side wrote it last, or `null`. Takes no params.
fn handle_clipboard_get(id: RequestId, clipboard: &MemoryClipboard) -> Response {
    Response::ok(
        id,
        serde_json::to_value(ClipboardText {
            text: clipboard.text(),
        })
        .unwrap_or(serde_json::Value::Object(Default::default())),
    )
}

/// Validate a `resize` request and round it as the GUI would.
///
/// Each side must be 1 to `MAX_SURFACE_DIMENSION`: zero is not a
/// display, and ryll clamps any surface larger than that, so it could
/// not show what it asked for. Within that range each side is rounded
/// down to the 8-pixel grid with a floor of 8, by the same
/// [`align_monitor_dimension`] the GUI's window-driven resize uses.
fn validate_resize(p: ResizeParams) -> Result<ResizeParams, String> {
    for (name, value) in [("width", p.width), ("height", p.height)] {
        if value == 0 || value > MAX_SURFACE_DIMENSION {
            return Err(format!(
                "{} {} is out of range: expected 1 to {}",
                name, value, MAX_SURFACE_DIMENSION
            ));
        }
    }
    Ok(ResizeParams {
        width: align_monitor_dimension(p.width),
        height: align_monitor_dimension(p.height),
    })
}

/// Handle a `resize` request.
///
/// Queues the size on the main channel's display-size queue, as the
/// GUI does when its window is resized, and returns the size actually
/// queued. The main channel debounces it for 200 ms and then sends a
/// `VD_AGENT_MONITORS_CONFIG`, or, with no agent connected yet, holds
/// it and sends it once the agent connects; so this never fails with
/// `agent_not_connected`. A size equal to the last one sent is not
/// sent again.
fn handle_resize(
    id: RequestId,
    params: serde_json::Value,
    resize_tx: &mpsc::Sender<DisplaySize>,
) -> Response {
    let p: ResizeParams = match serde_json::from_value(params) {
        Ok(v) => v,
        Err(e) => {
            return Response::err(
                id,
                ErrorCode::BadParams,
                format!("invalid resize params: {}", e),
            );
        }
    };
    let size = match validate_resize(p) {
        Ok(size) => size,
        Err(message) => return Response::err(id, ErrorCode::BadParams, message),
    };
    match resize_tx.try_send(DisplaySize::Requested(size.width, size.height)) {
        Ok(()) => {}
        Err(mpsc::error::TrySendError::Full(_)) => {
            return Response::err(
                id,
                ErrorCode::QueueFull,
                "resize queue is full; retry shortly",
            );
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            return Response::err(
                id,
                ErrorCode::InternalError,
                "resize queue is closed: the session has ended",
            );
        }
    }
    debug!("control: resize to {}x{}", size.width, size.height);
    Response::ok(
        id,
        serde_json::to_value(size).unwrap_or(serde_json::Value::Object(Default::default())),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resize_rounds_down_to_the_gui_grid() {
        let check = |width, height| validate_resize(ResizeParams { width, height });
        assert_eq!(
            check(1024, 768),
            Ok(ResizeParams {
                width: 1024,
                height: 768
            })
        );
        assert_eq!(
            check(1366, 770),
            Ok(ResizeParams {
                width: 1360,
                height: 768
            })
        );
        // Below the grid's first step, the floor.
        assert_eq!(
            check(1, 7),
            Ok(ResizeParams {
                width: 8,
                height: 8
            })
        );
        assert_eq!(
            check(MAX_SURFACE_DIMENSION, MAX_SURFACE_DIMENSION),
            Ok(ResizeParams {
                width: MAX_SURFACE_DIMENSION,
                height: MAX_SURFACE_DIMENSION
            })
        );
    }

    #[test]
    fn clipboard_set_takes_up_to_the_cap_and_refuses_more() {
        assert_eq!(MAX_CLIPBOARD_SET_BYTES, 100 * 1024 * 1024);
        let clipboard = MemoryClipboard::new();

        let at_cap = "a".repeat(MAX_CLIPBOARD_SET_BYTES);
        let r = handle_clipboard_set(
            RequestId::Int(1),
            serde_json::json!({ "text": at_cap }),
            &clipboard,
        );
        assert!(r.ok);
        assert_eq!(
            clipboard.text().map(|t| t.len()),
            Some(MAX_CLIPBOARD_SET_BYTES)
        );

        clipboard.set_host_text("kept".to_string());
        let over = "a".repeat(MAX_CLIPBOARD_SET_BYTES + 1);
        let r = handle_clipboard_set(
            RequestId::Int(2),
            serde_json::json!({ "text": over }),
            &clipboard,
        );
        assert!(!r.ok);
        assert_eq!(r.error.map(|e| e.code), Some(ErrorCode::BadParams));
        assert_eq!(clipboard.text().as_deref(), Some("kept"));
    }

    #[test]
    fn resize_refuses_zero_and_oversized_sides() {
        let check = |width, height| validate_resize(ResizeParams { width, height });
        assert!(check(0, 768).is_err());
        assert!(check(1024, 0).is_err());
        assert!(check(MAX_SURFACE_DIMENSION + 1, 768).is_err());
        assert!(check(1024, MAX_SURFACE_DIMENSION + 1).is_err());
    }

    /// The mode must already be `0600` while the socket cannot yet be
    /// connected to, so there is no moment at which anyone else can
    /// connect.
    #[test]
    fn socket_is_owner_only_before_it_listens() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("control.sock");

        let (socket, socket_file) = bind_owner_only(path.clone()).expect("bind");
        let mode = std::fs::metadata(&path)
            .expect("socket file")
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let err = std::os::unix::net::UnixStream::connect(&path)
            .expect_err("a socket that is not listening must refuse connects");
        assert_eq!(err.kind(), std::io::ErrorKind::ConnectionRefused);

        socket.listen(LISTEN_BACKLOG).expect("listen");
        std::os::unix::net::UnixStream::connect(&path).expect("connect once listening");

        drop(socket_file);
        assert!(!path.exists());
    }
}
