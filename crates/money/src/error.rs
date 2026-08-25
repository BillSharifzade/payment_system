use crate::Currency;

/// Errors that can arise from money arithmetic and construction.
///
/// Every fallible money operation returns one of these instead of panicking or
/// silently producing a wrong value — in a ledger, a silent wrong value is the
/// worst possible outcome.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MoneyError {
    /// Attempted an operation between two different currencies (e.g. TJS + USD).
    /// Money in different currencies is never directly comparable or addable;
    /// crossing currencies must go through an explicit FX transaction.
    #[error("currency mismatch: {left} vs {right}")]
    CurrencyMismatch { left: Currency, right: Currency },

    /// An arithmetic operation overflowed the underlying integer.
    /// We never wrap silently — an overflow is a hard error.
    #[error("arithmetic overflow in {operation}")]
    Overflow { operation: &'static str },

    /// A currency code was not exactly three ASCII uppercase letters (ISO-4217 shape).
    #[error("invalid currency code: {0:?}")]
    InvalidCurrencyCode(String),

    /// Failed to parse a decimal string into minor units for the given currency.
    #[error("could not parse {input:?} as an amount in {currency}")]
    ParseError { input: String, currency: Currency },
}

/// Convenience alias for fallible money operations.
pub type Result<T> = core::result::Result<T, MoneyError>;
