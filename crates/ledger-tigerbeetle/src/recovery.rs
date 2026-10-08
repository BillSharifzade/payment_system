//! Settles reservations whose request died between the protocol's steps.
//!
//! A pass scans the cluster's reservations (`user_data_32 = TAG_RESERVE`) above a watermark
//! kept in Postgres per cluster, skips those already posted or voided, and resolves the rest
//! through the tombstone (`HybridLedger::resolve`). The scan never reaches past the newest
//! reservation it has seen: TigerBeetle timestamps strictly increase, so every reservation
//! created later sorts above the new watermark and none is skipped, whatever the clocks say.
//! Passes are idempotent and safe to run concurrently.

use std::collections::HashMap;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use uuid::Uuid;

use crate::client::{QueryFilter, TbClient, Transfer, LOOKUP_BATCH};
use crate::error::Result;
use crate::hybrid::{Attempt, HybridLedger, NoFaults, Probe, Resolved};
use crate::ids::{self, leg, LEG_MASK};
use crate::tb::low64;

/// Reservation legs per scan page; settling a page looks up two ids per attempt.
const PAGE: u32 = (LOOKUP_BATCH / 2) as u32;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RecoveryReport {
    /// Reservation legs read.
    pub scanned: usize,
    /// Attempts already posted or voided by their request.
    pub settled: usize,
    pub posted: usize,
    /// Committed attempts whose reservation had expired, re-posted as plain transfers.
    pub forced: usize,
    pub voided: usize,
    /// Every reservation at or below this TigerBeetle timestamp is settled.
    pub watermark: u64,
}

fn reservations(min: u64, max: u64, limit: u32, reversed: bool) -> QueryFilter {
    QueryFilter {
        user_data_32: ids::TAG_RESERVE,
        timestamp_min: min,
        timestamp_max: max,
        limit,
        reversed,
        ..QueryFilter::default()
    }
}

impl<C: TbClient> HybridLedger<C> {
    /// One pass, leaving reservations younger than the configured grace to their requests.
    pub async fn recover(&self) -> Result<RecoveryReport> {
        self.recover_probed(self.tb().config().recovery_grace, &NoFaults)
            .await
    }

    /// A pass with no grace, for when nothing is in flight (the cut-over commands, after a
    /// load test): it settles every reservation, so a request still running would lose its
    /// reservation and fail as retryable — safe, but not what a serving system wants.
    pub async fn recover_quiesced(&self) -> Result<RecoveryReport> {
        self.recover_probed(Duration::ZERO, &NoFaults).await
    }

    pub(crate) async fn recover_probed<P: Probe>(
        &self,
        grace: Duration,
        probe: &P,
    ) -> Result<RecoveryReport> {
        let cluster = Uuid::from_u128(self.tb().client().cluster_id());
        sqlx::query(
            "INSERT INTO tb_recovery_watermark (cluster_id, through_timestamp) VALUES ($1, 0)
             ON CONFLICT (cluster_id) DO NOTHING",
        )
        .bind(cluster)
        .execute(self.pool())
        .await?;
        let start: i64 = sqlx::query_scalar(
            "SELECT through_timestamp FROM tb_recovery_watermark WHERE cluster_id = $1",
        )
        .bind(cluster)
        .fetch_one(self.pool())
        .await?;
        let mut report = RecoveryReport {
            watermark: start as u64,
            ..RecoveryReport::default()
        };
        let client = self.tb().client();
        let newest = self
            .tb()
            .call(client.query_transfers(reservations(0, 0, 1, true)))
            .await?;
        let Some(newest) = newest.first() else {
            return Ok(report);
        };
        // The cluster's clock is at least at its newest timestamp; the grace only decides
        // whom an in-flight attempt is left to, so a skewed estimate costs latency, not safety.
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock after 1970")
            .as_nanos() as u64;
        let horizon = newest.timestamp.min(
            now.max(newest.timestamp)
                .saturating_sub(grace.as_nanos() as u64),
        );

        while report.watermark < horizon {
            let page = self
                .tb()
                .call(client.query_transfers(reservations(
                    report.watermark + 1,
                    horizon,
                    PAGE,
                    false,
                )))
                .await?;
            let Some(last) = page.last().map(|t| t.timestamp) else {
                break;
            };
            let full = page.len() == PAGE as usize;
            report.scanned += page.len();
            let attempts = self.attempts(page).await?;
            let settled = self.settled(&attempts).await?;
            for ((attempt, first_ts), done) in attempts.iter().zip(settled) {
                if done {
                    report.settled += 1;
                    continue;
                }
                match self.resolve(attempt, probe).await {
                    Ok(Resolved::Posted) => report.posted += 1,
                    Ok(Resolved::Forced) => report.forced += 1,
                    Ok(Resolved::Voided) => report.voided += 1,
                    Err(e) => {
                        self.save_watermark(cluster, first_ts - 1).await?;
                        return Err(e);
                    }
                }
            }
            report.watermark = last;
            self.save_watermark(cluster, last).await?;
            if !full {
                break;
            }
        }
        Ok(report)
    }

    async fn save_watermark(&self, cluster: Uuid, through: u64) -> Result<()> {
        sqlx::query(
            "UPDATE tb_recovery_watermark SET through_timestamp = $2, updated_at = now()
             WHERE cluster_id = $1 AND through_timestamp < $2",
        )
        .bind(cluster)
        .bind(through as i64)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Groups a page into attempts (in timestamp order), fetching the legs of a chain the
    /// page boundary cut.
    async fn attempts(&self, page: Vec<Transfer>) -> Result<Vec<(Attempt, u64)>> {
        let mut groups: Vec<(u128, u64, Vec<Transfer>)> = Vec::new();
        for t in page {
            let base = t.id & !LEG_MASK;
            match groups.last_mut() {
                Some((b, _, legs)) if *b == base => legs.push(t),
                _ => groups.push((base, t.timestamp, vec![t])),
            }
        }
        let mut out = Vec::with_capacity(groups.len());
        for (base, first_ts, legs) in groups {
            let legs = if legs.len() as u64 == legs[0].user_data_64 {
                legs
            } else {
                let all = (0..legs[0].user_data_64 as usize)
                    .map(|i| leg(base, i))
                    .collect();
                self.tb()
                    .call(self.tb().client().lookup_transfers(all))
                    .await?
            };
            out.push((Attempt::from_reservations(legs)?, first_ts));
        }
        Ok(out)
    }

    /// Whether the request (or an earlier pass) already posted or voided each attempt.
    async fn settled(&self, attempts: &[(Attempt, u64)]) -> Result<Vec<bool>> {
        let ids: Vec<u128> = attempts
            .iter()
            .flat_map(|(a, _)| {
                [
                    leg(ids::void_base(a.base), 0),
                    leg(ids::post_base(a.transaction), 0),
                ]
            })
            .collect();
        let found: HashMap<u128, Transfer> = self
            .tb()
            .call(self.tb().client().lookup_transfers(ids))
            .await?
            .into_iter()
            .map(|t| (t.id, t))
            .collect();
        Ok(attempts
            .iter()
            .map(|(a, _)| {
                found.contains_key(&leg(ids::void_base(a.base), 0))
                    || found
                        .get(&leg(ids::post_base(a.transaction), 0))
                        .is_some_and(|p| {
                            p.pending_id == leg(a.base, 0)
                                || (p.user_data_32 == ids::TAG_FORCED
                                    && p.user_data_64 == low64(a.base))
                        })
            })
            .collect())
    }
}
