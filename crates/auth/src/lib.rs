//! Authentication primitives — pure, no database.
//!
//! Three concerns, deliberately separated from storage so they can be tested in
//! isolation and reused anywhere:
//!
//! * **Passwords** — hashed with **Argon2id** (memory-hard; the current standard
//!   for password storage). We never store or log a plaintext password.
//! * **Access tokens** — short-lived **JWTs** (signed, stateless). They expire
//!   quickly so a leaked one is useful only briefly.
//! * **Refresh tokens** — long-lived **opaque random** tokens. Crucially, only a
//!   *hash* of the token is ever stored, so a database breach does not hand an
//!   attacker usable tokens, and they can be **revoked** server-side (which a
//!   pure JWT cannot be — see DESIGN.md §8).

use argon2::password_hash::rand_core::{OsRng, RngCore};
use argon2::password_hash::{PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use jsonwebtoken::{decode, encode, DecodingKey, EncodingKey, Header, Validation};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::{SystemTime, UNIX_EPOCH};
use uuid::Uuid;

/// Errors from auth operations.
#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error("password hashing failed")]
    Hashing,
    #[error("invalid or expired token")]
    InvalidToken,
    #[error("system clock is before the unix epoch")]
    Clock,
}

// ----- Passwords ------------------------------------------------------------

/// Hash a password with Argon2id, returning a PHC string safe to store.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let salt = SaltString::generate(&mut OsRng);
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|_| AuthError::Hashing)
}

/// Verify a password against a stored Argon2 PHC hash. Returns `false` for a
/// wrong password or a malformed hash — never panics, never leaks why.
pub fn verify_password(password: &str, phc_hash: &str) -> bool {
    let Ok(parsed) = PasswordHash::new(phc_hash) else {
        return false;
    };
    Argon2::default()
        .verify_password(password.as_bytes(), &parsed)
        .is_ok()
}

// ----- Access tokens (JWT) --------------------------------------------------

/// JWT claims. `sub` is the user id; `exp`/`iat` are unix timestamps (seconds).
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

/// Issue a signed access token for `user_id`, valid for `ttl_secs` seconds.
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

/// Verify an access token and return the authenticated user id. Rejects expired
/// or tampered tokens (signature + `exp` are checked).
pub fn verify_access_token(token: &str, secret: &str) -> Result<Uuid, AuthError> {
    let data = decode::<Claims>(
        token,
        &DecodingKey::from_secret(secret.as_bytes()),
        &Validation::default(),
    )
    .map_err(|_| AuthError::InvalidToken)?;
    Uuid::parse_str(&data.claims.sub).map_err(|_| AuthError::InvalidToken)
}

// ----- Refresh tokens (opaque) ----------------------------------------------

/// A freshly minted refresh token: the `plaintext` is returned to the client
/// exactly once; only the `hash` is ever persisted.
pub struct RefreshToken {
    pub plaintext: String,
    pub hash: String,
}

/// Generate a cryptographically random opaque refresh token (256 bits).
pub fn generate_refresh_token() -> RefreshToken {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let plaintext = hex::encode(bytes);
    let hash = hash_refresh_token(&plaintext);
    RefreshToken { plaintext, hash }
}

/// Hash a refresh token for storage / lookup. SHA-256 is appropriate here (the
/// input is high-entropy random, so it needs no slow password-style hashing).
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
        // Hash is salted: hashing the same password twice gives different output.
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
        // Mint a token that expired in 1970 — well beyond the default 60s leeway
        // — so the check is deterministic and needs no sleeping.
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
        assert_eq!(t.plaintext.len(), 64); // 32 bytes hex
        assert_eq!(t.hash, hash_refresh_token(&t.plaintext));
        assert_ne!(t.plaintext, t.hash);
    }
}
