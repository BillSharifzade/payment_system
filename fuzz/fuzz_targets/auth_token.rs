//! Access tokens the fuzzer builds and signs itself, so the accepting paths are reached:
//! - only an HS256 header with an HMAC-SHA256 under the server secret is ever accepted
//!   (other algorithms, `none`, other keys and algorithm confusion are refused);
//! - an accepted token has an unsigned integer `exp` no more than the leeway in the past, and
//!   the returned id is its `sub`; a well-formed unexpired one is always accepted;
//! - any mutation of an accepted token is refused;
//! - `issue_access_token` never panics for any TTL, and what it issues verifies, carries
//!   exp − iat = TTL, and fails under another secret.
#![no_main]

use arbitrary::Arbitrary;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use hmac::{Hmac, Mac};
use libfuzzer_sys::fuzz_target;
use sha2::{Digest, Sha256, Sha384, Sha512};
use uuid::Uuid;

const SECRET: &str = "fuzz-only-jwt-secret-of-at-least-32-chars";
const LEEWAY_SECS: i128 = auth::ACCESS_TOKEN_LEEWAY_SECS as i128;
// The clock moves while a case runs.
const SLACK_SECS: i128 = 5;
// Typed header fields of jsonwebtoken's `Header`, and the claims validation reads.
const HEADER_FIELDS: [&str; 10] = [
    "alg", "typ", "cty", "jku", "jwk", "kid", "x5u", "x5c", "x5t", "x5t#S256",
];
const CLAIM_FIELDS: [&str; 6] = ["sub", "exp", "iat", "aud", "nbf", "iss"];

#[derive(Arbitrary, Debug, Clone)]
enum Alg {
    Hs256,
    Hs384,
    Hs512,
    Unsigned,
    Other(String),
}

impl Alg {
    fn name(&self) -> String {
        match self {
            Alg::Hs256 => "HS256".into(),
            Alg::Hs384 => "HS384".into(),
            Alg::Hs512 => "HS512".into(),
            Alg::Unsigned => "none".into(),
            Alg::Other(s) => s.clone(),
        }
    }
}

#[derive(Arbitrary, Debug)]
enum Key {
    Server,
    ServerWithZeros(u8),
    Other(Vec<u8>),
}

#[derive(Arbitrary, Debug)]
enum Sub {
    Hyphenated(u128),
    Upper(u128),
    Simple(u128),
    Braced(u128),
    Urn(u128),
    Text(String),
    Number(i64),
    Missing,
}

#[derive(Arbitrary, Debug)]
enum Exp {
    FromNow(i32),
    At(u64),
    Negative(u32),
    Float(f64),
    Text(String),
    Missing,
}

#[derive(Arbitrary, Debug)]
enum Iat {
    Now,
    At(u64),
    Negative(u32),
    Text(String),
    Missing,
}

#[derive(Arbitrary, Debug)]
enum Mutation {
    Set(u16, u8),
    Insert(u16, u8),
    Remove(u16),
    Truncate(u16),
    Append(u8),
}

#[derive(Arbitrary, Debug)]
struct Input {
    alg: Alg,
    signed_with: Option<Alg>,
    typ: Option<String>,
    header_extra: Option<(String, String)>,
    sub: Sub,
    exp: Exp,
    iat: Iat,
    claims_extra: Option<(String, i64)>,
    raw_payload: Option<Vec<u8>>,
    key: Key,
    mutations: Vec<Mutation>,
    ttl: u64,
}

fn now() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i128
}

fn json(s: &str) -> String {
    serde_json::to_string(s).unwrap()
}

fn mac<M: Mac + hmac::digest::KeyInit>(key: &[u8], msg: &[u8]) -> Vec<u8> {
    let mut m = <M as Mac>::new_from_slice(key).unwrap();
    m.update(msg);
    m.finalize().into_bytes().to_vec()
}

/// HMAC treats keys up to the block size as zero-padded and longer ones as their hash, so
/// distinct byte strings can be the same HMAC-SHA256 key.
fn same_hs256_key(a: &[u8], b: &[u8]) -> bool {
    let block = |k: &[u8]| {
        let mut out = if k.len() > 64 {
            Sha256::digest(k).to_vec()
        } else {
            k.to_vec()
        };
        out.resize(64, 0);
        out
    };
    block(a) == block(b)
}

fn mutate(token: &str, mutations: &[Mutation]) -> Vec<u8> {
    let mut b = token.as_bytes().to_vec();
    for m in mutations {
        let at = |p: &u16, len: usize| *p as usize % len.max(1);
        match m {
            Mutation::Set(p, v) if !b.is_empty() => {
                let i = at(p, b.len());
                b[i] = *v;
            }
            Mutation::Insert(p, v) => {
                let i = at(p, b.len() + 1);
                b.insert(i, *v);
            }
            Mutation::Remove(p) if !b.is_empty() => {
                let i = at(p, b.len());
                b.remove(i);
            }
            Mutation::Truncate(n) => b.truncate(*n as usize),
            Mutation::Append(v) => b.push(*v),
            _ => {}
        }
    }
    b
}

fn check_issue(ttl: u64, user: Uuid) {
    let issued = auth::issue_access_token(user, SECRET, ttl);
    let fits = (now() + ttl as i128) <= u64::MAX as i128;
    let token = match issued {
        Ok(t) => t,
        Err(e) => {
            assert!(!fits, "refused TTL {ttl}: {e}");
            return;
        }
    };
    assert_eq!(auth::verify_access_token(&token, SECRET).ok(), Some(user));
    assert!(auth::verify_access_token(&token, &format!("{SECRET}x")).is_err());
    let payload = token.split('.').nth(1).unwrap();
    let claims: serde_json::Value = serde_json::from_slice(&B64.decode(payload).unwrap()).unwrap();
    let (exp, iat) = (
        claims["exp"].as_u64().unwrap(),
        claims["iat"].as_u64().unwrap(),
    );
    assert_eq!(exp.checked_sub(iat), Some(ttl), "exp − iat is the TTL");
}

fuzz_target!(|input: Input| {
    let t = now();
    let user = match &input.sub {
        Sub::Hyphenated(u) | Sub::Upper(u) | Sub::Simple(u) | Sub::Braced(u) | Sub::Urn(u) => {
            Some(Uuid::from_u128(*u))
        }
        _ => None,
    };
    check_issue(input.ttl, user.unwrap_or(Uuid::nil()));

    let mut header = format!("{{\"alg\":{}", json(&input.alg.name()));
    if let Some(typ) = &input.typ {
        header += &format!(",\"typ\":{}", json(typ));
    }
    if let Some((k, v)) = &input.header_extra {
        header += &format!(",{}:{}", json(k), json(v));
    }
    header.push('}');

    let mut claims = Vec::new();
    let sub = match &input.sub {
        Sub::Hyphenated(u) => Some(json(&Uuid::from_u128(*u).to_string())),
        Sub::Upper(u) => Some(json(&Uuid::from_u128(*u).to_string().to_uppercase())),
        Sub::Simple(u) => Some(json(&Uuid::from_u128(*u).simple().to_string())),
        Sub::Braced(u) => Some(json(&Uuid::from_u128(*u).braced().to_string())),
        Sub::Urn(u) => Some(json(&Uuid::from_u128(*u).urn().to_string())),
        Sub::Text(s) => Some(json(s)),
        Sub::Number(n) => Some(n.to_string()),
        Sub::Missing => None,
    };
    let exp_at: Option<i128> = match &input.exp {
        Exp::FromNow(d) => Some(t + *d as i128),
        Exp::At(x) => Some(*x as i128),
        Exp::Negative(x) => Some(-1 - *x as i128),
        _ => None,
    };
    let exp = match &input.exp {
        Exp::Float(f) => serde_json::Number::from_f64(*f).map(|n| {
            let s = n.to_string();
            if s.contains(['.', 'e', 'E']) {
                s
            } else {
                format!("{s}.0")
            }
        }),
        Exp::Text(s) => Some(json(s)),
        Exp::Missing => None,
        _ => exp_at.map(|e| e.to_string()),
    };
    let iat_ok = matches!(input.iat, Iat::Now | Iat::At(_));
    let iat = match &input.iat {
        Iat::Now => Some(t.to_string()),
        Iat::At(x) => Some(x.to_string()),
        Iat::Negative(x) => Some((-1 - *x as i64).to_string()),
        Iat::Text(s) => Some(json(s)),
        Iat::Missing => None,
    };
    for (name, value) in [("sub", sub), ("exp", exp), ("iat", iat)] {
        if let Some(v) = value {
            claims.push(format!("{}:{v}", json(name)));
        }
    }
    if let Some((k, v)) = &input.claims_extra {
        claims.push(format!("{}:{v}", json(k)));
    }
    let payload = match &input.raw_payload {
        Some(raw) => raw.clone(),
        None => format!("{{{}}}", claims.join(",")).into_bytes(),
    };

    let key: Vec<u8> = match &input.key {
        Key::Server => SECRET.as_bytes().to_vec(),
        Key::ServerWithZeros(n) => [SECRET.as_bytes(), &vec![0; *n as usize]].concat(),
        Key::Other(k) => k.clone(),
    };
    let message = format!("{}.{}", B64.encode(&header), B64.encode(&payload));
    let signer = input.signed_with.clone().unwrap_or(input.alg.clone());
    let signature = match signer {
        Alg::Hs256 | Alg::Other(_) => mac::<Hmac<Sha256>>(&key, message.as_bytes()),
        Alg::Hs384 => mac::<Hmac<Sha384>>(&key, message.as_bytes()),
        Alg::Hs512 => mac::<Hmac<Sha512>>(&key, message.as_bytes()),
        Alg::Unsigned => Vec::new(),
    };
    let token = format!("{message}.{}", B64.encode(&signature));
    let signed_hs256 =
        matches!(signer, Alg::Hs256 | Alg::Other(_)) && same_hs256_key(&key, SECRET.as_bytes());
    let verdict = auth::verify_access_token(&token, SECRET);

    if let Ok(uid) = verdict {
        assert_eq!(
            input.alg.name(),
            "HS256",
            "accepted alg {:?}",
            input.alg.name()
        );
        assert!(
            signed_hs256,
            "accepted a token not signed with the secret: {input:?}"
        );
        let claims: serde_json::Value = serde_json::from_slice(&payload).unwrap();
        let exp = claims["exp"]
            .as_u64()
            .expect("accepted without an integer exp");
        assert!(
            exp as i128 + LEEWAY_SECS + SLACK_SECS >= t,
            "accepted a token that expired at {exp} (now {t})"
        );
        let sub = claims["sub"]
            .as_str()
            .expect("accepted without a string sub");
        assert_eq!(Uuid::parse_str(sub).ok(), Some(uid));

        let mutated = mutate(&token, &input.mutations);
        if mutated != token.as_bytes() {
            if let Ok(m) = std::str::from_utf8(&mutated) {
                assert!(
                    auth::verify_access_token(m, SECRET).is_err(),
                    "accepted a mutated token {m:?} (from {token:?})"
                );
            }
        }
    } else {
        let benign_header = input
            .header_extra
            .as_ref()
            .is_none_or(|(k, _)| !HEADER_FIELDS.contains(&k.as_str()));
        let benign_claims = input
            .claims_extra
            .as_ref()
            .is_none_or(|(k, _)| !CLAIM_FIELDS.contains(&k.as_str()));
        let live = exp_at.is_some_and(|e| e >= t + SLACK_SECS && e <= u64::MAX as i128);
        let well_formed = input.alg.name() == "HS256"
            && signed_hs256
            && benign_header
            && benign_claims
            && input.raw_payload.is_none()
            && user.is_some()
            && live
            && iat_ok;
        assert!(
            !well_formed,
            "refused a well-formed live token: {verdict:?}\n{input:?}"
        );
    }
});
