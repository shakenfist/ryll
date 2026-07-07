/// Protocol traffic logging
///
/// Provides detailed logging of SPICE protocol messages for debugging
/// and protocol coverage testing.
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, OnceLock};

use tracing::{debug, warn};

/// Observer callback invoked the first time a given warn_once key fires
/// in this session. See [`register_gap_observer`] for semantics.
pub type GapObserver = Arc<dyn Fn(&'static str) + Send + Sync + 'static>;

fn registry() -> &'static Mutex<HashSet<&'static str>> {
    static REG: OnceLock<Mutex<HashSet<&'static str>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashSet::new()))
}

fn observer_list() -> &'static Mutex<Vec<GapObserver>> {
    static OBS: OnceLock<Mutex<Vec<GapObserver>>> = OnceLock::new();
    OBS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Insert `key` into the warn_once registry. Returns `true` if the key
/// was newly inserted (never seen before this call), `false` on repeat.
///
/// When a new key is inserted, registered gap observers are invoked
/// *after* the registry lock is released, so observers may freely call
/// back into `warn_once_*` or register additional observers without
/// deadlocking.
fn register_key(key: &'static str) -> bool {
    let is_new = {
        let mut set = registry().lock().expect("registry lock poisoned");
        set.insert(key)
    };
    if is_new {
        dispatch_new_gap(key);
    }
    is_new
}

/// Walk the observer list and invoke each observer with `key`. The
/// observer-list lock is held only long enough to clone the `Arc`
/// pointers into a local `Vec`; observers then run with no internal
/// locks held, so they may freely call back into the logging module.
fn dispatch_new_gap(key: &'static str) {
    let observers: Vec<GapObserver> = {
        let guard = observer_list().lock().expect("observer list lock poisoned");
        guard.iter().cloned().collect()
    };
    for observer in observers {
        observer(key);
    }
}

/// Register a callback to be invoked whenever a new warn_once key is
/// registered for the first time in this session.
///
/// On registration, the observer is immediately replayed with every
/// key that has already fired so far, so late observers see a complete
/// history.
///
/// # Threading
///
/// The observer runs on the thread of the triggering `register_key`
/// call, after the registry lock has been released. Observers may
/// therefore call `warn_once_*`, `warn_once_keys`, or
/// `register_gap_observer` without deadlocking.
///
/// # Double-fire during registration races
///
/// There is a small race window during registration: if another thread
/// fires a brand-new key between the moment this function pushes the
/// observer onto the list and the moment it snapshots the registry for
/// replay, the observer may see that key twice (once via normal
/// dispatch, once via replay). Observers must therefore be idempotent
/// per key. This is already a natural requirement, since the registry
/// is process-global and the same key may legitimately appear in
/// multiple observers' replay histories across a session.
pub fn register_gap_observer(observer: GapObserver) {
    // Push first so any key fired after this point is dispatched to
    // the new observer. The subsequent replay then covers keys that
    // fired before the push. Keys fired between the push and the
    // snapshot may arrive twice — callers must tolerate that.
    {
        let mut observers = observer_list().lock().expect("observer list lock poisoned");
        observers.push(observer.clone());
    }
    // Snapshot the registry AFTER releasing the observer-list lock so
    // an observer that itself calls `warn_once_*` during replay won't
    // deadlock on the observer-list lock.
    let snapshot = warn_once_keys();
    for key in snapshot {
        observer(key);
    }
}

/// Emit `tracing::warn!` exactly once per session for each distinct
/// `key`. Subsequent calls with the same key are silent. Thread-safe.
///
/// Prefer the `warn_once!` macro at call sites so `format!` is
/// deferred until the first occurrence.
pub fn warn_once_impl(key: &'static str, message: &str) {
    if register_key(key) {
        warn!("{}", message);
    }
}

/// Caller-side variant of the warn-once pattern. Returns `true` if
/// `key` was newly inserted into the session registry (caller should
/// fire side-effects such as a hex dump), `false` on repeat (caller
/// should stay silent). No formatting, no `warn!` call — the caller
/// controls what "fire" means.
pub fn warn_once_impl_if_new(key: &'static str) -> bool {
    register_key(key)
}

/// Intern a dynamically-composed key into process-lifetime memory so
/// the `HashSet<&'static str>` warn_once registry can accept it.
///
/// The leaked memory is bounded by the number of distinct dynamic keys
/// the session will ever produce — typically on the order of
/// `channel × msg_type` combinations (~50 in practice). Callers must
/// not pass unbounded per-message data as the key.
pub fn intern_key(key: String) -> &'static str {
    static INTERN: OnceLock<Mutex<HashMap<String, &'static str>>> = OnceLock::new();
    let map = INTERN.get_or_init(|| Mutex::new(HashMap::new()));
    let mut guard = map.lock().expect("intern map lock poisoned");
    if let Some(existing) = guard.get(&key) {
        existing
    } else {
        let leaked: &'static str = Box::leak(key.clone().into_boxed_str());
        guard.insert(key, leaked);
        leaked
    }
}

/// Per-channel cap on distinct unknown `msg_type` values that
/// `log_unknown_once` will register into the warn_once registry
/// before suppressing further variants. Guards against a hostile
/// server cycling through all 65 536 `msg_type` values per channel
/// to force unbounded `Box::leak` growth via `intern_key`. After
/// the cap is hit, a single "further unknown opcodes suppressed"
/// warn_once fires per channel; subsequent calls for that channel
/// are silent. Seven channels × 64 = bounded at ~450 distinct
/// registry entries for this category, ~20 KiB leaked.
const UNKNOWN_OPCODE_CAP_PER_CHANNEL: usize = 64;

fn unknown_seen() -> &'static Mutex<HashMap<&'static str, HashSet<u16>>> {
    static SEEN: OnceLock<Mutex<HashMap<&'static str, HashSet<u16>>>> = OnceLock::new();
    SEEN.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Outcome of the check-and-insert step in `log_unknown_once`.
/// Kept local because the three branches have no use outside
/// the one call site.
enum UnknownLogAction {
    New,
    Repeat,
    AtCap,
}

/// One-shot variant of `log_unknown`: hex-dumps the payload on the
/// first call for `(channel, msg_type)` in this session, silent on
/// repeats, and silent-with-one-suppression-notice once a channel
/// has seen `UNKNOWN_OPCODE_CAP_PER_CHANNEL` distinct unknown
/// `msg_type` values (cap defends against hostile-server
/// enumeration — see that constant's doc).
pub fn log_unknown_once(channel: &'static str, msg_type: u16, payload: &[u8]) {
    // Single critical section over the per-channel seen-set so the
    // "is it new?", "are we at the cap?", and "record it" decisions
    // are atomic with each other.
    let action = {
        let mut seen = unknown_seen().lock().expect("unknown_seen lock poisoned");
        let set = seen.entry(channel).or_default();
        if set.contains(&msg_type) {
            UnknownLogAction::Repeat
        } else if set.len() >= UNKNOWN_OPCODE_CAP_PER_CHANNEL {
            UnknownLogAction::AtCap
        } else {
            set.insert(msg_type);
            UnknownLogAction::New
        }
    };
    match action {
        UnknownLogAction::Repeat => {}
        UnknownLogAction::New => {
            let key = intern_key(format!("{}:hexdump:{}", channel, msg_type));
            if warn_once_impl_if_new(key) {
                warn!(
                    "{} {} byte UNKNOWN opcode {} (first occurrence; subsequent silent)",
                    channel,
                    payload.len(),
                    msg_type
                );
                hex_dump(payload, 64);
            }
        }
        UnknownLogAction::AtCap => {
            warn_once_impl(
                intern_key(format!("{}:hexdump_cap", channel)),
                &format!(
                    "{}: reached cap of {} distinct unknown opcodes; \
                     further unknown opcodes suppressed to bound memory",
                    channel, UNKNOWN_OPCODE_CAP_PER_CHANNEL
                ),
            );
        }
    }
}

/// Number of distinct keys that have fired so far this session.
/// Used by the phase-8 status-bar gap counter.
pub fn warn_once_count() -> usize {
    registry().lock().expect("registry lock poisoned").len()
}

/// Snapshot of the fired keys (in some order). Caller does not hold
/// the registry lock. Used by the phase-8 pedantic popup / bug
/// report assembly.
pub fn warn_once_keys() -> Vec<&'static str> {
    registry()
        .lock()
        .expect("registry lock poisoned")
        .iter()
        .copied()
        .collect()
}

/// Emit `tracing::warn!` exactly once per session for a given key.
///
/// The message expression is only evaluated on the first call for
/// each key, so `format!` overhead is paid at most once.
///
/// # Example
///
/// ```
/// use shakenfist_spice_protocol::warn_once;
/// warn_once!("my-feature:unsupported", "unsupported thing: {}", 42);
/// ```
#[macro_export]
macro_rules! warn_once {
    ($key:expr, $($arg:tt)+) => {{
        $crate::logging::warn_once_impl(
            $key,
            &format!($($arg)+),
        );
    }};
}

/// Log a protocol message
pub fn log_message(direction: &str, channel: &str, msg_type: u16, msg_type_str: &str, size: u32) {
    debug!(
        "{} {} {} byte opcode {} {}",
        channel, direction, size, msg_type, msg_type_str
    );
}

/// Log message details (indented continuation)
pub fn log_detail(detail: &str) {
    debug!("   ... {}", detail);
}

/// Log an unknown/undecoded message type
pub fn log_unknown(channel: &str, direction: &str, msg_type: u16, size: u32, data: &[u8]) {
    warn!(
        "{} {} {} byte UNKNOWN opcode {}",
        channel, direction, size, msg_type
    );
    hex_dump(data, 64);
}

/// Log incomplete message (waiting for more data)
#[allow(dead_code)]
pub fn log_incomplete(channel: &str, msg_type_str: &str, have: usize, want: usize) {
    debug!(
        "{} message {} incomplete: have {} bytes, want {}",
        channel, msg_type_str, have, want
    );
}

/// Hex dump of data for debugging unknown messages
pub fn hex_dump(data: &[u8], max_bytes: usize) {
    let dump_len = data.len().min(max_bytes);
    let mut offset = 0;

    while offset < dump_len {
        let chunk_end = (offset + 16).min(dump_len);
        let chunk = &data[offset..chunk_end];

        // Build printable, decimal, and hex representations
        let mut printable = String::with_capacity(16);
        let mut hex = String::with_capacity(48);

        for &byte in chunk {
            // Printable ASCII or dot
            if (0x20..0x7f).contains(&byte) {
                printable.push(byte as char);
            } else {
                printable.push('.');
            }

            // Hex representation
            hex.push_str(&format!("{:02x} ", byte));
        }

        // Pad printable to 16 chars for alignment
        while printable.len() < 16 {
            printable.push(' ');
        }

        debug!("   {:04x}: {}  {}", offset, printable, hex.trim_end());

        offset += 16;
    }

    if data.len() > max_bytes {
        debug!("   ... ({} more bytes)", data.len() - max_bytes);
    }
}

/// Message type lookup helpers for logging
pub mod message_names {
    use super::super::constants::*;

    fn common_client(msg_type: u16) -> Option<&'static str> {
        match msg_type {
            main_client::ACK_SYNC => Some("ack_sync"),
            main_client::ACK => Some("ack"),
            main_client::PONG => Some("pong"),
            _ => None,
        }
    }

    pub fn main_server(msg_type: u16) -> &'static str {
        match msg_type {
            main_server::MIGRATE => "migrate",
            main_server::MIGRATE_DATA => "migrate_data",
            main_server::SET_ACK => "set_ack",
            main_server::PING => "ping",
            main_server::WAIT_FOR_CHANNELS => "wait_for_channels",
            main_server::DISCONNECTING => "disconnecting",
            main_server::NOTIFY => "notify",
            main_server::MIGRATE_BEGIN => "migrate_begin",
            main_server::MIGRATE_CANCEL => "migrate_cancel",
            main_server::INIT => "init",
            main_server::CHANNELS_LIST => "channels_list",
            main_server::MOUSE_MODE => "mouse_mode",
            main_server::MULTI_MEDIA_TIME => "multi_media_time",
            main_server::AGENT_CONNECTED => "agent_connected",
            main_server::AGENT_DISCONNECTED => "agent_disconnected",
            main_server::AGENT_DATA => "agent_data",
            main_server::AGENT_TOKEN => "agent_token",
            main_server::MIGRATE_SWITCH_HOST => "migrate_switch_host",
            main_server::MIGRATE_END => "migrate_end",
            main_server::NAME => "name",
            main_server::UUID => "uuid",
            main_server::AGENT_CONNECTED_TOKENS => "agent_connected_tokens",
            main_server::MIGRATE_BEGIN_SEAMLESS => "migrate_begin_seamless",
            main_server::MIGRATE_DST_SEAMLESS_ACK => "migrate_dst_seamless_ack",
            main_server::MIGRATE_DST_SEAMLESS_NACK => "migrate_dst_seamless_nack",
            _ => "unknown",
        }
    }

    /// Get main channel client message name
    pub fn main_client(msg_type: u16) -> &'static str {
        match msg_type {
            main_client::MIGRATE_FLUSH_MARK => "migrate_flush_mark",
            main_client::MIGRATE_DATA => "migrate_data",
            main_client::DISCONNECTING => "disconnecting",
            main_client::CLIENT_INFO => "client_info",
            main_client::MIGRATE_CONNECTED => "migrate_connected",
            main_client::MIGRATE_CONNECT_ERROR => "migrate_connect_error",
            main_client::ATTACH_CHANNELS => "attach_channels",
            main_client::MOUSE_MODE_REQUEST => "mouse_mode_request",
            main_client::AGENT_START => "agent_start",
            main_client::AGENT_DATA => "agent_data",
            main_client::AGENT_TOKEN => "agent_token",
            main_client::MIGRATE_END => "migrate_end",
            main_client::MIGRATE_DST_DO_SEAMLESS => "migrate_dst_do_seamless",
            main_client::MIGRATE_CONNECTED_SEAMLESS => "migrate_connected_seamless",
            main_client::QUALITY_INDICATOR => "quality_indicator",
            _ => common_client(msg_type).unwrap_or("unknown"),
        }
    }

    /// Get display channel server message name
    pub fn display_server(msg_type: u16) -> &'static str {
        match msg_type {
            display_server::MODE => "mode",
            display_server::MARK => "mark",
            display_server::RESET => "reset",
            display_server::COPY_BITS => "copy_bits",
            display_server::INVALIDATE_LIST => "invalidate_list",
            display_server::INVAL_ALL_PIXMAPS => "inval_all_pixmaps",
            display_server::INVAL_PALETTE => "inval_palette",
            display_server::INVAL_ALL_PALETTES => "inval_all_palettes",
            display_server::STREAM_CREATE => "stream_create",
            display_server::STREAM_DATA => "stream_data",
            display_server::STREAM_CLIP => "stream_clip",
            display_server::STREAM_DESTROY => "stream_destroy",
            display_server::STREAM_DESTROY_ALL => "stream_destroy_all",
            display_server::STREAM_DATA_SIZED => "stream_data_sized",
            display_server::DRAW_FILL => "draw_fill",
            display_server::DRAW_OPAQUE => "draw_opaque",
            display_server::DRAW_COPY => "draw_copy",
            display_server::DRAW_BLEND => "draw_blend",
            display_server::DRAW_BLACKNESS => "draw_blackness",
            display_server::DRAW_WHITENESS => "draw_whiteness",
            display_server::DRAW_INVERS => "draw_invers",
            display_server::DRAW_ROP3 => "draw_rop3",
            display_server::DRAW_STROKE => "draw_stroke",
            display_server::DRAW_TEXT => "draw_text",
            display_server::DRAW_TRANSPARENT => "draw_transparent",
            display_server::DRAW_ALPHA_BLEND => "draw_alpha_blend",
            display_server::DRAW_COMPOSITE => "draw_composite",
            display_server::SURFACE_CREATE => "surface_create",
            display_server::SURFACE_DESTROY => "surface_destroy",
            display_server::MONITORS_CONFIG => "monitors_config",
            display_server::STREAM_ACTIVATE_REPORT => "stream_activate_report",
            display_server::GL_SCANOUT_UNIX => "gl_scanout_unix",
            display_server::GL_DRAW => "gl_draw",
            display_server::QUALITY_INDICATOR => "quality_indicator",
            display_server::GL_SCANOUT2_UNIX => "gl_scanout2_unix",
            display_server::SET_ACK => "set_ack",
            display_server::PING => "ping",
            display_server::NOTIFY => "notify",
            _ => "unknown",
        }
    }

    /// Get display channel client message name
    pub fn display_client(msg_type: u16) -> &'static str {
        match msg_type {
            display_client::INIT => "init",
            display_client::STREAM_REPORT => "stream_report",
            display_client::PREFERRED_COMPRESSION => "preferred_compression",
            display_client::GL_DRAW_DONE => "gl_draw_done",
            display_client::PREFERRED_VIDEO_CODEC_TYPE => "preferred_video_codec_type",
            _ => common_client(msg_type).unwrap_or("unknown"),
        }
    }

    /// Get inputs channel server message name
    pub fn inputs_server(msg_type: u16) -> &'static str {
        match msg_type {
            inputs_server::INIT => "init",
            inputs_server::KEY_MODIFIERS => "key_modifiers",
            inputs_server::MOUSE_MOTION_ACK => "mouse_motion_ack",
            inputs_server::SET_ACK => "set_ack",
            inputs_server::PING => "ping",
            inputs_server::NOTIFY => "notify",
            _ => "unknown",
        }
    }

    /// Get inputs channel client message name
    pub fn inputs_client(msg_type: u16) -> &'static str {
        match msg_type {
            inputs_client::KEY_DOWN => "key_down",
            inputs_client::KEY_UP => "key_up",
            inputs_client::KEY_MODIFIERS => "key_modifiers",
            inputs_client::KEY_SCANCODE => "key_scancode",
            inputs_client::MOUSE_MOTION => "mouse_motion",
            inputs_client::MOUSE_POSITION => "mouse_position",
            inputs_client::MOUSE_PRESS => "mouse_press",
            inputs_client::MOUSE_RELEASE => "mouse_release",
            _ => common_client(msg_type).unwrap_or("unknown"),
        }
    }

    /// Get cursor channel server message name
    pub fn cursor_server(msg_type: u16) -> &'static str {
        match msg_type {
            cursor_server::INIT => "init",
            cursor_server::RESET => "reset",
            cursor_server::SET => "set",
            cursor_server::MOVE => "move",
            cursor_server::HIDE => "hide",
            cursor_server::TRAIL => "trail",
            cursor_server::INVALIDATE_ONE => "invalidate_one",
            cursor_server::INVALIDATE_ALL => "invalidate_all",
            cursor_server::SET_ACK => "set_ack",
            cursor_server::PING => "ping",
            cursor_server::NOTIFY => "notify",
            _ => "unknown",
        }
    }

    /// Get cursor channel client message name
    pub fn cursor_client(msg_type: u16) -> &'static str {
        common_client(msg_type).unwrap_or("unknown")
    }

    pub fn playback_server(msg_type: u16) -> &'static str {
        match msg_type {
            playback_server::DATA => "data",
            playback_server::MODE => "mode",
            playback_server::START => "start",
            playback_server::STOP => "stop",
            playback_server::VOLUME => "volume",
            playback_server::MUTE => "mute",
            playback_server::LATENCY => "latency",
            playback_server::SET_ACK => "set_ack",
            playback_server::PING => "ping",
            playback_server::NOTIFY => "notify",
            _ => "unknown",
        }
    }

    pub fn playback_client(msg_type: u16) -> &'static str {
        common_client(msg_type).unwrap_or("unknown")
    }

    /// Get SpiceVMC/usbredir server message name
    pub fn spicevmc_server(msg_type: u16) -> &'static str {
        match msg_type {
            spicevmc_server::DATA => "vmc_data",
            spicevmc_server::COMPRESSED_DATA => "vmc_compressed_data",
            spicevmc_server::SET_ACK => "set_ack",
            spicevmc_server::PING => "ping",
            spicevmc_server::NOTIFY => "notify",
            _ => "unknown",
        }
    }

    /// Get SpiceVMC/usbredir client message name
    pub fn spicevmc_client(msg_type: u16) -> &'static str {
        match msg_type {
            spicevmc_client::DATA => "vmc_data",
            spicevmc_client::COMPRESSED_DATA => "vmc_compressed_data",
            _ => common_client(msg_type).unwrap_or("unknown"),
        }
    }

    /// Return the human-readable name for a display channel
    /// capability bit, or `None` if the bit position is not
    /// known to this version of the protocol crate.
    ///
    /// Used by the traffic viewer to annotate capability
    /// bitmask values with symbolic names.
    ///
    /// # Example
    ///
    /// ```
    /// use shakenfist_spice_protocol::logging::message_names;
    /// assert_eq!(
    ///     message_names::display_cap_name(4),
    ///     Some("stream_report"),
    /// );
    /// ```
    pub fn display_cap_name(bit: u8) -> Option<&'static str> {
        match bit {
            0 => Some("sized_stream"),
            1 => Some("monitors_config"),
            2 => Some("composite"),
            3 => Some("a8_surface"),
            4 => Some("stream_report"),
            5 => Some("lz4_compression"),
            8 => Some("multi_codec"),
            9 => Some("codec_mjpeg"),
            11 => Some("codec_h264"),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{
        intern_key, log_unknown_once, message_names, register_gap_observer, warn_once_keys,
    };
    use crate::constants::{display_client, display_server, main_client, main_server};

    // The registry is process-global and cargo-test runs tests in
    // parallel, so assertions here key off specific literals unique
    // to each test rather than `warn_once_count()` deltas.

    #[test]
    fn test_warn_once_fires_once() {
        warn_once!("test_warn_once_fires_once:k1", "msg 1");
        warn_once!("test_warn_once_fires_once:k1", "msg 1");
        warn_once!("test_warn_once_fires_once:k1", "msg 1");
        let keys = warn_once_keys();
        assert_eq!(
            keys.iter()
                .filter(|k| **k == "test_warn_once_fires_once:k1")
                .count(),
            1
        );
    }

    #[test]
    fn test_warn_once_distinct_keys_all_fire() {
        warn_once!("test_warn_once_distinct_keys_all_fire:a", "msg a");
        warn_once!("test_warn_once_distinct_keys_all_fire:b", "msg b");
        warn_once!("test_warn_once_distinct_keys_all_fire:c", "msg c");
        let keys = warn_once_keys();
        assert!(keys.contains(&"test_warn_once_distinct_keys_all_fire:a"));
        assert!(keys.contains(&"test_warn_once_distinct_keys_all_fire:b"));
        assert!(keys.contains(&"test_warn_once_distinct_keys_all_fire:c"));
    }

    #[test]
    fn test_warn_once_keys_snapshot_is_stable() {
        warn_once!(
            "test_warn_once_keys_snapshot_is_stable:unique",
            "unique msg"
        );
        let keys = warn_once_keys();
        assert!(keys.contains(&"test_warn_once_keys_snapshot_is_stable:unique"));
    }

    #[test]
    fn log_unknown_once_fires_once() {
        log_unknown_once("test_log_unknown_once_fires_once", 999, &[0xaa, 0xbb]);
        log_unknown_once("test_log_unknown_once_fires_once", 999, &[0xaa, 0xbb]);
        log_unknown_once("test_log_unknown_once_fires_once", 999, &[0xaa, 0xbb]);
        let keys = warn_once_keys();
        assert_eq!(
            keys.iter()
                .filter(|k| **k == "test_log_unknown_once_fires_once:hexdump:999")
                .count(),
            1
        );
    }

    #[test]
    fn log_unknown_once_distinct_opcodes() {
        log_unknown_once("test_log_unknown_once_distinct_opcodes", 9001, &[0x01]);
        log_unknown_once("test_log_unknown_once_distinct_opcodes", 9002, &[0x02]);
        let keys = warn_once_keys();
        assert!(keys.contains(&"test_log_unknown_once_distinct_opcodes:hexdump:9001"));
        assert!(keys.contains(&"test_log_unknown_once_distinct_opcodes:hexdump:9002"));
    }

    #[test]
    fn intern_key_returns_same_str_for_same_input() {
        let a = intern_key("test_intern_key_same:foo".to_string());
        let b = intern_key("test_intern_key_same:foo".to_string());
        assert!(std::ptr::eq(a.as_ptr(), b.as_ptr()));
    }

    #[test]
    fn register_gap_observer_fires_on_new_key() {
        const PREFIX: &str = "test_register_gap_observer_fires_on_new_key:";
        let captured: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        register_gap_observer(Arc::new(move |key: &'static str| {
            captured_clone
                .lock()
                .expect("captured lock poisoned")
                .push(key);
        }));
        warn_once!("test_register_gap_observer_fires_on_new_key:k1", "msg");
        let seen = captured.lock().expect("captured lock poisoned");
        let filtered: Vec<&&'static str> = seen.iter().filter(|k| k.starts_with(PREFIX)).collect();
        assert!(
            filtered
                .iter()
                .any(|k| ***k == *"test_register_gap_observer_fires_on_new_key:k1"),
            "observer did not see new key; saw: {:?}",
            filtered
        );
    }

    #[test]
    fn register_gap_observer_replays_existing_keys() {
        const PREFIX: &str = "test_register_gap_observer_replays_existing_keys:";
        warn_once!(
            "test_register_gap_observer_replays_existing_keys:pre",
            "msg"
        );
        let captured: Arc<Mutex<Vec<&'static str>>> = Arc::new(Mutex::new(Vec::new()));
        let captured_clone = Arc::clone(&captured);
        register_gap_observer(Arc::new(move |key: &'static str| {
            captured_clone
                .lock()
                .expect("captured lock poisoned")
                .push(key);
        }));
        let seen = captured.lock().expect("captured lock poisoned");
        let filtered: Vec<&&'static str> = seen.iter().filter(|k| k.starts_with(PREFIX)).collect();
        assert!(
            filtered
                .iter()
                .any(|k| ***k == *"test_register_gap_observer_replays_existing_keys:pre"),
            "observer did not replay pre-existing key; saw: {:?}",
            filtered
        );
    }

    #[test]
    fn test_log_unknown_once_caps_per_channel() {
        // Unique channel name so this test doesn't pollute / get polluted
        // by other tests' use of `log_unknown_once`.
        const CH: &str = "test_log_unknown_once_caps_per_channel";
        // Fire CAP + 3 distinct msg_types for this channel.
        let cap = super::UNKNOWN_OPCODE_CAP_PER_CHANNEL;
        for i in 0..(cap as u16) + 3 {
            log_unknown_once(CH, i, &[]);
        }
        let keys = warn_once_keys();
        // The first `cap` msg_types registered as individual hexdump keys.
        let hexdump_count = keys
            .iter()
            .filter(|k| k.starts_with(&format!("{}:hexdump:", CH)))
            .count();
        assert_eq!(
            hexdump_count, cap,
            "expected exactly {} hexdump keys, got {}",
            cap, hexdump_count
        );
        // The cap overflow fired a single suppression-notice key.
        let cap_key = format!("{}:hexdump_cap", CH);
        assert!(
            keys.iter().any(|k| **k == cap_key),
            "expected suppression-notice key {} in {:?}",
            cap_key,
            keys.iter().filter(|k| k.contains(CH)).collect::<Vec<_>>()
        );
    }

    // Guard against regressions where MULTI_MEDIA_TIME (106) loses its
    // const or name-table entry and starts showing up as a --pedantic
    // "main:hexdump:106" gap again.
    #[test]
    fn main_server_multi_media_time_const_and_name() {
        assert_eq!(main_server::MULTI_MEDIA_TIME, 106);
        assert_eq!(
            message_names::main_server(main_server::MULTI_MEDIA_TIME),
            "multi_media_time"
        );
    }

    // Guard against regressions where STREAM_REPORT (102) loses its
    // const or name-table entry and starts appearing as an unknown
    // display_client opcode in traffic logs.
    #[test]
    fn display_client_stream_report_const_and_name() {
        assert_eq!(display_client::STREAM_REPORT, 102);
        assert_eq!(message_names::display_client(102), "stream_report");
    }

    // The main_server (SPICE_MSG_MAIN_*) table was completed against
    // enums.h; a downstream firewall treats a "unknown" name as an
    // invalid message, so every opcode enums.h defines must resolve to
    // a real name. Values are from spice-protocol/spice/enums.h
    // (MIGRATE_BEGIN=101, auto-incrementing to
    // MIGRATE_DST_SEAMLESS_NACK=118).
    #[test]
    fn main_server_full_opcode_table_const_and_name() {
        for (op, value, name) in [
            (main_server::MIGRATE_BEGIN, 101, "migrate_begin"),
            (main_server::MIGRATE_CANCEL, 102, "migrate_cancel"),
            (main_server::MIGRATE_SWITCH_HOST, 111, "migrate_switch_host"),
            (main_server::MIGRATE_END, 112, "migrate_end"),
            (main_server::NAME, 113, "name"),
            (main_server::UUID, 114, "uuid"),
            (
                main_server::AGENT_CONNECTED_TOKENS,
                115,
                "agent_connected_tokens",
            ),
            (
                main_server::MIGRATE_BEGIN_SEAMLESS,
                116,
                "migrate_begin_seamless",
            ),
            (
                main_server::MIGRATE_DST_SEAMLESS_ACK,
                117,
                "migrate_dst_seamless_ack",
            ),
            (
                main_server::MIGRATE_DST_SEAMLESS_NACK,
                118,
                "migrate_dst_seamless_nack",
            ),
        ] {
            assert_eq!(op, value, "unexpected value for {}", name);
            assert_eq!(message_names::main_server(op), name);
            assert_ne!(message_names::main_server(op), "unknown");
        }
    }

    // main_client (SPICE_MSGC_MAIN_*) completed against enums.h
    // (CLIENT_INFO=101 auto-incrementing to QUALITY_INDICATOR=112).
    // CLIENT_INFO in particular was the motivating gap: a real message
    // being rejected as "unknown".
    #[test]
    fn main_client_full_opcode_table_const_and_name() {
        assert_eq!(main_client::CLIENT_INFO, 101);
        assert_eq!(
            message_names::main_client(main_client::CLIENT_INFO),
            "client_info"
        );
        for (op, value, name) in [
            (main_client::CLIENT_INFO, 101, "client_info"),
            (main_client::MIGRATE_CONNECTED, 102, "migrate_connected"),
            (
                main_client::MIGRATE_CONNECT_ERROR,
                103,
                "migrate_connect_error",
            ),
            (main_client::MIGRATE_END, 109, "migrate_end"),
            (
                main_client::MIGRATE_DST_DO_SEAMLESS,
                110,
                "migrate_dst_do_seamless",
            ),
            (
                main_client::MIGRATE_CONNECTED_SEAMLESS,
                111,
                "migrate_connected_seamless",
            ),
            (main_client::QUALITY_INDICATOR, 112, "quality_indicator"),
        ] {
            assert_eq!(op, value, "unexpected value for {}", name);
            assert_eq!(message_names::main_client(op), name);
            assert_ne!(message_names::main_client(op), "unknown");
        }
    }

    // Guard the INVAL mislabel fix: enums.h assigns 106=INVAL_ALL_PIXMAPS,
    // 107=INVAL_PALETTE, 108=INVAL_ALL_PALETTES consecutively. The crate
    // previously defined INVAL_ALL_PIXMAPS=108 (really INVAL_ALL_PALETTES).
    #[test]
    fn display_server_inval_opcodes_const_and_name() {
        assert_eq!(display_server::INVAL_ALL_PIXMAPS, 106);
        assert_eq!(display_server::INVAL_PALETTE, 107);
        assert_eq!(display_server::INVAL_ALL_PALETTES, 108);
        assert_eq!(message_names::display_server(106), "inval_all_pixmaps");
        assert_eq!(message_names::display_server(107), "inval_palette");
        assert_eq!(message_names::display_server(108), "inval_all_palettes");
    }

    // display_server (SPICE_MSG_DISPLAY_*) gained QUALITY_INDICATOR=322
    // and GL_SCANOUT2_UNIX=323 from enums.h.
    #[test]
    fn display_server_gl_and_quality_const_and_name() {
        assert_eq!(display_server::QUALITY_INDICATOR, 322);
        assert_eq!(display_server::GL_SCANOUT2_UNIX, 323);
        assert_eq!(message_names::display_server(322), "quality_indicator");
        assert_eq!(message_names::display_server(323), "gl_scanout2_unix");
        assert_ne!(message_names::display_server(322), "unknown");
        assert_ne!(message_names::display_server(323), "unknown");
    }

    // Guard against regressions where the new multi-codec display
    // capability bit positions shift or lose their name-table
    // entries, which would cause the traffic viewer to emit
    // unlabelled cap bits in session logs.
    #[test]
    fn display_cap_name_multi_codec_bits() {
        use crate::constants::capabilities;
        // Verify bit positions match the constants.rs definitions.
        assert_eq!(capabilities::DISPLAY_MULTI_CODEC, 1 << 8);
        assert_eq!(capabilities::DISPLAY_CODEC_MJPEG, 1 << 9);
        assert_eq!(capabilities::DISPLAY_CODEC_H264, 1 << 11);
        // Verify the name-table returns the expected strings.
        assert_eq!(message_names::display_cap_name(8), Some("multi_codec"),);
        assert_eq!(message_names::display_cap_name(9), Some("codec_mjpeg"),);
        assert_eq!(message_names::display_cap_name(11), Some("codec_h264"),);
        // Bit 10 is not allocated (VP8 in the SPICE spec but not
        // advertised); verify we return None for it.
        assert_eq!(message_names::display_cap_name(10), None);
    }

    // Guard against DEFAULT_DISPLAY accidentally dropping any of the
    // three new codec caps, which would silently stop the server
    // from offering H.264 streams.
    #[test]
    fn default_display_includes_codec_caps() {
        use crate::constants::capabilities;
        let d = capabilities::DEFAULT_DISPLAY;
        assert_ne!(
            d & capabilities::DISPLAY_MULTI_CODEC,
            0,
            "DEFAULT_DISPLAY must include DISPLAY_MULTI_CODEC"
        );
        assert_ne!(
            d & capabilities::DISPLAY_CODEC_MJPEG,
            0,
            "DEFAULT_DISPLAY must include DISPLAY_CODEC_MJPEG"
        );
        assert_ne!(
            d & capabilities::DISPLAY_CODEC_H264,
            0,
            "DEFAULT_DISPLAY must include DISPLAY_CODEC_H264"
        );
    }
}
