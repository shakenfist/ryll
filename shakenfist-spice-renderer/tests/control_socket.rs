//! Integration tests for the control-socket server (v1 protocol).
//!
//! Tests exercise every v1 verb and event through the public API
//! of the `control` module.  No real SPICE session is spawned; instead,
//! `Server::run` is driven directly with in-process test doubles:
//!
//! - A `MockStatusProvider` that returns a configurable `StatusResult`.
//! - A `tokio::sync::broadcast::Sender<ChannelEvent>` that tests push
//!   synthetic events onto directly.
//! - A `tokio::sync::mpsc` whose receiver the test inspects to verify
//!   that `send_key` and `paste` produce the expected `InputEvent`s.
//! - A `SurfaceMirror` pre-populated via the `with_test_surface` helper
//!   that step 3f adds to `surface_mirror.rs`.
//!
//! Each test binds the server to a temporary Unix-socket path (via
//! `tempfile::TempDir`), connects a client, drives one or more request/
//! response cycles, and asserts wire-level invariants.
//!
//! ## Why direct `Server::run` instead of full headless?
//!
//! The control surface is the unit under test.  A full `run_headless`
//! call would bring the entire SPICE session stack (TLS, protocol
//! parsing, channel tasks) with it — unnecessary weight for protocol
//! tests and hard to drive in a repeatable way.  The broadcast sender,
//! input mpsc, and surface mirror are all lightweight to construct in-
//! process, so the isolation is clean.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::{broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use shakenfist_spice_renderer::channels::{ChannelEvent, InputEvent};
use shakenfist_spice_renderer::control::protocol::{RequestId, StatusResult, SurfaceInfo};
use shakenfist_spice_renderer::control::server::{Server, StatusProvider};
use shakenfist_spice_renderer::surface_mirror::SurfaceMirror;

// ── Mock status provider ──────────────────────────────────────────

/// Configurable status snapshot for tests.  The `StatusResult` is
/// supplied at construction time; `snapshot()` clones and returns it.
struct MockStatusProvider {
    result: std::sync::Mutex<StatusResult>,
}

impl MockStatusProvider {
    fn new(result: StatusResult) -> Arc<Self> {
        Arc::new(Self {
            result: std::sync::Mutex::new(result),
        })
    }

    fn set(&self, result: StatusResult) {
        if let Ok(mut r) = self.result.lock() {
            *r = result;
        }
    }
}

impl StatusProvider for MockStatusProvider {
    fn snapshot(&self) -> StatusResult {
        self.result
            .lock()
            .map(|guard| guard.clone())
            .unwrap_or_else(|_| StatusResult {
                spice_connected: false,
                agent_connected: false,
                surfaces: vec![],
            })
    }
}

// ── Test inputs ───────────────────────────────────────────────────

/// Handles returned to a test from `spawn_server`.
struct ServerHandle {
    /// Fires this to tell the server to stop accepting connections.
    shutdown: CancellationToken,
    /// JoinHandle for the server task.
    task: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl ServerHandle {
    /// Stop the server and await the task.
    async fn stop(self) {
        self.shutdown.cancel();
        let _ = self.task.await;
    }
}

/// Channels that give the test control over the server's I/O sides.
struct TestInputs {
    /// Push synthetic `ChannelEvent`s here to exercise the event
    /// translator / subscription machinery.
    pub event_tx: broadcast::Sender<ChannelEvent>,
    /// Read `InputEvent`s emitted by `send_key` / `paste` verbs here.
    pub input_rx: mpsc::Receiver<InputEvent>,
    /// The surface mirror the server reads for `screenshot` verbs.
    /// Held here so it is not dropped while the server is running.
    #[allow(dead_code)]
    pub mirror: Arc<tokio::sync::Mutex<SurfaceMirror>>,
    /// A ref to the status provider so tests can mutate it mid-run.
    pub status: Arc<MockStatusProvider>,
}

// ── Helpers ───────────────────────────────────────────────────────

/// Bind the server to a fresh temp-dir socket and spawn it as a
/// background task.  Returns the socket path, a `ServerHandle` for
/// stopping the server, and `TestInputs` for driving it.
async fn spawn_server(
    status: Arc<MockStatusProvider>,
    mirror: Arc<tokio::sync::Mutex<SurfaceMirror>>,
) -> (PathBuf, ServerHandle, TestInputs) {
    // Pick a unique socket path inside a temp dir.
    let dir = tempfile::tempdir().expect("tempdir");
    let socket_path = dir.path().join("control.sock");
    // Keep `dir` alive by leaking it — the OS will clean up the temp
    // dir when the process exits, which is fine for short-lived tests.
    std::mem::forget(dir);

    let (event_tx, _) = broadcast::channel::<ChannelEvent>(1024);
    let (input_tx, input_rx) = mpsc::channel::<InputEvent>(256);

    let shutdown = CancellationToken::new();
    let server = Server::new(socket_path.clone());
    let task = tokio::spawn(server.run(
        status.clone(),
        event_tx.clone(),
        input_tx,
        mirror.clone(),
        shutdown.clone(),
    ));

    // Give the server a moment to bind before the caller connects.
    tokio::time::sleep(Duration::from_millis(20)).await;

    (
        socket_path,
        ServerHandle { shutdown, task },
        TestInputs {
            event_tx,
            input_rx,
            mirror,
            status,
        },
    )
}

/// Open a client connection and split it into a buffered reader and
/// a write half.
async fn connect_client(path: &Path) -> (BufReader<OwnedReadHalf>, OwnedWriteHalf) {
    let stream = UnixStream::connect(path).await.expect("connect to server");
    let (r, w) = stream.into_split();
    (BufReader::new(r), w)
}

/// Serialise one NDJSON request and write it to the socket.
async fn send_request(
    write: &mut OwnedWriteHalf,
    id: i64,
    method: &str,
    params: serde_json::Value,
) {
    let mut line = serde_json::json!({
        "id": id,
        "method": method,
        "params": params,
    })
    .to_string();
    line.push('\n');
    write
        .write_all(line.as_bytes())
        .await
        .expect("write request");
}

/// Read one NDJSON line from the server with a 2-second timeout.
async fn recv_line(reader: &mut BufReader<OwnedReadHalf>) -> serde_json::Value {
    let mut buf = String::new();
    tokio::time::timeout(Duration::from_secs(2), reader.read_line(&mut buf))
        .await
        .expect("recv_line timed out (2 s)")
        .expect("recv_line I/O error");
    assert!(!buf.is_empty(), "server closed connection unexpectedly");
    serde_json::from_str(buf.trim()).expect("recv_line: invalid JSON")
}

/// Perform the hello handshake and return the server's hello result value.
async fn hello(
    reader: &mut BufReader<OwnedReadHalf>,
    writer: &mut OwnedWriteHalf,
) -> serde_json::Value {
    send_request(
        writer,
        1,
        "hello",
        serde_json::json!({
            "client_name": "test-client",
            "protocol_version": "1.0",
        }),
    )
    .await;
    let resp = recv_line(reader).await;
    assert_eq!(resp["ok"], true, "hello must succeed");
    resp["result"].clone()
}

// ── Default test inputs ───────────────────────────────────────────

fn default_status() -> StatusResult {
    StatusResult {
        spice_connected: true,
        agent_connected: false,
        surfaces: vec![],
    }
}

fn empty_mirror() -> Arc<tokio::sync::Mutex<SurfaceMirror>> {
    Arc::new(tokio::sync::Mutex::new(SurfaceMirror::new()))
}

// ── Tests ─────────────────────────────────────────────────────────

/// 1. hello returns protocol version, server name, and all v1 supported sets.
#[tokio::test]
async fn hello_returns_protocol_version_and_supported_sets() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, _inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;

    let result = hello(&mut r, &mut w).await;
    assert_eq!(result["protocol_version"], "1.0");
    assert_eq!(result["server_name"], "ryll");

    let methods: Vec<String> =
        serde_json::from_value(result["supported_methods"].clone()).expect("supported_methods");
    assert!(methods.contains(&"hello".into()));
    assert!(methods.contains(&"status".into()));
    assert!(methods.contains(&"send_key".into()));
    assert!(methods.contains(&"paste".into()));
    assert!(methods.contains(&"screenshot".into()));
    assert!(methods.contains(&"subscribe".into()));
    assert!(methods.contains(&"unsubscribe".into()));
    assert_eq!(methods.len(), 7, "expected exactly 7 v1 methods");

    let events: Vec<String> =
        serde_json::from_value(result["supported_events"].clone()).expect("supported_events");
    assert!(events.contains(&"latency".into()));
    assert!(events.contains(&"agent_connected".into()));
    assert!(events.contains(&"paste_completed".into()));
    assert!(events.contains(&"paste_failed".into()));
    assert!(events.contains(&"dropped".into()));
    assert_eq!(events.len(), 5, "expected exactly 5 v1 events");

    handle.stop().await;
}

/// 2. Sending any method before hello returns no_hello_yet; connection stays
///    open; hello then succeeds.
#[tokio::test]
async fn methods_before_hello_return_no_hello_yet() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, _inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;

    // Send status before hello — must get no_hello_yet.
    send_request(&mut w, 10, "status", serde_json::json!({})).await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"]["code"], "no_hello_yet");
    assert_eq!(resp["id"], 10);

    // Now send hello — must succeed.
    hello(&mut r, &mut w).await;

    // Now send status — must succeed.
    send_request(&mut w, 11, "status", serde_json::json!({})).await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], true, "status after hello must succeed");

    handle.stop().await;
}

/// 3. Hello with a mismatched major version returns protocol_version_mismatch
///    and the server closes the connection.
#[tokio::test]
async fn protocol_version_mismatch_rejects_and_closes() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, _inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;

    send_request(
        &mut w,
        1,
        "hello",
        serde_json::json!({
            "client_name": "bad-client",
            "protocol_version": "2.0",
        }),
    )
    .await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"]["code"], "protocol_version_mismatch");

    // The server should close; next read yields EOF.
    let mut buf = String::new();
    let n = tokio::time::timeout(Duration::from_secs(2), r.read_line(&mut buf))
        .await
        .expect("timeout waiting for EOF")
        .expect("I/O error waiting for EOF");
    assert_eq!(n, 0, "expected EOF after version mismatch, got: {:?}", buf);

    handle.stop().await;
}

/// 4. A second connection while client A is active gets a busy error and is
///    closed; client A is unaffected.
#[tokio::test]
async fn busy_response_on_second_connection() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, _inputs) = spawn_server(status, empty_mirror()).await;

    // Client A connects and completes hello.
    let (mut ra, mut wa) = connect_client(&path).await;
    hello(&mut ra, &mut wa).await;

    // Client B connects while A is active.
    let (mut rb, _wb) = connect_client(&path).await;
    let resp = recv_line(&mut rb).await;
    assert_eq!(resp["ok"], false, "second client must get ok:false");
    assert_eq!(resp["error"]["code"], "busy");
    assert!(
        resp.get("id").is_none(),
        "busy response must not have an id field"
    );

    // Server closes B; next read is EOF.
    let mut buf = String::new();
    let n = tokio::time::timeout(Duration::from_secs(2), rb.read_line(&mut buf))
        .await
        .expect("timeout waiting for EOF on client B")
        .expect("I/O error waiting for EOF on client B");
    assert_eq!(n, 0, "expected EOF for busy-rejected client B");

    // Client A still works.
    send_request(&mut wa, 2, "status", serde_json::json!({})).await;
    let resp = recv_line(&mut ra).await;
    assert_eq!(
        resp["ok"], true,
        "client A must still function after B is rejected"
    );

    handle.stop().await;
}

/// 5. Status reflects the live provider state; updating the provider
///    causes subsequent status calls to see the new values.
#[tokio::test]
async fn status_reflects_live_state() {
    let status = MockStatusProvider::new(StatusResult {
        spice_connected: true,
        agent_connected: false,
        surfaces: vec![SurfaceInfo {
            channel_id: 0,
            surface_id: 0,
            width: 320,
            height: 240,
        }],
    });

    let (path, handle, inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    send_request(&mut w, 2, "status", serde_json::json!({})).await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], true);
    let result = &resp["result"];
    assert_eq!(result["spice_connected"], true);
    assert_eq!(result["agent_connected"], false);
    let surfaces = result["surfaces"].as_array().expect("surfaces array");
    assert_eq!(surfaces.len(), 1);
    assert_eq!(surfaces[0]["width"], 320);
    assert_eq!(surfaces[0]["height"], 240);

    // Update the provider to reflect agent connecting.
    inputs.status.set(StatusResult {
        spice_connected: true,
        agent_connected: true,
        surfaces: vec![],
    });

    send_request(&mut w, 3, "status", serde_json::json!({})).await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["result"]["agent_connected"], true);

    handle.stop().await;
}

/// 6. send_key translates each state variant to the correct InputEvent(s).
#[tokio::test]
async fn send_key_translates_to_input_events() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, mut inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    // "down" → one KeyDown
    send_request(
        &mut w,
        2,
        "send_key",
        serde_json::json!({ "scancode": 0x1E, "state": "down" }),
    )
    .await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["result"], serde_json::json!({}));

    let ev = inputs.input_rx.recv().await.expect("KeyDown event");
    assert!(
        matches!(ev, InputEvent::KeyDown(sc) if sc == 0x1E_u32),
        "expected KeyDown(0x1E), got {:?}",
        ev
    );

    // "up" → one KeyUp
    send_request(
        &mut w,
        3,
        "send_key",
        serde_json::json!({ "scancode": 0x1E, "state": "up" }),
    )
    .await;
    let _resp = recv_line(&mut r).await;
    let ev = inputs.input_rx.recv().await.expect("KeyUp event");
    assert!(
        matches!(ev, InputEvent::KeyUp(sc) if sc == 0x1E_u32),
        "expected KeyUp(0x1E), got {:?}",
        ev
    );

    // "press" → KeyDown then KeyUp
    send_request(
        &mut w,
        4,
        "send_key",
        serde_json::json!({ "scancode": 0x1E, "state": "press" }),
    )
    .await;
    let _resp = recv_line(&mut r).await;
    let ev1 = inputs.input_rx.recv().await.expect("KeyDown for press");
    let ev2 = inputs.input_rx.recv().await.expect("KeyUp for press");
    assert!(matches!(ev1, InputEvent::KeyDown(sc) if sc == 0x1E_u32));
    assert!(matches!(ev2, InputEvent::KeyUp(sc) if sc == 0x1E_u32));

    // "sideways" → bad_state error; no InputEvent enqueued.
    send_request(
        &mut w,
        5,
        "send_key",
        serde_json::json!({ "scancode": 0x1E, "state": "sideways" }),
    )
    .await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"]["code"], "bad_state");
    // Channel should be empty (no event was pushed for bad_state).
    assert!(
        inputs.input_rx.try_recv().is_err(),
        "no InputEvent expected for bad_state"
    );

    handle.stop().await;
}

/// 7. paste returns immediately and emits paste_completed when the
///    ChannelEvent arrives on the broadcast bus.
#[tokio::test]
async fn paste_returns_immediately_and_emits_paste_completed() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    // Subscribe to paste_completed before pasting.
    send_request(
        &mut w,
        2,
        "subscribe",
        serde_json::json!({ "events": ["paste_completed"] }),
    )
    .await;
    let _sub_resp = recv_line(&mut r).await;

    // Send paste.
    send_request(&mut w, 42, "paste", serde_json::json!({ "text": "ab" })).await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], true);
    assert_eq!(resp["result"], serde_json::json!({}));

    // Inject the PasteCompleted event as if the inputs channel fired it.
    inputs
        .event_tx
        .send(ChannelEvent::PasteCompleted {
            chars: 2,
            elapsed_ms: 20,
            request_id: Some(RequestId::Int(42)),
        })
        .expect("broadcast send");

    // The next line from the server should be the paste_completed event.
    let ev = recv_line(&mut r).await;
    assert_eq!(ev["event"], "paste_completed");
    assert_eq!(ev["data"]["request_id"], 42);
    assert_eq!(ev["data"]["chars_sent"], 2);

    handle.stop().await;
}

/// 8. paste_failed emits the paste_failed event with reason and request_id.
#[tokio::test]
async fn paste_failed_emits_event() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    send_request(
        &mut w,
        2,
        "subscribe",
        serde_json::json!({ "events": ["paste_failed"] }),
    )
    .await;
    let _sub_resp = recv_line(&mut r).await;

    // Send a paste so the server registers the in-flight entry for id=7.
    send_request(&mut w, 7, "paste", serde_json::json!({ "text": "x" })).await;
    let _resp = recv_line(&mut r).await;

    // Inject a PasteFailed event.
    inputs
        .event_tx
        .send(ChannelEvent::PasteFailed {
            reason: "cancelled".into(),
            request_id: Some(RequestId::Int(7)),
        })
        .expect("broadcast send");

    let ev = recv_line(&mut r).await;
    assert_eq!(ev["event"], "paste_failed");
    assert_eq!(ev["data"]["request_id"], 7);
    assert_eq!(ev["data"]["reason"], "cancelled");

    handle.stop().await;
}

/// 9. subscribe returns only the known subset; unsubscribe returns only
///    names that were actually subscribed.
#[tokio::test]
async fn subscribe_returns_actual_subset() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, _inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    // "moose" is not a known event; it must be silently dropped.
    send_request(
        &mut w,
        2,
        "subscribe",
        serde_json::json!({ "events": ["latency", "moose", "agent_connected"] }),
    )
    .await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], true);
    let mut subscribed: Vec<String> =
        serde_json::from_value(resp["result"]["subscribed"].clone()).expect("subscribed array");
    subscribed.sort();
    assert_eq!(subscribed, vec!["agent_connected", "latency"]);

    // Unsubscribe agent_connected + a name we never subscribed to.
    send_request(
        &mut w,
        3,
        "unsubscribe",
        serde_json::json!({ "events": ["agent_connected", "nothing"] }),
    )
    .await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], true);
    let unsubscribed: Vec<String> =
        serde_json::from_value(resp["result"]["unsubscribed"].clone()).expect("unsubscribed array");
    assert_eq!(unsubscribed, vec!["agent_connected"]);

    handle.stop().await;
}

/// 10. A latency event is delivered when subscribed.
#[tokio::test]
async fn latency_event_delivered_when_subscribed() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    send_request(
        &mut w,
        2,
        "subscribe",
        serde_json::json!({ "events": ["latency"] }),
    )
    .await;
    let _sub_resp = recv_line(&mut r).await;

    inputs
        .event_tx
        .send(ChannelEvent::Latency { sample_ms: 4.2 })
        .expect("broadcast send");

    let ev = recv_line(&mut r).await;
    assert_eq!(ev["event"], "latency");
    let sample_ms = ev["data"]["sample_ms"].as_f64().expect("sample_ms");
    // Allow a small floating-point tolerance.
    assert!(
        (sample_ms - 4.2_f64).abs() < 0.01,
        "expected sample_ms ≈ 4.2, got {}",
        sample_ms
    );
    let wallclock_us = ev["data"]["wallclock_us"].as_u64().expect("wallclock_us");
    assert!(wallclock_us > 0, "wallclock_us must be positive");

    handle.stop().await;
}

/// 11. agent_connected events are emitted only on transitions.
#[tokio::test]
async fn agent_connected_event_only_on_transition() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    send_request(
        &mut w,
        2,
        "subscribe",
        serde_json::json!({ "events": ["agent_connected"] }),
    )
    .await;
    let _sub_resp = recv_line(&mut r).await;

    // First AgentConnected(true) → event delivered.
    inputs
        .event_tx
        .send(ChannelEvent::AgentConnected(true))
        .expect("broadcast send");
    let ev = recv_line(&mut r).await;
    assert_eq!(ev["event"], "agent_connected");
    assert_eq!(ev["data"]["connected"], true);

    // Second AgentConnected(true) → no event (same state, not a transition).
    // Inject another, then inject AgentConnected(false) to produce a
    // second observable event.  We use that to confirm there is no
    // intermediate event for the duplicate true.
    inputs
        .event_tx
        .send(ChannelEvent::AgentConnected(true))
        .expect("broadcast send (duplicate)");
    inputs
        .event_tx
        .send(ChannelEvent::AgentConnected(false))
        .expect("broadcast send (false)");

    let ev = recv_line(&mut r).await;
    assert_eq!(ev["event"], "agent_connected");
    assert_eq!(
        ev["data"]["connected"], false,
        "expected the false transition, not a second true"
    );

    handle.stop().await;
}

/// 12. Queue overflow triggers a `dropped` event with count > 0.
///
/// Strategy: we need the event translator's outbound queue (256 slots)
/// to overflow, which requires the client to not read while events pile
/// up.  In a single-threaded tokio test runtime, async tasks only run
/// when the current task yields, so we can:
///
///   1. Subscribe (yields at recv_line so the server processes it).
///   2. Flood 1500 events onto the broadcast channel without yielding —
///      this overfills the broadcast ring buffer (capacity 1024) so when
///      the translator next runs it will see `Lagged(n)` which
///      increments `dropped_count` by n.
///   3. Yield (via a sleep) to let the translator and writer tasks run.
///   4. Send one more event: the translator will try_send it and, if
///      dropped_count > 0 and the queue is near-empty, emit `dropped`.
///   5. Read from the client socket; we should eventually see a `dropped`
///      event.
///
/// This path exercises the `broadcast::error::RecvError::Lagged` arm in
/// `event_translator_task`, which is the primary drop path for high-rate
/// sources.
///
/// Note: the exact drop count is an implementation detail.  The test
/// only asserts count > 0.  A 5-second overall timeout guards against
/// CI hangs.
#[tokio::test]
async fn dropped_event_after_queue_overflow() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    send_request(
        &mut w,
        2,
        "subscribe",
        serde_json::json!({ "events": ["latency", "dropped"] }),
    )
    .await;
    let _sub_resp = recv_line(&mut r).await;

    // Flood 1500 events onto the broadcast.  The broadcast ring buffer
    // holds 1024; overfilling it guarantees the translator will see
    // Lagged when it next calls recv().
    for _ in 0..1500_u32 {
        let _ = inputs
            .event_tx
            .send(ChannelEvent::Latency { sample_ms: 1.0 });
    }

    // Give the server tasks (translator + writer) time to process the
    // flood and drain the outbound queue.
    tokio::time::sleep(Duration::from_millis(200)).await;

    // Send a few more events to trigger the `dropped` flush now that
    // dropped_count > 0 and the queue has drained.
    for _ in 0..10_u32 {
        let _ = inputs
            .event_tx
            .send(ChannelEvent::Latency { sample_ms: 2.0 });
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    // Drain the client side until we see a `dropped` event or 5 s elapse.
    let mut saw_dropped = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let short = remaining.min(Duration::from_millis(500));
        let mut buf = String::new();
        match tokio::time::timeout(short, r.read_line(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => break, // EOF or I/O error
            Err(_) => continue,              // sub-timeout, keep looping
            Ok(Ok(_)) => {
                let ev: serde_json::Value =
                    serde_json::from_str(buf.trim()).unwrap_or(serde_json::Value::Null);
                if ev["event"] == "dropped" {
                    let count = ev["data"]["count"].as_u64().expect("count");
                    assert!(count > 0, "dropped count must be positive");
                    saw_dropped = true;
                    break;
                }
                // latency event — keep draining
            }
        }
    }

    assert!(
        saw_dropped,
        "expected a `dropped` event after overflowing the broadcast ring buffer"
    );

    handle.stop().await;
}

/// 13. screenshot with format "png" returns a valid PNG of the right size.
#[tokio::test]
async fn screenshot_png_returns_valid_image() {
    // 4×4 test surface: each pixel cycles through RGBA bytes.
    let pixel_data: Vec<u8> = (0u8..16)
        .flat_map(|i| [i * 4, i * 4 + 1, i * 4 + 2, 255])
        .collect();
    let mirror = Arc::new(tokio::sync::Mutex::new(SurfaceMirror::with_test_surface(
        0,
        0,
        4,
        4,
        &pixel_data,
    )));

    let status = MockStatusProvider::new(default_status());
    let (path, handle, _inputs) = spawn_server(status, mirror).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    send_request(
        &mut w,
        2,
        "screenshot",
        serde_json::json!({ "surface_id": 0, "format": "png" }),
    )
    .await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], true, "screenshot png: {:?}", resp);
    let result = &resp["result"];
    assert_eq!(result["width"], 4);
    assert_eq!(result["height"], 4);
    assert_eq!(result["format"], "png");

    let b64 = result["data_base64"].as_str().expect("data_base64 string");
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("base64 decode");

    // Verify it is a valid PNG by inspecting the magic bytes.
    assert_eq!(
        &raw[0..8],
        b"\x89PNG\r\n\x1a\n",
        "PNG magic bytes not found in screenshot output"
    );

    // Decode with the `image` crate to confirm dimensions.
    let img = image::load_from_memory(&raw).expect("PNG decode");
    assert_eq!(img.width(), 4);
    assert_eq!(img.height(), 4);

    handle.stop().await;
}

/// 14. screenshot with format "rgba" returns raw pixel bytes.
#[tokio::test]
async fn screenshot_rgba_returns_raw_pixels() {
    let pixel_data: Vec<u8> = (0u8..16).flat_map(|i| [i, i, i, 255]).collect();
    let mirror = Arc::new(tokio::sync::Mutex::new(SurfaceMirror::with_test_surface(
        0,
        0,
        4,
        4,
        &pixel_data,
    )));

    let status = MockStatusProvider::new(default_status());
    let (path, handle, _inputs) = spawn_server(status, mirror).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    send_request(
        &mut w,
        2,
        "screenshot",
        serde_json::json!({ "surface_id": 0, "format": "rgba" }),
    )
    .await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], true, "screenshot rgba: {:?}", resp);
    let result = &resp["result"];
    assert_eq!(result["format"], "rgba");

    let b64 = result["data_base64"].as_str().expect("data_base64");
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .expect("base64 decode");

    // 4×4 RGBA = 64 bytes.
    assert_eq!(raw.len(), 64, "expected 4×4×4 = 64 bytes of RGBA data");

    handle.stop().await;
}

/// 15. screenshot for a non-existent surface returns no_such_surface.
#[tokio::test]
async fn screenshot_unknown_surface_returns_no_such_surface() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, _inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    send_request(
        &mut w,
        2,
        "screenshot",
        serde_json::json!({ "surface_id": 99 }),
    )
    .await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"]["code"], "no_such_surface");

    handle.stop().await;
}

/// 16. screenshot with an unsupported format returns unsupported_format.
#[tokio::test]
async fn screenshot_unsupported_format_returns_error() {
    let pixel_data: Vec<u8> = vec![0u8; 4 * 4 * 4];
    let mirror = Arc::new(tokio::sync::Mutex::new(SurfaceMirror::with_test_surface(
        0,
        0,
        4,
        4,
        &pixel_data,
    )));

    let status = MockStatusProvider::new(default_status());
    let (path, handle, _inputs) = spawn_server(status, mirror).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    send_request(
        &mut w,
        2,
        "screenshot",
        serde_json::json!({ "surface_id": 0, "format": "jpeg" }),
    )
    .await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"]["code"], "unsupported_format");

    handle.stop().await;
}

/// 17. Client disconnect cancels in-flight pastes.
///
/// The `CancellationToken` for each in-flight paste is fired by
/// `ClientState::cancel_all_pastes` on disconnect.  We verify this
/// indirectly: after dropping the writer, the `InputEvent::PasteText`
/// that was enqueued carries a cancellation token that `is_cancelled()`
/// after the client disconnects.
///
/// We cannot call `is_cancelled()` from the test directly because the
/// token lives inside the server's `ClientState`.  Instead we verify
/// that the token embedded in the `PasteText` event (retrieved from
/// `input_rx`) transitions to cancelled within 500 ms of the writer
/// being dropped.  This exercises the same code path that stops a long-
/// running paste from generating synthetic key events.
#[tokio::test]
async fn client_disconnect_cancels_in_flight_pastes() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, mut inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    send_request(
        &mut w,
        99,
        "paste",
        serde_json::json!({ "text": "hello world" }),
    )
    .await;
    let _resp = recv_line(&mut r).await;

    // Pull the PasteText event out of the input mpsc so we have a
    // handle to its cancellation token.
    let ev = tokio::time::timeout(Duration::from_secs(2), inputs.input_rx.recv())
        .await
        .expect("timeout waiting for PasteText event")
        .expect("input_rx closed");

    let cancel_token = match ev {
        InputEvent::PasteText {
            cancel: Some(tok), ..
        } => tok,
        other => panic!("expected PasteText with cancel token, got {:?}", other),
    };

    assert!(
        !cancel_token.is_cancelled(),
        "token should not be cancelled yet"
    );

    // Drop the writer — this closes the client connection.
    drop(w);
    // Also drop the reader to fully close our end.
    drop(r);

    // Give the server a moment to detect the disconnect and call
    // cancel_all_pastes().
    tokio::time::timeout(Duration::from_millis(500), cancel_token.cancelled())
        .await
        .expect("cancel token was not fired within 500 ms of client disconnect");

    handle.stop().await;
}

/// 18. An unknown method returns unknown_method.
#[tokio::test]
async fn unknown_method_returns_unknown_method() {
    let status = MockStatusProvider::new(default_status());
    let (path, handle, _inputs) = spawn_server(status, empty_mirror()).await;
    let (mut r, mut w) = connect_client(&path).await;
    hello(&mut r, &mut w).await;

    send_request(&mut w, 1, "frobnicate", serde_json::json!({})).await;
    let resp = recv_line(&mut r).await;
    assert_eq!(resp["ok"], false);
    assert_eq!(resp["error"]["code"], "unknown_method");

    handle.stop().await;
}
