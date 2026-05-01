/// egui-to-substrate adapter for keyboard and mouse input.
///
/// This module is the only place in the ryll binary that converts
/// egui input types to the substrate's neutral `LogicalKey` /
/// SPICE-button representations.  A future web-frontend adapter
/// will provide the same conversions for `KeyboardEvent.code` /
/// browser mouse-button values without touching this file.
use eframe::egui;

use crate::channels::inputs::{Direction, LogicalKey, NavKey, PunctKey, WSKey};

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
