#![no_main]

use libfuzzer_sys::fuzz_target;
use rsa::RsaPrivateKey;
use shakenfist_spice_protocol::link::{decrypt_password, generate_ticket_keypair};
use std::sync::OnceLock;

// RSA key generation is slow (order of tens of milliseconds for the
// 1024-bit key SPICE uses); generate one keypair per fuzzer process and
// reuse it across every iteration rather than regenerating per input.
static KEY: OnceLock<RsaPrivateKey> = OnceLock::new();

// decrypt_password is what a kerbside-style proxy calls on the 128-byte
// RSA-OAEP ticket blob read straight off a hostile client socket during
// auth. Only property under test: never panics for any 128-byte input.
fuzz_target!(|data: &[u8]| {
    let key = KEY.get_or_init(|| {
        generate_ticket_keypair()
            .expect("keypair generation for fuzz harness")
            .0
    });

    // decrypt_password takes a fixed [u8; 128] blob; take the first 128
    // bytes of the fuzzer input, zero-padding if shorter.
    let mut blob = [0u8; 128];
    let n = data.len().min(blob.len());
    blob[..n].copy_from_slice(&data[..n]);

    let _ = decrypt_password(key, &blob);
});
