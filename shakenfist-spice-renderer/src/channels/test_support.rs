//! Shared scaffolding for tests that drive a channel's message handler
//! directly, without a SPICE server.

use std::sync::Arc;
use std::time::{Duration, Instant};

use shakenfist_spice_protocol::link::SpiceStream;
use tokio::io::AsyncReadExt;
use tokio::sync::mpsc;

use super::{ChannelEvent, EventSink};
use crate::traffic::TrafficSink;

/// A `TrafficSink` that records nothing.
///
/// The channel calls into the sink on every message, and the recording
/// is noise to these tests. `elapsed` still has to advance monotonically
/// because the display channel's `retire_stream` stamps lifetimes with it.
pub(crate) struct NullTraffic {
    started: Instant,
}

impl NullTraffic {
    pub(crate) fn new() -> Self {
        NullTraffic {
            started: Instant::now(),
        }
    }
}

impl TrafficSink for NullTraffic {
    fn record_sent(&self, _: &'static str, _: u16, _: &'static str, _: &[u8]) {}
    fn record_received(&self, _: &'static str, _: u16, _: &'static str, _: &[u8]) {}
    fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }
}

/// Everything a channel under test needs kept alive alongside it.
///
/// Both are load-bearing even in a test that reads neither: dropping the
/// peer would make any write fail, and dropping the receiver would send
/// `emit` down its shutdown path.
pub(crate) struct TestChannelPeers {
    /// The server's end of the channel's socket.
    pub(crate) peer: tokio::net::TcpStream,
    /// What the channel emits.
    pub(crate) events: mpsc::Receiver<ChannelEvent>,
}

impl TestChannelPeers {
    /// Read the next `len` bytes the channel sent.
    pub(crate) async fn read_sent(&mut self, len: usize) -> Vec<u8> {
        let mut buf = vec![0; len];
        self.peer
            .read_exact(&mut buf)
            .await
            .expect("the channel sent the bytes");
        buf
    }
}

/// A loopback socket for a channel to own, the event sink it emits into,
/// and the other end of both.
pub(crate) async fn loopback() -> (SpiceStream, EventSink, TestChannelPeers) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind loopback listener");
    let addr = listener.local_addr().expect("local_addr");
    let client = tokio::net::TcpStream::connect(addr)
        .await
        .expect("connect to loopback listener");
    let (server, _) = listener.accept().await.expect("accept loopback connection");

    let (tx, rx) = mpsc::channel(256);
    let events = EventSink::new(tx, Arc::new(tokio::sync::Notify::new()));
    (
        SpiceStream::Plain(client),
        events,
        TestChannelPeers {
            peer: server,
            events: rx,
        },
    )
}
