use crate::error::{MoneyError, Result};
use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Currency {
    code: CurrencyCode,
    exponent: u8,
}

impl Currency {
    pub fn new(code: &str, exponent: u8) -> Result<Self> {
        Ok(Self {
            code: CurrencyCode::new(code)?,
            exponent,
        })
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

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct CurrencyCode([u8; 3]);

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
