mod common;

use std::str::FromStr;
use std::time::Duration;

use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use uuid::Uuid;

async fn lookup(app: &axum::Router, token: &str, id: &str) -> (StatusCode, Value) {
    send(
        app,
        req(
            "GET",
            &format!("/v1/transactions/{id}"),
            Some(token),
            None,
            Value::Null,
        ),
    )
    .await
}

async fn void(app: &axum::Router, token: &str, id: &str) -> (StatusCode, Value) {
    send(
        app,
        req(
            "POST",
            &format!("/v1/transactions/{id}/void"),
            Some(token),
            None,
            Value::Null,
        ),
    )
    .await
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn a_voided_key_can_never_post() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let alice = party(&app, &pool, None).await;
    let bob = party(&app, &pool, None).await;
    let alice_usd = create_wallet_cur(&app, &alice.token, "USD").await;
    admin_deposit(&app, &admin, &alice.wallet, 10_000).await;
    let seed = Uuid::new_v4();
    let (status, _) = enroll(&app, &alice.token, 3, seed).await;
    assert_eq!(status, StatusCode::CREATED);
    let check_key = Uuid::new_v4().to_string();
    let (status, _) =
        create_check(&app, &bob.token, &bob.wallet, 500, &check_key, Value::Null).await;
    assert_eq!(status, StatusCode::CREATED);

    let key = Uuid::new_v4().to_string();
    let (status, v) = void(&app, &alice.token, &key).await;
    assert_eq!(status, StatusCode::OK, "{v}");
    assert_eq!(v["transaction_id"], key);
    assert_eq!(v["status"], "voided");
    assert!(v["created_at"].as_str().unwrap().ends_with('Z'));
    assert!(v.get("entries").is_none());
    let (status, again) = void(&app, &alice.token, &key).await;
    assert_eq!(status, StatusCode::OK, "void is idempotent");
    assert_eq!(again, v);
    let (status, got) = lookup(&app, &alice.token, &key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(got, v);
    assert_eq!(
        lookup(&app, &bob.token, &key).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(void(&app, &bob.token, &key).await.0, StatusCode::NOT_FOUND);

    let voided = |(status, body): (StatusCode, Value)| {
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"]["code"], "voided");
    };
    voided(transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 100, &key).await);
    voided(
        send(
            &app,
            req(
                "POST",
                "/v1/fx",
                Some(&alice.token),
                Some(&key),
                json!({"from_account": alice.wallet, "to_account": alice_usd, "amount_minor": 100}),
            ),
        )
        .await,
    );
    voided(
        send(
            &app,
            req(
                "POST",
                "/v1/deposits",
                Some(&admin),
                Some(&key),
                json!({"user_account": alice.wallet, "amount_minor": 100}),
            ),
        )
        .await,
    );
    voided(pay_check_app(&app, &alice.token, &check_key, &key, json!({})).await);
    voided(pay_check(&app, &bob, &check_key, seed, &key).await);
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 10_000);
    let (_, check) = get_check(&app, &bob.token, &check_key).await;
    assert_eq!(check["status"], "open");

    let paid = Uuid::new_v4().to_string();
    let (status, body) =
        transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 2_500, &paid).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (status, mine) = lookup(&app, &alice.token, &paid).await;
    assert_eq!(status, StatusCode::OK, "{mine}");
    assert_eq!(mine["status"], "posted");
    assert_eq!(mine["kind"], "transfer");
    assert_eq!(
        mine["entries"],
        json!([{"account_id": alice.wallet, "direction": "debit", "amount_minor": 2_500, "currency": "TJS"}])
    );
    let (status, theirs) = lookup(&app, &bob.token, &paid).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(theirs["entries"][0]["account_id"], bob.wallet);
    assert_eq!(theirs["entries"][0]["direction"], "credit");
    let (status, body) = void(&app, &alice.token, &paid).await;
    assert_eq!(
        status,
        StatusCode::OK,
        "voiding a posted key reports it: {body}"
    );
    assert_eq!(body, mine);
    let stranger = party(&app, &pool, None).await;
    assert_eq!(
        lookup(&app, &stranger.token, &paid).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        void(&app, &stranger.token, &paid).await.0,
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        lookup(&app, &alice.token, &Uuid::new_v4().to_string())
            .await
            .0,
        StatusCode::NOT_FOUND
    );
    let entries: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM entries WHERE transaction_id = $1")
        .bind(Uuid::parse_str(&key).unwrap())
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(entries, 0);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn lookup_classifies_like_the_statement() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let alice = party(&app, &pool, None).await;
    let alice_usd = create_wallet_cur(&app, &alice.token, "USD").await;
    let deposit = Uuid::new_v4().to_string();
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/deposits",
            Some(&admin),
            Some(&deposit),
            json!({"user_account": alice.wallet, "amount_minor": 10_000}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (_, d) = lookup(&app, &alice.token, &deposit).await;
    assert_eq!(d["kind"], "deposit");
    assert_eq!(d["entries"].as_array().unwrap().len(), 1);

    sqlx::query(
        "INSERT INTO fx_rates (base_currency, quote_currency, rate_num, rate_den)
         VALUES ('TJS', 'USD', 1, 10) ON CONFLICT DO NOTHING",
    )
    .execute(&pool)
    .await
    .unwrap();
    let fx = Uuid::new_v4().to_string();
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/fx",
            Some(&alice.token),
            Some(&fx),
            json!({"from_account": alice.wallet, "to_account": alice_usd, "amount_minor": 1_000}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    let (_, f) = lookup(&app, &alice.token, &fx).await;
    assert_eq!(f["kind"], "fx", "{f}");
    let legs = f["entries"].as_array().unwrap();
    assert_eq!(
        legs.len(),
        2,
        "both of the caller's legs, no system legs: {f}"
    );
    assert!(legs
        .iter()
        .any(|e| e["account_id"] == alice_usd && e["direction"] == "credit"));
}

// The void commits after the transfer read its context but before it claimed the id: the
// transfer must learn from post_on's duplicate claim that the key was voided.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn a_void_that_wins_the_race_turns_the_post_into_voided() {
    let base = migrated_pool().await;
    let app_name = format!("void-race-{}", &Uuid::new_v4().simple().to_string()[..8]);
    let url = std::env::var("DATABASE_URL").unwrap();
    let racing_pool = PgPoolOptions::new()
        .max_connections(4)
        .connect_with(
            PgConnectOptions::from_str(&url)
                .unwrap()
                .application_name(&app_name),
        )
        .await
        .unwrap();
    let app = router(racing_pool, TestConfig::default());
    let admin = admin_token(&app, &base).await;
    let alice = party(&app, &base, None).await;
    let bob = party(&app, &base, None).await;
    admin_deposit(&app, &admin, &alice.wallet, 5_000).await;

    let key = Uuid::new_v4();
    let mut gate = base.begin().await.unwrap();
    sqlx::query("LOCK TABLE transactions IN SHARE ROW EXCLUSIVE MODE")
        .execute(&mut *gate)
        .await
        .unwrap();
    let racing = {
        let (app, alice) = (app.clone(), alice.clone());
        let (to, key) = (bob.wallet.clone(), key.to_string());
        tokio::spawn(
            async move { transfer(&app, &alice.token, &alice.wallet, &to, 1_000, &key).await },
        )
    };
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let waiting: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_stat_activity
             WHERE application_name = $1 AND wait_event_type = 'Lock'",
        )
        .bind(&app_name)
        .fetch_one(&base)
        .await
        .unwrap();
        if waiting > 0 {
            break;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "transfer never reached its claim"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    sqlx::query("INSERT INTO transactions (id) VALUES ($1)")
        .bind(key)
        .execute(&mut *gate)
        .await
        .unwrap();
    sqlx::query("INSERT INTO voided_transactions (id, voided_by) VALUES ($1, $2)")
        .bind(key)
        .bind(Uuid::parse_str(&alice.id).unwrap())
        .execute(&mut *gate)
        .await
        .unwrap();
    gate.commit().await.unwrap();

    let (status, body) = racing.await.unwrap();
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    assert_eq!(body["error"]["code"], "voided");
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 5_000);
    let (status, v) = lookup(&app, &alice.token, &key.to_string()).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["status"], "voided");
}
