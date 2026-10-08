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
    #[error("signed by a key retired after checkpoint {0}")]
    RetiredKey(i64),
    #[error("invalid trusted key entry {0:?}")]
    InvalidTrustEntry(String),
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
/// Rotation keeps retired keys here so history they signed still verifies —
/// bounded by the last checkpoint they signed, so a leaked retired key cannot
/// sign new ones.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TrustedKeys {
    /// The key, and the last checkpoint seq it may have signed (None: current).
    keys: Vec<([u8; 32], Option<i64>)>,
}

impl TrustedKeys {
    pub fn new() -> Self {
        Self::default()
    }

    /// Comma- (or whitespace-) separated hex Ed25519 public keys; `<hex>@<seq>`
    /// marks a key retired after checkpoint `seq`.
    pub fn from_hex_list(list: &str) -> Result<Self, SigningError> {
        let mut keys = Self::new();
        for item in list.split(|c: char| c == ',' || c.is_whitespace()) {
            if item.is_empty() {
                continue;
            }
            let bad = || SigningError::InvalidTrustEntry(item.to_string());
            let (key, last) = match item.split_once('@') {
                None => (item, None),
                Some((key, seq)) => (
                    key,
                    Some(
                        seq.parse::<i64>()
                            .ok()
                            .filter(|s| *s >= 1)
                            .ok_or_else(bad)?,
                    ),
                ),
            };
            let bytes: [u8; 32] = hex::decode(key)
                .ok()
                .and_then(|b| b.try_into().ok())
                .ok_or(SigningError::InvalidPublicKey)?;
            keys.add(bytes, last)?;
        }
        Ok(keys)
    }

    fn add(&mut self, key: [u8; 32], last: Option<i64>) -> Result<(), SigningError> {
        VerifyingKey::from_bytes(&key).map_err(|_| SigningError::InvalidPublicKey)?;
        match self.keys.iter().find(|(k, _)| *k == key) {
            None => self.keys.push((key, last)),
            Some((_, l)) if *l == last => {}
            Some(_) => {
                return Err(SigningError::InvalidTrustEntry(format!(
                    "{} is listed both retired and current",
                    hex::encode(key)
                )))
            }
        }
        Ok(())
    }

    /// Trusts `key` for every checkpoint (the current signing key).
    pub fn insert(&mut self, key: [u8; 32]) -> Result<(), SigningError> {
        self.add(key, None)
    }

    pub fn with(mut self, key: [u8; 32]) -> Result<Self, SigningError> {
        self.insert(key)?;
        Ok(self)
    }

    /// Trusts `key` for checkpoints up to `last_seq` only.
    pub fn with_retired(mut self, key: [u8; 32], last_seq: i64) -> Result<Self, SigningError> {
        self.add(key, Some(last_seq))?;
        Ok(self)
    }

    pub fn is_trusted(&self, key: &[u8; 32]) -> bool {
        self.keys.iter().any(|(k, _)| k == key)
    }

    /// Trusted and not retired: may sign the next checkpoint.
    pub fn is_current(&self, key: &[u8; 32]) -> bool {
        self.keys.iter().any(|(k, last)| k == key && last.is_none())
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    pub fn len(&self) -> usize {
        self.keys.len()
    }

    pub fn to_hex(&self) -> Vec<String> {
        self.keys
            .iter()
            .map(|(k, last)| match last {
                None => hex::encode(k),
                Some(seq) => format!("{}@{seq}", hex::encode(k)),
            })
            .collect()
    }

    /// Checkpoint `seq` signed by `signer`: trusted, not retired before
    /// `seq`, and the signature verifies.
    pub fn verify(
        &self,
        seq: i64,
        signer: &[u8; 32],
        hash: &Hash,
        signature: &[u8; 64],
    ) -> Result<(), SigningError> {
        match self.keys.iter().find(|(k, _)| k == signer) {
            None => Err(SigningError::UntrustedKey),
            Some((_, Some(last))) if seq > *last => Err(SigningError::RetiredKey(*last)),
            Some(_) => verify_hash(signer, hash, signature),
        }
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
                .verify(1, &s.public_key_bytes(), &h, &s.sign_hash(&h))
                .is_ok());
        }
        assert!(matches!(
            trusted.verify(1, &attacker.public_key_bytes(), &h, &attacker.sign_hash(&h)),
            Err(SigningError::UntrustedKey)
        ));
        assert!(matches!(
            trusted.verify(1, &current.public_key_bytes(), &h, &attacker.sign_hash(&h)),
            Err(SigningError::VerificationFailed)
        ));
        assert!(TrustedKeys::from_hex_list("abcd").is_err());
        assert!(TrustedKeys::from_hex_list("").unwrap().is_empty());
    }

    #[test]
    fn a_retired_key_verifies_only_the_history_it_signed() {
        let (old, new) = (Sealer::generate(), Sealer::generate());
        let list = format!("{}@41,{}", old.public_key_hex(), new.public_key_hex());
        let trusted = TrustedKeys::from_hex_list(&list).unwrap();
        assert_eq!(
            trusted.to_hex(),
            [format!("{}@41", old.public_key_hex()), new.public_key_hex()]
        );
        let h = sha256(b"cp");
        let sig = old.sign_hash(&h);
        assert!(trusted
            .verify(41, &old.public_key_bytes(), &h, &sig)
            .is_ok());
        assert!(matches!(
            trusted.verify(42, &old.public_key_bytes(), &h, &sig),
            Err(SigningError::RetiredKey(41))
        ));
        assert!(
            trusted.is_trusted(&old.public_key_bytes())
                && !trusted.is_current(&old.public_key_bytes())
        );
        assert!(trusted.is_current(&new.public_key_bytes()));
        // The current signing key cannot also be retired.
        assert!(trusted.clone().with(old.public_key_bytes()).is_err());
        for bad in [
            "@1",
            "x@1",
            &format!("{}@0", old.public_key_hex()),
            &format!("{}@-3", old.public_key_hex()),
            &format!("{}@one", old.public_key_hex()),
        ] {
            assert!(TrustedKeys::from_hex_list(bad).is_err(), "{bad}");
        }
        assert!(TrustedKeys::from_hex_list(&format!("{0}@3,{0}@3", old.public_key_hex())).is_ok());
        assert!(TrustedKeys::from_hex_list(&format!("{0}@3,{0}@4", old.public_key_hex())).is_err());
    }

    #[test]
    fn loading_from_bytes_is_deterministic() {
        let bytes = [7u8; 32];
        let a = Sealer::from_secret_bytes(&bytes);
        let b = Sealer::from_secret_bytes(&bytes);
        assert_eq!(a.public_key_bytes(), b.public_key_bytes());
    }
}
