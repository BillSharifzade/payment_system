//! The ledger backend (`LEDGER_BACKEND`): where balances live and who enforces no-overdraft.
//! Everything else stays in Postgres either way (the TigerBeetle hybrid writes the same
//! journal, outbox and idempotency records, and runs the same guards under the same row
//! locks), so only posting, account opening and balance reads dispatch here. Under
//! TigerBeetle the `balances` table is no longer written: nothing may read it but this module.
//!
//! Under direct posting (`TIGERBEETLE_POSTING=direct`) the cluster also decides transfers and
//! conversions by itself, AML limits included (`post_keyed`), and Postgres journals them
//! afterwards: a handler that reads the journal calls `settle_journal` first, so a client
//! always reads its own writes.

use ledger::{Account, AccountId, AccountType, LedgerError, NormalSide, Transaction};
use ledger_tigerbeetle::{
    AnyTb, Balance, HybridLedger, KeyOwner, Keyed, Posting, Screening, TbConfig, TbError,
};
use money::{Currency, Money};
use sqlx::{PgConnection, PgExecutor, PgPool, Row};
use storage::{PostOptions, PostgresLedger, StorageError};
use uuid::Uuid;

#[derive(Clone)]
pub enum Ledger {
    Postgres(PostgresLedger),
    TigerBeetle(HybridLedger<AnyTb>),
}

/// Every account's signed balance as the journal derives it — `reconciled_sums` plus the
/// entries of transactions not folded yet (`workers::reconcile`'s derivation) — for
/// aggregates under TigerBeetle, where a scan of every account's cluster balance would cost a
/// lookup per account. Equal to the cluster's balances once committed posts are applied, which
/// the reconciliation worker checks.
const JOURNAL: &str = "WITH w AS (SELECT through_sealed_seq AS s FROM reconcile_watermark),
     j AS (
         SELECT account_id, sum_minor AS raw FROM reconciled_sums
         UNION ALL
         SELECT e.account_id,
                CASE e.direction WHEN 'credit' THEN e.amount_minor ELSE -e.amount_minor END
         FROM (SELECT t.id FROM transactions t, w WHERE t.sealed_seq > w.s
               UNION ALL
               SELECT id FROM transactions WHERE sealed_seq IS NULL) t
         JOIN entries e ON e.transaction_id = t.id
     )";

fn orient(account_type: AccountType, raw: i128, currency: Currency) -> Money {
    Money::from_minor(
        match account_type.normal_side() {
            NormalSide::Credit => raw,
            NormalSide::Debit => -raw,
        },
        currency,
    )
}

fn tb_error(e: TbError) -> StorageError {
    let kind = match &e {
        TbError::Storage(_) => None,
        TbError::Unavailable(_) => Some("unavailable"),
        TbError::Retry(_) => Some("retry"),
        TbError::Protocol(_) => Some("protocol"),
    };
    if let Some(kind) = kind {
        metrics::counter!("tigerbeetle_request_errors_total", "kind" => kind).increment(1);
    }
    e.into()
}

impl Ledger {
    /// `LEDGER_BACKEND` (default `postgres`); for `tigerbeetle`, `TIGERBEETLE_*`, the native
    /// client and a prepared cluster (see `HybridLedger::prepare`). A build without the
    /// `tigerbeetle` feature refuses `tigerbeetle`.
    pub async fn from_env(pg: PostgresLedger) -> Result<Self, String> {
        Self::connect(ledger_tigerbeetle::backend_from_env()?, pg).await
    }

    /// The backend `ledger_tigerbeetle::backend_from_env` chose (`None`: Postgres), which a
    /// server parses before it connects to anything.
    pub async fn connect(
        tigerbeetle: Option<TbConfig>,
        pg: PostgresLedger,
    ) -> Result<Self, String> {
        Ok(match tigerbeetle {
            None => Ledger::Postgres(pg),
            Some(cfg) => Ledger::TigerBeetle(HybridLedger::connect(cfg, pg).await?),
        })
    }

    pub fn name(&self) -> &'static str {
        match self {
            Ledger::Postgres(_) => "postgres",
            Ledger::TigerBeetle(_) => "tigerbeetle",
        }
    }

    /// For everything that is not posting or a balance: users, accounts, currencies.
    pub fn postgres(&self) -> &PostgresLedger {
        match self {
            Ledger::Postgres(l) => l,
            Ledger::TigerBeetle(l) => l.postgres(),
        }
    }

    pub fn pool(&self) -> &PgPool {
        self.postgres().pool()
    }

    pub async fn lookup_currency(&self, code: &str) -> storage::Result<Currency> {
        self.postgres().lookup_currency(code).await
    }

    /// `conn` must not be in a transaction under TigerBeetle: the hybrid's COMMIT is the
    /// commit point of the posting protocol.
    pub async fn post_on(
        &self,
        conn: &mut PgConnection,
        txn: &Transaction,
        opts: PostOptions,
    ) -> storage::Result<()> {
        self.post_screened_on(conn, txn, opts, None).await
    }

    /// [`post_on`](Self::post_on) for a posting the AML limits apply to. Where the cluster
    /// enforces them ([`cluster_screens`](Self::cluster_screens)) it does so from `screening`
    /// and the guard in `opts` must leave them alone; everywhere else the guard enforces them
    /// and `screening` is ignored.
    pub async fn post_screened_on(
        &self,
        conn: &mut PgConnection,
        txn: &Transaction,
        opts: PostOptions,
        screening: Option<Screening>,
    ) -> storage::Result<()> {
        match self {
            Ledger::Postgres(l) => l.post_on(conn, txn, opts).await,
            Ledger::TigerBeetle(l) => {
                let result = l
                    .post_screened_on(conn, txn, opts, screening)
                    .await
                    .map_err(tb_error);
                // The handler asks Postgres whose the id is: let it see what the cluster knows.
                if let Err(StorageError::Ledger(LedgerError::DuplicateTransaction(_))) = &result {
                    l.settle_journal().await.map_err(tb_error)?;
                }
                result
            }
        }
    }

    fn direct(&self) -> Option<&HybridLedger<AnyTb>> {
        match self {
            Ledger::TigerBeetle(l) if l.tb().config().posting == Posting::Direct => Some(l),
            _ => None,
        }
    }

    fn direct_only(&self) -> storage::Result<&HybridLedger<AnyTb>> {
        self.direct().ok_or_else(|| {
            StorageError::DataIntegrity("this ledger backend does not post directly".into())
        })
    }

    /// Whether the cluster decides transfers and conversions and enforces the AML limits
    /// (direct posting): the Postgres guards must not, and the journal follows the cluster.
    pub fn cluster_screens(&self) -> bool {
        self.direct().is_some()
    }

    /// Direct posting only: `txn` as one chain in the cluster, with its key and AML holds.
    pub async fn post_keyed(
        &self,
        txn: &Transaction,
        fingerprint: &str,
        screening: &Screening,
    ) -> storage::Result<Keyed> {
        self.direct_only()?
            .tb()
            .post_keyed(txn, fingerprint, Some(screening))
            .await
            .map_err(tb_error)
    }

    /// Direct posting only: whom an idempotency key belongs to in the cluster.
    pub async fn key_owner(&self, key: Uuid) -> storage::Result<Option<KeyOwner>> {
        self.direct_only()?
            .tb()
            .key_owner(key)
            .await
            .map_err(tb_error)
    }

    /// Direct posting only: what a posted transaction moved out of `from` and into `to`.
    pub async fn keyed_movement(
        &self,
        key: Uuid,
        from: Uuid,
        to: Uuid,
    ) -> storage::Result<(i64, i64)> {
        let legs = self
            .direct_only()?
            .tb()
            .keyed_legs(key)
            .await
            .map_err(tb_error)?;
        let sum = |pick: &dyn Fn(&ledger_tigerbeetle::Transfer) -> bool| {
            let total: u128 = legs.iter().filter(|l| pick(l)).map(|l| l.amount).sum();
            i64::try_from(total).map_err(|_| {
                StorageError::AmountTooLarge(i128::try_from(total).unwrap_or(i128::MAX))
            })
        };
        Ok((
            sum(&|l| l.debit_account_id == from.as_u128())?,
            sum(&|l| l.credit_account_id == to.as_u128())?,
        ))
    }

    /// Direct posting only: voids an idempotency key in the cluster. `false`: the key
    /// already belongs to a posting, a guarded request or someone else's void.
    pub async fn void_key(&self, key: Uuid, by: Uuid) -> storage::Result<bool> {
        self.direct_only()?
            .tb()
            .void_key(key, by)
            .await
            .map_err(tb_error)
    }

    /// Makes the journal current before it is read: under direct posting it follows the
    /// cluster, and this waits for (and helps) the mirror to hold everything the cluster had
    /// applied. Nothing to do on the other backends, whose postings commit in Postgres.
    pub async fn settle_journal(&self) -> storage::Result<()> {
        match self.direct() {
            Some(l) => l.settle_journal().await.map_err(tb_error),
            None => Ok(()),
        }
    }

    /// In the caller's transaction (TigerBeetle first: an account left in the cluster by a
    /// rolled-back transaction is empty and reused when the id is opened again).
    pub async fn open_account_owned_on(
        &self,
        conn: &mut PgConnection,
        account: &Account,
        owner: Option<Uuid>,
    ) -> storage::Result<()> {
        match self {
            Ledger::Postgres(l) => l.open_account_owned_on(conn, account, owner).await,
            Ledger::TigerBeetle(l) => l
                .open_account_owned_on(conn, account, owner)
                .await
                .map_err(tb_error),
        }
    }

    /// Posted and available balances, in `ids` order (`UnknownAccount` for an id that does
    /// not exist). Postgres has no reservations, so there both are the posted balance; the
    /// Postgres read runs on `db`, so a handler holding a connection takes no second one.
    pub async fn balances<'e>(
        &self,
        db: impl PgExecutor<'e>,
        ids: &[AccountId],
    ) -> storage::Result<Vec<Balance>> {
        if let Ledger::TigerBeetle(l) = self {
            return l.tb().balances(ids).await.map_err(tb_error);
        }
        let uuids: Vec<Uuid> = ids.iter().map(|id| id.as_uuid()).collect();
        let rows = sqlx::query(
            "SELECT a.id, a.account_type, a.currency, c.exponent, b.raw_minor
             FROM accounts a
             JOIN balances b ON b.account_id = a.id
             JOIN currencies c ON c.code = a.currency
             WHERE a.id = ANY($1)",
        )
        .bind(&uuids)
        .fetch_all(db)
        .await?;
        let mut found = std::collections::HashMap::with_capacity(rows.len());
        for row in rows {
            let id: Uuid = row.try_get("id")?;
            let type_str: String = row.try_get("account_type")?;
            let code: String = row.try_get("currency")?;
            let exponent: i16 = row.try_get("exponent")?;
            let raw: i64 = row.try_get("raw_minor")?;
            let account_type = AccountType::from_db_str(&type_str)
                .ok_or_else(|| StorageError::DataIntegrity(format!("account_type={type_str}")))?;
            let currency = Currency::new(&code, exponent as u8)
                .map_err(|e| StorageError::DataIntegrity(e.to_string()))?;
            let posted = orient(account_type, raw as i128, currency);
            found.insert(
                id,
                Balance {
                    account: AccountId(id),
                    posted,
                    available: posted,
                    raw: raw as i128,
                },
            );
        }
        ids.iter()
            .map(|id| {
                found
                    .get(&id.as_uuid())
                    .copied()
                    .ok_or_else(|| LedgerError::UnknownAccount(*id).into())
            })
            .collect()
    }

    /// The owner and the posted balance of one account.
    pub async fn balance_with_owner(
        &self,
        id: AccountId,
    ) -> storage::Result<(Option<Uuid>, Money)> {
        match self {
            Ledger::Postgres(l) => l.balance_with_owner(id).await,
            Ledger::TigerBeetle(l) => {
                let owner = l.postgres().account_owner(id).await?;
                let balance = l.tb().balances(&[id]).await.map_err(tb_error)?;
                Ok((owner, balance[0].posted))
            }
        }
    }

    /// Per currency, what every account nets to: zero while money is conserved. Postgres sums
    /// its balances; under TigerBeetle, whose ledgers conserve by construction, the journal.
    pub async fn conservation(&self) -> storage::Result<Vec<(String, i64)>> {
        self.settle_journal().await?;
        let sql = match self {
            Ledger::Postgres(_) => "SELECT a.currency, SUM(b.raw_minor)::BIGINT
                 FROM balances b JOIN accounts a ON a.id = b.account_id
                 GROUP BY a.currency ORDER BY a.currency"
                .to_string(),
            Ledger::TigerBeetle(_) => format!(
                "{JOURNAL} SELECT a.currency, SUM(j.raw)::BIGINT
                 FROM j JOIN accounts a ON a.id = j.account_id
                 GROUP BY a.currency ORDER BY a.currency"
            ),
        };
        Ok(sqlx::query_as(&sql).fetch_all(self.pool()).await?)
    }

    /// Per currency, the funds held in user wallets.
    pub async fn customer_funds(&self) -> storage::Result<Vec<(String, i64)>> {
        self.settle_journal().await?;
        let sql = match self {
            Ledger::Postgres(_) => "SELECT a.currency, SUM(b.raw_minor)::BIGINT
                 FROM balances b JOIN accounts a ON a.id = b.account_id
                 WHERE a.account_type = 'user_wallet'
                 GROUP BY a.currency ORDER BY a.currency"
                .to_string(),
            Ledger::TigerBeetle(_) => format!(
                "{JOURNAL} SELECT a.currency, SUM(j.raw)::BIGINT
                 FROM j JOIN accounts a ON a.id = j.account_id
                 WHERE a.account_type = 'user_wallet'
                 GROUP BY a.currency ORDER BY a.currency"
            ),
        };
        Ok(sqlx::query_as(&sql).fetch_all(self.pool()).await?)
    }
}
