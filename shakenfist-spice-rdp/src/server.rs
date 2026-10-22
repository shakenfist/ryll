//! The server runner: builds the IronRDP server and runs it on an OS
//! thread of its own with a current-thread runtime, because the
//! server type is not `Send`, and hands the caller a `Send` handle to
//! stop it and wait for it.

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use ironrdp_server::{
    ConnectionHandler, Credentials, DesktopSize, PostConnectionAction, RdpServer, ServerEvent,
};
use tokio::sync::{mpsc, oneshot, watch};
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::auth::ProviderValidator;
use crate::display::ClientSize;
use crate::input::HeldInput;
use crate::tls::TlsIdentity;
use crate::{MirrorDisplay, SessionProvider, SessionSlot, SpiceInputHandler};

/// The port RDP clients connect to when none is given, tried first
/// when [`RdpConfig::port`] is `None`.
pub const DEFAULT_RDP_PORT: u16 = 3389;

/// The desktop size announced to a client before its session is
/// known or before the session has a primary surface. The display
/// handler resizes the client to the surface's size once there is
/// one.
const FALLBACK_DESKTOP_SIZE: DesktopSize = DesktopSize {
    width: 1024,
    height: 768,
};

/// Name of the thread the server runs on, as shown by debuggers and
/// `top -H`.
const THREAD_NAME: &str = "rdp-server";

/// How an RDP client authenticates.
///
/// IronRDP 0.13 offers one or the other on a listener, not both.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RdpSecurity {
    /// TLS, with the username and password sent inside it and checked
    /// by the [`SessionProvider`]. Every RDP client supports this,
    /// including clients that also offer NLA.
    Tls,
    /// Network Level Authentication: CredSSP inside TLS, where the
    /// client proves it knows the password before the session starts.
    /// Clients that offer only TLS are refused. CredSSP's NTLM checks
    /// the password itself against [`RdpConfig::credentials`], so the
    /// session is chosen once, when the server starts.
    Nla,
}

impl RdpSecurity {
    /// The mode's lower-case name, as a command line would spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            RdpSecurity::Tls => "tls",
            RdpSecurity::Nla => "nla",
        }
    }
}

impl fmt::Display for RdpSecurity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Everything [`serve`] needs to start an RDP server.
pub struct RdpConfig {
    /// The address to listen on.
    pub host: IpAddr,
    /// The port to listen on. `Some(port)` uses that port or fails
    /// (`Some(0)` asks the operating system for a free one). `None`
    /// tries [`DEFAULT_RDP_PORT`] and, if it is taken, uses a free
    /// port; [`RdpServerHandle::local_addr`] says which.
    pub port: Option<u16>,
    /// How clients authenticate.
    pub security: RdpSecurity,
    /// The certificate and key the server presents.
    pub identity: TlsIdentity,
    /// The username and password clients must present. Used by NLA
    /// directly, and handed to `provider` for an NLA server's session;
    /// in TLS mode the provider alone decides.
    pub credentials: Credentials,
    /// Chooses the SPICE session an authenticated client is served.
    pub provider: Arc<dyn SessionProvider>,
}

/// A running RDP server, on a thread of its own.
///
/// Dropping the handle does not stop the server; call
/// [`shutdown`](Self::shutdown) and then [`join`](Self::join).
pub struct RdpServerHandle {
    local_addr: SocketAddr,
    events: mpsc::UnboundedSender<ServerEvent>,
    stopping: Arc<AtomicBool>,
    /// The server thread's result, sent as the thread finishes. A
    /// mutex only so the handle is `Sync`; nothing contends for it.
    done: std::sync::Mutex<std_mpsc::Receiver<Result<()>>>,
    /// Closed when the server thread exits, however it exits.
    finished: watch::Receiver<()>,
    thread: JoinHandle<()>,
}

impl fmt::Debug for RdpServerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdpServerHandle")
            .field("local_addr", &self.local_addr)
            .field("stopping", &self.stopping.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

impl RdpServerHandle {
    /// The address the server is listening on, with the port it
    /// actually bound.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Ask the server to stop. Returns at once; [`join`](Self::join)
    /// waits for the stop.
    ///
    /// The server serves one connection at a time, and what happens
    /// next depends on where that connection is:
    ///
    /// - With no client connected, the server stops straight away.
    /// - A client in an established session is disconnected and the
    ///   server then stops.
    /// - A client still in the connection handshake (TLS, CredSSP,
    ///   licensing or capability exchange) is not interrupted: IronRDP
    ///   reads the stop request only once the handshake has finished
    ///   or failed, and it sets no time limit on the handshake. A
    ///   client that stalls there keeps the server running.
    ///
    /// Clients still waiting in the listen backlog are never served.
    pub fn shutdown(&self) {
        self.stopping.store(true, Ordering::Release);
        // Fails only once the server has stopped and dropped its
        // receiver, in which case there is nothing to stop.
        let _ = self
            .events
            .send(ServerEvent::Quit("shutdown requested".to_string()));
    }

    /// Wait up to `timeout` for the server thread to finish, and
    /// return the server's result.
    ///
    /// Blocks the calling thread. A server is finished only after
    /// [`shutdown`](Self::shutdown) or a failure; and since a client
    /// stalled in the connection handshake can delay the stop
    /// indefinitely (see [`shutdown`](Self::shutdown)), the wait is
    /// bounded. On timeout an error is returned and the thread is left
    /// to end with the process.
    pub fn join(self, timeout: Duration) -> Result<()> {
        let received = self
            .done
            .into_inner()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .recv_timeout(timeout);
        match received {
            Ok(result) => {
                // The thread sends its result as its last act, so this
                // join does not wait.
                if self.thread.join().is_err() {
                    bail!("the RDP server thread panicked");
                }
                result
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => Err(anyhow!(
                "the RDP server did not stop within {timeout:?}; a client may be stalled \
                 in the connection handshake"
            )),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                let _ = self.thread.join();
                bail!("the RDP server thread panicked")
            }
        }
    }

    /// Whether the server thread has finished, after a stop or a
    /// failure.
    pub fn is_finished(&self) -> bool {
        self.thread.is_finished()
    }

    /// A future that completes when the server thread finishes, for a
    /// caller that waits on the server alongside other events (a
    /// Ctrl+C, say). [`join`](Self::join) then collects the result
    /// without waiting.
    pub fn stopped(&self) -> impl Future<Output = ()> + Send + 'static {
        let mut finished = self.finished.clone();
        async move {
            // Nothing is ever sent; the channel closes when the thread
            // drops its sender on the way out.
            while finished.changed().await.is_ok() {}
        }
    }
}

/// Start an RDP server serving the sessions `config.provider`
/// chooses, on a thread of its own.
///
/// Returns once the server is listening, with a handle that knows the
/// bound address, or with the reason it could not start: the port is
/// taken, the TLS identity is unusable, no rustls crypto provider is
/// installed, or (for NLA) the provider refuses the configured
/// credentials. The calling thread blocks for that long, which is the
/// time to bind a socket.
///
/// With [`RdpSecurity::Tls`], the server is given no credentials of
/// its own, because IronRDP's built-in check compares the domain as
/// well; the provider checks the Client Info credentials instead and
/// fills the slot the display and input handlers read, and the slot
/// is emptied when the client disconnects. With [`RdpSecurity::Nla`],
/// CredSSP needs the plaintext pair, so the server is given
/// `config.credentials`, and the provider is asked once, now, for the
/// session every NLA client will be served.
pub fn serve(config: RdpConfig) -> Result<RdpServerHandle> {
    serve_preferring(config, DEFAULT_RDP_PORT)
}

/// [`serve`], trying `preferred` rather than [`DEFAULT_RDP_PORT`]
/// when the configuration names no port, so tests need not depend on
/// whether the host's port 3389 is free.
fn serve_preferring(config: RdpConfig, preferred: u16) -> Result<RdpServerHandle> {
    let RdpConfig {
        host,
        port,
        security,
        identity,
        credentials,
        provider,
    } = config;

    let port = port.unwrap_or_else(|| probe_port(host, preferred));
    let addr = SocketAddr::new(host, port);

    // Build everything that can fail on the caller's thread, so the
    // error reaches the caller rather than a log.
    let acceptor = identity.acceptor()?;
    let slot = SessionSlot::new();
    let mode = security;
    let security = match security {
        RdpSecurity::Tls => Security::Tls {
            acceptor,
            validator: Arc::new(ProviderValidator::new(provider, slot.clone())),
        },
        RdpSecurity::Nla => {
            let session = provider
                .session(&credentials)
                .map_err(|_| anyhow!("the session provider refused the configured credentials"))?;
            slot.set(session);
            Security::Nla {
                acceptor,
                pub_key: identity.pub_key,
                credentials,
            }
        }
    };

    let stopping = Arc::new(AtomicBool::new(false));
    let parts = ServerParts {
        addr,
        security,
        slot,
        stopping: Arc::clone(&stopping),
    };

    let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
    let (done_tx, done_rx) = std_mpsc::sync_channel(1);
    let (finished_tx, finished_rx) = watch::channel(());
    let thread = std::thread::Builder::new()
        .name(THREAD_NAME.to_string())
        .spawn(move || {
            // Dropped when this closure returns or unwinds, which is
            // what `RdpServerHandle::stopped` waits for.
            let _finished = finished_tx;
            let result = run_server(parts, ready_tx);
            if let Err(error) = &result {
                info!(
                    error = format!("{error:#}"),
                    "RDP server stopped with an error"
                );
            }
            let _ = done_tx.send(result);
        })
        .context("spawning the RDP server thread")?;

    let (local_addr, events) = match ready_rx.recv() {
        Ok(Ok(ready)) => ready,
        Ok(Err(error)) => {
            let _ = thread.join();
            return Err(error);
        }
        Err(_) => {
            let _ = thread.join();
            bail!("the RDP server thread exited before it was listening");
        }
    };
    info!(%local_addr, security = %mode, "RDP server listening");

    Ok(RdpServerHandle {
        local_addr,
        events,
        stopping,
        done: std::sync::Mutex::new(done_rx),
        finished: finished_rx,
        thread,
    })
}

/// `preferred` if a listener can be bound to it on `host` right now,
/// otherwise 0 so the server takes whatever port is free.
///
/// The probe listener is closed before the server binds, so another
/// process can take the port in between; the server then fails to
/// bind and [`serve`] reports it.
fn probe_port(host: IpAddr, preferred: u16) -> u16 {
    match TcpListener::bind(SocketAddr::new(host, preferred)) {
        Ok(_probe) => preferred,
        Err(error) => {
            info!(
                %host,
                port = preferred,
                %error,
                "RDP port is unavailable; using a free port instead"
            );
            0
        }
    }
}

/// The security mode, with what each needs, resolved from
/// [`RdpConfig`] on the caller's thread.
enum Security {
    Tls {
        acceptor: TlsAcceptor,
        validator: Arc<ProviderValidator>,
    },
    Nla {
        acceptor: TlsAcceptor,
        pub_key: Vec<u8>,
        credentials: Credentials,
    },
}

/// What the server thread builds the server from. All `Send`, unlike
/// the server itself.
struct ServerParts {
    addr: SocketAddr,
    security: Security,
    slot: SessionSlot,
    stopping: Arc<AtomicBool>,
}

/// What the server thread reports once the server is listening: the
/// bound address, and the sender the handle stops it with.
type Ready = (SocketAddr, mpsc::UnboundedSender<ServerEvent>);

/// The body of the server thread: build a runtime and the server,
/// report the bound address (or the failure to bind) on `ready`, and
/// run until stopped.
fn run_server(parts: ServerParts, ready: std_mpsc::SyncSender<Result<Ready>>) -> Result<()> {
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let error = anyhow::Error::new(error).context("building the RDP server runtime");
            let _ = ready.send(Err(anyhow!("{error:#}")));
            return Err(error);
        }
    };

    // IronRDP's own documentation runs the server inside a `LocalSet`,
    // which also lets any task it spawns locally run.
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let mut server = build_server(parts);
        let events = server.event_sender().clone();

        let (addr_tx, addr_rx) = oneshot::channel();
        // Cannot fail: the server holds the receiver.
        let _ = events.send(ServerEvent::GetLocalAddr(addr_tx));

        let mut run = std::pin::pin!(server.run());
        tokio::select! {
            result = &mut run => {
                // The server answers the address request only once it
                // is listening, so finishing first means it never was.
                let error = match result {
                    Err(error) => error.context("starting the RDP server"),
                    Ok(()) => anyhow!("the RDP server stopped before it was listening"),
                };
                let _ = ready.send(Err(anyhow!("{error:#}")));
                return Err(error);
            }
            addr = addr_rx => match addr {
                Ok(Some(addr)) => {
                    let _ = ready.send(Ok((addr, events)));
                }
                _ => {
                    let error = anyhow!("the RDP server did not report its address");
                    let _ = ready.send(Err(anyhow!("{error}")));
                    return Err(error);
                }
            },
        }

        run.await
    })
}

/// Build the IronRDP server for `parts`.
fn build_server(parts: ServerParts) -> RdpServer {
    let ServerParts {
        addr,
        security,
        slot,
        stopping,
    } = parts;

    let builder = RdpServer::builder().with_addr(addr);
    let (builder, validator, credentials, clear_on_disconnect) = match security {
        Security::Tls {
            acceptor,
            validator,
        } => (builder.with_tls(acceptor), Some(validator), None, true),
        Security::Nla {
            acceptor,
            pub_key,
            credentials,
        } => (
            builder.with_hybrid(acceptor, pub_key),
            None,
            Some(credentials),
            false,
        ),
    };

    let validator = validator.map(|v| v as Arc<dyn ironrdp_server::CredentialValidator>);
    let display = MirrorDisplay::new(slot.clone(), FALLBACK_DESKTOP_SIZE);
    let input = SpiceInputHandler::new(slot.clone());
    let connections = ConnectionLog {
        slot,
        client_size: display.client_size(),
        held: input.held(),
        dropped: input.dropped_counter(),
        clear_on_disconnect,
        stopping,
    };
    let mut server = builder
        .with_input_handler(input)
        .with_display_handler(display)
        .with_connection_handler(Some(Box::new(connections)))
        .with_credential_validator(validator)
        .build();
    // Left unset in TLS mode: the server would compare the Client
    // Info credentials to these including the domain, which clients
    // fill inconsistently, and the validator does the check instead.
    if credentials.is_some() {
        server.set_credentials(credentials);
    }
    server
}

/// Logs each connection's start and end, starts each connection with
/// no known client desktop size, releases whatever a leaving client
/// still held, empties the session slot when a TLS client leaves, and
/// stops the server's accept loop once a shutdown has been asked for.
struct ConnectionLog {
    slot: SessionSlot,
    /// The display handler's record of the size the client was told.
    /// Forgotten on accept, which runs before the server first asks
    /// the display for the connection's size, so that ask records the
    /// size this client is told.
    client_size: ClientSize,
    /// The input handler's record of held keys and buttons, ended
    /// with each connection.
    held: HeldInput,
    /// The input handler's dropped-event count, which releases that
    /// cannot be queued are added to.
    dropped: Arc<AtomicU64>,
    /// Whether to empty the slot on disconnect. True in TLS mode,
    /// where the credential check fills it per connection; false for
    /// NLA, where it is filled once when the server starts.
    clear_on_disconnect: bool,
    stopping: Arc<AtomicBool>,
}

impl ConnectionHandler for ConnectionLog {
    fn on_accept(&mut self, peer: SocketAddr) -> bool {
        if self.stopping.load(Ordering::Acquire) {
            info!(%peer, "RDP connection refused: the server is stopping");
            return false;
        }
        info!(%peer, "RDP client connected");
        self.client_size.forget();
        true
    }

    fn on_disconnected(
        &mut self,
        peer: SocketAddr,
        duration: Duration,
        error: Option<&anyhow::Error>,
    ) -> PostConnectionAction {
        match error {
            Some(error) => info!(
                %peer,
                ?duration,
                error = format!("{error:#}"),
                "RDP client disconnected"
            ),
            None => info!(%peer, ?duration, "RDP client disconnected"),
        }
        // Released before the slot is emptied, while the session the
        // client was using is still in it.
        // With the slot already empty the session is gone, and its
        // releases have nowhere to go, so they are let go uncounted.
        let releases = self.held.end_connection();
        if let Some(session) = self.slot.current() {
            if !releases.is_empty() {
                debug!(
                    count = releases.len(),
                    "releasing input the RDP client left held"
                );
            }
            let total = releases.len();
            for (sent, event) in releases.into_iter().enumerate() {
                if session.input_tx.try_send(event).is_err() {
                    let unsent = total - sent;
                    self.dropped.fetch_add(unsent as u64, Ordering::Relaxed);
                    warn!(
                        session = %session.label,
                        unsent,
                        "could not release keys or buttons the RDP client left held"
                    );
                    break;
                }
            }
        }
        if self.clear_on_disconnect {
            self.slot.clear();
        }
        // A stop request read by the connection's own event loop ends
        // the connection but not the accept loop, so the accept loop
        // is ended here.
        if self.stopping.load(Ordering::Acquire) {
            debug!("RDP server stopping after its client disconnected");
            PostConnectionAction::Stop
        } else {
            PostConnectionAction::Continue
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::Ipv4Addr;

    use ironrdp_server::{KeyboardEvent, RdpServerInputHandler};
    use shakenfist_spice_renderer::{InputEvent, SurfaceMirror};
    use tokio::sync::watch as tokio_watch;

    use crate::{SessionHandles, SingleSession};

    const JOIN_TIMEOUT: Duration = Duration::from_secs(10);

    fn install_crypto_provider() {
        // Losing an install race with another test is fine: all that
        // matters is that some provider is installed.
        let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
    }

    /// The configured pair. The password is minted once per test run
    /// rather than written into the source.
    fn credentials() -> Credentials {
        static PASSWORD: std::sync::LazyLock<String> =
            std::sync::LazyLock::new(crate::generate_password);
        Credentials {
            username: "ryll".to_string(),
            password: PASSWORD.clone(),
            domain: None,
        }
    }

    fn handles() -> Arc<SessionHandles> {
        let (input_tx, _) = mpsc::channel(1);
        let (_, mouse_mode) = tokio_watch::channel(0);
        let (resize_tx, _) = mpsc::channel(1);
        let mirror = SurfaceMirror::with_test_surface(0, 0, 64, 48, &[]);
        Arc::new(SessionHandles {
            surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
            input_tx,
            mouse_mode,
            resize_tx,
            label: "test".to_string(),
        })
    }

    fn config(port: Option<u16>, security: RdpSecurity) -> RdpConfig {
        install_crypto_provider();
        RdpConfig {
            host: IpAddr::V4(Ipv4Addr::LOCALHOST),
            port,
            security,
            identity: TlsIdentity::self_signed(&["127.0.0.1".to_string()])
                .expect("self-signed identity"),
            credentials: credentials(),
            provider: Arc::new(SingleSession::new(credentials(), handles())),
        }
    }

    #[test]
    fn serve_on_port_zero_listens_on_an_ephemeral_port_and_stops() {
        let handle = serve(config(Some(0), RdpSecurity::Tls)).expect("serve");
        let addr = handle.local_addr();
        assert_eq!(addr.ip(), IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert_ne!(addr.port(), 0);
        // Something is really listening there.
        std::net::TcpStream::connect(addr).expect("connect to the server");

        assert!(!handle.is_finished());
        handle.shutdown();
        handle.join(JOIN_TIMEOUT).expect("server stops cleanly");
    }

    #[test]
    fn serve_nla_listens_and_stops() {
        let handle = serve(config(Some(0), RdpSecurity::Nla)).expect("serve");
        assert_ne!(handle.local_addr().port(), 0);
        handle.shutdown();
        handle.join(JOIN_TIMEOUT).expect("server stops cleanly");
    }

    #[test]
    fn serve_without_a_port_falls_back_when_the_preferred_port_is_taken() {
        // Hold a port of our own as the "preferred" one, so the test
        // does not depend on whether this host's port 3389 is free.
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a port to hold");
        let held_port = held.local_addr().expect("held address").port();

        let handle = serve_preferring(config(None, RdpSecurity::Tls), held_port).expect("serve");
        let port = handle.local_addr().port();
        assert_ne!(port, 0);
        assert_ne!(port, held_port);

        handle.shutdown();
        handle.join(JOIN_TIMEOUT).expect("server stops cleanly");
        drop(held);
    }

    #[test]
    fn serve_without_a_port_uses_the_preferred_port_when_free() {
        // Find a free port, release it, and prefer it.
        let free = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .expect("bind")
            .local_addr()
            .expect("address")
            .port();
        let handle = serve_preferring(config(None, RdpSecurity::Tls), free).expect("serve");
        // Another process may have taken the port in between, in which
        // case the fallback is correct too; only a bound port matters.
        assert_ne!(handle.local_addr().port(), 0);
        handle.shutdown();
        handle.join(JOIN_TIMEOUT).expect("server stops cleanly");
    }

    #[test]
    fn serve_fails_when_the_given_port_is_taken() {
        let held = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).expect("bind a port to hold");
        let held_port = held.local_addr().expect("held address").port();
        let error =
            serve(config(Some(held_port), RdpSecurity::Tls)).expect_err("a taken port must fail");
        assert!(format!("{error:#}").contains("bind"), "{error:#}");
        drop(held);
    }

    #[test]
    fn serve_nla_fails_when_the_provider_refuses_the_configured_pair() {
        let mut config = config(Some(0), RdpSecurity::Nla);
        config.provider = Arc::new(SingleSession::new(
            Credentials {
                username: "someone-else".to_string(),
                password: credentials().password,
                domain: None,
            },
            handles(),
        ));
        let error = serve(config).expect_err("a refused pair must fail");
        assert!(format!("{error:#}").contains("refused"), "{error:#}");
    }

    #[tokio::test]
    async fn stopped_completes_after_shutdown() {
        let handle = serve(config(Some(0), RdpSecurity::Tls)).expect("serve");
        let stopped = handle.stopped();
        handle.shutdown();
        tokio::time::timeout(JOIN_TIMEOUT, stopped)
            .await
            .expect("server thread finishes");
        // The thread sends its result before the channel `stopped`
        // waits on closes, so the result is ready now, even though the
        // OS thread itself may take a moment longer to exit.
        handle.join(Duration::ZERO).expect("result is ready");
    }

    #[test]
    fn join_times_out_while_the_server_runs() {
        let handle = serve(config(Some(0), RdpSecurity::Tls)).expect("serve");
        // Not shut down, so the bounded join gives up. The thread is
        // left running until the test process exits.
        let error = handle
            .join(Duration::from_millis(50))
            .expect_err("a running server does not finish");
        assert!(format!("{error}").contains("did not stop"), "{error}");
    }

    #[test]
    fn accepting_a_connection_forgets_the_client_size() {
        let client_size = ClientSize::default();
        client_size.set(FALLBACK_DESKTOP_SIZE);
        let mut connections = ConnectionLog {
            slot: SessionSlot::new(),
            client_size: client_size.clone(),
            held: HeldInput::default(),
            dropped: Arc::default(),
            clear_on_disconnect: true,
            stopping: Arc::new(AtomicBool::new(false)),
        };
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 50000);
        assert!(connections.on_accept(peer));
        assert_eq!(client_size.get(), None);
    }

    #[test]
    fn a_disconnect_releases_held_input_before_emptying_the_slot() {
        let (input_tx, mut input_rx) = mpsc::channel(8);
        let (_mode_tx, mouse_mode) = tokio_watch::channel(0);
        let (resize_tx, _resize_rx) = mpsc::channel(1);
        let mirror = SurfaceMirror::with_test_surface(0, 0, 64, 48, &[]);
        let slot = SessionSlot::new();
        slot.set(Arc::new(SessionHandles {
            surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
            input_tx,
            mouse_mode,
            resize_tx,
            label: "test".to_string(),
        }));
        let mut input = SpiceInputHandler::new(slot.clone());
        let mut connections = ConnectionLog {
            slot: slot.clone(),
            client_size: ClientSize::default(),
            held: input.held(),
            dropped: input.dropped_counter(),
            clear_on_disconnect: true,
            stopping: Arc::new(AtomicBool::new(false)),
        };

        // The client presses A and leaves without releasing it.
        input.keyboard(KeyboardEvent::Pressed {
            code: 0x1E,
            extended: false,
        });
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 50000);
        connections.on_disconnected(peer, Duration::from_secs(1), None);

        assert!(matches!(input_rx.try_recv(), Ok(InputEvent::KeyDown(0x1E))));
        assert!(matches!(input_rx.try_recv(), Ok(InputEvent::KeyUp(0x9E))));
        assert!(input_rx.try_recv().is_err());
        assert!(slot.current().is_none());
    }

    #[test]
    fn releases_that_cannot_be_queued_are_counted_as_dropped() {
        // Room for one event: the first key-down fills the queue.
        let (input_tx, mut input_rx) = mpsc::channel(1);
        let (_mode_tx, mouse_mode) = tokio_watch::channel(0);
        let (resize_tx, _resize_rx) = mpsc::channel(1);
        let mirror = SurfaceMirror::with_test_surface(0, 0, 64, 48, &[]);
        let slot = SessionSlot::new();
        slot.set(Arc::new(SessionHandles {
            surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
            input_tx,
            mouse_mode,
            resize_tx,
            label: "test".to_string(),
        }));
        let mut input = SpiceInputHandler::new(slot.clone());
        let held = input.held();
        let mut connections = ConnectionLog {
            slot: slot.clone(),
            client_size: ClientSize::default(),
            held: held.clone(),
            dropped: input.dropped_counter(),
            clear_on_disconnect: true,
            stopping: Arc::new(AtomicBool::new(false)),
        };

        // A is queued; B finds the queue full and is dropped, but is
        // still recorded as held.
        for code in [0x1E, 0x30] {
            input.keyboard(KeyboardEvent::Pressed {
                code,
                extended: false,
            });
        }
        assert_eq!(input.dropped(), 1);
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 50000);
        connections.on_disconnected(peer, Duration::from_secs(1), None);

        // Neither release fits, so both are counted, and the record
        // is cleared regardless.
        assert_eq!(input.dropped(), 3);
        assert!(matches!(input_rx.try_recv(), Ok(InputEvent::KeyDown(0x1E))));
        assert!(input_rx.try_recv().is_err());
        assert!(held.end_connection().is_empty());
        assert!(slot.current().is_none());
    }

    #[test]
    fn security_names() {
        assert_eq!(RdpSecurity::Tls.to_string(), "tls");
        assert_eq!(RdpSecurity::Nla.to_string(), "nla");
    }
}
