//! The guarded posting protocol: TigerBeetle holds balances, Postgres decides.
//!
//! 1. **Reserve.** The transaction's legs go to TigerBeetle as one linked chain of pending
//!    transfers with fresh ids. Balance limits are checked here, atomically: a wallet can never
//!    be overdrawn, whatever Postgres does next.
//! 2. **Commit.** One Postgres transaction claims the transaction id, records the intent
//!    (`tb_intents`: this attempt commits), runs the guards (AML under the user-row lock,
//!    single-use transitions, audit rows) and writes the journal mirror (entries, outbox,
//!    idempotency record). Its COMMIT is the commit point of the whole protocol.
//! 3. **Post.** The reservations are posted as one linked chain whose ids derive from the
//!    transaction id (its commit record), so at most one attempt ever posts. When step 2 fails,
//!    the reservations are voided instead.
//!
//! A crash between steps leaves a reservation behind; recovery settles it with Postgres as the
//! source of truth. It inserts a `void` tombstone for the attempt (`ON CONFLICT DO NOTHING`):
//! if the request's step 2 still runs, that insert waits on the intent's key, so exactly one of
//! the two wins. A `commit` row means post, anything else means void. Pending timeouts are only
//! the backstop for a recovery worker that is down.

use std::future::Future;
use std::time::Instant;

use ledger::{Account as LedgerAccount, LedgerError, Transaction};
use sqlx::{Connection, PgConnection, PgPool};
use storage::{HookError, IdempotencyRecord, PostOptions, PostgresLedger, StorageError};
use uuid::Uuid;

use crate::client::{transfer_flags as tf, TbClient, Transfer, TransferResult, AMOUNT_MAX};
use crate::error::{first_failure, Result, TbError};
use crate::ids::{self, leg};
use crate::plan::Leg;
use crate::tb::{link, low64, TbLedger};

/// Points where a test can kill the protocol (`testkit`): the process "dies" there, leaving
/// TigerBeetle and Postgres exactly as a crash would.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Step {
    /// The reservation is applied; Postgres has not been touched.
    Reserved,
    /// Guards and mirror are written, COMMIT not sent (Postgres rolls back).
    Mirrored,
    /// COMMIT was lost: not applied, and the caller saw an error.
    CommitLost,
    /// COMMIT was applied, but the caller saw an error.
    CommitUnacked,
    /// Committed; the post was not sent.
    Committed,
    /// Recovery wrote (or found) the outcome but did not act on it.
    Tombstoned,
}

/// Asked at every step; `true` kills the protocol there. Async, so a test can also run
/// recovery at that exact point.
pub(crate) trait Probe: Send + Sync {
    fn at(&self, step: Step) -> impl Future<Output = bool> + Send;
}

pub(crate) struct NoFaults;

impl Probe for NoFaults {
    async fn at(&self, _: Step) -> bool {
        false
    }
}

pub(crate) async fn crash<P: Probe>(probe: &P, step: Step) -> Result<()> {
    match probe.at(step).await {
        true => Err(TbError::Unavailable(format!("injected crash at {step:?}"))),
        false => Ok(()),
    }
}

/// One try at posting a transaction: its reservations are `base + leg`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Attempt {
    pub base: u128,
    pub transaction: u128,
    pub legs: Vec<Leg>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Resolved {
    Posted,
    /// The reservation had expired; the committed movement was posted as plain transfers.
    Forced,
    Voided,
}

impl Attempt {
    pub(crate) fn reserve(&self, timeout: u32) -> Vec<Transfer> {
        link(
            self.legs
                .iter()
                .enumerate()
                .map(|(i, l)| Transfer {
                    id: leg(self.base, i),
                    debit_account_id: l.debit,
                    credit_account_id: l.credit,
                    amount: l.amount,
                    ledger: l.ledger,
                    code: ids::TRANSFER_LEG,
                    user_data_128: self.transaction,
                    user_data_64: self.legs.len() as u64,
                    user_data_32: ids::TAG_RESERVE,
                    timeout,
                    flags: tf::PENDING,
                    ..Transfer::default()
                })
                .collect(),
        )
    }

    pub(crate) fn post(&self) -> Vec<Transfer> {
        let base = ids::post_base(self.transaction);
        link(
            (0..self.legs.len())
                .map(|i| Transfer {
                    id: leg(base, i),
                    pending_id: leg(self.base, i),
                    amount: AMOUNT_MAX,
                    user_data_32: ids::TAG_POST,
                    flags: tf::POST_PENDING_TRANSFER,
                    ..Transfer::default()
                })
                .collect(),
        )
    }

    /// Not linked: each void only releases a hold, and one leg may already have expired.
    pub(crate) fn void(&self) -> Vec<Transfer> {
        let base = ids::void_base(self.base);
        (0..self.legs.len())
            .map(|i| Transfer {
                id: leg(base, i),
                pending_id: leg(self.base, i),
                user_data_32: ids::TAG_VOID,
                flags: tf::VOID_PENDING_TRANSFER,
                ..Transfer::default()
            })
            .collect()
    }

    /// The committed movement as plain transfers under the same commit-record ids.
    pub(crate) fn forced(&self) -> Vec<Transfer> {
        let base = ids::post_base(self.transaction);
        link(
            self.legs
                .iter()
                .enumerate()
                .map(|(i, l)| Transfer {
                    id: leg(base, i),
                    debit_account_id: l.debit,
                    credit_account_id: l.credit,
                    amount: l.amount,
                    ledger: l.ledger,
                    code: ids::TRANSFER_LEG,
                    user_data_128: self.transaction,
                    user_data_64: low64(self.base),
                    user_data_32: ids::TAG_FORCED,
                    ..Transfer::default()
                })
                .collect(),
        )
    }

    /// Rebuilds an attempt from its reservations (all legs, any order).
    pub(crate) fn from_reservations(mut legs: Vec<Transfer>) -> Result<Self> {
        legs.sort_by_key(|t| t.id);
        let first = legs.first().ok_or(TbError::Protocol("no legs".into()))?;
        let base = first.id & !ids::LEG_MASK;
        let whole = legs.len() as u64 == first.user_data_64
            && legs.iter().enumerate().all(|(i, t)| {
                t.id == leg(base, i)
                    && t.user_data_128 == first.user_data_128
                    && t.has(tf::PENDING)
                    && t.user_data_32 == ids::TAG_RESERVE
            });
        if !whole {
            return Err(TbError::Protocol(format!(
                "reservation {base:032x} is not a whole chain"
            )));
        }
        Ok(Self {
            base,
            transaction: first.user_data_128,
            legs: legs
                .iter()
                .map(|t| Leg {
                    debit: t.debit_account_id,
                    credit: t.credit_account_id,
                    amount: t.amount,
                    ledger: t.ledger,
                })
                .collect(),
        })
    }
}

enum Commit {
    /// Postgres rolled back: nothing of this attempt is in the books.
    RolledBack(TbError),
    /// COMMIT failed in a way that leaves its outcome unknown.
    Unknown(TbError),
    /// Injected crash: the caller must not clean up.
    Crashed(TbError),
}

/// TigerBeetle for balances, Postgres for everything a posting must decide or record.
pub struct HybridLedger<C> {
    tb: TbLedger<C>,
    pg: PostgresLedger,
}

impl<C> Clone for HybridLedger<C> {
    fn clone(&self) -> Self {
        Self {
            tb: self.tb.clone(),
            pg: self.pg.clone(),
        }
    }
}

impl<C: TbClient> HybridLedger<C> {
    pub fn new(tb: TbLedger<C>, pg: PostgresLedger) -> Self {
        Self { tb, pg }
    }

    pub fn tb(&self) -> &TbLedger<C> {
        &self.tb
    }

    pub fn pool(&self) -> &PgPool {
        self.pg.pool()
    }

    /// For everything but posting and balances (accounts, currencies, owners).
    pub fn postgres(&self) -> &PostgresLedger {
        &self.pg
    }

    pub async fn open_account(&self, account: &LedgerAccount) -> Result<()> {
        self.tb.open_account(account).await?;
        Ok(self.pg.open_account_owned(account, None).await?)
    }

    /// TigerBeetle first: if the caller's transaction then rolls back, an empty account is
    /// left in the cluster, which is harmless and reused when the same id is opened again.
    pub async fn open_account_owned_on(
        &self,
        conn: &mut PgConnection,
        account: &LedgerAccount,
        owner: Option<Uuid>,
    ) -> Result<()> {
        self.tb.open_account(account).await?;
        Ok(self.pg.open_account_owned_on(conn, account, owner).await?)
    }

    pub async fn post(&self, txn: &Transaction) -> Result<()> {
        let mut conn = self.pool().acquire().await?;
        self.post_on(&mut conn, txn, PostOptions::default()).await
    }

    /// The drop-in for `PostgresLedger::post_on`: same guards, same idempotency record, same
    /// outbox event, same errors for the same rules.
    pub async fn post_on(
        &self,
        conn: &mut PgConnection,
        txn: &Transaction,
        opts: PostOptions,
    ) -> Result<()> {
        self.post_probed(conn, txn, opts, &NoFaults).await
    }

    pub(crate) async fn post_probed<P: Probe>(
        &self,
        conn: &mut PgConnection,
        txn: &Transaction,
        opts: PostOptions,
        probe: &P,
    ) -> Result<()> {
        txn.validate()?;
        let plan = self.tb.plan(txn).await?;
        if plan.legs.is_empty() {
            // Nothing moves (every account nets to zero): Postgres alone records it.
            return match self.commit(conn, txn, opts, None, probe).await {
                Ok(()) => Ok(()),
                Err(Commit::RolledBack(e) | Commit::Unknown(e) | Commit::Crashed(e)) => Err(e),
            };
        }
        let attempt = Attempt {
            base: ids::new_attempt(),
            transaction: txn.id.as_uuid().as_u128(),
            legs: plan.legs.clone(),
        };
        let started = Instant::now();
        let reserve = attempt.reserve(self.tb.config().pending_timeout_secs);
        if let Some((i, r)) = first_failure(&self.tb.create(reserve).await?) {
            return Err(self.reserve_failed(txn, &plan, i, r).await);
        }
        crash(probe, Step::Reserved).await?;

        match self
            .commit(conn, txn, opts, Some((&attempt, started)), probe)
            .await
        {
            Ok(()) => {}
            Err(Commit::RolledBack(e)) => {
                // Our transaction rolled back, so no `commit` intent for this attempt can
                // exist: voiding needs no tombstone.
                if let Err(v) = self.settle_void(&attempt).await {
                    tracing::warn!(attempt = %hex(attempt.base), error = %v, "void left to recovery");
                }
                return Err(e);
            }
            Err(Commit::Crashed(e)) => return Err(e),
            Err(Commit::Unknown(e)) => {
                // Settle it the way recovery would; the tombstone decides what COMMIT did.
                return match self.resolve(&attempt, probe).await {
                    Ok(Resolved::Posted | Resolved::Forced) => Ok(()),
                    Ok(Resolved::Voided) => Err(e),
                    Err(r) => {
                        tracing::warn!(attempt = %hex(attempt.base), error = %r, "unknown commit left to recovery");
                        Err(e)
                    }
                };
            }
        }
        crash(probe, Step::Committed).await?;

        // Committed: whatever happens to the post, the transaction stands and recovery
        // finishes the job, so the caller gets success either way.
        if let Err(e) = self.settle_commit(&attempt).await {
            tracing::error!(attempt = %hex(attempt.base), transaction = %txn.id, error = %e, "post left to recovery");
        }
        Ok(())
    }

    async fn reserve_failed(
        &self,
        txn: &Transaction,
        plan: &crate::plan::Plan,
        i: usize,
        r: TransferResult,
    ) -> TbError {
        let err = self.tb.leg_error(plan, i, r).await;
        // `InMemoryLedger` reports a posted transaction as a duplicate before anything else;
        // here the claimed `transactions` row is that record.
        if err.as_ledger().is_some() {
            match self.is_claimed(txn).await {
                Ok(true) => return LedgerError::DuplicateTransaction(txn.id).into(),
                Ok(false) => {}
                Err(e) => return e,
            }
        }
        err
    }

    async fn is_claimed(&self, txn: &Transaction) -> Result<bool> {
        Ok(
            sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM transactions WHERE id = $1)")
                .bind(txn.id.as_uuid())
                .fetch_one(self.pool())
                .await?,
        )
    }

    async fn commit<P: Probe>(
        &self,
        conn: &mut PgConnection,
        txn: &Transaction,
        opts: PostOptions,
        attempt: Option<(&Attempt, Instant)>,
        probe: &P,
    ) -> core::result::Result<(), Commit> {
        let mut db = conn
            .begin()
            .await
            .map_err(|e| Commit::RolledBack(e.into()))?;
        // Roll back explicitly: until it happens the claim and intent keys stay locked, and
        // recovery's tombstone would wait on them.
        let body = async {
            self.journal(&mut db, txn, opts, attempt.map(|(a, _)| a))
                .await?;
            if probe.at(Step::Mirrored).await {
                return Err(None);
            }
            if let (Some((_, started)), Some(budget)) = (attempt, self.tb.config().commit_budget())
            {
                if started.elapsed() >= budget {
                    return Err(Some(TbError::Retry(
                        "the reservation would expire before the commit",
                    )));
                }
            }
            Ok(())
        };
        match body.await {
            Ok(()) => {}
            Err(e) => {
                let _ = db.rollback().await;
                return Err(match e {
                    Some(e) => Commit::RolledBack(e),
                    None => Commit::Crashed(TbError::Unavailable(format!(
                        "injected crash at {:?}",
                        Step::Mirrored
                    ))),
                });
            }
        }
        if probe.at(Step::CommitLost).await {
            let _ = db.rollback().await;
            return Err(Commit::Unknown(TbError::Unavailable(
                "injected: COMMIT lost".into(),
            )));
        }
        db.commit().await.map_err(|e| Commit::Unknown(e.into()))?;
        if probe.at(Step::CommitUnacked).await {
            return Err(Commit::Unknown(TbError::Unavailable(
                "injected: COMMIT unacknowledged".into(),
            )));
        }
        Ok(())
    }

    /// Claim, intent, guards and mirror, in that order: the claim first so a duplicate never
    /// runs the guards (nor queues on their locks while holding the claim).
    async fn journal(
        &self,
        db: &mut PgConnection,
        txn: &Transaction,
        opts: PostOptions,
        attempt: Option<&Attempt>,
    ) -> core::result::Result<(), Option<TbError>> {
        let db_err = |e: sqlx::Error| Some(TbError::from(e));
        let (claimed, intended): (bool, bool) = sqlx::query_as(
            "WITH t AS (
                 INSERT INTO transactions (id) VALUES ($1) ON CONFLICT (id) DO NOTHING RETURNING id
             ), i AS (
                 INSERT INTO tb_intents (attempt_id, transaction_id, outcome)
                 SELECT $2::uuid, t.id, 'commit' FROM t WHERE $2::uuid IS NOT NULL
                 ON CONFLICT (attempt_id) DO NOTHING
                 RETURNING attempt_id
             )
             SELECT EXISTS (SELECT 1 FROM t), EXISTS (SELECT 1 FROM i)",
        )
        .bind(txn.id.as_uuid())
        .bind(attempt.map(|a| Uuid::from_u128(a.base)))
        .fetch_one(&mut *db)
        .await
        .map_err(db_err)?;
        if !claimed {
            return Err(Some(LedgerError::DuplicateTransaction(txn.id).into()));
        }
        if attempt.is_some() && !intended {
            return Err(Some(TbError::Retry(
                "recovery settled the reservation before it committed",
            )));
        }
        if let Some(guard) = opts.guard {
            guard(&mut *db).await.map_err(|e| {
                Some(match e {
                    HookError::Rejected { rule, message } => {
                        StorageError::Rejected { rule, message }.into()
                    }
                    HookError::Database(e) => e.into(),
                })
            })?;
        }
        mirror(db, txn, opts.idempotency.as_ref())
            .await
            .map_err(Some)
    }

    /// Records the outcome Postgres decided for an attempt, or a `void` tombstone if it has
    /// none yet, and acts on it. Waits for an in-flight step 2 of the same attempt.
    pub(crate) async fn resolve<P: Probe>(&self, attempt: &Attempt, probe: &P) -> Result<Resolved> {
        let id = Uuid::from_u128(attempt.base);
        let inserted: Option<String> = sqlx::query_scalar(
            "INSERT INTO tb_intents (attempt_id, transaction_id, outcome) VALUES ($1, $2, 'void')
             ON CONFLICT (attempt_id) DO NOTHING RETURNING outcome",
        )
        .bind(id)
        .bind(Uuid::from_u128(attempt.transaction))
        .fetch_optional(self.pool())
        .await?;
        // A conflict is only visible to a new statement (a new READ COMMITTED snapshot).
        let outcome = match inserted {
            Some(o) => o,
            None => {
                sqlx::query_scalar("SELECT outcome FROM tb_intents WHERE attempt_id = $1")
                    .bind(id)
                    .fetch_one(self.pool())
                    .await?
            }
        };
        crash(probe, Step::Tombstoned).await?;
        match outcome.as_str() {
            "commit" => self.settle_commit(attempt).await,
            "void" => self.settle_void(attempt).await,
            other => Err(StorageError::DataIntegrity(format!("tb_intents.outcome={other}")).into()),
        }
    }

    pub(crate) async fn settle_commit(&self, attempt: &Attempt) -> Result<Resolved> {
        type R = TransferResult;
        match first_failure(&self.tb.create(attempt.post()).await?) {
            None | Some((0, R::EXISTS)) => Ok(Resolved::Posted),
            Some((0, R::EXISTS_WITH_DIFFERENT_FLAGS)) if self.forced_by(attempt).await? => {
                Ok(Resolved::Forced)
            }
            Some((_, R::PENDING_TRANSFER_EXPIRED)) => self.force(attempt).await,
            Some((i, r)) => Err(TbError::Protocol(format!(
                "post of committed reservation {} leg {i}: {r}",
                hex(attempt.base)
            ))),
        }
    }

    pub(crate) async fn settle_void(&self, attempt: &Attempt) -> Result<Resolved> {
        type R = TransferResult;
        for (i, r) in self
            .tb
            .create(attempt.void())
            .await?
            .into_iter()
            .enumerate()
        {
            if !matches!(
                r,
                R::OK
                    | R::EXISTS
                    | R::PENDING_TRANSFER_ALREADY_VOIDED
                    | R::PENDING_TRANSFER_EXPIRED
            ) {
                return Err(TbError::Protocol(format!(
                    "void of reservation {} leg {i}: {r}",
                    hex(attempt.base)
                )));
            }
        }
        Ok(Resolved::Voided)
    }

    /// The commit outlived its reservation (only possible with recovery down for longer than
    /// the timeout): release what is still held and post the movement as plain transfers.
    /// Funds may have been spent meanwhile; then this fails loudly and needs a human.
    async fn force(&self, attempt: &Attempt) -> Result<Resolved> {
        self.settle_void(attempt).await?;
        match first_failure(&self.tb.create(attempt.forced()).await?) {
            None | Some((0, TransferResult::EXISTS)) => {
                tracing::warn!(transaction = %hex(attempt.transaction), "expired reservation re-posted");
                Ok(Resolved::Forced)
            }
            Some((i, r)) => Err(TbError::Protocol(format!(
                "transaction {} committed but its reservation expired, and re-posting leg {i} \
                 failed: {r}; repair by hand",
                hex(attempt.transaction)
            ))),
        }
    }

    async fn forced_by(&self, attempt: &Attempt) -> Result<bool> {
        let id = leg(ids::post_base(attempt.transaction), 0);
        let found = self
            .tb
            .call(self.tb.client().lookup_transfers(vec![id]))
            .await?;
        Ok(found.first().is_some_and(|t| {
            t.user_data_32 == ids::TAG_FORCED && t.user_data_64 == low64(attempt.base)
        }))
    }
}

pub(crate) fn hex(x: u128) -> String {
    format!("{x:032x}")
}

fn posted_event_payload(txn: &Transaction) -> serde_json::Value {
    let entries: Vec<serde_json::Value> = txn
        .entries
        .iter()
        .map(|e| {
            serde_json::json!({
                "account_id": e.account_id.as_uuid(),
                "direction": e.direction.as_db_str(),
                "amount_minor": e.amount.minor_units() as i64,
                "currency": e.amount.currency().code(),
            })
        })
        .collect();
    serde_json::json!({ "transaction_id": txn.id.as_uuid(), "entries": entries })
}

/// The journal `PostgresLedger` writes, minus the balance rows TigerBeetle now owns: the
/// entries (statements, sealing, reconciliation), the outbox event and the idempotency record.
async fn mirror(
    db: &mut PgConnection,
    txn: &Transaction,
    idempotency: Option<&IdempotencyRecord>,
) -> Result<()> {
    let n = txn.entries.len();
    let (mut ids, mut accounts, mut directions, mut amounts, mut currencies) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    for e in &txn.entries {
        ids.push(e.id.as_uuid());
        accounts.push(e.account_id.as_uuid());
        directions.push(e.direction.as_db_str());
        // plan() refused anything beyond BIGINT.
        amounts.push(e.amount.minor_units() as i64);
        currencies.push(e.amount.currency().code().to_string());
    }
    sqlx::query(
        "WITH e AS (
             INSERT INTO entries
               (id, transaction_id, account_id, direction, amount_minor, currency)
             SELECT u.id, $1, u.account_id, u.direction, u.amount_minor, u.currency
             FROM UNNEST($2::uuid[], $3::uuid[], $4::text[], $5::bigint[], $6::text[])
                  AS u(id, account_id, direction, amount_minor, currency)
         ), o AS (
             INSERT INTO outbox (id, aggregate_id, event_type, payload)
             VALUES ($7::uuid, $1, 'transaction.posted', $8::jsonb)
         )
         INSERT INTO idempotency_keys (key, fingerprint, response_status, response_body)
         SELECT $9::uuid, $10::text, $11::int, $12::jsonb
         WHERE $9::uuid IS NOT NULL
         ON CONFLICT (key) DO NOTHING",
    )
    .bind(txn.id.as_uuid())
    .bind(&ids)
    .bind(&accounts)
    .bind(&directions)
    .bind(&amounts)
    .bind(&currencies)
    .bind(Uuid::now_v7())
    .bind(posted_event_payload(txn))
    .bind(idempotency.map(|r| r.key))
    .bind(idempotency.map(|r| r.fingerprint.as_str()))
    .bind(idempotency.map(|r| r.response_status))
    .bind(idempotency.map(|r| &r.response_body))
    .execute(&mut *db)
    .await?;
    Ok(())
}
