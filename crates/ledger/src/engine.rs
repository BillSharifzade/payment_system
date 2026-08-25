use std::collections::{HashMap, HashSet};

use crate::account::{Account, NormalSide};
use crate::error::{LedgerError, Result};
use crate::ids::{AccountId, TransactionId};
use crate::transaction::Transaction;
use money::{Currency, Money};

/// The abstraction every ledger backend implements.
///
/// This is the seam described in the design doc: the in-memory engine here, a
/// `PostgresLedger` (Phase 1), and a `TigerBeetleLedger` (Phase 2) all implement
/// the same trait, so business logic never depends on which backend is wired in.
/// Swapping engines is configuration, not a rewrite.
///
/// It is intentionally synchronous for now — the pure core has no I/O. When the
/// Postgres backend lands, this becomes async; the in-memory engine stays as the
/// fast, deterministic reference used by simulation tests.
pub trait LedgerEngine {
    /// Register an account so entries may reference it.
    fn open_account(&mut self, account: Account) -> Result<()>;

    /// Atomically validate and post a transaction. Either every entry applies or
    /// none do. Posting is idempotent on [`Transaction::id`].
    fn post(&mut self, txn: &Transaction) -> Result<()>;

    /// The current *oriented* balance of an account (positive means "has funds"
    /// in the account's normal direction).
    fn balance(&self, account: AccountId) -> Result<Money>;
}

/// An in-memory, single-threaded double-entry ledger.
///
/// This is the **reference implementation**: simple enough to be obviously
/// correct, and the oracle that property/simulation tests check the real
/// backends against. Balances are stored *raw* (credit positive, debit
/// negative); the system-wide invariant is that raw balances sum to exactly zero
/// in every currency — see [`InMemoryLedger::is_conserved`].
#[derive(Debug, Default)]
pub struct InMemoryLedger {
    accounts: HashMap<AccountId, Account>,
    /// Raw signed balance per account (credit +, debit -).
    raw_balances: HashMap<AccountId, Money>,
    posted: HashSet<TransactionId>,
}

impl InMemoryLedger {
    pub fn new() -> Self {
        Self::default()
    }

    /// Re-orient a raw balance into the account's normal direction, so callers
    /// see an intuitive number (a user wallet credited 100 reads as +100; an
    /// asset account debited 100 also reads as +100).
    fn orient(account: &Account, raw: Money) -> Result<Money> {
        match account.normal_side() {
            NormalSide::Credit => Ok(raw),
            NormalSide::Debit => Ok(raw.checked_neg()?),
        }
    }

    /// True iff, for every currency, the raw balances of all accounts sum to
    /// exactly zero. This is the master invariant of double-entry bookkeeping:
    /// money is never created or destroyed. The reconciliation job (and the
    /// simulation tests) assert this continuously.
    pub fn is_conserved(&self) -> bool {
        let mut totals: HashMap<String, i128> = HashMap::new();
        for raw in self.raw_balances.values() {
            *totals.entry(raw.currency().code().to_string()).or_insert(0) += raw.minor_units();
        }
        totals.values().all(|&t| t == 0)
    }

    /// Net signed total still held across all accounts for a currency. Always 0
    /// when conserved; exposed for assertions and reconciliation reporting.
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
        // Idempotency: the same transaction id is never applied twice.
        if self.posted.contains(&txn.id) {
            return Err(LedgerError::DuplicateTransaction(txn.id));
        }

        // Structural validity (balanced, positive amounts, >= 2 entries).
        txn.validate()?;

        // Compute the would-be new raw balances without mutating anything yet,
        // so the whole transaction is all-or-nothing.
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

        // Enforce the no-negative-balance rule on accounts that require it.
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

        // Commit: nothing above this line mutated state, so we cannot leave the
        // ledger half-updated.
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
