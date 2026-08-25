use std::collections::BTreeMap;

use crate::error::{LedgerError, Result};
use crate::ids::{AccountId, EntryId, TransactionId};
use money::{Currency, Money};
use serde::{Deserialize, Serialize};

/// Whether an entry debits or credits its account.
///
/// We adopt the convention that, in the raw signed arithmetic used to check
/// balance, **a credit is positive and a debit is negative**. A transaction is
/// balanced when these signed amounts sum to zero (per currency).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Debit,
    Credit,
}

impl Direction {
    /// The stable string used to persist this direction (must match the DB CHECK
    /// constraint in the migrations).
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Direction::Debit => "debit",
            Direction::Credit => "credit",
        }
    }
}

/// A single posting against one account. The `amount` is always a positive
/// magnitude; the [`Direction`] carries the sign.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub id: EntryId,
    pub account_id: AccountId,
    pub direction: Direction,
    pub amount: Money,
}

impl Entry {
    pub fn new(account_id: AccountId, direction: Direction, amount: Money) -> Self {
        Self {
            id: EntryId::new(),
            account_id,
            direction,
            amount,
        }
    }

    /// Convenience: a debit posting.
    pub fn debit(account_id: AccountId, amount: Money) -> Self {
        Self::new(account_id, Direction::Debit, amount)
    }

    /// Convenience: a credit posting.
    pub fn credit(account_id: AccountId, amount: Money) -> Self {
        Self::new(account_id, Direction::Credit, amount)
    }

    /// The signed effect of this entry on raw balance: credit `+amount`,
    /// debit `-amount`. Errors only on the (impossible-in-practice) overflow of
    /// negating `i128::MIN`.
    pub fn signed_amount(&self) -> Result<Money> {
        match self.direction {
            Direction::Credit => Ok(self.amount),
            Direction::Debit => Ok(self.amount.checked_neg()?),
        }
    }
}

/// A logical money event, made of two or more balanced [`Entry`] postings.
///
/// A [`Transaction`] is *append-only and immutable* once posted. Corrections are
/// new, reversing transactions — never edits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    pub id: TransactionId,
    pub entries: Vec<Entry>,
}

impl Transaction {
    pub fn new(id: TransactionId, entries: Vec<Entry>) -> Self {
        Self { id, entries }
    }

    /// Build with a freshly generated id.
    pub fn with_entries(entries: Vec<Entry>) -> Self {
        Self::new(TransactionId::new(), entries)
    }

    /// Validate the transaction's *internal* consistency, independent of any
    /// ledger state:
    ///
    /// 1. at least two entries,
    /// 2. every amount strictly positive,
    /// 3. for **each** currency, debits and credits net to exactly zero.
    ///
    /// This is the structural half of correctness. The other half (accounts
    /// exist, currencies match, balances stay legal) needs ledger state and is
    /// enforced by the engine when posting.
    pub fn validate(&self) -> Result<()> {
        if self.entries.len() < 2 {
            return Err(LedgerError::TooFewEntries {
                count: self.entries.len(),
            });
        }

        // Net signed total per currency. A balanced transaction nets to zero in
        // every currency it touches (so multi-currency FX transactions must
        // balance each leg independently).
        let mut net_by_currency: BTreeMap<String, (Currency, Money)> = BTreeMap::new();

        for entry in &self.entries {
            if !entry.amount.is_positive() {
                return Err(LedgerError::NonPositiveAmount);
            }
            let currency = entry.amount.currency();
            let signed = entry.signed_amount()?;
            let slot = net_by_currency
                .entry(currency.code().to_string())
                .or_insert_with(|| (currency, Money::zero(currency)));
            slot.1 = slot.1.checked_add(&signed)?;
        }

        for (currency, net) in net_by_currency.into_values() {
            if !net.is_zero() {
                return Err(LedgerError::Unbalanced {
                    currency,
                    net_minor: net.minor_units(),
                });
            }
        }

        Ok(())
    }
}
