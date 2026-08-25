//! Payment system HTTP server entry point.

use std::time::Duration;

use api::{build_router, AmlConfig, AppState, AuthConfig, FeeConfig, RateLimitState};
use sqlx::postgres::PgPoolOptions;
use storage::PostgresLedger;
use tower_http::trace::TraceLayer;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Structured logging. Override verbosity with RUST_LOG (e.g. RUST_LOG=debug).
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    // In any non-dev environment, refuse to start on insecure defaults. Set
    // APP_ENV=dev (the default) only for local development.
    let is_prod = std::env::var("APP_ENV")
        .map(|v| v != "dev")
        .unwrap_or(false);

    let database_url = match std::env::var("DATABASE_URL") {
        Ok(url) => url,
        Err(_) if is_prod => {
            return Err("DATABASE_URL must be set when APP_ENV != dev".into());
        }
        Err(_) => "postgres://payment:payment_dev_pw@localhost:5432/payment".to_string(),
    };
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

    // JWT secret MUST be provided in any real environment; the dev default is
    // refused outside APP_ENV=dev. Validated BEFORE connecting so a
    // misconfigured deployment fails immediately, not after the DB round trip.
    let auth = match std::env::var("JWT_SECRET") {
        Ok(secret) if secret.len() < 32 && is_prod => {
            return Err("JWT_SECRET must be at least 32 characters".into());
        }
        Ok(secret) => AuthConfig {
            jwt_secret: secret,
            ..AuthConfig::default()
        },
        Err(_) if is_prod => {
            return Err("JWT_SECRET must be set when APP_ENV != dev".into());
        }
        Err(_) => {
            tracing::warn!(
                "JWT_SECRET not set — using an INSECURE dev secret. Do not use in production."
            );
            AuthConfig::default()
        }
    };

    let max_connections: u32 = std::env::var("DB_MAX_CONNECTIONS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(32);
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await?;

    let ledger = PostgresLedger::new(pool);
    ledger.migrate().await?;
    tracing::info!("migrations applied");

    // Rate-limit budget (per client per window), tunable for the environment.
    let rl_max: u32 = std::env::var("RATE_LIMIT_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(300);
    let rl_window = Duration::from_secs(
        std::env::var("RATE_LIMIT_WINDOW_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60),
    );
    // Per-account login throttle: tighter and keyed by phone, not client IP.
    let login_max: u32 = std::env::var("LOGIN_LIMIT_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(10);
    let login_window = Duration::from_secs(
        std::env::var("LOGIN_LIMIT_WINDOW_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(900),
    );

    // Use Redis-backed rate limiting (shared across instances) if configured.
    let (rate_limit, login_limit) = match std::env::var("REDIS_URL") {
        Ok(url) => {
            let pool = deadpool_redis::Config::from_url(url)
                .create_pool(Some(deadpool_redis::Runtime::Tokio1))?;
            tracing::info!("rate limiting backed by Redis");
            (
                RateLimitState::redis(pool.clone(), rl_max, rl_window),
                RateLimitState::redis(pool, login_max, login_window),
            )
        }
        Err(_) => {
            tracing::info!("rate limiting in-memory (per-instance)");
            (
                RateLimitState::new(rl_max, rl_window),
                RateLimitState::new(login_max, login_window),
            )
        }
    };

    // Transfer fee in basis points (e.g. TRANSFER_FEE_BPS=50 = 0.50%); default 0.
    let fees = FeeConfig {
        transfer_bps: std::env::var("TRANSFER_FEE_BPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0),
    };

    // Trust X-Forwarded-For for the rate-limit client identity only behind a
    // proxy that overwrites it (see AppState::trust_proxy).
    let trust_proxy = std::env::var("TRUST_PROXY")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);

    // Where uploaded KYC documents live (a Docker volume in production).
    let document_dir = std::path::PathBuf::from(
        std::env::var("DOCUMENT_STORE_DIR").unwrap_or_else(|_| "./kyc-documents".to_string()),
    );
    std::fs::create_dir_all(&document_dir)?;

    // Per-user KYC upload quota (rolling 24h).
    let kyc_upload_daily_max: i64 = std::env::var("KYC_UPLOAD_DAILY_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(20);

    let app = build_router(AppState {
        ledger,
        auth,
        rate_limit,
        login_limit,
        aml: AmlConfig::from_env(),
        document_dir,
        kyc_upload_daily_max,
        fees,
        trust_proxy,
    })
    .layer(TraceLayer::new_for_http());

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(%bind_addr, "payment-server listening");
    // ConnectInfo makes the real peer IP available to the rate limiter.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .await?;
    Ok(())
}
