//! The journal of direct postings: Postgres rows written from the cluster.
//!
//! Under `TIGERBEETLE_POSTING=direct` a transfer is committed when the cluster applies its
//! chain; nothing is written to Postgres on the request path. A mirror pass reads the chains
//! the cluster applied since the last pass (every transfer of one carries `TAG_JOURNALED`,
//! and timestamps strictly increase, so "since" is a single watermark) and writes what
//! `PostgresLedger` would have written: the `transactions` row, one entry per account and
//! side, the outbox event and, for a screened posting, its `allowed` screening event; for a
//! void, the claimed id and the `voided_transactions` row.
//!
//! Exactly once: a pass locks the cluster's watermark row, and the rows and the new watermark
//! commit together, so passes may run anywhere and concurrently — the workers' leader loops
//! over them, and a request that reads the journal (a statement, a transaction lookup) runs
//! them itself until the journal holds everything the cluster had when it asked
//! ([`settle_journal`](HybridLedger::settle_journal)): a client reads its own writes without
//! depending on the workers.
//!
//! The journal is net per account: a direct posting's entries are rebuilt from its legs, so
//! two entries of the original transaction on the same side of one account arrive as one.

use std::collections::{BTreeMap, HashSet};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use ledger::{AccountId, AccountType};
use sqlx::PgConnection;
use storage::StorageError;
use uuid::Uuid;

use crate::client::{QueryFilter, TbClient, Transfer};
use crate::config::Posting;
use crate::error::{Result, TbError};
use crate::hybrid::HybridLedger;
use crate::ids;

/// Journaled transfers per pass (a reply holds at most 8189).
const PAGE: u32 = 4_000;
/// How long a reader waits for the journal to reach what the cluster had when it asked.
const SETTLE_BUDGET: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MirrorReport {
    /// Journaled transfers read.
    pub scanned: usize,
    /// Postings written to the journal.
    pub transactions: usize,
    pub voids: usize,
    /// Every journaled transfer at or below this cluster timestamp is in Postgres.
    pub watermark: u64,
    /// The page was full: more is waiting.
    pub more: bool,
}

struct Posted {
    transaction: u128,
    /// The key's timestamp: when the cluster applied the chain.
    timestamp: u64,
    screened: bool,
    legs: Vec<Transfer>,
}

struct Voided {
    transaction: u128,
    by: u128,
    timestamp: u64,
}

/// One journal entry: an account's total on one side of a posting.
struct EntryRow {
    account: u128,
    credit: bool,
    amount: i64,
    currency: String,
}

fn journaled(min: u64, limit: u32, reversed: bool) -> QueryFilter {
    QueryFilter {
        user_data_32: ids::TAG_JOURNALED,
        timestamp_min: min,
        limit,
        reversed,
        ..QueryFilter::default()
    }
}

fn integrity(message: String) -> TbError {
    StorageError::DataIntegrity(message).into()
}

/// Nanoseconds since the epoch as a `timestamptz`, exactly (to its microseconds).
const AT: &str = "'epoch'::timestamptz + (u.ts / 1000) * interval '1 microsecond'";

impl<C: TbClient> HybridLedger<C> {
    /// The timestamp of the newest journaled transfer in the cluster (0 for none).
    async fn newest_journaled(&self) -> Result<u64> {
        let newest = self
            .tb()
            .call(self.tb().client().query_transfers(journaled(0, 1, true)))
            .await?;
        Ok(newest.first().map_or(0, |t| t.timestamp))
    }

    /// Starts this database's journal of the cluster at the cluster's present: if the
    /// database has no watermark for it yet, nothing the cluster journaled before now is this
    /// database's (it was not under direct posting on this cluster). Run when direct posting
    /// starts; a no-op ever after.
    pub async fn mirror_from_here(&self) -> Result<()> {
        let newest = self.newest_journaled().await?;
        sqlx::query(
            "INSERT INTO tb_mirror_watermark (cluster_id, through_timestamp) VALUES ($1, $2)
             ON CONFLICT (cluster_id) DO NOTHING",
        )
        .bind(self.cluster())
        .bind(newest as i64)
        .execute(self.pool())
        .await?;
        Ok(())
    }

    /// Whether this database has ever journaled from this cluster.
    pub(crate) async fn has_mirrored(&self) -> Result<bool> {
        Ok(sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM tb_mirror_watermark WHERE cluster_id = $1)",
        )
        .bind(self.cluster())
        .fetch_one(self.pool())
        .await?)
    }

    /// Makes the journal hold every direct posting and void the cluster had applied when this
    /// was called. A no-op under hybrid posting, and cheap when the mirror is current: one
    /// query to the cluster, plus one watermark read when this process has not seen it yet.
    pub async fn settle_journal(&self) -> Result<()> {
        if self.tb().config().posting != Posting::Direct {
            return Ok(());
        }
        let target = self.newest_journaled().await?;
        if self.mirror_seen.load(Ordering::Relaxed) >= target {
            return Ok(());
        }
        let stored: Option<i64> = sqlx::query_scalar(
            "SELECT through_timestamp FROM tb_mirror_watermark WHERE cluster_id = $1",
        )
        .bind(self.cluster())
        .fetch_optional(self.pool())
        .await?;
        self.mirror_seen
            .fetch_max(stored.unwrap_or(0) as u64, Ordering::Relaxed);
        let started = Instant::now();
        while self.mirror_seen.load(Ordering::Relaxed) < target {
            if started.elapsed() >= SETTLE_BUDGET {
                return Err(TbError::Unavailable(
                    "the journal mirror did not catch up with the cluster".into(),
                ));
            }
            self.mirror_pass().await?;
        }
        Ok(())
    }

    /// Runs passes until nothing is left to mirror (the cut-over commands, tests, the end of a
    /// load test), returning the totals.
    pub async fn mirror_all(&self) -> Result<MirrorReport> {
        let mut total = MirrorReport::default();
        loop {
            let pass = self.mirror_pass().await?;
            total.scanned += pass.scanned;
            total.transactions += pass.transactions;
            total.voids += pass.voids;
            total.watermark = pass.watermark;
            if !pass.more {
                return Ok(total);
            }
        }
    }

    /// One pass: up to a page of journaled transfers, written with the new watermark in one
    /// Postgres transaction. Safe to run concurrently with itself.
    pub async fn mirror_pass(&self) -> Result<MirrorReport> {
        let cluster = self.cluster();
        let mut db = self.pool().begin().await?;
        let lock = "SELECT through_timestamp FROM tb_mirror_watermark
                    WHERE cluster_id = $1 FOR UPDATE";
        let start: i64 = match sqlx::query_scalar(lock)
            .bind(cluster)
            .fetch_optional(&mut *db)
            .await?
        {
            Some(w) => w,
            None => {
                sqlx::query(
                    "INSERT INTO tb_mirror_watermark (cluster_id, through_timestamp)
                     VALUES ($1, 0) ON CONFLICT (cluster_id) DO NOTHING",
                )
                .bind(cluster)
                .execute(&mut *db)
                .await?;
                sqlx::query_scalar(lock)
                    .bind(cluster)
                    .fetch_one(&mut *db)
                    .await?
            }
        };
        let mut report = MirrorReport {
            watermark: start as u64,
            ..MirrorReport::default()
        };
        let page = self
            .tb()
            .call(
                self.tb()
                    .client()
                    .query_transfers(journaled(start as u64 + 1, PAGE, false)),
            )
            .await?;
        if page.is_empty() {
            db.rollback().await?;
            self.mirror_seen
                .fetch_max(report.watermark, Ordering::Relaxed);
            return Ok(report);
        }
        report.scanned = page.len();
        report.more = page.len() == PAGE as usize;
        let (posted, voided, through) = self.chains(page, report.more).await?;
        let written = self.write(&mut db, &posted, &voided).await?;
        report.transactions = written.0;
        report.voids = written.1;
        sqlx::query(
            "UPDATE tb_mirror_watermark SET through_timestamp = $2, updated_at = now()
             WHERE cluster_id = $1",
        )
        .bind(cluster)
        .bind(through as i64)
        .execute(&mut *db)
        .await?;
        db.commit().await?;
        report.watermark = through;
        self.mirror_seen.fetch_max(through, Ordering::Relaxed);
        Ok(report)
    }

    /// Groups a page into chains. A chain is applied as one request, so its transfers carry
    /// consecutive timestamps: a key, then everything of its transaction. A chain the page
    /// boundary may have cut is read again whole.
    async fn chains(
        &self,
        page: Vec<Transfer>,
        full: bool,
    ) -> Result<(Vec<Posted>, Vec<Voided>, u64)> {
        let client = self.tb().client();
        let (mut posted, mut voided) = (Vec::new(), Vec::new());
        let mut through = 0;
        let mut i = 0;
        while i < page.len() {
            let key = page[i];
            let transaction = key.user_data_128;
            match key.code {
                ids::TRANSFER_KEY_DIRECT => {
                    let belongs = |t: &Transfer| {
                        t.user_data_128 == transaction
                            && matches!(
                                t.code,
                                ids::TRANSFER_LEG | ids::TRANSFER_AML_DAY | ids::TRANSFER_AML_HOUR
                            )
                    };
                    let mut j = i + 1;
                    while j < page.len() && belongs(&page[j]) {
                        j += 1;
                    }
                    let rest: Vec<Transfer> = if j == page.len() && full {
                        self.tb()
                            .call(client.query_transfers(QueryFilter {
                                user_data_128: transaction,
                                timestamp_min: key.timestamp + 1,
                                limit: (ids::MAX_LEGS + 2) as u32,
                                ..journaled(0, 0, false)
                            }))
                            .await?
                            .into_iter()
                            .filter(belongs)
                            .collect()
                    } else {
                        page[i + 1..j].to_vec()
                    };
                    let legs: Vec<Transfer> = rest
                        .iter()
                        .filter(|t| t.code == ids::TRANSFER_LEG)
                        .copied()
                        .collect();
                    if legs
                        .first()
                        .is_some_and(|l| l.user_data_64 != legs.len() as u64)
                    {
                        return Err(TbError::Protocol(format!(
                            "direct posting {} has {} of its legs",
                            Uuid::from_u128(transaction),
                            legs.len()
                        )));
                    }
                    through = rest.last().map_or(key.timestamp, |t| t.timestamp);
                    posted.push(Posted {
                        transaction,
                        timestamp: key.timestamp,
                        screened: rest.len() > legs.len(),
                        legs,
                    });
                    i = j;
                }
                ids::TRANSFER_KEY_VOID => {
                    let witness = match page.get(i + 1) {
                        Some(w) => Some(*w),
                        None => self
                            .tb()
                            .call(client.lookup_transfers(vec![key.id | 1]))
                            .await?
                            .first()
                            .copied(),
                    };
                    let witness = witness
                        .filter(|w| w.id == key.id | 1 && w.code == ids::TRANSFER_VOID_WITNESS)
                        .ok_or_else(|| {
                            TbError::Protocol(format!(
                                "void of {} has no witness",
                                Uuid::from_u128(transaction)
                            ))
                        })?;
                    through = witness.timestamp;
                    voided.push(Voided {
                        transaction,
                        by: witness.user_data_128,
                        timestamp: key.timestamp,
                    });
                    i += 2;
                }
                code => {
                    return Err(TbError::Protocol(format!(
                        "journaled transfer {:032x} (code {code}) does not start a chain",
                        key.id
                    )))
                }
            }
        }
        Ok((posted, voided, through))
    }

    /// The journal rows of one page. Returns how many postings and voids were new.
    async fn write(
        &self,
        db: &mut PgConnection,
        posted: &[Posted],
        voided: &[Voided],
    ) -> Result<(usize, usize)> {
        // Claim the ids. One this journal already holds must be the same cluster event (a
        // watermark that was reset re-reads it); anything else is two transactions under
        // one id, which the keys rule out.
        let (ids, stamps): (Vec<Uuid>, Vec<i64>) = posted
            .iter()
            .map(|p| (p.transaction, p.timestamp))
            .chain(voided.iter().map(|v| (v.transaction, v.timestamp)))
            .map(|(t, ts)| (Uuid::from_u128(t), ts as i64))
            .unzip();
        let new: HashSet<Uuid> = sqlx::query_scalar(&format!(
            "INSERT INTO transactions (id, created_at, tb_timestamp)
             SELECT u.id, {AT}, u.ts FROM UNNEST($1::uuid[], $2::bigint[]) AS u(id, ts)
             ON CONFLICT (id) DO NOTHING
             RETURNING id"
        ))
        .bind(&ids)
        .bind(&stamps)
        .fetch_all(&mut *db)
        .await?
        .into_iter()
        .collect();
        if new.len() < ids.len() {
            let foreign: Vec<Uuid> = sqlx::query_scalar(
                "SELECT u.id FROM UNNEST($1::uuid[], $2::bigint[]) AS u(id, ts)
                 JOIN transactions t ON t.id = u.id
                 WHERE t.tb_timestamp IS DISTINCT FROM u.ts",
            )
            .bind(&ids)
            .bind(&stamps)
            .fetch_all(&mut *db)
            .await?;
            if let Some(id) = foreign.first() {
                return Err(integrity(format!(
                    "the cluster posted transaction {id} directly, but the journal already \
                     holds that id from elsewhere ({} such)",
                    foreign.len()
                )));
            }
        }

        let posted: Vec<&Posted> = posted
            .iter()
            .filter(|p| new.contains(&Uuid::from_u128(p.transaction)))
            .collect();
        let accounts: Vec<AccountId> = posted
            .iter()
            .flat_map(|p| &p.legs)
            .flat_map(|l| [l.debit_account_id, l.credit_account_id])
            .collect::<HashSet<u128>>()
            .into_iter()
            .map(|a| AccountId(Uuid::from_u128(a)))
            .collect();
        let info = self.tb().account_info(&accounts).await?;
        let wallet = |account: u128| {
            info.get(&AccountId(Uuid::from_u128(account)))
                .is_some_and(|i| i.account_type == AccountType::UserWallet)
        };

        let mut e = EntryColumns::default();
        let mut events: (Vec<Uuid>, Vec<Uuid>, Vec<serde_json::Value>) = Default::default();
        let mut s = ScreeningColumns::default();
        for p in &posted {
            let txn = Uuid::from_u128(p.transaction);
            let rows = entry_rows(p)?;
            let payload: Vec<serde_json::Value> = rows
                .iter()
                .map(|r| {
                    serde_json::json!({
                        "account_id": Uuid::from_u128(r.account),
                        "direction": direction(r.credit),
                        "amount_minor": r.amount,
                        "currency": r.currency,
                    })
                })
                .collect();
            events
                .0
                .push(Uuid::from_u128(ids::mirror_event(p.transaction)));
            events.1.push(txn);
            events
                .2
                .push(serde_json::json!({ "transaction_id": txn, "entries": payload }));
            if p.screened {
                // What `ScreenCtx` names: the payer's wallet and the wallet it paid (or
                // converted) into.
                let from = rows.iter().find(|r| !r.credit && wallet(r.account));
                let to = rows.iter().find(|r| r.credit && wallet(r.account));
                if let Some(from) = from {
                    s.ids
                        .push(Uuid::from_u128(ids::mirror_screening(p.transaction)));
                    s.from.push(Uuid::from_u128(from.account));
                    s.to.push(to.map(|r| Uuid::from_u128(r.account)));
                    s.amounts.push(from.amount);
                    s.currencies.push(from.currency.clone());
                    s.stamps.push(p.timestamp as i64);
                }
            }
            for r in rows {
                e.ids.push(Uuid::from_u128(ids::mirror_entry(
                    p.transaction,
                    r.account,
                    r.credit,
                )));
                e.transactions.push(txn);
                e.accounts.push(Uuid::from_u128(r.account));
                e.directions.push(direction(r.credit));
                e.amounts.push(r.amount);
                e.currencies.push(r.currency);
                e.stamps.push(p.timestamp as i64);
            }
        }
        if !e.ids.is_empty() {
            sqlx::query(&format!(
                "INSERT INTO entries
                   (id, transaction_id, account_id, direction, amount_minor, currency, created_at)
                 SELECT u.id, u.txn, u.account, u.direction, u.amount, u.currency, {AT}
                 FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[], $4::text[], $5::bigint[],
                             $6::text[], $7::bigint[])
                      AS u(id, txn, account, direction, amount, currency, ts)"
            ))
            .bind(&e.ids)
            .bind(&e.transactions)
            .bind(&e.accounts)
            .bind(&e.directions)
            .bind(&e.amounts)
            .bind(&e.currencies)
            .bind(&e.stamps)
            .execute(&mut *db)
            .await?;
        }
        if !events.0.is_empty() {
            sqlx::query(
                "INSERT INTO outbox (id, aggregate_id, event_type, payload)
                 SELECT u.id, u.txn, 'transaction.posted', u.payload
                 FROM UNNEST($1::uuid[], $2::uuid[], $3::jsonb[]) AS u(id, txn, payload)",
            )
            .bind(&events.0)
            .bind(&events.1)
            .bind(&events.2)
            .execute(&mut *db)
            .await?;
        }
        if !s.ids.is_empty() {
            sqlx::query(&format!(
                "INSERT INTO screening_events
                   (id, user_id, from_account, to_account, amount_minor, currency, decision,
                    created_at)
                 SELECT u.id, a.owner_user_id, u.from_account, u.to_account, u.amount,
                        u.currency, 'allowed', {AT}
                 FROM UNNEST($1::uuid[], $2::uuid[], $3::uuid[], $4::bigint[], $5::text[],
                             $6::bigint[])
                      AS u(id, from_account, to_account, amount, currency, ts)
                 LEFT JOIN accounts a ON a.id = u.from_account"
            ))
            .bind(&s.ids)
            .bind(&s.from)
            .bind(&s.to)
            .bind(&s.amounts)
            .bind(&s.currencies)
            .bind(&s.stamps)
            .execute(&mut *db)
            .await?;
        }

        let voided: Vec<&Voided> = voided
            .iter()
            .filter(|v| new.contains(&Uuid::from_u128(v.transaction)))
            .collect();
        if !voided.is_empty() {
            let (ids, by, stamps): (Vec<Uuid>, Vec<Uuid>, Vec<i64>) = voided.iter().fold(
                Default::default(),
                |mut acc: (Vec<Uuid>, Vec<Uuid>, Vec<i64>), v| {
                    acc.0.push(Uuid::from_u128(v.transaction));
                    acc.1.push(Uuid::from_u128(v.by));
                    acc.2.push(v.timestamp as i64);
                    acc
                },
            );
            sqlx::query(&format!(
                "INSERT INTO voided_transactions (id, voided_by, created_at)
                 SELECT u.id, u.by, {AT}
                 FROM UNNEST($1::uuid[], $2::uuid[], $3::bigint[]) AS u(id, by, ts)"
            ))
            .bind(&ids)
            .bind(&by)
            .bind(&stamps)
            .execute(&mut *db)
            .await?;
        }
        Ok((posted.len(), voided.len()))
    }
}

#[derive(Default)]
struct EntryColumns {
    ids: Vec<Uuid>,
    transactions: Vec<Uuid>,
    accounts: Vec<Uuid>,
    directions: Vec<&'static str>,
    amounts: Vec<i64>,
    currencies: Vec<String>,
    stamps: Vec<i64>,
}

#[derive(Default)]
struct ScreeningColumns {
    ids: Vec<Uuid>,
    from: Vec<Uuid>,
    to: Vec<Option<Uuid>>,
    amounts: Vec<i64>,
    currencies: Vec<String>,
    stamps: Vec<i64>,
}

fn direction(credit: bool) -> &'static str {
    if credit {
        "credit"
    } else {
        "debit"
    }
}

/// A posting's entries: per account and side, what its legs add up to; debits first, then by
/// account, so the rows (and the outbox payload) are the same whoever mirrors them.
fn entry_rows(p: &Posted) -> Result<Vec<EntryRow>> {
    let mut sums: BTreeMap<(bool, u128), (u128, u32)> = BTreeMap::new();
    for l in &p.legs {
        for (credit, account) in [(false, l.debit_account_id), (true, l.credit_account_id)] {
            let slot = sums.entry((credit, account)).or_insert((0, l.ledger));
            slot.0 += l.amount;
        }
    }
    sums.into_iter()
        .map(|((credit, account), (amount, ledger))| {
            let currency = ids::currency_of(ledger).ok_or_else(|| {
                TbError::Protocol(format!(
                    "leg of {} on ledger {ledger}, which is no currency",
                    Uuid::from_u128(p.transaction)
                ))
            })?;
            Ok(EntryRow {
                account,
                credit,
                amount: i64::try_from(amount).map_err(|_| {
                    TbError::Storage(StorageError::AmountTooLarge(
                        i128::try_from(amount).unwrap_or(i128::MAX),
                    ))
                })?,
                currency: currency.code().to_string(),
            })
        })
        .collect()
}
