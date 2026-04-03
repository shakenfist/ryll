/// Bug report infrastructure — per-channel traffic ring buffer.
///
/// Always active regardless of `--capture`.  Retains the most
/// recent protocol traffic for bug report export and live
/// traffic viewer display.
use std::collections::VecDeque;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use tracing::debug;

#[cfg(feature = "capture")]
use crate::capture;

/// Direction of a protocol message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrafficDirection {
    Sent,
    Received,
}

/// A single protocol message recorded in the ring buffer.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields read in later phases (traffic viewer, bug report zip)
pub struct TrafficEntry {
    /// Time elapsed since session start.
    pub timestamp: Duration,
    /// Which channel this message belongs to.
    pub channel: &'static str,
    /// Sent by the client or received from the server.
    pub direction: TrafficDirection,
    /// SPICE message type ID (from the 6-byte mini-header).
    pub message_type: u16,
    /// Human-readable message name (e.g. "draw_copy").
    pub message_name: &'static str,
    /// Total wire size including the 6-byte header.
    pub wire_size: u32,
    /// Payload size (wire_size minus 6-byte header).
    pub payload_size: u32,
    /// Full pcap frame bytes (Ethernet + IP + TCP + SPICE
    /// payload).  Used by `drain_to_pcap()` for bug report
    /// export.
    pub pcap_frame: Vec<u8>,
}

/// Per-channel ring buffer of recent protocol traffic.
pub struct TrafficRingBuffer {
    /// Ring of entries, newest at the back.
    entries: VecDeque<TrafficEntry>,
    /// Current total byte count of all pcap_frame data.
    total_bytes: usize,
    /// Maximum byte count before eviction.
    max_bytes: usize,
    /// TCP sequence number for client→server direction.
    client_seq: u32,
    /// TCP sequence number for server→client direction.
    server_seq: u32,
}

impl TrafficRingBuffer {
    /// Create a new ring buffer with the given byte cap.
    pub fn new(max_bytes: usize) -> Self {
        TrafficRingBuffer {
            entries: VecDeque::new(),
            total_bytes: 0,
            max_bytes,
            client_seq: 1000,
            server_seq: 2000,
        }
    }

    /// Push a new entry, evicting oldest entries if the byte
    /// cap would be exceeded.
    pub fn push(&mut self, entry: TrafficEntry) {
        self.total_bytes += entry.pcap_frame.len();
        self.entries.push_back(entry);
        while self.total_bytes > self.max_bytes {
            if let Some(old) = self.entries.pop_front() {
                self.total_bytes -= old.pcap_frame.len();
            } else {
                break;
            }
        }
    }

    /// Return a reference to all buffered entries (oldest first).
    #[allow(dead_code)] // used in later phases (traffic viewer, bug report zip)
    pub fn entries(&self) -> &VecDeque<TrafficEntry> {
        &self.entries
    }

    /// Write all buffered pcap frames to a pcap file at the
    /// given path.  Returns the number of frames written.
    #[cfg(feature = "capture")]
    #[allow(dead_code)] // used in Phase 3 (bug report zip)
    pub fn drain_to_pcap(&self, path: &std::path::Path) -> anyhow::Result<usize> {
        use pcap_file::pcap::{PcapHeader, PcapPacket, PcapWriter};
        use pcap_file::DataLink;
        use std::fs::File;

        let file = File::create(path)?;
        let header = PcapHeader {
            datalink: DataLink::ETHERNET,
            ..Default::default()
        };
        let mut writer = PcapWriter::with_header(file, header)?;

        let mut count = 0;
        for entry in &self.entries {
            let packet = PcapPacket::new(
                entry.timestamp,
                entry.pcap_frame.len() as u32,
                &entry.pcap_frame,
            );
            writer.write_packet(&packet).ok();
            count += 1;
        }

        Ok(count)
    }

    /// Current number of entries.
    #[allow(dead_code)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Current byte usage.
    #[allow(dead_code)]
    pub fn total_bytes(&self) -> usize {
        self.total_bytes
    }
}

/// Default per-channel ring buffer cap: 12.5 MB (50 MB / 4 channels).
const PER_CHANNEL_BYTES: usize = 50 * 1024 * 1024 / 4;

/// Known channel names.
#[allow(dead_code)] // used in later phases (bug report zip, traffic viewer)
const CHANNELS: [&str; 4] = ["main", "display", "inputs", "cursor"];

/// Holds all four per-channel ring buffers plus a shared session
/// start timestamp.
pub struct TrafficBuffers {
    main: Mutex<TrafficRingBuffer>,
    display: Mutex<TrafficRingBuffer>,
    inputs: Mutex<TrafficRingBuffer>,
    cursor: Mutex<TrafficRingBuffer>,
    /// Session start time for relative timestamps.
    start: Instant,
}

impl TrafficBuffers {
    /// Create a new set of traffic buffers.
    pub fn new() -> Self {
        TrafficBuffers {
            main: Mutex::new(TrafficRingBuffer::new(PER_CHANNEL_BYTES)),
            display: Mutex::new(TrafficRingBuffer::new(PER_CHANNEL_BYTES)),
            inputs: Mutex::new(TrafficRingBuffer::new(PER_CHANNEL_BYTES)),
            cursor: Mutex::new(TrafficRingBuffer::new(PER_CHANNEL_BYTES)),
            start: Instant::now(),
        }
    }

    /// Get the duration since session start.
    pub fn elapsed(&self) -> Duration {
        self.start.elapsed()
    }

    /// Get the ring buffer for a channel by name.
    fn buffer_for(&self, channel: &str) -> Option<&Mutex<TrafficRingBuffer>> {
        match channel {
            "main" => Some(&self.main),
            "display" => Some(&self.display),
            "inputs" => Some(&self.inputs),
            "cursor" => Some(&self.cursor),
            _ => None,
        }
    }

    /// Record a message received from the server.
    pub fn record_received(
        &self,
        channel: &'static str,
        msg_type: u16,
        msg_name: &'static str,
        raw_message: &[u8],
    ) {
        let buf = match self.buffer_for(channel) {
            Some(b) => b,
            None => return,
        };

        let elapsed = self.elapsed();
        let wire_size = raw_message.len() as u32;
        let payload_size = wire_size.saturating_sub(6);

        let mut guard = buf.lock().unwrap();
        let pcap_frame = self.build_frame(channel, false, raw_message, &mut guard);
        let entry = TrafficEntry {
            timestamp: elapsed,
            channel,
            direction: TrafficDirection::Received,
            message_type: msg_type,
            message_name: msg_name,
            wire_size,
            payload_size,
            pcap_frame,
        };
        guard.push(entry);
    }

    /// Record a message sent by the client.
    pub fn record_sent(
        &self,
        channel: &'static str,
        msg_type: u16,
        msg_name: &'static str,
        raw_message: &[u8],
    ) {
        let buf = match self.buffer_for(channel) {
            Some(b) => b,
            None => return,
        };

        let elapsed = self.elapsed();
        let wire_size = raw_message.len() as u32;
        let payload_size = wire_size.saturating_sub(6);

        let mut guard = buf.lock().unwrap();
        let pcap_frame = self.build_frame(channel, true, raw_message, &mut guard);
        let entry = TrafficEntry {
            timestamp: elapsed,
            channel,
            direction: TrafficDirection::Sent,
            message_type: msg_type,
            message_name: msg_name,
            wire_size,
            payload_size,
            pcap_frame,
        };
        guard.push(entry);
    }

    /// Build a pcap frame for the given message, updating TCP
    /// sequence numbers on the ring buffer.
    #[cfg(feature = "capture")]
    fn build_frame(
        &self,
        channel: &str,
        is_sent: bool,
        data: &[u8],
        ring: &mut TrafficRingBuffer,
    ) -> Vec<u8> {
        let port = capture::channel_port(channel);
        let (src_ip, src_port, dst_ip, dst_port, seq, ack) = if is_sent {
            let s = ring.client_seq;
            let a = ring.server_seq;
            ring.client_seq = ring.client_seq.wrapping_add(data.len() as u32);
            ([10, 0, 0, 1], port, [10, 0, 0, 2], 5900u16, s, a)
        } else {
            let s = ring.server_seq;
            let a = ring.client_seq;
            ring.server_seq = ring.server_seq.wrapping_add(data.len() as u32);
            ([10, 0, 0, 2], 5900u16, [10, 0, 0, 1], port, s, a)
        };
        capture::build_tcp_frame(src_ip, src_port, dst_ip, dst_port, seq, ack, data)
    }

    /// Stub when capture feature is disabled — produce an empty
    /// frame since pcap construction is unavailable.
    #[cfg(not(feature = "capture"))]
    fn build_frame(
        &self,
        _channel: &str,
        _is_sent: bool,
        _data: &[u8],
        _ring: &mut TrafficRingBuffer,
    ) -> Vec<u8> {
        Vec::new()
    }

    /// Log a summary of ring buffer state (for verbose mode).
    #[allow(dead_code)] // used in later phases
    pub fn log_summary(&self) {
        for name in &CHANNELS {
            if let Some(buf) = self.buffer_for(name) {
                let guard = buf.lock().unwrap();
                debug!(
                    "bugreport: {} ring buffer: {} entries, {} bytes",
                    name,
                    guard.len(),
                    guard.total_bytes()
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ring_buffer_push_and_evict() {
        let mut rb = TrafficRingBuffer::new(100);

        // Push entries totalling 60 bytes of pcap frames
        for i in 0..3 {
            rb.push(TrafficEntry {
                timestamp: Duration::from_millis(i * 100),
                channel: "test",
                direction: TrafficDirection::Received,
                message_type: i as u16,
                message_name: "test",
                wire_size: 20,
                payload_size: 14,
                pcap_frame: vec![0u8; 20],
            });
        }
        assert_eq!(rb.len(), 3);
        assert_eq!(rb.total_bytes(), 60);

        // Push more to exceed the 100-byte cap
        for i in 3..8 {
            rb.push(TrafficEntry {
                timestamp: Duration::from_millis(i * 100),
                channel: "test",
                direction: TrafficDirection::Sent,
                message_type: i as u16,
                message_name: "test",
                wire_size: 20,
                payload_size: 14,
                pcap_frame: vec![0u8; 20],
            });
        }

        // Should have evicted oldest entries to stay under 100 bytes
        assert!(rb.total_bytes() <= 100);
        assert!(rb.len() <= 5);

        // Oldest remaining entry should have a later timestamp
        let first = rb.entries().front().unwrap();
        assert!(first.timestamp >= Duration::from_millis(300));
    }

    #[test]
    fn test_traffic_buffers_record() {
        let buffers = TrafficBuffers::new();

        // Record some messages
        buffers.record_sent("main", 101, "key_down", &[0u8; 10]);
        buffers.record_received("display", 302, "draw_copy", &[0u8; 100]);
        buffers.record_sent("inputs", 101, "key_down", &[0u8; 10]);

        // Check entries are in the right buffers
        let main = buffers.main.lock().unwrap();
        assert_eq!(main.len(), 1);
        assert_eq!(main.entries().front().unwrap().message_name, "key_down");

        let display = buffers.display.lock().unwrap();
        assert_eq!(display.len(), 1);

        let inputs = buffers.inputs.lock().unwrap();
        assert_eq!(inputs.len(), 1);

        let cursor = buffers.cursor.lock().unwrap();
        assert_eq!(cursor.len(), 0);
    }

    #[test]
    fn test_traffic_buffers_unknown_channel_ignored() {
        let buffers = TrafficBuffers::new();
        buffers.record_sent("nonexistent", 1, "test", &[0u8; 10]);
        // Should not panic
    }
}
