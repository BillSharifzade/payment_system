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

    ledger
        .post(&deposit(settlement, alice, 10_000))
        .await
        .unwrap();
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 10_000);

    ledger.post(&transfer(alice, bob, 3_500)).await.unwrap();
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 6_500);
    assert_eq!(ledger.balance(bob).await.unwrap().minor_units(), 3_500);

    let err = ledger
        .post(&transfer(alice, bob, 1_000_000))
        .await
        .unwrap_err();
    assert!(matches!(
        err,
        StorageError::Ledger(ledger::LedgerError::InsufficientFunds { .. })
    ));
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 6_500);

    let dup = transfer(alice, bob, 100);
    ledger.post(&dup).await.unwrap();
    let err = ledger.post(&dup).await.unwrap_err();
    assert!(matches!(
        err,
        StorageError::Ledger(ledger::LedgerError::DuplicateTransaction(_))
    ));
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 6_400);
}

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

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn guard_rejection_rolls_back_and_idempotency_commits_with_post() {
    use storage::{HookError, IdempotencyRecord, PostOptions};

    let ledger = connect().await;
    let settlement = AccountId::new();
    let alice = AccountId::new();
    for (id, ty) in [
        (settlement, AccountType::SystemSettlement),
        (alice, AccountType::UserWallet),
    ] {
        ledger
            .open_account(&Account::new(id, ty, tjs()))
            .await
            .unwrap();
    }

    let txn = deposit(settlement, alice, 4_200);
    let rejected = ledger
        .post_with(
            &txn,
            PostOptions {
                idempotency: None,
                guard: Some(Box::new(|_conn| {
                    Box::pin(async {
                        Err(HookError::Rejected {
                            rule: "test".into(),
                            message: "computer says no".into(),
                        })
                    })
                })),
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(rejected, StorageError::Rejected { ref rule, .. } if rule == "test"));
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 0);
    let claimed: Option<i32> = sqlx::query_scalar("SELECT 1 FROM transactions WHERE id = $1")
        .bind(txn.id.as_uuid())
        .fetch_optional(ledger.pool())
        .await
        .unwrap();
    assert!(
        claimed.is_none(),
        "a rejected post must not keep its id claim"
    );

    let key = txn.id.as_uuid();
    ledger
        .post_with(
            &txn,
            PostOptions {
                idempotency: Some(IdempotencyRecord {
                    key,
                    fingerprint: "fp".into(),
                    response_status: 201,
                    response_body: serde_json::json!({"ok": true}),
                }),
                guard: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 4_200);
    assert_eq!(
        ledger.balance(settlement).await.unwrap().minor_units(),
        4_200
    );
    let stored: Option<String> =
        sqlx::query_scalar("SELECT fingerprint FROM idempotency_keys WHERE key = $1")
            .bind(key)
            .fetch_optional(ledger.pool())
            .await
            .unwrap();
    assert_eq!(stored.as_deref(), Some("fp"));

    let err = sqlx::query("UPDATE balances SET raw_minor = -1 WHERE account_id = $1")
        .bind(alice.as_uuid())
        .execute(ledger.pool())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("balances_min_raw_check"), "{err}");
}
