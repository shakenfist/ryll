use std::fmt::Write as FmtWrite;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use shakenfist_spice_webrtc::{UdpBindPolicy, WebrtcBridge};
use subtle::ConstantTimeEq;
use tokio::sync::{broadcast, mpsc, Mutex, Notify};
use tracing::info;

use super::signalling::EncoderInfra;

/// Default capacity of the broadcast channel that fan-outs
/// `ChannelEvent`s from the renderer to web-mode consumers
/// (surface mirror in 5b, cursor relay in 5d, audio sink in 5e).
/// 5a installs no real subscribers; a slow / absent subscriber
/// would simply lose old messages with `RecvError::Lagged`,
/// which is fine because the events are stateless deltas.
pub const EVENT_BROADCAST_CAPACITY: usize = 1024;
/// Capacity of the `InputEvent` mpsc fed by browser
/// keyboard/mouse messages.
pub const INPUT_CHANNEL_CAPACITY: usize = 256;
/// Capacity of the `(width, height)` resize mpsc fed when the
/// browser sends its initial viewport message.
pub const RESIZE_CHANNEL_CAPACITY: usize = 16;

/// Per-launch state shared across handlers. Holds the
/// auth token plus the per-viewer bridge + encoder slots
/// that the `POST /offer` handler manipulates.
///
/// Also holds the channel handles that bridge the renderer
/// (running inside `run_connection`) to web-mode consumers
/// and producers. The senders are owned by the input, cursor
/// and audio relays; the broadcast `event_tx` lets multiple
/// observers subscribe to `ChannelEvent`s independently.
pub struct WebState {
    pub token: String,
    /// Holds the active [`WebrtcBridge`] when one exists.
    /// Single-viewer enforcement: a new offer replaces the
    /// existing bridge.
    pub bridge_slot: Arc<Mutex<Option<WebrtcBridge>>>,
    /// Producer end of the outbound control queue. Cloned into the
    /// cursor relay, the mouse-mode tracker and each input relay; see
    /// [`super::control`] for why those producers do not touch
    /// `bridge_slot` themselves.
    pub(crate) control_tx: super::control::ControlSink,
    /// Consumer end, taken exactly once by `run_web` when it spawns
    /// [`super::control::run_control_writer`]. Left in place by unit
    /// tests, which read it directly to see what the browser would
    /// have been sent.
    pub(crate) control_rx: Mutex<Option<tokio::sync::mpsc::Receiver<Vec<u8>>>>,
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
    /// Monotonically increasing counter incremented by `POST
    /// /offer` whenever a new bridge is installed. The bridge
    /// reaper snapshots this before awaiting the dead signal;
    /// if the counter has advanced by the time the reaper
    /// wakes, a new bridge has replaced the old one and the
    /// reaper skips the reap to avoid closing a healthy bridge.
    pub bridge_generation: Arc<AtomicU64>,
    /// Raised by `POST /offer` once a new bridge is in the slot and
    /// [`Self::bridge_generation`] has been bumped.
    ///
    /// The reaper parks on the *current* bridge's dead signal, so
    /// without this its only way to notice a replacement is for the
    /// bridge it is watching to die — and a bridge closed by `/offer`
    /// is not guaranteed to raise `dead` at all. See
    /// `crate::web::lifecycle::run_bridge_reaper`.
    ///
    /// `notify_one`, not `notify_waiters`: the replacement can land in
    /// the window between the reaper reading the slot and parking on
    /// the signal, and a stored permit survives that race where a
    /// broadcast to zero waiters would be lost.
    pub bridge_replaced: Arc<Notify>,
    /// WebRTC media socket bind policy from `--web-media-addr` and
    /// `--web-media-port`, validated at startup by
    /// [`crate::config::web_media_bind_policy`]. Handed to every
    /// bridge `POST /offer` builds; the bridge re-resolves it each
    /// time, so an interface that appears mid-session is picked up.
    pub udp_bind: UdpBindPolicy,
    /// STUN/TURN URLs from `--web-ice-server`. Empty unless the
    /// operator supplied some, which is the LAN-only default.
    pub ice_servers: Vec<String>,
    /// Timestamp of the last accepted `POST /offer`. Used to
    /// enforce a 1-second cooldown between offers so an
    /// authenticated client cannot thrash the openh264 encoder
    /// init at arbitrary rate.
    ///
    /// `std::sync::Mutex` (not `tokio::sync::Mutex`) because the
    /// lock hold time is microseconds and no `.await` is held
    /// while the guard is live.
    pub last_offer_at: std::sync::Mutex<Instant>,
    /// The SPICE session's current mouse mode, maintained by
    /// [`crate::web::inputs::run_mouse_mode_tracker`] and read by
    /// each bridge's input relay to choose between absolute and
    /// relative pointer messages.
    ///
    /// Lives in shared state rather than in the relay because the
    /// mode is announced at session-init, seconds before any
    /// browser connects: a `broadcast::Receiver` subscribed when
    /// the relay spawns would never see that message.
    pub mouse_mode: Arc<AtomicU32>,
    /// Whether the current bridge's offer/answer failed to settle on
    /// a video codec, set by `post_offer` once `accept_offer`
    /// returns and read by the input relay when the browser says
    /// hello.
    ///
    /// A shared cell rather than a message pushed at negotiation
    /// time, for the reason `crate::web::control` gives at length:
    /// negotiation finishes long before SCTP opens the control
    /// datachannel, so anything pushed then is simply lost. The
    /// browser pulls it instead, and the pull cannot be too early by
    /// construction.
    ///
    /// False until proven otherwise, so a viewer is never wrongly
    /// told its video is broken.
    ///
    /// Shared state holding a per-bridge fact, which is the wrong
    /// shape and only safe because web mode supports one viewer at a
    /// time. See #314 for the fix: read `video_negotiated()` once and
    /// pass a plain `bool` into the relay.
    pub no_video_codec: Arc<AtomicBool>,
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
            // Loopback-capable, so the web tests still have an
            // address to bind inside a network-isolated build
            // sandbox. See `bind_policy_for_tests`.
            shakenfist_spice_webrtc::bind_policy_for_tests(),
            Vec::new(),
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
        udp_bind: UdpBindPolicy,
        ice_servers: Vec<String>,
    ) -> Self {
        Self::build(
            Some(input_tx),
            Some(resize_tx),
            Some(event_tx),
            surface_mirror,
            active_opus_tx,
            udp_bind,
            ice_servers,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        input_tx: Option<mpsc::Sender<InputEvent>>,
        resize_tx: Option<mpsc::Sender<(u32, u32)>>,
        event_tx: Option<broadcast::Sender<ChannelEvent>>,
        surface_mirror: Arc<Mutex<SurfaceMirror>>,
        active_opus_tx: super::audio::ActiveSenderSlot,
        udp_bind: UdpBindPolicy,
        ice_servers: Vec<String>,
    ) -> Self {
        let mut bytes = [0u8; 32];
        rand::thread_rng().fill_bytes(&mut bytes);
        let token = bytes.iter().fold(String::with_capacity(64), |mut acc, b| {
            let _ = write!(acc, "{:02x}", b);
            acc
        });
        let (control_tx, control_rx) = super::control::control_queue();
        Self {
            token,
            bridge_slot: Arc::new(Mutex::new(None)),
            control_tx,
            control_rx: Mutex::new(Some(control_rx)),
            encoder: Arc::new(Mutex::new(EncoderInfra::new())),
            input_tx,
            resize_tx,
            event_tx,
            surface_mirror,
            active_opus_tx,
            bridge_generation: Arc::new(AtomicU64::new(0)),
            bridge_replaced: Arc::new(Notify::new()),
            udp_bind,
            ice_servers,
            // Initialise 60 s in the past so the first offer
            // always succeeds without a cold-start delay.
            last_offer_at: std::sync::Mutex::new(Instant::now() - Duration::from_secs(60)),
            // Default to client mode: it is what a guest running
            // vdagent negotiates, and it keeps the pre-session
            // behaviour identical to what it was before the mode
            // was tracked at all. The tracker corrects this within
            // milliseconds of session-init in any real session.
            mouse_mode: Arc::new(AtomicU32::new(shakenfist_spice_protocol::MOUSE_MODE_CLIENT)),
            no_video_codec: Arc::new(AtomicBool::new(false)),
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
        .route("/static/sfui/tokens.css", get(serve_sfui_tokens_css))
        .route("/static/sfui/sf.css", get(serve_sfui_sf_css))
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
    css(super::assets::STYLE_CSS)
}

async fn serve_sfui_tokens_css() -> impl IntoResponse {
    css(super::assets::SFUI_TOKENS_CSS)
}

async fn serve_sfui_sf_css() -> impl IntoResponse {
    css(super::assets::SFUI_SF_CSS)
}

/// Shared response shape for the stylesheets. Three routes
/// serving three constants differ only in the constant, and a
/// fourth is one `include_str!` away if a page element ever
/// needs another piece of sfui.
fn css(body: &'static str) -> impl IntoResponse {
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "text/css; charset=utf-8")],
        body,
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
/// `run_web` uses this to build the config before handing it to
/// [`run_with_tls`].
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
/// regardless of TLS mode. The explicit-bridge-close sequence
/// in `main::run_web` runs after this future returns either
/// way.
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
    // `with_graceful_shutdown(future)` pattern that axum::serve uses).
    let handle = Handle::new();

    // Watcher task: poll SHUTDOWN_REQUESTED at the same 100 ms
    // cadence the old `shutdown_signal` future used. When the flag
    // flips, signal `axum_server` to drain in-flight requests and
    // stop accepting new ones, with a 5 s ceiling matching the
    // graceful-shutdown budget.
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
    let scheme = if tls.is_some() { "https" } else { "http" };
    let token = state.token.clone();
    let reporter_handle = handle.clone();
    let reporter = tokio::spawn(async move {
        if let Some(local_addr) = reporter_handle.listening().await {
            // Operator-visible: full URL (must be the way the
            // operator actually copies it to their browser).
            // Goes to stdout only — never through the tracing
            // pipeline so the token does not leak into journald
            // or log aggregators.
            println!(
                // audit-allow-println: operator-facing URL output, not for logs
                "ryll: serving web frontend at {}://{}/?token={}",
                scheme, local_addr, token
            );

            // Structured-log breadcrumb: bind address only. The
            // token prefix is enough to disambiguate sessions in
            // audit logs without giving log readers the full
            // credential.
            info!(
                "web: listening on {} (token prefix {}…)",
                local_addr,
                &token[..8.min(token.len())],
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
            body.contains(&format!("/static/sfui/tokens.css?token={}", token)),
            "rendered HTML should embed token in sfui tokens.css href"
        );
        assert!(
            body.contains(&format!("/static/sfui/sf.css?token={}", token)),
            "rendered HTML should embed token in sfui sf.css href"
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
            "app.js should create a data channel before the offer: \
             missing"
        );
        // The control channel is negotiated out of band, so the label
        // and stream id here are the only thing pairing app.js with
        // the bridge. A drift is silent: no DCEP open is exchanged, so
        // a mismatched id still brings the association up and still
        // fires `onopen`, and every keystroke, pointer event and the
        // hello handshake go nowhere while the UI reads "Connected".
        // These assertions are what turns that into a test failure.
        assert!(
            body.contains(&format!(
                "createDataChannel('{}'",
                shakenfist_spice_webrtc::CONTROL_DC_LABEL
            )),
            "app.js should label the control channel '{}': missing",
            shakenfist_spice_webrtc::CONTROL_DC_LABEL
        );
        assert!(
            body.contains("negotiated: true"),
            "app.js should negotiate the control channel out of band: \
             missing"
        );
        assert!(
            body.contains(&format!(
                "id: {},",
                shakenfist_spice_webrtc::CONTROL_DC_STREAM_ID
            )),
            "app.js should pin the control channel to SCTP stream {}: \
             missing",
            shakenfist_spice_webrtc::CONTROL_DC_STREAM_ID
        );
        assert!(
            body.contains("recvonly"),
            "app.js should request recvonly transceivers: missing"
        );
        // Auto-reconnect assertions.
        assert!(
            body.contains("scheduleReconnect"),
            "app.js should contain scheduleReconnect: missing"
        );
        assert!(
            body.contains("async function connect"),
            "app.js should expose connect() as a callable function: \
             missing"
        );
        assert!(
            body.contains("1000"),
            "app.js should contain the 1 s backoff value: missing"
        );
        assert!(
            body.contains("reconnect-btn"),
            "app.js should reference the reconnect button id: \
             missing"
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

    /// The page opts in to sfui, and opting in is three things
    /// that have to agree: the stylesheets in the right order
    /// (tokens before sf.css, page styles after both), the
    /// `sf-page` class that gates every sfui rule, and the
    /// pinned dark theme that stands in for the theme boot
    /// script this page does not serve. Each is a silent failure
    /// if it regresses -- an unstyled page still renders.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn index_opts_in_to_sfui() {
        let (router, token) = router();
        let req = HttpRequest::builder()
            .method(Method::GET)
            .uri(format!("/?token={}", token))
            .body(Body::empty())
            .unwrap();
        let resp = router.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body_bytes = axum::body::to_bytes(resp.into_body(), 1024 * 1024)
            .await
            .unwrap();
        let body = std::str::from_utf8(&body_bytes).unwrap();

        assert!(
            body.contains("class=\"sf-page\""),
            "body should carry sf-page, which gates every sfui rule"
        );
        assert!(
            body.contains("data-theme=\"dark\""),
            "page should pin the dark theme: it serves no theme \
             boot script"
        );

        let tokens_at = body
            .find("/static/sfui/tokens.css")
            .expect("tokens.css should be linked");
        let sf_at = body
            .find("/static/sfui/sf.css")
            .expect("sf.css should be linked");
        let page_at = body
            .find("/static/style.css")
            .expect("page styles should be linked");
        assert!(
            tokens_at < sf_at && sf_at < page_at,
            "stylesheet order must be tokens, sf.css, page styles: \
             {} {} {}",
            tokens_at,
            sf_at,
            page_at
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sfui_css_with_token_returns_css() {
        for path in ["/static/sfui/tokens.css", "/static/sfui/sf.css"] {
            let (router, token) = router();
            let req = HttpRequest::builder()
                .method(Method::GET)
                .uri(format!("{}?token={}", path, token))
                .body(Body::empty())
                .unwrap();
            let resp = router.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "{}", path);
            let ct = resp.headers().get(header::CONTENT_TYPE).unwrap();
            assert!(ct.to_str().unwrap().starts_with("text/css"), "{}", path);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn sfui_css_without_token_returns_unauthorized() {
        for path in ["/static/sfui/tokens.css", "/static/sfui/sf.css"] {
            let (router, _token) = router();
            let req = HttpRequest::builder()
                .method(Method::GET)
                .uri(path)
                .body(Body::empty())
                .unwrap();
            let resp = router.oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNAUTHORIZED, "{}", path);
        }
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
        std::fs::write(&key_path, cert.signing_key.serialize_pem()).unwrap();

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
