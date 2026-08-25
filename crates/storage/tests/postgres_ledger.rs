//! Integration tests for [`PostgresLedger`] against a real PostgreSQL.
//!
//! These need a database. Start one with `docker compose up -d` and run:
//!
//! ```bash
//! DATABASE_URL=postgres://payment:payment_dev_pw@localhost:5432/payment \
//!   cargo test -p storage -- --include-ignored
//! ```
//!
//! They are `#[ignore]` so a plain `cargo test` (e.g. in a DB-less CI lane)
//! stays green; the dedicated integration lane runs them with `--include-ignored`.

use ledger::{Account, AccountId, AccountType, Entry, Transaction};
use money::{Currency, Money};
use sqlx::postgres::PgPoolOptions;
use storage::{LedgerStore, PostgresLedger, StorageError};

fn tjs() -> Currency {
    Currency::tjs()
}

async fn connect() -> PostgresLedger {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    let pool = PgPoolOptions::new()
        .max_connections(16)
        .connect(&url)
        .await
        .expect("connect to postgres");
    let ledger = PostgresLedger::new(pool);
    ledger.migrate().await.expect("run migrations");
    ledger
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

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn deposit_transfer_overdraft_and_idempotency() {
    let ledger = connect().await;

    let settlement = AccountId::new();
    let alice = AccountId::new();
    let bob = AccountId::new();
    ledger
        .open_account(&Account::new(
            settlement,
            AccountType::SystemSettlement,
            tjs(),
        ))
        .await
        .unwrap();
    ledger
        .open_account(&Account::new(alice, AccountType::UserWallet, tjs()))
        .await
        .unwrap();
    ledger
        .open_account(&Account::new(bob, AccountType::UserWallet, tjs()))
        .await
        .unwrap();

    // Deposit 100.00 TJS to Alice.
    ledger
        .post(&deposit(settlement, alice, 10_000))
        .await
        .unwrap();
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 10_000);

    // Transfer 35.00 to Bob.
    ledger.post(&transfer(alice, bob, 3_500)).await.unwrap();
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 6_500);
    assert_eq!(ledger.balance(bob).await.unwrap().minor_units(), 3_500);

    // Overdraft is rejected and leaves balances untouched.
    let err = ledger
        .post(&transfer(alice, bob, 1_000_000))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::Ledger(ledger::LedgerError::InsufficientFunds { .. })
    ));
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 6_500);

    // Re-posting the same transaction id is rejected (not double-applied).
    let dup = transfer(alice, bob, 100);
    ledger.post(&dup).await.unwrap();
    let err = ledger.post(&dup).await.unwrap_err();
    assert!(matches!(
        err,
        StorageError::Ledger(ledger::LedgerError::DuplicateTransaction(_))
    ));
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 6_400);
}

/// The test that matters: fund an account with exactly enough for N transfers,
/// then fire 2N concurrent transfers at it. Row-locking must ensure exactly N
/// succeed, the account never goes negative, and no money is created or lost.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn concurrent_transfers_never_double_spend() {
    let ledger = connect().await;

    let settlement = AccountId::new();
    let alice = AccountId::new();
    let bob = AccountId::new();
    for (id, ty) in [
        (settlement, AccountType::SystemSettlement),
        (alice, AccountType::UserWallet),
        (bob, AccountType::UserWallet),
    ] {
        ledger
            .open_account(&Account::new(id, ty, tjs()))
            .await
            .unwrap();
    }

    const N: i128 = 100;
    ledger.post(&deposit(settlement, alice, N)).await.unwrap();

    // Fire 2N concurrent 1-unit transfers. Only N can possibly succeed.
    let mut handles = Vec::new();
    for _ in 0..(2 * N) {
        let l = ledger.clone();
        handles.push(tokio::spawn(async move {
            l.post(&transfer(alice, bob, 1)).await.is_ok()
        }));
    }
    let mut successes = 0i128;
    for h in handles {
        if h.await.unwrap() {
            successes += 1;
        }
    }

    assert_eq!(successes, N, "exactly N transfers should succeed");
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 0);
    assert_eq!(ledger.balance(bob).await.unwrap().minor_units(), N);
}
