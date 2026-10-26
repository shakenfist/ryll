//! `--rdp` mode: serve the SPICE session to stock RDP clients.
//!
//! The SPICE side is built as `--web` builds it (the same host-side
//! scaffolding, event bus, surface mirror and optional control
//! socket); the RDP side is `shakenfist_spice_rdp::serve`, which
//! runs IronRDP on a thread of its own and reads the session through
//! the mirror, the cursor relay, the clipboard, the audio relay and
//! the input, mouse-mode, resize and liveness channels.
//!
//! The RDP side outlives the SPICE connection. When an established
//! SPICE session is lost, the RDP listener and any connected client
//! stay up, the client is shown the last frame darkened, and ryll
//! reconnects with the GUI's retry rules (`crate::reconnect_policy`).
//! [`supervise`] runs the attempts. `run_connection` consumes its
//! channels, so each attempt has its own, and a [`SessionLink`]
//! connects them to the fixed ones the RDP server and the control
//! socket hold. `--rdp` exits once retrying is forbidden or
//! exhausted, or when the first attempt never reaches a session.

use std::future::Future;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use shakenfist_spice_protocol::link::H264Offer;
use shakenfist_spice_protocol::ConnectionConfig;
use shakenfist_spice_rdp::{
    generate_password, serve, spawn_cursor_relay, AudioRelay, Credentials, RdpConfig, RdpSecurity,
    SessionHandles, SingleSession, TlsIdentity, DEFAULT_HANDSHAKE_TIMEOUT,
};
use shakenfist_spice_renderer::channels::VolumeControl;
use shakenfist_spice_renderer::{
    ByteCounter, CancellationToken, CaptureSink, ChannelEvent, ClipboardBackend, DisplaySize,
    InputEvent, LogConfig, PlaybackOutput, SessionState, SurfaceMirror, TrafficSink,
    MOUSE_MODE_UNKNOWN,
};
use tokio::sync::{mpsc, watch, Notify};
use tracing::{info, warn};

use crate::bugreport::{AppSnapshot, BugReport, ChannelSnapshots, PedanticConfig, TrafficBuffers};
use crate::capture::CaptureSession;
use crate::config::{Args, Config, RdpSecurityMode, ShareDirConfig, VirtualDiskConfig};
use crate::notifications::{
    register_gap_notification_observer, NotificationStore, SharedNotifications,
};
use crate::reconnect_policy::{
    next_step, GiveUp, ReconnectPolicy, RetryDecision, TicketBlock, MAX_RECONNECT_ATTEMPTS,
};
use crate::{mib_to_usize_bytes, settings, shutdown_to_cancel_bridge, SHUTDOWN_REQUESTED};

/// Capacity of the `InputEvent` queue the RDP input handler feeds.
/// The handler cannot wait for space, so a full queue drops the
/// event; this is sized like the browser's queue in `--web`.
const INPUT_CHANNEL_CAPACITY: usize = 256;

/// Capacity of the guest display size queue.
const RESIZE_CHANNEL_CAPACITY: usize = 16;

/// How long to wait for the RDP server thread, and then for the
/// SPICE side, to finish once asked to stop.
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(2);

/// The names a generated self-signed certificate carries: the
/// listen address, then `localhost`. A wildcard bind address is never
/// what a client connects to, so it is left out.
fn certificate_names(host: IpAddr) -> Vec<String> {
    let mut names = vec!["localhost".to_string()];
    if !host.is_unspecified() {
        names.insert(0, host.to_string());
    }
    names
}

/// What ended the wait for the session.
enum Ended {
    /// Ctrl+C.
    ShutdownRequested,
    /// The SPICE supervisor returned, with its result.
    Spice(std::result::Result<Result<()>, tokio::task::JoinError>),
    /// The RDP server stopped on its own.
    ServerStopped,
}

/// One attempt's input and resize queues, which the RDP side's are
/// relayed to while the attempt's session exists.
struct Routes {
    input: mpsc::Sender<InputEvent>,
    resize: mpsc::Sender<DisplaySize>,
}

/// The fixed ends of the SPICE session that the RDP server and the
/// control socket hold, and which attempt they are connected to.
///
/// Input and resize requests are relayed (see [`relay`]) to the
/// attempt whose session exists, and dropped and counted while none
/// does, rather than queued for a session that does not exist yet.
/// Each attempt's mouse mode and agent state are copied into the
/// fixed watches (see [`forward_watch`]).
struct SessionLink {
    mirror: Arc<tokio::sync::Mutex<SurfaceMirror>>,
    /// Where the input relay sends: the current session's queue, or
    /// `None` while there is no session.
    input: watch::Sender<Option<mpsc::Sender<InputEvent>>>,
    /// Where the resize relay sends, likewise.
    resize: watch::Sender<Option<mpsc::Sender<DisplaySize>>>,
    mouse_mode: watch::Sender<u32>,
    agent_connected: watch::Sender<bool>,
    /// What the RDP display handler reads to darken the frame.
    spice_up: watch::Sender<bool>,
    /// What a control socket's `status` verb reports.
    connected: Arc<AtomicBool>,
}

impl SessionLink {
    /// A link for the first attempt, whose session is not up yet but
    /// is not an outage either: the display handler shows a fallback
    /// desktop until the first surface arrives, and `status` reports
    /// SPICE connected while it is dialled, as it always has.
    fn new(mirror: Arc<tokio::sync::Mutex<SurfaceMirror>>) -> Self {
        Self {
            mirror,
            input: watch::channel(None).0,
            resize: watch::channel(None).0,
            mouse_mode: watch::channel(MOUSE_MODE_UNKNOWN).0,
            agent_connected: watch::channel(false).0,
            spice_up: watch::channel(true).0,
            connected: Arc::new(AtomicBool::new(true)),
        }
    }

    /// Relay the RDP side's input and resize requests to `routes`.
    fn route(&self, routes: Routes) {
        self.input.send_replace(Some(routes.input));
        self.resize.send_replace(Some(routes.resize));
    }

    /// An attempt's session has begun. Drop every surface of the
    /// previous session from the mirror, so none lingers beside the
    /// new session's, and relay the RDP side to `routes`, if given.
    ///
    /// The previous attempt handed its last event to the bus before
    /// the backoff that preceded this one, and the caller holds this
    /// session's events back until this returns, so the mirror is
    /// emptied between the two sessions' surfaces.
    async fn session_began(&self, routes: Option<Routes>) {
        self.mirror.lock().await.surfaces.clear();
        if let Some(routes) = routes {
            self.route(routes);
        }
        self.connected.store(true, Ordering::Relaxed);
    }

    /// The session's first surface is on its way to the mirror. A
    /// display handler that looks before the mirror has it finds no
    /// surface and waits for one, so this need not wait either.
    fn surface_arrived(&self) {
        self.spice_up
            .send_if_modified(|up| !std::mem::replace(up, true));
    }

    /// The attempt has ended, with or without a session. Until the
    /// next session begins, what the RDP side sends is dropped, the
    /// client is shown the last frame darkened, and `status` reports
    /// SPICE down.
    fn session_ended(&self) {
        self.input.send_replace(None);
        self.resize.send_replace(None);
        self.agent_connected.send_replace(false);
        self.connected.store(false, Ordering::Relaxed);
        self.spice_up
            .send_if_modified(|up| std::mem::replace(up, false));
    }
}

/// Forward what the RDP side sends on `rx` to the session `route`
/// names, dropping it while there is none or that session's queue
/// has closed. Logs how many were dropped once one gets through
/// again, and returns how many were dropped in all once every sender
/// of `rx` has gone.
async fn relay<T>(
    mut rx: mpsc::Receiver<T>,
    route: watch::Receiver<Option<mpsc::Sender<T>>>,
    what: &str,
) -> u64 {
    let mut dropped = 0;
    let mut unreported = 0;
    while let Some(item) = rx.recv().await {
        let to = route.borrow().clone();
        let delivered = match to {
            Some(to) => to.send(item).await.is_ok(),
            None => false,
        };
        if !delivered {
            dropped += 1;
            unreported += 1;
        } else if unreported > 0 {
            info!(
                "rdp: dropped {} {} while the SPICE session was down",
                unreported, what
            );
            unreported = 0;
        }
    }
    dropped
}

/// Copy every value of one attempt's state watch to the fixed one the
/// RDP side reads, until the attempt's session drops its sender.
async fn forward_watch<T: Clone>(mut from: watch::Receiver<T>, to: &watch::Sender<T>) {
    loop {
        to.send_replace(from.borrow_and_update().clone());
        if from.changed().await.is_err() {
            break;
        }
    }
}

/// Pass one attempt's events on to the event bus, in order, telling
/// `link` when the attempt's session begins and when its first surface
/// arrives. `routes`, if given, are relayed to once the session
/// begins.
///
/// Returns whether a session began, once the attempt has dropped every
/// sender of `events`.
async fn pump_events(
    mut events: mpsc::Receiver<ChannelEvent>,
    bus: mpsc::Sender<ChannelEvent>,
    link: &SessionLink,
    mut routes: Option<Routes>,
) -> bool {
    let mut began = false;
    let mut surface = false;
    while let Some(event) = events.recv().await {
        if !began && matches!(event, ChannelEvent::SessionInitialized(_)) {
            link.session_began(routes.take()).await;
            began = true;
        }
        let first_surface = began
            && !surface
            && matches!(
                event,
                ChannelEvent::SurfaceCreated { .. } | ChannelEvent::ImageReady { .. }
            );
        // The bus closes only at teardown; the attempt is not held up
        // then.
        let _ = bus.send(event).await;
        if first_surface {
            link.surface_arrived();
            surface = true;
        }
    }
    began
}

/// How one SPICE attempt ended.
struct AttemptEnd {
    /// Whether the attempt reached a session: the server accepted the
    /// link and announced one.
    reached_session: bool,
    /// What `run_connection` returned.
    result: Result<()>,
}

/// Everything a SPICE attempt is built from that stays the same from
/// one attempt to the next.
struct SpiceAttempts {
    connection: ConnectionConfig,
    link: Arc<SessionLink>,
    /// The event bus's queue; each attempt's events reach it through
    /// [`pump_events`].
    bus: mpsc::Sender<ChannelEvent>,
    repaint_notify: Arc<Notify>,
    virtual_disks: Vec<VirtualDiskConfig>,
    share_dir: Option<ShareDirConfig>,
    capture: Option<Arc<dyn CaptureSink>>,
    byte_counter: Arc<ByteCounter>,
    traffic: Arc<dyn TrafficSink>,
    snapshots: ChannelSnapshots,
    monitors: u8,
    volume_control: Arc<VolumeControl>,
    log_config: LogConfig,
    cancel: Arc<AtomicBool>,
    clipboard: Option<Arc<dyn ClipboardBackend>>,
    playback: PlaybackOutput,
    h264: H264Offer,
    image_cache_cap_bytes: usize,
    glz_dictionary_cap_bytes: usize,
    preferred_compression: u8,
}

impl SpiceAttempts {
    /// Run attempt `number` (from 1) until its session ends, or until
    /// it fails to reach one.
    async fn run(&self, number: u32) -> AttemptEnd {
        info!("rdp: SPICE attempt {}: connecting", number);
        let (event_tx, event_rx) =
            mpsc::channel(shakenfist_spice_renderer::session::EVENT_CHANNEL_SIZE);
        let (input_tx, input_rx) = mpsc::channel(INPUT_CHANNEL_CAPACITY);
        let (resize_tx, resize_rx) = mpsc::channel(RESIZE_CHANNEL_CAPACITY);
        // USB and WebDAV stay closed: an RDP client cannot drive them.
        let (_, usb_rx) = mpsc::channel(1);
        let (_, webdav_rx) = mpsc::channel(1);
        let session_state = SessionState::new();
        let state = session_state.subscribe();

        let routes = Routes {
            input: input_tx,
            resize: resize_tx,
        };
        // The first attempt is relayed to from the start, as it always
        // was: there has been no outage, and a client that connected
        // before SPICE did expects its first requests, its desktop size
        // among them, to reach the session. A later attempt is relayed
        // to once its session begins.
        let routes = if number == 1 {
            self.link.route(routes);
            None
        } else {
            Some(routes)
        };

        let connection = shakenfist_spice_renderer::run_connection(
            self.connection.clone(),
            event_tx,
            session_state,
            self.repaint_notify.clone(),
            input_rx,
            usb_rx,
            webdav_rx,
            self.virtual_disks.clone(),
            self.share_dir.clone(),
            self.capture.clone(),
            self.byte_counter.clone(),
            self.traffic.clone(),
            self.snapshots.clone(),
            self.monitors,
            resize_rx,
            self.volume_control.clone(),
            /* enable_paste */ false,
            self.log_config,
            self.cancel.clone(),
            self.clipboard.clone(),
            self.playback.clone(),
            self.h264,
            self.image_cache_cap_bytes,
            self.glz_dictionary_cap_bytes,
            self.preferred_compression,
        );
        let (result, reached_session, (), ()) = tokio::join!(
            connection,
            pump_events(event_rx, self.bus.clone(), &self.link, routes),
            forward_watch(state.mouse_mode, &self.link.mouse_mode),
            forward_watch(state.agent_connected, &self.link.agent_connected),
        );
        self.link.session_ended();
        AttemptEnd {
            reached_session,
            result,
        }
    }
}

/// Run SPICE attempts until retrying is forbidden or exhausted, or
/// `stop` is cancelled. `attempt` runs attempt `n`, counting from 1,
/// to its end.
///
/// A first attempt that never reaches a session ends the run, as it
/// does in the GUI: it is usually a mistyped address or a server that
/// is down. After that, each lost session or failed attempt asks
/// [`next_step`] what to do, and a session that is reached restores
/// the retry budget. Giving up ends the run, so there is never an
/// earlier give-up for the flap rule to count from.
///
/// Returns the last attempt's error when it gives up, or `Ok` if that
/// attempt's session ended without one, or if `stop` was cancelled.
async fn supervise<F, Fut>(
    policy: ReconnectPolicy,
    stop: CancellationToken,
    mut attempt: F,
) -> Result<()>
where
    F: FnMut(u32) -> Fut,
    Fut: Future<Output = AttemptEnd>,
{
    let mut number = 1;
    let mut failed = 0u8;
    loop {
        let end = tokio::select! {
            end = attempt(number) => end,
            () = stop.cancelled() => return Ok(()),
        };
        match (&end.result, end.reached_session) {
            (Ok(()), true) => info!("rdp: SPICE attempt {}: the session ended", number),
            (Err(e), true) => warn!("rdp: SPICE attempt {}: the session ended: {:#}", number, e),
            (Ok(()), false) => warn!("rdp: SPICE attempt {}: no session", number),
            (Err(e), false) => warn!("rdp: SPICE attempt {}: failed: {:#}", number, e),
        }
        if number == 1 && !end.reached_session {
            return end.result.context("the SPICE session ended");
        }

        failed = if end.reached_session {
            0
        } else {
            failed.saturating_add(1)
        };
        let wait = match next_step(&policy, Instant::now(), SystemTime::now(), None, failed) {
            RetryDecision::RetryAfter(wait) => wait,
            RetryDecision::GiveUp(why) => return give_up(why, end.result),
        };
        info!(
            "rdp: reconnecting to SPICE in {}s (retry {} of {})",
            wait.as_secs(),
            failed + 1,
            MAX_RECONNECT_ATTEMPTS
        );
        tokio::select! {
            () = tokio::time::sleep(wait) => {}
            () = stop.cancelled() => return Ok(()),
        }
        // A long wait can outlive the ticket, and the server would
        // reject it.
        if let Some(block) = policy.forbid_retry(SystemTime::now()) {
            return give_up(GiveUp::Ticket(block), end.result);
        }
        number += 1;
    }
}

/// Stop reconnecting for `why`, ending the run with the last attempt's
/// result. Running out of attempts is always an error, even when the
/// last one ended without one: the session is gone and ryll could not
/// get it back.
fn give_up(why: GiveUp, last: Result<()>) -> Result<()> {
    let reason = match why {
        GiveUp::Exhausted => {
            format!("{MAX_RECONNECT_ATTEMPTS} attempts to reconnect have failed")
        }
        GiveUp::Ticket(TicketBlock::SingleUse) => {
            "the ticket was single-use, so ryll cannot reconnect".to_string()
        }
        GiveUp::Ticket(TicketBlock::Expired { expired_at }) => format!(
            "the ticket expired at {} (seconds since the Unix epoch)",
            expired_at
                .duration_since(UNIX_EPOCH)
                .map(|since| since.as_secs())
                .unwrap_or(0)
        ),
    };
    warn!("rdp: not reconnecting to SPICE: {}", reason);
    let last = match (why, last) {
        (GiveUp::Exhausted, Ok(())) => Err(anyhow!("the last attempt reached no session")),
        (_, last) => last,
    };
    last.with_context(|| format!("the SPICE session ended, and {reason}"))
}

pub fn run_rdp(
    config: Config,
    args: &Args,
    virtual_disks: Vec<VirtualDiskConfig>,
    share_dir: Option<ShareDirConfig>,
    capture: Option<Arc<CaptureSession>>,
    pedantic_config: Option<PedanticConfig>,
) -> Result<()> {
    info!("Running in RDP mode");

    let rdp_host = args.rdp_host;
    let rdp_port = args.rdp_port;
    let security = match args.rdp_security {
        RdpSecurityMode::Tls => RdpSecurity::Tls,
        RdpSecurityMode::Nla => RdpSecurity::Nla,
    };
    let credentials = Credentials {
        username: args.rdp_user.clone(),
        password: args.rdp_password.clone().unwrap_or_else(generate_password),
        domain: None,
    };
    // Loaded or generated before anything connects, so an unreadable
    // certificate fails the launch. Clap's `requires` enforces
    // both-or-neither for the two flags.
    let (identity, generated_certificate) = match (&args.rdp_tls_cert, &args.rdp_tls_key) {
        (Some(cert), Some(key)) => (TlsIdentity::from_pem_files(cert, key)?, false),
        _ => (
            TlsIdentity::self_signed(&certificate_names(rdp_host))?,
            true,
        ),
    };
    let monitors = args.monitors;
    let h264 = args.h264_offer();
    let image_cache_cap_bytes = mib_to_usize_bytes(args.image_cache_cap_mib);
    let glz_dictionary_cap_bytes = mib_to_usize_bytes(args.glz_dictionary_cap_mib);
    let preferred_compression = args.preferred_compression.wire_value();
    // The .vv file's ticket rules, as the GUI applies them.
    let policy = ReconnectPolicy {
        ticket_is_single_use: config.ticket_is_single_use,
        ticket_valid_until: config.ticket_valid_until,
    };

    // --control-socket is Unix-only (see config.rs). Bound before the
    // runtime starts, as in `--web`, so a socket that cannot be
    // created fails the launch.
    #[cfg(unix)]
    let control_server = args
        .control_socket
        .clone()
        .map(|path| {
            shakenfist_spice_renderer::control::Server::bind(path.clone())
                .with_context(|| format!("cannot create control socket {}", path.display()))
        })
        .transpose()?;

    let runtime =
        tokio::runtime::Runtime::new().context("failed to construct tokio runtime for --rdp")?;

    let capture_for_renderer = capture.clone();
    let result = runtime.block_on(async move {
        // Host-side scaffolding, the same shape as `run_web`.
        let byte_counter = Arc::new(ByteCounter::new());
        let traffic = Arc::new(TrafficBuffers::new());
        let notifications: SharedNotifications =
            Arc::new(std::sync::Mutex::new(NotificationStore::new()));
        let snapshots = ChannelSnapshots::new();
        // For a control socket's `stats` verb.
        #[cfg(unix)]
        let display_snapshot = snapshots.display.clone();
        #[cfg(unix)]
        let playback_snapshot = snapshots.playback.clone();
        let connection_config: ConnectionConfig = (&config).into();
        // `display_target()` keeps a tunnelled connection's signed
        // pseudo-hostname out of logs and bug reports.
        let session_label = connection_config.display_target();

        if let Some(pedantic) = pedantic_config {
            let app_snapshot = Arc::new(std::sync::Mutex::new(AppSnapshot::default()));
            warn!(
                "pedantic mode in rdp: traffic pcap and channel-state are \
                 live, but app-level snapshot (surfaces list, bandwidth, \
                 latency) is not populated — that field is updated by the \
                 GUI loop only."
            );
            BugReport::register_pedantic_observer(
                pedantic,
                session_label.clone(),
                traffic.clone(),
                snapshots.clone(),
                app_snapshot,
                notifications.clone(),
            );
        }
        register_gap_notification_observer(notifications.clone());

        let traffic_dyn: Arc<dyn TrafficSink> = traffic.clone() as Arc<dyn TrafficSink>;
        let capture_dyn: Option<Arc<dyn CaptureSink>> =
            capture_for_renderer.map(|c| c as Arc<dyn CaptureSink>);
        let log_config = settings::log_config();

        // The event bus's queue, fed by each SPICE attempt in turn.
        let (bus_tx, bus_rx) =
            mpsc::channel::<ChannelEvent>(shakenfist_spice_renderer::session::EVENT_CHANNEL_SIZE);
        // The input and resize queues for the whole run. The RDP input
        // handler and a control socket share them, so both paths land
        // on one ordering, and relays pass them on to the attempt with
        // a session.
        let (input_tx, input_rx) = mpsc::channel::<InputEvent>(INPUT_CHANNEL_CAPACITY);
        #[cfg(unix)]
        let input_tx_for_control = input_tx.clone();
        let (resize_tx, resize_rx) = mpsc::channel::<DisplaySize>(RESIZE_CHANNEL_CAPACITY);
        #[cfg(unix)]
        let resize_tx_for_control = resize_tx.clone();
        // One in-memory clipboard for the run, which the main channel
        // ferries to and from the guest's agent. The RDP client's
        // clipboard channel and, when there is one, a control socket
        // both read and write it, and text set while SPICE is down
        // reaches the agent once it is back.
        let clipboard = Arc::new(shakenfist_spice_renderer::MemoryClipboard::new());
        let clipboard_for_conn = Some(clipboard.clone() as Arc<dyn ClipboardBackend>);

        // The bus's mirror task keeps the surface mirror the RDP
        // display handler reads damage from; the mirror subscribes
        // itself when the bus is built. The cursor relay keeps the
        // cursor shape the display handler sends, and subscribes here,
        // before the session is spawned, because the first shape
        // arrives during session setup.
        let event_bus = shakenfist_spice_renderer::EventBus::new(
            shakenfist_spice_renderer::session::EVENT_BROADCAST_CAPACITY,
        );
        let (cursor, cursor_relay) = spawn_cursor_relay(event_bus.subscribe());
        #[cfg(unix)]
        let event_broadcast_tx = event_bus.sender();
        let surface_mirror = event_bus.mirror();

        // The guest's audio goes to the RDP client, through a relay
        // the client's audio channel listens to, rather than to this
        // host, which is not where the user is.
        let audio = Arc::new(AudioRelay::new());
        let playback = PlaybackOutput::Sink(audio.clone());

        // `run_connection` requires these even though nothing here
        // repaints a window or changes the volume.
        let repaint_notify = Arc::new(Notify::new());
        let volume_control = VolumeControl::new();

        let link = Arc::new(SessionLink::new(surface_mirror.clone()));
        let input_relay = tokio::spawn(relay(input_rx, link.input.subscribe(), "input events"));
        let resize_relay =
            tokio::spawn(relay(resize_rx, link.resize.subscribe(), "resize requests"));

        // Start the RDP server before dialling SPICE, so a port that
        // cannot be bound or an unusable certificate fails the launch
        // with nothing to tear down. Until the first surface arrives
        // the display handler announces a fallback desktop size and
        // resizes the client once the guest's is known.
        let handles = Arc::new(SessionHandles {
            surface_mirror: surface_mirror.clone(),
            input_tx,
            mouse_mode: link.mouse_mode.subscribe(),
            resize_tx,
            cursor,
            spice_up: link.spice_up.subscribe(),
            clipboard: Some(clipboard.clone()),
            audio: Some(audio),
            label: session_label,
        });
        let username = credentials.username.clone();
        let password = credentials.password.clone();
        let fingerprint = identity.fingerprint().to_string();
        let provider = Arc::new(SingleSession::new(credentials.clone(), handles));
        // `serve` blocks only while the listener binds, and this
        // thread is the runtime's `block_on` thread, not a worker.
        let server = serve(RdpConfig {
            host: rdp_host,
            port: rdp_port,
            security,
            identity,
            credentials,
            provider,
            handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
        })?;
        let local_addr = server.local_addr();

        // Operator-visible: what to type into the RDP client. Goes to
        // stdout only, never through the tracing pipeline, so the
        // password does not reach journald or log aggregators.
        // audit-allow-println: operator-facing launch lines, not for logs
        println!("RDP: {local_addr}");
        println!("user: {username}");
        println!("password: {password}");
        println!("security: {}", security.as_str());
        if generated_certificate {
            println!("certificate SHA-256: {fingerprint}");
        }
        // Structured-log breadcrumb: the bind address only.
        info!("rdp: listening on {} ({})", local_addr, security);

        // Cancel flag bridged from the process-global
        // `SHUTDOWN_REQUESTED`, as in `--web`. The bridge returns once
        // the flag is up, so its handle doubles as the Ctrl+C wait
        // below.
        let cancel = Arc::new(AtomicBool::new(false));
        let bridge_cancel = cancel.clone();
        let mut cancel_bridge_handle = tokio::spawn(async move {
            shutdown_to_cancel_bridge(&SHUTDOWN_REQUESTED, bridge_cancel).await;
        });

        let event_bus_handles = event_bus.start(bus_rx, "rdp");

        let attempts = Arc::new(SpiceAttempts {
            connection: connection_config,
            link: link.clone(),
            bus: bus_tx,
            repaint_notify,
            virtual_disks,
            share_dir,
            capture: capture_dyn,
            byte_counter,
            traffic: traffic_dyn,
            snapshots,
            monitors,
            volume_control,
            log_config,
            cancel: cancel.clone(),
            clipboard: clipboard_for_conn,
            playback,
            h264,
            image_cache_cap_bytes,
            glz_dictionary_cap_bytes,
            preferred_compression,
        });
        // Cancelled only once the wait below has ended, so a shutdown
        // never reads to the supervisor as a session that ended.
        let stop = CancellationToken::new();
        let supervisor_stop = stop.clone();
        let mut supervisor_handle = tokio::spawn(async move {
            supervise(policy, supervisor_stop, |number| {
                let attempts = attempts.clone();
                async move { attempts.run(number).await }
            })
            .await
        });

        // Control socket, when asked for, wired as `--web` wires it,
        // but to the fixed ends of the session, so it is unaffected by
        // a SPICE reconnect.
        #[cfg(unix)]
        let control_cancel = CancellationToken::new();
        #[cfg(unix)]
        let control_handle = control_server.map(|server| {
            let status: Arc<dyn shakenfist_spice_renderer::control::StatusProvider> =
                Arc::new(shakenfist_spice_renderer::SessionStatus::new(
                    link.connected.clone(),
                    link.agent_connected.subscribe(),
                    surface_mirror.clone(),
                ));
            info!("rdp: control socket at {}", server.path().display());
            // The digest poller belongs with the socket: it turns
            // framebuffer content into the `digest_updated` events the
            // QR scenario tests subscribe to.
            #[cfg(feature = "digest-decode")]
            let _digest_handle = shakenfist_spice_renderer::spawn_digest_poller(
                surface_mirror.clone(),
                event_broadcast_tx.clone(),
                cancel.clone(),
            );
            let handles = shakenfist_spice_renderer::control::SessionHandles {
                input_tx: input_tx_for_control,
                surface_mirror: surface_mirror.clone(),
                clipboard,
                resize_tx: resize_tx_for_control,
                display_snapshot: display_snapshot.clone(),
                playback_snapshot: playback_snapshot.clone(),
            };
            shakenfist_spice_renderer::spawn_control_socket(
                server,
                status,
                event_broadcast_tx.clone(),
                link.agent_connected.subscribe(),
                handles,
                control_cancel.clone(),
            )
        });
        #[cfg(unix)]
        drop(event_broadcast_tx);

        let ended = tokio::select! {
            _ = &mut cancel_bridge_handle => Ended::ShutdownRequested,
            res = &mut supervisor_handle => Ended::Spice(res),
            () = server.stopped() => Ended::ServerStopped,
        };
        match &ended {
            Ended::ShutdownRequested => info!("rdp: shutdown requested"),
            Ended::Spice(_) => info!("rdp: SPICE session ended; stopping the RDP server"),
            Ended::ServerStopped => warn!("rdp: the RDP server stopped on its own"),
        }

        // Stop the RDP server first, so no client is left looking at
        // a session that is being torn down. `join` blocks, so it
        // runs off the runtime's threads.
        server.shutdown();
        let joined = tokio::task::spawn_blocking(move || server.join(SHUTDOWN_TIMEOUT))
            .await
            .unwrap_or_else(|e| Err(anyhow!("the RDP server join task failed: {e}")));
        let result = match ended {
            // The server stopping first is the failure this run
            // reports.
            Ended::ServerStopped => joined,
            Ended::ShutdownRequested => {
                if let Err(e) = joined {
                    warn!("rdp: {:#}", e);
                }
                Ok(())
            }
            Ended::Spice(res) => {
                if let Err(e) = joined {
                    warn!("rdp: {:#}", e);
                }
                match res {
                    Ok(res) => res,
                    Err(e) => Err(anyhow!("the SPICE supervisor task failed: {e}")),
                }
            }
        };
        let supervisor_finished = supervisor_handle.is_finished();

        // Then `run_web`'s teardown order: cancel the renderer and
        // stop the supervisor, which drops any attempt under way and
        // with it the attempt's channel tasks; stop the control
        // socket; and abort the relays and the bus tasks.
        cancel.store(true, Ordering::Relaxed);
        stop.cancel();
        cancel_bridge_handle.abort();
        if !supervisor_finished {
            match tokio::time::timeout(SHUTDOWN_TIMEOUT, &mut supervisor_handle).await {
                Ok(Ok(Err(e))) => warn!("rdp: SPICE ended with an error: {:#}", e),
                Ok(_) => {}
                Err(_) => {
                    warn!(
                        "rdp: the SPICE side did not stop within {:?}",
                        SHUTDOWN_TIMEOUT
                    );
                    supervisor_handle.abort();
                }
            }
        }
        // Cancel rather than abort, and wait: the control server
        // unlinks its socket file on the way out.
        #[cfg(unix)]
        if let Some(handle) = control_handle {
            control_cancel.cancel();
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, handle).await;
        }

        input_relay.abort();
        resize_relay.abort();
        event_bus_handles.forwarder.abort();
        event_bus_handles.mirror.abort();
        cursor_relay.abort();

        result
    });

    // Close the capture session on the host side and wait for the
    // writers, as the other modes do.
    if let Some(ref capture) = capture {
        capture.close_and_wait();
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::pin::Pin;
    use std::sync::Mutex;

    #[test]
    fn certificate_names_leave_out_a_wildcard_address() {
        assert_eq!(
            certificate_names(IpAddr::from([192, 0, 2, 7])),
            ["192.0.2.7", "localhost"]
        );
        assert_eq!(
            certificate_names("::1".parse().expect("address")),
            ["::1", "localhost"]
        );
        assert_eq!(certificate_names(IpAddr::from([0, 0, 0, 0])), ["localhost"]);
        assert_eq!(
            certificate_names("::".parse().expect("address")),
            ["localhost"]
        );
    }

    // -------------------------------------------------------------------------
    // The supervisor
    // -------------------------------------------------------------------------

    type ScriptedAttempt = Pin<Box<dyn Future<Output = AttemptEnd> + Send>>;

    fn session(result: Result<()>) -> AttemptEnd {
        AttemptEnd {
            reached_session: true,
            result,
        }
    }

    fn no_session(error: &str) -> AttemptEnd {
        AttemptEnd {
            reached_session: false,
            result: Err(anyhow!("{error}")),
        }
    }

    /// Attempts that end as scripted, one entry each, and after the
    /// script never end; and when, from the script's creation, each
    /// one started. For tests with tokio's clock paused.
    struct Script {
        ends: Mutex<VecDeque<AttemptEnd>>,
        started: Mutex<Vec<(u32, Duration)>>,
        epoch: tokio::time::Instant,
    }

    impl Script {
        fn new(ends: Vec<AttemptEnd>) -> Arc<Self> {
            Arc::new(Self {
                ends: Mutex::new(ends.into()),
                started: Mutex::new(Vec::new()),
                epoch: tokio::time::Instant::now(),
            })
        }

        fn attempt(self: &Arc<Self>) -> impl FnMut(u32) -> ScriptedAttempt {
            let script = self.clone();
            move |number| {
                let script = script.clone();
                Box::pin(async move {
                    script
                        .started
                        .lock()
                        .unwrap()
                        .push((number, script.epoch.elapsed()));
                    let end = script.ends.lock().unwrap().pop_front();
                    match end {
                        Some(end) => end,
                        None => std::future::pending().await,
                    }
                })
            }
        }

        fn started(&self) -> Vec<(u32, Duration)> {
            self.started.lock().unwrap().clone()
        }
    }

    fn secs(s: u64) -> Duration {
        Duration::from_secs(s)
    }

    /// Supervise `script`'s attempts, failing the test if the run has
    /// not ended within an hour of the paused clock, so a run that
    /// never ends fails here rather than hanging the test.
    async fn supervised(
        policy: ReconnectPolicy,
        stop: CancellationToken,
        script: Arc<Script>,
    ) -> Result<()> {
        tokio::time::timeout(secs(3600), supervise(policy, stop, script.attempt()))
            .await
            .expect("the supervisor never returned")
    }

    #[tokio::test(start_paused = true)]
    async fn a_lost_session_is_retried_after_the_backoff_and_a_new_session_resets_it() {
        let script = Script::new(vec![
            session(Ok(())),
            no_session("connection refused"),
            session(Ok(())),
        ]);
        let stop = CancellationToken::new();
        let run = tokio::spawn(supervised(
            ReconnectPolicy::default(),
            stop.clone(),
            script.clone(),
        ));
        // Lost at 0 s: a retry after 1 s fails, and one 4 s later
        // reaches a session. That is lost at once, and the budget
        // starts over at 1 s.
        tokio::time::sleep(secs(60)).await;
        assert_eq!(
            script.started(),
            [(1, secs(0)), (2, secs(1)), (3, secs(5)), (4, secs(6))]
        );
        stop.cancel();
        run.await.unwrap().unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn three_failed_retries_give_up_with_the_last_error() {
        let script = Script::new(vec![
            session(Ok(())),
            no_session("refused 1"),
            no_session("refused 2"),
            no_session("refused 3"),
        ]);
        let err = supervised(
            ReconnectPolicy::default(),
            CancellationToken::new(),
            script.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(
            script.started(),
            [(1, secs(0)), (2, secs(1)), (3, secs(5)), (4, secs(21))]
        );
        let err = format!("{err:#}");
        assert!(err.contains("3 attempts to reconnect have failed"), "{err}");
        assert!(err.ends_with(": refused 3"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn running_out_of_retries_is_an_error_even_without_one() {
        let quiet = || AttemptEnd {
            reached_session: false,
            result: Ok(()),
        };
        let script = Script::new(vec![session(Ok(())), quiet(), quiet(), quiet()]);
        let err = supervised(
            ReconnectPolicy::default(),
            CancellationToken::new(),
            script.clone(),
        )
        .await
        .unwrap_err();
        let err = format!("{err:#}");
        assert!(err.contains("3 attempts to reconnect have failed"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn a_single_use_ticket_gives_up_at_once() {
        let script = Script::new(vec![session(Err(anyhow!("main channel closed")))]);
        let policy = ReconnectPolicy {
            ticket_is_single_use: true,
            ticket_valid_until: None,
        };
        let err = supervised(policy, CancellationToken::new(), script.clone())
            .await
            .unwrap_err();
        assert_eq!(script.started(), [(1, secs(0))]);
        assert_eq!(script.epoch.elapsed(), secs(0));
        let err = format!("{err:#}");
        assert!(err.contains("single-use"), "{err}");
        assert!(err.ends_with(": main channel closed"), "{err}");
    }

    #[tokio::test(start_paused = true)]
    async fn an_expired_ticket_gives_up_at_once() {
        // A session that ended without an error ends the run without
        // one, as it did before ryll reconnected.
        let script = Script::new(vec![session(Ok(()))]);
        let policy = ReconnectPolicy {
            ticket_is_single_use: false,
            ticket_valid_until: Some(SystemTime::now() - secs(60)),
        };
        supervised(policy, CancellationToken::new(), script.clone())
            .await
            .unwrap();
        assert_eq!(script.started(), [(1, secs(0))]);
    }

    #[tokio::test(start_paused = true)]
    async fn a_first_attempt_that_never_reaches_a_session_ends_the_run() {
        let script = Script::new(vec![no_session("connection refused")]);
        let err = supervised(
            ReconnectPolicy::default(),
            CancellationToken::new(),
            script.clone(),
        )
        .await
        .unwrap_err();
        assert_eq!(script.started(), [(1, secs(0))]);
        assert_eq!(script.epoch.elapsed(), secs(0));
        assert_eq!(
            format!("{err:#}"),
            "the SPICE session ended: connection refused"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_during_a_wait_ends_the_run_at_once() {
        // Lost, then a failed retry, then a 4 s wait.
        let script = Script::new(vec![session(Ok(())), no_session("refused")]);
        let stop = CancellationToken::new();
        let run = tokio::spawn(supervised(
            ReconnectPolicy::default(),
            stop.clone(),
            script.clone(),
        ));
        tokio::time::sleep(secs(2)).await;
        stop.cancel();
        run.await.unwrap().unwrap();
        assert_eq!(script.epoch.elapsed(), secs(2));
        assert_eq!(script.started(), [(1, secs(0)), (2, secs(1))]);
    }

    #[tokio::test(start_paused = true)]
    async fn stopping_during_an_attempt_ends_the_run_at_once() {
        // The first attempt never ends, as one dialling a host that
        // does not answer would not.
        let script = Script::new(Vec::new());
        let stop = CancellationToken::new();
        let run = tokio::spawn(supervised(
            ReconnectPolicy::default(),
            stop.clone(),
            script.clone(),
        ));
        tokio::time::sleep(secs(2)).await;
        stop.cancel();
        run.await.unwrap().unwrap();
        assert_eq!(script.epoch.elapsed(), secs(2));
        assert_eq!(script.started(), [(1, secs(0))]);
    }

    // -------------------------------------------------------------------------
    // The link between attempts
    // -------------------------------------------------------------------------

    fn surface_created(surface_id: u32) -> ChannelEvent {
        ChannelEvent::SurfaceCreated {
            display_channel_id: 0,
            surface_id,
            width: 16,
            height: 16,
        }
    }

    /// A link during an outage, its mirror holding the lost session's
    /// primary and secondary surfaces.
    fn link_in_an_outage() -> SessionLink {
        let mut mirror = SurfaceMirror::new();
        mirror.apply_event(&surface_created(0));
        mirror.apply_event(&surface_created(1));
        let link = SessionLink::new(Arc::new(tokio::sync::Mutex::new(mirror)));
        link.session_ended();
        link
    }

    fn routes() -> (Routes, mpsc::Receiver<InputEvent>) {
        let (input, input_rx) = mpsc::channel(8);
        let (resize, _) = mpsc::channel(8);
        (Routes { input, resize }, input_rx)
    }

    /// An attempt's event queue holding `events`, its sender dropped.
    async fn attempt_events(events: Vec<ChannelEvent>) -> mpsc::Receiver<ChannelEvent> {
        let (tx, rx) = mpsc::channel(8);
        for event in events {
            tx.send(event).await.unwrap();
        }
        rx
    }

    /// Wait until the relay reading from `tx` has taken and dealt
    /// with everything queued on it.
    async fn relayed<T>(tx: &mpsc::Sender<T>) {
        while tx.capacity() < tx.max_capacity() {
            tokio::task::yield_now().await;
        }
        tokio::task::yield_now().await;
    }

    #[tokio::test]
    async fn a_new_session_clears_the_old_surfaces_before_its_events_reach_the_bus() {
        let link = link_in_an_outage();
        let (bus_tx, mut bus_rx) = mpsc::channel(8);
        let (routes, _input_rx) = routes();

        let events = attempt_events(vec![ChannelEvent::SessionInitialized(7)]).await;
        assert!(pump_events(events, bus_tx, &link, Some(routes)).await);

        assert!(matches!(
            bus_rx.recv().await,
            Some(ChannelEvent::SessionInitialized(7))
        ));
        assert!(link.mirror.lock().await.surfaces.is_empty());
        assert!(link.connected.load(Ordering::Relaxed));
        // No surface yet, so the client is still shown the outage.
        assert!(!*link.spice_up.borrow());
    }

    #[tokio::test]
    async fn the_session_is_up_once_its_first_surface_has_been_passed_on() {
        let link = link_in_an_outage();
        let mut spice_up = link.spice_up.subscribe();
        let (bus_tx, mut bus_rx) = mpsc::channel(8);
        let (routes, _input_rx) = routes();

        let events = attempt_events(vec![
            ChannelEvent::SessionInitialized(7),
            surface_created(0),
            surface_created(1),
        ])
        .await;
        assert!(pump_events(events, bus_tx, &link, Some(routes)).await);

        assert!(spice_up.has_changed().unwrap());
        assert!(*spice_up.borrow_and_update());
        let mut passed_on = 0;
        while bus_rx.try_recv().is_ok() {
            passed_on += 1;
        }
        assert_eq!(passed_on, 3);
    }

    #[tokio::test]
    async fn an_attempt_without_a_session_changes_nothing() {
        let link = link_in_an_outage();
        let (bus_tx, _bus_rx) = mpsc::channel(8);
        let (routes, _input_rx) = routes();

        let events = attempt_events(Vec::new()).await;
        assert!(!pump_events(events, bus_tx, &link, Some(routes)).await);
        // The lost session's surfaces stay, for a client that connects
        // during the outage to be shown darkened.
        assert_eq!(link.mirror.lock().await.surfaces.len(), 2);
        assert!(link.input.borrow().is_none());
        assert!(!*link.spice_up.borrow());
    }

    #[tokio::test]
    async fn the_session_ending_marks_it_down() {
        let link = SessionLink::new(Arc::new(tokio::sync::Mutex::new(SurfaceMirror::new())));
        link.agent_connected.send_replace(true);
        let (routes, _input_rx) = routes();
        link.route(routes);

        link.session_ended();
        assert!(!*link.spice_up.borrow());
        assert!(!*link.agent_connected.borrow());
        assert!(!link.connected.load(Ordering::Relaxed));
        assert!(link.input.borrow().is_none());
        assert!(link.resize.borrow().is_none());
    }

    #[tokio::test]
    async fn input_reaches_only_a_session_that_exists() {
        let link = link_in_an_outage();
        let (fixed_tx, fixed_rx) = mpsc::channel(8);
        let relay = tokio::spawn(relay(fixed_rx, link.input.subscribe(), "input events"));
        let (routes, mut attempt_rx) = routes();

        // Before the attempt's session begins: dropped.
        fixed_tx.send(InputEvent::KeyDown(1)).await.unwrap();
        relayed(&fixed_tx).await;
        // While the session exists: delivered.
        let (bus_tx, _bus_rx) = mpsc::channel(8);
        let events = attempt_events(vec![ChannelEvent::SessionInitialized(7)]).await;
        pump_events(events, bus_tx, &link, Some(routes)).await;
        fixed_tx.send(InputEvent::KeyDown(2)).await.unwrap();
        relayed(&fixed_tx).await;
        // Once it has ended, even though its queue is still open:
        // dropped.
        link.session_ended();
        fixed_tx.send(InputEvent::KeyDown(3)).await.unwrap();
        relayed(&fixed_tx).await;

        drop(fixed_tx);
        assert_eq!(relay.await.unwrap(), 2);
        assert!(matches!(attempt_rx.try_recv(), Ok(InputEvent::KeyDown(2))));
        assert!(attempt_rx.try_recv().is_err());
    }

    /// Wait for `rx`'s next value, failing the test if none comes.
    async fn next_value<T>(rx: &mut watch::Receiver<T>) {
        tokio::time::timeout(secs(5), rx.changed())
            .await
            .expect("nothing was forwarded")
            .unwrap();
    }

    #[tokio::test]
    async fn every_value_of_an_attempts_watch_is_forwarded() {
        let (to, mut fixed) = watch::channel(MOUSE_MODE_UNKNOWN);
        let (from_tx, from_rx) = watch::channel(1u32);
        let forward = tokio::spawn(async move { forward_watch(from_rx, &to).await });

        next_value(&mut fixed).await;
        assert_eq!(*fixed.borrow_and_update(), 1);
        from_tx.send_replace(2);
        next_value(&mut fixed).await;
        assert_eq!(*fixed.borrow_and_update(), 2);
        // The last value before the session ends is kept.
        from_tx.send_replace(1);
        drop(from_tx);
        forward.await.unwrap();
        assert_eq!(*fixed.borrow(), 1);
    }
}
