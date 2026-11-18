//! `LEDGER_BACKEND=tigerbeetle`: settles reservations whose request died between the posting
//! protocol's steps (`HybridLedger::recover`, crates/ledger-tigerbeetle/src/recovery.rs).
//! Passes are idempotent and safe to overlap; the leader runs them only so replicas do not
//! repeat each other's work.

use std::time::{Duration, Instant};

use ledger_tigerbeetle::{AnyTb, HybridLedger, MirrorReport, RecoveryReport, TbError};
use tokio::sync::watch;

fn now_secs() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Counts what a pass settled: `posted` (committed, the request died before its post),
/// `voided` (never committed), `forced` (committed, but the reservation had expired: re-posted
/// as plain transfers; recovery was down for longer than TIGERBEETLE_PENDING_TIMEOUT_SECS / 2).
pub fn record_recovery(r: &RecoveryReport, by: &'static str) {
    for (outcome, n) in [
        ("posted", r.posted),
        ("voided", r.voided),
        ("forced", r.forced),
    ] {
        if n > 0 {
            metrics::counter!("tigerbeetle_recovered_total", "outcome" => outcome, "by" => by)
                .increment(n as u64);
        }
    }
    if r.forced > 0 {
        tracing::error!(
            forced = r.forced,
            "committed transactions outlived their reservations and were re-posted; recovery was down for too long"
        );
    }
    if r.posted + r.voided > 0 {
        tracing::info!(
            posted = r.posted,
            voided = r.voided,
            "settled abandoned TigerBeetle reservations"
        );
    }
}

/// One pass with metrics. A `Protocol` error means TigerBeetle answered something the protocol
/// rules out (an invariant is broken); it is counted apart so it can page.
pub async fn recover_once(ledger: &HybridLedger<AnyTb>) -> Result<RecoveryReport, TbError> {
    let started = Instant::now();
    let result = ledger.recover().await;
    metrics::histogram!("tigerbeetle_recovery_seconds").record(started.elapsed().as_secs_f64());
    match &result {
        Ok(r) => {
            record_recovery(r, "recovery");
            metrics::gauge!("tigerbeetle_recovery_last_success_timestamp_seconds").set(now_secs());
            metrics::gauge!("tigerbeetle_recovery_watermark").set(r.watermark as f64);
        }
        Err(e) => {
            let kind = match e {
                TbError::Protocol(_) => "protocol",
                TbError::Unavailable(_) | TbError::Retry(_) => "unavailable",
                TbError::Storage(_) => "storage",
            };
            metrics::counter!("tigerbeetle_recovery_errors_total", "kind" => kind).increment(1);
            tracing::error!(kind, error = %e, "TigerBeetle recovery pass failed");
        }
    }
    result
}

/// Runs [`recover_once`] every `interval` while `leader` is true, until `stop` changes.
/// `beat` is called every tick, leader or not (the worker's liveness).
pub async fn recovery_loop(
    ledger: HybridLedger<AnyTb>,
    interval: Duration,
    leader: watch::Receiver<bool>,
    mut stop: watch::Receiver<bool>,
    beat: impl Fn(),
) {
    // Staleness counts from the start, so a recovery that never once succeeds still alerts.
    metrics::gauge!("tigerbeetle_recovery_last_success_timestamp_seconds").set(now_secs());
    let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(100)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = ticker.tick() => {}
            _ = stop.changed() => break,
        }
        beat();
        if *leader.borrow() {
            let _ = recover_once(&ledger).await;
            beat();
        }
    }
    tracing::info!("TigerBeetle recovery loop stopped");
}

/// How long one mirror tick keeps running passes before it yields (and beats) again.
const MIRROR_SLICE: Duration = Duration::from_secs(1);

/// `TIGERBEETLE_POSTING=direct`: writes the journal of what the cluster applied since the last
/// pass (`HybridLedger::mirror_pass`), until nothing is left or the slice is over. Requests
/// that read the journal settle it themselves, so this loop is what keeps them from having to
/// (and what feeds the outbox). An integrity error means the journal already holds an id the
/// cluster posted directly: the keys rule that out, so it pages.
pub async fn mirror_once(ledger: &HybridLedger<AnyTb>) -> Result<MirrorReport, TbError> {
    let started = Instant::now();
    let mut total = MirrorReport::default();
    let result = loop {
        match ledger.mirror_pass().await {
            Ok(pass) => {
                total.scanned += pass.scanned;
                total.transactions += pass.transactions;
                total.voids += pass.voids;
                total.watermark = pass.watermark;
                total.more = pass.more;
                if !pass.more || started.elapsed() >= MIRROR_SLICE {
                    break Ok(total);
                }
            }
            Err(e) => break Err(e),
        }
    };
    metrics::histogram!("tigerbeetle_mirror_seconds").record(started.elapsed().as_secs_f64());
    match &result {
        Ok(r) => {
            for (kind, n) in [("transaction", r.transactions), ("void", r.voids)] {
                if n > 0 {
                    metrics::counter!("tigerbeetle_mirrored_total", "kind" => kind)
                        .increment(n as u64);
                }
            }
            metrics::gauge!("tigerbeetle_mirror_last_success_timestamp_seconds").set(now_secs());
            metrics::gauge!("tigerbeetle_mirror_watermark").set(r.watermark as f64);
            // With a backlog, how far behind the cluster's clock the journal is; none: current.
            let lag = match r.more {
                true => (now_secs() - r.watermark as f64 / 1e9).max(0.0),
                false => 0.0,
            };
            metrics::gauge!("tigerbeetle_mirror_lag_seconds").set(lag);
        }
        Err(e) => {
            let kind = match e {
                TbError::Protocol(_) => "protocol",
                TbError::Unavailable(_) | TbError::Retry(_) => "unavailable",
                TbError::Storage(storage::StorageError::DataIntegrity(_)) => "integrity",
                TbError::Storage(_) => "storage",
            };
            metrics::counter!("tigerbeetle_mirror_errors_total", "kind" => kind).increment(1);
            tracing::error!(kind, error = %e, "TigerBeetle journal mirror pass failed");
        }
    }
    result
}

/// Runs [`mirror_once`] every `interval` while `leader` is true, until `stop` changes; at once
/// again while a backlog remains. `beat` is called every tick, leader or not.
pub async fn mirror_loop(
    ledger: HybridLedger<AnyTb>,
    interval: Duration,
    leader: watch::Receiver<bool>,
    mut stop: watch::Receiver<bool>,
    beat: impl Fn(),
) {
    metrics::gauge!("tigerbeetle_mirror_last_success_timestamp_seconds").set(now_secs());
    let mut ticker = tokio::time::interval(interval.max(Duration::from_millis(10)));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut backlog = false;
    loop {
        if !backlog {
            tokio::select! {
                _ = ticker.tick() => {}
                _ = stop.changed() => break,
            }
        } else if *stop.borrow() {
            break;
        }
        beat();
        backlog = false;
        if *leader.borrow() {
            backlog = mirror_once(&ledger).await.is_ok_and(|r| r.more);
            beat();
        }
    }
    tracing::info!("TigerBeetle journal mirror loop stopped");
}
