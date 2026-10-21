//! `TemplateCipher`: open(seal(x)) = x under the same key and enrollment id; any other key,
//! any other enrollment id (the AAD) and any tampering of the sealed bytes fail; arbitrary
//! bytes never panic; `from_hex` accepts exactly 64 hex digits (after trimming) and builds
//! the same key as `from_bytes`.
#![no_main]

use arbitrary::Arbitrary;
use biometric::{BiometricError, TemplateCipher};
use libfuzzer_sys::fuzz_target;
use uuid::Uuid;

#[derive(Arbitrary, Debug)]
enum Tamper {
    Xor(u16, u8),
    Truncate(u16),
    Extend(u8),
    SwapHalves,
}

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    key: [u8; 32],
    other_key: [u8; 32],
    id: u128,
    other_id: u128,
    plaintext: Vec<u8>,
    tamper: Vec<Tamper>,
    arbitrary_sealed: &'a [u8],
    hex: &'a str,
}

fn apply(sealed: &[u8], tamper: &[Tamper]) -> Vec<u8> {
    let mut b = sealed.to_vec();
    for t in tamper {
        match t {
            Tamper::Xor(i, x) if !b.is_empty() => {
                let i = *i as usize % b.len();
                b[i] ^= x;
            }
            Tamper::Truncate(n) => b.truncate(*n as usize),
            Tamper::Extend(v) => b.push(*v),
            Tamper::SwapHalves => {
                let mid = b.len() / 2;
                b.rotate_left(mid);
            }
            _ => {}
        }
    }
    b
}

fuzz_target!(|input: Input| {
    let cipher = TemplateCipher::from_bytes(input.key);
    let id = Uuid::from_u128(input.id);
    let sealed = cipher.seal(id, &input.plaintext);
    assert_eq!(sealed.len(), 12 + input.plaintext.len() + 16);
    assert_eq!(cipher.open(id, &sealed), Ok(input.plaintext.clone()));

    if input.other_id != input.id {
        assert_eq!(
            cipher.open(Uuid::from_u128(input.other_id), &sealed),
            Err(BiometricError::Sealed),
            "opened under another enrollment id"
        );
    }
    if input.other_key != input.key {
        assert_eq!(
            TemplateCipher::from_bytes(input.other_key).open(id, &sealed),
            Err(BiometricError::Sealed),
            "opened under another key"
        );
    }
    let tampered = apply(&sealed, &input.tamper);
    if tampered != sealed {
        assert_eq!(
            cipher.open(id, &tampered),
            Err(BiometricError::Sealed),
            "tampered bytes opened"
        );
    }
    if let Ok(p) = cipher.open(id, input.arbitrary_sealed) {
        assert_eq!(p.len() + 28, input.arbitrary_sealed.len());
    }

    let trimmed = input.hex.trim();
    let valid_hex = trimmed.len() == 64 && trimmed.bytes().all(|b| b.is_ascii_hexdigit());
    match TemplateCipher::from_hex(input.hex) {
        Ok(c) => {
            assert!(valid_hex, "accepted key {:?}", input.hex);
            let key: [u8; 32] = hex::decode(trimmed).unwrap().try_into().unwrap();
            let same = TemplateCipher::from_bytes(key);
            assert_eq!(same.open(id, &c.seal(id, b"x")), Ok(b"x".to_vec()));
        }
        Err(e) => {
            assert_eq!(e, BiometricError::Key);
            assert!(!valid_hex, "refused key {:?}", input.hex);
        }
    }
    let from_hex = TemplateCipher::from_hex(&hex::encode(input.key)).unwrap();
    assert_eq!(from_hex.open(id, &sealed), Ok(input.plaintext));
});
