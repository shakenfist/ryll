#![no_main]

use libfuzzer_sys::fuzz_target;

// parse_proxy_uri parses the `.vv` file `proxy=` key -- a value taken
// from a downloaded file, not from a trusted source. Only property
// under test is "never panics" -- Ok/Err are both acceptable outcomes
// for arbitrary input.
fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let _ = shakenfist_spice_protocol::proxy::parse_proxy_uri(&s);
});
