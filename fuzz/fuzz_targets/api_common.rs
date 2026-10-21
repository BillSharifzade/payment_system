//! The API's request parsers in `crates/api/src/common.rs`, compiled here from that very file
//! (the module is private to the api crate, and these are pure functions):
//! - `normalize_phone`: output is 7..=15 ASCII digits, is the input minus the allowed
//!   separators, and normalising it again changes nothing;
//! - `parse_cursor`: round-trips `<timestamp>|<uuid>` for any timestamp text, '|' included;
//! - `idempotency_key`: accepts a header exactly when it is a UUID, returns that UUID, takes
//!   the first of repeated headers, and refuses a missing one.
#![no_main]

use api::{ApiError, ApiResult};
use arbitrary::Arbitrary;
use axum::http::{HeaderMap, HeaderValue};
use libfuzzer_sys::fuzz_target;
use uuid::Uuid;

#[allow(dead_code)]
#[path = "../../crates/api/src/common.rs"]
mod common;

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    phone: &'a str,
    cursor: &'a str,
    timestamp: &'a str,
    id: u128,
    key: &'a [u8],
    second_key: Option<&'a [u8]>,
}

fn check_phone(raw: &str) {
    let stripped: String = raw.chars().filter(|c| !" -()+".contains(*c)).collect();
    match common::normalize_phone(raw) {
        Some(n) => {
            assert!((7..=15).contains(&n.len()), "{raw:?} -> {n:?}");
            assert!(n.bytes().all(|b| b.is_ascii_digit()), "{raw:?} -> {n:?}");
            assert_eq!(n, stripped);
            assert_eq!(common::normalize_phone(&n).as_deref(), Some(n.as_str()));
        }
        None => assert!(
            !(7..=15).contains(&stripped.len()) || !stripped.bytes().all(|b| b.is_ascii_digit()),
            "refused {raw:?}"
        ),
    }
}

fn check_cursor(raw: &str, timestamp: &str, id: Uuid) {
    match common::parse_cursor(raw) {
        Ok((ts, parsed)) => {
            let (prefix, tail) = raw.rsplit_once('|').expect("accepted without '|'");
            assert_eq!(ts, prefix);
            assert_eq!(Uuid::parse_str(tail).ok(), Some(parsed));
        }
        Err(e) => assert!(matches!(e, ApiError::BadRequest(_)), "{e:?}"),
    }
    let built = format!("{timestamp}|{id}");
    let (ts, parsed) = common::parse_cursor(&built).expect("a cursor the API built parses");
    assert_eq!((ts.as_str(), parsed), (timestamp, id));
}

fn check_idempotency_key(key: &[u8], second: Option<&[u8]>) {
    let mut headers = HeaderMap::new();
    assert!(matches!(
        common::idempotency_key(&headers),
        Err(ApiError::BadRequest(_))
    ));
    let Ok(value) = HeaderValue::from_bytes(key) else {
        return;
    };
    headers.insert("idempotency-key", value);
    if let Some(v) = second.and_then(|s| HeaderValue::from_bytes(s).ok()) {
        headers.append("idempotency-key", v);
    }
    let got: ApiResult<Uuid> = common::idempotency_key(&headers);
    let want = std::str::from_utf8(key)
        .ok()
        .filter(|s| s.bytes().all(|b| (0x20..0x7f).contains(&b)))
        .and_then(|s| Uuid::parse_str(s).ok());
    match got {
        Ok(u) => assert_eq!(Some(u), want, "{key:?}"),
        Err(e) => {
            assert!(matches!(e, ApiError::BadRequest(_)));
            assert_eq!(want, None, "refused {key:?}");
        }
    }
    let mut canonical = HeaderMap::new();
    let id = Uuid::from_slice(&[key, &[0u8; 16]].concat()[..16]).unwrap();
    canonical.insert(
        "idempotency-key",
        HeaderValue::from_str(&id.to_string()).unwrap(),
    );
    assert_eq!(common::idempotency_key(&canonical).ok(), Some(id));
}

fuzz_target!(|input: Input| {
    check_phone(input.phone);
    check_cursor(input.cursor, input.timestamp, Uuid::from_u128(input.id));
    check_idempotency_key(input.key, input.second_key);
});
