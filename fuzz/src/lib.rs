//! Reference models shared by the fuzz targets. They are written independently of the code
//! under test (wide integers instead of checked i128 arithmetic, explicit per-currency sums),
//! so a target compares two implementations instead of re-running one.

use std::collections::BTreeMap;

use arbitrary::Arbitrary;
use ledger::{Direction, Entry, LedgerError};
use money::{Currency, MoneyError};

/// An exact sum of i128 values, `hi · 2^128 + lo`, that cannot overflow for any input a
/// fuzzer can produce.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
pub struct Wide {
    hi: i64,
    lo: i128,
}

impl Wide {
    pub fn add(&mut self, x: i128) {
        let (lo, overflow) = self.lo.overflowing_add(x);
        self.lo = lo;
        if overflow {
            self.hi += if x > 0 { 1 } else { -1 };
        }
    }

    pub fn sub(&mut self, x: i128) {
        let (lo, overflow) = self.lo.overflowing_sub(x);
        self.lo = lo;
        if overflow {
            self.hi += if x < 0 { 1 } else { -1 };
        }
    }

    pub fn of(x: i128) -> Self {
        Self { hi: 0, lo: x }
    }

    pub fn to_i128(self) -> Option<i128> {
        (self.hi == 0).then_some(self.lo)
    }

    pub fn is_zero(self) -> bool {
        self.hi == 0 && self.lo == 0
    }

    pub fn is_negative(self) -> bool {
        self.hi < 0 || (self.hi == 0 && self.lo < 0)
    }
}

/// Amounts biased toward the edges of i128 so overflow paths are reached in a few steps.
#[derive(Arbitrary, Debug, Clone, Copy)]
pub enum Amount {
    Small(u16),
    Large(u64),
    NearMax(u8),
    NearMin(u8),
    Zero,
    Negative(u16),
    Raw(i128),
}

impl Amount {
    pub fn minor(self) -> i128 {
        match self {
            Amount::Small(v) => v as i128,
            Amount::Large(v) => v as i128,
            Amount::NearMax(k) => i128::MAX - k as i128,
            Amount::NearMin(k) => i128::MIN + k as i128,
            Amount::Zero => 0,
            Amount::Negative(v) => -(v as i128) - 1,
            Amount::Raw(v) => v,
        }
    }
}

/// Error kinds of the ledger, without payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Kind {
    TooFewEntries,
    NonPositiveAmount,
    CurrencyClash,
    Overflow,
    Unbalanced,
    UnknownAccount,
    AccountCurrencyMismatch,
    InsufficientFunds,
    Duplicate,
}

pub fn kind(e: &LedgerError) -> Kind {
    match e {
        LedgerError::TooFewEntries { .. } => Kind::TooFewEntries,
        LedgerError::NonPositiveAmount => Kind::NonPositiveAmount,
        LedgerError::Money(MoneyError::CurrencyMismatch { .. }) => Kind::CurrencyClash,
        LedgerError::Money(MoneyError::Overflow { .. }) => Kind::Overflow,
        LedgerError::Money(other) => panic!("unexpected money error from the ledger: {other:?}"),
        LedgerError::Unbalanced { .. } => Kind::Unbalanced,
        LedgerError::UnknownAccount(_) => Kind::UnknownAccount,
        LedgerError::AccountCurrencyMismatch { .. } => Kind::AccountCurrencyMismatch,
        LedgerError::InsufficientFunds { .. } => Kind::InsufficientFunds,
        LedgerError::DuplicateTransaction(_) => Kind::Duplicate,
    }
}

/// What `Transaction::validate` must answer for these entries.
#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    Valid,
    /// Exactly this error.
    Exactly(LedgerError),
    /// Any of these kinds: when several entries are bad, which one is reported first depends
    /// on entry order.
    AnyOf(Vec<Kind>),
}

/// The specification of a balanced transaction: at least two entries, every amount strictly
/// positive, one exponent per currency code, and per currency the total debits equal the
/// total credits, each total representable as an i128. Order-independent by construction.
pub fn validate_verdict(entries: &[Entry]) -> Verdict {
    if entries.len() < 2 {
        return Verdict::Exactly(LedgerError::TooFewEntries {
            count: entries.len(),
        });
    }
    let mut reasons = Vec::new();
    let mut exponents: BTreeMap<String, Currency> = BTreeMap::new();
    // Per full currency, so the verdict does not depend on which exponent of a clashing code
    // comes first.
    let mut totals: BTreeMap<Currency, (Wide, Wide)> = BTreeMap::new();
    for e in entries {
        let minor = e.amount.minor_units();
        let currency = e.amount.currency();
        if minor <= 0 {
            reasons.push(Kind::NonPositiveAmount);
            continue;
        }
        if *exponents
            .entry(currency.code().to_string())
            .or_insert(currency)
            != currency
        {
            reasons.push(Kind::CurrencyClash);
        }
        let (debits, credits) = totals.entry(currency).or_default();
        match e.direction {
            Direction::Debit => debits.add(minor),
            Direction::Credit => credits.add(minor),
        }
    }
    for (debits, credits) in totals.values() {
        if debits.to_i128().is_none() || credits.to_i128().is_none() {
            reasons.push(Kind::Overflow);
        }
    }
    if !reasons.is_empty() {
        reasons.sort();
        reasons.dedup();
        return Verdict::AnyOf(reasons);
    }
    // One exponent per code here, so currency order is code order: the first unbalanced
    // currency is what a deterministic implementation reports.
    for (currency, (debits, credits)) in totals {
        if debits != credits {
            let mut net = credits;
            net.sub(debits.to_i128().expect("checked above"));
            return Verdict::Exactly(LedgerError::Unbalanced {
                currency,
                net_minor: net
                    .to_i128()
                    .expect("both totals fit, so does their difference"),
            });
        }
    }
    Verdict::Valid
}

/// Asserts that `got` is what `verdict` allows.
pub fn check_verdict(got: &Result<(), LedgerError>, verdict: &Verdict, ctx: &dyn core::fmt::Debug) {
    match (verdict, got) {
        (Verdict::Valid, Ok(())) => {}
        (Verdict::Exactly(want), Err(e)) if want == e => {}
        (Verdict::AnyOf(kinds), Err(e)) if kinds.contains(&kind(e)) => {}
        _ => panic!("validate answered {got:?}, the specification says {verdict:?}\n{ctx:?}"),
    }
}

/// Balancing legs for `entries`: per currency (in first-seen order), entries that bring
/// credits − debits to zero, each at most i128::MAX, sent to `account_for(currency)`.
pub fn balancing_legs(
    entries: &[Entry],
    mut account_for: impl FnMut(Currency) -> ledger::AccountId,
) -> Vec<Entry> {
    let mut order: Vec<Currency> = Vec::new();
    let mut nets: Vec<Wide> = Vec::new();
    for e in entries {
        let c = e.amount.currency();
        let i = match order.iter().position(|o| *o == c) {
            Some(i) => i,
            None => {
                order.push(c);
                nets.push(Wide::default());
                order.len() - 1
            }
        };
        match e.direction {
            Direction::Credit => nets[i].add(e.amount.minor_units()),
            Direction::Debit => nets[i].sub(e.amount.minor_units()),
        }
    }
    let mut legs = Vec::new();
    for (c, mut net) in order.into_iter().zip(nets) {
        let account = account_for(c);
        while !net.is_zero() {
            let (direction, chunk) = if net.is_negative() {
                let chunk = if net < Wide::of(-i128::MAX) {
                    i128::MAX
                } else {
                    -net.to_i128().expect("in range")
                };
                net.add(chunk);
                (Direction::Credit, chunk)
            } else {
                let chunk = if net > Wide::of(i128::MAX) {
                    i128::MAX
                } else {
                    net.to_i128().expect("in range")
                };
                net.sub(chunk);
                (Direction::Debit, chunk)
            };
            legs.push(Entry::new(
                account,
                direction,
                money::Money::from_minor(chunk, c),
            ));
        }
    }
    legs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wide_is_exact_past_i128() {
        let mut w = Wide::default();
        w.add(i128::MAX);
        w.add(i128::MAX);
        assert_eq!(w.to_i128(), None);
        w.sub(i128::MAX);
        assert_eq!(w.to_i128(), Some(i128::MAX));
        w.sub(i128::MAX);
        w.sub(i128::MAX);
        w.add(i128::MIN);
        assert!(w.is_negative());
        w.sub(i128::MIN);
        w.add(i128::MAX);
        assert!(w.is_zero());
    }
}
