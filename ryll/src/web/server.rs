use std::fmt::Write as FmtWrite;
use std::net::SocketAddr;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use rand::RngCore;
use shakenfist_spice_renderer::{ChannelEvent, InputEvent, SurfaceMirror};
use shakenfist_spice_webrtc::WebrtcBridge;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::{broadcast, mpsc, Mutex};
use tracing::info;

use super::signalling::EncoderInfra;

/// Default capacity of the broadcast channel that fan-outs
/// `ChannelEvent`s from the renderer to web-mode consumers
/// (surface mirror in 5b, cursor relay in 5d, audio sink in 5e).
/// 5a installs no real subscribers; a slow / absent subscriber
/// would simply lose old messages with `RecvError::Lagged`,
/// which is fine because the events are stateless deltas.
pub const EVENT_BROADCAST_CAPACITY: usize = 1024;
/// Capacity of the `InputEvent` mpsc that 5c will feed once
/// browser keyboard/mouse messages start flowing. 5a creates
/// the channel but nothing sends on it.
pub const INPUT_CHANNEL_CAPACITY: usize = 256;
/// Capacity of the `(width, height)` resize mpsc that 5c will
/// feed when the browser sends its initial viewport message.
pub const RESIZE_CHANNEL_CAPACITY: usize = 16;

/// Per-launch state shared across handlers. Holds the
/// auth token plus the per-viewer bridge + encoder slots
/// that 4c's `POST /offer` handler manipulates.
///
/// Phase 5a additions: channel handles that bridge the renderer
/// (running inside `run_connection`) to web-mode consumers and
/// producers. The senders are owned by the future input/cursor/
/// audio relays (5b–5e); the broadcast `event_tx` lets multiple
/// observers subscribe to `ChannelEvent`s without restructuring
/// when later steps add their consumers.
pub struct WebState {
    pub token: String,
    /// Holds the active [`WebrtcBridge`] when one exists.
    /// Single-viewer enforcement: a new offer replaces the
    /// existing bridge.
    pub bridge_slot: Arc<Mutex<Option<WebrtcBridge>>>,
    /// Per-launch encoder pipeline (synthetic source +
    /// `H264Encoder` + `EncoderTask`). `EncoderInfra::restart`
    /// stops any existing encoder and spawns a fresh one for
    /// each new viewer.
    pub encoder: Arc<Mutex<EncoderInfra>>,
    /// Sender for the [`InputEvent`] channel `run_connection`
    /// consumes. 5c will start sending real keyboard / pointer
    /// events here. `None` in tests / when web mode is not
    /// actually connected to a SPICE session.
    #[allow(dead_code)] // wired in 5c
    pub input_tx: Option<mpsc::Sender<InputEvent>>,
    /// Sender for the `(width, height)` resize channel
    /// `run_connection` plumbs into `MainChannel` to drive the
    /// SPICE vdagent's `VDAgentMonitorsConfig` flow. 5c will
    /// send the browser's initial viewport here.
    #[allow(dead_code)] // wired in 5c
    pub resize_tx: Option<mpsc::Sender<(u32, u32)>>,
    /// Broadcaster for `ChannelEvent`s emitted by the renderer's
    /// session orchestrator. 5b/5d/5e each spawn a subscriber.
    /// `None` outside web-with-SPICE sessions (e.g. in unit
    /// tests of the HTTP layer).
    #[allow(dead_code)] // subscribed in 5b/5d/5e
    pub event_tx: Option<broadcast::Sender<ChannelEvent>>,
    /// Live pixel store rebuilt from the renderer's `ChannelEvent`
    /// stream by the apply-event task spawned in `run_web`. The
    /// encoder reads from this via `RealFrameSource`. Wrapped in
    /// a `tokio::sync::Mutex` so the apply-event task can `lock`
    /// asynchronously while the encoder thread `try_lock`s
    /// synchronously from its blocking pool.
    pub surface_mirror: Arc<Mutex<SurfaceMirror>>,
}

impl WebState {
    /// Construct state without renderer channels. Used by the
    /// router unit tests and by any hypothetical caller that
    /// only wants to exercise the HTTP layer without a live
    /// SPICE session attached. The 5a `run_web` path uses
    /// [`Self::with_channels`] to wire the renderer.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::build(None, None, None, Arc::new(Mutex::new(SurfaceMirror::new())))
    }

    /// Construct state with the renderer channels populated.
    /// 5a's `run_web` calls this after spawning `run_connection`
    /// so the HTTP handlers (and 5b–5e relays) can find the
    /// senders.
    pub fn with_channels(
        input_tx: mpsc::Sender<InputEvent>,
        resize_tx: mpsc::Sender<(u32, u32)>,
        event_tx: broadcast::Sender<ChannelEvent>,
        surface_mirror: Arc<Mutex<SurfaceMirror>>,
    ) -> Self {
        Self::build(
            Some(input_tx),
            Some(resize_tx),
            Some(event_tx),
            surface_mirror,
        )
    }

    fn build(
        input_tx: Option<mpsc::Sender<InputEvent>>,
        resize_tx: Option<mpsc::Sender<(u32, u32)>>,
        event_tx: Option<broadcast::Sender<ChannelEvent>>,
        surface_mirror: Arc<Mutex<SurfaceMirror>>,
    ) -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let token = bytes.iter().fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{:02x}", b);
            acc
        });
        Self {
            token,
            bridge_slot: Arc::new(Mutex::new(None)),
            encoder: Arc::new(Mutex::new(EncoderInfra::new())),
            input_tx,
            resize_tx,
            event_tx,
            surface_mirror,
        }
    }
}

/// Build the axum router. The token middleware is applied to
/// all routes.
pub fn build_router(state: Arc<WebState>) -> Router {
    let token_state = state.clone();
    Router::new()
        .route("/", get(serve_index))
        .route("/static/app.js", get(serve_app_js))
        .route("/static/style.css", get(serve_style_css))
        .route("/offer", post(super::signalling::post_offer))
        .layer(middleware::from_fn_with_state(token_state, check_token))
        .with_state(state)
}

async fn serve_index(State(state): State<Arc<WebState>>) -> impl IntoResponse {
    let body = super::assets::render_index(&state.token);
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
        body,
    )
}

async fn serve_app_js() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/javascript; charset=utf-8")],
        super::assets::APP_JS,
    )
}

async fn serve_style_css() -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        super::assets::STYLE_CSS,
    )
}

/// Token-checking middleware. Reads `?token=...` from the
/// query string and compares it to the launch token using
/// constant-time equality. Rejects with 401 on missing or
/// mismatched token.
async fn check_token(
    State(state): State<Arc<WebState>>,
    req: Request,
    next: Next,
) -> Result<Response, StatusCode> {
    let token_param = req.uri().query().and_then(|q| {
        url::form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v.into_owned())
    });
    match token_param {
        Some(t) if bool::from(t.as_bytes().ct_eq(state.token.as_bytes())) => {
            Ok(next.run(req).await)
        }
        _ => Err(StatusCode::UNAUTHORIZED),
    }
}

/// Bind a TcpListener on `host:port` (port=0 -> ephemeral),
/// build the router with `state`, print the URL to stdout,
/// and run the server until the process-wide `SHUTDOWN_REQUESTED`
/// flag is raised (e.g. via Ctrl+C) or `axum::serve` exits for
/// another reason.
pub async fn run(state: Arc<WebState>, host: &str, port: u16) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address {}:{}: {}", host, port, e))?;
    let listener = TcpListener::bind(addr).await?;
    let local_addr = listener.local_addr()?;
    println!(
        "ryll: serving web frontend at http://{}/?token={}",
        local_addr, state.token
    );
    info!("web: listening on {}", local_addr);
    axum::serve(listener, build_router(state))
        .with_graceful_shutdown(shutdown_signal(&crate::SHUTDOWN_REQUESTED))
        .await?;
    Ok(())
}

/// Future that resolves when `flag` is set to `true`. Polls at
/// 100 ms cadence to match the headless-mode bridge in `main.rs`.
/// Passing the flag as a parameter (rather than hard-coding
/// `crate::SHUTDOWN_REQUESTED`) lets tests inject a private
/// `AtomicBool` and avoid interfering with other tests.
async fn shutdown_signal(flag: &'static AtomicBool) {
    use std::sync::atomic::Ordering;
    use std::time::Duration;
    loop {
        if flag.load(Ordering::Relaxed) {
            tracing::info!("web: shutdown requested; draining axum");
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::{header, Method, Request as HttpRequest, StatusCode};
    use tower::ServiceExt; // for `oneshot`

    fn router() -> (Router, String) {
        let state = Arc::new(WebState::new());
        let token = state.token.clone();
        (build_router(state), token)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_without_token() {
        let (router, _token) = router();
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri("/")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rejects_with_wrong_token() {
        let (router, _token) = router();
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri("/?token=clearlywrong")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn accepts_with_correct_token() {
        let (router, token) = router();
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri(format!("/?token={}", token))
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    #[test]
    fn token_is_64_hex_chars() {
        let state = WebState::new();
        assert_eq!(state.token.len(), 64);
        assert!(state
            .token
            .chars()
            .all(|c| c.is_ascii_hexdigit() && c.is_ascii_lowercase() || c.is_ascii_digit()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_with_token_includes_token_in_subresources() {
        let (router, token) = router();
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri(format!("/?token={}", token))
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(ct.to_str().unwrap().starts_with("text/html"));

        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();
        assert!(
            body.contains(&format!("/static/app.js?token={}", token)),
            "rendered HTML should embed token in app.js src"
        );
        assert!(
            body.contains(&format!("/static/style.css?token={}", token)),
            "rendered HTML should embed token in style.css href"
        );
        assert!(
            !body.contains("{{TOKEN}}"),
            "placeholder should be substituted: {}",
            body
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_includes_enable_audio_button() {
        let (router, token) = router();
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri(format!("/?token={}", token))
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();
        assert!(
            body.contains(r#"id="enable-audio""#),
            "rendered HTML should include the enable-audio button: {}",
            body
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_js_with_token_returns_javascript() {
        let (router, token) = router();
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri(format!("/static/app.js?token={}", token))
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(ct.to_str().unwrap().starts_with("text/javascript"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_js_without_token_returns_unauthorized() {
        let (router, _token) = router();
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri("/static/app.js")
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn app_js_reads_token_from_url() {
        let (router, token) = router();
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri(format!("/static/app.js?token={}", token))
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), 256 * 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();
        assert!(
            body.contains("URLSearchParams"),
            "app.js should read the token via URLSearchParams: missing"
        );
        assert!(
            body.contains("createDataChannel"),
            "app.js should create a data channel before offer \
             (Phase 3 finding): missing"
        );
        assert!(
            body.contains("recvonly"),
            "app.js should request recvonly transceivers: missing"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn style_css_with_token_returns_css() {
        let (router, token) = router();
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri(format!("/static/style.css?token={}", token))
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
        assert!(ct.to_str().unwrap().starts_with("text/css"));
    }

    /// Verify that `shutdown_signal` stays pending while the flag
    /// is false and resolves within 500 ms after the flag is set.
    /// Uses a private static so the test never touches the
    /// process-wide `SHUTDOWN_REQUESTED` flag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn shutdown_signal_observes_flag() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        static TEST_FLAG: AtomicBool = AtomicBool::new(false);
        // Ensure a clean starting state in case of test re-runs.
        TEST_FLAG.store(false, Ordering::SeqCst);

        // Spawn the shutdown_signal future; it must NOT complete
        // while the flag is false.
        let handle = tokio::spawn(shutdown_signal(&TEST_FLAG));

        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !handle.is_finished(),
            "shutdown_signal completed before flag was set"
        );

        // Raise the flag and confirm the future resolves quickly.
        TEST_FLAG.store(true, Ordering::SeqCst);
        let res = tokio::time::timeout(Duration::from_millis(500), handle).await;
        assert!(
            res.is_ok(),
            "shutdown_signal did not return within 500 ms after flag was set"
        );

        // Reset for safety (no other test uses TEST_FLAG, but be tidy).
        TEST_FLAG.store(false, Ordering::SeqCst);
    }
}
