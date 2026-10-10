//! VDAgent messages carried over the main channel's AGENT_DATA message.
//!
//! Layouts follow spice-protocol's `spice/vd_agent.h`, where every struct is
//! packed and little-endian. Each guest agent message is a
//! [`VdAgentMessageHeader`] followed by `size` bytes of body, whose layout the
//! header's type selects. spice-server forwards guest agent data to the
//! client in AGENT_DATA messages of at most 2048 bytes
//! (spice-common's `SPICE_AGENT_MAX_DATA_SIZE`), so a long agent message
//! spans several of them; reassembling them is the caller's job.
//!
//! The clipboard messages start with a selection header only when the peer
//! announced
//! [`VD_AGENT_CAP_CLIPBOARD_SELECTION`](crate::constants::vd_agent::VD_AGENT_CAP_CLIPBOARD_SELECTION),
//! so their layout depends on negotiation. They implement
//! [`ClipboardWireType`] rather than [`WireType`], and take that context as
//! `has_selection`.

use super::WireType;
use crate::constants::vd_agent::{
    VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD, VD_AGENT_CONFIG_MONITORS_FLAG_PHYSICAL_SIZE,
    VD_AGENT_PROTOCOL, VD_AGENT_SUCCESS,
};
use crate::reader::{BoundedReader, LinkError};

/// `VDAgentMessage` (vd_agent.h:49-58): the header in front of every guest
/// agent message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VdAgentMessageHeader {
    /// Should be [`VD_AGENT_PROTOCOL`]. Not checked on read.
    pub protocol: u32,
    /// One of the `VD_AGENT_*` message types.
    pub message_type: u32,
    /// Opaque to the receiver; senders write 0.
    pub opaque: u64,
    /// Length of the body that follows the header.
    pub size: u32,
}

impl VdAgentMessageHeader {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 20;

    /// A header for a `size`-byte body of `message_type`, with the current
    /// protocol and a zero `opaque`.
    #[must_use]
    pub fn new(message_type: u32, size: u32) -> Self {
        VdAgentMessageHeader {
            protocol: VD_AGENT_PROTOCOL,
            message_type,
            opaque: 0,
            size,
        }
    }
}

impl WireType for VdAgentMessageHeader {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(VdAgentMessageHeader {
            protocol: r.read_u32()?,
            message_type: r.read_u32()?,
            opaque: r.read_u64()?,
            size: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.protocol.to_le_bytes());
        out.extend_from_slice(&self.message_type.to_le_bytes());
        out.extend_from_slice(&self.opaque.to_le_bytes());
        out.extend_from_slice(&self.size.to_le_bytes());
    }
}

/// `VDAgentMonConfig` (vd_agent.h:193-204): one monitor of a
/// [`MonitorsConfig`]. A width and height of 0 marks a disabled monitor, for
/// agents with `VD_AGENT_CAP_SPARSE_MONITORS_CONFIG`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MonConfig {
    pub height: u32,
    pub width: u32,
    pub depth: u32,
    pub x: i32,
    pub y: i32,
}

impl MonConfig {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 20;
}

impl WireType for MonConfig {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(MonConfig {
            height: r.read_u32()?,
            width: r.read_u32()?,
            depth: r.read_u32()?,
            x: r.read_i32()?,
            y: r.read_i32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
        out.extend_from_slice(&self.depth.to_le_bytes());
        out.extend_from_slice(&self.x.to_le_bytes());
        out.extend_from_slice(&self.y.to_le_bytes());
    }
}

/// `VDAgentMonitorMM` (vd_agent.h:222-232): a monitor's physical size in
/// millimetres. Zero in both means no size information.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct MonitorMm {
    pub height: u16,
    pub width: u16,
}

impl MonitorMm {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 4;
}

impl WireType for MonitorMm {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(MonitorMm {
            height: r.read_u16()?,
            width: r.read_u16()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.height.to_le_bytes());
        out.extend_from_slice(&self.width.to_le_bytes());
    }
}

/// `VD_AGENT_MONITORS_CONFIG` (client to agent): `VDAgentMonitorsConfig`
/// (vd_agent.h:211-219).
///
/// Wire format: `num_of_monitors` (u32), `flags` (u32), that many
/// [`MonConfig`]s, then, only when `flags` has
/// [`VD_AGENT_CONFIG_MONITORS_FLAG_PHYSICAL_SIZE`], one [`MonitorMm`] per
/// monitor. `num_of_monitors` is `monitors.len()`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MonitorsConfig {
    /// `VD_AGENT_CONFIG_MONITORS_FLAG_*`, unknown bits included.
    pub flags: u32,
    pub monitors: Vec<MonConfig>,
    /// One per monitor when `flags` has the physical-size flag, and empty
    /// otherwise. So that the encoding is always well formed, the writer
    /// emits exactly `monitors.len()` entries when the flag is set, padding
    /// with zeros ("no size information") or dropping extras, and none when
    /// it is clear.
    pub physical_sizes: Vec<MonitorMm>,
}

impl MonitorsConfig {
    /// The fixed part: `num_of_monitors` and `flags`.
    pub const HEADER_SIZE: usize = 8;

    /// Whether the physical-size table follows the monitors.
    #[must_use]
    pub fn has_physical_size(&self) -> bool {
        self.flags & VD_AGENT_CONFIG_MONITORS_FLAG_PHYSICAL_SIZE != 0
    }
}

impl WireType for MonitorsConfig {
    /// # Errors
    ///
    /// [`LinkError::TooLarge`] if `num_of_monitors` exceeds what the rest of
    /// the body can hold, counting the physical-size table when the flag
    /// says it is present, and [`LinkError::Truncated`] if the body is
    /// shorter than the fixed part.
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let num_of_monitors = r.read_u32()? as usize;
        let flags = r.read_u32()?;
        let physical = flags & VD_AGENT_CONFIG_MONITORS_FLAG_PHYSICAL_SIZE != 0;

        // The count is peer-supplied. Bound it by the body before
        // reserving, as spice-common's agent_message_monitors_config_from_le
        // does, so that a short message cannot reserve gigabytes.
        let element = MonConfig::SIZE + if physical { MonitorMm::SIZE } else { 0 };
        r.check_count("num_of_monitors", num_of_monitors, element)?;

        let mut monitors = Vec::with_capacity(num_of_monitors);
        for _ in 0..num_of_monitors {
            monitors.push(MonConfig::read(r)?);
        }
        let mut physical_sizes = Vec::new();
        if physical {
            physical_sizes.reserve(num_of_monitors);
            for _ in 0..num_of_monitors {
                physical_sizes.push(MonitorMm::read(r)?);
            }
        }
        Ok(MonitorsConfig {
            flags,
            monitors,
            physical_sizes,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&(self.monitors.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        for m in &self.monitors {
            m.write(out);
        }
        if self.has_physical_size() {
            for i in 0..self.monitors.len() {
                self.physical_sizes
                    .get(i)
                    .copied()
                    .unwrap_or_default()
                    .write(out);
            }
        }
    }
}

/// `VD_AGENT_ANNOUNCE_CAPABILITIES` (both directions):
/// `VDAgentAnnounceCapabilities` (vd_agent.h:399-402).
///
/// Wire format: `request` (u32), then capability words (u32 each) to the
/// end of the body. The word count is implied by the body length, as
/// `VD_AGENT_CAPS_SIZE_FROM_MSG_SIZE` (vd_agent.h:404-405) computes it, so
/// a trailing partial word is ignored.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AnnounceCapabilities {
    /// Non-zero asks the peer to announce its capabilities in return.
    pub request: u32,
    /// Capability bitmap: bit `n % 32` of word `n / 32` is capability `n`
    /// (`VD_AGENT_CAP_*`).
    pub caps: Vec<u32>,
}

impl AnnounceCapabilities {
    /// The fixed part: `request`.
    pub const HEADER_SIZE: usize = 4;

    /// Whether capability `cap` is set, as `VD_AGENT_HAS_CAPABILITY`
    /// (vd_agent.h:411-412) tests it. A bit in a word the peer did not send
    /// is clear.
    #[must_use]
    pub fn has_capability(&self, cap: u32) -> bool {
        self.caps
            .get(cap as usize / 32)
            .is_some_and(|word| word & (1 << (cap % 32)) != 0)
    }

    /// One more than the highest capability
    /// [`set_capability`](Self::set_capability) will set: sixteen words of
    /// bits, far more than vd_agent.h defines.
    pub const MAX_CAPABILITY: u32 = 32 * 16;

    /// Set capability `cap`, growing the bitmap to hold it.
    ///
    /// This builds the caller's own announcement from `VD_AGENT_CAP_*`
    /// constants. Never pass it a peer-supplied value: the bitmap grows to
    /// hold `cap`. A `cap` at or above
    /// [`MAX_CAPABILITY`](Self::MAX_CAPABILITY) is ignored, so that a
    /// caller bug cannot allocate hundreds of MiB.
    pub fn set_capability(&mut self, cap: u32) {
        if cap >= Self::MAX_CAPABILITY {
            return;
        }
        let word = cap as usize / 32;
        if self.caps.len() <= word {
            self.caps.resize(word + 1, 0);
        }
        self.caps[word] |= 1 << (cap % 32);
    }
}

impl WireType for AnnounceCapabilities {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let request = r.read_u32()?;
        // The count comes from the body itself, so it needs no other bound.
        let count = r.remaining() / 4;
        let caps = r.read_vec_u32(count, count)?;
        Ok(AnnounceCapabilities { request, caps })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.request.to_le_bytes());
        for word in &self.caps {
            out.extend_from_slice(&word.to_le_bytes());
        }
    }
}

/// `VD_AGENT_REPLY` (agent to client): `VDAgentReply` (vd_agent.h:261-264),
/// the agent's answer to a request such as `VD_AGENT_MONITORS_CONFIG`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VdAgentReply {
    /// The `VD_AGENT_*` type of the request being answered.
    pub reply_type: u32,
    /// [`VD_AGENT_SUCCESS`] (1) or `VD_AGENT_ERROR` (2). Zero is not
    /// success.
    pub error: u32,
}

impl VdAgentReply {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 8;

    /// Whether the agent reported success: `error` is
    /// [`VD_AGENT_SUCCESS`], which vd_agent.h:267 defines as 1.
    #[must_use]
    pub fn is_success(&self) -> bool {
        self.error == VD_AGENT_SUCCESS
    }
}

impl WireType for VdAgentReply {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(VdAgentReply {
            reply_type: r.read_u32()?,
            error: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.reply_type.to_le_bytes());
        out.extend_from_slice(&self.error.to_le_bytes());
    }
}

/// A clipboard message body, whose layout depends on whether the peer
/// announced `VD_AGENT_CAP_CLIPBOARD_SELECTION`.
///
/// With the capability, the body starts with a `uint8_t selection` and
/// three reserved bytes (vd_agent.h:271-326); spice-vdagentd adds the header
/// only when its peer announced the capability, and spice-gtk reads it only
/// when the agent did. Without it there is no header, and the selection is
/// implicitly [`VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD`]: the reader returns
/// that, and the writer drops whatever selection the value holds.
///
/// The reserved bytes are not modelled. The reader skips them whatever they
/// hold, and the writer writes zeros.
pub trait ClipboardWireType: Sized {
    /// Parse a body. Trailing bytes are ignored where the layout has an end.
    fn read_with(r: &mut BoundedReader<'_>, has_selection: bool) -> Result<Self, LinkError>;
    /// Append the body's wire encoding to `out`.
    fn write_with(&self, out: &mut Vec<u8>, has_selection: bool);
    /// Parse a whole message body.
    fn decode_with(body: &[u8], has_selection: bool) -> Result<Self, LinkError> {
        Self::read_with(&mut BoundedReader::new(body), has_selection)
    }
}

/// The size of the clipboard selection header, when it is present.
pub const CLIPBOARD_SELECTION_HEADER_SIZE: usize = 4;

fn read_selection(r: &mut BoundedReader<'_>, has_selection: bool) -> Result<u8, LinkError> {
    if !has_selection {
        return Ok(VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD);
    }
    let header = r.read_array::<CLIPBOARD_SELECTION_HEADER_SIZE>()?;
    Ok(header[0])
}

fn write_selection(out: &mut Vec<u8>, selection: u8, has_selection: bool) {
    if has_selection {
        out.extend_from_slice(&[selection, 0, 0, 0]);
    }
}

/// `VD_AGENT_CLIPBOARD_GRAB` (both directions): `VDAgentClipboardGrab`
/// (vd_agent.h:301-310). The sender owns the clipboard and offers these
/// types.
///
/// The type list runs to the end of the body; a trailing partial type is
/// ignored. The `serial` that `VD_AGENT_CAP_CLIPBOARD_GRAB_SERIAL` adds is
/// not modelled. Like the selection header, a sender includes it only when
/// its peer announced the capability, so a side that does not announce it
/// (ryll does not) never receives one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardGrab {
    /// `VD_AGENT_CLIPBOARD_SELECTION_*`.
    pub selection: u8,
    /// `VD_AGENT_CLIPBOARD_*` types on offer.
    pub types: Vec<u32>,
}

impl ClipboardGrab {
    /// Whether the grab offers `clipboard_type`, anywhere in its list.
    #[must_use]
    pub fn offers(&self, clipboard_type: u32) -> bool {
        self.types.contains(&clipboard_type)
    }
}

impl ClipboardWireType for ClipboardGrab {
    fn read_with(r: &mut BoundedReader<'_>, has_selection: bool) -> Result<Self, LinkError> {
        let selection = read_selection(r, has_selection)?;
        // The count comes from the body itself, so it needs no other bound.
        let count = r.remaining() / 4;
        let types = r.read_vec_u32(count, count)?;
        Ok(ClipboardGrab { selection, types })
    }

    fn write_with(&self, out: &mut Vec<u8>, has_selection: bool) {
        write_selection(out, self.selection, has_selection);
        for ty in &self.types {
            out.extend_from_slice(&ty.to_le_bytes());
        }
    }
}

/// `VD_AGENT_CLIPBOARD_REQUEST` (both directions): `VDAgentClipboardRequest`
/// (vd_agent.h:312-318). Asks the clipboard owner for its data in one type.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardRequest {
    /// `VD_AGENT_CLIPBOARD_SELECTION_*`.
    pub selection: u8,
    /// The `VD_AGENT_CLIPBOARD_*` type wanted.
    pub clipboard_type: u32,
}

impl ClipboardWireType for ClipboardRequest {
    fn read_with(r: &mut BoundedReader<'_>, has_selection: bool) -> Result<Self, LinkError> {
        Ok(ClipboardRequest {
            selection: read_selection(r, has_selection)?,
            clipboard_type: r.read_u32()?,
        })
    }

    fn write_with(&self, out: &mut Vec<u8>, has_selection: bool) {
        write_selection(out, self.selection, has_selection);
        out.extend_from_slice(&self.clipboard_type.to_le_bytes());
    }
}

/// `VD_AGENT_CLIPBOARD` (both directions): `VDAgentClipboard`
/// (vd_agent.h:271-278). Clipboard data in one type; the data runs to the
/// end of the body. A message of type `VD_AGENT_CLIPBOARD_NONE` with no
/// data says the sender has nothing to give.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Clipboard {
    /// `VD_AGENT_CLIPBOARD_SELECTION_*`.
    pub selection: u8,
    /// The `VD_AGENT_CLIPBOARD_*` type of `data`.
    pub clipboard_type: u32,
    pub data: Vec<u8>,
}

impl ClipboardWireType for Clipboard {
    fn read_with(r: &mut BoundedReader<'_>, has_selection: bool) -> Result<Self, LinkError> {
        let selection = read_selection(r, has_selection)?;
        let clipboard_type = r.read_u32()?;
        let data = r.read_bytes(r.remaining())?.to_vec();
        Ok(Clipboard {
            selection,
            clipboard_type,
            data,
        })
    }

    fn write_with(&self, out: &mut Vec<u8>, has_selection: bool) {
        write_selection(out, self.selection, has_selection);
        out.extend_from_slice(&self.clipboard_type.to_le_bytes());
        out.extend_from_slice(&self.data);
    }
}

/// `VD_AGENT_CLIPBOARD_RELEASE` (both directions): `VDAgentClipboardRelease`
/// (vd_agent.h:320-326). The sender no longer owns the clipboard. Without
/// the selection capability the body is empty.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardRelease {
    /// `VD_AGENT_CLIPBOARD_SELECTION_*`.
    pub selection: u8,
}

impl ClipboardWireType for ClipboardRelease {
    fn read_with(r: &mut BoundedReader<'_>, has_selection: bool) -> Result<Self, LinkError> {
        Ok(ClipboardRelease {
            selection: read_selection(r, has_selection)?,
        })
    }

    fn write_with(&self, out: &mut Vec<u8>, has_selection: bool) {
        write_selection(out, self.selection, has_selection);
    }
}

/// Write `value` with `has_selection`, read it back the same way, and assert
/// it is unchanged and that the reader consumed exactly the bytes written.
#[cfg(test)]
pub(crate) fn assert_round_trip_with<T: ClipboardWireType + PartialEq + std::fmt::Debug>(
    value: &T,
    has_selection: bool,
) {
    let mut buf = Vec::new();
    value.write_with(&mut buf, has_selection);
    let mut reader = BoundedReader::new(&buf);
    let back = T::read_with(&mut reader, has_selection).expect("a written value reads back");
    assert_eq!(&back, value);
    assert_eq!(
        reader.position(),
        buf.len(),
        "reader consumed every written byte"
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::vd_agent::*;
    use crate::messages::assert_round_trip;

    fn encode<T: WireType>(value: &T) -> Vec<u8> {
        let mut out = Vec::new();
        value.write(&mut out);
        out
    }

    fn encode_with<T: ClipboardWireType>(value: &T, has_selection: bool) -> Vec<u8> {
        let mut out = Vec::new();
        value.write_with(&mut out, has_selection);
        out
    }

    // --- VdAgentMessageHeader tests ---

    #[test]
    fn header_round_trips() {
        assert_round_trip(&VdAgentMessageHeader::new(VD_AGENT_CLIPBOARD, 7));
        assert_round_trip(&VdAgentMessageHeader {
            protocol: 99,
            message_type: u32::MAX,
            opaque: u64::MAX,
            size: 0,
        });
    }

    #[test]
    fn header_decodes_vd_agent_h_layout() {
        // VDAgentMessage: uint32 protocol, uint32 type, uint64 opaque,
        // uint32 size, then the body.
        let body = [
            1, 0, 0, 0, // protocol
            6, 0, 0, 0, // type: ANNOUNCE_CAPABILITIES
            1, 2, 3, 4, 5, 6, 7, 8, // opaque
            8, 0, 0, 0,    // size
            0xaa, // the body that follows is not the header's
        ];
        assert_eq!(
            VdAgentMessageHeader::decode(&body).expect("decodes"),
            VdAgentMessageHeader {
                protocol: VD_AGENT_PROTOCOL,
                message_type: VD_AGENT_ANNOUNCE_CAPABILITIES,
                opaque: 0x0807_0605_0403_0201,
                size: 8,
            }
        );
        assert!(VdAgentMessageHeader::decode(&body[..19]).is_err());
    }

    #[test]
    fn header_new_encodes_what_ryll_sends() {
        // protocol 1, type MONITORS_CONFIG, opaque 0, size 28.
        assert_eq!(
            encode(&VdAgentMessageHeader::new(VD_AGENT_MONITORS_CONFIG, 28)),
            vec![1, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 28, 0, 0, 0]
        );
    }

    // --- MonConfig and MonitorMm tests ---

    #[test]
    fn mon_config_round_trips() {
        assert_round_trip(&MonConfig {
            height: 768,
            width: 1024,
            depth: 32,
            x: -1024,
            y: i32::MAX,
        });
    }

    #[test]
    fn mon_config_position_is_signed() {
        // VDAgentMonConfig: uint32 height, width, depth; int32 x, y.
        let mut body = Vec::new();
        for v in [600u32, 800, 32, 0xffff_fc00, 5] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        assert_eq!(
            MonConfig::decode(&body).expect("decodes"),
            MonConfig {
                height: 600,
                width: 800,
                depth: 32,
                x: -1024,
                y: 5,
            }
        );
        assert!(MonConfig::decode(&body[..19]).is_err());
    }

    #[test]
    fn monitor_mm_round_trips() {
        assert_round_trip(&MonitorMm {
            height: 300,
            width: u16::MAX,
        });
        // VDAgentMonitorMM: uint16 height, uint16 width.
        assert_eq!(
            MonitorMm::decode(&[0x2c, 0x01, 0x10, 0x02]).expect("decodes"),
            MonitorMm {
                height: 300,
                width: 528,
            }
        );
        assert!(MonitorMm::decode(&[0x2c, 0x01, 0x10]).is_err());
    }

    // --- MonitorsConfig tests ---

    fn two_monitors() -> Vec<MonConfig> {
        vec![
            MonConfig {
                height: 1080,
                width: 1920,
                depth: 32,
                x: 0,
                y: 0,
            },
            MonConfig {
                height: 1080,
                width: 1920,
                depth: 32,
                x: 1920,
                y: -200,
            },
        ]
    }

    #[test]
    fn monitors_config_round_trips() {
        assert_round_trip(&MonitorsConfig {
            flags: 0,
            monitors: Vec::new(),
            physical_sizes: Vec::new(),
        });
        assert_round_trip(&MonitorsConfig {
            flags: VD_AGENT_CONFIG_MONITORS_FLAG_USE_POS,
            monitors: two_monitors(),
            physical_sizes: Vec::new(),
        });
        assert_round_trip(&MonitorsConfig {
            flags: VD_AGENT_CONFIG_MONITORS_FLAG_USE_POS
                | VD_AGENT_CONFIG_MONITORS_FLAG_PHYSICAL_SIZE,
            monitors: two_monitors(),
            physical_sizes: vec![
                MonitorMm {
                    height: 300,
                    width: 530,
                },
                MonitorMm::default(),
            ],
        });
        // Unknown flag bits survive.
        assert_round_trip(&MonitorsConfig {
            flags: 0x8000_0000,
            monitors: two_monitors(),
            physical_sizes: Vec::new(),
        });
    }

    #[test]
    fn monitors_config_decodes_vd_agent_h_layout() {
        // VDAgentMonitorsConfig: uint32 num_of_monitors, uint32 flags,
        // VDAgentMonConfig monitors[], then VDAgentMonitorMM[] because
        // flags has PHYSICAL_SIZE (2) as well as USE_POS (1).
        let mut body = Vec::new();
        for v in [1u32, 3, 768, 1024, 32, 0xffff_ffff, 0] {
            body.extend_from_slice(&v.to_le_bytes());
        }
        body.extend_from_slice(&[0x2c, 0x01, 0x10, 0x02]);
        body.push(0xff); // trailing bytes are ignored
        assert_eq!(
            MonitorsConfig::decode(&body).expect("decodes"),
            MonitorsConfig {
                flags: 3,
                monitors: vec![MonConfig {
                    height: 768,
                    width: 1024,
                    depth: 32,
                    x: -1,
                    y: 0,
                }],
                physical_sizes: vec![MonitorMm {
                    height: 300,
                    width: 528,
                }],
            }
        );
        // Without its physical-size table the same message is too short
        // for the monitor it declares.
        assert_eq!(
            MonitorsConfig::decode(&body[..28]),
            Err(LinkError::TooLarge {
                what: "num_of_monitors",
                value: 1,
                max: 0,
            })
        );
        assert!(MonitorsConfig::decode(&body[..7]).is_err());
    }

    #[test]
    fn monitors_config_count_beyond_body_is_error() {
        let mut body = u32::MAX.to_le_bytes().to_vec();
        body.extend_from_slice(&0u32.to_le_bytes());
        body.extend_from_slice(&[0; MonConfig::SIZE]);
        assert_eq!(
            MonitorsConfig::decode(&body),
            Err(LinkError::TooLarge {
                what: "num_of_monitors",
                value: u32::MAX as usize,
                max: 1,
            })
        );
    }

    #[test]
    fn monitors_config_writer_sizes_the_physical_table_to_the_monitors() {
        let short = MonitorsConfig {
            flags: VD_AGENT_CONFIG_MONITORS_FLAG_PHYSICAL_SIZE,
            monitors: two_monitors(),
            physical_sizes: vec![MonitorMm {
                height: 1,
                width: 2,
            }],
        };
        let back = MonitorsConfig::decode(&encode(&short)).expect("well formed");
        assert_eq!(
            back.physical_sizes,
            vec![
                MonitorMm {
                    height: 1,
                    width: 2,
                },
                MonitorMm::default(),
            ]
        );
        // With the flag clear no table is written, whatever the field holds.
        let clear = MonitorsConfig { flags: 0, ..short };
        assert_eq!(
            encode(&clear).len(),
            MonitorsConfig::HEADER_SIZE + 2 * MonConfig::SIZE
        );
    }

    // --- AnnounceCapabilities tests ---

    // Guest ANNOUNCE_CAPABILITIES body from ryll test sessions 013-015:
    // request=0, caps=0x00038de7 (bit 6, CLIPBOARD_SELECTION, set).
    const GUEST_CAPS: [u8; 8] = [0, 0, 0, 0, 0xe7, 0x8d, 0x03, 0x00];

    #[test]
    fn announce_capabilities_round_trips() {
        assert_round_trip(&AnnounceCapabilities::default());
        assert_round_trip(&AnnounceCapabilities {
            request: 1,
            caps: vec![0x0003_8de7, 0, u32::MAX],
        });
    }

    #[test]
    fn announce_capabilities_decodes_a_guest_announcement() {
        let caps = AnnounceCapabilities::decode(&GUEST_CAPS).expect("decodes");
        assert_eq!(
            caps,
            AnnounceCapabilities {
                request: 0,
                caps: vec![0x0003_8de7],
            }
        );
        assert!(caps.has_capability(VD_AGENT_CAP_CLIPBOARD_SELECTION));
        // Bit 3 (CLIPBOARD, the pre-by-demand protocol) is clear.
        assert!(!caps.has_capability(VD_AGENT_CAP_CLIPBOARD));
        // A bit in a word the agent did not send is clear, not a panic.
        assert!(!caps.has_capability(40));
        assert!(!caps.has_capability(u32::MAX));
    }

    #[test]
    fn announce_capabilities_word_count_comes_from_the_body() {
        // VD_AGENT_CAPS_SIZE_FROM_MSG_SIZE: (size - 4) / 4, so a trailing
        // partial word is not a word.
        let mut body = GUEST_CAPS.to_vec();
        body.extend_from_slice(&[0xff, 0xff]);
        assert_eq!(
            AnnounceCapabilities::decode(&body).expect("decodes").caps,
            vec![0x0003_8de7]
        );
        assert_eq!(
            AnnounceCapabilities::decode(&GUEST_CAPS[..6])
                .expect("decodes")
                .caps,
            Vec::<u32>::new()
        );
        // The request word is required.
        assert!(AnnounceCapabilities::decode(&[0, 0]).is_err());
    }

    #[test]
    fn announce_capabilities_sets_bits_across_words() {
        let mut caps = AnnounceCapabilities {
            request: 1,
            caps: Vec::new(),
        };
        for cap in [
            VD_AGENT_CAP_MOUSE_STATE,
            VD_AGENT_CAP_MONITORS_CONFIG,
            VD_AGENT_CAP_REPLY,
            VD_AGENT_CAP_CLIPBOARD_BY_DEMAND,
            VD_AGENT_CAP_CLIPBOARD_SELECTION,
        ] {
            caps.set_capability(cap);
        }
        // ryll's own announcement: request=1, caps=0x67.
        assert_eq!(encode(&caps), vec![1, 0, 0, 0, 0x67, 0, 0, 0]);
        caps.set_capability(33);
        assert_eq!(caps.caps, vec![0x67, 0x2]);
        assert!(caps.has_capability(33));
        assert!(!caps.has_capability(32));
    }

    #[test]
    fn announce_capabilities_ignores_a_capability_beyond_the_bound() {
        let mut caps = AnnounceCapabilities::default();
        caps.set_capability(AnnounceCapabilities::MAX_CAPABILITY - 1);
        assert_eq!(caps.caps.len(), 16);
        caps.set_capability(AnnounceCapabilities::MAX_CAPABILITY);
        caps.set_capability(u32::MAX);
        assert_eq!(caps.caps.len(), 16);
        assert!(!caps.has_capability(u32::MAX));
    }

    // --- VdAgentReply tests ---

    #[test]
    fn vd_agent_reply_round_trips() {
        assert_round_trip(&VdAgentReply {
            reply_type: VD_AGENT_MONITORS_CONFIG,
            error: VD_AGENT_SUCCESS,
        });
        assert_round_trip(&VdAgentReply {
            reply_type: u32::MAX,
            error: u32::MAX,
        });
    }

    #[test]
    fn vd_agent_reply_decodes_vd_agent_h_layout() {
        // VDAgentReply: uint32 type, uint32 error; trailing bytes ignored.
        let body = [0x02, 0, 0, 0, 0x01, 0, 0, 0, 0xff, 0xff];
        assert_eq!(
            VdAgentReply::decode(&body).expect("decodes"),
            VdAgentReply {
                reply_type: VD_AGENT_MONITORS_CONFIG,
                error: VD_AGENT_SUCCESS,
            }
        );
        assert!(VdAgentReply::decode(&body[..7]).is_err());
        assert!(VdAgentReply::decode(&[]).is_err());
    }

    #[test]
    fn vd_agent_reply_success_is_one() {
        let reply = |error| VdAgentReply {
            reply_type: VD_AGENT_MONITORS_CONFIG,
            error,
        };
        assert!(reply(VD_AGENT_SUCCESS).is_success());
        assert!(!reply(VD_AGENT_ERROR).is_success());
        // vd_agent.h has no zero value; zero is not success.
        assert!(!reply(0).is_success());
    }

    // --- Clipboard message tests ---

    #[test]
    fn clipboard_messages_round_trip_with_the_selection_header() {
        for selection in [
            VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
            VD_AGENT_CLIPBOARD_SELECTION_PRIMARY,
            0xff,
        ] {
            assert_round_trip_with(
                &ClipboardGrab {
                    selection,
                    types: vec![VD_AGENT_CLIPBOARD_IMAGE_PNG, VD_AGENT_CLIPBOARD_UTF8_TEXT],
                },
                true,
            );
            assert_round_trip_with(
                &ClipboardGrab {
                    selection,
                    types: Vec::new(),
                },
                true,
            );
            assert_round_trip_with(
                &ClipboardRequest {
                    selection,
                    clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
                },
                true,
            );
            assert_round_trip_with(
                &Clipboard {
                    selection,
                    clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
                    data: b"hello".to_vec(),
                },
                true,
            );
            assert_round_trip_with(&ClipboardRelease { selection }, true);
        }
    }

    #[test]
    fn clipboard_messages_round_trip_without_the_selection_header() {
        // Without the capability the selection is implicitly CLIPBOARD,
        // the only selection that can round-trip.
        let selection = VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD;
        assert_round_trip_with(
            &ClipboardGrab {
                selection,
                types: Vec::new(),
            },
            false,
        );
        assert_round_trip_with(
            &ClipboardGrab {
                selection,
                types: vec![VD_AGENT_CLIPBOARD_UTF8_TEXT],
            },
            false,
        );
        assert_round_trip_with(
            &ClipboardRequest {
                selection,
                clipboard_type: VD_AGENT_CLIPBOARD_IMAGE_BMP,
            },
            false,
        );
        assert_round_trip_with(
            &Clipboard {
                selection,
                clipboard_type: VD_AGENT_CLIPBOARD_NONE,
                data: Vec::new(),
            },
            false,
        );
        assert_round_trip_with(&ClipboardRelease { selection }, false);
    }

    #[test]
    fn clipboard_writers_produce_the_bytes_ryll_sends() {
        // The CLIPBOARD request ryll sent in sessions 013 and 014.
        let request = ClipboardRequest {
            selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
            clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
        };
        assert_eq!(encode_with(&request, true), vec![0, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(encode_with(&request, false), vec![1, 0, 0, 0]);
        // A one-type grab has the same layout as the request.
        let grab = ClipboardGrab {
            selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
            types: vec![VD_AGENT_CLIPBOARD_UTF8_TEXT],
        };
        assert_eq!(encode_with(&grab, true), vec![0, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(encode_with(&grab, false), vec![1, 0, 0, 0]);
        // A NONE answer for PRIMARY carries that selection back.
        let none = Clipboard {
            selection: VD_AGENT_CLIPBOARD_SELECTION_PRIMARY,
            clipboard_type: VD_AGENT_CLIPBOARD_NONE,
            data: Vec::new(),
        };
        assert_eq!(encode_with(&none, true), vec![1, 0, 0, 0, 0, 0, 0, 0]);
        // Without the cap there is no selection header at all.
        let data = Clipboard {
            selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
            clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
            data: b"x".to_vec(),
        };
        assert_eq!(encode_with(&data, false), vec![1, 0, 0, 0, b'x']);
        assert_eq!(encode_with(&data, true), vec![0, 0, 0, 0, 1, 0, 0, 0, b'x']);
        let release = ClipboardRelease {
            selection: VD_AGENT_CLIPBOARD_SELECTION_SECONDARY,
        };
        assert_eq!(encode_with(&release, true), vec![2, 0, 0, 0]);
        assert_eq!(encode_with(&release, false), Vec::<u8>::new());
    }

    #[test]
    fn guest_primary_grab_from_sessions_013_014_is_not_clipboard() {
        // Wire bytes of the guest grab ryll answered with a CLIPBOARD
        // request: selection=PRIMARY (1), types=[UTF8_TEXT].
        let grab = [0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00];
        let grab = ClipboardGrab::decode_with(&grab, true).expect("decodes");
        assert_eq!(grab.selection, VD_AGENT_CLIPBOARD_SELECTION_PRIMARY);
        assert!(grab.offers(VD_AGENT_CLIPBOARD_UTF8_TEXT));
    }

    #[test]
    fn selection_header_is_a_u8_then_reserved_bytes() {
        // Reserved bytes are not part of the selection, whatever they hold.
        let msg = [0x00, 0xaa, 0xbb, 0xcc, 0x01, 0x00, 0x00, 0x00];
        assert_eq!(
            ClipboardRequest::decode_with(&msg, true).expect("decodes"),
            ClipboardRequest {
                selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
                clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
            }
        );
        // A header cut short fails every clipboard message.
        let short = [0x00, 0x00, 0x00];
        assert!(ClipboardGrab::decode_with(&short, true).is_err());
        assert!(ClipboardRequest::decode_with(&short, true).is_err());
        assert!(Clipboard::decode_with(&short, true).is_err());
        assert!(ClipboardRelease::decode_with(&short, true).is_err());
    }

    #[test]
    fn without_the_cap_the_selection_is_implicitly_clipboard() {
        // The first four bytes are the type, not a selection header.
        let msg = [0x01, 0x00, 0x00, 0x00];
        assert_eq!(
            ClipboardRequest::decode_with(&msg, false).expect("decodes"),
            ClipboardRequest {
                selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
                clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
            }
        );
        assert_eq!(
            ClipboardRelease::decode_with(&[], false).expect("decodes"),
            ClipboardRelease {
                selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
            }
        );
    }

    #[test]
    fn clipboard_grab_finds_a_type_anywhere_in_the_list() {
        // types = [IMAGE_PNG (2), UTF8_TEXT (1)]
        let types = [0x02, 0, 0, 0, 0x01, 0, 0, 0];
        let grab = ClipboardGrab::decode_with(&types, false).expect("decodes");
        assert!(grab.offers(VD_AGENT_CLIPBOARD_UTF8_TEXT));
        let first = ClipboardGrab::decode_with(&types[..4], false).expect("decodes");
        assert!(!first.offers(VD_AGENT_CLIPBOARD_UTF8_TEXT));
        // A trailing partial type is ignored rather than misread.
        let partial = ClipboardGrab::decode_with(&[0x01, 0, 0], false).expect("decodes");
        assert_eq!(partial.types, Vec::<u32>::new());
    }

    #[test]
    fn clipboard_reads_type_and_data() {
        let body = [0x01, 0, 0, 0, b'h', b'i'];
        assert_eq!(
            Clipboard::decode_with(&body, false).expect("decodes"),
            Clipboard {
                selection: VD_AGENT_CLIPBOARD_SELECTION_CLIPBOARD,
                clipboard_type: VD_AGENT_CLIPBOARD_UTF8_TEXT,
                data: b"hi".to_vec(),
            }
        );
        // The empty NONE reply the guest sends for a selection it does
        // not own (sessions 013 and 014).
        assert_eq!(
            Clipboard::decode_with(&[1, 0, 0, 0, 0, 0, 0, 0], true).expect("decodes"),
            Clipboard {
                selection: VD_AGENT_CLIPBOARD_SELECTION_PRIMARY,
                clipboard_type: VD_AGENT_CLIPBOARD_NONE,
                data: Vec::new(),
            }
        );
        // The type is required.
        assert!(Clipboard::decode_with(&[0x01, 0], false).is_err());
        assert!(ClipboardRequest::decode_with(&[0, 0, 0, 0, 0x01, 0], true).is_err());
    }
}
