use rand_core::{OsRng, RngCore};
use sha2::{Digest, Sha256};

pub const TERMINAL_KEY_BYTES: usize = 32;

// A terminal's API key: 32 random bytes, hex-encoded for the header. Only the SHA-256 of
// the key is stored, so a database leak yields no usable key; the key is high-entropy, so a
// fast hash is enough (no password stretching needed).
pub struct TerminalKey {
    pub plaintext: String,
    pub hash: [u8; 32],
}

impl std::fmt::Debug for TerminalKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TerminalKey(..)")
    }
}

impl TerminalKey {
    pub fn generate() -> Self {
        let mut bytes = [0u8; TERMINAL_KEY_BYTES];
        OsRng.fill_bytes(&mut bytes);
        let plaintext = hex::encode(bytes);
        let hash = hash_terminal_key(&plaintext);
        Self { plaintext, hash }
    }
}

pub fn hash_terminal_key(presented: &str) -> [u8; 32] {
    Sha256::digest(presented.as_bytes()).into()
}

pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let diff = a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y));
    std::hint::black_box(diff) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_random_hex_and_hash_to_their_stored_digest() {
        let a = TerminalKey::generate();
        let b = TerminalKey::generate();
        assert_eq!(a.plaintext.len(), TERMINAL_KEY_BYTES * 2);
        assert!(a.plaintext.bytes().all(|c| c.is_ascii_hexdigit()));
        assert_ne!(a.plaintext, b.plaintext);
        assert_eq!(hash_terminal_key(&a.plaintext), a.hash);
        assert_ne!(hash_terminal_key(&b.plaintext), a.hash);
        assert_eq!(format!("{a:?}"), "TerminalKey(..)");
    }

    #[test]
    fn constant_time_eq_compares_whole_slices() {
        let k = TerminalKey::generate();
        assert!(constant_time_eq(&k.hash, &hash_terminal_key(&k.plaintext)));
        let mut flipped = k.hash;
        flipped[31] ^= 1;
        assert!(!constant_time_eq(&k.hash, &flipped));
        assert!(!constant_time_eq(&k.hash, &k.hash[..31]));
        assert!(constant_time_eq(b"", b""));
    }
}
