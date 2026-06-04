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
//! 1. `Server::run` is spawned as a tokio task from `run_headless`.
//! 2. The server deletes any stale socket file, binds a new socket,
//!    and `fchmod`s it to `0600`.
//! 3. Accepts connections.  While a client is connected, additional
//!    accept attempts receive a `busy` response and are closed.
//! 4. When the `CancellationToken` fires (session shutdown), the
//!    accept loop exits, the socket file is unlinked, and the task
//!    returns.

use std::collections::{HashMap, HashSet};
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::protocol::{
    major_version_matches, ErrorCode, Event, HelloParams, HelloResult, Request, RequestId,
    Response, RpcError, StatusResult, SubscribeParams, SubscribeResult, UnsubscribeResult,
    PROTOCOL_VERSION, SUPPORTED_EVENTS, SUPPORTED_METHODS,
};
use crate::channels::{ChannelEvent, InputEvent};

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

// ── Server ────────────────────────────────────────────────────────

/// Handle to the control socket configuration. Cheap to clone; the
/// actual work happens in `run`.
pub struct Server {
    socket_path: PathBuf,
}

impl Server {
    pub fn new(socket_path: PathBuf) -> Self {
        Self { socket_path }
    }

    /// Bind the socket and run the accept loop until `shutdown` fires.
    ///
    /// `event_tx` is the broadcast bus that the headless event-loop
    /// fan-out publishes to.  Each accepted client subscribes a fresh
    /// `broadcast::Receiver` from this sender.
    ///
    /// `input_tx` is the mpsc sender used by `run_headless` to push
    /// `InputEvent`s to the SPICE inputs channel.  The control server
    /// clones it into each accepted client so `send_key` and `paste`
    /// verbs can enqueue events.
    pub async fn run(
        self,
        status_provider: Arc<dyn StatusProvider>,
        event_tx: broadcast::Sender<ChannelEvent>,
        input_tx: mpsc::Sender<InputEvent>,
        shutdown: CancellationToken,
    ) -> std::io::Result<()> {
        let path = &self.socket_path;

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

        let listener = UnixListener::bind(path)?;

        // Set file mode 0600: owner read/write only.
        let perms = std::fs::Permissions::from_mode(0o600);
        std::fs::set_permissions(path, perms)?;

        info!("control: listening on {}", path.display());

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
                                let tx = input_tx.clone();
                                tokio::spawn(async move {
                                    handle_client(stream, provider, event_rx, tx, cancel).await;
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

        // Unlink the socket file on clean shutdown.
        if let Err(e) = std::fs::remove_file(path) {
            warn!("control: failed to remove socket {}: {}", path.display(), e);
        } else {
            info!("control: removed socket {}", path.display());
        }

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
    input_tx: mpsc::Sender<InputEvent>,
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
    let event_handle = tokio::spawn(event_translator_task(event_rx, event_state, event_out_tx));

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
                            &input_tx,
                        );

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
/// result onto the outbound mpsc.
///
/// Backpressure rule: on `try_send` failure (queue full) or on a
/// broadcast `Lagged(n)` error, increment a local dropped counter.
/// When the next send succeeds AND the counter is non-zero, emit a
/// `dropped` event with the cumulative count and reset.
async fn event_translator_task(
    mut event_rx: broadcast::Receiver<ChannelEvent>,
    state: Arc<ClientState>,
    out_tx: mpsc::Sender<OutboundMessage>,
) {
    // Cumulative dropped-events counter, since the last `dropped`
    // event was emitted.  Saturates at `u32::MAX` on emit.
    let mut dropped_count: u64 = 0;

    // Track the previous agent-connected state so we emit only on
    // transitions, as the protocol doc commits to.  `None` means we
    // have not yet seen an `AgentConnected` event since the client
    // connected.
    let mut last_agent_connected: Option<bool> = None;

    loop {
        match event_rx.recv().await {
            Ok(event) => {
                // Translate the ChannelEvent into the wire shape.  Any
                // variant the protocol doesn't care about returns
                // `None` here and is silently discarded.
                let translated = translate_event(&event, &state, &mut last_agent_connected);
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
            Err(broadcast::error::RecvError::Lagged(n)) => {
                dropped_count = dropped_count.saturating_add(n);
            }
            Err(broadcast::error::RecvError::Closed) => {
                return;
            }
        }
    }
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
///
/// The `last_agent_connected` slot is used to suppress emitting the
/// `agent_connected` event for non-transitions (the SPICE main
/// channel can re-announce the same agent-connected state at
/// session-startup time; the protocol commits to delivering only
/// transitions).
fn translate_event(
    event: &ChannelEvent,
    state: &ClientState,
    last_agent_connected: &mut Option<bool>,
) -> Option<Event> {
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
        ChannelEvent::AgentConnected(connected) => {
            // Transitions-only: suppress if the new state equals the
            // last one we forwarded to this client.
            if last_agent_connected.as_ref() == Some(connected) {
                return None;
            }
            *last_agent_connected = Some(*connected);
            if !state.is_subscribed("agent_connected") {
                return None;
            }
            Some(Event {
                event: "agent_connected".into(),
                data: serde_json::json!({ "connected": *connected }),
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
        _ => None,
    }
}

/// Dispatch one request line and return the appropriate `Response`
/// along with a flag indicating whether the connection should be
/// closed after writing the response (currently set only for the
/// `protocol_version_mismatch` error path).
///
/// `input_tx` is forwarded to the verb handlers that need to push
/// `InputEvent`s (`send_key`, `paste`).  It is a borrow of the
/// sender that lives for the lifetime of the client connection.
fn dispatch_request(
    line: &str,
    helloed: &mut bool,
    status_provider: &dyn StatusProvider,
    client_state: &ClientState,
    input_tx: &mpsc::Sender<InputEvent>,
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
        "send_key" => (handle_send_key(id, request.params, input_tx), false),
        "paste" => (
            handle_paste(id, request.params, client_state, input_tx),
            false,
        ),
        // Recognised but not yet implemented:
        "screenshot" => (
            Response::err(
                id,
                ErrorCode::NotImplemented,
                "method \"screenshot\" is recognised but not yet implemented",
            ),
            false,
        ),
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
                supported_events: SUPPORTED_EVENTS.iter().map(|s| s.to_string()).collect(),
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

    // Filter requested names against the v1 supported set; unknown
    // names are silently ignored for forward compatibility.
    let mut subscribed: Vec<String> = Vec::new();
    if let Ok(mut subs) = state.subscriptions.lock() {
        for name in &p.events {
            if SUPPORTED_EVENTS.contains(&name.as_str()) {
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
/// Translates the `state` field into one or two `InputEvent`s:
/// - `"down"` → `InputEvent::KeyDown(scancode)`.
/// - `"up"` → `InputEvent::KeyUp(scancode)`.
/// - `"press"` → `InputEvent::KeyDown(scancode)` followed immediately
///   by `InputEvent::KeyUp(scancode)` (two separate sends).
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

    let scancode = p.scancode as u32;

    match p.state.as_str() {
        "down" => {
            if input_tx.try_send(InputEvent::KeyDown(scancode)).is_err() {
                return Response::err(id, ErrorCode::InternalError, "input channel closed or full");
            }
        }
        "up" => {
            if input_tx.try_send(InputEvent::KeyUp(scancode)).is_err() {
                return Response::err(id, ErrorCode::InternalError, "input channel closed or full");
            }
        }
        "press" => {
            // Send KeyDown then KeyUp.  If either fails, return an
            // error; the channel is likely closed.
            if input_tx.try_send(InputEvent::KeyDown(scancode)).is_err() {
                return Response::err(id, ErrorCode::InternalError, "input channel closed or full");
            }
            if input_tx.try_send(InputEvent::KeyUp(scancode)).is_err() {
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
/// does NOT use the vdagent clipboard path.  The `agent_not_connected`
/// error code exists in the enum for future clipboard-based verbs.
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
