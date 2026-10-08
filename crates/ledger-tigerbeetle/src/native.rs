//! TigerBeetle's native client (via `tigerbeetle-unofficial`, release 0.16.78, wire-compatible
//! with 0.16.4+ … 0.17.x servers) behind [`TbClient`]. The conversion is field for field; the
//! client reports only failed events, so the results are expanded back to one per event.
//! Feature `native-client`: building it needs Zig (`ZIG_PATH`, else the build downloads one)
//! and libclang.

use std::sync::atomic::{AtomicUsize, Ordering};

use tb::error::{CreateAccountsError, CreateTransfersError};
use tigerbeetle_unofficial as tb;

use crate::client::{
    Account, AccountResult, ClientError, QueryFilter, TbClient, Transfer, TransferResult,
};
use crate::config::TbConfig;

/// A client is one session with one request in flight; concurrent calls on it are batched
/// into its next request. The hybrid protocol makes two dependent calls per posting, so it
/// gains from a few sessions pipelining in the replica (`sessions`, round-robin).
pub struct LiveTb {
    clients: Vec<tb::Client>,
    next: AtomicUsize,
    cluster_id: u128,
}

impl LiveTb {
    pub fn connect(
        cluster_id: u128,
        addresses: &str,
        sessions: usize,
    ) -> Result<Self, ClientError> {
        let clients = (0..sessions.max(1))
            .map(|_| tb::Client::new(cluster_id, addresses))
            .collect::<Result<_, _>>()
            .map_err(|e| ClientError(format!("connecting to {addresses}: {e}")))?;
        Ok(Self {
            clients,
            next: AtomicUsize::new(0),
            cluster_id,
        })
    }

    pub fn from_config(cfg: &TbConfig) -> Result<Self, ClientError> {
        Self::connect(cfg.cluster_id, &cfg.addresses, cfg.sessions)
    }

    /// For tests and the benchmark: `TIGERBEETLE_ADDRESSES` (default 3000),
    /// `TIGERBEETLE_CLUSTER_ID` (default 0), `TIGERBEETLE_SESSIONS` (default 1).
    pub fn from_test_env() -> Self {
        let var = |k: &str, default: &str| std::env::var(k).unwrap_or_else(|_| default.into());
        let cfg = TbConfig::from_lookup(|k| {
            Some(match k {
                "TIGERBEETLE_CLUSTER_ID" => var(k, "0"),
                "TIGERBEETLE_ADDRESSES" => var(k, "3000"),
                _ => std::env::var(k).ok()?,
            })
        })
        .expect("TIGERBEETLE_* for the test cluster");
        Self::from_config(&cfg).expect("TigerBeetle client")
    }

    fn client(&self) -> &tb::Client {
        &self.clients[self.next.fetch_add(1, Ordering::Relaxed) % self.clients.len()]
    }
}

fn to_raw_account(a: &Account) -> tb::Account {
    tb::Account::from_raw(tb::account::Raw {
        id: a.id,
        debits_pending: a.debits_pending,
        debits_posted: a.debits_posted,
        credits_pending: a.credits_pending,
        credits_posted: a.credits_posted,
        user_data_128: a.user_data_128,
        user_data_64: a.user_data_64,
        user_data_32: a.user_data_32,
        reserved: 0,
        ledger: a.ledger,
        code: a.code,
        flags: a.flags,
        timestamp: a.timestamp,
    })
}

fn from_raw_account(a: &tb::Account) -> Account {
    let r = a.as_raw();
    Account {
        id: r.id,
        debits_pending: r.debits_pending,
        debits_posted: r.debits_posted,
        credits_pending: r.credits_pending,
        credits_posted: r.credits_posted,
        user_data_128: r.user_data_128,
        user_data_64: r.user_data_64,
        user_data_32: r.user_data_32,
        ledger: r.ledger,
        code: r.code,
        flags: r.flags,
        timestamp: r.timestamp,
    }
}

fn to_raw_transfer(t: &Transfer) -> tb::Transfer {
    tb::Transfer::from_raw(tb::transfer::Raw {
        id: t.id,
        debit_account_id: t.debit_account_id,
        credit_account_id: t.credit_account_id,
        amount: t.amount,
        pending_id: t.pending_id,
        user_data_128: t.user_data_128,
        user_data_64: t.user_data_64,
        user_data_32: t.user_data_32,
        timeout: t.timeout,
        ledger: t.ledger,
        code: t.code,
        flags: t.flags,
        timestamp: t.timestamp,
    })
}

fn from_raw_transfer(t: &tb::Transfer) -> Transfer {
    let r = t.as_raw();
    Transfer {
        id: r.id,
        debit_account_id: r.debit_account_id,
        credit_account_id: r.credit_account_id,
        amount: r.amount,
        pending_id: r.pending_id,
        user_data_128: r.user_data_128,
        user_data_64: r.user_data_64,
        user_data_32: r.user_data_32,
        timeout: r.timeout,
        ledger: r.ledger,
        code: r.code,
        flags: r.flags,
        timestamp: r.timestamp,
    }
}

fn send_error(e: impl std::fmt::Display) -> ClientError {
    ClientError(e.to_string())
}

impl TbClient for LiveTb {
    async fn create_accounts(
        &self,
        accounts: Vec<Account>,
    ) -> Result<Vec<AccountResult>, ClientError> {
        let mut out = vec![AccountResult::OK; accounts.len()];
        let raw: Vec<tb::Account> = accounts.iter().map(to_raw_account).collect();
        match self.client().create_accounts(raw).await {
            Ok(()) => {}
            Err(CreateAccountsError::Api(errors)) => {
                for e in errors.as_slice() {
                    out[e.index() as usize] = AccountResult(e.inner().code().get());
                }
            }
            Err(CreateAccountsError::Send(e)) => return Err(send_error(e)),
            Err(e) => return Err(send_error(e)),
        }
        Ok(out)
    }

    async fn create_transfers(
        &self,
        transfers: Vec<Transfer>,
    ) -> Result<Vec<TransferResult>, ClientError> {
        let mut out = vec![TransferResult::OK; transfers.len()];
        let raw: Vec<tb::Transfer> = transfers.iter().map(to_raw_transfer).collect();
        match self.client().create_transfers(raw).await {
            Ok(()) => {}
            Err(CreateTransfersError::Api(errors)) => {
                for e in errors.as_slice() {
                    out[e.index() as usize] = TransferResult(e.inner().code().get());
                }
            }
            Err(CreateTransfersError::Send(e)) => return Err(send_error(e)),
            Err(e) => return Err(send_error(e)),
        }
        Ok(out)
    }

    async fn lookup_accounts(&self, ids: Vec<u128>) -> Result<Vec<Account>, ClientError> {
        let found = self
            .client()
            .lookup_accounts(ids)
            .await
            .map_err(send_error)?;
        Ok(found.iter().map(from_raw_account).collect())
    }

    async fn lookup_transfers(&self, ids: Vec<u128>) -> Result<Vec<Transfer>, ClientError> {
        let found = self
            .client()
            .lookup_transfers(ids)
            .await
            .map_err(send_error)?;
        Ok(found.iter().map(from_raw_transfer).collect())
    }

    async fn query_transfers(&self, f: QueryFilter) -> Result<Vec<Transfer>, ClientError> {
        let filter = tb::QueryFilter::from_raw(tb::core::query_filter::Raw {
            user_data_128: f.user_data_128,
            user_data_64: f.user_data_64,
            user_data_32: f.user_data_32,
            ledger: f.ledger,
            code: f.code,
            reserved: [0; 6],
            timestamp_min: f.timestamp_min,
            timestamp_max: f.timestamp_max,
            limit: f.limit,
            flags: u32::from(f.reversed),
        });
        let found = self
            .client()
            .query_transfers(Box::new(filter))
            .await
            .map_err(send_error)?;
        Ok(found.iter().map(from_raw_transfer).collect())
    }

    fn cluster_id(&self) -> u128 {
        self.cluster_id
    }
}

/// Cluster time is wall time: the suites really wait out pending timeouts.
#[cfg(feature = "testkit")]
impl crate::testkit::Cluster for LiveTb {
    async fn pass_time(&self, secs: u64) {
        tokio::time::sleep(std::time::Duration::from_secs(secs)).await;
    }

    fn short_timeout(&self) -> u32 {
        2
    }
}
