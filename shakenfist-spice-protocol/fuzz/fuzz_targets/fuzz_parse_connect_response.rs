#![no_main]

use libfuzzer_sys::fuzz_target;

// parse_connect_response interprets the buffered header block of an
// HTTP CONNECT response from the proxy a `.vv` file names -- bytes from
// a network peer, before TLS. Only property under test is "never
// panics" -- Ok/Err are both acceptable outcomes for arbitrary bytes.
fuzz_target!(|data: &[u8]| {
    let _ = shakenfist_spice_protocol::proxy::parse_connect_response(data);
});
