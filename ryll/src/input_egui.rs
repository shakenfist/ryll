/// egui-to-substrate adapter for keyboard and mouse input.
///
/// This module is the only place in the ryll binary that converts
/// egui input types to the substrate's neutral `LogicalKey` /
/// SPICE-button representations.  A future web-frontend adapter
/// will provide the same conversions for `KeyboardEvent.code` /
/// browser mouse-button values without touching this file.
use std::collections::{HashMap, HashSet};

use eframe::egui;

use shakenfist_spice_renderer::channels::inputs::{
    scancode_for_logical_key, Direction, LogicalKey, NavKey, PunctKey, WSKey,
};
use shakenfist_spice_renderer::channels::InputEvent;

/// Convert an egui key event to the substrate-neutral `LogicalKey`.
///
/// Returns `None` for any key that has no scancode mapping (e.g.
/// `egui::Key::F13`, modifier keys reported via this path, etc.).
pub fn egui_key_to_logical(key: egui::Key) -> Option<LogicalKey> {
    match key {
        // Letters
        egui::Key::A => Some(LogicalKey::Letter('A')),
        egui::Key::B => Some(LogicalKey::Letter('B')),
        egui::Key::C => Some(LogicalKey::Letter('C')),
        egui::Key::D => Some(LogicalKey::Letter('D')),
        egui::Key::E => Some(LogicalKey::Letter('E')),
        egui::Key::F => Some(LogicalKey::Letter('F')),
        egui::Key::G => Some(LogicalKey::Letter('G')),
        egui::Key::H => Some(LogicalKey::Letter('H')),
        egui::Key::I => Some(LogicalKey::Letter('I')),
        egui::Key::J => Some(LogicalKey::Letter('J')),
        egui::Key::K => Some(LogicalKey::Letter('K')),
        egui::Key::L => Some(LogicalKey::Letter('L')),
        egui::Key::M => Some(LogicalKey::Letter('M')),
        egui::Key::N => Some(LogicalKey::Letter('N')),
        egui::Key::O => Some(LogicalKey::Letter('O')),
        egui::Key::P => Some(LogicalKey::Letter('P')),
        egui::Key::Q => Some(LogicalKey::Letter('Q')),
        egui::Key::R => Some(LogicalKey::Letter('R')),
        egui::Key::S => Some(LogicalKey::Letter('S')),
        egui::Key::T => Some(LogicalKey::Letter('T')),
        egui::Key::U => Some(LogicalKey::Letter('U')),
        egui::Key::V => Some(LogicalKey::Letter('V')),
        egui::Key::W => Some(LogicalKey::Letter('W')),
        egui::Key::X => Some(LogicalKey::Letter('X')),
        egui::Key::Y => Some(LogicalKey::Letter('Y')),
        egui::Key::Z => Some(LogicalKey::Letter('Z')),

        // Digits
        egui::Key::Num0 => Some(LogicalKey::Digit(0)),
        egui::Key::Num1 => Some(LogicalKey::Digit(1)),
        egui::Key::Num2 => Some(LogicalKey::Digit(2)),
        egui::Key::Num3 => Some(LogicalKey::Digit(3)),
        egui::Key::Num4 => Some(LogicalKey::Digit(4)),
        egui::Key::Num5 => Some(LogicalKey::Digit(5)),
        egui::Key::Num6 => Some(LogicalKey::Digit(6)),
        egui::Key::Num7 => Some(LogicalKey::Digit(7)),
        egui::Key::Num8 => Some(LogicalKey::Digit(8)),
        egui::Key::Num9 => Some(LogicalKey::Digit(9)),

        // Function keys
        egui::Key::F1 => Some(LogicalKey::Function(1)),
        egui::Key::F2 => Some(LogicalKey::Function(2)),
        egui::Key::F3 => Some(LogicalKey::Function(3)),
        egui::Key::F4 => Some(LogicalKey::Function(4)),
        egui::Key::F5 => Some(LogicalKey::Function(5)),
        egui::Key::F6 => Some(LogicalKey::Function(6)),
        egui::Key::F7 => Some(LogicalKey::Function(7)),
        egui::Key::F8 => Some(LogicalKey::Function(8)),
        egui::Key::F9 => Some(LogicalKey::Function(9)),
        egui::Key::F10 => Some(LogicalKey::Function(10)),
        egui::Key::F11 => Some(LogicalKey::Function(11)),
        egui::Key::F12 => Some(LogicalKey::Function(12)),

        // Whitespace-adjacent
        egui::Key::Space => Some(LogicalKey::Whitespace(WSKey::Space)),
        egui::Key::Enter => Some(LogicalKey::Whitespace(WSKey::Enter)),
        egui::Key::Backspace => Some(LogicalKey::Whitespace(WSKey::Backspace)),
        egui::Key::Tab => Some(LogicalKey::Whitespace(WSKey::Tab)),

        // Escape
        egui::Key::Escape => Some(LogicalKey::Escape),

        // Navigation cluster
        egui::Key::Delete => Some(LogicalKey::Navigation(NavKey::Delete)),
        egui::Key::Insert => Some(LogicalKey::Navigation(NavKey::Insert)),
        egui::Key::Home => Some(LogicalKey::Navigation(NavKey::Home)),
        egui::Key::End => Some(LogicalKey::Navigation(NavKey::End)),
        egui::Key::PageUp => Some(LogicalKey::Navigation(NavKey::PageUp)),
        egui::Key::PageDown => Some(LogicalKey::Navigation(NavKey::PageDown)),

        // Arrow keys
        egui::Key::ArrowUp => Some(LogicalKey::Arrow(Direction::Up)),
        egui::Key::ArrowDown => Some(LogicalKey::Arrow(Direction::Down)),
        egui::Key::ArrowLeft => Some(LogicalKey::Arrow(Direction::Left)),
        egui::Key::ArrowRight => Some(LogicalKey::Arrow(Direction::Right)),

        // Punctuation
        egui::Key::Minus => Some(LogicalKey::Punctuation(PunctKey::Minus)),
        egui::Key::Equals => Some(LogicalKey::Punctuation(PunctKey::Equals)),
        egui::Key::OpenBracket => Some(LogicalKey::Punctuation(PunctKey::OpenBracket)),
        egui::Key::CloseBracket => Some(LogicalKey::Punctuation(PunctKey::CloseBracket)),
        egui::Key::Backslash => Some(LogicalKey::Punctuation(PunctKey::Backslash)),
        egui::Key::Semicolon => Some(LogicalKey::Punctuation(PunctKey::Semicolon)),
        egui::Key::Quote => Some(LogicalKey::Punctuation(PunctKey::Quote)),
        egui::Key::Backtick => Some(LogicalKey::Punctuation(PunctKey::Backtick)),
        egui::Key::Comma => Some(LogicalKey::Punctuation(PunctKey::Comma)),
        egui::Key::Period => Some(LogicalKey::Punctuation(PunctKey::Period)),
        egui::Key::Slash => Some(LogicalKey::Punctuation(PunctKey::Slash)),

        // All other egui keys have no scancode mapping.
        _ => None,
    }
}

/// ryll's own single-key shortcuts.
///
/// This is the one list of keys ryll keeps for itself. `RyllApp::ui`
/// reads it to decide which key opens what, and `translate_key_events`
/// reads it to keep those keys from the guest, so a new shortcut added
/// here is withheld from the guest without anyone having to remember a
/// second list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostShortcut {
    /// Save the current display as PNG(s).
    Screenshot,
    /// Toggle the live traffic viewer side panel.
    TrafficViewer,
    /// Open or close the bug report dialog.
    BugReport,
}

impl HostShortcut {
    pub const ALL: [HostShortcut; 3] = [
        HostShortcut::Screenshot,
        HostShortcut::TrafficViewer,
        HostShortcut::BugReport,
    ];

    /// The key that triggers this shortcut.
    pub const fn key(self) -> egui::Key {
        match self {
            HostShortcut::Screenshot => egui::Key::F8,
            HostShortcut::TrafficViewer => egui::Key::F11,
            HostShortcut::BugReport => egui::Key::F12,
        }
    }

    /// Whether `key` belongs to one of ryll's shortcuts.
    pub fn is_shortcut_key(key: egui::Key) -> bool {
        Self::ALL.iter().any(|shortcut| shortcut.key() == key)
    }
}

/// A clipboard chord whose press egui-winit reports as a clipboard event
/// instead of as a key press.
///
/// egui-winit (`State::on_keyboard_input`) turns the press into
/// `Event::Cut`, `Event::Copy` or `Event::Paste` and emits no
/// `Event::Key` for it, but the release still arrives as an ordinary
/// key event. The chords are the command modifier (Ctrl, or Cmd on
/// macOS) with X, C or V; on Windows also Shift+Delete, Ctrl+Insert and
/// Shift+Insert; and the dedicated Cut / Copy / Paste keys. For a paste
/// it emits `Event::Paste` only when the host clipboard holds text, and
/// otherwise nothing at all.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum ClipboardChord {
    Cut,
    Copy,
    Paste,
}

impl ClipboardChord {
    const ALL: [ClipboardChord; 3] = [
        ClipboardChord::Cut,
        ClipboardChord::Copy,
        ClipboardChord::Paste,
    ];

    /// The key whose press egui-winit replaced, judged from the
    /// modifiers held when it did, or `None` for a dedicated Cut / Copy
    /// / Paste key (which has no scancode).
    ///
    /// The modifiers cannot tell two of the Windows variants apart:
    /// Ctrl+Insert arrives exactly like Ctrl+C, and Ctrl+Shift+Delete
    /// like Ctrl+Shift+X. The key is needed when the press happens, a
    /// release later, so those two are sent as C and X.
    fn replaced_key(self, modifiers: egui::Modifiers) -> Option<egui::Key> {
        match (self, modifiers.command, modifiers.shift) {
            (ClipboardChord::Cut, true, _) => Some(egui::Key::X),
            (ClipboardChord::Copy, true, _) => Some(egui::Key::C),
            (ClipboardChord::Paste, true, _) => Some(egui::Key::V),
            (ClipboardChord::Cut, false, true) => Some(egui::Key::Delete),
            (ClipboardChord::Paste, false, true) => Some(egui::Key::Insert),
            _ => None,
        }
    }

    /// Whether a release of `key` (the logical key, which is what
    /// egui-winit matched the chord on) can end a press reported as
    /// this chord.
    fn released_by(self, key: egui::Key) -> bool {
        match self {
            ClipboardChord::Cut => matches!(key, egui::Key::X | egui::Key::Delete | egui::Key::Cut),
            ClipboardChord::Copy => {
                matches!(key, egui::Key::C | egui::Key::Insert | egui::Key::Copy)
            }
            ClipboardChord::Paste => {
                matches!(key, egui::Key::V | egui::Key::Insert | egui::Key::Paste)
            }
        }
    }

    /// Whether egui-winit treats a press of `key` with `modifiers` as a
    /// paste chord that should reach the guest (so not a macOS Cmd
    /// chord). `Event::Paste` is only emitted for one when the host
    /// clipboard holds text.
    fn is_guest_paste(key: egui::Key, modifiers: egui::Modifiers) -> bool {
        !modifiers.mac_cmd
            && ((modifiers.command && key == egui::Key::V)
                || (modifiers.shift && !modifiers.ctrl && key == egui::Key::Insert))
    }
}

/// The keys ryll has forwarded to the guest as pressed, keyed by the
/// same key used for the scancode lookup.
///
/// egui's own `repeat` flag cannot be trusted for this. egui decides a
/// press is a repeat by looking its *logical* key up in
/// `InputState::keys_down`, and a key's logical identity can change
/// between press and release: Shift+; arrives as `Key::Colon` but, if
/// Shift is let go first, leaves as `Key::Semicolon`. `Colon` is then
/// never removed, and every later `:` is flagged as a repeat (and was
/// dropped) until the window lost focus. Tracking presses here, by the
/// physical key, keeps each press paired with its release.
///
/// It also remembers presses that were deliberately *not* forwarded
/// because they were part of a host Cmd chord, so that their releases
/// are withheld too and the guest never sees half of a key stroke, and
/// presses egui-winit reported as clipboard events, so their releases
/// can be paired up.
#[derive(Debug, Default)]
pub struct HeldKeys {
    /// Held key -> the wire scancode that releases it.
    held: HashMap<egui::Key, u32>,
    /// Keys whose press was withheld from the guest as part of a host
    /// Cmd chord, so their release must be withheld as well.
    suppressed: HashSet<egui::Key>,
    /// Clipboard chords whose press egui-winit reported as a clipboard
    /// event and whose release has not arrived yet. The value is the
    /// key pressed in the guest in its place (also in `held`), or
    /// `None` if the press was withheld.
    clipboard: HashMap<ClipboardChord, Option<egui::Key>>,
    /// The modifiers as of the latest event, because egui attaches none
    /// to clipboard events. egui-winit decides a press is a clipboard
    /// chord from its own copy of the modifiers, which it reports in
    /// `Event::ModifiersChanged` whenever it changes, except on focus
    /// loss; this follows those events, and `end_frame` keeps it
    /// current across frames whose events are never translated.
    modifiers: egui::Modifiers,
}

impl HeldKeys {
    /// Release every held key, returning the key-up events to send so
    /// the guest does not see a key stuck down while ryll is not
    /// forwarding input to it.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        // A withheld press whose release goes elsewhere must not
        // swallow the next, unrelated release of the same key.
        self.suppressed.clear();
        self.clipboard.clear();
        self.held
            .drain()
            .map(|(_, up_code)| InputEvent::KeyUp(up_code))
            .collect()
    }

    /// Record the modifiers a frame ended with (egui's `i.modifiers`).
    ///
    /// Call this once every frame, including the frames whose events
    /// are not passed to `translate_key_events` (a dialog is open, the
    /// session is not connected yet, ryll consumed the frame as its own
    /// paste shortcut). A modifier change in one of those frames would
    /// otherwise be missed, and a later Ctrl+C judged as if Ctrl were
    /// not held, which drops the press.
    pub fn end_frame(&mut self, modifiers: egui::Modifiers) {
        self.modifiers = modifiers;
    }

    /// Withhold the release of the paste chord ryll has just consumed as
    /// its own Ctrl+Alt+V shortcut. ryll does not forward that frame's
    /// input, so without this the V release would reach the guest on
    /// its own, or, with no `Event::Paste` recorded for it, be taken
    /// for a paste chord the guest never saw pressed.
    pub fn withhold_paste_release(&mut self) {
        self.clipboard.insert(ClipboardChord::Paste, None);
    }

    /// Handle a press egui-winit reported as a clipboard event.
    ///
    /// Off macOS the command modifier is Ctrl, so this is Ctrl+C, Ctrl+X
    /// or Ctrl+V meant for the guest (Ctrl has already been forwarded),
    /// and the key press it replaced is sent in its place. On macOS it
    /// is a Cmd chord, which belongs to the host and is withheld like
    /// any other (see `translate_key_events`).
    fn press_clipboard_chord(&mut self, chord: ClipboardChord, out: &mut Vec<InputEvent>) {
        if self.modifiers.mac_cmd {
            self.clipboard.insert(chord, None);
            return;
        }
        let Some(key) = chord.replaced_key(self.modifiers) else {
            return;
        };
        let Some((down_code, up_code)) =
            egui_key_to_logical(key).and_then(scancode_for_logical_key)
        else {
            return;
        };
        if self.held.contains_key(&key) {
            // Auto-repeat: egui-winit reports every repeat of the chord
            // as another clipboard event.
            return;
        }
        self.suppressed.remove(&key);
        self.held.insert(key, up_code);
        self.clipboard.insert(chord, Some(key));
        out.push(InputEvent::KeyDown(down_code));
    }

    /// Take the outstanding clipboard chord, if any, that a release of
    /// logical `key` ends.
    fn take_clipboard_chord(&mut self, key: egui::Key) -> Option<Option<egui::Key>> {
        let chord = ClipboardChord::ALL
            .into_iter()
            .find(|chord| chord.released_by(key) && self.clipboard.contains_key(chord))?;
        self.clipboard.remove(&chord)
    }
}

/// Translate one frame's egui events into the key events to forward to
/// the guest.
///
/// A press of a key that is already held is an auto-repeat and is not
/// forwarded. Releases are always forwarded, even for a key not
/// recorded as held. Losing window focus releases every held key,
/// because the matching key-ups will be delivered to another window.
/// Keys in [`HostShortcut`] are ryll's own and are never forwarded.
///
/// On macOS a key pressed while Cmd is held is a host shortcut (Cmd+Q,
/// Cmd+W, Cmd+Tab and so on). egui reports no key event for Cmd
/// itself, so forwarding the rest of the chord would type a bare
/// letter in the guest; those presses are withheld, and so are their
/// releases. A key that was already held when Cmd went down is still
/// released normally. `mac_cmd` is only ever set on macOS, so this
/// costs nothing elsewhere.
///
/// egui-winit reports the press of Ctrl+C, Ctrl+X and Ctrl+V (Cmd on
/// macOS) as `Event::Copy` / `Cut` / `Paste` rather than as a key press
/// (see [`ClipboardChord`]). Off macOS those are turned back into the
/// key press they replaced, so the guest gets the whole chord; the
/// pasted text is the host clipboard and is never typed. The press is
/// sent as the key in the US-QWERTY position, so on a layout that moves
/// C, X or V (Dvorak, say) the guest sees the key at that position; the
/// release is still paired with it. When a paste chord's press left
/// no event at all because the host clipboard had no text, the press is
/// sent together with the release; if Ctrl was let go before V that
/// release looks like any stray one, so only the V release is sent and
/// the stroke is lost. ryll's own Ctrl+Alt+V is never sent this way.
pub fn translate_key_events(events: &[egui::Event], held: &mut HeldKeys) -> Vec<InputEvent> {
    let mut out = Vec::new();
    for event in events {
        match event {
            egui::Event::WindowFocused(false) => {
                out.extend(held.release_all());
                // egui-winit forgets its modifiers on focus loss without
                // emitting `ModifiersChanged`; do the same.
                held.modifiers = egui::Modifiers::default();
            }
            egui::Event::ModifiersChanged(modifiers) => held.modifiers = *modifiers,
            egui::Event::Cut => held.press_clipboard_chord(ClipboardChord::Cut, &mut out),
            egui::Event::Copy => held.press_clipboard_chord(ClipboardChord::Copy, &mut out),
            egui::Event::Paste(_) => held.press_clipboard_chord(ClipboardChord::Paste, &mut out),
            egui::Event::Key {
                key,
                physical_key,
                pressed,
                modifiers,
                ..
            } => {
                held.modifiers = *modifiers;
                let lookup_key = physical_key.unwrap_or(*key);
                if HostShortcut::is_shortcut_key(lookup_key) {
                    continue;
                }
                if !*pressed {
                    if let Some(up_code) = held.held.remove(&lookup_key) {
                        held.clipboard.retain(|_, sent| *sent != Some(lookup_key));
                        out.push(InputEvent::KeyUp(up_code));
                        continue;
                    }
                    if let Some(sent) = held.take_clipboard_chord(*key) {
                        // The release of a clipboard chord whose press
                        // was sent as a different physical key, or
                        // withheld. Release whatever was sent.
                        if let Some(up_code) = sent.and_then(|k| held.held.remove(&k)) {
                            out.push(InputEvent::KeyUp(up_code));
                        }
                        continue;
                    }
                }
                let Some((down_code, up_code)) =
                    egui_key_to_logical(lookup_key).and_then(scancode_for_logical_key)
                else {
                    continue;
                };
                if *pressed {
                    if held.held.contains_key(&lookup_key) {
                        // Auto-repeat of a key the guest already holds.
                        continue;
                    }
                    if modifiers.mac_cmd {
                        held.suppressed.insert(lookup_key);
                        continue;
                    }
                    held.suppressed.remove(&lookup_key);
                    held.held.insert(lookup_key, up_code);
                    out.push(InputEvent::KeyDown(down_code));
                } else if held.suppressed.remove(&lookup_key)
                    || modifiers.mac_cmd
                    || (is_paste_shortcut_modifiers(*modifiers) && *key == egui::Key::V)
                {
                    // The release of a withheld Cmd chord press. The
                    // other two arms cover a Cmd+V, or ryll's own
                    // Ctrl+Alt+V, whose press left no event at all
                    // because the host clipboard held no text.
                } else {
                    if ClipboardChord::is_guest_paste(*key, *modifiers) {
                        // A paste chord whose press egui-winit swallowed
                        // without an `Event::Paste`, because the host
                        // clipboard held no text. The guest's clipboard
                        // may well hold some, so send the whole stroke.
                        out.push(InputEvent::KeyDown(down_code));
                    }
                    out.push(InputEvent::KeyUp(up_code));
                }
            }
            _ => {}
        }
    }
    out
}

/// Whether this frame's input holds ryll's paste-as-keystrokes chord,
/// Ctrl+Alt+V.
///
/// Off macOS Ctrl is egui's command modifier, so egui-winit reports the
/// V press of Ctrl+Alt+V as `Event::Paste` rather than as a key press,
/// and looking for the key press alone never sees it. (When the host
/// clipboard holds no text it reports nothing, but then there is
/// nothing to paste either, and `translate_key_events` withholds the V
/// release from the guest.)
pub fn paste_shortcut_pressed(modifiers: egui::Modifiers, events: &[egui::Event]) -> bool {
    is_paste_shortcut_modifiers(modifiers)
        && events.iter().any(|event| {
            matches!(
                event,
                egui::Event::Paste(_)
                    | egui::Event::Key {
                        key: egui::Key::V,
                        pressed: true,
                        ..
                    }
            )
        })
}

/// Whether `modifiers` are those of ryll's paste-as-keystrokes chord,
/// Ctrl+Alt+V.
fn is_paste_shortcut_modifiers(modifiers: egui::Modifiers) -> bool {
    modifiers.ctrl && modifiers.alt
}

/// Whether one modifier is held in a set of egui modifiers.
type ModifierHeld = fn(egui::Modifiers) -> bool;

/// The modifiers egui reports as state rather than as key events, with
/// the left-hand key's press scancode that stands in for each in the
/// guest. A release is the same code with 0x80 set.
const GUEST_MODIFIERS: [(ModifierHeld, u32); 3] = [
    (|m| m.ctrl, 0x1D),  // Left Ctrl
    (|m| m.shift, 0x2A), // Left Shift
    (|m| m.alt, 0x38),   // Left Alt
];

/// The key events that take the guest from modifiers `prev` to `now`.
///
/// egui sends no key events for the modifier keys themselves, so
/// `RyllApp::handle_input` presses and releases them from the change in
/// state between frames. Passing `egui::Modifiers::default()` as `now`
/// releases every modifier the guest was told is held.
pub fn modifier_events(prev: egui::Modifiers, now: egui::Modifiers) -> Vec<InputEvent> {
    GUEST_MODIFIERS
        .iter()
        .filter(|(held, _)| held(prev) != held(now))
        .map(|(held, code)| {
            if held(now) {
                InputEvent::KeyDown(*code)
            } else {
                InputEvent::KeyUp(*code | 0x80)
            }
        })
        .collect()
}

/// Convert an egui pointer button to the SPICE wire button flag.
///
/// This was previously in `channels::inputs` alongside the scancode
/// table.  It is an egui adapter, so it lives here now.
pub fn mouse_button_to_spice(button: egui::PointerButton) -> u32 {
    match button {
        egui::PointerButton::Primary => shakenfist_spice_protocol::mouse_buttons::LEFT,
        egui::PointerButton::Secondary => shakenfist_spice_protocol::mouse_buttons::RIGHT,
        egui::PointerButton::Middle => shakenfist_spice_protocol::mouse_buttons::MIDDLE,
        egui::PointerButton::Extra1 => shakenfist_spice_protocol::mouse_buttons::UP,
        egui::PointerButton::Extra2 => shakenfist_spice_protocol::mouse_buttons::DOWN,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(logical: egui::Key, physical: egui::Key, pressed: bool, repeat: bool) -> egui::Event {
        egui::Event::Key {
            key: logical,
            physical_key: Some(physical),
            pressed,
            repeat,
            modifiers: egui::Modifiers::default(),
        }
    }

    /// Ctrl as egui-winit reports it off macOS, where Ctrl is the
    /// command modifier.
    const CTRL: egui::Modifiers = egui::Modifiers {
        alt: false,
        ctrl: true,
        shift: false,
        mac_cmd: false,
        command: true,
    };

    /// Cmd as egui-winit reports it on macOS.
    const CMD: egui::Modifiers = egui::Modifiers {
        alt: false,
        ctrl: false,
        shift: false,
        mac_cmd: true,
        command: true,
    };

    fn key_with(
        logical: egui::Key,
        physical: egui::Key,
        pressed: bool,
        modifiers: egui::Modifiers,
    ) -> egui::Event {
        egui::Event::Key {
            key: logical,
            physical_key: Some(physical),
            pressed,
            repeat: false,
            modifiers,
        }
    }

    /// A key event with the macOS Cmd key held, as egui-winit reports
    /// it on macOS (`command` mirrors `mac_cmd` there).
    fn cmd_key(k: egui::Key, pressed: bool) -> egui::Event {
        egui::Event::Key {
            key: k,
            physical_key: Some(k),
            pressed,
            repeat: false,
            modifiers: egui::Modifiers {
                mac_cmd: true,
                command: true,
                ..Default::default()
            },
        }
    }

    /// Reduce events to (is_down, scancode) pairs for comparison.
    fn wire(events: Vec<InputEvent>) -> Vec<(bool, u32)> {
        events
            .into_iter()
            .map(|e| match e {
                InputEvent::KeyDown(code) => (true, code),
                InputEvent::KeyUp(code) => (false, code),
                other => panic!("unexpected event {other:?}"),
            })
            .collect()
    }

    #[test]
    fn colon_released_after_shift_is_not_stuck() {
        // What egui hands ryll when `:` is typed twice, releasing
        // Shift before ; each time. The first release arrives as
        // logical Semicolon, so egui never clears Colon from
        // keys_down and flags the second press as a repeat.
        let mut held = HeldKeys::default();
        let first = translate_key_events(
            &[
                key(egui::Key::Colon, egui::Key::Semicolon, true, false),
                key(egui::Key::Semicolon, egui::Key::Semicolon, false, false),
            ],
            &mut held,
        );
        let second = translate_key_events(
            &[
                key(egui::Key::Colon, egui::Key::Semicolon, true, true),
                key(egui::Key::Semicolon, egui::Key::Semicolon, false, false),
            ],
            &mut held,
        );
        assert_eq!(wire(first), vec![(true, 0x27), (false, 0xA7)]);
        assert_eq!(wire(second), vec![(true, 0x27), (false, 0xA7)]);
    }

    #[test]
    fn auto_repeat_of_held_key_is_not_forwarded() {
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                key(egui::Key::A, egui::Key::A, true, false),
                key(egui::Key::A, egui::Key::A, true, true),
                key(egui::Key::A, egui::Key::A, true, true),
                key(egui::Key::A, egui::Key::A, false, false),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x1E), (false, 0x9E)]);
    }

    #[test]
    fn focus_loss_releases_held_keys() {
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                key(egui::Key::Escape, egui::Key::Escape, true, false),
                egui::Event::WindowFocused(false),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x01), (false, 0x81)]);

        // The release went to another window; pressing the key again
        // after refocusing must still be forwarded.
        let events = translate_key_events(
            &[key(egui::Key::Escape, egui::Key::Escape, true, false)],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x01)]);
    }

    #[test]
    fn release_all_empties_the_set() {
        let mut held = HeldKeys::default();
        translate_key_events(&[key(egui::Key::A, egui::Key::A, true, false)], &mut held);
        assert_eq!(wire(held.release_all()), vec![(false, 0x9E)]);
        assert!(held.release_all().is_empty());
    }

    #[test]
    fn ryll_shortcuts_are_not_forwarded() {
        for shortcut in HostShortcut::ALL {
            let mut held = HeldKeys::default();
            let k = shortcut.key();
            let events = translate_key_events(
                &[key(k, k, true, false), key(k, k, false, false)],
                &mut held,
            );
            assert!(events.is_empty(), "{shortcut:?} ({k:?}) was forwarded");
        }
    }

    #[test]
    fn screenshot_key_is_not_forwarded() {
        // F8 became a shortcut after the original exclusion list was
        // written and was forwarded to the guest (#481). Pin it by
        // name, not just through HostShortcut::ALL.
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                key(egui::Key::F8, egui::Key::F8, true, false),
                key(egui::Key::F8, egui::Key::F8, false, false),
            ],
            &mut held,
        );
        assert!(events.is_empty());
        assert!(held.release_all().is_empty());
    }

    #[test]
    fn non_shortcut_function_key_is_forwarded() {
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                key(egui::Key::F7, egui::Key::F7, true, false),
                key(egui::Key::F7, egui::Key::F7, false, false),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x41), (false, 0xC1)]);
    }

    #[test]
    fn cmd_chord_is_not_forwarded() {
        // Cmd+Q, as captured in #480: Q down and up, both with Cmd
        // held. The guest used to see a bare `q`.
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[cmd_key(egui::Key::Q, true), cmd_key(egui::Key::Q, false)],
            &mut held,
        );
        assert!(events.is_empty());
        assert!(held.release_all().is_empty());
    }

    #[test]
    fn cmd_released_before_chord_key_withholds_the_release() {
        // Cmd+W, letting go of Cmd first: the W release arrives with
        // no modifiers but must still be withheld, because its press
        // never reached the guest.
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                cmd_key(egui::Key::W, true),
                key(egui::Key::W, egui::Key::W, false, false),
            ],
            &mut held,
        );
        assert!(events.is_empty());

        // The next, ordinary W is forwarded as a full key stroke.
        let events = translate_key_events(
            &[
                key(egui::Key::W, egui::Key::W, true, false),
                key(egui::Key::W, egui::Key::W, false, false),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x11), (false, 0x91)]);
    }

    #[test]
    fn cmd_copy_release_is_not_forwarded() {
        // egui-winit turns the Cmd+C press into Event::Copy, so only
        // the release reaches us as a key event. Cmd+C is the host's
        // copy, not a chord for the guest.
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                egui::Event::ModifiersChanged(CMD),
                egui::Event::Copy,
                cmd_key(egui::Key::C, false),
            ],
            &mut held,
        );
        assert!(events.is_empty());
        assert!(held.release_all().is_empty());
    }

    #[test]
    fn cmd_copy_with_cmd_released_first_is_not_forwarded() {
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                egui::Event::ModifiersChanged(CMD),
                egui::Event::Copy,
                egui::Event::ModifiersChanged(egui::Modifiers::default()),
                key(egui::Key::C, egui::Key::C, false, false),
            ],
            &mut held,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn ctrl_clipboard_chords_reach_the_guest() {
        // Ctrl+C, Ctrl+X and Ctrl+V as egui-winit reports them off
        // macOS: Ctrl changes, the press becomes a clipboard event, and
        // only the release is a key event (#494). Ctrl itself is
        // forwarded by `RyllApp::handle_input`.
        for (event, k, codes) in [
            (
                egui::Event::Copy,
                egui::Key::C,
                [(true, 0x2E), (false, 0xAE)],
            ),
            (
                egui::Event::Cut,
                egui::Key::X,
                [(true, 0x2D), (false, 0xAD)],
            ),
            (
                egui::Event::Paste("host clipboard".to_string()),
                egui::Key::V,
                [(true, 0x2F), (false, 0xAF)],
            ),
        ] {
            let mut held = HeldKeys::default();
            let events = translate_key_events(
                &[
                    egui::Event::ModifiersChanged(CTRL),
                    event.clone(),
                    key_with(k, k, false, CTRL),
                ],
                &mut held,
            );
            // Only the chord key: the pasted text is the host's
            // clipboard and is never typed.
            assert_eq!(wire(events), codes.to_vec(), "{event:?}");
            assert!(held.release_all().is_empty(), "{event:?}");
        }
    }

    #[test]
    fn ctrl_clipboard_chord_auto_repeat_is_not_forwarded() {
        // Each auto-repeat of a held Ctrl+C is another Event::Copy.
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                egui::Event::ModifiersChanged(CTRL),
                egui::Event::Copy,
                egui::Event::Copy,
                egui::Event::Copy,
                key_with(egui::Key::C, egui::Key::C, false, CTRL),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x2E), (false, 0xAE)]);
    }

    #[test]
    fn ctrl_clipboard_chord_with_ctrl_released_first_pairs_up() {
        // The press is judged by the modifiers when it happened, not
        // when the key comes up; the next Ctrl+C must still work.
        let mut held = HeldKeys::default();
        let sequence = [
            egui::Event::ModifiersChanged(CTRL),
            egui::Event::Copy,
            egui::Event::ModifiersChanged(egui::Modifiers::default()),
            key(egui::Key::C, egui::Key::C, false, false),
        ];
        let first = translate_key_events(&sequence, &mut held);
        let second = translate_key_events(&sequence, &mut held);
        assert_eq!(wire(first), vec![(true, 0x2E), (false, 0xAE)]);
        assert_eq!(wire(second), vec![(true, 0x2E), (false, 0xAE)]);
    }

    #[test]
    fn ctrl_clipboard_chord_on_moved_layout_releases_what_was_sent() {
        // On Dvorak, logical C is the physical I key. egui-winit
        // matches the chord on the logical key; the press is sent as
        // the US-QWERTY C position and the release must undo exactly
        // that, not release I.
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                egui::Event::ModifiersChanged(CTRL),
                egui::Event::Copy,
                key_with(egui::Key::C, egui::Key::I, false, CTRL),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x2E), (false, 0xAE)]);
        assert!(held.release_all().is_empty());
    }

    #[test]
    fn windows_insert_and_delete_clipboard_chords() {
        // Windows only: Shift+Delete is a Cut and Shift+Insert a
        // Paste, told apart from Ctrl+X / Ctrl+V by Ctrl not being
        // held.
        let shift = egui::Modifiers {
            shift: true,
            ..Default::default()
        };
        for (event, k) in [
            (egui::Event::Cut, egui::Key::Delete),
            (egui::Event::Paste("x".to_string()), egui::Key::Insert),
        ] {
            let (down, up) = egui_key_to_logical(k)
                .and_then(scancode_for_logical_key)
                .unwrap();
            let mut held = HeldKeys::default();
            let events = translate_key_events(
                &[
                    egui::Event::ModifiersChanged(shift),
                    event.clone(),
                    key_with(k, k, false, shift),
                ],
                &mut held,
            );
            assert_eq!(wire(events), vec![(true, down), (false, up)], "{event:?}");
        }

        // Ctrl+Insert arrives exactly like Ctrl+C, so it is sent as
        // Ctrl+C. The release must still pair with it.
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                egui::Event::ModifiersChanged(CTRL),
                egui::Event::Copy,
                key_with(egui::Key::Insert, egui::Key::Insert, false, CTRL),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x2E), (false, 0xAE)]);
    }

    #[test]
    fn ctrl_v_with_empty_host_clipboard_is_sent_on_release() {
        // With no text on the host clipboard egui-winit reports nothing
        // for the Ctrl+V press. The guest's own clipboard may still
        // hold something, so the stroke is sent when V comes up.
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                egui::Event::ModifiersChanged(CTRL),
                key_with(egui::Key::V, egui::Key::V, false, CTRL),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x2F), (false, 0xAF)]);
    }

    #[test]
    fn consumed_paste_shortcut_release_is_withheld() {
        // ryll consumed Ctrl+Alt+V as paste-as-keystrokes and did not
        // forward that frame. The V release that follows must not
        // reach the guest, whichever modifier is let go first.
        let ctrl_alt = egui::Modifiers { alt: true, ..CTRL };
        for release_mods in [ctrl_alt, CTRL, egui::Modifiers::default()] {
            let mut held = HeldKeys::default();
            held.withhold_paste_release();
            let events = translate_key_events(
                &[key_with(egui::Key::V, egui::Key::V, false, release_mods)],
                &mut held,
            );
            assert!(events.is_empty(), "{release_mods:?}");
        }
    }

    #[test]
    fn ctrl_alt_v_with_empty_host_clipboard_is_not_forwarded() {
        // Ctrl+Alt+V is ryll's own paste shortcut. With no text on the
        // host clipboard egui-winit reports nothing for the press, and
        // the release must not be mistaken for the guest's Ctrl+V.
        let ctrl_alt = egui::Modifiers { alt: true, ..CTRL };
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                egui::Event::ModifiersChanged(ctrl_alt),
                key_with(egui::Key::V, egui::Key::V, false, ctrl_alt),
            ],
            &mut held,
        );
        assert!(events.is_empty());
    }

    #[test]
    fn clipboard_chord_after_untranslated_frame_sees_its_modifiers() {
        // Ctrl went down in a frame whose events ryll never translated
        // (a dialog was open), so no ModifiersChanged reaches
        // translate_key_events before the Ctrl+C press.
        let mut held = HeldKeys::default();
        held.end_frame(CTRL);
        let events = translate_key_events(
            &[
                egui::Event::Copy,
                key_with(egui::Key::C, egui::Key::C, false, CTRL),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x2E), (false, 0xAE)]);
    }

    #[test]
    fn focus_loss_forgets_modifiers() {
        // egui-winit clears its modifiers on focus loss without a
        // ModifiersChanged; the tracked copy must follow.
        let mut held = HeldKeys::default();
        translate_key_events(
            &[
                egui::Event::ModifiersChanged(CTRL),
                egui::Event::WindowFocused(false),
            ],
            &mut held,
        );
        assert_eq!(held.modifiers, egui::Modifiers::default());
    }

    #[test]
    fn modifier_events_follow_the_change_in_state() {
        let ctrl_shift = egui::Modifiers {
            shift: true,
            ..CTRL
        };
        let alt = egui::Modifiers {
            alt: true,
            ..Default::default()
        };
        let none = egui::Modifiers::default();
        assert_eq!(
            wire(modifier_events(none, ctrl_shift)),
            vec![(true, 0x1D), (true, 0x2A)]
        );
        assert_eq!(
            wire(modifier_events(ctrl_shift, alt)),
            vec![(false, 0x9D), (false, 0xAA), (true, 0x38)]
        );
        assert!(modifier_events(alt, alt).is_empty());
        // Releasing everything, as RyllApp::release_guest_keys does.
        let all = egui::Modifiers {
            alt: true,
            ..ctrl_shift
        };
        assert_eq!(
            wire(modifier_events(all, none)),
            vec![(false, 0x9D), (false, 0xAA), (false, 0xB8)]
        );
    }

    #[test]
    fn paste_shortcut_is_seen_as_a_paste_event() {
        let ctrl_alt = egui::Modifiers { alt: true, ..CTRL };
        let paste = [egui::Event::Paste("text".to_string())];
        let v_press = [key_with(egui::Key::V, egui::Key::V, true, ctrl_alt)];
        // Off macOS egui-winit reports the press as Event::Paste.
        assert!(paste_shortcut_pressed(ctrl_alt, &paste));
        // On macOS Ctrl is not the command modifier, so V is a key.
        assert!(paste_shortcut_pressed(ctrl_alt, &v_press));
        // Ctrl+V on its own is the guest's.
        assert!(!paste_shortcut_pressed(CTRL, &paste));
        assert!(!paste_shortcut_pressed(ctrl_alt, &[]));
    }

    #[test]
    fn focus_loss_releases_a_clipboard_chord_key() {
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                egui::Event::ModifiersChanged(CTRL),
                egui::Event::Copy,
                egui::Event::WindowFocused(false),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x2E), (false, 0xAE)]);
    }

    #[test]
    fn key_held_before_cmd_is_still_released() {
        // A is held and forwarded, then Cmd goes down. A's
        // auto-repeat and release now carry Cmd, but the guest holds A
        // and must be told when it comes up.
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                key(egui::Key::A, egui::Key::A, true, false),
                cmd_key(egui::Key::A, true),
                cmd_key(egui::Key::A, false),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x1E), (false, 0x9E)]);
    }

    #[test]
    fn suppressed_press_without_release_does_not_eat_next_stroke() {
        // A Cmd chord press whose release never reaches ryll (Cmd+Tab
        // hands the release to another app) must not cause the next
        // ordinary stroke of that key to be dropped or half-sent.
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[
                cmd_key(egui::Key::Tab, true),
                key(egui::Key::Tab, egui::Key::Tab, true, false),
                key(egui::Key::Tab, egui::Key::Tab, false, false),
            ],
            &mut held,
        );
        assert_eq!(wire(events), vec![(true, 0x0F), (false, 0x8F)]);
    }
}
