use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use biometric::{constant_time_eq, hash_terminal_key, TerminalKey};
use serde::{Deserialize, Serialize};
use sqlx::postgres::PgRow;
use sqlx::Row;
use uuid::Uuid;

use crate::common::{db_err, rfc3339};
use crate::session::AdminUser;
use crate::{ApiError, ApiResult, AppState};

const MAX_LABEL_CHARS: usize = 64;

#[derive(Serialize)]
pub struct TerminalResponse {
    id: Uuid,
    merchant_user_id: Uuid,
    label: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    api_key: Option<String>,
    created_at: String,
    revoked_at: Option<String>,
    last_used_at: Option<String>,
}

fn terminal_columns(prefix: &str) -> String {
    format!(
        "{prefix}id, {prefix}merchant_user_id, {prefix}label, {} AS created_at,
         {} AS revoked_at, {} AS last_used_at",
        rfc3339(&format!("{prefix}created_at")),
        rfc3339(&format!("{prefix}revoked_at")),
        rfc3339(&format!("{prefix}last_used_at")),
    )
}

fn terminal_row(row: &PgRow) -> ApiResult<TerminalResponse> {
    Ok(TerminalResponse {
        id: row.try_get("id").map_err(db_err)?,
        merchant_user_id: row.try_get("merchant_user_id").map_err(db_err)?,
        label: row.try_get("label").map_err(db_err)?,
        api_key: None,
        created_at: row.try_get("created_at").map_err(db_err)?,
        revoked_at: row.try_get("revoked_at").map_err(db_err)?,
        last_used_at: row.try_get("last_used_at").map_err(db_err)?,
    })
}

// The presented key is hashed and compared against every active terminal of the merchant in
// constant time; the plaintext never touches the database.
pub(crate) fn authenticate(headers: &HeaderMap, active: &[(Uuid, Vec<u8>)]) -> ApiResult<Uuid> {
    let presented = headers
        .get("x-terminal-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .ok_or(ApiError::TerminalUnauthorized)?;
    let hash = hash_terminal_key(presented);
    let mut found = None;
    for (id, stored) in active {
        if constant_time_eq(&hash, stored) && found.is_none() {
            found = Some(*id);
        }
    }
    found.ok_or(ApiError::TerminalUnauthorized)
}

#[derive(Deserialize)]
pub struct CreateTerminalRequest {
    merchant_user_id: Uuid,
    label: String,
}

pub async fn create_terminal(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Json(req): Json<CreateTerminalRequest>,
) -> ApiResult<(StatusCode, Json<TerminalResponse>)> {
    let label = req.label.trim();
    if label.is_empty() || label.chars().count() > MAX_LABEL_CHARS {
        return Err(ApiError::BadRequest(format!(
            "label must be 1 to {MAX_LABEL_CHARS} characters"
        )));
    }
    let key = TerminalKey::generate();
    let row = sqlx::query(&format!(
        "WITH ins AS (
             INSERT INTO terminals (id, merchant_user_id, label, key_hash, created_by)
             SELECT $1, u.id, $3, $4, $5 FROM users u WHERE u.id = $2
             RETURNING *
         ), aud AS (
             INSERT INTO admin_actions (id, admin_id, action, target, details)
             SELECT $6, $5, 'terminal.create', ins.id::text,
                    jsonb_build_object('merchant_user_id', ins.merchant_user_id, 'label', ins.label)
             FROM ins
         )
         SELECT {} FROM ins",
        terminal_columns("ins.")
    ))
    .bind(Uuid::now_v7())
    .bind(req.merchant_user_id)
    .bind(label)
    .bind(key.hash.as_slice())
    .bind(admin_id)
    .bind(Uuid::now_v7())
    .fetch_optional(state.ledger.pool())
    .await
    .map_err(db_err)?
    .ok_or_else(|| ApiError::NotFound("no user with that id".to_string()))?;
    let mut terminal = terminal_row(&row)?;
    terminal.api_key = Some(key.plaintext);
    tracing::info!(%admin_id, terminal_id = %terminal.id, merchant = %req.merchant_user_id, "terminal created");
    Ok((StatusCode::CREATED, Json(terminal)))
}

#[derive(Deserialize)]
pub struct TerminalListParams {
    merchant_user_id: Option<Uuid>,
}

#[derive(Serialize)]
pub struct TerminalList {
    items: Vec<TerminalResponse>,
}

pub async fn list_terminals(
    _admin: AdminUser,
    State(state): State<AppState>,
    Query(params): Query<TerminalListParams>,
) -> ApiResult<Json<TerminalList>> {
    let rows = sqlx::query(&format!(
        "SELECT {} FROM terminals
         WHERE $1::uuid IS NULL OR merchant_user_id = $1
         ORDER BY created_at DESC, id DESC LIMIT 200",
        terminal_columns("")
    ))
    .bind(params.merchant_user_id)
    .fetch_all(state.ledger.pool())
    .await
    .map_err(db_err)?;
    let items = rows.iter().map(terminal_row).collect::<ApiResult<_>>()?;
    Ok(Json(TerminalList { items }))
}

pub async fn revoke_terminal(
    AdminUser(admin_id): AdminUser,
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> ApiResult<Json<TerminalResponse>> {
    let pool = state.ledger.pool();
    sqlx::query(
        "WITH upd AS (
             UPDATE terminals SET revoked_at = now()
             WHERE id = $1 AND revoked_at IS NULL
             RETURNING id, merchant_user_id
         )
         INSERT INTO admin_actions (id, admin_id, action, target, details)
         SELECT $3, $2, 'terminal.revoke', upd.id::text,
                jsonb_build_object('merchant_user_id', upd.merchant_user_id)
         FROM upd",
    )
    .bind(id)
    .bind(admin_id)
    .bind(Uuid::now_v7())
    .execute(pool)
    .await
    .map_err(db_err)?;
    let row = sqlx::query(&format!(
        "SELECT {} FROM terminals WHERE id = $1",
        terminal_columns("")
    ))
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?
    .ok_or_else(|| ApiError::NotFound("terminal not found".to_string()))?;
    tracing::info!(%admin_id, terminal_id = %id, "terminal revoked");
    Ok(Json(terminal_row(&row)?))
}
