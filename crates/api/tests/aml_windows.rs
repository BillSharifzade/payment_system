mod common;

use api::{AmlConfig, Limits, RateLimitState};
use axum::http::StatusCode;
use common::*;
use ledger::{AccountId, Entry, Transaction, TransactionId};
use money::{Currency, Money};
use sqlx::PgPool;
use uuid::Uuid;

// Daily debits from which the guard keeps a user's window (payments::AML_WINDOW_FROM_DEBITS).
const STORED_FROM: i64 = 64;

fn limits(daily_minor: i64, velocity_per_hour: i64) -> AmlConfig {
    let l = Limits {
        per_tx_minor: 1_000_000,
        daily_minor,
        velocity_per_hour,
    };
    AmlConfig {
        level1: l,
        level2: l,
    }
}

// `count` debits of `amount` from `wallet` written straight into the ledger as if `ago` in the
// past (balances kept consistent), the way an older server version or another code path would
// leave them.
async fn debits_at(pool: &PgPool, wallet: &str, count: i64, amount: i64, ago: &str) {
    let wallet = Uuid::parse_str(wallet).unwrap();
    let settlement = api::settlement_shard("TJS").unwrap();
    let txn = Uuid::now_v7();
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO transactions (id) VALUES ($1)")
        .bind(txn)
        .execute(&mut *tx)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO entries (id, transaction_id, account_id, direction, amount_minor, currency, created_at)
         SELECT gen_random_uuid(), $1, l.account, l.direction, $4, 'TJS', now() - $6::interval
         FROM generate_series(1, $5) g
         CROSS JOIN (VALUES ($2::uuid, 'debit'), ($3::uuid, 'credit')) AS l(account, direction)",
    )
    .bind(txn)
    .bind(wallet)
    .bind(settlement)
    .bind(amount)
    .bind(count)
    .bind(ago)
    .execute(&mut *tx)
    .await
    .unwrap();
    if backend() == Backend::Postgres {
        for (account, delta) in [(wallet, -amount * count), (settlement, amount * count)] {
            sqlx::query("UPDATE balances SET raw_minor = raw_minor + $2 WHERE account_id = $1")
                .bind(account)
                .bind(delta)
                .execute(&mut *tx)
                .await
                .unwrap();
        }
    }
    tx.commit().await.unwrap();
    let total = Money::from_minor((amount * count) as i128, Currency::tjs());
    let moved = Transaction::new(
        TransactionId(txn),
        vec![
            Entry::debit(AccountId(wallet), total),
            Entry::credit(AccountId(settlement), total),
        ],
    );
    mirror_into_cluster(pool, &moved).await;
}

/// The stored window next to a full recount of the entries at the same edges (what the guard
/// summed before migration 0030): (stored day_sum, stored hour_count, recount day, recount hour).
async fn window_vs_recount(pool: &PgPool, user: &str) -> (i64, i64, i64, i64) {
    sqlx::query_as(
        "SELECT w.day_sum, w.hour_count,
                COALESCE(SUM(e.amount_minor) FILTER (WHERE e.created_at >= w.day_from), 0)::BIGINT,
                COUNT(e.id) FILTER (WHERE e.created_at >= w.hour_from)
         FROM aml_windows w
         JOIN accounts a ON a.owner_user_id = w.user_id AND a.currency = w.currency
                        AND a.account_type = 'user_wallet'
         LEFT JOIN entries e ON e.account_id = a.id AND e.direction = 'debit'
         WHERE w.user_id = $1 AND w.currency = 'TJS'
         GROUP BY w.day_sum, w.hour_count",
    )
    .bind(Uuid::parse_str(user).unwrap())
    .fetch_one(pool)
    .await
    .unwrap()
}

async fn stored(pool: &PgPool, user: &str) -> bool {
    sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM aml_windows WHERE user_id = $1)")
        .bind(Uuid::parse_str(user).unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

// Rewinds the stored edges as if the last post had happened earlier: the row stays truthful
// (re-summed at the new edges), only staler.
async fn age_window(pool: &PgPool, user: &str, day_from: &str, hour_from: &str) {
    sqlx::query(
        "UPDATE aml_windows w
         SET day_from = now() - $2::interval, hour_from = now() - $3::interval,
             day_sum = (SELECT COALESCE(SUM(e.amount_minor), 0) FROM entries e
                        JOIN accounts a ON a.id = e.account_id
                        WHERE a.owner_user_id = w.user_id AND a.currency = w.currency
                          AND e.direction = 'debit' AND e.created_at >= now() - $2::interval),
             hour_count = (SELECT count(*) FROM entries e
                           JOIN accounts a ON a.id = e.account_id
                           WHERE a.owner_user_id = w.user_id AND a.currency = w.currency
                             AND e.direction = 'debit' AND e.created_at >= now() - $3::interval)
         WHERE w.user_id = $1 AND w.currency = 'TJS'",
    )
    .bind(Uuid::parse_str(user).unwrap())
    .bind(day_from)
    .bind(hour_from)
    .execute(pool)
    .await
    .unwrap();
}

// A stored window must always hold exactly what a full recount at its edges finds: when it is
// first stored, when its edges move forward past old debits, when they move back (a post whose
// transaction started earlier than the last one), and with debits written outside the guard.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn the_window_always_equals_a_full_recount() {
    let pool = migrated_pool().await;
    let app = router_with_aml(
        pool.clone(),
        RateLimitState::default(),
        limits(1_000_000, 1_000),
    );
    let admin = admin_token(&app, &pool).await;
    let alice = party(&app, &pool, None).await;
    let bob = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &alice.wallet, 100_000).await;
    for (count, amount, ago) in [
        (1, 1, "25 hours"),
        (1, 10, "23 hours"),
        (STORED_FROM, 1, "4 hours"),
        (1, 100, "2 hours"),
        (1, 1_000, "30 minutes"),
    ] {
        debits_at(&pool, &alice.wallet, count, amount, ago).await;
    }
    let pay = |amount: i64| {
        let (app, alice, bob) = (app.clone(), alice.clone(), bob.clone());
        async move {
            let key = Uuid::new_v4().to_string();
            let (status, body) =
                transfer(&app, &alice.token, &alice.wallet, &bob.wallet, amount, &key).await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
        }
    };
    let bulk = STORED_FROM;

    // First post: the window is filled from the entries and stored, then its own debit added.
    pay(10_000).await;
    let (day, hour, rday, rhour) = window_vs_recount(&pool, &alice.id).await;
    assert_eq!((day, hour), (rday, rhour));
    assert_eq!((day, hour), (10 + bulk + 100 + 1_000 + 10_000, 2));

    // Edges two hours stale: moving forward drops the 25 h debit from the day and the 2 h one
    // from the hour.
    age_window(&pool, &alice.id, "26 hours", "3 hours").await;
    pay(20_000).await;
    let (day, hour, rday, rhour) = window_vs_recount(&pool, &alice.id).await;
    assert_eq!((day, hour), (rday, rhour));
    assert_eq!((day, hour), (10 + bulk + 100 + 1_000 + 10_000 + 20_000, 3));

    // Edges ahead of now - 24h / now - 1h: moving back re-adds what lies between.
    age_window(&pool, &alice.id, "20 hours", "10 minutes").await;
    debits_at(&pool, &alice.wallet, 1, 7, "0 seconds").await;
    pay(30_000).await;
    let (day, hour, rday, rhour) = window_vs_recount(&pool, &alice.id).await;
    assert_eq!((day, hour), (rday, rhour));
    assert_eq!(
        (day, hour),
        (10 + bulk + 100 + 1_000 + 10_000 + 20_000 + 7 + 30_000, 5)
    );
}

// The decision is the one a full recount would make, for a light user (no stored window) and
// a heavy one alike: a debit written outside the guard counts, a debit older than 24 h does not.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn limits_count_every_debit_in_the_window() {
    for bulk in [0, STORED_FROM] {
        let pool = migrated_pool().await;
        let app = router_with_aml(
            pool.clone(),
            RateLimitState::default(),
            limits(5_000 + bulk, 1_000),
        );
        let admin = admin_token(&app, &pool).await;
        let alice = party(&app, &pool, None).await;
        let bob = party(&app, &pool, None).await;
        admin_deposit(&app, &admin, &alice.wallet, 100_000).await;
        debits_at(&pool, &alice.wallet, 1, 50_000, "25 hours").await;
        if bulk > 0 {
            debits_at(&pool, &alice.wallet, bulk, 1, "4 hours").await;
        }
        let pay = |amount: i64| {
            let (app, alice, bob) = (app.clone(), alice.clone(), bob.clone());
            async move {
                let key = Uuid::new_v4().to_string();
                transfer(&app, &alice.token, &alice.wallet, &bob.wallet, amount, &key)
                    .await
                    .0
            }
        };
        assert_eq!(pay(3_000).await, StatusCode::CREATED, "25 h ago is outside");
        assert_eq!(stored(&pool, &alice.id).await, bulk > 0, "bulk {bulk}");
        debits_at(&pool, &alice.wallet, 1, 1_500, "1 second").await;
        assert_eq!(
            pay(1_000).await,
            StatusCode::UNPROCESSABLE_ENTITY,
            "3 000 + 1 500 + 1 000 over the limit (bulk {bulk})"
        );
        assert_eq!(pay(500).await, StatusCode::CREATED, "exactly at the limit");
    }
}

// The concurrency proof of the original guard, through the stored window: twelve 1 000
// transfers race a 5 000 cap right as the user's window is first stored; exactly five post.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn a_stored_window_stays_exact_under_concurrency() {
    let pool = migrated_pool().await;
    let app = router_with_aml(
        pool.clone(),
        RateLimitState::default(),
        limits(5_000 + STORED_FROM, 1_000),
    );
    let admin = admin_token(&app, &pool).await;
    let alice = party(&app, &pool, None).await;
    let bob = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &alice.wallet, 100_000).await;
    debits_at(&pool, &alice.wallet, STORED_FROM, 1, "4 hours").await;

    let mut tasks = Vec::new();
    for _ in 0..12 {
        let (app, alice, bob) = (app.clone(), alice.clone(), bob.clone());
        tasks.push(tokio::spawn(async move {
            let key = Uuid::new_v4().to_string();
            transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 1_000, &key).await
        }));
    }
    let (mut posted, mut limited) = (0, 0);
    for t in tasks {
        let (status, body) = t.await.unwrap();
        match status {
            StatusCode::CREATED => posted += 1,
            StatusCode::UNPROCESSABLE_ENTITY => limited += 1,
            other => panic!("unexpected {other}: {body}"),
        }
    }
    assert_eq!((posted, limited), (5, 7));
    let (day, _, rday, _) = window_vs_recount(&pool, &alice.id).await;
    assert_eq!((day, rday), (5_000 + STORED_FROM, 5_000 + STORED_FROM));
}
