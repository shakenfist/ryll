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
//!
//! A second test opens the clipboard and audio output channels with
//! IronRDP's own client-side channel processors, against a session
//! with a clipboard and an audio relay. Text copied on the client
//! reaches the session's clipboard with LF line endings, and text the
//! guest copies is offered to the client and reaches it as CRLF
//! `CF_UNICODETEXT`. Audio the session plays reaches the client as a
//! wave in the format the two sides agreed: 16-bit PCM, unchanged
//! sample for sample, for a client that accepts only PCM, and the
//! guest's Opus packet, unchanged, for one that accepts Opus too.
//!
//! The rest follow clients through the server's connection lifecycle:
//! a client served after the one before it left; a second client with
//! the password taking the session over, which disconnects the first
//! and releases what it held; a client with the wrong password, over
//! TLS and over NLA, and a client of another session, each leaving the
//! client being served alone; a connection that never authenticates
//! being dropped once its time is up, having kept the next client
//! waiting no longer than that; and a stop with clients connected.

use std::borrow::Cow;
use std::collections::VecDeque;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use ironrdp_cliprdr::backend::CliprdrBackend;
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
    OwnedFormatDataResponse,
};
use ironrdp_cliprdr::CliprdrClient;
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
use ironrdp_rdpsnd::client::{Rdpsnd, RdpsndClientHandler};
use ironrdp_rdpsnd::pdu::{AudioFormat, PitchPdu, VolumePdu, WaveFormat};
use ironrdp_server::PixelFormat;
use ironrdp_session::image::DecodedImage;
use ironrdp_session::{fast_path, ActiveStage, ActiveStageBuilder, ActiveStageOutput};
use ironrdp_svc::{SvcProcessor, SvcProcessorMessages};
use ironrdp_tokio::{FramedWrite as _, NetworkClient, TokioFramed};
use shakenfist_spice_protocol::MOUSE_MODE_SERVER;
use shakenfist_spice_rdp::{
    serve, AudioRelay, Credentials, CursorState, RdpConfig, RdpSecurity, Rejected, SessionHandles,
    SessionProvider, SingleSession, TlsIdentity, DEFAULT_HANDSHAKE_TIMEOUT,
};
use shakenfist_spice_renderer::{
    ClipboardBackend as _, CursorImage, DisplaySize, InputEvent, MemoryClipboard,
    PlaybackSink as _, SurfaceMirror,
};
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

/// How long a client has to authenticate in the test that lets one
/// run out of time: short, so the test is quick, and long enough that
/// a client served without waiting for it is told apart.
const SHORT_HANDSHAKE_TIMEOUT: Duration = Duration::from_millis(500);

/// The longest any one step may take before the test fails, so a hang
/// is a failure rather than a stuck CI job. The longest wait is for the
/// resize, which the server holds back for a couple of seconds after
/// accepting the connection so that the client's dynamic channels can
/// open first.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// Scan code 0x1E is the A key.
const KEY_A: u8 = 0x1E;

/// The rate and channel count of the audio the session plays, which
/// is what decoded Opus always is.
const AUDIO_RATE_HZ: u32 = 48_000;
const AUDIO_CHANNELS: u8 = 2;

/// The frames in one 20 ms Opus packet at 48 kHz: one PCM wave's worth.
const PACKET_FRAMES: u32 = 960;

/// Stands in for one of the guest's Opus packets. The server passes
/// Opus through without decoding it, and the client here does not
/// decode it either, so it need not be real Opus.
const OPUS_PACKET: &[u8] = &[0xFC, 0xFF, 0xFE, 0x10, 0x32, 0x54, 0x76, 0x98];

/// How often the test looks for text the client sent to have reached
/// the session's clipboard, which it is stored in on the server's
/// thread.
const CLIPBOARD_POLL: Duration = Duration::from_millis(5);

/// How often the test looks for the server's audio channel to be
/// listening to the session's relay, which it starts doing on the
/// server's thread once the client has confirmed its training.
const AUDIO_POLL: Duration = Duration::from_millis(5);

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

/// The session's audio as the playback channel would hand it on: one
/// packet's interleaved stereo samples, varying in both bytes and
/// negative as often as not, so that samples reordered, truncated or
/// sent big-endian all arrive as other bytes.
fn decoded_samples() -> Vec<i16> {
    let samples = PACKET_FRAMES * u32::from(AUDIO_CHANNELS);
    (0..samples)
        .map(|i| {
            let i = i16::try_from(i).expect("a packet's samples fit an i16 index");
            i.wrapping_mul(97).wrapping_sub(16_000)
        })
        .collect()
}

/// `samples` as the bytes of a 16-bit PCM wave: little-endian.
fn pcm_bytes(samples: &[i16]) -> Vec<u8> {
    samples
        .iter()
        .flat_map(|sample| sample.to_le_bytes())
        .collect()
}

/// `text` as `CF_UNICODETEXT` data: NUL-terminated UTF-16LE.
fn unicode_text_bytes(text: &str) -> Vec<u8> {
    text.encode_utf16()
        .chain([0])
        .flat_map(|unit| unit.to_le_bytes())
        .collect()
}

/// An audio format as the server offers it: 48 kHz stereo, with the
/// derived fields of 16-bit PCM, which IronRDP's client matches
/// against the server's offer field for field.
fn audio_format(format: WaveFormat) -> AudioFormat {
    AudioFormat {
        format,
        n_channels: u16::from(AUDIO_CHANNELS),
        n_samples_per_sec: AUDIO_RATE_HZ,
        n_avg_bytes_per_sec: AUDIO_RATE_HZ * u32::from(AUDIO_CHANNELS) * 2,
        n_block_align: u16::from(AUDIO_CHANNELS) * 2,
        bits_per_sample: 16,
        data: None,
    }
}

/// A session with neither a clipboard nor audio.
fn session() -> Session {
    session_with(None, None)
}

fn session_with(
    clipboard: Option<Arc<MemoryClipboard>>,
    audio: Option<Arc<AudioRelay>>,
) -> Session {
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
    let (_, spice_up) = watch::channel(true);
    let (resize_tx, resize_rx) = mpsc::channel(1);
    let handles = Arc::new(SessionHandles {
        surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
        input_tx,
        mouse_mode,
        resize_tx,
        cursor,
        spice_up,
        clipboard,
        audio,
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
    server_config_with(
        RdpSecurity::Tls,
        Arc::new(SingleSession::new(server_credentials(), handles)),
    )
}

fn server_config_with(security: RdpSecurity, provider: Arc<dyn SessionProvider>) -> RdpConfig {
    RdpConfig {
        host: IpAddr::V4(Ipv4Addr::LOCALHOST),
        port: Some(0),
        security,
        identity: TlsIdentity::self_signed(&["127.0.0.1".to_string(), "localhost".to_string()])
            .expect("self-signed identity"),
        credentials: server_credentials(),
        provider,
        handshake_timeout: DEFAULT_HANDSHAKE_TIMEOUT,
    }
}

/// What a client presents, and how.
#[derive(Clone)]
struct Login {
    username: String,
    password: String,
    /// NLA (CredSSP inside TLS) rather than TLS alone.
    nla: bool,
}

impl Login {
    /// The session's username and password, over TLS alone.
    fn tls() -> Self {
        Self {
            username: USERNAME.to_string(),
            password: PASSWORD.clone(),
            nla: false,
        }
    }

    /// The session's username and password, over NLA.
    fn nla() -> Self {
        Self {
            nla: true,
            ..Self::tls()
        }
    }

    /// The same, with a password of the same length that differs in
    /// its first character.
    fn with_wrong_password(mut self) -> Self {
        let first = if self.password.starts_with('x') {
            "y"
        } else {
            "x"
        };
        self.password.replace_range(0..1, first);
        self
    }
}

/// A client asking for a `width` x `height` desktop and presenting
/// `login`. It presents a domain, as mstsc does, which the server must
/// ignore. It advertises IronRDP's default codecs, which is RemoteFX
/// alone.
fn client_config(
    width: u16,
    height: u16,
    channels: &Channels,
    login: &Login,
) -> ironrdp_connector::Config {
    ironrdp_connector::Config {
        desktop_size: ClientDesktopSize { width, height },
        desktop_scale_factor: 0,
        enable_tls: true,
        enable_credssp: login.nla,
        credentials: ironrdp_connector::Credentials::UsernamePassword {
            username: login.username.clone(),
            password: login.password.clone(),
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
        enable_audio_playback: matches!(channels, Channels::ClipboardAndAudio(_)),
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
    /// The server's clipboard channel asked for the client's clipboard
    /// formats, which the client answers with a copy.
    FormatListRequested,
    /// The server accepted the client's first Format List, so the
    /// client's clipboard channel is Ready.
    ClipboardReady,
    /// The server copied: its clipboard holds these formats.
    RemoteCopy(Vec<ClipboardFormatId>),
    /// The server asked for the client's clipboard in this format.
    DataRequested(ClipboardFormatId),
    /// The server sent the clipboard data the client pasted, or `None`
    /// for an error response.
    Data(Option<Vec<u8>>),
    /// The server sent a wave of audio in the client's format
    /// `format_no`.
    Wave {
        format_no: usize,
        timestamp: u32,
        data: Vec<u8>,
    },
}

/// The client's clipboard backend: it records what the server's
/// clipboard channel tells it, and the test answers.
#[derive(Debug)]
struct ClipboardRecorder(std::sync::mpsc::Sender<Event>);

impl ClipboardRecorder {
    fn record(&self, event: Event) {
        // The receiver lives as long as the client that owns this.
        let _ = self.0.send(event);
    }
}

ironrdp_core::impl_as_any!(ClipboardRecorder);

impl CliprdrBackend for ClipboardRecorder {
    fn temporary_directory(&self) -> &str {
        ""
    }

    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        self.record(Event::ClipboardReady);
    }

    fn on_request_format_list(&mut self) {
        self.record(Event::FormatListRequested);
    }

    fn on_process_negotiated_capabilities(&mut self, _: ClipboardGeneralCapabilityFlags) {}

    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        self.record(Event::RemoteCopy(
            available_formats.iter().map(ClipboardFormat::id).collect(),
        ));
    }

    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        self.record(Event::DataRequested(request.format));
    }

    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let data = (!response.is_error()).then(|| response.data().to_vec());
        self.record(Event::Data(data));
    }

    fn on_file_contents_request(&mut self, _: FileContentsRequest) {}

    fn on_file_contents_response(&mut self, _: FileContentsResponse<'_>) {}

    fn on_lock(&mut self, _: LockDataId) {}

    fn on_unlock(&mut self, _: LockDataId) {}
}

/// The client's audio output handler: it accepts `formats` and records
/// every wave.
#[derive(Debug)]
struct AudioRecorder {
    formats: Vec<AudioFormat>,
    events: std::sync::mpsc::Sender<Event>,
}

impl RdpsndClientHandler for AudioRecorder {
    fn get_formats(&self) -> &[AudioFormat] {
        &self.formats
    }

    fn wave(&mut self, format_no: usize, timestamp: u32, data: Cow<'_, [u8]>) {
        let _ = self.events.send(Event::Wave {
            format_no,
            timestamp,
            data: data.into_owned(),
        });
    }

    fn set_volume(&mut self, _: VolumePdu) {}

    fn set_pitch(&mut self, _: PitchPdu) {}

    fn close(&mut self) {}
}

/// The static channels the client opens.
enum Channels {
    /// None: the display and input alone.
    None,
    /// The clipboard and audio output channels, accepting these audio
    /// formats.
    ClipboardAndAudio(Vec<AudioFormat>),
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
    /// What the client's static channels recorded while a PDU was
    /// decoded.
    channel_events: std::sync::mpsc::Receiver<Event>,
}

impl Client {
    /// Connect to `addr`, asking for a `width` x `height` desktop and
    /// opening `channels`, and run the whole connection sequence: the
    /// security negotiation, the TLS upgrade, the settings and
    /// capability exchanges, and the finalization.
    async fn connect(addr: SocketAddr, width: u16, height: u16, channels: Channels) -> Self {
        Self::connect_as(addr, width, height, channels, &Login::tls())
            .await
            .unwrap_or_else(|error| panic!("{error}"))
    }

    /// As [`Self::connect`], presenting `login`, and returning why the
    /// connection sequence failed rather than failing the test.
    async fn connect_as(
        addr: SocketAddr,
        width: u16,
        height: u16,
        channels: Channels,
        login: &Login,
    ) -> Result<Self, String> {
        let tcp = TcpStream::connect(addr)
            .await
            .map_err(|error| format!("TCP connect: {error}"))?;
        let client_addr = tcp.local_addr().expect("client address");
        let mut framed = TokioFramed::new(tcp);
        let mut connector =
            ClientConnector::new(client_config(width, height, &channels, login), client_addr);
        let (events, channel_events) = std::sync::mpsc::channel();
        if let Channels::ClipboardAndAudio(formats) = channels {
            let clipboard = ClipboardRecorder(events.clone());
            let audio = AudioRecorder { formats, events };
            connector.attach_static_channel(CliprdrClient::new(Box::new(clipboard)));
            connector.attach_static_channel(Rdpsnd::new(Box::new(audio)));
        }

        let should_upgrade = ironrdp_tokio::connect_begin(&mut framed, &mut connector)
            .await
            .map_err(|error| format!("security negotiation: {error}"))?;
        let (tls, certificate) = ironrdp_tls::upgrade(framed.into_inner_no_leftover(), "127.0.0.1")
            .await
            .map_err(|error| format!("TLS upgrade: {error}"))?;
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
        .map_err(|error| format!("connection sequence: {error}"))?;

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

        Ok(Self {
            framed,
            stage,
            activation: result.activation_factory,
            image,
            desktop_size: result.desktop_size,
            pending: VecDeque::new(),
            channel_events,
        })
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
            // A static channel's answer to the server, if it had one,
            // went out above as a response frame; what it told its
            // backend or handler is the test's to act on.
            self.pending.extend(self.channel_events.try_iter());
        }
    }

    /// The next thing the clipboard or audio output channel was told,
    /// passing over the display stream's drawing and pointer updates.
    async fn next_channel_event(&mut self) -> Event {
        loop {
            match self.next_event().await {
                Event::Graphics(_)
                | Event::Pointer(_)
                | Event::DefaultPointer
                | Event::HiddenPointer => {}
                event => return event,
            }
        }
    }

    /// Send what a static channel processor produced to the server.
    async fn send_channel_messages<C: SvcProcessor + 'static>(
        &mut self,
        messages: SvcProcessorMessages<C>,
    ) {
        let frame = self
            .stage
            .process_svc_processor_messages(messages)
            .expect("encode static channel messages");
        self.framed
            .write_all(&frame)
            .await
            .expect("send static channel messages");
    }

    fn clipboard(&mut self) -> &mut CliprdrClient {
        self.stage
            .get_svc_processor_mut::<CliprdrClient>()
            .expect("the client has a clipboard channel")
    }

    /// Copy on the client: announce `formats` to the server.
    async fn copy(&mut self, formats: &[ClipboardFormatId]) {
        let formats: Vec<_> = formats.iter().map(|&id| ClipboardFormat::new(id)).collect();
        let messages = self.clipboard().initiate_copy(&formats).expect("copy");
        self.send_channel_messages(messages).await;
    }

    /// Paste on the client: ask the server for its clipboard as
    /// `format`.
    async fn paste(&mut self, format: ClipboardFormatId) {
        let messages = self.clipboard().initiate_paste(format).expect("paste");
        self.send_channel_messages(messages).await;
    }

    /// Answer the server's request for the client's clipboard.
    async fn send_clipboard_data(&mut self, response: OwnedFormatDataResponse) {
        let messages = self
            .clipboard()
            .submit_format_data(response)
            .expect("clipboard data");
        self.send_channel_messages(messages).await;
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

    /// Read, without decoding, until the server closes the connection.
    /// For a client the server has refused: the error it is sent last
    /// is a PDU this client's active stage cannot decode.
    async fn wait_refused(&mut self) {
        while self.framed.read_pdu().await.is_ok() {}
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
        Client::connect(
            server.local_addr(),
            CLIENT_WIDTH,
            CLIENT_HEIGHT,
            Channels::None,
        ),
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

/// Run the clipboard both ways over `client`'s clipboard channel,
/// which has just opened: the client's clipboard, `client_text` with
/// CRLF line endings, reaches the session's `clipboard` with LF ones,
/// and then a guest copy of `guest_text`, with LF line endings, is
/// offered to the client and pasted there with CRLF ones.
async fn exchange_clipboard(
    client: &mut Client,
    clipboard: &MemoryClipboard,
    client_text: &str,
    guest_text: &str,
) {
    // The server's channel asks for the client's formats, and the
    // client answers with its clipboard's, as a desktop client does
    // when it connects. The server accepting them makes the channel
    // Ready on both sides.
    let event = step(
        "the request for the client's formats",
        client.next_channel_event(),
    )
    .await;
    assert!(
        matches!(event, Event::FormatListRequested),
        "expected the request for the client's formats, got {event:?}"
    );
    client.copy(&[ClipboardFormatId::CF_UNICODETEXT]).await;
    let event = step("the clipboard to be ready", client.next_channel_event()).await;
    assert!(
        matches!(event, Event::ClipboardReady),
        "expected the server to accept the client's formats, got {event:?}"
    );

    // Offered text, the server asks for it straight away, and stores
    // it in the session's clipboard with LF line endings.
    let event = step(
        "the server's request for the client's text",
        client.next_channel_event(),
    )
    .await;
    assert!(
        matches!(
            event,
            Event::DataRequested(ClipboardFormatId::CF_UNICODETEXT)
        ),
        "expected a request for the client's text, got {event:?}"
    );
    let generation = clipboard.generation();
    client
        .send_clipboard_data(OwnedFormatDataResponse::new_unicode_string(client_text))
        .await;
    step("the client's text to reach the session", async {
        while clipboard.generation() == generation {
            tokio::time::sleep(CLIPBOARD_POLL).await;
        }
    })
    .await;
    assert_eq!(
        clipboard.text().as_deref(),
        Some(client_text.replace("\r\n", "\n").as_str()),
        "the client's text in the session's clipboard"
    );

    // A guest copy, stored as the main channel stores one, is offered
    // to the client, which pastes it as Windows text.
    clipboard.set_text(guest_text).expect("the guest's copy");
    let event = step(
        "the guest's copy to be offered",
        client.next_channel_event(),
    )
    .await;
    let Event::RemoteCopy(formats) = event else {
        panic!("expected the guest's copy to be offered, got {event:?}");
    };
    assert_eq!(formats, [ClipboardFormatId::CF_UNICODETEXT]);
    client.paste(ClipboardFormatId::CF_UNICODETEXT).await;
    let event = step("the guest's text", client.next_channel_event()).await;
    let Event::Data(data) = event else {
        panic!("expected the guest's text, got {event:?}");
    };
    assert_eq!(
        data,
        Some(unicode_text_bytes(&guest_text.replace('\n', "\r\n"))),
        "the guest's text as the client received it"
    );
}

/// The next audio the client was sent, failing the test on anything
/// else.
async fn next_wave(client: &mut Client, what: &str) -> (usize, u32, Vec<u8>) {
    match step(what, client.next_channel_event()).await {
        Event::Wave {
            format_no,
            timestamp,
            data,
        } => (format_no, timestamp, data),
        other => panic!("expected {what}, got {other:?}"),
    }
}

/// Wait until the server's audio channel is listening to `relay`, so
/// that audio played from then on reaches the client rather than being
/// dropped for want of a listener.
async fn wait_for_listener(relay: &AudioRelay) {
    step("the audio channel to listen", async {
        while !relay.listening() {
            tokio::time::sleep(AUDIO_POLL).await;
        }
    })
    .await;
}

/// Play one packet of the guest's audio, as the playback channel hands
/// a packet to the relay: the Opus packet, then what it decoded to.
fn play_packet(relay: &AudioRelay) {
    relay.on_opus_packet(OPUS_PACKET, PACKET_FRAMES);
    relay.on_decoded_pcm(&decoded_samples(), AUDIO_RATE_HZ, AUDIO_CHANNELS);
}

#[tokio::test]
async fn ironrdp_client_shares_the_clipboard_and_hears_the_guest() {
    install_crypto_provider();
    let clipboard = Arc::new(MemoryClipboard::new());
    let relay = Arc::new(AudioRelay::new());
    let session = session_with(Some(Arc::clone(&clipboard)), Some(Arc::clone(&relay)));
    let server = serve(server_config(Arc::clone(&session.handles))).expect("serve");

    // The client asks for the surface's own size, so the connection is
    // never resized: the deactivation a resize takes would drop any
    // static channel PDU that arrived during it. It accepts PCM alone.
    let mut client = step(
        "the connection sequence",
        Client::connect(
            server.local_addr(),
            WIDTH,
            HEIGHT,
            Channels::ClipboardAndAudio(vec![audio_format(WaveFormat::PCM)]),
        ),
    )
    .await;
    exchange_clipboard(
        &mut client,
        &clipboard,
        "copied on the client\r\nsecond line, na\u{ef}ve caf\u{e9}\r\n",
        "copied in the guest\nzweite Zeile, gr\u{fc}\u{df} dich",
    )
    .await;

    // The server starts the audio channel, and it starts listening to
    // the relay, when the client confirms its training. Audio played
    // before then is dropped, so the test waits for it.
    //
    // A client that accepts only PCM hears the decoded samples, as one
    // wave of little-endian 16-bit PCM, and not the Opus packet.
    wait_for_listener(&relay).await;
    play_packet(&relay);
    let (format_no, timestamp, data) = next_wave(&mut client, "the PCM wave").await;
    assert_eq!(format_no, 0, "the wave is in the client's only format");
    assert_eq!(timestamp, 0, "the first wave starts the stream's clock");
    assert_eq!(
        data,
        pcm_bytes(&decoded_samples()),
        "the PCM wave holds the decoded samples"
    );

    // The client disconnects, and one that accepts Opus as well
    // connects. Its clipboard works as the first one's did, and it
    // hears the guest's Opus packet unchanged. The first connection's
    // audio channel stops once the server has seen it end, which can
    // be after the next client is accepted; until then it would pass
    // for the second client's channel listening.
    drop(client);
    step("the first client's audio channel to stop", async {
        while relay.listening() {
            tokio::time::sleep(AUDIO_POLL).await;
        }
    })
    .await;
    let mut client = step(
        "the second connection sequence",
        Client::connect(
            server.local_addr(),
            WIDTH,
            HEIGHT,
            Channels::ClipboardAndAudio(vec![
                audio_format(WaveFormat::PCM),
                audio_format(WaveFormat::OPUS),
            ]),
        ),
    )
    .await;
    exchange_clipboard(
        &mut client,
        &clipboard,
        "the second client's copy\r\n",
        "the guest's second copy\n",
    )
    .await;
    wait_for_listener(&relay).await;
    play_packet(&relay);
    let (_, timestamp, data) = next_wave(&mut client, "the Opus wave").await;
    assert_eq!(timestamp, 0, "the first wave starts the stream's clock");
    assert_eq!(
        data, OPUS_PACKET,
        "the Opus wave is the guest's packet, unchanged"
    );

    server.shutdown();
    step("the client to be disconnected", client.wait_closed()).await;
    step("the server thread to finish", server.stopped()).await;
    server
        .join(Duration::ZERO)
        .expect("the server stops cleanly");
}

/// Serves each of two sessions to a username of its own, both with
/// the session password.
struct TwoSessions {
    sessions: [(&'static str, Arc<SessionHandles>); 2],
}

impl SessionProvider for TwoSessions {
    fn session(&self, credentials: &Credentials) -> Result<Arc<SessionHandles>, Rejected> {
        if credentials.password != *PASSWORD {
            return Err(Rejected);
        }
        self.sessions
            .iter()
            .find(|(username, _)| *username == credentials.username)
            .map(|(_, handles)| Arc::clone(handles))
            .ok_or(Rejected)
    }
}

/// Connect a client presenting `login` that asks for the surface's own
/// size, so it is never resized, and read until it has the whole
/// surface.
async fn connect_served(addr: SocketAddr, login: &Login, what: &str) -> Client {
    let mut client = step(
        what,
        Client::connect_as(addr, WIDTH, HEIGHT, Channels::None, login),
    )
    .await
    .unwrap_or_else(|error| panic!("{what}: {error}"));
    expect_whole_surface(&mut client, what).await;
    client
}

/// Read until `client` is sent the whole surface, passing over pointer
/// updates, and check it decoded.
async fn expect_whole_surface(client: &mut Client, what: &str) {
    step(what, async {
        loop {
            match client.next_event().await {
                Event::Pointer(_) | Event::DefaultPointer | Event::HiddenPointer => {}
                Event::Graphics(rect) => {
                    assert_eq!(rect, whole_desktop(), "{what}: the first drawing");
                    break;
                }
                other => panic!("{what}: expected the whole surface, got {other:?}"),
            }
        }
    })
    .await;
    client.assert_colour(WIDTH / 4, HEIGHT / 4, TOP_LEFT, what);
}

/// Draw the block on `handles`' mirror and check that `client` is sent
/// it.
async fn expect_block(client: &mut Client, handles: &SessionHandles, what: &str) {
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
    step(what, async {
        loop {
            match client.next_event().await {
                Event::Pointer(_) | Event::DefaultPointer | Event::HiddenPointer => {}
                Event::Graphics(_) => break,
                other => panic!("{what}: expected the block's update, got {other:?}"),
            }
        }
    })
    .await;
    client.assert_colour(
        BLOCK_LEFT + BLOCK_SIDE / 2,
        BLOCK_TOP + BLOCK_SIDE / 2,
        BLOCK_COLOUR,
        what,
    );
}

/// The next input the session was sent, passing over lock-key
/// synchronizes, which clients send as they connect.
async fn next_key(input_rx: &mut mpsc::Receiver<InputEvent>, what: &str) -> InputEvent {
    loop {
        match step(what, input_rx.recv()).await {
            Some(InputEvent::KeyModifiers(_)) => {}
            Some(event) => return event,
            None => panic!("{what}: the session's input queue closed"),
        }
    }
}

/// Press and release A on `client`, and check both reach the session.
async fn expect_typing(client: &mut Client, input_rx: &mut mpsc::Receiver<InputEvent>, what: &str) {
    client
        .send_input(&[
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::empty(), KEY_A),
            FastPathInputEvent::KeyboardEvent(KeyboardFlags::RELEASE, KEY_A),
        ])
        .await;
    let pressed = next_key(input_rx, what).await;
    assert!(
        matches!(pressed, InputEvent::KeyDown(0x1E)),
        "{what}: expected KeyDown(0x1E), got {pressed:?}"
    );
    let released = next_key(input_rx, what).await;
    assert!(
        matches!(released, InputEvent::KeyUp(0x9E)),
        "{what}: expected KeyUp(0x9E), got {released:?}"
    );
}

/// Stop `server`, check `client` is disconnected, and wait for the
/// server to finish.
async fn stop(server: shakenfist_spice_rdp::RdpServerHandle, client: &mut Client) {
    server.shutdown();
    step("the client to be disconnected", client.wait_closed()).await;
    step("the server thread to finish", server.stopped()).await;
    server
        .join(Duration::ZERO)
        .expect("the server stops cleanly");
}

#[tokio::test]
async fn a_client_is_served_after_the_one_before_it_leaves() {
    install_crypto_provider();
    let Session {
        handles,
        mut input_rx,
        ..
    } = session();
    let server = serve(server_config(Arc::clone(&handles))).expect("serve");

    let mut first = connect_served(server.local_addr(), &Login::tls(), "the first client").await;
    expect_typing(&mut first, &mut input_rx, "the first client's typing").await;
    drop(first);

    let mut second = connect_served(server.local_addr(), &Login::tls(), "the second client").await;
    expect_block(&mut second, &handles, "the block, on the second client").await;
    expect_typing(&mut second, &mut input_rx, "the second client's typing").await;
    stop(server, &mut second).await;
}

#[tokio::test]
async fn a_second_client_with_the_password_takes_the_session_over() {
    install_crypto_provider();
    let Session {
        handles,
        mut input_rx,
        ..
    } = session();
    let server = serve(server_config(Arc::clone(&handles))).expect("serve");

    // The first client is served, and is holding A down when the
    // second connects.
    let mut first = connect_served(server.local_addr(), &Login::tls(), "the first client").await;
    first
        .send_input(&[FastPathInputEvent::KeyboardEvent(
            KeyboardFlags::empty(),
            KEY_A,
        )])
        .await;
    let pressed = next_key(&mut input_rx, "the first client's key press").await;
    assert!(
        matches!(pressed, InputEvent::KeyDown(0x1E)),
        "expected KeyDown(0x1E), got {pressed:?}"
    );

    let mut second = connect_served(server.local_addr(), &Login::tls(), "the second client").await;

    // The first client is disconnected, and the key it held is
    // released, before anything the second client types.
    step("the first client to be disconnected", first.wait_closed()).await;
    let released = next_key(&mut input_rx, "the first client's key release").await;
    assert!(
        matches!(released, InputEvent::KeyUp(0x9E)),
        "expected the held key to be released, got {released:?}"
    );

    // The second client is served the session.
    expect_block(&mut second, &handles, "the block, on the second client").await;
    expect_typing(&mut second, &mut input_rx, "the second client's typing").await;
    stop(server, &mut second).await;
}

/// A second client presenting the wrong password, over `security`,
/// leaves the client being served alone.
async fn wrong_password_leaves_the_served_client_alone(security: RdpSecurity, login: Login) {
    install_crypto_provider();
    let Session {
        handles,
        mut input_rx,
        ..
    } = session();
    let provider = Arc::new(SingleSession::new(
        server_credentials(),
        Arc::clone(&handles),
    ));
    let server = serve(server_config_with(security, provider)).expect("serve");
    let mut first = connect_served(server.local_addr(), &login, "the first client").await;

    // NLA refuses the client during the connection sequence. TLS lets
    // the sequence finish and then refuses the client's credentials,
    // ending its connection.
    let refused = step(
        "the wrong password's connection",
        Client::connect_as(
            server.local_addr(),
            WIDTH,
            HEIGHT,
            Channels::None,
            &login.with_wrong_password(),
        ),
    )
    .await;
    if let Ok(mut refused) = refused {
        step(
            "the refused client to be disconnected",
            refused.wait_refused(),
        )
        .await;
    }

    expect_block(&mut first, &handles, "the block, on the first client").await;
    expect_typing(&mut first, &mut input_rx, "the first client's typing").await;
    stop(server, &mut first).await;
}

#[tokio::test]
async fn a_second_client_with_the_wrong_password_over_tls_leaves_the_first_alone() {
    wrong_password_leaves_the_served_client_alone(RdpSecurity::Tls, Login::tls()).await;
}

#[tokio::test]
async fn a_second_client_with_the_wrong_password_over_nla_leaves_the_first_alone() {
    wrong_password_leaves_the_served_client_alone(RdpSecurity::Nla, Login::nla()).await;
}

#[tokio::test]
async fn a_client_of_another_session_leaves_the_first_alone() {
    install_crypto_provider();
    let Session {
        handles: alice,
        input_rx: mut alice_rx,
        ..
    } = session();
    let Session {
        handles: bob,
        input_rx: mut bob_rx,
        ..
    } = session();
    let provider = Arc::new(TwoSessions {
        sessions: [("alice", Arc::clone(&alice)), ("bob", Arc::clone(&bob))],
    });
    let server = serve(server_config_with(RdpSecurity::Tls, provider)).expect("serve");
    let login = |username: &str| Login {
        username: username.to_string(),
        ..Login::tls()
    };

    let mut first = connect_served(server.local_addr(), &login("alice"), "alice's client").await;
    let mut second = connect_served(server.local_addr(), &login("bob"), "bob's client").await;

    // Both are served their own session.
    expect_block(&mut first, &alice, "the block, on alice's client").await;
    expect_typing(&mut first, &mut alice_rx, "alice's typing").await;
    expect_block(&mut second, &bob, "the block, on bob's client").await;
    expect_typing(&mut second, &mut bob_rx, "bob's typing").await;

    server.shutdown();
    step("bob's client to be disconnected", second.wait_closed()).await;
    stop(server, &mut first).await;
}

#[tokio::test]
async fn a_client_that_never_authenticates_is_dropped_in_time() {
    install_crypto_provider();
    let Session { handles, .. } = session();
    let mut config = server_config(Arc::clone(&handles));
    config.handshake_timeout = SHORT_HANDSHAKE_TIMEOUT;
    let server = serve(config).expect("serve");

    // A client that connects and says nothing holds the one place for
    // a client that has not yet authenticated, so the next client
    // waits, unaccepted, until it is dropped.
    let opened = tokio::time::Instant::now();
    let mut stalled = TcpStream::connect(server.local_addr())
        .await
        .expect("TCP connect");
    let mut client = connect_served(server.local_addr(), &Login::tls(), "the next client").await;
    assert!(
        opened.elapsed() >= SHORT_HANDSHAKE_TIMEOUT,
        "the next client was served after {:?}, before the stalled one's time was up",
        opened.elapsed()
    );

    // The server closed the stalled connection.
    let mut byte = [0u8; 1];
    let read = step(
        "the stalled connection to be closed",
        tokio::io::AsyncReadExt::read(&mut stalled, &mut byte),
    )
    .await;
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "the stalled connection was sent {read:?}"
    );
    stop(server, &mut client).await;
}

#[tokio::test]
async fn a_stop_ends_every_client_and_the_server() {
    install_crypto_provider();
    let Session { handles, .. } = session();
    let server = serve(server_config(Arc::clone(&handles))).expect("serve");
    let mut client = connect_served(server.local_addr(), &Login::tls(), "the client").await;
    // A second connection that has not authenticated, and never will.
    let mut stalled = TcpStream::connect(server.local_addr())
        .await
        .expect("TCP connect");

    server.shutdown();
    step("the client to be disconnected", client.wait_closed()).await;
    let mut byte = [0u8; 1];
    let read = step(
        "the stalled connection to be closed",
        tokio::io::AsyncReadExt::read(&mut stalled, &mut byte),
    )
    .await;
    assert!(
        matches!(read, Ok(0) | Err(_)),
        "the stalled connection was sent {read:?}"
    );
    // `join` blocks, so it runs off the test's runtime thread.
    let joined = tokio::task::spawn_blocking(move || server.join(STEP_TIMEOUT))
        .await
        .expect("the join task");
    joined.expect("the server stops cleanly");
}
