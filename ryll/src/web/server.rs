use std::fmt::Write as FmtWrite;
use std::net::SocketAddr;
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
use shakenfist_spice_webrtc::WebrtcBridge;
use subtle::ConstantTimeEq;
use tokio::net::TcpListener;
use tokio::sync::Mutex;
use tracing::info;

use super::signalling::EncoderInfra;

/// Per-launch state shared across handlers. Holds the
/// auth token plus the per-viewer bridge + encoder slots
/// that 4c's `POST /offer` handler manipulates.
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
}

impl WebState {
    /// Construct fresh state with a random 32-byte token
    /// (hex-encoded -> 64 chars).
    pub fn new() -> Self {
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
/// and run the server until `axum::serve` exits.
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
    axum::serve(listener, build_router(state)).await?;
    Ok(())
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
}
