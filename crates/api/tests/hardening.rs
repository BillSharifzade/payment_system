mod common;

use api::{AmlConfig, Limits, RateLimitState};
use axum::http::StatusCode;
use common::*;
use serde_json::{json, Value};
use storage::PostgresLedger;
use uuid::Uuid;

fn tight_aml(daily_minor: i64, velocity_per_hour: i64) -> AmlConfig {
    let l = Limits {
        per_tx_minor: 1_000,
        daily_minor,
        velocity_per_hour,
    };
    AmlConfig {
        level1: l,
        level2: l,
    }
}

static FX_RATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

async fn set_usd_tjs_rate(app: &axum::Router, admin: &str) {
    let (status, body) = send(
        app,
        req(
            "POST",
            "/v1/admin/fx-rates",
            Some(admin),
            None,
            json!({"base": "USD", "quote": "TJS", "rate_num": 10, "rate_den": 1}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "set rate: {body}");
}

async fn usd_deposit(app: &axum::Router, admin: &str, wallet: &str, amount_minor: i64) {
    let (status, body) = send(
        app,
        req(
            "POST",
            "/v1/deposits",
            Some(admin),
            Some(&Uuid::new_v4().to_string()),
            json!({"user_account": wallet, "amount_minor": amount_minor, "currency": "USD"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "usd deposit: {body}");
}

struct TwoWallets {
    token: String,
    tjs: String,
    usd: String,
}

async fn two_wallets(app: &axum::Router, pool: &sqlx::PgPool, admin: &str) -> TwoWallets {
    let (id, token) = register(app).await;
    verify_kyc(pool, &id).await;
    let tjs = create_wallet(app, &token).await;
    let usd = create_wallet_cur(app, &token, "USD").await;
    admin_deposit(app, admin, &tjs, 100_000).await;
    usd_deposit(app, admin, &usd, 10_000).await;
    TwoWallets { token, tjs, usd }
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn aml_daily_limit_is_per_user_across_wallets_under_concurrency() {
    let _fx = FX_RATE_LOCK.lock().await;
    let pool = migrated_pool().await;
    let app = router_with_aml(
        pool.clone(),
        RateLimitState::default(),
        tight_aml(5_000, 1_000),
    );
    let admin = admin_token(&app, &pool).await;
    set_usd_tjs_rate(&app, &admin).await;
    let alice = two_wallets(&app, &pool, &admin).await;
    let bob = two_wallets(&app, &pool, &admin).await;

    // 6 × 1 000 TJS from one wallet and 6 × 100 USD (= 1 000 TJS each) from the other, all at
    // once: a per-wallet window would let 10 through; the user's window allows exactly 5.
    let mut handles = Vec::new();
    for i in 0..12 {
        let app = app.clone();
        let tok = alice.token.clone();
        let (from, to, amount, currency) = if i % 2 == 0 {
            (alice.tjs.clone(), bob.tjs.clone(), 1_000, "TJS")
        } else {
            (alice.usd.clone(), bob.usd.clone(), 100, "USD")
        };
        handles.push(tokio::spawn(async move {
            let key = Uuid::new_v4().to_string();
            transfer_in(&app, &tok, (&from, &to), amount, currency, &key).await
        }));
    }
    let (mut posted, mut limited) = (0, 0);
    for h in handles {
        let (status, body) = h.await.unwrap();
        match status {
            StatusCode::CREATED => posted += 1,
            StatusCode::UNPROCESSABLE_ENTITY => {
                assert_eq!(body["error"]["code"], "limit_exceeded", "{body}");
                limited += 1;
            }
            other => panic!("unexpected {other}: {body}"),
        }
    }
    assert_eq!((posted, limited), (5, 7));
    let received_tjs = balance_of(&app, &bob.token, &bob.tjs).await - 100_000;
    let received_usd = balance_of(&app, &bob.token, &bob.usd).await - 10_000;
    assert_eq!(
        received_tjs + received_usd * 10,
        5_000,
        "TJS-equivalent posted"
    );
    let sent_tjs = 100_000 - balance_of(&app, &alice.token, &alice.tjs).await;
    let sent_usd = 10_000 - balance_of(&app, &alice.token, &alice.usd).await;
    assert_eq!(sent_tjs + sent_usd * 10, 5_000);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn aml_velocity_counts_every_wallet_of_the_user() {
    let _fx = FX_RATE_LOCK.lock().await;
    let pool = migrated_pool().await;
    let app = router_with_aml(
        pool.clone(),
        RateLimitState::default(),
        tight_aml(1_000_000, 3),
    );
    let admin = admin_token(&app, &pool).await;
    set_usd_tjs_rate(&app, &admin).await;
    let alice = two_wallets(&app, &pool, &admin).await;
    let bob = two_wallets(&app, &pool, &admin).await;

    for (from, to, amount, currency) in [
        (&alice.tjs, &bob.tjs, 100, "TJS"),
        (&alice.usd, &bob.usd, 10, "USD"),
        (&alice.tjs, &bob.tjs, 100, "TJS"),
    ] {
        let key = Uuid::new_v4().to_string();
        let (status, body) =
            transfer_in(&app, &alice.token, (from, to), amount, currency, &key).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
    }
    let key = Uuid::new_v4().to_string();
    let usd = (alice.usd.as_str(), bob.usd.as_str());
    let (status, body) = transfer_in(&app, &alice.token, usd, 10, "USD", &key).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "limit_exceeded");
    let rule: String = sqlx::query_scalar(
        "SELECT rule FROM screening_events WHERE from_account = $1 AND decision = 'blocked'
         ORDER BY created_at DESC LIMIT 1",
    )
    .bind(Uuid::parse_str(&alice.usd).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(rule, "velocity");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn one_wallet_per_currency_even_under_concurrency() {
    let (app, _pool) = app().await;
    let (_id, tok) = register(&app).await;
    let tjs = create_wallet(&app, &tok).await;

    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/wallets",
            Some(&tok),
            None,
            json!({"currency": "TJS"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "existing TJS wallet: {body}");
    assert_eq!(body["id"], tjs);
    assert_eq!(body["currency"], "TJS");

    let mut handles = Vec::new();
    for _ in 0..8 {
        let (app, tok) = (app.clone(), tok.clone());
        handles.push(tokio::spawn(async move {
            send(
                &app,
                req(
                    "POST",
                    "/v1/wallets",
                    Some(&tok),
                    None,
                    json!({"currency": "USD"}),
                ),
            )
            .await
        }));
    }
    let mut ids = Vec::new();
    let mut created = 0;
    for h in handles {
        let (status, body) = h.await.unwrap();
        match status {
            StatusCode::CREATED => created += 1,
            StatusCode::OK => {}
            other => panic!("unexpected {other}: {body}"),
        }
        ids.push(body["id"].as_str().unwrap().to_string());
    }
    assert_eq!(created, 1, "exactly one USD wallet is created");
    assert!(ids.windows(2).all(|w| w[0] == w[1]), "{ids:?}");

    let (_, wallets) = send(
        &app,
        req("GET", "/v1/wallets", Some(&tok), None, Value::Null),
    )
    .await;
    let wallets = wallets.as_array().unwrap();
    assert_eq!(wallets.len(), 2, "{wallets:?}");
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/wallets",
            Some(&tok),
            None,
            json!({"currency": "XYZ"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn registration_creates_user_and_wallet_atomically() {
    let (app, pool) = app().await;
    let phone = format!("992{:09}", Uuid::new_v4().as_u128() % 1_000_000_000);
    let tag = Uuid::new_v4().simple().to_string();
    let func = format!("test_refuse_wallet_{tag}");
    // Make the wallet insert fail for this phone only (concurrent tests are unaffected).
    sqlx::query(&format!(
        "CREATE FUNCTION {func}() RETURNS trigger LANGUAGE plpgsql AS $$
         BEGIN
             IF EXISTS (SELECT 1 FROM users WHERE id = NEW.owner_user_id AND phone = '{phone}') THEN
                 RAISE EXCEPTION 'wallet refused by test';
             END IF;
             RETURN NEW;
         END $$"
    ))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(&format!(
        "CREATE TRIGGER {func} BEFORE INSERT ON accounts FOR EACH ROW EXECUTE FUNCTION {func}()"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let creds = json!({"phone": phone, "password": "password123"});
    let (status, body) = send(
        &app,
        req("POST", "/v1/auth/register", None, None, creds.clone()),
    )
    .await;

    sqlx::query(&format!("DROP TRIGGER {func} ON accounts"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP FUNCTION {func}()"))
        .execute(&pool)
        .await
        .unwrap();

    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR, "{body}");
    let users: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users WHERE phone = $1")
        .bind(&phone)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        users, 0,
        "a failed wallet insert must not leave the user behind"
    );

    let (status, body) = send(&app, req("POST", "/v1/auth/register", None, None, creds)).await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "retry after the failure: {body}"
    );
    let tok = body["access_token"].as_str().unwrap();
    let (_, wallets) = send(
        &app,
        req("GET", "/v1/wallets", Some(tok), None, Value::Null),
    )
    .await;
    assert_eq!(wallets.as_array().unwrap().len(), 1);
    assert_eq!(wallets[0]["currency"], "TJS");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn payments_to_inactive_recipients_are_refused() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let alice = party(&app, &pool, None).await;
    let bob = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &alice.wallet, 10_000).await;
    let check_key = Uuid::new_v4().to_string();
    let (status, _) = create_check(
        &app,
        &bob.token,
        &bob.wallet,
        1_000,
        &check_key,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, _) = send(
        &app,
        req(
            "POST",
            &format!("/v1/admin/users/{}/status", bob.id),
            Some(&admin),
            None,
            json!({"status": "frozen"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let key = Uuid::new_v4().to_string();
    let (status, body) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 500, &key).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "recipient_unavailable");
    let key = Uuid::new_v4().to_string();
    let (status, body) = pay_check_app(&app, &alice.token, &check_key, &key, json!({})).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "recipient_unavailable");
    let (_, check) = get_check(&app, &bob.token, &check_key).await;
    assert_eq!(check["status"], "open");
    assert_eq!(balance_of(&app, &alice.token, &alice.wallet).await, 10_000);

    let (status, _) = send(
        &app,
        req(
            "POST",
            &format!("/v1/admin/users/{}/status", bob.id),
            Some(&admin),
            None,
            json!({"status": "active"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let key = Uuid::new_v4().to_string();
    let (status, body) = transfer(&app, &alice.token, &alice.wallet, &bob.wallet, 500, &key).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn admins_cannot_review_their_own_kyc() {
    let (app, pool) = app().await;
    let (admin_id, admin) = register(&app).await;
    make_admin(&pool, &admin_id).await;
    let other_admin = admin_token(&app, &pool).await;
    let submit = |name: &str| {
        json!({"requested_level": 1, "full_name": name,
               "document_type": "passport", "document_ref": "obj://doc/self"})
    };
    let (status, sub) = send(
        &app,
        req(
            "POST",
            "/v1/kyc/submissions",
            Some(&admin),
            None,
            submit("Self Reviewer"),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{sub}");
    let id = sub["id"].as_str().unwrap();

    for (action, body) in [
        ("approve", Value::Null),
        ("reject", json!({"reason": "mine"})),
    ] {
        let (status, body) = send(
            &app,
            req(
                "POST",
                &format!("/v1/kyc/submissions/{id}/{action}"),
                Some(&admin),
                None,
                body,
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "self {action}: {body}");
        assert_eq!(body["error"]["code"], "dual_control_required");
    }
    let (status, body) = send(
        &app,
        req(
            "POST",
            &format!("/v1/kyc/submissions/{}/approve", Uuid::new_v4()),
            Some(&admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

    let (status, body) = send(
        &app,
        req(
            "POST",
            &format!("/v1/kyc/submissions/{id}/approve"),
            Some(&other_admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let (_, kyc) = send(&app, req("GET", "/v1/kyc", Some(&admin), None, Value::Null)).await;
    assert_eq!(kyc["kyc_level"], 1);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn migrations_run_with_the_owner_credentials_they_are_given() {
    let url = std::env::var("DATABASE_URL").unwrap();
    let runtime = PostgresLedger::new(connect().await);

    api::run_migrations(&runtime, Some(&url)).await.unwrap();
    runtime.lookup_currency("USD").await.unwrap();
    let applied: i64 = sqlx::query_scalar("SELECT MAX(version) FROM _sqlx_migrations")
        .fetch_one(runtime.pool())
        .await
        .unwrap();
    assert!(applied >= 24, "latest migration applied: {applied}");

    // The owner URL, not the runtime pool, is what migrations connect with.
    let wrong = url.replacen("payment_dev_pw", "not-the-password", 1);
    assert_ne!(wrong, url, "test expects the dev password in DATABASE_URL");
    assert!(api::run_migrations(&runtime, Some(&wrong)).await.is_err());
    api::run_migrations(&runtime, None).await.unwrap();
}
