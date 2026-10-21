use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use crate::account::{Account, NormalSide};
use crate::error::{LedgerError, Result};
use crate::ids::{AccountId, TransactionId};
use crate::transaction::Transaction;
use money::{Currency, Money};

pub trait LedgerEngine {
    fn open_account(&mut self, account: Account) -> Result<()>;

    fn post(&mut self, txn: &Transaction) -> Result<()>;

    fn balance(&self, account: AccountId) -> Result<Money>;
}

#[derive(Debug, Default)]
pub struct InMemoryLedger {
    accounts: HashMap<AccountId, Account>,
    raw_balances: HashMap<AccountId, Money>,
    posted: HashSet<TransactionId>,
}

impl InMemoryLedger {
    pub fn new() -> Self {
        Self::default()
    }

    fn orient(account: &Account, raw: Money) -> Result<Money> {
        match account.normal_side() {
            NormalSide::Credit => Ok(raw),
            NormalSide::Debit => Ok(raw.checked_neg()?),
        }
    }

    pub fn is_conserved(&self) -> bool {
        let currencies: BTreeSet<Currency> = self
            .raw_balances
            .values()
            .map(|raw| raw.currency())
            .collect();
        currencies
            .into_iter()
            .all(|c| self.net_minor_units(c) == Some(0))
    }

    /// The exact sum of the raw balances in `currency`, or None if it is not an i128. A plain
    /// i128 sum could overflow on the way to zero, depending on iteration order.
    pub fn net_minor_units(&self, currency: Currency) -> Option<i128> {
        let (mut sum, mut wraps) = (0i128, 0i64);
        for raw in self
            .raw_balances
            .values()
            .filter(|r| r.currency() == currency)
        {
            let (next, overflowed) = sum.overflowing_add(raw.minor_units());
            sum = next;
            if overflowed {
                wraps += if raw.is_negative() { -1 } else { 1 };
            }
        }
        (wraps == 0).then_some(sum)
    }
}

impl LedgerEngine for InMemoryLedger {
    // Re-opening an id keeps the account as first opened, as Postgres's ON CONFLICT DO NOTHING
    // does: replacing it could, say, re-type a negative system account as a user wallet.
    fn open_account(&mut self, account: Account) -> Result<()> {
        let id = account.id;
        let currency = account.currency;
        self.accounts.entry(id).or_insert(account);
        self.raw_balances
            .entry(id)
            .or_insert_with(|| Money::zero(currency));
        Ok(())
    }

    fn post(&mut self, txn: &Transaction) -> Result<()> {
        if self.posted.contains(&txn.id) {
            return Err(LedgerError::DuplicateTransaction(txn.id));
        }

        txn.validate()?;

        // One net change per account. Validation bounds an account's debits and credits by the
        // transaction's totals, so the net fits whatever the entry order; a BTreeMap makes the
        // account an error names independent of hashing.
        let mut deltas: BTreeMap<AccountId, (&Account, Money)> = BTreeMap::new();
        for entry in &txn.entries {
            let account = self
                .accounts
                .get(&entry.account_id)
                .ok_or(LedgerError::UnknownAccount(entry.account_id))?;

            if account.currency != entry.amount.currency() {
                return Err(LedgerError::AccountCurrencyMismatch {
                    account: entry.account_id,
                    account_currency: account.currency,
                    entry_currency: entry.amount.currency(),
                });
            }

            let (_, delta) = deltas
                .entry(entry.account_id)
                .or_insert((account, Money::zero(account.currency)));
            *delta = delta.checked_add(&entry.signed_amount()?)?;
        }

        let mut next = Vec::with_capacity(deltas.len());
        for (account_id, (account, delta)) in deltas {
            let current = self.raw_balances[&account_id];
            let raw = current.checked_add(&delta)?;
            // Every balance must stay readable in its own orientation (a debit-normal raw of
            // i128::MIN has no positive counterpart).
            let balance = Self::orient(account, raw)?;
            if balance.is_negative() && !account.allows_negative_balance() {
                return Err(LedgerError::InsufficientFunds {
                    account: account_id,
                    balance_minor: current.minor_units(),
                    delta_minor: delta.minor_units(),
                });
            }
            next.push((account_id, raw));
        }

        self.raw_balances.extend(next);
        self.posted.insert(txn.id);
        Ok(())
    }

    fn balance(&self, account: AccountId) -> Result<Money> {
        let acct = self
            .accounts
            .get(&account)
            .ok_or(LedgerError::UnknownAccount(account))?;
        let raw = self.raw_balances[&account];
        Self::orient(acct, raw)
    }
}
