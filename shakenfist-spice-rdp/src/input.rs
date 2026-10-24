//! The RDP input handler: translates RDP keyboard, lock-key and mouse
//! events into the renderer's input events for the SPICE inputs
//! channel, in both SPICE mouse modes.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, Weak};

use ironrdp_server::{KeyboardEvent, MouseEvent, RdpServerInputHandler};
use shakenfist_spice_protocol::{keyboard_modifiers, mouse_buttons, warn_once, MOUSE_MODE_SERVER};
use shakenfist_spice_renderer::{make_scancode, InputEvent, PointerTranslator};
use tokio::sync::mpsc::error::TrySendError;
use tracing::{debug, info};

use crate::{SessionHandles, SessionSlot};

/// Wheel rotation units per notch of an ordinary mouse wheel.
///
/// RDP reports wheel movement in the units of MS-RDPBCGR's
/// `WheelRotationMask`, where one notch is 120 (Windows'
/// `WHEEL_DELTA`). High-resolution wheels and touchpads send fractions
/// of a notch, so the handler accumulates units and sends one SPICE
/// wheel click per whole notch.
const WHEEL_UNITS_PER_NOTCH: i32 = 120;

/// The most wheel clicks one RDP event may become. A physical wheel
/// event is a notch or two; the standard mouse PDU cannot carry more
/// than two. A larger count means a client encodes its units
/// differently from what the handler expects, and passing it through
/// would flood the SPICE inputs channel with thousands of clicks, so
/// the excess is dropped and logged once.
const MAX_WHEEL_CLICKS_PER_EVENT: i32 = 8;

/// The lock keys RDP's synchronize event and SPICE's
/// `KEY_MODIFIERS` share. Both put Scroll, Num and Caps Lock in the
/// low three bits in that order; RDP's fourth bit, Kana Lock, has no
/// SPICE counterpart and is dropped.
const LOCK_KEYS: u16 =
    keyboard_modifiers::SCROLL_LOCK | keyboard_modifiers::NUM_LOCK | keyboard_modifiers::CAPS_LOCK;

/// The keys and buttons a client has pressed and not yet released,
/// shared between the input handler and the connection handler.
///
/// An RDP client can vanish mid-press (a dropped network, a killed
/// client window), and its releases then never arrive, so the guest
/// would hold the key or button until some later client happened to
/// release it. The connection handler ends each connection with
/// [`end_connection`](Self::end_connection), which hands back the
/// releases for whatever is still held and starts a new connection,
/// so the next client's pointer starts clean even when it is served
/// the same session. Clones share the same record.
///
/// The record is per connection, not per session: it relies on one
/// connection serving one session, which holds because the slot is
/// filled once per TLS connection by the credential check, or once
/// at startup for NLA. A provider that switched sessions within a
/// connection would need to release to the old session first.
#[derive(Clone, Default)]
pub(crate) struct HeldInput(Arc<HeldShared>);

#[derive(Default)]
struct HeldShared {
    state: Mutex<HeldState>,
    /// Counts ended connections; the input handler resets its
    /// pointer state when this changes.
    connection: AtomicU64,
}

#[derive(Default)]
struct HeldState {
    /// The release scancodes of the keys held down.
    keys: BTreeSet<u32>,
    /// The SPICE button masks held down.
    buttons: BTreeSet<u32>,
    /// The pointer's position this connection, where held buttons
    /// are released; `None` until the client first moves.
    pos: Option<(u32, u32)>,
}

impl HeldInput {
    /// Record a key going down or up, by its release scancode.
    fn key(&self, release: u32, down: bool) {
        let mut state = self.lock();
        if down {
            state.keys.insert(release);
        } else {
            state.keys.remove(&release);
        }
    }

    /// Record a button going down or up.
    fn button(&self, button: u32, down: bool) {
        let mut state = self.lock();
        if down {
            state.buttons.insert(button);
        } else {
            state.buttons.remove(&button);
        }
    }

    /// Record where the pointer is.
    fn moved(&self, pos: (u32, u32)) {
        self.lock().pos = Some(pos);
    }

    /// The number of connections ended so far.
    fn connection(&self) -> u64 {
        self.0.connection.load(Ordering::Acquire)
    }

    /// End the current connection: start a new one, and return the
    /// events releasing every key and button still held. Buttons are
    /// released where this connection's pointer last was, or at the
    /// origin, as [`PointerTranslator`] does, if it never moved.
    pub(crate) fn end_connection(&self) -> Vec<InputEvent> {
        // Counted under the lock, so no key, button or position from
        // the next connection can land in this one's releases.
        let mut state = self.lock();
        self.0.connection.fetch_add(1, Ordering::AcqRel);
        let (x, y) = state.pos.take().unwrap_or((0, 0));
        let keys = std::mem::take(&mut state.keys);
        let buttons = std::mem::take(&mut state.buttons);
        keys.into_iter()
            .map(InputEvent::KeyUp)
            .chain(
                buttons
                    .into_iter()
                    .map(|button| InputEvent::MouseUp { button, x, y }),
            )
            .collect()
    }

    /// Lock the record. A panic while the lock was held leaves at
    /// worst a stale entry, which costs one spurious release, so a
    /// poisoned lock is recovered rather than propagated.
    fn lock(&self) -> MutexGuard<'_, HeldState> {
        self.0
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// Translates RDP keyboard and mouse events into SPICE
/// [`InputEvent`]s for the session in a [`SessionSlot`].
///
/// IronRDP calls the handler synchronously from its connection task,
/// so events are queued on the session's input sender with
/// `try_send` rather than awaited. An event that cannot be queued
/// (the queue is full or closed, no session has been chosen yet, or
/// the surface mirror is busy when a relative move needs its size) is
/// dropped and counted in [`dropped`](Self::dropped), and a full or
/// closed queue is logged once.
///
/// Pointer events go through a [`PointerTranslator`], which picks
/// absolute positions or relative deltas for the session's current
/// SPICE mouse mode. RDP's absolute coordinates are desktop pixels,
/// and the desktop is the session's primary surface, so they need no
/// scaling.
pub struct SpiceInputHandler {
    slot: SessionSlot,
    pointer: PointerTranslator,
    /// Wheel rotation units received but not yet sent as a whole
    /// notch.
    wheel_remainder: i32,
    /// Whether the client has sent relative mouse input, logged once
    /// so an operator can tell which pointer path a client uses.
    relative_seen: bool,
    /// The session and connection the pointer state belongs to, so
    /// a different session, or the next client of the same one,
    /// starts from a clean pointer rather than inheriting a position
    /// from the last one.
    session: Weak<SessionHandles>,
    connection: u64,
    held: HeldInput,
    dropped: Arc<AtomicU64>,
}

impl SpiceInputHandler {
    /// A handler serving whichever session `slot` holds when each
    /// event arrives.
    pub fn new(slot: SessionSlot) -> Self {
        Self {
            slot,
            pointer: PointerTranslator::new(),
            wheel_remainder: 0,
            relative_seen: false,
            session: Weak::new(),
            connection: 0,
            held: HeldInput::default(),
            dropped: Arc::new(AtomicU64::new(0)),
        }
    }

    /// The handler's record of held keys and buttons, for the
    /// connection handler to end each connection with.
    pub(crate) fn held(&self) -> HeldInput {
        self.held.clone()
    }

    /// The number of input events dropped so far.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// A shared handle on the dropped-event count, which stays
    /// readable after the handler has been handed to the server.
    pub fn dropped_counter(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.dropped)
    }

    /// The session to send to, or `None` (counting the drop) when no
    /// session has been chosen. A session other than the last one, or
    /// a new connection, resets the pointer and wheel state.
    fn session(&mut self) -> Option<Arc<SessionHandles>> {
        let Some(session) = self.slot.current() else {
            self.count_drop();
            debug!("RDP input: no session chosen yet; dropping an input event");
            return None;
        };
        let connection = self.held.connection();
        if connection != self.connection || !Weak::ptr_eq(&self.session, &Arc::downgrade(&session))
        {
            self.pointer.reset();
            self.wheel_remainder = 0;
            self.session = Arc::downgrade(&session);
            self.connection = connection;
        }
        Some(session)
    }

    fn count_drop(&self) {
        self.dropped.fetch_add(1, Ordering::Relaxed);
    }

    /// Queue `event` for the session's inputs channel, dropping and
    /// counting it if the queue cannot take it now. Returns whether
    /// it was queued.
    fn send(&self, session: &SessionHandles, event: InputEvent) -> bool {
        match session.input_tx.try_send(event) {
            Ok(()) => true,
            Err(TrySendError::Full(_)) => {
                self.count_drop();
                warn_once!(
                    "rdp-input:queue-full",
                    "RDP input: the SPICE input queue for {} is full; dropping input events",
                    session.label
                );
                false
            }
            Err(TrySendError::Closed(_)) => {
                self.count_drop();
                warn_once!(
                    "rdp-input:queue-closed",
                    "RDP input: the SPICE input queue for {} is closed; dropping input events",
                    session.label
                );
                false
            }
        }
    }

    /// Queue each of `events`, returning whether all were queued.
    fn send_all(&self, session: &SessionHandles, events: Vec<InputEvent>) -> bool {
        let mut all = true;
        for event in events {
            all &= self.send(session, event);
        }
        all
    }

    fn button(&mut self, session: &SessionHandles, mode: u32, button: u32, down: bool) {
        // RDP button events carry no position in this IronRDP
        // release, so the button lands where the last move left the
        // pointer.
        let events = self.pointer.button(mode, button, down, None);
        // A press is recorded whether or not it is queued, and a
        // release forgotten only once it is, so a release the queue
        // drops is sent again when the connection ends.
        if down {
            self.held.button(button, true);
            self.send_all(session, events);
        } else if self.send_all(session, events) {
            self.held.button(button, false);
        }
    }

    /// Add `units` of vertical wheel rotation (positive is away from
    /// the user, which is up) and send one wheel click per whole
    /// notch accumulated.
    fn wheel(&mut self, session: &SessionHandles, units: i32) {
        // A reversal starts a fresh notch, so a half notch one way
        // does not swallow half of the next notch the other way.
        if (units > 0 && self.wheel_remainder < 0) || (units < 0 && self.wheel_remainder > 0) {
            self.wheel_remainder = 0;
        }
        self.wheel_remainder = self.wheel_remainder.saturating_add(units);
        let mut clicks = self.wheel_remainder / WHEEL_UNITS_PER_NOTCH;
        debug!(
            "RDP input: wheel {} units, {} carried, {} clicks",
            units, self.wheel_remainder, clicks
        );
        if clicks == 0 {
            return;
        }
        if clicks.abs() > MAX_WHEEL_CLICKS_PER_EVENT {
            warn_once!(
                "rdp-input:wheel-flood",
                "RDP input: one wheel event asked for {} clicks; sending {} and dropping the rest",
                clicks,
                MAX_WHEEL_CLICKS_PER_EVENT
            );
            clicks = clicks.signum() * MAX_WHEEL_CLICKS_PER_EVENT;
            self.wheel_remainder = 0;
        }
        self.wheel_remainder -= clicks * WHEEL_UNITS_PER_NOTCH;
        let events = self.pointer.wheel(clicks);
        self.send_all(session, events);
    }

    /// A relative pointer move by `(dx, dy)`.
    fn rel_move(&mut self, session: &SessionHandles, mode: u32, dx: i32, dy: i32) {
        if !self.relative_seen {
            self.relative_seen = true;
            info!("RDP input: the client is sending relative mouse input");
        }
        if mode == MOUSE_MODE_SERVER {
            // Server mode consumes deltas, so the client's delta is
            // already the message. It bypasses the translator, whose
            // reference point is the client's absolute pointer and
            // means nothing to a relative stream. A zero delta would
            // take a slot in the motion ack window for nothing.
            if dx != 0 || dy != 0 {
                self.send(session, InputEvent::MouseMotion { dx, dy });
            }
            return;
        }

        // Client mode wants an absolute position, so integrate the
        // delta from the last position and keep it on the surface.
        // The size comes from the mirror, which the display handler
        // also locks; this handler cannot wait for it, so a busy
        // lock drops the move.
        let size = match session.surface_mirror.try_lock() {
            Ok(mirror) => mirror.primary_surface().map(|surface| surface.size()),
            Err(_) => {
                self.count_drop();
                debug!("RDP input: surface mirror busy; dropping a relative move");
                return;
            }
        };
        let Some((width, height)) = size else {
            self.count_drop();
            debug!("RDP input: no primary surface yet; dropping a relative move");
            return;
        };
        let (x, y) = self.pointer.last_pos().unwrap_or((width / 2, height / 2));
        let x = offset_within(x, dx, width);
        let y = offset_within(y, dy, height);
        if let Some(event) = self.pointer.motion(mode, x, y) {
            self.send(session, event);
        }
    }
}

/// `pos + delta`, clamped to `0..extent`.
fn offset_within(pos: u32, delta: i32, extent: u32) -> u32 {
    let max = i64::from(extent.saturating_sub(1));
    let moved = (i64::from(pos) + i64::from(delta)).clamp(0, max);
    // In 0..=u32::MAX by the clamp, since `max` came from a u32.
    u32::try_from(moved).unwrap_or(0)
}

/// The logical scancode for an RDP key: the 8-bit code, with the
/// `0xE0` prefix when RDP flags the key as extended. This is the form
/// [`make_scancode`] takes.
fn scancode_base(code: u8, extended: bool) -> u32 {
    let code = u32::from(code);
    if extended {
        0xE000 | code
    } else {
        code
    }
}

impl RdpServerInputHandler for SpiceInputHandler {
    fn keyboard(&mut self, event: KeyboardEvent) {
        let Some(session) = self.session() else {
            return;
        };
        match event {
            KeyboardEvent::Pressed { code, extended } => {
                let base = scancode_base(code, extended);
                self.held.key(make_scancode(base, true), true);
                self.send(&session, InputEvent::KeyDown(make_scancode(base, false)));
            }
            KeyboardEvent::Released { code, extended } => {
                // Forgotten only once queued, as for buttons.
                let scancode = make_scancode(scancode_base(code, extended), true);
                if self.send(&session, InputEvent::KeyUp(scancode)) {
                    self.held.key(scancode, false);
                }
            }
            KeyboardEvent::Synchronize(flags) => {
                let locks = u16::from(flags.bits()) & LOCK_KEYS;
                self.send(&session, InputEvent::KeyModifiers(locks));
            }
            KeyboardEvent::UnicodePressed(_) | KeyboardEvent::UnicodeReleased(_) => {
                // SPICE input is scancodes only, and turning a code
                // point into scancodes needs the guest's keyboard
                // layout, which the client does not tell us.
                warn_once!(
                    "rdp-input:unicode-key",
                    "RDP input: the client sent a Unicode key event, which SPICE cannot carry; dropping it"
                );
            }
        }
    }

    fn mouse(&mut self, event: MouseEvent) {
        let Some(session) = self.session() else {
            return;
        };
        // Copied out rather than matched on the borrow, which holds
        // the watch's read lock.
        let mode = *session.mouse_mode.borrow();
        match event {
            MouseEvent::Move { x, y } => {
                if let Some(event) = self.pointer.motion(mode, u32::from(x), u32::from(y)) {
                    self.send(&session, event);
                }
            }
            MouseEvent::LeftPressed => self.button(&session, mode, mouse_buttons::LEFT, true),
            MouseEvent::LeftReleased => self.button(&session, mode, mouse_buttons::LEFT, false),
            MouseEvent::RightPressed => self.button(&session, mode, mouse_buttons::RIGHT, true),
            MouseEvent::RightReleased => self.button(&session, mode, mouse_buttons::RIGHT, false),
            // IronRDP 0.13.0 produces these only from the relative
            // mouse PDU and FreeRDP's ainput channel; on the standard
            // mouse PDU a middle click arrives as a plain move. See
            // shakenfist/ryll#604.
            MouseEvent::MiddlePressed => self.button(&session, mode, mouse_buttons::MIDDLE, true),
            MouseEvent::MiddleReleased => self.button(&session, mode, mouse_buttons::MIDDLE, false),
            MouseEvent::Button4Pressed
            | MouseEvent::Button4Released
            | MouseEvent::Button5Pressed
            | MouseEvent::Button5Released => {
                warn_once!(
                    "rdp-input:side-buttons",
                    "RDP input: the client sent a side mouse button, which SPICE cannot carry; dropping it"
                );
            }
            MouseEvent::VerticalScroll { value } => self.wheel(&session, i32::from(value)),
            MouseEvent::Scroll { x, y } => {
                // From FreeRDP's Advanced Input channel, which carries
                // the wheel value the way WM_MOUSEWHEEL does, in the
                // high 16 bits: FreeRDP multiplies the rotation units
                // by 0x10000 before sending (client/common/client.c,
                // freerdp_client_send_wheel_event). One notch is 120
                // in the high word, so shift it down before counting.
                // Advanced Input is FreeRDP's own channel, with no
                // published specification of these units. FreeRDP's
                // encoding always leaves the low 16 bits clear, so a
                // value with any of them set came from a client
                // sending plain units, which the shift turns into
                // nothing (or one unit down, for a negative value);
                // that is logged once.
                if (y & 0xFFFF) != 0 {
                    warn_once!(
                        "rdp-input:ainput-wheel-units",
                        "RDP input: an Advanced Input wheel event carried {} with its low 16 bits set; \
                         this client may not encode wheel units as FreeRDP does",
                        y
                    );
                }
                if x >> 16 != 0 {
                    warn_once!(
                        "rdp-input:horizontal-wheel",
                        "RDP input: the client sent horizontal wheel movement, which SPICE cannot carry; \
                         dropping it"
                    );
                }
                self.wheel(&session, y >> 16);
            }
            MouseEvent::RelMove { x, y } => self.rel_move(&session, mode, x, y),
        }
        if let Some(pos) = self.pointer.last_pos() {
            self.held.moved(pos);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use ironrdp_pdu::input::fast_path::SynchronizeFlags;
    use shakenfist_spice_protocol::MOUSE_MODE_CLIENT;
    use shakenfist_spice_renderer::SurfaceMirror;
    use tokio::sync::{mpsc, watch};

    use crate::CursorState;

    const WIDTH: u32 = 640;
    const HEIGHT: u32 = 480;

    struct Harness {
        handler: SpiceInputHandler,
        input_rx: mpsc::Receiver<InputEvent>,
        mode_tx: watch::Sender<u32>,
        handles: Arc<SessionHandles>,
    }

    impl Harness {
        fn new(mode: u32) -> Self {
            Self::with_queue(mode, 64)
        }

        fn with_queue(mode: u32, capacity: usize) -> Self {
            let (input_tx, input_rx) = mpsc::channel(capacity);
            let (mode_tx, mouse_mode) = watch::channel(mode);
            let (_, cursor) = watch::channel(CursorState::default());
            let (resize_tx, _resize_rx) = mpsc::channel(1);
            let mirror = SurfaceMirror::with_test_surface(0, 0, WIDTH, HEIGHT, &[]);
            let handles = Arc::new(SessionHandles {
                surface_mirror: Arc::new(tokio::sync::Mutex::new(mirror)),
                input_tx,
                mouse_mode,
                resize_tx,
                cursor,
                clipboard: None,
                label: "test".to_string(),
            });
            let slot = SessionSlot::new();
            slot.set(Arc::clone(&handles));
            Self {
                handler: SpiceInputHandler::new(slot),
                input_rx,
                mode_tx,
                handles,
            }
        }

        fn drain(&mut self) -> Vec<InputEvent> {
            let mut events = Vec::new();
            while let Ok(event) = self.input_rx.try_recv() {
                events.push(event);
            }
            events
        }
    }

    fn assert_wheel_pairs(events: &[InputEvent], button: u32, pairs: usize) {
        assert_eq!(events.len(), pairs * 2, "{events:?}");
        for pair in events.chunks(2) {
            assert!(
                matches!(pair[0], InputEvent::MouseDown { button: b, .. } if b == button),
                "{pair:?}"
            );
            assert!(
                matches!(pair[1], InputEvent::MouseUp { button: b, .. } if b == button),
                "{pair:?}"
            );
        }
    }

    #[test]
    fn an_extended_key_is_sent_with_the_e0_prefix() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        // Up arrow: E0 48.
        h.handler.keyboard(KeyboardEvent::Pressed {
            code: 0x48,
            extended: true,
        });
        h.handler.keyboard(KeyboardEvent::Released {
            code: 0x48,
            extended: true,
        });
        let events = h.drain();
        assert_eq!(events.len(), 2);
        let down = make_scancode(0xE048, false);
        let up = make_scancode(0xE048, true);
        assert_eq!(down, 0x48E0);
        assert_eq!(up, 0xC8E0);
        assert!(matches!(events[0], InputEvent::KeyDown(sc) if sc == down));
        assert!(matches!(events[1], InputEvent::KeyUp(sc) if sc == up));
    }

    #[test]
    fn a_plain_key_is_sent_as_its_code() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.keyboard(KeyboardEvent::Pressed {
            code: 0x13,
            extended: false,
        });
        h.handler.keyboard(KeyboardEvent::Released {
            code: 0x13,
            extended: false,
        });
        let events = h.drain();
        assert_eq!(events.len(), 2);
        assert!(matches!(events[0], InputEvent::KeyDown(0x13)));
        assert!(matches!(events[1], InputEvent::KeyUp(0x93)));
    }

    #[test]
    fn synchronize_sets_the_lock_keys() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.keyboard(KeyboardEvent::Synchronize(
            SynchronizeFlags::CAPS_LOCK | SynchronizeFlags::NUM_LOCK,
        ));
        let events = h.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], InputEvent::KeyModifiers(0x6)));
    }

    #[test]
    fn synchronize_drops_kana_lock() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.keyboard(KeyboardEvent::Synchronize(
            SynchronizeFlags::KANA_LOCK | SynchronizeFlags::SCROLL_LOCK,
        ));
        let events = h.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], InputEvent::KeyModifiers(0x1)));
    }

    #[test]
    fn client_mode_move_and_left_press_are_absolute() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.mouse(MouseEvent::Move { x: 100, y: 200 });
        h.handler.mouse(MouseEvent::LeftPressed);
        h.handler.mouse(MouseEvent::LeftReleased);
        let events = h.drain();
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(
            events[0],
            InputEvent::MouseMove { x: 100, y: 200 }
        ));
        assert!(matches!(
            events[1],
            InputEvent::MouseDown {
                button: mouse_buttons::LEFT,
                x: 100,
                y: 200
            }
        ));
        assert!(matches!(
            events[2],
            InputEvent::MouseUp {
                button: mouse_buttons::LEFT,
                x: 100,
                y: 200
            }
        ));
    }

    #[test]
    fn right_and_middle_buttons_map_to_their_spice_masks() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.mouse(MouseEvent::Move { x: 5, y: 6 });
        h.handler.mouse(MouseEvent::RightPressed);
        h.handler.mouse(MouseEvent::MiddlePressed);
        h.handler.mouse(MouseEvent::MiddleReleased);
        let events = h.drain();
        assert_eq!(events.len(), 4, "{events:?}");
        assert!(matches!(
            events[1],
            InputEvent::MouseDown {
                button: mouse_buttons::RIGHT,
                x: 5,
                y: 6
            }
        ));
        assert!(matches!(
            events[2],
            InputEvent::MouseDown {
                button: mouse_buttons::MIDDLE,
                x: 5,
                y: 6
            }
        ));
        assert!(matches!(
            events[3],
            InputEvent::MouseUp {
                button: mouse_buttons::MIDDLE,
                x: 5,
                y: 6
            }
        ));
    }

    #[test]
    fn server_mode_move_after_move_is_a_delta() {
        let mut h = Harness::new(MOUSE_MODE_SERVER);
        h.handler.mouse(MouseEvent::Move { x: 100, y: 100 });
        h.handler.mouse(MouseEvent::Move { x: 130, y: 90 });
        let events = h.drain();
        // The first move only sets the reference point.
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(
            events[0],
            InputEvent::MouseMotion { dx: 30, dy: -10 }
        ));
    }

    #[test]
    fn a_mode_change_takes_effect_on_the_next_event() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.mouse(MouseEvent::Move { x: 100, y: 100 });
        h.mode_tx.send_replace(MOUSE_MODE_SERVER);
        h.handler.mouse(MouseEvent::Move { x: 110, y: 100 });
        let events = h.drain();
        assert_eq!(events.len(), 2, "{events:?}");
        assert!(matches!(
            events[0],
            InputEvent::MouseMove { x: 100, y: 100 }
        ));
        assert!(matches!(
            events[1],
            InputEvent::MouseMotion { dx: 10, dy: 0 }
        ));
    }

    #[test]
    fn one_notch_up_is_one_button_four_pair() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.mouse(MouseEvent::VerticalScroll { value: 120 });
        assert_wheel_pairs(&h.drain(), mouse_buttons::UP, 1);
    }

    #[test]
    fn two_notches_down_are_two_button_five_pairs() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.mouse(MouseEvent::VerticalScroll { value: -240 });
        assert_wheel_pairs(&h.drain(), mouse_buttons::DOWN, 2);
    }

    #[test]
    fn half_notches_accumulate_into_one_pair() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.mouse(MouseEvent::VerticalScroll { value: 60 });
        assert!(h.drain().is_empty());
        h.handler.mouse(MouseEvent::VerticalScroll { value: 60 });
        assert_wheel_pairs(&h.drain(), mouse_buttons::UP, 1);
        // Nothing is left over to leak into the next notch.
        h.handler.mouse(MouseEvent::VerticalScroll { value: 60 });
        assert!(h.drain().is_empty());
    }

    #[test]
    fn a_wheel_reversal_starts_a_fresh_notch() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.mouse(MouseEvent::VerticalScroll { value: 60 });
        h.handler.mouse(MouseEvent::VerticalScroll { value: -120 });
        assert_wheel_pairs(&h.drain(), mouse_buttons::DOWN, 1);
    }

    #[test]
    fn scroll_uses_the_vertical_units_and_drops_the_horizontal() {
        // Advanced Input scroll values carry the units in the high
        // 16 bits, as FreeRDP sends them.
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.mouse(MouseEvent::Scroll {
            x: 120 << 16,
            y: -120 << 16,
        });
        assert_wheel_pairs(&h.drain(), mouse_buttons::DOWN, 1);
        h.handler.mouse(MouseEvent::Scroll { x: 240 << 16, y: 0 });
        assert!(h.drain().is_empty());
        h.handler.mouse(MouseEvent::Scroll { x: 0, y: 240 << 16 });
        assert_wheel_pairs(&h.drain(), mouse_buttons::UP, 2);
    }

    #[test]
    fn a_runaway_wheel_event_is_capped() {
        // A value in the wrong units would otherwise become thousands
        // of clicks; the handler sends a few and drops the rest, with
        // nothing carried into the next event.
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.mouse(MouseEvent::Scroll {
            x: 0,
            y: 120 << 16 << 6,
        });
        assert_wheel_pairs(
            &h.drain(),
            mouse_buttons::UP,
            MAX_WHEEL_CLICKS_PER_EVENT as usize,
        );
        h.handler.mouse(MouseEvent::VerticalScroll { value: 60 });
        assert!(h.drain().is_empty());
    }

    #[test]
    fn server_mode_relative_move_is_sent_unchanged() {
        let mut h = Harness::new(MOUSE_MODE_SERVER);
        h.handler.mouse(MouseEvent::RelMove { x: -7, y: 12 });
        h.handler.mouse(MouseEvent::RelMove { x: 0, y: 0 });
        let events = h.drain();
        assert_eq!(events.len(), 1, "{events:?}");
        assert!(matches!(
            events[0],
            InputEvent::MouseMotion { dx: -7, dy: 12 }
        ));
    }

    #[test]
    fn client_mode_relative_move_starts_at_the_centre() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.mouse(MouseEvent::RelMove { x: 10, y: -20 });
        let events = h.drain();
        assert_eq!(events.len(), 1, "{events:?}");
        // WIDTH / 2 + 10, HEIGHT / 2 - 20.
        assert!(matches!(
            events[0],
            InputEvent::MouseMove { x: 330, y: 220 }
        ));
    }

    #[test]
    fn client_mode_relative_move_is_clamped_to_the_surface() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.mouse(MouseEvent::Move { x: 600, y: 10 });
        h.handler.mouse(MouseEvent::RelMove { x: 100, y: -50 });
        h.handler.mouse(MouseEvent::RelMove { x: -5000, y: 5000 });
        let events = h.drain();
        assert_eq!(events.len(), 3, "{events:?}");
        assert!(matches!(events[1], InputEvent::MouseMove { x: 639, y: 0 }));
        assert!(matches!(events[2], InputEvent::MouseMove { x: 0, y: 479 }));
    }

    #[test]
    fn client_mode_relative_move_is_dropped_while_the_mirror_is_busy() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        let mirror = Arc::clone(&h.handles.surface_mirror);
        let _guard = mirror.try_lock().expect("the mirror is free");
        h.handler.mouse(MouseEvent::RelMove { x: 1, y: 1 });
        assert!(h.drain().is_empty());
        assert_eq!(h.handler.dropped(), 1);
    }

    #[test]
    fn unicode_keys_and_side_buttons_send_nothing() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.handler.keyboard(KeyboardEvent::UnicodePressed(0x41));
        h.handler.keyboard(KeyboardEvent::UnicodeReleased(0x41));
        h.handler.mouse(MouseEvent::Button4Pressed);
        h.handler.mouse(MouseEvent::Button5Released);
        assert!(h.drain().is_empty());
        // Unsupported, not undeliverable, so not counted as dropped.
        assert_eq!(h.handler.dropped(), 0);
    }

    #[test]
    fn a_full_queue_drops_and_counts() {
        let mut h = Harness::with_queue(MOUSE_MODE_CLIENT, 1);
        let counter = h.handler.dropped_counter();
        for _ in 0..3 {
            h.handler.keyboard(KeyboardEvent::Pressed {
                code: 0x1E,
                extended: false,
            });
        }
        let events = h.drain();
        assert_eq!(events.len(), 1);
        assert!(matches!(events[0], InputEvent::KeyDown(0x1E)));
        assert_eq!(h.handler.dropped(), 2);
        assert_eq!(counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn a_closed_queue_drops_and_counts() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        h.input_rx.close();
        h.handler.keyboard(KeyboardEvent::Pressed {
            code: 0x1E,
            extended: false,
        });
        assert_eq!(h.handler.dropped(), 1);
    }

    #[test]
    fn with_no_session_events_are_dropped_and_counted() {
        let mut handler = SpiceInputHandler::new(SessionSlot::new());
        handler.keyboard(KeyboardEvent::Pressed {
            code: 0x1E,
            extended: false,
        });
        handler.mouse(MouseEvent::Move { x: 1, y: 1 });
        assert_eq!(handler.dropped(), 2);
    }

    #[test]
    fn a_new_session_starts_from_a_clean_pointer() {
        let mut h = Harness::new(MOUSE_MODE_SERVER);
        h.handler.mouse(MouseEvent::Move { x: 100, y: 100 });
        h.handler.mouse(MouseEvent::VerticalScroll { value: 60 });

        // The slot is shared with the handler; swap in a second
        // session on a fresh queue.
        let (input_tx, mut input_rx) = mpsc::channel(8);
        let (_mode_tx, mouse_mode) = watch::channel(MOUSE_MODE_SERVER);
        let (_, cursor) = watch::channel(CursorState::default());
        let (resize_tx, _resize_rx) = mpsc::channel(1);
        h.handler.slot.set(Arc::new(SessionHandles {
            surface_mirror: Arc::clone(&h.handles.surface_mirror),
            input_tx,
            mouse_mode,
            resize_tx,
            cursor,
            clipboard: None,
            label: "second".to_string(),
        }));

        // Without a reset this would be a delta from (100, 100), and
        // the half notch would complete a wheel click.
        h.handler.mouse(MouseEvent::Move { x: 300, y: 300 });
        h.handler.mouse(MouseEvent::VerticalScroll { value: 60 });
        assert!(input_rx.try_recv().is_err());
    }

    #[test]
    fn the_next_client_of_the_same_session_starts_from_a_clean_pointer() {
        let mut h = Harness::new(MOUSE_MODE_SERVER);
        h.handler.mouse(MouseEvent::Move { x: 100, y: 100 });
        h.handler.mouse(MouseEvent::VerticalScroll { value: 60 });
        h.drain();

        // The same session stays in the slot; only the connection
        // ends, as when one client leaves and another logs in.
        h.handler.held().end_connection();

        // Without a reset this would be a delta from (100, 100), and
        // the half notch would complete a wheel click.
        h.handler.mouse(MouseEvent::Move { x: 300, y: 300 });
        h.handler.mouse(MouseEvent::VerticalScroll { value: 60 });
        let events = h.drain();
        assert!(events.is_empty(), "{events:?}");
    }

    #[test]
    fn ending_a_connection_releases_what_is_still_held() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        let held = h.handler.held();
        // A, then Up arrow (E0 48), then A released: only Up arrow is
        // still held.
        h.handler.keyboard(KeyboardEvent::Pressed {
            code: 0x1E,
            extended: false,
        });
        h.handler.keyboard(KeyboardEvent::Pressed {
            code: 0x48,
            extended: true,
        });
        h.handler.keyboard(KeyboardEvent::Released {
            code: 0x1E,
            extended: false,
        });
        // Right clicked and let go, left pressed, then dragged.
        h.handler.mouse(MouseEvent::Move { x: 10, y: 20 });
        h.handler.mouse(MouseEvent::RightPressed);
        h.handler.mouse(MouseEvent::RightReleased);
        h.handler.mouse(MouseEvent::LeftPressed);
        h.handler.mouse(MouseEvent::Move { x: 100, y: 50 });
        h.drain();

        let up_arrow = make_scancode(0xE048, true);
        let releases = held.end_connection();
        assert_eq!(releases.len(), 2, "{releases:?}");
        assert!(
            matches!(releases[0], InputEvent::KeyUp(sc) if sc == up_arrow),
            "{releases:?}"
        );
        assert!(
            matches!(
                releases[1],
                InputEvent::MouseUp {
                    button: mouse_buttons::LEFT,
                    x: 100,
                    y: 50,
                }
            ),
            "{releases:?}"
        );
        // Released once: a second end has nothing left to release.
        let again = held.end_connection();
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn a_release_the_full_queue_drops_is_sent_when_the_connection_ends() {
        // Room for one event: the first key-down fills the queue, and
        // the autorepeat, the button and both releases are dropped.
        let mut h = Harness::with_queue(MOUSE_MODE_CLIENT, 1);
        let held = h.handler.held();
        for _ in 0..2 {
            h.handler.keyboard(KeyboardEvent::Pressed {
                code: 0x1E,
                extended: false,
            });
        }
        h.handler.mouse(MouseEvent::LeftPressed);
        h.handler.keyboard(KeyboardEvent::Released {
            code: 0x1E,
            extended: false,
        });
        h.handler.mouse(MouseEvent::LeftReleased);
        assert_eq!(h.handler.dropped(), 4);
        h.drain();

        // The guest saw A go down and nothing else, so A is released
        // once despite the autorepeat, and so is the button.
        let releases = held.end_connection();
        assert!(
            matches!(
                releases[..],
                [
                    InputEvent::KeyUp(0x9E),
                    InputEvent::MouseUp {
                        button: mouse_buttons::LEFT,
                        ..
                    }
                ]
            ),
            "{releases:?}"
        );
    }

    #[test]
    fn a_button_the_next_client_never_moved_is_released_at_the_origin() {
        let mut h = Harness::new(MOUSE_MODE_CLIENT);
        let held = h.handler.held();
        h.handler.mouse(MouseEvent::Move { x: 100, y: 50 });
        held.end_connection();

        // The next client presses without moving first, then leaves:
        // the last client's position is not where it is released.
        h.handler.mouse(MouseEvent::LeftPressed);
        h.drain();
        let releases = held.end_connection();
        assert!(
            matches!(
                releases[..],
                [InputEvent::MouseUp {
                    button: mouse_buttons::LEFT,
                    x: 0,
                    y: 0,
                }]
            ),
            "{releases:?}"
        );
    }
}
