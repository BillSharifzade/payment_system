mod common;

use api::{AmlConfig, FeeConfig, Limits, RateLimitState};
use axum::http::StatusCode;
use common::*;
use serde_json::json;
use uuid::Uuid;

const FUNDS: i64 = 1_000_000;
const AMOUNT: i64 = 100;
const ROUNDS: usize = 12;

fn roomy_limits() -> AmlConfig {
    let l = Limits {
        per_tx_minor: FUNDS,
        daily_minor: 1_000_000_000,
        velocity_per_hour: 1_000_000,
    };
    AmlConfig {
        level1: l,
        level2: l,
    }
}

// A post locks, in this order: its users row, at most one check, its wallets (one statement,
// id order), system shards (sorted). Payments crossing in opposite directions, running in a
// cycle, paying a merchant who is paying out at the same time, and paying that merchant's
// checks must never deadlock — Postgres would abort one side after deadlock_timeout and the
// client would see 503 retry_later. Fees are on, so every post also writes a fee shard.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn crossing_payments_never_deadlock() {
    let pool = migrated_pool().await;
    let app = router(
        pool.clone(),
        TestConfig {
            fees: FeeConfig { transfer_bps: 100 },
            aml: roomy_limits(),
            rate_limit: RateLimitState::new(1_000_000, std::time::Duration::from_secs(60)),
            ..TestConfig::default()
        },
    );
    let admin = admin_token(&app, &pool).await;
    let mut users = Vec::new();
    for _ in 0..7 {
        let p = party(&app, &pool, None).await;
        admin_deposit(&app, &admin, &p.wallet, FUNDS).await;
        users.push(p);
    }
    let merchant = 6;
    // Crossing pairs, a three-way cycle, and everyone paying the merchant while it pays back.
    let mut edges = vec![(0, 1), (1, 0), (2, 3), (3, 2), (3, 4), (4, 5), (5, 3)];
    for u in 0..merchant {
        edges.push((u, merchant));
        edges.push((merchant, u));
    }
    let mut checks = Vec::new();
    for _ in 0..ROUNDS * 2 {
        let id = Uuid::new_v4().to_string();
        let m = &users[merchant];
        let (status, body) = create_check(&app, &m.token, &m.wallet, AMOUNT, &id, json!({})).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        checks.push(id);
    }

    let mut tasks = Vec::new();
    for round in 0..ROUNDS {
        for &(from, to) in &edges {
            let (app, a, b) = (app.clone(), users[from].clone(), users[to].clone());
            tasks.push(tokio::spawn(async move {
                let key = Uuid::new_v4().to_string();
                transfer(&app, &a.token, &a.wallet, &b.wallet, AMOUNT, &key).await
            }));
        }
        for (i, check) in checks[round * 2..round * 2 + 2].iter().enumerate() {
            let (app, payer, check) = (app.clone(), users[i * 2].clone(), check.clone());
            tasks.push(tokio::spawn(async move {
                let key = Uuid::new_v4().to_string();
                let body = json!({"account": payer.wallet});
                pay_check_app(&app, &payer.token, &check, &key, body).await
            }));
        }
    }
    let posted = tasks.len() as i64;
    for t in tasks {
        let (status, body) = t.await.unwrap();
        assert_eq!(
            status,
            StatusCode::CREATED,
            "every post lands first time: {body}"
        );
    }

    // Each post moved AMOUNT and charged a 1% fee: the users together hold all but the fees.
    let mut held = 0;
    for u in &users {
        held += balance_of(&app, &u.token, &u.wallet).await;
    }
    assert_eq!(held, 7 * FUNDS - posted * AMOUNT / 100);
}

// Posts spread system-account deltas round-robin over api::system_shards(); one id missing
// from the database would fail a post now and then, whenever the counter reached it.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn every_system_shard_exists() {
    let pool = migrated_pool().await;
    let shards = api::system_shards();
    assert_eq!(shards.len(), 5 * 64);
    for (id, kind, currency) in shards {
        let row: Option<(String, String)> =
            sqlx::query_as("SELECT account_type, currency FROM accounts WHERE id = $1")
                .bind(id)
                .fetch_optional(&pool)
                .await
                .unwrap();
        assert_eq!(
            row,
            Some((kind.to_string(), currency.to_string())),
            "shard {id}"
        );
    }
}
