//! Direct posting (`TIGERBEETLE_POSTING=direct`): the cluster decides transfers and
//! conversions and enforces the AML limits; the journal follows it. These suites run under
//! `TEST_LEDGER_BACKEND=tigerbeetle-direct-sim` (the in-process cluster model) and
//! `tigerbeetle-direct` (a live cluster), and skip themselves on the other backends — whose
//! own suites (api.rs, aml_windows.rs, voids.rs, …) all run under direct posting as well.

mod common;

use std::time::Duration;

use api::{AmlConfig, Limits, RateLimitState};
use axum::http::StatusCode;
use common::*;
use ledger::{AccountId, Entry, Transaction, TransactionId};
use money::{Currency, Money};
use serde_json::{json, Value};
use sqlx::PgPool;
use uuid::Uuid;

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

fn with_limits(daily_minor: i64, velocity_per_hour: i64) -> TestConfig {
    TestConfig {
        aml: limits(daily_minor, velocity_per_hour),
        ..TestConfig::default()
    }
}

fn key() -> String {
    Uuid::new_v4().to_string()
}

async fn funded(app: &axum::Router, pool: &PgPool, amount: i64) -> Party {
    let p = party(app, pool, None).await;
    let admin = admin_token(app, pool).await;
    admin_deposit(app, &admin, &p.wallet, amount).await;
    p
}

async fn count(pool: &PgPool, sql: &str, id: Uuid) -> i64 {
    sqlx::query_scalar(sql)
        .bind(id)
        .fetch_one(pool)
        .await
        .unwrap()
}

fn code(body: &Value) -> &str {
    body["error"]["code"].as_str().unwrap_or("")
}

/// `n` transfers of `amount` at once, each with its own key: how many posted, and the
/// distinct error codes of the others.
async fn burst(
    app: &axum::Router,
    from: &Party,
    to: &Party,
    n: usize,
    amount: i64,
) -> (usize, Vec<String>) {
    let tasks: Vec<_> = (0..n)
        .map(|_| {
            let (app, token) = (app.clone(), from.token.clone());
            let (a, b) = (from.wallet.clone(), to.wallet.clone());
            tokio::spawn(async move { transfer(&app, &token, &a, &b, amount, &key()).await })
        })
        .collect();
    let mut posted = 0;
    let mut refused = Vec::new();
    for t in tasks {
        match t.await.unwrap() {
            (StatusCode::CREATED, _) => posted += 1,
            (status, body) => {
                assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
                let c = code(&body).to_string();
                if !refused.contains(&c) {
                    refused.push(c);
                }
            }
        }
    }
    (posted, refused)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn the_daily_limit_is_exact_under_concurrency() {
    if !backend().is_direct() {
        return;
    }
    let (app, pool) = app_with(with_limits(10_000, 1_000)).await;
    let alice = funded(&app, &pool, 1_000_000).await;
    let bob = party(&app, &pool, None).await;
    let alice_id = Uuid::parse_str(&alice.id).unwrap();

    // 40 payments of 700 against a limit of 10 000: exactly 14 fit, whatever the interleaving.
    let (posted, refused) = burst(&app, &alice, &bob, 40, 700).await;
    assert_eq!(posted, 14, "refused with {refused:?}");
    assert_eq!(refused, ["limit_exceeded"]);
    assert_eq!(
        balance_of(&app, &alice.token, &alice.wallet).await,
        1_000_000 - 14 * 700
    );

    // What is left of the limit is exactly 200: not one minor unit more.
    let over = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 201, &key()).await;
    assert_eq!(over.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", over.1);
    assert!(
        over.1["error"]["message"]
            .as_str()
            .unwrap()
            .contains("24h limit"),
        "{}",
        over.1
    );
    let exact = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 200, &key()).await;
    assert_eq!(exact.0, StatusCode::CREATED, "{}", exact.1);
    let one = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 1, &key()).await;
    assert_eq!(one.0, StatusCode::UNPROCESSABLE_ENTITY, "{}", one.1);

    // The audit trail the Postgres guard leaves: every decision, with the rule that refused.
    let allowed = count(
        &pool,
        "SELECT COUNT(*) FROM screening_events WHERE user_id = $1 AND decision = 'allowed'",
        alice_id,
    )
    .await;
    let blocked = count(
        &pool,
        "SELECT COUNT(*) FROM screening_events
         WHERE user_id = $1 AND decision = 'blocked' AND rule = 'daily_limit'",
        alice_id,
    )
    .await;
    assert_eq!((allowed, blocked), (15, 28));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn the_velocity_limit_is_exact_under_concurrency() {
    if !backend().is_direct() {
        return;
    }
    let (app, pool) = app_with(with_limits(1_000_000, 5)).await;
    let alice = funded(&app, &pool, 100_000).await;
    let bob = party(&app, &pool, None).await;

    let (posted, refused) = burst(&app, &alice, &bob, 12, 10).await;
    assert_eq!(posted, 5, "refused with {refused:?}");
    assert_eq!(refused, ["limit_exceeded"]);
    let (status, body) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 10, &key()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert!(
        body["error"]["message"]
            .as_str()
            .unwrap()
            .contains("last hour"),
        "{body}"
    );
    let rule: String = sqlx::query_scalar(
        "SELECT rule FROM screening_events WHERE user_id = $1 AND decision = 'blocked'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(Uuid::parse_str(&alice.id).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rule, "velocity");
    // The recipient's limits are their own.
    let (status, body) = transfer(&app, &bob.token, &bob.wallet, &alice.wallet, 10, &key()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn a_window_frees_what_it_held_and_a_refusal_leaves_the_key_free() {
    if !backend().is_direct() {
        return;
    }
    let pool = migrated_pool().await;
    // The windows are 24 h and 1 h; three and two seconds here, so the suite can wait them out
    // (on a live cluster too).
    let ledger = ledger_with(pool.clone(), |mut cfg| {
        cfg.aml_day_secs = 3;
        cfg.aml_hour_secs = 2;
        cfg
    });
    let app = router_on(ledger, with_limits(1_000, 2));
    let alice = funded(&app, &pool, 50_000).await;
    let bob = party(&app, &pool, None).await;
    let pay = |amount: i64, key: String| {
        let (app, alice, bob) = (app.clone(), alice.clone(), bob.clone());
        async move { transfer(&app, &alice.token, &alice.wallet, &bob.wallet, amount, &key).await }
    };

    assert_eq!(pay(1_000, key()).await.0, StatusCode::CREATED);
    let refused_key = key();
    let (status, body) = pay(1_000, refused_key.clone()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "limit_exceeded");

    // The day window (3 s here) passes: the same request, with the same key, now posts.
    tokio::time::sleep(Duration::from_millis(3_500)).await;
    let (status, body) = pay(1_000, refused_key.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, again) = pay(1_000, refused_key).await;
    assert_eq!(status, StatusCode::CREATED, "a replay: {again}");
    assert_eq!(again, body);
    assert_eq!(
        balance_of(&app, &alice.token, &alice.wallet).await,
        50_000 - 2_000
    );

    // Funds: refused, topped up, and the same key posts.
    let poor = party(&app, &pool, None).await;
    let k = key();
    let (status, body) = transfer(&app, &poor.token, &poor.wallet, &bob.wallet, 300, &k).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "insufficient_funds");
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &poor.wallet, 300).await;
    let (status, body) = transfer(&app, &poor.token, &poor.wallet, &bob.wallet, 300, &k).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(balance_of(&app, &poor.token, &poor.wallet).await, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn a_replay_is_answered_as_it_was() {
    if !backend().is_direct() {
        return;
    }
    let pool = migrated_pool().await;
    // No settling layer: the first replay arrives before the journal knows the transaction.
    let app = router_plain(ledger(pool.clone()), TestConfig::default());
    let alice = funded(&app, &pool, 10_000).await;
    let bob = party(&app, &pool, None).await;
    let k = key();

    let (status, first) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 1_000, &k).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(first["status"], "posted");
    let (status, replay) =
        transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 1_000, &k).await;
    assert_eq!((status, &replay), (StatusCode::CREATED, &first));
    let (status, other) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 999, &k).await;
    assert_eq!(status, StatusCode::CONFLICT, "{other}");
    assert_eq!(code(&other), "idempotency_conflict");

    // Once the journal has it (a lookup settles it), the same answers.
    let (status, got) = send(
        &app,
        req(
            "GET",
            &format!("/v1/transactions/{k}"),
            Some(&alice.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{got}");
    assert_eq!(got["status"], "posted");
    assert_eq!(got["kind"], "transfer");
    let (status, replay) =
        transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 1_000, &k).await;
    assert_eq!((status, &replay), (StatusCode::CREATED, &first));
    let (status, other) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 999, &k).await;
    assert_eq!(status, StatusCode::CONFLICT, "{other}");

    // A request that posted is answered as it was even if it would be refused today.
    sqlx::query("UPDATE users SET status = 'frozen' WHERE id = $1")
        .bind(Uuid::parse_str(&alice.id).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let (status, replay) =
        transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 1_000, &k).await;
    assert_eq!((status, &replay), (StatusCode::CREATED, &first));
    let (status, fresh) = transfer(
        &app,
        &alice.token,
        &alice.wallet,
        &bob.wallet,
        1_000,
        &key(),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{fresh}");
    sqlx::query("UPDATE users SET status = 'active' WHERE id = $1")
        .bind(Uuid::parse_str(&alice.id).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        balance_of(&app, &alice.token, &alice.wallet).await,
        9_000,
        "posted once"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn the_journal_follows_the_cluster_exactly_once() {
    if !backend().is_direct() {
        return;
    }
    let pool = migrated_pool().await;
    let cfg = TestConfig {
        fees: api::FeeConfig { transfer_bps: 100 },
        ..TestConfig::default()
    };
    let app = router_plain(ledger(pool.clone()), cfg);
    let alice = funded(&app, &pool, 100_000).await;
    let bob = party(&app, &pool, None).await;

    // 30 transfers at once, then readers at once: each reader settles the journal itself.
    let keys: Vec<Uuid> = (0..30).map(|_| Uuid::new_v4()).collect();
    let posts: Vec<_> = keys
        .iter()
        .map(|k| {
            let (app, alice, bob, k) = (app.clone(), alice.clone(), bob.clone(), k.to_string());
            tokio::spawn(async move {
                transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 1_000, &k).await
            })
        })
        .collect();
    for p in posts {
        let (status, body) = p.await.unwrap();
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let readers: Vec<_> = (0..8)
        .map(|_| {
            let (app, alice) = (app.clone(), alice.clone());
            tokio::spawn(async move {
                send(
                    &app,
                    req(
                        "GET",
                        &format!("/v1/accounts/{}/transactions?limit=100", alice.wallet),
                        Some(&alice.token),
                        None,
                        Value::Null,
                    ),
                )
                .await
            })
        })
        .collect();
    for r in readers {
        let (status, body) = r.await.unwrap();
        assert_eq!(status, StatusCode::OK, "{body}");
        // A client reads its own writes: all 30 debits (and the deposit).
        let debits = body["entries"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|e| e["direction"] == "debit")
            .count();
        assert_eq!(debits, 30, "{body}");
    }

    // More passes find nothing new, and every transaction is in the journal exactly once,
    // as Postgres would have written it: the entries (a fee shard gets its 1%), the outbox
    // event, the allowed screening, and no stored response.
    let hybrid = hybrid(pool.clone()).unwrap();
    let again = hybrid.mirror_all().await.unwrap();
    assert_eq!((again.transactions, again.voids), (0, 0));
    let alice_wallet = Uuid::parse_str(&alice.wallet).unwrap();
    let bob_wallet = Uuid::parse_str(&bob.wallet).unwrap();
    for k in &keys {
        let stamped = count(
            &pool,
            "SELECT COUNT(*) FROM transactions WHERE id = $1 AND tb_timestamp IS NOT NULL",
            *k,
        )
        .await;
        let entries: Vec<(Uuid, String, i64, String)> = sqlx::query_as(
            "SELECT account_id, direction, amount_minor, currency FROM entries
             WHERE transaction_id = $1 ORDER BY direction DESC, amount_minor DESC",
        )
        .bind(k)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(stamped, 1);
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert_eq!(
            entries[0],
            (alice_wallet, "debit".into(), 1_000, "TJS".into())
        );
        assert_eq!(entries[1], (bob_wallet, "credit".into(), 990, "TJS".into()));
        assert_eq!((entries[2].1.as_str(), entries[2].2), ("credit", 10));
        let (events, payload): (i64, Value) = sqlx::query_as(
            "SELECT COUNT(*) OVER (), payload FROM outbox
             WHERE aggregate_id = $1 AND event_type = 'transaction.posted'",
        )
        .bind(k)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(events, 1);
        assert_eq!(payload["transaction_id"], k.to_string());
        assert_eq!(payload["entries"].as_array().unwrap().len(), 3);
        assert_eq!(
            count(
                &pool,
                "SELECT COUNT(*) FROM idempotency_keys WHERE key = $1",
                *k
            )
            .await,
            0
        );
    }
    let screened = count(
        &pool,
        "SELECT COUNT(*) FROM screening_events
         WHERE user_id = $1 AND decision = 'allowed' AND amount_minor = 1000
           AND from_account IS NOT NULL AND to_account IS NOT NULL",
        Uuid::parse_str(&alice.id).unwrap(),
    )
    .await;
    assert_eq!(screened, 30);
    assert_eq!(
        balance_of(&app, &bob.token, &bob.wallet).await,
        30 * 990,
        "the cluster and the journal agree"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn a_key_belongs_to_one_request_whatever_the_endpoint() {
    if !backend().is_direct() {
        return;
    }
    let pool = migrated_pool().await;
    // Unsettled on purpose: the key in the cluster must decide before the journal knows.
    let app = router_plain(ledger(pool.clone()), TestConfig::default());
    let alice = funded(&app, &pool, 50_000).await;
    let bob = party(&app, &pool, None).await;
    let check = |amount: i64| {
        let (app, bob) = (app.clone(), bob.clone());
        async move {
            let (status, body) =
                create_check(&app, &bob.token, &bob.wallet, amount, &key(), Value::Null).await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            body["id"].as_str().unwrap().to_string()
        }
    };

    // A transfer's key cannot pay a check, before or after the journal has the transfer.
    let k = key();
    let (status, body) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 500, &k).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let c = check(700).await;
    for round in 0..2 {
        let (status, body) = pay_check_app(&app, &alice.token, &c, &k, json!({})).await;
        assert_eq!(status, StatusCode::CONFLICT, "round {round}: {body}");
        assert_eq!(code(&body), "idempotency_conflict", "round {round}");
        hybrid(pool.clone()).unwrap().mirror_all().await.unwrap();
    }
    let (_, open) = get_check(&app, &bob.token, &c).await;
    assert_eq!(open["status"], "open");

    // A check payment's key cannot transfer; the check is paid once, with its own key.
    let k2 = key();
    let (status, paid) = pay_check_app(&app, &alice.token, &c, &k2, json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{paid}");
    let (status, body) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 700, &k2).await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(code(&body), "idempotency_conflict");
    let (status, replay) = pay_check_app(&app, &alice.token, &c, &k2, json!({})).await;
    assert_eq!((status, &replay), (StatusCode::CREATED, &paid));

    // A voided key posts nowhere.
    let k3 = key();
    let (status, v) = send(
        &app,
        req(
            "POST",
            &format!("/v1/transactions/{k3}/void"),
            Some(&alice.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["status"], "voided");
    let (status, body) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 100, &k3).await;
    assert_eq!((status, code(&body)), (StatusCode::CONFLICT, "voided"));
    let c2 = check(100).await;
    let (status, body) = pay_check_app(&app, &alice.token, &c2, &k3, json!({})).await;
    assert_eq!(
        (status, code(&body)),
        (StatusCode::CONFLICT, "voided"),
        "{body}"
    );

    assert_eq!(
        balance_of(&app, &alice.token, &alice.wallet).await,
        50_000 - 500 - 700
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn a_void_and_a_transfer_race_in_the_cluster() {
    if !backend().is_direct() {
        return;
    }
    let (app, pool) = app().await;
    let alice = funded(&app, &pool, 100_000).await;
    let bob = party(&app, &pool, None).await;
    let (mut voids_won, mut posts_won) = (0, 0);
    for _ in 0..24 {
        let k = key();
        let void = {
            let (app, alice, k) = (app.clone(), alice.clone(), k.clone());
            tokio::spawn(async move {
                send(
                    &app,
                    req(
                        "POST",
                        &format!("/v1/transactions/{k}/void"),
                        Some(&alice.token),
                        None,
                        Value::Null,
                    ),
                )
                .await
            })
        };
        let post = {
            let (app, alice, bob, k) = (app.clone(), alice.clone(), bob.clone(), k.clone());
            tokio::spawn(async move {
                transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 100, &k).await
            })
        };
        let (void, post) = (void.await.unwrap(), post.await.unwrap());
        assert_eq!(void.0, StatusCode::OK, "{}", void.1);
        match post.0 {
            // The post won: the void answers with the posted transaction.
            StatusCode::CREATED => {
                assert_eq!(void.1["status"], "posted", "{}", void.1);
                posts_won += 1;
            }
            // The void won: the key can never post.
            StatusCode::CONFLICT => {
                assert_eq!(code(&post.1), "voided", "{}", post.1);
                assert_eq!(void.1["status"], "voided", "{}", void.1);
                voids_won += 1;
            }
            other => panic!("{other}: {}", post.1),
        }
    }
    assert_eq!(voids_won + posts_won, 24);
    assert_eq!(
        balance_of(&app, &alice.token, &alice.wallet).await,
        100_000 - 100 * posts_won as i64,
        "{posts_won} posted, {voids_won} voided"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn check_payments_count_in_the_same_windows() {
    if !backend().is_direct() {
        return;
    }
    let (app, pool) = app_with(with_limits(1_000, 100)).await;
    let alice = funded(&app, &pool, 50_000).await;
    let bob = party(&app, &pool, None).await;
    let check = |amount: i64| {
        let (app, bob) = (app.clone(), bob.clone());
        async move {
            let (status, body) =
                create_check(&app, &bob.token, &bob.wallet, amount, &key(), Value::Null).await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            body["id"].as_str().unwrap().to_string()
        }
    };

    let (status, body) =
        transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 600, &key()).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    // 600 + 500 is over: the check stays open and nothing of the refusal is left behind.
    let big = check(500).await;
    let k = key();
    let (status, body) = pay_check_app(&app, &alice.token, &big, &k, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(code(&body), "limit_exceeded");
    assert_eq!(get_check(&app, &bob.token, &big).await.1["status"], "open");
    // 600 + 400 fits exactly; then nothing more does, on either path.
    let small = check(400).await;
    let (status, body) = pay_check_app(&app, &alice.token, &small, &key(), json!({})).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, body) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 1, &key()).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    let (status, body) = pay_check_app(&app, &alice.token, &big, &k, json!({})).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(
        balance_of(&app, &alice.token, &alice.wallet).await,
        50_000 - 1_000
    );
    let screened = count(
        &pool,
        "SELECT COUNT(*) FROM screening_events WHERE user_id = $1 AND decision = 'allowed'",
        Uuid::parse_str(&alice.id).unwrap(),
    )
    .await;
    assert_eq!(screened, 2, "one transfer, one check payment");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn a_conversion_replay_answers_with_what_it_moved() {
    if !backend().is_direct() {
        return;
    }
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let alice = funded(&app, &pool, 100_000).await;
    let usd = create_wallet_cur(&app, &alice.token, "USD").await;
    let rate = |num: i64, den: i64| {
        let (app, admin) = (app.clone(), admin.clone());
        async move {
            let (status, _) = send(
                &app,
                req(
                    "POST",
                    "/v1/admin/fx-rates",
                    Some(&admin),
                    None,
                    json!({"base": "TJS", "quote": "USD", "rate_num": num, "rate_den": den}),
                ),
            )
            .await;
            assert_eq!(status, StatusCode::NO_CONTENT);
        }
    };
    let convert = |k: String| {
        let (app, alice, usd) = (app.clone(), alice.clone(), usd.clone());
        async move {
            send(
                &app,
                req(
                    "POST",
                    "/v1/fx",
                    Some(&alice.token),
                    Some(&k),
                    json!({"from_account": alice.wallet, "to_account": usd, "amount_minor": 10_000}),
                ),
            )
            .await
        }
    };

    rate(1, 10).await;
    let k = key();
    let (status, first) = convert(k.clone()).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    assert_eq!(
        (&first["debited_minor"], &first["credited_minor"]),
        (&json!(10_000), &json!(1_000))
    );
    // The rate moves; the replay still says what the first answer said, and moves nothing.
    rate(1, 20).await;
    let (status, replay) = convert(k).await;
    assert_eq!((status, &replay), (StatusCode::CREATED, &first));
    assert_eq!(balance_of(&app, &alice.token, &usd).await, 1_000);
    let (status, fresh) = convert(key()).await;
    assert_eq!(status, StatusCode::CREATED, "{fresh}");
    assert_eq!(fresh["credited_minor"], 500);
    rate(1, 10).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn the_switch_carries_the_windows_into_the_cluster() {
    if !backend().is_direct() {
        return;
    }
    let aml = limits(10_000, 100);
    let (app, pool) = app_with(TestConfig {
        aml,
        ..TestConfig::default()
    })
    .await;
    let alice = funded(&app, &pool, 100_000).await;
    let bob = party(&app, &pool, None).await;
    let (alice_id, wallet) = (
        Uuid::parse_str(&alice.id).unwrap(),
        Uuid::parse_str(&alice.wallet).unwrap(),
    );

    // Debits the journal holds from before direct posting (as hybrid posting left them): 6 000
    // two hours ago, 3 000 thirty hours ago. Only the first is still in its window.
    for (amount, ago) in [(6_000_i64, "2 hours"), (3_000, "30 hours")] {
        let txn = Uuid::now_v7();
        let settlement = api::settlement_shard("TJS").unwrap();
        let mut tx = pool.begin().await.unwrap();
        sqlx::query("INSERT INTO transactions (id) VALUES ($1)")
            .bind(txn)
            .execute(&mut *tx)
            .await
            .unwrap();
        sqlx::query(
            "INSERT INTO entries
               (id, transaction_id, account_id, direction, amount_minor, currency, created_at)
             SELECT gen_random_uuid(), $1, l.account, l.direction, $4, 'TJS', now() - $5::interval
             FROM (VALUES ($2::uuid, 'debit'), ($3::uuid, 'credit')) AS l(account, direction)",
        )
        .bind(txn)
        .bind(wallet)
        .bind(settlement)
        .bind(amount)
        .bind(ago)
        .execute(&mut *tx)
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let money = Money::from_minor(amount as i128, Currency::tjs());
        mirror_into_cluster(
            &pool,
            &Transaction::new(
                TransactionId(txn),
                vec![
                    Entry::debit(AccountId(wallet), money),
                    Entry::credit(AccountId(settlement), money),
                ],
            ),
        )
        .await;
    }

    // What `payment-server tigerbeetle-posting direct` does, for this user.
    let hybrid = hybrid(pool.clone()).unwrap();
    let seeds: Vec<_> = api::hold_seeds(&pool, &aml)
        .await
        .unwrap()
        .into_iter()
        .filter(|s| s.screening.user == alice_id)
        .collect();
    assert_eq!(seeds.len(), 1, "only the debit inside 24 h: {seeds:?}");
    assert_eq!(seeds[0].screening.tjs_minor, 6_000);
    assert_eq!(
        seeds[0].hour_left_secs, 0,
        "two hours old: out of the 1 h window"
    );
    assert!((79_000..=79_200).contains(&seeds[0].day_left_secs));
    assert_eq!(hybrid.tb().seed_holds(&seeds).await.unwrap(), 1);
    let free = hybrid.tb().allowances(alice_id).await.unwrap().unwrap();
    // Seeding again changes nothing.
    assert_eq!(hybrid.tb().seed_holds(&seeds).await.unwrap(), 1);
    assert_eq!(
        hybrid.tb().allowances(alice_id).await.unwrap().unwrap(),
        free
    );

    // 6 000 of the 10 000 are used: 4 001 is refused, 4 000 fits, then nothing.
    let pay = |amount: i64| {
        let (app, alice, bob) = (app.clone(), alice.clone(), bob.clone());
        async move {
            transfer(
                &app,
                &alice.token,
                &alice.wallet,
                &bob.wallet,
                amount,
                &key(),
            )
            .await
            .0
        }
    };
    assert_eq!(pay(4_001).await, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(pay(4_000).await, StatusCode::CREATED);
    assert_eq!(pay(1).await, StatusCode::UNPROCESSABLE_ENTITY);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn limits_follow_the_tier_without_touching_the_cluster() {
    if !backend().is_direct() {
        return;
    }
    // Level 1: 1 000 a day; level 2: 10 000 a day.
    let aml = AmlConfig {
        level1: Limits {
            per_tx_minor: 1_000_000,
            daily_minor: 1_000,
            velocity_per_hour: 100,
        },
        level2: Limits {
            per_tx_minor: 1_000_000,
            daily_minor: 10_000,
            velocity_per_hour: 100,
        },
    };
    let (app, pool) = app_with(TestConfig {
        aml,
        rate_limit: RateLimitState::default(),
        ..TestConfig::default()
    })
    .await;
    let alice = funded(&app, &pool, 100_000).await;
    let bob = party(&app, &pool, None).await;
    let pay = |amount: i64| {
        let (app, alice, bob) = (app.clone(), alice.clone(), bob.clone());
        async move {
            transfer(
                &app,
                &alice.token,
                &alice.wallet,
                &bob.wallet,
                amount,
                &key(),
            )
            .await
            .0
        }
    };
    // Half of the level-1 limit used.
    assert_eq!(pay(500).await, StatusCode::CREATED);
    assert_eq!(pay(501).await, StatusCode::UNPROCESSABLE_ENTITY);
    // Level 2 from the next request on: what was used stays the same share (half), so half of
    // the new limit is free.
    sqlx::query("UPDATE users SET kyc_level = 2 WHERE id = $1")
        .bind(Uuid::parse_str(&alice.id).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(pay(5_001).await, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(pay(5_000).await, StatusCode::CREATED);
    assert_eq!(pay(1).await, StatusCode::UNPROCESSABLE_ENTITY);
}
