//! Outgoing guest-agent data, released one `AGENT_DATA` chunk per server
//! token.
//!
//! A `VDAgentMessage` larger than one chunk is spread over several
//! `AGENT_DATA` messages, and spice-server reassembles them by counting
//! bytes against the size in the agent header (`agent-msg-filter.c`). Once
//! the first chunk of a message is on the wire, every remaining byte of it
//! must follow before anything else: if the client stops part-way, the
//! next message is read as the tail of the abandoned one, and when its
//! first chunk overruns what the server expected the server disconnects
//! the client (issue #447). So a message that cannot be sent in full now
//! waits here for `AGENT_TOKEN` rather than being cut short.

use std::collections::VecDeque;

/// Largest `AGENT_DATA` body we send: `VD_AGENT_MAX_DATA_SIZE` (2048)
/// less a 6-byte margin. spice-server rejects anything over 2048.
pub(crate) const MAX_AGENT_CHUNK: usize = 2048 - 6;

/// How many whole agent messages may wait for tokens at once.
///
/// Tokens only stop coming back when the guest agent stops reading, and
/// queueing behind a stalled agent achieves nothing, so this is a guard
/// against unbounded growth rather than a working depth: in practice the
/// queue holds at most the tail of one large clipboard transfer.
pub(crate) const MAX_QUEUED_AGENT_MESSAGES: usize = 32;

struct PendingMessage {
    /// The complete `VDAgentMessage`, header included.
    data: Vec<u8>,
    /// Bytes of `data` already sent.
    sent: usize,
}

#[derive(Default)]
pub(crate) struct AgentSendQueue {
    messages: VecDeque<PendingMessage>,
    /// Messages whose last chunk has been handed to the writer, since
    /// the queue was made. A caller that needs to know whether a
    /// message it queued has actually left ryll records
    /// `delivered() + len()` just after pushing it -- its message is
    /// the last of those still waiting -- and watches for `delivered()`
    /// to reach that figure. `discard_unstarted` never advances it, so
    /// a message dropped with the agent it was meant for is reported
    /// as never delivered, which is what it is.
    delivered: u64,
}

impl AgentSendQueue {
    /// Queue a complete `VDAgentMessage`. Returns false, leaving the queue
    /// unchanged, if `MAX_QUEUED_AGENT_MESSAGES` are already waiting.
    pub(crate) fn push(&mut self, message: Vec<u8>) -> bool {
        if self.messages.len() >= MAX_QUEUED_AGENT_MESSAGES {
            return false;
        }
        if !message.is_empty() {
            self.messages.push_back(PendingMessage {
                data: message,
                sent: 0,
            });
        }
        true
    }

    /// The next `AGENT_DATA` body to send, if any. The caller must send it
    /// and spend one token; chunks come out strictly in order.
    pub(crate) fn next_chunk(&mut self) -> Option<Vec<u8>> {
        let front = self.messages.front_mut()?;
        let end = (front.sent + MAX_AGENT_CHUNK).min(front.data.len());
        let chunk = front.data[front.sent..end].to_vec();
        front.sent = end;
        if front.sent == front.data.len() {
            self.messages.pop_front();
            self.delivered = self.delivered.saturating_add(1);
        }
        Some(chunk)
    }

    /// Drop messages meant for an agent that has gone away, keeping the
    /// tail of a message that is already partly on the wire.
    ///
    /// spice-server discards client data while no agent is attached
    /// (`reds_reset_vdp` sets `discard_all`), but its filter still counts
    /// the bytes owed for a message it has seen the start of. Dropping that
    /// tail would desync the stream for the next agent exactly as running
    /// out of tokens used to.
    pub(crate) fn discard_unstarted(&mut self) {
        let keep = usize::from(self.messages.front().is_some_and(|m| m.sent > 0));
        self.messages.truncate(keep);
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Number of whole or partial messages still waiting.
    pub(crate) fn len(&self) -> usize {
        self.messages.len()
    }

    /// Messages fully handed to the writer so far; see `delivered`.
    pub(crate) fn delivered(&self) -> u64 {
        self.delivered
    }
}

#[cfg(test)]
mod tests {
    use super::{AgentSendQueue, MAX_AGENT_CHUNK, MAX_QUEUED_AGENT_MESSAGES};

    fn message(len: usize, fill: u8) -> Vec<u8> {
        vec![fill; len]
    }

    /// Drain up to `tokens` chunks, as the main channel does per token
    /// grant.
    fn drain(queue: &mut AgentSendQueue, tokens: usize) -> Vec<Vec<u8>> {
        (0..tokens).map_while(|_| queue.next_chunk()).collect()
    }

    #[test]
    fn small_message_is_one_chunk() {
        let mut queue = AgentSendQueue::default();
        assert!(queue.push(message(28, 1)));
        assert_eq!(drain(&mut queue, 10), vec![message(28, 1)]);
        assert!(queue.is_empty());
    }

    #[test]
    fn large_message_resumes_where_tokens_ran_out() {
        // Session 015: a 15807-byte CLIPBOARD message (8 chunks) with 7
        // tokens left. The eighth chunk must wait for AGENT_TOKEN, not be
        // dropped, and must go out before anything queued after it.
        let mut queue = AgentSendQueue::default();
        let big: Vec<u8> = (0..15807u32).map(|i| i as u8).collect();
        assert!(queue.push(big.clone()));
        assert!(queue.push(message(28, 0xee)));

        let first = drain(&mut queue, 7);
        assert_eq!(first.len(), 7);
        assert!(first.iter().all(|c| c.len() == MAX_AGENT_CHUNK));
        assert!(!queue.is_empty());

        // Tokens come back.
        let rest = drain(&mut queue, 10);
        assert_eq!(rest.len(), 2);
        assert_eq!(rest[0].len(), 15807 - 7 * MAX_AGENT_CHUNK);
        assert_eq!(rest[1], message(28, 0xee));

        let reassembled: Vec<u8> = first.into_iter().chain(rest).flatten().collect();
        assert_eq!(&reassembled[..big.len()], &big[..]);
        assert!(queue.is_empty());
    }

    // `delivered` is what tells the main channel whether a message it
    // queued has left ryll: a resize still waiting for tokens has not
    // been offered to the guest, so the guest cannot be blamed for not
    // applying it. It counts whole messages, so a large message part
    // way out does not count, and the figure a caller recorded at push
    // time stays meaningful as other messages queue behind it.
    #[test]
    fn delivered_counts_only_whole_messages_as_they_go_out() {
        let mut queue = AgentSendQueue::default();
        assert_eq!(queue.delivered(), 0);

        // A monitors config behind a two-chunk clipboard message: the
        // target to watch for is 2.
        assert!(queue.push(message(2 * MAX_AGENT_CHUNK, 1)));
        assert!(queue.push(message(28, 2)));
        let target = queue.delivered() + queue.len() as u64;
        assert_eq!(target, 2);

        assert_eq!(drain(&mut queue, 1).len(), 1);
        assert_eq!(
            queue.delivered(),
            0,
            "a message part way out has not been delivered"
        );
        assert_eq!(drain(&mut queue, 1).len(), 1);
        assert_eq!(queue.delivered(), 1);
        assert!(queue.delivered() < target, "the resize is still queued");

        assert_eq!(drain(&mut queue, 1).len(), 1);
        assert_eq!(queue.delivered(), target);
    }

    // A message dropped with the agent it was meant for was never
    // offered to any guest, so it must not count as delivered.
    #[test]
    fn discarding_the_queue_delivers_nothing() {
        let mut queue = AgentSendQueue::default();
        assert!(queue.push(message(28, 1)));
        let target = queue.delivered() + queue.len() as u64;

        queue.discard_unstarted();

        assert!(queue.is_empty());
        assert_eq!(queue.delivered(), 0);
        assert!(queue.delivered() < target);
    }

    #[test]
    fn exact_multiple_of_chunk_size_leaves_no_empty_chunk() {
        let mut queue = AgentSendQueue::default();
        assert!(queue.push(message(2 * MAX_AGENT_CHUNK, 7)));
        assert_eq!(drain(&mut queue, 10).len(), 2);
        assert!(queue.is_empty());
    }

    #[test]
    fn push_refuses_past_the_cap() {
        let mut queue = AgentSendQueue::default();
        for _ in 0..MAX_QUEUED_AGENT_MESSAGES {
            assert!(queue.push(message(8, 0)));
        }
        assert!(!queue.push(message(8, 0)));
        assert_eq!(queue.len(), MAX_QUEUED_AGENT_MESSAGES);
    }

    #[test]
    fn discard_unstarted_keeps_a_partly_sent_tail() {
        let mut queue = AgentSendQueue::default();
        assert!(queue.push(message(3 * MAX_AGENT_CHUNK, 1)));
        assert!(queue.push(message(28, 2)));
        assert!(queue.push(message(28, 3)));
        assert_eq!(drain(&mut queue, 1).len(), 1);

        queue.discard_unstarted();
        let rest = drain(&mut queue, 10);
        assert_eq!(rest, vec![message(MAX_AGENT_CHUNK, 1); 2]);
        assert!(queue.is_empty());
    }

    #[test]
    fn discard_unstarted_drops_everything_when_nothing_is_on_the_wire() {
        let mut queue = AgentSendQueue::default();
        assert!(queue.push(message(28, 1)));
        assert!(queue.push(message(28, 2)));
        queue.discard_unstarted();
        assert!(queue.is_empty());
    }
}
