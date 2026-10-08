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
    #[error("signed by a key that is not trusted")]
    UntrustedKey,
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

/// The verifying keys an auditor accepts. A signature is only meaningful if the
/// key that made it is trusted out of band: a key stored next to the data it
/// signs proves nothing, since whoever can rewrite the data can re-sign it.
/// Rotation keeps retired keys here so history they signed still verifies.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedKeys {
    keys: Vec<[u8; 32]>,
}

impl TrustedKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Comma- (or whitespace-) separated hex Ed25519 public keys.
    pub fn from_hex_list(list: &str) -> Result<Self, SigningError> {
        let mut keys = Self::new();
        for item in list.split(|c: char| c == ',' || c.is_whitespace()) {
            if item.is_empty() {
                continue;
            }
            let bytes: [u8; 32] = hex::decode(item)
                .ok()
                .and_then(|b| b.try_into().ok())
                .ok_or(SigningError::InvalidPublicKey)?;
            keys.insert(bytes)?;
        }
        Ok(keys)
    }

    pub fn insert(&mut self, key: [u8; 32]) -> Result<(), SigningError> {
        VerifyingKey::from_bytes(&key).map_err(|_| SigningError::InvalidPublicKey)?;
        if !self.keys.contains(&key) {
            self.keys.push(key);
        }
        Ok(())
    }

    pub fn with(mut self, key: [u8; 32]) -> Result<Self, SigningError> {
        self.insert(key)?;
        Ok(self)
    }

    pub fn is_trusted(&self, key: &[u8; 32]) -> bool {
        self.keys.contains(key)
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn to_hex(&self) -> Vec<String> {
        self.keys.iter().map(hex::encode).collect()
    }

    pub fn verify(
        &self,
        signer: &[u8; 32],
        hash: &Hash,
        signature: &[u8; 64],
    ) -> Result<(), SigningError> {
        if !self.is_trusted(signer) {
            return Err(SigningError::UntrustedKey);
        }
        verify_hash(signer, hash, signature)
    }
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
    fn only_trusted_keys_verify() {
        let current = Sealer::generate();
        let retired = Sealer::generate();
        let attacker = Sealer::generate();
        let list = format!(" {}, \n", retired.public_key_hex());
        let trusted = TrustedKeys::from_hex_list(&list)
            .unwrap()
            .with(current.public_key_bytes())
            .unwrap();
        assert_eq!(trusted.len(), 2);
        let h = sha256(b"cp");
        for s in [&current, &retired] {
            assert!(trusted
                .verify(&s.public_key_bytes(), &h, &s.sign_hash(&h))
                .is_ok());
        }
        assert!(matches!(
            trusted.verify(&attacker.public_key_bytes(), &h, &attacker.sign_hash(&h)),
            Err(SigningError::UntrustedKey)
        ));
        assert!(matches!(
            trusted.verify(&current.public_key_bytes(), &h, &attacker.sign_hash(&h)),
            Err(SigningError::VerificationFailed)
        ));
        assert!(TrustedKeys::from_hex_list("abcd").is_err());
        assert!(TrustedKeys::from_hex_list("").unwrap().is_empty());
    }

    #[test]
    fn loading_from_bytes_is_deterministic() {
        let bytes = [7u8; 32];
        let a = Sealer::from_secret_bytes(&bytes);
        let b = Sealer::from_secret_bytes(&bytes);
        assert_eq!(a.public_key_bytes(), b.public_key_bytes());
    }
}
