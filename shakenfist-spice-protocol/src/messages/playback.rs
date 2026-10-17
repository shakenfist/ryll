//! Playback channel messages.
//!
//! Layouts follow spice-common's `spice.proto`, `channel PlaybackChannel`,
//! with the shared `AudioVolume` and `AudioMute` messages. Every message is
//! server to client: the channel has no client messages beyond the common
//! ack, ack-sync and pong. `STOP` has an empty body and needs no type.
//!
//! The data mode in [`PlaybackMode`] is an `audio_data_mode::*` value and
//! the format in [`PlaybackStart`] an `audio_fmt::*` value; the caps that
//! govern which messages a server sends are `capabilities::PLAYBACK_*`.

use super::WireType;
use crate::reader::{BoundedReader, LinkError};

/// `SPICE_MSG_PLAYBACK_START` (server to client): a stream of audio
/// begins, in this shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackStart {
    pub channels: u32,
    /// An `audio_fmt::*` value.
    pub format: u16,
    /// Samples a second, per channel.
    pub frequency: u32,
    /// The multimedia time of the stream's first sample, in milliseconds.
    pub time: u32,
}

impl PlaybackStart {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 14;
}

impl WireType for PlaybackStart {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(PlaybackStart {
            channels: r.read_u32()?,
            format: r.read_u16()?,
            frequency: r.read_u32()?,
            time: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.channels.to_le_bytes());
        out.extend_from_slice(&self.format.to_le_bytes());
        out.extend_from_slice(&self.frequency.to_le_bytes());
        out.extend_from_slice(&self.time.to_le_bytes());
    }
}

/// `SPICE_MSG_PLAYBACK_MODE` (server to client): the data mode of the
/// `DATA` messages that follow.
///
/// spice.proto gives the message a trailing `uint8 data[]`, which no
/// mode in use carries. The writer sends none and the reader ignores
/// any, as it ignores trailing bytes on every type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackMode {
    /// The multimedia time, in milliseconds.
    pub time: u32,
    /// An `audio_data_mode::*` value.
    pub mode: u16,
}

impl PlaybackMode {
    /// The size on the wire, in bytes, with no trailing data.
    pub const SIZE: usize = 6;
}

impl WireType for PlaybackMode {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(PlaybackMode {
            time: r.read_u32()?,
            mode: r.read_u16()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.time.to_le_bytes());
        out.extend_from_slice(&self.mode.to_le_bytes());
    }
}

/// `SPICE_MSG_PLAYBACK_DATA` (server to client, spice.proto's
/// `SpiceMsgPlaybackPacket`): the multimedia time of a frame, then the
/// frame in the current [`PlaybackMode`], which runs to the end of the
/// message. The data has no length field, so the body bounds it and the
/// reader never allocates more than the body holds. Empty data is valid.
///
/// [`PlaybackPacketRef`] is the same message with the data borrowed from
/// the body, for a reader that need not copy it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackPacket {
    /// The multimedia time of the frame's first sample, in milliseconds.
    pub time: u32,
    pub data: Vec<u8>,
}

impl PlaybackPacket {
    /// The size of the time field before the data.
    pub const HEADER_SIZE: usize = 4;
}

/// A [`PlaybackPacket`] whose data borrows the message body.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackPacketRef<'a> {
    pub time: u32,
    pub data: &'a [u8],
}

impl<'a> PlaybackPacketRef<'a> {
    /// Read the time and take the rest of the body as data, without
    /// copying it. [`PlaybackPacket::read`] is this plus the copy.
    ///
    /// # Errors
    ///
    /// [`LinkError::Truncated`] if the body is shorter than the time.
    pub fn read(r: &mut BoundedReader<'a>) -> Result<Self, LinkError> {
        let time = r.read_u32()?;
        let data = r.read_bytes(r.remaining())?;
        Ok(PlaybackPacketRef { time, data })
    }

    /// Parse a whole message body without copying its data.
    ///
    /// # Errors
    ///
    /// As [`read`](Self::read).
    pub fn decode(body: &'a [u8]) -> Result<Self, LinkError> {
        Self::read(&mut BoundedReader::new(body))
    }
}

impl From<PlaybackPacketRef<'_>> for PlaybackPacket {
    fn from(packet: PlaybackPacketRef<'_>) -> Self {
        PlaybackPacket {
            time: packet.time,
            data: packet.data.to_vec(),
        }
    }
}

impl WireType for PlaybackPacket {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        PlaybackPacketRef::read(r).map(PlaybackPacket::from)
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.time.to_le_bytes());
        out.extend_from_slice(&self.data);
    }
}

/// `SPICE_MSG_PLAYBACK_VOLUME` (server to client, spice.proto's
/// `AudioVolume`): the desktop's volume for each channel, as a `u8`
/// count and that many `u16`s.
///
/// spice-server sends it only to a client with
/// `capabilities::PLAYBACK_VOLUME`. A count of zero is valid on the wire
/// and reads as no volumes; spice-gtk ignores such a message, since it
/// applies only `volume[0]`.
///
/// The wire's count is a `u8`, so at most 255 volumes fit. Build one with
/// [`PlaybackVolume::new`], which refuses more; `write` sends only the
/// first 255 of a longer list built directly, so the count always
/// matches the volumes that follow it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlaybackVolume {
    pub volumes: Vec<u16>,
}

impl PlaybackVolume {
    /// The size of a body with no volumes: the count alone.
    pub const MIN_SIZE: usize = 1;
    /// The most volumes the `u8` count can describe.
    pub const MAX_CHANNELS: usize = u8::MAX as usize;

    /// A volume message, refusing more volumes than the wire's count can
    /// describe.
    ///
    /// # Errors
    ///
    /// [`LinkError::TooLarge`] if `volumes` has more than
    /// [`MAX_CHANNELS`](Self::MAX_CHANNELS) entries.
    pub fn new(volumes: Vec<u16>) -> Result<Self, LinkError> {
        if volumes.len() > Self::MAX_CHANNELS {
            return Err(LinkError::TooLarge {
                what: "nchannels",
                value: volumes.len(),
                max: Self::MAX_CHANNELS,
            });
        }
        Ok(PlaybackVolume { volumes })
    }
}

impl WireType for PlaybackVolume {
    /// # Errors
    ///
    /// [`LinkError::TooLarge`] if `nchannels` exceeds what the rest of the
    /// body can hold, and [`LinkError::Truncated`] if the body is empty.
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        let nchannels = r.read_u8()? as usize;
        r.check_count("nchannels", nchannels, 2)?;
        let mut volumes = Vec::with_capacity(nchannels);
        for _ in 0..nchannels {
            volumes.push(r.read_u16()?);
        }
        Ok(PlaybackVolume { volumes })
    }

    /// Writes at most [`MAX_CHANNELS`](PlaybackVolume::MAX_CHANNELS)
    /// volumes, so the count always matches the volumes that follow it;
    /// [`PlaybackVolume::new`] refuses a longer list.
    fn write(&self, out: &mut Vec<u8>) {
        debug_assert!(
            self.volumes.len() <= Self::MAX_CHANNELS,
            "a PlaybackVolume has at most 255 volumes"
        );
        let volumes = &self.volumes[..self.volumes.len().min(Self::MAX_CHANNELS)];
        out.push(volumes.len() as u8);
        for v in volumes {
            out.extend_from_slice(&v.to_le_bytes());
        }
    }
}

/// `SPICE_MSG_PLAYBACK_MUTE` (server to client, spice.proto's
/// `AudioMute`): whether the desktop's output is muted. The wire's `u8`
/// reads as muted when non-zero, and is written as 1 or 0.
///
/// spice-server sends it only to a client with
/// `capabilities::PLAYBACK_VOLUME`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackMute {
    pub mute: bool,
}

impl PlaybackMute {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 1;
}

impl WireType for PlaybackMute {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(PlaybackMute {
            mute: r.read_u8()? != 0,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.push(u8::from(self.mute));
    }
}

/// `SPICE_MSG_PLAYBACK_LATENCY` (server to client): the latency the
/// server asks the client to buffer, in milliseconds. spice-server has
/// not sent it since v0.16.0.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaybackLatency {
    pub latency_ms: u32,
}

impl PlaybackLatency {
    /// The size on the wire, in bytes.
    pub const SIZE: usize = 4;
}

impl WireType for PlaybackLatency {
    fn read(r: &mut BoundedReader<'_>) -> Result<Self, LinkError> {
        Ok(PlaybackLatency {
            latency_ms: r.read_u32()?,
        })
    }

    fn write(&self, out: &mut Vec<u8>) {
        out.extend_from_slice(&self.latency_ms.to_le_bytes());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::messages::assert_round_trip;
    use crate::{audio_data_mode, audio_fmt};

    fn encode<T: WireType>(value: &T) -> Vec<u8> {
        let mut out = Vec::new();
        value.write(&mut out);
        out
    }

    // --- PlaybackStart ---

    #[test]
    fn start_round_trips() {
        assert_round_trip(&PlaybackStart {
            channels: 2,
            format: audio_fmt::S16,
            frequency: 48_000,
            time: u32::MAX,
        });
    }

    #[test]
    fn start_decodes_spice_proto_layout() {
        // spice.proto PlaybackChannel `start`: uint32 channels,
        // audio_fmt format (enum16), uint32 frequency, uint32 time.
        let body = [
            0x02, 0x00, 0x00, 0x00, // channels
            0x01, 0x00, // format S16
            0x80, 0xbb, 0x00, 0x00, // frequency 48000
            0x78, 0x56, 0x34, 0x12, // time
            0xff, // trailing bytes are ignored
        ];
        let start = PlaybackStart::decode(&body).expect("decodes");
        assert_eq!(
            start,
            PlaybackStart {
                channels: 2,
                format: audio_fmt::S16,
                frequency: 48_000,
                time: 0x1234_5678,
            }
        );
        assert_eq!(encode(&start), body[..PlaybackStart::SIZE]);
        assert!(PlaybackStart::decode(&body[..PlaybackStart::SIZE - 1]).is_err());
        assert!(PlaybackStart::decode(&[]).is_err());
    }

    // --- PlaybackMode ---

    #[test]
    fn mode_round_trips() {
        assert_round_trip(&PlaybackMode {
            time: 7,
            mode: audio_data_mode::OPUS,
        });
    }

    #[test]
    fn mode_decodes_spice_proto_layout_and_ignores_its_data() {
        // spice.proto PlaybackChannel `mode`: uint32 time, audio_data_mode
        // mode (enum16), then uint8 data[], which the reader ignores.
        let body = [0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0xaa, 0xbb, 0xcc];
        let mode = PlaybackMode::decode(&body).expect("decodes");
        assert_eq!(
            mode,
            PlaybackMode {
                time: 16,
                mode: audio_data_mode::RAW,
            }
        );
        // The writer sends no data.
        assert_eq!(encode(&mode), body[..PlaybackMode::SIZE]);
        assert!(PlaybackMode::decode(&body[..5]).is_err());
    }

    // --- PlaybackPacket ---

    #[test]
    fn packet_round_trips() {
        assert_round_trip(&PlaybackPacket {
            time: 1,
            data: Vec::new(),
        });
        assert_round_trip(&PlaybackPacket {
            time: 0x8000_0000,
            data: vec![0xfc, 0xff, 0xfe, 1, 2, 3],
        });
    }

    #[test]
    fn packet_decodes_spice_proto_layout() {
        // spice.proto PlaybackChannel `data`: uint32 time, then uint8
        // data[] to the end of the message.
        let body = [0x04, 0x03, 0x02, 0x01, 0xde, 0xad, 0xbe, 0xef];
        let packet = PlaybackPacketRef::decode(&body).expect("decodes");
        assert_eq!(packet.time, 0x0102_0304);
        assert_eq!(packet.data, &[0xde, 0xad, 0xbe, 0xef]);
        assert_eq!(
            PlaybackPacket::decode(&body).expect("decodes"),
            PlaybackPacket::from(packet)
        );
        // A time with no data is a valid, empty packet.
        assert_eq!(
            PlaybackPacketRef::decode(&body[..4]).expect("decodes").data,
            &[] as &[u8]
        );
        assert!(PlaybackPacketRef::decode(&body[..3]).is_err());
        assert!(PlaybackPacket::decode(&[]).is_err());
    }

    // --- PlaybackVolume ---

    #[test]
    fn volume_round_trips() {
        assert_round_trip(&PlaybackVolume {
            volumes: Vec::new(),
        });
        assert_round_trip(&PlaybackVolume {
            volumes: vec![0, 0x7fff, u16::MAX],
        });
        assert_round_trip(&PlaybackVolume {
            volumes: vec![1; PlaybackVolume::MAX_CHANNELS],
        });
    }

    #[test]
    fn volume_decodes_spice_proto_layout() {
        // spice.proto `AudioVolume`: uint8 nchannels, then uint16
        // volume[nchannels].
        let body = [0x02, 0x34, 0x12, 0xff, 0xff, 0x99];
        let volume = PlaybackVolume::decode(&body).expect("decodes");
        assert_eq!(volume.volumes, vec![0x1234, 0xffff]);
        assert_eq!(encode(&volume), body[..5]);
        // A count of zero is accepted.
        assert_eq!(
            PlaybackVolume::decode(&[0]).expect("decodes").volumes,
            Vec::<u16>::new()
        );
    }

    #[test]
    fn volume_rejects_short_bodies() {
        assert!(PlaybackVolume::decode(&[]).is_err());
        assert_eq!(
            PlaybackVolume::decode(&[0x02, 0x34, 0x12, 0xff]),
            Err(LinkError::TooLarge {
                what: "nchannels",
                value: 2,
                max: 1,
            })
        );
    }

    #[test]
    fn volume_new_refuses_more_channels_than_the_count_holds() {
        assert!(PlaybackVolume::new(vec![0; PlaybackVolume::MAX_CHANNELS]).is_ok());
        assert_eq!(
            PlaybackVolume::new(vec![0; 256]),
            Err(LinkError::TooLarge {
                what: "nchannels",
                value: 256,
                max: 255,
            })
        );
    }

    // --- PlaybackMute ---

    #[test]
    fn mute_round_trips() {
        assert_round_trip(&PlaybackMute { mute: true });
        assert_round_trip(&PlaybackMute { mute: false });
    }

    #[test]
    fn mute_decodes_spice_proto_layout() {
        // spice.proto `AudioMute`: uint8 mute, non-zero meaning muted.
        let decode = |b: &[u8]| PlaybackMute::decode(b).ok().map(|m| m.mute);
        assert_eq!(decode(&[0]), Some(false));
        assert_eq!(decode(&[1]), Some(true));
        assert_eq!(decode(&[2, 0xff]), Some(true));
        assert_eq!(decode(&[]), None);
        assert_eq!(encode(&PlaybackMute { mute: true }), vec![1]);
    }

    // --- PlaybackLatency ---

    #[test]
    fn latency_round_trips() {
        assert_round_trip(&PlaybackLatency { latency_ms: 0 });
        assert_round_trip(&PlaybackLatency {
            latency_ms: u32::MAX,
        });
    }

    #[test]
    fn latency_decodes_spice_proto_layout() {
        // spice.proto PlaybackChannel `latency`: uint32 latency_ms.
        assert_eq!(
            PlaybackLatency::decode(&[0xc8, 0x00, 0x00, 0x00, 0x01]).expect("decodes"),
            PlaybackLatency { latency_ms: 200 }
        );
        assert!(PlaybackLatency::decode(&[0xc8, 0x00, 0x00]).is_err());
    }
}
