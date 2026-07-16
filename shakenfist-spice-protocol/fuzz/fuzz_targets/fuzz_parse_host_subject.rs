#![no_main]

use libfuzzer_sys::fuzz_target;

// parse_host_subject is a hand-rolled escape/state-machine parser of
// operator-supplied subject strings (the `.vv` file `host-subject`
// field, oVirt's `host.certificate.subject`, and so on). Only property
// under test is "never panics" -- Ok/Err are both acceptable outcomes
// for arbitrary input.
fuzz_target!(|data: &[u8]| {
    let s = String::from_utf8_lossy(data);
    let _ = shakenfist_spice_protocol::host_subject::parse_host_subject(&s);
});
