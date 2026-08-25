use crate::error::{MoneyError, Result};
use core::fmt;

/// A currency, identified by its ISO-4217-style 3-letter code and the number of
/// decimal places (the *exponent*) it uses.
///
/// The exponent is how many minor units make up one major unit:
/// - TJS (Somoni) has exponent 2 → 1 TJS = 100 diram.
/// - JPY (Yen) has exponent 0 → 1 JPY = 1 (no minor unit).
///
/// We launch with TJS only, but currencies are *data*, not hard-coded variants:
/// adding one is a new [`Currency`] value (and, later, a row in the database),
/// never a schema or code change. That is the "extensible from day one" goal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Currency {
    code: CurrencyCode,
    exponent: u8,
}

impl Currency {
    /// Construct a currency from a code and exponent.
    ///
    /// Returns [`MoneyError::InvalidCurrencyCode`] if `code` is not exactly three
    /// ASCII letters.
    pub fn new(code: &str, exponent: u8) -> Result<Self> {
        Ok(Self {
            code: CurrencyCode::new(code)?,
            exponent,
        })
    }

    /// The Tajikistani Somoni — our launch currency. 1 TJS = 100 diram.
    pub const fn tjs() -> Self {
        Self {
            // Safe: "TJS" is three ASCII uppercase letters.
            code: CurrencyCode([b'T', b'J', b'S']),
            exponent: 2,
        }
    }

    /// The three-letter code, e.g. `"TJS"`.
    pub fn code(&self) -> &str {
        self.code.as_str()
    }

    /// Number of minor units per major unit's decimal place (e.g. 2 for TJS).
    pub fn exponent(&self) -> u8 {
        self.exponent
    }

    /// 10^exponent — the number of minor units in one major unit
    /// (100 for TJS, 1 for JPY). Used for parsing and formatting.
    pub(crate) fn minor_per_major(&self) -> i128 {
        // exponent is small (currencies top out around 4); this cannot overflow i128.
        10i128.pow(self.exponent as u32)
    }
}

impl fmt::Display for Currency {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code.as_str())
    }
}

/// A validated three-letter currency code. Kept small and `Copy` so passing
/// currencies around is free.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CurrencyCode([u8; 3]);

impl CurrencyCode {
    /// Validate and construct from a string. Must be exactly three ASCII letters;
    /// lowercase is accepted and normalised to uppercase.
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
        // Safe: the only constructor guarantees three ASCII letters.
        core::str::from_utf8(&self.0).expect("currency code is valid ASCII by construction")
    }
}

impl fmt::Debug for CurrencyCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "CurrencyCode({:?})", self.as_str())
    }
}
