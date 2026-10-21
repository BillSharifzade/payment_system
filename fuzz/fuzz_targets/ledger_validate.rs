//! `ledger::Transaction::validate` on arbitrary entries: agrees with the order-independent
//! specification in `payment_fuzz::validate_verdict`, gives the same answer for every
//! permutation of the entries, and the same answer twice.
#![no_main]

use arbitrary::Arbitrary;
use ledger::{AccountId, Direction, Entry, Transaction, TransactionId};
use libfuzzer_sys::fuzz_target;
use money::{Currency, Money};
use payment_fuzz::{balancing_legs, check_verdict, validate_verdict, Amount};
use uuid::Uuid;

// Same code twice with different exponents: a clash the ledger must reject.
const CURRENCIES: [(&str, u8); 4] = [("TJS", 2), ("USD", 2), ("TJS", 0), ("JPY", 0)];

#[derive(Arbitrary, Debug)]
struct RawEntry {
    account: u8,
    credit: bool,
    amount: Amount,
    currency: u8,
}

#[derive(Arbitrary, Debug)]
struct Input {
    entries: Vec<RawEntry>,
    balance: bool,
    rotate: u8,
    swap: (u8, u8),
}

fn currency(i: u8) -> Currency {
    let (code, exponent) = CURRENCIES[i as usize % CURRENCIES.len()];
    Currency::new(code, exponent).unwrap()
}

fuzz_target!(|input: Input| {
    let mut entries: Vec<Entry> = input
        .entries
        .iter()
        .map(|e| {
            Entry::new(
                AccountId(Uuid::from_u128(e.account as u128)),
                if e.credit {
                    Direction::Credit
                } else {
                    Direction::Debit
                },
                Money::from_minor(e.amount.minor(), currency(e.currency)),
            )
        })
        .collect();
    if input.balance {
        let legs = balancing_legs(&entries, |_| AccountId(Uuid::from_u128(u128::MAX)));
        entries.extend(legs);
    }

    let id = TransactionId(Uuid::from_u128(1));
    let verdict = validate_verdict(&entries);
    let original = Transaction::new(id, entries.clone());
    let got = original.validate();
    check_verdict(&got, &verdict, &original);
    assert_eq!(original.validate(), got, "same input, same result");

    let n = entries.len().max(1);
    let mut permutations = vec![entries.iter().rev().cloned().collect::<Vec<_>>()];
    let mut rotated = entries.clone();
    rotated.rotate_left(input.rotate as usize % n);
    permutations.push(rotated);
    let mut swapped = entries.clone();
    if !swapped.is_empty() {
        swapped.swap(input.swap.0 as usize % n, input.swap.1 as usize % n);
    }
    permutations.push(swapped);
    for p in permutations {
        let txn = Transaction::new(id, p);
        let r = txn.validate();
        assert_eq!(
            r.is_ok(),
            got.is_ok(),
            "validity depends on entry order: {got:?} vs {r:?}\n{txn:?}"
        );
        check_verdict(&r, &verdict, &txn);
    }
});
