//! Mux protocol for the SPICE WebDAV channel.
//!
//! The WebDAV channel multiplexes multiple HTTP client connections
//! over a single SpiceVMC byte stream. Each chunk of data is framed
//! as:
//!
//!   client_id:  i64 LE  (8 bytes)
//!   data_size:  u16 LE  (2 bytes)
//!   data:       [u8; data_size]
//!
//! A data_size of 0 signals client disconnection.

/// Header size: 8 bytes client_id + 2 bytes data_size.
const MUX_HEADER_SIZE: usize = 10;

/// Maximum payload per mux frame (u16::MAX).
pub const MAX_MUX_SIZE: usize = u16::MAX as usize;

/// A parsed mux frame from the guest.
#[derive(Debug, PartialEq)]
pub struct MuxFrame {
    pub client_id: i64,
    pub data: Vec<u8>,
}

/// Accumulates raw bytes from the VMC channel and extracts
/// complete mux frames. Handles frames that span multiple
/// VMC messages or multiple frames packed in one message.
pub struct MuxDemuxer {
    buf: Vec<u8>,
}

impl Default for MuxDemuxer {
    fn default() -> Self {
        Self::new()
    }
}

impl MuxDemuxer {
    pub fn new() -> Self {
        MuxDemuxer {
            buf: Vec::with_capacity(MAX_MUX_SIZE + MUX_HEADER_SIZE),
        }
    }

    /// Append raw bytes from the VMC channel.
    pub fn feed(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Try to extract the next complete mux frame.
    /// Returns `None` if the buffer doesn't contain a
    /// complete frame yet.
    pub fn next_frame(&mut self) -> Option<MuxFrame> {
        if self.buf.len() < MUX_HEADER_SIZE {
            return None;
        }

        let client_id =
            i64::from_le_bytes(self.buf[0..8].try_into().expect("length checked above"));
        let data_size =
            u16::from_le_bytes(self.buf[8..10].try_into().expect("length checked above")) as usize;
        let total_size = MUX_HEADER_SIZE + data_size;

        if self.buf.len() < total_size {
            return None;
        }

        let data = self.buf[MUX_HEADER_SIZE..total_size].to_vec();
        self.buf.drain(..total_size);

        Some(MuxFrame { client_id, data })
    }

    /// Number of buffered bytes not yet consumed.
    #[allow(dead_code)]
    pub fn buffered(&self) -> usize {
        self.buf.len()
    }
}

/// Serialise a mux frame for sending to the guest.
///
/// # Panics
///
/// Panics if `data.len() > MAX_MUX_SIZE`.
pub fn encode_mux_frame(client_id: i64, data: &[u8]) -> Vec<u8> {
    assert!(
        data.len() <= MAX_MUX_SIZE,
        "mux frame payload {} exceeds maximum {}",
        data.len(),
        MAX_MUX_SIZE,
    );
    let mut buf = Vec::with_capacity(MUX_HEADER_SIZE + data.len());
    buf.extend_from_slice(&client_id.to_le_bytes());
    buf.extend_from_slice(&(data.len() as u16).to_le_bytes());
    buf.extend_from_slice(data);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_single_frame() {
        let mut demuxer = MuxDemuxer::new();
        let raw = encode_mux_frame(42, b"hello");
        demuxer.feed(&raw);

        let frame = demuxer.next_frame().unwrap();
        assert_eq!(frame.client_id, 42);
        assert_eq!(frame.data, b"hello");
        assert!(demuxer.next_frame().is_none());
    }

    #[test]
    fn parse_zero_length_frame() {
        let mut demuxer = MuxDemuxer::new();
        let raw = encode_mux_frame(7, &[]);
        demuxer.feed(&raw);

        let frame = demuxer.next_frame().unwrap();
        assert_eq!(frame.client_id, 7);
        assert!(frame.data.is_empty());
        assert!(demuxer.next_frame().is_none());
    }

    #[test]
    fn parse_multiple_frames_one_feed() {
        let mut demuxer = MuxDemuxer::new();
        let mut raw = encode_mux_frame(1, b"aaa");
        raw.extend_from_slice(&encode_mux_frame(2, b"bbb"));
        raw.extend_from_slice(&encode_mux_frame(3, b"ccc"));
        demuxer.feed(&raw);

        let f1 = demuxer.next_frame().unwrap();
        assert_eq!(f1.client_id, 1);
        assert_eq!(f1.data, b"aaa");

        let f2 = demuxer.next_frame().unwrap();
        assert_eq!(f2.client_id, 2);
        assert_eq!(f2.data, b"bbb");

        let f3 = demuxer.next_frame().unwrap();
        assert_eq!(f3.client_id, 3);
        assert_eq!(f3.data, b"ccc");

        assert!(demuxer.next_frame().is_none());
    }

    #[test]
    fn parse_incremental_split_in_header() {
        let mut demuxer = MuxDemuxer::new();
        let raw = encode_mux_frame(99, b"data!");

        // Feed 4 bytes (mid-header)
        demuxer.feed(&raw[..4]);
        assert!(demuxer.next_frame().is_none());

        // Feed the rest
        demuxer.feed(&raw[4..]);
        let frame = demuxer.next_frame().unwrap();
        assert_eq!(frame.client_id, 99);
        assert_eq!(frame.data, b"data!");
    }

    #[test]
    fn parse_incremental_split_in_payload() {
        let mut demuxer = MuxDemuxer::new();
        let payload = b"longer payload data here";
        let raw = encode_mux_frame(55, payload);

        // Feed header + partial payload
        demuxer.feed(&raw[..12]);
        assert!(demuxer.next_frame().is_none());

        // Feed rest of payload
        demuxer.feed(&raw[12..]);
        let frame = demuxer.next_frame().unwrap();
        assert_eq!(frame.client_id, 55);
        assert_eq!(frame.data, payload);
    }

    #[test]
    fn parse_incremental_byte_by_byte() {
        let mut demuxer = MuxDemuxer::new();
        let raw = encode_mux_frame(11, b"hi");

        for (i, &byte) in raw.iter().enumerate() {
            demuxer.feed(&[byte]);
            if i < raw.len() - 1 {
                assert!(
                    demuxer.next_frame().is_none(),
                    "premature frame at byte {}",
                    i
                );
            }
        }

        let frame = demuxer.next_frame().unwrap();
        assert_eq!(frame.client_id, 11);
        assert_eq!(frame.data, b"hi");
    }

    #[test]
    fn empty_buffer_returns_none() {
        let mut demuxer = MuxDemuxer::new();
        assert!(demuxer.next_frame().is_none());
    }

    #[test]
    fn partial_header_returns_none() {
        let mut demuxer = MuxDemuxer::new();
        demuxer.feed(&[0u8; 9]); // one byte short of header
        assert!(demuxer.next_frame().is_none());
    }

    #[test]
    fn negative_client_id() {
        let mut demuxer = MuxDemuxer::new();
        let raw = encode_mux_frame(-1, b"neg");
        demuxer.feed(&raw);

        let frame = demuxer.next_frame().unwrap();
        assert_eq!(frame.client_id, -1);
        assert_eq!(frame.data, b"neg");
    }

    #[test]
    fn zero_client_id() {
        let mut demuxer = MuxDemuxer::new();
        let raw = encode_mux_frame(0, b"zero");
        demuxer.feed(&raw);

        let frame = demuxer.next_frame().unwrap();
        assert_eq!(frame.client_id, 0);
        assert_eq!(frame.data, b"zero");
    }

    #[test]
    fn max_size_frame() {
        let mut demuxer = MuxDemuxer::new();
        let payload = vec![0xABu8; MAX_MUX_SIZE];
        let raw = encode_mux_frame(123, &payload);

        assert_eq!(raw.len(), MUX_HEADER_SIZE + MAX_MUX_SIZE);

        demuxer.feed(&raw);
        let frame = demuxer.next_frame().unwrap();
        assert_eq!(frame.client_id, 123);
        assert_eq!(frame.data.len(), MAX_MUX_SIZE);
        assert!(frame.data.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn round_trip() {
        let original = MuxFrame {
            client_id: 0x7FFFFFFFFFFFFFFF,
            data: b"round trip test".to_vec(),
        };

        let encoded = encode_mux_frame(original.client_id, &original.data);
        let mut demuxer = MuxDemuxer::new();
        demuxer.feed(&encoded);
        let parsed = demuxer.next_frame().unwrap();

        assert_eq!(parsed, original);
    }

    #[test]
    fn buffered_tracks_remaining() {
        let mut demuxer = MuxDemuxer::new();
        assert_eq!(demuxer.buffered(), 0);

        let raw = encode_mux_frame(1, b"abc");
        demuxer.feed(&raw);
        assert_eq!(demuxer.buffered(), MUX_HEADER_SIZE + 3);

        demuxer.next_frame().unwrap();
        assert_eq!(demuxer.buffered(), 0);
    }

    #[test]
    fn interleaved_frames_then_partial() {
        let mut demuxer = MuxDemuxer::new();
        let f1 = encode_mux_frame(10, b"first");
        let f2 = encode_mux_frame(20, b"second");

        // Feed both frames plus 5 bytes of a third
        let mut combined = f1;
        combined.extend_from_slice(&f2);
        combined.extend_from_slice(&[0u8; 5]); // partial header
        demuxer.feed(&combined);

        assert_eq!(demuxer.next_frame().unwrap().client_id, 10);
        assert_eq!(demuxer.next_frame().unwrap().client_id, 20);
        assert!(demuxer.next_frame().is_none());
        assert_eq!(demuxer.buffered(), 5);
    }

    #[test]
    #[should_panic(expected = "mux frame payload")]
    fn encode_oversized_panics() {
        let payload = vec![0u8; MAX_MUX_SIZE + 1];
        encode_mux_frame(1, &payload);
    }
}
