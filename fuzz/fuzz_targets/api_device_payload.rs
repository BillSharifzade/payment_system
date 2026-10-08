//! The device-signature payload (`api::devices::PaymentAuth::payload`) is injective: a strict
//! decoder recovers every field from the bytes (so no two field tuples can share them), and
//! a differential check confirms that two tuples give equal bytes only when they are equal.
#![no_main]

use api::devices::{MoneyMove, PaymentAuth, AUTH_PAYLOAD_TAG};
use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use uuid::Uuid;

#[derive(Arbitrary, Debug, Clone, PartialEq)]
struct Fields {
    kind: u8,
    user: u128,
    key: u128,
    from: Option<u128>,
    to: u128,
    amount: i64,
    currency: String,
    check: Option<u128>,
}

const KINDS: [MoneyMove; 3] = [MoneyMove::Transfer, MoneyMove::Check, MoneyMove::Fx];

impl Fields {
    fn normalised(mut self) -> Self {
        self.kind %= 3;
        self
    }

    fn auth(&self) -> PaymentAuth<'_> {
        PaymentAuth {
            kind: KINDS[self.kind as usize % 3],
            user_id: Uuid::from_u128(self.user),
            idempotency_key: Uuid::from_u128(self.key),
            from_account: self.from.map(Uuid::from_u128),
            to_account: Uuid::from_u128(self.to),
            amount_minor: self.amount,
            currency: &self.currency,
            check_id: self.check.map(Uuid::from_u128),
        }
    }
}

/// `<decimal byte length>:<value>;` nine times; the length has no sign, no leading zero.
fn decode(mut bytes: &[u8]) -> Option<Vec<&[u8]>> {
    let mut fields = Vec::new();
    while !bytes.is_empty() {
        let colon = bytes.iter().position(|&b| b == b':')?;
        let digits = std::str::from_utf8(&bytes[..colon]).ok()?;
        if digits.is_empty()
            || !digits.bytes().all(|b| b.is_ascii_digit())
            || (digits.len() > 1 && digits.starts_with('0'))
        {
            return None;
        }
        let len: usize = digits.parse().ok()?;
        let rest = &bytes[colon + 1..];
        if rest.len() < len + 1 || rest[len] != b';' {
            return None;
        }
        fields.push(&rest[..len]);
        bytes = &rest[len + 1..];
    }
    Some(fields)
}

fn uuid_field(f: &[u8]) -> Option<u128> {
    let s = std::str::from_utf8(f).ok()?;
    let u = Uuid::parse_str(s).ok()?;
    // Lowercase hyphenated only: one spelling per id.
    (u.hyphenated().to_string() == s).then(|| u.as_u128())
}

fn optional_uuid(f: &[u8]) -> Option<Option<u128>> {
    if f.is_empty() {
        Some(None)
    } else {
        uuid_field(f).map(Some)
    }
}

fn recover(payload: &[u8]) -> Option<Fields> {
    let f = decode(payload)?;
    let [tag, user, kind, key, from, to, amount, currency, check] = f[..] else {
        return None;
    };
    if tag != AUTH_PAYLOAD_TAG.as_bytes() {
        return None;
    }
    let kind = KINDS.iter().position(|k| k.wire().as_bytes() == kind)? as u8;
    let amount_text = std::str::from_utf8(amount).ok()?;
    let amount: i64 = amount_text.parse().ok()?;
    if amount.to_string() != amount_text {
        return None;
    }
    Some(Fields {
        kind,
        user: uuid_field(user)?,
        key: uuid_field(key)?,
        from: optional_uuid(from)?,
        to: uuid_field(to)?,
        amount,
        currency: String::from_utf8(currency.to_vec()).ok()?,
        check: optional_uuid(check)?,
    })
}

fuzz_target!(|pair: (Fields, Fields)| {
    let (a, b) = (pair.0.normalised(), pair.1.normalised());
    let pa = a.auth().payload();
    assert_eq!(
        recover(&pa).as_ref(),
        Some(&a),
        "{:?}",
        String::from_utf8_lossy(&pa)
    );
    assert_eq!(a.auth().payload(), pa, "deterministic");
    let pb = b.auth().payload();
    assert_eq!(pa == pb, a == b, "{a:?} / {b:?}");
});
