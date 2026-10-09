//! Inputs channel messages.
use byteorder::{LittleEndian, WriteBytesExt};
use std::io;

/// Input key modifiers message (client -> server)
#[derive(Debug, Clone, Copy)]
pub struct InputsKeyModifiers {
    pub modifiers: u16,
}

impl InputsKeyModifiers {
    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u16::<LittleEndian>(self.modifiers)?;
        Ok(())
    }
}

/// Key down/up message (client -> server)
#[derive(Debug, Clone, Copy)]
pub struct KeyEvent {
    pub scancode: u32,
}

impl KeyEvent {
    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u32::<LittleEndian>(self.scancode)?;
        Ok(())
    }
}

/// Mouse motion message (client -> server, relative deltas)
#[derive(Debug, Clone, Copy)]
pub struct MouseMotion {
    pub dx: i32,
    pub dy: i32,
    /// `flags16 mouse_button_mask` per spice.proto.
    pub buttons: u16,
}

impl MouseMotion {
    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_i32::<LittleEndian>(self.dx)?;
        buf.write_i32::<LittleEndian>(self.dy)?;
        buf.write_u16::<LittleEndian>(self.buttons)?;
        Ok(())
    }
}

/// Mouse position message (client -> server)
#[derive(Debug, Clone, Copy)]
pub struct MousePosition {
    pub x: u32,
    pub y: u32,
    /// `flags16 mouse_button_mask` per spice.proto.
    pub buttons: u16,
    pub display_id: u8,
}

impl MousePosition {
    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u32::<LittleEndian>(self.x)?;
        buf.write_u32::<LittleEndian>(self.y)?;
        buf.write_u16::<LittleEndian>(self.buttons)?;
        buf.write_u8(self.display_id)?;
        Ok(())
    }
}

/// Mouse button message (client -> server)
#[derive(Debug, Clone, Copy)]
pub struct MouseButton {
    /// `enum8 mouse_button` per spice.proto. Encoded to a
    /// button id on write via `mask_to_id`.
    pub button: u8,
    /// `flags16 mouse_button_mask` per spice.proto.
    pub buttons_state: u16,
}

impl MouseButton {
    fn mask_to_id(mask: u8) -> u8 {
        match mask {
            0x01 => 1, // LEFT
            0x02 => 2, // MIDDLE
            0x04 => 3, // RIGHT
            0x08 => 4, // UP (scroll)
            0x10 => 5, // DOWN (scroll)
            _ => 0,
        }
    }

    pub fn write(&self, buf: &mut Vec<u8>) -> io::Result<()> {
        buf.write_u8(Self::mask_to_id(self.button))?;
        buf.write_u16::<LittleEndian>(self.buttons_state)?;
        Ok(())
    }
}
