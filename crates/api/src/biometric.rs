use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use biometric::{
    decide, Candidate, Decision, HttpMatcher, MatchPolicy, MatcherError, Template, TemplateCipher,
};
use ledger::{AccountId, AccountType, Entry, Transaction, TransactionId};
use money::Money;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use storage::{HookError, PostHook, PostOptions, StorageError};
use uuid::Uuid;

use crate::common::{default_currency, idempotency_key};
use crate::config::{env_or, env_or_file};
use crate::payments::{
    aml_guard, fee_shard, finish, money_context, pre_screen, record, ScreenCtx,
    KYC_LEVEL_FOR_TRANSFER,
};
use crate::session::AuthUser;
use crate::{ApiError, ApiResult, AppState};

pub const KYC_LEVEL_FOR_ENROLLMENT: i16 = 1;
const MAX_DESCRIPTION_CHARS: usize = 140;
const MIN_CHECK_TTL_SECS: i64 = 30;
const IDENTIFY_LIMIT: usize = 5;
const EXACT_SCORE: f64 = 100.0;

#[derive(Clone)]
pub enum MatcherBackend {
    Exact,
    Http(Arc<HttpMatcher>),
}

impl MatcherBackend {
    pub fn name(&self) -> &'static str {
        match self {
            MatcherBackend::Exact => "exact",
            MatcherBackend::Http(_) => "http",
        }
    }
}

#[derive(Clone)]
pub struct BiometricConfig {
    pub cipher: TemplateCipher,
    pub matcher: MatcherBackend,
    pub policy: MatchPolicy,
    pub max_minor: i64,
    pub check_ttl_secs: i64,
    pub max_check_ttl_secs: i64,
}

impl BiometricConfig {
    pub const DEV_KEY_HEX: &'static str =
        "0000000000000000000000000000000000000000000000000000000000000000";

    pub fn dev() -> Self {
        Self {
            cipher: TemplateCipher::from_hex(Self::DEV_KEY_HEX).expect("dev key is valid hex"),
            matcher: MatcherBackend::Exact,
            policy: MatchPolicy::default(),
            max_minor: 200_000,
            check_ttl_secs: 300,
            max_check_ttl_secs: 3_600,
        }
    }

    pub fn from_env(is_prod: bool) -> Result<Self, String> {
        let d = Self::dev();
        let cipher = match env_or_file("BIOMETRIC_TEMPLATE_KEY")? {
            Some(hex_key) => TemplateCipher::from_hex(&hex_key)
                .map_err(|e| format!("BIOMETRIC_TEMPLATE_KEY: {e}"))?,
            None if is_prod => {
                return Err("BIOMETRIC_TEMPLATE_KEY must be set when APP_ENV != dev".to_string())
            }
            None => {
                tracing::warn!(
                    "BIOMETRIC_TEMPLATE_KEY not set — templates sealed with an INSECURE dev key"
                );
                d.cipher.clone()
            }
        };
        let matcher = match std::env::var("BIOMETRIC_MATCHER")
            .unwrap_or_else(|_| "exact".to_string())
            .trim()
        {
            "exact" => {
                if is_prod {
                    tracing::warn!(
                        "BIOMETRIC_MATCHER=exact only recognises byte-identical templates; set BIOMETRIC_MATCHER=http with a matching engine before enrolling real fingerprints"
                    );
                }
                MatcherBackend::Exact
            }
            "http" => {
                let url = env_or_file("BIOMETRIC_MATCHER_URL")?
                    .ok_or("BIOMETRIC_MATCHER_URL is required when BIOMETRIC_MATCHER=http")?;
                let timeout_ms: u64 = env_or("BIOMETRIC_MATCHER_TIMEOUT_MS", 2_000)?;
                let m = HttpMatcher::new(&url, Duration::from_millis(timeout_ms))
                    .map_err(|e| format!("BIOMETRIC_MATCHER_URL: {e}"))?;
                MatcherBackend::Http(Arc::new(m))
            }
            other => return Err(format!("BIOMETRIC_MATCHER={other:?} must be exact or http")),
        };
        let policy = MatchPolicy {
            threshold: env_or("BIOMETRIC_MATCH_THRESHOLD", d.policy.threshold)?,
            margin: env_or("BIOMETRIC_MATCH_MARGIN", d.policy.margin)?,
        };
        if !(policy.threshold.is_finite() && policy.threshold > 0.0) {
            return Err("BIOMETRIC_MATCH_THRESHOLD must be positive".to_string());
        }
        if !(policy.margin.is_finite() && policy.margin >= 0.0) {
            return Err("BIOMETRIC_MATCH_MARGIN must be non-negative".to_string());
        }
        let max_minor: i64 = env_or("BIOMETRIC_MAX_MINOR", d.max_minor)?;
        let check_ttl_secs: i64 = env_or("CHECK_TTL_SECS", d.check_ttl_secs)?;
        let max_check_ttl_secs: i64 = env_or("CHECK_MAX_TTL_SECS", d.max_check_ttl_secs)?;
        if max_minor <= 0 {
            return Err("BIOMETRIC_MAX_MINOR must be positive".to_string());
        }
        if check_ttl_secs < MIN_CHECK_TTL_SECS || check_ttl_secs > max_check_ttl_secs {
            return Err(format!(
                "CHECK_TTL_SECS must be between {MIN_CHECK_TTL_SECS} and CHECK_MAX_TTL_SECS ({max_check_ttl_secs})"
            ));
        }
        Ok(Self {
            cipher,
            matcher,
            policy,
            max_minor,
            check_ttl_secs,
            max_check_ttl_secs,
        })
    }
}

fn matcher_error(e: MatcherError) -> ApiError {
    match e {
        MatcherError::Unavailable(msg) => {
            tracing::warn!(error = %msg, "fingerprint matcher unavailable");
            ApiError::RetryLater
        }
        MatcherError::Protocol(msg) => ApiError::Internal(format!("fingerprint matcher: {msg}")),
    }
}

fn db<T>(r: std::result::Result<T, sqlx::Error>) -> ApiResult<T> {
    r.map_err(|e| StorageError::from(e).into())
}

#[derive(Deserialize)]
pub struct EnrollRequest {
    finger: i16,
    format: String,
    template: String,
    #[serde(default)]
    quality: Option<i16>,
    #[serde(default)]
    consent: bool,
}

#[derive(Serialize)]
pub struct EnrollmentResponse {
    id: Uuid,
    finger: i16,
    format: String,
    quality: Option<i16>,
    created_at_ms: i64,
}

fn enrollment_row(row: &PgRow) -> ApiResult<EnrollmentResponse> {
    Ok(EnrollmentResponse {
        id: db(row.try_get("id"))?,
        finger: db(row.try_get("finger"))?,
        format: db(row.try_get("format"))?,
        quality: db(row.try_get("quality"))?,
        created_at_ms: db(row.try_get("ms"))?,
    })
}

const ENROLLMENT_COLUMNS: &str =
    "id, finger, format, quality, (EXTRACT(EPOCH FROM created_at) * 1000)::BIGINT AS ms";

pub async fn enroll_fingerprint(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Json(req): Json<EnrollRequest>,
) -> ApiResult<(StatusCode, Json<EnrollmentResponse>)> {
    if !req.consent {
        return Err(ApiError::BadRequest(
            "consent must be true: the user must explicitly agree to biometric processing"
                .to_string(),
        ));
    }
    if !(1..=10).contains(&req.finger) {
        return Err(ApiError::BadRequest(
            "finger must be an ISO 19794-2 position between 1 and 10".to_string(),
        ));
    }
    if req.quality.is_some_and(|q| !(0..=100).contains(&q)) {
        return Err(ApiError::BadRequest(
            "quality must be between 0 and 100".to_string(),
        ));
    }
    let template = Template::from_base64(&req.format, &req.template)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let hash = template.hash().to_vec();

    let mut tx = db(state.ledger.pool().begin().await)?;
    let user = db(
        sqlx::query("SELECT status, kyc_level FROM users WHERE id = $1 FOR UPDATE")
            .bind(user_id)
            .fetch_optional(&mut *tx)
            .await,
    )?
    .ok_or_else(|| ApiError::Unauthorized("unknown user".to_string()))?;
    let status: String = db(user.try_get("status"))?;
    let kyc_level: i16 = db(user.try_get("kyc_level"))?;
    if status != "active" {
        return Err(ApiError::Forbidden(format!("account is {status}")));
    }
    if kyc_level < KYC_LEVEL_FOR_ENROLLMENT {
        return Err(ApiError::KycRequired(format!(
            "fingerprint enrolment requires KYC level {KYC_LEVEL_FOR_ENROLLMENT}"
        )));
    }

    let existing = db(sqlx::query(&format!(
        "SELECT user_id, {ENROLLMENT_COLUMNS} FROM fingerprint_enrollments
         WHERE template_hash = $1 AND revoked_at IS NULL"
    ))
    .bind(&hash)
    .fetch_optional(&mut *tx)
    .await)?;
    if let Some(row) = existing {
        let owner: Uuid = db(row.try_get("user_id"))?;
        let finger: i16 = db(row.try_get("finger"))?;
        if owner == user_id && finger == req.finger {
            return Ok((StatusCode::OK, Json(enrollment_row(&row)?)));
        }
        return Err(ApiError::Conflict(
            "this fingerprint is already enrolled".to_string(),
        ));
    }

    let revoked: Vec<Uuid> = db(sqlx::query_scalar(
        "UPDATE fingerprint_enrollments SET revoked_at = now()
         WHERE user_id = $1 AND finger = $2 AND revoked_at IS NULL
         RETURNING id",
    )
    .bind(user_id)
    .bind(req.finger)
    .fetch_all(&mut *tx)
    .await)?;

    let id = Uuid::now_v7();
    let sealed = state.biometric.cipher.seal(id, template.bytes());
    let inserted = sqlx::query(&format!(
        "INSERT INTO fingerprint_enrollments
           (id, user_id, finger, format, template, template_hash, quality, consent_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, now())
         RETURNING {ENROLLMENT_COLUMNS}"
    ))
    .bind(id)
    .bind(user_id)
    .bind(req.finger)
    .bind(template.format().as_str())
    .bind(&sealed)
    .bind(&hash)
    .bind(req.quality)
    .fetch_one(&mut *tx)
    .await;
    let row = match inserted {
        Err(sqlx::Error::Database(ref e)) if e.is_unique_violation() => {
            return Err(ApiError::Conflict(
                "this fingerprint is already enrolled".to_string(),
            ));
        }
        other => db(other)?,
    };

    if let MatcherBackend::Http(m) = &state.biometric.matcher {
        for old in &revoked {
            m.revoke(*old).await.map_err(matcher_error)?;
        }
        m.enroll(id, user_id, &template)
            .await
            .map_err(matcher_error)?;
    }
    db(tx.commit().await)?;
    metrics::counter!("biometric_enrollments_total").increment(1);
    tracing::info!(%user_id, enrollment_id = %id, finger = req.finger, "fingerprint enrolled");
    Ok((StatusCode::CREATED, Json(enrollment_row(&row)?)))
}

pub async fn list_fingerprints(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
) -> ApiResult<Json<Vec<EnrollmentResponse>>> {
    let rows = db(sqlx::query(&format!(
        "SELECT {ENROLLMENT_COLUMNS} FROM fingerprint_enrollments
         WHERE user_id = $1 AND revoked_at IS NULL ORDER BY finger"
    ))
    .bind(user_id)
    .fetch_all(state.ledger.pool())
    .await)?;
    rows.iter()
        .map(enrollment_row)
        .collect::<ApiResult<Vec<_>>>()
        .map(Json)
}

#[derive(Serialize)]
pub struct RevokeResponse {
    id: Uuid,
    status: String,
}

pub async fn revoke_fingerprint(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<RevokeResponse>> {
    let revoked: Option<Uuid> = db(sqlx::query_scalar(
        "UPDATE fingerprint_enrollments SET revoked_at = now()
         WHERE id = $1 AND user_id = $2 AND revoked_at IS NULL
         RETURNING id",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(state.ledger.pool())
    .await)?;
    if revoked.is_none() {
        return Err(ApiError::NotFound("enrollment not found".to_string()));
    }
    if let MatcherBackend::Http(m) = &state.biometric.matcher {
        if let Err(e) = m.revoke(id).await {
            tracing::warn!(enrollment_id = %id, error = %e, "matcher revoke failed; row is revoked and hits for it are ignored");
        }
    }
    tracing::info!(%user_id, enrollment_id = %id, "fingerprint revoked");
    Ok(Json(RevokeResponse {
        id,
        status: "revoked".to_string(),
    }))
}

#[derive(Deserialize)]
pub struct CreateCheckRequest {
    account: Uuid,
    amount_minor: i64,
    #[serde(default = "default_currency")]
    currency: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    expires_in_secs: Option<i64>,
}

#[derive(Serialize)]
pub struct CheckResponse {
    id: Uuid,
    status: String,
    amount_minor: i64,
    currency: String,
    description: Option<String>,
    account: Uuid,
    transaction_id: Option<Uuid>,
    payer_name: Option<String>,
    merchant_name: Option<String>,
    created_at_ms: i64,
    expires_at_ms: i64,
    paid_at_ms: Option<i64>,
}

const CHECK_SELECT: &str =
    "SELECT c.id, c.merchant_user_id, c.merchant_account, c.amount_minor, c.currency,
            c.description, c.status, c.payer_user_id, c.transaction_id,
            (c.status = 'open' AND c.expires_at <= now()) AS lapsed,
            (EXTRACT(EPOCH FROM c.created_at) * 1000)::BIGINT AS created_ms,
            (EXTRACT(EPOCH FROM c.expires_at) * 1000)::BIGINT AS expires_ms,
            (EXTRACT(EPOCH FROM c.paid_at) * 1000)::BIGINT AS paid_ms,
            k.full_name AS payer_name, m.full_name AS merchant_name
     FROM checks c
     LEFT JOIN LATERAL (
         SELECT full_name FROM kyc_submissions
         WHERE user_id = c.payer_user_id AND status = 'approved'
         ORDER BY reviewed_at DESC NULLS LAST, created_at DESC LIMIT 1
     ) k ON TRUE
     LEFT JOIN LATERAL (
         SELECT full_name FROM kyc_submissions
         WHERE user_id = c.merchant_user_id AND status = 'approved'
         ORDER BY reviewed_at DESC NULLS LAST, created_at DESC LIMIT 1
     ) m ON TRUE";

fn check_row(row: &PgRow) -> ApiResult<CheckResponse> {
    let lapsed: bool = db(row.try_get("lapsed"))?;
    let status: String = db(row.try_get("status"))?;
    Ok(CheckResponse {
        id: db(row.try_get("id"))?,
        status: if lapsed {
            "expired".to_string()
        } else {
            status
        },
        amount_minor: db(row.try_get("amount_minor"))?,
        currency: db(row.try_get("currency"))?,
        description: db(row.try_get("description"))?,
        account: db(row.try_get("merchant_account"))?,
        transaction_id: db(row.try_get("transaction_id"))?,
        payer_name: db(row.try_get("payer_name"))?,
        merchant_name: db(row.try_get("merchant_name"))?,
        created_at_ms: db(row.try_get("created_ms"))?,
        expires_at_ms: db(row.try_get("expires_ms"))?,
        paid_at_ms: db(row.try_get("paid_ms"))?,
    })
}

pub async fn create_check(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(req): Json<CreateCheckRequest>,
) -> ApiResult<(StatusCode, Json<CheckResponse>)> {
    let key = idempotency_key(&headers)?;
    if req.amount_minor <= 0 {
        return Err(ApiError::BadRequest(
            "amount_minor must be positive".to_string(),
        ));
    }
    let ttl = req
        .expires_in_secs
        .unwrap_or(state.biometric.check_ttl_secs);
    if ttl < MIN_CHECK_TTL_SECS || ttl > state.biometric.max_check_ttl_secs {
        return Err(ApiError::BadRequest(format!(
            "expires_in_secs must be between {MIN_CHECK_TTL_SECS} and {}",
            state.biometric.max_check_ttl_secs
        )));
    }
    let description = req
        .description
        .as_deref()
        .map(str::trim)
        .filter(|d| !d.is_empty())
        .map(str::to_string);
    if description
        .as_ref()
        .is_some_and(|d| d.chars().count() > MAX_DESCRIPTION_CHARS)
    {
        return Err(ApiError::BadRequest(format!(
            "description must be at most {MAX_DESCRIPTION_CHARS} characters"
        )));
    }

    let pool = state.ledger.pool();
    let ctx = db(sqlx::query(
        "SELECT u.status, u.kyc_level, a.account_type, a.owner_user_id, a.currency
         FROM users u LEFT JOIN accounts a ON a.id = $2
         WHERE u.id = $1",
    )
    .bind(user_id)
    .bind(req.account)
    .fetch_optional(pool)
    .await)?
    .ok_or_else(|| ApiError::Unauthorized("unknown user".to_string()))?;
    let status: String = db(ctx.try_get("status"))?;
    if status != "active" {
        return Err(ApiError::Forbidden(format!("account is {status}")));
    }
    let kyc_level: i16 = db(ctx.try_get("kyc_level"))?;
    if kyc_level < KYC_LEVEL_FOR_TRANSFER {
        return Err(ApiError::KycRequired(format!(
            "collecting payments requires KYC level {KYC_LEVEL_FOR_TRANSFER}"
        )));
    }
    let account_type: Option<String> = db(ctx.try_get("account_type"))?;
    let Some(account_type) = account_type else {
        return Err(ApiError::NotFound("account not found".to_string()));
    };
    let owner: Option<Uuid> = db(ctx.try_get("owner_user_id"))?;
    if owner != Some(user_id) {
        return Err(ApiError::Forbidden(
            "you do not own this account".to_string(),
        ));
    }
    if AccountType::from_db_str(&account_type) != Some(AccountType::UserWallet) {
        return Err(ApiError::BadRequest(
            "checks must be paid into a user wallet".to_string(),
        ));
    }
    let currency: String = db(ctx.try_get("currency"))?;
    if currency != req.currency {
        return Err(ApiError::BadRequest(format!(
            "wallet holds {currency}, not {}",
            req.currency
        )));
    }

    let inserted = db(sqlx::query_scalar::<_, Uuid>(
        "INSERT INTO checks
           (id, merchant_user_id, merchant_account, amount_minor, currency, description, expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, now() + make_interval(secs => $7))
         ON CONFLICT (id) DO NOTHING
         RETURNING id",
    )
    .bind(key)
    .bind(user_id)
    .bind(req.account)
    .bind(req.amount_minor)
    .bind(&currency)
    .bind(&description)
    .bind(ttl as f64)
    .fetch_optional(pool)
    .await)?;

    let row = db(sqlx::query(&format!("{CHECK_SELECT} WHERE c.id = $1"))
        .bind(key)
        .fetch_one(pool)
        .await)?;
    let merchant: Uuid = db(row.try_get("merchant_user_id"))?;
    let stored_amount: i64 = db(row.try_get("amount_minor"))?;
    let stored_account: Uuid = db(row.try_get("merchant_account"))?;
    if merchant != user_id || stored_amount != req.amount_minor || stored_account != req.account {
        return Err(ApiError::IdempotencyConflict);
    }
    let created = inserted.is_some();
    if created {
        metrics::counter!("checks_created_total").increment(1);
    }
    Ok((
        if created {
            StatusCode::CREATED
        } else {
            StatusCode::OK
        },
        Json(check_row(&row)?),
    ))
}

pub async fn get_check(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<CheckResponse>> {
    let row = db(sqlx::query(&format!(
        "{CHECK_SELECT} WHERE c.id = $1
           AND (c.merchant_user_id = $2 OR c.payer_user_id = $2
                OR (c.status = 'open' AND c.expires_at > now()))"
    ))
    .bind(id)
    .bind(user_id)
    .fetch_optional(state.ledger.pool())
    .await)?
    .ok_or_else(|| ApiError::NotFound("check not found".to_string()))?;
    Ok(Json(check_row(&row)?))
}

#[derive(Deserialize)]
pub struct ListChecksParams {
    status: Option<String>,
    limit: Option<i64>,
}

pub async fn list_checks(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Query(params): Query<ListChecksParams>,
) -> ApiResult<Json<Vec<CheckResponse>>> {
    let limit = params.limit.unwrap_or(20).clamp(1, 100);
    let filter = match params.status.as_deref() {
        None => "",
        Some("open") => " AND c.status = 'open' AND c.expires_at > now()",
        Some("expired") => {
            " AND (c.status = 'expired' OR (c.status = 'open' AND c.expires_at <= now()))"
        }
        Some("paid") => " AND c.status = 'paid'",
        Some("cancelled") => " AND c.status = 'cancelled'",
        Some(_) => {
            return Err(ApiError::BadRequest(
                "status must be open, paid, cancelled or expired".to_string(),
            ))
        }
    };
    let rows = db(sqlx::query(&format!(
        "{CHECK_SELECT} WHERE c.merchant_user_id = $1{filter}
         ORDER BY c.created_at DESC, c.id DESC LIMIT $2"
    ))
    .bind(user_id)
    .bind(limit)
    .fetch_all(state.ledger.pool())
    .await)?;
    rows.iter()
        .map(check_row)
        .collect::<ApiResult<Vec<_>>>()
        .map(Json)
}

pub async fn cancel_check(
    AuthUser(user_id): AuthUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<CheckResponse>> {
    let pool = state.ledger.pool();
    let updated: Option<Uuid> = db(sqlx::query_scalar(
        "UPDATE checks SET status = 'cancelled', cancelled_at = now()
         WHERE id = $1 AND merchant_user_id = $2 AND status = 'open' AND expires_at > now()
         RETURNING id",
    )
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await)?;
    let row = db(sqlx::query(&format!(
        "{CHECK_SELECT} WHERE c.id = $1 AND c.merchant_user_id = $2"
    ))
    .bind(id)
    .bind(user_id)
    .fetch_optional(pool)
    .await)?
    .ok_or_else(|| ApiError::NotFound("check not found".to_string()))?;
    let resp = check_row(&row)?;
    if updated.is_none() {
        return Err(ApiError::Conflict(format!("check is {}", resp.status)));
    }
    Ok(Json(resp))
}

#[derive(Deserialize)]
pub struct ProbeRequest {
    format: String,
    template: String,
}

#[derive(Deserialize, Default)]
pub struct PayCheckRequest {
    #[serde(default)]
    account: Option<Uuid>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PayCheckResponse {
    transaction_id: Uuid,
    status: String,
    check_id: Uuid,
    amount_minor: i64,
    currency: String,
    payer_name: Option<String>,
}

struct CheckCtx {
    id: Uuid,
    merchant_user_id: Uuid,
    merchant_account: Uuid,
    amount_minor: i64,
    currency: String,
    status: String,
    lapsed: bool,
    stored: Option<(String, i32, serde_json::Value)>,
}

async fn load_check(
    conn: &mut PgConnection,
    check_id: Uuid,
    key: Uuid,
) -> ApiResult<Option<CheckCtx>> {
    let row = db(sqlx::query(
        "SELECT c.merchant_user_id, c.merchant_account, c.amount_minor, c.currency, c.status,
                (c.expires_at <= now()) AS lapsed,
                ik.fingerprint AS ik_fingerprint, ik.response_status AS ik_status,
                ik.response_body AS ik_body
         FROM checks c
         LEFT JOIN idempotency_keys ik ON ik.key = $2
         WHERE c.id = $1",
    )
    .bind(check_id)
    .bind(key)
    .fetch_optional(&mut *conn)
    .await)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let ik_fingerprint: Option<String> = db(row.try_get("ik_fingerprint"))?;
    let stored = match ik_fingerprint {
        Some(fp) => Some((
            fp,
            db(row.try_get::<i32, _>("ik_status"))?,
            db(row.try_get::<serde_json::Value, _>("ik_body"))?,
        )),
        None => None,
    };
    Ok(Some(CheckCtx {
        id: check_id,
        merchant_user_id: db(row.try_get("merchant_user_id"))?,
        merchant_account: db(row.try_get("merchant_account"))?,
        amount_minor: db(row.try_get("amount_minor"))?,
        currency: db(row.try_get("currency"))?,
        status: db(row.try_get("status"))?,
        lapsed: db(row.try_get("lapsed"))?,
        stored,
    }))
}

impl CheckCtx {
    fn replay(&self, fingerprint: &str) -> ApiResult<Option<(StatusCode, Json<PayCheckResponse>)>> {
        let Some((stored_fp, status, body)) = &self.stored else {
            return Ok(None);
        };
        if stored_fp != fingerprint {
            return Err(ApiError::IdempotencyConflict);
        }
        let resp: PayCheckResponse = serde_json::from_value(body.clone())
            .map_err(|e| StorageError::DataIntegrity(format!("stored idempotent response: {e}")))?;
        let status = StatusCode::from_u16(*status as u16)
            .map_err(|e| StorageError::DataIntegrity(format!("stored status: {e}")))?;
        Ok(Some((status, Json(resp))))
    }

    async fn require_open(&self, conn: &mut PgConnection) -> ApiResult<()> {
        if self.status != "open" {
            return Err(ApiError::Conflict(format!("check is {}", self.status)));
        }
        if self.lapsed {
            db(sqlx::query(
                "UPDATE checks SET status = 'expired' WHERE id = $1 AND status = 'open'",
            )
            .bind(self.id)
            .execute(&mut *conn)
            .await)?;
            return Err(ApiError::Conflict("check has expired".to_string()));
        }
        Ok(())
    }
}

struct PaidEvent {
    terminal: Uuid,
    probe_hash: Vec<u8>,
    score: f64,
}

struct Settlement<'a> {
    key: Uuid,
    fingerprint: &'a str,
    check: &'a CheckCtx,
    payer_id: Uuid,
    wallet: Option<Uuid>,
    method: &'static str,
    event: Option<PaidEvent>,
}

async fn settle_check(
    conn: &mut PgConnection,
    state: &AppState,
    s: Settlement<'_>,
) -> ApiResult<(StatusCode, Json<PayCheckResponse>)> {
    let check = s.check;
    let (check_id, key, payer_id, merchant_account, amount_minor) = (
        check.id,
        s.key,
        s.payer_id,
        check.merchant_account,
        check.amount_minor,
    );
    let wallet = db(sqlx::query(
        "SELECT a.id, b.raw_minor, k.full_name
         FROM accounts a
         JOIN balances b ON b.account_id = a.id
         LEFT JOIN LATERAL (
             SELECT full_name FROM kyc_submissions
             WHERE user_id = $1 AND status = 'approved'
             ORDER BY reviewed_at DESC NULLS LAST, created_at DESC LIMIT 1
         ) k ON TRUE
         WHERE a.owner_user_id = $1 AND a.currency = $2 AND a.account_type = 'user_wallet'
           AND ($3::uuid IS NULL OR a.id = $3::uuid)
         ORDER BY b.raw_minor DESC, a.created_at ASC
         LIMIT 1",
    )
    .bind(payer_id)
    .bind(&check.currency)
    .bind(s.wallet)
    .fetch_optional(&mut *conn)
    .await)?;
    let Some(wallet) = wallet else {
        return Err(match s.wallet {
            Some(_) => {
                ApiError::BadRequest(format!("account is not your {} wallet", check.currency))
            }
            None => ApiError::InsufficientFunds(format!("payer has no {} wallet", check.currency)),
        });
    };
    let payer_account: Uuid = db(wallet.try_get("id"))?;
    let raw_minor: i64 = db(wallet.try_get("raw_minor"))?;
    let payer_name: Option<String> = db(wallet.try_get("full_name"))?;
    if raw_minor < amount_minor {
        return Err(ApiError::InsufficientFunds(
            "payer has insufficient funds".to_string(),
        ));
    }

    let ctx = money_context(&mut *conn, payer_id, payer_account, merchant_account, key).await?;
    ctx.require_active()?;
    ctx.require_kyc(KYC_LEVEL_FOR_TRANSFER)?;
    let from = ctx
        .from
        .as_ref()
        .ok_or_else(|| ApiError::NotFound("account not found".to_string()))?;
    let to = ctx
        .to
        .as_ref()
        .ok_or_else(|| ApiError::NotFound("merchant account not found".to_string()))?;
    if to.currency != from.currency {
        return Err(ApiError::BadRequest(
            "merchant wallet currency no longer matches the check".to_string(),
        ));
    }
    let currency = from.currency;

    let screen = ScreenCtx {
        user_id: payer_id,
        from_account: payer_account,
        to_account: merchant_account,
        amount_minor,
        currency: currency.code().to_string(),
    };
    let (tjs, limits) = pre_screen(&mut *conn, state, &ctx, &screen).await?;

    let amount = Money::from_minor(amount_minor as i128, currency);
    let fee_minor = if currency.code() == "TJS" {
        state.fees.fee_minor(amount_minor)
    } else {
        0
    };
    let entries = if fee_minor > 0 {
        let to_merchant = amount_minor
            .checked_sub(fee_minor)
            .filter(|v| *v > 0)
            .ok_or_else(|| ApiError::BadRequest("amount does not cover the fee".to_string()))?;
        let fee_account = fee_shard(currency.code())
            .ok_or_else(|| ApiError::Internal("no fee account for currency".to_string()))?;
        vec![
            Entry::debit(AccountId(payer_account), amount),
            Entry::credit(
                AccountId(merchant_account),
                Money::from_minor(to_merchant as i128, currency),
            ),
            Entry::credit(
                AccountId(fee_account),
                Money::from_minor(fee_minor as i128, currency),
            ),
        ]
    } else {
        vec![
            Entry::debit(AccountId(payer_account), amount),
            Entry::credit(AccountId(merchant_account), amount),
        ]
    };
    let txn = Transaction::new(TransactionId(key), entries);

    let created = PayCheckResponse {
        transaction_id: key,
        status: "posted".to_string(),
        check_id,
        amount_minor,
        currency: currency.code().to_string(),
        payer_name,
    };
    let raced = PayCheckResponse {
        status: "already_posted".to_string(),
        ..created.clone()
    };

    let aml = aml_guard(screen.clone(), tjs, limits);
    let method = s.method;
    let event = s.event;
    let guard: PostHook = Box::new(move |conn: &mut PgConnection| {
        Box::pin(async move {
            let updated = sqlx::query(
                "UPDATE checks
                 SET status = 'paid', payer_user_id = $2, payer_account = $3,
                     transaction_id = $4, method = $5, paid_at = now()
                 WHERE id = $1 AND status = 'open' AND expires_at > now()",
            )
            .bind(check_id)
            .bind(payer_id)
            .bind(payer_account)
            .bind(key)
            .bind(method)
            .execute(&mut *conn)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(HookError::Rejected {
                    rule: "check_not_open".to_string(),
                    message: "check is no longer open".to_string(),
                });
            }
            if let Some(ev) = event {
                sqlx::query(
                    "INSERT INTO biometric_events
                       (id, check_id, terminal_user_id, matched_user_id, outcome, score, probe_hash, detail)
                     VALUES ($1, $2, $3, $4, 'paid', $5, $6, NULL)",
                )
                .bind(Uuid::now_v7())
                .bind(check_id)
                .bind(ev.terminal)
                .bind(payer_id)
                .bind(ev.score)
                .bind(&ev.probe_hash)
                .execute(&mut *conn)
                .await?;
            }
            aml(conn).await
        })
    });
    let opts = PostOptions {
        idempotency: Some(record(key, s.fingerprint, &created)?),
        guard: Some(guard),
    };
    let result = state.ledger.post_on(&mut *conn, &txn, opts).await;
    if let Err(StorageError::Rejected { rule, .. }) = &result {
        if rule == "check_not_open" {
            return Err(ApiError::Conflict("check is no longer open".to_string()));
        }
    }
    let out = finish(
        &mut *conn,
        result,
        key,
        s.fingerprint,
        created,
        raced,
        Some(&screen),
    )
    .await?;
    metrics::counter!("checks_paid_total", "method" => method).increment(1);
    tracing::info!(%check_id, %payer_id, method, amount_minor, "check paid");
    Ok(out)
}

pub async fn pay_check(
    AuthUser(payer_id): AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(check_id): Path<Uuid>,
    body: Option<Json<PayCheckRequest>>,
) -> ApiResult<(StatusCode, Json<PayCheckResponse>)> {
    let key = idempotency_key(&headers)?;
    let req = body.map(|Json(b)| b).unwrap_or_default();
    let fingerprint = format!(
        "check_pay_app:{check_id}:{payer_id}:{}",
        req.account.map(|a| a.to_string()).unwrap_or_default()
    );
    let mut conn = db(state.ledger.pool().acquire().await)?;
    let check = load_check(&mut conn, check_id, key)
        .await?
        .ok_or_else(|| ApiError::NotFound("check not found".to_string()))?;
    if let Some(found) = check.replay(&fingerprint)? {
        return Ok(found);
    }
    check.require_open(&mut conn).await?;
    if check.merchant_user_id == payer_id {
        return Err(ApiError::BadRequest(
            "a check cannot be paid by its own merchant".to_string(),
        ));
    }
    settle_check(
        &mut conn,
        &state,
        Settlement {
            key,
            fingerprint: &fingerprint,
            check: &check,
            payer_id,
            wallet: req.account,
            method: "app",
            event: None,
        },
    )
    .await
}

struct Attempt {
    check_id: Uuid,
    terminal: Uuid,
    probe_hash: Vec<u8>,
}

async fn log_event(
    conn: &mut PgConnection,
    a: &Attempt,
    matched: Option<Uuid>,
    outcome: &str,
    score: Option<f64>,
    detail: Option<&str>,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO biometric_events
           (id, check_id, terminal_user_id, matched_user_id, outcome, score, probe_hash, detail)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
    )
    .bind(Uuid::now_v7())
    .bind(a.check_id)
    .bind(a.terminal)
    .bind(matched)
    .bind(outcome)
    .bind(score)
    .bind(&a.probe_hash)
    .bind(detail)
    .execute(&mut *conn)
    .await?;
    metrics::counter!("biometric_identify_total", "outcome" => outcome.to_string()).increment(1);
    Ok(())
}

async fn refuse(
    conn: &mut PgConnection,
    a: &Attempt,
    matched: Option<Uuid>,
    outcome: &str,
    score: Option<f64>,
    err: ApiError,
) -> ApiError {
    if let Err(e) = log_event(conn, a, matched, outcome, score, Some(&err.to_string())).await {
        return StorageError::from(e).into();
    }
    err
}

async fn identify(
    conn: &mut PgConnection,
    state: &AppState,
    probe: &Template,
    probe_hash: &[u8],
) -> ApiResult<Vec<Candidate>> {
    match &state.biometric.matcher {
        MatcherBackend::Exact => {
            let subject: Option<Uuid> = db(sqlx::query_scalar(
                "SELECT user_id FROM fingerprint_enrollments
                 WHERE template_hash = $1 AND revoked_at IS NULL",
            )
            .bind(probe_hash)
            .fetch_optional(&mut *conn)
            .await)?;
            Ok(subject
                .map(|subject| Candidate {
                    subject,
                    score: EXACT_SCORE,
                })
                .into_iter()
                .collect())
        }
        MatcherBackend::Http(m) => {
            let hits = m
                .identify(probe, IDENTIFY_LIMIT)
                .await
                .map_err(matcher_error)?;
            if hits.is_empty() {
                return Ok(Vec::new());
            }
            let ids: Vec<Uuid> = hits.iter().map(|h| h.enrollment_id).collect();
            let rows = db(sqlx::query(
                "SELECT id, user_id FROM fingerprint_enrollments
                 WHERE id = ANY($1) AND revoked_at IS NULL",
            )
            .bind(&ids)
            .fetch_all(&mut *conn)
            .await)?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let id: Uuid = db(row.try_get("id"))?;
                let subject: Uuid = db(row.try_get("user_id"))?;
                if let Some(h) = hits.iter().find(|h| h.enrollment_id == id) {
                    out.push(Candidate {
                        subject,
                        score: h.score,
                    });
                }
            }
            Ok(out)
        }
    }
}

pub async fn pay_check_fingerprint(
    AuthUser(merchant_id): AuthUser,
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(check_id): Path<Uuid>,
    Json(req): Json<ProbeRequest>,
) -> ApiResult<(StatusCode, Json<PayCheckResponse>)> {
    let key = idempotency_key(&headers)?;
    let probe = Template::from_base64(&req.format, &req.template)
        .map_err(|e| ApiError::BadRequest(e.to_string()))?;
    let attempt = Attempt {
        check_id,
        terminal: merchant_id,
        probe_hash: probe.hash().to_vec(),
    };
    let fingerprint = format!("check_pay:{check_id}:{merchant_id}");

    let mut conn = db(state.ledger.pool().acquire().await)?;
    let check = load_check(&mut conn, check_id, key)
        .await?
        .filter(|c| c.merchant_user_id == merchant_id)
        .ok_or_else(|| ApiError::NotFound("check not found".to_string()))?;
    if let Some(found) = check.replay(&fingerprint)? {
        return Ok(found);
    }
    check.require_open(&mut conn).await?;
    if check.amount_minor > state.biometric.max_minor {
        let err = ApiError::LimitExceeded(format!(
            "amount exceeds the fingerprint payment limit of {} minor units",
            state.biometric.max_minor
        ));
        return Err(refuse(&mut conn, &attempt, None, "rejected", None, err).await);
    }

    let candidates = match identify(&mut conn, &state, &probe, &attempt.probe_hash).await {
        Ok(c) => c,
        Err(e) => {
            return Err(refuse(&mut conn, &attempt, None, "matcher_error", None, e).await);
        }
    };
    let (payer_id, score) = match decide(&candidates, state.biometric.policy) {
        Decision::Match { subject, score } => (subject, score),
        Decision::NoMatch => {
            let best = candidates
                .iter()
                .map(|c| c.score)
                .fold(None, |acc: Option<f64>, s| {
                    Some(acc.map_or(s, |a| a.max(s)))
                });
            return Err(refuse(
                &mut conn,
                &attempt,
                None,
                "no_match",
                best,
                ApiError::NoMatch,
            )
            .await);
        }
        Decision::Ambiguous { best, .. } => {
            return Err(refuse(
                &mut conn,
                &attempt,
                Some(best),
                "ambiguous",
                None,
                ApiError::AmbiguousMatch,
            )
            .await);
        }
    };
    if payer_id == merchant_id {
        let err = ApiError::BadRequest("a check cannot be paid by its own merchant".to_string());
        return Err(refuse(
            &mut conn,
            &attempt,
            Some(payer_id),
            "rejected",
            Some(score),
            err,
        )
        .await);
    }

    let settled = settle_check(
        &mut conn,
        &state,
        Settlement {
            key,
            fingerprint: &fingerprint,
            check: &check,
            payer_id,
            wallet: None,
            method: "fingerprint",
            event: Some(PaidEvent {
                terminal: merchant_id,
                probe_hash: attempt.probe_hash.clone(),
                score,
            }),
        },
    )
    .await;
    match settled {
        Ok(out) => {
            metrics::counter!("biometric_identify_total", "outcome" => "paid").increment(1);
            Ok(out)
        }
        Err(e) => Err(refuse(
            &mut conn,
            &attempt,
            Some(payer_id),
            "rejected",
            Some(score),
            e,
        )
        .await),
    }
}
