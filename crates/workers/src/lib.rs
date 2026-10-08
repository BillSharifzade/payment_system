pub mod anchor;
pub mod env;
mod error;
mod leader;
mod reconcile;
mod relay;
mod retention;
mod sealer;
pub mod signer;

pub use error::{Result, WorkerError};
pub use leader::{LeaderLock, LEADER_LOCK_KEY};
pub use reconcile::{
    reconcile, reconcile_full, reconcile_incremental, unsealed_lag, BalanceMismatch,
    FullReconciliation, ReconcileConfig, ReconciliationReport,
};
pub use relay::{
    outbox_lag, relay_all, relay_once, EventPublisher, LoggingPublisher, NatsPublisher,
    OutboxEvent, PublishError, PUBLISH_TIMEOUT,
};
pub use retention::{prune_expired, RetentionConfig, RetentionReport};
pub use sealer::{
    seal_all, seal_next_batch, verify_chain, verify_chain_from, verify_chain_page,
    CheckpointSummary, VerifyPage, VerifyReport, VerifyState, VERIFY_PAGE_CHECKPOINTS,
    VERIFY_PAGE_TRANSACTIONS,
};
