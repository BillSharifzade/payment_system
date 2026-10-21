//! Cut-over from `LEDGER_BACKEND=postgres` (opening balances) and the TigerBeetle ↔ journal
//! reconciliation that holds afterwards.

use std::collections::{BTreeSet, HashMap};

use ledger::{Account as LedgerAccount, AccountId, AccountType};
use money::Currency;
use sqlx::postgres::PgRow;
use sqlx::Row;
use storage::StorageError;
use uuid::Uuid;

use crate::client::{
    Account, AccountResult, TbClient, Transfer, TransferResult, CREATE_BATCH, LOOKUP_BATCH,
};
use crate::error::Result;
use crate::hybrid::HybridLedger;
use crate::ids;
use crate::recovery::RecoveryReport;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RollbackReport {
    pub accounts: usize,
    /// `balances` rows that differed from the journal (all that moved since the cut-over).
    pub balances_rewritten: u64,
    /// The final recovery pass, with no grace: nothing was left pending.
    pub settled: RecoveryReport,
}

fn ledger_account(row: &PgRow) -> Result<LedgerAccount> {
    let type_str: String = row.try_get("account_type")?;
    let code: String = row.try_get("currency")?;
    let exponent: i16 = row.try_get("exponent")?;
    let account_type = AccountType::from_db_str(&type_str)
        .ok_or_else(|| StorageError::DataIntegrity(format!("account_type={type_str}")))?;
    let currency = Currency::new(&code, exponent as u8)
        .map_err(|e| StorageError::DataIntegrity(e.to_string()))?;
    Ok(LedgerAccount::new(
        AccountId(row.try_get("id")?),
        account_type,
        currency,
    ))
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImportReport {
    pub accounts: usize,
    /// Accounts with a non-zero balance, each now carrying one opening transfer.
    pub opening_balances: usize,
    /// Highest `transactions.seq` the imported balances include.
    pub through_seq: i64,
}

/// An account whose TigerBeetle balance differs from the sum of its journal entries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mismatch {
    pub account: AccountId,
    pub tigerbeetle_raw: i128,
    pub journal_raw: i128,
}

impl<C: TbClient> HybridLedger<C> {
    /// Copies every account and its balance into TigerBeetle. Each non-zero balance becomes
    /// one transfer against the currency's opening account, whose id derives from the
    /// account, so a re-run is a no-op (until posting resumes: from then on `balances` is no
    /// longer written and the drift check refuses). Afterwards every account satisfies
    /// `tigerbeetle balance = sum of its journal entries` (the opening transfer stands for the
    /// entries before the cut-over), which is what [`reconcile`](Self::reconcile) checks from
    /// then on; and each opening account nets to zero because the books did.
    ///
    /// Money movement must be stopped while it runs (and until `LEDGER_BACKEND=tigerbeetle`
    /// serves): `balances` is read once, in one snapshot, and refused unless it matches the
    /// entries and every currency sums to zero.
    pub async fn import_from_postgres(&self) -> Result<ImportReport> {
        self.refuse_rolled_back().await?;
        let mut db = self.pool().begin().await?;
        sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ, READ ONLY")
            .execute(&mut *db)
            .await?;
        let drifted: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM balances b
             WHERE b.raw_minor <> COALESCE((
                 SELECT SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                             ELSE -e.amount_minor END)
                 FROM entries e WHERE e.account_id = b.account_id), 0)",
        )
        .fetch_one(&mut *db)
        .await?;
        let imbalanced: Vec<(String, i64)> = sqlx::query_as(
            "SELECT a.currency, SUM(b.raw_minor)::BIGINT
             FROM balances b JOIN accounts a ON a.id = b.account_id
             GROUP BY a.currency HAVING SUM(b.raw_minor) <> 0",
        )
        .fetch_all(&mut *db)
        .await?;
        if drifted > 0 || !imbalanced.is_empty() {
            return Err(StorageError::DataIntegrity(format!(
                "refusing to import: {drifted} balances differ from their entries, \
                 unbalanced currencies {imbalanced:?}"
            ))
            .into());
        }
        let rows = sqlx::query(
            "SELECT a.id, a.account_type, a.currency, c.exponent, b.raw_minor
             FROM accounts a
             JOIN balances b ON b.account_id = a.id
             JOIN currencies c ON c.code = a.currency
             ORDER BY a.id",
        )
        .fetch_all(&mut *db)
        .await?;
        let through_seq: i64 = sqlx::query_scalar("SELECT COALESCE(MAX(seq), 0) FROM transactions")
            .fetch_one(&mut *db)
            .await?;
        db.commit().await?;

        let mut accounts = Vec::with_capacity(rows.len());
        for row in &rows {
            accounts.push((ledger_account(row)?, row.try_get::<i64, _>("raw_minor")?));
        }

        self.tb().ensure_control_accounts().await?;
        let ledgers: BTreeSet<u32> = accounts
            .iter()
            .map(|(a, _)| ids::ledger_of(a.currency))
            .collect();
        let mut to_create: Vec<Account> = ledgers
            .iter()
            .map(|l| Account {
                id: ids::opening_account(*l),
                ledger: *l,
                code: ids::CODE_OPENING,
                ..Account::default()
            })
            .collect();
        to_create.extend(accounts.iter().map(|(a, _)| Account {
            id: a.id.as_uuid().as_u128(),
            ledger: ids::ledger_of(a.currency),
            code: ids::code_of(a.account_type),
            flags: ids::flags_of(a.account_type),
            ..Account::default()
        }));
        for chunk in to_create.chunks(CREATE_BATCH) {
            let results = self
                .tb()
                .call(self.tb().client().create_accounts(chunk.to_vec()))
                .await?;
            if let Some((a, r)) = chunk
                .iter()
                .zip(results)
                .find(|(_, r)| *r != AccountResult::OK && *r != AccountResult::EXISTS)
            {
                return Err(StorageError::DataIntegrity(format!(
                    "creating tigerbeetle account {:032x}: {r}",
                    a.id
                ))
                .into());
            }
        }

        let opening: Vec<Transfer> = accounts
            .iter()
            .filter(|(_, raw)| *raw != 0)
            .map(|(a, raw)| {
                let (id, opening) = (
                    a.id.as_uuid().as_u128(),
                    ids::opening_account(ids::ledger_of(a.currency)),
                );
                let (debit, credit) = if *raw > 0 {
                    (opening, id)
                } else {
                    (id, opening)
                };
                Transfer {
                    id: ids::opening_transfer(id),
                    debit_account_id: debit,
                    credit_account_id: credit,
                    amount: raw.unsigned_abs() as u128,
                    ledger: ids::ledger_of(a.currency),
                    code: ids::TRANSFER_OPENING,
                    user_data_128: id,
                    user_data_32: ids::TAG_OPENING,
                    ..Transfer::default()
                }
            })
            .collect();
        for chunk in opening.chunks(CREATE_BATCH) {
            let results = self.tb().create(chunk.to_vec()).await?;
            if let Some((t, r)) = chunk
                .iter()
                .zip(results)
                .find(|(_, r)| *r != TransferResult::OK && *r != TransferResult::EXISTS)
            {
                return Err(StorageError::DataIntegrity(format!(
                    "opening balance of {:032x}: {r} (did money move during the cut-over?)",
                    t.user_data_128
                ))
                .into());
            }
        }

        let all: Vec<AccountId> = accounts.iter().map(|(a, _)| a.id).collect();
        let mismatches = self.reconcile(&all).await?;
        if !mismatches.is_empty() {
            return Err(StorageError::DataIntegrity(format!(
                "after import {} accounts differ from their entries, first {:?}",
                mismatches.len(),
                mismatches[0]
            ))
            .into());
        }
        let report = ImportReport {
            accounts: accounts.len(),
            opening_balances: opening.len(),
            through_seq,
        };
        sqlx::query(
            "INSERT INTO tb_cutover (cluster_id, accounts, opening_balances, through_seq)
             VALUES ($1, $2, $3, $4) ON CONFLICT (cluster_id) DO NOTHING",
        )
        .bind(self.cluster())
        .bind(report.accounts as i64)
        .bind(report.opening_balances as i64)
        .bind(report.through_seq)
        .execute(self.pool())
        .await?;
        Ok(report)
    }

    fn cluster(&self) -> Uuid {
        Uuid::from_u128(self.tb().client().cluster_id())
    }

    /// Readies the cluster to serve this database (run at every start of a server or worker
    /// with `LEDGER_BACKEND=tigerbeetle`): refuses a cluster that was rolled back, or a
    /// database whose balances were never imported into it; creates the control accounts and
    /// every system account Postgres knows (migrations seed them) and registers them.
    pub async fn prepare(&self) -> Result<()> {
        self.refuse_rolled_back().await?;
        let unimported: bool = sqlx::query_scalar(
            "SELECT NOT EXISTS (SELECT 1 FROM tb_cutover WHERE cluster_id = $1)
                    AND EXISTS (SELECT 1 FROM balances WHERE raw_minor <> 0)",
        )
        .bind(self.cluster())
        .fetch_one(self.pool())
        .await?;
        // Every wallet is created in the cluster before Postgres records it, so a cluster that
        // lacks recent ones is another cluster, a reformatted data file, or a database whose
        // (empty) wallets were never imported.
        let recent: Vec<AccountId> = sqlx::query_scalar(
            "SELECT id FROM accounts WHERE account_type = 'user_wallet' ORDER BY id DESC LIMIT 100",
        )
        .fetch_all(self.pool())
        .await?
        .into_iter()
        .map(AccountId)
        .collect();
        let present = self.tb().raw_balances(&recent).await?;
        let missing = recent.iter().filter(|a| !present.contains_key(a)).count();
        if unimported || missing > 0 {
            return Err(StorageError::DataIntegrity(format!(
                "the Postgres balances were never imported into this TigerBeetle cluster \
                 ({missing} of the {} newest wallets are missing from it): check \
                 TIGERBEETLE_CLUSTER_ID and the replica's data file, or stop money movement and \
                 run `payment-server tigerbeetle-import` first",
                recent.len()
            ))
            .into());
        }
        self.open_system_accounts().await
    }

    async fn refuse_rolled_back(&self) -> Result<()> {
        let rolled_back: bool = sqlx::query_scalar(
            "SELECT EXISTS (SELECT 1 FROM tb_cutover
                            WHERE cluster_id = $1 AND rolled_back_at IS NOT NULL)",
        )
        .bind(self.cluster())
        .fetch_one(self.pool())
        .await?;
        if rolled_back {
            return Err(StorageError::DataIntegrity(
                "this TigerBeetle cluster was rolled back to Postgres and is stale: \
                 cut over into a freshly formatted cluster"
                    .into(),
            )
            .into());
        }
        Ok(())
    }

    /// The control accounts and every system account Postgres knows, created (idempotently)
    /// and registered: [`prepare`](Self::prepare) without the cut-over checks.
    pub async fn open_system_accounts(&self) -> Result<()> {
        self.tb().ensure_control_accounts().await?;
        let rows = sqlx::query(
            "SELECT a.id, a.account_type, a.currency, c.exponent
             FROM accounts a JOIN currencies c ON c.code = a.currency
             WHERE a.account_type <> 'user_wallet' ORDER BY a.id",
        )
        .fetch_all(self.pool())
        .await?;
        let mut system = Vec::with_capacity(rows.len());
        for row in &rows {
            system.push(ledger_account(row)?);
        }
        self.tb().open_accounts(&system).await
    }

    /// The way back to `LEDGER_BACKEND=postgres`, with money movement stopped: settles every
    /// reservation, requires TigerBeetle to equal the journal for every account, then rewrites
    /// `balances` from the journal in one transaction (refusing unless the result conserves
    /// per currency) and marks this cluster rolled back, so it is never served again.
    pub async fn rollback_to_postgres(&self) -> Result<RollbackReport> {
        let settled = self.recover_quiesced().await?;
        let ids: Vec<AccountId> = sqlx::query_scalar("SELECT id FROM accounts ORDER BY id")
            .fetch_all(self.pool())
            .await?
            .into_iter()
            .map(AccountId)
            .collect();
        let mismatches = self.reconcile(&ids).await?;
        if !mismatches.is_empty() {
            return Err(StorageError::DataIntegrity(format!(
                "refusing to roll back: {} accounts differ between TigerBeetle and the journal, \
                 first {:?}",
                mismatches.len(),
                mismatches[0]
            ))
            .into());
        }
        let mut db = self.pool().begin().await?;
        let rewritten = sqlx::query(
            "WITH j AS (
                 SELECT b.account_id,
                        COALESCE((SELECT SUM(CASE e.direction WHEN 'credit' THEN e.amount_minor
                                                              ELSE -e.amount_minor END)
                                  FROM entries e WHERE e.account_id = b.account_id), 0)::BIGINT
                            AS raw
                 FROM balances b
             )
             UPDATE balances b SET raw_minor = j.raw, version = b.version + 1, updated_at = now()
             FROM j WHERE j.account_id = b.account_id AND b.raw_minor <> j.raw",
        )
        .execute(&mut *db)
        .await?
        .rows_affected();
        let unbalanced: Vec<String> = sqlx::query_scalar(
            "SELECT a.currency FROM balances b JOIN accounts a ON a.id = b.account_id
             GROUP BY a.currency HAVING SUM(b.raw_minor) <> 0",
        )
        .fetch_all(&mut *db)
        .await?;
        if !unbalanced.is_empty() {
            return Err(StorageError::DataIntegrity(format!(
                "refusing to roll back: the rebuilt balances do not conserve {unbalanced:?}"
            ))
            .into());
        }
        sqlx::query(
            "INSERT INTO tb_cutover
               (cluster_id, accounts, opening_balances, through_seq, rolled_back_at)
             VALUES ($1, 0, 0, 0, now())
             ON CONFLICT (cluster_id) DO UPDATE SET rolled_back_at = now()",
        )
        .bind(self.cluster())
        .execute(&mut *db)
        .await?;
        storage::commit_durable(db).await?;
        Ok(RollbackReport {
            accounts: ids.len(),
            balances_rewritten: rewritten,
            settled,
        })
    }

    /// TigerBeetle balances against the journal mirror. Exact once recovery has settled every
    /// committed attempt (until then a committed transaction may still await its post).
    pub async fn reconcile(&self, accounts: &[AccountId]) -> Result<Vec<Mismatch>> {
        let mut out = Vec::new();
        for chunk in accounts.chunks(LOOKUP_BATCH) {
            let ids: Vec<Uuid> = chunk.iter().map(|a| a.as_uuid()).collect();
            let journal: HashMap<Uuid, i64> = sqlx::query_as(
                "SELECT account_id, SUM(CASE direction WHEN 'credit' THEN amount_minor
                                                       ELSE -amount_minor END)::BIGINT
                 FROM entries WHERE account_id = ANY($1) GROUP BY account_id",
            )
            .bind(&ids)
            .fetch_all(self.pool())
            .await?
            .into_iter()
            .collect();
            // An account the cluster lacks holds nothing there.
            let tb = self.tb().raw_balances(chunk).await?;
            for account in chunk {
                let tigerbeetle_raw = tb.get(account).copied().unwrap_or(0);
                let journal_raw = journal.get(&account.as_uuid()).copied().unwrap_or(0) as i128;
                if tigerbeetle_raw != journal_raw {
                    out.push(Mismatch {
                        account: *account,
                        tigerbeetle_raw,
                        journal_raw,
                    });
                }
            }
        }
        Ok(out)
    }
}
