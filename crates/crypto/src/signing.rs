//! Ed25519 signing for checkpoint sealing.
//!
//! The sealer signs each checkpoint so that the chain of checkpoints is not only
//! tamper-*evident* (via hashing) but tamper-*proof* against anyone without the
//! private key. In production the key lives in an HSM/KMS (DESIGN.md §8/§12.8);
//! this type supports loading from raw secret bytes for exactly that handoff.

use crate::hash::Hash;
use ed25519_dalek::{Signature, Signer as _, SigningKey, Verifier as _, VerifyingKey};
use rand_core::OsRng;

#[derive(Debug, thiserror::Error)]
pub enum SigningError {
    #[error("invalid public key")]
    InvalidPublicKey,
    #[error("invalid signature")]
    InvalidSignature,
    #[error("signature verification failed")]
    VerificationFailed,
}

/// A signer holding an Ed25519 private key.
pub struct Sealer {
    key: SigningKey,
}

impl Sealer {
    /// Generate a fresh random key (e.g. for tests or first-time setup).
    pub fn generate() -> Self {
        Self {
            key: SigningKey::generate(&mut OsRng),
        }
    }

    /// Load from 32 raw secret-key bytes (e.g. fetched from Vault/HSM).
    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(bytes),
        }
    }

    /// The 32-byte public verifying key.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.public_key_bytes())
    }

    /// Sign a 32-byte hash, returning the 64-byte signature.
    pub fn sign_hash(&self, hash: &Hash) -> [u8; 64] {
        self.key.sign(hash.as_bytes()).to_bytes()
    }
}

/// Verify a signature over a hash against a public key. Standalone so auditors
/// can verify a checkpoint chain with only the public key.
pub fn verify_hash(
    public_key: &[u8; 32],
    hash: &Hash,
    signature: &[u8; 64],
) -> Result<(), SigningError> {
    let vk = VerifyingKey::from_bytes(public_key).map_err(|_| SigningError::InvalidPublicKey)?;
    let sig = Signature::from_bytes(signature);
    vk.verify(hash.as_bytes(), &sig)
        .map_err(|_| SigningError::VerificationFailed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hash::sha256;

    #[test]
    fn sign_and_verify_roundtrip() {
        let sealer = Sealer::generate();
        let h = sha256(b"checkpoint-42");
        let sig = sealer.sign_hash(&h);
        assert!(verify_hash(&sealer.public_key_bytes(), &h, &sig).is_ok());
    }

    #[test]
    fn wrong_hash_fails_verification() {
        let sealer = Sealer::generate();
        let sig = sealer.sign_hash(&sha256(b"real"));
        assert!(verify_hash(&sealer.public_key_bytes(), &sha256(b"forged"), &sig).is_err());
    }

    #[test]
    fn wrong_key_fails_verification() {
        let sealer = Sealer::generate();
        let other = Sealer::generate();
        let h = sha256(b"x");
        let sig = sealer.sign_hash(&h);
        assert!(verify_hash(&other.public_key_bytes(), &h, &sig).is_err());
    }

    #[test]
    fn loading_from_bytes_is_deterministic() {
        let bytes = [7u8; 32];
        let a = Sealer::from_secret_bytes(&bytes);
        let b = Sealer::from_secret_bytes(&bytes);
        assert_eq!(a.public_key_bytes(), b.public_key_bytes());
    }
}
