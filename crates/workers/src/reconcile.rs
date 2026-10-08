use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;

use crate::error::{Result, WorkerError};
use ledger::AccountId;
use ledger_tigerbeetle::{AnyTb, HybridLedger, RecoveryReport};
use sqlx::{PgConnection, PgPool, Postgres, Row};
use uuid::Uuid;

/// Where the balances under reconciliation are stored.
#[derive(Clone, Copy)]
pub enum Balances<'a> {
    /// The `balances` table, read in the reconciliation's own snapshot.
    Postgres,
    /// TigerBeetle (`LEDGER_BACKEND=tigerbeetle`, which no longer writes `balances`). Its
    /// balances are read outside the snapshot, so a mismatch may only be a committed post
    /// still in flight; each one is re-checked exactly after a recovery pass ([`confirm`]).
    TigerBeetle(&'a HybridLedger<AnyTb>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReconcileConfig {
    /// Sealed transactions folded into `reconciled_sums` per database transaction.
    pub fold_chunk: i64,
    /// Backlog chunks folded per incremental call; the rest waits for the next call.
    pub max_fold_chunks: u32,
    /// Entries one full-pass statement may re-sum (accounts are grouped by
    /// `balances.version`; an account above it is paged on its own).
    pub full_chunk_entries: i64,
}

impl Default for ReconcileConfig {
    fn default() -> Self {
        Self {
            fold_chunk: 50_000,
            max_fold_chunks: 20,
            full_chunk_entries: 100_000,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BalanceMismatch {
    pub account_id: Uuid,
    pub stored_minor: i64,
    pub derived_minor: i64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReconciliationReport {
    /// Full pass: currencies whose balances do not sum to zero.
    pub currency_imbalances: Vec<(String, i64)>,
    /// Incremental pass: new transactions whose entries do not net to zero.
    pub unbalanced_transactions: Vec<Uuid>,
    /// Stored balances that differ from the signed sum of their entries.
    pub balance_mismatches: Vec<BalanceMismatch>,
    /// Full pass: `reconciled_sums` rows that differ from the entries they
    /// summarise. Never rewritten automatically: the sums are a second record
    /// of what history added up to when it was folded, so entries changed
    /// after the fact stay flagged until restored.
    pub sum_mismatches: Vec<BalanceMismatch>,
    pub accounts_checked: i64,
    pub transactions_folded: i64,
    /// Folded while catching up a backlog, without a balance check; the
    /// accounts they touched are only covered by the next full pass.
    pub folded_unchecked: i64,
    /// The backlog was not fully folded in this call and no check ran.
    pub backlog_remaining: bool,
    /// TigerBeetle: accounts whose re-check kept meeting posts applied while it read them
    /// (a very busy account), so it could neither confirm nor clear them. Not a mismatch;
    /// the caller re-checks them on its next pass.
    pub inconclusive: Vec<Uuid>,
    /// TigerBeetle: the recovery passes the re-checks ran first.
    pub recovered: RecoveryReport,
}

impl ReconciliationReport {
    pub fn is_healthy(&self) -> bool {
        self.currency_imbalances.is_empty()
            && self.unbalanced_transactions.is_empty()
            && self.balance_mismatches.is_empty()
            && self.sum_mismatches.is_empty()
    }

    pub fn merge(&mut self, other: ReconciliationReport) {
        self.currency_imbalances.extend(other.currency_imbalances);
        self.unbalanced_transactions
            .extend(other.unbalanced_transactions);
        self.balance_mismatches.extend(other.balance_mismatches);
        self.sum_mismatches.extend(other.sum_mismatches);
        self.inconclusive.extend(other.inconclusive);
        absorb(&mut self.recovered, &other.recovered);
        self.accounts_checked += other.accounts_checked;
        self.transactions_folded += other.transactions_folded;
        self.folded_unchecked += other.folded_unchecked;
        self.backlog_remaining = other.backlog_remaining;
    }
}

fn absorb(into: &mut RecoveryReport, r: &RecoveryReport) {
    into.scanned += r.scanned;
    into.settled += r.settled;
    into.posted += r.posted;
    into.forced += r.forced;
    into.voided += r.voided;
    into.watermark = into.watermark.max(r.watermark);
}

/// Ids of the transactions not yet folded into `reconciled_sums`: sealed after
/// the watermark (parameter `$w`) or not sealed at all. Two index scans (the
/// sealed_seq index and the partial unsealed index), so the cost follows the
/// tail, never the ledger.
macro_rules! tail_ids {
    ($w:literal) => {
        concat!(
            "SELECT id FROM transactions WHERE sealed_seq > ",
            $w,
            " UNION ALL SELECT id FROM transactions WHERE sealed_seq IS NULL"
        )
    };
}

async fn snapshot(pool: &PgPool) -> Result<sqlx::Transaction<'static, Postgres>> {
    let mut db = pool.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ")
        .execute(&mut *db)
        .await?;
    Ok(db)
}

async fn watermark(conn: &mut PgConnection) -> Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT through_sealed_seq FROM reconcile_watermark")
            .fetch_one(conn)
            .await?,
    )
}

async fn fold(db: &mut PgConnection, after: i64, through: i64) -> Result<i64> {
    if through <= after {
        return Ok(0);
    }
    sqlx::query(
        "INSERT INTO reconciled_sums (account_id, sum_minor)
         SELECT e.account_id,
                SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                     ELSE -e.amount_minor END)::BIGINT
         FROM transactions t JOIN entries e ON e.transaction_id = t.id
         WHERE t.sealed_seq > $1 AND t.sealed_seq <= $2
         GROUP BY e.account_id
         ON CONFLICT (account_id) DO UPDATE
         SET sum_minor = reconciled_sums.sum_minor + EXCLUDED.sum_minor, updated_at = now()",
    )
    .bind(after)
    .bind(through)
    .execute(&mut *db)
    .await?;
    sqlx::query("UPDATE reconcile_watermark SET through_sealed_seq = $1, updated_at = now()")
        .bind(through)
        .execute(&mut *db)
        .await?;
    Ok(through - after)
}

/// One incremental pass, O(transactions since the last pass): every account
/// touched by a transaction sealed after the watermark or not yet sealed (plus
/// `also_check`, e.g. accounts that mismatched before) must satisfy
/// `balance = reconciled_sums + tail`, every tail transaction must net to zero
/// per currency, and the sealed part of the tail is then folded into the sums.
/// All of it runs in one REPEATABLE READ snapshot, so concurrent posts and
/// the sealer cannot skew the comparison.
pub async fn reconcile_incremental(
    pool: &PgPool,
    cfg: &ReconcileConfig,
    also_check: &[Uuid],
    balances: Balances<'_>,
) -> Result<ReconciliationReport> {
    let mut report = ReconciliationReport::default();
    let chunk = cfg.fold_chunk.max(1);
    let mut chunks = 0;
    loop {
        let mut db = snapshot(pool).await?;
        let after: i64 =
            sqlx::query_scalar("SELECT through_sealed_seq FROM reconcile_watermark FOR UPDATE")
                .fetch_one(&mut *db)
                .await?;
        let sealed_max: i64 = sqlx::query_scalar(
            "SELECT COALESCE(MAX(sealed_seq), $1) FROM transactions WHERE sealed_seq > $1",
        )
        .bind(after)
        .fetch_one(&mut *db)
        .await?;

        if sealed_max - after > chunk {
            if chunks >= cfg.max_fold_chunks {
                report.backlog_remaining = true;
                return Ok(report);
            }
            report.folded_unchecked += fold(&mut db, after, after + chunk).await?;
            db.commit().await?;
            chunks += 1;
            continue;
        }

        let rows = sqlx::query(concat!(
            "SELECT e.account_id,
                    SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                         ELSE -e.amount_minor END)::BIGINT AS tail
             FROM (",
            tail_ids!("$1"),
            ") t JOIN entries e ON e.transaction_id = t.id
             GROUP BY e.account_id"
        ))
        .bind(after)
        .fetch_all(&mut *db)
        .await?;
        let mut tail: HashMap<Uuid, i64> = HashMap::with_capacity(rows.len() + also_check.len());
        for row in rows {
            tail.insert(row.try_get("account_id")?, row.try_get("tail")?);
        }
        for id in also_check {
            tail.entry(*id).or_insert(0);
        }

        report.unbalanced_transactions = sqlx::query_scalar(concat!(
            "SELECT DISTINCT e.transaction_id
             FROM (",
            tail_ids!("$1"),
            ") t JOIN entries e ON e.transaction_id = t.id
             GROUP BY e.transaction_id, e.currency
             HAVING SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                         ELSE -e.amount_minor END) <> 0"
        ))
        .bind(after)
        .fetch_all(&mut *db)
        .await?;

        let (ids, tails): (Vec<Uuid>, Vec<i64>) = tail.into_iter().unzip();
        let rows = sqlx::query(
            "SELECT u.id, COALESCE(b.raw_minor, 0) AS stored,
                    COALESCE(r.sum_minor, 0) + u.tail AS derived
             FROM UNNEST($1::uuid[], $2::bigint[]) AS u(id, tail)
             LEFT JOIN balances b ON b.account_id = u.id
             LEFT JOIN reconciled_sums r ON r.account_id = u.id",
        )
        .bind(&ids)
        .bind(&tails)
        .fetch_all(&mut *db)
        .await?;
        report.accounts_checked += rows.len() as i64;
        let mut checked = Vec::with_capacity(rows.len());
        for row in rows {
            checked.push(BalanceMismatch {
                account_id: row.try_get("id")?,
                stored_minor: row.try_get("stored")?,
                derived_minor: row.try_get("derived")?,
            });
        }

        report.transactions_folded += fold(&mut db, after, sealed_max).await?;
        db.commit().await?;
        compare(pool, balances, checked, &mut report, Derive::Folded).await?;
        return Ok(report);
    }
}

/// How [`confirm`] derives an account's balance from the journal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Derive {
    /// `reconciled_sums` plus the tail: O(transactions since the last fold), what the
    /// incremental pass compares with. Trusts the folded sums, which the full pass audits.
    Folded,
    /// Every entry of the account: what the full pass compares with for accounts small
    /// enough to sum in one statement.
    Entries,
}

/// Keeps the accounts whose stored balance differs from the derived one. `checked` carries
/// `balances` as stored; under TigerBeetle the cluster's balance replaces it, and a
/// difference counts only once [`confirm`] proves it.
async fn compare(
    pool: &PgPool,
    balances: Balances<'_>,
    mut checked: Vec<BalanceMismatch>,
    report: &mut ReconciliationReport,
    derive: Derive,
) -> Result<()> {
    let Balances::TigerBeetle(ledger) = balances else {
        report.balance_mismatches.extend(
            checked
                .into_iter()
                .filter(|m| m.stored_minor != m.derived_minor),
        );
        return Ok(());
    };
    let ids: Vec<AccountId> = checked.iter().map(|m| AccountId(m.account_id)).collect();
    let tb = ledger.tb().raw_balances(&ids).await?;
    let mut suspects = Vec::new();
    for m in &mut checked {
        m.stored_minor = stored_minor(&tb, m.account_id)?;
        if m.stored_minor != m.derived_minor {
            suspects.push(m.account_id);
        }
    }
    if suspects.is_empty() {
        return Ok(());
    }
    let confirmed = confirm(pool, ledger, &suspects, derive).await?;
    report.balance_mismatches.extend(confirmed.mismatches);
    report.inconclusive.extend(confirmed.inconclusive);
    absorb(&mut report.recovered, &confirmed.recovered);
    Ok(())
}

fn stored_minor(tb: &HashMap<AccountId, i128>, id: Uuid) -> Result<i64> {
    let raw = tb.get(&AccountId(id)).copied().unwrap_or(0);
    i64::try_from(raw).map_err(|_| {
        WorkerError::DataIntegrity(format!("tigerbeetle balance of {id} is beyond BIGINT"))
    })
}

/// What [`confirm`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Confirmation {
    pub mismatches: Vec<BalanceMismatch>,
    pub inconclusive: Vec<Uuid>,
    pub recovered: RecoveryReport,
}

/// How far TigerBeetle's and Postgres's clocks may disagree: a transaction committed after a
/// recovery pass began is at most this much older on the Postgres clock than its
/// reservation is on the cluster's.
const CLOCK_SKEW: Duration = Duration::from_secs(60);
/// Reads of one re-check that may meet a post landing between them before it gives up.
const CONFIRM_ATTEMPTS: usize = 5;

/// The exact re-check of accounts whose TigerBeetle balance differed from the journal.
///
/// A post is applied after its Postgres COMMIT, so the cluster may lag the journal by the
/// transactions committed but not yet posted — and only by those. A recovery pass first
/// posts every committed attempt older than the recovery grace. Then the cluster balance is
/// read between two readings of the cluster's newest timestamp `n0` and `n1`, and the journal
/// in a snapshot taken after it, with each account's recent transactions (committed since the
/// pass began, less the grace and [`CLOCK_SKEW`]: no other one can still be unposted). Each of
/// those whose post is missing, or was applied after `n1`, was not in the read; one applied
/// at or before `n0` was. So `cluster = journal − those not in the read`, exactly — unless a
/// post landed between `n0` and `n1`, when the account is read again (at most
/// [`CONFIRM_ATTEMPTS`] times, then reported inconclusive).
pub async fn confirm(
    pool: &PgPool,
    ledger: &HybridLedger<AnyTb>,
    accounts: &[Uuid],
    derive: Derive,
) -> Result<Confirmation> {
    let began: f64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::FLOAT8")
        .fetch_one(pool)
        .await?;
    let mut out = Confirmation {
        recovered: ledger.recover().await?,
        ..Confirmation::default()
    };
    let since = began - (ledger.tb().config().recovery_grace + CLOCK_SKEW).as_secs_f64();
    let cluster = Uuid::from_u128(ledger_tigerbeetle::TbClient::cluster_id(
        ledger.tb().client(),
    ));
    let mut pending: Vec<Uuid> = accounts.to_vec();
    for _ in 0..CONFIRM_ATTEMPTS {
        if pending.is_empty() {
            break;
        }
        let ids: Vec<AccountId> = pending.iter().copied().map(AccountId).collect();
        let n0 = ledger.tb().newest_timestamp().await?;
        let tb = ledger.tb().raw_balances(&ids).await?;
        let n1 = ledger.tb().newest_timestamp().await?;

        let mut db = snapshot(pool).await?;
        let derived: HashMap<Uuid, i64> = match derive {
            Derive::Folded => {
                let through = watermark(&mut db).await?;
                sqlx::query_as(concat!(
                    "WITH tail AS (
                         SELECT e.account_id,
                                SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                                     ELSE -e.amount_minor END)::BIGINT AS tail
                         FROM (",
                    tail_ids!("$2"),
                    ") t JOIN entries e ON e.transaction_id = t.id
                         WHERE e.account_id = ANY($1)
                         GROUP BY e.account_id
                     )
                     SELECT u.id, COALESCE(r.sum_minor, 0) + COALESCE(tl.tail, 0)
                     FROM UNNEST($1::uuid[]) AS u(id)
                     LEFT JOIN reconciled_sums r ON r.account_id = u.id
                     LEFT JOIN tail tl ON tl.account_id = u.id"
                ))
                .bind(&pending)
                .bind(through)
                .fetch_all(&mut *db)
                .await?
            }
            Derive::Entries => {
                sqlx::query_as(
                    "SELECT e.account_id,
                            SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                                 ELSE -e.amount_minor END)::BIGINT
                     FROM entries e WHERE e.account_id = ANY($1)
                     GROUP BY e.account_id",
                )
                .bind(&pending)
                .fetch_all(&mut *db)
                .await?
            }
        }
        .into_iter()
        .collect();
        // Transactions from before the cut-over moved into the cluster as opening balances.
        let recent: Vec<(Uuid, Uuid, i64)> = sqlx::query_as(
            "SELECT e.transaction_id, e.account_id,
                    SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                         ELSE -e.amount_minor END)::BIGINT
             FROM entries e JOIN transactions t ON t.id = e.transaction_id
             WHERE e.account_id = ANY($1) AND e.created_at >= to_timestamp($2)
               AND t.seq > COALESCE((SELECT through_seq FROM tb_cutover
                                     WHERE cluster_id = $3), 0)
             GROUP BY e.transaction_id, e.account_id
             HAVING SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                         ELSE -e.amount_minor END) <> 0",
        )
        .bind(&pending)
        .bind(since)
        .bind(cluster)
        .fetch_all(&mut *db)
        .await?;
        db.commit().await?;

        let txns: Vec<Uuid> = recent
            .iter()
            .map(|(t, _, _)| *t)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let posted = ledger.tb().posted_at(&txns).await?;
        let mut unposted: HashMap<Uuid, i64> = HashMap::new();
        let mut ambiguous: HashSet<Uuid> = HashSet::new();
        for (txn, account, signed) in recent {
            match posted.get(&txn) {
                Some(&at) if at <= n0 => {}
                Some(&at) if at <= n1 => {
                    ambiguous.insert(account);
                }
                _ => *unposted.entry(account).or_default() += signed,
            }
        }
        let mut again = Vec::new();
        for id in pending {
            if ambiguous.contains(&id) {
                again.push(id);
                continue;
            }
            let stored_minor = stored_minor(&tb, id)?;
            let derived_minor = derived.get(&id).copied().unwrap_or(0);
            let in_flight = unposted.get(&id).copied().unwrap_or(0);
            if derived_minor.checked_sub(in_flight) != Some(stored_minor) {
                out.mismatches.push(BalanceMismatch {
                    account_id: id,
                    stored_minor,
                    derived_minor,
                });
            }
        }
        pending = again;
    }
    out.inconclusive = pending;
    Ok(out)
}

enum Unit {
    Accounts(Vec<Uuid>),
    Large(Uuid),
}

/// A large account's history, summed page by page across short statements.
/// Entries are immutable and `sealed_seq` is set once, so the sum of entries
/// sealed at or below `through` is the same whichever snapshot reads a page.
struct PagedAccount {
    id: Uuid,
    through: i64,
    sum: i64,
    cursor: (String, Uuid),
}

/// A full pass over every account, in bounded steps that each run one short
/// statement or snapshot, so it never approaches statement_timeout or pins an
/// old snapshot, and can be interleaved with other duties. Checks each balance
/// against its entries and each `reconciled_sums` row against the entries
/// sealed through the watermark, plus global per-currency conservation.
#[derive(Default)]
pub struct FullReconciliation {
    after: Uuid,
    queue: VecDeque<Unit>,
    paging: Option<PagedAccount>,
    started: bool,
    done: bool,
    report: ReconciliationReport,
}

const PLAN_ACCOUNTS: i64 = 1_000;

impl FullReconciliation {
    pub fn new() -> Self {
        Self::default()
    }

    /// A pass over only the accounts with an id above `after` (ids are
    /// UUIDv7, so: the accounts opened since). Conservation is still global.
    pub fn starting_after(after: Uuid) -> Self {
        Self {
            after,
            ..Self::default()
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    pub fn report(&self) -> &ReconciliationReport {
        &self.report
    }

    pub fn into_report(self) -> ReconciliationReport {
        self.report
    }

    /// Runs one bounded unit of work. Returns true once the pass is complete.
    pub async fn step(
        &mut self,
        pool: &PgPool,
        cfg: &ReconcileConfig,
        balances: Balances<'_>,
    ) -> Result<bool> {
        if self.done {
            return Ok(true);
        }
        if !self.started {
            self.started = true;
            // TigerBeetle conserves per ledger by construction (every transfer debits and
            // credits one ledger), so there what must sum to zero is the journal: the
            // reconciled sums plus the tail, one statement, one snapshot.
            let sql = match balances {
                Balances::Postgres => {
                    "SELECT a.currency, SUM(b.raw_minor)::BIGINT AS total
                     FROM balances b JOIN accounts a ON a.id = b.account_id
                     GROUP BY a.currency
                     HAVING SUM(b.raw_minor) <> 0"
                }
                Balances::TigerBeetle(_) => concat!(
                    "WITH w AS (SELECT through_sealed_seq AS s FROM reconcile_watermark),
                     j AS (
                         SELECT account_id, sum_minor AS raw FROM reconciled_sums
                         UNION ALL
                         SELECT e.account_id, CASE e.direction WHEN 'credit' THEN e.amount_minor
                                                               ELSE -e.amount_minor END
                         FROM (",
                    tail_ids!("(SELECT s FROM w)"),
                    ") t JOIN entries e ON e.transaction_id = t.id
                     )
                     SELECT a.currency, SUM(j.raw)::BIGINT AS total
                     FROM j JOIN accounts a ON a.id = j.account_id
                     GROUP BY a.currency
                     HAVING SUM(j.raw) <> 0"
                ),
            };
            let rows = sqlx::query(sql).fetch_all(pool).await?;
            for row in rows {
                self.report
                    .currency_imbalances
                    .push((row.try_get("currency")?, row.try_get("total")?));
            }
            return Ok(false);
        }
        if let Some(paged) = self.paging.as_mut() {
            if page_account(pool, paged, cfg.full_chunk_entries).await? {
                let paged = self.paging.take().expect("checked above");
                finish_account(pool, &paged, &mut self.report, balances).await?;
            }
            return Ok(false);
        }
        match self.queue.pop_front() {
            Some(Unit::Accounts(ids)) => {
                check_accounts(pool, &ids, &mut self.report, balances).await?
            }
            Some(Unit::Large(id)) => {
                let through = watermark(&mut *pool.acquire().await?).await?;
                self.paging = Some(PagedAccount {
                    id,
                    through,
                    sum: 0,
                    cursor: ("infinity".to_string(), Uuid::from_u128(u128::MAX)),
                });
            }
            None => {
                if !self.plan(pool, cfg, balances).await? {
                    self.done = true;
                }
            }
        }
        Ok(self.done)
    }

    async fn plan(
        &mut self,
        pool: &PgPool,
        cfg: &ReconcileConfig,
        balances: Balances<'_>,
    ) -> Result<bool> {
        let budget = cfg.full_chunk_entries.max(1);
        // Without balance rows (TigerBeetle) the entries themselves are counted, along the
        // statement index and no further than the budget.
        let sql = match balances {
            Balances::Postgres => {
                "SELECT a.id, COALESCE(b.version, 0) AS version
                 FROM accounts a LEFT JOIN balances b ON b.account_id = a.id
                 WHERE a.id > $1 ORDER BY a.id LIMIT $2"
            }
            Balances::TigerBeetle(_) => {
                "SELECT a.id, (SELECT count(*) FROM (SELECT 1 FROM entries e
                                                    WHERE e.account_id = a.id LIMIT $3) x)
                                  AS version
                 FROM accounts a WHERE a.id > $1 ORDER BY a.id LIMIT $2"
            }
        };
        let mut query = sqlx::query(sql).bind(self.after).bind(PLAN_ACCOUNTS);
        if let Balances::TigerBeetle(_) = balances {
            query = query.bind(budget + 1);
        }
        let rows = query.fetch_all(pool).await?;
        let mut group = Vec::new();
        let mut cost = 0i64;
        for row in rows {
            let id: Uuid = row.try_get("id")?;
            // `version` counts the posts that touched the account, a close
            // proxy for its entry count (and only ever a performance hint).
            let version: i64 = row.try_get::<i64, _>("version")?.max(1);
            self.after = id;
            if version > budget {
                self.queue.push_back(Unit::Large(id));
                continue;
            }
            if cost + version > budget && !group.is_empty() {
                self.queue
                    .push_back(Unit::Accounts(std::mem::take(&mut group)));
                cost = 0;
            }
            group.push(id);
            cost += version;
        }
        if !group.is_empty() {
            self.queue.push_back(Unit::Accounts(group));
        }
        Ok(!self.queue.is_empty())
    }
}

async fn check_accounts(
    pool: &PgPool,
    ids: &[Uuid],
    report: &mut ReconciliationReport,
    balances: Balances<'_>,
) -> Result<()> {
    let mut db = snapshot(pool).await?;
    let through = watermark(&mut db).await?;
    // Entries sealed through the watermark = all entries - the tail's, which
    // avoids looking up every entry's transaction.
    let rows = sqlx::query(concat!(
        "WITH tail AS (
             SELECT e.account_id,
                    SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                         ELSE -e.amount_minor END)::BIGINT AS tail
             FROM (",
        tail_ids!("$2"),
        ") t JOIN entries e ON e.transaction_id = t.id
             WHERE e.account_id = ANY($1)
             GROUP BY e.account_id
         )
         SELECT u.id, COALESCE(b.raw_minor, 0) AS stored, COALESCE(r.sum_minor, 0) AS base,
                COALESCE(d.total, 0) AS derived, COALESCE(tl.tail, 0) AS tail
         FROM UNNEST($1::uuid[]) AS u(id)
         LEFT JOIN balances b ON b.account_id = u.id
         LEFT JOIN reconciled_sums r ON r.account_id = u.id
         LEFT JOIN tail tl ON tl.account_id = u.id
         LEFT JOIN LATERAL (
             SELECT SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                         ELSE -e.amount_minor END)::BIGINT AS total
             FROM entries e WHERE e.account_id = u.id
         ) d ON true"
    ))
    .bind(ids)
    .bind(through)
    .fetch_all(&mut *db)
    .await?;
    db.commit().await?;
    report.accounts_checked += rows.len() as i64;
    let mut checked = Vec::with_capacity(rows.len());
    for row in rows {
        let account_id: Uuid = row.try_get("id")?;
        let derived_minor: i64 = row.try_get("derived")?;
        checked.push(BalanceMismatch {
            account_id,
            stored_minor: row.try_get("stored")?,
            derived_minor,
        });
        let base: i64 = row.try_get("base")?;
        let through_w = derived_minor
            .checked_sub(row.try_get("tail")?)
            .ok_or_else(|| WorkerError::DataIntegrity(format!("sum overflow on {account_id}")))?;
        if base != through_w {
            report.sum_mismatches.push(BalanceMismatch {
                account_id,
                stored_minor: base,
                derived_minor: through_w,
            });
        }
    }
    compare(pool, balances, checked, report, Derive::Entries).await
}

/// Sums the next page of a large account's entries (newest first, along the
/// statement index). Returns true once its history is exhausted.
async fn page_account(pool: &PgPool, paged: &mut PagedAccount, limit: i64) -> Result<bool> {
    let row = sqlx::query(
        "WITH page AS (
             SELECT e.created_at, e.id, e.transaction_id,
                    CASE e.direction WHEN 'credit' THEN e.amount_minor
                                     ELSE -e.amount_minor END AS signed
             FROM entries e
             WHERE e.account_id = $1 AND (e.created_at, e.id) < ($2::timestamptz, $3::uuid)
             ORDER BY e.created_at DESC, e.id DESC
             LIMIT $4
         ), last AS (
             SELECT created_at, id FROM page ORDER BY created_at, id LIMIT 1
         )
         SELECT (SELECT COUNT(*) FROM page)::BIGINT AS n,
                (SELECT COALESCE(SUM(p.signed), 0) FROM page p
                 JOIN transactions t ON t.id = p.transaction_id
                 WHERE t.sealed_seq <= $5)::BIGINT AS through_w,
                (SELECT created_at::text FROM last) AS last_at,
                (SELECT id FROM last) AS last_id",
    )
    .bind(paged.id)
    .bind(&paged.cursor.0)
    .bind(paged.cursor.1)
    .bind(limit.max(1))
    .bind(paged.through)
    .fetch_one(pool)
    .await?;
    let n: i64 = row.try_get("n")?;
    let through_w: i64 = row.try_get("through_w")?;
    paged.sum = paged
        .sum
        .checked_add(through_w)
        .ok_or_else(|| WorkerError::DataIntegrity(format!("sum overflow on {}", paged.id)))?;
    if let (Some(at), Some(id)) = (
        row.try_get::<Option<String>, _>("last_at")?,
        row.try_get::<Option<Uuid>, _>("last_id")?,
    ) {
        paged.cursor = (at, id);
    }
    Ok(n < limit.max(1))
}

async fn finish_account(
    pool: &PgPool,
    paged: &PagedAccount,
    report: &mut ReconciliationReport,
    balances: Balances<'_>,
) -> Result<()> {
    let mut db = snapshot(pool).await?;
    let row = sqlx::query(concat!(
        "SELECT w.through_sealed_seq AS through,
                COALESCE((SELECT raw_minor FROM balances WHERE account_id = $1), 0) AS stored,
                COALESCE((SELECT sum_minor FROM reconciled_sums WHERE account_id = $1), 0) AS base,
                (SELECT COALESCE(SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                                      ELSE -e.amount_minor END), 0)
                 FROM transactions t JOIN entries e ON e.transaction_id = t.id
                 WHERE t.sealed_seq > $2 AND t.sealed_seq <= w.through_sealed_seq
                   AND e.account_id = $1)::BIGINT AS since_paging,
                (SELECT COALESCE(SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                                      ELSE -e.amount_minor END), 0)
                 FROM (",
        tail_ids!("w.through_sealed_seq"),
        ") t JOIN entries e ON e.transaction_id = t.id
                 WHERE e.account_id = $1)::BIGINT AS tail
         FROM reconcile_watermark w"
    ))
    .bind(paged.id)
    .bind(paged.through)
    .fetch_one(&mut *db)
    .await?;
    let through: i64 = row.try_get("through")?;
    if through < paged.through {
        return Err(WorkerError::DataIntegrity(
            "reconcile watermark moved backwards".into(),
        ));
    }
    let stored_minor: i64 = row.try_get("stored")?;
    let base: i64 = row.try_get("base")?;
    let overflow = || WorkerError::DataIntegrity(format!("sum overflow on {}", paged.id));
    let expected_base = paged
        .sum
        .checked_add(row.try_get("since_paging")?)
        .ok_or_else(overflow)?;
    let derived_minor = expected_base
        .checked_add(row.try_get("tail")?)
        .ok_or_else(overflow)?;
    db.commit().await?;
    report.accounts_checked += 1;
    if base != expected_base {
        report.sum_mismatches.push(BalanceMismatch {
            account_id: paged.id,
            stored_minor: base,
            derived_minor: expected_base,
        });
    }
    let checked = BalanceMismatch {
        account_id: paged.id,
        stored_minor,
        derived_minor,
    };
    compare(pool, balances, vec![checked], report, Derive::Folded).await
}

pub async fn reconcile_full(
    pool: &PgPool,
    cfg: &ReconcileConfig,
    balances: Balances<'_>,
) -> Result<ReconciliationReport> {
    let mut full = FullReconciliation::new();
    while !full.step(pool, cfg, balances).await? {}
    Ok(full.into_report())
}

/// Brings the reconciled sums up to date, then checks every account.
pub async fn reconcile(pool: &PgPool, balances: Balances<'_>) -> Result<ReconciliationReport> {
    let cfg = ReconcileConfig::default();
    let mut report = ReconciliationReport::default();
    loop {
        let pass = reconcile_incremental(pool, &cfg, &[], balances).await?;
        let more = pass.backlog_remaining;
        report.merge(pass);
        if !more {
            break;
        }
    }
    report.merge(reconcile_full(pool, &cfg, balances).await?);
    Ok(report)
}

pub async fn unsealed_lag(pool: &PgPool) -> Result<(i64, f64)> {
    let row = sqlx::query(
        "SELECT COUNT(*)::BIGINT AS unsealed,
                COALESCE(EXTRACT(EPOCH FROM (now() - MIN(created_at))), 0)::FLOAT8 AS oldest_age
         FROM transactions WHERE sealed_seq IS NULL",
    )
    .fetch_one(pool)
    .await?;
    Ok((row.try_get("unsealed")?, row.try_get("oldest_age")?))
}
