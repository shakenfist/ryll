//! Control-socket server task.
//!
//! Binds a Unix-domain socket at a caller-supplied path, enforces
//! single-client concurrency (a second connect gets a `busy` response
//! and is closed immediately), runs an NDJSON request/response loop,
//! and dispatches the `hello` and `status` verbs.  All other v1 verbs
//! return `not_implemented` until later phase-3 steps wire them.
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

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use super::protocol::{
    major_version_matches, ErrorCode, HelloParams, HelloResult, Request, Response, StatusResult,
    PROTOCOL_VERSION, SUPPORTED_EVENTS, SUPPORTED_METHODS,
};

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
    pub async fn run(
        self,
        status_provider: Arc<dyn StatusProvider>,
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
                                tokio::spawn(async move {
                                    handle_client(stream, provider, cancel).await;
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

async fn handle_client(
    stream: tokio::net::UnixStream,
    status_provider: Arc<dyn StatusProvider>,
    shutdown: CancellationToken,
) {
    info!("control: client connected");

    let (read_half, mut write_half) = stream.into_split();
    let mut lines = BufReader::new(read_half).lines();

    // Track whether this client has completed a successful hello.
    let mut helloed = false;

    loop {
        tokio::select! {
            line_result = lines.next_line() => {
                match line_result {
                    Ok(Some(line)) => {
                        if line.trim().is_empty() {
                            continue;
                        }
                        let response = dispatch_request(
                            &line,
                            &mut helloed,
                            &*status_provider,
                        );

                        let close_after = response
                            .error
                            .as_ref()
                            .map(|e| e.code == ErrorCode::ProtocolVersionMismatch)
                            .unwrap_or(false);

                        let mut serialised = match serde_json::to_string(&response) {
                            Ok(s) => s,
                            Err(e) => {
                                error!("control: failed to serialise response: {}", e);
                                break;
                            }
                        };
                        serialised.push('\n');

                        if let Err(e) = write_half.write_all(serialised.as_bytes()).await {
                            warn!("control: write error: {}", e);
                            break;
                        }

                        if close_after {
                            // Protocol version mismatch: close after writing.
                            let _ = write_half.shutdown().await;
                            break;
                        }
                    }
                    Ok(None) => {
                        // EOF — client closed the connection.
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
                // Session is shutting down; drop the stream.
                info!("control: client task cancelled by shutdown");
                break;
            }
        }
    }
}

/// Dispatch one request line and return the appropriate `Response`.
fn dispatch_request(
    line: &str,
    helloed: &mut bool,
    status_provider: &dyn StatusProvider,
) -> Response {
    let request: Request = match serde_json::from_str(line) {
        Ok(r) => r,
        Err(e) => {
            // We don't have a valid id to echo back; use a synthetic
            // id of null is not valid protocol, so we use id=0 as
            // a best-effort placeholder for unparseable requests.
            warn!("control: failed to parse request: {}", e);
            return Response {
                id: None,
                ok: false,
                result: None,
                error: Some(super::protocol::RpcError {
                    code: ErrorCode::BadParams,
                    message: format!("failed to parse request: {}", e),
                }),
            };
        }
    };

    let id = request.id.clone();
    let method = request.method.as_str();

    // Hello must precede all other verbs.
    if !*helloed && method != "hello" {
        return Response::err(id, ErrorCode::NoHelloYet, "first request must be hello");
    }

    match method {
        "hello" => handle_hello(id, request.params, helloed),
        "status" => handle_status(id, status_provider),
        // Recognised but not yet implemented:
        "send_key" | "paste" | "screenshot" | "subscribe" | "unsubscribe" => Response::err(
            id,
            ErrorCode::NotImplemented,
            format!(
                "method \"{}\" is recognised but not yet implemented",
                method
            ),
        ),
        // Completely unknown:
        _ => Response::err(
            id,
            ErrorCode::UnknownMethod,
            format!("unknown method \"{}\"", method),
        ),
    }
}

fn handle_hello(
    id: super::protocol::RequestId,
    params: serde_json::Value,
    helloed: &mut bool,
) -> Response {
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
            // Parse the major version for the error message.
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

fn handle_status(id: super::protocol::RequestId, status_provider: &dyn StatusProvider) -> Response {
    let snapshot = status_provider.snapshot();
    Response::ok(
        id,
        serde_json::to_value(snapshot).unwrap_or(serde_json::Value::Object(Default::default())),
    )
}
