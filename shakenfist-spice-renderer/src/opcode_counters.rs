//! Bounded per-opcode message counters shared by every channel handler.
//!
//! Each channel keeps a count of how many messages it has received and
//! sent per SPICE opcode, plus the most recent opcode that reached its
//! catch-all arm. Those four values are mirrored into the channel's
//! `*Snapshot` and end up in a bug report.
//!
//! ## Why this is bounded
//!
//! The receive opcode is a `u16` chosen by the *server*. A hostile or
//! broken server can send ~65 000 header-only messages with distinct
//! opcodes — roughly 400 KB of traffic, once — and an unbounded map
//! would then hold 65 000 nodes that get cloned into the snapshot on
//! every publish, under a mutex the GUI and auto-snapshot threads also
//! take. So only opcodes this build has a protocol name for are given
//! a map entry; everything else folds into `last_unknown` /
//! `unknown_count`, which exist for exactly that purpose. The name
//! tables in `shakenfist_spice_protocol::logging::message_names` are
//! the source of truth for "known", so the bound tracks the protocol
//! rather than a second hand-maintained list. A distinct-key cap of
//! [`MAX_TRACKED_OPCODES`] backstops both maps in case a name table
//! ever grows past it.
//!
//! ## Publication cost
//!
//! `publish_into` is called once per read batch *and* once per send,
//! so it is on the hot path. Two things keep it cheap: the maps are
//! bounded above (tens of entries, not tens of thousands), and each
//! map is only re-cloned when a `record_*` call has actually changed
//! it since the last publish — a send never touches the receive map,
//! and an idle republish touches neither.

use std::cell::Cell;
use std::collections::BTreeMap;

use crate::snapshots::{
    CursorSnapshot, DisplaySnapshot, InputsSnapshot, MainSnapshot, PlaybackSnapshot,
    UsbredirSnapshot, WebdavSnapshot,
};

/// The name every `message_names::*` function returns for an opcode
/// this build does not recognise. Matching on it is how
/// [`OpcodeCounters`] decides whether an opcode is server-controlled
/// junk or a real protocol message worth its own map entry.
const UNKNOWN_OPCODE_NAME: &str = "unknown";

/// Hard ceiling on distinct opcodes tracked in either map.
///
/// The largest name table (`display_server`) has 38 entries, so 64
/// leaves headroom for protocol growth while keeping a map clone to a
/// handful of nodes. Reaching it means a name table outgrew this
/// constant; opcodes beyond the cap fold into `unknown_count` rather
/// than growing the map.
pub const MAX_TRACKED_OPCODES: usize = 64;

/// Maps an opcode to its protocol name, returning
/// [`UNKNOWN_OPCODE_NAME`] for opcodes this build does not recognise.
/// Always one of `shakenfist_spice_protocol::logging::message_names`'
/// per-channel functions.
pub type OpcodeNamer = fn(u16) -> &'static str;

/// Borrowed handles to the four opcode fields of a channel snapshot.
///
/// Exists so [`OpcodeCounters::publish_into`] has one implementation
/// rather than one copy per channel; see [`OpcodeSnapshotTarget`].
pub struct OpcodeFieldsMut<'a> {
    pub recv: &'a mut BTreeMap<u16, u64>,
    pub send: &'a mut BTreeMap<u16, u64>,
    pub last_unknown: &'a mut Option<u16>,
    pub unknown_count: &'a mut u64,
}

/// A channel snapshot that carries the four opcode fields.
///
/// Implemented by every `*Snapshot` type via
/// `impl_opcode_snapshot_target!` below. The field *names* are part of
/// the bug-report JSON contract, so they stay as plain fields on each
/// snapshot struct rather than being folded into a nested type.
pub trait OpcodeSnapshotTarget {
    fn opcode_fields_mut(&mut self) -> OpcodeFieldsMut<'_>;
}

macro_rules! impl_opcode_snapshot_target {
    ($($t:ty),+ $(,)?) => {
        $(
            impl OpcodeSnapshotTarget for $t {
                fn opcode_fields_mut(&mut self) -> OpcodeFieldsMut<'_> {
                    OpcodeFieldsMut {
                        recv: &mut self.messages_recv_by_opcode,
                        send: &mut self.messages_send_by_opcode,
                        last_unknown: &mut self.last_unknown_opcode,
                        unknown_count: &mut self.unknown_opcode_count,
                    }
                }
            }
        )+
    };
}

impl_opcode_snapshot_target!(
    CursorSnapshot,
    DisplaySnapshot,
    InputsSnapshot,
    MainSnapshot,
    PlaybackSnapshot,
    UsbredirSnapshot,
    WebdavSnapshot,
);

/// Per-channel opcode counters. One instance per channel handler,
/// constructed with that channel's server and client name tables.
pub struct OpcodeCounters {
    recv_namer: OpcodeNamer,
    send_namer: OpcodeNamer,
    recv: BTreeMap<u16, u64>,
    send: BTreeMap<u16, u64>,
    last_unknown: Option<u16>,
    unknown_count: u64,
    /// Bumped on every map-mutating `record_*` call. Compared against
    /// the published generation so an unchanged map is not re-cloned.
    recv_generation: u64,
    send_generation: u64,
    /// Generation last written into the snapshot. `None` until the
    /// first publish, so a fresh channel always overwrites whatever a
    /// previous channel instance left in a shared snapshot.
    published_recv_generation: Cell<Option<u64>>,
    published_send_generation: Cell<Option<u64>>,
}

impl OpcodeCounters {
    /// Create counters for a channel.
    ///
    /// `recv_namer` and `send_namer` are the channel's server and
    /// client `message_names` functions; they define which opcodes get
    /// their own map entry.
    pub fn new(recv_namer: OpcodeNamer, send_namer: OpcodeNamer) -> Self {
        Self {
            recv_namer,
            send_namer,
            recv: BTreeMap::new(),
            send: BTreeMap::new(),
            last_unknown: None,
            unknown_count: 0,
            recv_generation: 0,
            send_generation: 0,
            published_recv_generation: Cell::new(None),
            published_send_generation: Cell::new(None),
        }
    }

    /// Count one received message. Call before dispatch so known and
    /// unknown opcodes are counted uniformly.
    ///
    /// Opcodes with no protocol name are server-controlled and
    /// unbounded in number, so they never get a map entry; they fold
    /// into `last_unknown` / `unknown_count` instead.
    pub fn record_recv(&mut self, opcode: u16) {
        if (self.recv_namer)(opcode) == UNKNOWN_OPCODE_NAME || !bump_bounded(&mut self.recv, opcode)
        {
            self.fold_unknown(opcode);
            return;
        }
        self.recv_generation = self.recv_generation.wrapping_add(1);
    }

    /// Count one sent message. Call from the channel's single send
    /// path.
    ///
    /// Send opcodes are chosen by this client, not by the server, so
    /// this map cannot be grown by a hostile peer; the distinct-key
    /// cap is belt and braces.
    pub fn record_send(&mut self, opcode: u16) {
        debug_assert_ne!(
            (self.send_namer)(opcode),
            UNKNOWN_OPCODE_NAME,
            "client sent opcode {opcode} with no entry in its message_names table",
        );
        if bump_bounded(&mut self.send, opcode) {
            self.send_generation = self.send_generation.wrapping_add(1);
        }
    }

    /// Record an opcode that reached the handler's catch-all arm —
    /// i.e. a protocol-coverage gap rather than server junk.
    ///
    /// Opcodes with no protocol name were already folded by
    /// [`record_recv`][Self::record_recv]; counting them again here
    /// would double them, so only *named but unhandled* opcodes are
    /// counted.
    pub fn note_unknown(&mut self, opcode: u16) {
        if (self.recv_namer)(opcode) != UNKNOWN_OPCODE_NAME {
            self.fold_unknown(opcode);
        }
    }

    fn fold_unknown(&mut self, opcode: u16) {
        self.unknown_count = self.unknown_count.saturating_add(1);
        self.last_unknown = Some(opcode);
    }

    /// Mirror the counters into a channel snapshot.
    ///
    /// Each map is cloned only when a `record_*` call changed it since
    /// the last publish; the two scalars are always written, being
    /// free to copy.
    pub fn publish_into<T: OpcodeSnapshotTarget + ?Sized>(&self, target: &mut T) {
        let fields = target.opcode_fields_mut();
        if self.published_recv_generation.get() != Some(self.recv_generation) {
            fields.recv.clone_from(&self.recv);
            self.published_recv_generation
                .set(Some(self.recv_generation));
        }
        if self.published_send_generation.get() != Some(self.send_generation) {
            fields.send.clone_from(&self.send);
            self.published_send_generation
                .set(Some(self.send_generation));
        }
        *fields.last_unknown = self.last_unknown;
        *fields.unknown_count = self.unknown_count;
    }

    /// Per-opcode receive counts. Test and diagnostic accessor.
    pub fn recv_by_opcode(&self) -> &BTreeMap<u16, u64> {
        &self.recv
    }

    /// Per-opcode send counts. Test and diagnostic accessor.
    pub fn send_by_opcode(&self) -> &BTreeMap<u16, u64> {
        &self.send
    }

    /// Most recent opcode folded into the unknown counters.
    pub fn last_unknown(&self) -> Option<u16> {
        self.last_unknown
    }

    /// Total opcodes folded into the unknown counters this session.
    pub fn unknown_count(&self) -> u64 {
        self.unknown_count
    }
}

/// Increment `map[opcode]`, inserting a new key only while the map is
/// below [`MAX_TRACKED_OPCODES`]. Returns `false` when the cap refused
/// a new key, so the caller can fold the opcode elsewhere.
fn bump_bounded(map: &mut BTreeMap<u16, u64>, opcode: u16) -> bool {
    if let Some(count) = map.get_mut(&opcode) {
        *count = count.saturating_add(1);
        return true;
    }
    if map.len() >= MAX_TRACKED_OPCODES {
        return false;
    }
    map.insert(opcode, 1);
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use shakenfist_spice_protocol::cursor_server;
    use shakenfist_spice_protocol::logging::message_names;

    fn cursor_counters() -> OpcodeCounters {
        OpcodeCounters::new(message_names::cursor_server, message_names::cursor_client)
    }

    #[test]
    fn known_recv_opcodes_are_counted_per_opcode() {
        let mut c = cursor_counters();
        c.record_recv(cursor_server::SET);
        c.record_recv(cursor_server::SET);
        c.record_recv(cursor_server::MOVE);

        assert_eq!(c.recv_by_opcode().get(&cursor_server::SET), Some(&2));
        assert_eq!(c.recv_by_opcode().get(&cursor_server::MOVE), Some(&1));
        assert_eq!(c.unknown_count(), 0);
        assert_eq!(c.last_unknown(), None);
    }

    #[test]
    fn unnamed_recv_opcodes_never_grow_the_map() {
        let mut c = cursor_counters();
        // The DoS the bound exists for: every distinct u16 the name
        // table does not know, one message each.
        for opcode in 0u16..=u16::MAX {
            if message_names::cursor_server(opcode) == UNKNOWN_OPCODE_NAME {
                c.record_recv(opcode);
            }
        }

        assert!(
            c.recv_by_opcode().is_empty(),
            "unnamed opcodes must not be given map entries, got {} of them",
            c.recv_by_opcode().len(),
        );
        assert!(c.unknown_count() > 65_000, "all of them must be counted");
        assert_eq!(c.last_unknown(), Some(u16::MAX));
    }

    #[test]
    fn note_unknown_does_not_double_count_unnamed_opcodes() {
        let mut c = cursor_counters();
        // 0xBEEF has no cursor_server name, so record_recv folds it
        // and the handler's catch-all arm must not fold it again.
        c.record_recv(0xBEEF);
        c.note_unknown(0xBEEF);
        assert_eq!(c.unknown_count(), 1);

        // A *named* opcode that reaches the catch-all arm is a real
        // coverage gap and is counted there.
        c.record_recv(cursor_server::TRAIL);
        c.note_unknown(cursor_server::TRAIL);
        assert_eq!(c.unknown_count(), 2);
        assert_eq!(c.last_unknown(), Some(cursor_server::TRAIL));
    }

    #[test]
    fn publish_mirrors_all_four_fields() {
        let mut c = cursor_counters();
        c.record_recv(cursor_server::SET);
        c.record_send(shakenfist_spice_protocol::cursor_client::ACK);
        c.record_recv(0xBEEF);

        let mut snap = CursorSnapshot::default();
        c.publish_into(&mut snap);

        assert_eq!(
            snap.messages_recv_by_opcode.get(&cursor_server::SET),
            Some(&1)
        );
        assert_eq!(
            snap.messages_send_by_opcode
                .get(&shakenfist_spice_protocol::cursor_client::ACK),
            Some(&1),
        );
        assert_eq!(snap.last_unknown_opcode, Some(0xBEEF));
        assert_eq!(snap.unknown_opcode_count, 1);
    }

    #[test]
    fn publish_is_idempotent_and_tracks_later_changes() {
        let mut c = cursor_counters();
        let mut snap = CursorSnapshot::default();

        c.record_recv(cursor_server::SET);
        c.publish_into(&mut snap);
        // Second publish with nothing changed: the generation guard
        // skips the clone, so the snapshot must still be correct.
        c.publish_into(&mut snap);
        assert_eq!(
            snap.messages_recv_by_opcode.get(&cursor_server::SET),
            Some(&1)
        );

        c.record_recv(cursor_server::SET);
        c.publish_into(&mut snap);
        assert_eq!(
            snap.messages_recv_by_opcode.get(&cursor_server::SET),
            Some(&2)
        );
    }

    #[test]
    fn first_publish_overwrites_a_stale_snapshot() {
        // A reconnect builds a fresh channel over the snapshot the
        // previous one left behind; the first publish must clear it
        // even though the new counters have recorded nothing.
        let mut snap = CursorSnapshot::default();
        snap.messages_recv_by_opcode.insert(0x1234, 99);
        snap.messages_send_by_opcode.insert(0x5678, 99);

        let c = cursor_counters();
        c.publish_into(&mut snap);

        assert!(snap.messages_recv_by_opcode.is_empty());
        assert!(snap.messages_send_by_opcode.is_empty());
    }

    #[test]
    fn distinct_key_cap_bounds_the_map() {
        // A namer that claims to know every opcode, to exercise the
        // backstop cap independently of any real name table.
        fn always_known(_: u16) -> &'static str {
            "synthetic"
        }
        let mut c = OpcodeCounters::new(always_known, always_known);
        for opcode in 0..(MAX_TRACKED_OPCODES as u16 * 2) {
            c.record_recv(opcode);
        }

        assert_eq!(c.recv_by_opcode().len(), MAX_TRACKED_OPCODES);
        assert_eq!(c.unknown_count(), MAX_TRACKED_OPCODES as u64);
    }
}
