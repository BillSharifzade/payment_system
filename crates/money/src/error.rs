use crate::Currency;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    #[error("currency mismatch: {left} vs {right}")]
    CurrencyMismatch { left: Currency, right: Currency },

    #[error("arithmetic overflow in {operation}")]
    Overflow { operation: &'static str },

    #[error("invalid currency code: {0:?}")]
    InvalidCurrencyCode(String),

    #[error("currency exponent {0} exceeds the maximum of {max}", max = crate::MAX_EXPONENT)]
    InvalidExponent(u8),

    #[error("could not parse {input:?} as an amount in {currency}")]
    ParseError { input: String, currency: Currency },

    #[error("minor part {minor} is out of range (must be < {scale})")]
    MinorOutOfRange { minor: u32, scale: u64 },
}

pub type Result<T> = core::result::Result<T, MoneyError>;
