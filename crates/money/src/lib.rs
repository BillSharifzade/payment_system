//! Currency-safe integer money for the payment system.
//!
//! # The one rule
//!
//! **Money is never a float.** `0.10` cannot be represented exactly in binary
//! floating point, so floats silently lose fractions of a cent — the classic
//! cause of "the books don't balance". Every amount here is a signed 128-bit
//! integer **count of minor units** (diram for TJS), and the [`Currency`] knows
//! how many minor units make a major unit.
//!
//! # What this type guarantees
//!
//! - **No silent precision loss** — integers only.
//! - **No silent overflow** — every operation is checked and returns
//!   [`MoneyError::Overflow`] rather than wrapping.
//! - **No accidental currency mixing** — adding TJS to USD is a
//!   [`MoneyError::CurrencyMismatch`], not a wrong number. Crossing currencies
//!   must go through an explicit FX transaction at a higher layer.
//!
//! This crate is intentionally pure: no database, no async, no I/O. That keeps
//! it trivial to test exhaustively (see the property tests) and makes it the
//! stable bedrock the ledger is built on.

mod currency;
mod error;

pub use currency::{Currency, CurrencyCode};
pub use error::{MoneyError, Result};

use core::cmp::Ordering;
use core::fmt;

/// An amount of money in a specific currency, stored as an integer number of
/// minor units (e.g. diram for TJS).
///
/// `i128` is deliberate: it holds amounts far beyond any plausible monetary
/// total (~1.7×10^38 minor units) so the only realistic overflow is a genuine
/// bug, which we surface as an error rather than wrapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Money {
    /// Signed count of minor units. Negative is allowed (e.g. a debit posting,
    /// a reversal, or a system account that runs negative by design).
    minor_units: i128,
    currency: Currency,
}

impl Money {
    /// Construct from a raw count of minor units (e.g. `Money::from_minor(5000, tjs)`
    /// is 50.00 TJS).
    pub const fn from_minor(minor_units: i128, currency: Currency) -> Self {
        Self {
            minor_units,
            currency,
        }
    }

    /// Zero in the given currency.
    pub const fn zero(currency: Currency) -> Self {
        Self::from_minor(0, currency)
    }

    /// Construct from separate major and minor parts, e.g. `(50, 25)` TJS = 50.25 TJS.
    ///
    /// `minor` must be within the currency's minor range (0..10^exponent) and is
    /// combined with the sign of `major`. Returns an error on overflow.
    pub fn from_major_minor(major: i64, minor: u32, currency: Currency) -> Result<Self> {
        let scale = currency.minor_per_major();
        let major_part = (major as i128)
            .checked_mul(scale)
            .ok_or(MoneyError::Overflow {
                operation: "from_major_minor",
            })?;
        let minor_part = minor as i128;
        // Apply the major sign to the minor part so (-1, 50) reads as -1.50, not -0.50.
        let signed_minor = if major < 0 { -minor_part } else { minor_part };
        let minor_units = major_part
            .checked_add(signed_minor)
            .ok_or(MoneyError::Overflow {
                operation: "from_major_minor",
            })?;
        Ok(Self::from_minor(minor_units, currency))
    }

    /// The raw count of minor units.
    pub const fn minor_units(&self) -> i128 {
        self.minor_units
    }

    /// The currency of this amount.
    pub const fn currency(&self) -> Currency {
        self.currency
    }

    /// True if the amount is exactly zero.
    pub const fn is_zero(&self) -> bool {
        self.minor_units == 0
    }

    /// True if the amount is strictly negative.
    pub const fn is_negative(&self) -> bool {
        self.minor_units < 0
    }

    /// True if the amount is strictly positive.
    pub const fn is_positive(&self) -> bool {
        self.minor_units > 0
    }

    /// Checked addition. Errors on currency mismatch or overflow.
    pub fn checked_add(&self, other: &Money) -> Result<Money> {
        self.ensure_same_currency(other)?;
        let minor_units = self
            .minor_units
            .checked_add(other.minor_units)
            .ok_or(MoneyError::Overflow { operation: "add" })?;
        Ok(Money::from_minor(minor_units, self.currency))
    }

    /// Checked subtraction. Errors on currency mismatch or overflow.
    pub fn checked_sub(&self, other: &Money) -> Result<Money> {
        self.ensure_same_currency(other)?;
        let minor_units = self
            .minor_units
            .checked_sub(other.minor_units)
            .ok_or(MoneyError::Overflow { operation: "sub" })?;
        Ok(Money::from_minor(minor_units, self.currency))
    }

    /// Checked negation (flips debit/credit sign). Errors only on overflow
    /// (i.e. negating `i128::MIN`).
    pub fn checked_neg(&self) -> Result<Money> {
        let minor_units = self
            .minor_units
            .checked_neg()
            .ok_or(MoneyError::Overflow { operation: "neg" })?;
        Ok(Money::from_minor(minor_units, self.currency))
    }

    /// Compare two amounts of the *same* currency. Returns an error rather than
    /// a misleading ordering if the currencies differ.
    pub fn try_cmp(&self, other: &Money) -> Result<Ordering> {
        self.ensure_same_currency(other)?;
        Ok(self.minor_units.cmp(&other.minor_units))
    }

    fn ensure_same_currency(&self, other: &Money) -> Result<()> {
        if self.currency != other.currency {
            return Err(MoneyError::CurrencyMismatch {
                left: self.currency,
                right: other.currency,
            });
        }
        Ok(())
    }
}

/// Formats as a decimal string with the currency's number of fractional digits,
/// e.g. `50.25 TJS` or `-1.00 TJS`. Pure formatting — no rounding.
impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scale = self.currency.minor_per_major();
        let exponent = self.currency.exponent() as usize;
        let negative = self.minor_units < 0;
        // Work in unsigned magnitude to keep the formatting logic simple.
        let magnitude = self.minor_units.unsigned_abs();
        let scale_u = scale as u128;
        let major = magnitude / scale_u;
        let minor = magnitude % scale_u;
        let sign = if negative { "-" } else { "" };
        if exponent == 0 {
            write!(f, "{sign}{major} {}", self.currency)
        } else {
            write!(
                f,
                "{sign}{major}.{minor:0width$} {}",
                self.currency,
                width = exponent
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn tjs() -> Currency {
        Currency::tjs()
    }

    #[test]
    fn from_major_minor_builds_expected_minor_units() {
        let m = Money::from_major_minor(50, 25, tjs()).unwrap();
        assert_eq!(m.minor_units(), 5025);
        assert_eq!(m.to_string(), "50.25 TJS");
    }

    #[test]
    fn negative_major_carries_sign_to_minor() {
        let m = Money::from_major_minor(-1, 50, tjs()).unwrap();
        assert_eq!(m.minor_units(), -150);
        assert_eq!(m.to_string(), "-1.50 TJS");
    }

    #[test]
    fn add_and_sub_same_currency() {
        let a = Money::from_minor(5000, tjs());
        let b = Money::from_minor(2500, tjs());
        assert_eq!(a.checked_add(&b).unwrap().minor_units(), 7500);
        assert_eq!(a.checked_sub(&b).unwrap().minor_units(), 2500);
    }

    #[test]
    fn mixing_currencies_is_an_error() {
        let somoni = Money::from_minor(100, tjs());
        let dollars = Money::from_minor(100, Currency::new("USD", 2).unwrap());
        assert!(matches!(
            somoni.checked_add(&dollars),
            Err(MoneyError::CurrencyMismatch { .. })
        ));
        assert!(somoni.try_cmp(&dollars).is_err());
    }

    #[test]
    fn overflow_is_an_error_not_a_wrap() {
        let max = Money::from_minor(i128::MAX, tjs());
        let one = Money::from_minor(1, tjs());
        assert!(matches!(
            max.checked_add(&one),
            Err(MoneyError::Overflow { .. })
        ));
    }

    #[test]
    fn invalid_currency_codes_rejected() {
        assert!(Currency::new("TJSX", 2).is_err());
        assert!(Currency::new("T1S", 2).is_err());
        assert!(Currency::new("tj", 2).is_err());
        // lowercase is normalised, not rejected
        assert_eq!(Currency::new("usd", 2).unwrap().code(), "USD");
    }

    #[test]
    fn zero_currency_formats_without_decimals() {
        let jpy = Currency::new("JPY", 0).unwrap();
        assert_eq!(Money::from_minor(500, jpy).to_string(), "500 JPY");
    }

    proptest! {
        // The property that underpins the whole ledger: addition of same-currency
        // money is exactly integer addition — associative, commutative, and never
        // lossy — for any amounts that don't overflow.
        #[test]
        fn add_is_commutative(a in i64::MIN..i64::MAX, b in i64::MIN..i64::MAX) {
            let x = Money::from_minor(a as i128, tjs());
            let y = Money::from_minor(b as i128, tjs());
            prop_assert_eq!(
                x.checked_add(&y).unwrap().minor_units(),
                y.checked_add(&x).unwrap().minor_units()
            );
        }

        // a + b - b == a, always. No precision is ever lost. (This is exactly
        // what floats fail to guarantee, and why we use integers.)
        #[test]
        fn add_then_sub_is_identity(a in i64::MIN..i64::MAX, b in i64::MIN..i64::MAX) {
            let x = Money::from_minor(a as i128, tjs());
            let y = Money::from_minor(b as i128, tjs());
            let back = x.checked_add(&y).unwrap().checked_sub(&y).unwrap();
            prop_assert_eq!(back.minor_units(), a as i128);
        }
    }
}
