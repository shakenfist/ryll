//! Wire types for the Ryll control-socket protocol (version 1.2).
//!
//! All types derive `serde::Serialize` / `serde::Deserialize` so they
//! can be round-tripped through NDJSON without any manual parsing.
//! The serialisation choices here are the binding contract: changing
//! them is a protocol-breaking change.
//!
//! v1.0 → v1.1 added the `surface_drawn` event (and, under the
//! `digest-decode` Cargo feature, `digest_updated`).  Hello
//! handshake compatibility is at the major-version level, so
//! v1.0 clients still hello successfully; they just never
//! subscribe to v1.1 events.
//!
//! v1.1 → v1.2 corrected `send_key`'s handling of 0xE0-prefixed
//! extended scancodes, which were transmitted with the prefix byte
//! second and the break bit on the wrong byte.  A minor bump rather
//! than a major one because the envelope, verbs and field types are
//! unchanged, and because no client could have depended on the old
//! behaviour and reached the guest correctly.  It is called out in
//! the version number so a client needing arrow keys has something
//! to require.

use serde::{Deserialize, Serialize};

pub use crate::channels::RequestId;

/// Protocol version this server speaks.
pub const PROTOCOL_VERSION: &str = "1.2";

/// All v1 verb names the server recognises. Advertised in the `hello`
/// response so clients can adapt at runtime rather than hard-coding
/// assumptions about which verbs a given server supports.
pub const SUPPORTED_METHODS: &[&str] = &[
    "hello",
    "status",
    "send_key",
    "paste",
    "screenshot",
    "subscribe",
    "unsubscribe",
];

/// All event names the server can emit. Advertised in the `hello`
/// response alongside `SUPPORTED_METHODS`.  `surface_drawn` was
/// added in v1.1; `digest_updated` lives behind the
/// `digest-decode` Cargo feature and is appended at runtime by
/// [`supported_events`].
const BASE_SUPPORTED_EVENTS: &[&str] = &[
    "latency",
    "agent_connected",
    "paste_completed",
    "paste_failed",
    "dropped",
    "surface_drawn",
];

/// Names of every event this server can emit, including any
/// feature-gated entries (currently `digest_updated`, gated by
/// `digest-decode`).  Returned as an owned `Vec` because the
/// list grows at compile time only — the runtime cost is a
/// trivial allocation per hello.
pub fn supported_events() -> Vec<String> {
    // `mut` is needed in the `digest-decode` configuration; allow
    // the unused-mut lint to fire silently on the slim build.
    #[allow(unused_mut)]
    let mut out: Vec<String> = BASE_SUPPORTED_EVENTS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    #[cfg(feature = "digest-decode")]
    out.push("digest_updated".to_string());
    out
}

/// Compile-time slice of base events.  Existing callers that
/// only need the always-on list (e.g. tests asserting on a
/// known set) can continue using this; new code that needs the
/// feature-aware list should call [`supported_events`] instead.
pub const SUPPORTED_EVENTS: &[&str] = BASE_SUPPORTED_EVENTS;

// ── Request envelope ─────────────────────────────────────────────

/// A single client-to-server request line.
#[derive(Debug, Deserialize)]
pub struct Request {
    pub id: RequestId,
    pub method: String,
    /// Raw params object.  Each verb handler deserialises this
    /// into its own param struct so the server does not need a
    /// giant tagged-union here.
    pub params: serde_json::Value,
}

// ── Response envelope ────────────────────────────────────────────

/// A single server-to-client response line.
///
/// The `busy` synthetic response (written when a second client
/// connects while one is active) has no `id` field — hence
/// `Option<RequestId>` with `skip_serializing_if`.
#[derive(Debug, Serialize)]
pub struct Response {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<RequestId>,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<RpcError>,
}

impl Response {
    /// Construct a success response.
    pub fn ok(id: RequestId, result: serde_json::Value) -> Self {
        Self {
            id: Some(id),
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    /// Construct an error response for a known request.
    pub fn err(id: RequestId, code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            id: Some(id),
            ok: false,
            result: None,
            error: Some(RpcError {
                code,
                message: message.into(),
            }),
        }
    }

    /// Construct the synthetic `busy` response written to a second
    /// client that connects while one is already active.  This
    /// response has no `id` field.
    pub fn busy() -> Self {
        Self {
            id: None,
            ok: false,
            result: None,
            error: Some(RpcError {
                code: ErrorCode::Busy,
                message: "another client is connected".into(),
            }),
        }
    }
}

// ── Error descriptor ─────────────────────────────────────────────

/// Error descriptor nested inside a failed `Response`.
#[derive(Debug, Serialize, Deserialize)]
pub struct RpcError {
    pub code: ErrorCode,
    pub message: String,
}

// ── Error codes ──────────────────────────────────────────────────

/// Stable machine-readable error codes defined in protocol v1.0.
///
/// `rename_all = "snake_case"` makes the serde-derived name match
/// the wire representation (e.g. `NoHelloYet` → `"no_hello_yet"`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    NoHelloYet,
    ProtocolVersionMismatch,
    Busy,
    UnknownMethod,
    BadParams,
    BadState,
    AgentNotConnected,
    NoSuchSurface,
    UnsupportedFormat,
    NotImplemented,
    InternalError,
}

// ── Event envelope ───────────────────────────────────────────────

/// An unsolicited server-to-client event line.  Events have no `id`
/// and do not correspond to a request.
#[derive(Debug, Serialize)]
pub struct Event {
    pub event: String,
    pub data: serde_json::Value,
}

// ── Verb-specific params / results ───────────────────────────────

/// Params for the `hello` verb.
#[derive(Debug, Deserialize)]
pub struct HelloParams {
    pub client_name: String,
    pub protocol_version: String,
}

/// Result payload for a successful `hello` response.
#[derive(Debug, Serialize)]
pub struct HelloResult {
    pub server_name: String,
    pub protocol_version: String,
    pub supported_methods: Vec<String>,
    pub supported_events: Vec<String>,
}

/// Result payload for a successful `status` response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusResult {
    pub spice_connected: bool,
    pub agent_connected: bool,
    pub surfaces: Vec<SurfaceInfo>,
}

/// Per-surface entry within a `StatusResult`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceInfo {
    pub channel_id: u8,
    pub surface_id: u32,
    pub width: u32,
    pub height: u32,
}

/// Data payload of the v1.1 `surface_drawn` event.
///
/// Emitted once per draw command on the display channel — the
/// granularity that gives consumers (the loadtest orchestrator's
/// keypress-to-screen latency probe) the earliest visible-pixel
/// signal.  Carries both the renderer-internal `produced_at_secs`
/// monotonic timestamp (lifted unchanged from the underlying
/// `ChannelEvent`) and a host wallclock timestamp in microseconds
/// so cross-process consumers can compute deltas against keypress
/// times recorded in wallclock without straddling clocks.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SurfaceDrawnData {
    pub display_channel_id: u8,
    pub surface_id: u32,
    pub produced_at_secs: f64,
    pub wallclock_us: u64,
}

/// Params for the `subscribe` and `unsubscribe` verbs.
///
/// Both verbs share the same shape (`{"events": [...]}`); only the
/// result type differs.  Unknown event names are silently ignored
/// for forward compatibility — a client compiled against a future
/// version of the protocol can ask for `digest_updated` without
/// breaking the call.
#[derive(Debug, Deserialize)]
pub struct SubscribeParams {
    pub events: Vec<String>,
}

/// Result payload for `subscribe`: the subset of requested event
/// names the server actually agreed to deliver.
#[derive(Debug, Serialize, Deserialize)]
pub struct SubscribeResult {
    pub subscribed: Vec<String>,
}

/// Result payload for `unsubscribe`: the subset of requested event
/// names that were actually removed from the active subscription
/// set (i.e. were present beforehand).
#[derive(Debug, Serialize, Deserialize)]
pub struct UnsubscribeResult {
    pub unsubscribed: Vec<String>,
}

// ── Version helpers ──────────────────────────────────────────────

/// Parse a `"major.minor"` protocol-version string and return the
/// two components as `(major, minor)`.
///
/// Returns an error if the string is not exactly two dot-separated
/// non-negative integers.
pub fn parse_protocol_version(s: &str) -> Result<(u32, u32), String> {
    let parts: Vec<&str> = s.splitn(2, '.').collect();
    if parts.len() != 2 {
        return Err(format!(
            "expected \"major.minor\" version string, got {:?}",
            s
        ));
    }
    let major = parts[0]
        .parse::<u32>()
        .map_err(|e| format!("invalid major component {:?}: {}", parts[0], e))?;
    let minor = parts[1]
        .parse::<u32>()
        .map_err(|e| format!("invalid minor component {:?}: {}", parts[1], e))?;
    Ok((major, minor))
}

/// Returns `true` when the client's major version matches the
/// server's major version (the only compatibility criterion for
/// v1).
pub fn major_version_matches(client_version: &str) -> Result<bool, String> {
    let (server_major, _) = parse_protocol_version(PROTOCOL_VERSION)?;
    let (client_major, _) = parse_protocol_version(client_version)?;
    Ok(client_major == server_major)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_version_ok() {
        assert_eq!(parse_protocol_version("1.0").unwrap(), (1, 0));
        assert_eq!(parse_protocol_version("2.3").unwrap(), (2, 3));
    }

    #[test]
    fn parse_version_bad_format() {
        assert!(parse_protocol_version("1").is_err());
        // splitn(2, '.') on "1.0.0" gives ["1", "0.0"];
        // "0.0".parse::<u32>() fails, so the whole call is Err.
        assert!(parse_protocol_version("1.0.0").is_err());
    }

    #[test]
    fn major_version_match_same() {
        assert!(major_version_matches("1.0").unwrap());
        assert!(major_version_matches("1.99").unwrap());
    }

    #[test]
    fn major_version_mismatch() {
        assert!(!major_version_matches("2.0").unwrap());
        assert!(!major_version_matches("0.9").unwrap());
    }

    #[test]
    fn response_busy_has_no_id() {
        let r = Response::busy();
        let json = serde_json::to_string(&r).unwrap();
        assert!(!json.contains("\"id\""));
        assert!(json.contains("\"busy\""));
    }

    #[test]
    fn response_ok_serialises() {
        let r = Response::ok(RequestId::Int(1), serde_json::json!({"foo": "bar"}));
        let json = serde_json::to_string(&r).unwrap();
        assert!(json.contains("\"ok\":true"));
        assert!(json.contains("\"id\":1"));
        assert!(!json.contains("\"error\""));
    }

    #[test]
    fn error_code_snake_case() {
        let code = ErrorCode::NoHelloYet;
        let s = serde_json::to_string(&code).unwrap();
        assert_eq!(s, "\"no_hello_yet\"");
    }
}
