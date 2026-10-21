use axum::http::HeaderMap;
use uuid::Uuid;

use crate::{ApiError, ApiResult};

pub fn default_currency() -> String {
    "TJS".to_string()
}

pub fn rfc3339(column: &str) -> String {
    format!(r#"to_char({column} AT TIME ZONE 'UTC', 'YYYY-MM-DD"T"HH24:MI:SS.US"Z"')"#)
}

pub fn db_err(e: sqlx::Error) -> ApiError {
    storage::StorageError::from(e).into()
}

pub fn normalize_phone(raw: &str) -> Option<String> {
    let digits: String = raw
        .chars()
        .filter(|c| !matches!(c, ' ' | '-' | '(' | ')' | '+'))
        .collect();
    if !(7..=15).contains(&digits.len()) || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(digits)
}

pub fn idempotency_key(headers: &HeaderMap) -> ApiResult<Uuid> {
    let raw = headers
        .get("idempotency-key")
        .ok_or_else(|| ApiError::BadRequest("missing Idempotency-Key header".to_string()))?;
    let s = raw
        .to_str()
        .map_err(|_| ApiError::BadRequest("invalid Idempotency-Key header".to_string()))?;
    Uuid::parse_str(s)
        .map_err(|_| ApiError::BadRequest("Idempotency-Key must be a UUID".to_string()))
}

pub fn cursor_db_error(e: sqlx::Error) -> ApiError {
    match &e {
        sqlx::Error::Database(db)
            if db
                .code()
                .is_some_and(|c| matches!(c.as_ref(), "22007" | "22008" | "22P02")) =>
        {
            ApiError::BadRequest("malformed cursor".to_string())
        }
        _ => storage::StorageError::from(e).into(),
    }
}

pub fn parse_cursor(raw: &str) -> ApiResult<(String, Uuid)> {
    let (ts, id) = raw
        .rsplit_once('|')
        .ok_or_else(|| ApiError::BadRequest("malformed cursor".to_string()))?;
    let id =
        Uuid::parse_str(id).map_err(|_| ApiError::BadRequest("malformed cursor".to_string()))?;
    Ok((ts.to_string(), id))
}
