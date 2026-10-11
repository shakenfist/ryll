#![no_main]

use libfuzzer_sys::fuzz_target;
use shakenfist_spice_protocol::constants::vd_agent::{VD_AGENT_MAX_DATA_SIZE, VD_AGENT_PROTOCOL};
use shakenfist_spice_protocol::messages::agent_stream::{AgentReassembler, AgentStreamItem};

// The guest agent reassembler over an untrusted AGENT_DATA stream. The first
// two input bytes pick a small cap, so oversized messages are reachable, and
// the third seeds the AGENT_DATA lengths the rest is cut into. Whatever the
// input, every message must carry exactly the body its header declared,
// within the cap. Unless a bad header made a run drop part of an AGENT_DATA,
// cutting the stream differently must not change what comes out.

fn run(cap: usize, stream: &[u8], mut seed: u32) -> Vec<AgentStreamItem> {
    let mut r = AgentReassembler::new(cap);
    let mut out = Vec::new();
    let mut rest = stream;
    while !rest.is_empty() {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let len = 1 + (seed >> 8) as usize % VD_AGENT_MAX_DATA_SIZE as usize;
        let (chunk, tail) = rest.split_at(len.min(rest.len()));
        out.extend(r.push(chunk));
        rest = tail;
    }
    out
}

fn has_bad_protocol(items: &[AgentStreamItem]) -> bool {
    items
        .iter()
        .any(|i| matches!(i, AgentStreamItem::BadProtocol(_)))
}

fuzz_target!(|data: &[u8]| {
    let [a, b, seed, stream @ ..] = data else {
        return;
    };
    let cap = usize::from(u16::from_le_bytes([*a, *b]) % 8192);

    let chunked = run(cap, stream, u32::from(*seed));
    for item in &chunked {
        match item {
            AgentStreamItem::Message(m) => {
                assert_eq!(m.header.protocol, VD_AGENT_PROTOCOL);
                assert_eq!(m.payload.len(), m.header.size as usize);
                assert!(m.payload.len() <= cap);
            }
            AgentStreamItem::Oversized(h) => assert!(h.size as usize > cap),
            AgentStreamItem::BadProtocol(h) => assert_ne!(h.protocol, VD_AGENT_PROTOCOL),
        }
    }

    let mut whole = AgentReassembler::new(cap);
    let whole = whole.push(stream);
    if !has_bad_protocol(&chunked) && !has_bad_protocol(&whole) {
        assert_eq!(chunked, whole);
    }
});
