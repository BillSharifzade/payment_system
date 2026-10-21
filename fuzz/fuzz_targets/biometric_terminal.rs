//! Terminal API keys: `constant_time_eq` agrees with `==` on arbitrary slices, the stored
//! digest is SHA-256 of exactly the presented text, and a generated key matches only itself.
#![no_main]

use arbitrary::Arbitrary;
use biometric::{constant_time_eq, hash_terminal_key, TerminalKey, TERMINAL_KEY_BYTES};
use libfuzzer_sys::fuzz_target;
use sha2::{Digest, Sha256};

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    a: &'a [u8],
    b: &'a [u8],
    presented: &'a str,
}

fuzz_target!(|input: Input| {
    assert_eq!(constant_time_eq(input.a, input.b), input.a == input.b);
    assert!(constant_time_eq(input.a, input.a));
    assert_eq!(
        constant_time_eq(input.a, input.b),
        constant_time_eq(input.b, input.a)
    );

    let digest = hash_terminal_key(input.presented);
    assert_eq!(digest, <[u8; 32]>::from(Sha256::digest(input.presented)));

    let key = TerminalKey::generate();
    assert_eq!(key.plaintext.len(), 2 * TERMINAL_KEY_BYTES);
    assert!(key
        .plaintext
        .bytes()
        .all(|c| matches!(c, b'0'..=b'9' | b'a'..=b'f')));
    assert!(constant_time_eq(
        &hash_terminal_key(&key.plaintext),
        &key.hash
    ));
    if input.presented != key.plaintext {
        assert!(!constant_time_eq(&digest, &key.hash));
    }
});
