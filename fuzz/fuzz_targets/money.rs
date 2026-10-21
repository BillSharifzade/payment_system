//! `money`: currency-code validation (also through Deserialize), `from_major_minor` and
//! Display (round-trip and sign), and checked arithmetic that errors instead of wrapping,
//! against a reference model.
#![no_main]

use arbitrary::Arbitrary;
use libfuzzer_sys::fuzz_target;
use money::{Currency, Money, MoneyError};
use payment_fuzz::Wide;

#[derive(Arbitrary, Debug)]
struct Input<'a> {
    code: &'a str,
    exponent: u8,
    other_code: &'a str,
    other_exponent: u8,
    major: i64,
    minor: u32,
    a: i128,
    b: i128,
    wire_code: [u8; 3],
}

fn code_is_valid(code: &str) -> bool {
    code.len() == 3 && code.bytes().all(|b| b.is_ascii_alphabetic())
}

/// "[-]<int>[.<exponent digits>] <CODE>", built from the decimal string instead of div/mod.
fn reference_display(minor: i128, exponent: usize, code: &str) -> String {
    let digits = minor.unsigned_abs().to_string();
    let padded = format!("{digits:0>width$}", width = exponent + 1);
    let (int, frac) = padded.split_at(padded.len() - exponent);
    let sign = if minor < 0 { "-" } else { "" };
    if exponent == 0 {
        format!("{sign}{int} {code}")
    } else {
        format!("{sign}{int}.{frac} {code}")
    }
}

/// Parses `reference_display`'s grammar strictly; None for anything else.
fn parse_display(s: &str, exponent: usize, code: &str) -> Option<i128> {
    let number = s.strip_suffix(code)?.strip_suffix(' ')?;
    let (negative, unsigned) = match number.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, number),
    };
    let digits: String = if exponent == 0 {
        unsigned.to_string()
    } else {
        let (int, frac) = unsigned.split_once('.')?;
        if frac.len() != exponent || int.is_empty() {
            return None;
        }
        format!("{int}{frac}")
    };
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let magnitude: u128 = digits.parse().ok()?;
    if negative {
        if magnitude == 0 {
            return None; // "-0.00" is never printed
        }
        0i128.checked_sub_unsigned(magnitude)
    } else {
        i128::try_from(magnitude).ok()
    }
}

fn check_currency_code(code: &str, exponent: u8) -> Option<Currency> {
    match Currency::new(code, exponent) {
        Ok(c) => {
            assert!(code_is_valid(code), "accepted invalid code {code:?}");
            assert_eq!(c.code(), code.to_ascii_uppercase());
            assert_eq!(c.exponent(), exponent);
            assert_eq!(Currency::new(c.code(), exponent), Ok(c), "not idempotent");
            Some(c)
        }
        Err(MoneyError::InvalidCurrencyCode(s)) => {
            assert!(!code_is_valid(code), "rejected valid code {code:?}");
            assert_eq!(s, code);
            None
        }
        Err(MoneyError::InvalidExponent(e)) => {
            assert!(code_is_valid(code));
            assert_eq!(e, exponent);
            assert!(e > money::MAX_EXPONENT, "rejected exponent {e}");
            None
        }
        Err(e) => panic!("unexpected error for {code:?}/{exponent}: {e:?}"),
    }
}

fn check_major_minor(c: Currency, scale: i128, major: i64, minor: u32) {
    let exponent = c.exponent() as usize;
    let got = Money::from_major_minor(major, minor, c);
    if minor as i128 >= scale {
        assert_eq!(
            got,
            Err(MoneyError::MinorOutOfRange {
                minor,
                scale: u64::try_from(scale).expect("the reported scale is exact"),
            })
        );
        return;
    }
    // |major| · scale + minor in unsigned arithmetic, then the sign of major.
    let want = (major.unsigned_abs() as u128)
        .checked_mul(scale as u128)
        .and_then(|m| m.checked_add(minor as u128))
        .and_then(|m| {
            if major < 0 {
                0i128.checked_sub_unsigned(m)
            } else {
                i128::try_from(m).ok()
            }
        });
    match want {
        None => assert_eq!(
            got,
            Err(MoneyError::Overflow {
                operation: "from_major_minor"
            })
        ),
        Some(value) => {
            let m = got.expect("in range, so it must build");
            assert_eq!(m.minor_units(), value);
            assert_eq!(m.is_negative(), major < 0, "sign follows major");
            let shown = m.to_string();
            let expected = if exponent == 0 {
                format!("{major} {}", c.code())
            } else {
                let sign = if major < 0 { "-" } else { "" };
                format!(
                    "{sign}{}.{minor:0exponent$} {}",
                    major.unsigned_abs(),
                    c.code()
                )
            };
            assert_eq!(shown, expected);
        }
    }
}

fn check_display(c: Currency, a: i128) {
    let exponent = c.exponent() as usize;
    let m = Money::from_minor(a, c);
    let shown = m.to_string();
    assert_eq!(shown, reference_display(a, exponent, c.code()));
    assert_eq!(parse_display(&shown, exponent, c.code()), Some(a));
    assert_eq!(shown.starts_with('-'), a < 0);
    assert_eq!(m.is_negative(), a < 0);
    assert_eq!(m.is_zero(), a == 0);
    assert_eq!(m.is_positive(), a > 0);
}

fn check_arithmetic(c: Currency, other: Option<Currency>, a: i128, b: i128) {
    let x = Money::from_minor(a, c);
    let y = Money::from_minor(b, c);

    let mut sum = Wide::of(a);
    sum.add(b);
    match sum.to_i128() {
        Some(v) => assert_eq!(x.checked_add(&y), Ok(Money::from_minor(v, c))),
        None => assert_eq!(
            x.checked_add(&y),
            Err(MoneyError::Overflow { operation: "add" })
        ),
    }
    let mut diff = Wide::of(a);
    diff.sub(b);
    match diff.to_i128() {
        Some(v) => assert_eq!(x.checked_sub(&y), Ok(Money::from_minor(v, c))),
        None => assert_eq!(
            x.checked_sub(&y),
            Err(MoneyError::Overflow { operation: "sub" })
        ),
    }
    let mut neg = Wide::default();
    neg.sub(a);
    match neg.to_i128() {
        Some(v) => assert_eq!(x.checked_neg(), Ok(Money::from_minor(v, c))),
        None => assert_eq!(
            x.checked_neg(),
            Err(MoneyError::Overflow { operation: "neg" })
        ),
    }
    assert_eq!(x.try_cmp(&y), Ok(a.cmp(&b)));
    if let Ok(s) = x.checked_add(&y) {
        assert_eq!(s.checked_sub(&y), Ok(x), "add then sub is the identity");
    }

    if let Some(o) = other.filter(|o| *o != c) {
        let z = Money::from_minor(b, o);
        let mismatch = MoneyError::CurrencyMismatch { left: c, right: o };
        assert_eq!(x.checked_add(&z), Err(mismatch.clone()));
        assert_eq!(x.checked_sub(&z), Err(mismatch.clone()));
        assert_eq!(x.try_cmp(&z), Err(mismatch));
    }
}

/// Deserialisation accepts exactly the canonical form `Currency::new` produces.
fn check_deserialise(code: [u8; 3], exponent: u8) {
    let json = format!(
        r#"{{"code":[{},{},{}],"exponent":{exponent}}}"#,
        code[0], code[1], code[2]
    );
    let got = serde_json::from_str::<Currency>(&json);
    let canonical = code.iter().all(u8::is_ascii_uppercase) && exponent <= money::MAX_EXPONENT;
    assert_eq!(got.is_ok(), canonical, "{json}");
    if let Ok(c) = got {
        let text = std::str::from_utf8(&code).unwrap();
        assert_eq!(Currency::new(text, exponent), Ok(c));
        let m = Money::from_minor(-1, c);
        let back: Money = serde_json::from_str(&serde_json::to_string(&m).unwrap()).unwrap();
        assert_eq!(back, m);
    }
}

fuzz_target!(|input: Input| {
    check_deserialise(input.wire_code, input.exponent);
    let Some(c) = check_currency_code(input.code, input.exponent) else {
        return;
    };
    let other = check_currency_code(input.other_code, input.other_exponent);
    let scale = 10i128
        .checked_pow(c.exponent() as u32)
        .expect("an accepted currency has a representable scale");

    check_major_minor(c, scale, input.major, input.minor);
    check_display(c, input.a);
    check_display(c, input.b);
    check_arithmetic(c, other, input.a, input.b);
});
