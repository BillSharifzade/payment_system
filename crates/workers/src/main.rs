//! Background worker process: relays outbox events, seals new transactions into
//! signed checkpoints, verifies the checkpoint chain, runs reconciliation, and
//! prunes expired operational data — alerting on any inconsistency.

use std::time::{Duration, Instant};

use crypto::Sealer;
use sqlx::postgres::PgPoolOptions;
use sqlx::PgPool;
use tracing_subscriber::EnvFilter;
use workers::{EventPublisher, LoggingPublisher, NatsPublisher, RetentionConfig, VerifyState};

fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key)
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(default)
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    // Same fail-fast discipline as the API server: outside APP_ENV=dev, refuse
    // to run on insecure defaults instead of warning and limping on.
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

    let interval_secs = env_u64("WORKER_INTERVAL_SECS", 5);
    let batch_size = env_u64("SEAL_BATCH_SIZE", 500) as i64;
    // Heavier passes run on their own cadence: reconciliation re-derives every
    // account from its entries (O(history)), and chain verification re-hashes
    // newly sealed batches. Neither belongs on the hot 5s tick.
    let reconcile_interval = Duration::from_secs(env_u64("RECONCILE_INTERVAL_SECS", 60));
    let verify_interval = Duration::from_secs(env_u64("VERIFY_INTERVAL_SECS", 300));
    let retention_interval = Duration::from_secs(env_u64("RETENTION_INTERVAL_SECS", 24 * 3600));
    let retention = RetentionConfig {
        outbox_sent_days: env_u64("RETENTION_OUTBOX_DAYS", 7) as i32,
        idempotency_days: env_u64("RETENTION_IDEMPOTENCY_DAYS", 30) as i32,
        refresh_tokens_days: env_u64("RETENTION_REFRESH_TOKENS_DAYS", 30) as i32,
        kyc_orphan_days: env_u64("RETENTION_KYC_ORPHAN_DAYS", 7) as i32,
        // Must point at the same volume the API writes documents to; pruning
        // is skipped (rows kept) if the directory does not exist here.
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
    // Cap how many batches each duty may process per tick, so a sustained
    // write burst can't trap the loop in catch-up and starve the other duties.
    let max_batches_per_tick = env_u64("MAX_BATCHES_PER_TICK", 40);

    // The signing key must come from a secret store (Vault/HSM) in production.
    let sealer = match std::env::var("WORKER_SIGNING_KEY") {
        Ok(hex_key) => {
            let bytes = hex::decode(hex_key.trim())
                .map_err(|_| "WORKER_SIGNING_KEY must be hex-encoded")?;
            let arr: [u8; 32] = bytes
                .try_into()
                .map_err(|_| "WORKER_SIGNING_KEY must be 32 bytes (64 hex chars)")?;
            Sealer::from_secret_bytes(&arr)
        }
        Err(_) if is_prod => {
            // An ephemeral key would sign checkpoints nobody can verify after a
            // restart — silently worthless tamper-evidence. Refuse.
            return Err("WORKER_SIGNING_KEY must be set when APP_ENV != dev".into());
        }
        Err(_) => {
            tracing::warn!("WORKER_SIGNING_KEY not set — generating an EPHEMERAL key. Checkpoints will not verify across restarts. Do not use in production.");
            Sealer::generate()
        }
    };
    tracing::info!(public_key = %sealer.public_key_hex(), "sealer ready");

    let pool = PgPoolOptions::new()
        .max_connections(8)
        .connect(&database_url)
        .await?;

    // Publish to NATS if configured, otherwise just log (dev default).
    match std::env::var("NATS_URL") {
        Ok(url) => {
            let prefix = std::env::var("NATS_SUBJECT_PREFIX").unwrap_or_else(|_| "payments".into());
            let publisher = NatsPublisher::connect(&url, &prefix).await?;
            tracing::info!(%url, "publishing events to NATS");
            run(Config {
                pool,
                sealer,
                publisher,
                interval_secs,
                batch_size,
                max_batches_per_tick,
                reconcile_interval,
                verify_interval,
                retention_interval,
                retention,
            })
            .await
        }
        Err(_) if is_prod => Err("NATS_URL must be set when APP_ENV != dev".into()),
        Err(_) => {
            tracing::warn!("NATS_URL not set — events will only be logged, not published");
            run(Config {
                pool,
                sealer,
                publisher: LoggingPublisher,
                interval_secs,
                batch_size,
                max_batches_per_tick,
                reconcile_interval,
                verify_interval,
                retention_interval,
                retention,
            })
            .await
        }
    }
}

struct Config<P> {
    pool: PgPool,
    sealer: Sealer,
    publisher: P,
    interval_secs: u64,
    batch_size: i64,
    max_batches_per_tick: u64,
    reconcile_interval: Duration,
    verify_interval: Duration,
    retention_interval: Duration,
    retention: RetentionConfig,
}

/// The worker loop: relay outbox events and seal checkpoints every tick;
/// verify, reconcile, and prune on their own (longer) cadences.
async fn run<P: EventPublisher>(cfg: Config<P>) -> Result<(), Box<dyn std::error::Error>> {
    let mut ticker = tokio::time::interval(Duration::from_secs(cfg.interval_secs));
    // Run the slow passes immediately on startup, then on their intervals.
    let mut last_reconcile: Option<Instant> = None;
    let mut last_verify: Option<Instant> = None;
    let mut last_retention: Option<Instant> = None;
    // Verification is incremental: each pass picks up after the last verified
    // checkpoint. Starting from genesis on boot re-checks the whole chain once,
    // which doubles as the startup integrity audit.
    let mut verify_state = VerifyState::genesis();

    loop {
        ticker.tick().await;

        // Relay outbox events first, so downstream consumers see movements
        // promptly; bounded per tick so catch-up can't starve the other duties.
        let mut relayed = 0u64;
        for _ in 0..cfg.max_batches_per_tick {
            match workers::relay_once(&cfg.pool, &cfg.publisher, cfg.batch_size).await {
                Ok(n) => {
                    relayed += n;
                    if n < cfg.batch_size as u64 {
                        break;
                    }
                }
                Err(e) => {
                    tracing::error!(error = %e, "outbox relay failed");
                    break;
                }
            }
        }
        if relayed > 0 {
            tracing::info!(events = relayed, "relayed outbox events");
        }

        let mut sealed = 0u64;
        for _ in 0..cfg.max_batches_per_tick {
            match workers::seal_next_batch(&cfg.pool, &cfg.sealer, cfg.batch_size).await {
                Ok(Some(_)) => sealed += 1,
                Ok(None) => break,
                Err(e) => {
                    tracing::error!(error = %e, "sealing failed");
                    break;
                }
            }
        }
        if sealed > 0 {
            tracing::info!(checkpoints = sealed, "sealed new checkpoints");
        }

        // Independent verification of the signed checkpoint chain (incremental).
        if last_verify.is_none_or(|t| t.elapsed() >= cfg.verify_interval) {
            last_verify = Some(Instant::now());
            match workers::verify_chain_from(&cfg.pool, &verify_state).await {
                Ok((report, new_state)) => {
                    verify_state = new_state;
                    if report.checkpoints_verified > 0 {
                        tracing::info!(
                            checkpoints = report.checkpoints_verified,
                            transactions = report.transactions_covered,
                            "checkpoint chain verified"
                        );
                    }
                }
                Err(e) => tracing::error!(
                    error = %e,
                    "CHAIN VERIFICATION FAILED — possible ledger tampering"
                ),
            }
        }

        if last_reconcile.is_none_or(|t| t.elapsed() >= cfg.reconcile_interval) {
            last_reconcile = Some(Instant::now());
            match workers::reconcile(&cfg.pool).await {
                Ok(report) if report.is_healthy() => tracing::debug!("reconciliation OK"),
                Ok(report) => tracing::error!(
                    imbalances = ?report.currency_imbalances,
                    mismatches = report.balance_mismatches.len(),
                    "RECONCILIATION FAILED — ledger inconsistency detected"
                ),
                Err(e) => tracing::error!(error = %e, "reconciliation error"),
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
                Err(e) => tracing::error!(error = %e, "retention pruning failed"),
            }
        }
    }
}
