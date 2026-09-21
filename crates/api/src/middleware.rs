use std::time::{Duration, Instant};

use axum::extract::{MatchedPath, Request, State};
use axum::http::{header, HeaderValue, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

use crate::{ApiError, AppState};

tokio::task_local! {
    static REQUEST_ID: String;
}

pub fn current_request_id() -> String {
    REQUEST_ID
        .try_with(|id| id.clone())
        .unwrap_or_else(|_| "-".to_string())
}

pub async fn request_context(request: Request, next: Next) -> Response {
    let request_id = request
        .headers()
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
        .unwrap_or_else(|| "-".to_string());
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map(|m| m.as_str().to_string())
        .unwrap_or_else(|| "unmatched".to_string());
    let method = request.method().to_string();
    let started = Instant::now();

    let response = REQUEST_ID.scope(request_id, next.run(request)).await;

    let status = response.status().as_u16().to_string();
    metrics::counter!(
        "http_requests_total",
        "route" => route.clone(),
        "method" => method,
        "status" => status
    )
    .increment(1);
    metrics::histogram!("http_request_duration_seconds", "route" => route)
        .record(started.elapsed().as_secs_f64());
    response
}

static REQUEST_TIMEOUT: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();

pub fn configure_request_timeout(limit: Duration) {
    let _ = REQUEST_TIMEOUT.set(limit);
}

pub async fn request_timeout(request: Request, next: Next) -> Response {
    let limit = *REQUEST_TIMEOUT.get_or_init(|| Duration::from_secs(10));
    match tokio::time::timeout(limit, next.run(request)).await {
        Ok(response) => response,
        Err(_) => {
            let request_id = current_request_id();
            tracing::warn!(request_id = %request_id, "request timed out");
            metrics::counter!("http_timeouts_total").increment(1);
            let mut response = (
                StatusCode::GATEWAY_TIMEOUT,
                Json(json!({
                    "error": {
                        "code": "timeout",
                        "message": "the request took too long; the outcome is unknown — retry with the same idempotency key",
                        "request_id": request_id
                    }
                })),
            )
                .into_response();
            response
                .headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("1"));
            response
        }
    }
}

pub async fn rate_limit(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, ApiError> {
    let path = request.uri().path();
    if path == "/health" || path == "/ready" {
        return Ok(next.run(request).await);
    }
    let key = client_key(&state, &request);
    if !state.rate_limit.allow(&key).await {
        metrics::counter!("http_rate_limited_total").increment(1);
        return Err(ApiError::TooManyRequests);
    }
    Ok(next.run(request).await)
}

pub fn client_key(state: &AppState, request: &Request) -> String {
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
