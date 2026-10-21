use crate::error::{MoneyError, Result};
use core::fmt;

/// The largest exponent a currency may have: 10^18 is the largest power of ten in an i64, so
/// the scale fits every integer type amounts use, and `major · scale ± minor` cannot overflow
/// i128 for any i64 major. Real currencies use 0–4; the database allows 0–8.
pub const MAX_EXPONENT: u8 = 18;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "RawCurrency"))]
pub struct Currency {
    code: CurrencyCode,
    exponent: u8,
}

// Deserialisation goes through the same checks as `Currency::new`.
#[cfg(feature = "serde")]
#[derive(serde::Deserialize)]
struct RawCurrency {
    code: CurrencyCode,
    exponent: u8,
}

#[cfg(feature = "serde")]
impl TryFrom<RawCurrency> for Currency {
    type Error = MoneyError;

    fn try_from(raw: RawCurrency) -> Result<Self> {
        Self::with_code(raw.code, raw.exponent)
    }
}

impl Currency {
    pub fn new(code: &str, exponent: u8) -> Result<Self> {
        Self::with_code(CurrencyCode::new(code)?, exponent)
    }

    fn with_code(code: CurrencyCode, exponent: u8) -> Result<Self> {
        if exponent > MAX_EXPONENT {
            return Err(MoneyError::InvalidExponent(exponent));
        }
        Ok(Self { code, exponent })
    }

    pub const fn tjs() -> Self {
        Self {
            code: CurrencyCode(*b"TJS"),
            exponent: 2,
        }
    }

    pub fn code(&self) -> &str {
        self.code.as_str()
    }

    pub fn exponent(&self) -> u8 {
        self.exponent
    }

    pub(crate) fn minor_per_major(&self) -> i128 {
        10i128.pow(self.exponent as u32)
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code.as_str())
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "[u8; 3]"))]
pub struct CurrencyCode([u8; 3]);

// The serialised form is the canonical one: three uppercase ASCII letters.
impl TryFrom<[u8; 3]> for CurrencyCode {
    type Error = MoneyError;

    fn try_from(bytes: [u8; 3]) -> Result<Self> {
        if !bytes.iter().all(u8::is_ascii_uppercase) {
            return Err(MoneyError::InvalidCurrencyCode(
                String::from_utf8_lossy(&bytes).into_owned(),
            ));
        }
        Ok(Self(bytes))
    }
}

impl CurrencyCode {
    pub fn new(code: &str) -> Result<Self> {
        let bytes = code.as_bytes();
        if bytes.len() != 3 || !bytes.iter().all(|b| b.is_ascii_alphabetic()) {
            return Err(MoneyError::InvalidCurrencyCode(code.to_string()));
        }
        let mut out = [0u8; 3];
        for (i, b) in bytes.iter().enumerate() {
            out[i] = b.to_ascii_uppercase();
        }
        Ok(Self(out))
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.0).expect("currency code is valid ASCII by construction")
    }
}

impl fmt::Debug for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CurrencyCode({:?})", self.as_str())
    }
}
