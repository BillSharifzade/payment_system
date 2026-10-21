mod account;
mod engine;
mod error;
mod ids;
mod transaction;

pub use account::{Account, AccountType, NormalSide};
pub use engine::{InMemoryLedger, LedgerEngine};
pub use error::{LedgerError, Result};
pub use ids::{AccountId, EntryId, TransactionId};
pub use transaction::{Direction, Entry, Transaction};

#[cfg(test)]
mod tests {
    use super::*;
    use money::{Currency, Money};
    use proptest::prelude::*;

    fn tjs() -> Currency {
        Currency::tjs()
    }

    fn ledger_with_users(n: usize) -> (InMemoryLedger, AccountId, Vec<AccountId>) {
        let mut ledger = InMemoryLedger::new();
        let settlement = AccountId::new();
        ledger
            .open_account(Account::new(
                settlement,
                AccountType::SystemSettlement,
                tjs(),
            ))
            .unwrap();
        let users: Vec<AccountId> = (0..n)
            .map(|_| {
                let id = AccountId::new();
                ledger
                    .open_account(Account::new(id, AccountType::UserWallet, tjs()))
                    .unwrap();
                id
            })
            .collect();
        (ledger, settlement, users)
    }

    fn deposit(settlement: AccountId, user: AccountId, minor: i128) -> Transaction {
        Transaction::with_entries(vec![
            Entry::debit(settlement, Money::from_minor(minor, tjs())),
            Entry::credit(user, Money::from_minor(minor, tjs())),
        ])
    }

    fn transfer(from: AccountId, to: AccountId, minor: i128) -> Transaction {
        Transaction::with_entries(vec![
            Entry::debit(from, Money::from_minor(minor, tjs())),
            Entry::credit(to, Money::from_minor(minor, tjs())),
        ])
    }

    #[test]
    fn deposit_then_transfer_moves_money_correctly() {
        let (mut ledger, settlement, users) = ledger_with_users(2);
        let (alice, bob) = (users[0], users[1]);

        ledger.post(&deposit(settlement, alice, 10_000)).unwrap();
        assert_eq!(ledger.balance(alice).unwrap().minor_units(), 10_000);

        ledger.post(&transfer(alice, bob, 3_500)).unwrap();
        assert_eq!(ledger.balance(alice).unwrap().minor_units(), 6_500);
        assert_eq!(ledger.balance(bob).unwrap().minor_units(), 3_500);

        assert_eq!(ledger.balance(settlement).unwrap().minor_units(), 10_000);
        assert!(ledger.is_conserved());
    }

    #[test]
    fn cannot_overdraw_a_user_wallet() {
        let (mut ledger, settlement, users) = ledger_with_users(2);
        let (alice, bob) = (users[0], users[1]);
        ledger.post(&deposit(settlement, alice, 1_000)).unwrap();

        let err = ledger.post(&transfer(alice, bob, 1_001)).unwrap_err();
        assert!(matches!(err, LedgerError::InsufficientFunds { .. }));
        assert_eq!(ledger.balance(alice).unwrap().minor_units(), 1_000);
        assert_eq!(ledger.balance(bob).unwrap().minor_units(), 0);
        assert!(ledger.is_conserved());
    }

    #[test]
    fn unbalanced_transaction_is_rejected() {
        let (mut ledger, settlement, users) = ledger_with_users(1);
        let alice = users[0];
        let bad = Transaction::with_entries(vec![
            Entry::debit(settlement, Money::from_minor(100, tjs())),
            Entry::credit(alice, Money::from_minor(99, tjs())),
        ]);
        assert!(matches!(
            ledger.post(&bad).unwrap_err(),
            LedgerError::Unbalanced { .. }
        ));
    }

    #[test]
    fn reposting_same_transaction_id_is_rejected() {
        let (mut ledger, settlement, users) = ledger_with_users(1);
        let txn = deposit(settlement, users[0], 500);
        ledger.post(&txn).unwrap();
        assert!(matches!(
            ledger.post(&txn).unwrap_err(),
            LedgerError::DuplicateTransaction(_)
        ));
        assert_eq!(ledger.balance(users[0]).unwrap().minor_units(), 500);
    }

    #[test]
    fn three_entry_transaction_with_a_fee_balances() {
        let mut ledger = InMemoryLedger::new();
        let settlement = AccountId::new();
        let fee = AccountId::new();
        let alice = AccountId::new();
        let bob = AccountId::new();
        for (id, ty) in [
            (settlement, AccountType::SystemSettlement),
            (fee, AccountType::SystemFeeRevenue),
            (alice, AccountType::UserWallet),
            (bob, AccountType::UserWallet),
        ] {
            ledger.open_account(Account::new(id, ty, tjs())).unwrap();
        }
        ledger.post(&deposit(settlement, alice, 5_000)).unwrap();

        let payment = Transaction::with_entries(vec![
            Entry::debit(alice, Money::from_minor(1_000, tjs())),
            Entry::credit(bob, Money::from_minor(950, tjs())),
            Entry::credit(fee, Money::from_minor(50, tjs())),
        ]);
        ledger.post(&payment).unwrap();

        assert_eq!(ledger.balance(alice).unwrap().minor_units(), 4_000);
        assert_eq!(ledger.balance(bob).unwrap().minor_units(), 950);
        assert_eq!(ledger.balance(fee).unwrap().minor_units(), 50);
        assert!(ledger.is_conserved());
    }

    fn permutations(entries: &[Entry]) -> Vec<Vec<Entry>> {
        if entries.len() <= 1 {
            return vec![entries.to_vec()];
        }
        let mut out = Vec::new();
        for i in 0..entries.len() {
            let mut rest = entries.to_vec();
            let first = rest.remove(i);
            for mut p in permutations(&rest) {
                p.insert(0, first.clone());
                out.push(p);
            }
        }
        out
    }

    // Found by fuzz/ledger_validate: a running net accepted [C, D, D, C] with credits of
    // 2·(MAX − 97) but refused other orders of the same entries.
    #[test]
    fn validity_does_not_depend_on_entry_order() {
        let (a, b) = (AccountId::new(), AccountId::new());
        let m = |minor| Money::from_minor(minor, tjs());
        let overflowing = [
            Entry::credit(a, m(i128::MAX - 97)),
            Entry::debit(b, m(i128::MAX)),
            Entry::debit(b, m(i128::MAX - 194)),
            Entry::credit(a, m(i128::MAX - 97)),
        ];
        let large = [
            Entry::debit(a, m(i128::MAX)),
            Entry::credit(b, m(i128::MAX - 1)),
            Entry::credit(b, m(1)),
        ];
        for (entries, want) in [
            (
                &overflowing[..],
                Err(LedgerError::Money(money::MoneyError::Overflow {
                    operation: "add",
                })),
            ),
            (&large[..], Ok(())),
        ] {
            for p in permutations(entries) {
                assert_eq!(Transaction::with_entries(p).validate(), want);
            }
        }
    }

    #[test]
    fn reopening_an_account_keeps_the_original() {
        let (mut ledger, settlement, users) = ledger_with_users(1);
        ledger.post(&deposit(settlement, users[0], 100)).unwrap();
        // Before: the settlement account (raw −100) became a wallet holding −100.
        ledger
            .open_account(Account::new(settlement, AccountType::UserWallet, tjs()))
            .unwrap();
        assert_eq!(ledger.balance(settlement).unwrap().minor_units(), 100);
        ledger.post(&deposit(settlement, users[0], 50)).unwrap();
        assert_eq!(ledger.balance(settlement).unwrap().minor_units(), 150);
        assert!(ledger.is_conserved());
    }

    #[test]
    fn a_debit_normal_balance_stays_representable() {
        let (mut ledger, settlement, users) = ledger_with_users(2);
        ledger
            .post(&deposit(settlement, users[0], i128::MAX))
            .unwrap();
        let err = ledger.post(&deposit(settlement, users[1], 1)).unwrap_err();
        assert!(matches!(err, LedgerError::Money(_)), "{err:?}");
        assert_eq!(ledger.balance(settlement).unwrap().minor_units(), i128::MAX);
        assert_eq!(ledger.balance(users[1]).unwrap().minor_units(), 0);
    }

    #[test]
    fn insufficient_funds_reports_the_net_change_and_the_lowest_account() {
        for _ in 0..20 {
            let (mut ledger, settlement, mut users) = ledger_with_users(3);
            users.sort();
            ledger.post(&deposit(settlement, users[0], 5)).unwrap();
            ledger.post(&deposit(settlement, users[1], 5)).unwrap();
            let overdraw_both = Transaction::with_entries(vec![
                Entry::debit(users[1], Money::from_minor(i128::MAX - 10, tjs())),
                Entry::debit(users[0], Money::from_minor(1, tjs())),
                Entry::debit(users[0], Money::from_minor(9, tjs())),
                Entry::credit(users[2], Money::from_minor(i128::MAX, tjs())),
            ]);
            assert_eq!(
                ledger.post(&overdraw_both).unwrap_err(),
                LedgerError::InsufficientFunds {
                    account: users[0],
                    balance_minor: 5,
                    delta_minor: -10,
                }
            );
        }
    }

    #[test]
    fn conservation_is_exact_near_the_limits() {
        let mut ledger = InMemoryLedger::new();
        let mut open = |ty| {
            let id = AccountId::new();
            ledger.open_account(Account::new(id, ty, tjs())).unwrap();
            id
        };
        let pairs: Vec<(AccountId, AccountId)> = (0..16)
            .map(|_| {
                (
                    open(AccountType::SystemSettlement),
                    open(AccountType::UserWallet),
                )
            })
            .collect();
        for (s, w) in pairs {
            ledger.post(&deposit(s, w, i128::MAX)).unwrap();
        }
        // Raw balances: 16 of +MAX and 16 of −MAX. An i128 running sum overflows unless the
        // hash order alternates them exactly (p ≈ 3e-9).
        assert!(ledger.is_conserved());
        assert_eq!(ledger.net_minor_units(tjs()), Some(0));
        assert_eq!(
            ledger.net_minor_units(Currency::new("USD", 2).unwrap()),
            Some(0)
        );
    }

    struct MultiCurrency {
        ledger: InMemoryLedger,
        usd: Currency,
        settlement: [AccountId; 2],
        fx: [AccountId; 2],
        fee: AccountId,
        wallets: [Vec<AccountId>; 2],
    }

    fn multi_currency() -> MultiCurrency {
        let usd = Currency::new("USD", 2).unwrap();
        let mut ledger = InMemoryLedger::new();
        let mut open = |ty, currency| {
            let id = AccountId::new();
            ledger.open_account(Account::new(id, ty, currency)).unwrap();
            id
        };
        let settlement = [
            open(AccountType::SystemSettlement, tjs()),
            open(AccountType::SystemSettlement, usd),
        ];
        let fx = [
            open(AccountType::SystemFxGainLoss, tjs()),
            open(AccountType::SystemFxGainLoss, usd),
        ];
        let fee = open(AccountType::SystemFeeRevenue, tjs());
        let wallets = [
            (0..3)
                .map(|_| open(AccountType::UserWallet, tjs()))
                .collect(),
            (0..3).map(|_| open(AccountType::UserWallet, usd)).collect(),
        ];
        MultiCurrency {
            ledger,
            usd,
            settlement,
            fx,
            fee,
            wallets,
        }
    }

    proptest! {
        #[test]
        fn money_is_conserved_across_currencies_fees_and_fx(
            ops in prop::collection::vec(
                (0u8..5, 0usize..3, 0usize..3, 1i128..100_000i128, any::<bool>()),
                0..200,
            )
        ) {
            let mut book = multi_currency();
            let currencies = [tjs(), book.usd];
            let mut deposited = [0i128; 2];
            let mut fees = 0i128;
            let every: Vec<AccountId> = book
                .settlement
                .iter()
                .chain(&book.fx)
                .chain([&book.fee])
                .chain(book.wallets.iter().flatten())
                .copied()
                .collect();

            for (kind, a, b, amount, usd_side) in ops {
                let c = usd_side as usize;
                let cur = currencies[c];
                let money = |minor, currency| Money::from_minor(minor, currency);
                let txn = match kind {
                    0 => Transaction::with_entries(vec![
                        Entry::debit(book.settlement[c], money(amount, cur)),
                        Entry::credit(book.wallets[c][a], money(amount, cur)),
                    ]),
                    1 => Transaction::with_entries(vec![
                        Entry::debit(book.wallets[c][a], money(amount, cur)),
                        Entry::credit(book.wallets[c][b], money(amount, cur)),
                    ]),
                    2 => {
                        let fee = (amount / 100).max(1);
                        Transaction::with_entries(vec![
                            Entry::debit(book.wallets[0][a], money(amount + fee, tjs())),
                            Entry::credit(book.wallets[0][b], money(amount, tjs())),
                            Entry::credit(book.fee, money(fee, tjs())),
                        ])
                    }
                    3 => {
                        // Two legs, each balanced in its own currency.
                        let other = 1 - c;
                        let converted = (amount * 3 / 7).max(1);
                        Transaction::with_entries(vec![
                            Entry::debit(book.wallets[c][a], money(amount, cur)),
                            Entry::credit(book.fx[c], money(amount, cur)),
                            Entry::debit(book.fx[other], money(converted, currencies[other])),
                            Entry::credit(book.wallets[other][b], money(converted, currencies[other])),
                        ])
                    }
                    _ => Transaction::with_entries(vec![
                        Entry::debit(book.wallets[c][a], money(amount, cur)),
                        Entry::credit(book.wallets[1 - c][b], money(amount, currencies[1 - c])),
                    ]),
                };

                let before: Vec<i128> = every
                    .iter()
                    .map(|id| book.ledger.balance(*id).unwrap().minor_units())
                    .collect();
                match book.ledger.post(&txn) {
                    Ok(()) => {
                        prop_assert!(kind != 4, "a cross-currency transfer must not post");
                        if kind == 0 {
                            deposited[c] += amount;
                        }
                        if kind == 2 {
                            fees += (amount / 100).max(1);
                        }
                    }
                    Err(_) => {
                        let after: Vec<i128> = every
                            .iter()
                            .map(|id| book.ledger.balance(*id).unwrap().minor_units())
                            .collect();
                        prop_assert_eq!(before, after, "a rejected transaction changes nothing");
                    }
                }

                prop_assert!(book.ledger.is_conserved());
                for (i, currency) in currencies.iter().enumerate() {
                    prop_assert_eq!(book.ledger.net_minor_units(*currency), Some(0));
                    prop_assert_eq!(
                        book.ledger.balance(book.settlement[i]).unwrap().minor_units(),
                        deposited[i]
                    );
                    for w in &book.wallets[i] {
                        prop_assert!(book.ledger.balance(*w).unwrap().minor_units() >= 0);
                    }
                }
                prop_assert_eq!(book.ledger.balance(book.fee).unwrap().minor_units(), fees);
            }
        }

        #[test]
        fn money_is_always_conserved(
            ops in prop::collection::vec(
                (0usize..3, 0usize..3, 1i128..100_000i128, any::<bool>()),
                0..200,
            )
        ) {
            let (mut ledger, settlement, users) = ledger_with_users(3);

            for (a, b, amount, is_deposit) in ops {
                let txn = if is_deposit {
                    deposit(settlement, users[a], amount)
                } else {
                    transfer(users[a], users[b], amount)
                };
                let _ = ledger.post(&txn);

                prop_assert!(ledger.is_conserved());
                for &u in &users {
                    prop_assert!(ledger.balance(u).unwrap().minor_units() >= 0);
                }
            }
        }
    }
}
