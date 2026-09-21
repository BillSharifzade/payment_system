use std::str::FromStr;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crypto::Sealer;
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use tokio::sync::watch;
use tracing_subscriber::EnvFilter;
use workers::{EventPublisher, LoggingPublisher, NatsPublisher, RetentionConfig, VerifyState};

type BoxError = Box<dyn std::error::Error>;

fn env_parse<T: FromStr>(key: &str) -> Result<Option<T>, String> {
    match std::env::var(key) {
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(e) => Err(format!("{key}: {e}")),
        Ok(raw) => raw
            .trim()
            .parse::<T>()
            .map(Some)
            .map_err(|_| format!("{key}={raw:?} is not a valid value")),
    }
}

fn env_or<T: FromStr>(key: &str, default: T) -> Result<T, String> {
    Ok(env_parse(key)?.unwrap_or(default))
}

fn env_or_file(key: &str) -> Result<Option<String>, String> {
    if let Ok(v) = std::env::var(key) {
        return Ok(Some(v));
    }
    let file_key = format!("{key}_FILE");
    match std::env::var(&file_key) {
        Ok(path) => std::fs::read_to_string(&path)
            .map(|s| Some(s.trim().to_string()))
            .map_err(|e| format!("{file_key}={path}: {e}")),
        Err(_) => Ok(None),
    }
}

fn heartbeat_path() -> std::path::PathBuf {
    std::path::PathBuf::from(
        std::env::var("WORKER_HEARTBEAT_FILE")
            .unwrap_or_else(|_| "/tmp/payment-workers.heartbeat".to_string()),
    )
}

fn healthcheck() -> Result<(), BoxError> {
    let interval: u64 = env_or("WORKER_INTERVAL_SECS", 5)?;
    let max_age = Duration::from_secs((3 * interval).max(30));
    let age = std::fs::metadata(heartbeat_path())?.modified()?.elapsed()?;
    if age <= max_age {
        Ok(())
    } else {
        eprintln!("healthcheck failed: heartbeat is {}s old", age.as_secs());
        std::process::exit(1);
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    if std::env::args().nth(1).as_deref() == Some("healthcheck") {
        return healthcheck();
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

    let interval_secs: u64 = env_or("WORKER_INTERVAL_SECS", 5)?;
    let relay_interval_secs: u64 = env_or("RELAY_INTERVAL_SECS", 2)?;
    let batch_size: i64 = env_or("SEAL_BATCH_SIZE", 500)?;
    let reconcile_interval = Duration::from_secs(env_or("RECONCILE_INTERVAL_SECS", 60)?);
    let reconcile_full_every = Duration::from_secs(env_or("RECONCILE_FULL_EVERY_SECS", 86_400)?);
    let verify_interval = Duration::from_secs(env_or("VERIFY_INTERVAL_SECS", 300)?);
    let retention_interval = Duration::from_secs(env_or("RETENTION_INTERVAL_SECS", 24 * 3600)?);
    let retention = RetentionConfig {
        outbox_sent_days: env_or("RETENTION_OUTBOX_DAYS", 7)?,
        idempotency_days: env_or("RETENTION_IDEMPOTENCY_DAYS", 30)?,
        refresh_tokens_days: env_or("RETENTION_REFRESH_TOKENS_DAYS", 30)?,
        kyc_orphan_days: env_or("RETENTION_KYC_ORPHAN_DAYS", 7)?,
        document_dir: {
            let dir = std::path::PathBuf::from(
                std::env::var("DOCUMENT_STORE_DIR")
                    .unwrap_or_else(|_| "./kyc-documents".to_string()),
            );
            if dir.is_dir() {
                Some(dir)
            } else {
                tracing::warn!(dir = %dir.display(), "document dir not found — orphan KYC document pruning disabled");
                None
            }
        },
    };
    let max_batches_per_tick: u64 = env_or("MAX_BATCHES_PER_TICK", 40)?;

    let metrics_addr: std::net::SocketAddr = env_or(
        "METRICS_ADDR",
        "127.0.0.1:9101".parse::<std::net::SocketAddr>()?,
    )?;
    metrics_exporter_prometheus::PrometheusBuilder::new()
        .with_http_listener(metrics_addr)
        .install()
        .map_err(|e| format!("metrics exporter on {metrics_addr}: {e}"))?;
    tracing::info!(%metrics_addr, "metrics exporter listening");

    let sealer = match env_or_file("WORKER_SIGNING_KEY")? {
        Some(hex_key) => {
            let bytes = hex::decode(hex_key.trim())
                .map_err(|_| "WORKER_SIGNING_KEY must be hex-encoded")?;
            let arr: [u8; 32] = bytes
                .try_into()
                .map_err(|_| "WORKER_SIGNING_KEY must be 32 bytes (64 hex chars)")?;
            Sealer::from_secret_bytes(&arr)
        }
        None if is_prod => {
            return Err("WORKER_SIGNING_KEY must be set when APP_ENV != dev".into());
        }
        None => {
            tracing::warn!("WORKER_SIGNING_KEY not set — generating an EPHEMERAL key. Checkpoints will not verify across restarts. Do not use in production.");
            Sealer::generate()
        }
    };
    tracing::info!(public_key = %sealer.public_key_hex(), "sealer ready");

    let statement_timeout_ms: u64 = env_or("DB_STATEMENT_TIMEOUT_MS", 60_000)?;
    let idle_tx_timeout_ms: u64 = env_or("DB_IDLE_TX_TIMEOUT_MS", 60_000)?;
    let connect_options = PgConnectOptions::from_str(&database_url)?
        .application_name("payment-workers")
        .options([
            ("statement_timeout", statement_timeout_ms.to_string()),
            ("lock_timeout", "5000".to_string()),
            (
                "idle_in_transaction_session_timeout",
                idle_tx_timeout_ms.to_string(),
            ),
        ]);
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(connect_options)
        .await?;

    let cfg = Config {
        pool,
        sealer,
        interval_secs,
        relay_interval_secs,
        batch_size,
        max_batches_per_tick,
        reconcile_interval,
        reconcile_full_every,
        verify_interval,
        retention_interval,
        retention,
    };

    match env_or_file("NATS_URL")? {
        Some(url) => {
            let prefix = std::env::var("NATS_SUBJECT_PREFIX").unwrap_or_else(|_| "payments".into());
            let publisher = NatsPublisher::connect(&url, &prefix).await?;
            tracing::info!(%url, stream = publisher.stream(), "publishing events to NATS JetStream");
            run(cfg, publisher).await
        }
        None if is_prod => Err("NATS_URL must be set when APP_ENV != dev".into()),
        None => {
            tracing::warn!("NATS_URL not set — events will only be logged, not published");
            run(cfg, LoggingPublisher).await
        }
    }
}

struct Config {
    pool: PgPool,
    sealer: Sealer,
    interval_secs: u64,
    relay_interval_secs: u64,
    batch_size: i64,
    max_batches_per_tick: u64,
    reconcile_interval: Duration,
    reconcile_full_every: Duration,
    verify_interval: Duration,
    retention_interval: Duration,
    retention: RetentionConfig,
}

fn now_epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
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
}

async fn relay_loop<P: EventPublisher + Send + Sync + 'static>(
    pool: PgPool,
    publisher: P,
    interval_secs: u64,
    batch_size: i64,
    max_batches_per_tick: u64,
    mut stop: watch::Receiver<bool>,
) {
    let mut ticker = tokio::time::interval(Duration::from_secs(interval_secs.max(1)));
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stop.changed() => break,
        }
        let mut relayed = 0u64;
        for _ in 0..max_batches_per_tick {
            match workers::relay_once(&pool, &publisher, batch_size).await {
                Ok(n) => {
                    relayed += n;
                    if n < batch_size as u64 {
                        break;
                    }
                }
                Err(e) => {
                    metrics::counter!("worker_errors_total", "duty" => "relay").increment(1);
                    tracing::error!(error = %e, "outbox relay failed");
                    break;
                }
            }
        }
        if relayed > 0 {
            metrics::counter!("outbox_relayed_total").increment(relayed);
            tracing::info!(events = relayed, "relayed outbox events");
        }
        match workers::outbox_lag(&pool).await {
            Ok((unsent, oldest_age)) => {
                metrics::gauge!("outbox_unsent").set(unsent as f64);
                metrics::gauge!("outbox_oldest_unsent_age_seconds").set(oldest_age);
            }
            Err(e) => tracing::warn!(error = %e, "outbox lag query failed"),
        }
    }
    tracing::info!("relay loop stopped");
}

async fn run<P: EventPublisher + Send + Sync + 'static>(
    cfg: Config,
    publisher: P,
) -> Result<(), BoxError> {
    let (stop_tx, stop_rx) = watch::channel(false);
    {
        let stop_tx = stop_tx.clone();
        tokio::spawn(async move {
            shutdown_signal().await;
            tracing::info!("shutdown signal received; finishing the current batch");
            let _ = stop_tx.send(true);
        });
    }
    let relay = tokio::spawn(relay_loop(
        cfg.pool.clone(),
        publisher,
        cfg.relay_interval_secs,
        cfg.batch_size,
        cfg.max_batches_per_tick,
        stop_rx.clone(),
    ));

    let mut stop = stop_rx;
    let mut ticker = tokio::time::interval(Duration::from_secs(cfg.interval_secs.max(1)));
    let mut last_reconcile: Option<Instant> = None;
    let mut last_full_reconcile: Option<Instant> = None;
    let mut last_healthy_started_at: Option<f64> = None;
    let mut last_verify: Option<Instant> = None;
    let mut last_retention: Option<Instant> = None;
    let mut verify_state = VerifyState::genesis();
    let heartbeat = heartbeat_path();

    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stop.changed() => break,
        }
        metrics::counter!("worker_ticks_total").increment(1);

        let mut sealed = 0u64;
        for _ in 0..cfg.max_batches_per_tick {
            match workers::seal_next_batch(&cfg.pool, &cfg.sealer, cfg.batch_size).await {
                Ok(Some(_)) => sealed += 1,
                Ok(None) => break,
                Err(e) => {
                    metrics::counter!("worker_errors_total", "duty" => "seal").increment(1);
                    tracing::error!(error = %e, "sealing failed");
                    break;
                }
            }
        }
        if sealed > 0 {
            metrics::counter!("checkpoints_sealed_total").increment(sealed);
            tracing::info!(checkpoints = sealed, "sealed new checkpoints");
        }
        match workers::unsealed_lag(&cfg.pool).await {
            Ok((unsealed, oldest_age)) => {
                metrics::gauge!("ledger_unsealed_transactions").set(unsealed as f64);
                metrics::gauge!("ledger_oldest_unsealed_age_seconds").set(oldest_age);
            }
            Err(e) => tracing::warn!(error = %e, "unsealed lag query failed"),
        }

        if last_verify.is_none_or(|t| t.elapsed() >= cfg.verify_interval) {
            last_verify = Some(Instant::now());
            match workers::verify_chain_from(&cfg.pool, &verify_state).await {
                Ok((report, new_state)) => {
                    verify_state = new_state;
                    metrics::gauge!("ledger_chain_verified").set(1.0);
                    if report.checkpoints_verified > 0 {
                        tracing::info!(
                            checkpoints = report.checkpoints_verified,
                            transactions = report.transactions_covered,
                            "checkpoint chain verified"
                        );
                    }
                }
                Err(e) => {
                    metrics::gauge!("ledger_chain_verified").set(0.0);
                    metrics::counter!("worker_errors_total", "duty" => "verify").increment(1);
                    tracing::error!(
                        error = %e,
                        "CHAIN VERIFICATION FAILED — possible ledger tampering"
                    );
                }
            }
        }

        if last_reconcile.is_none_or(|t| t.elapsed() >= cfg.reconcile_interval) {
            last_reconcile = Some(Instant::now());
            let full = last_full_reconcile.is_none_or(|t| t.elapsed() >= cfg.reconcile_full_every);
            let started_at = now_epoch_secs();
            let since = if full {
                None
            } else {
                last_healthy_started_at.map(|t| t - 5.0)
            };
            match workers::reconcile_since(&cfg.pool, since).await {
                Ok(report) if report.is_healthy() => {
                    if full {
                        last_full_reconcile = Some(Instant::now());
                    }
                    last_healthy_started_at = Some(started_at);
                    metrics::gauge!("reconciliation_healthy").set(1.0);
                    metrics::gauge!("reconciliation_accounts_checked")
                        .set(report.accounts_checked as f64);
                    tracing::debug!(
                        full,
                        accounts = report.accounts_checked,
                        "reconciliation OK"
                    );
                }
                Ok(report) => {
                    metrics::gauge!("reconciliation_healthy").set(0.0);
                    metrics::counter!("worker_errors_total", "duty" => "reconcile").increment(1);
                    tracing::error!(
                        full,
                        imbalances = ?report.currency_imbalances,
                        mismatches = report.balance_mismatches.len(),
                        "RECONCILIATION FAILED — ledger inconsistency detected"
                    );
                }
                Err(e) => {
                    metrics::counter!("worker_errors_total", "duty" => "reconcile").increment(1);
                    tracing::error!(error = %e, "reconciliation error");
                }
            }
        }

        if last_retention.is_none_or(|t| t.elapsed() >= cfg.retention_interval) {
            last_retention = Some(Instant::now());
            match workers::prune_expired(&cfg.pool, &cfg.retention).await {
                Ok(report) => tracing::info!(
                    outbox = report.outbox,
                    idempotency_keys = report.idempotency_keys,
                    refresh_tokens = report.refresh_tokens,
                    kyc_documents = report.kyc_documents,
                    "pruned expired operational data"
                ),
                Err(e) => {
                    metrics::counter!("worker_errors_total", "duty" => "retention").increment(1);
                    tracing::error!(error = %e, "retention pruning failed");
                }
            }
        }

        if let Err(e) = tokio::fs::write(&heartbeat, format!("{}\n", now_epoch_secs())).await {
            tracing::warn!(error = %e, path = %heartbeat.display(), "cannot write heartbeat");
        }
    }

    let _ = relay.await;
    tracing::info!("payment-workers stopped");
    Ok(())
}
