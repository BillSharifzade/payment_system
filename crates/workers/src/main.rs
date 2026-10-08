use std::collections::BTreeSet;
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crypto::{Sealer, TrustedKeys};
use sqlx::postgres::{PgConnectOptions, PgPoolOptions};
use sqlx::PgPool;
use tokio::sync::watch;
use tokio::time::MissedTickBehavior;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;
use workers::{
    EventPublisher, FullReconciliation, LeaderLock, LoggingPublisher, NatsPublisher,
    ReconcileConfig, RetentionConfig, VerifyReport, VerifyState, WorkerError, LEADER_LOCK_KEY,
};

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

fn is_prod() -> bool {
    std::env::var("APP_ENV")
        .map(|v| v != "dev")
        .unwrap_or(false)
}

fn database_url() -> Result<String, BoxError> {
    match env_or_file("DATABASE_URL")? {
        Some(url) => Ok(url),
        None if is_prod() => Err("DATABASE_URL must be set when APP_ENV != dev".into()),
        None => Ok("postgres://payment:payment_dev_pw@localhost:5432/payment".to_string()),
    }
}

fn connect_options(database_url: &str) -> Result<PgConnectOptions, BoxError> {
    let statement_timeout_ms: u64 = env_or("DB_STATEMENT_TIMEOUT_MS", 60_000)?;
    let idle_tx_timeout_ms: u64 = env_or("DB_IDLE_TX_TIMEOUT_MS", 60_000)?;
    Ok(PgConnectOptions::from_str(database_url)?
        .application_name("payment-workers")
        .options([
            ("statement_timeout", statement_timeout_ms.to_string()),
            ("lock_timeout", "5000".to_string()),
            (
                "idle_in_transaction_session_timeout",
                idle_tx_timeout_ms.to_string(),
            ),
        ]))
}

fn signing_key() -> Result<Option<Sealer>, BoxError> {
    let Some(hex_key) = env_or_file("WORKER_SIGNING_KEY")? else {
        return Ok(None);
    };
    let bytes =
        hex::decode(hex_key.trim()).map_err(|_| "WORKER_SIGNING_KEY must be hex-encoded")?;
    let arr: [u8; 32] = bytes
        .try_into()
        .map_err(|_| "WORKER_SIGNING_KEY must be 32 bytes (64 hex chars)")?;
    Ok(Some(Sealer::from_secret_bytes(&arr)))
}

/// WORKER_TRUSTED_PUBLIC_KEYS plus the current signing key's public key. Keep
/// a rotated-out key in the list for as long as checkpoints it signed exist.
fn trusted_keys(sealer: Option<&Sealer>) -> Result<TrustedKeys, BoxError> {
    let mut keys = match env_or_file("WORKER_TRUSTED_PUBLIC_KEYS")? {
        Some(list) => TrustedKeys::from_hex_list(&list)
            .map_err(|e| format!("WORKER_TRUSTED_PUBLIC_KEYS: {e}"))?,
        None => TrustedKeys::new(),
    };
    if let Some(sealer) = sealer {
        keys.insert(sealer.public_key_bytes())?;
    }
    Ok(keys)
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

/// `payment-workers verify-chain`: full verification of the checkpoint chain in
/// DATABASE_URL against the trusted keys. One JSON object on stdout; exit 0 =
/// intact, 1 = broken, 2 = could not verify.
async fn verify_chain_command() -> i32 {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_writer(std::io::stderr)
        .json()
        .init();
    let started = Instant::now();
    let (code, mut report) = match verify_chain_report().await {
        Ok((true, report)) => (0, report),
        Ok((false, report)) => (1, report),
        Err(e) => (
            2,
            serde_json::json!({"status": "error", "error": e.to_string()}),
        ),
    };
    report["elapsed_ms"] = serde_json::json!(started.elapsed().as_millis() as u64);
    println!("{report}");
    code
}

async fn verify_chain_report() -> Result<(bool, serde_json::Value), BoxError> {
    let sealer = signing_key()?;
    let trusted = trusted_keys(sealer.as_ref())?;
    if trusted.is_empty() {
        return Err(
            "no trusted public keys: set WORKER_TRUSTED_PUBLIC_KEYS (or WORKER_SIGNING_KEY)".into(),
        );
    }
    let pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(connect_options(&database_url()?)?)
        .await?;

    let mut state = VerifyState::genesis();
    let mut report = VerifyReport::default();
    let outcome = loop {
        match workers::verify_chain_page(&pool, &trusted, &state).await {
            Ok(page) => {
                report.checkpoints_verified += page.report.checkpoints_verified;
                report.transactions_covered += page.report.transactions_covered;
                state = page.state;
                if page.done {
                    break Ok(());
                }
            }
            Err(e) => break Err(e),
        }
    };
    let (unsealed, _) = workers::unsealed_lag(&pool).await?;
    let mut json = serde_json::json!({
        "checkpoints_verified": report.checkpoints_verified,
        "transactions_covered": report.transactions_covered,
        "last_checkpoint_seq": state.last_checkpoint_seq,
        "last_checkpoint_hash": hex::encode(state.last_checkpoint_hash),
        "last_sealed_seq": state.last_to_txn_seq,
        "unsealed_transactions": unsealed,
        "trusted_keys": trusted.to_hex(),
    });
    match outcome {
        Ok(()) => {
            json["status"] = "intact".into();
            Ok((true, json))
        }
        Err(WorkerError::ChainBroken { seq, reason }) => {
            json["status"] = "broken".into();
            json["broken_at_seq"] = seq.into();
            json["reason"] = reason.into();
            Ok((false, json))
        }
        Err(e) => Err(e.into()),
    }
}

#[tokio::main]
async fn main() -> Result<(), BoxError> {
    match std::env::args().nth(1).as_deref() {
        None => {}
        Some("healthcheck") => return healthcheck(),
        Some("verify-chain") => std::process::exit(verify_chain_command().await),
        Some(other) => {
            return Err(format!(
                "unknown subcommand {other:?} (expected healthcheck or verify-chain)"
            )
            .into())
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .json()
        .init();

    let database_url = database_url()?;
    let interval_secs: u64 = env_or("WORKER_INTERVAL_SECS", 5)?;
    let relay_interval_secs: u64 = env_or("RELAY_INTERVAL_SECS", 2)?;
    let batch_size: i64 = env_or("SEAL_BATCH_SIZE", 500)?;
    let reconcile_interval = Duration::from_secs(env_or("RECONCILE_INTERVAL_SECS", 60)?);
    let reconcile_full_every = Duration::from_secs(env_or("RECONCILE_FULL_EVERY_SECS", 86_400)?);
    let verify_interval = Duration::from_secs(env_or("VERIFY_INTERVAL_SECS", 300)?);
    let verify_full_every = Duration::from_secs(env_or("VERIFY_FULL_EVERY_SECS", 21_600)?);
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

    let sealer = match signing_key()? {
        Some(sealer) => sealer,
        None if is_prod() => {
            return Err("WORKER_SIGNING_KEY must be set when APP_ENV != dev".into());
        }
        None => {
            tracing::warn!("WORKER_SIGNING_KEY not set — generating an EPHEMERAL key. Checkpoints will not verify across restarts. Do not use in production.");
            Sealer::generate()
        }
    };
    let trusted = trusted_keys(Some(&sealer))?;
    tracing::info!(public_key = %sealer.public_key_hex(), trusted_keys = ?trusted.to_hex(), "sealer ready");

    let connect_options = connect_options(&database_url)?;
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .acquire_timeout(Duration::from_secs(10))
        .connect_with(connect_options.clone())
        .await?;

    let cfg = Config {
        pool,
        connect_options,
        sealer,
        trusted,
        interval_secs,
        relay_interval_secs,
        batch_size,
        max_batches_per_tick,
        reconcile_interval,
        reconcile_full_every,
        verify_interval,
        verify_full_every,
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
        None if is_prod() => Err("NATS_URL must be set when APP_ENV != dev".into()),
        None => {
            tracing::warn!("NATS_URL not set — events will only be logged, not published");
            run(cfg, LoggingPublisher).await
        }
    }
}

struct Config {
    pool: PgPool,
    connect_options: PgConnectOptions,
    sealer: Sealer,
    trusted: TrustedKeys,
    interval_secs: u64,
    relay_interval_secs: u64,
    batch_size: i64,
    max_batches_per_tick: u64,
    reconcile_interval: Duration,
    reconcile_full_every: Duration,
    verify_interval: Duration,
    verify_full_every: Duration,
    retention_interval: Duration,
    retention: RetentionConfig,
}

fn now_epoch_secs() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

fn ticker(period: Duration) -> tokio::time::Interval {
    let mut ticker = tokio::time::interval(period.max(Duration::from_secs(1)));
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    ticker
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

/// Last time each loop completed a tick. The heartbeat file is only refreshed
/// while every loop is making progress, so a wedged loop fails the healthcheck
/// even though the process is alive.
#[derive(Default)]
struct Liveness {
    relay: AtomicU64,
    seal: AtomicU64,
    duties: AtomicU64,
}

impl Liveness {
    fn beat(slot: &AtomicU64) {
        slot.store(now_epoch_secs() as u64, Ordering::Relaxed);
    }

    fn age(slot: &AtomicU64) -> u64 {
        (now_epoch_secs() as u64).saturating_sub(slot.load(Ordering::Relaxed))
    }
}

async fn heartbeat_loop(
    liveness: Arc<Liveness>,
    interval_secs: u64,
    relay_interval_secs: u64,
    mut stop: watch::Receiver<bool>,
) {
    let path = heartbeat_path();
    let limits = [
        ("relay", &liveness.relay, 3 * relay_interval_secs + 120),
        ("seal", &liveness.seal, 3 * interval_secs + 120),
        ("duties", &liveness.duties, 3 * interval_secs + 600),
    ];
    let mut ticker = ticker(Duration::from_secs(interval_secs));
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stop.changed() => break,
        }
        let stale: Vec<&str> = limits
            .iter()
            .filter(|(_, slot, limit)| Liveness::age(slot) > *limit)
            .map(|(name, _, _)| *name)
            .collect();
        if !stale.is_empty() {
            tracing::error!(loops = ?stale, "worker loop stalled; withholding heartbeat");
            continue;
        }
        if let Err(e) = tokio::fs::write(&path, format!("{}\n", now_epoch_secs())).await {
            tracing::warn!(error = %e, path = %path.display(), "cannot write heartbeat");
        }
    }
}

async fn election_loop(
    options: PgConnectOptions,
    leader: watch::Sender<bool>,
    interval_secs: u64,
    mut stop: watch::Receiver<bool>,
) -> Option<LeaderLock> {
    let timeout = Duration::from_secs(interval_secs.max(5));
    let mut held: Option<LeaderLock> = None;
    let mut ticker = ticker(Duration::from_secs(interval_secs));
    metrics::gauge!("worker_leader").set(0.0);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stop.changed() => break,
        }
        match held.as_mut() {
            Some(lock) => {
                if !matches!(
                    tokio::time::timeout(timeout, lock.check()).await,
                    Ok(Ok(()))
                ) {
                    held = None;
                    leader.send_replace(false);
                    metrics::gauge!("worker_leader").set(0.0);
                    tracing::error!("lost the leader lock; sealing, verification, reconciliation and retention paused");
                }
            }
            None => {
                match tokio::time::timeout(
                    timeout,
                    LeaderLock::try_acquire(&options, LEADER_LOCK_KEY),
                )
                .await
                {
                    Ok(Ok(Some(lock))) => {
                        held = Some(lock);
                        leader.send_replace(true);
                        metrics::gauge!("worker_leader").set(1.0);
                        tracing::info!("acquired the leader lock; this replica seals, verifies, reconciles and prunes");
                    }
                    Ok(Ok(None)) => {}
                    Ok(Err(e)) => tracing::warn!(error = %e, "leader election failed"),
                    Err(_) => tracing::warn!("leader election timed out"),
                }
            }
        }
    }
    held
}

async fn relay_loop<P: EventPublisher + Send + Sync + 'static>(
    pool: PgPool,
    publisher: P,
    interval_secs: u64,
    batch_size: i64,
    max_batches_per_tick: u64,
    liveness: Arc<Liveness>,
    mut stop: watch::Receiver<bool>,
) {
    let mut ticker = ticker(Duration::from_secs(interval_secs));
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
        Liveness::beat(&liveness.relay);
    }
    tracing::info!("relay loop stopped");
}

/// Sealing has its own loop so a long verification or reconciliation pass
/// never delays it.
#[allow(clippy::too_many_arguments)]
async fn seal_loop(
    pool: PgPool,
    sealer: Sealer,
    interval_secs: u64,
    batch_size: i64,
    max_batches_per_tick: u64,
    leader: watch::Receiver<bool>,
    liveness: Arc<Liveness>,
    mut stop: watch::Receiver<bool>,
) {
    let mut ticker = ticker(Duration::from_secs(interval_secs));
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stop.changed() => break,
        }
        Liveness::beat(&liveness.seal);
        if !*leader.borrow() {
            continue;
        }
        metrics::counter!("worker_ticks_total").increment(1);
        let mut sealed = 0u64;
        for _ in 0..max_batches_per_tick {
            match workers::seal_next_batch(&pool, &sealer, batch_size).await {
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
        match workers::unsealed_lag(&pool).await {
            Ok((unsealed, oldest_age)) => {
                metrics::gauge!("ledger_unsealed_transactions").set(unsealed as f64);
                metrics::gauge!("ledger_oldest_unsealed_age_seconds").set(oldest_age);
            }
            Err(e) => tracing::warn!(error = %e, "unsealed lag query failed"),
        }
        Liveness::beat(&liveness.seal);
    }
    tracing::info!("seal loop stopped");
}

/// Verification, reconciliation and retention. Full passes advance in bounded
/// steps under a per-tick time budget, so incremental checks keep their cadence
/// while a full pass over a large ledger is in progress.
struct Duties {
    pool: PgPool,
    trusted: TrustedKeys,
    budget: Duration,
    verify_interval: Duration,
    verify_full_every: Duration,
    reconcile_interval: Duration,
    reconcile_full_every: Duration,
    retention_interval: Duration,
    retention: RetentionConfig,
    reconcile: ReconcileConfig,

    // None until the first full verification completes; then advances only
    // over new checkpoints.
    verify_state: Option<VerifyState>,
    verify_pending: bool,
    last_verify: Option<Instant>,
    full_verify: Option<(VerifyState, VerifyReport, Instant)>,
    next_full_verify: Instant,
    incremental_chain_broken: bool,
    full_chain_broken: bool,

    // Accounts whose balance disagreed; re-checked every pass until they agree.
    suspects: BTreeSet<Uuid>,
    reconcile_backlog: bool,
    last_reconcile: Option<Instant>,
    unbalanced_seen: bool,
    full_reconcile: Option<(FullReconciliation, Instant)>,
    next_full_reconcile: Instant,
    full_reconcile_unhealthy: bool,

    last_retention: Option<Instant>,
}

impl Duties {
    fn new(cfg: &Config) -> Self {
        let now = Instant::now();
        Self {
            pool: cfg.pool.clone(),
            trusted: cfg.trusted.clone(),
            budget: Duration::from_secs(cfg.interval_secs.max(1)),
            verify_interval: cfg.verify_interval,
            verify_full_every: cfg.verify_full_every,
            reconcile_interval: cfg.reconcile_interval,
            reconcile_full_every: cfg.reconcile_full_every,
            retention_interval: cfg.retention_interval,
            retention: cfg.retention.clone(),
            reconcile: ReconcileConfig::default(),
            verify_state: None,
            verify_pending: false,
            last_verify: None,
            full_verify: None,
            next_full_verify: now,
            incremental_chain_broken: false,
            full_chain_broken: false,
            suspects: BTreeSet::new(),
            reconcile_backlog: false,
            last_reconcile: None,
            unbalanced_seen: false,
            full_reconcile: None,
            next_full_reconcile: now,
            full_reconcile_unhealthy: false,
            last_retention: None,
        }
    }

    async fn tick(&mut self) {
        self.verify_incremental(Instant::now() + self.budget).await;
        self.reconcile_incremental().await;
        self.prune().await;

        let now = Instant::now();
        let deadline = now + self.budget;
        if self.full_verify.is_none() && now >= self.next_full_verify {
            self.full_verify = Some((VerifyState::genesis(), VerifyReport::default(), now));
        }
        if self.full_reconcile.is_none() && now >= self.next_full_reconcile {
            self.full_reconcile = Some((FullReconciliation::new(), now));
        }
        // At least one step each per tick, however long the incremental work
        // took; a step that fails or finishes sits out the rest of the tick.
        let (mut verifying, mut reconciling) = (true, true);
        while verifying || reconciling {
            if verifying {
                verifying = self.full_verify_step().await;
            }
            if reconciling {
                reconciling = self.full_reconcile_step().await;
            }
            if Instant::now() >= deadline {
                break;
            }
        }

        metrics::gauge!("ledger_chain_verified").set(
            if self.incremental_chain_broken || self.full_chain_broken {
                0.0
            } else {
                1.0
            },
        );
        metrics::gauge!("reconciliation_healthy").set(
            if self.suspects.is_empty() && !self.unbalanced_seen && !self.full_reconcile_unhealthy {
                1.0
            } else {
                0.0
            },
        );
    }

    async fn verify_incremental(&mut self, deadline: Instant) {
        let Some(mut state) = self.verify_state else {
            return;
        };
        if !self.verify_pending
            && self
                .last_verify
                .is_some_and(|t| t.elapsed() < self.verify_interval)
        {
            return;
        }
        self.last_verify = Some(Instant::now());
        let mut report = VerifyReport::default();
        self.verify_pending = true;
        while Instant::now() < deadline {
            match workers::verify_chain_page(&self.pool, &self.trusted, &state).await {
                Ok(page) => {
                    report.checkpoints_verified += page.report.checkpoints_verified;
                    report.transactions_covered += page.report.transactions_covered;
                    state = page.state;
                    if page.done {
                        self.verify_pending = false;
                        self.incremental_chain_broken = false;
                        break;
                    }
                }
                Err(e) => {
                    self.verify_pending = false;
                    self.chain_error("incremental", &e);
                    break;
                }
            }
        }
        self.verify_state = Some(state);
        if report.checkpoints_verified > 0 {
            tracing::info!(
                checkpoints = report.checkpoints_verified,
                transactions = report.transactions_covered,
                through_seq = state.last_checkpoint_seq,
                "checkpoint chain verified"
            );
        }
    }

    fn chain_error(&mut self, pass: &str, e: &WorkerError) {
        metrics::counter!("worker_errors_total", "duty" => "verify").increment(1);
        match e {
            WorkerError::ChainBroken { .. } => {
                if pass == "full" {
                    self.full_chain_broken = true;
                } else {
                    self.incremental_chain_broken = true;
                }
                tracing::error!(pass, error = %e, "CHAIN VERIFICATION FAILED — possible ledger tampering");
            }
            _ => tracing::error!(pass, error = %e, "chain verification error"),
        }
    }

    /// One page of the full re-verification. Returns whether it did work.
    async fn full_verify_step(&mut self) -> bool {
        let Some((state, report, _)) = self.full_verify.as_mut() else {
            return false;
        };
        match workers::verify_chain_page(&self.pool, &self.trusted, state).await {
            Ok(page) => {
                report.checkpoints_verified += page.report.checkpoints_verified;
                report.transactions_covered += page.report.transactions_covered;
                *state = page.state;
                if page.done {
                    let (state, report, started) = self.full_verify.take().expect("checked above");
                    self.full_chain_broken = false;
                    self.next_full_verify = started + self.verify_full_every;
                    if self
                        .verify_state
                        .is_none_or(|s| s.last_checkpoint_seq < state.last_checkpoint_seq)
                    {
                        self.verify_state = Some(state);
                    }
                    metrics::gauge!("ledger_chain_full_verified_timestamp_seconds")
                        .set(now_epoch_secs());
                    tracing::info!(
                        checkpoints = report.checkpoints_verified,
                        transactions = report.transactions_covered,
                        elapsed_secs = started.elapsed().as_secs(),
                        "full checkpoint chain re-verification OK"
                    );
                }
                true
            }
            Err(e) => {
                let broken = matches!(e, WorkerError::ChainBroken { .. });
                self.chain_error("full", &e);
                if broken {
                    // Re-run soon, so the gauge clears once history is restored.
                    self.full_verify = None;
                    self.next_full_verify = Instant::now() + self.verify_interval;
                }
                false
            }
        }
    }

    async fn reconcile_incremental(&mut self) {
        if !self.reconcile_backlog
            && self
                .last_reconcile
                .is_some_and(|t| t.elapsed() < self.reconcile_interval)
        {
            return;
        }
        self.last_reconcile = Some(Instant::now());
        let suspects: Vec<Uuid> = self.suspects.iter().copied().collect();
        let report =
            match workers::reconcile_incremental(&self.pool, &self.reconcile, &suspects).await {
                Ok(report) => report,
                Err(e) => {
                    metrics::counter!("worker_errors_total", "duty" => "reconcile").increment(1);
                    tracing::error!(error = %e, "reconciliation error");
                    return;
                }
            };
        if report.folded_unchecked > 0 {
            tracing::info!(
                transactions = report.folded_unchecked,
                remaining = report.backlog_remaining,
                "folded a reconciliation backlog; a full pass will cover its accounts"
            );
            if self.full_reconcile.is_none() {
                self.next_full_reconcile = Instant::now();
            }
        }
        self.reconcile_backlog = report.backlog_remaining;
        if report.backlog_remaining {
            return;
        }
        self.suspects = report
            .balance_mismatches
            .iter()
            .map(|m| m.account_id)
            .collect();
        self.unbalanced_seen = !report.unbalanced_transactions.is_empty();
        metrics::gauge!("reconciliation_accounts_checked").set(report.accounts_checked as f64);
        if report.is_healthy() {
            tracing::debug!(
                accounts = report.accounts_checked,
                transactions = report.transactions_folded,
                "reconciliation OK"
            );
        } else {
            metrics::counter!("worker_errors_total", "duty" => "reconcile").increment(1);
            tracing::error!(
                mismatches = ?report.balance_mismatches,
                unbalanced_transactions = ?report.unbalanced_transactions,
                "RECONCILIATION FAILED — ledger inconsistency detected"
            );
        }
    }

    /// One step of the full reconciliation. Returns whether it did work.
    async fn full_reconcile_step(&mut self) -> bool {
        let Some((full, started)) = self.full_reconcile.as_mut() else {
            return false;
        };
        let started = *started;
        match full.step(&self.pool, &self.reconcile).await {
            Ok(false) => true,
            Ok(true) => {
                let report = self
                    .full_reconcile
                    .take()
                    .expect("checked above")
                    .0
                    .into_report();
                self.suspects
                    .extend(report.balance_mismatches.iter().map(|m| m.account_id));
                self.full_reconcile_unhealthy =
                    !report.currency_imbalances.is_empty() || !report.sum_mismatches.is_empty();
                self.next_full_reconcile = if report.is_healthy() {
                    started + self.reconcile_full_every
                } else {
                    Instant::now() + self.reconcile_interval
                };
                if report.is_healthy() {
                    metrics::gauge!("reconciliation_full_timestamp_seconds").set(now_epoch_secs());
                    tracing::info!(
                        accounts = report.accounts_checked,
                        elapsed_secs = started.elapsed().as_secs(),
                        "full reconciliation OK"
                    );
                } else {
                    metrics::counter!("worker_errors_total", "duty" => "reconcile").increment(1);
                    tracing::error!(
                        imbalances = ?report.currency_imbalances,
                        mismatches = ?report.balance_mismatches,
                        sum_mismatches = ?report.sum_mismatches,
                        "FULL RECONCILIATION FAILED — ledger inconsistency detected"
                    );
                }
                true
            }
            Err(e) => {
                metrics::counter!("worker_errors_total", "duty" => "reconcile").increment(1);
                tracing::error!(error = %e, "full reconciliation error; retrying next tick");
                false
            }
        }
    }

    async fn prune(&mut self) {
        if self
            .last_retention
            .is_some_and(|t| t.elapsed() < self.retention_interval)
        {
            return;
        }
        self.last_retention = Some(Instant::now());
        match workers::prune_expired(&self.pool, &self.retention).await {
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
}

async fn duty_loop(
    mut duties: Duties,
    interval_secs: u64,
    leader: watch::Receiver<bool>,
    liveness: Arc<Liveness>,
    mut stop: watch::Receiver<bool>,
) {
    let mut ticker = ticker(Duration::from_secs(interval_secs));
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stop.changed() => break,
        }
        if *leader.borrow() {
            duties.tick().await;
        }
        Liveness::beat(&liveness.duties);
    }
    tracing::info!("duty loop stopped");
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
    let liveness = Arc::new(Liveness::default());
    for slot in [&liveness.relay, &liveness.seal, &liveness.duties] {
        Liveness::beat(slot);
    }
    let (leader_tx, leader_rx) = watch::channel(false);

    let election = tokio::spawn(election_loop(
        cfg.connect_options.clone(),
        leader_tx,
        cfg.interval_secs,
        stop_rx.clone(),
    ));
    let relay = tokio::spawn(relay_loop(
        cfg.pool.clone(),
        publisher,
        cfg.relay_interval_secs,
        cfg.batch_size,
        cfg.max_batches_per_tick,
        liveness.clone(),
        stop_rx.clone(),
    ));
    let duties = tokio::spawn(duty_loop(
        Duties::new(&cfg),
        cfg.interval_secs,
        leader_rx.clone(),
        liveness.clone(),
        stop_rx.clone(),
    ));
    let heartbeat = tokio::spawn(heartbeat_loop(
        liveness.clone(),
        cfg.interval_secs,
        cfg.relay_interval_secs,
        stop_rx.clone(),
    ));
    let seal = tokio::spawn(seal_loop(
        cfg.pool,
        cfg.sealer,
        cfg.interval_secs,
        cfg.batch_size,
        cfg.max_batches_per_tick,
        leader_rx,
        liveness,
        stop_rx,
    ));

    let _ = tokio::join!(relay, seal, duties, heartbeat);
    // Released only after the leader-only loops have finished their batch.
    if let Ok(Some(lock)) = election.await {
        if let Err(e) = lock.release().await {
            tracing::warn!(error = %e, "could not release the leader lock cleanly");
        }
    }
    tracing::info!("payment-workers stopped");
    Ok(())
}
