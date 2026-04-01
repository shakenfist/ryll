/// Capture session for protocol and display debugging.
///
/// When `--capture <DIR>` is specified, all SPICE protocol
/// traffic and display frames are written to files in the
/// given directory. When not enabled, all methods are no-ops.
use std::collections::HashMap;
use std::fs::{self, File};
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

use pcap_file::pcap::{PcapHeader, PcapPacket, PcapWriter};
use pcap_file::DataLink;
use tracing::{debug, info};

/// Fake IP addresses for pcap headers.
const CLIENT_IP: [u8; 4] = [10, 0, 0, 1];
const SERVER_IP: [u8; 4] = [10, 0, 0, 2];
const SERVER_PORT: u16 = 5900;

/// Per-channel pcap writer with TCP state tracking.
struct PcapChannelWriter {
    writer: PcapWriter<BufWriter<File>>,
    client_seq: u32,
    server_seq: u32,
    client_port: u16,
}

impl PcapChannelWriter {
    fn new(path: PathBuf, client_port: u16) -> anyhow::Result<Self> {
        let file = BufWriter::new(File::create(&path)?);
        let header = PcapHeader {
            datalink: DataLink::ETHERNET,
            ..Default::default()
        };
        let writer = PcapWriter::with_header(file, header)?;
        info!("capture: opened {}", path.display());
        Ok(PcapChannelWriter {
            writer,
            client_seq: 1000,
            server_seq: 2000,
            client_port,
        })
    }

    fn write_sent(&mut self, data: &[u8], elapsed: std::time::Duration) {
        let frame = build_tcp_frame(
            CLIENT_IP,
            self.client_port,
            SERVER_IP,
            SERVER_PORT,
            self.client_seq,
            self.server_seq,
            data,
        );
        self.client_seq = self.client_seq.wrapping_add(data.len() as u32);
        self.write_frame(&frame, elapsed);
    }

    fn write_received(&mut self, data: &[u8], elapsed: std::time::Duration) {
        let frame = build_tcp_frame(
            SERVER_IP,
            SERVER_PORT,
            CLIENT_IP,
            self.client_port,
            self.server_seq,
            self.client_seq,
            data,
        );
        self.server_seq = self.server_seq.wrapping_add(data.len() as u32);
        self.write_frame(&frame, elapsed);
    }

    fn write_frame(&mut self, frame: &[u8], elapsed: std::time::Duration) {
        let packet = PcapPacket::new(elapsed, frame.len() as u32, frame);
        self.writer.write_packet(&packet).ok();
    }
}

/// Build a fake Ethernet + IPv4 + TCP frame wrapping `payload`.
fn build_tcp_frame(
    src_ip: [u8; 4],
    src_port: u16,
    dst_ip: [u8; 4],
    dst_port: u16,
    seq: u32,
    ack: u32,
    payload: &[u8],
) -> Vec<u8> {
    // Build headers manually for full control
    use etherparse::{Ethernet2Header, IpNumber, Ipv4Header, TcpHeader};

    let tcp_payload_len = payload.len();

    // TCP header
    let mut tcp = TcpHeader::new(src_port, dst_port, seq, 65535);
    tcp.acknowledgment_number = ack;
    tcp.ack = true;

    // IPv4 header
    let ip_payload_len = tcp.header_len() + tcp_payload_len;
    let mut ipv4 =
        Ipv4Header::new(ip_payload_len as u16, 64, IpNumber::TCP, src_ip, dst_ip).unwrap();
    ipv4.dont_fragment = true;

    // TCP checksum
    tcp.checksum = tcp.calc_checksum_ipv4(&ipv4, payload).unwrap_or(0);

    // Ethernet header
    let eth = Ethernet2Header {
        source: [0x02, 0x00, 0x00, 0x00, 0x00, 0x01],
        destination: [0x02, 0x00, 0x00, 0x00, 0x00, 0x02],
        ether_type: etherparse::ether_type::IPV4,
    };

    let mut frame = Vec::with_capacity(14 + 20 + tcp.header_len() + tcp_payload_len);
    eth.write(&mut frame).ok();
    ipv4.write(&mut frame).ok();
    tcp.write(&mut frame).ok();
    frame.extend_from_slice(payload);
    frame
}

/// Map channel name to a unique client port number.
fn channel_port(channel: &str) -> u16 {
    match channel {
        "main" => 10001,
        "display" => 10002,
        "inputs" => 10003,
        "cursor" => 10004,
        _ => 10099,
    }
}

/// Known channel names that get pcap writers.
const CHANNELS: &[&str] = &["main", "display", "cursor", "inputs"];

/// Holds state for an active capture session.
pub struct CaptureSession {
    /// Output directory for capture files.
    pub dir: PathBuf,
    /// Timestamp of session start, for relative timing.
    pub start: Instant,
    /// Per-channel pcap writers.
    pcap_writers: HashMap<String, Mutex<PcapChannelWriter>>,
}

impl CaptureSession {
    /// Create a new capture session writing to `dir`.
    pub fn new(dir: PathBuf) -> anyhow::Result<Self> {
        fs::create_dir_all(&dir)?;
        info!("capture: writing to {}", dir.display());

        let mut pcap_writers = HashMap::new();
        for &channel in CHANNELS {
            let path = dir.join(format!("{}.pcap", channel));
            let port = channel_port(channel);
            let writer = PcapChannelWriter::new(path, port)?;
            pcap_writers.insert(channel.to_string(), Mutex::new(writer));
        }

        Ok(CaptureSession {
            dir,
            start: Instant::now(),
            pcap_writers,
        })
    }

    /// Record a packet sent by the client on the given channel.
    pub fn packet_sent(&self, channel: &str, data: &[u8]) {
        if let Some(writer) = self.pcap_writers.get(channel) {
            let elapsed = self.start.elapsed();
            let mut w = writer.lock().unwrap();
            w.write_sent(data, elapsed);
        } else {
            debug!("capture: no pcap writer for channel '{}'", channel);
        }
    }

    /// Record a packet received from the server on the given channel.
    pub fn packet_received(&self, channel: &str, data: &[u8]) {
        if let Some(writer) = self.pcap_writers.get(channel) {
            let elapsed = self.start.elapsed();
            let mut w = writer.lock().unwrap();
            w.write_received(data, elapsed);
        } else {
            debug!("capture: no pcap writer for channel '{}'", channel);
        }
    }

    /// Record a display frame after a MARK boundary.
    /// Phase 3 will encode this as a video frame.
    #[allow(dead_code)] // implemented in phase 3
    pub fn frame(&self, _surface_id: u32, _pixels: &[u8], _width: u32, _height: u32) {
        // Stub — implemented in phase 3
    }

    /// Finalise and close the capture session.
    pub fn close(&mut self) {
        // Flush all pcap writers
        // Drop all writers to flush BufWriter buffers
        for (name, _) in self.pcap_writers.drain() {
            debug!("capture: closing {}.pcap", name);
        }
        info!("capture: session closed ({})", self.dir.display());
    }
}

impl Drop for CaptureSession {
    fn drop(&mut self) {
        self.close();
    }
}
