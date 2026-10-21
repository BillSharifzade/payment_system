use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, RwLock};

use ledger::{Account as LedgerAccount, AccountId, LedgerError, NormalSide, Transaction};
use money::{Money, MoneyError};
use storage::StorageError;
use uuid::Uuid;

use crate::client::{transfer_flags as tf, Account, AccountResult, TbClient, Transfer};
use crate::config::TbConfig;
use crate::error::{first_failure, leg_failure, LegFailure, Result, TbError};
use crate::ids::{self, CONTROL_LEDGER};
use crate::plan::{plan, AccountInfo, Plan};

/// Balances and the no-overdraft rule in TigerBeetle, with no Postgres at all: the registry,
/// balance reads and the direct (single-phase) posting path. [`HybridLedger`] builds the
/// guarded protocol on top of it.
///
/// [`HybridLedger`]: crate::HybridLedger
pub struct TbLedger<C> {
    tb: Arc<C>,
    cfg: Arc<TbConfig>,
    registry: Arc<RwLock<HashMap<AccountId, AccountInfo>>>,
}

impl<C> Clone for TbLedger<C> {
    fn clone(&self) -> Self {
        Self {
            tb: self.tb.clone(),
            cfg: self.cfg.clone(),
            registry: self.registry.clone(),
        }
    }
}

/// A TigerBeetle account seen through the ledger's orientation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Balance {
    pub account: AccountId,
    /// Posted balance on the account's normal side (what `LedgerEngine::balance` reports).
    pub posted: Money,
    /// Posted minus reservations still pending against it: what can be spent right now.
    pub available: Money,
    /// Raw posted balance, credit positive: comparable with the Postgres entries mirror.
    pub raw: i128,
}

pub(crate) fn low64(x: u128) -> u64 {
    x as u64
}

/// Chains transfers: all succeed or none does.
pub(crate) fn link(mut chain: Vec<Transfer>) -> Vec<Transfer> {
    let last = chain.len().saturating_sub(1);
    for t in &mut chain[..last] {
        t.flags |= tf::LINKED;
    }
    chain
}

impl<C: TbClient> TbLedger<C> {
    pub fn new(tb: C, cfg: TbConfig) -> Self {
        Self {
            tb: Arc::new(tb),
            cfg: Arc::new(cfg),
            registry: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    pub fn client(&self) -> &C {
        &self.tb
    }

    /// The same cluster connection and registry under another configuration.
    pub fn with_config(&self, cfg: TbConfig) -> Self {
        Self {
            tb: self.tb.clone(),
            cfg: Arc::new(cfg),
            registry: self.registry.clone(),
        }
    }

    pub fn config(&self) -> &TbConfig {
        &self.cfg
    }

    /// Bounds a request: the native client retries until the cluster answers.
    pub(crate) async fn call<T>(
        &self,
        fut: impl Future<Output = core::result::Result<T, crate::ClientError>>,
    ) -> Result<T> {
        match tokio::time::timeout(self.cfg.request_timeout, fut).await {
            Ok(r) => Ok(r?),
            Err(_) => Err(TbError::Unavailable(format!(
                "no reply within {:?}",
                self.cfg.request_timeout
            ))),
        }
    }

    pub(crate) async fn create(&self, chain: Vec<Transfer>) -> Result<Vec<crate::TransferResult>> {
        let n = chain.len();
        let results = self.call(self.tb.create_transfers(chain)).await?;
        if results.len() != n {
            return Err(TbError::Protocol(format!(
                "{} results for {n} transfers",
                results.len()
            )));
        }
        Ok(results)
    }

    async fn create_accounts(&self, accounts: Vec<Account>) -> Result<()> {
        let results = self.call(self.tb.create_accounts(accounts.clone())).await?;
        for (a, r) in accounts.iter().zip(&results) {
            if *r != AccountResult::OK && *r != AccountResult::EXISTS {
                return Err(StorageError::DataIntegrity(format!(
                    "tigerbeetle account {:032x}: {r}",
                    a.id
                ))
                .into());
            }
        }
        Ok(())
    }

    /// The two control accounts the direct path's commit markers run between. Idempotent;
    /// run at startup.
    pub async fn ensure_control_accounts(&self) -> Result<()> {
        self.create_accounts(
            (0..2)
                .map(|side| Account {
                    id: ids::control_account(side),
                    ledger: CONTROL_LEDGER,
                    code: ids::CODE_CONTROL,
                    ..Account::default()
                })
                .collect(),
        )
        .await
    }

    /// Creates the account in TigerBeetle (idempotent) and registers it.
    pub async fn open_account(&self, account: &LedgerAccount) -> Result<()> {
        self.open_accounts(std::slice::from_ref(account)).await
    }

    /// [`open_account`](Self::open_account) for many, in request-sized batches.
    pub async fn open_accounts(&self, accounts: &[LedgerAccount]) -> Result<()> {
        for chunk in accounts.chunks(crate::client::CREATE_BATCH) {
            self.create_accounts(
                chunk
                    .iter()
                    .map(|a| Account {
                        id: a.id.as_uuid().as_u128(),
                        ledger: ids::ledger_of(a.currency),
                        code: ids::code_of(a.account_type),
                        flags: ids::flags_of(a.account_type),
                        ..Account::default()
                    })
                    .collect(),
            )
            .await?;
            let mut reg = self.registry.write().expect("registry poisoned");
            for a in chunk {
                reg.insert(
                    a.id,
                    AccountInfo {
                        account_type: a.account_type,
                        currency: a.currency,
                    },
                );
            }
        }
        Ok(())
    }

    /// Account types and currencies, from the cache or TigerBeetle (accounts never change).
    pub async fn account_info(&self, ids: &[AccountId]) -> Result<HashMap<AccountId, AccountInfo>> {
        let mut found = HashMap::with_capacity(ids.len());
        let mut missing = Vec::new();
        {
            let reg = self.registry.read().expect("registry poisoned");
            for id in ids {
                match reg.get(id) {
                    Some(info) => {
                        found.insert(*id, *info);
                    }
                    None if !missing.contains(&id.as_uuid().as_u128()) => {
                        missing.push(id.as_uuid().as_u128())
                    }
                    None => {}
                }
            }
        }
        if missing.is_empty() {
            return Ok(found);
        }
        let accounts = self.call(self.tb.lookup_accounts(missing)).await?;
        let mut reg = self.registry.write().expect("registry poisoned");
        for a in accounts {
            let info = describe(&a)?;
            let id = AccountId(Uuid::from_u128(a.id));
            reg.insert(id, info);
            found.insert(id, info);
        }
        Ok(found)
    }

    pub(crate) async fn plan(&self, txn: &Transaction) -> Result<Plan> {
        let ids: Vec<AccountId> = txn.entries.iter().map(|e| e.account_id).collect();
        let info = self.account_info(&ids).await?;
        plan(txn, |id| info.get(&id).copied())
    }

    pub async fn balances(&self, accounts: &[AccountId]) -> Result<Vec<Balance>> {
        let mut by_id: HashMap<u128, Account> = HashMap::with_capacity(accounts.len());
        for chunk in accounts.chunks(crate::client::LOOKUP_BATCH) {
            let ids = chunk.iter().map(|a| a.as_uuid().as_u128()).collect();
            let found = self.call(self.tb.lookup_accounts(ids)).await?;
            by_id.extend(found.into_iter().map(|a| (a.id, a)));
        }
        accounts
            .iter()
            .map(|id| {
                let a = by_id
                    .get(&id.as_uuid().as_u128())
                    .ok_or(LedgerError::UnknownAccount(*id))?;
                balance_of(*id, a)
            })
            .collect()
    }

    /// Raw posted balances (credit positive, comparable with the journal) of the accounts
    /// that exist in the cluster; the others are absent.
    pub async fn raw_balances(&self, accounts: &[AccountId]) -> Result<HashMap<AccountId, i128>> {
        let mut out = HashMap::with_capacity(accounts.len());
        for chunk in accounts.chunks(crate::client::LOOKUP_BATCH) {
            let ids = chunk.iter().map(|a| a.as_uuid().as_u128()).collect();
            for a in self.call(self.tb.lookup_accounts(ids)).await? {
                let id = AccountId(Uuid::from_u128(a.id));
                out.insert(id, balance_of(id, &a)?.raw);
            }
        }
        Ok(out)
    }

    pub async fn balance(&self, account: AccountId) -> Result<Money> {
        Ok(self.balances(&[account]).await?[0].posted)
    }

    /// The timestamp of the newest transfer in the cluster (0 for none). Timestamps strictly
    /// increase, so every transfer applied before this call is at or below it, and every one
    /// applied after it is above.
    pub async fn newest_timestamp(&self) -> Result<u64> {
        let newest = self
            .call(self.tb.query_transfers(crate::client::QueryFilter {
                limit: 1,
                reversed: true,
                ..Default::default()
            }))
            .await?;
        Ok(newest.first().map_or(0, |t| t.timestamp))
    }

    /// When each transaction's movement was applied (the cluster timestamp of its post, or of
    /// its forced re-post); transactions not (yet) posted are absent.
    pub async fn posted_at(&self, transactions: &[Uuid]) -> Result<HashMap<Uuid, u64>> {
        let mut out = HashMap::with_capacity(transactions.len());
        for chunk in transactions.chunks(crate::client::LOOKUP_BATCH) {
            let by_post: HashMap<u128, Uuid> = chunk
                .iter()
                .map(|t| (ids::leg(ids::post_base(t.as_u128()), 0), *t))
                .collect();
            let found = self
                .call(self.tb.lookup_transfers(by_post.keys().copied().collect()))
                .await?;
            out.extend(
                found
                    .into_iter()
                    .filter_map(|t| by_post.get(&t.id).map(|txn| (*txn, t.timestamp))),
            );
        }
        Ok(out)
    }

    /// The pure-TigerBeetle fast path: one linked chain, no Postgres, no guards. The chain
    /// starts with the transaction's commit marker (a zero-amount transfer between the
    /// control accounts whose id derives from the transaction id), so a transaction posts at
    /// most once and a retry reports `DuplicateTransaction`. The marker cannot fail
    /// transiently, so a transaction rejected for funds can be retried later; the legs carry
    /// fresh ids for the same reason.
    pub async fn post_direct(&self, txn: &Transaction) -> Result<()> {
        txn.validate()?;
        let plan = self.plan(txn).await?;
        let t = txn.id.as_uuid().as_u128();
        let attempt = ids::new_attempt();
        let mut chain = vec![Transfer {
            id: ids::leg(ids::post_base(t), 0),
            debit_account_id: ids::control_account(0),
            credit_account_id: ids::control_account(1),
            ledger: CONTROL_LEDGER,
            code: ids::TRANSFER_MARKER,
            user_data_128: t,
            user_data_64: low64(attempt),
            user_data_32: ids::TAG_MARKER,
            ..Transfer::default()
        }];
        chain.extend(plan.legs.iter().enumerate().map(|(i, l)| Transfer {
            id: ids::leg(attempt, i),
            debit_account_id: l.debit,
            credit_account_id: l.credit,
            amount: l.amount,
            ledger: l.ledger,
            code: ids::TRANSFER_LEG,
            user_data_128: t,
            user_data_64: plan.legs.len() as u64,
            user_data_32: ids::TAG_DIRECT,
            ..Transfer::default()
        }));
        let results = self.create(link(chain)).await?;
        match first_failure(&results) {
            None => Ok(()),
            Some((0, r)) if r.name().starts_with("EXISTS") => {
                Err(LedgerError::DuplicateTransaction(txn.id).into())
            }
            Some((0, r)) => Err(TbError::Protocol(format!("commit marker: {r}"))),
            Some((i, r)) => Err(self.leg_error(&plan, i - 1, r).await),
        }
    }

    /// Maps a failed ledger leg onto the ledger's errors, with `InMemoryLedger`'s details.
    pub(crate) async fn leg_error(
        &self,
        plan: &Plan,
        i: usize,
        r: crate::TransferResult,
    ) -> TbError {
        let leg = plan.legs[i];
        let side = |debit_side: bool| if debit_side { leg.debit } else { leg.credit };
        match leg_failure(r) {
            LegFailure::Insufficient { debit_side } => {
                let account = AccountId(Uuid::from_u128(side(debit_side)));
                match self.balances(&[account]).await {
                    Ok(b) => LedgerError::InsufficientFunds {
                        account,
                        balance_minor: b[0].raw,
                        delta_minor: plan.deltas[&side(debit_side)],
                    }
                    .into(),
                    Err(e) => e,
                }
            }
            LegFailure::UnknownAccount { debit_side } => {
                LedgerError::UnknownAccount(AccountId(Uuid::from_u128(side(debit_side)))).into()
            }
            LegFailure::LedgerMismatch => StorageError::DataIntegrity(format!(
                "tigerbeetle and the account registry disagree on the ledger of {:032x} or {:032x}",
                leg.debit, leg.credit
            ))
            .into(),
            LegFailure::Overflow => {
                LedgerError::Money(MoneyError::Overflow { operation: "post" }).into()
            }
            LegFailure::Protocol => TbError::Protocol(format!("leg {i}: {r}")),
        }
    }
}

fn describe(a: &Account) -> Result<AccountInfo> {
    match (ids::account_type_of(a.code), ids::currency_of(a.ledger)) {
        (Some(account_type), Some(currency)) => Ok(AccountInfo {
            account_type,
            currency,
        }),
        _ => Err(StorageError::DataIntegrity(format!(
            "tigerbeetle account {:032x} has code {} on ledger {}: not a ledger account",
            a.id, a.code, a.ledger
        ))
        .into()),
    }
}

fn balance_of(id: AccountId, a: &Account) -> Result<Balance> {
    let info = describe(a)?;
    let int = |v: u128| {
        i128::try_from(v).map_err(|_| MoneyError::Overflow {
            operation: "balance",
        })
    };
    let raw = int(a.credits_posted)? - int(a.debits_posted)?;
    let (posted, available) = match info.account_type.normal_side() {
        NormalSide::Credit => (raw, raw - int(a.debits_pending)?),
        NormalSide::Debit => (-raw, -raw - int(a.credits_pending)?),
    };
    Ok(Balance {
        account: id,
        posted: Money::from_minor(posted, info.currency),
        available: Money::from_minor(available, info.currency),
        raw,
    })
}
