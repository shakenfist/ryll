use std::fmt::Write as FmtWrite;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use axum::{
    extract::{Request, State},
    http::{header, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Router,
};
use axum_server::tls_rustls::RustlsConfig;
use axum_server::Handle;
use rand::RngCore;
use shakenfist_spice_renderer::{ChannelEvent, InputEvent, SurfaceMirror};
use shakenfist_spice_webrtc::WebrtcBridge;
use subtle::ConstantTimeEq;
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
    /// Slot holding the active bridge's audio-pump
    /// `mpsc::Sender`. The renderer-side
    /// [`crate::web::audio::WebOpusSink`] reads from this slot
    /// on every Opus DATA packet; the signalling handler writes
    /// a fresh `Sender` into the slot at every `/offer`. `None`
    /// outside web-with-SPICE sessions (e.g. unit tests of the
    /// HTTP layer construct a `WebState` without a sink).
    pub active_opus_tx: super::audio::ActiveSenderSlot,
}

impl WebState {
    /// Construct state without renderer channels. Used by the
    /// router unit tests and by any hypothetical caller that
    /// only wants to exercise the HTTP layer without a live
    /// SPICE session attached. The 5a `run_web` path uses
    /// [`Self::with_channels`] to wire the renderer.
    #[cfg(test)]
    pub fn new() -> Self {
        Self::build(
            None,
            None,
            None,
            Arc::new(Mutex::new(SurfaceMirror::new())),
            Arc::new(std::sync::Mutex::new(None)),
        )
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
        active_opus_tx: super::audio::ActiveSenderSlot,
    ) -> Self {
        Self::build(
            Some(input_tx),
            Some(resize_tx),
            Some(event_tx),
            surface_mirror,
            active_opus_tx,
        )
    }

    fn build(
        input_tx: Option<mpsc::Sender<InputEvent>>,
        resize_tx: Option<mpsc::Sender<(u32, u32)>>,
        event_tx: Option<broadcast::Sender<ChannelEvent>>,
        surface_mirror: Arc<Mutex<SurfaceMirror>>,
        active_opus_tx: super::audio::ActiveSenderSlot,
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
            active_opus_tx,
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

/// Load a [`RustlsConfig`] from PEM-encoded cert and key files.
/// Surfaces an `anyhow` error chain on missing / malformed files.
/// Phase 8a uses this from `run_web` to build the config before
/// handing it to [`run_with_tls`].
pub async fn load_tls_config(cert: &Path, key: &Path) -> Result<RustlsConfig> {
    RustlsConfig::from_pem_file(cert, key)
        .await
        .with_context(|| {
            format!(
                "loading --web TLS cert/key from {} / {}",
                cert.display(),
                key.display()
            )
        })
}

/// Bind on `host:port` (port=0 -> ephemeral), build the router
/// with `state`, print the URL to stdout, and run the server
/// until the process-wide `SHUTDOWN_REQUESTED` flag is raised
/// (e.g. via Ctrl+C) or the bind future exits for another
/// reason.
///
/// Plain-HTTP entry point. For HTTPS, see [`run_with_tls`]. Both
/// branches drive `axum_server` so the graceful-shutdown wiring
/// (a `Handle` + a SHUTDOWN_REQUESTED watcher task) is uniform
/// regardless of TLS mode. The Phase 6 explicit-bridge-close
/// sequence in `main::run_web` runs after this future returns
/// either way.
pub async fn run(state: Arc<WebState>, host: &str, port: u16) -> Result<()> {
    run_inner(state, host, port, None, &crate::SHUTDOWN_REQUESTED).await
}

/// HTTPS variant of [`run`]. Same shutdown semantics; binds via
/// `axum_server::bind_rustls`. Caller is responsible for loading
/// the [`RustlsConfig`] (typically via [`load_tls_config`]).
pub async fn run_with_tls(
    state: Arc<WebState>,
    host: &str,
    port: u16,
    config: RustlsConfig,
) -> Result<()> {
    run_inner(state, host, port, Some(config), &crate::SHUTDOWN_REQUESTED).await
}

/// Shared implementation behind [`run`] / [`run_with_tls`]. The
/// `flag` parameter is the SHUTDOWN_REQUESTED-shaped watcher;
/// passing it explicitly lets tests inject a private
/// `AtomicBool`.
async fn run_inner(
    state: Arc<WebState>,
    host: &str,
    port: u16,
    tls: Option<RustlsConfig>,
    flag: &'static AtomicBool,
) -> Result<()> {
    let addr: SocketAddr = format!("{}:{}", host, port)
        .parse()
        .map_err(|e| anyhow::anyhow!("invalid bind address {}:{}: {}", host, port, e))?;

    // axum-server's Handle exposes both `listening()` (so we can
    // discover the ephemeral port post-bind) and
    // `graceful_shutdown(timeout)` (replacing the
    // `with_graceful_shutdown(future)` pattern that
    // axum::serve used pre-Phase-8a).
    let handle = Handle::new();

    // Watcher task: poll SHUTDOWN_REQUESTED at the same 100 ms
    // cadence the old `shutdown_signal` future used. When the
    // flag flips, signal `axum_server` to drain in-flight
    // requests and stop accepting new ones, with a 5 s ceiling
    // matching the Phase 6 graceful-shutdown budget.
    let shutdown_handle = handle.clone();
    let watcher = tokio::spawn(async move {
        use std::sync::atomic::Ordering;
        loop {
            if flag.load(Ordering::Relaxed) {
                tracing::info!("web: shutdown requested; draining axum");
                shutdown_handle.graceful_shutdown(Some(Duration::from_secs(5)));
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    });

    // Reporter task: emits the URL once the listener is bound.
    // axum-server doesn't hand back a TcpListener pre-serve
    // (unlike axum::serve), so we use Handle::listening() —
    // resolves to `Some(local_addr)` once the bind succeeds.
    // Goes through tracing rather than raw stdout so the line
    // respects the operator's logging configuration. The
    // workspace's tracing-subscriber writes fmt::layer to stdout
    // by default, so the smoke test continues to grep stdout.
    let scheme = if tls.is_some() { "https" } else { "http" };
    let token = state.token.clone();
    let reporter_handle = handle.clone();
    let reporter = tokio::spawn(async move {
        if let Some(local_addr) = reporter_handle.listening().await {
            info!(
                "ryll: serving web frontend at {}://{}/?token={}",
                scheme, local_addr, token
            );
        }
    });

    let app = build_router(state);

    let result = match tls {
        Some(config) => {
            axum_server::bind_rustls(addr, config)
                .handle(handle)
                .serve(app.into_make_service())
                .await
        }
        None => {
            axum_server::bind(addr)
                .handle(handle)
                .serve(app.into_make_service())
                .await
        }
    };

    // Either the watcher signalled graceful shutdown and the
    // serve future returned `Ok(())`, or the bind itself errored
    // (e.g. address-in-use, malformed cert at runtime). Either
    // way, tear down the helper tasks before propagating.
    watcher.abort();
    reporter.abort();
    result.map_err(anyhow::Error::from)
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
    async fn index_includes_cursor_overlay() {
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
            body.contains(r#"id="cursor""#),
            "rendered HTML should include the cursor overlay element: {}",
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
        // Phase 6c: auto-reconnect assertions.
        assert!(
            body.contains("scheduleReconnect"),
            "app.js should contain scheduleReconnect (Phase 6c): missing"
        );
        assert!(
            body.contains("async function connect"),
            "app.js should expose connect() as a callable function \
             (Phase 6c): missing"
        );
        assert!(
            body.contains("1000"),
            "app.js should contain the 1 s backoff value (Phase 6c): missing"
        );
        assert!(
            body.contains("reconnect-btn"),
            "app.js should reference the reconnect button id \
             (Phase 6c): missing"
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

    /// Verify that the SHUTDOWN_REQUESTED watcher inside
    /// `run_inner` fires `Handle::graceful_shutdown` shortly
    /// after the flag flips. We exercise the plain-HTTP path
    /// (port 0 → ephemeral) and assert the bind future returns
    /// promptly. Uses a private static so the test never
    /// touches the process-wide `SHUTDOWN_REQUESTED` flag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn run_inner_observes_shutdown_flag() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        static TEST_FLAG: AtomicBool = AtomicBool::new(false);
        TEST_FLAG.store(false, Ordering::SeqCst);

        let state = Arc::new(WebState::new());
        let server = tokio::spawn(super::run_inner(state, "127.0.0.1", 0, None, &TEST_FLAG));

        // Give the bind a moment to come up.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(
            !server.is_finished(),
            "run_inner returned before shutdown flag was set"
        );

        // Raise the flag and confirm the server drains promptly.
        TEST_FLAG.store(true, Ordering::SeqCst);
        let res = tokio::time::timeout(Duration::from_secs(2), server).await;
        assert!(
            res.is_ok(),
            "run_inner did not return within 2 s of flag flip"
        );

        TEST_FLAG.store(false, Ordering::SeqCst);
    }

    /// rcgen-backed integration test: generate a self-signed
    /// cert + key into a tempdir, load it via
    /// `load_tls_config`, bind axum-server with TLS on, and
    /// hit `https://127.0.0.1:port/?token=...` with reqwest
    /// (cert verification disabled). Asserts 200 OK + the
    /// embedded HTML body contains the expected markers, then
    /// signals shutdown via a private SHUTDOWN_REQUESTED-shaped
    /// flag.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn web_tls_loads_self_signed_cert() {
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        // Install the rustls ring provider once for the test
        // process. Mirrors the production install in main.rs.
        let _ = rustls::crypto::ring::default_provider().install_default();

        static TLS_FLAG: AtomicBool = AtomicBool::new(false);
        TLS_FLAG.store(false, Ordering::SeqCst);

        // Generate a self-signed cert for CN=localhost.
        let cert = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
            .expect("rcgen self-signed cert");
        let tmp = tempfile::tempdir().unwrap();
        let cert_path = tmp.path().join("cert.pem");
        let key_path = tmp.path().join("key.pem");
        std::fs::write(&cert_path, cert.cert.pem()).unwrap();
        std::fs::write(&key_path, cert.key_pair.serialize_pem()).unwrap();

        let config = super::load_tls_config(&cert_path, &key_path)
            .await
            .expect("load_tls_config");

        // Bind on an ephemeral port so parallel test runs do
        // not collide. We need to discover the local port
        // post-bind, so reach into axum-server directly here
        // rather than going through `run_with_tls` (which prints
        // to stdout only).
        let state = Arc::new(WebState::new());
        let token = state.token.clone();
        let app = build_router(state);

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let local_addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();

        let handle = axum_server::Handle::new();
        let shutdown_handle = handle.clone();
        let watcher = tokio::spawn(async move {
            loop {
                if TLS_FLAG.load(Ordering::Relaxed) {
                    shutdown_handle.graceful_shutdown(Some(Duration::from_secs(2)));
                    return;
                }
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        });

        let server = tokio::spawn(async move {
            let server = axum_server::from_tcp_rustls(listener, config)
                .expect("axum-server from_tcp_rustls")
                .handle(handle);
            server
                .serve(app.into_make_service())
                .await
                .expect("axum-server tls test serve")
        });

        // Give the listener a moment to actually start
        // accepting; from_tcp_rustls is synchronous up to
        // accept loop entry but the spawn boundary is async.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Hit https://localhost:port/?token=... with cert
        // verification disabled. reqwest's default-tls
        // (native-tls) handles a self-signed cert fine when
        // `danger_accept_invalid_certs` is on.
        let url = format!("https://{}/?token={}", local_addr, token);
        let client = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .build()
            .unwrap();
        let resp = client.get(&url).send().await.expect("https GET");
        assert_eq!(resp.status(), reqwest::StatusCode::OK);
        let body = resp.text().await.unwrap();
        assert!(
            body.contains("<video"),
            "TLS-served HTML should contain <video>: {}",
            body
        );
        assert!(
            body.contains(&format!("/static/app.js?token={}", token)),
            "TLS-served HTML should embed the token in app.js src: {}",
            body
        );

        // Drain.
        TLS_FLAG.store(true, Ordering::SeqCst);
        let _ = tokio::time::timeout(Duration::from_secs(3), server).await;
        watcher.abort();
        TLS_FLAG.store(false, Ordering::SeqCst);
    }

    /// Bad cert path should produce a clear `anyhow` error
    /// chain mentioning the file we tried to load.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn web_tls_missing_cert_file_errors_clearly() {
        let err = super::load_tls_config(
            std::path::Path::new("/no/such/cert.pem"),
            std::path::Path::new("/no/such/key.pem"),
        )
        .await
        .expect_err("expected error for missing cert file");
        let msg = format!("{:#}", err);
        assert!(
            msg.contains("/no/such/cert.pem"),
            "error should mention the cert path: {}",
            msg
        );
    }

    /// Clap rejects supplying only one of the --web-tls-cert /
    /// --web-tls-key pair. The `requires =` attribute on each
    /// flag enforces this at parse time.
    #[test]
    fn web_tls_flags_require_both() {
        use clap::Parser;
        // Cert without key: rejected.
        let res = crate::config::Args::try_parse_from([
            "ryll",
            "--web",
            "--file",
            "x.vv",
            "--web-tls-cert",
            "cert.pem",
        ]);
        assert!(
            res.is_err(),
            "clap should reject --web-tls-cert without --web-tls-key"
        );

        // Key without cert: rejected.
        let res = crate::config::Args::try_parse_from([
            "ryll",
            "--web",
            "--file",
            "x.vv",
            "--web-tls-key",
            "key.pem",
        ]);
        assert!(
            res.is_err(),
            "clap should reject --web-tls-key without --web-tls-cert"
        );

        // Both: accepted.
        let res = crate::config::Args::try_parse_from([
            "ryll",
            "--web",
            "--file",
            "x.vv",
            "--web-tls-cert",
            "cert.pem",
            "--web-tls-key",
            "key.pem",
        ]);
        assert!(
            res.is_ok(),
            "clap should accept both flags: {:?}",
            res.err()
        );

        // Neither: accepted (plain-HTTP is the default).
        let res = crate::config::Args::try_parse_from(["ryll", "--web", "--file", "x.vv"]);
        assert!(
            res.is_ok(),
            "clap should accept --web with neither TLS flag: {:?}",
            res.err()
        );
    }
}
