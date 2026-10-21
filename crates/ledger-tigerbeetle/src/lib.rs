//! TigerBeetle ledger backend (DESIGN.md §15, Phase 2).
//!
//! - [`TbLedger`]: accounts, balances and the pure-TigerBeetle fast path (`post_direct`):
//!   one linked chain per transaction, no Postgres, no guards.
//! - [`HybridLedger`]: the drop-in for `PostgresLedger::post_on` that `LEDGER_BACKEND=tigerbeetle`
//!   serves. Balances and the no-overdraft rule move to TigerBeetle; the guards that must hold
//!   inside the posting lock (per-user AML, single-use transitions, audit rows), the idempotency
//!   record, the outbox and the journal mirror stay in one Postgres transaction. See
//!   `hybrid.rs` for the reserve → commit → post protocol and `recovery.rs` for crash recovery.
//! - [`SimTb`]: an in-process model of a cluster for tests. With the `native-client` feature,
//!   `LiveTb` is TigerBeetle's own client (building it needs Zig and libclang) and the same
//!   suites run against a real cluster (`tests/live.rs`); [`AnyTb`] is either.

mod any;
mod client;
mod config;
mod error;
mod hybrid;
mod ids;
mod import;
#[cfg(feature = "native-client")]
mod native;
mod plan;
mod recovery;
mod sim;
mod tb;

#[cfg(any(test, feature = "testkit"))]
pub mod testkit;
#[cfg(test)]
mod tests;

pub use any::{backend_from_env, AnyTb};
pub use client::{
    account_flags, transfer_flags, Account, AccountResult, ClientError, QueryFilter, TbClient,
    Transfer, TransferResult, AMOUNT_MAX,
};
pub use config::{Backend, TbConfig};
pub use error::{Result, TbError};
pub use hybrid::HybridLedger;
pub use import::{ImportReport, Mismatch, RollbackReport};
#[cfg(feature = "native-client")]
pub use native::LiveTb;
pub use plan::AccountInfo;
pub use recovery::RecoveryReport;
pub use sim::{Fault, Op, SimTb};
pub use tb::{Balance, TbLedger};
