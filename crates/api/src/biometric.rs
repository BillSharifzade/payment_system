use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use biometric::{
    decide, Candidate, Decision, HttpMatcher, MatchPolicy, MatcherError, Template, TemplateCipher,
    DEFAULT_IDENTIFY_SCALE,
};
use ledger::{AccountId, AccountType, Entry, Transaction, TransactionId};
use money::Money;
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::{PgConnection, Row};
use storage::{HookError, PostHook, PostOptions, StorageError};
use uuid::Uuid;

use crate::common::{default_currency, idempotency_key, normalize_phone};
use crate::config::{env_bool, env_or, env_or_file};
use crate::payments::{
    aml_check, fee_shard, finish, lock_user, money_context, pre_screen, record, KeyState,
    ScreenCtx, KEY_STATE_COLUMNS, KYC_LEVEL_FOR_TRANSFER,
};
use crate::session::AuthUser;
use crate::terminals;
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
    pub identify: bool,
    pub identify_scale: f64,
    pub max_attempts: i32,
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
            identify: false,
            identify_scale: DEFAULT_IDENTIFY_SCALE,
            max_attempts: 5,
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
            "exact" if is_prod => {
                return Err(
                    "BIOMETRIC_MATCHER=exact (the default) only recognises byte-identical templates and is refused when APP_ENV != dev; set BIOMETRIC_MATCHER=http and BIOMETRIC_MATCHER_URL"
                        .to_string(),
                )
            }
            "exact" => MatcherBackend::Exact,
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
        let identify = env_bool("BIOMETRIC_IDENTIFY", d.identify)?;
        let identify_scale: f64 = env_or("BIOMETRIC_IDENTIFY_SCALE", d.identify_scale)?;
        if !(identify_scale.is_finite() && identify_scale >= 0.0) {
            return Err("BIOMETRIC_IDENTIFY_SCALE must be non-negative".to_string());
        }
        let max_attempts: i32 = env_or("BIOMETRIC_MAX_ATTEMPTS", d.max_attempts)?;
        if max_attempts < 1 {
            return Err("BIOMETRIC_MAX_ATTEMPTS must be at least 1".to_string());
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
            identify,
            identify_scale,
            max_attempts,
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

// Merchants see who paid, not the payer's full legal name: first given name plus the initial
// of the family name ("Bilal S.").
pub(crate) fn mask_name(full: &str) -> Option<String> {
    let mut parts = full.split_whitespace();
    let first = parts.next()?;
    let initial = parts.last().and_then(|family| family.chars().next());
    Some(match initial {
        Some(initial) => format!("{first} {}.", initial.to_uppercase()),
        None => first.to_string(),
    })
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

async fn forget(state: &AppState, enrollment_id: Uuid) {
    if let MatcherBackend::Http(m) = &state.biometric.matcher {
        if let Err(e) = m.revoke(enrollment_id).await {
            tracing::warn!(%enrollment_id, error = %e, "matcher revoke failed; the row is not live, so hits for it are ignored");
        }
    }
}

// No row lock or open transaction is held across a matcher call (a slow matcher must not stall
// the user's payments, whose AML guard locks the users row). The gallery is pushed before the
// row exists and old templates are removed after their rows are revoked, so in every
// interleaving or failure the gallery can only hold ids that are not live — hits for those are
// ignored — or lack a live one: a stale gallery can only miss, never pay the wrong person.
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
    let pool = state.ledger.pool();

    let ctx = db(sqlx::query(
        "SELECT u.status, u.kyc_level, e.user_id AS holder, e.id, e.finger, e.format, e.quality,
                (EXTRACT(EPOCH FROM e.created_at) * 1000)::BIGINT AS ms
         FROM users u
         LEFT JOIN fingerprint_enrollments e ON e.template_hash = $2 AND e.revoked_at IS NULL
         WHERE u.id = $1",
    )
    .bind(user_id)
    .bind(&hash)
    .fetch_optional(pool)
    .await)?
    .ok_or_else(|| ApiError::Unauthorized("unknown user".to_string()))?;
    let status: String = db(ctx.try_get("status"))?;
    let kyc_level: i16 = db(ctx.try_get("kyc_level"))?;
    if status != "active" {
        return Err(ApiError::Forbidden(format!("account is {status}")));
    }
    if kyc_level < KYC_LEVEL_FOR_ENROLLMENT {
        return Err(ApiError::KycRequired(format!(
            "fingerprint enrolment requires KYC level {KYC_LEVEL_FOR_ENROLLMENT}"
        )));
    }
    let holder: Option<Uuid> = db(ctx.try_get("holder"))?;
    if let Some(holder) = holder {
        let finger: i16 = db(ctx.try_get("finger"))?;
        if holder != user_id || finger != req.finger {
            return Err(ApiError::Conflict(
                "this fingerprint is already enrolled".to_string(),
            ));
        }
        let existing = enrollment_row(&ctx)?;
        if let MatcherBackend::Http(m) = &state.biometric.matcher {
            m.enroll(existing.id, user_id, &template)
                .await
                .map_err(matcher_error)?;
        }
        return Ok((StatusCode::OK, Json(existing)));
    }

    let id = Uuid::now_v7();
    if let MatcherBackend::Http(m) = &state.biometric.matcher {
        m.enroll(id, user_id, &template)
            .await
            .map_err(matcher_error)?;
    }
    let sealed = state.biometric.cipher.seal(id, template.bytes());
    let stored = async {
        let mut tx = pool.begin().await?;
        let revoked: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE fingerprint_enrollments SET revoked_at = now()
             WHERE user_id = $1 AND finger = $2 AND revoked_at IS NULL
             RETURNING id",
        )
        .bind(user_id)
        .bind(req.finger)
        .fetch_all(&mut *tx)
        .await?;
        let row = sqlx::query(&format!(
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
        .await?;
        tx.commit().await?;
        Ok::<_, sqlx::Error>((revoked, row))
    }
    .await;
    let (revoked, row) = match stored {
        Ok(v) => v,
        Err(e) => {
            forget(&state, id).await;
            return Err(match e {
                sqlx::Error::Database(ref d) if d.is_unique_violation() => {
                    ApiError::Conflict("this fingerprint is already enrolled".to_string())
                }
                other => StorageError::from(other).into(),
            });
        }
    };
    for old in revoked {
        forget(&state, old).await;
    }
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
    forget(&state, id).await;
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
    let payer_name: Option<String> = db(row.try_get("payer_name"))?;
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
        payer_name: payer_name.as_deref().and_then(mask_name),
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
    #[serde(default)]
    payer_phone: Option<String>,
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
    failed_attempts: i32,
    key: KeyState,
    terminals: Vec<(Uuid, Vec<u8>)>,
}

async fn load_check(
    conn: &mut PgConnection,
    check_id: Uuid,
    key: Uuid,
    with_terminals: bool,
) -> ApiResult<Option<CheckCtx>> {
    let row = db(sqlx::query(&format!(
        "SELECT c.merchant_user_id, c.merchant_account, c.amount_minor, c.currency, c.status,
                c.failed_attempts, (c.expires_at <= now()) AS lapsed,
                COALESCE(tm.ids, '{{}}') AS terminal_ids,
                COALESCE(tm.hashes, '{{}}') AS terminal_hashes,
                {KEY_STATE_COLUMNS}
         FROM checks c
         LEFT JOIN LATERAL (
             SELECT array_agg(id) AS ids, array_agg(key_hash) AS hashes FROM terminals
             WHERE merchant_user_id = c.merchant_user_id AND revoked_at IS NULL AND $3
         ) tm ON TRUE
         LEFT JOIN idempotency_keys ik ON ik.key = $2
         LEFT JOIN voided_transactions vt ON vt.id = $2
         WHERE c.id = $1"
    ))
    .bind(check_id)
    .bind(key)
    .bind(with_terminals)
    .fetch_optional(&mut *conn)
    .await)?;
    let Some(row) = row else {
        return Ok(None);
    };
    let ids: Vec<Uuid> = db(row.try_get("terminal_ids"))?;
    let hashes: Vec<Vec<u8>> = db(row.try_get("terminal_hashes"))?;
    Ok(Some(CheckCtx {
        id: check_id,
        merchant_user_id: db(row.try_get("merchant_user_id"))?,
        merchant_account: db(row.try_get("merchant_account"))?,
        amount_minor: db(row.try_get("amount_minor"))?,
        currency: db(row.try_get("currency"))?,
        status: db(row.try_get("status"))?,
        lapsed: db(row.try_get("lapsed"))?,
        failed_attempts: db(row.try_get("failed_attempts"))?,
        key: KeyState::from_row(&row)?,
        terminals: ids.into_iter().zip(hashes).collect(),
    }))
}

impl CheckCtx {
    fn replay(&self, fingerprint: &str) -> ApiResult<Option<(StatusCode, Json<PayCheckResponse>)>> {
        self.key.replay(fingerprint)
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
    terminal_id: Uuid,
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
    let (check_id, key, payer_id, merchant_id, merchant_account, amount_minor) = (
        check.id,
        s.key,
        s.payer_id,
        check.merchant_user_id,
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
    let full_name: Option<String> = db(wallet.try_get("full_name"))?;
    let payer_name = full_name.as_deref().and_then(mask_name);
    if raw_minor < amount_minor {
        return Err(ApiError::InsufficientFunds(
            "payer has insufficient funds".to_string(),
        ));
    }

    let ctx = money_context(&mut *conn, payer_id, payer_account, merchant_account, key).await?;
    ctx.require_active()?;
    ctx.require_kyc(KYC_LEVEL_FOR_TRANSFER)?;
    ctx.require_recipient_active()?;
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

    let method = s.method;
    let event = s.event;
    let release_slot = i32::from(event.is_some());
    let aml_screen = screen.clone();
    let guard: PostHook = Box::new(move |conn: &mut PgConnection| {
        Box::pin(async move {
            lock_user(conn, payer_id).await?;
            let updated = sqlx::query(
                "UPDATE checks
                 SET status = 'paid', payer_user_id = $2, payer_account = $3,
                     transaction_id = $4, method = $5, paid_at = now(),
                     attempts_in_flight = GREATEST(attempts_in_flight - $6, 0)
                 WHERE id = $1 AND status = 'open' AND expires_at > now()",
            )
            .bind(check_id)
            .bind(payer_id)
            .bind(payer_account)
            .bind(key)
            .bind(method)
            .bind(release_slot)
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
                       (id, check_id, terminal_user_id, terminal_id, matched_user_id, outcome,
                        score, probe_hash, detail)
                     VALUES ($1, $2, $3, $4, $5, 'paid', $6, $7, NULL)",
                )
                .bind(Uuid::now_v7())
                .bind(check_id)
                .bind(merchant_id)
                .bind(ev.terminal_id)
                .bind(payer_id)
                .bind(ev.score)
                .bind(&ev.probe_hash)
                .execute(&mut *conn)
                .await?;
            }
            aml_check(conn, &aml_screen, tjs, limits).await
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
    let check = load_check(&mut conn, check_id, key, false)
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
    merchant: Uuid,
    terminal: Uuid,
    probe_hash: Vec<u8>,
}

// Attempt slots: every probe reserves one (attempts_in_flight + 1, only while failed + in
// flight is below the cap) BEFORE the matcher sees it, so concurrent probes can never get more
// than BIOMETRIC_MAX_ATTEMPTS evaluations of one check. A failed identification turns its slot
// into a failure; any other end (matcher down, payer refused after a match, payment posted)
// just gives the slot back. A request dropped mid-attempt (timeout) leaves its slot taken,
// which can only make the check stricter until it expires.
async fn log_event(
    conn: &mut PgConnection,
    a: &Attempt,
    (matched, outcome, score): (Option<Uuid>, &str, Option<f64>),
    detail: Option<&str>,
    release_slot: bool,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "WITH released AS (
             UPDATE checks SET attempts_in_flight = GREATEST(attempts_in_flight - 1, 0)
             WHERE id = $2 AND $10
         )
         INSERT INTO biometric_events
           (id, check_id, terminal_user_id, terminal_id, matched_user_id, outcome, score,
            probe_hash, detail)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)",
    )
    .bind(Uuid::now_v7())
    .bind(a.check_id)
    .bind(a.merchant)
    .bind(a.terminal)
    .bind(matched)
    .bind(outcome)
    .bind(score)
    .bind(&a.probe_hash)
    .bind(detail)
    .bind(release_slot)
    .execute(&mut *conn)
    .await?;
    metrics::counter!("biometric_identify_total", "outcome" => outcome.to_string()).increment(1);
    Ok(())
}

async fn refuse(
    conn: &mut PgConnection,
    a: &Attempt,
    event: (Option<Uuid>, &str, Option<f64>),
    err: ApiError,
    release_slot: bool,
) -> ApiError {
    let detail = err.to_string();
    if let Err(e) = log_event(conn, a, event, Some(&detail), release_slot).await {
        return StorageError::from(e).into();
    }
    err
}

// No slot: cancel the check if its failures are exhausted; otherwise every remaining slot is
// taken by a concurrent probe, or the check stopped being open under us.
async fn locked_out(conn: &mut PgConnection, check_id: Uuid, max: i32) -> ApiResult<ApiError> {
    let row = db(sqlx::query(
        "UPDATE checks
         SET status = CASE WHEN status = 'open' AND failed_attempts >= $2
                           THEN 'cancelled' ELSE status END,
             cancelled_at = CASE WHEN status = 'open' AND failed_attempts >= $2
                                 THEN now() ELSE cancelled_at END
         WHERE id = $1
         RETURNING status, failed_attempts >= $2 AS exhausted",
    )
    .bind(check_id)
    .bind(max)
    .fetch_one(&mut *conn)
    .await)?;
    if db(row.try_get("exhausted"))? {
        return Ok(ApiError::CheckLocked);
    }
    let status: String = db(row.try_get("status"))?;
    Ok(ApiError::Conflict(if status == "open" {
        "another fingerprint attempt for this check is in progress".to_string()
    } else {
        format!("check is {status}")
    }))
}

// The failure that reaches the cap cancels the check in the statement that records it.
async fn fail(
    conn: &mut PgConnection,
    state: &AppState,
    a: &Attempt,
    (matched, outcome): (Option<Uuid>, &str),
    score: Option<f64>,
    err: ApiError,
) -> ApiError {
    let locked = sqlx::query_scalar::<_, bool>(
        "WITH ev AS (
             INSERT INTO biometric_events
               (id, check_id, terminal_user_id, terminal_id, matched_user_id, outcome, score,
                probe_hash, detail)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         )
         UPDATE checks
         SET attempts_in_flight = GREATEST(attempts_in_flight - 1, 0),
             failed_attempts = failed_attempts + 1,
             status = CASE WHEN status = 'open' AND failed_attempts + 1 >= $10
                           THEN 'cancelled' ELSE status END,
             cancelled_at = CASE WHEN status = 'open' AND failed_attempts + 1 >= $10
                                 THEN now() ELSE cancelled_at END
         WHERE id = $2
         RETURNING failed_attempts >= $10",
    )
    .bind(Uuid::now_v7())
    .bind(a.check_id)
    .bind(a.merchant)
    .bind(a.terminal)
    .bind(matched)
    .bind(outcome)
    .bind(score)
    .bind(&a.probe_hash)
    .bind(err.to_string())
    .bind(state.biometric.max_attempts)
    .fetch_one(&mut *conn)
    .await;
    metrics::counter!("biometric_identify_total", "outcome" => outcome.to_string()).increment(1);
    match locked {
        Ok(false) => err,
        Ok(true) => {
            tracing::warn!(check_id = %a.check_id, terminal_id = %a.terminal, "fingerprint attempts exhausted; check cancelled");
            metrics::counter!("checks_locked_total").increment(1);
            ApiError::CheckLocked
        }
        Err(e) => StorageError::from(e).into(),
    }
}

// 1:N: who, among everyone enrolled, is this? The threshold grows with the gallery so the
// system-wide false-match rate stays at the 1:1 operating point (biometric::policy).
async fn identify(
    conn: &mut PgConnection,
    state: &AppState,
    probe: &Template,
    probe_hash: &[u8],
) -> ApiResult<(Vec<Candidate>, u64)> {
    const GALLERY: &str =
        "(SELECT COUNT(*) FROM fingerprint_enrollments WHERE revoked_at IS NULL) AS gallery";
    let (candidates, gallery): (Vec<Candidate>, i64) =
        match &state.biometric.matcher {
            MatcherBackend::Exact => {
                let row = db(sqlx::query(&format!(
                    "SELECT (SELECT user_id FROM fingerprint_enrollments
                         WHERE template_hash = $1 AND revoked_at IS NULL) AS subject, {GALLERY}"
                ))
                .bind(probe_hash)
                .fetch_one(&mut *conn)
                .await)?;
                let subject: Option<Uuid> = db(row.try_get("subject"))?;
                let candidates = subject
                    .map(|subject| Candidate {
                        subject,
                        score: EXACT_SCORE,
                    })
                    .into_iter()
                    .collect();
                (candidates, db(row.try_get("gallery"))?)
            }
            MatcherBackend::Http(m) => {
                let hits = m
                    .identify(probe, IDENTIFY_LIMIT)
                    .await
                    .map_err(matcher_error)?;
                let ids: Vec<Uuid> = hits.iter().map(|h| h.enrollment_id).collect();
                let rows = db(sqlx::query(&format!(
                    "SELECT f.id, f.user_id, {GALLERY}
                 FROM (SELECT 1) one
                 LEFT JOIN fingerprint_enrollments f ON f.id = ANY($1) AND f.revoked_at IS NULL"
                ))
                .bind(&ids)
                .fetch_all(&mut *conn)
                .await)?;
                let mut out = Vec::with_capacity(rows.len());
                let mut gallery = 0;
                for row in rows {
                    gallery = db(row.try_get("gallery"))?;
                    let id: Option<Uuid> = db(row.try_get("id"))?;
                    let subject: Option<Uuid> = db(row.try_get("user_id"))?;
                    if let (Some(id), Some(subject)) = (id, subject) {
                        out.extend(hits.iter().filter(|h| h.enrollment_id == id).map(|h| {
                            Candidate {
                                subject,
                                score: h.score,
                            }
                        }));
                    }
                }
                (out, gallery)
            }
        };
    Ok((candidates, gallery.max(0) as u64))
}

// 1:1: is this the person whose phone number the cashier typed? Only that person's live
// enrolments are compared, so the plain 1:1 threshold applies.
async fn verify(
    conn: &mut PgConnection,
    state: &AppState,
    probe: &Template,
    probe_hash: &[u8],
    phone: &str,
) -> ApiResult<Vec<Candidate>> {
    let row = db(sqlx::query(
        "SELECT u.id AS payer,
                COALESCE(array_agg(f.id) FILTER (WHERE f.id IS NOT NULL), '{}') AS enrollments,
                COALESCE(bool_or(f.template_hash = $2), false) AS exact_hit
         FROM users u
         LEFT JOIN fingerprint_enrollments f ON f.user_id = u.id AND f.revoked_at IS NULL
         WHERE u.phone = $1
         GROUP BY u.id",
    )
    .bind(phone)
    .bind(probe_hash)
    .fetch_optional(&mut *conn)
    .await)?;
    let Some(row) = row else {
        return Ok(Vec::new());
    };
    let payer: Uuid = db(row.try_get("payer"))?;
    let enrollments: Vec<Uuid> = db(row.try_get("enrollments"))?;
    let candidate = |score| Candidate {
        subject: payer,
        score,
    };
    match &state.biometric.matcher {
        MatcherBackend::Exact => {
            let hit: bool = db(row.try_get("exact_hit"))?;
            Ok(hit.then(|| candidate(EXACT_SCORE)).into_iter().collect())
        }
        MatcherBackend::Http(_) if enrollments.is_empty() => Ok(Vec::new()),
        MatcherBackend::Http(m) => Ok(m
            .verify(probe, &enrollments)
            .await
            .map_err(matcher_error)?
            .into_iter()
            .filter(|h| enrollments.contains(&h.enrollment_id))
            .map(|h| candidate(h.score))
            .collect()),
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
    let payer_phone = match req.payer_phone.as_deref().map(str::trim) {
        Some(raw) if !raw.is_empty() => Some(
            normalize_phone(raw)
                .ok_or_else(|| ApiError::BadRequest("malformed payer_phone".to_string()))?,
        ),
        _ if state.biometric.identify => None,
        _ => {
            return Err(ApiError::BadRequest(
                "payer_phone is required: identification without it is disabled".to_string(),
            ))
        }
    };
    let fingerprint = format!("check_pay:{check_id}:{merchant_id}");

    let mut conn = db(state.ledger.pool().acquire().await)?;
    let check = load_check(&mut conn, check_id, key, true)
        .await?
        .filter(|c| c.merchant_user_id == merchant_id)
        .ok_or_else(|| ApiError::NotFound("check not found".to_string()))?;
    let terminal = terminals::authenticate(&headers, &check.terminals)?;
    if let Some(found) = check.replay(&fingerprint)? {
        return Ok(found);
    }
    let max_attempts = state.biometric.max_attempts;
    if check.failed_attempts >= max_attempts {
        return Err(locked_out(&mut conn, check_id, max_attempts).await?);
    }
    check.require_open(&mut conn).await?;
    let attempt = Attempt {
        check_id,
        merchant: merchant_id,
        terminal,
        probe_hash: probe.hash().to_vec(),
    };
    if check.amount_minor > state.biometric.max_minor {
        let err = ApiError::LimitExceeded(format!(
            "amount exceeds the fingerprint payment limit of {} minor units",
            state.biometric.max_minor
        ));
        return Err(refuse(&mut conn, &attempt, (None, "rejected", None), err, false).await);
    }

    // One statement: touch the terminal, reserve an attempt slot, and look the probe up. A
    // scanner never yields byte-identical captures, so bytes seen before are a replay; the
    // exact (dev) matcher needs identical bytes by design and is exempt.
    let row = db(sqlx::query(
        "WITH touch AS (UPDATE terminals SET last_used_at = now() WHERE id = $1),
         slot AS (
             UPDATE checks SET attempts_in_flight = attempts_in_flight + 1
             WHERE id = $4 AND status = 'open' AND failed_attempts + attempts_in_flight < $5
             RETURNING id
         )
         SELECT EXISTS (SELECT 1 FROM slot) AS reserved,
                $3 AND EXISTS (
                    SELECT 1 FROM biometric_events
                    WHERE probe_hash = $2 AND outcome <> 'matcher_error'
                ) AS replayed",
    )
    .bind(terminal)
    .bind(&attempt.probe_hash)
    .bind(matches!(state.biometric.matcher, MatcherBackend::Http(_)))
    .bind(check_id)
    .bind(max_attempts)
    .fetch_one(&mut *conn)
    .await)?;
    if !db(row.try_get::<bool, _>("reserved"))? {
        return Err(locked_out(&mut conn, check_id, max_attempts).await?);
    }
    if db(row.try_get::<bool, _>("replayed"))? {
        tracing::warn!(%check_id, terminal_id = %terminal, "replayed fingerprint probe refused");
        let err = ApiError::ProbeReplayed;
        return Err(fail(&mut conn, &state, &attempt, (None, "replayed"), None, err).await);
    }

    let matched = match &payer_phone {
        Some(phone) => verify(&mut conn, &state, &probe, &attempt.probe_hash, phone)
            .await
            .map(|c| (c, state.biometric.policy)),
        None => identify(&mut conn, &state, &probe, &attempt.probe_hash)
            .await
            .map(|(c, gallery)| {
                let policy = state
                    .biometric
                    .policy
                    .for_gallery(state.biometric.identify_scale, gallery);
                (c, policy)
            }),
    };
    let (candidates, policy) = match matched {
        Ok(v) => v,
        Err(e) => {
            let event = (None, "matcher_error", None);
            return Err(refuse(&mut conn, &attempt, event, e, true).await);
        }
    };
    let (payer_id, score) = match decide(&candidates, policy) {
        Decision::Match { subject, score } => (subject, score),
        Decision::NoMatch => {
            let best = candidates
                .iter()
                .map(|c| c.score)
                .fold(None, |acc: Option<f64>, s| {
                    Some(acc.map_or(s, |a| a.max(s)))
                });
            let err = ApiError::NoMatch;
            return Err(fail(&mut conn, &state, &attempt, (None, "no_match"), best, err).await);
        }
        Decision::Ambiguous { best, .. } => {
            let err = ApiError::AmbiguousMatch;
            let outcome = (Some(best), "ambiguous");
            return Err(fail(&mut conn, &state, &attempt, outcome, None, err).await);
        }
    };
    let event = (Some(payer_id), "rejected", Some(score));
    if payer_id == merchant_id {
        let err = ApiError::BadRequest("a check cannot be paid by its own merchant".to_string());
        return Err(refuse(&mut conn, &attempt, event, err, true).await);
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
                terminal_id: terminal,
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
        Err(e) => Err(refuse(&mut conn, &attempt, event, e, true).await),
    }
}

#[cfg(test)]
mod tests {
    use super::mask_name;

    #[test]
    fn payer_names_are_masked_to_given_name_and_family_initial() {
        assert_eq!(mask_name("Bilal Sharifzade").as_deref(), Some("Bilal S."));
        assert_eq!(
            mask_name("  Abdullo Rahmon  karimov ").as_deref(),
            Some("Abdullo K.")
        );
        assert_eq!(mask_name("Madonna").as_deref(), Some("Madonna"));
        assert_eq!(mask_name("Фируза Каримова").as_deref(), Some("Фируза К."));
        assert_eq!(mask_name("   "), None);
    }
}
