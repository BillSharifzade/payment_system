use api::{
    build_router, AmlConfig, AppState, AuthConfig, BiometricConfig, FeeConfig, Limits,
    RateLimitState,
};
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

const PNG_BYTES: &[u8] = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR-test-bytes";

fn router_with_auth(pool: PgPool, auth: AuthConfig) -> axum::Router {
    router_custom(
        pool,
        RateLimitState::default(),
        AmlConfig::default(),
        FeeConfig::default(),
        10_000,
        auth,
    )
}

fn router_full_quota(
    pool: PgPool,
    rate_limit: RateLimitState,
    aml: AmlConfig,
    fees: FeeConfig,
    kyc_upload_daily_max: i64,
) -> axum::Router {
    router_custom(
        pool,
        rate_limit,
        aml,
        fees,
        kyc_upload_daily_max,
        AuthConfig::default(),
    )
}

fn router_custom(
    pool: PgPool,
    rate_limit: RateLimitState,
    aml: AmlConfig,
    fees: FeeConfig,
    kyc_upload_daily_max: i64,
    auth: AuthConfig,
) -> axum::Router {
    let ledger = PostgresLedger::new(pool);
    build_router(AppState {
        ledger,
        auth,
        rate_limit,
        login_limit: RateLimitState::new(10_000, std::time::Duration::from_secs(60)),
        resolve_limit: RateLimitState::new(10_000, std::time::Duration::from_secs(60)),
        aml,
        fees,
        biometric: BiometricConfig::dev(),
        trust_proxy: true,
        document_dir: std::env::temp_dir().join("payment-kyc-docs-test"),
        kyc_upload_daily_max,
    })
}

async fn app() -> (axum::Router, PgPool) {
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    (router_with(pool.clone(), RateLimitState::default()), pool)
}

async fn admin_token(app: &axum::Router, pool: &PgPool) -> String {
    let (uid, tok) = register(app).await;
    sqlx::query("UPDATE users SET is_admin = true WHERE id = $1")
        .bind(Uuid::parse_str(&uid).unwrap())
        .execute(pool)
        .await
        .unwrap();
    tok
}

async fn verify_kyc(pool: &PgPool, user_id: &str) {
    sqlx::query("UPDATE users SET kyc_level = 1 WHERE id = $1")
        .bind(Uuid::parse_str(user_id).unwrap())
        .execute(pool)
        .await
        .unwrap();
}

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

    verify_kyc(&pool, &alice_id).await;

    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &alice, 10_000).await;

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

    let (status, _) = send(&app, req("POST", "/v1/wallets", None, None, json!({}))).await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);

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

    let successor = refreshed["refresh_token"].as_str().unwrap().to_string();

    let (status, grace) = send(
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
    assert_eq!(status, StatusCode::OK, "grace reuse: {grace}");
    let grace_token = grace["refresh_token"].as_str().unwrap().to_string();

    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/auth/refresh",
            None,
            None,
            json!({"refresh_token": successor}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/auth/refresh",
            None,
            None,
            json!({"refresh_token": grace_token}),
        ),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::UNAUTHORIZED,
        "family revoked after replay"
    );
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn refresh_replay_without_grace_revokes_family() {
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    let app = router_with_auth(
        pool,
        AuthConfig {
            refresh_reuse_grace_secs: 0,
            ..AuthConfig::default()
        },
    );
    let phone = format!("+992{:09}", Uuid::new_v4().as_u128() % 1_000_000_000);
    let (_, reg) = send(
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
    let first = reg["refresh_token"].as_str().unwrap().to_string();
    let (status, rotated) = send(
        &app,
        req(
            "POST",
            "/v1/auth/refresh",
            None,
            None,
            json!({"refresh_token": first}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    let second = rotated["refresh_token"].as_str().unwrap().to_string();

    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/auth/refresh",
            None,
            None,
            json!({"refresh_token": first}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::UNAUTHORIZED);
    let (status, _) = send(
        &app,
        req(
            "POST",
            "/v1/auth/refresh",
            None,
            None,
            json!({"refresh_token": second}),
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
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    let app = router_with(pool, RateLimitState::new(3, Duration::from_secs(60)));

    let client_ip = "203.0.113.7";
    let mut statuses = Vec::new();
    for _ in 0..4 {
        let request = Request::builder()
            .method("GET")
            .uri("/v1/config")
            .header("x-forwarded-for", client_ip)
            .body(Body::empty())
            .unwrap();
        statuses.push(app.clone().oneshot(request).await.unwrap().status());
    }
    assert_eq!(statuses[0], StatusCode::UNAUTHORIZED);
    assert_eq!(statuses[2], StatusCode::UNAUTHORIZED);
    assert_eq!(statuses[3], StatusCode::TOO_MANY_REQUESTS);

    let other = Request::builder()
        .method("GET")
        .uri("/v1/config")
        .header("x-forwarded-for", "198.51.100.9")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(other).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );

    let probe = Request::builder()
        .method("GET")
        .uri("/ready")
        .header("x-forwarded-for", client_ip)
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(probe).await.unwrap().status(),
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

    let (status, body) = send(&app, post(2_000, &alice_tok)).await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
    assert_eq!(body["error"]["code"], "limit_exceeded");

    assert_eq!(
        send(&app, post(500, &alice_tok)).await.0,
        StatusCode::CREATED
    );
    assert_eq!(
        send(&app, post(600, &alice_tok)).await.0,
        StatusCode::CREATED
    );

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

    let app = router_with(
        pool,
        RateLimitState::redis(redis_pool, 3, Duration::from_secs(60)),
    );

    let client = format!("test-{}", Uuid::new_v4());
    let mut statuses = Vec::new();
    for _ in 0..4 {
        let request = Request::builder()
            .method("GET")
            .uri("/v1/config")
            .header("x-forwarded-for", &client)
            .body(Body::empty())
            .unwrap();
        statuses.push(app.clone().oneshot(request).await.unwrap().status());
    }
    assert_eq!(statuses[0], StatusCode::UNAUTHORIZED);
    assert_eq!(statuses[2], StatusCode::UNAUTHORIZED);
    assert_eq!(statuses[3], StatusCode::TOO_MANY_REQUESTS);
}

async fn system_total(pool: &PgPool, account_type: &str, currency: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COALESCE(SUM(b.raw_minor), 0)::BIGINT
         FROM balances b JOIN accounts a ON a.id = b.account_id
         WHERE a.account_type = $1 AND a.currency = $2",
    )
    .bind(account_type)
    .bind(currency)
    .fetch_one(pool)
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn transfer_fee_is_charged_as_three_entries() {
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
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

    let fee_before = system_total(&pool, "system_fee_revenue", "TJS").await;

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
    assert_eq!(a_bal["balance_minor"], 90_000);
    assert_eq!(b_bal["balance_minor"], 9_900);

    let fee_after = system_total(&pool, "system_fee_revenue", "TJS").await;
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

static FX_RATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn fx_conversion_balances_both_legs() {
    let _fx = FX_RATE_LOCK.lock().await;
    let (app, pool) = app().await;
    let (uid, tok) = register(&app).await;
    let tjs = create_wallet(&app, &tok).await;
    let usd = create_wallet_cur(&app, &tok, "USD").await;
    verify_kyc(&pool, &uid).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &tjs, 10_000).await;

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

    let fx_tjs_before = system_total(&pool, "system_fx_gain_loss", "TJS").await;
    let fx_usd_before = system_total(&pool, "system_fx_gain_loss", "USD").await;

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

    assert_eq!(
        system_total(&pool, "system_fx_gain_loss", "TJS").await - fx_tjs_before,
        10_000
    );
    assert_eq!(
        system_total(&pool, "system_fx_gain_loss", "USD").await - fx_usd_before,
        -1_000
    );

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

    let (s3, replay) = send(
        &app,
        req("POST", "/v1/transfers", Some(&alice_tok), Some(&key), xfer),
    )
    .await;
    assert_eq!(s3, StatusCode::CREATED, "replay: {replay}");
    assert_eq!(replay["transaction_id"], first["transaction_id"]);

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

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn system_accounts_are_not_valid_targets() {
    let (app, pool) = app().await;
    let (alice_id, alice_tok) = register(&app).await;
    let alice = create_wallet(&app, &alice_tok).await;
    verify_kyc(&pool, &alice_id).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &alice, 10_000).await;

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

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn kyc_documents_and_admin_review_queue() {
    let (app, pool) = app().await;
    let (_uid, tok) = register(&app).await;
    let admin = admin_token(&app, &pool).await;

    let payload = b"\x89PNG\r\n\x1a\nnot-really-a-png-but-the-magic-is-right";
    let (s, up) = send(
        &app,
        multipart_request("/v1/kyc/documents", &tok, "image/png", payload),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "upload: {up}");
    let doc_ref = up["document_ref"].as_str().unwrap().to_string();
    assert!(doc_ref.ends_with(".png"));

    let (s, _) = send(
        &app,
        multipart_request("/v1/kyc/documents", &tok, "text/html", b"nope"),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);

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

    let (s, status) = send(
        &app,
        req("GET", "/v1/admin/status", Some(&admin), None, Value::Null),
    )
    .await;
    assert_eq!(s, StatusCode::OK, "status: {status}");
    for c in status["conservation"].as_array().unwrap() {
        assert_eq!(c["net_minor"], 0, "conservation broken: {status}");
    }

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
            multipart_request("/v1/kyc/documents", &tok, "image/png", PNG_BYTES),
        )
        .await;
        assert_eq!(s, StatusCode::CREATED, "upload {i}: {body}");
    }
    let (s, body) = send(
        &app,
        multipart_request("/v1/kyc/documents", &tok, "image/png", PNG_BYTES),
    )
    .await;
    assert_eq!(s, StatusCode::TOO_MANY_REQUESTS, "over quota: {body}");
    assert_eq!(body["error"]["code"], "rate_limited");
}

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

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn admin_metrics_and_user_list() {
    let (app, pool) = app().await;
    let (uid, tok) = register(&app).await;
    verify_kyc(&pool, &uid).await;
    let wallet = create_wallet(&app, &tok).await;
    let admin = admin_token(&app, &pool).await;
    admin_deposit(&app, &admin, &wallet, 12_345).await;

    for uri in ["/v1/admin/metrics", "/v1/admin/users/list"] {
        let (s, _) = send(&app, req("GET", uri, Some(&tok), None, Value::Null)).await;
        assert_eq!(s, StatusCode::FORBIDDEN, "{uri} must be admin-only");
    }

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

fn chrono_like_today() -> String {
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

    let (sender_id, sender_tok) = register(&app).await;

    let (named_id, named_tok) = register(&app).await;
    let named_wallet = create_wallet(&app, &named_tok).await;
    approve_named_kyc(&pool, &named_id, "Firuz Rahimov").await;
    let named_phone = page_phone(&app, &admin, &named_id).await;

    let (nameless_id, nameless_tok) = register(&app).await;
    let _nameless_wallet = create_wallet(&app, &nameless_tok).await;
    let nameless_phone = page_phone(&app, &admin, &nameless_id).await;

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

async fn page_phone(app: &axum::Router, admin: &str, user_id: &str) -> String {
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
    assert!(entries[1]["counterparty_phone"].is_null());
    assert!(entries[1]["counterparty_name"].is_null());

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

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn aml_daily_limit_holds_under_concurrency() {
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    let aml = AmlConfig {
        level1: Limits {
            per_tx_minor: 1_000,
            daily_minor: 5_000,
            velocity_per_hour: 1_000,
        },
        level2: AmlConfig::default().level2,
    };
    let app = router_with_aml(pool.clone(), RateLimitState::default(), aml);
    let admin = admin_token(&app, &pool).await;
    let (alice_id, alice_tok) = register(&app).await;
    let (_bob_id, bob_tok) = register(&app).await;
    verify_kyc(&pool, &alice_id).await;
    let alice = create_wallet(&app, &alice_tok).await;
    let bob = create_wallet(&app, &bob_tok).await;
    admin_deposit(&app, &admin, &alice, 100_000).await;

    let mut handles = Vec::new();
    for _ in 0..12 {
        let app = app.clone();
        let (tok, from, to) = (alice_tok.clone(), alice.clone(), bob.clone());
        handles.push(tokio::spawn(async move {
            send(
                &app,
                req(
                    "POST",
                    "/v1/transfers",
                    Some(&tok),
                    Some(&Uuid::new_v4().to_string()),
                    json!({"from_account": from, "to_account": to, "amount_minor": 1_000}),
                ),
            )
            .await
        }));
    }
    let mut posted = 0;
    let mut limited = 0;
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
    assert_eq!(posted, 5, "exactly floor(5000/1000) transfers may post");
    assert_eq!(limited, 7);

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
    assert_eq!(b_bal["balance_minor"], 5_000);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn frozen_user_cannot_transfer_with_live_token() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let (alice_id, alice_tok) = register(&app).await;
    let (_bob_id, bob_tok) = register(&app).await;
    verify_kyc(&pool, &alice_id).await;
    let alice = create_wallet(&app, &alice_tok).await;
    let bob = create_wallet(&app, &bob_tok).await;
    admin_deposit(&app, &admin, &alice, 10_000).await;

    let (status, _) = send(
        &app,
        req(
            "POST",
            &format!("/v1/admin/users/{alice_id}/status"),
            Some(&admin),
            None,
            json!({"status": "frozen"}),
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
            json!({"from_account": alice, "to_account": bob, "amount_minor": 100}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "forbidden");
    assert!(body["error"]["request_id"].is_string());
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn kyc_upload_rejects_mismatched_content() {
    let (app, _pool) = app().await;
    let (_uid, tok) = register(&app).await;
    let (s, body) = send(
        &app,
        multipart_request(
            "/v1/kyc/documents",
            &tok,
            "image/png",
            b"<html>not a png</html>",
        ),
    )
    .await;
    assert_eq!(s, StatusCode::BAD_REQUEST, "{body}");
    let (s, body) = send(
        &app,
        multipart_request(
            "/v1/kyc/documents",
            &tok,
            "application/pdf",
            b"%PDF-1.4 minimal",
        ),
    )
    .await;
    assert_eq!(s, StatusCode::CREATED, "{body}");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn readiness_request_ids_and_cache_policy() {
    let (app, _pool) = app().await;
    let resp = app
        .clone()
        .oneshot(req("GET", "/ready", None, None, Value::Null))
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-request-id").is_some());
    assert_eq!(resp.headers().get("cache-control").unwrap(), "no-store");

    let mut request = req("GET", "/v1/config", None, None, Value::Null);
    request
        .headers_mut()
        .insert("x-request-id", "abc-123".parse().unwrap());
    let resp = app.clone().oneshot(request).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    assert_eq!(resp.headers().get("x-request-id").unwrap(), "abc-123");
    let bytes = resp.into_body().collect().await.unwrap().to_bytes();
    let body: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(body["error"]["request_id"], "abc-123");
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn usd_deposit_uses_usd_settlement() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let (_uid, tok) = register(&app).await;
    let usd = create_wallet_cur(&app, &tok, "USD").await;
    let before = system_total(&pool, "system_settlement", "USD").await;
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/deposits",
            Some(&admin),
            Some(&Uuid::new_v4().to_string()),
            json!({"user_account": usd, "amount_minor": 2_500, "currency": "USD"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(
        system_total(&pool, "system_settlement", "USD").await - before,
        -2_500
    );
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/deposits",
            Some(&admin),
            Some(&Uuid::new_v4().to_string()),
            json!({"user_account": usd, "amount_minor": 100, "currency": "TJS"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let audited: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM admin_actions WHERE action = 'deposit' AND target = $1",
    )
    .bind(&usd)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(audited, 1);
}

fn template_bytes(tag: Uuid) -> Vec<u8> {
    tag.as_bytes()
        .iter()
        .cycle()
        .take(64)
        .enumerate()
        .map(|(i, b)| b.wrapping_add(i as u8))
        .collect()
}

fn template_b64(tag: Uuid) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(template_bytes(tag))
}

async fn enroll(app: &axum::Router, token: &str, finger: i16, seed: Uuid) -> (StatusCode, Value) {
    send(
        app,
        req(
            "POST",
            "/v1/biometric/fingerprints",
            Some(token),
            None,
            json!({"finger": finger, "format": "raw", "template": template_b64(seed), "consent": true}),
        ),
    )
    .await
}

async fn create_check(
    app: &axum::Router,
    token: &str,
    account: &str,
    amount_minor: i64,
    key: &str,
    extra: Value,
) -> (StatusCode, Value) {
    let mut body = json!({"account": account, "amount_minor": amount_minor});
    if let Value::Object(m) = extra {
        for (k, v) in m {
            body[k] = v;
        }
    }
    send(app, req("POST", "/v1/checks", Some(token), Some(key), body)).await
}

async fn pay_check(
    app: &axum::Router,
    token: &str,
    check_id: &str,
    seed: Uuid,
    key: &str,
) -> (StatusCode, Value) {
    send(
        app,
        req(
            "POST",
            &format!("/v1/checks/{check_id}/pay/fingerprint"),
            Some(token),
            Some(key),
            json!({"format": "raw", "template": template_b64(seed)}),
        ),
    )
    .await
}

async fn balance_of(app: &axum::Router, token: &str, account: &str) -> i64 {
    let (status, body) = send(
        app,
        req(
            "GET",
            &format!("/v1/accounts/{account}/balance"),
            Some(token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "balance: {body}");
    body["balance_minor"].as_i64().unwrap()
}

async fn get_check(app: &axum::Router, token: &str, id: &str) -> (StatusCode, Value) {
    send(
        app,
        req(
            "GET",
            &format!("/v1/checks/{id}"),
            Some(token),
            None,
            Value::Null,
        ),
    )
    .await
}

struct Party {
    id: String,
    token: String,
    wallet: String,
}

async fn party(app: &axum::Router, pool: &PgPool, name: Option<&str>) -> Party {
    let (id, token) = register(app).await;
    match name {
        Some(n) => approve_named_kyc(pool, &id, n).await,
        None => verify_kyc(pool, &id).await,
    }
    let wallet = create_wallet(app, &token).await;
    Party { id, token, wallet }
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn fingerprint_check_payment_flow() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let merchant = party(&app, &pool, Some("Shop Owner")).await;
    let customer = party(&app, &pool, Some("Firuza Karimova")).await;
    admin_deposit(&app, &admin, &customer.wallet, 50_000).await;
    let seed = Uuid::new_v4();

    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/biometric/fingerprints",
            Some(&customer.token),
            None,
            json!({"finger": 2, "format": "raw", "template": template_b64(seed)}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "no consent: {body}");

    let (status, body) = enroll(&app, &customer.token, 2, seed).await;
    assert_eq!(status, StatusCode::CREATED, "enroll: {body}");
    assert_eq!(body["finger"], 2);
    let (status, body) = enroll(&app, &customer.token, 2, seed).await;
    assert_eq!(status, StatusCode::OK, "re-enroll same template: {body}");
    let (status, body) = send(
        &app,
        req(
            "GET",
            "/v1/biometric/fingerprints",
            Some(&customer.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body.as_array().unwrap().len(), 1, "{body}");

    let check_key = Uuid::new_v4().to_string();
    let (status, check) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        20_000,
        &check_key,
        json!({"description": "  2 kg apples "}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "create check: {check}");
    assert_eq!(check["id"], check_key);
    assert_eq!(check["status"], "open");
    assert_eq!(check["description"], "2 kg apples");
    assert!(check["expires_at_ms"].as_i64().unwrap() > check["created_at_ms"].as_i64().unwrap());

    let (status, again) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        20_000,
        &check_key,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "replayed create: {again}");
    assert_eq!(again["id"], check_key);
    let (status, _) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        30_000,
        &check_key,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT);

    let (status, body) = pay_check(
        &app,
        &merchant.token,
        &check_key,
        Uuid::new_v4(),
        &Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "unknown finger: {body}");
    assert_eq!(body["error"]["code"], "no_match");

    let pay_key = Uuid::new_v4().to_string();
    let (status, paid) = pay_check(&app, &merchant.token, &check_key, seed, &pay_key).await;
    assert_eq!(status, StatusCode::CREATED, "pay: {paid}");
    assert_eq!(paid["status"], "posted");
    assert_eq!(paid["transaction_id"], pay_key);
    assert_eq!(paid["check_id"], check_key);
    assert_eq!(paid["amount_minor"], 20_000);
    assert_eq!(paid["currency"], "TJS");
    assert_eq!(paid["payer_name"], "Firuza Karimova");

    assert_eq!(
        balance_of(&app, &customer.token, &customer.wallet).await,
        30_000
    );
    assert_eq!(
        balance_of(&app, &merchant.token, &merchant.wallet).await,
        20_000
    );

    let (status, replay) = pay_check(&app, &merchant.token, &check_key, seed, &pay_key).await;
    assert_eq!(status, StatusCode::CREATED, "replay: {replay}");
    assert_eq!(replay, paid);
    assert_eq!(
        balance_of(&app, &customer.token, &customer.wallet).await,
        30_000
    );

    let (status, body) = pay_check(
        &app,
        &merchant.token,
        &check_key,
        seed,
        &Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "second payment: {body}");

    let (status, body) = get_check(&app, &merchant.token, &check_key).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(body["status"], "paid");
    assert_eq!(body["transaction_id"], pay_key);
    assert_eq!(body["payer_name"], "Firuza Karimova");
    assert!(body["paid_at_ms"].is_i64());
    let (status, body) = get_check(&app, &customer.token, &check_key).await;
    assert_eq!(status, StatusCode::OK, "payer may read: {body}");
    let (status, _) = get_check(&app, &admin, &check_key).await;
    assert_eq!(status, StatusCode::NOT_FOUND);

    let (status, body) = send(
        &app,
        req(
            "GET",
            "/v1/checks?status=paid",
            Some(&merchant.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert!(body
        .as_array()
        .unwrap()
        .iter()
        .any(|c| c["id"] == check_key));

    let events: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM biometric_events WHERE check_id = $1 AND outcome = 'paid'",
    )
    .bind(Uuid::parse_str(&check_key).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(events, 1);
    let no_match: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM biometric_events WHERE check_id = $1 AND outcome = 'no_match'",
    )
    .bind(Uuid::parse_str(&check_key).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(no_match, 1);
    let sealed: Vec<u8> = sqlx::query_scalar(
        "SELECT template FROM fingerprint_enrollments WHERE user_id = $1 AND revoked_at IS NULL",
    )
    .bind(Uuid::parse_str(&customer.id).unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    let plain = template_bytes(seed);
    assert!(!sealed.windows(plain.len()).any(|w| w == plain.as_slice()));
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn fingerprint_payment_refuses_insufficient_funds_and_keeps_check_open() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let merchant = party(&app, &pool, None).await;
    let customer = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &customer.wallet, 10_000).await;
    let seed = Uuid::new_v4();
    let (status, body) = enroll(&app, &customer.token, 1, seed).await;
    assert_eq!(status, StatusCode::CREATED, "{body}");

    let check_key = Uuid::new_v4().to_string();
    let (status, _) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        20_000,
        &check_key,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = pay_check(
        &app,
        &merchant.token,
        &check_key,
        seed,
        &Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
    assert_eq!(body["error"]["code"], "insufficient_funds");
    let (_, body) = get_check(&app, &merchant.token, &check_key).await;
    assert_eq!(body["status"], "open");
    assert_eq!(
        balance_of(&app, &customer.token, &customer.wallet).await,
        10_000
    );

    admin_deposit(&app, &admin, &customer.wallet, 10_000).await;
    let (status, body) = pay_check(
        &app,
        &merchant.token,
        &check_key,
        seed,
        &Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{body}");
    assert_eq!(balance_of(&app, &customer.token, &customer.wallet).await, 0);
    assert_eq!(
        balance_of(&app, &merchant.token, &merchant.wallet).await,
        20_000
    );
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn fingerprint_enrollment_rules() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let seed = Uuid::new_v4();

    let (_, unverified_tok) = register(&app).await;
    let (status, body) = enroll(&app, &unverified_tok, 1, seed).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    assert_eq!(body["error"]["code"], "kyc_required");

    let alice = party(&app, &pool, None).await;
    let bob = party(&app, &pool, None).await;
    let (status, first) = enroll(&app, &alice.token, 1, seed).await;
    assert_eq!(status, StatusCode::CREATED, "{first}");
    let (status, body) = enroll(&app, &bob.token, 1, seed).await;
    assert_eq!(
        status,
        StatusCode::CONFLICT,
        "same template, other user: {body}"
    );

    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/biometric/fingerprints",
            Some(&alice.token),
            None,
            json!({"finger": 11, "format": "raw", "template": template_b64(seed), "consent": true}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = send(
        &app,
        req(
            "POST",
            "/v1/biometric/fingerprints",
            Some(&alice.token),
            None,
            json!({"finger": 3, "format": "iso-19794-2", "template": template_b64(seed), "consent": true}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "iso magic: {body}");

    let (status, second) = enroll(&app, &alice.token, 1, Uuid::new_v4()).await;
    assert_eq!(status, StatusCode::CREATED, "{second}");
    assert_ne!(first["id"], second["id"]);
    let (_, list) = send(
        &app,
        req(
            "GET",
            "/v1/biometric/fingerprints",
            Some(&alice.token),
            None,
            Value::Null,
        ),
    )
    .await;
    let list = list.as_array().unwrap();
    assert_eq!(list.len(), 1);
    assert_eq!(list[0]["id"], second["id"]);

    let (status, body) = enroll(&app, &bob.token, 1, seed).await;
    assert_eq!(status, StatusCode::CREATED, "released template: {body}");

    let merchant = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &bob.wallet, 5_000).await;
    let (status, body) = send(
        &app,
        req(
            "DELETE",
            &format!(
                "/v1/biometric/fingerprints/{}",
                body["id"].as_str().unwrap()
            ),
            Some(&bob.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "revoked");
    let (status, _) = send(
        &app,
        req(
            "DELETE",
            &format!("/v1/biometric/fingerprints/{}", Uuid::new_v4()),
            Some(&bob.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let check_key = Uuid::new_v4().to_string();
    let (status, _) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        1_000,
        &check_key,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = pay_check(
        &app,
        &merchant.token,
        &check_key,
        seed,
        &Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "revoked finger: {body}");
    assert_eq!(balance_of(&app, &bob.token, &bob.wallet).await, 5_000);
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn check_lifecycle_cancel_expiry_and_limits() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let merchant = party(&app, &pool, None).await;
    let customer = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &customer.wallet, 1_000_000).await;
    admin_deposit(&app, &admin, &merchant.wallet, 1_000).await;
    let seed = Uuid::new_v4();
    let (status, _) = enroll(&app, &customer.token, 5, seed).await;
    assert_eq!(status, StatusCode::CREATED);
    let merchant_seed = Uuid::new_v4();
    let (status, _) = enroll(&app, &merchant.token, 5, merchant_seed).await;
    assert_eq!(status, StatusCode::CREATED);

    let (status, body) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        1_000,
        &Uuid::new_v4().to_string(),
        json!({"expires_in_secs": 5}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    let (status, body) = create_check(
        &app,
        &merchant.token,
        &customer.wallet,
        1_000,
        &Uuid::new_v4().to_string(),
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::FORBIDDEN, "foreign wallet: {body}");
    let (status, body) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        1_000,
        &Uuid::new_v4().to_string(),
        json!({"currency": "USD"}),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "currency mismatch: {body}");

    let cancel_key = Uuid::new_v4().to_string();
    let (status, _) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        1_000,
        &cancel_key,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = send(
        &app,
        req(
            "POST",
            &format!("/v1/checks/{cancel_key}/cancel"),
            Some(&customer.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "stranger cancel: {body}");
    let (status, body) = send(
        &app,
        req(
            "POST",
            &format!("/v1/checks/{cancel_key}/cancel"),
            Some(&merchant.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(body["status"], "cancelled");
    let (status, body) = send(
        &app,
        req(
            "POST",
            &format!("/v1/checks/{cancel_key}/cancel"),
            Some(&merchant.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "{body}");
    let (status, body) = pay_check(
        &app,
        &merchant.token,
        &cancel_key,
        seed,
        &Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "pay cancelled: {body}");

    let expired_key = Uuid::new_v4().to_string();
    let (status, _) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        1_000,
        &expired_key,
        json!({"expires_in_secs": 30}),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    sqlx::query("UPDATE checks SET expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(Uuid::parse_str(&expired_key).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    let (_, body) = get_check(&app, &merchant.token, &expired_key).await;
    assert_eq!(body["status"], "expired");
    let (status, body) = pay_check(
        &app,
        &merchant.token,
        &expired_key,
        seed,
        &Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::CONFLICT, "pay expired: {body}");
    let (_, body) = send(
        &app,
        req(
            "GET",
            "/v1/checks?status=expired",
            Some(&merchant.token),
            None,
            Value::Null,
        ),
    )
    .await;
    assert!(
        body.as_array()
            .unwrap()
            .iter()
            .any(|c| c["id"] == expired_key),
        "{body}"
    );

    let big_key = Uuid::new_v4().to_string();
    let (status, _) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        250_000,
        &big_key,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = pay_check(
        &app,
        &merchant.token,
        &big_key,
        seed,
        &Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "over cap: {body}");
    assert_eq!(body["error"]["code"], "limit_exceeded");

    let own_key = Uuid::new_v4().to_string();
    let (status, _) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        500,
        &own_key,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);
    let (status, body) = pay_check(
        &app,
        &merchant.token,
        &own_key,
        merchant_seed,
        &Uuid::new_v4().to_string(),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "self pay: {body}");
    assert_eq!(
        balance_of(&app, &customer.token, &customer.wallet).await,
        1_000_000
    );
}

#[tokio::test]
#[ignore = "requires a running PostgreSQL (docker compose up -d)"]
async fn concurrent_fingerprint_payments_of_one_check_post_once() {
    let (app, pool) = app().await;
    let admin = admin_token(&app, &pool).await;
    let merchant = party(&app, &pool, None).await;
    let customer = party(&app, &pool, None).await;
    admin_deposit(&app, &admin, &customer.wallet, 100_000).await;
    let seed = Uuid::new_v4();
    let (status, _) = enroll(&app, &customer.token, 7, seed).await;
    assert_eq!(status, StatusCode::CREATED);
    let check_key = Uuid::new_v4().to_string();
    let (status, _) = create_check(
        &app,
        &merchant.token,
        &merchant.wallet,
        30_000,
        &check_key,
        Value::Null,
    )
    .await;
    assert_eq!(status, StatusCode::CREATED);

    let mut tasks = Vec::new();
    for _ in 0..8 {
        let app = app.clone();
        let tok = merchant.token.clone();
        let ck = check_key.clone();
        tasks.push(tokio::spawn(async move {
            pay_check(&app, &tok, &ck, seed, &Uuid::new_v4().to_string()).await
        }));
    }
    let mut posted = 0;
    let mut conflicts = 0;
    for t in tasks {
        let (status, body) = t.await.unwrap();
        match status {
            StatusCode::CREATED => posted += 1,
            StatusCode::CONFLICT => conflicts += 1,
            other => panic!("unexpected {other}: {body}"),
        }
    }
    assert_eq!(posted, 1);
    assert_eq!(conflicts, 7);
    assert_eq!(
        balance_of(&app, &customer.token, &customer.wallet).await,
        70_000
    );
    assert_eq!(
        balance_of(&app, &merchant.token, &merchant.wallet).await,
        30_000
    );
}
