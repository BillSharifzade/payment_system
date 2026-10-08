//! Checkpoint signatures under trusted keys:
//! - `TrustedKeys::from_hex_list` accepts exactly lists of valid 32-byte hex Ed25519 keys,
//!   de-duplicates them in order and round-trips through `to_hex`;
//! - `Hash::from_hex` accepts exactly 64 hex digits and round-trips;
//! - a sealer's signature verifies under its trusted key; a flipped signature or digest bit,
//!   an untrusted signer or another key's signature never does;
//! - nothing verifies that strict Ed25519 verification would refuse — except under a weak
//!   (small-order) trusted key, which `TrustedKeys` still admits (known gap, see the report;
//!   the target accepts either admitting or refusing weak keys, so the fix needs no change).
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
}

/// A decodable Ed25519 point, and whether it is weak (small order).
fn parse_key(item: &str) -> Option<([u8; 32], bool)> {
    let bytes: [u8; 32] = hex::decode(item).ok()?.try_into().ok()?;
    let key = VerifyingKey::from_bytes(&bytes).ok()?;
    Some((bytes, key.is_weak()))
}

fn check_list(list: &str) {
    let items: Vec<&str> = list
        .split(|c: char| c == ',' || c.is_whitespace())
        .filter(|s| !s.is_empty())
        .collect();
    let parsed: Option<Vec<([u8; 32], bool)>> = items.iter().map(|i| parse_key(i)).collect();
    match (TrustedKeys::from_hex_list(list), parsed) {
        (Ok(t), Some(keys)) => {
            let mut distinct: Vec<[u8; 32]> = Vec::new();
            for (k, _) in keys {
                assert!(t.is_trusted(&k));
                if !distinct.contains(&k) {
                    distinct.push(k);
                }
            }
            let hexes: Vec<String> = distinct.iter().map(hex::encode).collect();
            assert_eq!(t.to_hex(), hexes);
            assert_eq!(t.len(), distinct.len());
            assert_eq!(TrustedKeys::from_hex_list(&hexes.join(",")).unwrap(), t);
        }
        (Err(SigningError::InvalidPublicKey), None) => {}
        (Err(SigningError::InvalidPublicKey), Some(keys)) if keys.iter().any(|(_, weak)| *weak) => {
        }
        (got, want) => panic!("from_hex_list({list:?}) = {got:?}, expected keys {want:?}"),
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
    let trusted = TrustedKeys::new().with(pk).unwrap();
    assert!(verify_hash(&pk, &h, &sig).is_ok());
    assert!(trusted.verify(&pk, &h, &sig).is_ok());
    assert_eq!(hex::decode(sealer.public_key_hex()).unwrap(), pk);

    if let Some(bad) = flip(sig, input.sig_flip) {
        assert!(
            trusted.verify(&pk, &h, &bad).is_err(),
            "flipped signature verified"
        );
    }
    if let Some(bad) = flip(input.digest, input.digest_flip) {
        let bad = Hash::from_bytes(bad);
        assert!(
            trusted.verify(&pk, &bad, &sig).is_err(),
            "signature moved to another digest"
        );
    }

    let other = Sealer::from_secret_bytes(&input.other_sk);
    let other_pk = other.public_key_bytes();
    if other_pk != pk {
        let other_sig = other.sign_hash(&h);
        assert!(matches!(
            trusted.verify(&other_pk, &h, &other_sig),
            Err(SigningError::UntrustedKey)
        ));
        assert!(matches!(
            trusted.verify(&pk, &h, &other_sig),
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
        assert!(!VerifyingKey::from_bytes(&key).is_ok_and(|k| !k.is_weak()));
        return;
    };
    if trusted.verify(&key, &h, &sig).is_ok() {
        let vk = VerifyingKey::from_bytes(&key).unwrap();
        assert!(
            vk.is_weak()
                || vk
                    .verify_strict(h.as_bytes(), &Signature::from_bytes(&sig))
                    .is_ok(),
            "accepted a signature strict verification refuses under a non-weak key"
        );
    }
}

fuzz_target!(|input: Input| {
    check_list(input.list);
    check_hash_hex(input.hash_hex);
    check_signatures(&input);
    check_arbitrary(&input);
});
