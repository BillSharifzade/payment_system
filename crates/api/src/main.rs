use std::str::FromStr;
use std::time::Duration;

use api::config::{env_flag, env_or, env_or_file};
use api::{
    build_router, warm_password_hasher, AmlConfig, AppState, AuthConfig, BiometricConfig,
    FeeConfig, RateLimitState,
};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use storage::PostgresLedger;
use tracing_subscriber::EnvFilter;

type BoxError = Box<dyn std::error::Error>;

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck().await;
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let is_prod = std::env::var("APP_ENV")
        .map(|v| v != "dev")
        .unwrap_or(false);

    let database_url = match env_or_file("DATABASE_URL")? {
        Some(url) => url,
        None if is_prod => {
            return Err("DATABASE_URL must be set when APP_ENV != dev".into());
        }
        None => "postgres://payment:payment_dev_pw@localhost:5432/payment".to_string(),
    };
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());

    let refresh_reuse_grace_secs: u64 = env_or("REFRESH_REUSE_GRACE_SECS", 30)?;
    let auth = match env_or_file("JWT_SECRET")? {
        Some(secret) if secret.len() < 32 && is_prod => {
            return Err("JWT_SECRET must be at least 32 characters".into());
        }
        Some(secret) => AuthConfig {
            jwt_secret: secret,
            refresh_reuse_grace_secs,
            ..AuthConfig::default()
        },
        None if is_prod => {
            return Err("JWT_SECRET must be set when APP_ENV != dev".into());
        }
        None => {
            tracing::warn!(
                "JWT_SECRET not set — using an INSECURE dev secret. Do not use in production."
            );
            AuthConfig {
                refresh_reuse_grace_secs,
                ..AuthConfig::default()
            }
        }
    };

    let fees = FeeConfig::from_env()?;
    let aml = AmlConfig::from_env()?;
    let biometric = BiometricConfig::from_env(is_prod)?;
    tracing::info!(
        matcher = biometric.matcher.name(),
        max_minor = biometric.max_minor,
        "biometric payments configured"
    );
    let request_timeout = Duration::from_secs(env_or("REQUEST_TIMEOUT_SECS", 10u64)?);
    api::middleware::configure_request_timeout(request_timeout);

    let metrics_addr: std::net::SocketAddr = env_or(
        "METRICS_ADDR",
        "127.0.0.1:9100".parse::<std::net::SocketAddr>()?,
    )?;
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(metrics_addr)
        .install()
        .map_err(|e| format!("metrics exporter on {metrics_addr}: {e}"))?;
    tracing::info!(%metrics_addr, "metrics exporter listening");

    let max_connections: u32 = env_or("DB_MAX_CONNECTIONS", 32)?;
    let statement_timeout_ms: u64 = env_or("DB_STATEMENT_TIMEOUT_MS", 5_000)?;
    let lock_timeout_ms: u64 = env_or("DB_LOCK_TIMEOUT_MS", 2_000)?;
    let idle_tx_timeout_ms: u64 = env_or("DB_IDLE_TX_TIMEOUT_MS", 10_000)?;
    let connect_options = PgConnectOptions::from_str(&database_url)?
        .application_name("payment-server")
        .options([
            ("statement_timeout", statement_timeout_ms.to_string()),
            ("lock_timeout", lock_timeout_ms.to_string()),
            (
                "idle_in_transaction_session_timeout",
                idle_tx_timeout_ms.to_string(),
            ),
        ]);
    let pool = PgPoolOptions::new()
        .max_connections(max_connections)
        .acquire_timeout(Duration::from_secs(5))
        .test_before_acquire(false)
        .connect_with(connect_options)
        .await?;

    let ledger = PostgresLedger::new(pool.clone());
    ledger.migrate().await?;
    tracing::info!("migrations applied");

    {
        let pool = pool.clone();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(Duration::from_secs(5));
            loop {
                tick.tick().await;
                metrics::gauge!("db_pool_size").set(pool.size() as f64);
                metrics::gauge!("db_pool_idle").set(pool.num_idle() as f64);
            }
        });
    }

    let rl_max: u32 = env_or("RATE_LIMIT_MAX", 300)?;
    let rl_window = Duration::from_secs(env_or("RATE_LIMIT_WINDOW_SECS", 60)?);
    let login_max: u32 = env_or("LOGIN_LIMIT_MAX", 10)?;
    let login_window = Duration::from_secs(env_or("LOGIN_LIMIT_WINDOW_SECS", 900)?);
    let resolve_max: u32 = env_or("RESOLVE_LIMIT_MAX", 60)?;
    let resolve_window = Duration::from_secs(env_or("RESOLVE_LIMIT_WINDOW_SECS", 3600)?);

    let (rate_limit, login_limit, resolve_limit) = match env_or_file("REDIS_URL")? {
        Some(url) => {
            let mut cfg = deadpool_redis::Config::from_url(url);
            cfg.pool = Some(deadpool_redis::PoolConfig {
                max_size: 16,
                timeouts: deadpool_redis::Timeouts {
                    wait: Some(Duration::from_millis(50)),
                    create: Some(Duration::from_millis(500)),
                    recycle: Some(Duration::from_millis(100)),
                },
                ..Default::default()
            });
            let pool = cfg.create_pool(Some(deadpool_redis::Runtime::Tokio1))?;
            tracing::info!("rate limiting backed by Redis (in-memory fallback on outage)");
            (
                RateLimitState::redis(pool.clone(), rl_max, rl_window),
                RateLimitState::redis(pool.clone(), login_max, login_window),
                RateLimitState::redis(pool, resolve_max, resolve_window),
            )
        }
        None => {
            tracing::info!("rate limiting in-memory (per-instance)");
            (
                RateLimitState::new(rl_max, rl_window),
                RateLimitState::new(login_max, login_window),
                RateLimitState::new(resolve_max, resolve_window),
            )
        }
    };

    let trust_proxy = env_flag("TRUST_PROXY");

    let document_dir = std::path::PathBuf::from(
        std::env::var("DOCUMENT_STORE_DIR").unwrap_or_else(|_| "./kyc-documents".to_string()),
    );
    std::fs::create_dir_all(&document_dir)?;

    let kyc_upload_daily_max: i64 = env_or("KYC_UPLOAD_DAILY_MAX", 20)?;

    warm_password_hasher().await;

    let app = build_router(AppState {
        ledger,
        auth,
        rate_limit,
        login_limit,
        resolve_limit,
        aml,
        document_dir,
        kyc_upload_daily_max,
        fees,
        biometric,
        trust_proxy,
    });

    let listener = tokio::net::TcpListener::bind(&bind_addr).await?;
    tracing::info!(%bind_addr, "payment-server listening");
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<std::net::SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    tracing::info!("payment-server stopped");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(e) => tracing::error!(error = %e, "cannot listen for SIGTERM"),
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown signal received; draining in-flight requests");
}

async fn healthcheck() -> Result<(), BoxError> {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let bind_addr = std::env::var("BIND_ADDR").unwrap_or_else(|_| "0.0.0.0:8080".to_string());
    let port = bind_addr.rsplit(':').next().unwrap_or("8080");
    let target = format!("127.0.0.1:{port}");
    let probe = async {
        let mut stream = tokio::net::TcpStream::connect(&target).await?;
        stream
            .write_all(b"GET /ready HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
            .await?;
        let mut buf = Vec::with_capacity(512);
        stream.read_to_end(&mut buf).await?;
        let head = String::from_utf8_lossy(&buf);
        if head.starts_with("HTTP/1.1 200") || head.starts_with("HTTP/1.0 200") {
            Ok::<(), BoxError>(())
        } else {
            Err(format!("not ready: {}", head.lines().next().unwrap_or("")).into())
        }
    };
    match tokio::time::timeout(Duration::from_secs(3), probe).await {
        Ok(Ok(())) => Ok(()),
        Ok(Err(e)) => {
            eprintln!("healthcheck failed: {e}");
            std::process::exit(1);
        }
        Err(_) => {
            eprintln!("healthcheck failed: timeout");
            std::process::exit(1);
        }
    }
}
