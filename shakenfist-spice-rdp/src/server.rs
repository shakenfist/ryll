//! The server runner: listens for RDP clients on an OS thread of its
//! own with a current-thread runtime, because IronRDP's server type is
//! not `Send`, builds an IronRDP server for each client it accepts,
//! and hands the caller a `Send` handle to stop it and wait for it.
//!
//! IronRDP's own accept loop serves one connection at a time and
//! accepts no other until that one ends, so a second client could not
//! take a session over while the first is connected. The runner
//! therefore owns the listener and runs each connection through
//! IronRDP's per-connection entry point. Every connection has its own
//! server, handlers and session slot, and so its own event channel:
//! nothing a leaving connection queued can reach the next one.
//!
//! A client that has authenticated takes its session over from the
//! client already served it (see `Connection::take_over`). Before it
//! authenticates, a connection is bounded: only one client at a time
//! may be between being accepted and authenticating, others wait
//! unaccepted in the listen backlog, and one that has not
//! authenticated in time is dropped, so a stream of half-open
//! connections cannot each hold a TLS handshake and an encoder.

use std::fmt;
use std::future::Future;
use std::net::{IpAddr, SocketAddr, TcpListener};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc as std_mpsc, Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use ironrdp_server::{CredentialValidator, Credentials, DesktopSize, RdpServer, ServerEvent};
use tokio::net::{TcpSocket, TcpStream};
use tokio::sync::{oneshot, watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::JoinSet;
use tokio::time::Instant;
use tokio_rustls::TlsAcceptor;
use tracing::{debug, info, warn};

use crate::audio::SoundFactory;
use crate::auth::ProviderValidator;
use crate::clipboard::ClipboardFactory;
use crate::input::HeldInput;
use crate::tls::TlsIdentity;
use crate::{MirrorDisplay, SessionHandles, SessionProvider, SessionSlot, SpiceInputHandler};

/// The port RDP clients connect to when none is given, tried first
/// when [`RdpConfig::port`] is `None`.
pub const DEFAULT_RDP_PORT: u16 = 3389;

/// How long a client has, from being accepted, to authenticate, unless
/// [`RdpConfig::handshake_timeout`] says otherwise. Ample for a person
/// typing a password into a client that prompts for one during NLA.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

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

/// The listen backlog, which is where clients wait while another is
/// authenticating. The same as IronRDP's own listener uses.
const LISTENER_BACKLOG: u32 = 1024;

/// How long a connection asked to end (by a stop, or by another
/// client taking its session over) has to do so before it is dropped.
/// IronRDP reads the request between PDUs, so only a connection stuck
/// writing to a client that has stopped reading takes this long.
const QUIT_GRACE: Duration = Duration::from_secs(5);

/// How long the accept loop waits after the listener fails to accept,
/// so a persistent failure (out of file descriptors, say) is not
/// retried in a tight loop.
const ACCEPT_RETRY_DELAY: Duration = Duration::from_millis(100);

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
    /// How long a client has, from being accepted, to authenticate
    /// before it is dropped. While one client is authenticating the
    /// next is not accepted, so this also bounds how long a client
    /// that stalls can keep others waiting.
    /// [`DEFAULT_HANDSHAKE_TIMEOUT`] suits interactive use.
    pub handshake_timeout: Duration,
}

/// A running RDP server, on a thread of its own.
///
/// Dropping the handle does not stop the server; call
/// [`shutdown`](Self::shutdown) and then [`join`](Self::join).
pub struct RdpServerHandle {
    local_addr: SocketAddr,
    /// Raised to stop the server. The accept loop and every
    /// connection watch it.
    stop: watch::Sender<bool>,
    /// The server thread's result, sent as the thread finishes. A
    /// mutex only so the handle is `Sync`; nothing contends for it.
    done: Mutex<std_mpsc::Receiver<Result<()>>>,
    /// Closed when the server thread exits, however it exits.
    finished: watch::Receiver<()>,
    thread: JoinHandle<()>,
}

impl fmt::Debug for RdpServerHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RdpServerHandle")
            .field("local_addr", &self.local_addr)
            .field("stopping", &*self.stop.borrow())
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
    /// The server stops accepting clients and closes its listener, so
    /// clients still waiting in the listen backlog are never served.
    /// A client still in the connection handshake is dropped at once.
    /// A client in an established session is asked to disconnect, and
    /// is dropped if it has not within a few seconds. The server has
    /// stopped once every connection has ended.
    pub fn shutdown(&self) {
        self.stop.send_replace(true);
    }

    /// Wait up to `timeout` for the server thread to finish, and
    /// return the server's result.
    ///
    /// Blocks the calling thread. A server is finished only after
    /// [`shutdown`](Self::shutdown) or a failure, and its connections
    /// can take a few seconds to end after a shutdown (see
    /// [`shutdown`](Self::shutdown)), so the wait is bounded. On
    /// timeout an error is returned and the thread is left to end with
    /// the process.
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
                "the RDP server did not stop within {timeout:?}; a client connection may not \
                 have ended yet"
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
/// Each client is served on a connection of its own. A client that
/// authenticates takes its session over from the client already
/// served it, which is disconnected and released at once: from that
/// moment it forwards no input and is sent no more of the session's
/// display, and whatever keys and buttons it held are released.
/// Clients of different sessions do not affect one another. Only one
/// client at a time may be between being accepted and authenticating;
/// the rest wait in the listen backlog, and one that has not
/// authenticated within `config.handshake_timeout` is dropped. A
/// client that fails to authenticate leaves the client being served
/// alone.
///
/// With [`RdpSecurity::Tls`], the server is given no credentials of
/// its own, because IronRDP's built-in check compares the domain as
/// well; the provider checks each client's Client Info credentials
/// instead and chooses the session that connection is served. With
/// [`RdpSecurity::Nla`], CredSSP needs the plaintext pair, so the
/// server is given `config.credentials`, and the provider is asked
/// once, now, for the session every NLA client will be served.
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
        handshake_timeout,
    } = config;

    let port = port.unwrap_or_else(|| probe_port(host, preferred));
    let addr = SocketAddr::new(host, port);

    // Build everything that can fail on the caller's thread, so the
    // error reaches the caller rather than a log.
    let acceptor = identity.acceptor()?;
    let mode = security;
    let security = match security {
        RdpSecurity::Tls => Security::Tls { acceptor, provider },
        RdpSecurity::Nla => {
            let session = provider
                .session(&credentials)
                .map_err(|_| anyhow!("the session provider refused the configured credentials"))?;
            Security::Nla {
                acceptor,
                pub_key: identity.pub_key,
                credentials,
                session,
            }
        }
    };

    let (stop, stop_rx) = watch::channel(false);
    let parts = ServerParts {
        addr,
        security,
        handshake_timeout,
        stop: stop_rx,
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

    let local_addr = match ready_rx.recv() {
        Ok(Ok(local_addr)) => local_addr,
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
        stop,
        done: Mutex::new(done_rx),
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
        /// Asked for each client's session by that connection's
        /// credential check.
        provider: Arc<dyn SessionProvider>,
    },
    Nla {
        acceptor: TlsAcceptor,
        pub_key: Vec<u8>,
        credentials: Credentials,
        /// The session every client is served, chosen at startup.
        session: Arc<SessionHandles>,
    },
}

/// What the server thread builds each connection's server from. All
/// `Send`, unlike the server itself.
struct ServerParts {
    addr: SocketAddr,
    security: Security,
    handshake_timeout: Duration,
    stop: watch::Receiver<bool>,
}

/// The body of the server thread: build a runtime, bind the listener,
/// report the bound address (or the failure to bind) on `ready`, and
/// accept clients until stopped.
fn run_server(parts: ServerParts, ready: std_mpsc::SyncSender<Result<SocketAddr>>) -> Result<()> {
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

    // Each connection's server runs as a local task, since it is not
    // `Send`; IronRDP's own documentation runs its server inside a
    // `LocalSet` too.
    let local = tokio::task::LocalSet::new();
    local.block_on(&runtime, async move {
        let listener = match listen(parts.addr) {
            Ok(listener) => listener,
            Err(error) => {
                let error = error.context("starting the RDP server");
                // The caller gets a copy, since the error is not
                // `Clone` and is returned here too.
                let _ = ready.send(Err(anyhow!("{error:#}")));
                return Err(error);
            }
        };
        let _ = ready.send(Ok(listener.local_addr));
        accept_loop(listener, parts).await;
        Ok(())
    })
}

/// A bound listener and the address it is bound to.
struct Listener {
    socket: tokio::net::TcpListener,
    local_addr: SocketAddr,
}

/// Bind a listener on `addr` with the options IronRDP's own listener
/// uses.
fn listen(addr: SocketAddr) -> Result<Listener> {
    let socket = match addr {
        SocketAddr::V4(_) => TcpSocket::new_v4(),
        // Dual-stack where the platform makes it the default (Linux),
        // IPv6 only where it does not.
        SocketAddr::V6(_) => TcpSocket::new_v6(),
    }
    .context("creating the listening socket")?;
    // So a restarted server can bind while the last one's connections
    // are in TIME_WAIT. Not on Windows, where SO_REUSEADDR would let
    // another process bind the same port.
    #[cfg(unix)]
    socket.set_reuseaddr(true).context("setting SO_REUSEADDR")?;
    socket
        .bind(addr)
        .with_context(|| format!("binding {addr}"))?;
    let socket = socket.listen(LISTENER_BACKLOG).context("listening")?;
    let local_addr = socket.local_addr().context("reading the bound address")?;
    Ok(Listener { socket, local_addr })
}

/// Wait until a stop has been asked for. A handle dropped without
/// asking never stops the server, as [`RdpServerHandle`] documents.
async fn stop_requested(stop: &mut watch::Receiver<bool>) {
    if stop.wait_for(|stop| *stop).await.is_err() {
        std::future::pending::<()>().await;
    }
}

/// Accept clients, one authenticating at a time, until a stop is asked
/// for; then close the listener and wait for every connection to end.
async fn accept_loop(listener: Listener, parts: ServerParts) {
    let ServerParts {
        security,
        handshake_timeout,
        mut stop,
        ..
    } = parts;
    let served = Served::default();
    // The one place for a client between accept and authentication.
    let handshake = Arc::new(Semaphore::new(1));
    let mut connections = JoinSet::new();

    loop {
        let place = tokio::select! {
            biased;
            () = stop_requested(&mut stop) => break,
            place = Arc::clone(&handshake).acquire_owned() => {
                place.expect("the handshake semaphore is never closed")
            }
        };
        let (stream, peer) = tokio::select! {
            biased;
            () = stop_requested(&mut stop) => break,
            accepted = listener.socket.accept() => match accepted {
                Ok(accepted) => accepted,
                Err(error) => {
                    warn!(%error, "the RDP listener failed to accept a connection");
                    tokio::time::sleep(ACCEPT_RETRY_DELAY).await;
                    continue;
                }
            },
        };
        while let Some(ended) = connections.try_join_next() {
            report_connection_task(ended);
        }

        info!(%peer, "RDP client connected");
        let (server, connection, authenticated) =
            new_connection(listener.local_addr, peer, &security, &served);
        connections.spawn_local(serve_connection(
            server,
            stream,
            connection,
            served.clone(),
            Handshake {
                place,
                authenticated,
                timeout: handshake_timeout,
            },
            stop.clone(),
        ));
    }

    debug!("RDP server stopping: no more clients are accepted");
    drop(listener);
    while let Some(ended) = connections.join_next().await {
        report_connection_task(ended);
    }
}

/// Log a connection task that did not end normally. A task's own
/// outcome is logged by the task; this catches only a panic.
fn report_connection_task(ended: Result<(), tokio::task::JoinError>) {
    if let Err(error) = ended {
        warn!(%error, "an RDP connection task failed");
    }
}

/// Build the server, handlers and takeover record for a client just
/// accepted from `peer`. The receiver is answered once the client has
/// authenticated, which is also when it takes its session over.
fn new_connection(
    addr: SocketAddr,
    peer: SocketAddr,
    security: &Security,
    served: &Served,
) -> (RdpServer, Arc<Connection>, oneshot::Receiver<()>) {
    let slot = SessionSlot::new();
    let input = SpiceInputHandler::new(slot.clone());
    let connection = Arc::new(Connection::new(
        peer,
        slot,
        input.held(),
        input.dropped_counter(),
    ));
    let (authenticated_tx, authenticated_rx) = oneshot::channel();
    let authenticated = {
        let connection = Arc::clone(&connection);
        let served = served.clone();
        move || {
            connection.take_over(&served);
            // The connection's task may already have ended it.
            let _ = authenticated_tx.send(());
        }
    };
    let server = build_server(addr, security, &connection.slot, input, authenticated);
    (server, connection, authenticated_rx)
}

/// Build the IronRDP server for one connection, with handlers serving
/// `slot` and `input` as its input handler. `authenticated` is called
/// once the connection's client has authenticated.
fn build_server(
    addr: SocketAddr,
    security: &Security,
    slot: &SessionSlot,
    input: SpiceInputHandler,
    authenticated: impl FnOnce() + Send + Sync + 'static,
) -> RdpServer {
    // The address is required by IronRDP's builder but used only by
    // its own accept loop, which is not run.
    let builder = RdpServer::builder().with_addr(addr);
    let (builder, validator, credentials) = match security {
        Security::Tls { acceptor, provider } => {
            let validator: Arc<dyn CredentialValidator> =
                Arc::new(ProviderValidator::new(Arc::clone(provider), slot.clone()));
            (builder.with_tls(acceptor.clone()), Some(validator), None)
        }
        Security::Nla {
            acceptor,
            pub_key,
            credentials,
            session,
        } => {
            slot.set(Arc::clone(session));
            (
                builder.with_hybrid(acceptor.clone(), pub_key.clone()),
                None,
                Some(credentials.clone()),
            )
        }
    };

    let mut display = connection_display(slot.clone());
    display.when_authenticated(authenticated);
    let clipboard = ClipboardFactory::new(slot.clone());
    let sound = SoundFactory::new(slot.clone());
    let mut server = builder
        .with_input_handler(input)
        .with_display_handler(display)
        // Negotiate the desktop size the client asks for in its Client
        // Core Data rather than the surface's, so that the guest can be
        // asked for it. IronRDP still builds the encoder at the
        // surface's size, a mismatch that would drop the client; the
        // display handler records the negotiated size and sends no
        // bitmap until the two agree. Either the guest adopts the
        // client's size, or, once a short grace period after accept has
        // let the client's dynamic channels open, the display handler
        // resizes the client to the surface's.
        .with_honor_client_desktop_size(true)
        // Always offered; a session with no clipboard ignores it.
        .with_cliprdr_factory(Some(Box::new(clipboard)))
        // Always offered; a session with no audio declines every format.
        .with_sound_factory(Some(Box::new(sound)))
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

/// The display handler for a connection accepted now, with no known
/// client desktop size yet. The grace period before the client is
/// resized runs from now.
fn connection_display(slot: SessionSlot) -> MirrorDisplay {
    let display = MirrorDisplay::new(slot, FALLBACK_DESKTOP_SIZE);
    display.client_size().start_connection();
    display
}

/// What bounds a connection until its client authenticates.
struct Handshake {
    /// The one place for a client between accept and authentication,
    /// given back when this client authenticates or leaves.
    place: OwnedSemaphorePermit,
    /// Answered when the client authenticates.
    authenticated: oneshot::Receiver<()>,
    /// How long the client has, from being accepted, to authenticate.
    timeout: Duration,
}

/// How a connection's handshake ended.
enum HandshakeOutcome {
    /// The client authenticated, and is now served.
    Authenticated,
    /// The connection ended first, with this result.
    Ended(Result<()>),
    /// The client did not authenticate in time.
    TimedOut,
    /// The server is stopping.
    Stopped,
}

/// How a served connection's time came to an end.
enum ServedOutcome {
    /// The connection ended, with this result.
    Ended(Result<()>),
    /// The connection is to end, for this reason.
    ToEnd(&'static str),
}

/// Run one connection to its end: bounded by `handshake` until its
/// client authenticates, then until the client leaves, the server
/// stops, or another client takes its session over.
async fn serve_connection(
    mut server: RdpServer,
    stream: TcpStream,
    connection: Arc<Connection>,
    served: Served,
    handshake: Handshake,
    mut stop: watch::Receiver<bool>,
) {
    let started = Instant::now();
    let events = server.event_sender().clone();
    let mut taken_over = connection.taken_over.subscribe();
    let result = {
        let run = server.run_connection(stream);
        tokio::pin!(run);

        let Handshake {
            place,
            authenticated,
            timeout,
        } = handshake;
        let outcome = tokio::select! {
            result = &mut run => HandshakeOutcome::Ended(result),
            Ok(()) = authenticated => HandshakeOutcome::Authenticated,
            () = tokio::time::sleep(timeout) => HandshakeOutcome::TimedOut,
            () = stop_requested(&mut stop) => HandshakeOutcome::Stopped,
        };
        drop(place);

        match outcome {
            HandshakeOutcome::Ended(result) => result,
            HandshakeOutcome::TimedOut => Err(anyhow!(
                "the client did not authenticate within {timeout:?}"
            )),
            HandshakeOutcome::Stopped => Err(anyhow!(
                "the server stopped before the client authenticated"
            )),
            HandshakeOutcome::Authenticated => {
                let outcome = tokio::select! {
                    result = &mut run => ServedOutcome::Ended(result),
                    () = stop_requested(&mut stop) => {
                        ServedOutcome::ToEnd("the server is stopping")
                    }
                    _ = taken_over.wait_for(|taken| *taken) => {
                        ServedOutcome::ToEnd("another client took the session over")
                    }
                };
                match outcome {
                    ServedOutcome::Ended(result) => result,
                    ServedOutcome::ToEnd(reason) => {
                        debug!(peer = %connection.peer, reason, "asking the RDP connection to end");
                        // The server holds the receiver while `run` is
                        // alive, so this cannot fail.
                        let _ = events.send(ServerEvent::Quit(reason.to_string()));
                        match tokio::time::timeout(QUIT_GRACE, &mut run).await {
                            Ok(result) => result,
                            Err(_) => Err(anyhow!(
                                "the connection did not end within {QUIT_GRACE:?} of being \
                                 asked to ({reason}); dropped"
                            )),
                        }
                    }
                }
            }
        }
    };
    // Dropping the server closes the stream, if `run` had not, and
    // every channel the connection opened.
    drop(server);
    connection.end(&served);

    let peer = connection.peer;
    let duration = started.elapsed();
    match result {
        Ok(()) => info!(%peer, ?duration, "RDP client disconnected"),
        Err(error) => info!(
            %peer,
            ?duration,
            error = format!("{error:#}"),
            "RDP client disconnected"
        ),
    }
}

/// The connections being served, each with the session it took over.
/// Clones share the same list.
#[derive(Clone, Default)]
struct Served(Arc<Mutex<Vec<ServedConnection>>>);

/// A session, and the connection serving it.
type ServedConnection = (Arc<SessionHandles>, Arc<Connection>);

impl Served {
    fn lock(&self) -> MutexGuard<'_, Vec<ServedConnection>> {
        lock(&self.0)
    }
}

/// One accepted client, as its takeover and its end see it.
struct Connection {
    peer: SocketAddr,
    /// The session this connection's handlers serve. Filled by the
    /// credential check in TLS mode, and when the server is built for
    /// NLA; emptied when another client takes the session over, and
    /// when the connection ends.
    slot: SessionSlot,
    /// The input handler's record of held keys and buttons.
    held: HeldInput,
    /// The input handler's dropped-event count, which releases that
    /// cannot be queued are added to.
    dropped: Arc<AtomicU64>,
    /// The session the connection took over when its client
    /// authenticated. Kept when a takeover empties the slot, so input
    /// still held when the connection ends is released to it.
    session: Mutex<Option<Arc<SessionHandles>>>,
    /// Raised when another client takes the session over.
    taken_over: watch::Sender<bool>,
}

impl Connection {
    fn new(peer: SocketAddr, slot: SessionSlot, held: HeldInput, dropped: Arc<AtomicU64>) -> Self {
        Self {
            peer,
            slot,
            held,
            dropped,
            session: Mutex::new(None),
            taken_over: watch::channel(false).0,
        }
    }

    /// Make this connection the one serving the session in its slot,
    /// taking it over from the connection that was. Called when the
    /// client has authenticated, which is the first time IronRDP asks
    /// the display handler for its update stream, before the client
    /// is sent any pixels.
    ///
    /// Only a connection serving the same session is taken over, so a
    /// provider serving several sessions does not let the client of
    /// one disconnect the client of another.
    fn take_over(self: &Arc<Self>, served: &Served) {
        let Some(session) = self.slot.current() else {
            debug!(peer = %self.peer, "RDP client authenticated with no session chosen");
            return;
        };
        *lock(&self.session) = Some(Arc::clone(&session));
        let previous = {
            let mut served = served.lock();
            let previous = served
                .iter()
                .position(|(serving, _)| Arc::ptr_eq(serving, &session))
                .map(|index| served.swap_remove(index).1);
            served.push((Arc::clone(&session), Arc::clone(self)));
            previous
        };
        if let Some(previous) = previous {
            info!(
                peer = %self.peer,
                previous = %previous.peer,
                session = %session.label,
                "RDP client took the session over from the client being served"
            );
            previous.lose(&session);
        }
    }

    /// Stop serving `session`, which another client has taken over.
    /// From now on this connection's handlers find no session, so it
    /// forwards no input and takes no damage from the mirror; what its
    /// client held is released; and the connection is asked to end.
    fn lose(&self, session: &SessionHandles) {
        self.slot.clear();
        self.release_held(session);
        self.taken_over.send_replace(true);
    }

    /// Release, to `session`, whatever keys and buttons the client
    /// still holds, counting those the session's queue cannot take as
    /// dropped.
    fn release_held(&self, session: &SessionHandles) {
        let releases = self.held.end_connection();
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

    /// End the connection: it no longer serves any session, and what
    /// its client still held is released to the session it served. If
    /// it served none, the releases have nowhere to go and are
    /// discarded uncounted.
    fn end(self: &Arc<Self>, served: &Served) {
        let session = lock(&self.session).take().or_else(|| self.slot.current());
        self.slot.clear();
        served
            .lock()
            .retain(|(_, connection)| !Arc::ptr_eq(connection, self));
        match session {
            Some(session) => self.release_held(&session),
            None => drop(self.held.end_connection()),
        }
    }
}

/// Lock `mutex`. Nothing held under these locks can be left
/// half-written by a panic, so a poisoned lock is recovered rather
/// than propagated.
fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::net::Ipv4Addr;

    use ironrdp_server::{KeyboardEvent, RdpServerDisplay, RdpServerInputHandler};
    use shakenfist_spice_renderer::{InputEvent, SurfaceMirror};
    use tokio::sync::{mpsc, watch as tokio_watch};

    use crate::{CursorState, SingleSession};

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
        let (_, cursor) = tokio_watch::channel(CursorState::default());
        let (_, spice_up) = tokio_watch::channel(true);
        let (resize_tx, _) = mpsc::channel(1);
        let mirror = SurfaceMirror::with_test_surface(0, 0, 64, 48, &[]);
        Arc::new(SessionHandles {
            surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
            input_tx,
            mouse_mode,
            resize_tx,
            cursor,
            spice_up,
            clipboard: None,
            audio: None,
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
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        }
    }

    /// A session whose input queue holds `capacity` events, with the
    /// receiving end of that queue. Its mirror has one 64 x 48 primary
    /// surface.
    fn session_with_input(capacity: usize) -> (Arc<SessionHandles>, mpsc::Receiver<InputEvent>) {
        let (input_tx, input_rx) = mpsc::channel(capacity);
        let (_mode_tx, mouse_mode) = tokio_watch::channel(0);
        let (_, cursor) = tokio_watch::channel(CursorState::default());
        let (_, spice_up) = tokio_watch::channel(true);
        let (resize_tx, _resize_rx) = mpsc::channel(1);
        let mirror = SurfaceMirror::with_test_surface(0, 0, 64, 48, &[]);
        let handles = Arc::new(SessionHandles {
            surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
            input_tx,
            mouse_mode,
            resize_tx,
            cursor,
            spice_up,
            clipboard: None,
            audio: None,
            label: "test".to_string(),
        });
        (handles, input_rx)
    }

    /// A connection whose slot holds `session`, and its input handler.
    fn connection(session: &Arc<SessionHandles>) -> (Arc<Connection>, SpiceInputHandler) {
        let slot = SessionSlot::new();
        slot.set(Arc::clone(session));
        let input = SpiceInputHandler::new(slot.clone());
        let peer = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 50000);
        let connection = Arc::new(Connection::new(
            peer,
            slot,
            input.held(),
            input.dropped_counter(),
        ));
        (connection, input)
    }

    fn press(input: &mut SpiceInputHandler, code: u8) {
        input.keyboard(KeyboardEvent::Pressed {
            code,
            extended: false,
        });
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
    fn a_connection_starts_with_the_client_size_afresh() {
        let display = connection_display(SessionSlot::new());
        let client_size = display.client_size();
        assert_eq!(client_size.get(), None);
        assert!(client_size.resize_held_until().is_some());
    }

    #[test]
    fn an_ending_connection_releases_held_input_and_empties_its_slot() {
        let (session, mut input_rx) = session_with_input(8);
        let (connection, mut input) = connection(&session);

        // The client presses A and leaves without releasing it.
        press(&mut input, 0x1E);
        connection.end(&Served::default());

        assert!(matches!(input_rx.try_recv(), Ok(InputEvent::KeyDown(0x1E))));
        assert!(matches!(input_rx.try_recv(), Ok(InputEvent::KeyUp(0x9E))));
        assert!(input_rx.try_recv().is_err());
        assert!(connection.slot.current().is_none());
    }

    #[test]
    fn releases_that_cannot_be_queued_are_counted_as_dropped() {
        // Room for one event: the first key-down fills the queue.
        let (session, mut input_rx) = session_with_input(1);
        let (connection, mut input) = connection(&session);
        let held = input.held();

        // A is queued; B finds the queue full and is dropped, but is
        // still recorded as held.
        for code in [0x1E, 0x30] {
            press(&mut input, code);
        }
        assert_eq!(input.dropped(), 1);
        connection.end(&Served::default());

        // Neither release fits, so both are counted, and the record
        // is cleared regardless.
        assert_eq!(input.dropped(), 3);
        assert!(matches!(input_rx.try_recv(), Ok(InputEvent::KeyDown(0x1E))));
        assert!(input_rx.try_recv().is_err());
        assert!(held.end_connection().is_empty());
        assert!(connection.slot.current().is_none());
    }

    #[test]
    fn a_takeover_releases_the_old_client_and_stops_its_input_at_once() {
        let (session, mut input_rx) = session_with_input(8);
        let served = Served::default();
        let (old, mut old_input) = connection(&session);
        old.take_over(&served);
        press(&mut old_input, 0x1E);
        assert!(matches!(input_rx.try_recv(), Ok(InputEvent::KeyDown(0x1E))));

        let (new, _new_input) = connection(&session);
        new.take_over(&served);

        // What the old client held is released, and it is asked to
        // end.
        assert!(matches!(input_rx.try_recv(), Ok(InputEvent::KeyUp(0x9E))));
        assert!(*old.taken_over.borrow());
        assert!(!*new.taken_over.borrow());
        // From that moment it forwards nothing, though IronRDP has not
        // yet read the request to end.
        let dropped = old_input.dropped();
        press(&mut old_input, 0x30);
        assert!(input_rx.try_recv().is_err());
        assert_eq!(old_input.dropped(), dropped + 1);
        assert!(old.slot.current().is_none());
        // The new client is served the session.
        assert!(new
            .slot
            .current()
            .is_some_and(|current| Arc::ptr_eq(&current, &session)));
    }

    #[tokio::test(start_paused = true)]
    async fn a_takeover_stops_the_old_client_taking_damage_at_once() {
        let (session, _input_rx) = session_with_input(8);
        let served = Served::default();
        let (old, _old_input) = connection(&session);
        old.take_over(&served);
        let mut display = MirrorDisplay::new(old.slot.clone(), FALLBACK_DESKTOP_SIZE);
        display.size().await;
        let mut updates = display.updates().await.expect("updates");
        // The default pointer, then the whole surface: the old client
        // is being served.
        for _ in 0..2 {
            tokio::time::timeout(Duration::from_secs(1), updates.next_update())
                .await
                .expect("an update")
                .expect("next_update");
        }

        let (new, _new_input) = connection(&session);
        new.take_over(&served);
        session
            .surface_mirror
            .lock()
            .await
            .primary_surface_mut()
            .expect("surface")
            .blit(0, 0, 4, 4, &[0x55; 4 * 4 * 4]);

        // The old stream sends nothing more, and leaves the damage in
        // the mirror for the new client's stream.
        if let Ok(update) =
            tokio::time::timeout(Duration::from_secs(1), updates.next_update()).await
        {
            panic!("the old client was sent {update:?} after the takeover");
        }
        let mut mirror = session.surface_mirror.lock().await;
        let damage = mirror.primary_surface_mut().expect("surface").take_damage();
        assert!(!damage.rects().is_empty(), "the old client took the damage");
    }

    #[test]
    fn a_takeover_leaves_the_client_of_another_session_alone() {
        let (first, mut first_rx) = session_with_input(8);
        let (second, _second_rx) = session_with_input(8);
        let served = Served::default();
        let (one, mut one_input) = connection(&first);
        one.take_over(&served);
        press(&mut one_input, 0x1E);
        assert!(matches!(first_rx.try_recv(), Ok(InputEvent::KeyDown(0x1E))));

        let (other, _other_input) = connection(&second);
        other.take_over(&served);

        assert!(!*one.taken_over.borrow());
        assert!(first_rx.try_recv().is_err(), "nothing was released");
        press(&mut one_input, 0x30);
        assert!(matches!(first_rx.try_recv(), Ok(InputEvent::KeyDown(0x30))));
    }

    #[test]
    fn an_ended_connection_is_not_taken_over() {
        let (session, mut input_rx) = session_with_input(8);
        let served = Served::default();
        let (old, mut old_input) = connection(&session);
        old.take_over(&served);
        press(&mut old_input, 0x1E);
        old.end(&served);
        while input_rx.try_recv().is_ok() {}

        let (new, _new_input) = connection(&session);
        new.take_over(&served);
        assert!(!*old.taken_over.borrow());
        assert!(input_rx.try_recv().is_err());
        assert_eq!(served.lock().len(), 1);
    }

    #[test]
    fn security_names() {
        assert_eq!(RdpSecurity::Tls.to_string(), "tls");
        assert_eq!(RdpSecurity::Nla.to_string(), "nla");
    }
}
