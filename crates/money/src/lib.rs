mod currency;
mod error;

pub use currency::{Currency, CurrencyCode};
pub use error::{MoneyError, Result};

use core::cmp::Ordering;
use core::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct Money {
    minor_units: i128,
    currency: Currency,
}

impl Money {
    pub const fn from_minor(minor_units: i128, currency: Currency) -> Self {
        Self {
            minor_units,
            currency,
        }
    }

    pub const fn zero(currency: Currency) -> Self {
        Self::from_minor(0, currency)
    }

    pub fn from_major_minor(major: i64, minor: u32, currency: Currency) -> Result<Self> {
        let scale = currency.minor_per_major();
        if (minor as i128) >= scale {
            return Err(MoneyError::MinorOutOfRange {
                minor,
                scale: scale as u64,
            });
        }
        let major_part = (major as i128)
            .checked_mul(scale)
            .ok_or(MoneyError::Overflow {
                operation: "from_major_minor",
            })?;
        let minor_part = minor as i128;
        let signed_minor = if major < 0 { -minor_part } else { minor_part };
        let minor_units = major_part
            .checked_add(signed_minor)
            .ok_or(MoneyError::Overflow {
                operation: "from_major_minor",
            })?;
        Ok(Self::from_minor(minor_units, currency))
    }

    pub const fn minor_units(&self) -> i128 {
        self.minor_units
    }

    pub const fn currency(&self) -> Currency {
        self.currency
    }

    pub const fn is_zero(&self) -> bool {
        self.minor_units == 0
    }

    pub const fn is_negative(&self) -> bool {
        self.minor_units < 0
    }

    pub const fn is_positive(&self) -> bool {
        self.minor_units > 0
    }

    pub fn checked_add(&self, other: &Money) -> Result<Money> {
        self.ensure_same_currency(other)?;
        let minor_units = self
            .minor_units
            .checked_add(other.minor_units)
            .ok_or(MoneyError::Overflow { operation: "add" })?;
        Ok(Money::from_minor(minor_units, self.currency))
    }

    pub fn checked_sub(&self, other: &Money) -> Result<Money> {
        self.ensure_same_currency(other)?;
        let minor_units = self
            .minor_units
            .checked_sub(other.minor_units)
            .ok_or(MoneyError::Overflow { operation: "sub" })?;
        Ok(Money::from_minor(minor_units, self.currency))
    }

    pub fn checked_neg(&self) -> Result<Money> {
        let minor_units = self
            .minor_units
            .checked_neg()
            .ok_or(MoneyError::Overflow { operation: "neg" })?;
        Ok(Money::from_minor(minor_units, self.currency))
    }

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

impl fmt::Display for Money {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scale = self.currency.minor_per_major();
        let exponent = self.currency.exponent() as usize;
        let negative = self.minor_units < 0;
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
        assert_eq!(Currency::new("usd", 2).unwrap().code(), "USD");
    }

    #[test]
    fn zero_currency_formats_without_decimals() {
        let jpy = Currency::new("JPY", 0).unwrap();
        assert_eq!(Money::from_minor(500, jpy).to_string(), "500 JPY");
    }

    proptest! {
        #[test]
        fn add_is_commutative(a in i64::MIN..i64::MAX, b in i64::MIN..i64::MAX) {
            let x = Money::from_minor(a as i128, tjs());
            let y = Money::from_minor(b as i128, tjs());
            prop_assert_eq!(
                x.checked_add(&y).unwrap().minor_units(),
                y.checked_add(&x).unwrap().minor_units()
            );
        }

        #[test]
        fn add_then_sub_is_identity(a in i64::MIN..i64::MAX, b in i64::MIN..i64::MAX) {
            let x = Money::from_minor(a as i128, tjs());
            let y = Money::from_minor(b as i128, tjs());
            let back = x.checked_add(&y).unwrap().checked_sub(&y).unwrap();
            prop_assert_eq!(back.minor_units(), a as i128);
        }
    }
}
