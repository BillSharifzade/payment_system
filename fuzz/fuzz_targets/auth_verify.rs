//! `auth::verify_access_token` on arbitrary strings: never panics, and anything it accepts is
//! checked independently — three segments, an HS256 header, an HMAC-SHA256 under the server
//! secret recomputed here with the `hmac` crate, an unexpired `exp`, and the returned id equal
//! to `sub`. A fuzzer cannot forge the MAC, so in practice this asserts "never accepted";
//! `auth_token` reaches the accepting paths with tokens it signs itself.
#![no_main]

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use libfuzzer_sys::fuzz_target;
use sha2::Sha256;
use uuid::Uuid;

const SECRET: &str = "fuzz-only-jwt-secret-of-at-least-32-chars";

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

fn independently_valid(token: &str, uid: Uuid) -> Result<(), String> {
    let parts: Vec<&str> = token.split('.').collect();
    let [header, payload, signature] = parts[..] else {
        return Err(format!("{} segments", parts.len()));
    };
    let mut mac = Hmac::<Sha256>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(format!("{header}.{payload}").as_bytes());
    let sig = B64
        .decode(signature)
        .map_err(|e| format!("signature: {e}"))?;
    mac.verify_slice(&sig)
        .map_err(|_| "MAC mismatch".to_string())?;
    let header: serde_json::Value =
        serde_json::from_slice(&B64.decode(header).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    if header["alg"] != "HS256" {
        return Err(format!("alg {}", header["alg"]));
    }
    let claims: serde_json::Value =
        serde_json::from_slice(&B64.decode(payload).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    let exp = claims["exp"]
        .as_u64()
        .ok_or("exp is not an unsigned integer")?;
    if exp.saturating_add(auth::ACCESS_TOKEN_LEEWAY_SECS + 5) < now() {
        return Err(format!("expired at {exp}"));
    }
    let sub = claims["sub"].as_str().ok_or("sub is not a string")?;
    if Uuid::parse_str(sub).ok() != Some(uid) {
        return Err(format!("sub {sub:?} is not {uid}"));
    }
    Ok(())
}

fuzz_target!(|data: &[u8]| {
    let Ok(token) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(uid) = auth::verify_access_token(token, SECRET) {
        if let Err(why) = independently_valid(token, uid) {
            panic!("accepted {token:?} as {uid}, but {why}");
        }
    }
});
