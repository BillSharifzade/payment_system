//! The database itself refuses to rewrite ledger history or store an
//! unbalanced transaction (migration 0025), whatever code path tries.

use ledger::{Account, AccountId, AccountType, Entry, Transaction};
use money::{Currency, Money};
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Postgres};
use storage::{LedgerStore, PostgresLedger};
use uuid::Uuid;

async fn connect() -> PostgresLedger {
    let url =
        std::env::var("DATABASE_URL").expect("DATABASE_URL must be set for integration tests");
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .connect(&url)
        .await
        .expect("connect to postgres");
    let ledger = PostgresLedger::new(pool);
    ledger.migrate().await.expect("run migrations");
    ledger
}

async fn accounts(ledger: &PostgresLedger) -> (AccountId, AccountId) {
    let settlement = AccountId::new();
    let wallet = AccountId::new();
    ledger
        .open_account(&Account::new(
            settlement,
            AccountType::SystemSettlement,
            Currency::tjs(),
        ))
        .await
        .unwrap();
    ledger
        .open_account(&Account::new(
            wallet,
            AccountType::UserWallet,
            Currency::tjs(),
        ))
        .await
        .unwrap();
    (settlement, wallet)
}

fn deposit(settlement: AccountId, wallet: AccountId, minor: i128) -> Transaction {
    Transaction::with_entries(vec![
        Entry::debit(settlement, Money::from_minor(minor, Currency::tjs())),
        Entry::credit(wallet, Money::from_minor(minor, Currency::tjs())),
    ])
}

async fn rejected(tx: &mut sqlx::Transaction<'_, Postgres>, sql: &str, id: Uuid) -> String {
    sqlx::query("SAVEPOINT probe")
        .execute(&mut **tx)
        .await
        .unwrap();
    let err = sqlx::query(sql)
        .bind(id)
        .execute(&mut **tx)
        .await
        .expect_err(sql)
        .to_string();
    sqlx::query("ROLLBACK TO SAVEPOINT probe")
        .execute(&mut **tx)
        .await
        .unwrap();
    err
}

async fn insert_entries(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    txn: Uuid,
    rows: &[(AccountId, &str, i64, &str)],
) -> Result<(), sqlx::Error> {
    let mut accounts = Vec::new();
    let mut directions = Vec::new();
    let mut amounts = Vec::new();
    let mut currencies = Vec::new();
    for (a, d, m, c) in rows {
        accounts.push(a.as_uuid());
        directions.push(d.to_string());
        amounts.push(*m);
        currencies.push(c.to_string());
    }
    sqlx::query(
        "INSERT INTO entries (id, transaction_id, account_id, direction, amount_minor, currency)
         SELECT gen_random_uuid(), $1, u.a, u.d, u.m, u.c
         FROM UNNEST($2::uuid[], $3::text[], $4::bigint[], $5::text[]) AS u(a, d, m, c)",
    )
    .bind(txn)
    .bind(accounts)
    .bind(directions)
    .bind(amounts)
    .bind(currencies)
    .execute(&mut **tx)
    .await
    .map(|_| ())
}

async fn begin(pool: &PgPool) -> sqlx::Transaction<'static, Postgres> {
    pool.begin().await.unwrap()
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn history_cannot_be_updated_or_deleted() {
    let ledger = connect().await;
    let (settlement, wallet) = accounts(&ledger).await;
    let txn = deposit(settlement, wallet, 500);
    ledger.post(&txn).await.unwrap();
    let id = txn.id.as_uuid();

    let mut tx = begin(ledger.pool()).await;
    for sql in [
        "UPDATE entries SET amount_minor = amount_minor + 1 WHERE transaction_id = $1",
        "DELETE FROM entries WHERE transaction_id = $1",
        "DELETE FROM transactions WHERE id = $1",
        "DELETE FROM checkpoints WHERE id = $1",
        "UPDATE admin_actions SET action = 'x' WHERE id = $1",
        "DELETE FROM screening_events WHERE id = $1",
        "UPDATE biometric_events SET outcome = 'paid' WHERE id = $1",
    ] {
        let err = rejected(&mut tx, sql, id).await;
        assert!(err.contains("append-only"), "{sql}: {err}");
    }
    for table in [
        "entries",
        "transactions",
        "checkpoints",
        "admin_actions",
        "screening_events",
        "biometric_events",
    ] {
        let sql = format!("TRUNCATE {table} CASCADE");
        sqlx::query("SAVEPOINT probe")
            .execute(&mut *tx)
            .await
            .unwrap();
        let err = sqlx::raw_sql(&sql).execute(&mut *tx).await.unwrap_err();
        assert!(err.to_string().contains("append-only"), "{sql}: {err}");
        sqlx::query("ROLLBACK TO SAVEPOINT probe")
            .execute(&mut *tx)
            .await
            .unwrap();
    }

    let err = rejected(
        &mut tx,
        "UPDATE transactions SET created_at = created_at - interval '1 day' WHERE id = $1",
        id,
    )
    .await;
    assert!(err.contains("immutable"), "{err}");
    let err = rejected(
        &mut tx,
        "UPDATE transactions SET sealed_seq = 9000000000, created_at = now() WHERE id = $1",
        id,
    )
    .await;
    assert!(err.contains("immutable"), "{err}");

    // The sealer's stamp is the one permitted change, and only once.
    sqlx::query("UPDATE transactions SET sealed_seq = 9000000000 WHERE id = $1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .expect("sealer may set sealed_seq");
    let err = rejected(
        &mut tx,
        "UPDATE transactions SET sealed_seq = 9000000001 WHERE id = $1",
        id,
    )
    .await;
    assert!(err.contains("immutable"), "{err}");
    sqlx::query("SAVEPOINT probe")
        .execute(&mut *tx)
        .await
        .unwrap();
    let err = insert_entries(
        &mut tx,
        id,
        &[
            (settlement, "debit", 1, "TJS"),
            (wallet, "credit", 1, "TJS"),
        ],
    )
    .await
    .unwrap_err();
    assert!(err.to_string().contains("is sealed"), "{err}");
    tx.rollback().await.unwrap();

    let err = sqlx::query("INSERT INTO transactions (id, sealed_seq) VALUES ($1, 9000000002)")
        .bind(Uuid::now_v7())
        .execute(ledger.pool())
        .await
        .unwrap_err();
    assert!(err.to_string().contains("already sealed"), "{err}");
    assert_eq!(ledger.balance(wallet).await.unwrap().minor_units(), 500);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn unbalanced_entries_are_rejected_by_the_database() {
    let ledger = connect().await;
    let (settlement, wallet) = accounts(&ledger).await;
    let usd = AccountId::new();
    ledger
        .open_account(&Account::new(
            usd,
            AccountType::SystemSettlement,
            Currency::new("USD", 2).unwrap(),
        ))
        .await
        .unwrap();

    let cases: [&[(AccountId, &str, i64, &str)]; 4] = [
        &[
            (settlement, "debit", 100, "TJS"),
            (wallet, "credit", 99, "TJS"),
        ],
        &[(wallet, "credit", 100, "TJS")],
        // Balanced in total but not per currency.
        &[(usd, "debit", 100, "USD"), (wallet, "credit", 100, "TJS")],
        &[
            (settlement, "debit", 100, "TJS"),
            (wallet, "credit", 100, "TJS"),
            (usd, "debit", 5, "USD"),
        ],
    ];
    for rows in cases {
        let mut tx = begin(ledger.pool()).await;
        let id = Uuid::now_v7();
        sqlx::query("INSERT INTO transactions (id) VALUES ($1)")
            .bind(id)
            .execute(&mut *tx)
            .await
            .unwrap();
        let err = insert_entries(&mut tx, id, rows).await.unwrap_err();
        assert!(
            err.to_string().contains("does not balance"),
            "{rows:?}: {err}"
        );
    }

    // Each statement is checked against everything already stored for the
    // transaction, so a balanced batch cannot be unbalanced by a second insert.
    let mut tx = begin(ledger.pool()).await;
    let id = Uuid::now_v7();
    sqlx::query("INSERT INTO transactions (id) VALUES ($1)")
        .bind(id)
        .execute(&mut *tx)
        .await
        .unwrap();
    insert_entries(
        &mut tx,
        id,
        &[
            (settlement, "debit", 7, "TJS"),
            (wallet, "credit", 7, "TJS"),
        ],
    )
    .await
    .expect("balanced");
    let err = insert_entries(&mut tx, id, &[(wallet, "credit", 1, "TJS")])
        .await
        .unwrap_err();
    assert!(err.to_string().contains("does not balance"), "{err}");
    drop(tx);

    // A transaction with no entries at all (a voided key) is legal.
    let voided = Uuid::now_v7();
    sqlx::query("INSERT INTO transactions (id) VALUES ($1)")
        .bind(voided)
        .execute(ledger.pool())
        .await
        .expect("an entry-less transaction is allowed");

    // The normal write path is unaffected, including multi-currency postings.
    ledger
        .post(&deposit(settlement, wallet, 250))
        .await
        .unwrap();
    assert_eq!(ledger.balance(wallet).await.unwrap().minor_units(), 250);
}
