//! Double-entry accounting core.
//!
//! This crate models the heart of the payment system: [`Account`]s, and
//! [`Transaction`]s made of balanced [`Entry`] postings. It is **pure** — no
//! database, no async, no I/O — so its invariants can be tested exhaustively in
//! isolation and reused unchanged behind any storage backend.
//!
//! # The invariants this crate enforces
//!
//! 1. **Every transaction balances.** Debits equal credits, per currency
//!    ([`Transaction::validate`]). Money is never created or destroyed by a
//!    single transaction.
//! 2. **The whole ledger stays conserved.** Across all accounts, raw balances
//!    sum to zero in every currency ([`InMemoryLedger::is_conserved`]).
//! 3. **No unauthorised negative balances.** A user wallet can never go below
//!    zero (no spending money you don't have).
//! 4. **Posting is atomic and idempotent.** A transaction applies fully or not
//!    at all, and the same transaction id never applies twice.
//!
//! [`LedgerEngine`] is the seam that lets us swap the in-memory reference for a
//! Postgres or TigerBeetle backend later without touching business logic.

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

    /// A ledger with one settlement (funding) account and `n` user wallets,
    /// returning their ids.
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

    /// A deposit: debit the settlement (asset) account, credit the user wallet.
    fn deposit(settlement: AccountId, user: AccountId, minor: i128) -> Transaction {
        Transaction::with_entries(vec![
            Entry::debit(settlement, Money::from_minor(minor, tjs())),
            Entry::credit(user, Money::from_minor(minor, tjs())),
        ])
    }

    /// A transfer between two user wallets.
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

        // The settlement account is a debit-normal asset; debiting it on deposit
        // increases it. Oriented, it reads +10_000: the system now holds 10_000
        // more in assets, exactly matching the total credited to user wallets.
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
        // Failed post left balances untouched.
        assert_eq!(ledger.balance(alice).unwrap().minor_units(), 1_000);
        assert_eq!(ledger.balance(bob).unwrap().minor_units(), 0);
        assert!(ledger.is_conserved());
    }

    #[test]
    fn unbalanced_transaction_is_rejected() {
        let (mut ledger, settlement, users) = ledger_with_users(1);
        let alice = users[0];
        // Debit 100 but credit 99 — does not net to zero.
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
        // Not double-applied.
        assert_eq!(ledger.balance(users[0]).unwrap().minor_units(), 500);
    }

    #[test]
    fn three_entry_transaction_with_a_fee_balances() {
        // Alice pays Bob 1_000, of which 50 is a platform fee.
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

    proptest! {
        /// The headline simulation test: throw a random sequence of deposits and
        /// transfers (including ones that must be rejected for insufficient
        /// funds) at the ledger, and assert that **money is always conserved** and
        /// **no user wallet ever goes negative** — no matter the sequence.
        ///
        /// This is the property that, for a single-entry design, you simply
        /// cannot state. Here it holds by construction and we prove it over
        /// thousands of random histories.
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
                    // a transfer; may be rejected for insufficient funds, which
                    // is fine — we only require invariants hold afterwards.
                    transfer(users[a], users[b], amount)
                };
                let _ = ledger.post(&txn);

                // Invariants must hold after *every* operation, accepted or not.
                prop_assert!(ledger.is_conserved());
                for &u in &users {
                    prop_assert!(ledger.balance(u).unwrap().minor_units() >= 0);
                }
            }
        }
    }
}
