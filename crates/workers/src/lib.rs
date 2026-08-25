//! Background workers for the payment system.
//!
//! - [`seal_all`] / [`seal_next_batch`] / [`verify_chain`] — the cryptographic
//!   checkpoint sealer and its independent verifier (tamper-evidence).
//! - [`reconcile`] — the continuous correctness check (conservation + balance
//!   integrity).
//!
//! These are deliberately plain functions over a `PgPool`, so they are trivial
//! to call from the worker binary, from tests, or on demand from an admin tool.

mod error;
mod reconcile;
mod relay;
mod retention;
mod sealer;

pub use error::{Result, WorkerError};
pub use reconcile::{reconcile, BalanceMismatch, ReconciliationReport};
pub use relay::{
    relay_all, relay_once, EventPublisher, LoggingPublisher, NatsPublisher, OutboxEvent,
    PublishError,
};
pub use retention::{prune_expired, RetentionConfig, RetentionReport};
pub use sealer::{
    seal_all, seal_next_batch, verify_chain, verify_chain_from, CheckpointSummary, VerifyReport,
    VerifyState,
};
