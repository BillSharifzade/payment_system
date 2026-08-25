//! HTTP-level integration tests, driving the real router against a real
//! PostgreSQL via `tower::oneshot` (no sockets).
//!
//! Requires a database:
//! ```bash
//! DATABASE_URL=postgres://payment:payment_dev_pw@localhost:5432/payment \
//!   cargo test -p api -- --include-ignored
//! ```

use api::{build_router, AmlConfig, AppState, AuthConfig, FeeConfig, Limits, RateLimitState};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use std::time::Duration;
use storage::PostgresLedger;
use tower::ServiceExt;
use uuid::Uuid;

async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .unwrap()
}

fn router_with(pool: PgPool, rate_limit: RateLimitState) -> axum::Router {
    router_full(pool, rate_limit, AmlConfig::default(), FeeConfig::default())
}

fn router_with_aml(pool: PgPool, rate_limit: RateLimitState, aml: AmlConfig) -> axum::Router {
    router_full(pool, rate_limit, aml, FeeConfig::default())
}

fn router_full(
    pool: PgPool,
    rate_limit: RateLimitState,
    aml: AmlConfig,
    fees: FeeConfig,
) -> axum::Router {
    router_full_quota(pool, rate_limit, aml, fees, 10_000)
}

fn router_full_quota(
    pool: PgPool,
    rate_limit: RateLimitState,
    aml: AmlConfig,
    fees: FeeConfig,
    kyc_upload_daily_max: i64,
) -> axum::Router {
    let ledger = PostgresLedger::new(pool);
    build_router(AppState {
        ledger,
        auth: AuthConfig::default(),
        rate_limit,
        // Generous so ordinary tests never trip the per-account throttle.
        login_limit: RateLimitState::new(10_000, std::time::Duration::from_secs(60)),
        aml,
        fees,
        // Tests drive the limiter via X-Forwarded-For over `oneshot` (no socket).
        trust_proxy: true,
        document_dir: std::env::temp_dir().join("payment-kyc-docs-test"),
        kyc_upload_daily_max,
    })
}

/// Standard app with a generous rate limit. Returns the router and the pool (so
/// tests can promote a user to admin directly).
async fn app() -> (axum::Router, PgPool) {
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    (router_with(pool.clone(), RateLimitState::default()), pool)
}

/// Register a user and promote them to admin (via direct DB write, as ops would).
async fn admin_token(app: &axum::Router, pool: &PgPool) -> String {
    let (uid, tok) = register(app).await;
    sqlx::query("UPDATE users SET is_admin = true WHERE id = $1")
        .bind(Uuid::parse_str(&uid).unwrap())
        .execute(pool)
        .await
        .unwrap();
    tok
}

/// Mark a user KYC-verified (level 1) directly, as an approved review would.
async fn verify_kyc(pool: &PgPool, user_id: &str) {
    sqlx::query("UPDATE users SET kyc_level = 1 WHERE id = $1")
        .bind(Uuid::parse_str(user_id).unwrap())
        .execute(pool)
        .await
        .unwrap();
}

/// Deposit `amount_minor` into `account`, authorised by an admin token.
async fn admin_deposit(app: &axum::Router, admin_tok: &str, account: &str, amount_minor: i64) {
    let (status, body) = send(
        app,
        req(
            "POST",
            "/v1/deposits",
            Some(admin_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"user_account": account, "amount_minor": amount_minor}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "admin deposit: {body}");
}

async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
    let resp = app.clone().oneshot(req).await.unwrap();
    let status = resp.status();
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

/// Build a request. `auth` adds a Bearer header; `key` adds an Idempotency-Key.
fn req(
    method: &str,
    uri: &str,
    auth: Option<&str>,
    key: Option<&str>,
    body: Value,
) -> Request<Body> {
    let mut b = Request::builder()
        .method(method)
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(t) = auth {
        b = b.header("authorization", format!("Bearer {t}"));
    }
    if let Some(k) = key {
        b = b.header("idempotency-key", k);
    }
    let body = if body.is_null() {
        Body::empty()
    } else {
        Body::from(serde_json::to_vec(&body).unwrap())
    };
    b.body(body).unwrap()
}

/// Register a fresh user; returns (user_id, access_token).
async fn register(app: &axum::Router) -> (String, String) {
    let phone = format!("+992{:09}", Uuid::new_v4().as_u128() % 1_000_000_000);
    let (status, body) = send(
        app,
        req(
            "POST",
            "/v1/auth/register",
            None,
            None,
            json!({"phone": phone, "password": "password123"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register: {body}");
    (
        body["user_id"].as_str().unwrap().to_string(),
        body["access_token"].as_str().unwrap().to_string(),
    )
}

/// The user's TJS wallet — auto-created at registration, so this reads the
/// wallet list rather than opening a second one.
async fn create_wallet(app: &axum::Router, token: &str) -> String {
    let (status, body) = send(
        app,
        req("GET", "/v1/wallets", Some(token), None, Value::Null),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "list wallets: {body}");
    body.as_array()
        .and_then(|w| w.first())
        .and_then(|w| w["id"].as_str())
        .expect("registration must auto-create a TJS wallet")
        .to_string()
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn full_authenticated_payment_flow() {
    let (app, pool) = app().await;

    let (alice_id, alice_tok) = register(&app).await;
    let (_bob_id, bob_tok) = register(&app).await;
    let alice = create_wallet(&app, &alice_tok).await;
    let bob = create_wallet(&app, &bob_tok).await;

    // Alice must be KYC-verified to send.
    verify_kyc(&pool, &alice_id).await;

    // An admin funds Alice's wallet with 100.00 (deposits are admin-only).
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &alice, 10_000).await;

    // Alice transfers 35.00 to Bob.
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": alice, "to_account": bob, "amount_minor": 3_500}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "transfer: {body}");

    // Balances reflect it (each reads their own).
    let (_, a_bal) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{alice}/balance"),
            Some(&alice_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(a_bal["balance_minor"], 6_500);
    assert_eq!(a_bal["display"], "65.00 TJS");
    let (_, b_bal) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{bob}/balance"),
            Some(&bob_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(b_bal["balance_minor"], 3_500);

    // Overdraft → 422 insufficient_funds. (50_000 is under the AML per-tx limit
    // but over Alice's 6_500 balance, so it reaches the ledger's funds check.)
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": alice, "to_account": bob, "amount_minor": 50_000}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "insufficient_funds");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn authorization_is_enforced() {
    let (app, _pool) = app().await;
    let (_alice_id, alice_tok) = register(&app).await;
    let (_bob_id, bob_tok) = register(&app).await;
    let alice = create_wallet(&app, &alice_tok).await;
    let bob = create_wallet(&app, &bob_tok).await;

    // No token → 401.
    let (status, _) = send(&app, req("POST", "/v1/wallets", None, None, json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Bob cannot read Alice's balance → 403.
    let (status, _) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{alice}/balance"),
            Some(&bob_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Bob cannot spend from Alice's wallet → 403 (even before funds are checked).
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&bob_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": alice, "to_account": bob, "amount_minor": 100}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn idempotent_replay_and_conflict() {
    let (app, pool) = app().await;
    let (id, tok) = register(&app).await;
    let (_id2, tok2) = register(&app).await;
    let alice = create_wallet(&app, &tok).await;
    let bob = create_wallet(&app, &tok2).await;
    verify_kyc(&pool, &id).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &alice, 10_000).await;

    let key = Uuid::new_v4().to_string();
    let body = json!({"from_account": alice, "to_account": bob, "amount_minor": 2_000});

    let (s1, first) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&tok),
            Some(&key),
            body.clone(),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED);
    let (s2, replay) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&tok),
            Some(&key),
            body.clone(),
        ),
    )
    .await;
    assert_eq!(s2, StatusCode::CREATED);
    assert_eq!(first["transaction_id"], replay["transaction_id"]);

    // Same key, different body → 409.
    let (s3, conflict) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&tok),
            Some(&key),
            json!({"from_account": alice, "to_account": bob, "amount_minor": 9_999}),
        ),
    )
    .await;
    assert_eq!(s3, StatusCode::CONFLICT);
    assert_eq!(conflict["error"]["code"], "idempotency_conflict");

    // Only one 20.00 transfer applied.
    let (_, a_bal) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{alice}/balance"),
            Some(&tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(a_bal["balance_minor"], 8_000);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn login_and_refresh_flow() {
    let (app, _pool) = app().await;
    let phone = format!("+992{:09}", Uuid::new_v4().as_u128() % 1_000_000_000);

    // Register.
    let (status, reg) = send(
        &app,
        req(
            "POST",
            "/v1/auth/register",
            None,
            None,
            json!({"phone": phone, "password": "password123"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let refresh_token = reg["refresh_token"].as_str().unwrap().to_string();

    // Wrong password → 401.
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/auth/login",
            None,
            None,
            json!({"phone": phone, "password": "wrong"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

    // Correct login → 200 with tokens.
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/auth/login",
            None,
            None,
            json!({"phone": phone, "password": "password123"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    // Refresh rotates: the old token works once, then is revoked.
    let (status, refreshed) = send(
        &app,
        req(
            "POST",
            "/v1/auth/refresh",
            None,
            None,
            json!({"refresh_token": refresh_token}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(refreshed["access_token"].is_string());

    // Reusing the now-rotated (revoked) refresh token → 401.
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/auth/refresh",
            None,
            None,
            json!({"refresh_token": refresh_token}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn deposits_require_admin() {
    let (app, pool) = app().await;
    let (_id, user_tok) = register(&app).await;
    let wallet = create_wallet(&app, &user_tok).await;

    // A normal user cannot deposit (fund) — 403.
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/deposits",
            Some(&user_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"user_account": wallet, "amount_minor": 5_000}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "non-admin deposit: {body}");
    assert_eq!(body["error"]["code"], "forbidden");

    // An admin can fund the same wallet.
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &wallet, 5_000).await;
    let (_, bal) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{wallet}/balance"),
            Some(&user_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(bal["balance_minor"], 5_000);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn rate_limiter_returns_429() {
    // Tight limit: 3 requests per long window, so the 4th from the same client trips.
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    let app = router_with(pool, RateLimitState::new(3, Duration::from_secs(60)));

    let client_ip = "203.0.113.7";
    let mut statuses = Vec::new();
    for _ in 0..4 {
        let request = Request::builder()
            .method("GET")
            .uri("/health")
            .header("x-forwarded-for", client_ip)
            .body(Body::empty())
            .unwrap();
        statuses.push(app.clone().oneshot(request).await.unwrap().status());
    }
    assert_eq!(statuses[0], StatusCode::OK);
    assert_eq!(statuses[2], StatusCode::OK);
    assert_eq!(statuses[3], StatusCode::TOO_MANY_REQUESTS);

    // A different client is unaffected (separate bucket).
    let other = Request::builder()
        .method("GET")
        .uri("/health")
        .header("x-forwarded-for", "198.51.100.9")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(other).await.unwrap().status(),
        StatusCode::OK
    );
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn kyc_gates_transfers_and_supports_review() {
    let (app, pool) = app().await;
    let (_alice_id, alice_tok) = register(&app).await;
    let (_bob_id, bob_tok) = register(&app).await;
    let alice = create_wallet(&app, &alice_tok).await;
    let bob = create_wallet(&app, &bob_tok).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &alice, 10_000).await;

    // Unverified Alice cannot transfer → 403 kyc_required.
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": alice, "to_account": bob, "amount_minor": 1_000}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "kyc_required");

    // Alice submits KYC → pending.
    let submit = json!({
        "requested_level": 1, "full_name": "Alice A",
        "document_type": "passport", "document_ref": "obj://doc/alice"
    });
    let (status, sub) = send(
        &app,
        req(
            "POST",
            "/v1/kyc/submissions",
            Some(&alice_tok),
            None,
            submit.clone(),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let submission_id = sub["id"].as_str().unwrap().to_string();

    // A second pending submission is rejected → 409.
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/kyc/submissions",
            Some(&alice_tok),
            None,
            submit,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    // A normal user cannot approve KYC → 403.
    let (status, _) = send(
        &app,
        req(
            "POST",
            &format!("/v1/kyc/submissions/{submission_id}/approve"),
            Some(&bob_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);

    // Admin approves → Alice is level 1.
    let (status, _) = send(
        &app,
        req(
            "POST",
            &format!("/v1/kyc/submissions/{submission_id}/approve"),
            Some(&admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);

    let (_, kyc) = send(
        &app,
        req("GET", "/v1/kyc", Some(&alice_tok), None, Value::Null),
    )
    .await;
    assert_eq!(kyc["kyc_level"], 1);
    assert_eq!(kyc["latest_submission"]["status"], "approved");

    // Now Alice's transfer succeeds.
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": alice, "to_account": bob, "amount_minor": 1_000}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Rejection path: a new user's submission can be rejected, leaving them unverified.
    let (_carol_id, carol_tok) = register(&app).await;
    let (_, csub) = send(
        &app,
        req(
            "POST",
            "/v1/kyc/submissions",
            Some(&carol_tok),
            None,
            json!({"requested_level": 1, "full_name": "Carol C",
                   "document_type": "id_card", "document_ref": "obj://doc/carol"}),
        ),
    )
    .await;
    let csub_id = csub["id"].as_str().unwrap().to_string();
    let (status, _) = send(
        &app,
        req(
            "POST",
            &format!("/v1/kyc/submissions/{csub_id}/reject"),
            Some(&admin),
            None,
            json!({"reason": "document unreadable"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let (_, ckyc) = send(
        &app,
        req("GET", "/v1/kyc", Some(&carol_tok), None, Value::Null),
    )
    .await;
    assert_eq!(ckyc["kyc_level"], 0);
    assert_eq!(ckyc["latest_submission"]["status"], "rejected");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn aml_blocklist_enforced() {
    let (app, pool) = app().await;
    let (alice_id, alice_tok) = register(&app).await;
    let (bob_id, bob_tok) = register(&app).await;
    let alice = create_wallet(&app, &alice_tok).await;
    let bob = create_wallet(&app, &bob_tok).await;
    verify_kyc(&pool, &alice_id).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &alice, 100_000).await;

    let xfer =
        |amount: i64| json!({"from_account": alice, "to_account": bob, "amount_minor": amount});

    // Block the recipient → sender's transfer is refused.
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/admin/blocks",
            Some(&admin),
            None,
            json!({"user_id": bob_id, "reason": "sanctions match"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            xfer(1_000),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "account_blocked");

    // Unblock the recipient → transfer succeeds.
    let (status, _) = send(
        &app,
        req(
            "DELETE",
            &format!("/v1/admin/blocks/{bob_id}"),
            Some(&admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            xfer(1_000),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Block the sender → their transfers are refused too.
    send(
        &app,
        req(
            "POST",
            "/v1/admin/blocks",
            Some(&admin),
            None,
            json!({"user_id": alice_id, "reason": "fraud review"}),
        ),
    )
    .await;
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            xfer(1_000),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN);
    assert_eq!(body["error"]["code"], "account_blocked");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn aml_limits_and_velocity_enforced() {
    // Tight limits so small amounts exercise the rules.
    let tight = AmlConfig {
        level1: Limits {
            per_tx_minor: 1_000,
            daily_minor: 1_500,
            velocity_per_hour: 2,
        },
        level2: Limits {
            per_tx_minor: 1_000,
            daily_minor: 1_500,
            velocity_per_hour: 2,
        },
    };
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    let app = router_with_aml(pool.clone(), RateLimitState::default(), tight);

    let (alice_id, alice_tok) = register(&app).await;
    let (_bob_id, bob_tok) = register(&app).await;
    let alice = create_wallet(&app, &alice_tok).await;
    let bob = create_wallet(&app, &bob_tok).await;
    verify_kyc(&pool, &alice_id).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &alice, 100_000).await;

    let xfer =
        |amount: i64| json!({"from_account": alice, "to_account": bob, "amount_minor": amount});
    let post = |amount: i64, tok: &str| {
        req(
            "POST",
            "/v1/transfers",
            Some(tok),
            Some(&Uuid::new_v4().to_string()),
            xfer(amount),
        )
    };

    // Over the per-transaction limit → 422 limit_exceeded.
    let (status, body) = send(&app, post(2_000, &alice_tok)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "limit_exceeded");

    // Two transfers within limits succeed (velocity budget = 2/hour).
    assert_eq!(
        send(&app, post(500, &alice_tok)).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        send(&app, post(600, &alice_tok)).await.0,
        StatusCode::CREATED
    );

    // Third trips velocity (or daily) → 422.
    let (status, body) = send(&app, post(100, &alice_tok)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "limit_exceeded");
}

#[tokio::test]
#[ignore = "requires PostgreSQL and Redis (docker compose up -d)"]
async fn redis_rate_limiter_returns_429() {
    let redis_url = std::env::var("REDIS_URL").unwrap_or_else(|_| "redis://localhost:6379".into());
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    let redis_pool = deadpool_redis::Config::from_url(redis_url)
        .create_pool(Some(deadpool_redis::Runtime::Tokio1))
        .unwrap();

    // 3 requests per window, backed by Redis.
    let app = router_with(
        pool,
        RateLimitState::redis(redis_pool, 3, Duration::from_secs(60)),
    );

    // Unique client key so the test is independent of previous runs.
    let client = format!("test-{}", Uuid::new_v4());
    let mut statuses = Vec::new();
    for _ in 0..4 {
        let request = Request::builder()
            .method("GET")
            .uri("/health")
            .header("x-forwarded-for", &client)
            .body(Body::empty())
            .unwrap();
        statuses.push(app.clone().oneshot(request).await.unwrap().status());
    }
    assert_eq!(statuses[0], StatusCode::OK);
    assert_eq!(statuses[2], StatusCode::OK);
    assert_eq!(statuses[3], StatusCode::TOO_MANY_REQUESTS);
}

/// Read a system account's raw (signed) balance directly, for assertions on
/// accounts the API won't expose to a normal user (e.g. the fee-revenue account).
async fn raw_balance(pool: &PgPool, account_id: Uuid) -> i64 {
    sqlx::query_scalar("SELECT raw_minor FROM balances WHERE account_id = $1")
        .bind(account_id)
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn transfer_fee_is_charged_as_three_entries() {
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    // 1% transfer fee.
    let app = router_full(
        pool.clone(),
        RateLimitState::default(),
        AmlConfig::default(),
        FeeConfig { transfer_bps: 100 },
    );

    let (alice_id, alice_tok) = register(&app).await;
    let (_bob_id, bob_tok) = register(&app).await;
    let alice = create_wallet(&app, &alice_tok).await;
    let bob = create_wallet(&app, &bob_tok).await;
    verify_kyc(&pool, &alice_id).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &alice, 100_000).await;

    // The fee-revenue account (Uuid::from_u128(2)) is shared, so measure a delta.
    let fee_account = Uuid::from_u128(2);
    let fee_before = raw_balance(&pool, fee_account).await;

    // Transfer 10_000 (100 TJS) → 1% fee = 100.
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": alice, "to_account": bob, "amount_minor": 10_000}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    // Sender debited the full amount; recipient receives amount − fee.
    let (_, a_bal) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{alice}/balance"),
            Some(&alice_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    let (_, b_bal) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{bob}/balance"),
            Some(&bob_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(a_bal["balance_minor"], 90_000); // 100_000 − 10_000
    assert_eq!(b_bal["balance_minor"], 9_900); //  10_000 − 100 fee

    // The fee-revenue account gained exactly the fee.
    let fee_after = raw_balance(&pool, fee_account).await;
    assert_eq!(fee_after - fee_before, 100);
}

async fn create_wallet_cur(app: &axum::Router, token: &str, currency: &str) -> String {
    let (status, body) = send(
        app,
        req(
            "POST",
            "/v1/wallets",
            Some(token),
            None,
            json!({"currency": currency}),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::CREATED,
        "create {currency} wallet: {body}"
    );
    body["id"].as_str().unwrap().to_string()
}

/// `fx_rates` is one global table and tests run in parallel: every test that
/// WRITES the TJS→USD rate must hold this lock, or two of them interleave and
/// one asserts against the other's rate.
static FX_RATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn fx_conversion_balances_both_legs() {
    let _fx = FX_RATE_LOCK.lock().await;
    let (app, pool) = app().await;
    let (uid, tok) = register(&app).await;
    let tjs = create_wallet(&app, &tok).await; // default TJS
    let usd = create_wallet_cur(&app, &tok, "USD").await;
    verify_kyc(&pool, &uid).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &tjs, 10_000).await; // 100.00 TJS

    // Admin sets TJS->USD: 1 TJS-minor = 0.1 USD-minor (num=1, den=10).
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/admin/fx-rates",
            Some(&admin),
            None,
            json!({"base": "TJS", "quote": "USD", "rate_num": 1, "rate_den": 10}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT);

    // FX-position accounts are shared/persistent — measure deltas.
    let fx_tjs_before = raw_balance(&pool, Uuid::from_u128(3)).await;
    let fx_usd_before = raw_balance(&pool, Uuid::from_u128(4)).await;

    // Convert 10_000 TJS-minor → 1_000 USD-minor.
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/fx",
            Some(&tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": tjs, "to_account": usd, "amount_minor": 10_000}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "fx: {body}");
    assert_eq!(body["debited_minor"], 10_000);
    assert_eq!(body["credited_minor"], 1_000);
    assert_eq!(body["from_currency"], "TJS");
    assert_eq!(body["to_currency"], "USD");

    // Balances: TJS drained, USD funded.
    let (_, tjs_bal) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{tjs}/balance"),
            Some(&tok),
            None,
            Value::Null,
        ),
    )
    .await;
    let (_, usd_bal) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{usd}/balance"),
            Some(&tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(tjs_bal["balance_minor"], 0);
    assert_eq!(usd_bal["balance_minor"], 1_000);
    assert_eq!(usd_bal["display"], "10.00 USD");

    // Each currency leg balanced to zero: the platform FX positions mirror the user.
    // TJS fx position gained the 10_000 the user spent; USD fx position went short 1_000.
    assert_eq!(
        raw_balance(&pool, Uuid::from_u128(3)).await - fx_tjs_before,
        10_000
    );
    assert_eq!(
        raw_balance(&pool, Uuid::from_u128(4)).await - fx_usd_before,
        -1_000
    );

    // No same-currency FX.
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/fx",
            Some(&tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": tjs, "to_account": tjs, "amount_minor": 1}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
}

/// Regression (audit 2026-07-03): a retry of an already-posted transfer must
/// replay the stored response, even when the original transfer consumed the
/// AML budget that a fresh screening would now fail — a lost-response retry
/// must never be told "limit exceeded" for money that actually moved.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn retry_of_posted_transfer_replays_despite_consumed_limits() {
    let tight = AmlConfig {
        level1: Limits {
            per_tx_minor: 1_000,
            daily_minor: 1_500,
            velocity_per_hour: 10,
        },
        level2: Limits {
            per_tx_minor: 1_000,
            daily_minor: 1_500,
            velocity_per_hour: 10,
        },
    };
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    let app = router_with_aml(pool.clone(), RateLimitState::default(), tight);

    let (alice_id, alice_tok) = register(&app).await;
    let (_bob_id, _bob_tok) = register(&app).await;
    let alice = create_wallet(&app, &alice_tok).await;
    let bob = create_wallet(&app, &_bob_tok).await;
    verify_kyc(&pool, &alice_id).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &alice, 10_000).await;

    let key = Uuid::new_v4().to_string();
    let xfer = json!({"from_account": alice, "to_account": bob, "amount_minor": 1_000});

    // Post the transfer; it consumes 1_000 of the 1_500 daily budget.
    let (s1, first) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&key),
            xfer.clone(),
        ),
    )
    .await;
    assert_eq!(s1, StatusCode::CREATED, "first post: {first}");

    // A DIFFERENT transfer of the same size would now blow the daily limit.
    let (s2, blocked) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            xfer.clone(),
        ),
    )
    .await;
    assert_eq!(
        s2,
        StatusCode::UNPROCESSABLE_ENTITY,
        "fresh post: {blocked}"
    );
    assert_eq!(blocked["error"]["code"], "limit_exceeded");

    // But the RETRY of the first (same key, same body) must replay the stored
    // 201 — not be re-screened into a bogus 422.
    let (s3, replay) = send(
        &app,
        req("POST", "/v1/transfers", Some(&alice_tok), Some(&key), xfer),
    )
    .await;
    assert_eq!(s3, StatusCode::CREATED, "replay: {replay}");
    assert_eq!(replay["transaction_id"], first["transaction_id"]);

    // And the money moved exactly once.
    let (_, bal) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{alice}/balance"),
            Some(&alice_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(bal["balance_minor"], 9_000);
}

/// Regression (audit 2026-07-03): system accounts have well-known ids; money
/// moved into them is unrecoverable, so transfers and deposits must refuse any
/// target that is not a user wallet.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn system_accounts_are_not_valid_targets() {
    let (app, pool) = app().await;
    let (alice_id, alice_tok) = register(&app).await;
    let alice = create_wallet(&app, &alice_tok).await;
    verify_kyc(&pool, &alice_id).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &alice, 10_000).await;

    // The seeded fee-revenue account.
    let fee_account = "00000000-0000-0000-0000-000000000002";

    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": alice, "to_account": fee_account, "amount_minor": 100}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "transfer: {body}");

    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/deposits",
            Some(&admin),
            Some(&Uuid::new_v4().to_string()),
            json!({"user_account": fee_account, "amount_minor": 100}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "deposit: {body}");
}

/// Phase A (console/app support): wallet list + keyset-paginated statement.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn wallet_list_and_statement_pagination() {
    let (app, pool) = app().await;
    let (alice_id, alice_tok) = register(&app).await;
    let (_bob_id, bob_tok) = register(&app).await;
    let alice = create_wallet(&app, &alice_tok).await;
    let bob = create_wallet(&app, &bob_tok).await;
    verify_kyc(&pool, &alice_id).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &alice, 5_000).await;

    // One outgoing transfer so the statement has two entries (credit + debit).
    let (s, b) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": alice, "to_account": bob, "amount_minor": 1_000}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "transfer: {b}");

    // Wallet list shows the wallet with its post-transfer balance.
    let (s, wallets) = send(
        &app,
        req("GET", "/v1/wallets", Some(&alice_tok), None, Value::Null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "wallets: {wallets}");
    let list = wallets.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], alice);
    assert_eq!(list[0]["balance_minor"], 4_000);
    assert_eq!(list[0]["display"], "40.00 TJS");

    // Statement, one entry per page: newest first (the transfer debit)...
    let (s, page1) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{alice}/transactions?limit=1"),
            Some(&alice_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "page1: {page1}");
    assert_eq!(page1["entries"][0]["direction"], "debit");
    assert_eq!(page1["entries"][0]["amount_minor"], 1_000);
    let cursor = page1["next_cursor"].as_str().unwrap().to_string();

    // ...then the older deposit credit on page 2.
    let encoded = cursor
        .replace('+', "%2B")
        .replace(' ', "%20")
        .replace('|', "%7C");
    let (s, page2) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{alice}/transactions?limit=1&cursor={encoded}"),
            Some(&alice_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "page2: {page2}");
    assert_eq!(page2["entries"][0]["direction"], "credit");
    assert_eq!(page2["entries"][0]["amount_minor"], 5_000);

    // Someone else's statement is forbidden.
    let (s, _) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{alice}/transactions"),
            Some(&bob_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);

    // A garbage cursor is a 400, not a 500.
    let (s, _) = send(
        &app,
        req(
            "GET",
            &format!(
                "/v1/accounts/{alice}/transactions?cursor=garbage%7C{}",
                Uuid::new_v4()
            ),
            Some(&alice_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

fn multipart_request(uri: &str, token: &str, content_type: &str, payload: &[u8]) -> Request<Body> {
    let boundary = "phase-a-test-boundary";
    let mut body = Vec::new();
    body.extend_from_slice(format!("--{boundary}\r\n").as_bytes());
    body.extend_from_slice(
        format!(
            "content-disposition: form-data; name=\"file\"; filename=\"doc\"\r\ncontent-type: {content_type}\r\n\r\n"
        )
        .as_bytes(),
    );
    body.extend_from_slice(payload);
    body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    Request::builder()
        .method("POST")
        .uri(uri)
        .header(
            "content-type",
            format!("multipart/form-data; boundary={boundary}"),
        )
        .header("authorization", format!("Bearer {token}"))
        .body(Body::from(body))
        .unwrap()
}

/// Phase A: KYC document upload, admin-only retrieval, and the review queue.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn kyc_documents_and_admin_review_queue() {
    let (app, pool) = app().await;
    let (_uid, tok) = register(&app).await;
    let admin = admin_token(&app, &pool).await;

    // Upload a (fake) PNG.
    let payload = b"\x89PNG-not-really-but-bytes";
    let (s, up) = send(
        &app,
        multipart_request("/v1/kyc/documents", &tok, "image/png", payload),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "upload: {up}");
    let doc_ref = up["document_ref"].as_str().unwrap().to_string();
    assert!(doc_ref.ends_with(".png"));

    // Unsupported content type → 400.
    let (s, _) = send(
        &app,
        multipart_request("/v1/kyc/documents", &tok, "text/html", b"nope"),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // The uploader cannot read documents back; an admin can, byte-exact.
    let (s, _) = send(
        &app,
        req(
            "GET",
            &format!("/v1/admin/kyc/documents/{doc_ref}"),
            Some(&tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let resp = app
        .clone()
        .oneshot(req(
            "GET",
            &format!("/v1/admin/kyc/documents/{doc_ref}"),
            Some(&admin),
            None,
            Value::Null,
        ))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["content-type"], "image/png");
    let got = resp.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(got.as_ref(), payload);

    // Path traversal shapes are rejected outright.
    let (s, _) = send(
        &app,
        req(
            "GET",
            "/v1/admin/kyc/documents/..%2F..%2Fetc%2Fpasswd",
            Some(&admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

    // Submit KYC citing the uploaded ref; it appears in the pending queue.
    let (s, sub) = send(
        &app,
        req(
            "POST",
            "/v1/kyc/submissions",
            Some(&tok),
            None,
            json!({"requested_level": 1, "full_name": "Console Test", "document_type": "passport", "document_ref": doc_ref}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "submit: {sub}");
    let (s, queue) = send(
        &app,
        req(
            "GET",
            "/v1/admin/kyc/submissions",
            Some(&admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "queue: {queue}");
    let found =
        queue.as_array().unwrap().iter().any(|it| {
            it["id"] == sub["id"] && it["document_ref"].as_str() == Some(doc_ref.as_str())
        });
    assert!(found, "submission missing from pending queue: {queue}");

    // Non-admins cannot list the queue.
    let (s, _) = send(
        &app,
        req(
            "GET",
            "/v1/admin/kyc/submissions",
            Some(&tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
}

/// Phase A: admin user lookup, integrity status, and the FX rate list.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn admin_lookup_status_and_fx_rates() {
    let _fx = FX_RATE_LOCK.lock().await;
    let (app, pool) = app().await;
    let phone = format!("+992{:09}", Uuid::new_v4().as_u128() % 1_000_000_000);
    let (s, reg) = send(
        &app,
        req(
            "POST",
            "/v1/auth/register",
            None,
            None,
            json!({"phone": phone, "password": "password123"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED);
    let tok = reg["access_token"].as_str().unwrap().to_string();
    let wallet = create_wallet(&app, &tok).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &wallet, 2_500).await;

    // Lookup by phone (with separators, exercising normalization).
    let spaced = format!("{} {}", &phone[..4], &phone[4..]).replace(' ', "%20");
    let (s, user) = send(
        &app,
        req(
            "GET",
            &format!("/v1/admin/users?phone={spaced}"),
            Some(&admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "lookup: {user}");
    assert_eq!(user["phone"], phone.trim_start_matches('+'));
    assert_eq!(user["wallets"][0]["balance_minor"], 2_500);
    assert!(user["blocked_reason"].is_null());

    // Status: conservation must be exactly zero in every currency.
    let (s, status) = send(
        &app,
        req("GET", "/v1/admin/status", Some(&admin), None, Value::Null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "status: {status}");
    for c in status["conservation"].as_array().unwrap() {
        assert_eq!(c["net_minor"], 0, "conservation broken: {status}");
    }

    // FX rates: set one as admin, read it back as a normal user.
    let (s, _) = send(
        &app,
        req(
            "POST",
            "/v1/admin/fx-rates",
            Some(&admin),
            None,
            json!({"base": "TJS", "quote": "USD", "rate_num": 917, "rate_den": 10000}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, rates) = send(
        &app,
        req("GET", "/v1/fx/rates", Some(&tok), None, Value::Null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "rates: {rates}");
    let found = rates
        .as_array()
        .unwrap()
        .iter()
        .any(|r| r["base"] == "TJS" && r["quote"] == "USD" && r["rate_num"] == 917);
    assert!(found, "rate missing: {rates}");
}

/// Audit 2026-07-08: freezing an account must end its sessions — refresh is
/// status-gated and a freeze revokes the whole token family.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn frozen_account_cannot_refresh_or_login() {
    let (app, pool) = app().await;

    let phone = format!("992{:09}", Uuid::new_v4().as_u128() % 1_000_000_000);
    let (s, reg) = send(
        &app,
        req(
            "POST",
            "/v1/auth/register",
            None,
            None,
            json!({"phone": phone, "password": "password123"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "register: {reg}");
    let uid = reg["user_id"].as_str().unwrap().to_string();
    let user_tok = reg["access_token"].as_str().unwrap().to_string();
    let refresh = reg["refresh_token"].as_str().unwrap().to_string();

    let admin = admin_token(&app, &pool).await;
    let status_uri = format!("/v1/admin/users/{uid}/status");

    // Only admins may change a status, and only to a known one.
    let (s, _) = send(
        &app,
        req(
            "POST",
            &status_uri,
            Some(&user_tok),
            None,
            json!({"status": "frozen"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN);
    let (s, _) = send(
        &app,
        req(
            "POST",
            &status_uri,
            Some(&admin),
            None,
            json!({"status": "banished"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let (s, _) = send(
        &app,
        req(
            "POST",
            &format!("/v1/admin/users/{}/status", Uuid::new_v4()),
            Some(&admin),
            None,
            json!({"status": "frozen"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND);

    // Freeze: the pre-freeze refresh token is revoked, and login is refused.
    let (s, _) = send(
        &app,
        req(
            "POST",
            &status_uri,
            Some(&admin),
            None,
            json!({"status": "frozen"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, body) = send(
        &app,
        req(
            "POST",
            "/v1/auth/refresh",
            None,
            None,
            json!({"refresh_token": refresh}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED, "revoked at freeze: {body}");
    let (s, body) = send(
        &app,
        req(
            "POST",
            "/v1/auth/login",
            None,
            None,
            json!({"phone": phone, "password": "password123"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "frozen login: {body}");

    // Reactivate, log back in, then freeze via a path that does NOT revoke
    // tokens (a direct DB write): refresh alone must still refuse.
    let (s, _) = send(
        &app,
        req(
            "POST",
            &status_uri,
            Some(&admin),
            None,
            json!({"status": "active"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT);
    let (s, login) = send(
        &app,
        req(
            "POST",
            "/v1/auth/login",
            None,
            None,
            json!({"phone": phone, "password": "password123"}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "reactivated login: {login}");
    let live_refresh = login["refresh_token"].as_str().unwrap().to_string();

    sqlx::query("UPDATE users SET status = 'frozen' WHERE id = $1")
        .bind(Uuid::parse_str(&uid).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let (s, body) = send(
        &app,
        req(
            "POST",
            "/v1/auth/refresh",
            None,
            None,
            json!({"refresh_token": live_refresh}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "status-gated refresh: {body}");
    assert_eq!(body["error"]["code"], "forbidden");
}

/// Audit 2026-07-08: uploads are quota'd per user, so one registered account
/// cannot fill the document volume at the global rate limit.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn kyc_upload_quota_enforced() {
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    let app = router_full_quota(
        pool,
        RateLimitState::default(),
        AmlConfig::default(),
        FeeConfig::default(),
        2,
    );
    let (_uid, tok) = register(&app).await;

    for i in 0..2 {
        let (s, body) = send(
            &app,
            multipart_request("/v1/kyc/documents", &tok, "image/png", b"png-bytes"),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "upload {i}: {body}");
    }
    let (s, body) = send(
        &app,
        multipart_request("/v1/kyc/documents", &tok, "image/png", b"png-bytes"),
    )
    .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "over quota: {body}");
    assert_eq!(body["error"]["code"], "rate_limited");
}

/// Audit 2026-07-08: a self-transfer moves nothing but would consume AML
/// budget (and pay a fee, if configured) — reject it outright.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn self_transfer_is_rejected() {
    let (app, pool) = app().await;
    let (uid, tok) = register(&app).await;
    verify_kyc(&pool, &uid).await;
    let wallet = create_wallet(&app, &tok).await;

    let (s, body) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": wallet, "to_account": wallet, "amount_minor": 100}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "self-transfer: {body}");
    assert_eq!(body["error"]["code"], "bad_request");
}

/// Console overhaul 2026-07-09: dashboard metrics + browsable user directory.
#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn admin_metrics_and_user_list() {
    let (app, pool) = app().await;
    let (uid, tok) = register(&app).await;
    verify_kyc(&pool, &uid).await;
    let wallet = create_wallet(&app, &tok).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &wallet, 12_345).await;

    // Both endpoints are admin-only.
    for uri in ["/v1/admin/metrics", "/v1/admin/users/list"] {
        let (s, _) = send(&app, req("GET", uri, Some(&tok), None, Value::Null)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri} must be admin-only");
    }

    // Metrics: sane shape, and today's deposit is visible in the aggregates.
    let (s, m) = send(
        &app,
        req("GET", "/v1/admin/metrics", Some(&admin), None, Value::Null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "metrics: {m}");
    let today = chrono_like_today();
    let vol_today: i64 = m["daily_volume"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|v| v["date"] == today.as_str() && v["currency"] == "TJS")
        .map(|v| v["volume_minor"].as_i64().unwrap())
        .sum();
    assert!(vol_today >= 12_345, "today's TJS volume: {m}");
    let deposits = m["mix_30d"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["kind"] == "deposit")
        .map(|v| v["count"].as_i64().unwrap())
        .unwrap_or(0);
    assert!(deposits >= 1, "mix must count the deposit: {m}");
    let tjs_funds = m["customer_funds"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["currency"] == "TJS")
        .map(|v| v["total_minor"].as_i64().unwrap())
        .unwrap_or(0);
    assert!(tjs_funds >= 12_345, "customer funds: {m}");
    assert!(m["users"]["total"].as_i64().unwrap() >= 2);
    assert!(m["kyc"]["pending"].is_i64() && m["aml_blocked_30d"].is_i64());

    // User list: newest-first browse pages by cursor…
    let (s, page) = send(
        &app,
        req(
            "GET",
            "/v1/admin/users/list?limit=1",
            Some(&admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "list: {page}");
    assert_eq!(page["users"].as_array().unwrap().len(), 1);
    let cursor = page["next_cursor"].as_str().unwrap().to_string();
    let (s, page2) = send(
        &app,
        req(
            "GET",
            &format!("/v1/admin/users/list?limit=1&cursor={}", urlencode(&cursor)),
            Some(&admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "page2: {page2}");
    assert_ne!(
        page["users"][0]["id"], page2["users"][0]["id"],
        "cursor must advance"
    );

    // …and prefix search finds the freshly registered user by phone.
    let phone = page_phone(&app, &admin, &uid).await;
    let (s, found) = send(
        &app,
        req(
            "GET",
            &format!("/v1/admin/users/list?q={}", &phone[..9]),
            Some(&admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "search: {found}");
    assert!(
        found["users"]
            .as_array()
            .unwrap()
            .iter()
            .any(|u| u["id"] == uid.as_str()),
        "search by prefix {} must find the user: {found}",
        &phone[..9]
    );
}

/// Today's UTC date in the same YYYY-MM-DD form the metrics endpoint emits.
/// Both this and the container's Postgres bucket in UTC, so they agree even
/// across a local-timezone midnight.
fn chrono_like_today() -> String {
    // std-only: seconds since epoch → civil date (Howard Hinnant's algorithm).
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let z = secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            other => format!("%{other:02X}"),
        })
        .collect()
}

/// Record an approved KYC submission with a verified name, as a real review
/// would — this is the only source of a name the resolve endpoint will reveal.
async fn approve_named_kyc(pool: &PgPool, user_id: &str, full_name: &str) {
    sqlx::query(
        "INSERT INTO kyc_submissions
             (id, user_id, requested_level, full_name, document_type,
              document_ref, status, reviewed_at)
         VALUES ($1, $2, 1, $3, 'passport', 'test-ref', 'approved', now())",
    )
    .bind(Uuid::new_v4())
    .bind(Uuid::parse_str(user_id).unwrap())
    .bind(full_name)
    .execute(pool)
    .await
    .unwrap();
    verify_kyc(pool, user_id).await;
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn resolve_recipient_by_phone_and_wallet() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;

    // Sender must be KYC-1 to use resolve at all.
    let (sender_id, sender_tok) = register(&app).await;

    // Case 1: recipient with an approved KYC name + a TJS wallet.
    let (named_id, named_tok) = register(&app).await;
    let named_wallet = create_wallet(&app, &named_tok).await;
    approve_named_kyc(&pool, &named_id, "Firuz Rahimov").await;
    let named_phone = page_phone(&app, &admin, &named_id).await;

    // Case 2: recipient registered with a TJS wallet but no verified name.
    let (nameless_id, nameless_tok) = register(&app).await;
    let _nameless_wallet = create_wallet(&app, &nameless_tok).await;
    let nameless_phone = page_phone(&app, &admin, &nameless_id).await;

    // Un-verified sender is refused (403 kyc_required) — not an open oracle.
    let (s, body) = send(
        &app,
        req(
            "GET",
            &format!("/v1/users/resolve?phone={}", urlencode(&named_phone)),
            Some(&sender_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::FORBIDDEN, "unverified sender: {body}");
    assert_eq!(body["error"]["code"], "kyc_required");

    verify_kyc(&pool, &sender_id).await;

    // Verified recipient → name revealed, marked verified, wallet returned.
    let (s, body) = send(
        &app,
        req(
            "GET",
            &format!("/v1/users/resolve?phone={}", urlencode(&named_phone)),
            Some(&sender_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "resolve named: {body}");
    assert_eq!(body["name"], "Firuz Rahimov");
    assert_eq!(body["name_verified"], true);
    assert_eq!(body["wallet_id"], named_wallet);
    assert_eq!(body["currency"], "TJS");

    // Registered but unverified recipient → found & sendable, no name.
    let (s, body) = send(
        &app,
        req(
            "GET",
            &format!("/v1/users/resolve?phone={}", urlencode(&nameless_phone)),
            Some(&sender_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "resolve nameless: {body}");
    assert!(body["name"].is_null());
    assert_eq!(body["name_verified"], false);

    // Unknown number → 404 not_found (the "different message" case).
    let (s, body) = send(
        &app,
        req(
            "GET",
            "/v1/users/resolve?phone=992000000000",
            Some(&sender_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "resolve unknown: {body}");
    assert_eq!(body["error"]["code"], "not_found");

    // QR path: resolve the same recipient by wallet id → same result.
    let (s, body) = send(
        &app,
        req(
            "GET",
            &format!("/v1/users/resolve?wallet={named_wallet}"),
            Some(&sender_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "resolve by wallet: {body}");
    assert_eq!(body["name"], "Firuz Rahimov");
    assert_eq!(body["wallet_id"], named_wallet);

    // A system account id is never a valid recipient.
    let system = format!("{}", uuid::Uuid::from_u128(0x1000));
    let (s, _) = send(
        &app,
        req(
            "GET",
            &format!("/v1/users/resolve?wallet={system}"),
            Some(&sender_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NOT_FOUND, "system account must not resolve");

    // Neither key, or both, is a 400.
    let (s, _) = send(
        &app,
        req(
            "GET",
            "/v1/users/resolve",
            Some(&sender_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "no key → 400");
}

/// Fetch a user's phone via the admin lookup-by-id path (through the list).
async fn page_phone(app: &axum::Router, admin: &str, user_id: &str) -> String {
    // The registration helper doesn't expose the phone, so find it by id in
    // a big first page (tests share the DB; the user was just created, so
    // newest-first finds it immediately).
    let (s, page) = send(
        app,
        req(
            "GET",
            "/v1/admin/users/list?limit=100",
            Some(admin),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK);
    page["users"]
        .as_array()
        .unwrap()
        .iter()
        .find(|u| u["id"] == user_id)
        .map(|u| u["phone"].as_str().unwrap().to_string())
        .expect("freshly registered user must be on the first page")
}

/// A user's phone, read straight from the DB (test-only shortcut).
async fn db_phone(pool: &PgPool, user_id: &str) -> String {
    sqlx::query_scalar("SELECT phone FROM users WHERE id = $1")
        .bind(Uuid::parse_str(user_id).unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn statement_is_enriched_with_kind_and_counterparty() {
    let _fx = FX_RATE_LOCK.lock().await;
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;

    let (alice_id, alice_tok) = register(&app).await;
    let alice_wallet = create_wallet(&app, &alice_tok).await;
    approve_named_kyc(&pool, &alice_id, "Alice Statement").await;
    let (bob_id, bob_tok) = register(&app).await;
    let bob_wallet = create_wallet(&app, &bob_tok).await;
    approve_named_kyc(&pool, &bob_id, "Bobojon Qurbonov").await;
    let alice_phone = db_phone(&pool, &alice_id).await;
    let bob_phone = db_phone(&pool, &bob_id).await;

    // Fund Alice, then she pays Bob.
    admin_deposit(&app, &admin, &alice_wallet, 10_000).await;
    let (s, body) = send(
        &app,
        req(
            "POST",
            "/v1/transfers",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({
                "from_account": alice_wallet, "to_account": bob_wallet,
                "amount_minor": 2_500, "currency": "TJS"
            }),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "transfer: {body}");

    // Alice's statement, newest first: [transfer out to Bob, deposit].
    let (s, body) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{alice_wallet}/transactions"),
            Some(&alice_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "alice statement: {body}");
    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries.len(), 2, "alice entries: {body}");
    assert_eq!(entries[0]["kind"], "transfer");
    assert_eq!(entries[0]["direction"], "debit");
    assert_eq!(entries[0]["counterparty_phone"], json!(bob_phone));
    assert_eq!(entries[0]["counterparty_name"], "Bobojon Qurbonov");
    assert_eq!(entries[1]["kind"], "deposit");
    assert_eq!(entries[1]["direction"], "credit");
    // System counterparts expose nothing.
    assert!(entries[1]["counterparty_phone"].is_null());
    assert!(entries[1]["counterparty_name"].is_null());

    // Bob's side of the same transfer: received from Alice, with her identity.
    let (s, body) = send(
        &app,
        req(
            "GET",
            &format!("/v1/accounts/{bob_wallet}/transactions"),
            Some(&bob_tok),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "bob statement: {body}");
    let entries = body["entries"].as_array().unwrap();
    assert_eq!(entries[0]["kind"], "transfer");
    assert_eq!(entries[0]["direction"], "credit");
    assert_eq!(entries[0]["counterparty_phone"], json!(alice_phone));
    assert_eq!(entries[0]["counterparty_name"], "Alice Statement");

    // FX between Alice's own wallets is kind "fx" on both sides, no counterparty.
    let (s, body) = send(
        &app,
        req(
            "POST",
            "/v1/admin/fx-rates",
            Some(&admin),
            None,
            json!({"base": "TJS", "quote": "USD", "rate_num": 917, "rate_den": 10_000}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::NO_CONTENT, "set rate: {body}");
    let alice_usd = create_wallet_cur(&app, &alice_tok, "USD").await;
    let (s, body) = send(
        &app,
        req(
            "POST",
            "/v1/fx",
            Some(&alice_tok),
            Some(&Uuid::new_v4().to_string()),
            json!({"from_account": alice_wallet, "to_account": alice_usd, "amount_minor": 1_000}),
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "fx: {body}");

    for wallet in [&alice_wallet, &alice_usd] {
        let (s, body) = send(
            &app,
            req(
                "GET",
                &format!("/v1/accounts/{wallet}/transactions?limit=1"),
                Some(&alice_tok),
                None,
                Value::Null,
            ),
        )
        .await;
        assert_eq!(s, StatusCode::OK, "fx statement: {body}");
        let e = &body["entries"][0];
        assert_eq!(e["kind"], "fx", "wallet {wallet}: {body}");
        assert!(e["counterparty_phone"].is_null());
        assert!(e["counterparty_name"].is_null());
    }
}
