use std::sync::OnceLock;

use axum::extract::{FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::{header::AUTHORIZATION, StatusCode};
use axum::Json;
use ledger::{Account, AccountId, AccountType};
use serde::{Deserialize, Serialize};
use sqlx::{PgConnection, Row};
use storage::StorageError;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::common::normalize_phone;
use crate::{ApiError, ApiResult, AppState};

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

pub struct AdminUser(pub Uuid);

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, ApiError> {
        let AuthUser(user_id) = AuthUser::from_request_parts(parts, state).await?;
        let row = sqlx::query("SELECT is_admin, status FROM users WHERE id = $1")
            .bind(user_id)
            .fetch_optional(state.ledger.pool())
            .await
            .map_err(StorageError::from)?;
        let Some(row) = row else {
            return Err(ApiError::Forbidden("admin privileges required".to_string()));
        };
        let is_admin: bool = row.try_get("is_admin").map_err(StorageError::from)?;
        let status: String = row.try_get("status").map_err(StorageError::from)?;
        if !is_admin {
            return Err(ApiError::Forbidden("admin privileges required".to_string()));
        }
        if status != "active" {
            return Err(ApiError::Forbidden(format!("account is {status}")));
        }
        Ok(AdminUser(user_id))
    }
}

fn argon2_permits() -> &'static Semaphore {
    static PERMITS: OnceLock<Semaphore> = OnceLock::new();
    PERMITS.get_or_init(|| {
        let cores = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(2);
        Semaphore::new(cores * 2)
    })
}

async fn hash_password_async(password: String) -> ApiResult<String> {
    let _permit = argon2_permits()
        .acquire()
        .await
        .map_err(|_| ApiError::Internal("hasher unavailable".to_string()))?;
    tokio::task::spawn_blocking(move || auth::hash_password(&password))
        .await
        .map_err(|e| ApiError::Internal(format!("hash task failed: {e}")))?
        .map_err(|_| ApiError::Internal("password hashing failed".to_string()))
}

async fn verify_password_async(password: String, hash: String) -> ApiResult<bool> {
    let _permit = argon2_permits()
        .acquire()
        .await
        .map_err(|_| ApiError::Internal("hasher unavailable".to_string()))?;
    tokio::task::spawn_blocking(move || auth::verify_password(&password, &hash))
        .await
        .map_err(|e| ApiError::Internal(format!("verify task failed: {e}")))
}

fn dummy_password_hash() -> String {
    static DUMMY: OnceLock<String> = OnceLock::new();
    DUMMY
        .get_or_init(|| {
            auth::hash_password("timing-equaliser-not-a-real-password").unwrap_or_default()
        })
        .clone()
}

pub async fn warm_password_hasher() {
    let _ = tokio::task::spawn_blocking(dummy_password_hash).await;
}

#[derive(Deserialize)]
pub struct CredentialsRequest {
    phone: String,
    password: String,
}

#[derive(Serialize)]
pub struct TokenResponse {
    user_id: Uuid,
    access_token: String,
    refresh_token: String,
    token_type: &'static str,
    expires_in: u64,
}

fn validate_credentials(req: &CredentialsRequest) -> ApiResult<String> {
    let phone = normalize_phone(&req.phone).ok_or_else(|| {
        ApiError::BadRequest("phone must be 7-15 digits, optionally with +".to_string())
    })?;
    if req.password.len() < 8 {
        return Err(ApiError::BadRequest(
            "password must be at least 8 characters".to_string(),
        ));
    }
    if req.password.len() > 128 {
        return Err(ApiError::BadRequest(
            "password must be at most 128 characters".to_string(),
        ));
    }
    Ok(phone)
}

pub async fn register(
    State(state): State<AppState>,
    Json(req): Json<CredentialsRequest>,
) -> ApiResult<(StatusCode, Json<TokenResponse>)> {
    let phone = validate_credentials(&req)?;

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

    let wallet = async {
        let tjs = state.ledger.lookup_currency("TJS").await?;
        state
            .ledger
            .open_account_owned(
                &Account::new(AccountId::new(), AccountType::UserWallet, tjs),
                Some(user_id),
            )
            .await
            .map_err(ApiError::from)
    }
    .await;
    if let Err(e) = wallet {
        tracing::warn!(user = %user_id, error = %e, "could not auto-create TJS wallet at registration");
    }

    let tokens = issue_tokens(&state, user_id).await?;
    Ok((StatusCode::CREATED, Json(tokens)))
}

pub async fn login(
    State(state): State<AppState>,
    Json(req): Json<CredentialsRequest>,
) -> ApiResult<Json<TokenResponse>> {
    let Some(phone) = normalize_phone(&req.phone) else {
        let _ = verify_password_async(req.password, dummy_password_hash()).await;
        return Err(ApiError::Unauthorized("invalid credentials".to_string()));
    };

    let throttle_key = format!("login:{phone}");
    if state.login_limit.over_limit(&throttle_key).await {
        return Err(ApiError::TooManyRequests);
    }

    let row = sqlx::query("SELECT id, password_hash, status FROM users WHERE phone = $1")
        .bind(&phone)
        .fetch_optional(state.ledger.pool())
        .await
        .map_err(StorageError::from)?;

    let Some(row) = row else {
        let _ = verify_password_async(req.password, dummy_password_hash()).await;
        state.login_limit.hit(&throttle_key).await;
        return Err(ApiError::Unauthorized("invalid credentials".to_string()));
    };
    let user_id: Uuid = row.try_get("id").map_err(StorageError::from)?;
    let hash: String = row.try_get("password_hash").map_err(StorageError::from)?;
    let status: String = row.try_get("status").map_err(StorageError::from)?;

    if !verify_password_async(req.password, hash).await? {
        state.login_limit.hit(&throttle_key).await;
        return Err(ApiError::Unauthorized("invalid credentials".to_string()));
    }
    if status != "active" {
        return Err(ApiError::Forbidden(format!("account is {status}")));
    }

    Ok(Json(issue_tokens(&state, user_id).await?))
}

#[derive(Deserialize)]
pub struct RefreshRequest {
    refresh_token: String,
}

pub async fn refresh(
    State(state): State<AppState>,
    Json(req): Json<RefreshRequest>,
) -> ApiResult<Json<TokenResponse>> {
    let token_hash = auth::hash_refresh_token(&req.refresh_token);
    let grace_secs = state.auth.refresh_reuse_grace_secs as f64;

    let mut tx = state
        .ledger
        .pool()
        .begin()
        .await
        .map_err(StorageError::from)?;

    let row = sqlx::query(
        "SELECT t.id, t.user_id, u.status, t.successor_id,
                (t.revoked_at IS NOT NULL) AS revoked,
                (t.expires_at <= now()) AS expired,
                (t.revoked_at IS NOT NULL
                   AND t.revoked_at > now() - make_interval(secs => $2)) AS in_grace
         FROM refresh_tokens t JOIN users u ON u.id = t.user_id
         WHERE t.token_hash = $1
         FOR UPDATE OF t",
    )
    .bind(&token_hash)
    .bind(grace_secs)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StorageError::from)?;

    let Some(row) = row else {
        return Err(ApiError::Unauthorized("invalid refresh token".to_string()));
    };
    let token_id: Uuid = row.try_get("id").map_err(StorageError::from)?;
    let user_id: Uuid = row.try_get("user_id").map_err(StorageError::from)?;
    let status: String = row.try_get("status").map_err(StorageError::from)?;
    let successor_id: Option<Uuid> = row.try_get("successor_id").map_err(StorageError::from)?;
    let revoked: bool = row.try_get("revoked").map_err(StorageError::from)?;
    let expired: bool = row.try_get("expired").map_err(StorageError::from)?;
    let in_grace: bool = row.try_get("in_grace").map_err(StorageError::from)?;

    if expired {
        return Err(ApiError::Unauthorized("invalid refresh token".to_string()));
    }

    if revoked {
        if in_grace {
            if let Some(successor) = successor_id {
                let claimed: Option<bool> = sqlx::query_scalar(
                    "UPDATE refresh_tokens SET revoked_at = now()
                     WHERE id = $1 AND revoked_at IS NULL AND NOT grace_issued
                     RETURNING true",
                )
                .bind(successor)
                .fetch_optional(&mut *tx)
                .await
                .map_err(StorageError::from)?;
                if claimed.is_some() {
                    if status != "active" {
                        return Err(ApiError::Forbidden(format!("account is {status}")));
                    }
                    let (tokens, new_id) = issue_tokens_on(&mut tx, &state, user_id, true).await?;
                    sqlx::query("UPDATE refresh_tokens SET successor_id = $2 WHERE id = $1")
                        .bind(successor)
                        .bind(new_id)
                        .execute(&mut *tx)
                        .await
                        .map_err(StorageError::from)?;
                    tx.commit().await.map_err(StorageError::from)?;
                    tracing::info!(%user_id, "refresh token re-presented inside the grace window; rotated its successor");
                    metrics::counter!("auth_refresh_total", "outcome" => "grace").increment(1);
                    return Ok(Json(tokens));
                }
            }
        }
        sqlx::query(
            "UPDATE refresh_tokens SET revoked_at = now()
             WHERE user_id = $1 AND revoked_at IS NULL",
        )
        .bind(user_id)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
        tx.commit().await.map_err(StorageError::from)?;
        tracing::warn!(%user_id, "revoked refresh token replayed — all sessions revoked");
        metrics::counter!("auth_refresh_total", "outcome" => "replay").increment(1);
        return Err(ApiError::Unauthorized("invalid refresh token".to_string()));
    }

    if status != "active" {
        return Err(ApiError::Forbidden(format!("account is {status}")));
    }

    let rotated: Option<Uuid> = sqlx::query_scalar(
        "UPDATE refresh_tokens SET revoked_at = now()
         WHERE id = $1 AND revoked_at IS NULL
         RETURNING id",
    )
    .bind(token_id)
    .fetch_optional(&mut *tx)
    .await
    .map_err(StorageError::from)?;
    if rotated.is_none() {
        return Err(ApiError::Unauthorized("invalid refresh token".to_string()));
    }
    let (tokens, new_id) = issue_tokens_on(&mut tx, &state, user_id, false).await?;
    sqlx::query("UPDATE refresh_tokens SET successor_id = $2 WHERE id = $1")
        .bind(token_id)
        .bind(new_id)
        .execute(&mut *tx)
        .await
        .map_err(StorageError::from)?;
    tx.commit().await.map_err(StorageError::from)?;
    metrics::counter!("auth_refresh_total", "outcome" => "rotated").increment(1);
    Ok(Json(tokens))
}

pub async fn logout(
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

pub async fn issue_tokens(state: &AppState, user_id: Uuid) -> ApiResult<TokenResponse> {
    let mut conn = state
        .ledger
        .pool()
        .acquire()
        .await
        .map_err(StorageError::from)?;
    Ok(issue_tokens_on(&mut conn, state, user_id, false).await?.0)
}

async fn issue_tokens_on(
    conn: &mut PgConnection,
    state: &AppState,
    user_id: Uuid,
    grace_issued: bool,
) -> ApiResult<(TokenResponse, Uuid)> {
    let access_token =
        auth::issue_access_token(user_id, &state.auth.jwt_secret, state.auth.access_ttl_secs)
            .map_err(|_| ApiError::Internal("failed to issue access token".to_string()))?;
    let refresh = auth::generate_refresh_token();
    let id = Uuid::new_v4();

    sqlx::query(
        "INSERT INTO refresh_tokens (id, user_id, token_hash, expires_at, grace_issued)
         VALUES ($1, $2, $3, now() + make_interval(days => $4), $5)",
    )
    .bind(id)
    .bind(user_id)
    .bind(&refresh.hash)
    .bind(state.auth.refresh_ttl_days)
    .bind(grace_issued)
    .execute(&mut *conn)
    .await
    .map_err(StorageError::from)?;

    Ok((
        TokenResponse {
            user_id,
            access_token,
            refresh_token: refresh.plaintext,
            token_type: "Bearer",
            expires_in: state.auth.access_ttl_secs,
        },
        id,
    ))
}
