use axum::http::{header, HeaderValue, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use ledger::LedgerError;
use serde_json::json;
use storage::StorageError;

use crate::middleware::current_request_id;

#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    Unauthorized(String),

    #[error("{0}")]
    Forbidden(String),

    #[error("{0}")]
    Conflict(String),

    #[error("idempotency key reused with a different request")]
    IdempotencyConflict,

    #[error("{0}")]
    KycRequired(String),

    #[error("{0}")]
    Blocked(String),

    #[error("{0}")]
    LimitExceeded(String),

    #[error("{0}")]
    InsufficientFunds(String),

    #[error("no enrolled fingerprint matches")]
    NoMatch,

    #[error("fingerprint matches more than one person; use another finger or method")]
    AmbiguousMatch,

    #[error("this idempotency key was voided; it can never post")]
    Voided,

    #[error("{0}")]
    DualControlRequired(String),

    #[error("{0}")]
    RecipientUnavailable(String),

    #[error("a valid X-Terminal-Key of an active terminal of this merchant is required")]
    TerminalUnauthorized,

    #[error("this fingerprint capture was already presented; scan the finger again")]
    ProbeReplayed,

    #[error("too many failed fingerprint attempts; the check was cancelled")]
    CheckLocked,

    #[error("rate limit exceeded")]
    TooManyRequests,

    #[error("temporarily unavailable, retry")]
    RetryLater,

    #[error("{0}")]
    Internal(String),

    #[error(transparent)]
    Storage(#[from] StorageError),
}

fn is_transient_sqlstate(code: &str) -> bool {
    matches!(code, "55P03" | "57014" | "40001" | "40P01")
}

impl ApiError {
    fn parts(&self) -> (StatusCode, &'static str) {
        match self {
            ApiError::BadRequest(_) => (StatusCode::BAD_REQUEST, "bad_request"),
            ApiError::NotFound(_) => (StatusCode::NOT_FOUND, "not_found"),
            ApiError::Unauthorized(_) => (StatusCode::UNAUTHORIZED, "unauthorized"),
            ApiError::Forbidden(_) => (StatusCode::FORBIDDEN, "forbidden"),
            ApiError::Conflict(_) => (StatusCode::CONFLICT, "conflict"),
            ApiError::IdempotencyConflict => (StatusCode::CONFLICT, "idempotency_conflict"),
            ApiError::KycRequired(_) => (StatusCode::FORBIDDEN, "kyc_required"),
            ApiError::Blocked(_) => (StatusCode::FORBIDDEN, "account_blocked"),
            ApiError::LimitExceeded(_) => (StatusCode::UNPROCESSABLE_ENTITY, "limit_exceeded"),
            ApiError::InsufficientFunds(_) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "insufficient_funds")
            }
            ApiError::NoMatch => (StatusCode::NOT_FOUND, "no_match"),
            ApiError::AmbiguousMatch => (StatusCode::CONFLICT, "ambiguous_match"),
            ApiError::Voided => (StatusCode::CONFLICT, "voided"),
            ApiError::DualControlRequired(_) => (StatusCode::FORBIDDEN, "dual_control_required"),
            ApiError::RecipientUnavailable(_) => (StatusCode::FORBIDDEN, "recipient_unavailable"),
            ApiError::TerminalUnauthorized => (StatusCode::UNAUTHORIZED, "terminal_unauthorized"),
            ApiError::ProbeReplayed => (StatusCode::CONFLICT, "probe_replayed"),
            ApiError::CheckLocked => (StatusCode::CONFLICT, "check_locked"),
            ApiError::TooManyRequests => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
            ApiError::RetryLater => (StatusCode::SERVICE_UNAVAILABLE, "retry_later"),
            ApiError::Internal(_) => (StatusCode::INTERNAL_SERVER_ERROR, "internal_error"),
            ApiError::Storage(StorageError::Ledger(e)) => match e {
                LedgerError::InsufficientFunds { .. } => {
                    (StatusCode::UNPROCESSABLE_ENTITY, "insufficient_funds")
                }
                LedgerError::Unbalanced { .. }
                | LedgerError::TooFewEntries { .. }
                | LedgerError::NonPositiveAmount => {
                    (StatusCode::BAD_REQUEST, "invalid_transaction")
                }
                LedgerError::AccountCurrencyMismatch { .. } => {
                    (StatusCode::BAD_REQUEST, "currency_mismatch")
                }
                LedgerError::UnknownAccount(_) => (StatusCode::NOT_FOUND, "unknown_account"),
                LedgerError::DuplicateTransaction(_) => {
                    (StatusCode::CONFLICT, "duplicate_transaction")
                }
                LedgerError::Money(_) => (StatusCode::BAD_REQUEST, "invalid_amount"),
            },
            ApiError::Storage(StorageError::UnknownCurrency(_)) => {
                (StatusCode::BAD_REQUEST, "unknown_currency")
            }
            ApiError::Storage(StorageError::AmountTooLarge(_)) => {
                (StatusCode::BAD_REQUEST, "amount_too_large")
            }
            ApiError::Storage(StorageError::Rejected { .. }) => {
                (StatusCode::UNPROCESSABLE_ENTITY, "rejected")
            }
            ApiError::Storage(StorageError::Database(sqlx::Error::PoolTimedOut)) => {
                (StatusCode::SERVICE_UNAVAILABLE, "retry_later")
            }
            ApiError::Storage(StorageError::Database(sqlx::Error::Database(db)))
                if db.code().is_some_and(|c| is_transient_sqlstate(&c)) =>
            {
                (StatusCode::SERVICE_UNAVAILABLE, "retry_later")
            }
            ApiError::Storage(StorageError::Database(_))
            | ApiError::Storage(StorageError::DataIntegrity(_)) => {
                (StatusCode::INTERNAL_SERVER_ERROR, "internal_error")
            }
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let (status, code) = self.parts();
        let request_id = current_request_id();
        let message = if status.is_server_error() {
            if status == StatusCode::SERVICE_UNAVAILABLE {
                tracing::warn!(error = %self, request_id = %request_id, "request shed: retry later");
                "temporarily unavailable, please retry".to_string()
            } else {
                tracing::error!(error = %self, request_id = %request_id, "internal error serving request");
                "an internal error occurred".to_string()
            }
        } else {
            self.to_string()
        };
        let mut response = (
            status,
            Json(json!({
                "error": { "code": code, "message": message, "request_id": request_id }
            })),
        )
            .into_response();
        if status == StatusCode::SERVICE_UNAVAILABLE || status == StatusCode::TOO_MANY_REQUESTS {
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
        }
        response
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
