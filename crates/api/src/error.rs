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

    #[error(
        "this request must be signed by a registered device (X-Device-Id, X-Device-Signature)"
    )]
    DeviceSignatureRequired,

    #[error("the device signature does not match this request, or the device is not registered and active")]
    DeviceSignatureInvalid,

    #[error("rate limit exceeded")]
    TooManyRequests,

    #[error("temporarily unavailable, retry")]
    RetryLater,

    #[error("{0}")]
    Internal(String),

    #[error(transparent)]
    Storage(#[from] StorageError),
}

// Database failures after which the client should retry with the same Idempotency-Key (503
// retry_later; a COMMIT cut off this way has an unknown outcome, which is what the retry
// settles): the pool timed out, closed or crashed; the connection broke or never came up
// (failover, restart — HAProxy with no primary answers the SSL probe with 0x00, a Protocol
// error); the server shed the statement (lock or statement timeout, serialization failure,
// deadlock), is shutting down or restarting (57P01-57P03), is a read-only standby (25006), or
// reported a connection exception (class 08).
fn is_transient(e: &sqlx::Error) -> bool {
    match e {
        sqlx::Error::PoolTimedOut
        | sqlx::Error::PoolClosed
        | sqlx::Error::WorkerCrashed
        | sqlx::Error::Io(_)
        | sqlx::Error::Tls(_)
        | sqlx::Error::Protocol(_) => true,
        sqlx::Error::Database(db) => db.code().is_some_and(|c| {
            matches!(
                c.as_ref(),
                "55P03" | "57014" | "40001" | "40P01" | "57P01" | "57P02" | "57P03" | "25006"
            ) || c.starts_with("08")
        }),
        _ => false,
    }
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
            ApiError::DeviceSignatureRequired => {
                (StatusCode::FORBIDDEN, "device_signature_required")
            }
            ApiError::DeviceSignatureInvalid => (StatusCode::FORBIDDEN, "device_signature_invalid"),
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
            ApiError::Storage(StorageError::Database(e)) if is_transient(e) => {
                (StatusCode::SERVICE_UNAVAILABLE, "retry_later")
            }
            // TigerBeetle did not answer, or the attempt lost its reservation: nothing was
            // posted that recovery will not settle, so the same key may be retried.
            ApiError::Storage(StorageError::Unavailable(_)) => {
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

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use sqlx::error::{DatabaseError, ErrorKind};

    use super::*;

    #[derive(Debug)]
    struct Sqlstate(&'static str);

    impl std::fmt::Display for Sqlstate {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "SQLSTATE {}", self.0)
        }
    }

    impl std::error::Error for Sqlstate {}

    impl DatabaseError for Sqlstate {
        fn message(&self) -> &str {
            "test"
        }
        fn code(&self) -> Option<Cow<'_, str>> {
            Some(Cow::Borrowed(self.0))
        }
        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }
        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }
        fn kind(&self) -> ErrorKind {
            ErrorKind::Other
        }
    }

    fn parts_of(e: sqlx::Error) -> (StatusCode, &'static str) {
        ApiError::Storage(StorageError::Database(e)).parts()
    }

    fn sqlstate(code: &'static str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(Sqlstate(code)))
    }

    // A failover (connection lost, HAProxy without a primary, a standby or a restarting
    // server answering) or a shed statement must tell the client to retry with the same key,
    // never "internal error"; a real fault stays 500.
    #[test]
    fn failover_and_shed_statements_are_retry_later() {
        let retry = (StatusCode::SERVICE_UNAVAILABLE, "retry_later");
        let reset = std::io::Error::from(std::io::ErrorKind::ConnectionReset);
        for e in [
            sqlx::Error::PoolTimedOut,
            sqlx::Error::PoolClosed,
            sqlx::Error::WorkerCrashed,
            sqlx::Error::Io(reset),
            sqlx::Error::Tls("handshake failed".into()),
            sqlx::Error::Protocol("unexpected response from SSLRequest: 0x00".into()),
        ] {
            let name = format!("{e:?}");
            assert_eq!(parts_of(e), retry, "{name}");
        }
        for code in [
            "55P03", "57014", "40001", "40P01", "57P01", "57P02", "57P03", "25006", "08000",
            "08003", "08006", "08P01",
        ] {
            assert_eq!(parts_of(sqlstate(code)), retry, "SQLSTATE {code}");
        }

        let fault = (StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
        for code in ["23505", "22003", "42501", "P0001", "XX000"] {
            assert_eq!(parts_of(sqlstate(code)), fault, "SQLSTATE {code}");
        }
        assert_eq!(parts_of(sqlx::Error::RowNotFound), fault);
    }

    #[test]
    fn an_unavailable_ledger_backend_is_retry_later() {
        let e = ApiError::Storage(StorageError::Unavailable("no reply within 5s".into()));
        assert_eq!(e.parts(), (StatusCode::SERVICE_UNAVAILABLE, "retry_later"));
    }
}
