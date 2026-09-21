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

pub struct Sealer {
    key: SigningKey,
}

impl Sealer {
    pub fn generate() -> Self {
        Self {
            key: SigningKey::generate(&mut OsRng),
        }
    }

    pub fn from_secret_bytes(bytes: &[u8; 32]) -> Self {
        Self {
            key: SigningKey::from_bytes(bytes),
        }
    }

    pub fn public_key_bytes(&self) -> [u8; 32] {
        self.key.verifying_key().to_bytes()
    }

    pub fn public_key_hex(&self) -> String {
        hex::encode(self.public_key_bytes())
    }

    pub fn sign_hash(&self, hash: &Hash) -> [u8; 64] {
        self.key.sign(hash.as_bytes()).to_bytes()
    }
}

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
