#![no_main]

use libfuzzer_sys::fuzz_target;

// SpiceLinkMess::parse faces the public internet: a client sends this as the
// very first bytes on a new TCP connection, before any authentication. The
// only property under test is "never panics" -- Ok/Err are both acceptable
// outcomes for arbitrary bytes.
fuzz_target!(|data: &[u8]| {
    let _ = shakenfist_spice_protocol::link::SpiceLinkMess::parse(data);
});
