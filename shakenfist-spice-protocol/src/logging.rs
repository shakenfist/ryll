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
            main_server::INIT => "init",
            main_server::CHANNELS_LIST => "channels_list",
            main_server::MOUSE_MODE => "mouse_mode",
            main_server::MULTI_MEDIA_TIME => "multi_media_time",
            main_server::AGENT_CONNECTED => "agent_connected",
            main_server::AGENT_DISCONNECTED => "agent_disconnected",
            main_server::AGENT_DATA => "agent_data",
            main_server::AGENT_TOKEN => "agent_token",
            _ => "unknown",
        }
    }

    /// Get main channel client message name
    pub fn main_client(msg_type: u16) -> &'static str {
        match msg_type {
            main_client::MIGRATE_FLUSH_MARK => "migrate_flush_mark",
            main_client::MIGRATE_DATA => "migrate_data",
            main_client::DISCONNECTING => "disconnecting",
            main_client::ATTACH_CHANNELS => "attach_channels",
            main_client::MOUSE_MODE_REQUEST => "mouse_mode_request",
            main_client::AGENT_START => "agent_start",
            main_client::AGENT_DATA => "agent_data",
            main_client::AGENT_TOKEN => "agent_token",
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
            display_server::STREAM_CREATE => "stream_create",
            display_server::STREAM_DATA => "stream_data",
            display_server::STREAM_CLIP => "stream_clip",
            display_server::STREAM_DESTROY => "stream_destroy",
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
            display_server::SET_ACK => "set_ack",
            display_server::PING => "ping",
            _ => "unknown",
        }
    }

    /// Get display channel client message name
    pub fn display_client(msg_type: u16) -> &'static str {
        match msg_type {
            display_client::INIT => "init",
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
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::{
        intern_key, log_unknown_once, message_names, register_gap_observer, warn_once_keys,
    };
    use crate::constants::main_server;

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
}
