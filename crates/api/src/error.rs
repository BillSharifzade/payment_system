use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use ledger::LedgerError;
use serde_json::json;
use storage::StorageError;

/// The API's error type. Maps internal failures to appropriate HTTP status codes
/// and a stable JSON error shape: `{ "error": { "code": ..., "message": ... } }`.
#[derive(Debug, thiserror::Error)]
pub enum ApiError {
    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    NotFound(String),

    /// Authentication missing or invalid (401).
    #[error("{0}")]
    Unauthorized(String),

    /// Authenticated, but not allowed to act on this resource (403).
    #[error("{0}")]
    Forbidden(String),

    /// A uniqueness conflict, e.g. phone number already registered (409).
    #[error("{0}")]
    Conflict(String),

    /// Idempotency key reused with a different request body.
    #[error("idempotency key reused with a different request")]
    IdempotencyConflict,

    /// The caller's KYC verification level is insufficient for this action (403).
    #[error("{0}")]
    KycRequired(String),

    /// The sender or recipient is on the blocklist (403).
    #[error("{0}")]
    Blocked(String),

    /// A transaction or velocity limit was exceeded (422).
    #[error("{0}")]
    LimitExceeded(String),

    /// Too many requests from this client (429).
    #[error("rate limit exceeded")]
    TooManyRequests,

    /// An unexpected server-side failure (logged, not exposed to the client).
    #[error("{0}")]
    Internal(String),

    #[error(transparent)]
    Storage(#[from] StorageError),
}

impl ApiError {
    /// The stable machine-readable error code and HTTP status for this error.
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
            ApiError::TooManyRequests => (StatusCode::TOO_MANY_REQUESTS, "rate_limited"),
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
            // Database / data-integrity failures are our fault, not the client's.
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
        // Never leak internal error detail to clients on 5xx.
        let message = if status.is_server_error() {
            tracing::error!(error = %self, "internal error serving request");
            "an internal error occurred".to_string()
        } else {
            self.to_string()
        };
        (
            status,
            Json(json!({ "error": { "code": code, "message": message } })),
        )
            .into_response()
    }
}

pub type ApiResult<T> = Result<T, ApiError>;
