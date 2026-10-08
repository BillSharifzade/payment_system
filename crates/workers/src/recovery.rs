//! `LEDGER_BACKEND=tigerbeetle`: settles reservations whose request died between the posting
//! protocol's steps (`HybridLedger::recover`, crates/ledger-tigerbeetle/src/recovery.rs).
//! Passes are idempotent and safe to overlap; the leader runs them only so replicas do not
//! repeat each other's work.

use std::time::{Duration, Instant};

use ledger_tigerbeetle::{AnyTb, HybridLedger, RecoveryReport, TbError};
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
