use std::collections::{HashMap, HashSet};

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
        let mut totals: HashMap<String, i128> = HashMap::new();
        for raw in self.raw_balances.values() {
            *totals.entry(raw.currency().code().to_string()).or_insert(0) += raw.minor_units();
        }
        totals.values().all(|&t| t == 0)
    }

    pub fn net_minor_units(&self, currency: Currency) -> i128 {
        self.raw_balances
            .values()
            .filter(|raw| raw.currency() == currency)
            .map(|raw| raw.minor_units())
            .sum()
    }
}

impl LedgerEngine for InMemoryLedger {
    fn open_account(&mut self, account: Account) -> Result<()> {
        let currency = account.currency;
        let id = account.id;
        self.accounts.insert(id, account);
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

        let mut proposed: HashMap<AccountId, Money> = HashMap::new();
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

            let current = proposed
                .get(&entry.account_id)
                .copied()
                .unwrap_or_else(|| self.raw_balances[&entry.account_id]);
            let next = current.checked_add(&entry.signed_amount()?)?;
            proposed.insert(entry.account_id, next);
        }

        for (account_id, raw) in &proposed {
            let account = &self.accounts[account_id];
            if !account.allows_negative_balance() {
                let oriented = Self::orient(account, *raw)?;
                if oriented.is_negative() {
                    return Err(LedgerError::InsufficientFunds {
                        account: *account_id,
                        balance_minor: self.raw_balances[account_id].minor_units(),
                        delta_minor: raw.minor_units()
                            - self.raw_balances[account_id].minor_units(),
                    });
                }
            }
        }

        for (account_id, raw) in proposed {
            self.raw_balances.insert(account_id, raw);
        }
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
