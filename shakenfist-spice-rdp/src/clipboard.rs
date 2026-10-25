//! The clipboard channel: text the RDP client copies reaches the
//! guest, and text the guest copies reaches the client, through the
//! session's [`MemoryClipboard`], which the main channel ferries to
//! and from the guest's agent.
//!
//! IronRDP builds a [`SessionClipboard`] backend for every connection
//! from the [`ClipboardFactory`] registered with the server, and calls
//! it as the client's clipboard PDUs arrive. The backend answers by
//! sending [`ClipboardMessage`]s on the server's event sender, which
//! the connection's loop applies to the channel with `?`: a message
//! the channel refuses ends the whole RDP connection, not just the
//! paste. The channel refuses a paste request or a data response until
//! it is Ready, which on the server side is when the client's first
//! Format List arrives. The backend therefore cannot send anything
//! before [`CliprdrBackend::on_ready`]: until then it holds the sender
//! in a form with no way to send on it (see [`gate`]).
//!
//! When the client copies text, the backend fetches it at once and
//! stores it as host text, because the main channel reads host text
//! synchronously and has no way to ask for it later. A client sends
//! its current clipboard as its first Format List, so on connect the
//! client's clipboard is the one that reaches the guest; guest copies
//! made before the client connected are not offered to it, only the
//! ones made after the channel is Ready. Those are offered as a Format
//! List naming `CF_UNICODETEXT`, and the text is read from the
//! clipboard only when the client asks for it.
//!
//! `CF_UNICODETEXT` is Windows text, with CRLF line endings, while the
//! session's host-side text has LF line endings, as on a Unix host.
//! The backend converts at this boundary in both directions.
//!
//! Text from the client longer than the guest's agent can be sent in
//! one clipboard message is dropped rather than stored.
//!
//! Only byte counts are logged, never the text.

use ironrdp_cliprdr::backend::{ClipboardMessage, CliprdrBackend, CliprdrBackendFactory};
use ironrdp_cliprdr::pdu::{
    ClipboardFormat, ClipboardFormatId, ClipboardGeneralCapabilityFlags, FileContentsRequest,
    FileContentsResponse, FormatDataRequest, FormatDataResponse, LockDataId,
    OwnedFormatDataResponse,
};
use ironrdp_server::{CliprdrServerFactory, ServerEvent, ServerEventSender};
use shakenfist_spice_protocol::messages::agent_stream::MAX_AGENT_MESSAGE_SIZE;
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::SessionSlot;

/// The longest text taken from the client, in bytes once its line
/// endings are LF: 100 MiB, the most the guest's agent can be sent in
/// one clipboard message (whose budget adds 4 KiB for its headers), and
/// the most the control socket's `clipboard_set` accepts.
const MAX_CLIENT_TEXT_BYTES: usize = MAX_AGENT_MESSAGE_SIZE - 4096;

/// Builds a [`SessionClipboard`] for each RDP connection.
///
/// The server gives the factory its event sender once, when the
/// server is built, and each backend gets a clone of it.
pub(crate) struct ClipboardFactory {
    slot: SessionSlot,
    events: Option<mpsc::UnboundedSender<ServerEvent>>,
}

impl ClipboardFactory {
    /// A factory whose backends serve the clipboard of the session in
    /// `slot`.
    pub(crate) fn new(slot: SessionSlot) -> Self {
        Self { slot, events: None }
    }
}

impl CliprdrBackendFactory for ClipboardFactory {
    fn build_cliprdr_backend(&self) -> Box<dyn CliprdrBackend> {
        Box::new(SessionClipboard::new(
            self.slot.clone(),
            self.events.clone(),
        ))
    }
}

impl ServerEventSender for ClipboardFactory {
    fn set_sender(&mut self, sender: mpsc::UnboundedSender<ServerEvent>) {
        self.events = Some(sender);
    }
}

impl CliprdrServerFactory for ClipboardFactory {}

/// The only way to send a clipboard message to the client.
///
/// Kept in a module of its own so that the event sender's fields are
/// private to it: the rest of this file can reach the sender only
/// through [`Ready`], and gets a `Ready` only by giving up an
/// [`Unready`], which [`SessionClipboard::on_ready`] alone does. So
/// no code path can send before the channel is Ready, rather than
/// each send site checking that it is.
mod gate {
    use std::sync::Arc;

    use ironrdp_cliprdr::backend::ClipboardMessage;
    use ironrdp_server::ServerEvent;
    use shakenfist_spice_renderer::MemoryClipboard;
    use tokio::sync::mpsc;

    /// The server's event sender, held until the channel is Ready. It
    /// offers no way to send.
    pub(super) struct Unready(mpsc::UnboundedSender<ServerEvent>);

    impl Unready {
        pub(super) fn new(events: mpsc::UnboundedSender<ServerEvent>) -> Self {
            Self(events)
        }

        /// The channel is Ready: send on its behalf from now on,
        /// serving `clipboard`.
        pub(super) fn into_ready(self, clipboard: Arc<MemoryClipboard>) -> Ready {
            Ready {
                events: self.0,
                clipboard,
            }
        }
    }

    /// A Ready channel serving a session's clipboard.
    #[derive(Clone)]
    pub(super) struct Ready {
        events: mpsc::UnboundedSender<ServerEvent>,
        clipboard: Arc<MemoryClipboard>,
    }

    impl Ready {
        /// Send `message` to the connection's loop, which applies it
        /// to the channel. Returns false once the server has gone,
        /// when there is no one left to send to.
        pub(super) fn send(&self, message: ClipboardMessage) -> bool {
            self.events.send(ServerEvent::Clipboard(message)).is_ok()
        }

        /// The session clipboard this channel serves.
        pub(super) fn clipboard(&self) -> &MemoryClipboard {
            &self.clipboard
        }
    }
}

/// Where a connection's clipboard channel is.
enum State {
    /// Before the channel is Ready. `None` if the server never gave
    /// the factory a sender, which leaves nothing to send with.
    Starting(Option<gate::Unready>),
    /// Ready, and serving the session's clipboard.
    Ready(gate::Ready),
    /// Ready, with nothing to serve: the session has no clipboard, or
    /// there is no sender. The client is ignored.
    Idle,
}

/// The clipboard backend for one RDP connection.
pub(crate) struct SessionClipboard {
    slot: SessionSlot,
    state: State,
    /// The task that offers each guest copy to the client, from when
    /// the channel is Ready until the backend is dropped with its
    /// connection.
    guest_copies: Option<JoinHandle<()>>,
}

impl SessionClipboard {
    fn new(slot: SessionSlot, events: Option<mpsc::UnboundedSender<ServerEvent>>) -> Self {
        Self {
            slot,
            state: State::Starting(events.map(gate::Unready::new)),
            guest_copies: None,
        }
    }

    /// The channel, if it is Ready and serving a clipboard.
    fn ready(&self) -> Option<&gate::Ready> {
        match &self.state {
            State::Ready(ready) => Some(ready),
            State::Starting(_) | State::Idle => None,
        }
    }

    /// Make the channel Ready, serving the clipboard of the session in
    /// the slot, and start offering the guest's copies to the client.
    fn become_ready(&mut self, unready: Option<gate::Unready>) -> State {
        let clipboard = self
            .slot
            .current()
            .and_then(|session| session.clipboard.clone());
        let (Some(unready), Some(clipboard)) = (unready, clipboard) else {
            debug!("rdp clipboard: the session has no clipboard; the client's is ignored");
            return State::Idle;
        };
        // Subscribed here rather than in the task, so that the
        // subscription starts now: a guest copy made before this point
        // is not offered, and one made after it is, even if the task
        // has not run yet.
        let copies = clipboard.subscribe_guest_copies();
        let ready = unready.into_ready(clipboard);
        // IronRDP calls the backend from inside the server's runtime.
        // Without one the client's copies still reach the guest; only
        // the guest's are not offered.
        match tokio::runtime::Handle::try_current() {
            Ok(runtime) => {
                self.guest_copies = Some(runtime.spawn(offer_guest_copies(ready.clone(), copies)));
            }
            Err(_) => warn!("rdp clipboard: no runtime; the guest's copies are not offered"),
        }
        debug!("rdp clipboard: channel ready");
        State::Ready(ready)
    }
}

impl Drop for SessionClipboard {
    fn drop(&mut self) {
        if let Some(task) = self.guest_copies.take() {
            task.abort();
        }
    }
}

impl std::fmt::Debug for SessionClipboard {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = match self.state {
            State::Starting(_) => "starting",
            State::Ready(_) => "ready",
            State::Idle => "idle",
        };
        f.debug_struct("SessionClipboard")
            .field("state", &state)
            .finish()
    }
}

ironrdp_core::impl_as_any!(SessionClipboard);

/// Offer the client each guest copy reported on `copies`, until the
/// clipboard or the server goes away.
async fn offer_guest_copies(channel: gate::Ready, mut copies: watch::Receiver<Option<usize>>) {
    while copies.changed().await.is_ok() {
        let bytes = *copies.borrow_and_update();
        debug!(
            ?bytes,
            "rdp clipboard: the guest copied text; offering it to the client"
        );
        let offered = channel.send(ClipboardMessage::SendInitiateCopy(vec![
            ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT),
        ]));
        if !offered {
            break;
        }
    }
}

impl CliprdrBackend for SessionClipboard {
    /// Unused: files are not carried.
    fn temporary_directory(&self) -> &str {
        ""
    }

    /// Text only: no file streams and no clipboard locking.
    fn client_capabilities(&self) -> ClipboardGeneralCapabilityFlags {
        ClipboardGeneralCapabilityFlags::empty()
    }

    fn on_ready(&mut self) {
        let state = std::mem::replace(&mut self.state, State::Idle);
        self.state = match state {
            State::Starting(unready) => self.become_ready(unready),
            already => already,
        };
    }

    fn on_request_format_list(&mut self) {
        debug!("rdp clipboard: format list request ignored");
    }

    fn on_process_negotiated_capabilities(
        &mut self,
        capabilities: ClipboardGeneralCapabilityFlags,
    ) {
        debug!(?capabilities, "rdp clipboard: capabilities negotiated");
    }

    /// The client copied: fetch its text now, if it has any.
    fn on_remote_copy(&mut self, available_formats: &[ClipboardFormat]) {
        let Some(channel) = self.ready() else {
            debug!("rdp clipboard: client copy ignored");
            return;
        };
        if !available_formats
            .iter()
            .any(|format| format.id() == ClipboardFormatId::CF_UNICODETEXT)
        {
            debug!(
                formats = available_formats.len(),
                "rdp clipboard: the client copied no text"
            );
            return;
        }
        channel.send(ClipboardMessage::SendInitiatePaste(
            ClipboardFormatId::CF_UNICODETEXT,
        ));
    }

    /// The client asks for the text a guest copy offered it.
    fn on_format_data_request(&mut self, request: FormatDataRequest) {
        let Some(channel) = self.ready() else {
            debug!("rdp clipboard: data request ignored");
            return;
        };
        let text = if request.format == ClipboardFormatId::CF_UNICODETEXT {
            channel.clipboard().text().filter(|text| !text.is_empty())
        } else {
            debug!(
                format = request.format.value(),
                "rdp clipboard: data request for a format not offered"
            );
            None
        };
        let response = match text {
            Some(text) => {
                let text = lf_to_crlf(&text);
                debug!(
                    bytes = text.len(),
                    "rdp clipboard: sending the guest's text to the client"
                );
                OwnedFormatDataResponse::new_unicode_string(&text)
            }
            None => OwnedFormatDataResponse::new_error(),
        };
        channel.send(ClipboardMessage::SendFormatData(response));
    }

    /// The client's text, fetched by [`on_remote_copy`](Self::on_remote_copy).
    fn on_format_data_response(&mut self, response: FormatDataResponse<'_>) {
        let Some(channel) = self.ready() else {
            debug!("rdp clipboard: data response ignored");
            return;
        };
        if response.is_error() {
            debug!("rdp clipboard: the client could not supply its text");
            return;
        }
        match client_text(&response, MAX_CLIENT_TEXT_BYTES) {
            Ok(text) => {
                debug!(
                    bytes = text.len(),
                    "rdp clipboard: storing the client's text for the guest"
                );
                channel.clipboard().set_host_text(text);
            }
            Err(ClientTextError::Utf16TooLong(utf16_bytes)) => warn!(
                utf16_bytes,
                limit = MAX_CLIENT_TEXT_BYTES,
                "rdp clipboard: the client's text is too long for the guest; dropped"
            ),
            Err(ClientTextError::TooLong(bytes)) => warn!(
                bytes,
                limit = MAX_CLIENT_TEXT_BYTES,
                "rdp clipboard: the client's text is too long for the guest; dropped"
            ),
            Err(ClientTextError::Undecodable(error)) => {
                warn!(%error, "rdp clipboard: the client's text could not be decoded")
            }
        }
    }

    fn on_file_contents_request(&mut self, _request: FileContentsRequest) {
        debug!("rdp clipboard: file contents request ignored");
    }

    fn on_file_contents_response(&mut self, _response: FileContentsResponse<'_>) {
        debug!("rdp clipboard: file contents response ignored");
    }

    fn on_lock(&mut self, _data_id: LockDataId) {
        debug!("rdp clipboard: lock ignored");
    }

    fn on_unlock(&mut self, _data_id: LockDataId) {
        debug!("rdp clipboard: unlock ignored");
    }
}

/// Why the client's text was not taken.
#[derive(Debug)]
enum ClientTextError {
    /// Its UTF-16 data, this many bytes, could not decode to text short
    /// enough.
    Utf16TooLong(usize),
    /// It decoded to this many bytes, more than the limit.
    TooLong(usize),
    /// It is not UTF-16.
    Undecodable(ironrdp_core::DecodeError),
}

/// The client's `CF_UNICODETEXT` data as text with LF line endings, if
/// that is at most `limit` bytes.
///
/// Data too long for any decode of it to fit is refused before it is
/// decoded. A UTF-16 unit, two bytes, decodes to at least one byte, and
/// a CRLF, two units, becomes one LF, so that is data more than four
/// times the limit. Anything after a NUL is not text, and does not
/// count.
fn client_text(response: &FormatDataResponse<'_>, limit: usize) -> Result<String, ClientTextError> {
    let utf16_bytes = response.data().len();
    if utf16_bytes / 4 > limit {
        return Err(ClientTextError::Utf16TooLong(utf16_bytes));
    }
    let text = response
        .to_unicode_string()
        .map_err(ClientTextError::Undecodable)?;
    let text = crlf_to_lf(&text);
    if text.len() > limit {
        return Err(ClientTextError::TooLong(text.len()));
    }
    Ok(text)
}

/// Windows text from the client, with each CRLF turned into LF. A lone
/// CR is kept.
fn crlf_to_lf(text: &str) -> String {
    text.replace("\r\n", "\n")
}

/// Text for the client, with each LF not already preceded by CR turned
/// into CRLF, so text that already has CRLF line endings is unchanged.
fn lf_to_crlf(text: &str) -> String {
    let mut converted = String::with_capacity(text.len() + text.matches('\n').count());
    let mut previous = None;
    for c in text.chars() {
        if c == '\n' && previous != Some('\r') {
            converted.push('\r');
        }
        converted.push(c);
        previous = Some(c);
    }
    converted
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use shakenfist_spice_renderer::{ClipboardBackend, MemoryClipboard, SurfaceMirror};

    use super::*;
    use crate::{CursorState, SessionHandles};

    /// A backend built by the factory for a session with or without a
    /// clipboard, and the receiving end of the server's event sender.
    struct Harness {
        backend: Box<dyn CliprdrBackend>,
        events: mpsc::UnboundedReceiver<ServerEvent>,
        clipboard: Arc<MemoryClipboard>,
    }

    /// A slot holding a session with `clipboard`.
    fn session_slot(clipboard: Option<Arc<MemoryClipboard>>) -> SessionSlot {
        let (input_tx, _) = mpsc::channel(1);
        let (_, mouse_mode) = watch::channel(0);
        let (_, cursor) = watch::channel(CursorState::default());
        let (resize_tx, _) = mpsc::channel(1);
        let mirror = SurfaceMirror::with_test_surface(0, 0, 16, 16, &[]);
        let slot = SessionSlot::new();
        slot.set(Arc::new(SessionHandles {
            surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
            input_tx,
            mouse_mode,
            resize_tx,
            cursor,
            clipboard,
            audio: None,
            label: "test".to_string(),
        }));
        slot
    }

    fn harness(with_clipboard: bool) -> Harness {
        let clipboard = Arc::new(MemoryClipboard::new());
        let slot = session_slot(with_clipboard.then(|| clipboard.clone()));
        let (events_tx, events) = mpsc::unbounded_channel();
        let mut factory = ClipboardFactory::new(slot);
        factory.set_sender(events_tx);
        Harness {
            backend: factory.build_cliprdr_backend(),
            events,
            clipboard,
        }
    }

    /// The next clipboard message the backend sent, or `None` if it
    /// sent nothing. Time is paused, so the wait costs nothing and
    /// ends as soon as every task is idle.
    async fn next_message(
        events: &mut mpsc::UnboundedReceiver<ServerEvent>,
    ) -> Option<ClipboardMessage> {
        match tokio::time::timeout(Duration::from_secs(1), events.recv()).await {
            Ok(Some(ServerEvent::Clipboard(message))) => Some(message),
            Ok(Some(other)) => panic!("unexpected server event {other:?}"),
            Ok(None) | Err(_) => None,
        }
    }

    fn text_format() -> ClipboardFormat {
        ClipboardFormat::new(ClipboardFormatId::CF_UNICODETEXT)
    }

    fn utf16_with_nul(text: &str) -> Vec<u8> {
        let mut bytes: Vec<u8> = text.encode_utf16().flat_map(u16::to_le_bytes).collect();
        bytes.extend_from_slice(&[0, 0]);
        bytes
    }

    fn request(format: ClipboardFormatId) -> FormatDataRequest {
        FormatDataRequest { format }
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_copy_after_ready_is_offered_once_as_text() {
        let mut h = harness(true);
        h.backend.on_ready();
        h.clipboard.set_text("copied in the guest").unwrap();
        match next_message(&mut h.events).await {
            Some(ClipboardMessage::SendInitiateCopy(formats)) => {
                assert_eq!(formats, [text_format()])
            }
            other => panic!("expected a format list, got {other:?}"),
        }
        assert!(next_message(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_guest_copy_before_ready_is_never_offered() {
        let mut h = harness(true);
        h.clipboard
            .set_text("copied before the client connected")
            .unwrap();
        assert!(next_message(&mut h.events).await.is_none());
        h.backend.on_ready();
        assert!(next_message(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_text_copy_after_ready_is_fetched_at_once() {
        let mut h = harness(true);
        h.backend.on_ready();
        h.backend.on_remote_copy(&[
            ClipboardFormat::new(ClipboardFormatId::CF_TEXT),
            text_format(),
        ]);
        match next_message(&mut h.events).await {
            Some(ClipboardMessage::SendInitiatePaste(format)) => {
                assert_eq!(format, ClipboardFormatId::CF_UNICODETEXT);
            }
            other => panic!("expected a paste request, got {other:?}"),
        }
        assert!(next_message(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_copy_without_unicode_text_is_not_fetched() {
        let mut h = harness(true);
        h.backend.on_ready();
        h.backend.on_remote_copy(&[
            ClipboardFormat::new(ClipboardFormatId::CF_TEXT),
            ClipboardFormat::new(ClipboardFormatId::CF_DIB),
        ]);
        h.backend.on_remote_copy(&[]);
        assert!(next_message(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_client_copy_before_ready_is_not_fetched() {
        let mut h = harness(true);
        h.backend.on_remote_copy(&[text_format()]);
        assert!(next_message(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn the_clients_text_is_stored_with_lf_and_not_echoed() {
        let mut h = harness(true);
        let guest_copies = h.clipboard.subscribe_guest_copies();
        h.backend.on_ready();
        h.backend
            .on_format_data_response(FormatDataResponse::new_unicode_string(
                "first line\r\nsecond line\r\n",
            ));
        assert_eq!(
            h.clipboard.text().as_deref(),
            Some("first line\nsecond line\n")
        );
        assert!(
            !guest_copies.has_changed().unwrap(),
            "the client's text is not a guest copy"
        );
        // Nor is it offered back to the client.
        assert!(next_message(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn an_error_response_stores_nothing() {
        let mut h = harness(true);
        h.clipboard.set_host_text("earlier".to_string());
        h.backend.on_ready();
        h.backend
            .on_format_data_response(FormatDataResponse::new_error());
        assert_eq!(h.clipboard.text().as_deref(), Some("earlier"));
    }

    #[tokio::test(start_paused = true)]
    async fn a_data_request_is_answered_with_crlf_utf16_text() {
        let mut h = harness(true);
        h.backend.on_ready();
        h.clipboard
            .set_text("línea uno\nzweite Zeile ü\n三\n")
            .unwrap();
        // The offer the guest copy made.
        assert!(matches!(
            next_message(&mut h.events).await,
            Some(ClipboardMessage::SendInitiateCopy(_))
        ));
        h.backend
            .on_format_data_request(request(ClipboardFormatId::CF_UNICODETEXT));
        match next_message(&mut h.events).await {
            Some(ClipboardMessage::SendFormatData(response)) => {
                assert!(!response.is_error());
                assert_eq!(
                    response.data(),
                    utf16_with_nul("línea uno\r\nzweite Zeile ü\r\n三\r\n")
                );
            }
            other => panic!("expected format data, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_data_request_with_no_text_is_answered_with_an_error() {
        let mut h = harness(true);
        h.backend.on_ready();
        h.backend
            .on_format_data_request(request(ClipboardFormatId::CF_UNICODETEXT));
        h.clipboard.set_host_text(String::new());
        h.backend
            .on_format_data_request(request(ClipboardFormatId::CF_UNICODETEXT));
        for _ in 0..2 {
            match next_message(&mut h.events).await {
                Some(ClipboardMessage::SendFormatData(response)) => assert!(response.is_error()),
                other => panic!("expected an error response, got {other:?}"),
            }
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_data_request_for_another_format_is_answered_with_an_error() {
        let mut h = harness(true);
        h.clipboard.set_host_text("text".to_string());
        h.backend.on_ready();
        h.backend
            .on_format_data_request(request(ClipboardFormatId::CF_TEXT));
        match next_message(&mut h.events).await {
            Some(ClipboardMessage::SendFormatData(response)) => assert!(response.is_error()),
            other => panic!("expected an error response, got {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_data_request_before_ready_is_not_answered() {
        let mut h = harness(true);
        h.clipboard.set_host_text("text".to_string());
        h.backend
            .on_format_data_request(request(ClipboardFormatId::CF_UNICODETEXT));
        assert!(next_message(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_session_without_a_clipboard_sends_nothing() {
        let mut h = harness(false);
        h.backend.on_ready();
        h.clipboard.set_text("guest").unwrap();
        h.backend.on_remote_copy(&[text_format()]);
        h.backend
            .on_format_data_request(request(ClipboardFormatId::CF_UNICODETEXT));
        h.backend
            .on_format_data_response(FormatDataResponse::new_unicode_string("client"));
        assert!(next_message(&mut h.events).await.is_none());
        assert_eq!(h.clipboard.text().as_deref(), Some("guest"));
    }

    #[tokio::test(start_paused = true)]
    async fn guest_copies_stop_with_the_backend() {
        let mut h = harness(true);
        h.backend.on_ready();
        drop(h.backend);
        h.clipboard.set_text("after the connection").unwrap();
        assert!(next_message(&mut h.events).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn a_backend_with_no_sender_ignores_the_client() {
        let clipboard = Arc::new(MemoryClipboard::new());
        let mut backend = SessionClipboard::new(session_slot(Some(clipboard.clone())), None);
        backend.on_ready();
        assert!(backend.ready().is_none());
        backend.on_format_data_response(FormatDataResponse::new_unicode_string("client"));
        assert_eq!(clipboard.text(), None);
    }

    #[test]
    fn the_clients_text_is_taken_up_to_the_limit_once_its_line_endings_are_lf() {
        // Seven UTF-16 units with the NUL, and five bytes once the CRLF
        // is LF.
        let response = FormatDataResponse::new_unicode_string("ab\r\ncd");
        assert_eq!(client_text(&response, 5).unwrap(), "ab\ncd");
        assert!(matches!(
            client_text(&response, 4),
            Err(ClientTextError::TooLong(5))
        ));
    }

    #[test]
    fn data_that_could_decode_to_text_that_fits_is_decoded() {
        // Four bytes of UTF-16 for each LF it becomes, and the NUL.
        let limit = 8;
        let response = FormatDataResponse::new_unicode_string(&"\r\n".repeat(limit));
        assert_eq!(response.data().len(), 4 * limit + 2);
        assert_eq!(client_text(&response, limit).unwrap(), "\n".repeat(limit));
    }

    #[test]
    fn data_too_long_for_any_decode_to_fit_is_refused_before_decoding() {
        // Lone surrogates, which do not decode: only a refusal made
        // before decoding says the data is too long.
        let limit = 8;
        let lone_surrogates = |units: usize| [0x00, 0xD8].repeat(units);
        let longest = FormatDataResponse::new_data(lone_surrogates(2 * limit + 1));
        assert!(matches!(
            client_text(&longest, limit),
            Err(ClientTextError::Undecodable(_))
        ));
        let too_long = FormatDataResponse::new_data(lone_surrogates(2 * limit + 2));
        assert!(matches!(
            client_text(&too_long, limit),
            Err(ClientTextError::Utf16TooLong(36))
        ));
    }

    #[test]
    fn crlf_becomes_lf_and_a_lone_cr_is_kept() {
        assert_eq!(crlf_to_lf("a\r\nb\nc\rd\r\n"), "a\nb\nc\rd\n");
        assert_eq!(crlf_to_lf("no line ending"), "no line ending");
        assert_eq!(crlf_to_lf(""), "");
    }

    #[test]
    fn lf_becomes_crlf_without_doubling_an_existing_crlf() {
        assert_eq!(lf_to_crlf("a\nb\r\nc\n"), "a\r\nb\r\nc\r\n");
        assert_eq!(lf_to_crlf("a\r\n\r\nb"), "a\r\n\r\nb");
        assert_eq!(lf_to_crlf("\n\n"), "\r\n\r\n");
        assert_eq!(lf_to_crlf("lone\rcr"), "lone\rcr");
        assert_eq!(crlf_to_lf(&lf_to_crlf("x\ny\n")), "x\ny\n");
    }
}
