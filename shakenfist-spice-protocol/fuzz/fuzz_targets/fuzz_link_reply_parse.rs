#![no_main]

use libfuzzer_sys::fuzz_target;

// SpiceLinkReply::parse is what a ryll client feeds untrusted bytes from a
// SPICE server (or, for kerbside's use case, a hostile man-in-the-middle)
// into immediately after connecting. As with fuzz_link_mess_parse, the only
// property under test is "never panics".
fuzz_target!(|data: &[u8]| {
    let _ = shakenfist_spice_protocol::link::SpiceLinkReply::parse(data);
});
