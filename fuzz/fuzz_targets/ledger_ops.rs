//! Random operation sequences against `InMemoryLedger`, checked after every operation against
//! a reference model (BTreeMaps and exact wide sums):
//! - the engine accepts exactly what the model accepts, and a duplicate id is always refused;
//! - money is conserved per currency, and `is_conserved` says so;
//! - no user wallet is ever negative, every open account's balance is readable;
//! - a rejected operation leaves every balance unchanged;
//! - the same input gives the same result (two engines, each with its own hash seeds).
#![no_main]

use std::collections::{BTreeMap, BTreeSet};

use arbitrary::Arbitrary;
use ledger::{
    Account, AccountId, AccountType, Direction, Entry, InMemoryLedger, LedgerEngine, LedgerError,
    NormalSide, Transaction, TransactionId,
};
use libfuzzer_sys::fuzz_target;
use money::{Currency, Money};
use payment_fuzz::{balancing_legs, check_verdict, validate_verdict, Amount, Kind, Verdict, Wide};
use uuid::Uuid;

const ACCOUNTS: u8 = 10;
// Same code twice with different exponents: never one currency.
const CURRENCIES: [(&str, u8); 3] = [("TJS", 2), ("USD", 2), ("TJS", 0)];
const TYPES: [AccountType; 5] = [
    AccountType::UserWallet,
    AccountType::SystemSettlement,
    AccountType::SystemFeeRevenue,
    AccountType::SystemFxGainLoss,
    AccountType::SystemSuspense,
];

#[derive(Arbitrary, Debug)]
enum Op {
    Open {
        account: u8,
        kind: u8,
        currency: u8,
    },
    Post {
        txn: u8,
        legs: Vec<Leg>,
        balance: bool,
    },
    Repost {
        nth: u8,
    },
}

#[derive(Arbitrary, Debug)]
struct Leg {
    account: u8,
    credit: bool,
    amount: Amount,
    currency: Option<u8>,
}

fn account_id(i: u8) -> AccountId {
    AccountId(Uuid::from_u128((i % ACCOUNTS) as u128 + 1))
}

fn currency(i: u8) -> Currency {
    let (code, exponent) = CURRENCIES[i as usize % CURRENCIES.len()];
    Currency::new(code, exponent).unwrap()
}

fn oriented(a: &Account, raw: i128) -> Option<i128> {
    match a.normal_side() {
        NormalSide::Credit => Some(raw),
        NormalSide::Debit => raw.checked_neg(),
    }
}

#[derive(Default)]
struct Model {
    accounts: BTreeMap<AccountId, (Account, i128)>,
    posted: BTreeSet<TransactionId>,
}

impl Model {
    // Re-opening an id keeps the original account, as Postgres's ON CONFLICT DO NOTHING does.
    fn open(&mut self, a: Account) {
        self.accounts.entry(a.id).or_insert((a, 0));
    }

    fn post(&self, txn: &Transaction) -> Result<BTreeMap<AccountId, i128>, Verdict> {
        if self.posted.contains(&txn.id) {
            return Err(Verdict::Exactly(LedgerError::DuplicateTransaction(txn.id)));
        }
        match validate_verdict(&txn.entries) {
            Verdict::Valid => {}
            v => return Err(v),
        }
        let mut reasons = Vec::new();
        let mut deltas: BTreeMap<AccountId, Wide> = BTreeMap::new();
        for e in &txn.entries {
            match self.accounts.get(&e.account_id) {
                None => reasons.push(Kind::UnknownAccount),
                Some((a, _)) if a.currency != e.amount.currency() => {
                    reasons.push(Kind::AccountCurrencyMismatch)
                }
                Some(_) => {
                    let d = deltas.entry(e.account_id).or_default();
                    match e.direction {
                        Direction::Credit => d.add(e.amount.minor_units()),
                        Direction::Debit => d.sub(e.amount.minor_units()),
                    }
                }
            }
        }
        if !reasons.is_empty() {
            return Err(Verdict::AnyOf(reasons));
        }
        let mut next = BTreeMap::new();
        for (id, delta) in deltas {
            let (a, raw) = &self.accounts[&id];
            let mut w = Wide::of(*raw);
            w.add(delta.to_i128().expect("bounded by the validated totals"));
            match w.to_i128().and_then(|r| Some((r, oriented(a, r)?))) {
                None => reasons.push(Kind::Overflow),
                Some((_, balance)) if balance < 0 && !a.allows_negative_balance() => {
                    reasons.push(Kind::InsufficientFunds)
                }
                Some((r, _)) => {
                    next.insert(id, r);
                }
            }
        }
        if !reasons.is_empty() {
            return Err(Verdict::AnyOf(reasons));
        }
        Ok(next)
    }

    fn system_account_for(&self, c: Currency) -> Option<AccountId> {
        self.accounts
            .values()
            .find(|(a, _)| a.currency == c && a.allows_negative_balance())
            .map(|(a, _)| a.id)
    }
}

fn build(model: &Model, txn: u8, legs: &[Leg], balance: bool) -> Transaction {
    let mut entries: Vec<Entry> = legs
        .iter()
        .map(|leg| {
            let id = account_id(leg.account);
            let c = match leg.currency {
                Some(i) => currency(i),
                None => model
                    .accounts
                    .get(&id)
                    .map_or(currency(0), |(a, _)| a.currency),
            };
            let direction = if leg.credit {
                Direction::Credit
            } else {
                Direction::Debit
            };
            Entry::new(id, direction, Money::from_minor(leg.amount.minor(), c))
        })
        .collect();
    if balance {
        let fallback = entries.first().map_or(account_id(0), |e| e.account_id);
        let legs = balancing_legs(&entries, |c| {
            model.system_account_for(c).unwrap_or(fallback)
        });
        entries.extend(legs);
    }
    Transaction::new(TransactionId(Uuid::from_u128(txn as u128 + 1)), entries)
}

fn check_post(
    got: &Result<(), LedgerError>,
    want: &Result<BTreeMap<AccountId, i128>, Verdict>,
    model: &Model,
    txn: &Transaction,
) {
    match want {
        Ok(_) => assert_eq!(got, &Ok(()), "the model accepts\n{txn:?}"),
        Err(verdict) => check_verdict(got, verdict, txn),
    }
    if let Err(LedgerError::InsufficientFunds {
        account,
        balance_minor,
        delta_minor,
    }) = got
    {
        let (a, raw) = &model.accounts[account];
        assert!(!a.allows_negative_balance(), "{a:?} may go negative");
        assert_eq!(balance_minor, raw, "reported balance");
        let mut delta = Wide::default();
        for e in txn.entries.iter().filter(|e| e.account_id == *account) {
            match e.direction {
                Direction::Credit => delta.add(e.amount.minor_units()),
                Direction::Debit => delta.sub(e.amount.minor_units()),
            }
        }
        assert_eq!(Some(*delta_minor), delta.to_i128(), "reported change");
    }
}

fn check_state(engine: &InMemoryLedger, model: &Model) {
    let mut net: BTreeMap<Currency, Wide> = BTreeMap::new();
    for i in 0..ACCOUNTS {
        let id = account_id(i);
        match model.accounts.get(&id) {
            Some((a, raw)) => {
                let want = oriented(a, *raw).expect("the model keeps balances representable");
                assert_eq!(engine.balance(id), Ok(Money::from_minor(want, a.currency)));
                assert!(
                    want >= 0 || a.allows_negative_balance(),
                    "{a:?} is negative"
                );
                net.entry(a.currency).or_default().add(*raw);
            }
            None => assert_eq!(engine.balance(id), Err(LedgerError::UnknownAccount(id))),
        }
    }
    assert!(
        net.values().all(|w| w.is_zero()),
        "model lost money: {net:?}"
    );
    assert!(engine.is_conserved(), "engine reports money not conserved");
}

fuzz_target!(|ops: Vec<Op>| {
    let mut engines = [InMemoryLedger::new(), InMemoryLedger::new()];
    let mut model = Model::default();
    let mut submitted: Vec<Transaction> = Vec::new();
    for op in &ops {
        let txn = match op {
            Op::Open {
                account,
                kind,
                currency: c,
            } => {
                let a = Account::new(
                    account_id(*account),
                    TYPES[*kind as usize % TYPES.len()],
                    currency(*c),
                );
                for e in &mut engines {
                    assert_eq!(e.open_account(a.clone()), Ok(()));
                }
                model.open(a);
                None
            }
            Op::Post { txn, legs, balance } => Some(build(&model, *txn, legs, *balance)),
            Op::Repost { nth } => {
                (!submitted.is_empty()).then(|| submitted[*nth as usize % submitted.len()].clone())
            }
        };
        if let Some(txn) = txn {
            let [a, b] = &mut engines;
            let got = a.post(&txn);
            assert_eq!(b.post(&txn), got, "same input, same result");
            let want = model.post(&txn);
            check_post(&got, &want, &model, &txn);
            if let Ok(next) = want {
                for (id, raw) in next {
                    model.accounts.get_mut(&id).unwrap().1 = raw;
                }
                model.posted.insert(txn.id);
            }
            submitted.push(txn);
        }
        for e in &engines {
            check_state(e, &model);
        }
    }
});
