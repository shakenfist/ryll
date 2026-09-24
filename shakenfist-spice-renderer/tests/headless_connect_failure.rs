//! `run_headless` against a port nothing listens on.
//!
//! A failed connect must come back as an `Err`, so `ryll --headless`
//! exits non-zero. It used to be logged and swallowed, and even the
//! log line was lost whenever the loop's "event stream closed" branch
//! won its `select!` race against the connection-result branch (both
//! become ready together when `run_connection` returns). The race is
//! decided at random on each run, so the test repeats the connect
//! enough times that a regression cannot hide behind a lucky draw.

use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use shakenfist_spice_protocol::ConnectionConfig;
use shakenfist_spice_renderer::{
    run_headless, ByteCounter, ChannelSnapshots, LogConfig, NotificationEntry, NotificationSink,
    TrafficSink,
};

struct NullTraffic;

impl TrafficSink for NullTraffic {
    fn record_sent(&self, _: &'static str, _: u16, _: &'static str, _: &[u8]) {}
    fn record_received(&self, _: &'static str, _: u16, _: &'static str, _: &[u8]) {}
    fn elapsed(&self) -> Duration {
        Duration::ZERO
    }
}

struct NullNotifications;

impl NotificationSink for NullNotifications {
    fn push(&self, _: NotificationEntry) {}
}

/// A loopback port that refuses connections: bind an ephemeral port,
/// then release it.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral loopback port");
    listener
        .local_addr()
        .expect("bound listener has an address")
        .port()
}

async fn headless_against(port: u16) -> anyhow::Result<()> {
    let config = ConnectionConfig {
        host: "127.0.0.1".to_string(),
        port,
        tls_port: None,
        password: None,
        ca_cert: None,
        host_subject: None,
        proxy: None,
    };
    run_headless(
        config,
        false,
        None,
        16,
        false,
        Vec::new(),
        None,
        None,
        1,
        Arc::new(ByteCounter::new()),
        Arc::new(NullTraffic),
        ChannelSnapshots::new(),
        Arc::new(NullNotifications),
        LogConfig::default(),
        Arc::new(AtomicBool::new(false)),
        64 * 1024 * 1024,
        64 * 1024 * 1024,
        None,
    )
    .await
}

#[tokio::test]
async fn headless_connect_failure_is_an_error() {
    let port = closed_port();
    for attempt in 0..20 {
        let result = tokio::time::timeout(Duration::from_secs(10), headless_against(port))
            .await
            .unwrap_or_else(|_| panic!("attempt {}: run_headless did not return", attempt));
        let err = result.expect_err("a refused connect must be an Err");
        assert_eq!(
            err.to_string(),
            "headless SPICE connection failed",
            "attempt {}",
            attempt
        );
        assert!(
            err.chain().count() > 1,
            "attempt {}: the error must carry the connection failure: {:?}",
            attempt,
            err
        );
    }
}
