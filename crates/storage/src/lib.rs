mod error;

pub use error::{Result, StorageError};

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use ledger::{Account, AccountId, AccountType, LedgerError, NormalSide, Transaction};
use money::{Currency, Money};
use sqlx::{Connection, PgConnection, PgPool, Row};
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
    pub guard: Option<PostHook>,
}

struct LockedWallet {
    account_type: AccountType,
    currency: Currency,
    raw_minor: i64,
}

struct SystemAccount {
    account_type: AccountType,
    currency: Currency,
}

fn parse_account_type(type_str: &str) -> Result<AccountType> {
    AccountType::from_db_str(type_str)
        .ok_or_else(|| StorageError::DataIntegrity(format!("account_type={type_str}")))
}

fn parse_currency(code: &str, exponent: i16) -> Result<Currency> {
    Currency::new(code, exponent as u8).map_err(|e| StorageError::DataIntegrity(e.to_string()))
}

impl PostgresLedger {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            currencies: Arc::new(RwLock::new(HashMap::new())),
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
        sqlx::query(
            "INSERT INTO accounts (id, account_type, currency, owner_user_id)
             VALUES ($1, $2, $3, $4)
             ON CONFLICT (id) DO NOTHING",
        )
        .bind(account.id.as_uuid())
        .bind(account.account_type.as_db_str())
        .bind(account.currency.code())
        .bind(owner)
        .execute(&mut *tx)
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
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
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
        Ok((owner, Self::orient(account_type, raw_minor, currency)))
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

    fn orient(account_type: AccountType, raw_minor: i64, currency: Currency) -> Money {
        let oriented = match account_type.normal_side() {
            NormalSide::Credit => raw_minor as i128,
            NormalSide::Debit => -(raw_minor as i128),
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

        let mut db = conn.begin().await?;

        let claim =
            sqlx::query("INSERT INTO transactions (id) VALUES ($1) ON CONFLICT (id) DO NOTHING")
                .bind(txn.id.as_uuid())
                .execute(&mut *db)
                .await?;
        if claim.rows_affected() == 0 {
            return Err(LedgerError::DuplicateTransaction(txn.id).into());
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

        let system_ids: Vec<Uuid> = account_ids
            .iter()
            .copied()
            .filter(|id| !wallets.contains_key(id))
            .collect();
        let mut system: HashMap<Uuid, SystemAccount> = HashMap::with_capacity(system_ids.len());
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
                system.insert(
                    id,
                    SystemAccount {
                        account_type,
                        currency: parse_currency(&code, exponent)?,
                    },
                );
            }
            if let Some(missing) = system_ids.iter().find(|id| !system.contains_key(id)) {
                return Err(LedgerError::UnknownAccount(AccountId(*missing)).into());
            }
        }

        if let Some(guard) = opts.guard {
            guard(&mut db).await.map_err(|e| match e {
                HookError::Rejected { rule, message } => StorageError::Rejected { rule, message },
                HookError::Database(e) => StorageError::Database(e),
            })?;
        }

        let mut proposed: HashMap<Uuid, i64> =
            wallets.iter().map(|(id, w)| (*id, w.raw_minor)).collect();
        let mut deltas: HashMap<Uuid, i64> = system.keys().map(|id| (*id, 0)).collect();

        for entry in &txn.entries {
            let id = entry.account_id.as_uuid();
            let signed = Self::to_i64(entry.signed_amount()?.minor_units())?;
            let (account_currency, slot) = if let Some(w) = wallets.get(&id) {
                (w.currency, proposed.get_mut(&id))
            } else if let Some(s) = system.get(&id) {
                (s.currency, deltas.get_mut(&id))
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
            let slot = slot.ok_or_else(|| StorageError::DataIntegrity("balance slot".into()))?;
            *slot = slot.checked_add(signed).ok_or(LedgerError::Money(
                money::MoneyError::Overflow { operation: "post" },
            ))?;
        }

        for (id, wallet) in &wallets {
            if !wallet.account_type.allows_negative_balance() {
                let oriented = Self::orient(wallet.account_type, proposed[id], wallet.currency);
                if oriented.is_negative() {
                    return Err(LedgerError::InsufficientFunds {
                        account: AccountId(*id),
                        balance_minor: wallet.raw_minor as i128,
                        delta_minor: (proposed[id] - wallet.raw_minor) as i128,
                    }
                    .into());
                }
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
            sqlx::query(
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
                 ), o AS (
                     INSERT INTO outbox (id, aggregate_id, event_type, payload)
                     VALUES ($9::uuid, $1, 'transaction.posted', $10::jsonb)
                 )
                 INSERT INTO idempotency_keys (key, fingerprint, response_status, response_body)
                 SELECT $11::uuid, $12::text, $13::int, $14::jsonb
                 WHERE $11::uuid IS NOT NULL
                 ON CONFLICT (key) DO NOTHING",
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
            .execute(&mut *db)
            .await?;
        }

        let mut system_order: Vec<Uuid> = deltas.keys().copied().collect();
        system_order.sort_unstable();
        for id in system_order {
            let delta = deltas[&id];
            if delta == 0 {
                continue;
            }
            let _ = system[&id].account_type;
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

        db.commit().await?;
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
