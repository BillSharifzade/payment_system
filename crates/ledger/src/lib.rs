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
                    prop_assert_eq!(book.ledger.net_minor_units(*currency), 0);
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
