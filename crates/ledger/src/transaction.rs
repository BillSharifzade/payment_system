use std::collections::BTreeMap;

use crate::error::{LedgerError, Result};
use crate::ids::{AccountId, EntryId, TransactionId};
use money::{Currency, Money};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Direction {
    Debit,
    Credit,
}

impl Direction {
    pub fn as_db_str(&self) -> &'static str {
        match self {
            Direction::Debit => "debit",
            Direction::Credit => "credit",
        }
    }
}

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

    pub fn debit(account_id: AccountId, amount: Money) -> Self {
        Self::new(account_id, Direction::Debit, amount)
    }

    pub fn credit(account_id: AccountId, amount: Money) -> Self {
        Self::new(account_id, Direction::Credit, amount)
    }

    pub fn signed_amount(&self) -> Result<Money> {
        match self.direction {
            Direction::Credit => Ok(self.amount),
            Direction::Debit => Ok(self.amount.checked_neg()?),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Transaction {
    pub id: TransactionId,
    pub entries: Vec<Entry>,
}

impl Transaction {
    pub fn new(id: TransactionId, entries: Vec<Entry>) -> Self {
        Self { id, entries }
    }

    pub fn with_entries(entries: Vec<Entry>) -> Self {
        Self::new(TransactionId::new(), entries)
    }

    pub fn validate(&self) -> Result<()> {
        if self.entries.len() < 2 {
            return Err(LedgerError::TooFewEntries {
                count: self.entries.len(),
            });
        }

        // Debits and credits are totalled apart: sums of positive amounts overflow regardless
        // of entry order, where a running net would accept or refuse the same entries
        // depending on how they are ordered.
        let mut totals: BTreeMap<String, (Currency, Money, Money)> = BTreeMap::new();
        for entry in &self.entries {
            if !entry.amount.is_positive() {
                return Err(LedgerError::NonPositiveAmount);
            }
            let currency = entry.amount.currency();
            let (_, debits, credits) = totals
                .entry(currency.code().to_string())
                .or_insert_with(|| (currency, Money::zero(currency), Money::zero(currency)));
            let total = match entry.direction {
                Direction::Debit => debits,
                Direction::Credit => credits,
            };
            *total = total.checked_add(&entry.amount)?;
        }

        for (currency, debits, credits) in totals.into_values() {
            if debits != credits {
                return Err(LedgerError::Unbalanced {
                    currency,
                    net_minor: credits.minor_units() - debits.minor_units(),
                });
            }
        }

        Ok(())
    }
}
