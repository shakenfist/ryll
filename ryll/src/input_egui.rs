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
/// are withheld too and the guest never sees half of a key stroke.
#[derive(Debug, Default)]
pub struct HeldKeys {
    /// Held key -> the wire scancode that releases it.
    held: HashMap<egui::Key, u32>,
    /// Keys whose press was withheld from the guest as part of a host
    /// Cmd chord, so their release must be withheld as well.
    suppressed: HashSet<egui::Key>,
}

impl HeldKeys {
    /// Release every held key, returning the key-up events to send so
    /// the guest does not see a key stuck down while ryll is not
    /// forwarding input to it.
    pub fn release_all(&mut self) -> Vec<InputEvent> {
        // A withheld press whose release goes elsewhere must not
        // swallow the next, unrelated release of the same key.
        self.suppressed.clear();
        self.held
            .drain()
            .map(|(_, up_code)| InputEvent::KeyUp(up_code))
            .collect()
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
pub fn translate_key_events(events: &[egui::Event], held: &mut HeldKeys) -> Vec<InputEvent> {
    let mut out = Vec::new();
    for event in events {
        match event {
            egui::Event::WindowFocused(false) => out.extend(held.release_all()),
            egui::Event::Key {
                key,
                physical_key,
                pressed,
                modifiers,
                ..
            } => {
                let lookup_key = physical_key.unwrap_or(*key);
                if HostShortcut::is_shortcut_key(lookup_key) {
                    continue;
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
                } else if held.held.remove(&lookup_key).is_some() {
                    out.push(InputEvent::KeyUp(up_code));
                } else if held.suppressed.remove(&lookup_key) || modifiers.mac_cmd {
                    // The release of a withheld Cmd chord press. The
                    // `mac_cmd` arm covers presses egui turned into
                    // Copy / Cut / Paste events, which never reach
                    // this function as key presses.
                } else {
                    out.push(InputEvent::KeyUp(up_code));
                }
            }
            _ => {}
        }
    }
    out
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
        // the release reaches us as a key event.
        let mut held = HeldKeys::default();
        let events = translate_key_events(
            &[egui::Event::Copy, cmd_key(egui::Key::C, false)],
            &mut held,
        );
        assert!(events.is_empty());
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
