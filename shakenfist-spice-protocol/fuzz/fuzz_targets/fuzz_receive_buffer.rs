#![no_main]

use libfuzzer_sys::fuzz_target;
use shakenfist_spice_protocol::messages::{MessageHeader, ReceiveBuffer};

// Message framing over an untrusted byte stream. The first two input bytes
// pick a small body limit, so oversized messages are reachable, and the third
// seeds the read sizes the rest is cut into. Whatever the cutting, the buffer
// must frame exactly what a plain header-then-body walk of the whole stream
// frames, stop with an error at the same oversized header, and leave the same
// partial message waiting.

/// What framing the whole stream produced: the messages, and whether it ended
/// on an oversized header.
struct Framed {
    messages: Vec<Vec<u8>>,
    refused: bool,
    waiting: Vec<u8>,
}

fn reference(stream: &[u8], max_body: usize) -> Framed {
    let mut messages = Vec::new();
    let mut at = 0;
    loop {
        let rest = &stream[at..];
        if rest.len() < MessageHeader::SIZE {
            break;
        }
        let size = u32::from_le_bytes([rest[2], rest[3], rest[4], rest[5]]) as usize;
        if size > max_body {
            return Framed {
                messages,
                refused: true,
                waiting: Vec::new(),
            };
        }
        let total = MessageHeader::SIZE + size;
        if rest.len() < total {
            break;
        }
        messages.push(rest[..total].to_vec());
        at += total;
    }
    Framed {
        messages,
        refused: false,
        waiting: stream[at..].to_vec(),
    }
}

fn chunked(stream: &[u8], max_body: usize, mut seed: u32) -> Framed {
    let mut buffer = ReceiveBuffer::new();
    let mut messages = Vec::new();
    let mut rest = stream;
    while !rest.is_empty() {
        seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12_345);
        let len = 1 + (seed >> 8) as usize % 512;
        let (chunk, tail) = rest.split_at(len.min(rest.len()));
        buffer.extend_from_slice(chunk);
        rest = tail;
        loop {
            match buffer.take_message(max_body) {
                Ok(Some(message)) => {
                    assert_eq!(
                        message.payload().len(),
                        message.header.message_size as usize
                    );
                    messages.push(message.raw);
                }
                Ok(None) => break,
                Err(_) => {
                    return Framed {
                        messages,
                        refused: true,
                        waiting: Vec::new(),
                    }
                }
            }
        }
    }
    Framed {
        messages,
        refused: false,
        waiting: buffer.unconsumed().to_vec(),
    }
}

fuzz_target!(|data: &[u8]| {
    let [a, b, seed, stream @ ..] = data else {
        return;
    };
    let max_body = usize::from(u16::from_le_bytes([*a, *b]) % 1024);

    let expected = reference(stream, max_body);
    let got = chunked(stream, max_body, u32::from(*seed));
    assert_eq!(got.messages, expected.messages);
    assert_eq!(got.refused, expected.refused);
    assert_eq!(got.waiting, expected.waiting);
});
