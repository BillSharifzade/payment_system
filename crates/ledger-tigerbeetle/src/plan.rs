use std::collections::{BTreeMap, HashMap};

use ledger::{AccountId, AccountType, LedgerError, Transaction};
use money::{Currency, MoneyError};
use storage::StorageError;

use crate::error::{Result, TbError};
use crate::ids::{ledger_of, MAX_LEGS};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AccountInfo {
    pub account_type: AccountType,
    pub currency: Currency,
}

/// One TigerBeetle transfer of a transaction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Leg {
    pub debit: u128,
    pub credit: u128,
    pub amount: u128,
    pub ledger: u32,
}

/// Per ledger: (debtors, creditors) with the amount each still owes or is owed.
type Sides = (Vec<(u128, i128)>, Vec<(u128, i128)>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Plan {
    pub legs: Vec<Leg>,
    /// Net raw change per account (credit positive), for error reports.
    pub deltas: HashMap<u128, i128>,
}

/// Turns a validated transaction into TigerBeetle transfers. Entries are netted per account
/// first, so within the chain an account is only debited or only credited: the order of the
/// legs cannot matter and each balance limit sees exactly the transaction's net effect, which
/// is the rule `InMemoryLedger` applies. Per currency, debtors pay creditors greedily in id
/// order: deterministic, and at most `debtors + creditors - 1` legs. Checks run in entry order,
/// as `InMemoryLedger`'s do, so the same error wins.
pub fn plan(txn: &Transaction, info: impl Fn(AccountId) -> Option<AccountInfo>) -> Result<Plan> {
    let mut net: BTreeMap<(u32, u128), i128> = BTreeMap::new();
    for e in &txn.entries {
        let account = info(e.account_id).ok_or(LedgerError::UnknownAccount(e.account_id))?;
        if account.currency != e.amount.currency() {
            return Err(LedgerError::AccountCurrencyMismatch {
                account: e.account_id,
                account_currency: account.currency,
                entry_currency: e.amount.currency(),
            }
            .into());
        }
        // The Postgres mirror stores BIGINT amounts.
        let minor = e.amount.minor_units();
        if i64::try_from(minor).is_err() {
            return Err(StorageError::AmountTooLarge(minor).into());
        }
        let slot = net
            .entry((
                ledger_of(account.currency),
                e.account_id.as_uuid().as_u128(),
            ))
            .or_default();
        *slot = slot
            .checked_add(e.signed_amount()?.minor_units())
            .ok_or(MoneyError::Overflow { operation: "post" })?;
    }

    let mut legs = Vec::new();
    let mut by_ledger: BTreeMap<u32, Sides> = BTreeMap::new();
    for (&(ledger, account), &delta) in &net {
        let (debtors, creditors) = by_ledger.entry(ledger).or_default();
        match delta {
            d if d < 0 => debtors.push((account, -d)),
            d if d > 0 => creditors.push((account, d)),
            _ => {}
        }
    }
    for (ledger, (mut debtors, mut creditors)) in by_ledger {
        let (mut d, mut c) = (0, 0);
        while d < debtors.len() && c < creditors.len() {
            let amount = debtors[d].1.min(creditors[c].1);
            legs.push(Leg {
                debit: debtors[d].0,
                credit: creditors[c].0,
                amount: amount as u128,
                ledger,
            });
            debtors[d].1 -= amount;
            creditors[c].1 -= amount;
            d += usize::from(debtors[d].1 == 0);
            c += usize::from(creditors[c].1 == 0);
        }
        debug_assert!(
            d == debtors.len() && c == creditors.len(),
            "validated: nets to zero"
        );
    }
    if legs.len() > MAX_LEGS {
        return Err(TbError::Storage(StorageError::Rejected {
            rule: "too_many_legs".to_string(),
            message: format!("a transaction nets to at most {MAX_LEGS} transfers"),
        }));
    }
    Ok(Plan {
        legs,
        deltas: net.into_iter().map(|((_, a), d)| (a, d)).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ledger::Entry;
    use money::Money;

    fn usd() -> Currency {
        Currency::new("USD", 2).unwrap()
    }

    fn registry(ids: &[(AccountId, AccountType, Currency)]) -> HashMap<AccountId, AccountInfo> {
        ids.iter()
            .map(|(id, t, c)| {
                (
                    *id,
                    AccountInfo {
                        account_type: *t,
                        currency: *c,
                    },
                )
            })
            .collect()
    }

    #[test]
    fn plain_fee_and_fx_transactions_become_one_leg_per_movement() {
        let tjs = Currency::tjs();
        let [a, b, fee, fx_t, fx_u, w_u] = [(); 6].map(|_| AccountId::new());
        let reg = registry(&[
            (a, AccountType::UserWallet, tjs),
            (b, AccountType::UserWallet, tjs),
            (fee, AccountType::SystemFeeRevenue, tjs),
            (fx_t, AccountType::SystemFxGainLoss, tjs),
            (fx_u, AccountType::SystemFxGainLoss, usd()),
            (w_u, AccountType::UserWallet, usd()),
        ]);
        let m = |v, c| Money::from_minor(v, c);
        let info = |id| reg.get(&id).copied();

        let fee_txn = Transaction::with_entries(vec![
            Entry::debit(a, m(1_000, tjs)),
            Entry::credit(b, m(990, tjs)),
            Entry::credit(fee, m(10, tjs)),
        ]);
        let p = plan(&fee_txn, info).unwrap();
        assert_eq!(p.legs.len(), 2);
        assert!(p.legs.iter().all(|l| l.debit == a.0.as_u128()));
        assert_eq!(p.legs.iter().map(|l| l.amount).sum::<u128>(), 1_000);
        assert_eq!(p.deltas[&a.0.as_u128()], -1_000);

        let fx = Transaction::with_entries(vec![
            Entry::debit(a, m(1_050, tjs)),
            Entry::credit(fx_t, m(1_050, tjs)),
            Entry::debit(fx_u, m(100, usd())),
            Entry::credit(w_u, m(100, usd())),
        ]);
        let p = plan(&fx, info).unwrap();
        assert_eq!(p.legs.len(), 2);
        assert_ne!(p.legs[0].ledger, p.legs[1].ledger);

        let self_transfer = Transaction::with_entries(vec![
            Entry::debit(a, m(5, tjs)),
            Entry::credit(a, m(5, tjs)),
        ]);
        assert!(plan(&self_transfer, info).unwrap().legs.is_empty());
    }

    #[test]
    fn checks_follow_entry_order() {
        let tjs = Currency::tjs();
        let [a, b] = [(); 2].map(|_| AccountId::new());
        let ghost = AccountId::new();
        let reg = registry(&[
            (a, AccountType::UserWallet, tjs),
            (b, AccountType::UserWallet, usd()),
        ]);
        let info = |id| reg.get(&id).copied();
        let txn = Transaction::with_entries(vec![
            Entry::debit(a, Money::from_minor(5, tjs)),
            Entry::credit(b, Money::from_minor(5, tjs)),
            Entry::credit(ghost, Money::from_minor(5, usd())),
        ]);
        assert!(matches!(
            plan(&txn, info).unwrap_err().as_ledger(),
            Some(LedgerError::AccountCurrencyMismatch { account, .. }) if *account == b
        ));
        let txn = Transaction::with_entries(vec![
            Entry::debit(ghost, Money::from_minor(5, tjs)),
            Entry::credit(a, Money::from_minor(5, tjs)),
        ]);
        assert_eq!(
            plan(&txn, info).unwrap_err().as_ledger(),
            Some(&LedgerError::UnknownAccount(ghost))
        );
    }
}
