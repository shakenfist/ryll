//! Drives [`serve`] with IronRDP's own client over loopback, so the
//! RDP handshake, the display stream, the cursor shape, the resize
//! request, keyboard input and the server's stop are proven in CI
//! without a desktop RDP client.
//!
//! The server runs in TLS mode, as `ryll --rdp` does by default. The
//! client is first told the desktop size it asked for, and the guest
//! is asked for that size. The test session has no guest to answer,
//! so its surface keeps its own size, and the client is resized to it
//! by a deactivation and reactivation, which is the sequence every
//! client whose size differs from the guest's goes through. The
//! no-security mode cannot be used here: IronRDP's client connector
//! refuses standard RDP security outright.
//!
//! The client takes the server's pointer updates and decodes them
//! itself, as a desktop client does, so the cursor shape it ends up
//! with is compared with the one the guest sent, pixel for pixel.
//!
//! The client advertises RemoteFX, as FreeRDP does, so the server
//! encodes with it. RemoteFX is lossy, so pixels are compared with a
//! small tolerance, at points well inside flat areas of colour.

use std::collections::VecDeque;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ironrdp_connector::connection_activation::{
    ConnectionActivationFactory, ConnectionActivationState,
};
use ironrdp_connector::sspi::generator::NetworkRequest;
use ironrdp_connector::{ClientConnector, ConnectorResult, DesktopSize as ClientDesktopSize};
use ironrdp_pdu::gcc::KeyboardType;
use ironrdp_pdu::geometry::{InclusiveRectangle, Rectangle as _};
use ironrdp_pdu::input::fast_path::{FastPathInputEvent, KeyboardFlags};
use ironrdp_pdu::rdp::capability_sets::MajorPlatformType;
use ironrdp_pdu::WriteBuf;
use ironrdp_server::PixelFormat;
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{fast_path, ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_tokio::{FramedWrite as _, NetworkClient, TokioFramed};
use shakenfist_spice_protocol::MOUSE_MODE_SERVER;
use shakenfist_spice_rdp::{
    serve, Credentials, CursorState, RdpConfig, RdpSecurity, SessionHandles, SingleSession,
    TlsIdentity,
};
use shakenfist_spice_renderer::{CursorImage, DisplaySize, InputEvent, SurfaceMirror};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, watch};

/// The test surface's size.
const WIDTH: u16 = 640;
const HEIGHT: u16 = 480;

/// The desktop size the client asks for: not the surface's, so the
/// test sees the client told it, the guest asked for it, and the
/// client then resized to the surface. Multiples of 8, so the request
/// to the guest is not changed by aligning it.
const CLIENT_WIDTH: u16 = 800;
const CLIENT_HEIGHT: u16 = 600;

/// The cursor shape the guest has sent: 4 pixels square, with its hot
/// spot off the diagonal so that swapped coordinates show.
const CURSOR_SIDE: u8 = 4;
const CURSOR_HOT_SPOT: (u16, u16) = (1, 2);

/// The block drawn on the mirror once the client has the whole
/// surface: 16 pixels square, with its top-left corner here.
const BLOCK_LEFT: u16 = 100;
const BLOCK_TOP: u16 = 100;
const BLOCK_SIDE: u16 = 16;

/// The surface is four flat quadrants, each with distinct red, green
/// and blue values, so a misplaced row, a flipped image or swapped
/// channels all show at the quadrant centres.
const TOP_LEFT: [u8; 3] = [200, 40, 90];
const TOP_RIGHT: [u8; 3] = [30, 160, 220];
const BOTTOM_LEFT: [u8; 3] = [90, 210, 40];
const BOTTOM_RIGHT: [u8; 3] = [240, 200, 20];
const BLOCK_COLOUR: [u8; 3] = [250, 120, 10];

/// How far a decoded channel may stray from the surface's value.
/// RemoteFX converts to YCbCr and quantises, which costs a unit or
/// two in flat areas; channels swapped or misplaced are off by tens.
const TOLERANCE: u8 = 8;

/// The longest any one step may take before the test fails, so a hang
/// is a failure rather than a stuck CI job. The longest wait is for the
/// resize, which the server holds back for a couple of seconds after
/// accepting the connection so that the client's dynamic channels can
/// open first.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// Scan code 0x1E is the A key.
const KEY_A: u8 = 0x1E;

const USERNAME: &str = "ryll";
/// The session password, minted once per run rather than written into
/// the source; the server and the client read the same value.
static PASSWORD: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(shakenfist_spice_rdp::generate_password);

/// Await `future`, failing the test if it takes longer than
/// [`STEP_TIMEOUT`].
async fn step<T>(what: &str, future: impl Future<Output = T>) -> T {
    tokio::time::timeout(STEP_TIMEOUT, future)
        .await
        .unwrap_or_else(|_| panic!("timed out after {STEP_TIMEOUT:?} waiting for {what}"))
}

fn install_crypto_provider() {
    // Losing an install race with another test is fine: all that
    // matters is that some provider is installed.
    let _ = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default();
}

/// The colour the test surface has at (`x`, `y`).
fn quadrant_colour(x: u16, y: u16) -> [u8; 3] {
    match (x < WIDTH / 2, y < HEIGHT / 2) {
        (true, true) => TOP_LEFT,
        (false, true) => TOP_RIGHT,
        (true, false) => BOTTOM_LEFT,
        (false, false) => BOTTOM_RIGHT,
    }
}

/// RGBA pixels for the whole test surface.
fn surface_pixels() -> Vec<u8> {
    let mut pixels = Vec::with_capacity(usize::from(WIDTH) * usize::from(HEIGHT) * 4);
    for y in 0..HEIGHT {
        for x in 0..WIDTH {
            pixels.extend_from_slice(&quadrant_colour(x, y));
            pixels.push(0xFF);
        }
    }
    pixels
}

/// RGBA pixels for the block.
fn block_pixels() -> Vec<u8> {
    let [r, g, b] = BLOCK_COLOUR;
    [r, g, b, 0xFF].repeat(usize::from(BLOCK_SIDE) * usize::from(BLOCK_SIDE))
}

/// Straight-alpha RGBA pixels for the cursor shape, top row first.
/// Red follows the column and green the row, blue falls as red
/// rises, and alpha differs at every pixel, so a pointer flipped or
/// mirrored, with red and blue swapped, or with its alpha
/// premultiplied into the colour all decode to other values.
fn cursor_pixels() -> Vec<u8> {
    let mut pixels = Vec::new();
    for y in 0..CURSOR_SIDE {
        for x in 0..CURSOR_SIDE {
            let alpha = 0x30 + 0x0C * (y * CURSOR_SIDE + x);
            pixels.extend_from_slice(&[0x10 + 0x30 * x, 0x20 + 0x30 * y, 0xF0 - 0x40 * x, alpha]);
        }
    }
    pixels
}

fn cursor_image() -> CursorImage {
    CursorImage {
        width: u16::from(CURSOR_SIDE),
        height: u16::from(CURSOR_SIDE),
        hot_spot_x: CURSOR_HOT_SPOT.0,
        hot_spot_y: CURSOR_HOT_SPOT.1,
        pixels: cursor_pixels(),
    }
}

/// A SPICE session whose mirror holds the test surface, with the
/// test's ends of its channels.
struct Session {
    handles: Arc<SessionHandles>,
    input_rx: mpsc::Receiver<InputEvent>,
    resize_rx: mpsc::Receiver<DisplaySize>,
    /// Stands in for the session's cursor relay.
    cursor_tx: watch::Sender<CursorState>,
}

fn session() -> Session {
    let mirror = SurfaceMirror::with_test_surface(
        0,
        0,
        u32::from(WIDTH),
        u32::from(HEIGHT),
        &surface_pixels(),
    );
    let (input_tx, input_rx) = mpsc::channel(16);
    let (_mouse_mode_tx, mouse_mode) = watch::channel(MOUSE_MODE_SERVER);
    let (cursor_tx, cursor) = watch::channel(CursorState::default());
    let (resize_tx, resize_rx) = mpsc::channel(1);
    let handles = Arc::new(SessionHandles {
        surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
        input_tx,
        mouse_mode,
        resize_tx,
        cursor,
        label: "loopback".to_string(),
    });
    Session {
        handles,
        input_rx,
        resize_rx,
        cursor_tx,
    }
}

fn server_credentials() -> Credentials {
    Credentials {
        username: USERNAME.to_string(),
        password: PASSWORD.clone(),
        domain: None,
    }
}

fn server_config(handles: Arc<SessionHandles>) -> RdpConfig {
    RdpConfig {
        host: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: Some(0),
        security: RdpSecurity::Tls,
        identity: TlsIdentity::self_signed(&["127.0.0.1".to_string(), "localhost".to_string()])
            .expect("self-signed identity"),
        credentials: server_credentials(),
        provider: Arc::new(SingleSession::new(server_credentials(), handles)),
    }
}

/// A TLS-only client asking for a desktop of a size other than the
/// surface's. It presents a domain, as mstsc does, which the server
/// must ignore. It advertises IronRDP's default codecs, which is
/// RemoteFX alone.
fn client_config() -> ironrdp_connector::Config {
    ironrdp_connector::Config {
        desktop_size: ClientDesktopSize {
            width: CLIENT_WIDTH,
            height: CLIENT_HEIGHT,
        },
        desktop_scale_factor: 0,
        enable_tls: true,
        enable_credssp: false,
        credentials: ironrdp_connector::Credentials::UsernamePassword {
            username: USERNAME.to_string(),
            password: PASSWORD.clone(),
        },
        domain: Some("WORKGROUP".to_string()),
        client_build: 0,
        client_name: "ryll-loopback".to_string(),
        keyboard_type: KeyboardType::IbmEnhanced,
        keyboard_subtype: 0,
        keyboard_functional_keys_count: 12,
        keyboard_layout: 0,
        ime_file_name: String::new(),
        bitmap: None,
        dig_product_id: String::new(),
        client_dir: String::new(),
        alternate_shell: String::new(),
        work_dir: String::new(),
        platform: MajorPlatformType::UNIX,
        hardware_id: None,
        request_data: None,
        autologon: false,
        enable_audio_playback: false,
        performance_flags: Default::default(),
        license_cache: None,
        timezone_info: Default::default(),
        compression_type: None,
        // The client takes the server's pointer updates and hands them
        // back decoded rather than drawing them into the image, so the
        // image holds only what the server drew.
        enable_server_pointer: true,
        pointer_software_rendering: false,
        multitransport_flags: None,
    }
}

/// CredSSP's network client, for Kerberos. A TLS-only connection
/// never calls it.
struct NoNetwork;

impl NetworkClient for NoNetwork {
    async fn send(&mut self, _request: &NetworkRequest) -> ConnectorResult<Vec<u8>> {
        Err(ironrdp_connector::general_err!(
            "the loopback client has no network access"
        ))
    }
}

/// A pointer shape as the client decoded it.
#[derive(Debug)]
struct Pointer {
    width: u16,
    height: u16,
    hot_spot: (u16, u16),
    /// Straight-alpha RGBA, top row first.
    pixels: Vec<u8>,
}

/// What the client saw the server do.
#[derive(Debug)]
enum Event {
    /// Pixels inside this rectangle of the client's image changed.
    Graphics(InclusiveRectangle),
    /// The server sent a pointer shape.
    Pointer(Pointer),
    /// The server told the client to show its own default pointer.
    DefaultPointer,
    /// The server hid the pointer.
    HiddenPointer,
    /// The server deactivated the connection, as it does to resize it.
    Deactivated,
    /// The server ended the session or closed the connection.
    Closed,
}

/// IronRDP's client, connected and active.
struct Client {
    framed: TokioFramed<ironrdp_tls::TlsStream<TcpStream>>,
    stage: ActiveStage,
    activation: ConnectionActivationFactory,
    /// The desktop as the client has decoded it.
    image: DecodedImage,
    /// The desktop size the server announced.
    desktop_size: ClientDesktopSize,
    /// Events decoded from one PDU and not yet returned.
    pending: VecDeque<Event>,
}

impl Client {
    /// Connect to `addr` and run the whole connection sequence: the
    /// security negotiation, the TLS upgrade, the settings and
    /// capability exchanges, and the finalization.
    async fn connect(addr: SocketAddr) -> Self {
        let tcp = TcpStream::connect(addr).await.expect("TCP connect");
        let client_addr = tcp.local_addr().expect("client address");
        let mut framed = TokioFramed::new(tcp);
        let mut connector = ClientConnector::new(client_config(), client_addr);

        let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
            .await
            .expect("security negotiation");
        let (tls, certificate) = ironrdp_tls::upgrade(framed.into_inner_no_leftover(), "127.0.0.1")
            .await
            .expect("TLS upgrade");
        let upgraded = ironrdp_tokio::mark_as_upgraded(should_upgrade, &mut connector);
        let mut framed = TokioFramed::new(tls);
        let server_public_key = ironrdp_tls::extract_tls_server_public_key(&certificate)
            .expect("server public key")
            .to_vec();

        let result = ironrdp_tokio::connect_finalize(
            upgraded,
            connector,
            &mut framed,
            &mut NoNetwork,
            "127.0.0.1".into(),
            server_public_key,
            None,
        )
        .await
        .expect("connection sequence");

        let stage = ActiveStageBuilder {
            static_channels: result.static_channels,
            user_channel_id: result.user_channel_id,
            io_channel_id: result.io_channel_id,
            message_channel_id: result.message_channel_id,
            share_id: result.share_id,
            compression_type: result.compression_type,
            enable_server_pointer: result.enable_server_pointer,
            pointer_software_rendering: result.pointer_software_rendering,
        }
        .build();
        let image = DecodedImage::new(
            PixelFormat::RgbA32,
            result.desktop_size.width,
            result.desktop_size.height,
        );

        Self {
            framed,
            stage,
            activation: result.activation_factory,
            image,
            desktop_size: result.desktop_size,
            pending: VecDeque::new(),
        }
    }

    /// Read and decode PDUs until the server does something the test
    /// watches for, answering whatever the server expects an answer
    /// to (frame acknowledgements, for one) on the way.
    async fn next_event(&mut self) -> Event {
        loop {
            if let Some(event) = self.pending.pop_front() {
                return event;
            }
            let Ok((action, frame)) = self.framed.read_pdu().await else {
                return Event::Closed;
            };
            let outputs = self
                .stage
                .process(&mut self.image, action, &frame)
                .expect("decode a server PDU");
            for output in outputs {
                match output {
                    ActiveStageOutput::ResponseFrame(response) if !response.is_empty() => {
                        if self.framed.write_all(&response).await.is_err() {
                            return Event::Closed;
                        }
                    }
                    // Surface commands holding only frame markers
                    // decode to an empty rectangle; nothing was drawn.
                    ActiveStageOutput::GraphicsUpdate(rect)
                        if rect != InclusiveRectangle::empty() =>
                    {
                        self.pending.push_back(Event::Graphics(rect));
                    }
                    ActiveStageOutput::PointerBitmap(pointer) => {
                        self.pending.push_back(Event::Pointer(Pointer {
                            width: pointer.width,
                            height: pointer.height,
                            hot_spot: (pointer.hotspot_x, pointer.hotspot_y),
                            pixels: pointer.bitmap_data.clone(),
                        }));
                    }
                    ActiveStageOutput::PointerDefault => {
                        self.pending.push_back(Event::DefaultPointer);
                    }
                    ActiveStageOutput::PointerHidden => {
                        self.pending.push_back(Event::HiddenPointer);
                    }
                    ActiveStageOutput::DeactivateAll => self.pending.push_back(Event::Deactivated),
                    ActiveStageOutput::Terminate(_) => self.pending.push_back(Event::Closed),
                    _ => {}
                }
            }
        }
    }

    /// The next drawing, failing the test on anything else.
    async fn next_graphics(&mut self) -> InclusiveRectangle {
        match self.next_event().await {
            Event::Graphics(rect) => rect,
            other => panic!("expected a graphics update, got {other:?}"),
        }
    }

    /// The next pointer shape, failing the test on anything else.
    async fn next_pointer(&mut self) -> Pointer {
        match self.next_event().await {
            Event::Pointer(pointer) => pointer,
            other => panic!("expected a pointer shape, got {other:?}"),
        }
    }

    /// Run the deactivation-reactivation sequence after a
    /// [`Event::Deactivated`], and return the desktop size the server
    /// announced in it. The client's image is rebuilt at that size.
    async fn reactivate(&mut self) -> ClientDesktopSize {
        let mut sequence = self.activation.create();
        let mut buf = WriteBuf::new();
        loop {
            ironrdp_tokio::single_sequence_step(&mut self.framed, &mut sequence, &mut buf)
                .await
                .expect("reactivation step");
            if let ConnectionActivationState::Finalized {
                desktop_size,
                share_id,
                enable_server_pointer,
                pointer_software_rendering,
            } = sequence.connection_activation_state()
            {
                self.stage.set_fastpath_processor(
                    fast_path::ProcessorBuilder {
                        io_channel_id: sequence.io_channel_id(),
                        user_channel_id: sequence.user_channel_id(),
                        share_id,
                        enable_server_pointer,
                        pointer_software_rendering,
                        bulk_decompressor: None,
                    }
                    .build(),
                );
                self.stage.set_share_id(share_id);
                self.stage.set_enable_server_pointer(enable_server_pointer);
                self.image =
                    DecodedImage::new(PixelFormat::RgbA32, desktop_size.width, desktop_size.height);
                self.desktop_size = desktop_size;
                return desktop_size;
            }
        }
    }

    /// Send fast-path input events to the server.
    async fn send_input(&mut self, events: &[FastPathInputEvent]) {
        let outputs = self
            .stage
            .process_fastpath_input(&mut self.image, events)
            .expect("encode input");
        for output in outputs {
            if let ActiveStageOutput::ResponseFrame(frame) = output {
                self.framed.write_all(&frame).await.expect("send input");
            }
        }
    }

    /// Read until the server ends the session or closes the
    /// connection.
    async fn wait_closed(&mut self) {
        while !matches!(self.next_event().await, Event::Closed) {}
    }

    /// The decoded RGB value at (`x`, `y`).
    fn pixel(&self, x: u16, y: u16) -> [u8; 3] {
        let offset =
            usize::from(y) * self.image.stride() + usize::from(x) * self.image.bytes_per_pixel();
        let data = self.image.data();
        [data[offset], data[offset + 1], data[offset + 2]]
    }

    fn assert_colour(&self, x: u16, y: u16, expected: [u8; 3], what: &str) {
        let actual = self.pixel(x, y);
        let close = actual
            .iter()
            .zip(expected)
            .all(|(&a, e)| a.abs_diff(e) <= TOLERANCE);
        assert!(
            close,
            "{what} at ({x}, {y}): decoded {actual:?}, expected {expected:?} within {TOLERANCE}"
        );
    }
}

/// Fail the test unless `pointer` is the guest's cursor shape, decoded
/// to the same pixels.
fn assert_cursor(pointer: &Pointer, what: &str) {
    assert_eq!(
        (pointer.width, pointer.height),
        (u16::from(CURSOR_SIDE), u16::from(CURSOR_SIDE)),
        "{what}: size"
    );
    assert_eq!(pointer.hot_spot, CURSOR_HOT_SPOT, "{what}: hot spot");
    assert_eq!(pointer.pixels, cursor_pixels(), "{what}: pixels");
}

/// The rectangle of the whole desktop, inclusive of its last pixel.
fn whole_desktop() -> InclusiveRectangle {
    InclusiveRectangle {
        left: 0,
        top: 0,
        right: WIDTH - 1,
        bottom: HEIGHT - 1,
    }
}

#[tokio::test]
async fn ironrdp_client_is_served_the_session_and_its_input_arrives() {
    install_crypto_provider();
    let Session {
        handles,
        mut input_rx,
        mut resize_rx,
        cursor_tx,
    } = session();
    // The guest has sent a cursor shape before the client connects.
    cursor_tx.send_modify(|state| state.shape = Some(Arc::new(cursor_image())));
    let server = serve(server_config(Arc::clone(&handles))).expect("serve");

    // The connection activates, and the client is told the desktop
    // size it asked for, which the guest is asked for too.
    let mut client = step(
        "the connection sequence",
        Client::connect(server.local_addr()),
    )
    .await;
    assert_eq!(
        (client.desktop_size.width, client.desktop_size.height),
        (CLIENT_WIDTH, CLIENT_HEIGHT),
        "the client is told the desktop size it asked for"
    );
    let requested = step("the request for the client's size", resize_rx.recv()).await;
    assert_eq!(
        requested,
        Some(DisplaySize::Requested(
            u32::from(CLIENT_WIDTH),
            u32::from(CLIENT_HEIGHT)
        ))
    );

    // The stream sends the cursor shape first, and the client decodes
    // it to the guest's pixels. The surface is not the size the client
    // was told, so the next update is a resize to the surface's size,
    // which arrives as a deactivation. The server holds it back for a
    // grace period after accepting the connection, and sends nothing
    // else in the meantime, since the surface does not fit the
    // client's desktop.
    let pointer = step("the cursor shape", client.next_pointer()).await;
    assert_cursor(&pointer, "the first cursor shape");
    let event = step("the resize to the surface", client.next_event()).await;
    assert!(
        matches!(event, Event::Deactivated),
        "expected a deactivation, got {event:?}"
    );
    let size = step("the reactivation", client.reactivate()).await;
    assert_eq!((size.width, size.height), (WIDTH, HEIGHT));

    // A reactivated client starts from its default pointer, so the new
    // stream sends the cursor shape again before anything else.
    let pointer = step("the cursor shape again", client.next_pointer()).await;
    assert_cursor(&pointer, "the cursor shape after the reactivation");
    // The reactivation's capability exchange echoed the surface's size
    // back to the server, which must not be taken for a new request
    // from the client. The server handles that exchange before it
    // builds the new stream, so by the time the stream has sent
    // anything, any request it made would be queued.
    assert!(
        resize_rx.try_recv().is_err(),
        "the reactivation asked the guest for another size"
    );

    // The first drawing is the whole surface.
    let seed = step("the whole surface", client.next_graphics()).await;
    assert_eq!(seed, whole_desktop());
    for (x, y, colour, what) in [
        (WIDTH / 4, HEIGHT / 4, TOP_LEFT, "top-left quadrant"),
        (WIDTH * 3 / 4, HEIGHT / 4, TOP_RIGHT, "top-right quadrant"),
        (
            WIDTH / 4,
            HEIGHT * 3 / 4,
            BOTTOM_LEFT,
            "bottom-left quadrant",
        ),
        (
            WIDTH * 3 / 4,
            HEIGHT * 3 / 4,
            BOTTOM_RIGHT,
            "bottom-right quadrant",
        ),
    ] {
        client.assert_colour(x, y, colour, what);
    }

    // A draw on the mirror reaches the client as an update covering it,
    // not as the whole surface again.
    handles
        .surface_mirror
        .lock()
        .await
        .primary_surface_mut()
        .expect("primary surface")
        .blit(
            u32::from(BLOCK_LEFT),
            u32::from(BLOCK_TOP),
            u32::from(BLOCK_SIDE),
            u32::from(BLOCK_SIDE),
            &block_pixels(),
        );
    let damage = step("the block's update", client.next_graphics()).await;
    let block_right = BLOCK_LEFT + BLOCK_SIDE - 1;
    let block_bottom = BLOCK_TOP + BLOCK_SIDE - 1;
    assert!(
        damage.left <= BLOCK_LEFT
            && damage.top <= BLOCK_TOP
            && damage.right >= block_right
            && damage.bottom >= block_bottom,
        "update {damage:?} does not cover the block at ({BLOCK_LEFT}, {BLOCK_TOP})"
    );
    assert_ne!(
        damage,
        whole_desktop(),
        "the block was sent as the whole surface"
    );
    client.assert_colour(
        BLOCK_LEFT + BLOCK_SIDE / 2,
        BLOCK_TOP + BLOCK_SIDE / 2,
        BLOCK_COLOUR,
        "block centre",
    );
    client.assert_colour(
        WIDTH / 4,
        HEIGHT / 4,
        TOP_LEFT,
        "top-left quadrant after the block",
    );

    // A key press and release reach the SPICE input queue as scan codes,
    // the release with its top bit set. Anything the handshake queued
    // (a lock-key synchronize, say) is not what this checks.
    while input_rx.try_recv().is_ok() {}
    client
        .send_input(&[
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), KEY_A),
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, KEY_A),
        ])
        .await;
    let pressed = step("the key press", input_rx.recv()).await;
    assert!(
        matches!(pressed, Some(InputEvent::KeyDown(0x1E))),
        "expected KeyDown(0x1E), got {pressed:?}"
    );
    let released = step("the key release", input_rx.recv()).await;
    assert!(
        matches!(released, Some(InputEvent::KeyUp(0x9E))),
        "expected KeyUp(0x9E), got {released:?}"
    );

    // A stop asked for while the client is connected disconnects it and
    // ends the server.
    server.shutdown();
    step("the client to be disconnected", client.wait_closed()).await;
    step("the server thread to finish", server.stopped()).await;
    server
        .join(Duration::ZERO)
        .expect("the server stops cleanly");
}
