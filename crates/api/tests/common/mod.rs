#![allow(dead_code)]

use std::sync::Arc;

use api::{
    build_router, AmlConfig, AppState, AuthConfig, BiometricConfig, DepositConfig, DeviceConfig,
    FeeConfig, Ledger, RateLimitState,
};
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use ledger::{AccountId, Transaction};
use ledger_tigerbeetle::{AnyTb, HybridLedger, SimTb, TbConfig, TbLedger};
use serde_json::{json, Value};
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use storage::PostgresLedger;
use tokio::sync::OnceCell;
use tower::ServiceExt;
use uuid::Uuid;

/// The ledger backend every suite runs against, `TEST_LEDGER_BACKEND`:
/// - `postgres` (default): `PostgresLedger`;
/// - `tigerbeetle`: the hybrid against a live cluster (`TIGERBEETLE_ADDRESSES`, default 3000;
///   `TIGERBEETLE_CLUSTER_ID`, default 0), in a build with `--features tigerbeetle`;
/// - `tigerbeetle-sim`: the hybrid against the in-process cluster model.
///
/// Give each backend a database of its own: under TigerBeetle `balances` is stale by design,
/// and suites read the whole database (conservation, statements).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Backend {
    Postgres,
    TigerBeetle,
    TigerBeetleSim,
}

pub fn backend() -> Backend {
    match std::env::var("TEST_LEDGER_BACKEND").as_deref() {
        Err(std::env::VarError::NotPresent) | Ok("postgres") => Backend::Postgres,
        Ok("tigerbeetle") => Backend::TigerBeetle,
        Ok("tigerbeetle-sim") => Backend::TigerBeetleSim,
        other => panic!("TEST_LEDGER_BACKEND={other:?}: postgres, tigerbeetle or tigerbeetle-sim"),
    }
}

/// One cluster client per test process (each test has its own runtime; the client has its
/// own thread), with the migrated database's system accounts created in it.
static CLUSTER: OnceCell<Option<AnyTb>> = OnceCell::const_new();

fn tb_config(tb: &AnyTb) -> TbConfig {
    use ledger_tigerbeetle::TbClient;
    TbConfig::new(tb.cluster_id(), "test")
}

#[cfg(feature = "tigerbeetle")]
fn live_cluster() -> AnyTb {
    AnyTb::Live(Arc::new(ledger_tigerbeetle::LiveTb::from_test_env()))
}

#[cfg(not(feature = "tigerbeetle"))]
fn live_cluster() -> AnyTb {
    panic!("TEST_LEDGER_BACKEND=tigerbeetle needs `--features tigerbeetle`")
}

async fn cluster(pool: &PgPool) -> Option<AnyTb> {
    CLUSTER
        .get_or_init(|| async {
            let tb = match backend() {
                Backend::Postgres => return None,
                Backend::TigerBeetle => live_cluster(),
                Backend::TigerBeetleSim => AnyTb::Sim(Arc::new(SimTb::new())),
            };
            let pg = PostgresLedger::new(pool.clone());
            pg.migrate().await.unwrap();
            HybridLedger::new(TbLedger::new(tb.clone(), tb_config(&tb)), pg)
                .open_system_accounts()
                .await
                .expect("system accounts in the cluster");
            Some(tb)
        })
        .await
        .clone()
}

/// The backend `router` serves with. Needs `connect` (or `migrated_pool`) to have run.
pub fn ledger(pool: PgPool) -> Ledger {
    let pg = PostgresLedger::new(pool);
    match CLUSTER.get() {
        Some(Some(tb)) => Ledger::TigerBeetle(HybridLedger::new(
            TbLedger::new(tb.clone(), tb_config(tb)),
            pg,
        )),
        Some(None) => Ledger::Postgres(pg),
        None => panic!("connect() before router(): it prepares the ledger backend"),
    }
}

pub async fn connect() -> PgPool {
    let url = std::env::var("DATABASE_URL").expect("DATABASE_URL must be set");
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&url)
        .await
        .unwrap();
    cluster(&pool).await;
    pool
}

/// For a transaction a test wrote into the journal itself (as another code path would have),
/// keeps the books consistent: under TigerBeetle its movement goes into the cluster directly
/// (no guard, no journal); under Postgres the caller updates `balances` in its own transaction.
pub async fn mirror_into_cluster(pool: &PgPool, txn: &Transaction) {
    if let Ledger::TigerBeetle(l) = ledger(pool.clone()) {
        l.tb().post_direct(txn).await.unwrap();
    }
}

pub struct TestConfig {
    pub rate_limit: RateLimitState,
    pub aml: AmlConfig,
    pub fees: FeeConfig,
    pub kyc_upload_daily_max: i64,
    pub auth: AuthConfig,
    pub deposits: DepositConfig,
    pub biometric: BiometricConfig,
    pub devices: DeviceConfig,
    pub login_limit: RateLimitState,
}

// The dev test state: single-admin deposits (dual control has dedicated tests) and 1:N
// fingerprint identification on, as most fingerprint tests exercise identification.
impl Default for TestConfig {
    fn default() -> Self {
        Self {
            rate_limit: RateLimitState::default(),
            aml: AmlConfig::default(),
            fees: FeeConfig::default(),
            kyc_upload_daily_max: 10_000,
            auth: AuthConfig::default(),
            deposits: DepositConfig {
                dual_control: false,
                ..DepositConfig::default()
            },
            biometric: BiometricConfig {
                identify: true,
                ..BiometricConfig::dev()
            },
            devices: DeviceConfig::dev(),
            login_limit: RateLimitState::new(10_000, std::time::Duration::from_secs(60)),
        }
    }
}

pub fn router(pool: PgPool, cfg: TestConfig) -> axum::Router {
    router_on(ledger(pool), cfg)
}

/// A router on a backend the test built itself.
pub fn router_on(ledger: Ledger, cfg: TestConfig) -> axum::Router {
    build_router(AppState {
        ledger,
        auth: cfg.auth,
        rate_limit: cfg.rate_limit,
        login_limit: cfg.login_limit,
        resolve_limit: RateLimitState::new(10_000, std::time::Duration::from_secs(60)),
        aml: cfg.aml,
        fees: cfg.fees,
        deposits: cfg.deposits,
        biometric: cfg.biometric,
        devices: cfg.devices,
        trust_proxy: true,
        document_dir: std::env::temp_dir().join("payment-kyc-docs-test"),
        kyc_upload_daily_max: cfg.kyc_upload_daily_max,
    })
}

pub fn router_with(pool: PgPool, rate_limit: RateLimitState) -> axum::Router {
    router(
        pool,
        TestConfig {
            rate_limit,
            ..TestConfig::default()
        },
    )
}

pub fn router_with_aml(pool: PgPool, rate_limit: RateLimitState, aml: AmlConfig) -> axum::Router {
    router(
        pool,
        TestConfig {
            rate_limit,
            aml,
            ..TestConfig::default()
        },
    )
}

pub fn router_full(
    pool: PgPool,
    rate_limit: RateLimitState,
    aml: AmlConfig,
    fees: FeeConfig,
) -> axum::Router {
    router_full_quota(pool, rate_limit, aml, fees, 10_000)
}

pub fn router_with_auth(pool: PgPool, auth: AuthConfig) -> axum::Router {
    router(
        pool,
        TestConfig {
            auth,
            ..TestConfig::default()
        },
    )
}

pub fn router_full_quota(
    pool: PgPool,
    rate_limit: RateLimitState,
    aml: AmlConfig,
    fees: FeeConfig,
    kyc_upload_daily_max: i64,
) -> axum::Router {
    router(
        pool,
        TestConfig {
            rate_limit,
            aml,
            fees,
            kyc_upload_daily_max,
            ..TestConfig::default()
        },
    )
}

pub async fn migrated_pool() -> PgPool {
    let pool = connect().await;
    PostgresLedger::new(pool.clone()).migrate().await.unwrap();
    pool
}

pub async fn app() -> (axum::Router, PgPool) {
    let pool = migrated_pool().await;
    (router_with(pool.clone(), RateLimitState::default()), pool)
}

pub async fn app_with(cfg: TestConfig) -> (axum::Router, PgPool) {
    let pool = migrated_pool().await;
    (router(pool.clone(), cfg), pool)
}

pub async fn make_admin(pool: &PgPool, user_id: &str) {
    sqlx::query("UPDATE users SET is_admin = true WHERE id = $1")
        .bind(Uuid::parse_str(user_id).unwrap())
        .execute(pool)
        .await
        .unwrap();
}

pub async fn admin_token(app: &axum::Router, pool: &PgPool) -> String {
    let (uid, tok) = register(app).await;
    make_admin(pool, &uid).await;
    tok
}

pub async fn verify_kyc(pool: &PgPool, user_id: &str) {
    sqlx::query("UPDATE users SET kyc_level = 1 WHERE id = $1")
        .bind(Uuid::parse_str(user_id).unwrap())
        .execute(pool)
        .await
        .unwrap();
}

pub async fn admin_deposit(app: &axum::Router, admin_tok: &str, account: &str, amount_minor: i64) {
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

pub async fn send(app: &axum::Router, req: Request<Body>) -> (StatusCode, Value) {
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

pub fn req(
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

pub fn random_phone() -> String {
    format!("+992{:09}", Uuid::new_v4().as_u128() % 1_000_000_000)
}

pub async fn register(app: &axum::Router) -> (String, String) {
    let (status, body) = send(
        app,
        req(
            "POST",
            "/v1/auth/register",
            None,
            None,
            json!({"phone": random_phone(), "password": "password123"}),
        ),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "register: {body}");
    (
        body["user_id"].as_str().unwrap().to_string(),
        body["access_token"].as_str().unwrap().to_string(),
    )
}

pub async fn create_wallet(app: &axum::Router, token: &str) -> String {
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

pub async fn create_wallet_cur(app: &axum::Router, token: &str, currency: &str) -> String {
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

/// The raw (credit-positive) balance of every account of a kind, summed, from the backend.
pub async fn system_total(pool: &PgPool, account_type: &str, currency: &str) -> i64 {
    let ids: Vec<AccountId> =
        sqlx::query_scalar("SELECT id FROM accounts WHERE account_type = $1 AND currency = $2")
            .bind(account_type)
            .bind(currency)
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .map(AccountId)
            .collect();
    let total: i128 = match ledger(pool.clone()) {
        // An account the cluster lacks (opened by a direct write) holds nothing there.
        Ledger::TigerBeetle(l) => l.tb().raw_balances(&ids).await.unwrap().values().sum(),
        backend => backend
            .balances(pool, &ids)
            .await
            .unwrap()
            .iter()
            .map(|b| b.raw)
            .sum(),
    };
    total as i64
}

pub async fn approve_named_kyc(pool: &PgPool, user_id: &str, full_name: &str) {
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

pub async fn db_phone(pool: &PgPool, user_id: &str) -> String {
    sqlx::query_scalar("SELECT phone FROM users WHERE id = $1")
        .bind(Uuid::parse_str(user_id).unwrap())
        .fetch_one(pool)
        .await
        .unwrap()
}

pub async fn balance_of(app: &axum::Router, token: &str, account: &str) -> i64 {
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

pub async fn transfer(
    app: &axum::Router,
    token: &str,
    from: &str,
    to: &str,
    amount_minor: i64,
    key: &str,
) -> (StatusCode, Value) {
    transfer_in(app, token, (from, to), amount_minor, "TJS", key).await
}

pub async fn transfer_in(
    app: &axum::Router,
    token: &str,
    (from, to): (&str, &str),
    amount_minor: i64,
    currency: &str,
    key: &str,
) -> (StatusCode, Value) {
    send(
        app,
        req(
            "POST",
            "/v1/transfers",
            Some(token),
            Some(key),
            json!({"from_account": from, "to_account": to, "amount_minor": amount_minor,
                   "currency": currency}),
        ),
    )
    .await
}

pub fn template_bytes(tag: Uuid) -> Vec<u8> {
    tag.as_bytes()
        .iter()
        .cycle()
        .take(64)
        .enumerate()
        .map(|(i, b)| b.wrapping_add(i as u8))
        .collect()
}

pub fn template_b64(tag: Uuid) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(template_bytes(tag))
}

pub async fn enroll(
    app: &axum::Router,
    token: &str,
    finger: i16,
    seed: Uuid,
) -> (StatusCode, Value) {
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

pub async fn create_check(
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

pub fn probe_request(merchant: &Party, check_id: &str, key: &str, body: Value) -> Request<Body> {
    let mut r = req(
        "POST",
        &format!("/v1/checks/{check_id}/pay/fingerprint"),
        Some(&merchant.token),
        Some(key),
        body,
    );
    r.headers_mut()
        .insert("x-terminal-key", merchant.terminal.parse().unwrap());
    r
}

pub async fn pay_check(
    app: &axum::Router,
    merchant: &Party,
    check_id: &str,
    seed: Uuid,
    key: &str,
) -> (StatusCode, Value) {
    let body = json!({"format": "raw", "template": template_b64(seed)});
    send(app, probe_request(merchant, check_id, key, body)).await
}

pub async fn pay_check_app(
    app: &axum::Router,
    token: &str,
    check_id: &str,
    key: &str,
    body: Value,
) -> (StatusCode, Value) {
    send(
        app,
        req(
            "POST",
            &format!("/v1/checks/{check_id}/pay"),
            Some(token),
            Some(key),
            body,
        ),
    )
    .await
}

pub async fn get_check(app: &axum::Router, token: &str, id: &str) -> (StatusCode, Value) {
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

#[derive(Clone)]
pub struct Party {
    pub id: String,
    pub token: String,
    pub wallet: String,
    pub terminal: String,
}

// Registers the scanner terminal straight in the database (the admin endpoint has its own
// tests), so every party can act as a merchant.
pub async fn add_terminal(pool: &PgPool, merchant_id: &str) -> String {
    let key = biometric::TerminalKey::generate();
    let merchant = Uuid::parse_str(merchant_id).unwrap();
    sqlx::query(
        "INSERT INTO terminals (id, merchant_user_id, label, key_hash, created_by)
         VALUES ($1, $2, 'test terminal', $3, $2)",
    )
    .bind(Uuid::new_v4())
    .bind(merchant)
    .bind(key.hash.as_slice())
    .execute(pool)
    .await
    .unwrap();
    key.plaintext
}

pub async fn party(app: &axum::Router, pool: &PgPool, name: Option<&str>) -> Party {
    let (id, token) = register(app).await;
    match name {
        Some(n) => approve_named_kyc(pool, &id, n).await,
        None => verify_kyc(pool, &id).await,
    }
    let wallet = create_wallet(app, &token).await;
    let terminal = add_terminal(pool, &id).await;
    Party {
        id,
        token,
        wallet,
        terminal,
    }
}
