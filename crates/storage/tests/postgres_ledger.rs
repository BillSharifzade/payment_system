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

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn posts_plan_generically_without_leaking_the_setting() {
    use std::sync::{Arc, Mutex};
    use storage::{HookError, PostOptions};

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
    let mut conn = ledger.pool().acquire().await.unwrap();
    let show = "SELECT current_setting('plan_cache_mode')";
    let default: String = sqlx::query_scalar(show)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_ne!(default, "force_generic_plan");

    // The guard runs inside the post's transaction and sees the setting; the connection
    // goes back to the pool without it, whether the post committed or rolled back.
    for reject in [false, true] {
        let seen = Arc::new(Mutex::new(None));
        let probe = seen.clone();
        let opts = PostOptions {
            idempotency: None,
            guard: Some(Box::new(move |c| {
                Box::pin(async move {
                    let mode: String = sqlx::query_scalar(show).fetch_one(&mut *c).await?;
                    *probe.lock().unwrap() = Some(mode);
                    match reject {
                        true => Err(HookError::Rejected {
                            rule: "test".into(),
                            message: "no".into(),
                        }),
                        false => Ok(()),
                    }
                })
            })),
        };
        let result = ledger
            .post_on(&mut conn, &deposit(settlement, alice, 100), opts)
            .await;
        assert_eq!(result.is_err(), reject);
        assert_eq!(seen.lock().unwrap().as_deref(), Some("force_generic_plan"));
        let after: String = sqlx::query_scalar(show)
            .fetch_one(&mut *conn)
            .await
            .unwrap();
        assert_eq!(after, default, "the setting must not outlive the post");
    }

    // A caller's own transaction still works (a savepoint, without the setting).
    let mut tx = ledger.pool().begin().await.unwrap();
    ledger
        .post_on(
            &mut tx,
            &deposit(settlement, alice, 50),
            PostOptions::default(),
        )
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 150);
}

// A deferred constraint trigger on a temp table fires inside COMMIT and records the
// statement_timeout in force there; the session's own timeout is 5 s.
async fn commit_probe(conn: &mut sqlx::PgConnection) {
    for ddl in [
        "SET statement_timeout = '5s'",
        "CREATE TEMP TABLE probed (x int)",
        "CREATE TEMP TABLE seen_at_commit (statement_timeout text)",
        "CREATE FUNCTION pg_temp.record_timeout() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             INSERT INTO seen_at_commit VALUES (current_setting('statement_timeout'));
             RETURN NULL;
         END $$",
        "CREATE CONSTRAINT TRIGGER probe AFTER INSERT ON probed DEFERRABLE INITIALLY DEFERRED
         FOR EACH ROW EXECUTE FUNCTION pg_temp.record_timeout()",
    ] {
        sqlx::query(ddl).execute(&mut *conn).await.unwrap();
    }
}

async fn seen_at_commit(conn: &mut sqlx::PgConnection) -> Vec<String> {
    sqlx::query_scalar("SELECT statement_timeout FROM seen_at_commit")
        .fetch_all(&mut *conn)
        .await
        .unwrap()
}

async fn session_timeout(conn: &mut sqlx::PgConnection) -> String {
    sqlx::query_scalar("SELECT current_setting('statement_timeout')")
        .fetch_one(&mut *conn)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn durable_commits_run_without_a_statement_timeout() {
    use sqlx::Connection;

    let ledger = connect().await;
    let mut conn = ledger.pool().acquire().await.unwrap();
    commit_probe(&mut conn).await;
    let mut tx = conn.begin().await.unwrap();
    sqlx::query("INSERT INTO probed VALUES (1)")
        .execute(&mut *tx)
        .await
        .unwrap();
    storage::commit_durable(tx).await.unwrap();

    assert_eq!(
        seen_at_commit(&mut conn).await,
        ["0"],
        "the COMMIT must not be cut short by statement_timeout"
    );
    assert_eq!(
        session_timeout(&mut conn).await,
        "5s",
        "and only the COMMIT"
    );
    // Temp objects die with the session; do not hand them back to the pool.
    conn.detach().close().await.unwrap();
}

// post_on lifts the timeout inside its last statement instead (no extra round trip): same
// guarantee for its COMMIT, nothing left on the connection, and inside a caller's transaction
// it leaves the caller's COMMIT alone.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn posts_commit_without_a_statement_timeout() {
    use sqlx::Connection;
    use storage::PostOptions;

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
    let mut conn = ledger.pool().acquire().await.unwrap();
    commit_probe(&mut conn).await;
    let probe = || PostOptions {
        idempotency: None,
        guard: Some(Box::new(|c| {
            Box::pin(async move {
                sqlx::query("INSERT INTO probed VALUES (1)")
                    .execute(&mut *c)
                    .await?;
                Ok(())
            })
        })),
    };

    ledger
        .post_on(&mut conn, &deposit(settlement, alice, 10), probe())
        .await
        .unwrap();
    assert_eq!(seen_at_commit(&mut conn).await, ["0"]);
    assert_eq!(session_timeout(&mut conn).await, "5s");

    let mut tx = conn.begin().await.unwrap();
    ledger
        .post_on(&mut tx, &deposit(settlement, alice, 10), probe())
        .await
        .unwrap();
    tx.commit().await.unwrap();
    assert_eq!(seen_at_commit(&mut conn).await, ["0", "5s"]);
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 20);
    conn.detach().close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn balance_updates_stay_hot_eligible() {
    // Every post rewrites raw_minor, version and updated_at of the balances it touches. An index
    // over any of them makes each of those updates non-HOT: new entries in every balance index
    // and a dead tuple for vacuum, on the hottest table of the system (migration 0029).
    let ledger = connect().await;
    let columns: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT a.attname::text FROM pg_index i
         JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey)
         WHERE i.indrelid = 'balances'::regclass ORDER BY 1",
    )
    .fetch_all(ledger.pool())
    .await
    .unwrap();
    assert_eq!(columns, ["account_id"], "indexed balance columns");
    let derived: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_index WHERE indrelid = 'balances'::regclass
           AND (indexprs IS NOT NULL OR indpred IS NOT NULL)",
    )
    .fetch_one(ledger.pool())
    .await
    .unwrap();
    assert_eq!(derived, 0, "expression or partial indexes on balances");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn system_accounts_are_verified_once_and_cannot_be_retyped() {
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
    // The first post verifies the system account, the second trusts the cache; both land.
    for _ in 0..2 {
        ledger.post(&deposit(settlement, alice, 10)).await.unwrap();
    }
    assert_eq!(ledger.balance(alice).await.unwrap().minor_units(), 20);
    assert_eq!(ledger.balance(settlement).await.unwrap().minor_units(), 20);

    // An account that does not exist is still refused, and not remembered as one that does.
    let ghost = AccountId::new();
    for _ in 0..2 {
        let err = ledger.post(&deposit(ghost, alice, 10)).await.unwrap_err();
        assert!(
            matches!(err, StorageError::Ledger(ledger::LedgerError::UnknownAccount(id)) if id == ghost),
            "{err}"
        );
    }

    // What the cache trusts is what the database refuses to change.
    for change in [
        "UPDATE accounts SET account_type = 'user_wallet' WHERE id = $1",
        "UPDATE accounts SET currency = 'USD' WHERE id = $1",
    ] {
        let err = sqlx::query(change)
            .bind(settlement.as_uuid())
            .execute(ledger.pool())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("immutable"), "{change}: {err}");
    }
    sqlx::query("UPDATE accounts SET created_at = created_at WHERE id = $1")
        .bind(settlement.as_uuid())
        .execute(ledger.pool())
        .await
        .expect("other columns stay writable");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn overdrafts_and_overflows_do_not_depend_on_entry_or_hash_order() {
    use storage::PostOptions;

    let ledger = connect().await;
    let settlement = AccountId::new();
    let (a, b, c) = (AccountId::new(), AccountId::new(), AccountId::new());
    ledger
        .open_account(&Account::new(
            settlement,
            AccountType::SystemSettlement,
            tjs(),
        ))
        .await
        .unwrap();
    for id in [a, b, c] {
        ledger
            .open_account(&Account::new(id, AccountType::UserWallet, tjs()))
            .await
            .unwrap();
    }
    for id in [a, b] {
        ledger.post(&deposit(settlement, id, 1_000)).await.unwrap();
    }
    let m = |minor: i128| Money::from_minor(minor, tjs());

    // Two wallets overdrawn at once: the refusal always names the lower id, whatever the
    // order of the entries (and of any hash map).
    let lower = a.min(b);
    for round in 0..32 {
        let mut entries = vec![
            Entry::debit(a, m(5_000)),
            Entry::debit(b, m(5_000)),
            Entry::credit(c, m(10_000)),
        ];
        if round % 2 == 1 {
            entries.reverse();
        }
        let err = ledger
            .post(&Transaction::with_entries(entries))
            .await
            .unwrap_err();
        match err {
            StorageError::Ledger(ledger::LedgerError::InsufficientFunds {
                account,
                balance_minor,
                delta_minor,
            }) => {
                assert_eq!(account, lower, "round {round}");
                assert_eq!((balance_minor, delta_minor), (1_000, -5_000));
            }
            other => panic!("round {round}: {other}"),
        }
    }

    // A credit and a debit of the same wallet that cancel out pass i64 only part-way through
    // the entries, which must not matter: each account is netted before it is checked. Posted
    // in a transaction that is rolled back — entries this large would overflow every BIGINT
    // SUM over the shared test database (admin metrics, reconciliation).
    let big = i64::MAX as i128 - 500;
    for credit_first in [true, false] {
        let mut entries = vec![Entry::credit(a, m(big)), Entry::debit(a, m(big))];
        if !credit_first {
            entries.reverse();
        }
        entries.push(Entry::debit(a, m(100)));
        entries.push(Entry::credit(c, m(100)));
        let mut tx = ledger.pool().begin().await.unwrap();
        ledger
            .post_on(
                &mut tx,
                &Transaction::with_entries(entries),
                PostOptions::default(),
            )
            .await
            .unwrap_or_else(|e| panic!("credit first {credit_first}: {e}"));
        let raw: i64 = sqlx::query_scalar("SELECT raw_minor FROM balances WHERE account_id = $1")
            .bind(a.as_uuid())
            .fetch_one(&mut *tx)
            .await
            .unwrap();
        assert_eq!(raw, 900, "credit first {credit_first}");
        tx.rollback().await.unwrap();
    }

    // A net that does not fit the balance column is refused as an overflow, not a panic, and
    // nothing is written.
    let err = ledger
        .post(&deposit(settlement, a, i64::MAX as i128 - 10))
        .await
        .unwrap_err();
    assert!(
        matches!(
            err,
            StorageError::Ledger(ledger::LedgerError::Money(
                money::MoneyError::Overflow { .. }
            ))
        ),
        "{err}"
    );
    assert_eq!(ledger.balance(a).await.unwrap().minor_units(), 1_000);
    assert_eq!(ledger.balance(c).await.unwrap().minor_units(), 0);
}
