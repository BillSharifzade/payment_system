//! Checkpoint signatures under trusted keys:
//! - `TrustedKeys::from_hex_list` accepts exactly lists of valid, non-weak 32-byte hex Ed25519
//!   keys, each optionally `@<seq>` (retired after checkpoint seq ≥ 1), with the same error
//!   precedence as an independent model; it de-duplicates in order, refuses a key listed with
//!   two different retirements, and round-trips through `to_hex`;
//! - `Hash::from_hex` accepts exactly 64 hex digits and round-trips;
//! - a sealer's signature verifies under its trusted key at any checkpoint seq; a retired key
//!   only up to its last seq; a flipped signature or digest bit, an untrusted signer or another
//!   key's signature never does;
//! - weak (small-order) keys are never trusted, and nothing verifies that strict Ed25519
//!   verification would refuse.
#![no_main]

use arbitrary::Arbitrary;
use crypto::{verify_hash, Hash, Sealer, SigningError, TrustedKeys};
use curve25519_dalek::constants::EIGHT_TORSION;
use ed25519_dalek::{Signature, VerifyingKey};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    list: &'a str,
    hash_hex: &'a str,
    sk: [u8; 32],
    other_sk: [u8; 32],
    digest: [u8; 32],
    sig_flip: (u8, u8),
    digest_flip: (u8, u8),
    raw_key: [u8; 32],
    raw_sig: [u8; 64],
    torsion: Option<(u8, u8)>,
    seq: u16,
    last: u16,
}

enum Want {
    Keys(Vec<([u8; 32], Option<i64>)>),
    BadKey,
    BadEntry,
}

/// An independent reading of the trust-list syntax: per item, a malformed `@seq` is reported
/// before the key; an undecodable or weak key is `InvalidPublicKey`; a key listed again with a
/// different retirement is `InvalidTrustEntry`; the first bad item decides.
fn model(list: &str) -> Want {
    let mut keys: Vec<([u8; 32], Option<i64>)> = Vec::new();
    for item in list.split(|c: char| c == ',' || c.is_whitespace()) {
        if item.is_empty() {
            continue;
        }
        let (key, last) = match item.split_once('@') {
            None => (item, None),
            Some((key, seq)) => match seq.parse::<i64>() {
                Ok(n) if n >= 1 => (key, Some(n)),
                _ => return Want::BadEntry,
            },
        };
        let Some(bytes) = hex::decode(key)
            .ok()
            .and_then(|b| <[u8; 32]>::try_from(b).ok())
        else {
            return Want::BadKey;
        };
        if VerifyingKey::from_bytes(&bytes).map_or(true, |k| k.is_weak()) {
            return Want::BadKey;
        }
        match keys.iter().find(|(k, _)| *k == bytes) {
            None => keys.push((bytes, last)),
            Some((_, l)) if *l == last => {}
            Some(_) => return Want::BadEntry,
        }
    }
    Want::Keys(keys)
}

fn check_list(list: &str) {
    match (TrustedKeys::from_hex_list(list), model(list)) {
        (Ok(t), Want::Keys(keys)) => {
            for (k, last) in &keys {
                assert!(t.is_trusted(k));
                assert_eq!(t.is_current(k), last.is_none());
            }
            let hexes: Vec<String> = keys
                .iter()
                .map(|(k, last)| match last {
                    None => hex::encode(k),
                    Some(seq) => format!("{}@{seq}", hex::encode(k)),
                })
                .collect();
            assert_eq!(t.to_hex(), hexes);
            assert_eq!(t.len(), keys.len());
            assert_eq!(TrustedKeys::from_hex_list(&hexes.join(",")).unwrap(), t);
        }
        (Err(SigningError::InvalidPublicKey), Want::BadKey) => {}
        (Err(SigningError::InvalidTrustEntry(_)), Want::BadEntry) => {}
        (got, _) => panic!("from_hex_list({list:?}) = {got:?} disagrees with the model"),
    }
}

fn check_hash_hex(s: &str) {
    let valid = s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit());
    match Hash::from_hex(s) {
        Some(h) => {
            assert!(valid, "accepted {s:?}");
            assert_eq!(h.to_hex(), s.to_ascii_lowercase());
        }
        None => assert!(!valid, "refused {s:?}"),
    }
}

fn flip<const N: usize>(mut bytes: [u8; N], (at, x): (u8, u8)) -> Option<[u8; N]> {
    if x == 0 {
        return None;
    }
    bytes[at as usize % N] ^= x;
    Some(bytes)
}

fn check_signatures(input: &Input) {
    let sealer = Sealer::from_secret_bytes(&input.sk);
    let pk = sealer.public_key_bytes();
    let h = Hash::from_bytes(input.digest);
    let sig = sealer.sign_hash(&h);
    let seq = i64::from(input.seq);
    let trusted = TrustedKeys::new().with(pk).unwrap();
    assert!(verify_hash(&pk, &h, &sig).is_ok());
    assert!(trusted.verify(seq, &pk, &h, &sig).is_ok());
    assert_eq!(hex::decode(sealer.public_key_hex()).unwrap(), pk);

    // A retired key verifies exactly the checkpoints up to the last one it signed.
    let last = 1 + i64::from(input.last);
    let retired = TrustedKeys::new().with_retired(pk, last).unwrap();
    match retired.verify(seq, &pk, &h, &sig) {
        Ok(()) => assert!(seq <= last),
        Err(SigningError::RetiredKey(l)) => assert!(l == last && seq > last),
        Err(e) => panic!("retired key: {e:?}"),
    }

    if let Some(bad) = flip(sig, input.sig_flip) {
        assert!(
            trusted.verify(seq, &pk, &h, &bad).is_err(),
            "flipped signature verified"
        );
    }
    if let Some(bad) = flip(input.digest, input.digest_flip) {
        let bad = Hash::from_bytes(bad);
        assert!(
            trusted.verify(seq, &pk, &bad, &sig).is_err(),
            "signature moved to another digest"
        );
    }

    let other = Sealer::from_secret_bytes(&input.other_sk);
    let other_pk = other.public_key_bytes();
    if other_pk != pk {
        let other_sig = other.sign_hash(&h);
        assert!(matches!(
            trusted.verify(seq, &other_pk, &h, &other_sig),
            Err(SigningError::UntrustedKey)
        ));
        assert!(matches!(
            trusted.verify(seq, &pk, &h, &other_sig),
            Err(SigningError::VerificationFailed)
        ));
    }
}

fn check_arbitrary(input: &Input) {
    let (key, sig) = match input.torsion {
        // Small-order key and R with s = 0: the classic forgery against non-strict checks.
        Some((a, r)) => {
            let mut sig = [0u8; 64];
            sig[..32].copy_from_slice(EIGHT_TORSION[r as usize % 8].compress().as_bytes());
            (*EIGHT_TORSION[a as usize % 8].compress().as_bytes(), sig)
        }
        None => (input.raw_key, input.raw_sig),
    };
    let h = Hash::from_bytes(input.digest);
    let Ok(trusted) = TrustedKeys::new().with(key) else {
        assert!(VerifyingKey::from_bytes(&key).map_or(true, |k| k.is_weak()));
        return;
    };
    let vk = VerifyingKey::from_bytes(&key).unwrap();
    assert!(!vk.is_weak(), "a weak key was trusted");
    if trusted.verify(i64::from(input.seq), &key, &h, &sig).is_ok() {
        assert!(
            vk.verify_strict(h.as_bytes(), &Signature::from_bytes(&sig))
                .is_ok(),
            "accepted a signature strict verification refuses"
        );
    }
}

fuzz_target!(|input: Input| {
    check_list(input.list);
    check_hash_hex(input.hash_hex);
    check_signatures(&input);
    check_arbitrary(&input);
});
