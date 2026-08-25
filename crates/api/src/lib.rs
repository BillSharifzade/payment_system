//! HTTP API for the payment system.
//!
//! A thin, versioned REST layer over the `storage` ledger and the `auth`
//! primitives. Its responsibilities: authenticate callers, validate and shape
//! input, enforce **ownership** and **idempotency** on money-moving endpoints,
//! map errors to proper HTTP status codes, and otherwise stay out of the way.
//! All money correctness lives below it, in `ledger` and `storage`.
//!
//! # Auth
//!
//! `POST /v1/auth/{register,login,refresh,logout}` issue and manage tokens.
//! Protected endpoints require a `Bearer` access token (see [`AuthUser`]); a
//! user may only read their own balances and spend from wallets they own.
//!
//! # Idempotency
//!
//! Every write endpoint requires an `Idempotency-Key` header (a UUID), which is
//! also the ledger transaction id, so the database primary key makes
//! double-spending impossible under retries or concurrent duplicates.

mod error;

pub use error::{ApiError, ApiResult};

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use axum::extract::{FromRequestParts, Multipart, Path, Query, Request, State};
use axum::http::request::Parts;
use axum::http::{header::AUTHORIZATION, HeaderMap, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use ledger::{Account, AccountId, AccountType, Entry, LedgerError, Transaction, TransactionId};
use money::{Currency, Money};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use storage::{LedgerStore, PostgresLedger, StorageError};
use uuid::Uuid;

/// Number of settlement shards seeded by migration 0013.
const SETTLEMENT_SHARD_COUNT: u64 = 16;
/// Base id of the settlement shards: `Uuid::from_u128(0x1000 + i)`.
const SETTLEMENT_SHARD_BASE: u128 = 0x1000;
/// Round-robin cursor over the settlement shards.
static SETTLEMENT_RR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Pick the next settlement shard to debit, round-robin. Sharding spreads
/// deposits across many balance rows so concurrent deposits don't all serialise
/// on one hot row (see migration 0013). All shards are TJS system_settlement
/// accounts, so conservation is unaffected.
fn settlement_shard() -> Uuid {
    let i =
        SETTLEMENT_RR.fetch_add(1, std::sync::atomic::Ordering::Relaxed) % SETTLEMENT_SHARD_COUNT;
    Uuid::from_u128(SETTLEMENT_SHARD_BASE + i as u128)
}

/// The seeded system fee-revenue account that transfer fees are credited to
/// (see migration 0011). `Uuid::from_u128(2)`.
fn fee_account() -> Uuid {
    Uuid::from_u128(2)
}

/// The seeded per-currency FX-position account (see migration 0012).
fn fx_account(currency_code: &str) -> Option<Uuid> {
    match currency_code {
        "TJS" => Some(Uuid::from_u128(3)),
        "USD" => Some(Uuid::from_u128(4)),
        _ => None,
    }
}

/// Transfer fee policy. Defaults to no fee (`transfer_bps: 0`); deployments set
/// their own rate.
#[derive(Clone, Copy, Default)]
pub struct FeeConfig {
    /// Fee charged on each transfer, in basis points (1 bp = 0.01%). The fee is
    /// deducted from the amount the recipient receives and credited to the
    /// platform's fee-revenue account.
    pub transfer_bps: u32,
}

impl FeeConfig {
    /// The fee (in minor units) for a transfer of `amount_minor`, floored.
    /// Computed in i128 and saturated into i64 so an extreme configuration can
    /// never silently wrap into a negative or truncated fee.
    pub fn fee_minor(&self, amount_minor: i64) -> i64 {
        let fee = (amount_minor as i128 * self.transfer_bps as i128) / 10_000;
        i64::try_from(fee).unwrap_or(i64::MAX)
    }
}

/// Auth-related configuration.
#[derive(Clone)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub access_ttl_secs: u64,
    pub refresh_ttl_days: i32,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            jwt_secret: "dev-insecure-secret-change-me".to_string(),
            access_ttl_secs: 900, // 15 minutes
            refresh_ttl_days: 30,
        }
    }
}

/// A fixed-window rate limiter, keyed by client identity (taken from
/// `X-Forwarded-For` — the real client IP behind our ingress/LB).
///
/// Two backends: `InMemory` (per-instance; fine for a single node or tests) and
/// `Redis` (shared across instances, for horizontal scaling — DESIGN.md §11).
/// Guards against abuse, notably credential stuffing on the auth endpoints.
#[derive(Clone)]
pub enum RateLimitState {
    InMemory {
        buckets: Arc<Mutex<HashMap<String, (u32, Instant)>>>,
        max_requests: u32,
        window: Duration,
    },
    Redis {
        pool: deadpool_redis::Pool,
        max_requests: u32,
        window_secs: u64,
    },
}

impl RateLimitState {
    /// In-memory limiter: `max_requests` per `window` per client.
    pub fn new(max_requests: u32, window: Duration) -> Self {
        RateLimitState::InMemory {
            buckets: Arc::new(Mutex::new(HashMap::new())),
            max_requests,
            window,
        }
    }

    /// Redis-backed limiter shared across instances.
    pub fn redis(pool: deadpool_redis::Pool, max_requests: u32, window: Duration) -> Self {
        RateLimitState::Redis {
            pool,
            max_requests,
            window_secs: window.as_secs().max(1),
        }
    }

    /// Record a request from `key`; returns `true` if it is within the limit.
    async fn allow(&self, key: &str) -> bool {
        match self {
            RateLimitState::InMemory {
                buckets,
                max_requests,
                window,
            } => {
                let now = Instant::now();
                let mut buckets = buckets.lock().expect("rate-limit mutex poisoned");
                match buckets.get_mut(key) {
                    Some((count, start)) if now.duration_since(*start) < *window => {
                        if *count >= *max_requests {
                            false
                        } else {
                            *count += 1;
                            true
                        }
                    }
                    _ => {
                        // Evict dead windows before adding a new key, so a
                        // rotating flood of distinct client addresses cannot
                        // grow the map without bound.
                        if buckets.len() >= 10_000 {
                            buckets.retain(|_, (_, start)| now.duration_since(*start) < *window);
                        }
                        buckets.insert(key.to_string(), (1, now));
                        true
                    }
                }
            }
            RateLimitState::Redis {
                pool,
                max_requests,
                window_secs,
            } => Self::allow_redis(pool, *max_requests, *window_secs, key)
                .await
                .unwrap_or_else(|e| {
                    // Fail open: if Redis is unavailable we prefer availability
                    // over hard-blocking all traffic, but we shout about it.
                    tracing::error!(error = %e, "rate-limit Redis error; failing open");
                    true
                }),
        }
    }

    /// Fixed-window counter in Redis. `SET NX EX` creates the counter *with*
    /// its TTL in the same MULTI/EXEC as the `INCR`, so there is no window in
    /// which a crash could leave an immortal counter (the old INCR-then-EXPIRE
    /// could permanently rate-limit a client if the EXPIRE never ran).
    async fn allow_redis(
        pool: &deadpool_redis::Pool,
        max_requests: u32,
        window_secs: u64,
        key: &str,
    ) -> Result<bool, deadpool_redis::PoolError> {
        let mut conn = pool.get().await?;
        let rkey = format!("ratelimit:{key}");
        let (count,): (i64,) = deadpool_redis::redis::pipe()
            .atomic()
            .cmd("SET")
            .arg(&rkey)
            .arg(0)
            .arg("NX")
            .arg("EX")
            .arg(window_secs)
            .ignore()
            .incr(&rkey, 1)
            .query_async(&mut conn)
            .await?;
        Ok(count <= max_requests as i64)
    }
}

impl Default for RateLimitState {
    fn default() -> Self {
        // Generous default: 300 requests per minute per client.
        Self::new(300, Duration::from_secs(60))
    }
}

/// Per-tier transaction limits used by AML screening.
#[derive(Clone, Copy)]
pub struct Limits {
    /// Maximum value of a single transfer, in minor units.
    pub per_tx_minor: i64,
    /// Maximum outbound value from one wallet in a rolling 24h window.
    pub daily_minor: i64,
    /// Maximum number of outbound transfers from one wallet per rolling hour.
    pub velocity_per_hour: i64,
}

/// AML configuration: limits keyed by KYC level. Higher verification unlocks
/// higher limits (DESIGN.md §11).
#[derive(Clone, Copy)]
pub struct AmlConfig {
    pub level1: Limits,
    pub level2: Limits,
}

impl AmlConfig {
    /// Limits applicable to a given KYC level (levels above 2 use level-2 limits).
    pub fn limits_for(&self, kyc_level: i16) -> Limits {
        if kyc_level >= 2 {
            self.level2
        } else {
            self.level1
        }
    }

    /// Build from environment variables, falling back to the compiled defaults
    /// for any variable that is unset or unparseable. All amounts are in minor
    /// units. This lets operators (and compliance) retune limits with a restart
    /// instead of a code change.
    ///
    /// Vars: `AML_L{1,2}_PER_TX_MINOR`, `AML_L{1,2}_DAILY_MINOR`,
    /// `AML_L{1,2}_VELOCITY_PER_HOUR`.
    pub fn from_env() -> Self {
        fn env_i64(key: &str, default: i64) -> i64 {
            std::env::var(key)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        }
        let d = Self::default();
        Self {
            level1: Limits {
                per_tx_minor: env_i64("AML_L1_PER_TX_MINOR", d.level1.per_tx_minor),
                daily_minor: env_i64("AML_L1_DAILY_MINOR", d.level1.daily_minor),
                velocity_per_hour: env_i64("AML_L1_VELOCITY_PER_HOUR", d.level1.velocity_per_hour),
            },
            level2: Limits {
                per_tx_minor: env_i64("AML_L2_PER_TX_MINOR", d.level2.per_tx_minor),
                daily_minor: env_i64("AML_L2_DAILY_MINOR", d.level2.daily_minor),
                velocity_per_hour: env_i64("AML_L2_VELOCITY_PER_HOUR", d.level2.velocity_per_hour),
            },
        }
    }
}

impl Default for AmlConfig {
    fn default() -> Self {
        // Amounts in minor units (diram). TJS exponent 2 → 100 diram = 1 TJS.
        Self {
            level1: Limits {
                per_tx_minor: 500_000,  // 5,000 TJS
                daily_minor: 2_000_000, // 20,000 TJS
                velocity_per_hour: 50,
            },
            level2: Limits {
                per_tx_minor: 50_000_000, // 500,000 TJS
                daily_minor: 200_000_000, // 2,000,000 TJS
                velocity_per_hour: 500,
            },
        }
    }
}

/// Shared application state. Cheap to clone (the pool is reference-counted).
#[derive(Clone)]
pub struct AppState {
    pub ledger: PostgresLedger,
    pub auth: AuthConfig,
    pub rate_limit: RateLimitState,
    /// A separate, much tighter limiter for login attempts, keyed by *phone*
    /// rather than client address — credential stuffing spread across many IPs
    /// is still throttled per targeted account.
    pub login_limit: RateLimitState,
    pub aml: AmlConfig,
    pub fees: FeeConfig,
    /// Whether to trust the `X-Forwarded-For` header for the client identity used
    /// by rate limiting. Only enable this when the server sits behind a reverse
    /// proxy that *overwrites* (not appends to) `X-Forwarded-For`; otherwise a
    /// client can spoof the header to evade the limiter. When false, the raw
    /// connection peer IP is used.
    pub trust_proxy: bool,
    /// Directory where uploaded KYC documents are stored. The API only ever
    /// hands out opaque `document_ref` names; files are served exclusively
    /// through the admin document endpoint.
    pub document_dir: std::path::PathBuf,
    /// Maximum KYC document uploads per user per rolling 24h. Uploads are
    /// otherwise capped only by the global per-client rate limit, which at
    /// 5 MB per file would let one registered user fill the document volume.
    pub kyc_upload_daily_max: i64,
}

/// Build the application router with all routes mounted, fronted by the rate
/// limiter.
pub fn build_router(state: AppState) -> Router {
    let router = Router::new()
        .route("/health", get(health))
        .route("/v1/auth/register", post(register))
        .route("/v1/auth/login", post(login))
        .route("/v1/auth/refresh", post(refresh))
        .route("/v1/auth/logout", post(logout))
        .route("/v1/wallets", post(create_wallet).get(list_wallets))
        .route("/v1/config", get(client_config))
        .route("/v1/users/resolve", get(resolve_recipient))
        .route("/v1/accounts/{id}/balance", get(get_balance))
        .route(
            "/v1/accounts/{id}/transactions",
            get(list_account_transactions),
        )
        .route("/v1/kyc", get(get_kyc_status))
        .route("/v1/kyc/submissions", post(submit_kyc))
        .route(
            "/v1/kyc/documents",
            post(upload_kyc_document).layer(axum::extract::DefaultBodyLimit::max(
                MAX_DOCUMENT_BYTES + 64 * 1024,
            )),
        )
        .route("/v1/kyc/submissions/{id}/approve", post(approve_kyc))
        .route("/v1/kyc/submissions/{id}/reject", post(reject_kyc))
        .route("/v1/admin/kyc/submissions", get(list_kyc_submissions))
        .route(
            "/v1/admin/kyc/documents/{document_ref}",
            get(get_kyc_document),
        )
        .route("/v1/admin/users", get(admin_user_lookup))
        .route("/v1/admin/users/list", get(admin_user_list))
        .route("/v1/admin/users/{user_id}/status", post(set_user_status))
        .route("/v1/admin/status", get(admin_status))
        .route("/v1/admin/metrics", get(admin_metrics))
        .route("/v1/admin/blocks", post(block_user))
        .route(
            "/v1/admin/blocks/{user_id}",
            axum::routing::delete(unblock_user),
        )
        .route("/v1/admin/fx-rates", post(set_fx_rate))
        .route("/v1/fx/rates", get(list_fx_rates))
        .route("/v1/deposits", post(create_deposit))
        .route("/v1/transfers", post(create_transfer))
        .route("/v1/fx", post(create_fx))
        .with_state(state.clone());

    router.layer(axum::middleware::from_fn_with_state(state, rate_limit))
}

/// Rate-limiting middleware. Returns 429 when a client exceeds the configured
/// budget. The client identity comes from [`client_key`].
async fn rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let key = client_key(&state, &request);
    if !state.rate_limit.allow(&key).await {
        return Err(ApiError::TooManyRequests);
    }
    Ok(next.run(request).await)
}

/// Resolve the client identity used for rate limiting.
///
/// `X-Forwarded-For` is honoured **only** when `trust_proxy` is set, and then we
/// take the *right-most* entry — the hop appended by our own trusted proxy —
/// rather than the left-most, which is client-supplied and trivially spoofed.
/// Otherwise we key on the real connection peer IP (from `ConnectInfo`).
fn client_key(state: &AppState, request: &Request) -> String {
    if state.trust_proxy {
        if let Some(hop) = request
            .headers()
            .get("x-forwarded-for")
            .and_then(|v| v.to_str().ok())
            .and_then(|xff| {
                xff.split(',')
                    .map(str::trim)
                    .rfind(|s| !s.is_empty())
                    .map(str::to_string)
            })
        {
            return hop;
        }
    }
    request
        .extensions()
        .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
        .map(|ci| ci.0.ip().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

// ----- Authenticated-user extractor -----------------------------------------

/// Extractor that authenticates the caller from the `Authorization: Bearer
/// <jwt>` header and yields their user id. Any handler taking an `AuthUser`
/// argument is automatically protected.
pub struct AuthUser(pub Uuid);

impl FromRequestParts<AppState> for AuthUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let header = parts
            .headers
            .get(AUTHORIZATION)
            .ok_or_else(|| ApiError::Unauthorized("missing Authorization header".to_string()))?;
        let token = header
            .to_str()
            .ok()
            .and_then(|s| s.strip_prefix("Bearer "))
            .ok_or_else(|| ApiError::Unauthorized("expected a Bearer token".to_string()))?;
        let user_id = auth::verify_access_token(token, &state.auth.jwt_secret)
            .map_err(|_| ApiError::Unauthorized("invalid or expired token".to_string()))?;
        Ok(AuthUser(user_id))
    }
}

/// Like [`AuthUser`], but additionally requires the user to be an admin. The
/// admin flag is checked against the database on every request (so revoking
/// admin takes effect immediately, and it is never trusted from the token).
pub struct AdminUser(pub Uuid);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let AuthUser(user_id) = AuthUser::from_request_parts(parts, state).await?;
        let is_admin: Option<bool> = sqlx::query_scalar("SELECT is_admin FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(state.ledger.pool())
            .await
            .map_err(StorageError::from)?;
        match is_admin {
            Some(true) => Ok(AdminUser(user_id)),
            _ => Err(ApiError::Forbidden("admin privileges required".to_string())),
        }
    }
}

// ----- Auth endpoints -------------------------------------------------------

#[derive(Deserialize)]
struct CredentialsRequest {
    phone: String,
    password: String,
}

#[derive(Serialize)]
struct TokenResponse {
    user_id: Uuid,
    access_token: String,
    refresh_token: String,
    token_type: &'static str,
    expires_in: u64,
}

/// Hash a password off the async runtime (Argon2 is intentionally CPU-heavy).
async fn hash_password_async(password: String) -> ApiResult<String> {
    tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await
        .map_err(|e| ApiError::Internal(format!("hash task failed: {e}")))?
        .map_err(|_| ApiError::Internal("password hashing failed".to_string()))
}

/// A precomputed Argon2 hash of a fixed dummy password, used to equalise login
/// timing when the account does not exist. Computed once, lazily.
fn dummy_password_hash() -> String {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY
        .get_or_init(|| {
            auth::hash_password("timing-equaliser-not-a-real-password").unwrap_or_default()
        })
        .clone()
}

async fn verify_password_async(password: String, hash: String) -> ApiResult<bool> {
    tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash))
        .await
        .map_err(|e| ApiError::Internal(format!("verify task failed: {e}")))
}

/// Normalize a phone number to its canonical form: digits only (E.164 without
/// the `+`), 7-15 of them. Separators and the `+` are stripped, so
/// "+992 90-123-45-67", "992901234567" and "+992901234567" are all the same
/// identity — and the form survives URL query strings, where a literal `+`
/// decodes as a space. Returns `None` if it is not a plausible phone number.
fn normalize_phone(raw: &str) -> Option<String> {
    let digits: String = raw
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '+'))
        .collect();
    if !(7..=15).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(digits)
}

/// Validate registration credentials; returns the normalized phone.
fn validate_credentials(req: &CredentialsRequest) -> ApiResult<String> {
    let phone = normalize_phone(&req.phone).ok_or_else(|| {
        ApiError::BadRequest("phone must be 7-15 digits, optionally with +".to_string())
    })?;
    if req.password.len() < 8 {
        return Err(ApiError::BadRequest(
            "password must be at least 8 characters".to_string(),
        ));
    }
    // Cap the length so an attacker cannot buy an arbitrarily expensive Argon2
    // run with a huge request body.
    if req.password.len() > 128 {
        return Err(ApiError::BadRequest(
            "password must be at most 128 characters".to_string(),
        ));
    }
    Ok(phone)
}

async fn register(
    State(state): State<AppState>,
    Json(req): Json<CredentialsRequest>,
) -> ApiResult<(StatusCode, Json<TokenResponse>)> {
    let phone = validate_credentials(&req)?;

    // Refuse duplicates BEFORE burning an Argon2 hash on the request; the
    // ON CONFLICT below still settles the race between two first registrations.
    let taken: Option<i32> = sqlx::query_scalar("SELECT 1 FROM users WHERE phone = $1")
        .bind(&phone)
        .fetch_optional(state.ledger.pool())
        .await
        .map_err(StorageError::from)?;
    if taken.is_some() {
        return Err(ApiError::Conflict("phone already registered".to_string()));
    }

    let password_hash = hash_password_async(req.password).await?;
    let user_id = Uuid::new_v4();

    let inserted = sqlx::query(
        "INSERT INTO users (id, phone, password_hash)
         VALUES ($1, $2, $3)
         ON CONFLICT (phone) DO NOTHING",
    )
    .bind(user_id)
    .bind(&phone)
    .bind(&password_hash)
    .execute(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    if inserted.rows_affected() == 0 {
        return Err(ApiError::Conflict("phone already registered".to_string()));
    }

    let tokens = issue_tokens(&state, user_id).await?;
    Ok((StatusCode::CREATED, Json(tokens)))
}

async fn login(
    State(state): State<AppState>,
    Json(req): Json<CredentialsRequest>,
) -> ApiResult<Json<TokenResponse>> {
    // Unparseable phones can't be registered, so answer the uniform 401 without
    // touching the database (after the same dummy verify as a missing account).
    let Some(phone) = normalize_phone(&req.phone) else {
        let _ = verify_password_async(req.password, dummy_password_hash()).await;
        return Err(ApiError::Unauthorized("invalid credentials".to_string()));
    };

    // Per-ACCOUNT throttle (the global limiter is per client address; stuffing
    // one account from a botnet of addresses would sail past it).
    if !state.login_limit.allow(&format!("login:{phone}")).await {
        return Err(ApiError::TooManyRequests);
    }

    let row = sqlx::query("SELECT id, password_hash, status FROM users WHERE phone = $1")
        .bind(&phone)
        .fetch_optional(state.ledger.pool())
        .await
        .map_err(StorageError::from)?;

    // Uniform "invalid credentials" whether the user exists or not.
    let Some(row) = row else {
        // Verify against a dummy hash so a missing account takes the same time as
        // a wrong password — otherwise the timing difference leaks which phone
        // numbers are registered (user enumeration).
        let _ = verify_password_async(req.password, dummy_password_hash()).await;
        return Err(ApiError::Unauthorized("invalid credentials".to_string()));
    };
    let user_id: Uuid = row.try_get("id").map_err(StorageError::from)?;
    let hash: String = row.try_get("password_hash").map_err(StorageError::from)?;
    let status: String = row.try_get("status").map_err(StorageError::from)?;

    if !verify_password_async(req.password, hash).await? {
        return Err(ApiError::Unauthorized("invalid credentials".to_string()));
    }
    if status != "active" {
        return Err(ApiError::Forbidden(format!("account is {status}")));
    }

    Ok(Json(issue_tokens(&state, user_id).await?))
}

#[derive(Deserialize)]
struct RefreshRequest {
    refresh_token: String,
}

async fn refresh(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> ApiResult<Json<TokenResponse>> {
    let token_hash = auth::hash_refresh_token(&req.refresh_token);

    // Look the token up regardless of state: a *revoked* token being presented
    // again is not a normal miss, it is the signature of token theft.
    let row = sqlx::query(
        "SELECT t.id, t.user_id, u.status,
                (t.revoked_at IS NOT NULL OR t.expires_at <= now()) AS dead
         FROM refresh_tokens t JOIN users u ON u.id = t.user_id
         WHERE t.token_hash = $1",
    )
    .bind(&token_hash)
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    let Some(row) = row else {
        return Err(ApiError::Unauthorized("invalid refresh token".to_string()));
    };
    let token_id: Uuid = row.try_get("id").map_err(StorageError::from)?;
    let user_id: Uuid = row.try_get("user_id").map_err(StorageError::from)?;
    let status: String = row.try_get("status").map_err(StorageError::from)?;
    let dead: bool = row.try_get("dead").map_err(StorageError::from)?;

    if dead {
        // Replay of a rotated/expired token: either the legitimate client or a
        // thief holds a stale copy — we cannot tell which party the *active*
        // token belongs to, so revoke the whole session family and force a
        // fresh password login.
        sqlx::query(
            "UPDATE refresh_tokens SET revoked_at = now()
             WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .execute(state.ledger.pool())
        .await
        .map_err(StorageError::from)?;
        tracing::warn!(%user_id, "revoked refresh token replayed — all sessions revoked");
        return Err(ApiError::Unauthorized("invalid refresh token".to_string()));
    }

    // Sessions end when the account does: freezing revokes the family (see
    // set_user_status), but this also covers a status changed by any other
    // path. Login enforces the same rule; refresh must not be a way around it.
    if status != "active" {
        return Err(ApiError::Forbidden(format!("account is {status}")));
    }

    // Rotate: revoke the presented token, then issue a fresh pair. Rotation
    // limits the damage if a refresh token is ever leaked.
    sqlx::query("UPDATE refresh_tokens SET revoked_at = now() WHERE id = $1")
        .bind(token_id)
        .execute(state.ledger.pool())
        .await
        .map_err(StorageError::from)?;

    Ok(Json(issue_tokens(&state, user_id).await?))
}

async fn logout(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> ApiResult<StatusCode> {
    let token_hash = auth::hash_refresh_token(&req.refresh_token);
    sqlx::query(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE token_hash = $1 AND revoked_at IS NULL",
    )
    .bind(&token_hash)
    .execute(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

/// Issue an access token and a fresh (rotating) refresh token, persisting only
/// the refresh token's hash.
async fn issue_tokens(state: &AppState, user_id: Uuid) -> ApiResult<TokenResponse> {
    let access_token =
        auth::issue_access_token(user_id, &state.auth.jwt_secret, state.auth.access_ttl_secs)
            .map_err(|_| ApiError::Internal("failed to issue access token".to_string()))?;
    let refresh = auth::generate_refresh_token();

    sqlx::query(
        "INSERT INTO refresh_tokens (id, user_id, token_hash, expires_at)
         VALUES ($1, $2, $3, now() + make_interval(days => $4))",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(&refresh.hash)
    .bind(state.auth.refresh_ttl_days)
    .execute(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    Ok(TokenResponse {
        user_id,
        access_token,
        refresh_token: refresh.plaintext,
        token_type: "Bearer",
        expires_in: state.auth.access_ttl_secs,
    })
}

// ----- Wallets & balances ---------------------------------------------------

#[derive(Deserialize)]
struct CreateWalletRequest {
    /// Optional currency code; defaults to TJS.
    #[serde(default = "default_currency")]
    currency: String,
}

fn default_currency() -> String {
    "TJS".to_string()
}

#[derive(Serialize)]
struct AccountResponse {
    id: Uuid,
    account_type: String,
    currency: String,
}

async fn create_wallet(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<CreateWalletRequest>,
) -> ApiResult<(StatusCode, Json<AccountResponse>)> {
    let currency = state.ledger.lookup_currency(&req.currency).await?;
    let id = AccountId::new();
    state
        .ledger
        .open_account_owned(
            &Account::new(id, AccountType::UserWallet, currency),
            Some(user_id),
        )
        .await?;
    Ok((
        StatusCode::CREATED,
        Json(AccountResponse {
            id: id.as_uuid(),
            account_type: AccountType::UserWallet.as_db_str().to_string(),
            currency: currency.code().to_string(),
        }),
    ))
}

#[derive(Serialize)]
struct BalanceResponse {
    account_id: Uuid,
    balance_minor: i64,
    currency: String,
    display: String,
}

async fn get_balance(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<BalanceResponse>> {
    ensure_owner(&state, AccountId(id), user_id).await?;
    let balance = state.ledger.balance(AccountId(id)).await?;
    let balance_minor = i64::try_from(balance.minor_units())
        .map_err(|_| StorageError::AmountTooLarge(balance.minor_units()))?;
    Ok(Json(BalanceResponse {
        account_id: id,
        balance_minor,
        currency: balance.currency().code().to_string(),
        display: balance.to_string(),
    }))
}

/// Authorize that `user_id` owns `account` (404 if it doesn't exist, 403 if it
/// belongs to someone else or is a system account).
async fn ensure_owner(state: &AppState, account: AccountId, user_id: Uuid) -> ApiResult<()> {
    match state.ledger.account_owner(account).await {
        Ok(Some(owner)) if owner == user_id => Ok(()),
        Ok(_) => Err(ApiError::Forbidden(
            "you do not own this account".to_string(),
        )),
        Err(StorageError::Ledger(LedgerError::UnknownAccount(_))) => {
            Err(ApiError::NotFound("account not found".to_string()))
        }
        Err(e) => Err(e.into()),
    }
}

// ----- KYC -------------------------------------------------------------------

/// Minimum KYC level required to send a transfer.
const KYC_LEVEL_FOR_TRANSFER: i16 = 1;

#[derive(Deserialize)]
struct SubmitKycRequest {
    /// Level being requested (1 = basic, 2 = full).
    requested_level: i16,
    full_name: String,
    document_type: String,
    /// Reference to the supporting document (e.g. an object-store key).
    document_ref: String,
}

#[derive(Serialize)]
struct KycSubmissionResponse {
    id: Uuid,
    status: String,
    requested_level: i16,
}

#[derive(Serialize)]
struct KycStatusResponse {
    kyc_level: i16,
    latest_submission: Option<KycSubmissionResponse>,
}

/// Look up a user's current KYC level.
async fn user_kyc_level(state: &AppState, user_id: Uuid) -> ApiResult<i16> {
    let level: Option<i16> = sqlx::query_scalar("SELECT kyc_level FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(state.ledger.pool())
        .await
        .map_err(StorageError::from)?;
    level.ok_or_else(|| ApiError::Unauthorized("unknown user".to_string()))
}

/// Require that the user meets a minimum KYC level.
async fn ensure_kyc(state: &AppState, user_id: Uuid, min_level: i16) -> ApiResult<()> {
    if user_kyc_level(state, user_id).await? >= min_level {
        Ok(())
    } else {
        Err(ApiError::KycRequired(format!(
            "this action requires KYC level {min_level}"
        )))
    }
}

/// Submit identity information for verification. Creates a pending submission;
/// a user can never approve their own KYC.
async fn submit_kyc(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<SubmitKycRequest>,
) -> ApiResult<(StatusCode, Json<KycSubmissionResponse>)> {
    if !(1..=2).contains(&req.requested_level) {
        return Err(ApiError::BadRequest(
            "requested_level must be 1 or 2".to_string(),
        ));
    }
    if req.full_name.trim().is_empty() || req.document_ref.trim().is_empty() {
        return Err(ApiError::BadRequest(
            "full_name and document_ref are required".to_string(),
        ));
    }

    let id = Uuid::new_v4();
    let result = sqlx::query(
        "INSERT INTO kyc_submissions
           (id, user_id, requested_level, full_name, document_type, document_ref)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(id)
    .bind(user_id)
    .bind(req.requested_level)
    .bind(req.full_name.trim())
    .bind(&req.document_type)
    .bind(req.document_ref.trim())
    .execute(state.ledger.pool())
    .await;

    // The partial unique index rejects a second pending submission.
    if let Err(sqlx::Error::Database(ref e)) = result {
        if e.is_unique_violation() {
            return Err(ApiError::Conflict(
                "you already have a pending KYC submission".to_string(),
            ));
        }
    }
    result.map_err(StorageError::from)?;

    Ok((
        StatusCode::CREATED,
        Json(KycSubmissionResponse {
            id,
            status: "pending".to_string(),
            requested_level: req.requested_level,
        }),
    ))
}

/// The caller's current KYC level plus their most recent submission, if any.
async fn get_kyc_status(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<KycStatusResponse>> {
    let kyc_level = user_kyc_level(&state, user_id).await?;
    let row = sqlx::query(
        "SELECT id, status, requested_level FROM kyc_submissions
         WHERE user_id = $1 ORDER BY created_at DESC LIMIT 1",
    )
    .bind(user_id)
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    let latest_submission = match row {
        Some(row) => Some(KycSubmissionResponse {
            id: row.try_get("id").map_err(StorageError::from)?,
            status: row.try_get("status").map_err(StorageError::from)?,
            requested_level: row.try_get("requested_level").map_err(StorageError::from)?,
        }),
        None => None,
    };
    Ok(Json(KycStatusResponse {
        kyc_level,
        latest_submission,
    }))
}

/// Admin: approve a pending submission and raise the user's KYC level.
async fn approve_kyc(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Path(submission_id): Path<Uuid>,
) -> ApiResult<Json<KycSubmissionResponse>> {
    let mut tx = state
        .ledger
        .pool()
        .begin()
        .await
        .map_err(StorageError::from)?;

    // Claim the submission only if it is still pending.
    let row = sqlx::query(
        "UPDATE kyc_submissions
         SET status = 'approved', reviewed_by = $2, reviewed_at = now()
         WHERE id = $1 AND status = 'pending'
         RETURNING user_id, requested_level",
    )
    .bind(submission_id)
    .bind(admin_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StorageError::from)?;

    let Some(row) = row else {
        return Err(ApiError::NotFound(
            "no pending KYC submission with that id".to_string(),
        ));
    };
    let user_id: Uuid = row.try_get("user_id").map_err(StorageError::from)?;
    let requested_level: i16 = row.try_get("requested_level").map_err(StorageError::from)?;

    // Never lower an existing level.
    sqlx::query("UPDATE users SET kyc_level = GREATEST(kyc_level, $2) WHERE id = $1")
        .bind(user_id)
        .bind(requested_level)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;

    tx.commit().await.map_err(StorageError::from)?;
    Ok(Json(KycSubmissionResponse {
        id: submission_id,
        status: "approved".to_string(),
        requested_level,
    }))
}

#[derive(Deserialize)]
struct RejectKycRequest {
    reason: String,
}

/// Admin: reject a pending submission (does not change the user's level).
async fn reject_kyc(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Path(submission_id): Path<Uuid>,
    Json(req): Json<RejectKycRequest>,
) -> ApiResult<Json<KycSubmissionResponse>> {
    let row = sqlx::query(
        "UPDATE kyc_submissions
         SET status = 'rejected', reviewed_by = $2, reviewed_at = now(), rejection_reason = $3
         WHERE id = $1 AND status = 'pending'
         RETURNING requested_level",
    )
    .bind(submission_id)
    .bind(admin_id)
    .bind(&req.reason)
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    let Some(row) = row else {
        return Err(ApiError::NotFound(
            "no pending KYC submission with that id".to_string(),
        ));
    };
    let requested_level: i16 = row.try_get("requested_level").map_err(StorageError::from)?;
    Ok(Json(KycSubmissionResponse {
        id: submission_id,
        status: "rejected".to_string(),
        requested_level,
    }))
}

// ----- AML / transaction screening ------------------------------------------

#[derive(Deserialize)]
struct BlockUserRequest {
    user_id: Uuid,
    reason: String,
}

/// Admin: add a user to the blocklist (sanctions / fraud). They can no longer
/// send or receive.
async fn block_user(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Json(req): Json<BlockUserRequest>,
) -> ApiResult<StatusCode> {
    sqlx::query(
        "INSERT INTO blocked_users (user_id, reason, blocked_by)
         VALUES ($1, $2, $3)
         ON CONFLICT (user_id) DO UPDATE SET reason = EXCLUDED.reason, blocked_by = EXCLUDED.blocked_by",
    )
    .bind(req.user_id)
    .bind(&req.reason)
    .bind(admin_id)
    .execute(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct SetUserStatusRequest {
    status: String,
}

/// Admin: set a user's account status (`active` / `frozen` / `closed`). The
/// status gates login and refresh; setting a non-active status also revokes
/// every live session in the same transaction, so the only residual access is
/// an already-issued access token (≤ 15 minutes).
async fn set_user_status(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
    Json(req): Json<SetUserStatusRequest>,
) -> ApiResult<StatusCode> {
    if !["active", "frozen", "closed"].contains(&req.status.as_str()) {
        return Err(ApiError::BadRequest(
            "status must be active, frozen or closed".to_string(),
        ));
    }
    let mut tx = state
        .ledger
        .pool()
        .begin()
        .await
        .map_err(StorageError::from)?;
    let updated = sqlx::query("UPDATE users SET status = $2 WHERE id = $1")
        .bind(user_id)
        .bind(&req.status)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    if updated.rows_affected() == 0 {
        return Err(ApiError::NotFound("no user with that id".to_string()));
    }
    if req.status != "active" {
        sqlx::query(
            "UPDATE refresh_tokens SET revoked_at = now()
             WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    }
    tx.commit().await.map_err(StorageError::from)?;
    tracing::info!(%admin_id, %user_id, status = %req.status, "user status changed");
    Ok(StatusCode::NO_CONTENT)
}

/// Admin: remove a user from the blocklist.
async fn unblock_user(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(user_id): Path<Uuid>,
) -> ApiResult<StatusCode> {
    sqlx::query("DELETE FROM blocked_users WHERE user_id = $1")
        .bind(user_id)
        .execute(state.ledger.pool())
        .await
        .map_err(StorageError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn is_blocked(state: &AppState, user_id: Uuid) -> ApiResult<bool> {
    let blocked: Option<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM blocked_users WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(state.ledger.pool())
            .await
            .map_err(StorageError::from)?;
    Ok(blocked.is_some())
}

/// Record a screening decision for the compliance audit trail.
#[allow(clippy::too_many_arguments)]
async fn log_screening(
    state: &AppState,
    user_id: Uuid,
    from_account: Uuid,
    to_account: Uuid,
    amount_minor: i64,
    currency: &str,
    decision: &str,
    rule: Option<&str>,
    detail: Option<&str>,
) -> ApiResult<()> {
    sqlx::query(
        "INSERT INTO screening_events
           (id, user_id, from_account, to_account, amount_minor, currency, decision, rule, detail)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(Uuid::new_v4())
    .bind(user_id)
    .bind(from_account)
    .bind(to_account)
    .bind(amount_minor)
    .bind(currency)
    .bind(decision)
    .bind(rule)
    .bind(detail)
    .execute(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    Ok(())
}

/// Screen a transfer before it is posted: blocklist, per-transaction limit,
/// rolling-24h value limit, and hourly velocity. Every decision is logged.
/// `recipient_owner` is the recipient wallet's owner (the caller has already
/// looked it up while validating the target).
#[allow(clippy::too_many_arguments)]
async fn screen_transfer(
    state: &AppState,
    sender: Uuid,
    from_account: Uuid,
    to_account: Uuid,
    recipient_owner: Option<Uuid>,
    amount_minor: i64,
    currency: &str,
) -> ApiResult<()> {
    // Helper to log a block then return the error.
    macro_rules! block {
        ($rule:expr, $msg:expr) => {{
            log_screening(
                state,
                sender,
                from_account,
                to_account,
                amount_minor,
                currency,
                "blocked",
                Some($rule),
                Some($msg),
            )
            .await?;
            return Err(if $rule == "blocklist" {
                ApiError::Blocked($msg.to_string())
            } else {
                ApiError::LimitExceeded($msg.to_string())
            });
        }};
    }

    // 1. Blocklist: sender and recipient owner.
    if is_blocked(state, sender).await? {
        block!("blocklist", "sender is blocked");
    }
    if let Some(recipient) = recipient_owner {
        if is_blocked(state, recipient).await? {
            block!("blocklist", "recipient is blocked");
        }
    }

    // 2. Limits by KYC tier. The limits are denominated in TJS minor units, so
    // amounts in other currencies are converted at the configured rate first —
    // otherwise a USD transfer would be checked against a diram-denominated cap
    // and be ~11x looser in real value.
    let level = user_kyc_level(state, sender).await?;
    let limits = state.aml.limits_for(level);

    let (tjs_num, tjs_den): (i64, i64) = if currency == "TJS" {
        (1, 1)
    } else {
        let rate: Option<(i64, i64)> = sqlx::query_as(
            "SELECT rate_num, rate_den FROM fx_rates
             WHERE base_currency = $1 AND quote_currency = 'TJS'",
        )
        .bind(currency)
        .fetch_optional(state.ledger.pool())
        .await
        .map_err(StorageError::from)?;
        match rate {
            Some(r) => r,
            // No rate → we cannot value the transfer, so we cannot prove it is
            // within limits. Deny conservatively rather than wave it through.
            None => block!(
                "no_fx_rate",
                "no TJS conversion rate configured for AML screening of this currency"
            ),
        }
    };
    let to_tjs = |minor: i64| -> i64 {
        i64::try_from((minor as i128 * tjs_num as i128) / tjs_den as i128).unwrap_or(i64::MAX)
    };

    if to_tjs(amount_minor) > limits.per_tx_minor {
        block!("per_tx_limit", "amount exceeds the per-transaction limit");
    }

    // 3. Rolling 24h outbound value from this wallet (single-currency, so the
    // sum converts at the same rate).
    let daily_out: i64 = sqlx::query_scalar(
        "SELECT COALESCE(SUM(amount_minor), 0)::BIGINT FROM entries
         WHERE account_id = $1 AND direction = 'debit' AND created_at >= now() - interval '24 hours'",
    )
    .bind(from_account)
    .fetch_one(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    if to_tjs(daily_out).saturating_add(to_tjs(amount_minor)) > limits.daily_minor {
        block!("daily_limit", "amount exceeds the rolling 24h limit");
    }

    // 4. Hourly velocity (count of outbound transfers).
    let last_hour: i64 = sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM entries
         WHERE account_id = $1 AND direction = 'debit' AND created_at >= now() - interval '1 hour'",
    )
    .bind(from_account)
    .fetch_one(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    if last_hour >= limits.velocity_per_hour {
        block!("velocity", "too many transfers in the last hour");
    }

    log_screening(
        state,
        sender,
        from_account,
        to_account,
        amount_minor,
        currency,
        "allowed",
        None,
        None,
    )
    .await?;
    Ok(())
}

// ----- Deposits & transfers (authenticated + idempotent) --------------------

#[derive(Deserialize)]
struct DepositRequest {
    /// The wallet to credit. An admin may fund any wallet.
    user_account: Uuid,
    amount_minor: i64,
    #[serde(default = "default_currency")]
    currency: String,
}

#[derive(Deserialize)]
struct TransferRequest {
    from_account: Uuid,
    to_account: Uuid,
    amount_minor: i64,
    #[serde(default = "default_currency")]
    currency: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct PostResponse {
    transaction_id: Uuid,
    status: String,
}

async fn create_deposit(
    _admin: AdminUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<DepositRequest>,
) -> ApiResult<(StatusCode, Json<PostResponse>)> {
    // Deposits represent funds entering the system from a partner bank, so they
    // are admin-only. The target wallet need not belong to the admin.
    if req.amount_minor <= 0 {
        return Err(ApiError::BadRequest(
            "amount_minor must be positive".to_string(),
        ));
    }
    let key = idempotency_key(&headers)?;

    // Only user wallets can be funded — crediting a system account (fee,
    // settlement, FX position) would silently distort the platform's books.
    let (target_type, _) = account_info_or_404(&state, req.user_account).await?;
    if target_type != AccountType::UserWallet {
        return Err(ApiError::BadRequest(
            "deposits must credit a user wallet".to_string(),
        ));
    }

    let currency = state.ledger.lookup_currency(&req.currency).await?;
    let amount = Money::from_minor(req.amount_minor as i128, currency);

    let fingerprint = format!(
        "deposit:{}:{}:{}",
        req.user_account, req.amount_minor, req.currency
    );
    let txn = Transaction::new(
        TransactionId(key),
        vec![
            Entry::debit(AccountId(settlement_shard()), amount),
            Entry::credit(AccountId(req.user_account), amount),
        ],
    );
    run_idempotent(&state, key, fingerprint, txn).await
}

async fn create_transfer(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<TransferRequest>,
) -> ApiResult<(StatusCode, Json<PostResponse>)> {
    // You may only spend from a wallet you own, and must be KYC-verified.
    if req.amount_minor <= 0 {
        return Err(ApiError::BadRequest(
            "amount_minor must be positive".to_string(),
        ));
    }
    // A self-transfer nets to zero but would still consume the sender's AML
    // daily/velocity budget (and, with a fee configured, charge a fee to move
    // nothing).
    if req.from_account == req.to_account {
        return Err(ApiError::BadRequest(
            "from_account and to_account must differ".to_string(),
        ));
    }
    let key = idempotency_key(&headers)?;
    ensure_owner(&state, AccountId(req.from_account), user_id).await?;
    ensure_kyc(&state, user_id, KYC_LEVEL_FOR_TRANSFER).await?;

    let fingerprint = format!(
        "transfer:{}:{}:{}:{}",
        req.from_account, req.to_account, req.amount_minor, req.currency
    );

    // Replay check BEFORE screening: a retry of an already-posted transfer must
    // return the stored outcome. Re-screening it would double-count the money
    // against the very limits its first attempt consumed and answer a bogus
    // 422 for a transfer that in fact happened.
    if let Some(found) = load_idempotent::<PostResponse>(&state, key, &fingerprint).await? {
        return Ok(found);
    }

    // The recipient must be a user wallet: the system accounts' ids are
    // well-known, and money "transferred" into them is unrecoverable.
    let (to_type, to_owner) = account_info_or_404(&state, req.to_account).await?;
    if to_type != AccountType::UserWallet {
        return Err(ApiError::BadRequest(
            "recipient must be a user wallet".to_string(),
        ));
    }

    // AML screening: blocklist, limits, velocity.
    screen_transfer(
        &state,
        user_id,
        req.from_account,
        req.to_account,
        to_owner,
        req.amount_minor,
        &req.currency,
    )
    .await?;

    let currency = state.ledger.lookup_currency(&req.currency).await?;
    let amount = Money::from_minor(req.amount_minor as i128, currency);

    // Fee policy (TJS only for now — the fee-revenue account is TJS). The sender
    // is debited the full amount; the recipient receives amount − fee; the fee
    // is credited to the fee-revenue account. This is the canonical three-entry
    // double-entry transfer-with-fee (DESIGN.md §4.2).
    let fee_minor = if currency.code() == "TJS" {
        state.fees.fee_minor(req.amount_minor)
    } else {
        0
    };

    let entries = if fee_minor > 0 {
        let to_recipient = Money::from_minor((req.amount_minor - fee_minor) as i128, currency);
        let fee = Money::from_minor(fee_minor as i128, currency);
        vec![
            Entry::debit(AccountId(req.from_account), amount),
            Entry::credit(AccountId(req.to_account), to_recipient),
            Entry::credit(AccountId(fee_account()), fee),
        ]
    } else {
        vec![
            Entry::debit(AccountId(req.from_account), amount),
            Entry::credit(AccountId(req.to_account), amount),
        ]
    };

    let txn = Transaction::new(TransactionId(key), entries);
    post_idempotent(
        &state,
        key,
        fingerprint,
        txn,
        PostResponse {
            transaction_id: key,
            status: "posted".to_string(),
        },
        PostResponse {
            transaction_id: key,
            status: "already_posted".to_string(),
        },
    )
    .await
}

/// An account's (type, owner), mapping a missing account to 404.
async fn account_info_or_404(
    state: &AppState,
    account: Uuid,
) -> ApiResult<(AccountType, Option<Uuid>)> {
    match state.ledger.account_info(AccountId(account)).await {
        Ok(info) => Ok(info),
        Err(StorageError::Ledger(LedgerError::UnknownAccount(_))) => {
            Err(ApiError::NotFound("account not found".to_string()))
        }
        Err(e) => Err(e.into()),
    }
}

// ----- FX / currency conversion ---------------------------------------------

#[derive(Deserialize)]
struct SetFxRateRequest {
    base: String,
    quote: String,
    /// `amount_quote_minor = amount_base_minor * rate_num / rate_den`.
    rate_num: i64,
    rate_den: i64,
}

/// Admin: set the conversion rate for a currency pair.
async fn set_fx_rate(
    _admin: AdminUser,
    State(state): State<AppState>,
    Json(req): Json<SetFxRateRequest>,
) -> ApiResult<StatusCode> {
    if req.rate_num <= 0 || req.rate_den <= 0 {
        return Err(ApiError::BadRequest(
            "rate_num and rate_den must be positive".to_string(),
        ));
    }
    // Validate both currencies exist.
    state.ledger.lookup_currency(&req.base).await?;
    state.ledger.lookup_currency(&req.quote).await?;

    sqlx::query(
        "INSERT INTO fx_rates (base_currency, quote_currency, rate_num, rate_den, updated_at)
         VALUES ($1, $2, $3, $4, now())
         ON CONFLICT (base_currency, quote_currency)
         DO UPDATE SET rate_num = EXCLUDED.rate_num, rate_den = EXCLUDED.rate_den, updated_at = now()",
    )
    .bind(req.base.to_uppercase())
    .bind(req.quote.to_uppercase())
    .bind(req.rate_num)
    .bind(req.rate_den)
    .execute(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize)]
struct FxRequest {
    /// Source wallet (currency A), owned by the caller.
    from_account: Uuid,
    /// Destination wallet (currency B), owned by the caller.
    to_account: Uuid,
    /// Amount to convert, in currency A's minor units.
    amount_minor: i64,
}

#[derive(Serialize, Deserialize, Clone)]
struct FxResponse {
    transaction_id: Uuid,
    debited_minor: i64,
    credited_minor: i64,
    from_currency: String,
    to_currency: String,
}

/// Convert between two of the caller's own wallets at the admin-set rate. The
/// transaction has one balanced leg per currency.
async fn create_fx(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<FxRequest>,
) -> ApiResult<(StatusCode, Json<FxResponse>)> {
    // Both wallets must belong to the caller, who must be KYC-verified.
    let key = idempotency_key(&headers)?;
    ensure_owner(&state, AccountId(req.from_account), user_id).await?;
    ensure_owner(&state, AccountId(req.to_account), user_id).await?;
    ensure_kyc(&state, user_id, KYC_LEVEL_FOR_TRANSFER).await?;

    let from_cur = state
        .ledger
        .account_currency(AccountId(req.from_account))
        .await?;
    let to_cur = state
        .ledger
        .account_currency(AccountId(req.to_account))
        .await?;
    if from_cur == to_cur {
        return Err(ApiError::BadRequest(
            "use /v1/transfers for same-currency movements".to_string(),
        ));
    }
    if req.amount_minor <= 0 {
        return Err(ApiError::BadRequest(
            "amount_minor must be positive".to_string(),
        ));
    }

    let fingerprint = format!(
        "fx:{}:{}:{}:{}->{}",
        req.from_account,
        req.to_account,
        req.amount_minor,
        from_cur.code(),
        to_cur.code()
    );

    // Replay BEFORE screening (a retry must not be re-screened — see
    // create_transfer) and BEFORE the rate lookup: the stored response reports
    // the conversion that actually posted, not one recomputed at whatever the
    // rate happens to be at retry time.
    if let Some(found) = load_idempotent::<FxResponse>(&state, key, &fingerprint).await? {
        return Ok(found);
    }

    // AML screen the outbound leg (blocklist, limits, velocity).
    screen_transfer(
        &state,
        user_id,
        req.from_account,
        req.to_account,
        Some(user_id),
        req.amount_minor,
        from_cur.code(),
    )
    .await?;

    // Look up the rate and compute the converted amount (floored; the remainder
    // stays in the platform FX position — money is never created).
    let rate: Option<(i64, i64)> = sqlx::query_as(
        "SELECT rate_num, rate_den FROM fx_rates WHERE base_currency = $1 AND quote_currency = $2",
    )
    .bind(from_cur.code())
    .bind(to_cur.code())
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    let (rate_num, rate_den) = rate.ok_or_else(|| {
        ApiError::BadRequest(format!(
            "no FX rate configured for {}->{}",
            from_cur.code(),
            to_cur.code()
        ))
    })?;

    let credited_128 = (req.amount_minor as i128 * rate_num as i128) / rate_den as i128;
    let credited_minor = i64::try_from(credited_128)
        .map_err(|_| ApiError::BadRequest("converted amount is too large".to_string()))?;
    if credited_minor <= 0 {
        return Err(ApiError::BadRequest(
            "converted amount rounds to zero".to_string(),
        ));
    }

    let fx_from = fx_account(from_cur.code())
        .ok_or_else(|| ApiError::BadRequest("unsupported source currency".to_string()))?;
    let fx_to = fx_account(to_cur.code())
        .ok_or_else(|| ApiError::BadRequest("unsupported target currency".to_string()))?;

    let amount_a = Money::from_minor(req.amount_minor as i128, from_cur);
    let amount_b = Money::from_minor(credited_minor as i128, to_cur);

    // Two balanced legs: currency A nets to zero, currency B nets to zero.
    let txn = Transaction::new(
        TransactionId(key),
        vec![
            Entry::debit(AccountId(req.from_account), amount_a),
            Entry::credit(AccountId(fx_from), amount_a),
            Entry::debit(AccountId(fx_to), amount_b),
            Entry::credit(AccountId(req.to_account), amount_b),
        ],
    );

    // Idempotent post, storing the FULL conversion report as this key's
    // response so any replay returns what actually happened.
    let resp = FxResponse {
        transaction_id: key,
        debited_minor: req.amount_minor,
        credited_minor,
        from_currency: from_cur.code().to_string(),
        to_currency: to_cur.code().to_string(),
    };
    post_idempotent(&state, key, fingerprint, txn, resp.clone(), resp).await
}

// ----- Client & console read APIs ---------------------------------------------

/// One wallet with its oriented balance, as shown in a client's wallet list.
#[derive(Serialize)]
struct WalletResponse {
    id: Uuid,
    currency: String,
    balance_minor: i64,
    display: String,
}

/// Read the wallets (id, currency, oriented balance) owned by `owner`.
async fn wallets_of(state: &AppState, owner: Uuid) -> ApiResult<Vec<WalletResponse>> {
    let rows = sqlx::query(
        "SELECT a.id, a.currency, c.exponent, b.raw_minor
         FROM accounts a
         JOIN balances b ON b.account_id = a.id
         JOIN currencies c ON c.code = a.currency
         WHERE a.owner_user_id = $1
         ORDER BY a.created_at",
    )
    .bind(owner)
    .fetch_all(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    let mut wallets = Vec::with_capacity(rows.len());
    for row in rows {
        let id: Uuid = row.try_get("id").map_err(StorageError::from)?;
        let code: String = row.try_get("currency").map_err(StorageError::from)?;
        let exponent: i16 = row.try_get("exponent").map_err(StorageError::from)?;
        // User wallets are credit-normal, so the oriented balance is the raw one.
        let raw_minor: i64 = row.try_get("raw_minor").map_err(StorageError::from)?;
        let currency = Currency::new(&code, exponent as u8)
            .map_err(|e| StorageError::DataIntegrity(e.to_string()))?;
        wallets.push(WalletResponse {
            id,
            currency: code,
            balance_minor: raw_minor,
            display: Money::from_minor(raw_minor as i128, currency).to_string(),
        });
    }
    Ok(wallets)
}

/// The caller's wallets with balances (the app home screen).
async fn list_wallets(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<WalletResponse>>> {
    Ok(Json(wallets_of(&state, user_id).await?))
}

/// The pricing/config facts a client needs to render truthful previews.
#[derive(Serialize)]
struct ClientConfigResponse {
    /// Transfer fee in basis points, deducted from what the recipient receives.
    /// The client uses this for a display-only fee line; the server remains the
    /// authority on what is actually charged.
    transfer_fee_bps: u32,
}

async fn client_config(
    AuthUser(_user_id): AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<ClientConfigResponse>> {
    Ok(Json(ClientConfigResponse {
        transfer_fee_bps: state.fees.transfer_bps,
    }))
}

#[derive(Deserialize)]
struct ResolveParams {
    /// Look up a recipient by phone number (the "check number" flow).
    phone: Option<String>,
    /// Look up a recipient by wallet id (the QR-scan flow; a user's receive-QR
    /// encodes this id).
    wallet: Option<Uuid>,
}

/// A recipient the caller may send money to. Returned by `GET /v1/users/resolve`.
#[derive(Serialize)]
struct ResolveResponse {
    /// The wallet to credit — feed straight into `POST /v1/transfers` `to_account`.
    wallet_id: Uuid,
    currency: String,
    /// The recipient's verified full name, present only when they have an
    /// *approved* KYC submission. `null` means the account exists and can
    /// receive, but we hold no verified name to confirm — the client shows a
    /// distinct "name not verified" state rather than a name.
    name: Option<String>,
    name_verified: bool,
}

/// Resolve a phone number (or wallet id) to a sendable recipient, so the client
/// can show *who* is about to be paid before the transfer is confirmed. This is
/// the "check number" button and the QR-scan confirm, and the only bridge from a
/// human-friendly phone number to the wallet id `POST /v1/transfers` requires.
///
/// Authenticated + KYC level 1 (the same bar as sending) so this is not an
/// anonymous user-enumeration oracle; the global rate limiter applies on top.
/// Only a verified name (approved KYC) is ever revealed — never self-asserted
/// data. 404 with code `not_found` distinguishes "no such number" from a match.
async fn resolve_recipient(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Query(params): Query<ResolveParams>,
) -> ApiResult<Json<ResolveResponse>> {
    ensure_kyc(&state, user_id, 1).await?;

    // Exactly one lookup key.
    let (recipient_id, wallet_id, currency): (Uuid, Uuid, String) =
        match (params.phone.as_deref(), params.wallet) {
            (Some(_), Some(_)) | (None, None) => {
                return Err(ApiError::BadRequest(
                    "provide exactly one of `phone` or `wallet`".to_string(),
                ));
            }
            (Some(raw), None) => {
                let phone = normalize_phone(raw)
                    .ok_or_else(|| ApiError::BadRequest("malformed phone number".to_string()))?;
                let owner: Uuid = sqlx::query_scalar("SELECT id FROM users WHERE phone = $1")
                    .bind(&phone)
                    .fetch_optional(state.ledger.pool())
                    .await
                    .map_err(StorageError::from)?
                    .ok_or_else(|| ApiError::NotFound("no account with that number".to_string()))?;
                // Their TJS wallet is the sendable target.
                let row = sqlx::query(
                    "SELECT id, currency FROM accounts
                     WHERE owner_user_id = $1 AND currency = 'TJS'
                     ORDER BY created_at LIMIT 1",
                )
                .bind(owner)
                .fetch_optional(state.ledger.pool())
                .await
                .map_err(StorageError::from)?
                .ok_or_else(|| ApiError::NotFound("recipient cannot receive TJS".to_string()))?;
                (
                    owner,
                    row.try_get("id").map_err(StorageError::from)?,
                    row.try_get("currency").map_err(StorageError::from)?,
                )
            }
            (None, Some(wid)) => {
                let row = sqlx::query("SELECT owner_user_id, currency FROM accounts WHERE id = $1")
                    .bind(wid)
                    .fetch_optional(state.ledger.pool())
                    .await
                    .map_err(StorageError::from)?
                    .ok_or_else(|| ApiError::NotFound("no such wallet".to_string()))?;
                let owner: Option<Uuid> =
                    row.try_get("owner_user_id").map_err(StorageError::from)?;
                // System accounts (settlement/fees/FX) are never valid recipients.
                let owner =
                    owner.ok_or_else(|| ApiError::NotFound("not a user wallet".to_string()))?;
                (
                    owner,
                    wid,
                    row.try_get("currency").map_err(StorageError::from)?,
                )
            }
        };

    // A verified name exists only via an approved KYC submission.
    let name: Option<String> = sqlx::query_scalar(
        "SELECT full_name FROM kyc_submissions
         WHERE user_id = $1 AND status = 'approved'
         ORDER BY reviewed_at DESC NULLS LAST, created_at DESC
         LIMIT 1",
    )
    .bind(recipient_id)
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    Ok(Json(ResolveResponse {
        wallet_id,
        currency,
        name_verified: name.is_some(),
        name,
    }))
}

#[derive(Deserialize)]
struct StatementParams {
    /// Opaque keyset cursor from a previous page's `next_cursor`.
    cursor: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct StatementEntry {
    entry_id: Uuid,
    transaction_id: Uuid,
    direction: String,
    amount_minor: i64,
    currency: String,
    created_at_ms: i64,
    /// What this movement was, from the viewing account's side: `transfer`
    /// (another user), `deposit` (settlement top-up), `fx` (between the caller's
    /// own wallets), `fee`, or `other`.
    kind: String,
    /// The other user's phone / verified name — only for `transfer` (both
    /// parties to a transfer already know each other; system accounts and other
    /// kinds expose nothing).
    counterparty_phone: Option<String>,
    counterparty_name: Option<String>,
}

#[derive(Serialize)]
struct StatementResponse {
    entries: Vec<StatementEntry>,
    /// Pass back as `cursor` to fetch the next (older) page; absent on the last page.
    next_cursor: Option<String>,
}

/// The account's entries, newest first, keyset-paginated on (created_at, id).
/// The cursor embeds the timestamp as Postgres text so the round trip is exact.
async fn list_account_transactions(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<StatementParams>,
) -> ApiResult<Json<StatementResponse>> {
    ensure_owner(&state, AccountId(id), user_id).await?;
    let limit = params.limit.unwrap_or(50).clamp(1, 100);

    let cursor: Option<(String, Uuid)> = match &params.cursor {
        None => None,
        Some(raw) => {
            let (ts, eid) = raw
                .rsplit_once('|')
                .ok_or_else(|| ApiError::BadRequest("malformed cursor".to_string()))?;
            let eid = Uuid::parse_str(eid)
                .map_err(|_| ApiError::BadRequest("malformed cursor".to_string()))?;
            Some((ts.to_string(), eid))
        }
    };

    // Each entry is joined (LATERAL, both index-backed) to the most relevant
    // *counterpart* entry of its transaction — opposite direction, preferring a
    // user wallet — so the client can render "sent to / received from whom"
    // instead of a bare debit/credit. The counterpart's verified name comes only
    // from an approved KYC submission, mirroring `resolve_recipient`.
    let base = "SELECT e.id, e.transaction_id, e.direction, e.amount_minor, e.currency,
                       e.created_at::text AS ts,
                       (EXTRACT(EPOCH FROM e.created_at) * 1000)::BIGINT AS ms,
                       cp.account_type AS cp_type,
                       cp.owner_user_id AS cp_owner,
                       cp.phone AS cp_phone,
                       cp.verified_name AS cp_name
                FROM entries e
                LEFT JOIN LATERAL (
                    SELECT ca.account_type, ca.owner_user_id, u.phone,
                           (SELECT ks.full_name FROM kyc_submissions ks
                             WHERE ks.user_id = ca.owner_user_id AND ks.status = 'approved'
                             ORDER BY ks.reviewed_at DESC NULLS LAST, ks.created_at DESC
                             LIMIT 1) AS verified_name
                    FROM entries ce
                    JOIN accounts ca ON ca.id = ce.account_id
                    LEFT JOIN users u ON u.id = ca.owner_user_id
                    WHERE ce.transaction_id = e.transaction_id
                      AND ce.account_id <> e.account_id
                    ORDER BY (ce.direction <> e.direction) DESC,
                             (ca.account_type = 'user_wallet') DESC,
                             (ce.currency = e.currency) DESC
                    LIMIT 1
                ) cp ON TRUE
                WHERE e.account_id = $1";
    let rows = match &cursor {
        None => {
            sqlx::query(&format!(
                "{base} ORDER BY e.created_at DESC, e.id DESC LIMIT $2"
            ))
            .bind(id)
            .bind(limit)
            .fetch_all(state.ledger.pool())
            .await
        }
        Some((ts, eid)) => {
            sqlx::query(&format!(
                "{base} AND (e.created_at, e.id) < ($3::timestamptz, $4)
                 ORDER BY e.created_at DESC, e.id DESC LIMIT $2"
            ))
            .bind(id)
            .bind(limit)
            .bind(ts)
            .bind(eid)
            .fetch_all(state.ledger.pool())
            .await
        }
    }
    .map_err(|e| match e {
        // A garbage timestamp in the cursor surfaces as a DB error; it is the
        // client's cursor, not our bug.
        sqlx::Error::Database(_) => ApiError::BadRequest("malformed cursor".to_string()),
        other => StorageError::from(other).into(),
    })?;

    let mut entries = Vec::with_capacity(rows.len());
    let mut last: Option<(String, Uuid)> = None;
    for row in rows {
        let entry_id: Uuid = row.try_get("id").map_err(StorageError::from)?;
        let ts: String = row.try_get("ts").map_err(StorageError::from)?;
        let direction: String = row.try_get("direction").map_err(StorageError::from)?;
        let cp_type: Option<String> = row.try_get("cp_type").map_err(StorageError::from)?;
        let cp_owner: Option<Uuid> = row.try_get("cp_owner").map_err(StorageError::from)?;

        let kind = match (cp_type.as_deref(), cp_owner) {
            // The caller's own other wallet — only FX moves money between them.
            (Some("user_wallet"), Some(owner)) if owner == user_id => "fx",
            (Some("user_wallet"), _) => "transfer",
            (Some("system_settlement"), _) if direction == "credit" => "deposit",
            (Some("system_settlement"), _) => "withdrawal",
            (Some("system_fx_gain_loss"), _) => "fx",
            (Some("system_fee_revenue"), _) => "fee",
            _ => "other",
        };
        let (counterparty_phone, counterparty_name) = if kind == "transfer" {
            (
                row.try_get("cp_phone").map_err(StorageError::from)?,
                row.try_get("cp_name").map_err(StorageError::from)?,
            )
        } else {
            (None, None)
        };

        entries.push(StatementEntry {
            entry_id,
            transaction_id: row.try_get("transaction_id").map_err(StorageError::from)?,
            direction,
            amount_minor: row.try_get("amount_minor").map_err(StorageError::from)?,
            currency: row.try_get("currency").map_err(StorageError::from)?,
            created_at_ms: row.try_get("ms").map_err(StorageError::from)?,
            kind: kind.to_string(),
            counterparty_phone,
            counterparty_name,
        });
        last = Some((ts, entry_id));
    }

    let next_cursor = if entries.len() as i64 == limit {
        last.map(|(ts, eid)| format!("{ts}|{eid}"))
    } else {
        None
    };
    Ok(Json(StatementResponse {
        entries,
        next_cursor,
    }))
}

#[derive(Serialize)]
struct FxRateResponse {
    base: String,
    quote: String,
    rate_num: i64,
    rate_den: i64,
    updated_at_ms: i64,
}

/// The currently configured FX rates (any authenticated caller; clients use it
/// to quote conversions before posting).
async fn list_fx_rates(
    _user: AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<FxRateResponse>>> {
    let rows = sqlx::query(
        "SELECT base_currency, quote_currency, rate_num, rate_den,
                (EXTRACT(EPOCH FROM updated_at) * 1000)::BIGINT AS ms
         FROM fx_rates ORDER BY base_currency, quote_currency",
    )
    .fetch_all(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    let mut rates = Vec::with_capacity(rows.len());
    for row in rows {
        rates.push(FxRateResponse {
            base: row.try_get("base_currency").map_err(StorageError::from)?,
            quote: row.try_get("quote_currency").map_err(StorageError::from)?,
            rate_num: row.try_get("rate_num").map_err(StorageError::from)?,
            rate_den: row.try_get("rate_den").map_err(StorageError::from)?,
            updated_at_ms: row.try_get("ms").map_err(StorageError::from)?,
        });
    }
    Ok(Json(rates))
}

// ----- KYC documents ----------------------------------------------------------

/// Upload size cap for KYC documents (the route's body limit adds headroom for
/// multipart framing).
const MAX_DOCUMENT_BYTES: usize = 5 * 1024 * 1024;

/// Allowed document content types and the extension each is stored under.
fn document_extension(content_type: &str) -> Option<&'static str> {
    match content_type {
        "image/jpeg" => Some("jpg"),
        "image/png" => Some("png"),
        "image/webp" => Some("webp"),
        "application/pdf" => Some("pdf"),
        _ => None,
    }
}

#[derive(Serialize)]
struct DocumentResponse {
    /// Opaque reference to pass as `document_ref` in a KYC submission.
    document_ref: String,
}

/// Upload an identity document (multipart field `file`). Returns the opaque
/// `document_ref` to cite in a KYC submission. Only admins can ever read the
/// file back, and only through the admin endpoint.
async fn upload_kyc_document(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    mut multipart: Multipart,
) -> ApiResult<(StatusCode, Json<DocumentResponse>)> {
    // Per-user quota, checked before the body is read: a KYC flow needs a
    // handful of uploads a day, not hundreds of 5 MB files.
    let uploaded_today: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM kyc_documents
         WHERE user_id = $1 AND created_at >= now() - interval '24 hours'",
    )
    .bind(user_id)
    .fetch_one(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    if uploaded_today >= state.kyc_upload_daily_max {
        return Err(ApiError::TooManyRequests);
    }

    let field = loop {
        match multipart
            .next_field()
            .await
            .map_err(|e| ApiError::BadRequest(format!("malformed multipart body: {e}")))?
        {
            None => {
                return Err(ApiError::BadRequest(
                    "expected a multipart field named 'file'".to_string(),
                ))
            }
            Some(f) if f.name() == Some("file") => break f,
            Some(_) => continue,
        }
    };

    let ext = field
        .content_type()
        .and_then(document_extension)
        .ok_or_else(|| {
            ApiError::BadRequest(
                "file must be image/jpeg, image/png, image/webp or application/pdf".to_string(),
            )
        })?;
    let data = field
        .bytes()
        .await
        .map_err(|_| ApiError::BadRequest("document exceeds the 5 MB limit".to_string()))?;
    if data.is_empty() {
        return Err(ApiError::BadRequest("document is empty".to_string()));
    }
    if data.len() > MAX_DOCUMENT_BYTES {
        return Err(ApiError::BadRequest(
            "document exceeds the 5 MB limit".to_string(),
        ));
    }

    let document_ref = format!("{}.{ext}", Uuid::new_v4());
    tokio::fs::create_dir_all(&state.document_dir)
        .await
        .map_err(|e| ApiError::Internal(format!("document store unavailable: {e}")))?;
    tokio::fs::write(state.document_dir.join(&document_ref), &data)
        .await
        .map_err(|e| ApiError::Internal(format!("failed to store document: {e}")))?;
    // Record the upload so the quota above has something to count and the
    // retention worker can prune documents no submission ever cites.
    sqlx::query("INSERT INTO kyc_documents (document_ref, user_id, bytes) VALUES ($1, $2, $3)")
        .bind(&document_ref)
        .bind(user_id)
        .bind(data.len() as i64)
        .execute(state.ledger.pool())
        .await
        .map_err(StorageError::from)?;
    tracing::info!(%user_id, %document_ref, bytes = data.len(), "kyc document uploaded");

    Ok((StatusCode::CREATED, Json(DocumentResponse { document_ref })))
}

/// Admin: fetch an uploaded KYC document by its reference. The reference must
/// be exactly `<uuid>.<known extension>`, which also forecloses path traversal.
async fn get_kyc_document(
    _admin: AdminUser,
    State(state): State<AppState>,
    Path(document_ref): Path<String>,
) -> ApiResult<Response> {
    let (stem, ext) = document_ref
        .rsplit_once('.')
        .ok_or_else(|| ApiError::BadRequest("malformed document ref".to_string()))?;
    if Uuid::parse_str(stem).is_err() {
        return Err(ApiError::BadRequest("malformed document ref".to_string()));
    }
    let content_type = match ext {
        "jpg" => "image/jpeg",
        "png" => "image/png",
        "webp" => "image/webp",
        "pdf" => "application/pdf",
        _ => return Err(ApiError::BadRequest("malformed document ref".to_string())),
    };

    let bytes = tokio::fs::read(state.document_dir.join(&document_ref))
        .await
        .map_err(|_| ApiError::NotFound("document not found".to_string()))?;
    // These are user-supplied bytes served from the API origin: pin the
    // declared type (nosniff) and sandbox the response so even a crafted file
    // can never run script in our origin. The console fetches documents into
    // blob: URLs, so these headers cost it nothing.
    Ok((
        StatusCode::OK,
        [
            (axum::http::header::CONTENT_TYPE, content_type),
            (axum::http::header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
            (axum::http::header::CONTENT_SECURITY_POLICY, "sandbox"),
        ],
        bytes,
    )
        .into_response())
}

// ----- Admin console reads ----------------------------------------------------

#[derive(Deserialize)]
struct KycListParams {
    status: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct KycSubmissionDetail {
    id: Uuid,
    user_id: Uuid,
    phone: String,
    requested_level: i16,
    full_name: String,
    document_type: String,
    document_ref: String,
    status: String,
    created_at_ms: i64,
}

/// Admin: list KYC submissions (default: the pending review queue, oldest first).
async fn list_kyc_submissions(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(params): Query<KycListParams>,
) -> ApiResult<Json<Vec<KycSubmissionDetail>>> {
    let status = params.status.unwrap_or_else(|| "pending".to_string());
    if !["pending", "approved", "rejected"].contains(&status.as_str()) {
        return Err(ApiError::BadRequest(
            "status must be pending, approved or rejected".to_string(),
        ));
    }
    let limit = params.limit.unwrap_or(100).clamp(1, 500);

    let rows = sqlx::query(
        "SELECT k.id, k.user_id, u.phone, k.requested_level, k.full_name,
                k.document_type, k.document_ref, k.status,
                (EXTRACT(EPOCH FROM k.created_at) * 1000)::BIGINT AS ms
         FROM kyc_submissions k
         JOIN users u ON u.id = k.user_id
         WHERE k.status = $1
         ORDER BY k.created_at
         LIMIT $2",
    )
    .bind(&status)
    .bind(limit)
    .fetch_all(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    let mut out = Vec::with_capacity(rows.len());
    for row in rows {
        out.push(KycSubmissionDetail {
            id: row.try_get("id").map_err(StorageError::from)?,
            user_id: row.try_get("user_id").map_err(StorageError::from)?,
            phone: row.try_get("phone").map_err(StorageError::from)?,
            requested_level: row.try_get("requested_level").map_err(StorageError::from)?,
            full_name: row.try_get("full_name").map_err(StorageError::from)?,
            document_type: row.try_get("document_type").map_err(StorageError::from)?,
            document_ref: row.try_get("document_ref").map_err(StorageError::from)?,
            status: row.try_get("status").map_err(StorageError::from)?,
            created_at_ms: row.try_get("ms").map_err(StorageError::from)?,
        });
    }
    Ok(Json(out))
}

#[derive(Deserialize)]
struct UserLookupParams {
    phone: String,
}

#[derive(Serialize)]
struct AdminUserResponse {
    id: Uuid,
    phone: String,
    status: String,
    kyc_level: i16,
    is_admin: bool,
    created_at_ms: i64,
    blocked_reason: Option<String>,
    wallets: Vec<WalletResponse>,
}

/// Admin: full profile of a user by phone number — the console's lookup screen.
async fn admin_user_lookup(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(params): Query<UserLookupParams>,
) -> ApiResult<Json<AdminUserResponse>> {
    let phone = normalize_phone(&params.phone)
        .ok_or_else(|| ApiError::BadRequest("malformed phone number".to_string()))?;

    let row = sqlx::query(
        "SELECT id, phone, status, kyc_level, is_admin,
                (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS ms
         FROM users WHERE phone = $1",
    )
    .bind(&phone)
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?
    .ok_or_else(|| ApiError::NotFound("no user with that phone".to_string()))?;

    let user_id: Uuid = row.try_get("id").map_err(StorageError::from)?;
    let blocked_reason: Option<String> =
        sqlx::query_scalar("SELECT reason FROM blocked_users WHERE user_id = $1")
            .bind(user_id)
            .fetch_optional(state.ledger.pool())
            .await
            .map_err(StorageError::from)?;

    Ok(Json(AdminUserResponse {
        id: user_id,
        phone: row.try_get("phone").map_err(StorageError::from)?,
        status: row.try_get("status").map_err(StorageError::from)?,
        kyc_level: row.try_get("kyc_level").map_err(StorageError::from)?,
        is_admin: row.try_get("is_admin").map_err(StorageError::from)?,
        created_at_ms: row.try_get("ms").map_err(StorageError::from)?,
        blocked_reason,
        wallets: wallets_of(&state, user_id).await?,
    }))
}

#[derive(Serialize)]
struct CheckpointStatus {
    seq: i64,
    to_txn_seq: i64,
    txn_count: i64,
    created_at_ms: i64,
}

#[derive(Serialize)]
struct ConservationStatus {
    currency: String,
    /// SUM of raw balances — 0 when money is conserved.
    net_minor: i64,
}

#[derive(Serialize)]
struct AdminStatusResponse {
    latest_checkpoint: Option<CheckpointStatus>,
    unsealed_transactions: i64,
    conservation: Vec<ConservationStatus>,
}

/// Admin: one-glance integrity status for the console header — latest sealed
/// checkpoint, unsealed backlog, and per-currency conservation totals.
async fn admin_status(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<AdminStatusResponse>> {
    let cp = sqlx::query(
        "SELECT seq, to_txn_seq, txn_count,
                (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS ms
         FROM checkpoints ORDER BY seq DESC LIMIT 1",
    )
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    let latest_checkpoint = match cp {
        None => None,
        Some(row) => Some(CheckpointStatus {
            seq: row.try_get("seq").map_err(StorageError::from)?,
            to_txn_seq: row.try_get("to_txn_seq").map_err(StorageError::from)?,
            txn_count: row.try_get("txn_count").map_err(StorageError::from)?,
            created_at_ms: row.try_get("ms").map_err(StorageError::from)?,
        }),
    };

    let unsealed_transactions: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM transactions WHERE sealed_seq IS NULL")
            .fetch_one(state.ledger.pool())
            .await
            .map_err(StorageError::from)?;

    let rows = sqlx::query(
        "SELECT a.currency, SUM(b.raw_minor)::BIGINT AS net
         FROM balances b JOIN accounts a ON a.id = b.account_id
         GROUP BY a.currency ORDER BY a.currency",
    )
    .fetch_all(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;
    let mut conservation = Vec::with_capacity(rows.len());
    for row in rows {
        conservation.push(ConservationStatus {
            currency: row.try_get("currency").map_err(StorageError::from)?,
            net_minor: row.try_get("net").map_err(StorageError::from)?,
        });
    }

    Ok(Json(AdminStatusResponse {
        latest_checkpoint,
        unsealed_transactions,
        conservation,
    }))
}

// ----- Admin analytics ----------------------------------------------------------

#[derive(Serialize)]
struct DailyVolume {
    /// ISO date (YYYY-MM-DD).
    date: String,
    currency: String,
    /// Total value moved that day (sum of debit entries), in minor units.
    volume_minor: i64,
}

#[derive(Serialize)]
struct DailyCount {
    date: String,
    count: i64,
}

#[derive(Serialize)]
struct MixSlice {
    /// "deposit" | "transfer" | "fx".
    kind: String,
    count: i64,
}

#[derive(Serialize)]
struct KycDaily {
    date: String,
    approved: i64,
    rejected: i64,
}

#[derive(Serialize)]
struct KycFunnel {
    pending: i64,
    approved: i64,
    rejected: i64,
    /// Review decisions per day over the last 14 days.
    decisions_14d: Vec<KycDaily>,
}

#[derive(Serialize)]
struct CurrencyTotal {
    currency: String,
    total_minor: i64,
}

#[derive(Serialize)]
struct UsersMetrics {
    total: i64,
    blocked: i64,
    new_30d: Vec<DailyCount>,
}

#[derive(Serialize)]
struct MetricsResponse {
    /// Value moved per day per currency, last 30 days.
    daily_volume: Vec<DailyVolume>,
    /// Transactions posted per day, last 30 days.
    daily_transactions: Vec<DailyCount>,
    /// Payment mix (deposits / transfers / fx) over the last 30 days.
    mix_30d: Vec<MixSlice>,
    kyc: KycFunnel,
    /// Money currently held by customers, per currency (user wallets only).
    customer_funds: Vec<CurrencyTotal>,
    users: UsersMetrics,
    /// AML screening blocks in the last 30 days.
    aml_blocked_30d: i64,
}

/// Admin: aggregates behind the console dashboard. Every query is bounded to a
/// short window or a partial index (see migration 0018), so the endpoint stays
/// fast regardless of total ledger size.
async fn admin_metrics(
    _admin: AdminUser,
    State(state): State<AppState>,
) -> ApiResult<Json<MetricsResponse>> {
    let pool = state.ledger.pool();

    let volume_rows = sqlx::query(
        "SELECT to_char(date_trunc('day', created_at), 'YYYY-MM-DD') AS date, currency,
                SUM(amount_minor)::BIGINT AS volume
         FROM entries
         WHERE direction = 'debit' AND created_at >= now() - interval '30 days'
         GROUP BY 1, 2 ORDER BY 1, 2",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let mut daily_volume = Vec::with_capacity(volume_rows.len());
    for row in volume_rows {
        daily_volume.push(DailyVolume {
            date: row.try_get("date").map_err(StorageError::from)?,
            currency: row.try_get("currency").map_err(StorageError::from)?,
            volume_minor: row.try_get("volume").map_err(StorageError::from)?,
        });
    }

    let txn_rows = sqlx::query(
        "SELECT to_char(date_trunc('day', created_at), 'YYYY-MM-DD') AS date,
                COUNT(*)::BIGINT AS count
         FROM transactions
         WHERE created_at >= now() - interval '30 days'
         GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let mut daily_transactions = Vec::with_capacity(txn_rows.len());
    for row in txn_rows {
        daily_transactions.push(DailyCount {
            date: row.try_get("date").map_err(StorageError::from)?,
            count: row.try_get("count").map_err(StorageError::from)?,
        });
    }

    // Classify each transaction by the account types it touched: settlement
    // leg → deposit, FX-position leg → fx, otherwise a wallet-to-wallet
    // transfer. bool_or folds the per-entry flags into one row per txn.
    let mix_rows = sqlx::query(
        "SELECT kind, COUNT(*)::BIGINT AS count FROM (
             SELECT t.id,
                    CASE
                      WHEN bool_or(a.account_type = 'system_settlement') THEN 'deposit'
                      WHEN bool_or(a.account_type = 'system_fx_gain_loss') THEN 'fx'
                      ELSE 'transfer'
                    END AS kind
             FROM transactions t
             JOIN entries e ON e.transaction_id = t.id
             JOIN accounts a ON a.id = e.account_id
             WHERE t.created_at >= now() - interval '30 days'
             GROUP BY t.id
         ) classified GROUP BY kind ORDER BY kind",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let mut mix_30d = Vec::with_capacity(mix_rows.len());
    for row in mix_rows {
        mix_30d.push(MixSlice {
            kind: row.try_get("kind").map_err(StorageError::from)?,
            count: row.try_get("count").map_err(StorageError::from)?,
        });
    }

    let funnel_rows = sqlx::query(
        "SELECT status, COUNT(*)::BIGINT AS count FROM kyc_submissions GROUP BY status",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let (mut pending, mut approved, mut rejected) = (0i64, 0i64, 0i64);
    for row in funnel_rows {
        let status: String = row.try_get("status").map_err(StorageError::from)?;
        let count: i64 = row.try_get("count").map_err(StorageError::from)?;
        match status.as_str() {
            "pending" => pending = count,
            "approved" => approved = count,
            "rejected" => rejected = count,
            _ => {}
        }
    }
    let decision_rows = sqlx::query(
        "SELECT to_char(date_trunc('day', reviewed_at), 'YYYY-MM-DD') AS date,
                COUNT(*) FILTER (WHERE status = 'approved')::BIGINT AS approved,
                COUNT(*) FILTER (WHERE status = 'rejected')::BIGINT AS rejected
         FROM kyc_submissions
         WHERE reviewed_at >= now() - interval '14 days'
         GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let mut decisions_14d = Vec::with_capacity(decision_rows.len());
    for row in decision_rows {
        decisions_14d.push(KycDaily {
            date: row.try_get("date").map_err(StorageError::from)?,
            approved: row.try_get("approved").map_err(StorageError::from)?,
            rejected: row.try_get("rejected").map_err(StorageError::from)?,
        });
    }

    let funds_rows = sqlx::query(
        "SELECT a.currency, SUM(b.raw_minor)::BIGINT AS total
         FROM balances b JOIN accounts a ON a.id = b.account_id
         WHERE a.account_type = 'user_wallet'
         GROUP BY a.currency ORDER BY a.currency",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let mut customer_funds = Vec::with_capacity(funds_rows.len());
    for row in funds_rows {
        customer_funds.push(CurrencyTotal {
            currency: row.try_get("currency").map_err(StorageError::from)?,
            total_minor: row.try_get("total").map_err(StorageError::from)?,
        });
    }

    let users_total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM users")
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)?;
    let users_blocked: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM blocked_users")
        .fetch_one(pool)
        .await
        .map_err(StorageError::from)?;
    let new_rows = sqlx::query(
        "SELECT to_char(date_trunc('day', created_at), 'YYYY-MM-DD') AS date,
                COUNT(*)::BIGINT AS count
         FROM users WHERE created_at >= now() - interval '30 days'
         GROUP BY 1 ORDER BY 1",
    )
    .fetch_all(pool)
    .await
    .map_err(StorageError::from)?;
    let mut new_30d = Vec::with_capacity(new_rows.len());
    for row in new_rows {
        new_30d.push(DailyCount {
            date: row.try_get("date").map_err(StorageError::from)?,
            count: row.try_get("count").map_err(StorageError::from)?,
        });
    }

    let aml_blocked_30d: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM screening_events
         WHERE decision = 'blocked' AND created_at >= now() - interval '30 days'",
    )
    .fetch_one(pool)
    .await
    .map_err(StorageError::from)?;

    Ok(Json(MetricsResponse {
        daily_volume,
        daily_transactions,
        mix_30d,
        kyc: KycFunnel {
            pending,
            approved,
            rejected,
            decisions_14d,
        },
        customer_funds,
        users: UsersMetrics {
            total: users_total,
            blocked: users_blocked,
            new_30d,
        },
        aml_blocked_30d,
    }))
}

// ----- Admin user list -----------------------------------------------------------

#[derive(Deserialize)]
struct UserListParams {
    /// Phone-prefix search (digits; other characters are stripped).
    q: Option<String>,
    /// Keyset cursor from a previous page (newest-first browse only).
    cursor: Option<String>,
    limit: Option<i64>,
}

#[derive(Serialize)]
struct UserListItem {
    id: Uuid,
    phone: String,
    status: String,
    kyc_level: i16,
    is_admin: bool,
    is_blocked: bool,
    created_at_ms: i64,
}

#[derive(Serialize)]
struct UserListResponse {
    users: Vec<UserListItem>,
    next_cursor: Option<String>,
}

/// Admin: browsable user directory. Without `q`, pages newest-first by keyset
/// cursor; with `q`, does an indexed phone-prefix search (no cursor — refine
/// the prefix instead). Both paths ride the migration-0018 indexes.
async fn admin_user_list(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(params): Query<UserListParams>,
) -> ApiResult<Json<UserListResponse>> {
    let limit = params.limit.unwrap_or(25).clamp(1, 100);

    let base = "SELECT u.id, u.phone, u.status, u.kyc_level, u.is_admin,
                       u.created_at::text AS ts,
                       (EXTRACT(EPOCH FROM u.created_at) * 1000)::BIGINT AS ms,
                       EXISTS(SELECT 1 FROM blocked_users b WHERE b.user_id = u.id) AS is_blocked
                FROM users u";

    let prefix: Option<String> = params.q.as_deref().map(|q| {
        q.chars()
            .filter(|c| c.is_ascii_digit())
            .take(15)
            .collect::<String>()
    });

    let rows = match &prefix {
        Some(p) if p.is_empty() => Vec::new(),
        Some(p) => sqlx::query(&format!(
            "{base} WHERE u.phone LIKE $1 ORDER BY u.phone LIMIT $2"
        ))
        .bind(format!("{p}%"))
        .bind(limit)
        .fetch_all(state.ledger.pool())
        .await
        .map_err(StorageError::from)?,
        None => match &params.cursor {
            None => sqlx::query(&format!(
                "{base} ORDER BY u.created_at DESC, u.id DESC LIMIT $1"
            ))
            .bind(limit)
            .fetch_all(state.ledger.pool())
            .await
            .map_err(StorageError::from)?,
            Some(raw) => {
                let (ts, uid) = raw
                    .rsplit_once('|')
                    .ok_or_else(|| ApiError::BadRequest("malformed cursor".to_string()))?;
                let uid = Uuid::parse_str(uid)
                    .map_err(|_| ApiError::BadRequest("malformed cursor".to_string()))?;
                sqlx::query(&format!(
                    "{base} WHERE (u.created_at, u.id) < ($2::timestamptz, $3)
                     ORDER BY u.created_at DESC, u.id DESC LIMIT $1"
                ))
                .bind(limit)
                .bind(ts)
                .bind(uid)
                .fetch_all(state.ledger.pool())
                .await
                .map_err(|e| match e {
                    sqlx::Error::Database(_) => {
                        ApiError::BadRequest("malformed cursor".to_string())
                    }
                    other => StorageError::from(other).into(),
                })?
            }
        },
        // Unreachable: covered by the two Some arms above.
    };

    let mut users = Vec::with_capacity(rows.len());
    let mut last: Option<(String, Uuid)> = None;
    for row in rows {
        let id: Uuid = row.try_get("id").map_err(StorageError::from)?;
        let ts: String = row.try_get("ts").map_err(StorageError::from)?;
        users.push(UserListItem {
            id,
            phone: row.try_get("phone").map_err(StorageError::from)?,
            status: row.try_get("status").map_err(StorageError::from)?,
            kyc_level: row.try_get("kyc_level").map_err(StorageError::from)?,
            is_admin: row.try_get("is_admin").map_err(StorageError::from)?,
            is_blocked: row.try_get("is_blocked").map_err(StorageError::from)?,
            created_at_ms: row.try_get("ms").map_err(StorageError::from)?,
        });
        last = Some((ts, id));
    }

    // Cursors only make sense on the browse path (search is prefix-refined).
    let next_cursor = if prefix.is_none() && users.len() as i64 == limit {
        last.map(|(ts, id)| format!("{ts}|{id}"))
    } else {
        None
    };

    Ok(Json(UserListResponse { users, next_cursor }))
}

/// Extract and validate the `Idempotency-Key` header as a UUID.
fn idempotency_key(headers: &HeaderMap) -> ApiResult<Uuid> {
    let raw = headers
        .get("idempotency-key")
        .ok_or_else(|| ApiError::BadRequest("missing Idempotency-Key header".to_string()))?;
    let s = raw
        .to_str()
        .map_err(|_| ApiError::BadRequest("invalid Idempotency-Key header".to_string()))?;
    Uuid::parse_str(s)
        .map_err(|_| ApiError::BadRequest("Idempotency-Key must be a UUID".to_string()))
}

/// Run a money-moving transaction idempotently (see module docs): replay a
/// stored response if the key was seen before, otherwise post and store.
async fn run_idempotent(
    state: &AppState,
    key: Uuid,
    fingerprint: String,
    txn: Transaction,
) -> ApiResult<(StatusCode, Json<PostResponse>)> {
    if let Some(found) = load_idempotent::<PostResponse>(state, key, &fingerprint).await? {
        return Ok(found);
    }
    post_idempotent(
        state,
        key,
        fingerprint,
        txn,
        PostResponse {
            transaction_id: key,
            status: "posted".to_string(),
        },
        PostResponse {
            transaction_id: key,
            status: "already_posted".to_string(),
        },
    )
    .await
}

/// Post `txn` and persist `created` as this key's replayable response. The
/// caller must have consulted [`load_idempotent`] first. `raced` is returned
/// (with 200) only in the concurrent-duplicate window where the ledger already
/// holds the transaction but the winner has not stored its response yet.
async fn post_idempotent<T>(
    state: &AppState,
    key: Uuid,
    fingerprint: String,
    txn: Transaction,
    created: T,
    raced: T,
) -> ApiResult<(StatusCode, Json<T>)>
where
    T: Serialize + serde::de::DeserializeOwned,
{
    match state.ledger.post(&txn).await {
        Ok(()) => {
            let body = serde_json::to_value(&created).expect("response serialises");
            sqlx::query(
                "INSERT INTO idempotency_keys (key, fingerprint, response_status, response_body)
                 VALUES ($1, $2, $3, $4)
                 ON CONFLICT (key) DO NOTHING",
            )
            .bind(key)
            .bind(&fingerprint)
            .bind(StatusCode::CREATED.as_u16() as i32)
            .bind(&body)
            .execute(state.ledger.pool())
            .await
            .map_err(StorageError::from)?;
            Ok((StatusCode::CREATED, Json(created)))
        }
        Err(StorageError::Ledger(LedgerError::DuplicateTransaction(_))) => {
            if let Some(found) = load_idempotent::<T>(state, key, &fingerprint).await? {
                return Ok(found);
            }
            Ok((StatusCode::OK, Json(raced)))
        }
        Err(e) => Err(e.into()),
    }
}

/// Look up a previously stored idempotent response, or `Err` if the key was
/// reused with a different request body.
async fn load_idempotent<T: serde::de::DeserializeOwned>(
    state: &AppState,
    key: Uuid,
    fingerprint: &str,
) -> ApiResult<Option<(StatusCode, Json<T>)>> {
    let row = sqlx::query(
        "SELECT fingerprint, response_status, response_body FROM idempotency_keys WHERE key = $1",
    )
    .bind(key)
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(StorageError::from)?;

    let Some(row) = row else { return Ok(None) };

    let stored_fingerprint: String = row.try_get("fingerprint").map_err(StorageError::from)?;
    if stored_fingerprint != fingerprint {
        return Err(ApiError::IdempotencyConflict);
    }
    let status: i32 = row.try_get("response_status").map_err(StorageError::from)?;
    let body: serde_json::Value = row.try_get("response_body").map_err(StorageError::from)?;
    let resp: T = serde_json::from_value(body)
        .map_err(|e| StorageError::DataIntegrity(format!("stored idempotent response: {e}")))?;
    let status = StatusCode::from_u16(status as u16)
        .map_err(|e| StorageError::DataIntegrity(format!("stored status: {e}")))?;
    Ok(Some((status, Json(resp))))
}
