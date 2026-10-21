mod error;

pub use error::{Result, StorageError};

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use ledger::{Account, AccountId, AccountType, LedgerError, NormalSide, Transaction};
use money::{Currency, Money};
use sqlx::{Connection, PgConnection, PgPool, Postgres, Row};
use uuid::Uuid;

#[allow(async_fn_in_trait)]
pub trait LedgerStore {
    async fn open_account(&self, account: &Account) -> Result<()>;

    async fn post(&self, txn: &Transaction) -> Result<()>;

    async fn post_with(&self, txn: &Transaction, opts: PostOptions) -> Result<()>;

    async fn balance(&self, account_id: AccountId) -> Result<Money>;
}

#[derive(Debug, Clone)]
pub struct PostgresLedger {
    pool: PgPool,
    currencies: Arc<RwLock<HashMap<String, Currency>>>,
    // System accounts (settlement, fee, FX shards) a post has verified. An account's type and
    // currency never change (migration 0029 makes the database refuse it), so after the first
    // post that touches one its metadata costs no query.
    system_accounts: Arc<RwLock<HashMap<Uuid, Currency>>>,
}

#[derive(Debug, Clone)]
pub struct IdempotencyRecord {
    pub key: Uuid,
    pub fingerprint: String,
    pub response_status: i32,
    pub response_body: serde_json::Value,
}

#[derive(Debug, thiserror::Error)]
pub enum HookError {
    #[error("{message}")]
    Rejected { rule: String, message: String },
    #[error(transparent)]
    Database(#[from] sqlx::Error),
}

pub type PostHook = Box<
    dyn for<'c> FnOnce(
            &'c mut PgConnection,
        ) -> Pin<
            Box<dyn Future<Output = std::result::Result<(), HookError>> + Send + 'c>,
        > + Send,
>;

#[derive(Default)]
pub struct PostOptions {
    pub idempotency: Option<IdempotencyRecord>,
    /// Runs in the posting transaction right after the id is claimed and before any wallet is
    /// locked; a rejection rolls the whole post back. It may lock rows that never wait for a
    /// balance (a users row, a check, a deposit request) — never a balance itself.
    pub guard: Option<PostHook>,
}

struct LockedWallet {
    account_type: AccountType,
    currency: Currency,
    raw_minor: i64,
}

fn parse_account_type(type_str: &str) -> Result<AccountType> {
    AccountType::from_db_str(type_str)
        .ok_or_else(|| StorageError::DataIntegrity(format!("account_type={type_str}")))
}

/// COMMIT of a transaction whose outcome is acknowledged to a client. statement_timeout is
/// lifted first, so a COMMIT waiting for a synchronous standby ends by replication, by failover
/// (the connection drops: 503) or by the request deadline (504), never by a timeout cancel —
/// Postgres answers a cancelled wait with success ("committed locally, but might not have been
/// replicated") for data the standby may lack. An idempotent retry settles the other outcomes.
pub async fn commit_durable(
    mut tx: sqlx::Transaction<'_, Postgres>,
) -> std::result::Result<(), sqlx::Error> {
    sqlx::query("SET LOCAL statement_timeout = 0")
        .execute(&mut *tx)
        .await?;
    tx.commit().await
}

fn parse_currency(code: &str, exponent: i16) -> Result<Currency> {
    Currency::new(code, exponent as u8).map_err(|e| StorageError::DataIntegrity(e.to_string()))
}

impl PostgresLedger {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            currencies: Arc::new(RwLock::new(HashMap::new())),
            system_accounts: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("../../migrations")
            .run(&self.pool)
            .await
            .map_err(|e| StorageError::Database(e.into()))?;
        self.load_currencies().await
    }

    pub async fn load_currencies(&self) -> Result<()> {
        let rows = sqlx::query("SELECT code, exponent FROM currencies")
            .fetch_all(&self.pool)
            .await?;
        let mut map = HashMap::with_capacity(rows.len());
        for row in rows {
            let code: String = row.try_get("code")?;
            let exponent: i16 = row.try_get("exponent")?;
            map.insert(code.clone(), parse_currency(&code, exponent)?);
        }
        *self.currencies.write().expect("currency cache poisoned") = map;
        Ok(())
    }

    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    pub async fn open_account_owned(&self, account: &Account, owner: Option<Uuid>) -> Result<()> {
        let mut tx = self.pool.begin().await?;
        self.open_account_owned_on(&mut tx, account, owner).await?;
        commit_durable(tx).await?;
        Ok(())
    }

    // The caller owns the transaction, so an account can be created atomically with its owner.
    pub async fn open_account_owned_on(
        &self,
        conn: &mut PgConnection,
        account: &Account,
        owner: Option<Uuid>,
    ) -> Result<()> {
        sqlx::query(
            "INSERT INTO accounts (id, account_type, currency, owner_user_id)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(account.id.as_uuid())
        .bind(account.account_type.as_db_str())
        .bind(account.currency.code())
        .bind(owner)
        .execute(&mut *conn)
        .await?;

        let min_raw: Option<i64> = if account.allows_negative_balance() {
            None
        } else {
            Some(0)
        };
        sqlx::query(
            "INSERT INTO balances (account_id, raw_minor, version, min_raw)
             VALUES ($1, 0, 0, $2)
             ON CONFLICT (account_id) DO NOTHING",
        )
        .bind(account.id.as_uuid())
        .bind(min_raw)
        .execute(&mut *conn)
        .await?;
        Ok(())
    }

    pub async fn account_owner(&self, account_id: AccountId) -> Result<Option<Uuid>> {
        let row = sqlx::query("SELECT owner_user_id FROM accounts WHERE id = $1")
            .bind(account_id.as_uuid())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(LedgerError::UnknownAccount(account_id))?;
        Ok(row.try_get("owner_user_id")?)
    }

    pub async fn account_info(&self, account_id: AccountId) -> Result<(AccountType, Option<Uuid>)> {
        let row = sqlx::query("SELECT account_type, owner_user_id FROM accounts WHERE id = $1")
            .bind(account_id.as_uuid())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(LedgerError::UnknownAccount(account_id))?;
        let type_str: String = row.try_get("account_type")?;
        let owner: Option<Uuid> = row.try_get("owner_user_id")?;
        Ok((parse_account_type(&type_str)?, owner))
    }

    pub async fn account_currency(&self, account_id: AccountId) -> Result<Currency> {
        let row = sqlx::query(
            "SELECT a.currency, c.exponent
             FROM accounts a JOIN currencies c ON c.code = a.currency
             WHERE a.id = $1",
        )
        .bind(account_id.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(LedgerError::UnknownAccount(account_id))?;
        let code: String = row.try_get("currency")?;
        let exponent: i16 = row.try_get("exponent")?;
        parse_currency(&code, exponent)
    }

    pub async fn lookup_currency(&self, code: &str) -> Result<Currency> {
        if let Some(c) = self
            .currencies
            .read()
            .expect("currency cache poisoned")
            .get(code)
        {
            return Ok(*c);
        }
        let row = sqlx::query("SELECT exponent FROM currencies WHERE code = $1")
            .bind(code)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| StorageError::UnknownCurrency(code.to_string()))?;
        let exponent: i16 = row.try_get("exponent")?;
        let currency = parse_currency(code, exponent)?;
        self.currencies
            .write()
            .expect("currency cache poisoned")
            .insert(code.to_string(), currency);
        Ok(currency)
    }

    /// Reads `balances`, which only this backend writes: under `LEDGER_BACKEND=tigerbeetle` it
    /// is stale, and callers read balances through the backend (`api::Ledger`).
    pub async fn balance_with_owner(&self, account_id: AccountId) -> Result<(Option<Uuid>, Money)> {
        let row = sqlx::query(
            "SELECT a.account_type, a.currency, a.owner_user_id, c.exponent, b.raw_minor
             FROM accounts a
             JOIN balances b ON b.account_id = a.id
             JOIN currencies c ON c.code = a.currency
             WHERE a.id = $1",
        )
        .bind(account_id.as_uuid())
        .fetch_optional(&self.pool)
        .await?
        .ok_or(LedgerError::UnknownAccount(account_id))?;
        let type_str: String = row.try_get("account_type")?;
        let code: String = row.try_get("currency")?;
        let exponent: i16 = row.try_get("exponent")?;
        let raw_minor: i64 = row.try_get("raw_minor")?;
        let owner: Option<Uuid> = row.try_get("owner_user_id")?;
        let account_type = parse_account_type(&type_str)?;
        let currency = parse_currency(&code, exponent)?;
        Ok((
            owner,
            Self::orient(account_type, raw_minor as i128, currency),
        ))
    }

    fn to_i64(amount: i128) -> Result<i64> {
        i64::try_from(amount).map_err(|_| StorageError::AmountTooLarge(amount))
    }

    fn posted_event_payload(txn: &Transaction) -> Result<serde_json::Value> {
        let mut entries = Vec::with_capacity(txn.entries.len());
        for e in &txn.entries {
            entries.push(serde_json::json!({
                "account_id": e.account_id.as_uuid(),
                "direction": e.direction.as_db_str(),
                "amount_minor": Self::to_i64(e.amount.minor_units())?,
                "currency": e.amount.currency().code(),
            }));
        }
        Ok(serde_json::json!({
            "transaction_id": txn.id.as_uuid(),
            "entries": entries,
        }))
    }

    fn orient(account_type: AccountType, raw_minor: i128, currency: Currency) -> Money {
        let oriented = match account_type.normal_side() {
            NormalSide::Credit => raw_minor,
            NormalSide::Debit => -raw_minor,
        };
        Money::from_minor(oriented, currency)
    }

    pub async fn post_on(
        &self,
        conn: &mut PgConnection,
        txn: &Transaction,
        opts: PostOptions,
    ) -> Result<()> {
        txn.validate()?;

        // Every statement of a post (and of its guard and triggers) is a keyed lookup or write
        // whose generic plan is the right one. In the default `auto` mode Postgres re-planned
        // the array-parameter statements on every execution — their generic estimate assumes
        // ten elements, so a custom plan always looked cheaper — which cost about a quarter of
        // the database CPU of a post (crates/loadtest/README.md). One round trip with BEGIN;
        // SET LOCAL ends with the transaction, so nothing leaks onto the pooled connection.
        let nested = conn.is_in_transaction();
        let mut db = if nested {
            conn.begin().await?
        } else {
            conn.begin_with("BEGIN; SET LOCAL plan_cache_mode = force_generic_plan")
                .await?
        };

        let claim =
            sqlx::query("INSERT INTO transactions (id) VALUES ($1) ON CONFLICT (id) DO NOTHING")
                .bind(txn.id.as_uuid())
                .execute(&mut *db)
                .await?;
        if claim.rows_affected() == 0 {
            return Err(LedgerError::DuplicateTransaction(txn.id).into());
        }

        // The guard runs before any wallet is locked: it serialises on rows of its own (a users
        // row, a check, a deposit request), so a hot wallet — a merchant everyone pays — stays
        // locked only for the write and the commit, not for every payer's guard. Lock order
        // of a post: transactions row, guard rows, wallets (id order), system shards (sorted).
        if let Some(guard) = opts.guard {
            guard(&mut db).await.map_err(|e| match e {
                HookError::Rejected { rule, message } => StorageError::Rejected { rule, message },
                HookError::Database(e) => StorageError::Database(e),
            })?;
        }

        let mut account_ids: Vec<Uuid> =
            txn.entries.iter().map(|e| e.account_id.as_uuid()).collect();
        account_ids.sort_unstable();
        account_ids.dedup();

        let wallet_rows = sqlx::query(
            "SELECT a.id, a.account_type, a.currency, c.exponent, b.raw_minor
             FROM accounts a
             JOIN balances b ON b.account_id = a.id
             JOIN currencies c ON c.code = a.currency
             WHERE a.id = ANY($1) AND a.account_type = 'user_wallet'
             ORDER BY a.id
             FOR UPDATE OF b",
        )
        .bind(&account_ids)
        .fetch_all(&mut *db)
        .await?;

        let mut wallets: HashMap<Uuid, LockedWallet> = HashMap::with_capacity(wallet_rows.len());
        for row in wallet_rows {
            let id: Uuid = row.try_get("id")?;
            let type_str: String = row.try_get("account_type")?;
            let code: String = row.try_get("currency")?;
            let exponent: i16 = row.try_get("exponent")?;
            wallets.insert(
                id,
                LockedWallet {
                    account_type: parse_account_type(&type_str)?,
                    currency: parse_currency(&code, exponent)?,
                    raw_minor: row.try_get("raw_minor")?,
                },
            );
        }

        let mut system: HashMap<Uuid, Currency> = HashMap::new();
        let mut system_ids: Vec<Uuid> = Vec::new();
        {
            let known = self
                .system_accounts
                .read()
                .expect("system account cache poisoned");
            for id in account_ids.iter().filter(|id| !wallets.contains_key(id)) {
                match known.get(id) {
                    Some(currency) => {
                        system.insert(*id, *currency);
                    }
                    None => system_ids.push(*id),
                }
            }
        }
        if !system_ids.is_empty() {
            let rows = sqlx::query(
                "SELECT a.id, a.account_type, a.currency, c.exponent
                 FROM accounts a JOIN currencies c ON c.code = a.currency
                 WHERE a.id = ANY($1)",
            )
            .bind(&system_ids)
            .fetch_all(&mut *db)
            .await?;
            for row in rows {
                let id: Uuid = row.try_get("id")?;
                let type_str: String = row.try_get("account_type")?;
                let code: String = row.try_get("currency")?;
                let exponent: i16 = row.try_get("exponent")?;
                let account_type = parse_account_type(&type_str)?;
                if !account_type.allows_negative_balance() {
                    return Err(StorageError::DataIntegrity(format!(
                        "account {id} is {type_str} but was not lockable as a wallet"
                    )));
                }
                system.insert(id, parse_currency(&code, exponent)?);
            }
            if let Some(missing) = system_ids.iter().find(|id| !system.contains_key(id)) {
                return Err(LedgerError::UnknownAccount(AccountId(*missing)).into());
            }
        }

        // Net every account in i128 first, so whether a post overflows cannot depend on the
        // order of its entries (a debit before the matching credit), then check and convert.
        let overflow = || -> StorageError {
            LedgerError::Money(money::MoneyError::Overflow { operation: "post" }).into()
        };
        let mut net: HashMap<Uuid, i128> = HashMap::with_capacity(account_ids.len());
        for entry in &txn.entries {
            let id = entry.account_id.as_uuid();
            let account_currency = if let Some(w) = wallets.get(&id) {
                w.currency
            } else if let Some(currency) = system.get(&id) {
                *currency
            } else {
                return Err(LedgerError::UnknownAccount(entry.account_id).into());
            };
            if account_currency != entry.amount.currency() {
                return Err(LedgerError::AccountCurrencyMismatch {
                    account: entry.account_id,
                    account_currency,
                    entry_currency: entry.amount.currency(),
                }
                .into());
            }
            let signed = entry.signed_amount()?.minor_units();
            Self::to_i64(signed)?; // every entry must fit the entries column
            let slot = net.entry(id).or_insert(0);
            *slot = slot.checked_add(signed).ok_or_else(overflow)?;
        }

        // In id order, so the account an InsufficientFunds names does not depend on hash order.
        let mut proposed: HashMap<Uuid, i64> = HashMap::with_capacity(wallets.len());
        for id in &account_ids {
            let Some(wallet) = wallets.get(id) else {
                continue;
            };
            let delta = net.get(id).copied().unwrap_or(0);
            let next = (wallet.raw_minor as i128)
                .checked_add(delta)
                .ok_or_else(overflow)?;
            if !wallet.account_type.allows_negative_balance()
                && Self::orient(wallet.account_type, next, wallet.currency).is_negative()
            {
                return Err(LedgerError::InsufficientFunds {
                    account: AccountId(*id),
                    balance_minor: wallet.raw_minor as i128,
                    delta_minor: next - wallet.raw_minor as i128,
                }
                .into());
            }
            proposed.insert(*id, i64::try_from(next).map_err(|_| overflow())?);
        }
        let mut deltas: HashMap<Uuid, i64> = HashMap::with_capacity(system.len());
        for id in system.keys() {
            let delta = net.get(id).copied().unwrap_or(0);
            deltas.insert(*id, i64::try_from(delta).map_err(|_| overflow())?);
        }

        // System-account deltas go last, in id order, as additive updates of rows nobody locked
        // earlier, so each hot shard is held only from its update to the commit. The highest
        // rides in the write statement itself — one round trip less while the wallets are
        // locked; a transfer with a fee has exactly one — and any lower ones go just before it.
        let mut system_order: Vec<(Uuid, i64)> =
            deltas.into_iter().filter(|(_, d)| *d != 0).collect();
        system_order.sort_unstable();
        let last = system_order.pop();
        for (id, delta) in system_order {
            let updated = sqlx::query(
                "UPDATE balances
                 SET raw_minor = raw_minor + $2, version = version + 1, updated_at = now()
                 WHERE account_id = $1",
            )
            .bind(id)
            .bind(delta)
            .execute(&mut *db)
            .await?;
            if updated.rows_affected() != 1 {
                return Err(StorageError::DataIntegrity(format!(
                    "system account {id} has no balance row"
                )));
            }
        }
        {
            let n = txn.entries.len();
            let mut ids = Vec::with_capacity(n);
            let mut accounts = Vec::with_capacity(n);
            let mut directions = Vec::with_capacity(n);
            let mut amounts = Vec::with_capacity(n);
            let mut currencies = Vec::with_capacity(n);
            for entry in &txn.entries {
                ids.push(entry.id.as_uuid());
                accounts.push(entry.account_id.as_uuid());
                directions.push(entry.direction.as_db_str().to_string());
                amounts.push(Self::to_i64(entry.amount.minor_units())?);
                currencies.push(entry.amount.currency().code().to_string());
            }
            let (w_ids, w_raws): (Vec<Uuid>, Vec<i64>) =
                proposed.iter().map(|(id, raw)| (*id, *raw)).unzip();
            let payload = Self::posted_event_payload(txn)?;
            let (ik, ifp, ist, ibody) = match &opts.idempotency {
                Some(r) => (
                    Some(r.key),
                    Some(r.fingerprint.clone()),
                    Some(r.response_status),
                    Some(r.response_body.clone()),
                ),
                None => (None, None, None, None),
            };
            // The last statement before COMMIT, so it also lifts statement_timeout for the COMMIT
            // (see commit_durable) — not inside a caller's transaction, which it would outlive.
            let (system_updated, _): (i64, Option<String>) = sqlx::query_as(
                "WITH e AS (
                     INSERT INTO entries
                       (id, transaction_id, account_id, direction, amount_minor, currency)
                     SELECT u.id, $1, u.account_id, u.direction, u.amount_minor, u.currency
                     FROM UNNEST($2::uuid[], $3::uuid[], $4::text[], $5::bigint[], $6::text[])
                          AS u(id, account_id, direction, amount_minor, currency)
                 ), b AS (
                     UPDATE balances b
                     SET raw_minor = u.raw_minor, version = b.version + 1, updated_at = now()
                     FROM UNNEST($7::uuid[], $8::bigint[]) AS u(account_id, raw_minor)
                     WHERE b.account_id = u.account_id
                 ), s AS (
                     UPDATE balances
                     SET raw_minor = raw_minor + $16, version = version + 1, updated_at = now()
                     WHERE account_id = $15
                     RETURNING 1
                 ), o AS (
                     INSERT INTO outbox (id, aggregate_id, event_type, payload)
                     VALUES ($9::uuid, $1, 'transaction.posted', $10::jsonb)
                 ), i AS (
                     INSERT INTO idempotency_keys (key, fingerprint, response_status, response_body)
                     SELECT $11::uuid, $12::text, $13::int, $14::jsonb
                     WHERE $11::uuid IS NOT NULL
                     ON CONFLICT (key) DO NOTHING
                 )
                 SELECT (SELECT count(*) FROM s),
                        CASE WHEN $17 THEN set_config('statement_timeout', '0', true) END",
            )
            .bind(txn.id.as_uuid())
            .bind(&ids)
            .bind(&accounts)
            .bind(&directions)
            .bind(&amounts)
            .bind(&currencies)
            .bind(&w_ids)
            .bind(&w_raws)
            .bind(Uuid::now_v7())
            .bind(payload)
            .bind(ik)
            .bind(ifp)
            .bind(ist)
            .bind(ibody)
            .bind(last.map(|(id, _)| id))
            .bind(last.map(|(_, d)| d))
            .bind(!nested)
            .fetch_one(&mut *db)
            .await?;
            if let Some((id, _)) = last.filter(|_| system_updated != 1) {
                return Err(StorageError::DataIntegrity(format!(
                    "system account {id} has no balance row"
                )));
            }
        }

        // The write statement already lifted statement_timeout for the COMMIT (commit_durable's
        // rule, without its extra round trip); a savepoint leaves that to the caller's COMMIT.
        db.commit().await?;
        // Only what a committed transaction read: never an account an enclosing transaction
        // may still roll back.
        if !nested && !system_ids.is_empty() {
            let mut known = self
                .system_accounts
                .write()
                .expect("system account cache poisoned");
            known.extend(system_ids.iter().map(|id| (*id, system[id])));
        }
        Ok(())
    }
}

impl LedgerStore for PostgresLedger {
    async fn open_account(&self, account: &Account) -> Result<()> {
        self.open_account_owned(account, None).await
    }

    async fn post(&self, txn: &Transaction) -> Result<()> {
        self.post_with(txn, PostOptions::default()).await
    }

    async fn post_with(&self, txn: &Transaction, opts: PostOptions) -> Result<()> {
        let mut conn = self.pool.acquire().await?;
        self.post_on(&mut conn, txn, opts).await
    }

    async fn balance(&self, account_id: AccountId) -> Result<Money> {
        Ok(self.balance_with_owner(account_id).await?.1)
    }
}
