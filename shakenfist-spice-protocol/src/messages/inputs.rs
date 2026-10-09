//! Inputs channel messages.
//!
//! Layouts follow spice-common's `spice.proto`, `channel InputsChannel`.
//! The server's `MOUSE_MOTION_ACK` has an empty body and needs no type.

use super::WireType;
use crate::reader::{BoundedReader, LinkError};

/// `SPICE_MSG_INPUTS_INIT` (server to client): the guest's keyboard
/// modifier state when the channel opens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputsInit {
    /// `keyboard_modifiers::*` flags.
    pub keyboard_modifiers: u16,
}

impl InputsInit {
    pub const SIZE: usize = 2;
}

impl WireType for InputsInit {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(InputsInit {
            keyboard_modifiers: r.read_u16()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.keyboard_modifiers.to_le_bytes());
    }
}

/// Keyboard modifier state. Two messages share this layout, a single
/// `keyboard_modifier_flags`:
///
/// - `SPICE_MSG_INPUTS_KEY_MODIFIERS` (server to client): the guest's
///   lock keys changed;
/// - `SPICE_MSGC_INPUTS_KEY_MODIFIERS` (client to server): the lock keys
///   the client wants the guest to have.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyModifiers {
    /// `keyboard_modifiers::*` flags.
    pub modifiers: u16,
}

impl KeyModifiers {
    pub const SIZE: usize = 2;
}

impl WireType for KeyModifiers {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(KeyModifiers {
            modifiers: r.read_u16()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.modifiers.to_le_bytes());
    }
}

/// `SPICE_MSGC_INPUTS_KEY_DOWN` and `SPICE_MSGC_INPUTS_KEY_UP` (client to
/// server), which share this layout: up to four PC AT scan-code bytes
/// (spice.proto `code`). spice-server feeds them to the guest low byte
/// first, stopping at the first zero byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyEvent {
    pub scancode: u32,
}

impl KeyEvent {
    pub const SIZE: usize = 4;
}

impl WireType for KeyEvent {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(KeyEvent {
            scancode: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.scancode.to_le_bytes());
    }
}

/// `SPICE_MSGC_INPUTS_KEY_SCANCODE` (client to server): raw scan-code bytes
/// (spice.proto `Data`), which run to the end of the message. The body
/// bounds them; an empty body is valid.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyScancode {
    pub codes: Vec<u8>,
}

impl WireType for KeyScancode {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(KeyScancode {
            codes: r.read_bytes(r.remaining())?.to_vec(),
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.codes);
    }
}

/// `SPICE_MSGC_INPUTS_MOUSE_MOTION` (client to server): a relative pointer
/// movement, in server mouse mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseMotion {
    pub dx: i32,
    pub dy: i32,
    /// `mouse_buttons::*` mask (spice.proto `mouse_button_mask`).
    pub buttons_state: u16,
}

impl MouseMotion {
    pub const SIZE: usize = 10;
}

impl WireType for MouseMotion {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(MouseMotion {
            dx: i32::from_le_bytes(r.read_array()?),
            dy: i32::from_le_bytes(r.read_array()?),
            buttons_state: r.read_u16()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.dx.to_le_bytes());
        out.extend_from_slice(&self.dy.to_le_bytes());
        out.extend_from_slice(&self.buttons_state.to_le_bytes());
    }
}

/// `SPICE_MSGC_INPUTS_MOUSE_POSITION` (client to server): an absolute
/// pointer position on one display, in client mouse mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MousePosition {
    pub x: u32,
    pub y: u32,
    /// `mouse_buttons::*` mask (spice.proto `mouse_button_mask`).
    pub buttons_state: u16,
    pub display_id: u8,
}

impl MousePosition {
    pub const SIZE: usize = 11;
}

impl WireType for MousePosition {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(MousePosition {
            x: r.read_u32()?,
            y: r.read_u32()?,
            buttons_state: r.read_u16()?,
            display_id: r.read_u8()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
        out.extend_from_slice(&self.buttons_state.to_le_bytes());
        out.push(self.display_id);
    }
}

/// `SPICE_MSGC_INPUTS_MOUSE_PRESS` and `SPICE_MSGC_INPUTS_MOUSE_RELEASE`
/// (client to server), which share this layout: the button that changed,
/// and the state of every button after the change.
///
/// spice.proto gives the two fields different types. `button` is a
/// `mouse_button` enum (`mouse_button_id::*`, LEFT = 1), while
/// `buttons_state` is a `mouse_button_mask` (`mouse_buttons::*`, LEFT =
/// 1 << 0). A caller holding a mask converts it to an id itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MouseButton {
    /// A `mouse_button_id::*` value, kept raw so unknown ids survive.
    pub button: u8,
    /// `mouse_buttons::*` mask.
    pub buttons_state: u16,
}

impl MouseButton {
    pub const SIZE: usize = 3;
}

impl WireType for MouseButton {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(MouseButton {
            button: r.read_u8()?,
            buttons_state: r.read_u16()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.push(self.button);
        out.extend_from_slice(&self.buttons_state.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::mouse_button_id;
    use crate::messages::assert_round_trip;

    #[test]
    fn inputs_init_round_trips_and_decodes() {
        assert_round_trip(&InputsInit {
            keyboard_modifiers: 0x8007,
        });
        assert_eq!(
            InputsInit::decode(&[0x05, 0x00, 0xff]).expect("decodes"),
            InputsInit {
                keyboard_modifiers: 5
            }
        );
        assert!(InputsInit::decode(&[5]).is_err());
    }

    #[test]
    fn key_modifiers_round_trips_and_decodes() {
        assert_round_trip(&KeyModifiers { modifiers: 0xffff });
        assert_eq!(
            KeyModifiers::decode(&[0x06, 0x00]).expect("decodes"),
            KeyModifiers { modifiers: 6 }
        );
        assert!(KeyModifiers::decode(&[]).is_err());
    }

    #[test]
    fn key_event_round_trips_and_decodes() {
        assert_round_trip(&KeyEvent { scancode: 0xe048 });
        assert_eq!(
            KeyEvent::decode(&[0x48, 0xe0, 0x00, 0x00]).expect("decodes"),
            KeyEvent { scancode: 0xe048 }
        );
        assert!(KeyEvent::decode(&[0x48, 0xe0, 0x00]).is_err());
    }

    #[test]
    fn key_scancode_round_trips_and_decodes() {
        assert_round_trip(&KeyScancode { codes: Vec::new() });
        assert_round_trip(&KeyScancode {
            codes: vec![0xe0, 0x48, 0xe0, 0xc8],
        });
        assert_eq!(
            KeyScancode::decode(&[0x1e, 0x9e]).expect("decodes"),
            KeyScancode {
                codes: vec![0x1e, 0x9e]
            }
        );
    }

    #[test]
    fn mouse_motion_round_trips_and_decodes() {
        assert_round_trip(&MouseMotion {
            dx: i32::MIN,
            dy: i32::MAX,
            buttons_state: 0x7f,
        });
        assert_eq!(
            MouseMotion::decode(&[0xfe, 0xff, 0xff, 0xff, 3, 0, 0, 0, 0xff, 0x01])
                .expect("decodes"),
            MouseMotion {
                dx: -2,
                dy: 3,
                buttons_state: 0x01ff,
            }
        );
        assert!(MouseMotion::decode(&[0; 9]).is_err());
    }

    #[test]
    fn mouse_position_round_trips_and_decodes() {
        assert_round_trip(&MousePosition {
            x: u32::MAX,
            y: 0,
            buttons_state: 4,
            display_id: 3,
        });
        assert_eq!(
            MousePosition::decode(&[4, 3, 2, 1, 0x0d, 0x0c, 0x0b, 0x0a, 0xff, 0x01, 2])
                .expect("decodes"),
            MousePosition {
                x: 0x0102_0304,
                y: 0x0a0b_0c0d,
                buttons_state: 0x01ff,
                display_id: 2,
            }
        );
        assert!(MousePosition::decode(&[0; 10]).is_err());
    }

    #[test]
    fn mouse_button_round_trips_and_decodes() {
        assert_round_trip(&MouseButton {
            button: mouse_button_id::EXTRA,
            buttons_state: 0x40,
        });
        // An id the enum does not define survives.
        assert_round_trip(&MouseButton {
            button: 0xff,
            buttons_state: 0,
        });
        // The id goes on the wire as given: RIGHT is 3, not its mask 4.
        let mut out = Vec::new();
        MouseButton {
            button: mouse_button_id::RIGHT,
            buttons_state: 0x0004,
        }
        .write(&mut out);
        assert_eq!(out, [3, 0x04, 0x00]);
        assert_eq!(
            MouseButton::decode(&out).expect("decodes"),
            MouseButton {
                button: 3,
                buttons_state: 4,
            }
        );
        assert!(MouseButton::decode(&[3, 4]).is_err());
    }
}
