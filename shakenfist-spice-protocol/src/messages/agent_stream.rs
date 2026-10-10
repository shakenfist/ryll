//! Reassembly of guest agent messages from the main channel's AGENT_DATA
//! stream.
//!
//! A guest agent message is a [`VdAgentMessageHeader`] followed by `size`
//! bytes of body. spice-server forwards guest agent data to the client in
//! AGENT_DATA messages of at most
//! [`VD_AGENT_MAX_DATA_SIZE`](crate::constants::vd_agent::VD_AGENT_MAX_DATA_SIZE)
//! (2048) bytes, so a message larger than about 2 KB -- a clipboard copy,
//! typically -- spans several of them, and only the first carries the
//! header. [`AgentReassembler`] puts those messages back together.
//!
//! It treats the AGENT_DATA bodies as one byte stream, as spice-gtk does
//! (`main_handle_agent_data_msg` in `channel-main.c`): a header may be split
//! between two AGENT_DATAs, and one AGENT_DATA may carry the end of one
//! message and the start of the next. spice-server never does either -- its
//! own filter (`agent-msg-filter.c`) starts every message on an AGENT_DATA
//! boundary -- but other servers may.
//!
//! The server is not trusted, so unlike spice-gtk the reassembler:
//!
//! - refuses a message whose header declares a body larger than its cap.
//!   The body is not buffered: its bytes are counted off and dropped as they
//!   arrive, as spice-server's filter discards a message it will not
//!   forward, so the message after it is still found;
//! - grows a message's buffer as its bytes arrive, never from the declared
//!   size alone, so a header costs nothing until the data behind it does;
//! - refuses a header whose `protocol` is not
//!   [`VD_AGENT_PROTOCOL`](crate::constants::vd_agent::VD_AGENT_PROTOCOL).
//!   spice-server's filter treats that as a protocol error and forwards
//!   nothing else, so such a header means the stream is out of step: the
//!   rest of that AGENT_DATA is dropped and reassembly restarts at the next
//!   one, where spice-server starts a message.
//!
//! A message cut short by the agent going away must not be completed by the
//! next agent's data, so the caller calls [`AgentReassembler::reset`] when the
//! agent disconnects.

use super::vd_agent::VdAgentMessageHeader;
use super::WireType;
use crate::constants::vd_agent::VD_AGENT_PROTOCOL;

/// The default cap on a reassembled message's body, in bytes.
///
/// This is spice-gtk's default `max-clipboard` (100 MiB), the largest
/// clipboard the reference client accepts, plus 4 KiB for the clipboard
/// message's own headers in front of the data. Any clipboard spice-gtk
/// would take, the reassembler takes too.
pub const MAX_AGENT_MESSAGE_SIZE: usize = 100 * 1024 * 1024 + 4096;

/// A complete guest agent message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentMessage {
    pub header: VdAgentMessageHeader,
    /// The body: exactly `header.size` bytes.
    pub payload: Vec<u8>,
}

/// What [`AgentReassembler::push`] found in the stream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStreamItem {
    /// A message is complete.
    Message(AgentMessage),
    /// A header declared a body over the cap. The body is being dropped as
    /// it arrives, and reassembly resumes after it.
    Oversized(VdAgentMessageHeader),
    /// A header carried the wrong protocol, so the stream is out of step.
    /// The rest of this AGENT_DATA was dropped, and reassembly restarts at
    /// the next one.
    BadProtocol(VdAgentMessageHeader),
}

enum State {
    /// Collecting a header, `filled` bytes of it so far.
    Header {
        buf: [u8; VdAgentMessageHeader::SIZE],
        filled: usize,
    },
    /// Collecting the body `header` declared.
    Body {
        header: VdAgentMessageHeader,
        payload: Vec<u8>,
    },
    /// Dropping the body of an oversized message.
    Skip { remaining: usize },
    /// Dropping the rest of an AGENT_DATA after a bad header.
    Resync,
}

impl State {
    fn header() -> Self {
        State::Header {
            buf: [0; VdAgentMessageHeader::SIZE],
            filled: 0,
        }
    }
}

/// Turns the bodies of successive AGENT_DATA messages back into complete
/// guest agent messages. See the module documentation.
pub struct AgentReassembler {
    max_message_size: usize,
    state: State,
}

impl Default for AgentReassembler {
    fn default() -> Self {
        AgentReassembler::new(MAX_AGENT_MESSAGE_SIZE)
    }
}

impl AgentReassembler {
    /// A reassembler that refuses bodies larger than `max_message_size`.
    #[must_use]
    pub fn new(max_message_size: usize) -> Self {
        AgentReassembler {
            max_message_size,
            state: State::header(),
        }
    }

    /// Forget any partly received message, so the next byte pushed is taken
    /// as the start of a header.
    pub fn reset(&mut self) {
        self.state = State::header();
    }

    /// True when no message is partly received: the next byte pushed starts
    /// a header.
    #[must_use]
    pub fn at_message_boundary(&self) -> bool {
        match &self.state {
            State::Header { filled, .. } => *filled == 0,
            State::Resync => true,
            State::Body { .. } | State::Skip { .. } => false,
        }
    }

    /// Feed the body of one AGENT_DATA message, and return what it
    /// completed, in stream order.
    pub fn push(&mut self, chunk: &[u8]) -> Vec<AgentStreamItem> {
        let mut out = Vec::new();
        if matches!(self.state, State::Resync) {
            self.state = State::header();
        }
        let mut rest = chunk;
        while !rest.is_empty() {
            let state = std::mem::replace(&mut self.state, State::Resync);
            self.state = self.step(state, &mut rest, &mut out);
        }
        out
    }

    /// Consume what `state` wants from the front of `rest`, and return the
    /// state that follows.
    fn step(&self, state: State, rest: &mut &[u8], out: &mut Vec<AgentStreamItem>) -> State {
        match state {
            State::Header {
                mut buf,
                mut filled,
            } => {
                let n = (VdAgentMessageHeader::SIZE - filled).min(rest.len());
                buf[filled..filled + n].copy_from_slice(&rest[..n]);
                filled += n;
                *rest = &rest[n..];
                if filled < VdAgentMessageHeader::SIZE {
                    return State::Header { buf, filled };
                }
                let header = VdAgentMessageHeader::decode(&buf).expect("length checked above");
                self.start_body(header, rest, out)
            }
            State::Body {
                header,
                mut payload,
            } => {
                let size = header.size as usize;
                let n = (size - payload.len()).min(rest.len());
                grow(&mut payload, n, size);
                payload.extend_from_slice(&rest[..n]);
                *rest = &rest[n..];
                if payload.len() < size {
                    return State::Body { header, payload };
                }
                out.push(AgentStreamItem::Message(AgentMessage { header, payload }));
                State::header()
            }
            State::Skip { remaining } => {
                let n = remaining.min(rest.len());
                *rest = &rest[n..];
                if remaining > n {
                    State::Skip {
                        remaining: remaining - n,
                    }
                } else {
                    State::header()
                }
            }
            State::Resync => {
                *rest = &[];
                State::Resync
            }
        }
    }

    /// The state after a complete `header`.
    fn start_body(
        &self,
        header: VdAgentMessageHeader,
        rest: &mut &[u8],
        out: &mut Vec<AgentStreamItem>,
    ) -> State {
        let size = header.size as usize;
        if header.protocol != VD_AGENT_PROTOCOL {
            out.push(AgentStreamItem::BadProtocol(header));
            *rest = &[];
            State::Resync
        } else if size > self.max_message_size {
            out.push(AgentStreamItem::Oversized(header));
            State::Skip { remaining: size }
        } else if size == 0 {
            out.push(AgentStreamItem::Message(AgentMessage {
                header,
                payload: Vec::new(),
            }));
            State::header()
        } else {
            State::Body {
                header,
                payload: Vec::new(),
            }
        }
    }
}

/// Make room in `payload` for `extra` more bytes of a `total`-byte body,
/// doubling as `Vec` does but never past `total`.
fn grow(payload: &mut Vec<u8>, extra: usize, total: usize) {
    let needed = payload.len() + extra;
    if needed > payload.capacity() {
        let target = needed.max(payload.capacity().saturating_mul(2)).min(total);
        payload.reserve_exact(target - payload.len());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::vd_agent::{VD_AGENT_CLIPBOARD, VD_AGENT_MAX_DATA_SIZE, VD_AGENT_REPLY};

    const CHUNK: usize = VD_AGENT_MAX_DATA_SIZE as usize;

    /// A whole agent message on the wire: header, then `payload`.
    fn wire(ty: u32, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        VdAgentMessageHeader::new(ty, payload.len() as u32).write(&mut out);
        out.extend_from_slice(payload);
        out
    }

    fn body(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i * 7 + 3) as u8).collect()
    }

    fn message(ty: u32, payload: &[u8]) -> AgentStreamItem {
        AgentStreamItem::Message(AgentMessage {
            header: VdAgentMessageHeader::new(ty, payload.len() as u32),
            payload: payload.to_vec(),
        })
    }

    /// Push `stream` in AGENT_DATAs of at most `chunk` bytes, as
    /// spice-server sends it, and collect everything that comes out.
    fn push_chunked(r: &mut AgentReassembler, stream: &[u8], chunk: usize) -> Vec<AgentStreamItem> {
        stream.chunks(chunk).flat_map(|c| r.push(c)).collect()
    }

    #[test]
    fn single_chunk_message() {
        let mut r = AgentReassembler::default();
        let payload = body(100);
        assert_eq!(
            r.push(&wire(VD_AGENT_CLIPBOARD, &payload)),
            vec![message(VD_AGENT_CLIPBOARD, &payload)]
        );
        assert!(r.at_message_boundary());
    }

    #[test]
    fn empty_body_completes_with_the_header() {
        let mut r = AgentReassembler::default();
        assert_eq!(
            r.push(&wire(VD_AGENT_REPLY, &[])),
            vec![message(VD_AGENT_REPLY, &[])]
        );
    }

    #[test]
    fn message_split_across_several_chunks() {
        // Five AGENT_DATAs, as for a ~9 KB guest clipboard copy.
        let mut r = AgentReassembler::default();
        let payload = body(9000);
        let stream = wire(VD_AGENT_CLIPBOARD, &payload);
        let chunks: Vec<&[u8]> = stream.chunks(CHUNK).collect();
        assert_eq!(chunks.len(), 5);
        for c in &chunks[..4] {
            assert_eq!(r.push(c), vec![], "nothing completes early");
            assert!(!r.at_message_boundary());
        }
        assert_eq!(
            r.push(chunks[4]),
            vec![message(VD_AGENT_CLIPBOARD, &payload)]
        );
        assert!(r.at_message_boundary());
    }

    #[test]
    fn continuation_chunks_are_not_read_as_headers() {
        // The ryll#474 symptom: the second AGENT_DATA's first 20 bytes were
        // parsed as a header. Make them look like a plausible one.
        let mut r = AgentReassembler::default();
        let mut payload = body(3000);
        let fake = wire(VD_AGENT_REPLY, &[0; 8]);
        let at = CHUNK - VdAgentMessageHeader::SIZE;
        payload[at..at + fake.len()].copy_from_slice(&fake);
        let out = push_chunked(&mut r, &wire(VD_AGENT_CLIPBOARD, &payload), CHUNK);
        assert_eq!(out, vec![message(VD_AGENT_CLIPBOARD, &payload)]);
    }

    #[test]
    fn exact_chunk_boundaries() {
        // A message filling exactly one, two and three whole AGENT_DATAs,
        // then one a byte either side of a boundary.
        for total in [CHUNK, 2 * CHUNK, 3 * CHUNK, 2 * CHUNK - 1, 2 * CHUNK + 1] {
            let mut r = AgentReassembler::default();
            let payload = body(total - VdAgentMessageHeader::SIZE);
            let stream = wire(VD_AGENT_CLIPBOARD, &payload);
            assert_eq!(stream.len(), total);
            assert_eq!(
                push_chunked(&mut r, &stream, CHUNK),
                vec![message(VD_AGENT_CLIPBOARD, &payload)],
                "total {total}"
            );
            assert!(r.at_message_boundary(), "total {total}");
        }
    }

    #[test]
    fn two_messages_in_one_stream() {
        // Each message starting on its own AGENT_DATA, as spice-server sends.
        let mut r = AgentReassembler::default();
        let first = body(5000);
        let second = body(30);
        let mut out = push_chunked(&mut r, &wire(VD_AGENT_CLIPBOARD, &first), CHUNK);
        out.extend(r.push(&wire(VD_AGENT_REPLY, &second)));
        assert_eq!(
            out,
            vec![
                message(VD_AGENT_CLIPBOARD, &first),
                message(VD_AGENT_REPLY, &second)
            ]
        );
    }

    #[test]
    fn one_chunk_may_end_one_message_and_start_the_next() {
        // spice-gtk's byte-stream behaviour: any split of the concatenated
        // stream, including inside a header, gives the same messages.
        let first = body(3000);
        let second = body(10);
        let third = body(2100);
        let mut stream = wire(VD_AGENT_CLIPBOARD, &first);
        stream.extend(wire(VD_AGENT_REPLY, &second));
        stream.extend(wire(VD_AGENT_CLIPBOARD, &third));
        let expected = vec![
            message(VD_AGENT_CLIPBOARD, &first),
            message(VD_AGENT_REPLY, &second),
            message(VD_AGENT_CLIPBOARD, &third),
        ];
        for chunk in [1, 7, 19, 20, 21, 1000, CHUNK, stream.len()] {
            let mut r = AgentReassembler::default();
            assert_eq!(
                push_chunked(&mut r, &stream, chunk),
                expected,
                "chunk {chunk}"
            );
        }
    }

    #[test]
    fn oversized_message_is_skipped_and_the_next_is_found() {
        let mut r = AgentReassembler::new(4096);
        let big = body(10_000);
        let small = body(40);
        let mut out = push_chunked(&mut r, &wire(VD_AGENT_CLIPBOARD, &big), CHUNK);
        out.extend(r.push(&wire(VD_AGENT_REPLY, &small)));
        assert_eq!(
            out,
            vec![
                AgentStreamItem::Oversized(VdAgentMessageHeader::new(VD_AGENT_CLIPBOARD, 10_000)),
                message(VD_AGENT_REPLY, &small),
            ]
        );
    }

    #[test]
    fn cap_is_inclusive() {
        let mut r = AgentReassembler::new(100);
        let payload = body(100);
        assert_eq!(
            r.push(&wire(VD_AGENT_CLIPBOARD, &payload)),
            vec![message(VD_AGENT_CLIPBOARD, &payload)]
        );
        assert_eq!(
            r.push(&wire(VD_AGENT_CLIPBOARD, &body(101))),
            vec![AgentStreamItem::Oversized(VdAgentMessageHeader::new(
                VD_AGENT_CLIPBOARD,
                101
            ))]
        );
    }

    #[test]
    fn declared_size_alone_allocates_nothing() {
        // A header claiming the whole cap, with no data behind it.
        let mut r = AgentReassembler::default();
        let mut header = Vec::new();
        VdAgentMessageHeader::new(VD_AGENT_CLIPBOARD, MAX_AGENT_MESSAGE_SIZE as u32)
            .write(&mut header);
        assert_eq!(r.push(&header), vec![]);
        let State::Body { payload, .. } = &r.state else {
            panic!("collecting the body");
        };
        assert_eq!(payload.capacity(), 0);
    }

    #[test]
    fn bad_protocol_drops_the_chunk_and_resyncs_at_the_next() {
        let mut r = AgentReassembler::default();
        let mut bad = wire(VD_AGENT_CLIPBOARD, &body(50));
        bad[0] = 9; // protocol
                    // Trailing bytes in the same AGENT_DATA are dropped with it.
        bad.extend(wire(VD_AGENT_REPLY, &[1, 2]));
        let out = r.push(&bad);
        assert_eq!(out.len(), 1);
        assert!(matches!(&out[0], AgentStreamItem::BadProtocol(h) if h.protocol == 9));
        let good = body(12);
        assert_eq!(
            r.push(&wire(VD_AGENT_REPLY, &good)),
            vec![message(VD_AGENT_REPLY, &good)]
        );
    }

    #[test]
    fn reset_drops_a_truncated_message() {
        // The agent goes away part-way through a message; the next agent's
        // first message must not be read as its tail.
        let mut r = AgentReassembler::default();
        let stream = wire(VD_AGENT_CLIPBOARD, &body(5000));
        assert_eq!(r.push(&stream[..CHUNK]), vec![]);
        assert!(!r.at_message_boundary());
        r.reset();
        assert!(r.at_message_boundary());
        let next = body(16);
        assert_eq!(
            r.push(&wire(VD_AGENT_REPLY, &next)),
            vec![message(VD_AGENT_REPLY, &next)]
        );
    }

    #[test]
    fn reset_drops_a_partial_header_and_a_skip() {
        let mut r = AgentReassembler::new(10);
        assert_eq!(r.push(&wire(VD_AGENT_CLIPBOARD, &body(5))[..7]), vec![]);
        r.reset();
        let out = r.push(&wire(VD_AGENT_CLIPBOARD, &body(50))[..30]);
        assert!(matches!(&out[..], [AgentStreamItem::Oversized(_)]));
        r.reset();
        let next = body(4);
        assert_eq!(
            r.push(&wire(VD_AGENT_REPLY, &next)),
            vec![message(VD_AGENT_REPLY, &next)]
        );
    }

    #[test]
    fn empty_push_changes_nothing() {
        let mut r = AgentReassembler::default();
        assert_eq!(r.push(&[]), vec![]);
        assert!(r.at_message_boundary());
    }
}
