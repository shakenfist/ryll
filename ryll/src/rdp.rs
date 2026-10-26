//! `--rdp` mode: serve the SPICE session to stock RDP clients.
//!
//! The SPICE side is built as `--web` builds it (the same host-side
//! scaffolding, event bus, surface mirror and optional control
//! socket); the RDP side is `shakenfist_spice_rdp::serve`, which
//! runs IronRDP on a thread of its own and reads the session through
//! the mirror, the cursor relay, the clipboard, the audio relay and
//! the input, mouse-mode and resize channels.

use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use shakenfist_spice_rdp::{
    generate_password, serve, spawn_cursor_relay, AudioRelay, Credentials, RdpConfig, RdpSecurity,
    SessionHandles, SingleSession, TlsIdentity, DEFAULT_HANDSHAKE_TIMEOUT,
};
use tracing::{info, warn};

use crate::bugreport::{AppSnapshot, BugReport, ChannelSnapshots, PedanticConfig, TrafficBuffers};
use crate::capture::CaptureSession;
use crate::config::{Args, Config, RdpSecurityMode, ShareDirConfig, VirtualDiskConfig};
use crate::notifications::{
    register_gap_notification_observer, NotificationStore, SharedNotifications,
};
use crate::{mib_to_usize_bytes, settings, shutdown_to_cancel_bridge, SHUTDOWN_REQUESTED};

/// Capacity of the `InputEvent` queue the RDP input handler feeds.
/// The handler cannot wait for space, so a full queue drops the
/// event; this is sized like the browser's queue in `--web`.
const INPUT_CHANNEL_CAPACITY: usize = 256;

/// Capacity of the guest display size queue.
const RESIZE_CHANNEL_CAPACITY: usize = 16;

/// How long to wait for the RDP server thread, and then for the
/// SPICE connection, to finish once asked to stop.
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
    /// The SPICE connection task returned, with its result.
    Connection(std::result::Result<Result<()>, tokio::task::JoinError>),
    /// The RDP server stopped on its own.
    ServerStopped,
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
        let byte_counter = Arc::new(shakenfist_spice_renderer::ByteCounter::new());
        let traffic = Arc::new(TrafficBuffers::new());
        let notifications: SharedNotifications =
            Arc::new(std::sync::Mutex::new(NotificationStore::new()));
        let snapshots = ChannelSnapshots::new();
        // For a control socket's `stats` verb; `snapshots` itself
        // moves into the connection task.
        #[cfg(unix)]
        let display_snapshot = snapshots.display.clone();
        #[cfg(unix)]
        let playback_snapshot = snapshots.playback.clone();
        let connection_config: shakenfist_spice_protocol::ConnectionConfig = (&config).into();
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

        let traffic_dyn: Arc<dyn shakenfist_spice_renderer::TrafficSink> =
            traffic.clone() as Arc<dyn shakenfist_spice_renderer::TrafficSink>;
        let capture_dyn: Option<Arc<dyn shakenfist_spice_renderer::CaptureSink>> =
            capture_for_renderer.map(|c| c as Arc<dyn shakenfist_spice_renderer::CaptureSink>);
        let log_config = settings::log_config();

        // Channels the orchestrator consumes. The RDP input handler
        // and a control socket share the input and resize queues, so
        // both paths land on one ordering; usb and webdav stay closed.
        let (event_tx_mpsc, event_rx_mpsc) =
            tokio::sync::mpsc::channel::<shakenfist_spice_renderer::ChannelEvent>(
                shakenfist_spice_renderer::session::EVENT_CHANNEL_SIZE,
            );
        let (input_tx, input_rx) = tokio::sync::mpsc::channel::<
            shakenfist_spice_renderer::InputEvent,
        >(INPUT_CHANNEL_CAPACITY);
        #[cfg(unix)]
        let input_tx_for_control = input_tx.clone();
        let (usb_tx, usb_rx) =
            tokio::sync::mpsc::channel::<shakenfist_spice_renderer::UsbCommand>(16);
        let (webdav_tx, webdav_rx) =
            tokio::sync::mpsc::channel::<shakenfist_spice_renderer::WebdavCommand>(16);
        let (resize_tx, resize_rx) = tokio::sync::mpsc::channel::<
            shakenfist_spice_renderer::DisplaySize,
        >(RESIZE_CHANNEL_CAPACITY);
        #[cfg(unix)]
        let resize_tx_for_control = resize_tx.clone();
        // One in-memory clipboard for the session, which the main
        // channel ferries to and from the guest's agent. The RDP
        // client's clipboard channel and, when there is one, a control
        // socket both read and write it.
        let clipboard = Arc::new(shakenfist_spice_renderer::MemoryClipboard::new());
        let clipboard_for_conn =
            Some(clipboard.clone() as Arc<dyn shakenfist_spice_renderer::ClipboardBackend>);
        drop(usb_tx);
        drop(webdav_tx);

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
        let playback = shakenfist_spice_renderer::PlaybackOutput::Sink(audio.clone());

        // `run_connection` requires these even though nothing here
        // repaints a window or changes the volume.
        let repaint_notify = Arc::new(tokio::sync::Notify::new());
        let volume_control = shakenfist_spice_renderer::channels::VolumeControl::new();

        // Mouse mode and agent state, published by the main channel
        // through watch channels that always hold the latest value.
        let session_state = shakenfist_spice_renderer::SessionState::new();
        let session_state_rx = session_state.subscribe();

        // Start the RDP server before dialling SPICE, so a port that
        // cannot be bound or an unusable certificate fails the launch
        // with nothing to tear down. Until the first surface arrives
        // the display handler announces a fallback desktop size and
        // resizes the client once the guest's is known. The RDP server
        // stops when the SPICE session ends, so the session is up for
        // as long as a client can see it: the watch's sender is dropped
        // at once, and a closed watch keeps its value.
        let (_, spice_up) = tokio::sync::watch::channel(true);
        let handles = Arc::new(SessionHandles {
            surface_mirror: surface_mirror.clone(),
            input_tx,
            mouse_mode: session_state_rx.mouse_mode.clone(),
            resize_tx,
            cursor,
            spice_up,
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

        // Liveness flag for a control socket's `status` verb: true
        // until the connection task returns.
        let spice_connected = Arc::new(AtomicBool::new(true));

        let connection_cancel = cancel.clone();
        let spice_connected_for_conn = spice_connected.clone();
        let mut connection_handle = tokio::spawn(async move {
            let res = shakenfist_spice_renderer::run_connection(
                connection_config,
                event_tx_mpsc,
                session_state,
                repaint_notify,
                input_rx,
                usb_rx,
                webdav_rx,
                virtual_disks,
                share_dir,
                capture_dyn,
                byte_counter,
                traffic_dyn,
                snapshots,
                monitors,
                resize_rx,
                volume_control,
                /* enable_paste */ false,
                log_config,
                connection_cancel,
                clipboard_for_conn,
                playback,
                h264,
                image_cache_cap_bytes,
                glz_dictionary_cap_bytes,
                preferred_compression,
            )
            .await;
            spice_connected_for_conn.store(false, Ordering::Relaxed);
            res
        });

        let event_bus_handles = event_bus.start(event_rx_mpsc, "rdp");

        // Control socket, when asked for, wired exactly as `--web`
        // wires it.
        #[cfg(unix)]
        let control_cancel = shakenfist_spice_renderer::CancellationToken::new();
        #[cfg(unix)]
        let control_handle = control_server.map(|server| {
            let status: Arc<dyn shakenfist_spice_renderer::control::StatusProvider> =
                Arc::new(shakenfist_spice_renderer::SessionStatus::new(
                    spice_connected.clone(),
                    session_state_rx.agent_connected.clone(),
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
                session_state_rx.agent_connected.clone(),
                handles,
                control_cancel.clone(),
            )
        });
        #[cfg(unix)]
        drop(event_broadcast_tx);

        let ended = tokio::select! {
            _ = &mut cancel_bridge_handle => Ended::ShutdownRequested,
            res = &mut connection_handle => Ended::Connection(res),
            () = server.stopped() => Ended::ServerStopped,
        };
        match &ended {
            Ended::ShutdownRequested => info!("rdp: shutdown requested"),
            Ended::Connection(_) => info!("rdp: SPICE session ended; stopping the RDP server"),
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
            Ended::Connection(res) => {
                if let Err(e) = joined {
                    warn!("rdp: {:#}", e);
                }
                match res {
                    Ok(Ok(())) => Ok(()),
                    Ok(Err(e)) => Err(e.context("the SPICE session ended")),
                    Err(e) => Err(anyhow!("the SPICE connection task failed: {e}")),
                }
            }
        };
        let connection_finished = connection_handle.is_finished();

        // Then `run_web`'s teardown order: cancel the renderer, give
        // it a brief window to unwind, stop the control socket, and
        // abort the bus tasks.
        cancel.store(true, Ordering::Relaxed);
        cancel_bridge_handle.abort();
        if !connection_finished {
            match tokio::time::timeout(SHUTDOWN_TIMEOUT, connection_handle).await {
                Ok(Ok(Err(e))) => warn!("rdp: SPICE connection ended with an error: {:#}", e),
                Ok(_) => {}
                Err(_) => warn!(
                    "rdp: the SPICE connection did not stop within {:?}",
                    SHUTDOWN_TIMEOUT
                ),
            }
        }
        // Cancel rather than abort, and wait: the control server
        // unlinks its socket file on the way out.
        #[cfg(unix)]
        if let Some(handle) = control_handle {
            control_cancel.cancel();
            let _ = tokio::time::timeout(SHUTDOWN_TIMEOUT, handle).await;
        }

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
}
