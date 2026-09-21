mod admin;
mod common;
pub mod config;
mod error;
mod kyc;
pub mod middleware;
mod payments;
mod ratelimit;
mod reads;
mod session;

pub use error::{ApiError, ApiResult};
pub use ratelimit::RateLimitState;
pub use session::{warm_password_hasher, AdminUser, AuthUser};

use std::time::Duration;

use axum::extract::State;
use axum::http::{header, HeaderValue, Request, StatusCode};
use axum::routing::{get, post};
use axum::{Json, Router};
use storage::PostgresLedger;
use tower_http::request_id::{MakeRequestUuid, PropagateRequestIdLayer, SetRequestIdLayer};
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::{DefaultOnResponse, TraceLayer};
use tower_http::LatencyUnit;
use tracing::Level;

#[derive(Clone, Copy, Default)]
pub struct FeeConfig {
    pub transfer_bps: u32,
}

impl FeeConfig {
    pub const MAX_BPS: u32 = 10_000;

    pub fn from_env() -> Result<Self, String> {
        let transfer_bps: u32 = config::env_or("TRANSFER_FEE_BPS", 0)?;
        if transfer_bps > Self::MAX_BPS {
            return Err(format!(
                "TRANSFER_FEE_BPS={transfer_bps} exceeds the maximum of {}",
                Self::MAX_BPS
            ));
        }
        Ok(Self { transfer_bps })
    }

    pub fn fee_minor(&self, amount_minor: i64) -> i64 {
        let fee = (amount_minor as i128 * self.transfer_bps as i128) / 10_000;
        i64::try_from(fee).unwrap_or(i64::MAX)
    }
}

#[derive(Clone)]
pub struct AuthConfig {
    pub jwt_secret: String,
    pub access_ttl_secs: u64,
    pub refresh_ttl_days: i32,
    pub refresh_reuse_grace_secs: u64,
}

impl Default for AuthConfig {
    fn default() -> Self {
        Self {
            jwt_secret: "dev-insecure-secret-change-me".to_string(),
            access_ttl_secs: 900,
            refresh_ttl_days: 30,
            refresh_reuse_grace_secs: 30,
        }
    }
}

#[derive(Clone, Copy)]
pub struct Limits {
    pub per_tx_minor: i64,
    pub daily_minor: i64,
    pub velocity_per_hour: i64,
}

#[derive(Clone, Copy)]
pub struct AmlConfig {
    pub level1: Limits,
    pub level2: Limits,
}

impl AmlConfig {
    pub fn limits_for(&self, kyc_level: i16) -> Limits {
        if kyc_level >= 2 {
            self.level2
        } else {
            self.level1
        }
    }

    pub fn from_env() -> Result<Self, String> {
        let d = Self::default();
        let cfg = Self {
            level1: Limits {
                per_tx_minor: config::env_or("AML_L1_PER_TX_MINOR", d.level1.per_tx_minor)?,
                daily_minor: config::env_or("AML_L1_DAILY_MINOR", d.level1.daily_minor)?,
                velocity_per_hour: config::env_or(
                    "AML_L1_VELOCITY_PER_HOUR",
                    d.level1.velocity_per_hour,
                )?,
            },
            level2: Limits {
                per_tx_minor: config::env_or("AML_L2_PER_TX_MINOR", d.level2.per_tx_minor)?,
                daily_minor: config::env_or("AML_L2_DAILY_MINOR", d.level2.daily_minor)?,
                velocity_per_hour: config::env_or(
                    "AML_L2_VELOCITY_PER_HOUR",
                    d.level2.velocity_per_hour,
                )?,
            },
        };
        for (name, l) in [("level 1", cfg.level1), ("level 2", cfg.level2)] {
            if l.per_tx_minor <= 0 || l.daily_minor <= 0 || l.velocity_per_hour <= 0 {
                return Err(format!("AML {name} limits must be positive"));
            }
        }
        Ok(cfg)
    }
}

impl Default for AmlConfig {
    fn default() -> Self {
        Self {
            level1: Limits {
                per_tx_minor: 500_000,
                daily_minor: 2_000_000,
                velocity_per_hour: 50,
            },
            level2: Limits {
                per_tx_minor: 50_000_000,
                daily_minor: 200_000_000,
                velocity_per_hour: 500,
            },
        }
    }
}

#[derive(Clone)]
pub struct AppState {
    pub ledger: PostgresLedger,
    pub auth: AuthConfig,
    pub rate_limit: RateLimitState,
    pub login_limit: RateLimitState,
    pub resolve_limit: RateLimitState,
    pub aml: AmlConfig,
    pub fees: FeeConfig,
    pub trust_proxy: bool,
    pub document_dir: std::path::PathBuf,
    pub kyc_upload_daily_max: i64,
}

pub fn build_router(state: AppState) -> Router {
    let router = Router::new()
        .route("/health", get(health))
        .route("/ready", get(ready))
        .route("/v1/auth/register", post(session::register))
        .route("/v1/auth/login", post(session::login))
        .route("/v1/auth/refresh", post(session::refresh))
        .route("/v1/auth/logout", post(session::logout))
        .route(
            "/v1/wallets",
            post(reads::create_wallet).get(reads::list_wallets),
        )
        .route("/v1/config", get(reads::client_config))
        .route("/v1/users/resolve", get(reads::resolve_recipient))
        .route("/v1/accounts/{id}/balance", get(reads::get_balance))
        .route(
            "/v1/accounts/{id}/transactions",
            get(reads::list_account_transactions),
        )
        .route("/v1/kyc", get(kyc::get_kyc_status))
        .route("/v1/kyc/submissions", post(kyc::submit_kyc))
        .route(
            "/v1/kyc/documents",
            post(kyc::upload_kyc_document).layer(axum::extract::DefaultBodyLimit::max(
                kyc::MAX_DOCUMENT_BYTES + 64 * 1024,
            )),
        )
        .route("/v1/kyc/submissions/{id}/approve", post(kyc::approve_kyc))
        .route("/v1/kyc/submissions/{id}/reject", post(kyc::reject_kyc))
        .route(
            "/v1/admin/kyc/submissions",
            get(admin::list_kyc_submissions),
        )
        .route(
            "/v1/admin/kyc/documents/{document_ref}",
            get(kyc::get_kyc_document),
        )
        .route("/v1/admin/users", get(admin::admin_user_lookup))
        .route("/v1/admin/users/list", get(admin::admin_user_list))
        .route(
            "/v1/admin/users/{user_id}/status",
            post(admin::set_user_status),
        )
        .route("/v1/admin/status", get(admin::admin_status))
        .route("/v1/admin/metrics", get(admin::admin_metrics))
        .route("/v1/admin/blocks", post(admin::block_user))
        .route(
            "/v1/admin/blocks/{user_id}",
            axum::routing::delete(admin::unblock_user),
        )
        .route("/v1/admin/fx-rates", post(admin::set_fx_rate))
        .route("/v1/fx/rates", get(reads::list_fx_rates))
        .route("/v1/deposits", post(payments::create_deposit))
        .route("/v1/transfers", post(payments::create_transfer))
        .route("/v1/fx", post(payments::create_fx))
        .with_state(state.clone());

    router
        .layer(SetResponseHeaderLayer::if_not_present(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-store"),
        ))
        .layer(axum::middleware::from_fn_with_state(
            state,
            middleware::rate_limit,
        ))
        .layer(axum::middleware::from_fn(middleware::request_context))
        .layer(axum::middleware::from_fn(middleware::request_timeout))
        .layer(
            TraceLayer::new_for_http()
                .make_span_with(|req: &Request<axum::body::Body>| {
                    tracing::info_span!(
                        "request",
                        method = %req.method(),
                        path = %req.uri().path(),
                        request_id = %req
                            .headers()
                            .get("x-request-id")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("-"),
                    )
                })
                .on_response(
                    DefaultOnResponse::new()
                        .level(Level::INFO)
                        .latency_unit(LatencyUnit::Micros),
                ),
        )
        .layer(PropagateRequestIdLayer::x_request_id())
        .layer(SetRequestIdLayer::x_request_id(MakeRequestUuid))
}

async fn health() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "status": "ok" }))
}

async fn ready(State(state): State<AppState>) -> (StatusCode, Json<serde_json::Value>) {
    let db = tokio::time::timeout(
        Duration::from_secs(1),
        sqlx::query_scalar::<_, i32>("SELECT 1").fetch_one(state.ledger.pool()),
    )
    .await;
    match db {
        Ok(Ok(_)) => (
            StatusCode::OK,
            Json(serde_json::json!({ "status": "ready" })),
        ),
        Ok(Err(e)) => {
            tracing::warn!(error = %e, "readiness: database check failed");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "status": "degraded", "db": "error" })),
            )
        }
        Err(_) => {
            tracing::warn!("readiness: database check timed out");
            (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(serde_json::json!({ "status": "degraded", "db": "timeout" })),
            )
        }
    }
}
