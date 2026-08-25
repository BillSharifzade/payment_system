//! Durable, concurrency-safe ledger persistence.
//!
//! [`PostgresLedger`] enforces the **same** double-entry rules as the pure
//! `ledger` crate (it reuses [`Transaction::validate`] for the structural half),
//! but adds the two things a real backend must provide and an in-memory map
//! cannot: **durability** and **correctness under concurrency**.
//!
//! # How concurrency is made safe
//!
//! Two requests touching the same account must never both read a stale balance
//! and both succeed (the classic double-spend). [`PostgresLedger::post`] prevents
//! this by, inside a single database transaction:
//!
//! 1. taking a row lock (`SELECT ... FOR UPDATE`) on each affected account's
//!    balance row, **acquired in a deterministic (sorted) order** so two
//!    concurrent transfers touching the same pair of accounts can never deadlock;
//! 2. re-reading the now-locked balances, checking the rules, and writing the
//!    new balances and entries; then committing.
//!
//! Unrelated accounts are never locked against each other, so throughput stays
//! high; the *same* account is serialised, which is exactly what correctness
//! demands.

mod error;

pub use error::{Result, StorageError};

use std::collections::HashMap;

use ledger::{Account, AccountId, AccountType, LedgerError, NormalSide, Transaction};
use money::{Currency, Money};
use sqlx::{PgPool, Row};
use uuid::Uuid;

/// The durable-ledger seam. `PostgresLedger` implements it now; a
/// `TigerBeetleLedger` would implement the same trait later (Phase 2), so
/// business logic never depends on the backend.
#[allow(async_fn_in_trait)]
pub trait LedgerStore {
    /// Register an account (idempotent — opening an existing account is a no-op).
    async fn open_account(&self, account: &Account) -> Result<()>;

    /// Atomically validate and post a transaction. All entries apply or none do;
    /// re-posting the same transaction id is rejected.
    async fn post(&self, txn: &Transaction) -> Result<()>;

    /// The current oriented balance of an account.
    async fn balance(&self, account_id: AccountId) -> Result<Money>;
}

/// A PostgreSQL-backed ledger.
#[derive(Debug, Clone)]
pub struct PostgresLedger {
    pool: PgPool,
}

/// The locked state of one account, read inside the posting transaction.
struct LockedAccount {
    account_type: AccountType,
    currency: Currency,
    raw_minor: i64,
}

impl PostgresLedger {
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Run embedded migrations. Safe to call repeatedly.
    pub async fn migrate(&self) -> Result<()> {
        sqlx::migrate!("../../migrations")
            .run(&self.pool)
            .await
            .map_err(|e| StorageError::Database(e.into()))?;
        Ok(())
    }

    /// The underlying connection pool, so higher layers (e.g. the API's
    /// idempotency table) can share it rather than open a second one.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    /// Open an account owned by a specific user (or `None` for a system
    /// account). Both the account row and its zero balance are created
    /// atomically. Idempotent on the account id.
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

        sqlx::query(
            "INSERT INTO balances (account_id, raw_minor, version)
             VALUES ($1, 0, 0)
             ON CONFLICT (account_id) DO NOTHING",
        )
        .bind(account.id.as_uuid())
        .execute(&mut *tx)
        .await?;

        tx.commit().await?;
        Ok(())
    }

    /// The owner of an account: `Ok(Some(user))` for a user wallet, `Ok(None)`
    /// for a system account, `Err(UnknownAccount)` if it does not exist.
    pub async fn account_owner(&self, account_id: AccountId) -> Result<Option<Uuid>> {
        let row = sqlx::query("SELECT owner_user_id FROM accounts WHERE id = $1")
            .bind(account_id.as_uuid())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(LedgerError::UnknownAccount(account_id))?;
        Ok(row.try_get("owner_user_id")?)
    }

    /// An account's type and owner in one lookup, so callers can authorize and
    /// validate the target kind without a second query.
    /// `Err(UnknownAccount)` if it does not exist.
    pub async fn account_info(&self, account_id: AccountId) -> Result<(AccountType, Option<Uuid>)> {
        let row = sqlx::query("SELECT account_type, owner_user_id FROM accounts WHERE id = $1")
            .bind(account_id.as_uuid())
            .fetch_optional(&self.pool)
            .await?
            .ok_or(LedgerError::UnknownAccount(account_id))?;
        let type_str: String = row.try_get("account_type")?;
        let account_type = AccountType::from_db_str(&type_str)
            .ok_or_else(|| StorageError::DataIntegrity(format!("account_type={type_str}")))?;
        let owner: Option<Uuid> = row.try_get("owner_user_id")?;
        Ok((account_type, owner))
    }

    /// The currency of an account. Errors with `UnknownAccount` if it does not
    /// exist.
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
        Currency::new(&code, exponent as u8).map_err(|e| StorageError::DataIntegrity(e.to_string()))
    }

    /// Look up a registered currency by code, reading its exponent from the
    /// `currencies` table. Errors if the currency is not registered.
    pub async fn lookup_currency(&self, code: &str) -> Result<Currency> {
        let row = sqlx::query("SELECT exponent FROM currencies WHERE code = $1")
            .bind(code)
            .fetch_optional(&self.pool)
            .await?
            .ok_or_else(|| StorageError::UnknownCurrency(code.to_string()))?;
        let exponent: i16 = row.try_get("exponent")?;
        Currency::new(code, exponent as u8).map_err(|e| StorageError::DataIntegrity(e.to_string()))
    }

    /// Convert a core i128 minor-unit amount to the i64 we persist, erroring
    /// (never truncating) if it somehow does not fit.
    fn to_i64(amount: i128) -> Result<i64> {
        i64::try_from(amount).map_err(|_| StorageError::AmountTooLarge(amount))
    }

    /// Build the JSON payload for a `transaction.posted` outbox event.
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

    /// Re-orient a raw signed balance into the account's normal direction.
    fn orient(account_type: AccountType, raw_minor: i64, currency: Currency) -> Money {
        let oriented = match account_type.normal_side() {
            NormalSide::Credit => raw_minor as i128,
            NormalSide::Debit => -(raw_minor as i128),
        };
        Money::from_minor(oriented, currency)
    }
}

impl LedgerStore for PostgresLedger {
    async fn open_account(&self, account: &Account) -> Result<()> {
        // System accounts (no owner) go through the same single code path.
        self.open_account_owned(account, None).await
    }

    async fn post(&self, txn: &Transaction) -> Result<()> {
        // Structural validity first — cheap, and needs no DB state.
        txn.validate()?;

        let mut db = self.pool.begin().await?;

        // Idempotency: claim the transaction id. A conflict means it was already
        // posted, so we reject rather than double-apply.
        let claim =
            sqlx::query("INSERT INTO transactions (id) VALUES ($1) ON CONFLICT (id) DO NOTHING")
                .bind(txn.id.as_uuid())
                .execute(&mut *db)
                .await?;
        if claim.rows_affected() == 0 {
            return Err(LedgerError::DuplicateTransaction(txn.id).into());
        }

        // Lock the affected accounts in a deterministic order to avoid deadlocks.
        let mut account_ids: Vec<Uuid> =
            txn.entries.iter().map(|e| e.account_id.as_uuid()).collect();
        account_ids.sort_unstable();
        account_ids.dedup();

        let mut locked: HashMap<Uuid, LockedAccount> = HashMap::with_capacity(account_ids.len());
        for id in &account_ids {
            let row = sqlx::query(
                "SELECT a.account_type, a.currency, c.exponent, b.raw_minor
                 FROM accounts a
                 JOIN balances b ON b.account_id = a.id
                 JOIN currencies c ON c.code = a.currency
                 WHERE a.id = $1
                 FOR UPDATE OF b",
            )
            .bind(id)
            .fetch_optional(&mut *db)
            .await?
            .ok_or(LedgerError::UnknownAccount(AccountId(*id)))?;

            let type_str: String = row.try_get("account_type")?;
            let account_type = AccountType::from_db_str(&type_str)
                .ok_or_else(|| StorageError::DataIntegrity(format!("account_type={type_str}")))?;
            let code: String = row.try_get("currency")?;
            let exponent: i16 = row.try_get("exponent")?;
            let currency = Currency::new(&code, exponent as u8)
                .map_err(|e| StorageError::DataIntegrity(e.to_string()))?;
            let raw_minor: i64 = row.try_get("raw_minor")?;

            locked.insert(
                *id,
                LockedAccount {
                    account_type,
                    currency,
                    raw_minor,
                },
            );
        }

        // Compute proposed new raw balances without writing yet (all-or-nothing).
        let mut proposed: HashMap<Uuid, i64> =
            locked.iter().map(|(id, a)| (*id, a.raw_minor)).collect();

        for entry in &txn.entries {
            let id = entry.account_id.as_uuid();
            let account = &locked[&id];

            if account.currency != entry.amount.currency() {
                return Err(LedgerError::AccountCurrencyMismatch {
                    account: entry.account_id,
                    account_currency: account.currency,
                    entry_currency: entry.amount.currency(),
                }
                .into());
            }

            let signed = Self::to_i64(entry.signed_amount()?.minor_units())?;
            let current = proposed[&id];
            let next = current.checked_add(signed).ok_or(LedgerError::Money(
                money::MoneyError::Overflow { operation: "post" },
            ))?;
            proposed.insert(id, next);
        }

        // Enforce the no-negative rule on accounts that require it.
        for (id, account) in &locked {
            if !account.account_type.allows_negative_balance() {
                let oriented = Self::orient(account.account_type, proposed[id], account.currency);
                if oriented.is_negative() {
                    return Err(LedgerError::InsufficientFunds {
                        account: AccountId(*id),
                        balance_minor: account.raw_minor as i128,
                        delta_minor: (proposed[id] - account.raw_minor) as i128,
                    }
                    .into());
                }
            }
        }

        // Write all entries in ONE round trip (UNNEST turns the arrays into
        // rows). Round trips inside this transaction directly extend how long
        // the balance row locks are held, so fewer statements = more write TPS.
        {
            let mut ids = Vec::with_capacity(txn.entries.len());
            let mut accounts = Vec::with_capacity(txn.entries.len());
            let mut directions = Vec::with_capacity(txn.entries.len());
            let mut amounts = Vec::with_capacity(txn.entries.len());
            let mut currencies = Vec::with_capacity(txn.entries.len());
            for entry in &txn.entries {
                ids.push(entry.id.as_uuid());
                accounts.push(entry.account_id.as_uuid());
                directions.push(entry.direction.as_db_str().to_string());
                amounts.push(Self::to_i64(entry.amount.minor_units())?);
                currencies.push(entry.amount.currency().code().to_string());
            }
            sqlx::query(
                "INSERT INTO entries
                   (id, transaction_id, account_id, direction, amount_minor, currency)
                 SELECT u.id, $2, u.account_id, u.direction, u.amount_minor, u.currency
                 FROM UNNEST($1::uuid[], $3::uuid[], $4::text[], $5::bigint[], $6::text[])
                      AS u(id, account_id, direction, amount_minor, currency)",
            )
            .bind(&ids)
            .bind(txn.id.as_uuid())
            .bind(&accounts)
            .bind(&directions)
            .bind(&amounts)
            .bind(&currencies)
            .execute(&mut *db)
            .await?;
        }

        // Update all balances in one round trip, same idea.
        {
            let (ids, raws): (Vec<Uuid>, Vec<i64>) =
                proposed.iter().map(|(id, raw)| (*id, *raw)).unzip();
            sqlx::query(
                "UPDATE balances b
                 SET raw_minor = u.raw_minor, version = b.version + 1, updated_at = now()
                 FROM UNNEST($1::uuid[], $2::bigint[]) AS u(account_id, raw_minor)
                 WHERE b.account_id = u.account_id",
            )
            .bind(&ids)
            .bind(&raws)
            .execute(&mut *db)
            .await?;
        }

        // Transactional outbox: emit a 'transaction.posted' event in the SAME
        // transaction as the entries. It is published if and only if this
        // commits — no dual write (DESIGN.md §5.4).
        let payload = Self::posted_event_payload(txn)?;
        sqlx::query(
            "INSERT INTO outbox (id, aggregate_id, event_type, payload)
             VALUES ($1, $2, $3, $4)",
        )
        .bind(Uuid::new_v4())
        .bind(txn.id.as_uuid())
        .bind("transaction.posted")
        .bind(payload)
        .execute(&mut *db)
        .await?;

        db.commit().await?;
        Ok(())
    }

    async fn balance(&self, account_id: AccountId) -> Result<Money> {
        let row = sqlx::query(
            "SELECT a.account_type, a.currency, c.exponent, b.raw_minor
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
        let account_type = AccountType::from_db_str(&type_str)
            .ok_or_else(|| StorageError::DataIntegrity(format!("account_type={type_str}")))?;
        let code: String = row.try_get("currency")?;
        let exponent: i16 = row.try_get("exponent")?;
        let currency = Currency::new(&code, exponent as u8)
            .map_err(|e| StorageError::DataIntegrity(e.to_string()))?;
        let raw_minor: i64 = row.try_get("raw_minor")?;

        Ok(Self::orient(account_type, raw_minor, currency))
    }
}
