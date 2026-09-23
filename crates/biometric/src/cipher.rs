use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Key, Nonce};
use rand_core::{OsRng, RngCore};
use uuid::Uuid;

use crate::error::BiometricError;

const NONCE_BYTES: usize = 12;
const TAG_BYTES: usize = 16;

#[derive(Clone)]
pub struct TemplateCipher {
    key: Key<Aes256Gcm>,
}

impl std::fmt::Debug for TemplateCipher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TemplateCipher(..)")
    }
}

impl TemplateCipher {
    pub fn from_bytes(key: [u8; 32]) -> Self {
        Self {
            key: Key::<Aes256Gcm>::from(key),
        }
    }

    pub fn from_hex(hex_key: &str) -> Result<Self, BiometricError> {
        let bytes = hex::decode(hex_key.trim()).map_err(|_| BiometricError::Key)?;
        let key: [u8; 32] = bytes.try_into().map_err(|_| BiometricError::Key)?;
        Ok(Self::from_bytes(key))
    }

    pub fn seal(&self, enrollment_id: Uuid, plaintext: &[u8]) -> Vec<u8> {
        let mut nonce_bytes = [0u8; NONCE_BYTES];
        OsRng.fill_bytes(&mut nonce_bytes);
        let nonce = Nonce::from(nonce_bytes);
        let cipher = Aes256Gcm::new(&self.key);
        let ciphertext = cipher
            .encrypt(
                &nonce,
                Payload {
                    msg: plaintext,
                    aad: enrollment_id.as_bytes(),
                },
            )
            .expect("AES-GCM encryption cannot fail for in-memory buffers");
        let mut out = Vec::with_capacity(NONCE_BYTES + ciphertext.len());
        out.extend_from_slice(&nonce_bytes);
        out.extend_from_slice(&ciphertext);
        out
    }

    pub fn open(&self, enrollment_id: Uuid, sealed: &[u8]) -> Result<Vec<u8>, BiometricError> {
        if sealed.len() < NONCE_BYTES + TAG_BYTES {
            return Err(BiometricError::Sealed);
        }
        let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_BYTES);
        let nonce = Nonce::from_slice(nonce_bytes);
        let cipher = Aes256Gcm::new(&self.key);
        cipher
            .decrypt(
                nonce,
                Payload {
                    msg: ciphertext,
                    aad: enrollment_id.as_bytes(),
                },
            )
            .map_err(|_| BiometricError::Sealed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cipher() -> TemplateCipher {
        TemplateCipher::from_bytes([0x42; 32])
    }

    #[test]
    fn seal_open_round_trip() {
        let id = Uuid::new_v4();
        let sealed = cipher().seal(id, b"minutiae-bytes");
        assert_ne!(&sealed[NONCE_BYTES..], b"minutiae-bytes");
        assert_eq!(cipher().open(id, &sealed).unwrap(), b"minutiae-bytes");
    }

    #[test]
    fn sealed_bytes_are_bound_to_their_enrollment() {
        let sealed = cipher().seal(Uuid::new_v4(), b"minutiae-bytes");
        assert_eq!(
            cipher().open(Uuid::new_v4(), &sealed),
            Err(BiometricError::Sealed)
        );
    }

    #[test]
    fn tampering_is_detected() {
        let id = Uuid::new_v4();
        let mut sealed = cipher().seal(id, b"minutiae-bytes");
        let last = sealed.len() - 1;
        sealed[last] ^= 1;
        assert_eq!(cipher().open(id, &sealed), Err(BiometricError::Sealed));
        assert_eq!(cipher().open(id, &[0u8; 5]), Err(BiometricError::Sealed));
    }

    #[test]
    fn each_seal_uses_a_fresh_nonce() {
        let id = Uuid::new_v4();
        let a = cipher().seal(id, b"same");
        let b = cipher().seal(id, b"same");
        assert_ne!(a, b);
    }

    #[test]
    fn hex_key_must_be_32_bytes() {
        assert!(TemplateCipher::from_hex(&"ab".repeat(32)).is_ok());
        assert!(matches!(
            TemplateCipher::from_hex("abcd"),
            Err(BiometricError::Key)
        ));
        assert!(matches!(
            TemplateCipher::from_hex("zz"),
            Err(BiometricError::Key)
        ));
    }
}
