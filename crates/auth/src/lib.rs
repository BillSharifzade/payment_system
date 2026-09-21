use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("password hashing failed")]
    Hashing,
    #[error("invalid or expired token")]
    InvalidToken,
    #[error("system clock is before the unix epoch")]
    Clock,
}

pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| AuthError::Hashing)
}

pub fn verify_password(password: &str, phc_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Claims {
    pub sub: String,
    pub exp: usize,
    pub iat: usize,
}

fn now_secs() -> Result<usize, AuthError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as usize)
        .map_err(|_| AuthError::Clock)
}

pub fn issue_access_token(user_id: Uuid, secret: &str, ttl_secs: u64) -> Result<String, AuthError> {
    let iat = now_secs()?;
    let claims = Claims {
        sub: user_id.to_string(),
        iat,
        exp: iat + ttl_secs as usize,
    };
    encode(
        &Header::default(),
        &claims,
        &EncodingKey::from_secret(secret.as_bytes()),
    )
    .map_err(|_| AuthError::InvalidToken)
}

pub fn verify_access_token(token: &str, secret: &str) -> Result<Uuid, AuthError> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| AuthError::InvalidToken)?;
    Uuid::parse_str(&data.claims.sub).map_err(|_| AuthError::InvalidToken)
}

pub struct RefreshToken {
    pub plaintext: String,
    pub hash: String,
}

pub fn generate_refresh_token() -> RefreshToken {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let plaintext = hex::encode(bytes);
    let hash = hash_refresh_token(&plaintext);
    RefreshToken { plaintext, hash }
}

pub fn hash_refresh_token(plaintext: &str) -> String {
    let digest = Sha256::digest(plaintext.as_bytes());
    hex::encode(digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_roundtrip() {
        let hash = hash_password("correct horse battery staple").unwrap();
        assert!(verify_password("correct horse battery staple", &hash));
        assert!(!verify_password("wrong password", &hash));
        let hash2 = hash_password("correct horse battery staple").unwrap();
        assert_ne!(hash, hash2);
    }

    #[test]
    fn malformed_hash_does_not_panic() {
        assert!(!verify_password("anything", "not-a-real-phc-hash"));
    }

    #[test]
    fn access_token_roundtrip() {
        let uid = Uuid::new_v4();
        let token = issue_access_token(uid, "test-secret", 900).unwrap();
        assert_eq!(verify_access_token(&token, "test-secret").unwrap(), uid);
    }

    #[test]
    fn token_with_wrong_secret_is_rejected() {
        let token = issue_access_token(Uuid::new_v4(), "secret-a", 900).unwrap();
        assert!(verify_access_token(&token, "secret-b").is_err());
    }

    #[test]
    fn expired_token_is_rejected() {
        let claims = Claims {
            sub: Uuid::new_v4().to_string(),
            iat: 0,
            exp: 1,
        };
        let token = jsonwebtoken::encode(
            &jsonwebtoken::Header::default(),
            &claims,
            &jsonwebtoken::EncodingKey::from_secret(b"s"),
        )
        .unwrap();
        assert!(verify_access_token(&token, "s").is_err());
    }

    #[test]
    fn refresh_token_is_opaque_and_hashed() {
        let t = generate_refresh_token();
        assert_eq!(t.plaintext.len(), 64);
        assert_eq!(t.hash, hash_refresh_token(&t.plaintext));
        assert_ne!(t.plaintext, t.hash);
    }
}
